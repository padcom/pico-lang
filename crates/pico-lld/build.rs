//! Compiles the lld shim and links lld's static libraries (from the LLVM installation, or
//! `PICO_LLD_PREFIX` where lld is installed separately, as with Homebrew). lld is required on
//! Linux; elsewhere pico builds without it until the targets that need it (cfg `pico_lld`).

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=PICO_LLD_PREFIX");
    println!("cargo:rerun-if-env-changed=LLVM_SYS_231_PREFIX");
    println!("cargo:rerun-if-changed=src/shim.cpp");
    println!("cargo::rustc-check-cfg=cfg(pico_lld)");
    let llvm = std::env::var("LLVM_SYS_231_PREFIX").map(PathBuf::from).ok();
    let lld = std::env::var("PICO_LLD_PREFIX")
        .map(PathBuf::from)
        .ok()
        .or(llvm.clone());
    let Some(lld) = lld.filter(|p| p.join("lib/liblldELF.a").is_file()) else {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
            panic!("lld's libraries (liblldELF.a) not found: install liblld-23-dev");
        }
        return;
    };
    let mut b = cc::Build::new();
    b.cpp(true)
        .file("src/shim.cpp")
        .include(lld.join("include"))
        .std("c++17")
        .flag_if_supported("-fno-rtti")
        .warnings(false);
    if let Some(llvm) = &llvm {
        b.include(llvm.join("include"));
    }
    b.compile("pico_lld_shim");
    println!(
        "cargo:rustc-link-search=native={}",
        lld.join("lib").display()
    );
    for l in [
        "lldELF",
        "lldCOFF",
        "lldMinGW",
        "lldMachO",
        "lldWasm",
        "lldCommon",
    ] {
        println!("cargo:rustc-link-lib=static={l}");
    }
    println!("cargo:rustc-cfg=pico_lld");
}
