//! Linking through lld (spec §15.2), without a system linker: the C runtime is compiled for
//! the target by clang (once, cached), then lld links it with the program's object file.
//!
//! Only `linux-gnu` so far. Until ppl bundles glibc stubs, the C library comes from the
//! system: the host's (`/usr/lib/<multiarch>`) or a Debian cross package
//! (`/usr/<multiarch>`, e.g. `libc6-dev-amd64-cross`).

use crate::target::{Arch, Target};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// Bump to invalidate cached runtime builds made by older compilers.
const RUNTIME_CACHE_FORMAT: u32 = 1;

/// Links `obj` with the runtime into the executable `exe` for a `linux-gnu` target.
pub fn link_linux_gnu(t: Target, obj: &Path, exe: &Path, debug: bool) -> Result<(), String> {
    let libc = Glibc::find(t)?;
    let runtime = runtime_objects(t, &libc)?;
    let (emulation, loader) = match t.arch {
        Arch::X86_64 => ("elf_x86_64", "/lib64/ld-linux-x86-64.so.2"),
        Arch::Aarch64 => ("aarch64linux", "/lib/ld-linux-aarch64.so.1"),
        Arch::Wasm32 => unreachable!("wasm isn't linux"),
    };
    let lib = |name: &str| libc.lib.join(name).display().to_string();
    let mut args: Vec<String> = [
        "--hash-style=gnu",
        "--build-id",
        "--eh-frame-hdr",
        "-z",
        "relro",
        "-z",
        "now",
        "-m",
        emulation,
        "-pie",
        "-dynamic-linker",
        loader,
    ]
    .map(String::from)
    .into();
    if debug {
        // Panic traces name frames through the dynamic symbol table.
        args.push("-export-dynamic".into());
    }
    args.push("-o".into());
    args.push(exe.display().to_string());
    args.push(lib("Scrt1.o"));
    args.push(lib("crti.o"));
    args.push(obj.display().to_string());
    // The runtime's objects behave like an archive: only the ones the program needs.
    args.push("--start-lib".into());
    args.extend(runtime.iter().map(|o| o.display().to_string()));
    args.push("--end-lib".into());
    args.push(format!("-L{}", libc.lib.display()));
    args.push("-lm".into());
    args.push("-lc".into());
    args.push(lib("crtn.o"));
    ppl_lld::link_elf(&args).map_err(|e| format!("linking failed:\n{e}"))
}

/// Where a target's glibc lives on this machine.
struct Glibc {
    /// The directory with `Scrt1.o`, `libc.so`, …
    lib: PathBuf,
    /// Header directories, most specific first.
    include: Vec<PathBuf>,
}

impl Glibc {
    fn find(t: Target) -> Result<Glibc, String> {
        let m = t.multiarch();
        let mut candidates = Vec::new();
        if t.is_host() {
            candidates.push((
                PathBuf::from(format!("/usr/lib/{m}")),
                vec![
                    PathBuf::from(format!("/usr/include/{m}")),
                    "/usr/include".into(),
                ],
            ));
            for lib in ["/usr/lib64", "/usr/lib"] {
                candidates.push((lib.into(), vec!["/usr/include".into()]));
            }
        }
        candidates.push((
            PathBuf::from(format!("/usr/{m}/lib")),
            vec![PathBuf::from(format!("/usr/{m}/include"))],
        ));
        candidates
            .into_iter()
            .find(|(lib, inc)| {
                lib.join("Scrt1.o").is_file() && inc.iter().any(|i| i.join("stdio.h").is_file())
            })
            .map(|(lib, include)| Glibc { lib, include })
            .ok_or_else(|| {
                let deb = match t.arch {
                    Arch::X86_64 => "amd64",
                    _ => "arm64",
                };
                format!(
                    "no glibc for {m} to link against (ppl doesn't bundle glibc stubs yet): \
                     install it, e.g. `libc6-dev-{deb}-cross` on Debian and Ubuntu"
                )
            })
    }
}

/// clang from the LLVM installation ppl was built with (or the one on `PATH`), its resource
/// directory (for `stddef.h` and friends) and version.
struct Clang {
    exe: PathBuf,
    resource_dir: String,
    version: String,
}

fn clang() -> Result<&'static Clang, String> {
    static CLANG: OnceLock<Result<Clang, String>> = OnceLock::new();
    CLANG
        .get_or_init(|| {
            let exe = option_env!("LLVM_SYS_231_PREFIX")
                .map(|p| Path::new(p).join("bin/clang"))
                .filter(|p| p.is_file())
                .unwrap_or_else(|| "clang".into());
            let ask = |arg: &str| -> Result<String, String> {
                let out = Command::new(&exe)
                    .arg(arg)
                    .output()
                    .map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
                Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
            };
            Ok(Clang {
                resource_dir: ask("-print-resource-dir")?,
                version: ask("--version")?,
                exe,
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The runtime (and Mbed TLS) compiled for `t`: object files in a cache directory keyed by
/// the sources, the compiler and the flags.
fn runtime_objects(t: Target, libc: &Glibc) -> Result<Vec<PathBuf>, String> {
    // Threads of one process (the golden tests) wait for one build; other processes race
    // harmlessly, each renaming a finished directory into place.
    static BUILT: Mutex<Vec<(Target, Vec<PathBuf>)>> = Mutex::new(Vec::new());
    let mut built = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, objs)) = built.iter().find(|(b, _)| *b == t) {
        return Ok(objs.clone());
    }
    let cc = clang()?;
    let jobs = compile_jobs(t, libc, cc)?;
    let dir = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        RUNTIME_CACHE_FORMAT.hash(&mut h);
        cc.exe.hash(&mut h);
        cc.version.hash(&mut h);
        for j in &jobs {
            j.args.hash(&mut h);
            std::fs::read(&j.src)
                .map_err(|e| format!("cannot read {}: {e}", j.src.display()))?
                .hash(&mut h);
        }
        // Headers of the runtime (Mbed TLS's are vendored and never change on their own).
        let inc = ppl_runtime::include_dir();
        for e in std::fs::read_dir(&inc)
            .map_err(|e| e.to_string())?
            .flatten()
        {
            std::fs::read(e.path()).unwrap_or_default().hash(&mut h);
        }
        std::env::temp_dir()
            .join("ppl-cache")
            .join("runtime")
            .join(format!("{}-{:016x}", t.llvm_triple(), h.finish()))
    };
    let objects: Vec<PathBuf> = jobs.iter().map(|j| dir.join(&j.obj)).collect();
    if dir.is_dir() {
        built.push((t, objects.clone()));
        return Ok(objects);
    }
    let tmp = dir.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    compile_all(&cc.exe, &jobs, &tmp)?;
    if std::fs::rename(&tmp, &dir).is_err() {
        // Another process got there first.
        let _ = std::fs::remove_dir_all(&tmp);
        if !dir.is_dir() {
            return Err(format!("cannot create {}", dir.display()));
        }
    }
    built.push((t, objects.clone()));
    Ok(objects)
}

struct Job {
    src: PathBuf,
    obj: String,
    args: Vec<String>,
}

fn compile_jobs(t: Target, libc: &Glibc, cc: &Clang) -> Result<Vec<Job>, String> {
    let mut common: Vec<String> = vec![
        format!("--target={}", t.llvm_triple()),
        "-nostdinc".into(),
        "-isystem".into(),
        format!("{}/include", cc.resource_dir),
    ];
    for i in &libc.include {
        common.push("-isystem".into());
        common.push(i.display().to_string());
    }
    common.extend(
        [
            "-O2",
            "-g",
            "-fPIC",
            "-fno-omit-frame-pointer",
            "-ffunction-sections",
            "-fdata-sections",
            "-w",
        ]
        .map(String::from),
    );
    if t.arch == Arch::Aarch64 {
        // Inline LL/SC atomics, not calls into libgcc's helpers (nothing provides them).
        common.push("-mno-outline-atomics".into());
    }
    let mbed = ppl_runtime::mbedtls_dir();
    for d in [
        ppl_runtime::include_dir(),
        mbed.join("include"),
        mbed.join("library"),
    ] {
        common.push(format!("-I{}", d.display()));
    }
    common.push("-DMBEDTLS_USER_CONFIG_FILE=\"ppl_mbedtls_config.h\"".into());
    let mut jobs = Vec::new();
    for (dir, std, prefix) in [
        (ppl_runtime::source_dir(), "-std=c11", ""),
        (mbed.join("library"), "-std=c99", "mbedtls_"),
    ] {
        let mut srcs: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "c"))
            .collect();
        srcs.sort();
        for src in srcs {
            let stem = src.file_stem().unwrap().to_string_lossy();
            let mut args = common.clone();
            args.push(std.into());
            jobs.push(Job {
                obj: format!("{prefix}{stem}.o"),
                src,
                args,
            });
        }
    }
    Ok(jobs)
}

/// Runs the jobs on as many threads as there are CPUs.
fn compile_all(clang: &Path, jobs: &[Job], out: &Path) -> Result<(), String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let errors = Mutex::new(Vec::new());
    let n = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    std::thread::scope(|s| {
        for _ in 0..n {
            s.spawn(|| {
                while let Some(j) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let r = Command::new(clang)
                        .args(&j.args)
                        .arg("-c")
                        .arg(&j.src)
                        .arg("-o")
                        .arg(out.join(&j.obj))
                        .output();
                    let failed = match r {
                        Ok(o) if o.status.success() => continue,
                        Ok(o) => String::from_utf8_lossy(&o.stderr).into_owned(),
                        Err(e) => format!("cannot run {}: {e}", clang.display()),
                    };
                    errors.lock().unwrap().push(format!(
                        "compiling the runtime ({}) failed:\n{failed}",
                        j.src.display()
                    ));
                }
            });
        }
    });
    let errors = errors.into_inner().unwrap();
    match errors.first() {
        None => Ok(()),
        Some(e) => Err(e.clone()),
    }
}
