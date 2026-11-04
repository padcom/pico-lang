//! Document symbols (PLAN P10.6d2): the outline of a document, from its syntax tree (so a
//! document with errors still has one).

use pico_diag::{FileId, SourceFile, Span};
use pico_syntax::ast;
use serde_json::{Value, json};

/// LSP `SymbolKind`s.
mod kind {
    pub const CLASS: u32 = 5;
    pub const METHOD: u32 = 6;
    pub const PROPERTY: u32 = 7;
    pub const FIELD: u32 = 8;
    pub const CONSTRUCTOR: u32 = 9;
    pub const ENUM: u32 = 10;
    pub const INTERFACE: u32 = 11;
    pub const FUNCTION: u32 = 12;
    pub const VARIABLE: u32 = 13;
    pub const CONSTANT: u32 = 14;
    pub const ENUM_MEMBER: u32 = 22;
    pub const STRUCT: u32 = 23;
    pub const OPERATOR: u32 = 25;
    pub const TYPE_PARAMETER: u32 = 26;
}

/// The document's symbols (`DocumentSymbol[]`, nested).
pub fn symbols(text: &str) -> Vec<Value> {
    let (m, _) = pico_syntax::parse_module(FileId(0), text);
    let f = SourceFile::new("", text);
    let mut out = Vec::new();
    for stmt in &m.stmts {
        stmt_symbols(&f, stmt, &mut out);
    }
    out
}

fn symbol(f: &SourceFile, name: &str, kind: u32, span: Span, name_span: Span) -> Value {
    let r =
        |s: Span| json!({ "start": crate::position(f, s.start), "end": crate::position(f, s.end) });
    // (The selection range must be inside the range.)
    let name_span = if name_span.start >= span.start && name_span.end <= span.end {
        name_span
    } else {
        span
    };
    json!({ "name": name, "kind": kind, "range": r(span), "selectionRange": r(name_span) })
}

fn with_children(mut s: Value, children: Vec<Value>) -> Value {
    if !children.is_empty() {
        s["children"] = json!(children);
    }
    s
}

fn stmt_symbols(f: &SourceFile, stmt: &ast::Stmt, out: &mut Vec<Value>) {
    match &stmt.kind {
        ast::StmtKind::Let { kind, pat, .. } => {
            let k = if matches!(kind, ast::BindingKind::Const) {
                kind::CONSTANT
            } else {
                kind::VARIABLE
            };
            let mut names = Vec::new();
            pattern_names(pat, &mut names);
            for n in names {
                out.push(symbol(f, &n.name, k, stmt.span, n.span));
            }
        }
        ast::StmtKind::Decl(d) => decl_symbols(f, d, out),
        _ => {}
    }
}

fn decl_symbols(f: &SourceFile, d: &ast::Decl, out: &mut Vec<Value>) {
    let span = d.span;
    match &d.kind {
        ast::DeclKind::Function(func) => {
            if let Some(n) = &func.name {
                out.push(symbol(f, &n.name, kind::FUNCTION, span, n.span));
            }
        }
        ast::DeclKind::Struct(s) => {
            let children = members(f, &s.members);
            out.push(with_children(
                symbol(f, &s.name.name, kind::STRUCT, span, s.name.span),
                children,
            ));
        }
        ast::DeclKind::Class(c) => {
            let children = members(f, &c.members);
            out.push(with_children(
                symbol(f, &c.name.name, kind::CLASS, span, c.name.span),
                children,
            ));
        }
        ast::DeclKind::Interface(i) => {
            let children = members(f, &i.members);
            out.push(with_children(
                symbol(f, &i.name.name, kind::INTERFACE, span, i.name.span),
                children,
            ));
        }
        ast::DeclKind::Enum(e) => {
            let mut children: Vec<Value> = e
                .variants
                .iter()
                .map(|v| symbol(f, &v.name.name, kind::ENUM_MEMBER, v.span, v.name.span))
                .collect();
            children.extend(members(f, &e.members));
            out.push(with_children(
                symbol(f, &e.name.name, kind::ENUM, span, e.name.span),
                children,
            ));
        }
        ast::DeclKind::TypeAlias { name, .. } => {
            out.push(symbol(f, &name.name, kind::TYPE_PARAMETER, span, name.span));
        }
        ast::DeclKind::Binding(b) => {
            let mut inner = Vec::new();
            stmt_symbols(f, b, &mut inner);
            // (The exported binding's range is the whole declaration.)
            for mut s in inner {
                s["range"] = json!({ "start": crate::position(f, span.start), "end": crate::position(f, span.end) });
                out.push(s);
            }
        }
        ast::DeclKind::Extern { items, .. } => {
            for i in items {
                decl_symbols(f, i, out);
            }
        }
        _ => {}
    }
}

fn members(f: &SourceFile, ms: &[ast::Member]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in ms {
        let (name, k, name_span) = match &m.kind {
            ast::MemberKind::Field { name, .. } => (name.name.clone(), kind::FIELD, name.span),
            ast::MemberKind::Method(func) => match &func.name {
                Some(n) => (n.name.clone(), kind::METHOD, n.span),
                None => continue,
            },
            ast::MemberKind::Getter(func) | ast::MemberKind::Setter(func) => match &func.name {
                Some(n) => (n.name.clone(), kind::PROPERTY, n.span),
                None => continue,
            },
            ast::MemberKind::Constructor(func) => {
                let s = Span::new(m.span.file, func.span.start, func.span.start);
                ("constructor".to_string(), kind::CONSTRUCTOR, s)
            }
            ast::MemberKind::Operator { op, func } => {
                (format!("operator {op}"), kind::OPERATOR, func.span)
            }
            ast::MemberKind::StaticBlock(_) => continue,
        };
        out.push(symbol(f, &name, k, m.span, name_span));
    }
    out
}

/// The names a binding's pattern declares (`const [a, { b }] = …`).
fn pattern_names<'a>(p: &'a ast::Pattern, out: &mut Vec<&'a ast::Ident>) {
    match &p.kind {
        ast::PatternKind::Ident(n) => out.push(n),
        ast::PatternKind::Array(elems) => {
            for e in elems {
                pattern_names(e, out);
            }
        }
        ast::PatternKind::Rest(Some(n)) => out.push(n),
        ast::PatternKind::Default { pat, .. } => pattern_names(pat, out),
        ast::PatternKind::Struct { fields, rest, .. } => {
            for f in fields {
                match &f.pat {
                    Some(p) => pattern_names(p, out),
                    None => out.push(&f.name),
                }
            }
            if let Some(Some(n)) = rest {
                out.push(n);
            }
        }
        _ => {}
    }
}
