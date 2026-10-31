//! Expressions (spec §3). Precedence, lowest first:
//!
//! | level | operators |
//! |-------|-----------|
//! | assignment | `=` `+=` … `??=` (right-assoc) |
//! | ternary | `?:` |
//! | 1 | `\|\|` `??` |
//! | 2 | `&&` |
//! | 3 | `\|` |
//! | 4 | `^` |
//! | 5 | `&` |
//! | 6 | `==` `!=` |
//! | 7 | `<` `<=` `>` `>=` `instanceof` `is` |
//! | 8 | `..` |
//! | 9 | `<<` `>>` `>>>` |
//! | 10 | `+` `-` |
//! | 11 | `*` `/` `%` |
//! | 12 | `**` (right-assoc) |
//! | — | `as` (binds tighter than `**`, looser than unary) |
//! | — | prefix unary, `await`, `typeof` |
//! | — | postfix: `.` `?.` `()` `[]` `!` `++` `--` |

use super::Parser;
use crate::ast::*;
use crate::token::{Kw, Punct, TokenKind};
use ppl_diag::{Diagnostic, Span, codes};

enum BinKind {
    Op(BinOp),
    Range,
    Instanceof,
    Is,
}

fn binary_op(kind: &TokenKind) -> Option<(u8, BinKind)> {
    let TokenKind::Punct(p) = kind else {
        return match kind {
            TokenKind::Kw(Kw::Instanceof) => Some((7, BinKind::Instanceof)),
            TokenKind::Kw(Kw::Is) => Some((7, BinKind::Is)),
            _ => None,
        };
    };
    use BinOp::*;
    let (prec, op) = match p {
        Punct::OrOr => (1, Or),
        Punct::QQ => (1, Nullish),
        Punct::AndAnd => (2, And),
        Punct::Or => (3, BitOr),
        Punct::Caret => (4, BitXor),
        Punct::And => (5, BitAnd),
        Punct::EqEq => (6, Eq),
        Punct::NotEq => (6, Ne),
        Punct::Lt => (7, Lt),
        Punct::LtEq => (7, Le),
        Punct::Gt => (7, Gt),
        Punct::GtEq => (7, Ge),
        Punct::DotDot => return Some((8, BinKind::Range)),
        Punct::Shl => (9, Shl),
        Punct::Shr => (9, Shr),
        Punct::UShr => (9, UShr),
        Punct::Plus => (10, Add),
        Punct::Minus => (10, Sub),
        Punct::Star => (11, Mul),
        Punct::Slash => (11, Div),
        Punct::Percent => (11, Rem),
        Punct::StarStar => (12, Pow),
        _ => return None,
    };
    Some((prec, BinKind::Op(op)))
}

fn assign_op(kind: &TokenKind) -> Option<Option<BinOp>> {
    use BinOp::*;
    let TokenKind::Punct(p) = kind else {
        return None;
    };
    Some(match p {
        Punct::Eq => None,
        Punct::PlusEq => Some(Add),
        Punct::MinusEq => Some(Sub),
        Punct::StarEq => Some(Mul),
        Punct::SlashEq => Some(Div),
        Punct::PercentEq => Some(Rem),
        Punct::StarStarEq => Some(Pow),
        Punct::ShlEq => Some(Shl),
        Punct::ShrEq => Some(Shr),
        Punct::UShrEq => Some(UShr),
        Punct::AndEq => Some(BitAnd),
        Punct::OrEq => Some(BitOr),
        Punct::CaretEq => Some(BitXor),
        Punct::AndAndEq => Some(And),
        Punct::OrOrEq => Some(Or),
        Punct::QQEq => Some(Nullish),
        _ => return None,
    })
}

fn mk(kind: ExprKind, span: Span) -> Expr {
    Expr { kind, span }
}

/// Tokens after which a `break`/`return`/`yield` has no operand.
fn ends_operand(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Newline
            | TokenKind::Eof
            | TokenKind::Punct(
                Punct::Semi
                    | Punct::RBrace
                    | Punct::RParen
                    | Punct::RBracket
                    | Punct::Comma
                    | Punct::Colon
            )
            | TokenKind::TemplateMiddle { .. }
            | TokenKind::TemplateTail { .. }
    ) || matches!(
        kind,
        TokenKind::Kw(Kw::Else | Kw::Catch | Kw::Finally | Kw::Case | Kw::Default)
    )
}

impl Parser {
    pub(crate) fn expr(&mut self) -> Expr {
        self.assign()
    }

    fn assign(&mut self) -> Expr {
        let lhs = self.ternary();
        if let Some(op) = assign_op(self.peek()) {
            self.bump();
            self.skip_newlines();
            if !is_assignable(&lhs) {
                self.error_at(
                    codes::INVALID_ASSIGN_TARGET,
                    lhs.span,
                    "invalid assignment target",
                    "can't assign to this",
                );
            }
            let value = self.assign();
            let span = lhs.span.to(value.span);
            return mk(
                ExprKind::Assign {
                    op,
                    target: Box::new(lhs),
                    value: Box::new(value),
                },
                span,
            );
        }
        lhs
    }

    fn ternary(&mut self) -> Expr {
        let cond = self.binary(1);
        if self.at(Punct::Question) {
            self.bump();
            let then = self.assign();
            self.expect(Punct::Colon);
            let else_ = self.assign();
            let span = cond.span.to(else_.span);
            return mk(
                ExprKind::Ternary {
                    cond: Box::new(cond),
                    then: Box::new(then),
                    else_: Box::new(else_),
                },
                span,
            );
        }
        cond
    }

    fn binary(&mut self, min: u8) -> Expr {
        let mut lhs = self.as_expr();
        while let Some((prec, kind)) = binary_op(self.peek()) {
            if prec < min {
                break;
            }
            let op_span = self.bump().span;
            // `a === b`
            if matches!(kind, BinKind::Op(BinOp::Eq | BinOp::Ne)) && self.at(Punct::Eq) {
                let span = op_span.to(self.span());
                self.bump();
                self.report(
                    Diagnostic::error(codes::UNEXPECTED_TOKEN, "ppl has no `===` or `!==`")
                        .primary(span, "use `==` or `!=`")
                        .note("`==` is already strict: it never converts between types"),
                );
            }
            self.skip_newlines();
            lhs = match kind {
                BinKind::Is => {
                    let pat = self.pattern(super::pattern::PatCtx::Match);
                    let span = lhs.span.to(pat.span);
                    mk(
                        ExprKind::Is {
                            expr: Box::new(lhs),
                            pat: Box::new(pat),
                        },
                        span,
                    )
                }
                BinKind::Instanceof => {
                    let ty = self.ty_no_union();
                    let span = lhs.span.to(ty.span);
                    mk(
                        ExprKind::Instanceof {
                            expr: Box::new(lhs),
                            ty: Box::new(ty),
                        },
                        span,
                    )
                }
                BinKind::Range => {
                    let rhs = self.binary(prec + 1);
                    let span = lhs.span.to(rhs.span);
                    mk(
                        ExprKind::Range {
                            start: Box::new(lhs),
                            end: Box::new(rhs),
                        },
                        span,
                    )
                }
                BinKind::Op(op) => {
                    // `**` is right-associative.
                    let next = if op == BinOp::Pow { prec } else { prec + 1 };
                    let rhs = self.binary(next);
                    self.check_nullish_mix(op, &lhs, &rhs);
                    let span = lhs.span.to(rhs.span);
                    mk(
                        ExprKind::Binary {
                            op,
                            lhs: Box::new(lhs),
                            rhs: Box::new(rhs),
                        },
                        span,
                    )
                }
            };
        }
        lhs
    }

    /// `??` can't be mixed with `&&`/`||` without parentheses (spec §3.3).
    fn check_nullish_mix(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr) {
        let is_logic = |e: &Expr| {
            matches!(
                e.kind,
                ExprKind::Binary {
                    op: BinOp::And | BinOp::Or,
                    ..
                }
            )
        };
        let is_nullish = |e: &Expr| {
            matches!(
                e.kind,
                ExprKind::Binary {
                    op: BinOp::Nullish,
                    ..
                }
            )
        };
        let bad = match op {
            BinOp::Nullish => is_logic(lhs) || is_logic(rhs),
            BinOp::And | BinOp::Or => is_nullish(lhs) || is_nullish(rhs),
            _ => false,
        };
        if bad {
            let span = lhs.span.to(rhs.span);
            self.report(
                Diagnostic::error(
                    codes::MIXED_NULLISH,
                    "`??` can't be mixed with `&&` or `||`",
                )
                .primary(span, "add parentheses to make the order explicit"),
            );
        }
    }

    /// Continues an expression whose primary part was already parsed (a statement-level block).
    /// Only member access and calls apply; a block followed by an operator is a new statement.
    pub(super) fn postfix_and_binary(&mut self, e: Expr) -> Expr {
        if self.at(Punct::Dot) {
            self.postfix(e)
        } else {
            e
        }
    }

    fn as_expr(&mut self) -> Expr {
        let mut e = self.unary();
        while self.at_kw(Kw::As) {
            self.bump();
            let ty = self.ty_no_union();
            let span = e.span.to(ty.span);
            e = mk(
                ExprKind::As {
                    expr: Box::new(e),
                    ty: Box::new(ty),
                },
                span,
            );
        }
        e
    }

    fn unary(&mut self) -> Expr {
        let start = self.span();
        let op = match self.peek() {
            TokenKind::Punct(Punct::Minus) => Some(UnOp::Neg),
            TokenKind::Punct(Punct::Plus) => Some(UnOp::Plus),
            TokenKind::Punct(Punct::Bang) => Some(UnOp::Not),
            TokenKind::Punct(Punct::Tilde) => Some(UnOp::BitNot),
            TokenKind::Punct(Punct::PlusPlus) => Some(UnOp::PreInc),
            TokenKind::Punct(Punct::MinusMinus) => Some(UnOp::PreDec),
            TokenKind::Kw(Kw::Typeof) => Some(UnOp::Typeof),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let e = self.unary();
            let span = start.to(e.span);
            return mk(
                ExprKind::Unary {
                    op,
                    expr: Box::new(e),
                },
                span,
            );
        }
        if self.at_kw(Kw::Await) {
            self.bump();
            let e = self.unary();
            let span = start.to(e.span);
            return mk(ExprKind::Await(Box::new(e)), span);
        }
        let e = self.primary();
        self.postfix(e)
    }

    fn postfix(&mut self, mut e: Expr) -> Expr {
        loop {
            match self.peek().clone() {
                TokenKind::Punct(Punct::Dot) => {
                    self.bump();
                    let Some(prop) = self.property_name() else {
                        return e;
                    };
                    let span = e.span.to(prop.span);
                    e = mk(
                        ExprKind::Member {
                            object: Box::new(e),
                            prop,
                            optional: false,
                        },
                        span,
                    );
                }
                TokenKind::Punct(Punct::QDot) => {
                    self.bump();
                    let Some(prop) = self.property_name() else {
                        return e;
                    };
                    let span = e.span.to(prop.span);
                    e = mk(
                        ExprKind::Member {
                            object: Box::new(e),
                            prop,
                            optional: true,
                        },
                        span,
                    );
                }
                TokenKind::Punct(Punct::LParen) => {
                    let args = self.call_args();
                    let span = e.span.to(self.prev_span());
                    e = mk(
                        ExprKind::Call {
                            callee: Box::new(e),
                            type_args: Vec::new(),
                            args,
                        },
                        span,
                    );
                }
                TokenKind::Punct(Punct::LBracket) => {
                    let open = self.bump().span;
                    let index = self.expr();
                    self.expect_close(Punct::RBracket, open);
                    let span = e.span.to(self.prev_span());
                    e = mk(
                        ExprKind::Index {
                            object: Box::new(e),
                            index: Box::new(index),
                        },
                        span,
                    );
                }
                TokenKind::Punct(Punct::Bang) => {
                    let span = e.span.to(self.bump().span);
                    e = mk(ExprKind::NonNull(Box::new(e)), span);
                }
                TokenKind::Punct(Punct::PlusPlus | Punct::MinusMinus) => {
                    let tok = self.bump();
                    let op = if tok.kind.is_punct(Punct::PlusPlus) {
                        UnOp::PostInc
                    } else {
                        UnOp::PostDec
                    };
                    let span = e.span.to(tok.span);
                    e = mk(
                        ExprKind::Unary {
                            op,
                            expr: Box::new(e),
                        },
                        span,
                    );
                }
                TokenKind::Punct(Punct::Lt) => {
                    // Generic call `f<T>(…)` or generic struct literal `Pair<A, B> { … }`.
                    let Some(type_args) = self.speculate(|p| {
                        let args = p.type_args()?;
                        (p.at(Punct::LParen) || (p.at(Punct::LBrace) && is_path(&e)))
                            .then_some(args)
                    }) else {
                        return e;
                    };
                    if self.at(Punct::LParen) {
                        let args = self.call_args();
                        let span = e.span.to(self.prev_span());
                        e = mk(
                            ExprKind::Call {
                                callee: Box::new(e),
                                type_args,
                                args,
                            },
                            span,
                        );
                    } else {
                        let span = e.span;
                        let ty = path_type(&e, type_args, span);
                        e = self.object_literal(Some(ty), span);
                    }
                }
                TokenKind::Punct(Punct::LBrace) if is_path(&e) => {
                    let span = e.span;
                    let ty = path_type(&e, Vec::new(), span);
                    e = self.object_literal(Some(ty), span);
                }
                TokenKind::Template(_) | TokenKind::TemplateHead(_) => {
                    let tag = e;
                    e = self.template(Some(tag));
                }
                _ => return e,
            }
        }
    }

    /// `(a, name: b, ...c)`
    pub(super) fn call_args(&mut self) -> Vec<Arg> {
        let saved = std::mem::replace(&mut self.in_guard, false);
        let args = self.call_args_inner();
        self.in_guard = saved;
        args
    }

    fn call_args_inner(&mut self) -> Vec<Arg> {
        let open = self.bump().span; // (
        let mut args = Vec::new();
        let mut seen_named: Option<Span> = None;
        while !self.at(Punct::RParen) && !self.at_eof() {
            let start = self.span();
            let arg = if self.eat(Punct::Ellipsis) {
                Arg {
                    name: None,
                    spread: true,
                    value: self.expr(),
                }
            } else if matches!(self.peek(), TokenKind::Ident(_))
                && self.peek_nth(1).is_punct(Punct::Colon)
            {
                let name = self.ident();
                self.bump(); // :
                Arg {
                    name,
                    spread: false,
                    value: self.expr(),
                }
            } else {
                Arg {
                    name: None,
                    spread: false,
                    value: self.expr(),
                }
            };
            if arg.name.is_some() {
                seen_named.get_or_insert(start);
            } else if let Some(named) = seen_named
                && !arg.spread
            {
                self.report(
                    Diagnostic::error(
                        codes::POSITIONAL_AFTER_NAMED,
                        "positional argument after a named argument",
                    )
                    .primary(arg.value.span, "positional argument")
                    .secondary(named, "named arguments start here")
                    .help("move positional arguments before the named ones"),
                );
            }
            let failed = matches!(arg.value.kind, ExprKind::Error);
            args.push(arg);
            if failed || !self.eat(Punct::Comma) {
                break;
            }
        }
        self.expect_close(Punct::RParen, open);
        args
    }

    // ------------------------------------------------------------ primary

    fn primary(&mut self) -> Expr {
        let start = self.span();
        let tok = self.peek().clone();
        let lit = |l: Lit| ExprKind::Lit(l);
        match tok {
            TokenKind::Int { value, suffix } => {
                self.bump();
                mk(lit(Lit::Int { value, suffix }), start)
            }
            TokenKind::Float { value, suffix } => {
                self.bump();
                mk(lit(Lit::Float { value, suffix }), start)
            }
            TokenKind::Str(s) => {
                self.bump();
                mk(lit(Lit::Str(s)), start)
            }
            TokenKind::Regex { pattern, flags } => {
                self.bump();
                mk(lit(Lit::Regex { pattern, flags }), start)
            }
            TokenKind::Template(_) | TokenKind::TemplateHead(_) => self.template(None),
            TokenKind::Kw(Kw::True) => {
                self.bump();
                mk(lit(Lit::Bool(true)), start)
            }
            TokenKind::Kw(Kw::False) => {
                self.bump();
                mk(lit(Lit::Bool(false)), start)
            }
            TokenKind::Kw(Kw::Null) => {
                self.bump();
                mk(lit(Lit::Null), start)
            }
            TokenKind::Kw(Kw::This) => {
                self.bump();
                mk(ExprKind::This, start)
            }
            TokenKind::Kw(Kw::Super) => {
                self.bump();
                mk(ExprKind::Super, start)
            }
            TokenKind::Ident(name) => {
                if self.peek_nth(1).is_punct(Punct::FatArrow) && !self.in_guard {
                    return self.arrow_single_param(FnFlags {
                        arrow: true,
                        ..Default::default()
                    });
                }
                if name == "arena" && self.peek_nth(1).is_punct(Punct::LBrace) {
                    self.bump();
                    let b = self.block();
                    let span = start.to(b.span);
                    return mk(ExprKind::Arena(b), span);
                }
                self.bump();
                mk(ExprKind::Ident(name), start)
            }
            TokenKind::Punct(Punct::LParen) => self.paren_or_arrow(FnFlags {
                arrow: true,
                ..Default::default()
            }),
            TokenKind::Punct(Punct::LBracket) => self.array_literal(),
            TokenKind::Punct(Punct::LBrace) => {
                if self.brace_is_object() {
                    self.object_literal(None, start)
                } else {
                    let b = self.block();
                    let span = b.span;
                    mk(ExprKind::Block(b), span)
                }
            }
            TokenKind::Kw(Kw::Async) => {
                self.bump();
                let flags = FnFlags {
                    is_async: true,
                    arrow: true,
                    ..Default::default()
                };
                match self.peek() {
                    TokenKind::Kw(Kw::Function) => self.function_expr(true),
                    TokenKind::Ident(_) if self.peek_nth(1).is_punct(Punct::FatArrow) => {
                        self.arrow_single_param(flags)
                    }
                    TokenKind::Punct(Punct::LParen) => self.paren_or_arrow(flags),
                    _ => {
                        self.error_expected("`function` or an arrow function after `async`");
                        mk(ExprKind::Error, start)
                    }
                }
            }
            TokenKind::Kw(Kw::Function) => self.function_expr(false),
            TokenKind::Kw(Kw::If) => self.if_expr(),
            TokenKind::Kw(Kw::Match) => self.match_expr(),
            TokenKind::Kw(Kw::Switch) => self.switch_expr(),
            TokenKind::Kw(Kw::Loop) => {
                self.bump();
                self.skip_newlines();
                let b = self.block();
                let span = start.to(b.span);
                mk(ExprKind::Loop(b), span)
            }
            TokenKind::Kw(Kw::Try) => self.try_expr(),
            TokenKind::Kw(Kw::New) => self.new_expr(),
            TokenKind::Kw(Kw::Throw) => {
                self.bump();
                let e = self.expr();
                let span = start.to(e.span);
                mk(ExprKind::Throw(Box::new(e)), span)
            }
            TokenKind::Kw(Kw::Return) => {
                self.bump();
                let e = self.optional_operand();
                mk(ExprKind::Return(e), start.to(self.prev_span()))
            }
            TokenKind::Kw(Kw::Break) => {
                self.bump();
                let e = self.optional_operand();
                mk(ExprKind::Break(e), start.to(self.prev_span()))
            }
            TokenKind::Kw(Kw::Continue) => {
                self.bump();
                mk(ExprKind::Continue, start)
            }
            TokenKind::Kw(Kw::Yield) => {
                self.bump();
                let delegate = self.eat(Punct::Star);
                let e = self.optional_operand();
                mk(
                    ExprKind::Yield { expr: e, delegate },
                    start.to(self.prev_span()),
                )
            }
            TokenKind::Kw(Kw::Tailcall) => {
                self.bump();
                let e = self.unary();
                let span = start.to(e.span);
                mk(ExprKind::Tailcall(Box::new(e)), span)
            }
            TokenKind::Kw(Kw::Comptime) if !self.peek_nth(1).is_punct(Punct::Dot) => {
                self.bump();
                let e = if self.at(Punct::LBrace) {
                    let b = self.block();
                    let span = b.span;
                    mk(ExprKind::Block(b), span)
                } else {
                    self.unary()
                };
                let span = start.to(e.span);
                mk(ExprKind::Comptime(Box::new(e)), span)
            }
            TokenKind::Kw(Kw::Unsafe) if self.peek_nth(1).is_punct(Punct::LBrace) => {
                self.bump();
                let b = self.block();
                let span = start.to(b.span);
                mk(ExprKind::Unsafe(b), span)
            }
            // `comptime.readFile`, `unsafe.cast`: the keyword used as a namespace.
            TokenKind::Kw(k @ (Kw::Comptime | Kw::Unsafe)) => {
                self.bump();
                mk(ExprKind::Ident(k.as_str().into()), start)
            }
            _ => {
                self.error_expected("an expression");
                // Consume the token unless it's a likely boundary, so callers make progress.
                if !matches!(
                    self.peek(),
                    TokenKind::Newline
                        | TokenKind::Eof
                        | TokenKind::Punct(
                            Punct::RBrace | Punct::RParen | Punct::RBracket | Punct::Semi
                        )
                ) {
                    self.bump();
                }
                mk(ExprKind::Error, start)
            }
        }
    }

    fn optional_operand(&mut self) -> Option<Box<Expr>> {
        if ends_operand(self.peek()) {
            None
        } else {
            Some(Box::new(self.expr()))
        }
    }

    /// Decides whether `{` in expression position opens an object literal (spec §3.2) or a
    /// block: `{}`, `{ ...x }`, `{ name: …`, `{ name, …` and `{ name }` are objects.
    fn brace_is_object(&self) -> bool {
        let mut i = 1;
        while matches!(self.peek_nth(i), TokenKind::Newline) {
            i += 1;
        }
        match self.peek_nth(i) {
            TokenKind::Punct(Punct::RBrace | Punct::Ellipsis) => true,
            TokenKind::Ident(_) | TokenKind::Str(_) | TokenKind::Kw(_) => {
                let mut j = i + 1;
                while matches!(self.peek_nth(j), TokenKind::Newline) {
                    j += 1;
                }
                let after = self.peek_nth(j);
                matches!(
                    self.peek_nth(i + 1),
                    TokenKind::Punct(Punct::Colon | Punct::Comma)
                ) || (matches!(self.peek_nth(i), TokenKind::Ident(_))
                    && after.is_punct(Punct::RBrace))
            }
            _ => false,
        }
    }

    /// `{ a: 1, b, ...c }`, optionally preceded by a type (`Point { … }`).
    pub(super) fn object_literal(&mut self, ty: Option<Type>, start: Span) -> Expr {
        let open = self.span();
        self.expect(Punct::LBrace);
        let mut fields = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(Punct::RBrace) || self.at_eof() {
                break;
            }
            if self.eat(Punct::Ellipsis) {
                fields.push(ObjField::Spread(self.expr()));
            } else {
                let key = match self.peek().clone() {
                    TokenKind::Str(s) => {
                        let span = self.bump().span;
                        Some(PropKey::Str(s, span))
                    }
                    _ => self.property_name().map(PropKey::Ident),
                };
                let Some(key) = key else { break };
                if self.eat(Punct::Colon) {
                    self.skip_newlines();
                    fields.push(ObjField::KeyValue {
                        key,
                        value: self.expr(),
                    });
                } else if let PropKey::Ident(i) = key {
                    fields.push(ObjField::Shorthand(i));
                } else {
                    self.error_expected("`:`");
                }
            }
            self.skip_newlines();
            if !self.eat(Punct::Comma) {
                break;
            }
        }
        self.skip_newlines();
        self.expect_close(Punct::RBrace, open);
        let span = start.to(self.prev_span());
        mk(
            ExprKind::Object {
                ty: ty.map(Box::new),
                fields,
            },
            span,
        )
    }

    /// `[a, ...b]` or `[value; count]`.
    fn array_literal(&mut self) -> Expr {
        let open = self.bump().span;
        let mut elems = Vec::new();
        if !self.at(Punct::RBracket) {
            if self.eat(Punct::Ellipsis) {
                elems.push(ArrayElem::Spread(self.expr()));
            } else {
                let first = self.expr();
                if self.eat(Punct::Semi) {
                    let count = self.expr();
                    self.expect_close(Punct::RBracket, open);
                    let span = open.to(self.prev_span());
                    return mk(
                        ExprKind::ArrayRepeat {
                            value: Box::new(first),
                            count: Box::new(count),
                        },
                        span,
                    );
                }
                elems.push(ArrayElem::Expr(first));
            }
            while self.eat(Punct::Comma) {
                if self.at(Punct::RBracket) {
                    break;
                }
                if self.eat(Punct::Ellipsis) {
                    elems.push(ArrayElem::Spread(self.expr()));
                } else {
                    let e = self.expr();
                    let failed = matches!(e.kind, ExprKind::Error);
                    elems.push(ArrayElem::Expr(e));
                    if failed {
                        break;
                    }
                }
            }
        }
        self.expect_close(Punct::RBracket, open);
        mk(ExprKind::Array(elems), open.to(self.prev_span()))
    }

    /// Template literal tokens, starting at a `Template` or `TemplateHead`.
    fn template(&mut self, tag: Option<Expr>) -> Expr {
        let start = tag.as_ref().map_or(self.span(), |t| t.span);
        let mut quasis = Vec::new();
        let mut exprs = Vec::new();
        match self.bump().kind {
            TokenKind::Template(t) => quasis.push(t),
            TokenKind::TemplateHead(t) => {
                quasis.push(t);
                loop {
                    let e = self.expr();
                    match self.peek().clone() {
                        TokenKind::TemplateMiddle { spec, text } => {
                            self.bump();
                            exprs.push(TemplateExpr { expr: e, spec });
                            quasis.push(text);
                        }
                        TokenKind::TemplateTail { spec, text } => {
                            self.bump();
                            exprs.push(TemplateExpr { expr: e, spec });
                            quasis.push(text);
                            break;
                        }
                        _ => {
                            self.error_expected("`}` to end the template substitution");
                            // Skip to the end of the template.
                            while !matches!(
                                self.peek(),
                                TokenKind::TemplateTail { .. } | TokenKind::Eof
                            ) {
                                self.bump();
                            }
                            self.bump();
                            exprs.push(TemplateExpr {
                                expr: e,
                                spec: None,
                            });
                            quasis.push(String::new());
                            break;
                        }
                    }
                }
            }
            _ => unreachable!("template() called on a non-template token"),
        }
        let span = start.to(self.prev_span());
        mk(
            ExprKind::Template {
                tag: tag.map(Box::new),
                quasis,
                exprs,
            },
            span,
        )
    }

    // ------------------------------------------------------------ functions

    /// `x => body`
    fn arrow_single_param(&mut self, flags: FnFlags) -> Expr {
        let start = self.span();
        let name = self.ident().unwrap();
        let pat = Pattern {
            span: name.span,
            kind: PatternKind::Ident(name),
        };
        let param = Param {
            decorators: Vec::new(),
            mods: Modifiers::default(),
            span: pat.span,
            pat,
            ty: None,
            optional: false,
            rest: false,
            comptime: false,
            default: None,
        };
        self.bump(); // =>
        let body = self.arrow_body();
        let span = start.to(self.prev_span());
        let f = FnDecl {
            name: None,
            flags,
            generics: Vec::new(),
            params: vec![param],
            ret: None,
            throws: None,
            where_clause: Vec::new(),
            body: Some(body),
            span,
        };
        mk(ExprKind::Function(Box::new(f)), span)
    }

    fn arrow_body(&mut self) -> FnBody {
        self.skip_newlines();
        let saved = std::mem::replace(&mut self.in_guard, false);
        let body = self.arrow_body_inner();
        self.in_guard = saved;
        body
    }

    fn arrow_body_inner(&mut self) -> FnBody {
        if self.at(Punct::LBrace) {
            FnBody::Block(self.block())
        } else {
            FnBody::Expr(Box::new(self.expr()))
        }
    }

    /// `(params): Ret => body`, or a parenthesized expression.
    fn paren_or_arrow(&mut self, flags: FnFlags) -> Expr {
        let start = self.span();
        let head = self.speculate(|p| {
            let params = p.params()?;
            let ret = if p.eat(Punct::Colon) {
                Some(p.ty_no_union())
            } else {
                None
            };
            let throws = if p.eat_kw(Kw::Throws) {
                Some(p.ty())
            } else {
                None
            };
            p.at(Punct::FatArrow).then_some((params, ret, throws))
        });
        if let Some((params, ret, throws)) = head {
            self.bump(); // =>
            let body = self.arrow_body();
            let span = start.to(self.prev_span());
            let f = FnDecl {
                name: None,
                flags,
                generics: Vec::new(),
                params,
                ret,
                throws,
                where_clause: Vec::new(),
                body: Some(body),
                span,
            };
            return mk(ExprKind::Function(Box::new(f)), span);
        }
        if flags.is_async {
            self.error_expected("an arrow function after `async`");
            return mk(ExprKind::Error, start);
        }
        let open = self.bump().span;
        let inner = self.expr();
        self.expect_close(Punct::RParen, open);
        mk(ExprKind::Paren(Box::new(inner)), open.to(self.prev_span()))
    }

    fn function_expr(&mut self, is_async: bool) -> Expr {
        let start = self.span();
        let mut f = self.fn_signature(FnFlags {
            is_async,
            ..Default::default()
        });
        self.skip_newlines();
        f.body = Some(FnBody::Block(self.block()));
        f.span = start.to(self.prev_span());
        let span = f.span;
        mk(ExprKind::Function(Box::new(f)), span)
    }

    // ------------------------------------------------------------ control flow

    fn if_expr(&mut self) -> Expr {
        let start = self.bump().span; // if
        let open = self.span();
        self.expect(Punct::LParen);
        let cond = self.expr();
        self.expect_close(Punct::RParen, open);
        let then = self.branch();
        let else_ = if self.eat_newlines_before(|k| k.is_kw(Kw::Else)) || self.at_kw(Kw::Else) {
            self.bump();
            Some(Box::new(self.branch()))
        } else {
            None
        };
        let span = start.to(self.prev_span());
        mk(
            ExprKind::If {
                cond: Box::new(cond),
                then: Box::new(then),
                else_,
            },
            span,
        )
    }

    fn match_expr(&mut self) -> Expr {
        let start = self.bump().span; // match
        let open = self.span();
        self.expect(Punct::LParen);
        let scrutinee = self.expr();
        self.expect_close(Punct::RParen, open);
        self.skip_newlines();
        let lbrace = self.span();
        let mut arms = Vec::new();
        if self.expect(Punct::LBrace) {
            loop {
                self.skip_separators();
                if self.at(Punct::RBrace) || self.at_eof() {
                    break;
                }
                let arm_start = self.span();
                let pat = self.pattern(super::pattern::PatCtx::Match);
                let guard = if self.eat_kw(Kw::If) {
                    let saved = std::mem::replace(&mut self.in_guard, true);
                    let g = self.expr();
                    self.in_guard = saved;
                    Some(g)
                } else {
                    None
                };
                if !self.expect(Punct::FatArrow) {
                    self.recover_in_braces();
                    continue;
                }
                self.skip_newlines();
                let body = if self.at(Punct::LBrace) {
                    let b = self.block();
                    let span = b.span;
                    mk(ExprKind::Block(b), span)
                } else {
                    self.expr()
                };
                arms.push(MatchArm {
                    pat,
                    guard,
                    body,
                    span: arm_start.to(self.prev_span()),
                });
                self.skip_newlines();
                if !self.eat(Punct::Comma) && !self.at(Punct::RBrace) {
                    // Arms on separate lines don't need commas.
                    if !matches!(self.tokens[self.pos - 1].kind, TokenKind::Newline) {
                        self.error_expected("`,` or `}` after the match arm");
                        self.recover_in_braces();
                    }
                }
            }
            self.expect_close(Punct::RBrace, lbrace);
        }
        let span = start.to(self.prev_span());
        mk(
            ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            span,
        )
    }

    /// Skips to the next `,`/newline at this brace level, or to the closing `}`.
    pub(super) fn recover_in_braces(&mut self) {
        let mut depth = 0;
        loop {
            match self.peek() {
                TokenKind::Eof => return,
                TokenKind::Punct(Punct::RBrace) if depth == 0 => return,
                TokenKind::Punct(Punct::Comma) | TokenKind::Newline if depth == 0 => {
                    self.bump();
                    return;
                }
                TokenKind::Punct(Punct::LBrace | Punct::LParen | Punct::LBracket) => depth += 1,
                TokenKind::Punct(Punct::RBrace | Punct::RParen | Punct::RBracket) => depth -= 1,
                _ => {}
            }
            self.bump();
        }
    }

    fn switch_expr(&mut self) -> Expr {
        let start = self.bump().span; // switch
        let open = self.span();
        self.expect(Punct::LParen);
        let scrutinee = self.expr();
        self.expect_close(Punct::RParen, open);
        self.skip_newlines();
        let lbrace = self.span();
        let mut cases = Vec::new();
        if self.expect(Punct::LBrace) {
            loop {
                self.skip_separators();
                if self.at(Punct::RBrace) || self.at_eof() {
                    break;
                }
                let case_start = self.span();
                let pat = if self.eat_kw(Kw::Case) {
                    Some(self.pattern(super::pattern::PatCtx::Case))
                } else if self.eat_kw(Kw::Default) {
                    None
                } else {
                    self.error_expected("`case` or `default`");
                    self.synchronize();
                    continue;
                };
                let guard = if self.eat_kw(Kw::If) {
                    Some(self.expr())
                } else {
                    None
                };
                self.expect(Punct::Colon);
                let body_start = self.span();
                let stmts = self.stmt_list(|p| {
                    p.at_kw(Kw::Case) || p.at_kw(Kw::Default) || p.at(Punct::RBrace)
                });
                let body = Block {
                    stmts,
                    span: body_start.to(self.prev_span()),
                };
                cases.push(SwitchCase {
                    pat,
                    guard,
                    body,
                    span: case_start.to(self.prev_span()),
                });
            }
            self.expect_close(Punct::RBrace, lbrace);
        }
        let span = start.to(self.prev_span());
        mk(
            ExprKind::Switch {
                scrutinee: Box::new(scrutinee),
                cases,
            },
            span,
        )
    }

    fn try_expr(&mut self) -> Expr {
        let start = self.bump().span; // try
        // `try?` / `try!`
        if self.at(Punct::Question) || self.at(Punct::Bang) {
            let opt = self.bump().kind.is_punct(Punct::Question);
            let e = self.unary();
            let span = start.to(e.span);
            let kind = if opt {
                ExprKind::TryOpt(Box::new(e))
            } else {
                ExprKind::TryBang(Box::new(e))
            };
            return mk(kind, span);
        }
        let body = if self.at(Punct::LBrace) {
            let b = self.block();
            let span = b.span;
            mk(ExprKind::Block(b), span)
        } else {
            self.ternary()
        };
        let mut catches = Vec::new();
        while self.eat_newlines_before(|k| k.is_kw(Kw::Catch)) || self.at_kw(Kw::Catch) {
            let cstart = self.bump().span;
            let open = self.span();
            self.expect(Punct::LParen);
            let pat = self.pattern(super::pattern::PatCtx::Match);
            self.expect_close(Punct::RParen, open);
            let body = self.branch();
            catches.push(Catch {
                pat,
                body,
                span: cstart.to(self.prev_span()),
            });
        }
        let finally =
            if self.eat_newlines_before(|k| k.is_kw(Kw::Finally)) || self.at_kw(Kw::Finally) {
                self.bump();
                self.skip_newlines();
                Some(self.block())
            } else {
                None
            };
        if catches.is_empty() && finally.is_none() {
            self.error_expected("`catch` or `finally`");
        }
        let span = start.to(self.prev_span());
        mk(
            ExprKind::Try {
                body: Box::new(body),
                catches,
                finally,
            },
            span,
        )
    }

    /// `new Callee<T>(args)`
    fn new_expr(&mut self) -> Expr {
        let start = self.bump().span; // new
        let mut callee = match self.peek().clone() {
            TokenKind::Ident(name) => {
                let span = self.bump().span;
                mk(ExprKind::Ident(name), span)
            }
            TokenKind::Punct(Punct::LParen) => {
                let open = self.bump().span;
                let e = self.expr();
                self.expect_close(Punct::RParen, open);
                e
            }
            _ => {
                self.error_expected("a class name after `new`");
                return mk(ExprKind::Error, start);
            }
        };
        while self.at(Punct::Dot) {
            self.bump();
            let Some(prop) = self.property_name() else {
                break;
            };
            let span = callee.span.to(prop.span);
            callee = mk(
                ExprKind::Member {
                    object: Box::new(callee),
                    prop,
                    optional: false,
                },
                span,
            );
        }
        let type_args = if self.at(Punct::Lt) {
            self.speculate(|p| p.type_args()).unwrap_or_default()
        } else {
            Vec::new()
        };
        let args = if self.at(Punct::LParen) {
            self.call_args()
        } else {
            Vec::new()
        };
        let span = start.to(self.prev_span());
        mk(
            ExprKind::New {
                callee: Box::new(callee),
                type_args,
                args,
            },
            span,
        )
    }
}

fn is_assignable(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Ident(_)
        | ExprKind::Member {
            optional: false, ..
        }
        | ExprKind::Index { .. }
        | ExprKind::NonNull(_)
        | ExprKind::Array(_)
        | ExprKind::Object { ty: None, .. }
        | ExprKind::Error => true,
        ExprKind::Paren(inner) => is_assignable(inner),
        _ => false,
    }
}

/// A plain dotted name: `Point`, `Shape.Circle`.
fn is_path(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Ident(_) => true,
        ExprKind::Member {
            object,
            optional: false,
            ..
        } => is_path(object),
        _ => false,
    }
}

fn path_of(e: &Expr, out: &mut Path) {
    match &e.kind {
        ExprKind::Ident(n) => out.push(Ident {
            name: n.clone(),
            span: e.span,
        }),
        ExprKind::Member { object, prop, .. } => {
            path_of(object, out);
            out.push(prop.clone());
        }
        _ => {}
    }
}

fn path_type(e: &Expr, args: Vec<Type>, span: Span) -> Type {
    let mut path = Vec::new();
    path_of(e, &mut path);
    Type {
        kind: TypeKind::Path { path, args },
        span,
    }
}
