//! wasi-libc (P8.5a, decision 255): `wasm32-wasi` programs are linked with wasi-libc compiled
//! from the vendored sources (Zig 0.15.1's, `third_party/zig-libc/wasi`, with upstream musl's
//! shared files from `third_party/musl`) by clang, with the flags Zig's `wasi_libc.zig` uses,
//! cached like the runtime. Preview 1 core modules for now.

use crate::link::{Clang, Job, Libc, cached_build};
use crate::target::Target;
use std::path::{Path, PathBuf};

/// wasi-libc compiled for a target.
pub struct Wasi {
    /// `crt1-command.o`: linked first.
    pub start: PathBuf,
    /// libc's objects, linked as needed.
    pub lib: Vec<PathBuf>,
}

fn zig() -> PathBuf {
    ppl_runtime::zig_libc_dir()
}

/// clang's own headers, then the installed ones (Zig's `wasm-wasi-musl`, `generic-musl`), as
/// Zig orders them: wasi-libc's `<bits/alltypes.h>` takes `wchar_t` and friends from clang's
/// `<stddef.h>` (`__need_wchar_t`).
fn header_args(cc: &Clang) -> Vec<String> {
    let mut args = Vec::new();
    for d in [
        PathBuf::from(format!("{}/include", cc.resource_dir)),
        zig().join("include/wasm-wasi-musl"),
        zig().join("include/generic-musl"),
    ] {
        args.push("-isystem".into());
        args.push(d.display().to_string());
    }
    args
}

/// The runtime's compile arguments: the installed headers, single-threaded. Our own
/// `pthread.h` (`runtime/wasm`) stands in for the threads wasi-libc doesn't have.
pub fn runtime_libc(cc: &Clang) -> Libc {
    let mut args = vec![
        "-isystem".into(),
        ppl_runtime::include_dir()
            .join("../wasm")
            .display()
            .to_string(),
        "-mthread-model".into(),
        "single".into(),
    ];
    args.extend(header_args(cc));
    Libc {
        name: "wasi".into(),
        args,
        start: Vec::new(),
    }
}

/// The sections of `wasi/sources.txt`.
fn sources() -> Result<Vec<(String, String, PathBuf)>, String> {
    let manifest = zig().join("wasi/sources.txt");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("cannot read {}: {e}", manifest.display()))?;
    let third = ppl_runtime::musl_dir().join("..");
    let mut section = String::new();
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s.to_string();
        } else {
            let base = if line.starts_with("musl/") {
                third.clone()
            } else {
                zig()
            };
            out.push((section.clone(), line.to_string(), base.join(line)));
        }
    }
    Ok(out)
}

/// `-I` for each directory.
fn includes(dirs: &[PathBuf]) -> Vec<String> {
    dirs.iter().map(|d| format!("-I{}", d.display())).collect()
}

/// wasi-libc for `t`, compiled on first use.
pub fn libc(t: Target, cc: &Clang) -> Result<Wasi, String> {
    let w = zig().join("wasi");
    let musl = ppl_runtime::musl_dir();
    // `internal/version.h`, which musl's Makefile generates.
    let generated = version_header()?;
    let mut common: Vec<String> = vec![
        format!("--target={}", t.llvm_triple()),
        "-nostdinc".into(),
        "-std=gnu17".into(),
        "-fno-trapping-math".into(),
        "-w".into(),
        // (`long *` for `int *` in a few files: the same 32 bits on wasm32; clang 23 makes it
        // an error by default, Zig's clang a warning.)
        "-Wno-error=incompatible-pointer-types".into(),
        "-mthread-model".into(),
        "single".into(),
        "-DBULK_MEMORY_THRESHOLD=32".into(),
        "-ffunction-sections".into(),
        "-fdata-sections".into(),
    ];
    common.extend(header_args(cc));
    let bottom = includes(&[
        w.join("libc-bottom-half/headers/private"),
        w.join("libc-bottom-half/cloudlibc/src/include"),
        w.join("libc-bottom-half/cloudlibc/src"),
        w.join("libc-top-half/musl/src/include"),
        musl.join("src/include"),
        w.join("libc-top-half/musl/src/internal"),
        musl.join("src/internal"),
        generated.clone(),
    ]);
    let top = includes(&[
        w.join("libc-top-half/musl/src/include"),
        musl.join("src/include"),
        w.join("libc-top-half/musl/src/internal"),
        musl.join("src/internal"),
        w.join("libc-top-half/musl/arch/wasm32"),
        musl.join("arch/generic"),
        w.join("libc-top-half/headers/private"),
        generated,
    ]);
    let mut jobs = Vec::new();
    for (section, rel, src) in sources()? {
        let mut args = common.clone();
        match section.as_str() {
            "emmalloc" => args.extend(["-O3".into(), "-fno-strict-aliasing".into()]),
            "top" => {
                args.push("-O3".into());
                args.extend(top.iter().cloned());
            }
            _ => {
                args.push(if section == "crt" { "-Os" } else { "-O3" }.into());
                args.extend(bottom.iter().cloned());
            }
        }
        let name = rel.trim_end_matches(".c").replace('/', "_");
        jobs.push(Job {
            obj: if section == "crt" {
                "crt1-command.o".into()
            } else {
                format!("{section}_{name}.o")
            },
            src,
            args,
            start: section == "crt",
        });
    }
    let dir = cached_build("wasi", "wasm32-libc", &jobs, &[], |_| Ok(()))?;
    Ok(Wasi {
        start: dir.join("crt1-command.o"),
        lib: jobs
            .iter()
            .filter(|j| !j.start)
            .map(|j| dir.join(&j.obj))
            .collect(),
    })
}

/// A directory with musl's generated `version.h`.
fn version_header() -> Result<PathBuf, String> {
    let version = std::fs::read_to_string(ppl_runtime::musl_dir().join("VERSION"))
        .map_err(|e| format!("cannot read musl's VERSION: {e}"))?;
    let dir = std::env::temp_dir()
        .join("ppl-cache")
        .join("wasi")
        .join(format!("include-{}", version.trim()));
    let file = dir.join("version.h");
    if !file.is_file() {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        write_atomically(&file, &format!("#define VERSION \"{}\"\n", version.trim()))?;
    }
    Ok(dir)
}

fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}
