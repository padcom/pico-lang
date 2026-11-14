//! `project.json`'s `"lint"` (spec §14.7): each rule `false` (off), `true` (its defaults) or
//! options, and `"overrides"` that change rules for some files.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The constructs `blankLines` can ask a blank line before (`import` means after the
/// imports, `member` between class members after a multi-line one).
pub const CONSTRUCTS: &[&str] = &[
    "if",
    "switch",
    "match",
    "function",
    "method",
    "constructor",
    "getter",
    "setter",
    "return",
    "for",
    "while",
    "do",
    "loop",
    "try",
    "block",
    "class",
    "export",
    "import",
    "member",
];

/// The options a rule takes besides `true`/`false`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Opt {
    /// Only on or off.
    Flag,
    /// A maximum: a number, or `{ "max": n }`; `true` means `default`.
    Max(u32),
    /// One of the words; `true` means the first.
    Choice(&'static [&'static str]),
}

/// The rules beyond the original five: name, options, on by default.
pub const RULES: &[(&str, Opt, bool)] = &[
    ("explicitReturn", Opt::Flag, true),
    ("maxParams", Opt::Max(4), false),
    ("maxDepth", Opt::Max(2), false),
    ("maxLines", Opt::Max(32), false),
    ("maxNestedCallbacks", Opt::Max(2), false),
    ("maxStatementsPerLine", Opt::Max(2), false),
    ("maxLen", Opt::Max(130), false),
    ("shadow", Opt::Flag, false),
    ("unusedExpressions", Opt::Flag, false),
    ("unreachable", Opt::Flag, false),
    ("selfCompare", Opt::Flag, false),
    ("selfAssign", Opt::Flag, false),
    ("constantBinary", Opt::Flag, false),
    ("unsafeFinally", Opt::Flag, false),
    ("requireYield", Opt::Flag, false),
    ("forDirection", Opt::Flag, false),
    ("emptyBlock", Opt::Flag, false),
    ("dupeElseIf", Opt::Flag, false),
    ("uselessCatch", Opt::Flag, false),
    ("templateInString", Opt::Flag, false),
    ("loopFunc", Opt::Flag, false),
    ("floatingPromise", Opt::Flag, false),
    ("awaitInLoop", Opt::Flag, false),
    ("requireAwait", Opt::Flag, false),
    ("preferAwait", Opt::Flag, false),
    ("promiseReturnWrap", Opt::Flag, false),
    ("promiseParamNames", Opt::Flag, false),
    ("preferConst", Opt::Flag, false),
    ("objectShorthand", Opt::Flag, false),
    ("uselessRename", Opt::Flag, false),
    ("operatorAssignment", Opt::Flag, false),
    ("preferTemplate", Opt::Flag, false),
    ("uselessConcat", Opt::Flag, false),
    ("arrowBodyStyle", Opt::Flag, false),
    ("preferArrowCallback", Opt::Flag, false),
    ("funcStyle", Opt::Flag, false),
    ("lonelyIf", Opt::Flag, false),
    ("loneBlocks", Opt::Flag, false),
    ("uselessReturn", Opt::Flag, false),
    ("uselessConstructor", Opt::Flag, false),
    ("preferDestructuring", Opt::Flag, false),
    ("preferExponentiation", Opt::Flag, false),
    ("yoda", Opt::Flag, false),
    ("groupedAccessors", Opt::Flag, false),
    ("curly", Opt::Flag, false),
    ("extraParens", Opt::Flag, false),
    ("mixedOperators", Opt::Flag, false),
    ("commaDangle", Opt::Flag, false),
    ("braceStyle", Opt::Flag, false),
    ("operatorLinebreak", Opt::Flag, false),
    ("multilineTernary", Opt::Flag, false),
    ("spacedComment", Opt::Flag, false),
    ("extraSemi", Opt::Flag, false),
    ("quotes", Opt::Choice(&["single", "double"]), false),
    ("docTags", Opt::Flag, false),
    ("docFormat", Opt::Flag, false),
    ("underscoreDangle", Opt::Flag, false),
    ("preferNamedCaptureGroup", Opt::Flag, false),
];

/// The setting of a rule that's on: its maximum or the index of its choice (0 for flags).
pub type Setting = u32;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub naming: bool,
    pub unused: bool,
    /// The maximum cyclomatic complexity of a function.
    pub complexity: Option<u32>,
    /// The constructs that need a blank line before them.
    pub blank_lines: Option<Vec<String>>,
    pub docs: bool,
    /// The other rules that are on (see [`RULES`]).
    pub rules: BTreeMap<&'static str, Setting>,
    /// `"overrides"`: file patterns and the `"lint"` changes for the files they match.
    pub overrides: Vec<(Vec<String>, Value)>,
    /// The directory of the `project.json` (patterns are relative to it).
    pub root: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            naming: true,
            unused: true,
            complexity: None,
            blank_lines: None,
            docs: false,
            rules: RULES
                .iter()
                .filter(|(.., on)| *on)
                .map(|(r, o, _)| (*r, default_setting(*o)))
                .collect(),
            overrides: Vec::new(),
            root: None,
        }
    }
}

fn default_setting(o: Opt) -> Setting {
    match o {
        Opt::Max(n) => n,
        Opt::Flag | Opt::Choice(_) => 0,
    }
}

/// A problem with the configuration itself (reported once, not per file).
pub type ConfigError = String;

impl Config {
    /// Every rule on with its defaults (for the explanations' examples).
    pub fn all() -> Config {
        Config {
            complexity: Some(4),
            blank_lines: Some(CONSTRUCTS.iter().map(|s| s.to_string()).collect()),
            docs: true,
            rules: RULES
                .iter()
                .map(|(r, o, _)| (*r, default_setting(*o)))
                .collect(),
            ..Config::default()
        }
    }

    /// The setting of rule `rule` when it's on.
    pub fn on(&self, rule: &str) -> Option<Setting> {
        self.rules.get(rule).copied()
    }

    /// The configuration for files in `dir`: the nearest `project.json` in it or above.
    pub fn for_dir(dir: &Path) -> (Config, Vec<ConfigError>) {
        let mut d = dir.canonicalize().ok();
        while let Some(cur) = d {
            if let Ok(text) = std::fs::read_to_string(cur.join("project.json")) {
                return match serde_json::from_str::<Value>(&text) {
                    Ok(json) => {
                        let (mut c, e) = Config::from_json(json.get("lint"));
                        c.root = Some(cur);
                        (c, e)
                    }
                    Err(e) => (Config::default(), vec![format!("project.json: {e}")]),
                };
            }
            d = cur.parent().map(Path::to_path_buf);
        }
        (Config::default(), Vec::new())
    }

    /// The configuration for the file at `path`: its directory's, with the overrides whose
    /// patterns match it applied in order.
    pub fn for_file(path: &Path) -> (Config, Vec<ConfigError>) {
        let (c, errors) = Config::for_dir(path.parent().unwrap_or(Path::new(".")));
        (c.for_path(path), errors)
    }

    /// This configuration with the overrides that match `path` applied.
    pub fn for_path(&self, path: &Path) -> Config {
        let mut c = self.clone();
        let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let rel = self
            .root
            .as_ref()
            .and_then(|r| abs.strip_prefix(r).ok())
            .unwrap_or(&abs)
            .to_string_lossy()
            .replace('\\', "/");
        for (patterns, rules) in &self.overrides {
            if patterns.iter().any(|p| glob_match(p, &rel)) {
                c.apply(rules, &mut Vec::new());
            }
        }
        c
    }

    /// `"lint"`'s value (`None`: the defaults).
    pub fn from_json(lint: Option<&Value>) -> (Config, Vec<ConfigError>) {
        let mut c = Config::default();
        let mut errors = Vec::new();
        let Some(lint) = lint else {
            return (c, errors);
        };
        if !lint.is_object() {
            return (c, vec!["\"lint\" must be an object".into()]);
        }
        c.apply(lint, &mut errors);
        (c, errors)
    }

    /// Applies the rule settings of object `lint` (`"overrides"` only at the top).
    fn apply(&mut self, lint: &Value, errors: &mut Vec<ConfigError>) {
        let Some(obj) = lint.as_object() else {
            errors.push("\"lint\": an override's \"rules\" must be an object".into());
            return;
        };
        for (rule, v) in obj {
            let bad = |errors: &mut Vec<ConfigError>, what: &str| {
                errors.push(format!("\"lint.{rule}\": {what}"))
            };
            match (rule.as_str(), v) {
                ("naming", Value::Bool(b)) => self.naming = *b,
                ("unused", Value::Bool(b)) => self.unused = *b,
                ("docs", Value::Bool(b)) => self.docs = *b,
                ("complexity", Value::Bool(b)) => self.complexity = b.then_some(4),
                ("complexity", Value::Number(_) | Value::Object(_)) => match max_of(v) {
                    Some(n) => self.complexity = Some(n),
                    None => bad(errors, "the maximum must be a whole number of at least 1"),
                },
                ("blankLines", Value::Bool(b)) => {
                    self.blank_lines =
                        b.then(|| CONSTRUCTS.iter().map(|s| s.to_string()).collect());
                }
                ("blankLines", Value::Object(o)) => {
                    let mut list = Vec::new();
                    match o.get("before").and_then(Value::as_array) {
                        Some(items) => {
                            for i in items {
                                match i.as_str() {
                                    Some(s) if CONSTRUCTS.contains(&s) => list.push(s.to_string()),
                                    _ => bad(
                                        errors,
                                        &format!(
                                            "`before` lists {i}; it can list {}",
                                            CONSTRUCTS.join(", ")
                                        ),
                                    ),
                                }
                            }
                        }
                        None => bad(errors, "`before` must be a list of constructs"),
                    }
                    self.blank_lines = Some(list);
                }
                ("naming" | "unused" | "docs", _) => bad(errors, "must be true or false"),
                ("complexity", _) => bad(errors, "must be true, false, a number or { \"max\": n }"),
                ("blankLines", _) => bad(errors, "must be true, false or { \"before\": [...] }"),
                ("overrides", Value::Array(items)) => {
                    for i in items {
                        let files = match i.get("files") {
                            Some(Value::String(s)) => vec![s.clone()],
                            Some(Value::Array(a)) if a.iter().all(Value::is_string) => a
                                .iter()
                                .filter_map(|s| s.as_str().map(String::from))
                                .collect(),
                            _ => {
                                bad(
                                    errors,
                                    "each override needs \"files\": a pattern or a list of them",
                                );
                                continue;
                            }
                        };
                        match i.get("rules") {
                            Some(r) => {
                                // Validate now, apply per file later.
                                Config::default().apply(r, errors);
                                self.overrides.push((files, r.clone()));
                            }
                            None => errors
                                .push("\"lint.overrides\": each override needs \"rules\"".into()),
                        }
                    }
                }
                ("overrides", _) => bad(
                    errors,
                    "must be a list of { \"files\": …, \"rules\": { … } }",
                ),
                (name, v) => match RULES.iter().find(|(r, ..)| *r == name) {
                    Some((r, o, _)) => match setting(*o, v) {
                        Ok(Some(s)) => {
                            self.rules.insert(r, s);
                        }
                        Ok(None) => {
                            self.rules.remove(r);
                        }
                        Err(what) => bad(errors, &what),
                    },
                    None => errors.push(unknown_rule(rule)),
                },
            }
        }
    }

    pub fn blank_before(&self, construct: &str) -> bool {
        self.blank_lines
            .as_ref()
            .is_some_and(|l| l.iter().any(|c| c == construct))
    }
}

/// A rule's setting from its JSON value: `None` when it's off.
fn setting(o: Opt, v: &Value) -> Result<Option<Setting>, String> {
    match (o, v) {
        (_, Value::Bool(false)) => Ok(None),
        (_, Value::Bool(true)) => Ok(Some(default_setting(o))),
        (Opt::Max(_), Value::Number(_) | Value::Object(_)) => max_of(v)
            .map(Some)
            .ok_or_else(|| "the maximum must be a whole number of at least 1".into()),
        (Opt::Max(_), _) => Err("must be true, false, a number or { \"max\": n }".into()),
        (Opt::Choice(words), Value::String(s)) => match words.iter().position(|w| w == s) {
            Some(i) => Ok(Some(i as u32)),
            None => Err(format!(
                "must be true, false or one of {}",
                words.join(", ")
            )),
        },
        (Opt::Choice(words), _) => Err(format!(
            "must be true, false or one of {}",
            words.join(", ")
        )),
        (Opt::Flag, _) => Err("must be true or false".into()),
    }
}

/// The message for an unknown rule, with the closest rule's name when there's one.
fn unknown_rule(rule: &str) -> String {
    let all = [
        "naming",
        "unused",
        "complexity",
        "blankLines",
        "docs",
        "overrides",
    ]
    .into_iter()
    .chain(RULES.iter().map(|(r, ..)| *r));
    let close = all
        .map(|r| (distance(&rule.to_lowercase(), &r.to_lowercase()), r))
        .min()
        .filter(|(d, _)| *d <= 3);
    match close {
        Some((_, r)) => format!("\"lint\": unknown rule `{rule}` (did you mean `{r}`?)"),
        None => format!("\"lint\": unknown rule `{rule}` (the rules are listed in spec §14.7)"),
    }
}

/// The edit distance between two words.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

fn max_of(v: &Value) -> Option<u32> {
    let n = match v {
        Value::Object(o) => o.get("max")?.as_u64()?,
        v => v.as_u64()?,
    };
    (n >= 1).then_some(n as u32)
}

/// Whether `path` (relative, `/`-separated) matches `pattern`: `*` matches within a path
/// segment, `**` across them; a pattern without `/` matches the file name anywhere.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    if !pattern.contains('/') {
        let name = path.rsplit('/').next().unwrap_or(path);
        return glob(pattern.as_bytes(), name.as_bytes());
    }
    glob(pattern.trim_start_matches("./").as_bytes(), path.as_bytes())
}

fn glob(p: &[u8], s: &[u8]) -> bool {
    match p {
        [] => s.is_empty(),
        [b'*', b'*', rest @ ..] => {
            let rest = rest.strip_prefix(b"/").unwrap_or(rest);
            (0..=s.len()).any(|i| glob(rest, &s[i..]))
        }
        [b'*', rest @ ..] => (0..=s.len())
            .take_while(|&i| i == 0 || s[i - 1] != b'/')
            .any(|i| glob(rest, &s[i..])),
        [b'?', rest @ ..] => !s.is_empty() && s[0] != b'/' && glob(rest, &s[1..]),
        [c, rest @ ..] => !s.is_empty() && s[0] == *c && glob(rest, &s[1..]),
    }
}
