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
/// `dev_root`: the package whose files see their `devDependencies` (a test's); `prepared`: the
/// build's packages (git dependencies come from it), or why they couldn't be prepared.
pub fn resolve(
    spec: &str,
    from_dir: &Path,
    dev_root: Option<&Path>,
    prepared: Option<&Result<Option<Prepared>, String>>,
) -> Result<PathBuf, String> {
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
        Source::Git { url, .. } => match prepared {
            Some(Ok(Some(p))) => match p.dependency(&m.dir, spec) {
                Some(d) => d.clone(),
                None => {
                    return Err(format!(
                        "`{spec}` ({url}) isn't in project.lock: run `ppl fetch`"
                    ));
                }
            },
            Some(Err(e)) => return Err(e.clone()),
            _ => return Err(format!("`{spec}` ({url}): run `ppl fetch`")),
        },
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

// ---------------------------------------------------------------- the lockfile (P10.3b)

/// Whether preparing packages may use the network and write `project.lock` (the CLI's builds,
/// `ppl fetch`): the language server's checks only read the lockfile and the cache.
static FETCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn allow_fetching(on: bool) {
    FETCH.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn fetching() -> bool {
    FETCH.load(std::sync::atomic::Ordering::Relaxed)
}

/// Where a locked package comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockSource {
    /// A workspace member, by its directory relative to the workspace's root.
    Path(String),
    /// A git commit: the reference the manifest gave (`tag:v1.0.0`, `branch:main`, `rev:…`,
    /// `default`) and what it resolved to.
    Git {
        url: String,
        reference: String,
        commit: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Locked {
    pub name: String,
    pub version: String,
    pub source: LockSource,
    /// `sha256:…` of a git package's files.
    pub hash: Option<String>,
    /// Its dependencies, by name: the locked package each resolved to (`name@version`).
    pub dependencies: BTreeMap<String, String>,
}

impl Locked {
    pub fn id(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

/// `project.lock` (decision 285).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lock {
    /// By id (`name@version`).
    pub packages: BTreeMap<String, Locked>,
}

impl Lock {
    pub fn read(path: &Path) -> Result<Option<Lock>, String> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(None);
        };
        let bad = |what: &str| format!("{} is invalid: {what}", path.display());
        let json: Value = serde_json::from_str(&text).map_err(|e| bad(&e.to_string()))?;
        let mut lock = Lock::default();
        for p in json["packages"]
            .as_array()
            .ok_or_else(|| bad("no `packages`"))?
        {
            let s = |k: &str| p[k].as_str().map(str::to_string);
            let name = s("name").ok_or_else(|| bad("a package without `name`"))?;
            let version = s("version").ok_or_else(|| bad("a package without `version`"))?;
            let source = match (s("path"), s("git")) {
                (Some(path), _) => LockSource::Path(path),
                (None, Some(url)) => LockSource::Git {
                    url,
                    reference: s("ref").unwrap_or_else(|| "default".into()),
                    commit: s("commit").ok_or_else(|| bad("a git package without `commit`"))?,
                },
                _ => return Err(bad("a package without `path` or `git`")),
            };
            let dependencies = p["dependencies"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let l = Locked {
                name,
                version,
                source,
                hash: s("hash"),
                dependencies,
            };
            lock.packages.insert(l.id(), l);
        }
        Ok(Some(lock))
    }

    pub fn to_json(&self) -> String {
        let packages: Vec<Value> = self
            .packages
            .values()
            .map(|l| {
                let mut o = serde_json::json!({ "name": l.name, "version": l.version });
                match &l.source {
                    LockSource::Path(p) => o["path"] = serde_json::json!(p),
                    LockSource::Git {
                        url,
                        reference,
                        commit,
                    } => {
                        o["git"] = serde_json::json!(url);
                        o["ref"] = serde_json::json!(reference);
                        o["commit"] = serde_json::json!(commit);
                    }
                }
                if let Some(h) = &l.hash {
                    o["hash"] = serde_json::json!(h);
                }
                if !l.dependencies.is_empty() {
                    o["dependencies"] = serde_json::json!(l.dependencies);
                }
                o
            })
            .collect();
        let mut s = serde_json::to_string_pretty(
            &serde_json::json!({ "version": 1, "packages": packages }),
        )
        .expect("json");
        s.push('\n');
        s
    }
}

/// The packages of a build, prepared (decision 285): the lockfile, and where each locked package
/// is (its directory: a workspace member's, or a git checkout in the cache).
#[derive(Clone, Debug, Default)]
pub struct Prepared {
    pub root: PathBuf,
    pub lock: Lock,
    /// By id.
    pub dirs: BTreeMap<String, PathBuf>,
}

impl Prepared {
    /// The id of the package whose directory is `dir`.
    fn id_of(&self, dir: &Path) -> Option<&String> {
        self.dirs
            .iter()
            .find(|(_, d)| d.as_path() == dir)
            .map(|(id, _)| id)
    }

    /// The directory the dependency `spec` of the package at `pkg_dir` resolved to.
    pub fn dependency(&self, pkg_dir: &Path, spec: &str) -> Option<&PathBuf> {
        let id = self.id_of(pkg_dir)?;
        let dep = self.lock.packages.get(id)?.dependencies.get(spec)?;
        self.dirs.get(dep)
    }
}

fn reference(pin: &GitPin) -> String {
    match pin {
        GitPin::Tag(t) => format!("tag:{t}"),
        GitPin::Branch(b) => format!("branch:{b}"),
        GitPin::Rev(r) => format!("rev:{r}"),
        GitPin::Default => "default".into(),
    }
}

/// The workspace (or package) root for builds from `dir`: where `project.lock` lives.
pub fn lock_root(dir: &Path) -> Option<Manifest> {
    let m = package_of(dir)?.ok()?;
    Some(workspace_of(&m).unwrap_or(m))
}

/// Makes sure `project.lock` covers the packages a build from `dir` uses and that the cache has
/// their sources, matching their hashes: locked git packages are checked out of the cache (or
/// fetched, if allowed); new or changed dependencies are resolved when fetching is allowed (the
/// CLI), else it's an error (`ppl fetch` brings the lockfile up to date). `None` without a
/// project.
pub fn prepare(dir: &Path) -> Result<Option<Prepared>, String> {
    prepare_with(dir, fetching())
}

pub fn prepare_with(dir: &Path, fetch: bool) -> Result<Option<Prepared>, String> {
    let Some(root) = lock_root(dir) else {
        return Ok(None);
    };
    let lock_path = root.dir.join("project.lock");
    let old = Lock::read(&lock_path)?.unwrap_or_default();
    let mut new = Lock::default();
    let mut dirs = BTreeMap::new();
    // The packages to visit: the workspace's members (or the root package itself).
    let mut queue: Vec<(Manifest, String)> = Vec::new();
    let roots: Vec<PathBuf> = if root.workspace.is_empty() {
        vec![root.dir.clone()]
    } else {
        members(&root)
    };
    for d in roots {
        let Some(Ok(m)) = Manifest::read(&d) else {
            continue;
        };
        let rel = d
            .strip_prefix(&root.dir)
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| d.display().to_string());
        let l = Locked {
            name: m
                .name
                .clone()
                .unwrap_or_else(|| format!("local/{}", rel.replace('/', "-"))),
            version: m.version.as_ref().map_or("0.0.0".into(), |v| v.to_string()),
            source: LockSource::Path(if rel.is_empty() { ".".into() } else { rel }),
            hash: None,
            dependencies: BTreeMap::new(),
        };
        let id = l.id();
        dirs.insert(id.clone(), d.clone());
        new.packages.insert(id.clone(), l);
        queue.push((m, id));
    }
    // Compatible versions unify: (name, major) → the locked package.
    let mut by_major: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    while let Some((m, id)) = queue.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let deps: Vec<(String, Source)> = m
            .dependencies
            .iter()
            .chain(&m.dev_dependencies)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (spec, source) in deps {
            let dep_id = match &source {
                Source::Range(_) => {
                    // A workspace member (resolve finds it again on import).
                    let found = new
                        .packages
                        .values()
                        .find(|l| l.name == spec && matches!(l.source, LockSource::Path(_)));
                    match found {
                        Some(l) => l.id(),
                        None => continue,
                    }
                }
                Source::Git { url, pin } => {
                    let r = reference(pin);
                    let locked = old
                        .packages
                        .get(&id)
                        .and_then(|p| p.dependencies.get(&spec))
                        .and_then(|d| old.packages.get(d))
                        .filter(|l| {
                            matches!(&l.source, LockSource::Git { url: u, reference: rr, .. } if u == url && *rr == r)
                        })
                        .cloned();
                    let l = match locked {
                        Some(l) => {
                            let LockSource::Git { commit, .. } = &l.source else {
                                unreachable!("filtered")
                            };
                            let Some(dir) = crate::git::checkout(url, commit, fetch)? else {
                                return Err(format!(
                                    "`{spec}` ({url} at {}) isn't in the cache: run `ppl fetch`",
                                    &commit[..commit.len().min(12)]
                                ));
                            };
                            let hash = crate::git::content_hash(&dir)?;
                            if l.hash.as_deref() != Some(hash.as_str()) {
                                return Err(format!(
                                    "`{spec}`'s cached source ({}) doesn't match its hash in project.lock",
                                    dir.display()
                                ));
                            }
                            dirs.insert(l.id(), dir);
                            l
                        }
                        None if !fetch => {
                            return Err(format!(
                                "project.lock doesn't have `{spec}` ({url}, {}): run `ppl fetch`",
                                crate::git::describe(pin)
                            ));
                        }
                        None => {
                            let commit = crate::git::resolve(url, pin)?;
                            let dir = crate::git::checkout(url, &commit, true)?.expect("fetched");
                            let hash = crate::git::content_hash(&dir)?;
                            let dm = match Manifest::read(&dir) {
                                Some(Ok(dm)) => dm,
                                Some(Err(e)) => {
                                    return Err(format!(
                                        "`{spec}` ({url}): its project.json is invalid: {}",
                                        e.join("; ")
                                    ));
                                }
                                None => {
                                    return Err(format!("`{spec}` ({url}) has no project.json"));
                                }
                            };
                            if dm.name.as_deref() != Some(spec.as_str()) {
                                return Err(format!(
                                    "{url} is `{}`, not `{spec}`",
                                    dm.name.unwrap_or_else(|| "a package without a name".into())
                                ));
                            }
                            let Some(version) = dm.version.clone() else {
                                return Err(format!("`{spec}` ({url}) has no version"));
                            };
                            let l = Locked {
                                name: spec.clone(),
                                version: version.to_string(),
                                source: LockSource::Git {
                                    url: url.clone(),
                                    reference: r.clone(),
                                    commit,
                                },
                                hash: Some(hash),
                                dependencies: BTreeMap::new(),
                            };
                            dirs.insert(l.id(), dir);
                            l
                        }
                    };
                    let dep_id = l.id();
                    // One copy per major (decision 285).
                    let major = Version::parse(&l.version)
                        .map(|v| v.compat_key())
                        .unwrap_or_default();
                    match by_major.get(&(spec.clone(), major.clone())) {
                        Some(other) if *other != dep_id => {
                            return Err(format!(
                                "`{spec}` is required as {other} and as {dep_id}: versions of one major must be one (depend on the same tag)"
                            ));
                        }
                        _ => {
                            by_major.insert((spec.clone(), major), dep_id.clone());
                        }
                    }
                    if !new.packages.contains_key(&dep_id) {
                        let dir = dirs[&dep_id].clone();
                        new.packages.insert(dep_id.clone(), l.clone());
                        if let Some(Ok(dm)) = Manifest::read(&dir) {
                            queue.push((dm, dep_id.clone()));
                        }
                    } else if new.packages[&dep_id].source != l.source {
                        return Err(format!(
                            "`{dep_id}` comes from two sources: {:?} and {:?}",
                            new.packages[&dep_id].source, l.source
                        ));
                    }
                    dep_id
                }
            };
            if let Some(p) = new.packages.get_mut(&id) {
                p.dependencies.insert(spec.clone(), dep_id);
            }
        }
    }
    if fetch && new != old {
        std::fs::write(&lock_path, new.to_json())
            .map_err(|e| format!("cannot write {}: {e}", lock_path.display()))?;
    }
    Ok(Some(Prepared {
        root: root.dir.clone(),
        lock: new,
        dirs,
    }))
}
