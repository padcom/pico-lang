//! `js:` imports (spec §12.3, decision 259): a subset of TypeScript declaration files
//! (`.d.ts`) mapped, best effort, to a ppl module of typed wrappers over `std/js`.
//!
//! The parser reads interfaces (merged by name), `declare var`/`declare function`, type
//! aliases and classes; namespaces, `export =`, generics and anything else it can't read are
//! skipped. The generator emits the names a program imports and, for what they use, the
//! interfaces up to [`DEPTH`] steps away (base interfaces always): each one a class that
//! extends `JsObject` (or its first base) with getters/setters for properties and methods
//! calling through `JsValue`. Types map as the spec says: `number` → `f64`, `string`,
//! `boolean` → `bool`, `T | null`/`T | undefined` → `T?`, string-literal unions → `string`,
//! included interfaces → their classes; the rest is `JsValue` (results) or `JsArg`
//! (parameters). Every member `throws JsError`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write;

/// How many steps of member types are followed from the imported names.
pub const DEPTH: u32 = 1;

// ---------------------------------------------------------------- lexing

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num(String),
    Punct(char),
    /// `=>`
    Arrow,
    /// `...`
    Dots,
}

fn lex(src: &str) -> Vec<Tok> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if c.is_alphabetic() || c == '_' || c == '$' {
            let s = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '$') {
                i += 1;
            }
            out.push(Tok::Ident(b[s..i].iter().collect()));
        } else if c.is_ascii_digit() {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '.') {
                i += 1;
            }
            out.push(Tok::Num(b[s..i].iter().collect()));
        } else if c == '"' || c == '\'' || c == '`' {
            let q = c;
            i += 1;
            let mut s = String::new();
            while i < b.len() && b[i] != q {
                if b[i] == '\\' && i + 1 < b.len() {
                    i += 1;
                }
                s.push(b[i]);
                i += 1;
            }
            i += 1;
            out.push(Tok::Str(s));
        } else if c == '=' && b.get(i + 1) == Some(&'>') {
            out.push(Tok::Arrow);
            i += 2;
        } else if c == '.' && b.get(i + 1) == Some(&'.') && b.get(i + 2) == Some(&'.') {
            out.push(Tok::Dots);
            i += 3;
        } else {
            out.push(Tok::Punct(c));
            i += 1;
        }
    }
    out
}

// ---------------------------------------------------------------- syntax

/// A TypeScript type, as far as the mapping cares.
#[derive(Clone, Debug, PartialEq)]
pub enum TsType {
    Named(String, Vec<TsType>),
    Union(Vec<TsType>),
    Array(Box<TsType>),
    Func,
    Object(Vec<Member>),
    StrLit,
    NumLit,
    BoolLit,
    /// Anything else (conditional, mapped, indexed, `keyof`, tuples, intersections…).
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: TsType,
    pub optional: bool,
    pub rest: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Member {
    Prop {
        name: String,
        ty: TsType,
        readonly: bool,
        optional: bool,
    },
    Method {
        name: String,
        params: Vec<Param>,
        ret: TsType,
    },
    Get {
        name: String,
        ty: TsType,
    },
    Set {
        name: String,
        ty: TsType,
    },
    Construct {
        params: Vec<Param>,
        ret: TsType,
    },
    Other,
}

#[derive(Clone, Debug, Default)]
pub struct Iface {
    pub bases: Vec<String>,
    pub members: Vec<Member>,
    pub generic: bool,
}

/// What a declaration file declares.
#[derive(Debug, Default)]
pub struct Decls {
    pub ifaces: BTreeMap<String, Iface>,
    pub vars: BTreeMap<String, TsType>,
    pub fns: BTreeMap<String, (Vec<Param>, TsType)>,
    pub aliases: HashMap<String, TsType>,
}

struct P {
    t: Vec<Tok>,
    i: usize,
    /// Type parameters in scope (mapped to their default or constraint).
    tparams: Vec<HashMap<String, TsType>>,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn peek_at(&self, k: usize) -> Option<&Tok> {
        self.t.get(self.i + k)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.t.get(self.i).cloned();
        self.i += 1;
        t
    }
    fn is(&self, c: char) -> bool {
        self.peek() == Some(&Tok::Punct(c))
    }
    fn eat(&mut self, c: char) -> bool {
        if self.is(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s == w)
    }
    fn eat_word(&mut self, w: &str) -> bool {
        if self.is_word(w) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn ident(&mut self) -> Option<String> {
        match self.peek() {
            Some(Tok::Ident(s)) => {
                let s = s.clone();
                self.i += 1;
                Some(s)
            }
            _ => None,
        }
    }

    /// Skips a balanced group starting at the current opener, or one token.
    fn skip_balanced(&mut self) {
        let (open, close) = match self.peek() {
            Some(Tok::Punct('{')) => ('{', '}'),
            Some(Tok::Punct('(')) => ('(', ')'),
            Some(Tok::Punct('[')) => ('[', ']'),
            Some(Tok::Punct('<')) => ('<', '>'),
            _ => {
                self.i += 1;
                return;
            }
        };
        let mut depth = 0;
        while let Some(t) = self.next() {
            if t == Tok::Punct(open) {
                depth += 1;
            } else if t == Tok::Punct(close) {
                depth -= 1;
                if depth == 0 {
                    return;
                }
            } else if open == '<' && t == Tok::Arrow {
                // (`=>` inside type arguments isn't a `>`.)
            }
        }
    }

    /// Skips to the end of a statement or member (`;` or a newline-less `}`), balancing groups.
    fn skip_member(&mut self) {
        while let Some(t) = self.peek() {
            match t {
                Tok::Punct(';') | Tok::Punct(',') => {
                    self.i += 1;
                    return;
                }
                Tok::Punct('}') => return,
                Tok::Punct('{') | Tok::Punct('(') | Tok::Punct('[') => self.skip_balanced(),
                _ => self.i += 1,
            }
        }
    }

    /// `<T extends U = V, …>`: the parameters with what they stand for.
    fn type_params(&mut self) -> HashMap<String, TsType> {
        let mut out = HashMap::new();
        if !self.eat('<') {
            return out;
        }
        while let Some(name) = self.ident() {
            let mut ty = TsType::Other;
            if self.eat_word("extends") {
                ty = self.ty();
            }
            if self.eat('=') {
                ty = self.ty();
            }
            out.insert(name, ty);
            if !self.eat(',') {
                break;
            }
        }
        while !self.eat('>') && self.peek().is_some() {
            self.i += 1;
        }
        out
    }

    fn ty(&mut self) -> TsType {
        // A leading `|` or `&`.
        self.eat('|');
        self.eat('&');
        let mut parts = vec![self.ty_postfix()];
        let mut inter = false;
        loop {
            if self.eat('|') {
                parts.push(self.ty_postfix());
            } else if self.eat('&') {
                inter = true;
                self.ty_postfix();
            } else {
                break;
            }
        }
        // Conditional types: `A extends B ? C : D`.
        if self.is_word("extends") {
            self.i += 1;
            self.ty();
            if self.eat('?') {
                self.ty();
                self.eat(':');
                self.ty();
            }
            return TsType::Other;
        }
        if inter {
            return TsType::Other;
        }
        if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            TsType::Union(parts)
        }
    }

    fn ty_postfix(&mut self) -> TsType {
        let mut t = self.ty_atom();
        while self.is('[') {
            if self.peek_at(1) == Some(&Tok::Punct(']')) {
                self.i += 2;
                t = TsType::Array(Box::new(t));
            } else {
                self.skip_balanced();
                t = TsType::Other;
            }
        }
        t
    }

    fn ty_atom(&mut self) -> TsType {
        match self.peek().cloned() {
            Some(Tok::Punct('(')) => {
                // A function type `(a: T) => R`, or a parenthesized type.
                let save = self.i;
                self.skip_balanced();
                if self.peek() == Some(&Tok::Arrow) {
                    self.i += 1;
                    self.ty();
                    return TsType::Func;
                }
                self.i = save + 1;
                let t = self.ty();
                self.eat(')');
                t
            }
            Some(Tok::Punct('<')) => {
                // A generic function type.
                self.skip_balanced();
                self.ty_atom()
            }
            Some(Tok::Punct('{')) => {
                self.i += 1;
                let members = self.members();
                TsType::Object(members)
            }
            Some(Tok::Punct('[')) => {
                self.skip_balanced();
                TsType::Other
            }
            Some(Tok::Str(_)) => {
                self.i += 1;
                TsType::StrLit
            }
            Some(Tok::Num(_)) => {
                self.i += 1;
                TsType::NumLit
            }
            Some(Tok::Punct('-')) => {
                self.i += 1;
                self.ty_atom();
                TsType::NumLit
            }
            Some(Tok::Ident(w)) => {
                self.i += 1;
                match w.as_str() {
                    "true" | "false" => return TsType::BoolLit,
                    "keyof" | "typeof" | "unique" | "readonly" | "infer" | "asserts" => {
                        self.ty_atom();
                        return TsType::Other;
                    }
                    "new" | "abstract" => {
                        self.eat_word("new");
                        if self.is('<') {
                            self.skip_balanced();
                        }
                        self.skip_balanced();
                        if self.peek() == Some(&Tok::Arrow) {
                            self.i += 1;
                            self.ty();
                        }
                        return TsType::Other;
                    }
                    _ => {}
                }
                let mut name = w;
                while self.is('.') {
                    self.i += 1;
                    if let Some(n) = self.ident() {
                        name = format!("{name}.{n}");
                    }
                }
                // `x is T` (type predicates).
                if self.is_word("is") {
                    self.i += 1;
                    self.ty();
                    return TsType::Named("boolean".into(), Vec::new());
                }
                let mut args = Vec::new();
                if self.eat('<') {
                    loop {
                        args.push(self.ty());
                        if !self.eat(',') {
                            break;
                        }
                    }
                    self.eat('>');
                }
                for scope in self.tparams.iter().rev() {
                    if let Some(t) = scope.get(&name) {
                        return t.clone();
                    }
                }
                TsType::Named(name, args)
            }
            _ => {
                self.i += 1;
                TsType::Other
            }
        }
    }

    fn params(&mut self) -> Vec<Param> {
        let mut out = Vec::new();
        if !self.eat('(') {
            return out;
        }
        while !self.eat(')') {
            if self.peek().is_none() {
                break;
            }
            let rest = self.peek() == Some(&Tok::Dots);
            if rest {
                self.i += 1;
            }
            let name = match self.next() {
                Some(Tok::Ident(s)) => s,
                _ => {
                    self.skip_member();
                    continue;
                }
            };
            let optional = self.eat('?');
            let ty = if self.eat(':') {
                self.ty()
            } else {
                TsType::Other
            };
            if name != "this" {
                out.push(Param {
                    name,
                    ty,
                    optional,
                    rest,
                });
            }
            self.eat(',');
        }
        out
    }

    /// Members up to the closing `}` (consumed).
    fn members(&mut self) -> Vec<Member> {
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => break,
                Some(Tok::Punct('}')) => {
                    self.i += 1;
                    break;
                }
                Some(Tok::Punct(';')) | Some(Tok::Punct(',')) => {
                    self.i += 1;
                }
                _ => out.push(self.member()),
            }
        }
        out
    }

    fn member(&mut self) -> Member {
        let start = self.i;
        let mut readonly = false;
        loop {
            if (self.is_word("readonly")
                || self.is_word("static")
                || self.is_word("public")
                || self.is_word("protected")
                || self.is_word("private"))
                && !matches!(self.peek_at(1), Some(Tok::Punct(':' | '(' | '?' | ';')))
            {
                readonly |= self.is_word("readonly");
                self.i += 1;
            } else {
                break;
            }
        }
        // Accessors.
        for (kw, is_get) in [("get", true), ("set", false)] {
            if self.is_word(kw) && matches!(self.peek_at(1), Some(Tok::Ident(_) | Tok::Str(_))) {
                self.i += 1;
                let name = match self.next() {
                    Some(Tok::Ident(s)) | Some(Tok::Str(s)) => s,
                    _ => String::new(),
                };
                let params = self.params();
                let ty = if self.eat(':') {
                    self.ty()
                } else {
                    params
                        .first()
                        .map(|p| p.ty.clone())
                        .unwrap_or(TsType::Other)
                };
                self.skip_member();
                return if is_get {
                    Member::Get { name, ty }
                } else {
                    Member::Set { name, ty }
                };
            }
        }
        if self.is('[') {
            // Index signatures and computed names.
            self.skip_member();
            return Member::Other;
        }
        if self.eat_word("new") && (self.is('(') || self.is('<')) {
            let tp = self.type_params();
            self.tparams.push(tp);
            let params = self.params();
            let ret = if self.eat(':') {
                self.ty()
            } else {
                TsType::Other
            };
            self.tparams.pop();
            self.skip_member();
            return Member::Construct { params, ret };
        }
        if self.is('(') || self.is('<') {
            // A call signature.
            self.skip_member();
            return Member::Other;
        }
        let name = match self.next() {
            Some(Tok::Ident(s)) | Some(Tok::Str(s)) | Some(Tok::Num(s)) => s,
            _ => {
                self.i = start + 1;
                self.skip_member();
                return Member::Other;
            }
        };
        let optional = self.eat('?');
        if self.is('(') || self.is('<') {
            let tp = self.type_params();
            self.tparams.push(tp);
            let params = self.params();
            let ret = if self.eat(':') {
                self.ty()
            } else {
                TsType::Other
            };
            self.tparams.pop();
            self.skip_member();
            return Member::Method { name, params, ret };
        }
        let ty = if self.eat(':') {
            self.ty()
        } else {
            TsType::Other
        };
        self.skip_member();
        Member::Prop {
            name,
            ty,
            readonly,
            optional,
        }
    }
}

/// Parses a declaration file (what the mapping can read of it).
pub fn parse(src: &str) -> Decls {
    let mut p = P {
        t: lex(src),
        i: 0,
        tparams: Vec::new(),
    };
    let mut d = Decls::default();
    while p.peek().is_some() {
        // Modifiers.
        while p.eat_word("declare") || p.eat_word("export") || p.eat_word("default") {}
        if p.eat_word("interface") || p.eat_word("class") {
            let Some(name) = p.ident() else { continue };
            let tp = p.type_params();
            let generic = !tp.is_empty();
            p.tparams.push(tp);
            let mut bases = Vec::new();
            while p.eat_word("extends") || p.eat_word("implements") || p.eat(',') {
                if let TsType::Named(b, _) = p.ty_postfix() {
                    bases.push(b);
                }
            }
            if !p.eat('{') {
                p.tparams.pop();
                continue;
            }
            let members = p.members();
            p.tparams.pop();
            let e = d.ifaces.entry(name).or_default();
            e.generic |= generic;
            for b in bases {
                if !e.bases.contains(&b) {
                    e.bases.push(b);
                }
            }
            e.members.extend(members);
        } else if p.eat_word("var") || p.eat_word("let") || p.eat_word("const") {
            let Some(name) = p.ident() else { continue };
            let ty = if p.eat(':') { p.ty() } else { TsType::Other };
            p.skip_member();
            d.vars.insert(name, ty);
        } else if p.eat_word("function") {
            let Some(name) = p.ident() else { continue };
            let tp = p.type_params();
            p.tparams.push(tp);
            let params = p.params();
            let ret = if p.eat(':') { p.ty() } else { TsType::Other };
            p.tparams.pop();
            p.skip_member();
            // (The last overload is usually the most general one.)
            d.fns.insert(name, (params, ret));
        } else if p.eat_word("type") {
            let Some(name) = p.ident() else { continue };
            let tp = p.type_params();
            p.tparams.push(tp);
            p.eat('=');
            let t = p.ty();
            p.tparams.pop();
            p.skip_member();
            d.aliases.insert(name, t);
        } else if p.eat_word("namespace") || p.eat_word("module") || p.eat_word("enum") {
            while p.peek().is_some() && !p.is('{') {
                p.i += 1;
            }
            p.skip_balanced();
        } else {
            p.skip_member();
            if p.is('}') {
                p.i += 1;
            }
        }
    }
    d
}

// ---------------------------------------------------------------- mapping

/// What a TypeScript type is in ppl.
#[derive(Clone, Debug, PartialEq)]
enum Mapped {
    Num,
    Str,
    Bool,
    Void,
    Class(String),
    Value,
    Opt(Box<Mapped>),
}

impl Mapped {
    fn ppl(&self) -> String {
        match self {
            Mapped::Num => "f64".into(),
            Mapped::Str => "string".into(),
            Mapped::Bool => "bool".into(),
            Mapped::Void => "void".into(),
            Mapped::Class(c) => ppl_ident(c),
            Mapped::Value => "JsValue".into(),
            Mapped::Opt(t) => format!("{}?", t.ppl()),
        }
    }
}

struct Gen<'a> {
    d: &'a Decls,
    /// The interfaces made into classes.
    classes: BTreeSet<String>,
    out: String,
    /// The module expression the generated code reads globals from.
    module: String,
}

/// The ppl module for `import { names } from "js:<module>"`, from declarations `d`.
pub fn generate(d: &Decls, module: &str, names: &BTreeSet<String>) -> String {
    let mut g = Gen {
        d,
        classes: BTreeSet::new(),
        out: String::new(),
        module: module.to_string(),
    };
    g.closure(names);
    let _ = writeln!(
        g.out,
        "// Generated by ppl from TypeScript declarations for `js:{module}` (decision 259).\nimport {{ JsValue, JsError, JsArg, JsObject, js, jsNull, __jsArray, __jsCallRest }} from \"std/js\"\n"
    );
    for c in g.classes.clone() {
        g.class(&c);
    }
    for n in names {
        if g.d.fns.contains_key(n) {
            g.function(n);
        } else if let Some(t) = g.d.vars.get(n).cloned()
            && !g.classes.contains(n)
        {
            // (A class's constructor variable is the class: `X.create(…)`.)
            g.var(n, &t);
        }
    }
    g.out
}

impl Gen<'_> {
    /// The interfaces to generate: the imported ones, those up to `DEPTH` member steps away,
    /// and all their bases.
    fn closure(&mut self, names: &BTreeSet<String>) {
        let mut work: Vec<(String, u32)> = Vec::new();
        for n in names {
            if self.d.ifaces.contains_key(n) {
                work.push((n.clone(), 0));
            } else if let Some(t) = self.d.vars.get(n) {
                self.refs(t, 0, &mut work);
            } else if let Some((ps, r)) = self.d.fns.get(n) {
                for p in ps {
                    self.refs(&p.ty, 0, &mut work);
                }
                self.refs(r, 0, &mut work);
            }
        }
        while let Some((n, depth)) = work.pop() {
            let Some(i) = self.d.ifaces.get(&n) else {
                continue;
            };
            if i.generic || self.classes.contains(&n) || keyword(&n) {
                continue;
            }
            self.classes.insert(n.clone());
            for b in &i.bases {
                work.push((b.clone(), depth));
            }
            if depth < DEPTH {
                for m in &i.members {
                    match m {
                        Member::Prop { ty, .. } | Member::Get { ty, .. } => {
                            self.refs(ty, depth + 1, &mut work)
                        }
                        Member::Method { params, ret, .. } => {
                            self.refs(ret, depth + 1, &mut work);
                            for p in params {
                                self.refs(&p.ty, depth + 1, &mut work);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // A base that isn't generated (generic, or unknown) is left out of the chain.
    }

    fn refs(&self, t: &TsType, depth: u32, work: &mut Vec<(String, u32)>) {
        match t {
            TsType::Named(n, _) => {
                if let Some(a) = self.d.aliases.get(n) {
                    if depth <= DEPTH + 1 {
                        self.refs(a, depth, work);
                    }
                } else if self.d.ifaces.contains_key(n) {
                    work.push((n.clone(), depth));
                }
            }
            TsType::Union(ts) => {
                for t in ts {
                    self.refs(t, depth, work);
                }
            }
            TsType::Object(ms) => {
                for m in ms {
                    if let Member::Construct { ret, .. } = m {
                        self.refs(ret, depth, work);
                    }
                }
            }
            _ => {}
        }
    }

    fn map(&self, t: &TsType) -> Mapped {
        self.map_in(t, 0)
    }

    fn map_in(&self, t: &TsType, depth: u32) -> Mapped {
        if depth > 8 {
            return Mapped::Value;
        }
        match t {
            TsType::Named(n, _) => match n.as_str() {
                "number" => Mapped::Num,
                "string" => Mapped::Str,
                "boolean" => Mapped::Bool,
                "void" | "undefined" | "never" => Mapped::Void,
                _ => {
                    if let Some(a) = self.d.aliases.get(n) {
                        self.map_in(a, depth + 1)
                    } else if self.classes.contains(n) {
                        Mapped::Class(n.clone())
                    } else {
                        Mapped::Value
                    }
                }
            },
            TsType::StrLit => Mapped::Str,
            TsType::NumLit => Mapped::Num,
            TsType::BoolLit => Mapped::Bool,
            TsType::Union(ts) => {
                let nullish = |t: &TsType| matches!(t, TsType::Named(n, _) if n == "null" || n == "undefined" || n == "void");
                let rest: Vec<Mapped> = ts
                    .iter()
                    .filter(|t| !nullish(t))
                    .map(|t| self.map_in(t, depth + 1))
                    .collect();
                let base = match rest.as_slice() {
                    [] => return Mapped::Value,
                    [one] => one.clone(),
                    [first, others @ ..] if others.iter().all(|o| o == first) => first.clone(),
                    _ => Mapped::Value,
                };
                if rest.len() < ts.len() && base != Mapped::Value {
                    Mapped::Opt(Box::new(base))
                } else {
                    base
                }
            }
            _ => Mapped::Value,
        }
    }

    /// The members a class declares itself: its own and those of its other bases (mixins), minus
    /// what its first generated base chain already has.
    fn own_members(&self, name: &str) -> Vec<Member> {
        let i = &self.d.ifaces[name];
        let inherited = self
            .base(name)
            .map(|b| self.all_names(&b))
            .unwrap_or_default();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut out = Vec::new();
        let mut add = |ms: &[Member], out: &mut Vec<Member>| {
            // (The last overload of a method wins, so walk backwards.)
            for m in ms.iter().rev() {
                let n = member_name(m);
                let key = match m {
                    Member::Set { .. } => format!("set {n}"),
                    _ => n.to_string(),
                };
                if n.is_empty() || inherited.contains(n) || !seen.insert(key) {
                    continue;
                }
                out.push(m.clone());
            }
        };
        add(&i.members, &mut out);
        let first = self.base(name);
        let mut mixins: Vec<String> = i
            .bases
            .iter()
            .filter(|b| Some(*b) != first.as_ref())
            .cloned()
            .collect();
        let mut k = 0;
        while k < mixins.len() {
            if let Some(m) = self.d.ifaces.get(&mixins[k]) {
                add(&m.members, &mut out);
                for b in &m.bases {
                    if !mixins.contains(b) && !inherited.contains(b) {
                        mixins.push(b.clone());
                    }
                }
            }
            k += 1;
        }
        out.reverse();
        out
    }

    /// The generated base class: the first base that's generated.
    fn base(&self, name: &str) -> Option<String> {
        self.d.ifaces[name]
            .bases
            .iter()
            .find(|b| self.classes.contains(*b))
            .cloned()
    }

    /// Every member name a class has, through its chain.
    fn all_names(&self, name: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut cur = Some(name.to_string());
        while let Some(c) = cur {
            for m in self.own_members(&c) {
                out.insert(member_name(&m).to_string());
            }
            cur = self.base(&c);
        }
        out
    }

    fn class(&mut self, name: &str) {
        let base = self.base(name);
        let members = self.own_members(name);
        let id = ppl_ident(name);
        let _ = writeln!(
            self.out,
            "export class {id} extends {} {{\n  constructor(v: JsValue) {{\n    super(v)\n  }}",
            base.as_deref().map(ppl_ident).unwrap_or("JsObject".into())
        );
        // A constructor (`declare var X: { new(…): X }`): `X.create(…)`.
        if let Some(TsType::Object(ms)) = self.d.vars.get(name)
            && let Some(Member::Construct { params, .. }) = ms
                .iter()
                .rev()
                .find(|m| matches!(m, Member::Construct { .. }))
        {
            let (sig, args) = self.params(params);
            let module = self.module.clone();
            let _ = writeln!(
                self.out,
                "  public static create({sig}): {id} throws JsError {{\n    new {id}(js.module(\"{module}\").get(\"{name}\").construct({args}))\n  }}"
            );
        }
        let setters: BTreeSet<String> = members
            .iter()
            .filter_map(|m| match m {
                Member::Set { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        for m in &members {
            match m {
                Member::Prop {
                    name,
                    ty,
                    readonly,
                    optional,
                } => {
                    let mut t = self.map(ty);
                    if *optional && !matches!(t, Mapped::Opt(_) | Mapped::Value) {
                        t = Mapped::Opt(Box::new(t));
                    }
                    self.getter(name, &t);
                    if !readonly {
                        self.setter(name, &t);
                    }
                }
                Member::Get { name, ty } => {
                    let t = self.map(ty);
                    self.getter(name, &t);
                    if setters.contains(name) {
                        self.setter(name, &t);
                    }
                }
                Member::Method { name, params, ret } => self.method(name, params, ret),
                _ => {}
            }
        }
        self.out.push_str("}\n\n");
    }

    fn getter(&mut self, name: &str, t: &Mapped) {
        if t == &Mapped::Void || !ppl_member_ok(name) {
            return;
        }
        let body = convert(t, &format!("this.jsValue.get({name:?})"));
        let _ = writeln!(
            self.out,
            "  public get {}(): {} throws JsError {{\n{body}  }}",
            ppl_ident(name),
            t.ppl()
        );
    }

    fn setter(&mut self, name: &str, t: &Mapped) {
        if t == &Mapped::Void || !ppl_member_ok(name) {
            return;
        }
        let _ = writeln!(
            self.out,
            "  public set {}(v: {}) throws JsError {{\n    this.jsValue.set({name:?}, {});\n  }}",
            ppl_ident(name),
            param_type(t),
            arg("v", t)
        );
    }

    fn method(&mut self, name: &str, params: &[Param], ret: &TsType) {
        if !ppl_member_ok(name) {
            return;
        }
        let r = self.map(ret);
        let (sig, args, rest) = self.params_rest(params);
        let call = Self::call_expr("this.jsValue", name, &args, &rest);
        let _ = writeln!(
            self.out,
            "  public {}({sig}): {} throws JsError {{\n{}  }}",
            ppl_ident(name),
            r.ppl(),
            convert(&r, &call)
        );
    }

    /// A signature's ppl parameters and the call's arguments.
    fn params(&self, params: &[Param]) -> (String, String) {
        let (sig, args, _) = self.params_rest(params);
        (sig, args)
    }

    /// [`Gen::params`], and the rest parameter's name (its arguments are spread).
    fn params_rest(&self, params: &[Param]) -> (String, String, Option<String>) {
        let mut sig = Vec::new();
        let mut args = Vec::new();
        let mut optional = false;
        for p in params {
            let n = format!("{}_", ppl_ident(&p.name).trim_end_matches('_'));
            if p.rest {
                sig.push(format!("...{n}: JsArg[]"));
                return (sig.join(", "), args.join(", "), Some(n));
            }
            let t = self.map(&p.ty);
            optional |= p.optional;
            if optional {
                let inner = match &t {
                    Mapped::Opt(i) => (**i).clone(),
                    t => t.clone(),
                };
                if inner == Mapped::Void {
                    continue;
                }
                sig.push(format!("{n}: {}? = null", param_type(&inner)));
                args.push(arg(&n, &Mapped::Opt(Box::new(inner))));
            } else {
                if t == Mapped::Void {
                    continue;
                }
                sig.push(format!("{n}: {}", param_type(&t)));
                args.push(arg(&n, &t));
            }
        }
        (sig.join(", "), args.join(", "), None)
    }

    /// The call of `target[name]` with these arguments.
    fn call_expr(target: &str, name: &str, args: &str, rest: &Option<String>) -> String {
        match rest {
            Some(r) => format!("__jsCallRest({target}, {name:?}, [{args}], {r})"),
            None => format!(
                "{target}.call({name:?}{}{args})",
                if args.is_empty() { "" } else { ", " }
            ),
        }
    }

    fn function(&mut self, name: &str) {
        if !ppl_member_ok(name) {
            return;
        }
        let (params, ret) = self.d.fns[name].clone();
        let r = self.map(&ret);
        let (sig, args, rest) = self.params_rest(&params);
        let target = format!("js.module({:?})", self.module);
        let call = Self::call_expr(&target, name, &args, &rest);
        let _ = writeln!(
            self.out,
            "export function {}({sig}): {} throws JsError {{\n{}}}\n",
            ppl_ident(name),
            r.ppl(),
            convert(&r, &call)
        );
    }

    fn var(&mut self, name: &str, t: &TsType) {
        let m = self.map(t);
        if m == Mapped::Void {
            return;
        }
        let get = format!("js.module({:?}).get({name:?})", self.module);
        // (Read once, at the program's start: a missing global panics there.)
        let body = convert(&m, &get).replace("\n    ", "\n      ");
        let _ = writeln!(
            self.out,
            "function __get_{name}(): {t} {{\n  try {{\n  {body}  }} catch (e: JsError) {{\n    panic(`js:{module}: {name}: ${{e.message}}`)\n  }}\n}}\n\nexport const {id}: {t} = __get_{name}()\n",
            t = m.ppl(),
            module = self.module,
            id = ppl_ident(name),
        );
    }
}

/// The body that converts JavaScript value `call` to `t`.
fn convert(t: &Mapped, call: &str) -> String {
    match t {
        Mapped::Void => format!("    {call};\n"),
        Mapped::Value => format!("    {call}\n"),
        Mapped::Num => format!("    {call}.asNumber()\n"),
        Mapped::Str => format!("    {call}.asString()\n"),
        Mapped::Bool => format!("    {call}.asBool()\n"),
        Mapped::Class(c) => format!("    new {}({call})\n", ppl_ident(c)),
        Mapped::Opt(inner) => {
            let v = match &**inner {
                Mapped::Num => "r.asNumber()".to_string(),
                Mapped::Str => "r.asString()".to_string(),
                Mapped::Bool => "r.asBool()".to_string(),
                Mapped::Class(c) => format!("new {}(r)", ppl_ident(c)),
                _ => "r".to_string(),
            };
            format!(
                "    const r = {call}\n    if (r.isNullish()) {{\n      return null\n    }}\n    {v}\n"
            )
        }
    }
}

/// What a parameter of type `t` takes in ppl (unknown types: any `JsArg`).
fn param_type(t: &Mapped) -> String {
    match t {
        Mapped::Value => "JsArg".into(),
        Mapped::Opt(i) if **i == Mapped::Value => "JsArg?".into(),
        t => t.ppl(),
    }
}

/// The argument expression (a `JsArg`) for parameter `n` of type `t`.
fn arg(n: &str, t: &Mapped) -> String {
    match t {
        Mapped::Class(_) => format!("{n}.jsValue"),
        Mapped::Opt(i) => {
            let inner = match &**i {
                Mapped::Class(_) => format!("JsValue.from({n}.jsValue)"),
                _ => format!("JsValue.from({n})"),
            };
            format!("(if ({n} != null) {inner} else jsNull())")
        }
        _ => n.to_string(),
    }
}

fn member_name(m: &Member) -> &str {
    match m {
        Member::Prop { name, .. }
        | Member::Method { name, .. }
        | Member::Get { name, .. }
        | Member::Set { name, .. } => name,
        _ => "",
    }
}

const PPL_KEYWORDS: &[&str] = &[
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
];

fn keyword(n: &str) -> bool {
    PPL_KEYWORDS.contains(&n)
}

/// A ppl identifier for a name (keywords get a `_` after them).
fn ppl_ident(n: &str) -> String {
    let s: String = n
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if keyword(&s) { format!("{s}_") } else { s }
}

/// Members the wrapper classes can't have: `JsObject`'s own and ppl's reserved names.
fn ppl_member_ok(n: &str) -> bool {
    !matches!(
        n,
        "jsValue" | "constructor" | "toString" | "equals" | "hashCode"
    ) && n
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
interface EventTarget { addEventListener(type: string, listener: EventListener | null, options?: boolean): void; }
interface Node extends EventTarget {
    readonly nodeName: string;
    textContent: string | null;
    appendChild<T extends Node>(node: T): T;
}
interface Element extends Node, ParentNode { id: string; getAttribute(qualifiedName: string): string | null; }
interface ParentNode { querySelector<E extends Element = Element>(selectors: string): E | null; }
interface Document extends Node {
    readonly body: HTMLElement;
    title: string;
    createElement(tagName: string, options?: ElementCreationOptions): HTMLElement;
    getElementById(elementId: string): HTMLElement | null;
}
interface HTMLElement extends Element { hidden: boolean; click(): void; }
type DocumentReadyState = "complete" | "interactive" | "loading";
declare var document: Document;
declare var HTMLElement: { prototype: HTMLElement; new(): HTMLElement; };
declare function alert(message?: any): void;
declare namespace CSS { function escape(ident: string): string; }
"#;

    #[test]
    fn parses_and_generates() {
        let d = parse(SAMPLE);
        assert_eq!(d.ifaces["Element"].bases, ["Node", "ParentNode"]);
        assert!(d.vars.contains_key("document"));
        assert!(d.fns.contains_key("alert"));
        assert!(!d.fns.contains_key("escape"));
        let names: BTreeSet<String> = ["document", "alert"].map(String::from).into();
        let out = generate(&d, "dom", &names);
        assert!(
            out.contains("export class Document extends Node {"),
            "{out}"
        );
        assert!(
            out.contains("public get title(): string throws JsError"),
            "{out}"
        );
        assert!(
            out.contains("public getElementById(elementId_: string): HTMLElement? throws JsError"),
            "{out}"
        );
        // (Mixins are flattened in; the type parameter is its default.)
        assert!(
            out.contains("public querySelector(selectors_: string): Element? throws JsError"),
            "{out}"
        );
        assert!(
            out.contains("export function alert(message_: JsArg? = null): void throws JsError"),
            "{out}"
        );
        assert!(
            out.contains("public static create(): HTMLElement throws JsError"),
            "{out}"
        );
    }
}
