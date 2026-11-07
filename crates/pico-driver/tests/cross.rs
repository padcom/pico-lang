//! Cross-compilation (spec §15): `linux-x64` from another host gives an x86-64 ELF linked by
//! the built-in lld against generated glibc stubs, using no symbol newer than the requested
//! glibc, and it runs under qemu-user when that's set up; macOS and Windows targets build too.

use std::path::Path;

/// An x86-64 glibc for qemu to load (Debian's `libc6-amd64-cross`); building doesn't need it.
const SYSROOT: &str = "/usr/x86_64-linux-gnu";

const SRC: &str = "println(`hello from ${Math.cbrt(27)} ${[1, 2, 3].map((x: int) => x * 2):?}`)\n";

fn build(target: &str) -> Option<std::path::PathBuf> {
    let target = pico_driver::Target::parse(target).unwrap();
    if !pico_lld::AVAILABLE {
        eprintln!("skipped: needs lld");
        return None;
    }
    let checked = pico_driver::check_source("cross.pi", SRC);
    assert!(!checked.has_errors(), "{}", checked.render_diagnostics());
    let opts = pico_driver::BuildOptions {
        release: false,
        target: Some(target),
    };
    Some(pico_driver::build_cached(&checked, opts).unwrap())
}

/// The glibc symbol versions `exe` requires, as (major, minor), via LLVM's readelf.
fn glibc_versions(exe: &Path) -> Option<Vec<(u32, u32)>> {
    let out = std::process::Command::new("llvm-readelf")
        .arg("-V")
        .arg(exe)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut vs: Vec<(u32, u32)> = text
        .split("GLIBC_")
        .skip(1)
        .filter_map(|s| {
            let v: String = s
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            let mut it = v.split('.');
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect();
    vs.sort();
    vs.dedup();
    Some(vs)
}

#[test]
fn linux_x64() {
    let Some(exe) = build("linux-x64") else {
        return;
    };
    let bytes = std::fs::read(&exe).unwrap();
    assert_eq!(&bytes[..4], b"\x7fELF");
    assert_eq!(
        u16::from_le_bytes([bytes[18], bytes[19]]),
        62,
        "e_machine: x86-64"
    );
    match glibc_versions(&exe) {
        Some(vs) => assert!(vs.last().is_some_and(|v| *v <= (2, 31)), "{vs:?}"),
        None => eprintln!("versions not checked: no llvm-readelf"),
    }
    let host_x64 = cfg!(target_arch = "x86_64");
    if !host_x64
        && !(Path::new("/proc/sys/fs/binfmt_misc/qemu-x86_64").exists()
            && Path::new(SYSROOT).is_dir())
    {
        eprintln!("not run: no qemu-x86_64 binfmt handler or {SYSROOT}");
        return;
    }
    let out = std::process::Command::new(&exe)
        .env("QEMU_LD_PREFIX", SYSROOT)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hello from 3 [2, 4, 6]\n"
    );
}

/// A newer glibc than the default gets that version's symbols: `.2.39` resolves `strtol` &c.
/// the 2.38 way and threads from libc, not libpthread.
#[test]
fn glibc_version_suffix() {
    let host = if cfg!(target_arch = "x86_64") {
        "linux-x64"
    } else {
        "linux-arm64"
    };
    let Some(exe) = build(&format!("{host}.2.39")) else {
        return;
    };
    if let Some(vs) = glibc_versions(&exe) {
        assert!(vs.last().is_some_and(|v| *v <= (2, 39)), "{vs:?}");
    }
    let out = std::process::Command::new(&exe).output().unwrap();
    // The host's glibc may be older than 2.39; then the loader refuses, which is the point.
    if out.status.success() {
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "hello from 3 [2, 4, 6]\n"
        );
    }
}

/// musl targets are fully static (spec §15.1): no interpreter, no shared libraries.
#[test]
fn musl_static() {
    for target in ["linux-arm64-musl", "linux-x64-musl"] {
        let Some(exe) = build(target) else {
            return;
        };
        let bytes = std::fs::read(&exe).unwrap();
        let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
        assert_eq!(machine, if target.contains("x64") { 62 } else { 183 });
        // e_type ET_EXEC and no PT_INTERP among the program headers.
        assert_eq!(
            u16::from_le_bytes([bytes[16], bytes[17]]),
            2,
            "{target}: ET_EXEC"
        );
        let phoff = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
        let phnum = u16::from_le_bytes([bytes[56], bytes[57]]) as usize;
        for i in 0..phnum {
            let ty = u32::from_le_bytes(bytes[phoff + i * 56..][..4].try_into().unwrap());
            assert_ne!(ty, 3, "{target}: has PT_INTERP");
        }
        let native = (machine == 62) == cfg!(target_arch = "x86_64");
        if !native && !Path::new("/proc/sys/fs/binfmt_misc/qemu-x86_64").exists() {
            continue;
        }
        let out = std::process::Command::new(&exe).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "hello from 3 [2, 4, 6]\n",
            "{target}"
        );
    }
}

/// macOS from any host (P8.4): Mach-O executables for both architectures, linked by lld
/// against the bundled `libSystem.tbd`; on a Mac they run (x86-64 under Rosetta).
#[test]
fn macos() {
    for (target, cpu) in [("macos-arm64", 0x0100_000c_u32), ("macos-x64", 0x0100_0007)] {
        let Some(exe) = build(target) else {
            return;
        };
        let bytes = std::fs::read(&exe).unwrap();
        let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!(word(0), 0xfeed_facf, "{target}: 64-bit Mach-O");
        assert_eq!(word(4), cpu, "{target}: CPU type");
        assert_eq!(word(12), 2, "{target}: MH_EXECUTE");
        if !cfg!(target_os = "macos") {
            continue;
        }
        let out = std::process::Command::new(&exe).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "hello from 3 [2, 4, 6]\n",
            "{target}"
        );
    }
}

/// `@link` with `project.json`'s `c.libDirs` (spec §12.1, #113): a static C library built for
/// each target is found in the directory listed for that target, on Windows and macOS (pico's
/// lld) as on Linux.
#[test]
fn link_libraries_from_lib_dirs() {
    if !pico_lld::AVAILABLE {
        eprintln!("skipped: needs lld");
        return;
    }
    let (Some(clang), Some(ar)) = (on_path("clang"), on_path("llvm-ar")) else {
        eprintln!("skipped: needs clang and llvm-ar on PATH");
        return;
    };
    let dir = tempdir("libdirs");
    std::fs::write(
        dir.join("adder.c"),
        "int add_numbers(int a, int b) { return a + b; }\n",
    )
    .unwrap();
    let host = pico_driver::Target::host().to_string();
    let targets = [
        ("windows-x64", "x86_64-w64-windows-gnu"),
        ("macos-arm64", "arm64-apple-macos11"),
        ("host", host.as_str()),
    ];
    let mut map = Vec::new();
    for (name, triple) in targets {
        let sub = dir.join(name);
        std::fs::create_dir_all(&sub).unwrap();
        let obj = sub.join("adder.o");
        let ok = std::process::Command::new(&clang)
            .arg(format!("--target={triple}"))
            .args(["-c", "-o"])
            .arg(&obj)
            .arg(dir.join("adder.c"))
            .status()
            .unwrap()
            .success();
        assert!(ok, "compiling adder.c for {triple}");
        let ok = std::process::Command::new(&ar)
            .arg("rcs")
            .arg(sub.join("libadder.a"))
            .arg(&obj)
            .status()
            .unwrap()
            .success();
        assert!(ok, "archiving for {triple}");
        let key = if name == "host" {
            host.clone()
        } else {
            name.to_string()
        };
        map.push(format!("\"{key}\": [\"{name}\"]"));
    }
    std::fs::write(
        dir.join("project.json"),
        format!("{{ \"c\": {{ \"libDirs\": {{ {} }} }} }}", map.join(", ")),
    )
    .unwrap();
    let src = "@link(\"adder\")\nextern \"C\" {\n  function add_numbers(a: c_int, b: c_int): c_int\n}\n\
               println(add_numbers(2 as c_int, 3 as c_int))\n";
    for (name, _) in targets {
        let target = (name != "host").then(|| pico_driver::Target::parse(name).unwrap());
        let opts = pico_driver::BuildOptions {
            release: false,
            target,
        };
        let checked = pico_driver::check_source_with("main.pi", src, &dir, opts);
        assert!(!checked.has_errors(), "{}", checked.render_diagnostics());
        let exe =
            pico_driver::build_cached(&checked, opts).unwrap_or_else(|e| panic!("{name}: {e}"));
        if name == "host" {
            let out = std::process::Command::new(&exe).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), "5\n");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `@link` on Windows (#113): a system DLL gets an import library from MinGW's `.def` files;
/// a library that's nowhere is an error naming it and saying where to put it.
#[test]
fn windows_link() {
    if !pico_lld::AVAILABLE {
        eprintln!("skipped: needs lld");
        return;
    }
    let target = Some(pico_driver::Target::parse("windows-x64").unwrap());
    let opts = pico_driver::BuildOptions {
        release: false,
        target,
    };
    let src = "@link(\"user32\")\nextern \"C\" {\n  function MessageBeep(kind: c_uint): i32\n}\n\
               if (false) MessageBeep(0 as c_uint)\nprintln(\"hi\")\n";
    let checked = pico_driver::check_source("user32.pi", src);
    assert!(!checked.has_errors(), "{}", checked.render_diagnostics());
    let exe = pico_driver::build_cached(&checked, opts).unwrap();
    let bytes = std::fs::read(&exe).unwrap();
    let has = |s: &[u8]| bytes.windows(s.len()).any(|w| w.eq_ignore_ascii_case(s));
    assert!(has(b"user32.dll"), "imports user32.dll");
    let missing = src.replace("user32", "nosuchlib");
    let checked = pico_driver::check_source("missing.pi", missing);
    let err = pico_driver::build_cached(&checked, opts).unwrap_err();
    assert!(err.contains("can't find the library `nosuchlib`"), "{err}");
    assert!(err.contains("-L"), "{err}");
}

fn on_path(name: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(name))
            .find(|f| f.is_file())
    })
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pico-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}
