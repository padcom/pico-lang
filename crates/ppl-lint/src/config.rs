//! `project.json`'s `"lint"` (spec §14.7): each rule `false` (off), `true` (its defaults) or
//! options.

use serde_json::Value;
use std::path::Path;

/// The constructs `blankLines` can ask a blank line before.
pub const CONSTRUCTS: &[&str] = &[
    "if",
    "switch",
    "match",
    "function",
    "method",
    "constructor",
    "getter",
    "setter",
];

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub naming: bool,
    pub unused: bool,
    /// The maximum cyclomatic complexity of a function.
    pub complexity: Option<u32>,
    /// The constructs that need a blank line before them.
    pub blank_lines: Option<Vec<String>>,
    pub docs: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            naming: true,
            unused: true,
            complexity: None,
            blank_lines: None,
            docs: false,
        }
    }
}

/// A problem with the configuration itself (reported once, not per file).
pub type ConfigError = String;

impl Config {
    /// The configuration for files in `dir`: the nearest `project.json` in it or above.
    pub fn for_dir(dir: &Path) -> (Config, Vec<ConfigError>) {
        let mut d = dir.canonicalize().ok();
        while let Some(cur) = d {
            if let Ok(text) = std::fs::read_to_string(cur.join("project.json")) {
                return match serde_json::from_str::<Value>(&text) {
                    Ok(json) => Config::from_json(json.get("lint")),
                    Err(e) => (Config::default(), vec![format!("project.json: {e}")]),
                };
            }
            d = cur.parent().map(Path::to_path_buf);
        }
        (Config::default(), Vec::new())
    }

    /// `"lint"`'s value (`None`: the defaults).
    pub fn from_json(lint: Option<&Value>) -> (Config, Vec<ConfigError>) {
        let mut c = Config::default();
        let mut errors = Vec::new();
        let Some(lint) = lint else {
            return (c, errors);
        };
        let Some(obj) = lint.as_object() else {
            return (c, vec!["\"lint\" must be an object".into()]);
        };
        for (rule, v) in obj {
            let mut bad = |what: &str| errors.push(format!("\"lint.{rule}\": {what}"));
            match (rule.as_str(), v) {
                ("naming", Value::Bool(b)) => c.naming = *b,
                ("unused", Value::Bool(b)) => c.unused = *b,
                ("docs", Value::Bool(b)) => c.docs = *b,
                ("complexity", Value::Bool(b)) => c.complexity = b.then_some(4),
                ("complexity", Value::Number(n)) => match n.as_u64() {
                    Some(n) if n >= 1 => c.complexity = Some(n as u32),
                    _ => bad("the maximum must be a whole number of at least 1"),
                },
                ("complexity", Value::Object(o)) => match o.get("max").and_then(Value::as_u64) {
                    Some(n) if n >= 1 => c.complexity = Some(n as u32),
                    _ => bad("`max` must be a whole number of at least 1"),
                },
                ("blankLines", Value::Bool(b)) => {
                    c.blank_lines = b.then(|| CONSTRUCTS.iter().map(|s| s.to_string()).collect());
                }
                ("blankLines", Value::Object(o)) => {
                    let mut list = Vec::new();
                    match o.get("before").and_then(Value::as_array) {
                        Some(items) => {
                            for i in items {
                                match i.as_str() {
                                    Some(s) if CONSTRUCTS.contains(&s) => list.push(s.to_string()),
                                    _ => bad(&format!(
                                        "`before` lists {i}; it can list {}",
                                        CONSTRUCTS.join(", ")
                                    )),
                                }
                            }
                        }
                        None => bad("`before` must be a list of constructs"),
                    }
                    c.blank_lines = Some(list);
                }
                ("naming" | "unused" | "docs", _) => bad("must be true or false"),
                ("complexity", _) => bad("must be true, false, a number or { \"max\": n }"),
                ("blankLines", _) => bad("must be true, false or { \"before\": [...] }"),
                _ => errors.push(format!(
                    "\"lint\": unknown rule `{rule}` (the rules: naming, unused, complexity, blankLines, docs)"
                )),
            }
        }
        (c, errors)
    }

    pub fn blank_before(&self, construct: &str) -> bool {
        self.blank_lines
            .as_ref()
            .is_some_and(|l| l.iter().any(|c| c == construct))
    }
}
