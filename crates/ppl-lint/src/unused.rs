//! `unused`: imports, variables, parameters, module-level bindings and private methods that
//! are never used. A name starting with `_` is exempt, and so are bindings that carry a type
//! test (`catch (e: IoError)`, `s: string => …`, `c as Rect`), `comptime` bindings (comptime
//! code reads them through their thunks), and private methods the language calls implicitly
//! (`toString`, `equals`, `hashCode`, `compareTo`, `format`, `iterator`, `next`, `dispose`,
//! `disposeAsync`). Private fields aren't checked: printing, reflection and derived methods
//! read them in ways the program doesn't show.
//!
//! Imports are used when their name appears as an identifier outside the import statements
//! (also inside `${…}`). The rest comes from the checked program: a variable is used when it's
//! read (assigning it isn't a use), matched by its declaration's span (desugaring copies
//! locals into other functions). Parameters are reported after the last used one only (an
//! unused one before it keeps the others' positions), and not at all for methods whose
//! signature an overridden or implemented method fixes.

use crate::walk::{self, Visit, bound_names};
use ppl_diag::{Diagnostic, FileId, Span, codes};
use ppl_driver::Checked;
use ppl_sema::hir::{self, ExprKind, Program, StructId};
use ppl_syntax::ast;
use ppl_syntax::token::TokenKind;
use std::collections::HashSet;

/// Private methods the language calls without a visible call (protocols).
const IMPLICIT: &[&str] = &[
    "toString",
    "equals",
    "hashCode",
    "compareTo",
    "format",
    "iterator",
    "next",
    "dispose",
    "disposeAsync",
];

pub fn check(m: &ast::Module, text: &str, file: FileId, c: &Checked, out: &mut Vec<Diagnostic>) {
    imports(m, text, file, out);
    if let (Some(p), Some(t)) = (&c.hir, &c.types) {
        let exported = exported_names(m);
        let mut exempt = Exempt::default();
        for s in &m.stmts {
            exempt.stmt(s);
        }
        from_program(p, t, &c.sm, file, &exported, &exempt.spans, out);
    }
}

/// The names of bindings that carry a type test or are `comptime`.
#[derive(Default)]
struct Exempt {
    spans: HashSet<Span>,
}

impl<'a> Visit<'a> for Exempt {
    fn stmt(&mut self, s: &'a ast::Stmt) {
        if let ast::StmtKind::Let {
            pat,
            comptime: true,
            ..
        } = &s.kind
        {
            let mut names = Vec::new();
            bound_names(pat, &mut names);
            self.spans.extend(names.iter().map(|n| n.span));
        }
        walk::walk_stmt(self, s);
    }

    fn expr(&mut self, e: &'a ast::Expr) {
        if let ast::ExprKind::Try { catches, .. } = &e.kind {
            for c in catches {
                let mut names = Vec::new();
                bound_names(&c.pat, &mut names);
                self.spans.extend(names.iter().map(|n| n.span));
            }
        }
        walk::walk_expr(self, e);
    }

    fn pattern(&mut self, p: &'a ast::Pattern) {
        if let ast::PatternKind::Typed { name, .. } | ast::PatternKind::As { name, .. } = &p.kind {
            self.spans.insert(name.span);
        }
    }
}

fn unused(span: Span, what: &str, name: &str, help: &str) -> Diagnostic {
    Diagnostic::warning(format!("{what} `{name}` is never used"))
        .with_code(codes::LINT_UNUSED)
        .primary(span, "unused")
        .help(help.to_string())
}

fn imports(m: &ast::Module, text: &str, file: FileId, out: &mut Vec<Diagnostic>) {
    let mut names: Vec<&ast::Ident> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    for s in &m.stmts {
        if let ast::StmtKind::Decl(d) = &s.kind
            && let ast::DeclKind::Import(i) = &d.kind
        {
            spans.push(s.span);
            match &i.kind {
                ast::ImportKind::Named(ns) => {
                    names.extend(ns.iter().map(|n| n.alias.as_ref().unwrap_or(&n.name)));
                }
                ast::ImportKind::Namespace(alias) => names.push(alias),
            }
        }
    }
    if names.is_empty() {
        return;
    }
    let (tokens, _) = ppl_syntax::lexer::lex(file, text);
    let used: HashSet<&str> = tokens
        .iter()
        .filter(|t| {
            !spans
                .iter()
                .any(|s| s.start <= t.span.start && t.span.end <= s.end)
        })
        .filter_map(|t| match &t.kind {
            TokenKind::Ident(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    for n in names {
        if !n.name.starts_with('_') && !used.contains(n.name.as_str()) {
            out.push(unused(
                n.span,
                "import",
                &n.name,
                "remove it from the import",
            ));
        }
    }
}

/// The names the module exports (`export const`, `export function`, …).
fn exported_names(m: &ast::Module) -> HashSet<String> {
    let mut out = HashSet::new();
    for s in &m.stmts {
        let ast::StmtKind::Decl(d) = &s.kind else {
            continue;
        };
        if let ast::DeclKind::ExportList { names, from: None } = &d.kind {
            out.extend(names.iter().map(|n| n.name.name.clone()));
        }
        if !d.exported {
            continue;
        }
        if let ast::DeclKind::Binding(b) = &d.kind
            && let ast::StmtKind::Let { pat, .. } = &b.kind
        {
            let mut ns = Vec::new();
            crate::walk::bound_names(pat, &mut ns);
            out.extend(ns.iter().map(|n| n.name.clone()));
        }
    }
    out
}

/// What the program reads: variables by their declaration's span, and functions.
#[derive(Default)]
struct Reads {
    vars: HashSet<Span>,
    fns: HashSet<hir::FnId>,
}

/// Variables, parameters, module-level bindings and private members, from the checked
/// program.
fn from_program(
    p: &Program,
    t: &ppl_sema::Types,
    sm: &ppl_diag::SourceMap,
    file: FileId,
    exported: &HashSet<String>,
    exempt: &HashSet<Span>,
    out: &mut Vec<Diagnostic>,
) {
    let mut r = Reads::default();
    for (i, f) in p.fns.iter().enumerate() {
        let fid = hir::FnId(i as u32);
        let mut visit = |e: &hir::Expr| reads(p, fid, e, &mut r);
        for d in f.defaults.iter().flatten() {
            visit(d);
        }
        block(&f.body, &mut visit);
        // A closure's captures read the enclosing function's variables.
        if let Some(c) = &f.closure {
            for cap in &c.captures {
                if let Some(b) = p.fns[c.parent.0 as usize].locals.get(cap.outer.0 as usize) {
                    r.vars.insert(b.span);
                }
            }
        }
    }
    for f in t
        .methods
        .values()
        .chain(t.getters.values())
        .chain(t.setters.values())
    {
        r.fns.insert(*f);
    }
    for f in t
        .extensions
        .values()
        .chain(t.ctor_calls.values())
        .chain(t.overloads.values())
    {
        r.fns.insert(*f);
    }
    let real = |s: Span, name: &str| {
        s.file == file
            && !name.starts_with('_')
            && !name.starts_with('<')
            && sm.snippet(s) == name
            && !exempt.contains(&s)
    };
    let fixed = fixed_signatures(p);
    let externs: HashSet<hir::FnId> = p.externs.iter().map(|e| e.func).collect();
    let mut reported: HashSet<Span> = HashSet::new();
    for (i, f) in p.fns.iter().enumerate() {
        let fid = hir::FnId(i as u32);
        // Parameters: those after the last used one.
        let skip_params = fixed.contains(&fid)
            || externs.contains(&fid)
            || p.structs.iter().any(|s| s.abstract_fns.contains(&fid));
        let last_used = f
            .params
            .iter()
            .rposition(|l| r.vars.contains(&f.local(*l).span))
            .map_or(0, |i| i + 1);
        for (pi, l) in f.params.iter().enumerate() {
            let b = f.local(*l);
            if !skip_params
                && pi >= last_used
                && b.name != "this"
                && real(b.span, &b.name)
                && !r.vars.contains(&b.span)
                && reported.insert(b.span)
            {
                out.push(unused(
                    b.span,
                    "parameter",
                    &b.name,
                    "remove it, or start its name with `_`",
                ));
            }
        }
        for (li, b) in f.locals.iter().enumerate() {
            let l = hir::LocalId(li as u32);
            if f.params.contains(&l) || b.name == "this" {
                continue;
            }
            if real(b.span, &b.name) && !r.vars.contains(&b.span) && reported.insert(b.span) {
                out.push(unused(
                    b.span,
                    "variable",
                    &b.name,
                    "remove it, or start its name with `_`",
                ));
            }
        }
    }
    for g in &p.globals {
        if real(g.span, &g.name)
            && !g.name.contains('.')
            && !exported.contains(&g.name)
            && !r.vars.contains(&g.span)
            && reported.insert(g.span)
        {
            out.push(unused(
                g.span,
                "constant",
                &g.name,
                "remove it, export it, or start its name with `_`",
            ));
        }
    }
    // Private members of the module's classes.
    for s in &p.structs {
        if s.name_span.file != file || !s.is_class {
            continue;
        }
        for name in &s.private {
            if name.starts_with('_')
                || s.declared_in.contains_key(name)
                || IMPLICIT.contains(&name.as_str())
                || s.field(name).is_some()
            {
                continue;
            }
            let fns: Vec<hir::FnId> = s
                .methods
                .iter()
                .chain(&s.getters)
                .chain(&s.setters)
                .chain(&s.static_methods)
                .filter(|(n, _)| n == name)
                .map(|(_, f)| *f)
                .collect();
            if let Some(f) = fns.first()
                && !fns.iter().any(|f| r.fns.contains(f))
            {
                let span = p.fns[f.0 as usize].name_span;
                if real(span, name) {
                    out.push(unused(
                        span,
                        "private method",
                        name,
                        "remove it, or start its name with `_`",
                    ));
                }
            }
        }
    }
}

fn block(b: &hir::Block, f: &mut dyn FnMut(&hir::Expr)) {
    for s in &b.stmts {
        match s {
            hir::Stmt::Let { init: Some(i), .. } => f(i),
            hir::Stmt::Let { .. } => {}
            hir::Stmt::Expr(e) => f(e),
        }
    }
    if let Some(t) = &b.tail {
        f(t);
    }
}

fn reads(p: &Program, fid: hir::FnId, e: &hir::Expr, r: &mut Reads) {
    match &e.kind {
        ExprKind::Var(hir::Var::Local(l)) => {
            if let Some(b) = p.fns[fid.0 as usize].locals.get(l.0 as usize) {
                r.vars.insert(b.span);
            }
        }
        ExprKind::Var(hir::Var::Global(g)) => {
            r.vars.insert(p.globals[g.0 as usize].span);
        }
        ExprKind::Fn(f) | ExprKind::FnAddr(f) => {
            r.fns.insert(*f);
        }
        ExprKind::Overloaded(fs) => r.fns.extend(fs.iter().copied()),
        _ => {}
    }
    match &e.kind {
        ExprKind::Block(b) | ExprKind::Loop(b) | ExprKind::Unsafe(b) => {
            block(b, &mut |c| reads(p, fid, c, r));
        }
        _ => e.for_each_child(&mut |c| reads(p, fid, c, r)),
    }
}

/// Methods whose parameters an overridden or implemented method fixes (and interface
/// methods, whose implementations fix theirs).
fn fixed_signatures(p: &Program) -> HashSet<hir::FnId> {
    let mut out = HashSet::new();
    for s in &p.structs {
        let ups: Vec<StructId> = s.base.iter().chain(&s.implements).copied().collect();
        for (name, f) in &s.methods {
            if s.is_interface {
                out.insert(*f);
            }
            // An override and what it overrides.
            for u in &ups {
                if let Some(g) = p.structs[u.0 as usize].method(name) {
                    out.insert(*f);
                    out.insert(g);
                }
            }
        }
    }
    out
}
