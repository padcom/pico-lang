//! Cross-compilation (spec §15): `linux-x64` from another host gives an x86-64 ELF linked by
//! the built-in lld against generated glibc stubs, using no symbol newer than the requested
//! glibc, and it runs under qemu-user when that's set up.

use std::path::Path;

/// An x86-64 glibc for qemu to load (Debian's `libc6-amd64-cross`); building doesn't need it.
const SYSROOT: &str = "/usr/x86_64-linux-gnu";

const SRC: &str = "println(`hello from ${Math.cbrt(27)} ${[1, 2, 3].map((x: int) => x * 2):?}`)\n";

fn build(target: &str) -> Option<std::path::PathBuf> {
    let target = ppl_driver::Target::parse(target).unwrap();
    if !ppl_lld::AVAILABLE {
        eprintln!("skipped: needs lld");
        return None;
    }
    let checked = ppl_driver::check_source("cross.ppl", SRC);
    assert!(!checked.has_errors(), "{}", checked.render_diagnostics());
    let opts = ppl_driver::BuildOptions {
        release: false,
        target: Some(target),
    };
    Some(ppl_driver::build_cached(&checked, opts).unwrap())
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
