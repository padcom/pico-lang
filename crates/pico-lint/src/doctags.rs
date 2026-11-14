//! Doc comments (P10.2i), after the author's `jsdoc` rules with TypeScript's overrides.
//!
//! `docTags`, on documented functions and methods: `@param` names the parameters, in order,
//! once each, with a description, and every parameter has one; `@returns` (with a
//! description) when the declared result isn't `void`, `never` or `Promise<void>`, and not
//! when it is; `@yields` exactly on generators; no `{types}` or `[defaults]` in tags (the
//! signature has them); known tag names; tags that take nothing (`@deprecated` aside) are
//! empty; `@access` and `@version`/`@since` values; `@implements` only on classes. A doc
//! comment with `@inheritdoc` is exempt.
//!
//! `docFormat`, on `/** … */` blocks: each line's `*` under the opening one, no doubled `*`,
//! no text on the opening or closing line of a multi-line block, no extra indentation after
//! `*` (outside `@example`), and (in `///` comments too) one blank line between the
//! description and the tags and none between tags.

use crate::cx::Cx;
use crate::walk::{self, Visit};
use pico_diag::{Diagnostic, Span};
use pico_syntax::ast::*;

const KNOWN: &[&str] = &[
    "param",
    "arg",
    "argument",
    "returns",
    "return",
    "yields",
    "yield",
    "throws",
    "exception",
    "example",
    "deprecated",
    "see",
    "since",
    "author",
    "todo",
    "version",
    "license",
    "copyright",
    "file",
    "fileoverview",
    "overview",
    "module",
    "remarks",
    "privateRemarks",
    "defaultValue",
    "typeParam",
    "template",
    "internal",
    "experimental",
    "beta",
    "alpha",
    "inheritdoc",
    "inheritDoc",
    "link",
    "linkcode",
    "linkplain",
    "access",
    "private",
    "protected",
    "public",
    "package",
    "readonly",
    "override",
    "async",
    "abstract",
    "static",
    "ignore",
    "hideconstructor",
    "implements",
    "summary",
    "description",
    "desc",
    "tutorial",
    "fires",
    "listens",
    "event",
    "requires",
    "variation",
    "category",
];

/// Tags that take no text.
const EMPTY: &[&str] = &[
    "abstract",
    "async",
    "hideconstructor",
    "ignore",
    "inheritdoc",
    "inheritDoc",
    "internal",
    "override",
    "readonly",
    "package",
    "private",
    "protected",
    "public",
    "static",
    "beta",
    "alpha",
    "experimental",
];

pub fn check(cx: &Cx, out: &mut Vec<Diagnostic>) {
    if cx.on("docFormat").is_some() {
        format(cx, out);
    }
    if cx.on("docTags").is_some() {
        let mut d = Docs { cx, out };
        for s in &cx.module.stmts {
            d.stmt(s);
        }
    }
}

/// A doc comment's lines: (the text after the `*`/`///` prefix, the offset where it starts).
fn doc_lines(cx: &Cx, comments: &[Span]) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    for c in comments {
        let text = cx.src(*c);
        if let Some(body) = text.strip_prefix("///") {
            let skip = body.len() - body.trim_start().len();
            out.push((body.trim().to_string(), c.start + 3 + skip as u32));
            continue;
        }
        let inner = text
            .strip_prefix("/**")
            .and_then(|t| t.strip_suffix("*/"))
            .unwrap_or("");
        let mut off = c.start + 3;
        for line in inner.split('\n') {
            let t = line.trim_start();
            let t2 = t.strip_prefix('*').unwrap_or(t);
            let lead = line.len() - t2.len();
            let skip = t2.len() - t2.trim_start().len();
            out.push((t2.trim().to_string(), off + (lead + skip) as u32));
            off += line.len() as u32 + 1;
        }
    }
    out
}

/// The doc comment directly above `start` (decorators in between are skipped): its
/// comments (one `/** */` block, or a run of `///` lines).
fn doc_above(cx: &Cx, start: u32) -> Vec<Span> {
    let mut i = cx.comments.partition_point(|c| c.end <= start);
    let mut found = Vec::new();
    let mut limit = start;
    while i > 0 {
        let c = cx.comments[i - 1];
        let gap = &cx.text[c.end as usize..limit as usize];
        let only_decorators = gap
            .split('\n')
            .all(|l| l.trim().is_empty() || l.trim().starts_with('@'));
        if !only_decorators || gap.matches('\n').count() > 1 + gap.matches('@').count() {
            break;
        }
        let t = cx.src(c);
        if t.starts_with("///") {
            found.insert(0, c);
            limit = c.start;
            i -= 1;
            continue;
        }
        if t.starts_with("/**") && !t.starts_with("/**/") && found.is_empty() {
            found.push(c);
        }
        break;
    }
    found
}

struct Tag {
    name: String,
    text: String,
    at: Span,
}

fn tags(cx: &Cx, lines: &[(String, u32)]) -> Vec<Tag> {
    let mut out: Vec<Tag> = Vec::new();
    for (l, off) in lines {
        if let Some(rest) = l.strip_prefix('@') {
            let name: String = rest.chars().take_while(|c| c.is_alphanumeric()).collect();
            let text = rest[name.len()..].trim().to_string();
            out.push(Tag {
                at: cx.span(*off, *off + 1 + name.len() as u32),
                name,
                text,
            });
        } else if let Some(t) = out.last_mut()
            && !l.is_empty()
        {
            t.text.push(' ');
            t.text.push_str(l);
        }
    }
    out
}

fn format(cx: &Cx, out: &mut Vec<Diagnostic>) {
    let mut warn = |msg: &str, span: Span, label: &str, help: &str| {
        out.push(
            cx.warn("docFormat", msg, span, label)
                .help(help.to_string()),
        );
    };
    for c in cx.comments.clone() {
        let t = cx.src(c);
        if !t.starts_with("/**") || t.starts_with("/**/") || !t.contains('\n') {
            continue;
        }
        let col = c.start - cx.line_start(cx.line(c.start));
        let lines: Vec<&str> = t.split('\n').collect();
        let first = lines[0].trim_start_matches("/**").trim();
        if !first.is_empty() {
            warn(
                "a multi-line doc comment starts on the line after `/**`",
                cx.span(c.start, c.start + lines[0].len() as u32),
                "text on the `/**` line",
                "break the line after `/**`",
            );
        }
        let mut off = c.start + lines[0].len() as u32 + 1;
        let mut in_example = false;
        for (i, line) in lines.iter().enumerate().skip(1) {
            let lspan = cx.span(off, off + line.len() as u32);
            off += line.len() as u32 + 1;
            let trimmed = line.trim_start();
            let indent = (line.len() - trimmed.len()) as u32;
            let last = i + 1 == lines.len();
            if last {
                if trimmed != "*/" {
                    warn(
                        "a multi-line doc comment ends with `*/` on a line of its own",
                        lspan,
                        "text before `*/`",
                        "break the line before `*/`",
                    );
                }
                if indent != col + 1 && trimmed.starts_with('*') {
                    warn(
                        "the `*` isn't under the first `*` of `/**`",
                        lspan,
                        "misaligned",
                        "align it",
                    );
                }
                continue;
            }
            if !trimmed.starts_with('*') {
                continue;
            }
            if indent != col + 1 {
                warn(
                    "the `*` isn't under the first `*` of `/**`",
                    lspan,
                    "misaligned",
                    "align it",
                );
            }
            let after = &trimmed[1..];
            if after.starts_with('*') {
                warn(
                    "a doc comment line starts with more than one `*`",
                    lspan,
                    "extra `*`",
                    "keep one `*`",
                );
            }
            let text = after.trim_start();
            if text.starts_with("@example") {
                in_example = true;
            } else if text.starts_with('@') {
                in_example = false;
            }
            if !in_example && after.starts_with("  ") && !text.is_empty() {
                warn(
                    "a doc comment line has extra indentation after `*`",
                    lspan,
                    "indented",
                    "keep one space after `*`",
                );
            }
        }
    }
    // tag-lines, on every doc comment (`/** */` and runs of `///`).
    let mut i = 0;
    let comments = cx.comments.clone();
    while i < comments.len() {
        let t = cx.src(comments[i]);
        let mut group = vec![comments[i]];
        if t.starts_with("///") {
            while i + 1 < comments.len()
                && cx.src(comments[i + 1]).starts_with("///")
                && cx.line(comments[i + 1].start) == cx.line(group.last().expect("one").start) + 1
            {
                i += 1;
                group.push(comments[i]);
            }
        } else if !t.starts_with("/**") || t.starts_with("/**/") {
            i += 1;
            continue;
        }
        i += 1;
        let lines = doc_lines(cx, &group);
        let Some(first_tag) = lines.iter().position(|(l, _)| l.starts_with('@')) else {
            continue;
        };
        let desc_end = lines[..first_tag].iter().rposition(|(l, _)| !l.is_empty());
        if let Some(d) = desc_end {
            let blanks = first_tag - d - 1;
            if blanks != 1 {
                warn(
                    "one blank line separates a doc comment's description from its tags",
                    cx.span(lines[first_tag].1, lines[first_tag].1 + 1),
                    if blanks == 0 {
                        "no blank line above"
                    } else {
                        "more than one blank line above"
                    },
                    "leave exactly one blank line before the first tag",
                );
            }
        }
        let mut in_example = false;
        for w in first_tag..lines.len().saturating_sub(1) {
            let (l, _) = &lines[w];
            if l.starts_with("@example") {
                in_example = true;
            } else if l.starts_with('@') {
                in_example = false;
            }
            let next_tag = lines[w + 1..].iter().position(|(l, _)| !l.is_empty());
            if !in_example
                && l.is_empty()
                && next_tag.is_some_and(|n| lines[w + 1 + n].0.starts_with('@'))
            {
                warn(
                    "doc comment tags aren't separated by blank lines",
                    cx.span(lines[w].1, lines[w].1),
                    "blank line",
                    "remove it",
                );
            }
        }
    }
}

struct Docs<'c, 'a, 'o> {
    cx: &'c Cx<'a>,
    out: &'o mut Vec<Diagnostic>,
}

impl Docs<'_, '_, '_> {
    fn warn(&mut self, msg: impl Into<String>, at: Span, label: &str, help: &str) {
        let d = self
            .cx
            .warn("docTags", msg, at, label)
            .help(help.to_string());
        self.out.push(d);
    }

    fn common(&mut self, tags: &[Tag], class: bool) {
        for t in tags {
            if !KNOWN.contains(&t.name.as_str()) {
                self.warn(
                    format!("`@{}` isn't a known tag", t.name),
                    t.at,
                    "unknown",
                    "use a known tag, or describe it in the text",
                );
                continue;
            }
            if EMPTY.contains(&t.name.as_str()) && !t.text.is_empty() {
                self.warn(
                    format!("`@{}` takes no text", t.name),
                    t.at,
                    "not empty",
                    "remove the text after it",
                );
            }
            if t.name == "access"
                && !matches!(
                    t.text.as_str(),
                    "public" | "private" | "protected" | "package"
                )
            {
                self.warn(
                    "`@access` is `public`, `private`, `protected` or `package`",
                    t.at,
                    "unknown access",
                    "use one of them",
                );
            }
            if matches!(t.name.as_str(), "version" | "since") {
                let ok = t.text.split('.').count() <= 3
                    && !t.text.is_empty()
                    && t.text
                        .trim_start_matches('v')
                        .split(['.', '-', '+'])
                        .next()
                        .is_some_and(|n| n.parse::<u64>().is_ok());
                if !ok {
                    self.warn(
                        format!("`@{}` takes a version (`1.2.3`)", t.name),
                        t.at,
                        "not a version",
                        "write a version number",
                    );
                }
            }
            if t.name == "implements" && !class {
                self.warn(
                    "`@implements` belongs on classes",
                    t.at,
                    "not a class",
                    "remove it",
                );
            }
            if t.text.starts_with('{')
                && matches!(
                    t.name.as_str(),
                    "param"
                        | "arg"
                        | "argument"
                        | "returns"
                        | "return"
                        | "yields"
                        | "yield"
                        | "throws"
                )
            {
                self.warn(
                    "types in doc tags repeat the signature",
                    t.at,
                    "typed tag",
                    "remove the `{…}`: the declaration has the type",
                );
            }
        }
    }

    fn function(&mut self, f: &FnDecl, start: u32, constructor: bool, setter: bool) {
        let comments = doc_above(self.cx, start);
        if comments.is_empty() {
            return;
        }
        let lines = doc_lines(self.cx, &comments);
        let tags = tags(self.cx, &lines);
        self.common(&tags, false);
        if tags
            .iter()
            .any(|t| t.name.eq_ignore_ascii_case("inheritdoc"))
        {
            return;
        }
        let at = f
            .name
            .as_ref()
            .map_or(Span::new(self.cx.file, start, start + 1), |n| n.span);
        let names: Vec<&str> = f
            .params
            .iter()
            .filter_map(|p| match &p.pat.kind {
                PatternKind::Ident(n) => Some(n.name.as_str()),
                _ => None,
            })
            .collect();
        let mut documented: Vec<String> = Vec::new();
        for t in tags
            .iter()
            .filter(|t| matches!(t.name.as_str(), "param" | "arg" | "argument"))
        {
            // (A `{type}` was reported already; look past it.)
            let text = match t.text.strip_prefix('{') {
                Some(r) => r.split_once('}').map_or("", |(_, r)| r).trim().to_string(),
                None => t.text.clone(),
            };
            let mut words = text.splitn(2, char::is_whitespace);
            let name = words.next().unwrap_or("");
            let desc = words
                .next()
                .unwrap_or("")
                .trim()
                .trim_start_matches('-')
                .trim();
            if name.is_empty() {
                self.warn(
                    "`@param` needs the parameter's name",
                    t.at,
                    "no name",
                    "write `@param name description`",
                );
                continue;
            }
            if name.starts_with('[') {
                self.warn(
                    "`@param` doesn't give defaults (the signature has them)",
                    t.at,
                    "default in tag",
                    "write just the name",
                );
                continue;
            }
            if !names.contains(&name) {
                self.warn(
                    format!("there's no parameter `{name}`"),
                    t.at,
                    "unknown parameter",
                    "fix the name, or remove the tag",
                );
            } else if documented.iter().any(|d| d == name) {
                self.warn(
                    format!("`{name}` is documented twice"),
                    t.at,
                    "duplicate",
                    "remove one",
                );
            } else {
                documented.push(name.to_string());
            }
            if desc.is_empty() {
                self.warn(
                    format!("`@param {name}` has no description"),
                    t.at,
                    "no description",
                    "say what the parameter is for",
                );
            }
        }
        let order_ok = documented
            .iter()
            .map(|d| names.iter().position(|n| n == d))
            .collect::<Vec<_>>()
            .windows(2)
            .all(|w| w[0] < w[1]);
        if !order_ok {
            self.warn(
                "the `@param` tags aren't in the parameters' order",
                at,
                "out of order",
                "order them as the parameters are",
            );
        }
        for n in &names {
            if !documented.iter().any(|d| d == n) && !n.starts_with('_') {
                self.warn(
                    format!("parameter `{n}` has no `@param`"),
                    at,
                    "undocumented parameter",
                    &format!("add `@param {n} …`"),
                );
            }
        }
        let ret = f.ret.as_ref().map(|t| {
            self.cx
                .src(t.span)
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        });
        let returns_nothing = ret
            .as_deref()
            .is_none_or(|r| r == "void" || r == "never" || r.starts_with("Promise<void"));
        let returns_tag = tags
            .iter()
            .find(|t| matches!(t.name.as_str(), "returns" | "return"));
        match returns_tag {
            None if !returns_nothing && !constructor && !setter && !f.flags.generator => {
                self.warn(
                    "the result has no `@returns`",
                    at,
                    "undocumented result",
                    "add `@returns …`",
                );
            }
            Some(t) if ret.is_some() && returns_nothing => {
                self.warn(
                    "`@returns` on a function without a result",
                    t.at,
                    "nothing returned",
                    "remove it",
                );
            }
            Some(t) if t.text.is_empty() => {
                self.warn(
                    "`@returns` has no description",
                    t.at,
                    "no description",
                    "say what the result is",
                );
            }
            _ => {}
        }
        let yields_tag = tags
            .iter()
            .find(|t| matches!(t.name.as_str(), "yields" | "yield"));
        match (f.flags.generator, yields_tag) {
            (true, None) => self.warn(
                "the generator has no `@yields`",
                at,
                "undocumented values",
                "add `@yields …`",
            ),
            (false, Some(t)) => self.warn(
                "`@yields` on a function that isn't a generator",
                t.at,
                "not a generator",
                "remove it",
            ),
            _ => {}
        }
    }

    fn other(&mut self, start: u32, class: bool) {
        let comments = doc_above(self.cx, start);
        if comments.is_empty() {
            return;
        }
        let lines = doc_lines(self.cx, &comments);
        let tags = tags(self.cx, &lines);
        self.common(&tags, class);
    }
}

impl<'a> Visit<'a> for Docs<'_, '_, '_> {
    fn decl(&mut self, d: &'a Decl) {
        match &d.kind {
            DeclKind::Function(f) => self.function(f, d.span.start, false, false),
            DeclKind::Class(_) => self.other(d.span.start, true),
            DeclKind::Struct(_)
            | DeclKind::Interface(_)
            | DeclKind::Enum(_)
            | DeclKind::TypeAlias { .. } => self.other(d.span.start, false),
            DeclKind::Binding(_) => self.other(d.span.start, false),
            _ => {}
        }
        match &d.kind {
            DeclKind::Struct(s) => s.members.iter().for_each(|m| self.member(m)),
            DeclKind::Class(c) => c.members.iter().for_each(|m| self.member(m)),
            DeclKind::Interface(i) => i.members.iter().for_each(|m| self.member(m)),
            DeclKind::Enum(e) => e.members.iter().for_each(|m| self.member(m)),
            DeclKind::Function(f) => walk::walk_fn(self, f),
            _ => walk::walk_decl(self, d),
        }
    }

    fn member(&mut self, m: &'a Member) {
        match &m.kind {
            MemberKind::Method(f)
            | MemberKind::Getter(f)
            | MemberKind::Operator { func: f, .. } => self.function(f, m.span.start, false, false),
            MemberKind::Setter(f) => self.function(f, m.span.start, false, true),
            MemberKind::Constructor(f) => self.function(f, m.span.start, true, false),
            MemberKind::Field { .. } => self.other(m.span.start, false),
            MemberKind::StaticBlock(_) => {}
        }
        walk::walk_member(self, m);
    }

    fn func(&mut self, f: &'a FnDecl) {
        walk::walk_fn(self, f);
    }
}
