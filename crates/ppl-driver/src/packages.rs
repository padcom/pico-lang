//! Packages (spec §8.3, decisions 284–286): `project.json` manifests, and what a package import
//! (`import … from "scope/name"`) names: the dependency's root `index.ppl`, found among the
//! workspace's members by name and version range. (Git sources and the lockfile: P10.3b.)

use crate::semver::{Range, Version};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where a dependency comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// A version range: a workspace member of that name (or, from P10.4, the registry).
    Range(Range),
    /// A git repository at a tag, a branch or a commit.
    Git { url: String, pin: GitPin },
}

#[derive(Clone, Debug, PartialEq)]
pub enum GitPin {
    Tag(String),
    Branch(String),
    Rev(String),
    /// The default branch.
    Default,
}

/// A parsed `project.json`.
#[derive(Clone, Debug, Default)]
pub struct Manifest {
    /// The directory it's in.
    pub dir: PathBuf,
    pub name: Option<String>,
    pub version: Option<Version>,
    /// An application's main file, relative to `dir`.
    pub entry: Option<String>,
    pub dependencies: BTreeMap<String, Source>,
    pub dev_dependencies: BTreeMap<String, Source>,
    /// Member directory patterns (`packages/*`), in a workspace's root manifest.
    pub workspace: Vec<String>,
}

/// Whether `s` is a package name: `scope/name`, each part lowercase letters, digits, `-`,
/// `_`, `.`.
pub fn is_package_name(s: &str) -> bool {
    let ok = |p: &str| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c))
    };
    matches!(s.split_once('/'), Some((scope, name)) if ok(scope) && ok(name))
}

impl Manifest {
    /// Reads `dir/project.json`: `None` if there's none, the problems if it's invalid.
    pub fn read(dir: &Path) -> Option<Result<Manifest, Vec<String>>> {
        let text = std::fs::read_to_string(dir.join("project.json")).ok()?;
        Some(Manifest::parse(dir, &text))
    }

    pub fn parse(dir: &Path, text: &str) -> Result<Manifest, Vec<String>> {
        let json: Value = serde_json::from_str(text).map_err(|e| vec![e.to_string()])?;
        let mut m = Manifest {
            dir: dir.to_path_buf(),
            ..Manifest::default()
        };
        let mut errors = Vec::new();
        let Some(obj) = json.as_object() else {
            return Err(vec!["the manifest must be an object".into()]);
        };
        let string = |k: &str, errors: &mut Vec<String>| match obj.get(k) {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                errors.push(format!("`{k}` must be a string"));
                None
            }
        };
        m.name = string("name", &mut errors);
        if let Some(n) = &m.name
            && !is_package_name(n)
        {
            errors.push(format!(
                "`name`: `{n}` isn't `scope/name` (lowercase letters, digits, `-`, `_`, `.`)"
            ));
        }
        if let Some(v) = string("version", &mut errors) {
            match Version::parse(&v) {
                Ok(v) => m.version = Some(v),
                Err(e) => errors.push(format!("`version`: {e}")),
            }
        }
        m.entry = string("entry", &mut errors);
        for (key, field) in [
            ("dependencies", &mut m.dependencies),
            ("devDependencies", &mut m.dev_dependencies),
        ] {
            match obj.get(key) {
                None => {}
                Some(Value::Object(deps)) => {
                    for (name, v) in deps {
                        match dependency(name, v) {
                            Ok(s) => {
                                field.insert(name.clone(), s);
                            }
                            Err(e) => errors.push(format!("`{key}.{name}`: {e}")),
                        }
                    }
                }
                Some(_) => errors.push(format!("`{key}` must be an object")),
            }
        }
        match obj.get("workspace") {
            None => {}
            Some(Value::Array(items)) => {
                for i in items {
                    match i.as_str() {
                        Some(s) => m.workspace.push(s.to_string()),
                        None => {
                            errors.push("`workspace` lists directory patterns (strings)".into())
                        }
                    }
                }
            }
            Some(_) => errors.push("`workspace` must be a list of directory patterns".into()),
        }
        if errors.is_empty() {
            Ok(m)
        } else {
            Err(errors)
        }
    }

    /// The manifest file's path.
    pub fn path(&self) -> PathBuf {
        self.dir.join("project.json")
    }

    /// What it's called in messages: its name, else its directory.
    pub fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.dir.display().to_string())
    }
}

fn dependency(name: &str, v: &Value) -> Result<Source, String> {
    if !is_package_name(name) {
        return Err(format!("`{name}` isn't a package name (`scope/name`)"));
    }
    match v {
        Value::String(r) => Range::parse(r).map(Source::Range),
        Value::Object(o) => {
            let Some(url) = o.get("git").and_then(Value::as_str) else {
                return Err("a dependency is a version range or `{ \"git\": url, … }`".into());
            };
            let s = |k: &str| o.get(k).and_then(Value::as_str).map(str::to_string);
            let pins = [
                s("tag").map(GitPin::Tag),
                s("branch").map(GitPin::Branch),
                s("rev").map(GitPin::Rev),
            ];
            let mut given = pins.into_iter().flatten();
            let pin = given.next().unwrap_or(GitPin::Default);
            if given.next().is_some() {
                return Err("give one of `tag`, `branch` and `rev`".into());
            }
            Ok(Source::Git {
                url: url.to_string(),
                pin,
            })
        }
        _ => Err("a dependency is a version range or `{ \"git\": url, … }`".into()),
    }
}

/// The manifest of the package `dir` is in: the nearest `project.json` in it or above.
pub fn package_of(dir: &Path) -> Option<Result<Manifest, Vec<String>>> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    dir.ancestors().find_map(Manifest::read)
}

/// The root manifest of the workspace `m` belongs to (one with `workspace` listing its
/// directory), if any.
fn workspace_of(m: &Manifest) -> Option<Manifest> {
    if !m.workspace.is_empty() {
        return Some(m.clone());
    }
    m.dir.ancestors().skip(1).find_map(|d| {
        let root = Manifest::read(d)?.ok()?;
        let members = members(&root);
        members.contains(&m.dir).then_some(root)
    })
}

/// A workspace's member directories (its patterns: a path, or a directory ending in `/*`).
pub fn members(root: &Manifest) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for pattern in &root.workspace {
        match pattern.strip_suffix("/*") {
            Some(parent) => {
                let Ok(entries) = std::fs::read_dir(root.dir.join(parent)) else {
                    continue;
                };
                let mut dirs: Vec<PathBuf> = entries
                    .filter_map(|e| Some(e.ok()?.path()))
                    .filter(|p| p.join("project.json").is_file())
                    .collect();
                dirs.sort();
                out.extend(dirs);
            }
            None => out.push(root.dir.join(pattern)),
        }
    }
    out.into_iter()
        .map(|p| p.canonicalize().unwrap_or(p))
        .collect()
}

/// What a package specifier names: the package's `index.ppl` path without the extension (the
/// loader adds it and the platform variants), or why it can't be imported.
/// `dev_root`: the package whose files see their `devDependencies` (a test's).
pub fn resolve(spec: &str, from_dir: &Path, dev_root: Option<&Path>) -> Result<PathBuf, String> {
    if !spec.contains('/') {
        return Err(format!(
            "`{spec}` isn't a module: imports are relative (`./{spec}`), `std/…` or packages (`scope/name`)"
        ));
    }
    if spec.matches('/').count() > 1 {
        let root: Vec<&str> = spec.splitn(3, '/').take(2).collect();
        return Err(format!(
            "imports reach only a package's root: `{}` (its `index.ppl`), not its other files",
            root.join("/")
        ));
    }
    let m = match package_of(from_dir) {
        Some(Ok(m)) => m,
        Some(Err(e)) => return Err(format!("project.json is invalid: {}", e.join("; "))),
        None => {
            return Err(format!(
                "`{spec}` isn't a dependency: there's no project.json (add one with `dependencies`)"
            ));
        }
    };
    let dev = dev_root.is_some_and(|d| d == m.dir);
    let source = m
        .dependencies
        .get(spec)
        .or_else(|| dev.then(|| m.dev_dependencies.get(spec)).flatten());
    let Some(source) = source else {
        let hint = if m.dev_dependencies.contains_key(spec) {
            " (it's a devDependency: only `*.test.ppl` files see it)"
        } else {
            ""
        };
        return Err(format!(
            "`{spec}` isn't a dependency of {} (add it to {}'s `dependencies`){hint}",
            m.label(),
            m.path().display()
        ));
    };
    let dir = match source {
        Source::Range(range) => {
            let Some(root) = workspace_of(&m) else {
                return Err(format!(
                    "`{spec}` `{range}`: there's no workspace member of that name (registry packages come with P10.4)"
                ));
            };
            let mut found = None;
            let mut versions = Vec::new();
            for dir in members(&root) {
                let Some(Ok(member)) = Manifest::read(&dir) else {
                    continue;
                };
                if member.name.as_deref() != Some(spec) {
                    continue;
                }
                match &member.version {
                    Some(v) if range.matches(v) => {
                        found = Some(dir);
                        break;
                    }
                    Some(v) => versions.push(v.to_string()),
                    None => versions.push("no version".into()),
                }
            }
            match found {
                Some(d) => d,
                None if versions.is_empty() => {
                    return Err(format!(
                        "`{spec}` isn't a member of the workspace at {} (registry packages come with P10.4)",
                        root.dir.display()
                    ));
                }
                None => {
                    return Err(format!(
                        "`{spec}` `{range}` isn't satisfied: the workspace has {}",
                        versions.join(", ")
                    ));
                }
            }
        }
        Source::Git { url, .. } => {
            return Err(format!(
                "`{spec}` comes from git ({url}): git dependencies come with P10.3b"
            ));
        }
    };
    let index = dir.join("index");
    let has_index = std::fs::read_dir(&dir).is_ok_and(|entries| {
        entries.filter_map(|e| e.ok()).any(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n == "index.ppl" || (n.starts_with("index.") && n.ends_with(".ppl"))
        })
    });
    if !has_index {
        return Err(format!(
            "`{spec}` exports nothing: {} has no index.ppl",
            dir.display()
        ));
    }
    Ok(index)
}

/// The project's main file: the `entry` of the nearest `project.json` in `dir` or above.
pub fn entry_of(dir: &Path) -> Result<PathBuf, String> {
    match package_of(dir) {
        Some(Ok(m)) => match &m.entry {
            Some(e) => Ok(m.dir.join(e)),
            None => Err(format!(
                "{} has no `entry` (the main file, such as \"src/main.ppl\"); name a file to build",
                m.path().display()
            )),
        },
        Some(Err(e)) => Err(format!("project.json is invalid: {}", e.join("; "))),
        None => Err("no file given, and no project.json with an `entry` here or above".into()),
    }
}
