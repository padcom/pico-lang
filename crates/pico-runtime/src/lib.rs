//! Builds and locates the pico C runtime.
//!
//! The runtime's sources live in `runtime/` at the repository root. The build script compiles
//! them for the host into a static library that the driver links into every native program.
//! (Per-target builds come with cross-compilation, P8.1.)

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// An installed pico's resource directory (decision 296): `PICO_HOME`, or `share/pico` next to
/// the executable's `bin/` (the release archive's layout). It holds `runtime/`, `third_party/`
/// and `stdlib/` as in the repository, and `lib/` with the host runtime libraries. `None` for a
/// compiler run from the repository's `target/`, which uses the paths it was built with.
pub fn home() -> Option<&'static Path> {
    static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOME.get_or_init(|| {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|e| e.canonicalize().ok());
        home_from(std::env::var_os("PICO_HOME"), exe)
    })
    .as_deref()
}

fn home_from(env: Option<OsString>, exe: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(d) = env.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    let share = exe?.parent()?.parent()?.join("share/pico");
    share
        .join("runtime/include/pico_rt.h")
        .is_file()
        .then_some(share)
}

/// The directory of the host runtime libraries (`libpico_rt.a` and the crypto libraries).
fn lib_dir() -> PathBuf {
    match home() {
        Some(h) => h.join("lib"),
        None => PathBuf::from(env!("PICO_RUNTIME_DIR")),
    }
}

/// The standard library's sources, when installed (`<home>/stdlib`).
pub fn installed_stdlib() -> Option<PathBuf> {
    home().map(|h| h.join("stdlib"))
}

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

/// The host runtime as a static library (`libpico_rt.a`).
pub fn static_lib() -> PathBuf {
    lib_dir().join("libpico_rt.a")
}

/// The libraries linked after the runtime, in link order: pico's crypto/TLS layer, then
/// Mbed TLS.
pub fn crypto_libs() -> [PathBuf; 2] {
    let dir = lib_dir();
    [dir.join("libpico_crypto.a"), dir.join("libpico_mbedtls.a")]
}

/// The directory containing `pico_rt.h`.
pub fn include_dir() -> PathBuf {
    match home() {
        Some(h) => h.join("runtime/include"),
        None => PathBuf::from(env!("PICO_RUNTIME_INCLUDE")),
    }
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

/// TypeScript's `lib.dom.d.ts` (`third_party/typescript`), for `js:dom` imports (P8.8).
pub fn typescript_dir() -> PathBuf {
    include_dir().join("../../third_party/typescript")
}

#[cfg(test)]
mod tests {
    use super::home_from;
    use std::path::PathBuf;

    #[test]
    fn home_lookup() {
        let tmp = std::env::temp_dir().join(format!("pico-home-{}", std::process::id()));
        let inc = tmp.join("share/pico/runtime/include");
        std::fs::create_dir_all(&inc).unwrap();
        std::fs::create_dir_all(tmp.join("bin")).unwrap();
        let exe = tmp.join("bin/pico");
        // Not installed until the runtime's header is there.
        assert_eq!(home_from(None, Some(exe.clone())), None);
        std::fs::write(inc.join("pico_rt.h"), "").unwrap();
        assert_eq!(
            home_from(None, Some(exe.clone())),
            Some(tmp.join("share/pico"))
        );
        // PICO_HOME wins; an empty one is ignored.
        let env = Some("/opt/pico".into());
        assert_eq!(
            home_from(env, Some(exe.clone())),
            Some(PathBuf::from("/opt/pico"))
        );
        assert_eq!(
            home_from(Some("".into()), Some(exe)),
            Some(tmp.join("share/pico"))
        );
        assert_eq!(home_from(None, Some(PathBuf::from("/pico"))), None);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn library_exists() {
        assert!(
            super::static_lib().is_file(),
            "{}",
            super::static_lib().display()
        );
        assert!(super::include_dir().join("pico_rt.h").is_file());
        assert!(super::source_dir().join("pico_rt.c").is_file());
        assert!(super::mbedtls_dir().join("library/ssl_tls.c").is_file());
    }
}
