//! `c:` imports (spec §12.2, decision 262): a C header made into a pico module. Instead of an
//! embedded libclang, the compiler asks the clang it already uses for the header's AST as JSON
//! (`-Xclang -ast-dump=json`) and its macros (`-E -dM`), and generates:
//!
//! - `extern "C"` declarations for its functions, variadic ones too (not those taking or
//!   returning structs by value: C ABI lowering for those doesn't exist yet),
//! - `@pod` structs for complete structs whose fields all map,
//! - constants for enumerators and for `#define`s that are plain number or string literals.
//!
//! C types map as §12.1 says: `char`/`short`/`int`/`long`/`long long` (signed or not) to
//! `i8`…`u64` (`long` pointer-sized), `float`/`double`, `_Bool`, pointers to `ptr<T>` (`void *`
//! to `ptr<void>`, unknown pointees too), `const char *` parameters to `string`, function
//! pointers to `extern "C" (…) => R`, enums to `c_int`. Typedefs are seen through.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where a header comes from and how to build against it.
#[derive(Clone, Debug, Default)]
pub struct CConfig {
    /// `-I` directories (`project.json`'s `c.include`, pkg-config's).
    pub include: Vec<String>,
    /// Other compiler flags (pkg-config's `--cflags` besides `-I`).
    pub cflags: Vec<String>,
    /// Libraries to link (`c.link`, pkg-config's `-l`).
    pub link: Vec<String>,
}

/// `project.json`'s `"c": { "include": […], "link": […], "pkgConfig": […] }`, found in `dir`
/// or above it, with each pkg-config package's flags added.
pub fn config(dir: &Path) -> Result<CConfig, String> {
    let mut cfg = CConfig::default();
    let mut d = Some(pico_runtime::canonical(dir).unwrap_or_else(|_| dir.to_path_buf()));
    let mut found = None;
    while let Some(cur) = d {
        let p = cur.join("project.json");
        if p.is_file() {
            found = Some((cur.clone(), p));
            break;
        }
        d = cur.parent().map(Path::to_path_buf);
    }
    let Some((root, path)) = found else {
        return Ok(cfg);
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let json: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} isn't valid JSON: {e}", path.display()))?;
    let list = |k: &str| -> Vec<String> {
        json.get("c")
            .and_then(|c| c.get(k))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    for inc in list("include") {
        let p = Path::new(&inc);
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        };
        cfg.include.push(p.display().to_string());
    }
    cfg.link = list("link");
    for pkg in list("pkgConfig") {
        let run = |flag: &str| -> Result<String, String> {
            let out = Command::new("pkg-config")
                .args([flag, &pkg])
                .output()
                .map_err(|e| format!("cannot run pkg-config for `{pkg}`: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "pkg-config doesn't know `{pkg}`: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        };
        for f in run("--cflags")?.split_whitespace() {
            match f.strip_prefix("-I") {
                Some(d) => cfg.include.push(d.to_string()),
                None => cfg.cflags.push(f.to_string()),
            }
        }
        for f in run("--libs-only-l")?.split_whitespace() {
            if let Some(l) = f.strip_prefix("-l") {
                cfg.link.push(l.to_string());
            }
        }
    }
    Ok(cfg)
}

/// The library directories for `target` (spec §12.1): `-L`'s, then `project.json`'s
/// `"c": { "libDirs": […] }` (found in `dir` or above), a list for every target or a map from
/// target names (`windows-x64`, a triple, …) to lists. Relative paths are from the project.
pub fn lib_dirs(dir: &Path, target: crate::Target) -> Vec<PathBuf> {
    let mut out = crate::link::cli_lib_dirs();
    let Some((root, json)) = project_json(dir) else {
        return out;
    };
    let same = |name: &str| {
        crate::Target::parse(name)
            .is_ok_and(|t| (t.arch, t.os, t.env) == (target.arch, target.os, target.env))
    };
    let lists: Vec<&Value> = match json.get("c").and_then(|c| c.get("libDirs")) {
        Some(Value::Array(_)) => vec![&json["c"]["libDirs"]],
        Some(Value::Object(m)) => m.iter().filter(|(k, _)| same(k)).map(|(_, v)| v).collect(),
        _ => Vec::new(),
    };
    for list in lists {
        for d in list
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let p = Path::new(d);
            out.push(if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            });
        }
    }
    out
}

/// The nearest `project.json` at `dir` or above: its directory and contents (`None` if
/// there's none or it isn't valid JSON).
fn project_json(dir: &Path) -> Option<(PathBuf, Value)> {
    let start = pico_runtime::canonical(dir).unwrap_or_else(|_| dir.to_path_buf());
    let root = start
        .ancestors()
        .find(|d| d.join("project.json").is_file())?
        .to_path_buf();
    let text = std::fs::read_to_string(root.join("project.json")).ok()?;
    Some((root, serde_json::from_str(&text).ok()?))
}

/// The pico module for `c:<header>` (a system header like `math.h`, or `./x.h` relative to
/// `dir`).
pub fn generate(header: &str, dir: &Path, cfg: &CConfig) -> Result<String, String> {
    let cc = crate::link::clang()?;
    let include = if header.starts_with("./") || header.starts_with("../") {
        let p = dir.join(header);
        let p =
            pico_runtime::canonical(&p).map_err(|e| format!("cannot find {}: {e}", p.display()))?;
        format!("#include \"{}\"\n", p.display())
    } else {
        format!("#include <{header}>\n")
    };
    let tmp = std::env::temp_dir().join(format!(
        "pico-cheader-{}-{}.c",
        std::process::id(),
        header.replace(['/', '.'], "_")
    ));
    std::fs::write(&tmp, &include).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    let base = |c: &mut Command| {
        c.arg("-x").arg("c").arg("-std=gnu17");
        for i in &cfg.include {
            c.arg(format!("-I{i}"));
        }
        c.args(&cfg.cflags);
    };
    let mut ast = cc.command();
    base(&mut ast);
    ast.args(["-fsyntax-only", "-Xclang", "-ast-dump=json"])
        .arg(&tmp);
    let out = ast.output().map_err(|e| format!("cannot run clang: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "clang can't read `{header}`:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let json: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("clang's AST of `{header}`: {e}"))?;
    let mut macros = cc.command();
    base(&mut macros);
    macros.args(["-E", "-dM"]).arg(&tmp);
    let defs = macros
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    // The compiler's own macros (`linux`, `__x86_64__`) aren't the header's.
    let empty = std::env::temp_dir().join(format!("pico-cheader-{}-empty.c", std::process::id()));
    let _ = std::fs::write(&empty, "");
    let mut builtin = cc.command();
    base(&mut builtin);
    builtin.args(["-E", "-dM"]).arg(&empty);
    let predefined: BTreeSet<String> = builtin
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            l.strip_prefix("#define ")?
                .split(' ')
                .next()
                .map(String::from)
        })
        .collect();
    let _ = std::fs::remove_file(&empty);
    let defs: String = defs
        .lines()
        .filter(|l| {
            l.strip_prefix("#define ")
                .and_then(|r| r.split(' ').next())
                .is_some_and(|n| !predefined.contains(n))
        })
        .map(|l| format!("{l}\n"))
        .collect();
    let _ = std::fs::remove_file(&tmp);
    let text = Gen::default().module(header, &json, &defs, &cfg.link);
    // (`PICO_DUMP_C=1` shows what a header became.)
    if std::env::var_os("PICO_DUMP_C").is_some() {
        eprintln!("──── c:{header}\n{text}");
    }
    Ok(text)
}

#[derive(Default)]
struct Gen {
    /// Complete structs whose fields map: name → fields (name, pico type).
    structs: BTreeMap<String, Vec<(String, String)>>,
    /// Every name the module declares (C's separate tag namespace can't clash here).
    used: BTreeSet<String>,
    /// Typedefs: name → the type it stands for.
    typedefs: BTreeMap<String, String>,
}

/// What a C type is in pico.
#[derive(Clone, Debug, PartialEq)]
enum CTy {
    /// A pico type spelled out.
    Ty(String),
    Void,
    /// A struct by value (not supported in signatures yet).
    Struct(String),
    /// A `const char *` (a `string` parameter).
    ConstStr,
}

impl Gen {
    fn module(mut self, header: &str, json: &Value, defs: &str, link: &[String]) -> String {
        let decls: Vec<&Value> = json
            .get("inner")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|d| !d["isImplicit"].as_bool().unwrap_or(false))
                    .collect()
            })
            .unwrap_or_default();
        // (Implicit ones too: clang's own `__size_t`.)
        for d in json
            .get("inner")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if d["kind"] == "TypedefDecl"
                && let Some(name) = d["name"].as_str()
            {
                self.typedefs.insert(name.to_string(), c_type(d));
            }
        }
        // Structs first: their names make pointers to them typed.
        let mut pending: Vec<(String, &Value)> = Vec::new();
        for d in &decls {
            if d["kind"] == "RecordDecl"
                && d["tagUsed"] == "struct"
                && d["completeDefinition"].as_bool().unwrap_or(false)
                && let Some(name) = d["name"].as_str()
                && ok_name(name)
            {
                pending.push((name.to_string(), d));
            }
        }
        // (A struct whose fields name structs that don't map doesn't map either: iterate.)
        let names: BTreeSet<String> = pending.iter().map(|(n, _)| n.clone()).collect();
        let mut known: BTreeSet<String> = names.clone();
        loop {
            let mut changed = false;
            self.structs.clear();
            for (name, d) in &pending {
                if !known.contains(name) {
                    continue;
                }
                match self.fields(d, &known) {
                    Some(fs) => {
                        self.structs.insert(name.clone(), fs);
                    }
                    None => {
                        known.remove(name);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        let mut out =
            format!("// Generated by pico from the C header `{header}` (decision 262).\n\n");
        let mut fns = String::new();
        let mut consts = String::new();
        let mut seen_fns: BTreeSet<String> = BTreeSet::new();
        for d in &decls {
            match d["kind"].as_str() {
                Some("FunctionDecl") => {
                    let Some(name) = d["name"].as_str() else {
                        continue;
                    };
                    if !ok_name(name) || !seen_fns.insert(name.to_string()) {
                        continue;
                    }
                    if let Some(sig) = self.function(d) {
                        let _ = writeln!(fns, "  function {name}{sig}");
                        self.used.insert(name.to_string());
                    }
                }
                Some("EnumDecl") => self.enumerators(d, &mut consts),
                _ => {}
            }
        }
        for (name, fields) in &self.structs {
            if self.used.contains(name) {
                continue;
            }
            let fs: Vec<String> = fields.iter().map(|(n, t)| format!("  {n}: {t}")).collect();
            let _ = writeln!(out, "@pod export struct {name} {{\n{}\n}}\n", fs.join("\n"));
        }
        for l in link {
            let _ = writeln!(out, "@link({l:?})");
        }
        let _ = writeln!(out, "export extern \"C\" {{\n{fns}}}\n");
        out.push_str(&consts);
        let macro_consts = self.macros(defs);
        out.push_str(&macro_consts);
        out
    }

    /// A struct's fields, if all of them map.
    fn fields(&self, d: &Value, known: &BTreeSet<String>) -> Option<Vec<(String, String)>> {
        let mut out = Vec::new();
        for f in d["inner"].as_array()? {
            if f["kind"] != "FieldDecl" {
                continue;
            }
            if f["isBitfield"].as_bool().unwrap_or(false) {
                return None;
            }
            let name = f["name"].as_str()?;
            let t = c_type(f);
            let ty = self.field_type(&t, known)?;
            out.push((pico_field(name), ty));
        }
        (!out.is_empty()).then_some(out)
    }

    fn field_type(&self, t: &str, known: &BTreeSet<String>) -> Option<String> {
        let t = clean(t);
        // A fixed array.
        if let Some(open) = t.rfind('[')
            && t.ends_with(']')
        {
            let n: u64 = t[open + 1..t.len() - 1].trim().parse().ok()?;
            let elem = self.field_type(&t[..open], known)?;
            return Some(format!("[{elem}; {n}]"));
        }
        match self.map(&t, false, known) {
            CTy::Ty(s) => Some(s),
            CTy::Struct(s) if known.contains(&s) => Some(s),
            CTy::ConstStr => Some("ptr<u8>".into()),
            _ => None,
        }
    }

    /// `(params): R` for a function, if its signature maps.
    fn function(&self, d: &Value) -> Option<String> {
        let ft = d["type"]["qualType"].as_str()?;
        // A C variadic function ends with `...` (spec §12.1); a variadic function pointer
        // among the parameters doesn't map.
        let variadic = d["variadic"] == true;
        if !variadic && ft.contains("...") {
            return None;
        }
        let known: BTreeSet<String> = self.structs.keys().cloned().collect();
        let ret_c = ft.split('(').next()?.trim().to_string();
        let ret = match self.map(&ret_c, false, &known) {
            CTy::Void => "void".to_string(),
            CTy::Ty(s) => s,
            CTy::ConstStr => "ptr<u8>".into(),
            CTy::Struct(_) => return None,
        };
        let mut params = Vec::new();
        let empty = Vec::new();
        for (i, p) in d["inner"]
            .as_array()
            .unwrap_or(&empty)
            .iter()
            .filter(|p| p["kind"] == "ParmVarDecl")
            .enumerate()
        {
            let t = c_type(p);
            let ty = match self.map(&t, true, &known) {
                CTy::Ty(s) => s,
                CTy::ConstStr => "string".into(),
                _ => return None,
            };
            let name = p["name"]
                .as_str()
                .map(pico_field)
                .unwrap_or_else(|| format!("a{i}"));
            params.push(format!("{name}: {ty}"));
        }
        if variadic {
            params.push("...".into());
        }
        Some(format!("({}): {ret}", params.join(", ")))
    }

    /// A C type (as clang spells it, typedefs seen through) in pico.
    fn map(&self, t: &str, param: bool, known: &BTreeSet<String>) -> CTy {
        let raw = t.trim();
        let fp = strip_quals(raw);
        // Function pointers: `R (*)(A, B)`.
        if let Some(at) = fp.find("(*)") {
            let t = fp.clone();
            let ret = t[..at].trim();
            let rest = t[at + 3..].trim();
            let inner = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')'));
            let Some(inner) = inner else {
                return CTy::Ty("ptr<void>".into());
            };
            if inner.contains("...") {
                return CTy::Ty("ptr<void>".into());
            }
            let mut ps = Vec::new();
            if inner.trim() != "void" && !inner.trim().is_empty() {
                for (i, p) in split_top(inner).iter().enumerate() {
                    match self.map(p, false, known) {
                        CTy::Ty(s) => ps.push(format!("a{i}: {s}")),
                        CTy::ConstStr => ps.push(format!("a{i}: ptr<u8>")),
                        _ => return CTy::Ty("ptr<void>".into()),
                    }
                }
            }
            let r = match self.map(ret, false, known) {
                CTy::Void => "void".to_string(),
                CTy::Ty(s) => s,
                CTy::ConstStr => "ptr<u8>".into(),
                CTy::Struct(_) => return CTy::Ty("ptr<void>".into()),
            };
            return CTy::Ty(format!("extern \"C\" ({}) => {r}", ps.join(", ")));
        }
        let t = clean(raw);
        if t == "void" {
            return CTy::Void;
        }
        // Arrays as parameters are pointers.
        if let Some(open) = t.rfind('[')
            && t.ends_with(']')
        {
            return match self.map(&t[..open], false, known) {
                CTy::Ty(s) => CTy::Ty(format!("ptr<{s}>")),
                _ => CTy::Ty("ptr<void>".into()),
            };
        }
        if let Some(inner) = t.strip_suffix('*') {
            // (Qualifiers after the last `*` belong to the pointer: `const char *restrict`.)
            let inner_raw = raw.rfind('*').map_or(raw, |i| &raw[..i]);
            let inner = inner.trim();
            if param && inner == "char" && inner_raw.contains("const") && !inner_raw.contains('*') {
                return CTy::ConstStr;
            }
            // C strings are bytes (`string.fromC` takes `ptr<u8>`).
            if inner == "char" {
                return CTy::Ty("ptr<u8>".into());
            }
            return match self.map(inner, false, known) {
                CTy::Ty(s) if !s.starts_with("extern") => CTy::Ty(format!("ptr<{s}>")),
                CTy::Struct(s) if known.contains(&s) => CTy::Ty(format!("ptr<{s}>")),
                CTy::ConstStr => CTy::Ty("ptr<ptr<u8>>".into()),
                _ => CTy::Ty("ptr<void>".into()),
            };
        }
        if let Some(s) = t.strip_prefix("struct ") {
            return CTy::Struct(s.trim().to_string());
        }
        if t.starts_with("enum ") {
            return CTy::Ty("c_int".into());
        }
        if t.starts_with("union ") {
            return CTy::Struct(String::new());
        }
        let prim = match t.as_str() {
            "char" | "signed char" => "i8",
            "unsigned char" => "u8",
            "short" | "short int" | "signed short" => "i16",
            "unsigned short" | "unsigned short int" => "u16",
            "int" | "signed" | "signed int" => "i32",
            "unsigned int" | "unsigned" => "u32",
            "long" | "long int" | "signed long" => "isize",
            "unsigned long" | "unsigned long int" => "usize",
            "long long" | "long long int" | "signed long long" => "i64",
            // (clang's own typedefs for `size_t`'s and `ptrdiff_t`'s types.)
            "__size_t" => "usize",
            "__ptrdiff_t" => "isize",
            "unsigned long long" | "unsigned long long int" => "u64",
            "float" => "f32",
            "double" => "f64",
            "_Bool" | "bool" => "bool",
            _ => {
                // A typedef clang didn't see through (in function types).
                if let Some(under) = self.typedefs.get(&t)
                    && under != &t
                {
                    return self.map(&under.clone(), param, known);
                }
                if known.contains(&t) {
                    return CTy::Struct(t);
                }
                return CTy::Struct(String::new());
            }
        };
        CTy::Ty(prim.into())
    }

    /// `export const` per enumerator (`c_int`; values count on from the previous one).
    fn enumerators(&mut self, d: &Value, out: &mut String) {
        let mut next: i64 = 0;
        for e in d["inner"].as_array().into_iter().flatten() {
            if e["kind"] != "EnumConstantDecl" {
                continue;
            }
            let Some(name) = e["name"].as_str() else {
                continue;
            };
            let value = e["inner"]
                .as_array()
                .and_then(|i| i.iter().find_map(|c| c["value"].as_str()))
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(next);
            next = value.wrapping_add(1);
            if !ok_name(name) || !self.used.insert(name.to_string()) {
                continue;
            }
            let _ = writeln!(out, "export const {name}: c_int = {value}");
        }
    }

    /// Constants for object-like macros whose value is a number or string literal.
    fn macros(&mut self, defs: &str) -> String {
        let mut out = String::new();
        for line in defs.lines() {
            let Some(rest) = line.strip_prefix("#define ") else {
                continue;
            };
            let (name, value) = match rest.split_once(' ') {
                Some((n, v)) => (n, v.trim()),
                None => continue,
            };
            if name.contains('(') || !ok_name(name) || self.used.contains(name) {
                continue;
            }
            let Some((ty, lit)) = literal(value) else {
                continue;
            };
            self.used.insert(name.to_string());
            let _ = writeln!(out, "export const {name}: {ty} = {lit}");
        }
        out
    }
}

/// A clang type with qualifiers dropped (`const`, `volatile`, `restrict`, nullability) and
/// whitespace collapsed.
fn strip_quals(t: &str) -> String {
    const QUALS: &[&str] = &[
        "const",
        "volatile",
        "restrict",
        "__restrict",
        "_Nonnull",
        "_Nullable",
        "_Null_unspecified",
    ];
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if !word.is_empty() && !QUALS.contains(&word.as_str()) {
            out.push_str(word);
        }
        word.clear();
    };
    for c in t.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// [`strip_quals`], with pointer stars spaced uniformly (`char **` → `char * *`).
fn clean(t: &str) -> String {
    let mut out = String::new();
    let mut prev_alnum = false;
    for c in strip_quals(t).chars() {
        if c == '*' {
            out.push_str(" *");
            prev_alnum = false;
        } else if c == ' ' {
            if prev_alnum {
                out.push(' ');
            }
            prev_alnum = false;
        } else {
            out.push(c);
            prev_alnum = c.is_ascii_alphanumeric() || c == '_';
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A declaration's type as clang desugars it (typedefs seen through).
fn c_type(d: &Value) -> String {
    let t = &d["type"];
    t["desugaredQualType"]
        .as_str()
        .or_else(|| t["qualType"].as_str())
        .unwrap_or("void")
        .to_string()
}

/// Comma-separated parts at the top level (not inside parentheses).
fn split_top(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// A macro's value as a pico literal and its type.
fn literal(v: &str) -> Option<(String, String)> {
    let mut v = v.trim();
    while let Some(inner) = v.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        v = inner.trim();
    }
    if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 && !v[1..v.len() - 1].contains('"') {
        let body = &v[1..v.len() - 1];
        if body.contains('\\') && !body.contains("\\n") {
            return None;
        }
        return Some(("string".into(), v.to_string()));
    }
    let (neg, digits) = match v.strip_prefix('-') {
        Some(d) => (true, d.trim()),
        None => (false, v),
    };
    if digits.is_empty() || !digits.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    let lower = digits.to_ascii_lowercase();
    let is_hex = lower.starts_with("0x");
    let is_float = !is_hex && (lower.contains('.') || lower.contains('e'));
    if is_float {
        let body = lower.trim_end_matches(['f', 'l']);
        let x: f64 = body.parse().ok()?;
        let ty = if lower.ends_with('f') { "f32" } else { "f64" };
        let x = if neg { -x } else { x };
        let mut s = format!("{x:?}");
        if !s.contains('.') && !s.contains('e') && !s.contains("inf") && !s.contains("NaN") {
            s.push_str(".0");
        }
        return Some((ty.into(), s));
    }
    let unsigned = lower.contains('u');
    let body = lower.trim_end_matches(['u', 'l']);
    let n: u64 = if let Some(h) = body.strip_prefix("0x") {
        u64::from_str_radix(h, 16).ok()?
    } else if body.len() > 1 && body.starts_with('0') {
        u64::from_str_radix(&body[1..], 8).ok()?
    } else {
        body.parse().ok()?
    };
    let ty = if neg {
        if n <= i32::MAX as u64 + 1 {
            "i32"
        } else {
            "i64"
        }
    } else if unsigned {
        if n <= u32::MAX as u64 { "u32" } else { "u64" }
    } else if n <= i32::MAX as u64 {
        "i32"
    } else if n <= u32::MAX as u64 && is_hex {
        "u32"
    } else if n <= i64::MAX as u64 {
        "i64"
    } else {
        "u64"
    };
    let lit = if neg { format!("-{n}") } else { n.to_string() };
    Some((ty.into(), lit))
}

const PICO_KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "comptime",
    "const",
    "continue",
    "decorator",
    "default",
    "distinct",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "extern",
    "false",
    "final",
    "finally",
    "for",
    "function",
    "get",
    "if",
    "immutable",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "is",
    "let",
    "loop",
    "match",
    "new",
    "null",
    "operator",
    "override",
    "private",
    "protected",
    "public",
    "readonly",
    "return",
    "set",
    "static",
    "struct",
    "super",
    "switch",
    "tailcall",
    "this",
    "throw",
    "throws",
    "true",
    "try",
    "typeof",
    "unsafe",
    "using",
    "virtual",
    "where",
    "while",
    "yield",
    "isize",
    "usize",
    "ptr",
    "string",
    "int",
    "number",
    "bool",
    "char",
    "void",
    "never",
    "type",
    "of",
];

/// A name the module can declare as it is (C's reserved `_X`/`__x` names and pico's keywords
/// are left out).
fn ok_name(n: &str) -> bool {
    !n.is_empty()
        && !n.starts_with('_')
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !PICO_KEYWORDS.contains(&n)
}

/// A field or parameter name (keywords get a `_`).
fn pico_field(n: &str) -> String {
    if PICO_KEYWORDS.contains(&n) || n.starts_with("__") {
        format!("{n}_")
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals() {
        assert_eq!(literal("0x00000020u"), Some(("u32".into(), "32".into())));
        assert_eq!(literal("(-1)"), Some(("i32".into(), "-1".into())));
        assert_eq!(
            literal("3.14159265358979323846"),
            Some(("f64".into(), "3.141592653589793".into()))
        );
        assert_eq!(literal("1.5f"), Some(("f32".into(), "1.5".into())));
        assert_eq!(literal("010"), Some(("i32".into(), "8".into())));
        assert_eq!(
            literal("\"1.3.1\""),
            Some(("string".into(), "\"1.3.1\"".into()))
        );
        assert_eq!(literal("FOO"), None);
        assert_eq!(literal("(1 << 3)"), None);
    }

    #[test]
    fn types() {
        let g = Gen::default();
        let k = BTreeSet::from(["Rect".to_string()]);
        let m = |t: &str, p: bool| g.map(t, p, &k);
        assert_eq!(m("const char *", true), CTy::ConstStr);
        assert_eq!(m("char *", true), CTy::Ty("ptr<u8>".into()));
        assert_eq!(m("const char *", false), CTy::Ty("ptr<u8>".into()));
        assert_eq!(m("const char *restrict", true), CTy::ConstStr);
        assert_eq!(m("char *const", true), CTy::Ty("ptr<u8>".into()));
        assert_eq!(m("unsigned long", false), CTy::Ty("usize".into()));
        assert_eq!(m("struct Rect *", false), CTy::Ty("ptr<Rect>".into()));
        assert_eq!(m("struct Opaque *", false), CTy::Ty("ptr<void>".into()));
        assert_eq!(m("const void *", false), CTy::Ty("ptr<void>".into()));
        assert_eq!(
            m("int (*)(const void *, const void *)", false),
            CTy::Ty("extern \"C\" (a0: ptr<void>, a1: ptr<void>) => i32".into())
        );
        assert_eq!(m("struct Rect", false), CTy::Struct("Rect".into()));
    }
}
