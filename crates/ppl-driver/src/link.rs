//! Linking through lld (spec §15.2), without a system linker: the C runtime is compiled for
//! the target by clang (once, cached), then lld links it with the program's object file.
//!
//! Linux only so far. `linux-gnu` links dynamically against generated glibc stubs
//! ([`crate::glibc`]), compiled with Zig's multi-version glibc headers and our own start files
//! (`runtime/glibc`); `linux-musl` links statically with musl compiled from source
//! ([`crate::musl`]). Neither needs a C library installed.

use crate::target::{Arch, Env, Target};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// Bump to invalidate cached runtime builds made by older compilers.
pub(crate) const RUNTIME_CACHE_FORMAT: u32 = 3;

/// lld's `-m` for an architecture.
pub(crate) fn emulation(a: Arch) -> &'static str {
    match a {
        Arch::X86_64 => "elf_x86_64",
        Arch::Aarch64 => "aarch64linux",
        Arch::Wasm32 => unreachable!("wasm isn't linux"),
    }
}

/// The oldest macOS programs run on (spec §15.3) and the SDK the stubs are from
/// (`third_party/zig-libc/darwin/SDKSettings.json`).
const MACOS_MIN: &str = "13.0";
const MACOS_SDK: &str = "15.5";

/// The runtime's compile arguments for macOS: Zig's macOS headers, then clang's.
fn darwin_libc(cc: &Clang) -> Libc {
    let mut args = vec![format!("-mmacos-version-min={MACOS_MIN}")];
    for d in [
        ppl_runtime::zig_libc_dir().join("include/any-macos-any"),
        PathBuf::from(format!("{}/include", cc.resource_dir)),
    ] {
        args.push("-isystem".into());
        args.push(d.display().to_string());
    }
    Libc {
        name: "darwin".into(),
        args,
        start: Vec::new(),
    }
}

/// Links `obj` with the runtime into the executable `exe` for macOS, against `libSystem`'s
/// `.tbd` stubs (no SDK needed). dyld calls `main` (`LC_MAIN`), so there are no start files;
/// `libSystem` re-exports compiler-rt. arm64 executables get lld's ad-hoc signature.
pub fn link_macos(t: Target, obj: &Path, exe: &Path, debug: bool) -> Result<(), String> {
    let cc = clang()?;
    let runtime = runtime_objects(t, &darwin_libc(cc))?;
    let arch = match t.arch {
        Arch::X86_64 => "x86_64",
        _ => "arm64",
    };
    let mut args: Vec<String> = vec![
        "-arch".into(),
        arch.into(),
        "-platform_version".into(),
        "macos".into(),
        MACOS_MIN.into(),
        MACOS_SDK.into(),
        "-dead_strip".into(),
        "-o".into(),
        exe.display().to_string(),
    ];
    if debug {
        // Panic traces name frames through the dynamic symbol table.
        args.push("-export_dynamic".into());
    }
    args.push(obj.display().to_string());
    args.push("--start-lib".into());
    args.extend(paths(&runtime.lazy));
    args.push("--end-lib".into());
    args.push(
        ppl_runtime::zig_libc_dir()
            .join("darwin/libSystem.tbd")
            .display()
            .to_string(),
    );
    ppl_lld::link_macho(&args).map_err(|e| format!("linking failed:\n{e}"))
}

/// Links `obj` with the runtime into the module `exe` for `wasm32-wasi` (P8.5a): wasi-libc's
/// start file (it calls `main`), then the program, the runtime, wasi-libc and compiler-rt as
/// needed. An 8 MiB stack: wasm's default 64 KiB is too little for deep recursion.
pub fn link_wasi(t: Target, obj: &Path, exe: &Path) -> Result<(), String> {
    let cc = clang()?;
    let wasi = crate::wasi::libc(t, cc)?;
    let runtime = runtime_objects(t, &crate::wasi::runtime_libc(cc))?;
    let builtins = crate::builtins::objects(t, cc)?;
    let mut args: Vec<String> = vec![
        "-z".into(),
        "stack-size=8388608".into(),
        "--gc-sections".into(),
        "-o".into(),
        exe.display().to_string(),
        wasi.start.display().to_string(),
        obj.display().to_string(),
    ];
    for group in [&runtime.lazy, &wasi.lib, &builtins] {
        args.push("--start-lib".into());
        args.extend(paths(group));
        args.push("--end-lib".into());
    }
    ppl_lld::link_wasm(&args).map_err(|e| format!("linking failed:\n{e}"))?;
    crate::wasi::componentize(exe)
}

/// Links `obj` with the runtime into the executable `exe` for a Linux target.
pub fn link_linux(t: Target, obj: &Path, exe: &Path, debug: bool) -> Result<(), String> {
    match t.env {
        Env::Musl => link_musl(t, obj, exe),
        _ => link_gnu(t, obj, exe, debug),
    }
}

fn link_gnu(t: Target, obj: &Path, exe: &Path, debug: bool) -> Result<(), String> {
    let cc = clang()?;
    let runtime = runtime_objects(t, &gnu_libc(t, cc)?)?;
    let stubs = crate::glibc::stubs(t, &cc.exe)?;
    let loader = match t.arch {
        Arch::X86_64 => "/lib64/ld-linux-x86-64.so.2",
        _ => "/lib/ld-linux-aarch64.so.1",
    };
    let mut args = common_args(t);
    args.extend(["-pie", "-dynamic-linker", loader].map(String::from));
    if debug {
        // Panic traces name frames through the dynamic symbol table.
        args.push("-export-dynamic".into());
    }
    args.push("-o".into());
    args.push(exe.display().to_string());
    args.extend(paths(&runtime.start));
    args.push(obj.display().to_string());
    lazy(&mut args, &runtime.lazy);
    lazy(&mut args, &crate::builtins::objects(t, cc)?);
    // Only the libraries something refers to become dependencies (libdl, libutil, …).
    args.push("--as-needed".into());
    args.extend(paths(&stubs));
    ppl_lld::link_elf(&args).map_err(|e| format!("linking failed:\n{e}"))
}

/// musl targets are fully static (spec §15.1): musl's `libc.a` objects are linked in.
fn link_musl(t: Target, obj: &Path, exe: &Path) -> Result<(), String> {
    let cc = clang()?;
    let musl = crate::musl::libc(t, cc)?;
    let runtime = runtime_objects(t, &crate::musl::runtime_libc(t, cc, &musl))?;
    let mut args = common_args(t);
    args.extend(["-static", "-o"].map(String::from));
    args.push(exe.display().to_string());
    args.extend(paths(&musl.start));
    args.push(obj.display().to_string());
    lazy(&mut args, &runtime.lazy);
    lazy(&mut args, &musl.lib);
    lazy(&mut args, &crate::builtins::objects(t, cc)?);
    args.extend(paths(&musl.end));
    ppl_lld::link_elf(&args).map_err(|e| format!("linking failed:\n{e}"))
}

/// Windows (MinGW): lld's COFF linker in MinGW mode, MinGW's CRT and import libraries.
pub fn link_windows(t: Target, obj: &Path, exe: &Path) -> Result<(), String> {
    let cc = clang()?;
    let mingw = crate::mingw::libc(t, cc)?;
    let libc = Libc {
        name: "mingw".into(),
        args: crate::mingw::runtime_args(cc),
        start: Vec::new(),
    };
    let runtime = runtime_objects(t, &libc)?;
    let builtins = crate::builtins::objects(t, cc)?;
    let libm = crate::musl::libm(t)?;
    let machine = match t.arch {
        Arch::X86_64 => "x64",
        _ => "arm64",
    };
    let mut args: Vec<String> = [
        "-lldmingw",
        "-errorlimit:0",
        "-nologo",
        "-subsystem:console",
        "-dynamicbase",
        "-nxcompat",
        "-alternatename:__image_base__=__ImageBase",
    ]
    .map(String::from)
    .into();
    args.push(format!("-machine:{machine}"));
    if t.arch == Arch::X86_64 {
        args.push("-highentropyva".into());
    }
    args.push(format!("-out:{}", exe.display()));
    args.push(mingw.start.display().to_string());
    args.push(obj.display().to_string());
    // musl's math before MinGW's and the UCRT's (`crate::musl::libm`).
    for group in [&runtime.lazy, &libm, &mingw.lib, &builtins] {
        args.push("-start-lib".into());
        args.extend(paths(group));
        args.push("-end-lib".into());
    }
    args.extend(paths(&mingw.imports));
    ppl_lld::link_coff(&args).map_err(|e| format!("linking failed:\n{e}"))
}

fn common_args(t: Target) -> Vec<String> {
    [
        "--build-id",
        "--eh-frame-hdr",
        "-z",
        "relro",
        "-z",
        "now",
        "-z",
        "noexecstack",
        "-m",
        emulation(t.arch),
    ]
    .map(String::from)
    .into()
}

fn paths(ps: &[PathBuf]) -> impl Iterator<Item = String> + '_ {
    ps.iter().map(|p| p.display().to_string())
}

/// Objects that behave like an archive: only the ones the program needs are linked.
fn lazy(args: &mut Vec<String>, objs: &[PathBuf]) {
    args.push("--start-lib".into());
    args.extend(paths(objs));
    args.push("--end-lib".into());
}

/// How the runtime is compiled against a C library.
pub(crate) struct Libc {
    /// Name of the libc build in cache keys.
    pub name: String,
    /// Compiler arguments: header directories and defines.
    pub args: Vec<String>,
    /// Start files to compile with the runtime (glibc's are ours), with an extra flag.
    pub start: Vec<(PathBuf, Option<&'static str>)>,
}

/// glibc: Zig's multi-version headers (see `third_party/zig-libc/README.md`) for the target's
/// version, and our start files.
fn gnu_libc(t: Target, cc: &Clang) -> Result<Libc, String> {
    let inc = ppl_runtime::zig_libc_dir().join("include");
    let arch = match t.arch {
        Arch::X86_64 => "x86",
        _ => "aarch64",
    };
    let mut args = vec!["-isystem".into(), format!("{}/include", cc.resource_dir)];
    for d in [
        format!("{arch}-linux-gnu"),
        "generic-glibc".into(),
        format!("{arch}-linux-any"),
        "any-linux-any".into(),
    ] {
        args.push("-isystem".into());
        args.push(inc.join(d).display().to_string());
    }
    let (major, minor) = t.glibc.expect("a gnu target");
    if major != 2 {
        return Err(format!("glibc {major}.{minor} doesn't exist"));
    }
    // Zig's headers declare what this glibc version has.
    args.push(format!("-D__GLIBC_MINOR__={minor}"));
    let crt = ppl_runtime::source_dir().join("../glibc");
    let start = match t.arch {
        Arch::X86_64 => "start_x86_64.S",
        _ => "start_aarch64.S",
    };
    Ok(Libc {
        name: format!("glibc{major}.{minor}"),
        args,
        start: vec![
            (crt.join(start), None),
            (crt.join("crt.c"), Some("-std=c11")),
        ],
    })
}

/// clang from the LLVM installation ppl was built with (or the one on `PATH`), its resource
/// directory (for `stddef.h` and friends) and version.
pub(crate) struct Clang {
    pub exe: PathBuf,
    pub resource_dir: String,
    version: String,
}

pub(crate) fn clang() -> Result<&'static Clang, String> {
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

/// The runtime compiled for a target.
#[derive(Clone)]
struct RuntimeObjects {
    /// Linked as they are: the start files.
    start: Vec<PathBuf>,
    /// Linked as needed: the runtime and Mbed TLS.
    lazy: Vec<PathBuf>,
}

/// The runtime (and Mbed TLS) compiled for `t` against `libc`.
fn runtime_objects(t: Target, libc: &Libc) -> Result<RuntimeObjects, String> {
    // Threads of one process (the golden tests) wait for one build.
    static BUILT: Mutex<Vec<(Target, RuntimeObjects)>> = Mutex::new(Vec::new());
    let mut built = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, objs)) = built.iter().find(|(b, _)| *b == t) {
        return Ok(objs.clone());
    }
    let jobs = runtime_jobs(t, libc)?;
    let name = format!("{}-{}", t.llvm_triple(), libc.name);
    // The runtime's headers too (Mbed TLS's are vendored and never change on their own).
    let mut headers: Vec<PathBuf> = std::fs::read_dir(ppl_runtime::include_dir())
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .collect();
    let tz = ppl_runtime::tzdata_dir();
    headers.extend([tz.join("tzdata.bin"), tz.join("windows_zones.inc")]);
    headers.push(ppl_runtime::include_dir().join("../wasi/imports.h"));
    let dir = cached_build("runtime", &name, &jobs, &headers, |_| Ok(()))?;
    let pick = |start: bool| {
        jobs.iter()
            .filter(|j| j.start == start)
            .map(|j| dir.join(&j.obj))
            .collect()
    };
    let objects = RuntimeObjects {
        start: pick(true),
        lazy: pick(false),
    };
    built.push((t, objects.clone()));
    Ok(objects)
}

/// Compiles `jobs` into a cache directory `$TMPDIR/ppl-cache/<kind>/<name>-<hash>`, keyed by
/// the compiler, the arguments, the sources and `inputs`, unless it exists. `prepare` fills
/// the directory first (generated headers). Processes race harmlessly: each builds in a
/// directory of its own and renames it into place.
pub(crate) fn cached_build(
    kind: &str,
    name: &str,
    jobs: &[Job],
    inputs: &[PathBuf],
    prepare: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let cc = clang()?;
    let dir = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        RUNTIME_CACHE_FORMAT.hash(&mut h);
        cc.exe.hash(&mut h);
        cc.version.hash(&mut h);
        for j in jobs {
            j.args.hash(&mut h);
            j.obj.hash(&mut h);
            std::fs::read(&j.src)
                .map_err(|e| format!("cannot read {}: {e}", j.src.display()))?
                .hash(&mut h);
        }
        for p in inputs {
            std::fs::read(p).unwrap_or_default().hash(&mut h);
        }
        std::env::temp_dir()
            .join("ppl-cache")
            .join(kind)
            .join(format!("{name}-{:016x}", h.finish()))
    };
    if dir.is_dir() {
        return Ok(dir);
    }
    let tmp = dir.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    prepare(&tmp)?;
    compile_all(&cc.exe, kind, jobs, &tmp)?;
    if std::fs::rename(&tmp, &dir).is_err() {
        // Another process got there first.
        let _ = std::fs::remove_dir_all(&tmp);
        if !dir.is_dir() {
            return Err(format!("cannot create {}", dir.display()));
        }
    }
    Ok(dir)
}

pub(crate) struct Job {
    pub src: PathBuf,
    /// The object's file name in the build directory.
    pub obj: String,
    pub args: Vec<String>,
    /// A start file (always linked).
    pub start: bool,
}

fn runtime_jobs(t: Target, libc: &Libc) -> Result<Vec<Job>, String> {
    let mut common: Vec<String> =
        vec![format!("--target={}", t.clang_triple()), "-nostdinc".into()];
    common.extend(libc.args.iter().cloned());
    common.extend(
        [
            "-O2",
            "-g",
            "-fno-omit-frame-pointer",
            "-ffunction-sections",
            "-fdata-sections",
            "-w",
        ]
        .map(String::from),
    );
    if t.arch == Arch::Wasm32 {
        // setjmp/longjmp through Wasm exception handling, as generated code does.
        common.extend(
            [
                "-mexception-handling",
                "-mllvm",
                "-wasm-enable-sjlj",
                "-mllvm",
                "-wasm-use-legacy-eh=false",
            ]
            .map(String::from),
        );
    } else {
        // (PIC on wasm is the dynamic-linking ABI.)
        common.push("-fPIC".into());
    }
    if t.arch == Arch::Aarch64 {
        // Inline LL/SC atomics, not calls to the `__aarch64_*` helpers (compiler-rt's
        // assembly, which isn't vendored).
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
    // The bundled tz database (ppl_tzdata.c embeds it on Windows).
    let tz = ppl_runtime::tzdata_dir()
        .canonicalize()
        .map_err(|e| format!("cannot find the bundled tz database: {e}"))?;
    common.push(format!("-I{}", tz.display()));
    common.push(format!(
        "-DPPL_TZDATA_BIN=\"{}\"",
        tz.join("tzdata.bin").display()
    ));
    let mut jobs = Vec::new();
    let mut dirs = vec![
        (ppl_runtime::source_dir(), "-std=c11", ""),
        (mbed.join("library"), "-std=c99", "mbedtls_"),
    ];
    if t.os == crate::target::Os::Wasi {
        // The WASI bindings (`runtime/wasi`).
        dirs.push((
            ppl_runtime::include_dir().join("../wasi"),
            "-std=c11",
            "wasi_",
        ));
    }
    for (dir, std, prefix) in dirs {
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
                start: false,
            });
        }
    }
    for (src, flag) in &libc.start {
        let mut args = common.clone();
        args.extend(flag.map(String::from));
        jobs.push(Job {
            obj: format!("start_{}.o", src.file_name().unwrap().to_string_lossy()),
            src: src.clone(),
            args,
            start: true,
        });
    }
    Ok(jobs)
}

/// Runs the jobs on as many threads as there are CPUs.
fn compile_all(clang: &Path, what: &str, jobs: &[Job], out: &Path) -> Result<(), String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let errors = Mutex::new(Vec::new());
    let n = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    std::thread::scope(|s| {
        for _ in 0..n {
            s.spawn(|| {
                while let Some(j) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let mut cmd = Command::new(clang);
                    cmd.args(&j.args);
                    // For scripts/prune-windows-headers.sh: which headers each object used.
                    if std::env::var_os("PPL_DEPFILES").is_some() {
                        cmd.arg("-MD")
                            .arg("-MF")
                            .arg(out.join(format!("{}.d", j.obj)));
                    }
                    let r = cmd
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
                        "compiling the {what} ({}) failed:\n{failed}",
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
