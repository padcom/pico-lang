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
use std::collections::{HashMap, HashSet};

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
    /// The hover text of every declaration the program has (all files), by its name span.
    pub decls: HashMap<Span, String>,
    /// Every name of the document and the declaration it names, (use, declaration); a
    /// declaration names itself. Unlike `refs`, a span may name two declarations.
    pub links: Vec<(Span, Span)>,
    /// Shorthand fields of struct literals (`S { x }`), which name a field and a variable.
    pub shorthand: HashSet<Span>,
    /// Methods that override or implement each other, by any member's name span: all the
    /// members' name spans (renamed together).
    pub families: HashMap<Span, Vec<Span>>,
    /// For a method that implements or overrides others: their name spans (the nearest ones
    /// up `extends`/`implements`), where go-to-definition on its declaration leads.
    pub supers: HashMap<Span, Vec<Span>>,
    /// For a type: the name spans of the types below it (that extend or implement it,
    /// directly or not). For a method: those of the methods that override or implement it.
    pub implementations: HashMap<Span, Vec<Span>>,
}

/// Each type's subtypes (transitively) and each method's implementations and overrides in
/// them (the methods those types declare themselves), by name span.
fn implementations(p: &Program) -> HashMap<Span, Vec<Span>> {
    let n = p.structs.len();
    // Direct subtypes.
    let mut below: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, s) in p.structs.iter().enumerate() {
        for u in s.base.iter().chain(&s.implements) {
            below[u.0 as usize].push(i);
        }
    }
    let mut out: HashMap<Span, Vec<Span>> = HashMap::new();
    for (i, s) in p.structs.iter().enumerate() {
        // All types below `i`, breadth first.
        let mut all = Vec::new();
        let mut queue: std::collections::VecDeque<usize> = below[i].iter().copied().collect();
        let mut seen = HashSet::from([i]);
        while let Some(t) = queue.pop_front() {
            if seen.insert(t) {
                all.push(t);
                queue.extend(below[t].iter().copied());
            }
        }
        if all.is_empty() {
            continue;
        }
        out.insert(
            s.name_span,
            all.iter().map(|t| p.structs[*t].name_span).collect(),
        );
        for (name, f) in &s.methods {
            let mine = p.fns[f.0 as usize].name_span;
            let impls: Vec<Span> = all
                .iter()
                .filter_map(|t| {
                    let g = p.structs[*t].method(name)?;
                    let func = &p.fns[g.0 as usize];
                    // (Only the types' own methods: inherited ones are their declarer's.)
                    let own = func.this.map(|x| x.1).or(func.owner) == Some(StructId(*t as u32));
                    (own && func.name_span != mine).then_some(func.name_span)
                })
                .collect();
            if !impls.is_empty() {
                out.entry(mine).or_default().extend(impls);
            }
        }
    }
    for v in out.values_mut() {
        v.dedup();
    }
    out
}

/// For each method declared by a type, the nearest same-named methods up its `extends` and
/// `implements` (breadth first; all of the nearest level, e.g. two interfaces' methods).
fn super_methods(p: &Program) -> HashMap<Span, Vec<Span>> {
    let mut out = HashMap::new();
    for (i, s) in p.structs.iter().enumerate() {
        let sid = StructId(i as u32);
        let up = |s: StructId| {
            let d = &p.structs[s.0 as usize];
            d.base
                .into_iter()
                .chain(d.implements.iter().copied())
                .collect::<Vec<_>>()
        };
        for (name, f) in &s.methods {
            let func = &p.fns[f.0 as usize];
            // (Inherited methods are their declaring type's.)
            if func.this.map(|t| t.1).or(func.owner) != Some(sid) {
                continue;
            }
            let mut level = up(sid);
            let mut seen = HashSet::from([sid]);
            while !level.is_empty() {
                let found: Vec<Span> = level
                    .iter()
                    .filter_map(|t| p.structs[t.0 as usize].method(name))
                    .map(|g| p.fns[g.0 as usize].name_span)
                    .filter(|s| *s != func.name_span)
                    .collect();
                if !found.is_empty() {
                    out.insert(func.name_span, found);
                    break;
                }
                let next: Vec<StructId> = level
                    .iter()
                    .flat_map(|t| up(*t))
                    .filter(|t| seen.insert(*t))
                    .collect();
                level = next;
            }
        }
    }
    out
}

/// Groups the methods of a name along `extends`/`implements` edges where both types have
/// it (so an override and what it overrides, an implementation and its interface method).
fn method_families(p: &Program) -> HashMap<Span, Vec<Span>> {
    // Union-find over (type, method name).
    let mut parent: HashMap<(u32, &str), (u32, &str)> = HashMap::new();
    fn find<'a>(
        parent: &mut HashMap<(u32, &'a str), (u32, &'a str)>,
        k: (u32, &'a str),
    ) -> (u32, &'a str) {
        let mut k = k;
        while let Some(&up) = parent.get(&k) {
            if up == k {
                break;
            }
            k = up;
        }
        k
    }
    for (i, s) in p.structs.iter().enumerate() {
        for (m, _) in &s.methods {
            parent
                .entry((i as u32, m.as_str()))
                .or_insert((i as u32, m.as_str()));
        }
    }
    for (i, s) in p.structs.iter().enumerate() {
        let near = s.base.iter().chain(&s.implements);
        for n in near {
            let other = &p.structs[n.0 as usize];
            for (m, _) in &s.methods {
                if other.method(m).is_some() {
                    let a = find(&mut parent, (i as u32, m.as_str()));
                    let b = find(&mut parent, (n.0, m.as_str()));
                    if a != b {
                        parent.insert(a, b);
                    }
                }
            }
        }
    }
    let mut groups: HashMap<(u32, &str), Vec<Span>> = HashMap::new();
    for (i, s) in p.structs.iter().enumerate() {
        for (m, f) in &s.methods {
            let span = p.fns[f.0 as usize].name_span;
            let root = find(&mut parent, (i as u32, m.as_str()));
            let g = groups.entry(root).or_default();
            if !g.contains(&span) {
                g.push(span);
            }
        }
    }
    let mut out = HashMap::new();
    for g in groups.into_values().filter(|g| g.len() > 1) {
        for s in &g {
            out.insert(*s, g.clone());
        }
    }
    out
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
        shorthand: Vec::new(),
    };
    b.declarations();
    b.inherit_docs();
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
    let mut links: Vec<(Span, Span)> = refs.iter().map(|r| (r.def, r.def)).collect();
    let mut linked: HashSet<(Span, Span)> = links.iter().copied().collect();
    for (at, def) in std::mem::take(&mut b.uses) {
        if at.file != file || at.is_empty() || !same_name(sm.snippet(at), sm.snippet(def)) {
            continue;
        }
        let Some(hover) = b.decls.get(&def) else {
            continue;
        };
        if linked.insert((at, def)) {
            links.push((at, def));
        }
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
    let decls = std::mem::take(&mut b.decls);
    let shorthand = b.shorthand.iter().copied().collect();
    let families = method_families(program);
    let supers = super_methods(program);
    let implementations = implementations(program);
    Index {
        sm,
        file,
        text,
        refs,
        decls,
        links,
        shorthand,
        families,
        supers,
        implementations,
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
    /// See [`Index::shorthand`].
    shorthand: Vec<Span>,
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

    /// A method without a doc comment shows the one of the method it implements or
    /// overrides: the nearest documented same-named method up its type's `extends` and
    /// `implements` (base classes and interfaces, breadth first).
    fn inherit_docs(&mut self) {
        let p = self.p;
        for (i, s) in p.structs.iter().enumerate() {
            let sid = StructId(i as u32);
            for (name, f) in &s.methods {
                let func = &p.fns[f.0 as usize];
                // (Inherited methods are their declaring type's.)
                if func.this.map(|t| t.1).or(func.owner) != Some(sid) {
                    continue;
                }
                let span = func.name_span;
                if !doc_comment(self.sm, span).is_empty() {
                    continue;
                }
                let Some(docs) = self.ancestor_docs(sid, name) else {
                    continue;
                };
                if let Some(h) = self.decls.get_mut(&span) {
                    h.push_str("\n\n");
                    h.push_str(&docs);
                }
            }
        }
    }

    fn ancestor_docs(&self, sid: StructId, name: &str) -> Option<String> {
        let p = self.p;
        let up = |s: StructId| {
            let d = &p.structs[s.0 as usize];
            d.base.into_iter().chain(d.implements.iter().copied())
        };
        let mut queue: std::collections::VecDeque<StructId> = up(sid).collect();
        let mut seen = HashSet::from([sid]);
        while let Some(s) = queue.pop_front() {
            if !seen.insert(s) {
                continue;
            }
            if let Some(f) = p.structs[s.0 as usize].method(name) {
                let docs = doc_comment(self.sm, p.fns[f.0 as usize].name_span);
                if !docs.is_empty() {
                    return Some(docs);
                }
            }
            queue.extend(up(s));
        }
        None
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

    fn fn_sig(&self, fid: hir::FnId) -> String {
        fn_sig(self.p, self.t, fid)
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
                // A call through an interface value: the interface's method.
                let target = target.or_else(|| {
                    self.t.iface_calls.get(&e.id)?;
                    let ExprKind::Member { object, name, .. } = &callee.kind else {
                        return None;
                    };
                    let sid = struct_of(self.t.exprs.get(object.id.0 as usize)?)?;
                    self.p.structs.get(sid.0 as usize)?.method(name)
                });
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
                            // `S { x }`: the name is the field's and the variable's.
                            if fi.value.span == fi.name_span {
                                self.shorthand.push(fi.name_span);
                            }
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

/// `function name(a: A, b: B): R`, or `Class.name(…)` for methods.
pub(crate) fn fn_sig(p: &Program, t: &Types, fid: hir::FnId) -> String {
    let f = &p.fns[fid.0 as usize];
    let sig = t.sigs.get(fid.0 as usize);
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
        .filter(|(ty, m)| p.extensions.contains_key(&(ty.to_string(), m.to_string())))
    {
        name = format!("{ty}.{m}");
        prefix = "(method) ";
        if !params.is_empty() {
            params.remove(0);
        }
    } else if let Some(sid) = f.this.map(|(_, s)| s).or(f.owner) {
        let owner = &p.structs[sid.0 as usize].name;
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
    if let Some(ts) = t.fn_throws.get(fid.0 as usize)
        && !ts.is_empty()
    {
        let ts: Vec<String> = ts.iter().map(Ty::to_string).collect();
        s.push_str(&format!(" throws {}", ts.join(" | ")));
    }
    s
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
pub(crate) fn struct_of(t: &Ty) -> Option<StructId> {
    match t {
        Ty::Struct(s, ..) | Ty::Class(s, ..) | Ty::Iface(s, ..) => Some(*s),
        Ty::Nullable(t) | Ty::Immutable(t) | Ty::Ref(t) => struct_of(t),
        _ => None,
    }
}

/// The doc comment above the declaration whose name is at `at`, as Markdown.
pub fn doc_comment(sm: &SourceMap, at: Span) -> String {
    ppl_syntax::docs::doc_comment(&sm.file(at.file).text, at.start)
}
