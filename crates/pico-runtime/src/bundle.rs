//! The resource bundle's format (decision 297), shared by the build script (`pack`) and the
//! compiler (`unpack`): a magic line, then for each file a little-endian `u32` path length, the
//! path (UTF-8, `/`-separated, relative), a `u64` data length and the data. The build script
//! compresses it (gzip); this module only handles the uncompressed bytes.

use std::path::Path;

const MAGIC: &[u8] = b"PICOBUNDLE1\n";

/// The bundle of `files` (relative path, contents), in the order given.
#[allow(dead_code)] // (used by the build script)
pub fn pack(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    for (path, data) in files {
        out.extend_from_slice(&(path.len() as u32).to_le_bytes());
        out.extend_from_slice(path.as_bytes());
        out.extend_from_slice(&(data.len() as u64).to_le_bytes());
        out.extend_from_slice(data);
    }
    out
}

/// Writes the files of `bundle` under `dest`. Paths that would leave `dest` are rejected.
#[allow(dead_code)] // (not used by the build script)
pub fn unpack(bundle: &[u8], dest: &Path) -> Result<usize, String> {
    let mut rest = bundle
        .strip_prefix(MAGIC)
        .ok_or("not a pico resource bundle")?;
    let mut count = 0;
    while !rest.is_empty() {
        let len = u32::from_le_bytes(take(&mut rest, 4)?.try_into().unwrap()) as usize;
        let path = std::str::from_utf8(take(&mut rest, len)?).map_err(|e| e.to_string())?;
        let size = u64::from_le_bytes(take(&mut rest, 8)?.try_into().unwrap()) as usize;
        let data = take(&mut rest, size)?;
        if path.is_empty()
            || path.starts_with('/')
            || path.contains('\\')
            || path
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == "..")
        {
            return Err(format!("bad path in resource bundle: {path:?}"));
        }
        let file = dest.join(path);
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&file, data).map_err(|e| format!("{}: {e}", file.display()))?;
        count += 1;
    }
    Ok(count)
}

/// The next `n` bytes of `rest`.
#[allow(dead_code)]
fn take<'a>(rest: &mut &'a [u8], n: usize) -> Result<&'a [u8], String> {
    if rest.len() < n {
        return Err("truncated resource bundle".into());
    }
    let (a, b) = rest.split_at(n);
    *rest = b;
    Ok(a)
}
