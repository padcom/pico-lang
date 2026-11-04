//! `naming` (decision 39): camelCase values, functions, methods, fields and parameters;
//! PascalCase types, enum variants and type parameters. Constants and static fields may be
//! UPPER_SNAKE_CASE. Leading underscores are ignored; `extern "C"` declarations (C's names)
//! aren't checked.

use crate::walk::{self, Visit, bound_names};
use pico_diag::{Diagnostic, codes};
use pico_syntax::ast::*;

pub fn check(m: &Module, out: &mut Vec<Diagnostic>) {
    let mut n = Naming {
        out,
        in_extern: false,
    };
    for s in &m.stmts {
        n.stmt(s);
    }
}

struct Naming<'o> {
    out: &'o mut Vec<Diagnostic>,
    in_extern: bool,
}

#[derive(Clone, Copy)]
enum Case {
    Camel,
    /// camelCase or UPPER_SNAKE_CASE.
    CamelOrUpper,
    /// `comptime` parameters: values (camelCase) or types (PascalCase).
    CamelOrPascal,
    Pascal,
}

impl Naming<'_> {
    fn name(&mut self, n: &Ident, case: Case, what: &str) {
        if self.in_extern {
            return;
        }
        let bare = n.name.trim_start_matches('_');
        if bare.is_empty() {
            return;
        }
        let ok = match case {
            Case::Camel => is_camel(bare),
            Case::CamelOrUpper => is_camel(bare) || is_upper_snake(bare),
            Case::CamelOrPascal => is_camel(bare) || is_pascal(bare),
            Case::Pascal => is_pascal(bare),
        };
        if ok {
            return;
        }
        let (style, fixed) = match case {
            Case::Pascal => ("PascalCase", to_pascal(&n.name)),
            _ => ("camelCase", to_camel(&n.name)),
        };
        let mut d = Diagnostic::warning(format!("{what} `{}` should be {style}", n.name))
            .with_code(codes::LINT_NAMING)
            .primary(n.span, format!("not {style}"));
        if fixed != n.name && !fixed.is_empty() {
            d = d.help(format!("rename it to `{fixed}`"));
        }
        self.out.push(d);
    }

    fn generics(&mut self, gs: &[GenericParam]) {
        for g in gs {
            self.name(&g.name, Case::Pascal, "type parameter");
        }
    }

    fn params(&mut self, f: &FnDecl) {
        for p in &f.params {
            let mut names = Vec::new();
            bound_names(&p.pat, &mut names);
            let case = if p.comptime {
                Case::CamelOrPascal
            } else {
                Case::Camel
            };
            for n in names {
                self.name(n, case, "parameter");
            }
        }
    }
}

impl<'a> Visit<'a> for Naming<'_> {
    fn stmt(&mut self, s: &'a Stmt) {
        match &s.kind {
            StmtKind::Let { kind, pat, .. } => {
                let (case, what) = match kind {
                    BindingKind::Const => (Case::CamelOrUpper, "constant"),
                    BindingKind::Let => (Case::Camel, "variable"),
                };
                let mut names = Vec::new();
                bound_names(pat, &mut names);
                for n in names {
                    self.name(n, case, what);
                }
            }
            StmtKind::ForOf { pat, .. } => {
                let mut names = Vec::new();
                bound_names(pat, &mut names);
                for n in names {
                    self.name(n, Case::Camel, "variable");
                }
            }
            StmtKind::Using { name, .. } => self.name(name, Case::Camel, "variable"),
            _ => {}
        }
        walk::walk_stmt(self, s);
    }

    fn decl(&mut self, d: &'a Decl) {
        match &d.kind {
            DeclKind::Function(f) => {
                if let Some(n) = &f.name {
                    self.name(n, Case::Camel, "function");
                }
            }
            DeclKind::Struct(s) => {
                self.name(&s.name, Case::Pascal, "struct");
                self.generics(&s.generics);
            }
            DeclKind::Class(c) => {
                self.name(&c.name, Case::Pascal, "class");
                self.generics(&c.generics);
            }
            DeclKind::Interface(i) => {
                self.name(&i.name, Case::Pascal, "interface");
                self.generics(&i.generics);
            }
            DeclKind::Enum(e) => {
                self.name(&e.name, Case::Pascal, "enum");
                self.generics(&e.generics);
                for v in &e.variants {
                    self.name(&v.name, Case::Pascal, "enum variant");
                }
            }
            DeclKind::TypeAlias { name, generics, .. } => {
                self.name(name, Case::Pascal, "type");
                self.generics(generics);
            }
            DeclKind::Extern { items, .. } => {
                let was = std::mem::replace(&mut self.in_extern, true);
                items.iter().for_each(|i| self.decl(i));
                self.in_extern = was;
                return;
            }
            _ => {}
        }
        walk::walk_decl(self, d);
    }

    fn member(&mut self, m: &'a Member) {
        match &m.kind {
            MemberKind::Field { name, .. } => {
                let case = if m.mods.is_static {
                    Case::CamelOrUpper
                } else {
                    Case::Camel
                };
                self.name(name, case, "field");
            }
            MemberKind::Method(f) | MemberKind::Getter(f) | MemberKind::Setter(f) => {
                if let Some(n) = &f.name {
                    self.name(n, Case::Camel, "method");
                }
            }
            _ => {}
        }
        walk::walk_member(self, m);
    }

    fn func(&mut self, f: &'a FnDecl) {
        self.generics(&f.generics);
        self.params(f);
        walk::walk_fn(self, f);
    }

    fn pattern(&mut self, p: &'a Pattern) {
        // Bindings of match arms, `is` and `catch` (`let`/parameters are named above).
        if let PatternKind::Typed { name, .. } | PatternKind::As { name, .. } = &p.kind {
            self.name(name, Case::Camel, "variable");
        }
    }
}

fn is_camel(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '$')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '$')
}

fn is_pascal(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_ascii_uppercase()) && cs.all(|c| c.is_ascii_alphanumeric())
}

fn is_upper_snake(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_uppercase())
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// `my_var` → `myVar`, `MyVar` → `myVar` (leading underscores kept).
fn to_camel(s: &str) -> String {
    let lead = s.len() - s.trim_start_matches('_').len();
    let p = to_pascal(&s[lead..]);
    let mut cs = p.chars();
    let first = cs.next().map(|c| c.to_ascii_lowercase());
    format!(
        "{}{}{}",
        &s[..lead],
        first.map(String::from).unwrap_or_default(),
        cs.as_str()
    )
}

/// `my_var` → `MyVar`, `MY_THING` → `MyThing`, `point` → `Point`.
fn to_pascal(s: &str) -> String {
    let lead = s.len() - s.trim_start_matches('_').len();
    let words: Vec<&str> = s[lead..].split('_').filter(|w| !w.is_empty()).collect();
    let all_upper = words
        .iter()
        .all(|w| !w.chars().any(|c| c.is_ascii_lowercase()));
    let mut out = s[..lead].to_string();
    for w in words {
        let mut cs = w.chars();
        if let Some(c) = cs.next() {
            out.push(c.to_ascii_uppercase());
            let rest = cs.as_str();
            if all_upper {
                out.push_str(&rest.to_ascii_lowercase());
            } else {
                out.push_str(rest);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_conversions() {
        assert_eq!(to_camel("my_var"), "myVar");
        assert_eq!(to_camel("MyVar"), "myVar");
        assert_eq!(to_camel("_my_var"), "_myVar");
        assert_eq!(to_pascal("my_point"), "MyPoint");
        assert_eq!(to_pascal("MY_THING"), "MyThing");
        assert!(is_camel("parseURL") && !is_camel("Parse") && !is_camel("my_var"));
        assert!(is_pascal("HTTPServer") && !is_pascal("point"));
        assert!(is_upper_snake("MAX_SIZE") && !is_upper_snake("Max"));
    }
}
