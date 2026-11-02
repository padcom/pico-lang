//! Compiles the C runtime (`runtime/`) into `libppl_rt.a` for the host. The library is linked
//! into ppl programs, not into the compiler, so no link directives are emitted.

use std::path::PathBuf;

/// Spec §15.3: the oldest supported macOS. Must match `ppl_runtime::MACOS_MIN`.
const MACOS_MIN: &str = "13.0";

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../runtime");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut build = cc::Build::new();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        build.flag(format!("-mmacosx-version-min={MACOS_MIN}"));
    }
    build
        .file(root.join("src/ppl_rt.c"))
        .file(root.join("src/ppl_gc.c"))
        .file(root.join("src/ppl_loop.c"))
        .file(root.join("src/ppl_worker.c"))
        .file(root.join("src/ppl_io.c"))
        .include(root.join("include"))
        .std("c11")
        .opt_level(2)
        .warnings(true)
        .extra_warnings(true)
        .flag_if_supported("-Werror")
        .cargo_metadata(false)
        .compile("ppl_rt");
    // Mbed TLS (third_party/mbedtls, vendored and unmodified) and ppl's crypto/TLS layer over
    // it. Both are linked into ppl programs and, through Cargo, into the compiler (the
    // interpreter calls the same functions).
    let mbed = root.join("../third_party/mbedtls");
    println!("cargo:rerun-if-changed={}", mbed.display());
    let user_config = "\"ppl_mbedtls_config.h\"";
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
        .compile("ppl_mbedtls");
    let mut glue = cc::Build::new();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        glue.flag(format!("-mmacosx-version-min={MACOS_MIN}"));
    }
    glue.file(root.join("src/ppl_crypto.c"))
        .include(mbed.join("include"))
        .include(root.join("include"))
        .define("MBEDTLS_USER_CONFIG_FILE", user_config)
        .std("c11")
        .opt_level(2)
        .warnings(true)
        .extra_warnings(true)
        .flag_if_supported("-Werror")
        .compile("ppl_crypto");
    // (`compile` printed the link lines for both, glue first: the order static linking needs.)
    println!(
        "cargo:rustc-env=PPL_RUNTIME_DIR={}",
        std::env::var("OUT_DIR").unwrap()
    );
    println!(
        "cargo:rustc-env=PPL_RUNTIME_INCLUDE={}",
        root.join("include").display()
    );
}
