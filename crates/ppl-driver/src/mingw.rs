//! Windows (spec §15.1, decision 248): MinGW-w64 from Zig's vendored copy
//! (`third_party/zig-libc/mingw`), the way Zig builds it: `crt2.o` (the program entry),
//! `libmingw32` (the CRT support code and winpthreads, linked as needed) and import libraries
//! made from the `.def` files, for the UCRT and the system DLLs programs use.

use crate::link::{Clang, Job, cached_build};
use crate::target::{Arch, Target};
use std::path::{Path, PathBuf};

/// The import libraries every program links: the UCRT (Zig's `always_link_libs`), the system
/// DLLs it and winpthreads use, and the runtime's (sockets, randomness).
pub const LIBS: &[&str] = &[
    "api-ms-win-crt-conio-l1-1-0",
    "api-ms-win-crt-convert-l1-1-0",
    "api-ms-win-crt-environment-l1-1-0",
    "api-ms-win-crt-filesystem-l1-1-0",
    "api-ms-win-crt-heap-l1-1-0",
    "api-ms-win-crt-locale-l1-1-0",
    "api-ms-win-crt-math-l1-1-0",
    "api-ms-win-crt-multibyte-l1-1-0",
    "api-ms-win-crt-private-l1-1-0",
    "api-ms-win-crt-process-l1-1-0",
    "api-ms-win-crt-runtime-l1-1-0",
    "api-ms-win-crt-stdio-l1-1-0",
    "api-ms-win-crt-string-l1-1-0",
    "api-ms-win-crt-time-l1-1-0",
    "api-ms-win-crt-utility-l1-1-0",
    "advapi32",
    "kernel32",
    "ntdll",
    "shell32",
    "user32",
    "ws2_32",
    "bcrypt",
];

/// MinGW-w64 compiled for a target.
pub struct Mingw {
    /// `crt2.o`, linked first.
    pub start: PathBuf,
    /// libmingw32's objects, linked as needed.
    pub lib: Vec<PathBuf>,
    /// The import libraries.
    pub imports: Vec<PathBuf>,
}

fn dir() -> PathBuf {
    ppl_runtime::zig_libc_dir().join("mingw")
}

/// The Windows headers (mingw-w64's, as Zig ships them).
pub fn headers() -> PathBuf {
    ppl_runtime::zig_libc_dir().join("include/any-windows-any")
}

/// Zig's `addCcArgs`: what everything compiled against MinGW gets.
fn cc_args(t: Target, cc: &Clang) -> Vec<String> {
    vec![
        format!("--target={}", t.llvm_triple()),
        "-nostdinc".into(),
        "-isystem".into(),
        format!("{}/include", cc.resource_dir),
        "-isystem".into(),
        headers().display().to_string(),
        "-std=gnu11".into(),
        "-D__USE_MINGW_ANSI_STDIO=0".into(),
        "-O2".into(),
        "-ffunction-sections".into(),
        "-fdata-sections".into(),
        "-w".into(),
    ]
}

/// Zig's `addCrtCcArgs`: the CRT's own sources.
fn crt_args(t: Target, cc: &Clang) -> Vec<String> {
    let mut a = cc_args(t, cc);
    a.extend(
        [
            "-D__MSVCRT_VERSION__=0x700",
            "-D_CRTBLD",
            "-D_SYSCRT=1",
            "-D_WIN32_WINNT=0x0f00",
            "-DCRTDLL=1",
            "-DHAVE_CONFIG_H",
        ]
        .map(String::from),
    );
    a.push(format!("-I{}", dir().join("include").display()));
    a
}

/// What the runtime is compiled with for Windows: the UCRT and Windows 10 (spec §15.3).
pub fn runtime_args(cc: &Clang) -> Vec<String> {
    vec![
        "-isystem".into(),
        format!("{}/include", cc.resource_dir),
        "-isystem".into(),
        headers().display().to_string(),
        "-D__MSVCRT_VERSION__=0xE00".into(),
        "-D_WIN32_WINNT=0x0A00".into(),
    ]
}

/// The sections of `mingw/sources.txt` for an architecture.
fn sources(t: Target) -> Result<Vec<(String, PathBuf)>, String> {
    let manifest = dir().join("sources.txt");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("cannot read {}: {e}", manifest.display()))?;
    let wanted: &[&str] = match t.arch {
        Arch::X86_64 => &["generic", "x86", "winpthreads"],
        _ => &["generic", "arm", "arm64", "winpthreads"],
    };
    let mut section = String::new();
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s.to_string();
        } else if wanted.contains(&section.as_str()) && !line.starts_with("libsrc/") {
            // (libsrc/ defines COM interface GUIDs: unused, and they'd need most of the
            // Windows headers.)
            out.push((section.clone(), dir().join(line)));
        }
    }
    Ok(out)
}

/// MinGW for `t`, compiled on first use.
pub fn libc(t: Target, cc: &Clang) -> Result<Mingw, String> {
    let crt = crt_args(t, cc);
    let mut jobs = vec![Job {
        src: dir().join("crt/crtexe.c"),
        obj: "crt2.o".into(),
        args: crt.clone(),
        start: true,
    }];
    let mut pthread = cc_args(t, cc);
    pthread.extend([
        "-DIN_WINPTHREAD".into(),
        "-Wno-unknown-warning-option".into(),
    ]);
    for (section, src) in sources(t)? {
        let rel = src
            .strip_prefix(dir())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        jobs.push(Job {
            obj: format!("{}.o", rel.replace('/', "_")),
            src,
            args: if section == "winpthreads" {
                pthread.clone()
            } else {
                crt.clone()
            },
            start: false,
        });
    }
    let arch = match t.arch {
        Arch::X86_64 => "x64",
        _ => "arm64",
    };
    let built = cached_build("mingw", &format!("{arch}-crt"), &jobs, &[], |_| Ok(()))?;
    Ok(Mingw {
        start: built.join("crt2.o"),
        lib: jobs
            .iter()
            .filter(|j| !j.start)
            .map(|j| built.join(&j.obj))
            .collect(),
        imports: import_libs(t, cc)?,
    })
}

/// The `.def` for a DLL: the architecture's own (`lib64`, x86-64 only), else the common one,
/// else the common template (`.def.in`, preprocessed for the architecture).
fn find_def(t: Target, name: &str) -> Option<PathBuf> {
    let d = dir();
    let mut candidates = Vec::new();
    if t.arch == Arch::X86_64 {
        candidates.push(d.join(format!("lib64/{name}.def")));
    }
    candidates.push(d.join(format!("lib-common/{name}.def")));
    candidates.push(d.join(format!("lib-common/{name}.def.in")));
    candidates.into_iter().find(|p| p.is_file())
}

/// The import libraries in [`LIBS`], made on first use and cached.
fn import_libs(t: Target, cc: &Clang) -> Result<Vec<PathBuf>, String> {
    let defs: Vec<(&str, PathBuf)> = LIBS
        .iter()
        .map(|n| {
            find_def(t, n)
                .map(|p| (*n, p))
                .ok_or_else(|| format!("no .def file for {n}"))
        })
        .collect::<Result<_, _>>()?;
    let (arch, machine) = match t.arch {
        Arch::X86_64 => ("x64", ppl_lld::COFF_AMD64),
        _ => ("arm64", ppl_lld::COFF_ARM64),
    };
    let key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        crate::link::RUNTIME_CACHE_FORMAT.hash(&mut h);
        for (n, p) in &defs {
            n.hash(&mut h);
            std::fs::read(p).unwrap_or_default().hash(&mut h);
        }
        h.finish()
    };
    let out = std::env::temp_dir()
        .join("ppl-cache")
        .join("mingw")
        .join(format!("{arch}-imports-{key:016x}"));
    let libs: Vec<PathBuf> = defs
        .iter()
        .map(|(n, _)| out.join(format!("lib{n}.a")))
        .collect();
    if out.is_dir() {
        return Ok(libs);
    }
    let tmp = out.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    for (n, def) in &defs {
        let plain = tmp.join(format!("{n}.def"));
        preprocess_def(t, cc, def, &plain)?;
        ppl_lld::write_import_lib(&plain, machine, &tmp.join(format!("lib{n}.a")))
            .map_err(|e| format!("making the import library for {n} failed: {e}"))?;
    }
    if std::fs::rename(&tmp, &out).is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
        if !out.is_dir() {
            return Err(format!("cannot create {}", out.display()));
        }
    }
    Ok(libs)
}

/// Runs the C preprocessor over a `.def.in` (with `def-include/`), or copies a plain `.def`.
fn preprocess_def(t: Target, cc: &Clang, def: &Path, out: &Path) -> Result<(), String> {
    if def.extension().is_some_and(|x| x == "def") {
        return std::fs::copy(def, out)
            .map(|_| ())
            .map_err(|e| format!("cannot copy {}: {e}", def.display()));
    }
    let r = std::process::Command::new(&cc.exe)
        .arg(format!("--target={}", t.llvm_triple()))
        .args(["-E", "-P", "-x", "c"])
        .arg(format!("-I{}", dir().join("def-include").display()))
        .arg(def)
        .arg("-o")
        .arg(out)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", cc.exe.display()))?;
    if r.status.success() {
        Ok(())
    } else {
        Err(format!(
            "preprocessing {} failed:\n{}",
            def.display(),
            String::from_utf8_lossy(&r.stderr)
        ))
    }
}
