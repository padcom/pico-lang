//! musl (spec §15.1, decision 245): `*-linux-musl` programs are linked statically with musl
//! compiled from the vendored sources (`third_party/musl`) by clang, the way musl's Makefile
//! builds `libc.a`, cached like the runtime.

use crate::link::{Clang, Job, Libc, cached_build};
use crate::target::{Arch, Os, Target};
use std::path::{Path, PathBuf};

/// musl compiled for a target.
pub struct Musl {
    /// `crt1.o`, `crti.o`: linked first.
    pub start: Vec<PathBuf>,
    /// `crtn.o`: linked last.
    pub end: Vec<PathBuf>,
    /// libc's objects, linked as needed.
    pub lib: Vec<PathBuf>,
    /// The generated headers (`bits/alltypes.h`, `bits/syscall.h`).
    include: PathBuf,
}

fn arch(t: Target) -> &'static str {
    match t.arch {
        Arch::X86_64 => "x86_64",
        _ => "aarch64",
    }
}

/// musl's headers as installed, for compiling programs (the runtime) against it: generated,
/// architecture-specific, generic; then the compiler's own (`stdatomic.h`, intrinsics).
pub fn runtime_libc(t: Target, cc: &Clang, m: &Musl) -> Libc {
    let src = ppl_runtime::musl_dir();
    let mut args = Vec::new();
    for d in [
        m.include.clone(),
        src.join("arch").join(arch(t)),
        src.join("arch/generic"),
        src.join("include"),
        PathBuf::from(format!("{}/include", cc.resource_dir)),
    ] {
        args.push("-isystem".into());
        args.push(d.display().to_string());
    }
    Libc {
        name: "musl".into(),
        args,
        start: Vec::new(),
    }
}

/// musl for `t`, compiled on first use.
pub fn libc(t: Target, cc: &Clang) -> Result<Musl, String> {
    let src = ppl_runtime::musl_dir();
    let a = arch(t);
    let include = headers(t)?;
    let mut common: Vec<String> = [
        format!("--target={}", t.llvm_triple()),
        "-std=c99".into(),
        "-nostdinc".into(),
        "-ffreestanding".into(),
        "-fexcess-precision=standard".into(),
        "-frounding-math".into(),
        "-ffp-contract=off".into(),
        "-fno-strict-aliasing".into(),
        "-fno-stack-protector".into(),
        "-ffunction-sections".into(),
        "-fdata-sections".into(),
        "-Wa,--noexecstack".into(),
        "-D_XOPEN_SOURCE=700".into(),
        "-w".into(),
    ]
    .into();
    for d in [
        src.join("arch").join(a),
        src.join("arch/generic"),
        include.join("internal"),
        src.join("src/include"),
        src.join("src/internal"),
        include.clone(),
        src.join("include"),
    ] {
        common.push(format!("-I{}", d.display()));
    }
    let _ = cc;
    let mut jobs = Vec::new();
    for file in sources(&src, a)? {
        let rel = file
            .strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut args = common.clone();
        // musl's Makefile optimizes these for speed, everything else for size.
        let hot = ["src/internal/", "src/malloc/", "src/string/"]
            .iter()
            .any(|p| rel.starts_with(p));
        args.push(if hot { "-O3" } else { "-Os" }.into());
        jobs.push(Job {
            obj: format!("{}.o", rel.replace('/', "_")),
            src: file,
            args,
            start: false,
        });
    }
    let crt = |name: &str, file: PathBuf| {
        let mut args = common.clone();
        args.extend(["-Os".into(), "-DCRT".into()]);
        Job {
            obj: format!("{name}.o"),
            src: file,
            args,
            start: true,
        }
    };
    jobs.push(crt("crt1", src.join("crt/crt1.c")));
    jobs.push(crt("crti", src.join(format!("crt/{a}/crti.s"))));
    jobs.push(crt("crtn", src.join(format!("crt/{a}/crtn.s"))));
    let dir = cached_build("musl", &format!("{a}-lib"), &jobs, &[], |_| Ok(()))?;
    Ok(Musl {
        start: vec![dir.join("crt1.o"), dir.join("crti.o")],
        end: vec![dir.join("crtn.o")],
        lib: jobs
            .iter()
            .filter(|j| !j.start)
            .map(|j| dir.join(&j.obj))
            .collect(),
        include,
    })
}

/// The libc sources for an architecture, as musl's Makefile picks them: every `src/*/*.c`
/// (and the default malloc, mallocng), where a `src/*/<arch>/` file of the same name (`.c`,
/// `.s` or `.S`) replaces the generic one.
fn sources(src: &Path, arch: &str) -> Result<Vec<PathBuf>, String> {
    let list = |d: &Path| -> Result<Vec<PathBuf>, String> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(d)
            .map_err(|e| format!("cannot read {}: {e}", d.display()))?
            .flatten()
            .map(|e| e.path())
            .collect();
        v.sort();
        Ok(v)
    };
    let mut dirs: Vec<PathBuf> = list(&src.join("src"))?
        .into_iter()
        .filter(|p| p.is_dir())
        .collect();
    dirs.push(src.join("src/malloc/mallocng"));
    let mut out = Vec::new();
    for d in dirs {
        let arch_dir = d.join(arch);
        let arch_files: Vec<PathBuf> = if arch_dir.is_dir() {
            list(&arch_dir)?
                .into_iter()
                .filter(|p| {
                    p.extension()
                        .is_some_and(|x| x == "c" || x == "s" || x == "S")
                })
                .collect()
        } else {
            Vec::new()
        };
        let replaced = |p: &Path| arch_files.iter().any(|a| a.file_stem() == p.file_stem());
        for f in list(&d)? {
            if f.extension().is_some_and(|x| x == "c") && !replaced(&f) {
                out.push(f);
            }
        }
        out.extend(arch_files);
    }
    Ok(out)
}

/// musl's generated headers, as its Makefile makes them: `bits/alltypes.h` (from the
/// `alltypes.h.in` files through `tools/mkalltypes.sed`), `bits/syscall.h` (the `__NR_`
/// numbers again as `SYS_`) and `internal/version.h`.
fn headers(t: Target) -> Result<PathBuf, String> {
    let src = ppl_runtime::musl_dir();
    let a = arch(t);
    let read = |p: PathBuf| {
        std::fs::read_to_string(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))
    };
    let alltypes_in = read(src.join(format!("arch/{a}/bits/alltypes.h.in")))?
        + &read(src.join("include/alltypes.h.in"))?;
    let syscall_in = read(src.join(format!("arch/{a}/bits/syscall.h.in")))?;
    let version = read(src.join("VERSION"))?;
    let mut alltypes = mkalltypes(&alltypes_in);
    let windows = t.os == Os::Windows;
    if windows {
        // Windows is LLP64: `long` has 32 bits.
        for name in ["_Addr", "_Int64", "_Reg"] {
            let lp64 = format!("#define {name} long\n");
            assert!(alltypes.contains(&lp64), "{name} in alltypes.h");
            alltypes = alltypes.replace(&lp64, &format!("#define {name} long long\n"));
        }
    }
    let mut syscall = syscall_in.clone();
    for l in syscall_in.lines().filter(|l| l.contains("__NR_")) {
        syscall.push_str(&l.replacen("__NR_", "SYS_", 1));
        syscall.push('\n');
    }
    let version_h = format!("#define VERSION \"{}\"\n", version.trim());
    let mut files = vec![
        ("bits/alltypes.h", alltypes),
        ("bits/syscall.h", syscall),
        ("internal/version.h", version_h),
    ];
    if windows && t.arch == Arch::Aarch64 {
        // arm64 Windows' `long double` is `double` (musl's aarch64 port has 128 bits).
        files.push(("bits/float.h", WINDOWS_ARM64_FLOAT_H.into()));
    }
    // Keyed by the content: a directory per distinct set of headers.
    let key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        files.hash(&mut h);
        h.finish()
    };
    let dir = std::env::temp_dir()
        .join("ppl-cache")
        .join("musl")
        .join(format!("{a}-include-{key:016x}"));
    if dir.is_dir() {
        return Ok(dir);
    }
    let tmp = dir.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    for (name, text) in &files {
        let p = tmp.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(&p, text).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
    }
    if std::fs::rename(&tmp, &dir).is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
        if !dir.is_dir() {
            return Err(format!("cannot create {}", dir.display()));
        }
    }
    Ok(dir)
}

/// `bits/float.h` for a 64-bit `long double`.
const WINDOWS_ARM64_FLOAT_H: &str = "#define FLT_EVAL_METHOD 0

#define LDBL_TRUE_MIN 4.94065645841246544177e-324L
#define LDBL_MIN 2.22507385850720138309e-308L
#define LDBL_MAX 1.79769313486231570815e+308L
#define LDBL_EPSILON 2.22044604925031308085e-16L

#define LDBL_MANT_DIG 53
#define LDBL_MIN_EXP (-1021)
#define LDBL_MAX_EXP 1024

#define LDBL_DIG 15
#define LDBL_MIN_10_EXP (-307)
#define LDBL_MAX_10_EXP 308

#define DECIMAL_DIG 17
";

/// musl's double-precision math (`src/math`) for Windows targets (decision 249), so `Math`
/// gives the same results as on Linux: the UCRT's functions are up to an ulp off (x64's
/// `sin(π/2)` is `1 + 2⁻⁵²`). Linked before the UCRT, so these definitions win. Left out: the
/// `float` and `long double` variants (`sinf`, `sinl`, … where the double function exists:
/// arm64 Windows' `long double` isn't musl's) and the ELF-only assembly files.
pub fn libm(t: Target) -> Result<Vec<PathBuf>, String> {
    let src = ppl_runtime::musl_dir();
    let a = arch(t);
    let include = headers(t)?;
    let mut common: Vec<String> = [
        format!("--target={}", t.llvm_triple()),
        "-std=c99".into(),
        "-nostdinc".into(),
        "-ffreestanding".into(),
        "-fexcess-precision=standard".into(),
        "-frounding-math".into(),
        "-ffp-contract=off".into(),
        "-fno-strict-aliasing".into(),
        "-fno-stack-protector".into(),
        "-fno-builtin".into(),
        "-ffunction-sections".into(),
        "-fdata-sections".into(),
        "-D_XOPEN_SOURCE=700".into(),
        "-Os".into(),
        "-w".into(),
    ]
    .into();
    // The generated headers first: they replace `bits/float.h` on arm64.
    for d in [
        include.clone(),
        src.join("arch").join(a),
        src.join("arch/generic"),
        include.join("internal"),
        src.join("src/include"),
        src.join("src/internal"),
        src.join("include"),
    ] {
        common.push(format!("-I{}", d.display()));
    }
    let dir = src.join("src/math");
    let mut jobs = Vec::new();
    for file in sources(&src, a)? {
        if file.parent() != Some(dir.as_path()) && file.parent() != Some(dir.join(a).as_path()) {
            continue;
        }
        let stem = file.file_stem().unwrap().to_string_lossy().into_owned();
        let variant = (stem.ends_with('f') || stem.ends_with('l'))
            && dir.join(format!("{}.c", &stem[..stem.len() - 1])).is_file();
        if variant || file.extension().is_some_and(|x| x != "c") {
            continue;
        }
        let rel = file
            .strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        jobs.push(Job {
            obj: format!("{}.o", rel.replace('/', "_")),
            src: file,
            args: common.clone(),
            start: false,
        });
    }
    let built = cached_build("musl", &format!("{a}-windows-libm"), &jobs, &[], |_| Ok(()))?;
    Ok(jobs.iter().map(|j| built.join(&j.obj)).collect())
}

/// `tools/mkalltypes.sed`: `TYPEDEF t name;`, `STRUCT name body;` and `UNION name body;`
/// lines become definitions guarded by `__NEED_name` / `__DEFINED_name`.
fn mkalltypes(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let guarded = |key: &str, def: String| {
            format!(
                "#if defined(__NEED_{key}) && !defined(__DEFINED_{key})\n{def}\n#define __DEFINED_{key}\n#endif\n"
            )
        };
        let rule = if let Some(rest) = line.strip_prefix("TYPEDEF ") {
            rest.strip_suffix(';').and_then(|r| {
                let (ty, name) = r.rsplit_once(' ')?;
                Some(guarded(name, format!("typedef {ty} {name};")))
            })
        } else if let Some(rest) = line.strip_prefix("STRUCT ").or(line.strip_prefix("UNION ")) {
            let kind = if line.starts_with("STRUCT") {
                "struct"
            } else {
                "union"
            };
            rest.trim_start().strip_suffix(';').and_then(|r| {
                let (name, body) = r.split_once(' ')?;
                Some(guarded(
                    &format!("{kind}_{name}"),
                    format!("{kind} {name} {body};"),
                ))
            })
        } else {
            None
        };
        match rule {
            Some(text) => out.push_str(&text),
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mkalltypes_matches_the_sed_script() {
        let out = mkalltypes(
            "#define _Addr long\nTYPEDEF unsigned _Addr size_t;\nSTRUCT iovec { void *iov_base; size_t iov_len; };\n",
        );
        assert_eq!(
            out,
            "#define _Addr long\n\
             #if defined(__NEED_size_t) && !defined(__DEFINED_size_t)\n\
             typedef unsigned _Addr size_t;\n#define __DEFINED_size_t\n#endif\n\
             #if defined(__NEED_struct_iovec) && !defined(__DEFINED_struct_iovec)\n\
             struct iovec { void *iov_base; size_t iov_len; };\n#define __DEFINED_struct_iovec\n#endif\n"
        );
    }

    #[test]
    fn arch_files_replace_generic_ones() {
        let src = ppl_runtime::musl_dir();
        let files = sources(&src, "x86_64").unwrap();
        let has = |p: &str| files.iter().any(|f| f.ends_with(p));
        assert!(has("src/string/x86_64/memcpy.s"));
        assert!(!has("src/string/memcpy.c"));
        assert!(has("src/string/strlen.c"));
        assert!(has("src/malloc/mallocng/malloc.c"));
        assert!(!has("src/malloc/oldmalloc/malloc.c"));
        assert!(!has("src/string/aarch64/memcpy.S"));
    }
}
