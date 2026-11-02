//! Builds and locates the ppl C runtime.
//!
//! The runtime's sources live in `runtime/` at the repository root. The build script compiles
//! them for the host into a static library that the driver links into every native program.
//! (Per-target builds come with cross-compilation, P8.1.)

use std::path::{Path, PathBuf};

/// The oldest supported macOS (spec §15.3). Programs are linked with
/// `-mmacosx-version-min` set to this, matching how the runtime was compiled.
pub const MACOS_MIN: &str = "13.0";

/// Extra flags for linking a program with the system C compiler on this host.
pub fn host_link_flags() -> Vec<String> {
    if cfg!(target_os = "macos") {
        vec![format!("-mmacosx-version-min={MACOS_MIN}")]
    } else if cfg!(target_os = "linux") {
        // glibc keeps the math functions in libm.
        vec!["-lm".to_string()]
    } else {
        Vec::new()
    }
}

/// The host runtime as a static library (`libppl_rt.a`).
pub fn static_lib() -> PathBuf {
    Path::new(env!("PPL_RUNTIME_DIR")).join("libppl_rt.a")
}

/// The libraries linked after the runtime, in link order: ppl's crypto/TLS layer, then
/// Mbed TLS.
pub fn crypto_libs() -> [PathBuf; 2] {
    let dir = Path::new(env!("PPL_RUNTIME_DIR"));
    [dir.join("libppl_crypto.a"), dir.join("libppl_mbedtls.a")]
}

/// The directory containing `ppl_rt.h`.
pub fn include_dir() -> PathBuf {
    PathBuf::from(env!("PPL_RUNTIME_INCLUDE"))
}

/// The runtime's C sources (`runtime/src`), for building it for other targets.
pub fn source_dir() -> PathBuf {
    include_dir().join("../src")
}

/// Zig's glibc ABI list and libc headers (`third_party/zig-libc`).
pub fn zig_libc_dir() -> PathBuf {
    include_dir().join("../../third_party/zig-libc")
}

/// The vendored compiler-rt builtins (`third_party/compiler-rt`).
pub fn compiler_rt_dir() -> PathBuf {
    include_dir().join("../../third_party/compiler-rt")
}

/// The vendored musl sources (`third_party/musl`).
pub fn musl_dir() -> PathBuf {
    include_dir().join("../../third_party/musl")
}

/// The vendored Mbed TLS (`third_party/mbedtls`, with `include/` and `library/`).
pub fn mbedtls_dir() -> PathBuf {
    include_dir().join("../../third_party/mbedtls")
}

/// The bundled IANA tz database (`tzdata.bin`, `windows_zones.inc`; decision 253).
pub fn tzdata_dir() -> PathBuf {
    include_dir().join("../../third_party/tzdata")
}

#[cfg(test)]
mod tests {
    #[test]
    fn library_exists() {
        assert!(
            super::static_lib().is_file(),
            "{}",
            super::static_lib().display()
        );
        assert!(super::include_dir().join("ppl_rt.h").is_file());
        assert!(super::source_dir().join("ppl_rt.c").is_file());
        assert!(super::mbedtls_dir().join("library/ssl_tls.c").is_file());
    }
}
