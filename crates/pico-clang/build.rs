//! Compiles clang's driver (vendor/, from llvm-project) and pico's shim against the LLVM
//! installation (`LLVM_SYS_231_PREFIX`), and links clang's static libraries from it. Without
//! them (`libclangDriver.a`), pico builds without its own clang and runs an installed one
//! (cfg `pico_clang`; decision 298).

use std::path::PathBuf;
use std::process::Command;

/// clang's libraries, users before what they use (for linkers that read archives once).
const LIBS: &[&str] = &[
    "clangCodeGen",
    "clangFrontend",
    "clangDriver",
    // (The driver prints the analysis formats and scans module dependencies.)
    "clangScalableStaticAnalysisFrontend",
    "clangScalableStaticAnalysisSourceTransformation",
    "clangScalableStaticAnalysisAnalyses",
    "clangScalableStaticAnalysisCore",
    "clangDependencyScanning",
    "clangUnifiedSymbolResolution",
    "clangSerialization",
    "clangParse",
    "clangSema",
    "clangAPINotes",
    "clangAnalysis",
    "clangAnalysisLifetimeSafety",
    "clangEdit",
    "clangASTMatchers",
    "clangAST",
    "clangLex",
    "clangOptions",
    "clangBasic",
    "clangSupport",
];

fn main() {
    println!("cargo:rerun-if-env-changed=LLVM_SYS_231_PREFIX");
    println!("cargo:rerun-if-changed=src/shim.cpp");
    println!("cargo:rerun-if-changed=src/frontend.cpp");
    println!("cargo:rerun-if-changed=vendor");
    println!("cargo::rustc-check-cfg=cfg(pico_clang)");
    let Some(llvm) = std::env::var("LLVM_SYS_231_PREFIX").map(PathBuf::from).ok() else {
        return;
    };
    let lib = llvm.join("lib");
    if !lib.join("libclangDriver.a").is_file() {
        println!(
            "cargo:warning=clang's static libraries not found: pico will run an installed clang"
        );
        return;
    }
    let config = |arg: &str| -> String {
        let out = Command::new(llvm.join("bin/llvm-config"))
            .arg(arg)
            .output()
            .expect("llvm-config");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let mut b = cc::Build::new();
    b.cpp(true)
        .files([
            "src/shim.cpp",
            "src/frontend.cpp",
            "vendor/driver.cpp",
            "vendor/cc1_main.cpp",
            "vendor/cc1as_main.cpp",
        ])
        .include(llvm.join("include"))
        .std("c++17")
        .warnings(false);
    for flag in config("--cxxflags").split_whitespace() {
        if flag.starts_with("-D") || flag == "-fno-exceptions" || flag.starts_with("-stdlib") {
            b.flag(flag);
        }
    }
    if config("--has-rtti") != "YES" {
        b.flag("-fno-rtti");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        b.flag("-mmacosx-version-min=13.0");
    }
    b.compile("pico_clang_shim");
    println!("cargo:rustc-link-search=native={}", lib.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        // GNU ld reads each archive once, in order, and clang's libraries use each other in
        // cycles: one archive of all of them (which ld rescans until nothing new resolves).
        let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let all = out.join("libpico_clang_libs.a");
        let _ = std::fs::remove_file(&all);
        let status = Command::new(llvm.join("bin/llvm-ar"))
            .arg("qcsL")
            .arg(&all)
            .args(LIBS.iter().map(|l| lib.join(format!("lib{l}.a"))))
            .status()
            .expect("llvm-ar");
        assert!(status.success(), "llvm-ar failed");
        println!("cargo:rustc-link-search=native={}", out.display());
        println!("cargo:rustc-link-lib=static:-bundle=pico_clang_libs");
    } else {
        for l in LIBS {
            println!("cargo:rustc-link-lib=static={l}");
        }
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // (The driver asks Xcode's selection for the SDK.)
        println!("cargo:rustc-link-lib=dylib=xcselect");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // The driver reads Visual Studio's file versions; MSYS2's LLVM uses libffi (its
        // interpreter), which `llvm-config --system-libs` leaves out.
        for l in ["version", "ffi"] {
            println!("cargo:rustc-link-lib={l}");
        }
    }
    // The resource directory (builtin headers such as stddef.h) of this clang.
    let version = config("--version");
    let major = version.split('.').next().unwrap_or("23");
    println!(
        "cargo:rustc-env=PICO_CLANG_RESOURCE_DIR={}",
        lib.join("clang").join(major).display()
    );
    println!("cargo:rustc-cfg=pico_clang");
}
