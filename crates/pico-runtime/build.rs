//! Compiles the C runtime (`runtime/`) into `libpico_rt.a` for the host. The library is linked
//! into pico programs, not into the compiler, so no link directives are emitted.

use std::path::PathBuf;

#[path = "src/bundle.rs"]
mod bundle;

/// Spec §15.3: the oldest supported macOS. Must match `pico_runtime::MACOS_MIN`.
const MACOS_MIN: &str = "13.0";

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../runtime");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut build = cc::Build::new();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        build.flag(format!("-mmacosx-version-min={MACOS_MIN}"));
    }
    build
        .file(root.join("src/pico_rt.c"))
        .file(root.join("src/pico_main.c"))
        .file(root.join("src/pico_gc.c"))
        .file(root.join("src/pico_loop.c"))
        .file(root.join("src/pico_worker.c"))
        .file(root.join("src/pico_io.c"))
        .file(root.join("src/pico_tzdata.c"))
        .include(root.join("include"))
        .std("c11")
        .opt_level(2)
        .warnings(true)
        .extra_warnings(true)
        .flag_if_supported("-Werror")
        .cargo_metadata(false)
        .compile("pico_rt");
    // Mbed TLS (third_party/mbedtls, vendored and unmodified) and pico's crypto/TLS layer over
    // it. Both are linked into pico programs and, through Cargo, into the compiler (the
    // interpreter calls the same functions).
    let mbed = root.join("../third_party/mbedtls");
    println!("cargo:rerun-if-changed={}", mbed.display());
    let user_config = "\"pico_mbedtls_config.h\"";
    let mut lib = cc::Build::new();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        lib.flag(format!("-mmacosx-version-min={MACOS_MIN}"));
    }
    let mut sources: Vec<PathBuf> = std::fs::read_dir(mbed.join("library"))
        .expect("third_party/mbedtls/library")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "c"))
        .collect();
    sources.sort();
    lib.files(&sources)
        .include(mbed.join("include"))
        .include(mbed.join("library"))
        .include(root.join("include"))
        .define("MBEDTLS_USER_CONFIG_FILE", user_config)
        .std("c99")
        .opt_level(2)
        .warnings(false)
        .compile("pico_mbedtls");
    let mut glue = cc::Build::new();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        glue.flag(format!("-mmacosx-version-min={MACOS_MIN}"));
    }
    glue.file(root.join("src/pico_crypto.c"))
        .file(root.join("src/pico_tls.c"))
        .include(mbed.join("include"))
        .include(root.join("include"))
        .define("MBEDTLS_USER_CONFIG_FILE", user_config)
        .std("c11")
        .opt_level(2)
        .warnings(true)
        .extra_warnings(true)
        .flag_if_supported("-Werror")
        .compile("pico_crypto");
    // (`compile` printed the link lines for both, glue first: the order static linking needs.)
    // On Windows (MinGW) the runtime needs winpthreads, the certificate store and the system
    // random source, for the interpreter's copy too.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        for l in ["pthread", "crypt32", "bcrypt"] {
            println!("cargo:rustc-link-lib={l}");
        }
    }
    println!(
        "cargo:rustc-env=PICO_RUNTIME_DIR={}",
        std::env::var("OUT_DIR").unwrap()
    );
    println!(
        "cargo:rustc-env=PICO_RUNTIME_INCLUDE={}",
        root.join("include").display()
    );
    #[cfg(feature = "embed")]
    embed(&root);
}

/// With feature `embed` (release builds, decision 297): the resources an installed pico needs
/// (`runtime/`, `third_party/`, `stdlib/` and the host libraries in `lib/`) as one gzipped bundle
/// in `OUT_DIR`, included by `lib.rs` and unpacked into the cache on first use.
#[cfg(feature = "embed")]
fn embed(runtime: &std::path::Path) {
    use std::hash::{Hash, Hasher};
    use std::io::Write;
    let repo = runtime.join("..");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut files = Vec::new();
    for dir in ["runtime", "third_party", "stdlib"] {
        println!("cargo:rerun-if-changed={}", repo.join(dir).display());
        collect(&repo, &repo.join(dir), &mut files);
    }
    // clang's builtin headers (stddef.h, the intrinsics), for pico's own clang (decision 298):
    // `lib/clang/<major>/include` of the LLVM pico is built with, as `clang/include`.
    println!("cargo:rerun-if-env-changed=LLVM_SYS_231_PREFIX");
    if let Ok(llvm) = std::env::var("LLVM_SYS_231_PREFIX") {
        let clang = PathBuf::from(llvm).join("lib/clang");
        let major = std::fs::read_dir(&clang).ok().and_then(|d| {
            d.flatten()
                .map(|e| e.path())
                .find(|p| p.join("include").is_dir())
        });
        if let Some(dir) = major {
            let mut headers = Vec::new();
            collect(&dir, &dir.join("include"), &mut headers);
            files.extend(headers.into_iter().map(|(p, d)| (format!("clang/{p}"), d)));
        }
    }
    for lib in ["libpico_rt.a", "libpico_crypto.a", "libpico_mbedtls.a"] {
        let data = std::fs::read(out.join(lib)).expect(lib);
        files.push((format!("lib/{lib}"), data));
    }
    files.sort();
    let raw = bundle::pack(&files);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    raw.hash(&mut h);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&raw).unwrap();
    std::fs::write(out.join("resources.gz"), gz.finish().unwrap()).unwrap();
    println!("cargo:rustc-env=PICO_RESOURCES_HASH={:016x}", h.finish());
}

/// Every file under `dir`, as (path relative to `base` with `/`, contents).
#[cfg(feature = "embed")]
fn collect(base: &std::path::Path, dir: &std::path::Path, files: &mut Vec<(String, Vec<u8>)>) {
    for e in std::fs::read_dir(dir)
        .expect("resource directory")
        .flatten()
    {
        let p = e.path();
        let name = e.file_name();
        if name == ".DS_Store" {
            continue;
        }
        if p.is_dir() {
            collect(base, &p, files);
        } else {
            let rel = p.strip_prefix(base).unwrap().components();
            let rel: Vec<String> = rel
                .map(|c| c.as_os_str().to_string_lossy().into())
                .collect();
            files.push((rel.join("/"), std::fs::read(&p).unwrap()));
        }
    }
}
