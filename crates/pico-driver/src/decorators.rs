//! Decorators (spec §11.2): `decorator function`s run at compile time on descriptions of the
//! declarations they decorate (`std/meta`), before those are compiled. What they ask for
//! (members, hidden statics, wrapped bodies, diagnostics) is applied to the syntax tree.
//!
//! The decorators run in a program of their own: the core library, `std/meta`, the
//! declarations the decorator functions refer to (by name, transitively) and, in each
//! decorator's module, one `comptime { decorator(new FunctionDecl(…), args…) }` per use.
//! Generated code lives in files of its own, mapped to the module it belongs to
//! (`Modules::file_modules`).

use pico_diag::{Diagnostic, FileId, Severity, SourceMap, Span, codes};
use pico_sema::Modules;
use pico_syntax::TokenKind;
use pico_syntax::ast::{self, DeclKind, MemberKind, StmtKind};
use std::collections::{HashMap, HashSet};

/// The outcome of running a program's decorators.
#[derive(Default)]
pub struct Expansion {
    pub diags: Vec<Diagnostic>,
    /// What the decorators printed (part of the compile-time log).
    pub log: String,
    /// Each file of generated code: the decorator use it came from, and what it is.
    pub origins: HashMap<FileId, (Span, String)>,
}

impl Expansion {
    /// Points problems found in generated code at the decorator use that generated it.
    pub fn annotate(&self, diags: &mut [Diagnostic]) {
        for d in diags {
            let Some((at, what)) = d.main_span().and_then(|s| self.origins.get(&s.file)) else {
                continue;
            };
            *d = d.clone().secondary(*at, format!("in the {what}"));
        }
    }
}

/// What a use decorates, inside its statement (#54).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Target {
    /// The declaration itself: a class, struct, enum or function.
    Decl,
    /// Member `i` of a class, struct or enum: a method or a field.
    Member(usize),
    /// Parameter `j` of the function (`None`) or of method `i`.
    Param(Option<usize>, usize),
    /// A binding: the statement itself (top-level) or a local one inside it.
    Binding,
}

/// A decorated binding's name, type as written and whether it's a `const` (#54).
type BindingInfo = (String, String, bool);

/// What a binding decorator receives.
fn binding_descriptor(b: &BindingInfo, id: usize) -> String {
    format!(
        "new __BindingDecl({id}, {}, {}, {})",
        quote(&b.0),
        quote(&b.1),
        b.2
    )
}

fn binding_info(sm: &SourceMap, s: &ast::Stmt) -> Option<BindingInfo> {
    let StmtKind::Let { kind, pat, ty, .. } = &s.kind else {
        return None;
    };
    let name = match &pat.kind {
        ast::PatternKind::Ident(n) => n.name.clone(),
        _ => text(sm, pat.span),
    };
    let ty = ty.as_ref().map(|t| text(sm, t.span)).unwrap_or_default();
    Some((name, ty, matches!(kind, ast::BindingKind::Const)))
}

/// The decorated local bindings in a statement (their decorators, what they bind).
struct LocalBindings<'a> {
    sm: &'a SourceMap,
    found: Vec<(&'a ast::Decorator, BindingInfo)>,
}

impl<'a> pico_syntax::walk::Visit<'a> for LocalBindings<'a> {
    fn stmt(&mut self, s: &'a ast::Stmt) {
        if let StmtKind::Decl(d) = &s.kind
            && let DeclKind::Binding(inner) = &d.kind
            && let Some(info) = binding_info(self.sm, inner)
        {
            for dec in &d.decorators {
                self.found.push((dec, info.clone()));
            }
        }
        pico_syntax::walk::walk_stmt(self, s);
    }
}

/// The members of a class, struct or enum declaration.
fn members_of(d: &ast::Decl) -> Option<&Vec<ast::Member>> {
    match &d.kind {
        DeclKind::Class(c) => Some(&c.members),
        DeclKind::Struct(s) => Some(&s.members),
        DeclKind::Enum(e) => Some(&e.members),
        _ => None,
    }
}

fn members_of_mut(d: &mut ast::Decl) -> Option<&mut Vec<ast::Member>> {
    match &mut d.kind {
        DeclKind::Class(c) => Some(&mut c.members),
        DeclKind::Struct(s) => Some(&mut s.members),
        DeclKind::Enum(e) => Some(&mut e.members),
        _ => None,
    }
}

/// The function a target belongs to: the declaration's, or method `i`'s.
fn function_of(d: &mut ast::Decl, method: Option<usize>) -> Option<&mut ast::FnDecl> {
    match method {
        None => match &mut d.kind {
            DeclKind::Function(f) => Some(f),
            _ => None,
        },
        Some(i) => match &mut members_of_mut(d)?.get_mut(i)?.kind {
            MemberKind::Method(f) => Some(f),
            _ => None,
        },
    }
}

/// What a decorator taking `takes` can be used on, for messages.
fn kinds_for(what: &str) -> &'static str {
    match what {
        "FunctionDecl" => "functions and methods",
        "ClassDecl" => "classes, structs and enums",
        "FieldDecl" => "fields",
        "ParamDecl" => "parameters",
        _ => "bindings",
    }
}

/// A use of a decorator on a declaration.
struct Use {
    /// The decorated statement.
    stmt: usize,
    /// What in it.
    target: Target,
    /// What its decorator receives (`new __ClassDecl(…)`, …).
    desc: String,
    /// The decorator's name as the using module writes it (#54: its call runs there).
    written: String,
    /// The decorator: its module and its name there.
    decorator: (FileId, String),
    span: Span,
    /// The source text of the decorator's arguments.
    args: Vec<String>,
    /// For each argument that is a bare name, the name.
    bare: Vec<Option<String>>,
}

/// Runs the decorators of `stmts` (the whole program, prelude first) and applies what they
/// ask for. Their uses are removed, and so are decorator functions, unless `keep_functions`
/// (the language server's analysis: the program's own decorator functions stay, as ordinary
/// functions, so their code has hovers and links).
pub fn expand(
    sm: &mut SourceMap,
    stmts: &mut Vec<ast::Stmt>,
    modules: &mut Modules,
    entry: FileId,
    cfg: &pico_interp::comptime::Config,
    keep_functions: bool,
) -> Expansion {
    let mut out = Expansion::default();
    // Decorator functions, by (module, name): the type their first parameter takes.
    let mut decorators: HashMap<(FileId, String), String> = HashMap::new();
    for st in stmts.iter() {
        if let StmtKind::Decl(d) = &st.kind
            && let DeclKind::Function(f) = &d.kind
            && f.flags.decorator
            && let Some(name) = &f.name
        {
            let first = f
                .params
                .first()
                .and_then(|p| p.ty.as_ref())
                .map(|t| text(sm, t.span))
                .unwrap_or_default();
            decorators.insert((st.span.file, name.name.clone()), first);
        }
    }
    if decorators.is_empty() {
        return out;
    }
    // The uses: on declarations, their members and parameters (#54).
    let mut uses: Vec<Use> = Vec::new();
    let mut local_uses: Vec<Span> = Vec::new();
    for (i, st) in stmts.iter().enumerate() {
        let file = st.span.file;
        // (Decorator, what it's on, what that is, a binding's facts.)
        let mut found: Vec<(&ast::Decorator, Target, &str, Option<(BindingInfo, bool)>)> =
            Vec::new();
        // Bindings: top-level ones (their decorators kept aside when modules are read) and
        // local ones anywhere inside.
        if let Some(decs) = modules.binding_decorators.get(&st.span)
            && let Some(info) = binding_info(sm, st)
        {
            for dec in decs {
                found.push((
                    dec,
                    Target::Binding,
                    "BindingDecl",
                    Some((info.clone(), false)),
                ));
            }
        }
        let mut locals = LocalBindings {
            sm,
            found: Vec::new(),
        };
        pico_syntax::walk::walk_stmt(&mut locals, st);
        for (dec, info) in locals.found {
            found.push((dec, Target::Binding, "BindingDecl", Some((info, true))));
        }
        let decl = match &st.kind {
            StmtKind::Decl(d) => Some(d),
            _ => None,
        };
        let Some(d) = decl else {
            push_uses(
                sm,
                &decorators,
                modules,
                &mut out,
                &mut uses,
                &mut local_uses,
                None,
                i,
                file,
                found,
            );
            continue;
        };
        let decl_kind = match &d.kind {
            DeclKind::Function(_) => Some("FunctionDecl"),
            DeclKind::Class(_) | DeclKind::Struct(_) | DeclKind::Enum(_) => Some("ClassDecl"),
            _ => None,
        };
        for dec in &d.decorators {
            match decl_kind {
                Some(k) => found.push((dec, Target::Decl, k, None)),
                None => {
                    if matches!(&dec.path[..], [n] if find_decorator(&decorators, modules, file, &n.name).is_some())
                    {
                        out.diags.push(
                            Diagnostic::error(
                                codes::NOT_IMPLEMENTED,
                                "decorators on this kind of declaration aren't implemented yet",
                            )
                            .primary(
                                dec.span,
                                "classes, structs, enums, functions, methods, fields and parameters",
                            ),
                        );
                    }
                }
            }
        }
        if let DeclKind::Function(f) = &d.kind {
            for (j, p) in f.params.iter().enumerate() {
                for dec in &p.decorators {
                    found.push((dec, Target::Param(None, j), "ParamDecl", None));
                }
            }
        }
        if let Some(members) = members_of(d) {
            for (mi, m) in members.iter().enumerate() {
                let what = match &m.kind {
                    MemberKind::Method(_) => Some("FunctionDecl"),
                    MemberKind::Field { .. } => Some("FieldDecl"),
                    _ => None,
                };
                if let Some(w) = what {
                    for dec in &m.decorators {
                        found.push((dec, Target::Member(mi), w, None));
                    }
                }
                if let MemberKind::Method(f) = &m.kind {
                    for (j, p) in f.params.iter().enumerate() {
                        for dec in &p.decorators {
                            found.push((dec, Target::Param(Some(mi), j), "ParamDecl", None));
                        }
                    }
                }
            }
        }
        push_uses(
            sm,
            &decorators,
            modules,
            &mut out,
            &mut uses,
            &mut local_uses,
            Some(d),
            i,
            file,
            found,
        );
    }
    if out.diags.iter().any(Diagnostic::is_error) {
        return out;
    }
    if !uses.is_empty() {
        let edits = match run_decorators(sm, stmts, modules, entry, cfg, &decorators, &uses) {
            Ok((edits, log)) => {
                out.log = log;
                edits
            }
            Err(d) => {
                out.diags = d;
                return out;
            }
        };
        apply(
            sm,
            stmts,
            modules,
            &uses,
            &edits,
            &mut out.diags,
            &mut out.origins,
        );
    }
    // Decorators are gone from the program that runs.
    let user = |file: FileId, d: &ast::Decorator| matches!(&d.path[..], [n] if find_decorator(&decorators, modules, file, &n.name).is_some());
    let mut kept = Vec::with_capacity(stmts.len());
    let mut type_decorators = Vec::new();
    let mut member_decorators = Vec::new();
    for mut st in std::mem::take(stmts) {
        if let StmtKind::Decl(d) = &mut st.kind {
            if let DeclKind::Function(f) = &mut d.kind
                && f.flags.decorator
            {
                if keep_functions && !modules.prelude.contains(&st.span.file) {
                    f.flags.decorator = false;
                    kept.push(st);
                }
                continue;
            }
            let file = st.span.file;
            // Reflection still sees them (spec §11.3).
            let name_span = match &d.kind {
                DeclKind::Class(c) => Some(c.name.span),
                DeclKind::Struct(s) => Some(s.name.span),
                _ => None,
            };
            if let Some(ns) = name_span {
                let text = |sp: Span| {
                    sm.file(sp.file).text[sp.start as usize..sp.end as usize].to_string()
                };
                let seen: Vec<(String, Vec<String>)> = d
                    .decorators
                    .iter()
                    .filter(|dec| user(file, dec))
                    .map(|dec| {
                        let name: Vec<&str> = dec.path.iter().map(|p| p.name.as_str()).collect();
                        let args = dec
                            .args
                            .iter()
                            .flatten()
                            .map(|a| match &a.name {
                                Some(n) => format!("{}: {}", n.name, text(a.value.span)),
                                None => text(a.value.span),
                            })
                            .collect();
                        (name.join("."), args)
                    })
                    .collect();
                if !seen.is_empty() {
                    type_decorators.push((ns, seen));
                }
            }
            // A field's or method's too (#54).
            if let Some(members) = members_of(d) {
                for m in members {
                    let ns = match &m.kind {
                        MemberKind::Field { name, .. } => name.span,
                        MemberKind::Method(f) => match &f.name {
                            Some(n) => n.span,
                            None => continue,
                        },
                        _ => continue,
                    };
                    let seen: Vec<(String, Vec<String>)> = m
                        .decorators
                        .iter()
                        .filter(|dec| user(file, dec))
                        .map(|dec| {
                            let name: Vec<&str> =
                                dec.path.iter().map(|p| p.name.as_str()).collect();
                            let args = dec
                                .args
                                .iter()
                                .flatten()
                                .map(|a| match &a.name {
                                    Some(n) => format!("{}: {}", n.name, text(sm, a.value.span)),
                                    None => text(sm, a.value.span),
                                })
                                .collect();
                            (name.join("."), args)
                        })
                        .collect();
                    if !seen.is_empty() {
                        member_decorators.push((ns, seen));
                    }
                }
            }
            d.decorators.retain(|dec| !user(file, dec));
            // And from members and parameters (#54).
            if let DeclKind::Function(f) = &mut d.kind {
                for p in &mut f.params {
                    p.decorators.retain(|dec| !user(file, dec));
                }
            }
            if let Some(members) = members_of_mut(d) {
                for m in members {
                    m.decorators.retain(|dec| !user(file, dec));
                    if let MemberKind::Method(f) = &mut m.kind {
                        for p in &mut f.params {
                            p.decorators.retain(|dec| !user(file, dec));
                        }
                    }
                }
            }
        }
        kept.push(st);
    }
    *stmts = kept;
    // Bindings: a top-level one's user decorators are gone (`@comptimeLimit` stays); a local
    // one's are marked as run, so it resolves as an ordinary binding (#54).
    let bindings: Vec<(Span, Vec<ast::Decorator>)> = modules
        .binding_decorators
        .clone()
        .into_iter()
        .map(|(span, decs)| {
            let left = decs
                .into_iter()
                .filter(|dec| !user(span.file, dec))
                .collect();
            (span, left)
        })
        .collect();
    for (span, left) in bindings {
        if left.is_empty() {
            modules.binding_decorators.remove(&span);
        } else {
            modules.binding_decorators.insert(span, left);
        }
    }
    modules.type_decorators.extend(type_decorators);
    modules.member_decorators.extend(member_decorators);
    modules.expanded_decorators.extend(local_uses);
    out
}

/// Adds the uses among `found` (of statement `stmt`, declaration `d` unless it's a binding)
/// whose decorators are user decorators, checking each fits what it's on.
#[allow(clippy::too_many_arguments)]
fn push_uses(
    sm: &SourceMap,
    decorators: &HashMap<(FileId, String), String>,
    modules: &Modules,
    out: &mut Expansion,
    uses: &mut Vec<Use>,
    local_uses: &mut Vec<Span>,
    d: Option<&ast::Decl>,
    i: usize,
    file: FileId,
    found: Vec<(&ast::Decorator, Target, &str, Option<(BindingInfo, bool)>)>,
) {
    for (dec, target_in, what, binding) in found {
        let [name] = &dec.path[..] else { continue };
        let Some(target) = find_decorator(decorators, modules, file, &name.name) else {
            continue;
        };
        let takes = decorators[&target].trim_start_matches("__");
        let known = [
            "FunctionDecl",
            "ClassDecl",
            "FieldDecl",
            "ParamDecl",
            "BindingDecl",
        ];
        if known.contains(&takes) && takes != what {
            out.diags.push(
                Diagnostic::error(
                    codes::DECORATOR,
                    format!(
                        "`@{}` decorates a `{takes}` ({}), not {}",
                        name.name,
                        kinds_for(takes),
                        kinds_for(what)
                    ),
                )
                .primary(dec.span, "used on the wrong kind of declaration"),
            );
            continue;
        }
        let args = dec
            .args
            .iter()
            .flatten()
            .map(|a| text(sm, a.value.span))
            .collect();
        let bare = dec
            .args
            .iter()
            .flatten()
            .map(|a| match &a.value.kind {
                ast::ExprKind::Ident(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        let id = uses.len();
        let desc = match (&binding, d) {
            (Some((b, _)), _) => binding_descriptor(b, id),
            (None, Some(d)) => descriptor(sm, d, target_in, id),
            (None, None) => continue,
        };
        // (A local binding's: the resolver treats it as an ordinary binding once it ran.)
        if let Some((_, true)) = &binding {
            local_uses.push(dec.span);
        }
        uses.push(Use {
            bare,
            desc,
            stmt: i,
            target: target_in,
            written: name.name.clone(),
            decorator: target,
            span: dec.span,
            args,
        });
    }
}

/// The decorator `name` means in module `file`: declared there, or imported.
fn find_decorator(
    decorators: &HashMap<(FileId, String), String>,
    modules: &Modules,
    file: FileId,
    name: &str,
) -> Option<(FileId, String)> {
    let file = modules.file_modules.get(&file).copied().unwrap_or(file);
    if decorators.contains_key(&(file, name.to_string())) {
        return Some((file, name.to_string()));
    }
    // Imported, possibly through re-exports (`export { d } from "./x"`, `export *`, #54): a
    // name a module imports is followed to the module it comes from.
    let imported_into = |module: FileId, name: &str| {
        modules
            .imports
            .iter()
            .find(|i| i.module == module && i.alias.name == name && i.name.is_some())
            .and_then(|imp| {
                let exported = &imp.name.as_ref()?.name;
                let local = modules.exports.get(&imp.target)?.get(exported)?;
                Some((imp.target, local.clone()))
            })
    };
    let mut key = imported_into(file, name);
    if key.is_some() {
        // (An imported name that isn't a decorator shadows the core library's.)
        for _ in 0..32 {
            let Some(k) = &key else { break };
            if decorators.contains_key(k) {
                return key;
            }
            key = imported_into(k.0, &k.1);
        }
        return None;
    }
    // The core library's decorators (`@derive`) are seen everywhere.
    decorators
        .keys()
        .find(|(f, n)| n == name && modules.prelude.contains(f))
        .cloned()
}

fn text(sm: &SourceMap, s: Span) -> String {
    sm.file(s.file).text[s.start as usize..s.end as usize].to_string()
}

fn quote(s: &str) -> String {
    format!("{s:?}")
}

fn str_list(xs: &[String]) -> String {
    let parts: Vec<String> = xs.iter().map(|x| quote(x)).collect();
    format!("[{}]", parts.join(", "))
}

/// `new __FunctionDecl(…)` for a function or method.
fn fn_descriptor(sm: &SourceMap, f: &ast::FnDecl, id: usize) -> String {
    let (mut names, mut types) = (Vec::new(), Vec::new());
    for p in &f.params {
        names.push(match &p.pat.kind {
            ast::PatternKind::Ident(n) => n.name.clone(),
            _ => text(sm, p.pat.span),
        });
        types.push(p.ty.as_ref().map(|t| text(sm, t.span)).unwrap_or_default());
    }
    let ret = f
        .ret
        .as_ref()
        .map(|t| text(sm, t.span))
        .unwrap_or_else(|| "void".into());
    let name = f.name.as_ref().map(|n| n.name.clone()).unwrap_or_default();
    format!(
        "new __FunctionDecl({id}, {}, {}, {}, {}, {})",
        quote(&name),
        str_list(&names),
        str_list(&types),
        quote(&ret),
        f.flags.is_async
    )
}

/// `new __ClassDecl(…)` for a class, struct or enum.
fn class_descriptor(sm: &SourceMap, d: &ast::Decl, id: usize) -> String {
    let (name, kind, generics) = match &d.kind {
        DeclKind::Class(c) => (&c.name, "class", &c.generics),
        DeclKind::Struct(s) => (&s.name, "struct", &s.generics),
        DeclKind::Enum(e) => (&e.name, "enum", &e.generics),
        _ => unreachable!("checked"),
    };
    let (mut fields, mut types, mut methods) = (Vec::new(), Vec::new(), Vec::new());
    for m in members_of(d)
        .into_iter()
        .flatten()
        .filter(|m| !m.mods.is_static)
    {
        match &m.kind {
            MemberKind::Field { name, ty, .. } => {
                fields.push(name.name.clone());
                types.push(ty.as_ref().map(|t| text(sm, t.span)).unwrap_or_default());
            }
            // Parameter properties: `constructor(public id: i64)`.
            MemberKind::Constructor(c) => {
                for p in c.params.iter().filter(|p| !p.mods.is_empty()) {
                    if let ast::PatternKind::Ident(n) = &p.pat.kind {
                        fields.push(n.name.clone());
                        types.push(p.ty.as_ref().map(|t| text(sm, t.span)).unwrap_or_default());
                    }
                }
            }
            MemberKind::Method(f) => {
                if let Some(n) = &f.name {
                    methods.push(n.name.clone());
                }
            }
            _ => {}
        }
    }
    let params: Vec<String> = generics.iter().map(|g| g.name.name.clone()).collect();
    format!(
        "new __ClassDecl({id}, {}, {}, {}, {}, {}, {})",
        quote(&name.name),
        quote(kind),
        str_list(&fields),
        str_list(&types),
        str_list(&methods),
        str_list(&params)
    )
}

/// What a use's decorator receives: a `ClassDecl` or `FunctionDecl` for a declaration or a
/// method, a `FieldDecl` or `ParamDecl` (with its owner's description) for a field or a
/// parameter (#54).
fn descriptor(sm: &SourceMap, d: &ast::Decl, target: Target, id: usize) -> String {
    let mut d = d.clone();
    match target {
        Target::Decl => match &d.kind {
            DeclKind::Function(f) => fn_descriptor(sm, f, id),
            _ => class_descriptor(sm, &d, id),
        },
        Target::Member(i) => {
            let m = members_of(&d).expect("checked")[i].clone();
            match &m.kind {
                MemberKind::Method(f) => fn_descriptor(sm, f, id),
                MemberKind::Field { name, ty, .. } => format!(
                    "new __FieldDecl({id}, {}, {}, {})",
                    quote(&name.name),
                    quote(&ty.as_ref().map(|t| text(sm, t.span)).unwrap_or_default()),
                    class_descriptor(sm, &d, id)
                ),
                _ => unreachable!("checked"),
            }
        }
        Target::Binding => unreachable!("a binding's is `binding_descriptor`"),
        Target::Param(method, j) => {
            let f = function_of(&mut d, method).expect("checked").clone();
            let p = &f.params[j];
            let name = match &p.pat.kind {
                ast::PatternKind::Ident(n) => n.name.clone(),
                _ => text(sm, p.pat.span),
            };
            format!(
                "new __ParamDecl({id}, {}, {}, {j}, {})",
                quote(&name),
                quote(&p.ty.as_ref().map(|t| text(sm, t.span)).unwrap_or_default()),
                fn_descriptor(sm, &f, id)
            )
        }
    }
}

/// The names a declaration's source text uses but doesn't bind itself: what it may need from
/// the module. Its own `const`/`let` names, `catch` bindings, lambda parameters and function
/// parameters are left out, so a decorator's `(c: ClassDecl)` doesn't pull in the program's
/// own top-level `const c` (which may use a decorated declaration the decorator program
/// doesn't have). An over-approximation still: names are names, scopes aren't followed.
fn free_idents(sm: &SourceMap, s: Span) -> Vec<String> {
    use pico_syntax::token::{Kw, Punct};
    let (tokens, _) = pico_syntax::lex(s.file, &text(sm, s));
    let kinds: Vec<&TokenKind> = tokens.iter().map(|t| &t.kind).collect();
    let mut bound: HashSet<String> = HashSet::new();
    // The names in a parameter list, `(a: T, b)`, from its `(` at `open`.
    let params = |open: usize, bound: &mut HashSet<String>| {
        let mut depth = 0;
        for k in open..kinds.len() {
            match kinds[k] {
                TokenKind::Punct(Punct::LParen | Punct::LBracket | Punct::LBrace) => depth += 1,
                TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => {
                    depth -= 1;
                    if depth == 0 {
                        return;
                    }
                }
                TokenKind::Ident(n)
                    if depth == 1
                        && matches!(
                            kinds.get(k + 1),
                            Some(TokenKind::Punct(
                                Punct::Colon | Punct::Comma | Punct::RParen
                            ))
                        )
                        && matches!(
                            kinds.get(k.wrapping_sub(1)),
                            Some(TokenKind::Punct(Punct::LParen | Punct::Comma))
                        ) =>
                {
                    bound.insert(n.clone());
                }
                _ => {}
            }
        }
    };
    for (i, k) in kinds.iter().enumerate() {
        match k {
            TokenKind::Kw(Kw::Const | Kw::Let) => {
                if let Some(TokenKind::Ident(n)) = kinds.get(i + 1) {
                    bound.insert(n.clone());
                }
            }
            TokenKind::Kw(Kw::Catch) => {
                if let Some(TokenKind::Ident(n)) = kinds.get(i + 2) {
                    bound.insert(n.clone());
                }
            }
            // `function name(…)`: its parameters.
            TokenKind::Kw(Kw::Function) => {
                if let Some(open) = (i + 1..kinds.len().min(i + 4))
                    .find(|&j| matches!(kinds[j], TokenKind::Punct(Punct::LParen)))
                {
                    params(open, &mut bound);
                }
            }
            // A lambda's parameters: `x =>` or `(a, b) =>`.
            TokenKind::Punct(Punct::FatArrow) if i > 0 => match kinds[i - 1] {
                TokenKind::Ident(n) => {
                    bound.insert(n.clone());
                }
                TokenKind::Punct(Punct::RParen) => {
                    let mut depth = 0;
                    let mut j = i - 1;
                    loop {
                        match kinds[j] {
                            TokenKind::Punct(Punct::RParen) => depth += 1,
                            TokenKind::Punct(Punct::LParen) => {
                                depth -= 1;
                                if depth == 0 {
                                    params(j, &mut bound);
                                    break;
                                }
                            }
                            _ => {}
                        }
                        if j == 0 {
                            break;
                        }
                        j -= 1;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    kinds
        .iter()
        .filter_map(|k| match k {
            TokenKind::Ident(n) if !bound.contains(n) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// The names a top-level statement declares.
fn declares(st: &ast::Stmt) -> Vec<String> {
    match &st.kind {
        StmtKind::Decl(d) => match &d.kind {
            DeclKind::Function(f) => f.name.iter().map(|n| n.name.clone()).collect(),
            DeclKind::Struct(s) => vec![s.name.name.clone()],
            DeclKind::Class(c) => vec![c.name.name.clone()],
            DeclKind::Enum(e) => vec![e.name.name.clone()],
            DeclKind::Interface(i) => vec![i.name.name.clone()],
            DeclKind::TypeAlias { name, .. } => vec![name.name.clone()],
            _ => Vec::new(),
        },
        StmtKind::Let { pat, .. } => match &pat.kind {
            ast::PatternKind::Ident(n) => vec![n.name.clone()],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Compiles and runs the decorators; returns their requests and what they printed.
#[allow(clippy::too_many_arguments)]
fn run_decorators(
    sm: &mut SourceMap,
    stmts: &[ast::Stmt],
    modules: &Modules,
    entry: FileId,
    cfg: &pico_interp::comptime::Config,
    decorators: &HashMap<(FileId, String), String>,
    uses: &[Use],
) -> Result<(Vec<pico_interp::comptime::MetaEdit>, String), Vec<Diagnostic>> {
    let meta_file = modules
        .names
        .iter()
        .find(|(_, n)| n.as_str() == "std/meta")
        .map(|(f, _)| *f);
    let decorated: HashSet<usize> = uses.iter().map(|u| u.stmt).collect();
    // What the decorator functions refer to, transitively (by name: an over-approximation).
    // (Per module: a name used there means its own declaration, or what it imports by
    // that name.)
    let module_of = |f: FileId| modules.file_modules.get(&f).copied().unwrap_or(f);
    let mut keep: HashSet<usize> = HashSet::new();
    let mut seen: HashSet<(FileId, String)> = HashSet::new();
    let mut work: Vec<(FileId, String)> = Vec::new();
    let library = |f: FileId| modules.prelude.contains(&f) || Some(f) == meta_file;
    for (i, st) in stmts.iter().enumerate() {
        if let StmtKind::Decl(d) = &st.kind
            && matches!(&d.kind, DeclKind::Function(f) if f.flags.decorator)
            && !library(st.span.file)
        {
            keep.insert(i);
            let m = module_of(st.span.file);
            work.extend(free_idents(sm, st.span).into_iter().map(|n| (m, n)));
        }
    }
    // The names the decorators' arguments use, in the modules that use them (#54: arguments
    // are evaluated there).
    for u in uses {
        let um = module_of(stmts[u.stmt].span.file);
        for a in &u.args {
            let (tokens, _) = pico_syntax::lex(stmts[u.stmt].span.file, a);
            for t in tokens {
                if let TokenKind::Ident(n) = t.kind {
                    work.push((um, n));
                }
            }
        }
    }
    while let Some((m, n)) = work.pop() {
        if !seen.insert((m, n.clone())) {
            continue;
        }
        // Imported: look in the module it comes from, under its name there.
        let (m, n) = match modules
            .imports
            .iter()
            .find(|i| i.module == m && i.alias.name == n && i.name.is_some())
        {
            Some(imp) => {
                let exported = &imp.name.as_ref().expect("checked").name;
                let local = modules
                    .exports
                    .get(&imp.target)
                    .and_then(|e| e.get(exported))
                    .cloned()
                    .unwrap_or_else(|| exported.clone());
                (imp.target, local)
            }
            None => (m, n),
        };
        for (i, st) in stmts.iter().enumerate() {
            if library(st.span.file) || decorated.contains(&i) || keep.contains(&i) {
                continue;
            }
            if module_of(st.span.file) == m && declares(st).contains(&n) {
                keep.insert(i);
                work.extend(free_idents(sm, st.span).into_iter().map(|n| (m, n)));
            }
        }
    }
    let mut program: Vec<ast::Stmt> = Vec::new();
    for (i, st) in stmts.iter().enumerate() {
        let library = modules.prelude.contains(&st.span.file) || Some(st.span.file) == meta_file;
        if !(library || keep.contains(&i)) {
            continue;
        }
        let mut st = st.clone();
        if let StmtKind::Decl(d) = &mut st.kind {
            let file = st.span.file;
            d.decorators.retain(|dec| {
                !matches!(&dec.path[..], [n] if find_decorator(decorators, modules, file, &n.name).is_some())
            });
            if let DeclKind::Function(f) = &mut d.kind {
                f.flags.decorator = false;
            }
        }
        program.push(st);
    }
    // In each module that uses decorators: run its uses there, so their arguments see its
    // names (`@route(PREFIX + "/x")`, #54).
    // (A name it declares, apart from decorated declarations, or imports.)
    let value_in = |m: FileId, n: &str| {
        stmts.iter().enumerate().any(|(i, st)| {
            !decorated.contains(&i)
                && module_of(st.span.file) == m
                && declares(st).iter().any(|d| d == n)
        }) || modules
            .imports
            .iter()
            .any(|i| i.module == m && i.alias.name == n)
    };
    let mut modules = modules.clone();
    let mut by_module: HashMap<FileId, String> = HashMap::new();
    for u in uses {
        let um = module_of(stmts[u.stmt].span.file);
        let mut args = vec![u.desc.clone()];
        // A bare name the using module has no value for is passed as a string (`@derive(Eq)`).
        for (a, bare) in u.args.iter().zip(&u.bare) {
            match bare {
                Some(n) if !value_in(um, n) => args.push(quote(n)),
                _ => args.push(a.clone()),
            }
        }
        let code = by_module.entry(um).or_default();
        code.push_str(&format!(
            "comptime {{ {}({}); }}\n",
            u.written,
            args.join(", ")
        ));
    }
    let mut diags = Vec::new();
    let mut files: Vec<(FileId, String)> = by_module.into_iter().collect();
    files.sort_by_key(|(f, _)| f.0);
    for (module, code) in files {
        let name = format!("<decorators of {}>", sm.file(module).name);
        let f = sm.add(name, code.clone());
        modules.file_modules.insert(f, module);
        let (m, mut d) = pico_syntax::parse_module(f, &code);
        diags.append(&mut d);
        program.extend(m.stmts);
    }
    if diags.iter().any(Diagnostic::is_error) {
        return Err(diags);
    }
    let span = Span::new(entry, 0, 0);
    let module = ast::Module {
        stmts: program,
        span,
    };
    let (hir, mut d) = pico_sema::resolve_modules(&module, entry, &modules);
    diags.append(&mut d);
    if diags.iter().any(Diagnostic::is_error) {
        return Err(diags);
    }
    let (types, mut d) = pico_sema::check(&hir);
    diags.append(&mut d);
    if diags.iter().any(Diagnostic::is_error) {
        return Err(diags);
    }
    let (mut mir, mut d) = pico_mir::lower(&hir, &types, sm, entry);
    diags.append(&mut d);
    if diags.iter().any(Diagnostic::is_error) {
        return Err(diags);
    }
    let (mut d, log, edits) = pico_interp::comptime::run_meta(&mut mir, cfg);
    diags.append(&mut d);
    if diags.iter().any(Diagnostic::is_error) {
        return Err(diags);
    }
    Ok((edits, log))
}

/// Applies the decorators' requests to the program.
fn apply(
    sm: &mut SourceMap,
    stmts: &mut Vec<ast::Stmt>,
    modules: &mut Modules,
    uses: &[Use],
    edits: &[pico_interp::comptime::MetaEdit],
    diags: &mut Vec<Diagnostic>,
    origins: &mut HashMap<FileId, (Span, String)>,
) {
    // Generated code: a file of its own in the declaration's module.
    let mut generated = |sm: &mut SourceMap, module: FileId, what: &str, code: String, at: Span| {
        let f = sm.add(format!("<{what}>"), code.clone());
        modules.file_modules.insert(f, module);
        origins.insert(f, (at, what.to_string()));
        let (m, d) = pico_syntax::parse_module(f, &code);
        (m, d)
    };
    let mut statics: Vec<(FileId, ast::Stmt)> = Vec::new();
    for e in edits {
        let Some(u) = usize::try_from(e.decl).ok().and_then(|i| uses.get(i)) else {
            continue;
        };
        let module = stmts[u.stmt].span.file;
        let dec_name = &u.decorator.1;
        match e.kind.as_str() {
            "error" => diags.push(
                Diagnostic::error(codes::DECORATOR, e.a.clone())
                    .primary(u.span, format!("reported by `@{dec_name}`")),
            ),
            "warn" | "note" => {
                let sev = if e.kind == "warn" {
                    Severity::Warning
                } else {
                    Severity::Note
                };
                diags.push(
                    Diagnostic::new(sev, e.a.clone())
                        .primary(u.span, format!("reported by `@{dec_name}`")),
                );
            }
            "static" => {
                let code = format!("const {} = {}\n", e.a, e.b);
                let (m, mut d) = generated(
                    sm,
                    module,
                    &format!("static {} of @{dec_name}", e.a),
                    code,
                    u.span,
                );
                diags.append(&mut d);
                statics.extend(m.stmts.into_iter().map(|s| (module, s)));
            }
            "member" => {
                let StmtKind::Decl(d) = &mut stmts[u.stmt].kind else {
                    continue;
                };
                // (To the class, struct or enum, whatever in it the decorator was on.)
                let kw = match &d.kind {
                    DeclKind::Struct(_) => "struct",
                    _ => "class",
                };
                let Some(members) = members_of_mut(d) else {
                    continue;
                };
                let code = format!("{kw} __Generated {{\n{}\n}}\n", e.a);
                let (m, mut ds) = generated(
                    sm,
                    module,
                    &format!("member added by @{dec_name}"),
                    code,
                    u.span,
                );
                diags.append(&mut ds);
                for st in m.stmts {
                    if let StmtKind::Decl(ast::Decl {
                        kind:
                            DeclKind::Class(ast::ClassDecl { members: ms, .. })
                            | DeclKind::Struct(ast::StructDecl { members: ms, .. }),
                        ..
                    }) = st.kind
                    {
                        // A struct's members are all public: `public` (as the AST builder
                        // writes it) says nothing there.
                        members.extend(ms.into_iter().map(|mut m| {
                            if kw == "struct" && m.mods.visibility == Some(ast::Visibility::Public)
                            {
                                m.mods.visibility = None;
                            }
                            m
                        }));
                    }
                }
            }
            "wrap" => {
                let StmtKind::Decl(d) = &mut stmts[u.stmt].kind else {
                    continue;
                };
                // The function, the method, or the one a parameter belongs to.
                let method = match u.target {
                    Target::Binding => continue,
                    Target::Decl => None,
                    Target::Member(i) => Some(i),
                    Target::Param(m, _) => m,
                };
                let Some(f) = function_of(d, method) else {
                    continue;
                };
                let code = format!("function __wrapped() {{\n{}\n}}\n", e.a);
                let (m, mut ds) = generated(
                    sm,
                    module,
                    &format!("body made by @{dec_name}"),
                    code,
                    u.span,
                );
                diags.append(&mut ds);
                let Some(body) = m.stmts.into_iter().find_map(|st| match st.kind {
                    StmtKind::Decl(ast::Decl {
                        kind: DeclKind::Function(w),
                        ..
                    }) => w.body,
                    _ => None,
                }) else {
                    continue;
                };
                let ast::FnBody::Block(mut block) = body else {
                    continue;
                };
                // `const __orig = (params): R => { the original body }`, then the new body.
                let span = f.span;
                let orig = ast::FnDecl {
                    name: None,
                    flags: ast::FnFlags {
                        arrow: true,
                        ..Default::default()
                    },
                    generics: Vec::new(),
                    params: f.params.clone(),
                    ret: f.ret.clone(),
                    throws: f.throws.clone(),
                    where_clause: Vec::new(),
                    body: f.body.take(),
                    span,
                };
                let binding = ast::Stmt {
                    kind: StmtKind::Let {
                        kind: ast::BindingKind::Const,
                        pat: ast::Pattern {
                            kind: ast::PatternKind::Ident(ast::Ident {
                                name: "__orig".into(),
                                span,
                            }),
                            span,
                        },
                        ty: None,
                        init: Some(ast::Expr {
                            kind: ast::ExprKind::Function(Box::new(orig)),
                            span,
                        }),
                        comptime: false,
                    },
                    span,
                };
                block.stmts.insert(0, binding);
                f.body = Some(ast::FnBody::Block(block));
            }
            _ => {}
        }
    }
    // Hidden statics come first in their module, so they're initialized before its code runs.
    for (module, st) in statics.into_iter().rev() {
        let at = stmts
            .iter()
            .position(|s| {
                modules
                    .file_modules
                    .get(&s.span.file)
                    .copied()
                    .unwrap_or(s.span.file)
                    == module
            })
            .unwrap_or(stmts.len());
        stmts.insert(at, st);
    }
}
