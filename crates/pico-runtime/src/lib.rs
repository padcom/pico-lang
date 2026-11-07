//! Builds and locates the pico C runtime.
//!
//! The runtime's sources live in `runtime/` at the repository root. The build script compiles
//! them for the host into a static library that the driver links into every native program.
//! (Per-target builds come with cross-compilation, P8.1.)

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

mod bundle;

/// An installed pico's resource directory (decisions 296, 297): `PICO_HOME`, or `share/pico`
/// next to the executable's `bin/`, or (built with feature `embed`) the bundled resources,
/// unpacked into `$TMPDIR/pico-cache/home/` on first use. It holds `runtime/`, `third_party/`
/// and `stdlib/` as in the repository, and `lib/` with the host runtime libraries. `None` for a
/// compiler run from the repository's `target/`, which uses the paths it was built with.
pub fn home() -> Option<&'static Path> {
    static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOME.get_or_init(|| {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|e| canonical(&e).ok());
        home_from(std::env::var_os("PICO_HOME"), exe).or_else(embedded)
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

/// The bundled resources, unpacked (once per bundle) into the cache.
#[cfg(feature = "embed")]
fn embedded() -> Option<PathBuf> {
    static BUNDLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/resources.gz"));
    let name = format!(
        "{}-{}",
        env!("CARGO_PKG_VERSION"),
        env!("PICO_RESOURCES_HASH")
    );
    let dir = std::env::temp_dir().join("pico-cache/home").join(name);
    match unpack_once(BUNDLE, &dir) {
        Ok(()) => {
            mark_used_and_prune(&dir, std::time::SystemTime::now());
            Some(dir)
        }
        Err(e) => {
            eprintln!(
                "warning: cannot unpack pico's resources into {}: {e}",
                dir.display()
            );
            None
        }
    }
}

#[cfg(not(feature = "embed"))]
fn embedded() -> Option<PathBuf> {
    None
}

/// Unpacks the gzipped bundle into `dir` unless it's there: into a sibling first, then renamed,
/// so a directory that exists is complete (and concurrent compilers don't see a partial one).
#[cfg(feature = "embed")]
fn unpack_once(gz: &[u8], dir: &Path) -> Result<(), String> {
    use std::io::Read;
    if dir.is_dir() {
        return Ok(());
    }
    let parent = dir.parent().ok_or("no parent directory")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = parent.join(format!(
        ".{}-{}",
        dir.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_end(&mut raw)
        .map_err(|e| e.to_string())?;
    bundle::unpack(&raw, &tmp)?;
    if let Err(e) = std::fs::rename(&tmp, dir) {
        let _ = std::fs::remove_dir_all(&tmp);
        // (Another compiler may have unpacked it first.)
        if !dir.is_dir() {
            return Err(e.to_string());
        }
    }
    Ok(())
}

/// How long an unpacked version can go unused before another version's runs remove it (#50).
const UNUSED_FOR: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);

/// Marks `dir` (an unpacked version in `pico-cache/home`) as used `now`, at most once an hour,
/// and then removes the sibling versions not used for `UNUSED_FOR` (#50). Versions used side by
/// side all stay; failures are ignored (another compiler may be doing the same).
#[cfg_attr(not(feature = "embed"), allow(dead_code))]
fn mark_used_and_prune(dir: &Path, now: std::time::SystemTime) {
    let marker = dir.join(".used");
    let fresh = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .is_ok_and(|t| now.duration_since(t).is_ok_and(|d| d.as_secs() < 3600));
    if fresh {
        return;
    }
    if let Ok(f) = std::fs::File::create(&marker) {
        let _ = f.set_modified(now);
    }
    let Some(parent) = dir.parent() else { return };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        // Only other unpacked versions (`<version>-<hash>`), not partial ones (`.name-pid`).
        if p == dir || name.starts_with('.') || !name.contains('-') || !p.is_dir() {
            continue;
        }
        let last = std::fs::metadata(p.join(".used"))
            .or_else(|_| std::fs::metadata(&p))
            .and_then(|m| m.modified());
        if last.is_ok_and(|t| now.duration_since(t).is_ok_and(|d| d > UNUSED_FOR)) {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

/// `p` made absolute and resolved, as `canonicalize` does, but on Windows without its verbatim
/// prefix (`\\?\D:\…`), which clang and messages don't take.
pub fn canonical(p: &Path) -> std::io::Result<PathBuf> {
    let c = p.canonicalize()?;
    #[cfg(windows)]
    {
        let s = c.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && !rest.starts_with("UNC\\")
        {
            return Ok(PathBuf::from(rest));
        }
    }
    Ok(c)
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
    use super::{home_from, mark_used_and_prune};

    #[test]
    fn unused_versions_are_pruned() {
        use std::time::{Duration, SystemTime};
        let root = std::env::temp_dir().join(format!("pico-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let now = SystemTime::now();
        let make = |name: &str, used: Option<SystemTime>| {
            let d = root.join(name);
            std::fs::create_dir_all(d.join("stdlib")).unwrap();
            if let Some(t) = used {
                std::fs::File::create(d.join(".used"))
                    .unwrap()
                    .set_modified(t)
                    .unwrap();
            }
            d
        };
        let current = make("0.5.0-aaaa", None);
        let recent = make("0.4.1-bbbb", Some(now - Duration::from_secs(3600 * 24)));
        let old = make(
            "0.4.0-cccc",
            Some(now - Duration::from_secs(3600 * 24 * 60)),
        );
        let partial = make(
            ".0.5.0-aaaa-123",
            Some(now - Duration::from_secs(3600 * 24 * 60)),
        );
        mark_used_and_prune(&current, now);
        assert!(current.join(".used").is_file());
        assert!(recent.is_dir(), "a version used lately stays");
        assert!(!old.exists(), "a version unused for 60 days goes");
        assert!(partial.is_dir(), "another compiler's partial unpack stays");
        let _ = std::fs::remove_dir_all(&root);
    }
    use std::path::PathBuf;

    #[test]
    fn bundle_round_trip() {
        use super::bundle::{pack, unpack};
        let tmp = std::env::temp_dir().join(format!("pico-bundle-{}", std::process::id()));
        let files = vec![
            ("a.txt".to_string(), b"one".to_vec()),
            ("d/e/b.bin".to_string(), vec![0, 1, 2, 255]),
            ("empty".to_string(), Vec::new()),
        ];
        let b = pack(&files);
        assert_eq!(unpack(&b, &tmp), Ok(3));
        for (p, data) in &files {
            assert_eq!(&std::fs::read(tmp.join(p)).unwrap(), data);
        }
        // Truncated, foreign and escaping bundles are rejected.
        assert!(unpack(&b[..b.len() - 1], &tmp).is_err());
        assert!(unpack(b"nope", &tmp).is_err());
        for bad in ["../x", "/x", "a//b", "a/./b", "a\\b"] {
            assert!(
                unpack(&pack(&[(bad.into(), vec![])]), &tmp).is_err(),
                "{bad}"
            );
        }
        std::fs::remove_dir_all(&tmp).unwrap();
    }

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
