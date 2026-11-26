//! `explicitReturn`: a function whose result is a simple value returns it with `return`:
//! a variable, a literal (number, boolean, `null`, string, template, array, object), an
//! operation (`a + b`, `!x`, `a ?? b`, `c ? a : b`, `x as T`, `x!`), a member read or an index
//! (`return total`, `return 1`, `return a + b`, not a final `total`, `1` or `a + b`). Calls,
//! `new`, `if`, `match`, `switch`, blocks, `try`, `await` and closures stay implicit results.
//! Arrow functions with an expression body (`x => x`), constructors, setters, generators and
//! functions declared `: void` have no result to return, so they're exempt; blocks used as
//! values (`const a = { …; w }`) aren't functions and are exempt too.

use crate::walk::{self, Visit};
use pico_diag::{Diagnostic, Span, codes};
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
        let Some(what) = returnable(expr) else {
            return;
        };
        let src = &self.text[expr.span.start as usize..expr.span.end as usize];
        let msg = match what {
            Returnable::Variable(name) => {
                format!("the function's result is the variable `{name}`: return it with `return`")
            }
            Returnable::Value(kind) => {
                format!("the function's result is {kind}: return it with `return`")
            }
        };
        let help = if src.contains('\n') || src.contains('`') || src.chars().count() > 40 {
            "put `return` before it".to_string()
        } else {
            format!("write `return {src}`")
        };
        self.out.push(
            Diagnostic::warning(msg)
                .with_code(codes::LINT_EXPLICIT_RETURN)
                .primary(expr.span, "the function's result")
                .help(help)
                .fix(
                    Span::new(expr.span.file, expr.span.start, expr.span.start),
                    "return ",
                ),
        );
    }
}

enum Returnable<'e> {
    Variable(&'e str),
    /// Another simple value, described (`a number`, `an operation`).
    Value(&'static str),
}

/// A result that needs `return` (see the module's doc), described.
fn returnable(e: &Expr) -> Option<Returnable<'_>> {
    use Returnable::Value;
    Some(match &e.kind {
        ExprKind::Ident(name) => Returnable::Variable(name),
        ExprKind::Paren(x) => return returnable(x),
        ExprKind::Lit(l) => Value(match l {
            Lit::Int { .. } | Lit::Float { .. } => "a number",
            Lit::Str(_) => "a string",
            Lit::Bool(_) => "a boolean",
            Lit::Null => "`null`",
            _ => "a literal",
        }),
        ExprKind::Template { tag: None, .. } => Value("a template"),
        ExprKind::Array(_) | ExprKind::ArrayRepeat { .. } => Value("an array literal"),
        ExprKind::Object { .. } => Value("an object literal"),
        ExprKind::Binary { .. }
        | ExprKind::Unary { .. }
        | ExprKind::Ternary { .. }
        | ExprKind::As { .. }
        | ExprKind::Is { .. }
        | ExprKind::Instanceof { .. }
        | ExprKind::NonNull(_)
        | ExprKind::Range { .. } => Value("an operation"),
        ExprKind::Member { .. } => Value("a member read"),
        ExprKind::Index { .. } => Value("an element read"),
        _ => return None,
    })
}
