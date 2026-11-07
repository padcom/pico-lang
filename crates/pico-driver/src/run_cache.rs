//! Eviction for `pico run`'s executable cache, `$TMPDIR/pico-cache/<16 hex digits>/`: each
//! entry is one program as built by one compiler, so they pile up (a rebuilt compiler or a
//! changed program makes new ones). Entries record their last use in a `.used` file; after a
//! build, those unused for `MAX_AGE` go, and then the least recently used ones until the cache
//! is under its size limit (`PICO_RUN_CACHE_MB`, default `DEFAULT_LIMIT_MB`). Entries used in the
//! last `KEEP_RECENT` always stay, so a program that's about to run (or another `pico` working
//! alongside) never loses its executable. The other caches (`runtime/`, `musl/`, `home/`, …)
//! aren't touched. Failures are ignored: another compiler may be pruning too.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Entries unused this long go whatever the cache's size.
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Entries used this recently always stay.
const KEEP_RECENT: Duration = Duration::from_secs(10 * 60);
/// A scan at most this often (the `.pruned` marker in the cache).
const SCAN_EVERY: Duration = Duration::from_secs(60);
/// The default size limit, in MiB.
const DEFAULT_LIMIT_MB: u64 = 1024;

/// Whether `name` is a run-cache entry's (16 lowercase hex digits).
fn is_entry(name: &str) -> bool {
    name.len() == 16 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Records that the entry `dir` was used `now`.
pub fn mark_used(dir: &Path, now: SystemTime) {
    if let Ok(f) = std::fs::File::create(dir.join(".used")) {
        let _ = f.set_modified(now);
    }
}

/// The size limit in bytes: `PICO_RUN_CACHE_MB` or the default.
pub fn limit() -> u64 {
    std::env::var("PICO_RUN_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_LIMIT_MB)
        .saturating_mul(1024 * 1024)
}

/// Prunes `cache` (the `pico-cache` directory) unless it was pruned in the last `SCAN_EVERY`.
pub fn prune_throttled(cache: &Path, now: SystemTime, limit: u64) {
    let marker = cache.join(".pruned");
    let recent = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .is_ok_and(|t| now.duration_since(t).is_ok_and(|d| d < SCAN_EVERY));
    if recent {
        return;
    }
    if let Ok(f) = std::fs::File::create(&marker) {
        let _ = f.set_modified(now);
    }
    prune(cache, now, limit);
}

/// The total size of the files under `dir`.
fn size_of(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => size_of(&e.path()),
            Ok(_) => e.metadata().map_or(0, |m| m.len()),
            Err(_) => 0,
        })
        .sum()
}

/// Removes `cache`'s entries unused for `MAX_AGE`, then the least recently used ones until the
/// rest fit in `limit` bytes, keeping those used in the last `KEEP_RECENT`.
pub fn prune(cache: &Path, now: SystemTime, limit: u64) {
    let Ok(entries) = std::fs::read_dir(cache) else {
        return;
    };
    let age = |t: SystemTime| now.duration_since(t).unwrap_or_default();
    // (age since last use, size, path), oldest first.
    let mut kept = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let p = e.path();
        if !is_entry(&name.to_string_lossy()) || !p.is_dir() {
            continue;
        }
        let used = std::fs::metadata(p.join(".used"))
            .or_else(|_| std::fs::metadata(&p))
            .and_then(|m| m.modified())
            .map_or(Duration::ZERO, age);
        if used > MAX_AGE {
            let _ = std::fs::remove_dir_all(&p);
        } else {
            kept.push((used, size_of(&p), p));
        }
    }
    let mut total: u64 = kept.iter().map(|k| k.1).sum();
    kept.sort_by_key(|k| std::cmp::Reverse(k.0));
    for (used, size, p) in kept {
        if total <= limit || used < KEEP_RECENT {
            break;
        }
        if std::fs::remove_dir_all(&p).is_ok() {
            total -= size;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry `name` in `cache` with `size` bytes, last used `ago` before `now`.
    fn entry(cache: &Path, name: &str, size: usize, now: SystemTime, ago: u64) {
        let d = cache.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("main"), vec![0u8; size]).unwrap();
        mark_used(&d, now - Duration::from_secs(ago));
    }

    #[test]
    fn prunes_old_then_least_recently_used_entries() {
        let cache =
            std::env::temp_dir().join(format!("pico-run-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(&cache).unwrap();
        let now = SystemTime::now();
        let hour = 3600;
        entry(&cache, "000000000000000a", 100, now, 8 * 24 * hour); // too old
        entry(&cache, "000000000000000b", 100, now, 5 * hour); // least recently used
        entry(&cache, "000000000000000c", 100, now, 4 * hour);
        entry(&cache, "000000000000000d", 100, now, 60); // recent: stays
        entry(&cache, "000000000000000e", 100, now, 30); // recent: stays
        std::fs::create_dir_all(cache.join("runtime")).unwrap(); // not an entry
        std::fs::write(cache.join("runtime/x"), vec![0u8; 1000]).unwrap();
        // The four younger entries are 400 bytes (`.used` files are empty): the oldest one goes.
        prune(&cache, now, 300);
        let left = |n: &str| cache.join(n).exists();
        assert!(!left("000000000000000a"));
        assert!(!left("000000000000000b"));
        assert!(left("000000000000000c"));
        assert!(left("000000000000000d"));
        assert!(left("000000000000000e"));
        assert!(left("runtime/x"));
        // Recently used entries stay even over the limit.
        prune(&cache, now, 0);
        assert!(!left("000000000000000c"));
        assert!(left("000000000000000d") && left("000000000000000e"));
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn entry_names() {
        assert!(is_entry("00366ba7f344f4d5"));
        assert!(!is_entry("runtime"));
        assert!(!is_entry("00366ba7f344f4d"));
        assert!(!is_entry("00366BA7F344F4D5"));
    }
}
