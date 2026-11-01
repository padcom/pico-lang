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
        .include(root.join("include"))
        .std("c11")
        .opt_level(2)
        .warnings(true)
        .extra_warnings(true)
        .flag_if_supported("-Werror")
        .cargo_metadata(false)
        .compile("ppl_rt");
    println!(
        "cargo:rustc-env=PPL_RUNTIME_DIR={}",
        std::env::var("OUT_DIR").unwrap()
    );
    println!(
        "cargo:rustc-env=PPL_RUNTIME_INCLUDE={}",
        root.join("include").display()
    );
}
