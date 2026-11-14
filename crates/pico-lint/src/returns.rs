//! `explicitReturn`: a function whose result is a lone variable returns it with `return`
//! (`return total`, not a final `total`). Other final expressions (`a + b`, a call, a literal,
//! an `if`) stay as they are. Arrow functions with an expression body (`x => x`), constructors,
//! setters, generators and functions declared `: void` have no result to return, so they're
//! exempt; blocks used as values (`const a = { …; w }`) aren't functions and are exempt too.

use crate::walk::{self, Visit};
use pico_diag::{Diagnostic, codes};
use pico_syntax::ast::*;

pub fn check(m: &Module, text: &str, out: &mut Vec<Diagnostic>) {
    let mut r = Returns { text, out };
    for s in &m.stmts {
        r.stmt(s);
    }
}

struct Returns<'t, 'o> {
    text: &'t str,
    out: &'o mut Vec<Diagnostic>,
}

impl<'a> Visit<'a> for Returns<'_, '_> {
    fn member(&mut self, m: &'a Member) {
        if let MemberKind::Constructor(f) | MemberKind::Setter(f) = &m.kind {
            // No result; only the closures in the body are looked at.
            walk::walk_fn(self, f);
            return;
        }
        walk::walk_member(self, m);
    }

    fn func(&mut self, f: &'a FnDecl) {
        self.check(f);
        walk::walk_fn(self, f);
    }
}

impl Returns<'_, '_> {
    fn check(&mut self, f: &FnDecl) {
        let Some(FnBody::Block(b)) = &f.body else {
            return;
        };
        let returns_void = f
            .ret
            .as_ref()
            .is_some_and(|t| &self.text[t.span.start as usize..t.span.end as usize] == "void");
        if f.flags.generator || returns_void {
            return;
        }
        let Some(Stmt {
            kind: StmtKind::Expr { expr, semi: false },
            ..
        }) = b.stmts.last()
        else {
            return;
        };
        let Some(name) = lone_variable(expr) else {
            return;
        };
        self.out.push(
            Diagnostic::warning(format!(
                "the function's result is the variable `{name}`: return it with `return`"
            ))
            .with_code(codes::LINT_EXPLICIT_RETURN)
            .primary(expr.span, "the function's result")
            .help(format!("write `return {name}`")),
        );
    }
}

/// The name of a lone variable (`x`, `(x)`).
fn lone_variable(e: &Expr) -> Option<&str> {
    match &e.kind {
        ExprKind::Ident(name) => Some(name),
        ExprKind::Paren(x) => lone_variable(x),
        _ => None,
    }
}
