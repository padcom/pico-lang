//! Git sources of packages (spec §8.3, decision 285), through the system `git`: a mirror of
//! each repository in the cache (`<cache>/ppl/git/<url hash>/mirror`), and one checkout of
//! each commit used (`…/<commit>`, plain files, no `.git`). A package's content hash is
//! `sha256:` over its files sorted by path (each path, a NUL, the length, the bytes).

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The cache directory of a repository.
fn repo_dir(url: &str) -> PathBuf {
    let h = Sha256::digest(url.as_bytes());
    let short: String = h.iter().take(8).map(|b| format!("{b:02x}")).collect();
    crate::fingerprint::cache_dir()
        .join("ppl")
        .join("git")
        .join(short)
}

/// The mirror of `url` in the cache, cloned or updated (`update`: fetch new refs; needs the
/// network).
fn mirror(url: &str, update: bool) -> Result<PathBuf, String> {
    let dir = repo_dir(url).join("mirror");
    if !dir.join("HEAD").exists() {
        std::fs::create_dir_all(dir.parent().expect("parent")).map_err(|e| e.to_string())?;
        git(&[
            "clone",
            "--quiet",
            "--mirror",
            url,
            &dir.display().to_string(),
        ])?;
    } else if update {
        git(&[
            "--git-dir",
            &dir.display().to_string(),
            "fetch",
            "--quiet",
            "--prune",
            "origin",
        ])?;
    }
    Ok(dir)
}

/// The commit a tag, branch or revision names (fetching the repository first).
pub fn resolve(url: &str, pin: &crate::packages::GitPin) -> Result<String, String> {
    use crate::packages::GitPin;
    let m = mirror(url, true)
        .map_err(|e| format!("cannot fetch {url} (for its {}): {e}", describe(pin)))?;
    let spec = match pin {
        GitPin::Tag(t) => format!("refs/tags/{t}^{{commit}}"),
        GitPin::Branch(b) => format!("refs/heads/{b}^{{commit}}"),
        GitPin::Rev(r) => format!("{r}^{{commit}}"),
        GitPin::Default => "HEAD^{commit}".into(),
    };
    git(&[
        "--git-dir",
        &m.display().to_string(),
        "rev-parse",
        "--verify",
        "--quiet",
        &spec,
    ])
    .map_err(|_| format!("{url} has no {}", describe(pin)))
}

pub fn describe(pin: &crate::packages::GitPin) -> String {
    use crate::packages::GitPin;
    match pin {
        GitPin::Tag(t) => format!("tag `{t}`"),
        GitPin::Branch(b) => format!("branch `{b}`"),
        GitPin::Rev(r) => format!("revision `{r}`"),
        GitPin::Default => "default branch".into(),
    }
}

/// The checkout of `commit` in the cache: made from the mirror (cloning it if `fetch` allows,
/// when the cache has neither), or `None` when it isn't cached and fetching isn't allowed.
pub fn checkout(url: &str, commit: &str, fetch: bool) -> Result<Option<PathBuf>, String> {
    let dir = repo_dir(url).join(commit);
    if dir.join(".ppl-checkout").exists() {
        return Ok(Some(dir));
    }
    let m = repo_dir(url).join("mirror");
    if !m.join("HEAD").exists() {
        if !fetch {
            return Ok(None);
        }
        mirror(url, false)?;
    }
    let has = git(&[
        "--git-dir",
        &m.display().to_string(),
        "cat-file",
        "-e",
        &format!("{commit}^{{commit}}"),
    ])
    .is_ok();
    if !has {
        if !fetch {
            return Ok(None);
        }
        mirror(url, true)?;
    }
    // (Written next to the final place, then renamed: an interrupted checkout leaves nothing.)
    let tmp = repo_dir(url).join(format!("{commit}.tmp"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    git(&[
        "--git-dir",
        &m.display().to_string(),
        "--work-tree",
        &tmp.display().to_string(),
        "checkout",
        "--quiet",
        "--force",
        commit,
        "--",
        ".",
    ])?;
    std::fs::write(tmp.join(".ppl-checkout"), commit).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &dir).map_err(|e| e.to_string())?;
    Ok(Some(dir))
}

/// A package directory's content hash (`sha256:…`): its files sorted by path, without `.git`,
/// the cache's marker and the lockfile.
pub fn content_hash(dir: &Path) -> Result<String, String> {
    let mut files = Vec::new();
    collect(dir, dir, &mut files).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    files.sort();
    let mut h = Sha256::new();
    for rel in files {
        let bytes = std::fs::read(dir.join(&rel)).map_err(|e| e.to_string())?;
        h.update(rel.as_bytes());
        h.update([0]);
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("sha256:{digest}"))
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        let name = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if name == ".git" || name == ".ppl-checkout" || name == "project.lock" {
            continue;
        }
        if p.is_dir() {
            collect(root, &p, out)?;
        } else {
            let rel = p
                .strip_prefix(root)
                .expect("inside")
                .to_string_lossy()
                .replace('\\', "/");
            out.push(rel);
        }
    }
    Ok(())
}
