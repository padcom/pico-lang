//! The symbol index (PLAN P10.6c1): every name in a document, what it refers to and what
//! hover shows for it, built from a checked program's HIR and type checker results.
//!
//! Declarations are indexed by their name spans (locals, globals, functions, types, fields,
//! variants); uses come from the HIR (variables, functions, members, methods, getters and
//! setters, struct literal fields) and from the resolver's `name_refs` (type names, `new C`,
//! `S { … }`, imported names).
//! Desugaring creates expressions with borrowed spans, so a use is only kept where the source
//! text under it is the declaration's name.

use ppl_diag::{FileId, SourceMap, Span};
use ppl_sema::hir::{self, ExprKind, Program, StructId, Target};
use ppl_sema::{Ty, Types};
use std::collections::HashMap;

/// A name in the document: where it is, its declaration and its hover text.
#[derive(Clone, Debug)]
pub struct Ref {
    pub start: u32,
    pub end: u32,
    /// The name as written (checks a stale index against the current text).
    pub name: String,
    pub def: Span,
    /// Markdown: the declaration's signature in a `ppl` block, then its doc comment.
    pub hover: String,
}

/// The index of one document (file `file` of `sm`).
pub struct Index {
    pub sm: SourceMap,
    pub file: FileId,
    /// The text the index was built from.
    pub text: String,
    /// Sorted by start; the innermost name wins where they nest.
    pub refs: Vec<Ref>,
}

impl Index {
    /// The name at byte `offset` of the text the index was built from.
    pub fn at(&self, offset: u32) -> Option<&Ref> {
        self.refs
            .iter()
            .filter(|r| r.start <= offset && offset <= r.end)
            .min_by_key(|r| r.end - r.start)
    }
}

pub fn build(sm: SourceMap, program: &Program, types: &Types) -> Index {
    let file = program.file;
    let text = sm.file(file).text.clone();
    let mut b = Builder {
        sm: &sm,
        p: program,
        t: types,
        decls: HashMap::new(),
        uses: Vec::new(),
    };
    b.declarations();
    for (i, f) in program.fns.iter().enumerate() {
        let fid = hir::FnId(i as u32);
        for d in f.defaults.iter().flatten() {
            b.expr(fid, d);
        }
        for s in &f.body.stmts {
            b.stmt(fid, s);
        }
        if let Some(t) = &f.body.tail {
            b.expr(fid, t);
        }
    }
    for &(at, def) in &program.name_refs {
        b.uses.push((at, def));
    }
    let mut refs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let decl_list: Vec<(Span, String)> = b.decls.iter().map(|(s, h)| (*s, h.clone())).collect();
    for (at, hover) in decl_list {
        if at.file == file && seen.insert((at.start, at.end)) {
            refs.push(Ref {
                start: at.start,
                end: at.end,
                name: sm.snippet(at).to_string(),
                def: at,
                hover,
            });
        }
    }
    for (at, def) in std::mem::take(&mut b.uses) {
        if at.file != file || at.is_empty() || !same_name(sm.snippet(at), sm.snippet(def)) {
            continue;
        }
        let Some(hover) = b.decls.get(&def) else {
            continue;
        };
        if seen.insert((at.start, at.end)) {
            refs.push(Ref {
                start: at.start,
                end: at.end,
                name: sm.snippet(at).to_string(),
                def,
                hover: hover.clone(),
            });
        }
    }
    refs.sort_by_key(|r| (r.start, r.end));
    Index {
        sm,
        file,
        text,
        refs,
    }
}

struct Builder<'a> {
    sm: &'a SourceMap,
    p: &'a Program,
    t: &'a Types,
    /// Hover text of every declaration, by its name span.
    decls: HashMap<Span, String>,
    /// Uses: (where, the declaration's name span).
    uses: Vec<(Span, Span)>,
}

impl Builder<'_> {
    /// A declaration's hover text: `sig`, and with `docs` its doc comment.
    fn decl(&mut self, at: Span, sig: String, docs: bool) {
        // (Synthesized declarations, such as a constructor's `this`, borrow longer spans.)
        let name = self.sm.snippet(at);
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        {
            return;
        }
        // (Types that didn't resolve, in code with errors.)
        let sig = sig.replace("{error}", "unknown");
        let docs = if docs {
            doc_comment(self.sm, at)
        } else {
            String::new()
        };
        let mut h = format!("```ppl\n{sig}\n```");
        if !docs.is_empty() {
            h.push_str("\n\n");
            h.push_str(&docs);
        }
        self.decls.entry(at).or_insert(h);
    }

    fn declarations(&mut self) {
        let p = self.p;
        for (g, b) in p.globals.iter().enumerate() {
            let kind = if b.mutable { "let" } else { "const" };
            let sig = binding(kind, &b.name, self.t.globals.get(g));
            self.decl(b.span, sig, true);
        }
        for s in &p.structs {
            let kind = if s.is_interface {
                "interface"
            } else if s.is_class {
                "class"
            } else {
                "struct"
            };
            let mut sig = format!("{kind} {}", s.name);
            if !s.generics.is_empty() {
                let g: Vec<&str> = s.generics.iter().map(|g| g.name.as_str()).collect();
                sig.push_str(&format!("<{}>", g.join(", ")));
            }
            if let Some(b) = s.base {
                sig.push_str(&format!(" extends {}", p.structs[b.0 as usize].name));
            }
            self.decl(s.name_span, sig, true);
            for f in &s.fields {
                let sig = format!("(field) {}.{}: {}", s.name, f.name, f.ty);
                self.decl(f.span, sig, true);
            }
        }
        for e in &p.enums {
            if !matches!(e.origin, hir::EnumOrigin::Declared) {
                continue;
            }
            self.decl(e.name_span, format!("enum {}", e.name), true);
            for v in &e.variants {
                self.decl(v.span, format!("(variant) {}.{}", e.name, v.name), true);
            }
        }
        // (After the types: a synthesized constructor has its class's name span.)
        for (i, f) in p.fns.iter().enumerate() {
            if !f.is_entry && f.closure.is_none() {
                let sig = self.fn_sig(hir::FnId(i as u32));
                self.decl(f.name_span, sig, true);
            }
            for (l, b) in f.locals.iter().enumerate() {
                let ty = self.t.locals.get(i).and_then(|ls| ls.get(l));
                let kind = if f.params.contains(&hir::LocalId(l as u32)) {
                    "(parameter)"
                } else if b.mutable {
                    "let"
                } else {
                    "const"
                };
                let sig = binding(kind, &b.name, ty);
                self.decl(b.span, sig, false);
            }
        }
        // Type aliases: the declaration's line.
        for &(_, def) in &p.name_refs {
            if !self.decls.contains_key(&def) {
                let f = self.sm.file(def.file);
                let line = f.line_text(f.line_index(def.start)).trim();
                let line = line.strip_prefix("export ").unwrap_or(line);
                self.decl(def, line.trim_end_matches(';').to_string(), true);
            }
        }
    }

    /// `function name(a: A, b: B): R`, or `Class.name(…)` for methods.
    fn fn_sig(&self, fid: hir::FnId) -> String {
        let f = &self.p.fns[fid.0 as usize];
        let sig = self.t.sigs.get(fid.0 as usize);
        let mut params: Vec<String> = f
            .params
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let name = &f.local(*l).name;
                let rest = if f.rest && i + 1 == f.params.len() {
                    "..."
                } else {
                    ""
                };
                match sig.and_then(|s| s.params.get(i)) {
                    Some(t) => format!("{rest}{name}: {t}"),
                    None => format!("{rest}{name}"),
                }
            })
            .collect();
        let mut name = f.name.clone();
        let mut prefix = "function ";
        // Methods of built-in types written in ppl: `__Array_map(self, f)` is `Array.map(f)`.
        if let Some((ty, m)) = f
            .name
            .strip_prefix("__")
            .and_then(|r| r.split_once('_'))
            .filter(|(ty, m)| {
                self.p
                    .extensions
                    .contains_key(&(ty.to_string(), m.to_string()))
            })
        {
            name = format!("{ty}.{m}");
            prefix = "(method) ";
            if !params.is_empty() {
                params.remove(0);
            }
        } else if let Some(sid) = f.this.map(|(_, s)| s).or(f.owner) {
            let owner = &self.p.structs[sid.0 as usize].name;
            if !name.starts_with(&format!("{owner}.")) {
                name = format!("{owner}.{name}");
            }
            prefix = "(method) ";
        }
        let mut s = format!("{prefix}{name}");
        if !f.generics.is_empty() {
            let g: Vec<&str> = f.generics.iter().map(|g| g.name.as_str()).collect();
            s.push_str(&format!("<{}>", g.join(", ")));
        }
        s.push_str(&format!("({})", params.join(", ")));
        if let Some(sig) = sig {
            s.push_str(&format!(": {}", sig.ret));
        }
        if let Some(ts) = self.t.fn_throws.get(fid.0 as usize)
            && !ts.is_empty()
        {
            let ts: Vec<String> = ts.iter().map(Ty::to_string).collect();
            s.push_str(&format!(" throws {}", ts.join(" | ")));
        }
        s
    }

    fn fn_span(&self, f: hir::FnId) -> Span {
        self.p.fns[f.0 as usize].name_span
    }

    fn var_span(&self, fid: hir::FnId, v: hir::Var) -> Span {
        match v {
            hir::Var::Local(l) => self.p.fns[fid.0 as usize].local(l).span,
            hir::Var::Global(g) => self.p.globals[g.0 as usize].span,
        }
    }

    /// The field `idx` of the struct or class `object` has (its name span).
    fn field_span(&self, object: &hir::Expr, idx: u32) -> Option<Span> {
        let sid = struct_of(self.t.exprs.get(object.id.0 as usize)?)?;
        let def = self.p.structs.get(sid.0 as usize)?;
        Some(def.fields.get(idx as usize)?.span)
    }

    fn stmt(&mut self, fid: hir::FnId, s: &hir::Stmt) {
        match s {
            hir::Stmt::Let { init, .. } => {
                if let Some(i) = init {
                    self.expr(fid, i);
                }
            }
            hir::Stmt::Expr(e) => self.expr(fid, e),
        }
    }

    fn target(&mut self, t: &Target) {
        if let Target::Field {
            id,
            object,
            name_span,
            ..
        } = t
        {
            if let Some(&f) = self.t.setters.get(id) {
                self.uses.push((*name_span, self.fn_span(f)));
            } else if let Some(&idx) = self.t.fields.get(id)
                && let Some(def) = self.field_span(object, idx)
            {
                self.uses.push((*name_span, def));
            }
        }
    }

    fn expr(&mut self, fid: hir::FnId, e: &hir::Expr) {
        match &e.kind {
            ExprKind::Var(v) => {
                let def = self.var_span(fid, *v);
                self.uses.push((e.span, def));
            }
            ExprKind::Fn(f) | ExprKind::FnAddr(f) => {
                self.uses.push((e.span, self.fn_span(*f)));
            }
            ExprKind::Member {
                object, name_span, ..
            } => {
                if let Some(&f) = self.t.getters.get(&e.id) {
                    self.uses.push((*name_span, self.fn_span(f)));
                } else if let Some(&idx) = self.t.fields.get(&e.id)
                    && let Some(def) = self.field_span(object, idx)
                {
                    self.uses.push((*name_span, def));
                }
            }
            ExprKind::Call { callee, .. } => {
                let target = self
                    .t
                    .methods
                    .get(&e.id)
                    .or_else(|| self.t.extensions.get(&e.id))
                    .or_else(|| self.t.overloads.get(&e.id))
                    .copied();
                if let Some(f) = target {
                    let at = match &callee.kind {
                        ExprKind::Member { name_span, .. } => *name_span,
                        _ => callee.span,
                    };
                    self.uses.push((at, self.fn_span(f)));
                }
            }
            ExprKind::Object { fields, .. } => {
                if let Some((sid, idxs)) = self.t.objects.get(&e.id) {
                    let def = &self.p.structs[sid.0 as usize];
                    for (fi, &idx) in fields.iter().zip(idxs) {
                        if let Some(fd) = def.fields.get(idx as usize) {
                            self.uses.push((fi.name_span, fd.span));
                        }
                    }
                }
            }
            ExprKind::Assign { target, .. } | ExprKind::IncDec { target, .. } => {
                if let Target::Var(v) = target {
                    // `x = …`: the name is at the start of the assignment.
                    let def = self.var_span(fid, *v);
                    let name = self.sm.snippet(def);
                    let at = Span::new(e.span.file, e.span.start, e.span.start + name.len() as u32);
                    if at.end <= e.span.end {
                        self.uses.push((at, def));
                    }
                }
                self.target(target);
            }
            _ => {}
        }
        e.for_each_child(&mut |c| self.expr(fid, c));
    }
}

/// A use's text names its declaration: the same name, or a built-in type's method written
/// in ppl (`xs.map` → `__Array_map`).
fn same_name(at: &str, decl: &str) -> bool {
    at == decl
        || decl
            .strip_prefix("__")
            .and_then(|r| r.split_once('_'))
            .is_some_and(|(_, m)| m == at)
}

fn binding(kind: &str, name: &str, ty: Option<&Ty>) -> String {
    match ty {
        Some(t) if !t.is_error() => format!("{kind} {name}: {t}"),
        _ => format!("{kind} {name}"),
    }
}

/// The struct, class or interface a value of type `t` is (through `?`, `immutable` and
/// references).
fn struct_of(t: &Ty) -> Option<StructId> {
    match t {
        Ty::Struct(s, ..) | Ty::Class(s, ..) | Ty::Iface(s, ..) => Some(*s),
        Ty::Nullable(t) | Ty::Immutable(t) | Ty::Ref(t) => struct_of(t),
        _ => None,
    }
}

/// The doc comment above the declaration whose name is at `at` (`/** … */` or `///` lines,
/// decorators in between are skipped), as Markdown.
pub fn doc_comment(sm: &SourceMap, at: Span) -> String {
    let f = sm.file(at.file);
    let mut line = f.line_index(at.start);
    let mut out: Vec<String> = Vec::new();
    // Lines above, nearest first.
    let above = |l: usize| f.line_text(l).trim();
    while line > 0 && above(line - 1).starts_with('@') {
        line -= 1;
    }
    if line == 0 {
        return String::new();
    }
    let prev = above(line - 1);
    if prev.starts_with("///") {
        let mut l = line;
        while l > 0 && above(l - 1).starts_with("///") {
            l -= 1;
            out.push(above(l).trim_start_matches('/').trim_start().to_string());
        }
        out.reverse();
    } else if prev.ends_with("*/") {
        let mut l = line;
        let mut lines = Vec::new();
        loop {
            if l == 0 {
                return String::new();
            }
            l -= 1;
            let t = above(l);
            lines.push(t.to_string());
            if t.starts_with("/**") {
                break;
            }
            if t.starts_with("/*") || (l + 1 < line && t.ends_with("*/")) {
                return String::new();
            }
        }
        lines.reverse();
        for t in lines {
            let t = t.strip_prefix("/**").unwrap_or(&t);
            let t = t.strip_suffix("*/").unwrap_or(t).trim();
            let t = t.strip_prefix('*').map(str::trim_start).unwrap_or(t);
            out.push(t.to_string());
        }
        while out.first().is_some_and(|s| s.is_empty()) {
            out.remove(0);
        }
        while out.last().is_some_and(|s| s.is_empty()) {
            out.pop();
        }
    }
    out.join("\n")
}
