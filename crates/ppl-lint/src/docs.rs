//! `docs`: every file that exports something is library code (the author's rule), so its
//! exported declarations need a doc comment with a description, and so do the public members
//! (methods, getters, setters, fields) of its exported types: all of an interface's, a struct's
//! and an enum's unless `private`, a class's `public` ones. A method that implements or
//! overrides a documented method is documented by it (decision 276).

use ppl_diag::{Diagnostic, FileId, Span, codes};
use ppl_driver::Checked;
use ppl_sema::hir::{Program, StructId};
use ppl_syntax::ast::*;
use ppl_syntax::docs::doc_comment;
use std::collections::{HashSet, VecDeque};

pub fn check(m: &Module, text: &str, file: FileId, c: &Checked, out: &mut Vec<Diagnostic>) {
    let d = Docs {
        text,
        file,
        program: c.hir.as_ref(),
        sm: &c.sm,
    };
    for s in &m.stmts {
        let StmtKind::Decl(decl) = &s.kind else {
            continue;
        };
        if decl.exported {
            d.decl(decl, out);
        }
    }
}

struct Docs<'a> {
    text: &'a str,
    file: FileId,
    program: Option<&'a Program>,
    sm: &'a ppl_diag::SourceMap,
}

/// A doc comment with a description (not only `@tags`).
fn described(docs: &str) -> bool {
    docs.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .is_some_and(|l| !l.starts_with('@'))
}

impl Docs<'_> {
    fn documented(&self, at: Span) -> bool {
        described(&doc_comment(self.text, at.start))
    }

    fn missing(&self, what: &str, name: &Ident, out: &mut Vec<Diagnostic>) {
        out.push(
            Diagnostic::warning(format!(
                "{what} `{}` is exported but has no doc comment",
                name.name
            ))
            .with_code(codes::LINT_DOCS)
            .primary(name.span, "undocumented")
            .help("describe what it does in a `/** … */` comment above it"),
        );
    }

    fn decl(&self, d: &Decl, out: &mut Vec<Diagnostic>) {
        let (what, name, members, vis): (&str, &Ident, &[Member], Vis) = match &d.kind {
            DeclKind::Function(f) => match &f.name {
                Some(n) => ("function", n, &[], Vis::All),
                None => return,
            },
            DeclKind::Struct(s) => ("struct", &s.name, &s.members, Vis::UnlessPrivate),
            DeclKind::Class(c) => ("class", &c.name, &c.members, Vis::Public),
            DeclKind::Interface(i) => ("interface", &i.name, &i.members, Vis::All),
            DeclKind::Enum(e) => ("enum", &e.name, &e.members, Vis::UnlessPrivate),
            DeclKind::TypeAlias { name, .. } => ("type", name, &[], Vis::All),
            DeclKind::Binding(b) => {
                if let StmtKind::Let { pat, .. } = &b.kind {
                    let mut names = Vec::new();
                    crate::walk::bound_names(pat, &mut names);
                    if !self.documented(d.span) {
                        for n in names {
                            self.missing("constant", n, out);
                        }
                    }
                }
                return;
            }
            _ => return,
        };
        if !self.documented(d.span) {
            self.missing(what, name, out);
        }
        for m in members {
            let public = match (vis, m.mods.visibility) {
                (Vis::All, _) => true,
                (Vis::UnlessPrivate, v) => v != Some(Visibility::Private),
                (Vis::Public, v) => v == Some(Visibility::Public),
            };
            let (kind, n) = match &m.kind {
                MemberKind::Field { name, .. } => ("field", name),
                MemberKind::Method(f) => match &f.name {
                    Some(n) => ("method", n),
                    None => continue,
                },
                MemberKind::Getter(f) | MemberKind::Setter(f) => match &f.name {
                    Some(n) => ("property", n),
                    None => continue,
                },
                _ => continue,
            };
            if !public || self.documented(m.span) {
                continue;
            }
            if matches!(m.kind, MemberKind::Method(_)) && self.inherits_docs(&name.name, &n.name) {
                continue;
            }
            out.push(
                Diagnostic::warning(format!(
                    "{kind} `{}.{}` is public in an exported type but has no doc comment",
                    name.name, n.name
                ))
                .with_code(codes::LINT_DOCS)
                .primary(n.span, "undocumented")
                .help(if kind == "method" {
                    "describe it in a `/** … */` comment above it, or document the method it implements"
                } else {
                    "describe it in a `/** … */` comment above it"
                }),
            );
        }
    }

    /// Whether `ty.method` implements or overrides a documented method (up its `extends` and
    /// `implements`).
    fn inherits_docs(&self, ty: &str, method: &str) -> bool {
        let Some(p) = self.program else {
            return false;
        };
        let Some(start) = p
            .structs
            .iter()
            .position(|s| s.name == ty && s.name_span.file == self.file)
        else {
            return false;
        };
        let up = |s: StructId| {
            let d = &p.structs[s.0 as usize];
            d.base.into_iter().chain(d.implements.iter().copied())
        };
        let start = StructId(start as u32);
        let mut queue: VecDeque<StructId> = up(start).collect();
        let mut seen = HashSet::from([start]);
        while let Some(s) = queue.pop_front() {
            if !seen.insert(s) {
                continue;
            }
            if let Some(f) = p.structs[s.0 as usize].method(method) {
                let at = p.fns[f.0 as usize].name_span;
                if described(&doc_comment(&self.sm.file(at.file).text, at.start)) {
                    return true;
                }
            }
            queue.extend(up(s));
        }
        false
    }
}

#[derive(Clone, Copy)]
enum Vis {
    All,
    UnlessPrivate,
    Public,
}
