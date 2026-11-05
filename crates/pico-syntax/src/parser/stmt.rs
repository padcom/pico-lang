//! Statements and blocks (spec §3.1, §4.1, §4.3).

use super::Parser;
use crate::ast::*;
use crate::token::{Kw, Punct, TokenKind};
use pico_diag::{Diagnostic, codes};

impl Parser {
    /// Statements until `done` (not consumed). Each statement must be followed by a terminator,
    /// unless it ends with `}` or the list ends.
    pub(super) fn stmt_list(&mut self, done: impl Fn(&Parser) -> bool) -> Vec<Stmt> {
        let mut stmts = Vec::new();
        loop {
            self.skip_separators();
            if done(self) || self.at_eof() {
                break;
            }
            let before = self.pos;
            let diags_before = self.diags.len();
            let mut stmt = self.stmt();
            let had_errors = self.diags.len() > diags_before;
            let ended_with_brace =
                self.pos > before && self.tokens[self.pos - 1].kind.is_punct(Punct::RBrace);
            if self.at(Punct::Semi) {
                self.bump();
                if let StmtKind::Expr { semi, .. } = &mut stmt.kind {
                    *semi = true;
                }
            } else if matches!(self.peek(), TokenKind::Newline) {
                self.bump();
            } else if !(done(self) || self.at_eof() || ended_with_brace) {
                if !had_errors && self.last_error_pos != Some(self.pos) {
                    self.last_error_pos = Some(self.pos);
                    let d = Diagnostic::error(
                        codes::MISSING_SEPARATOR,
                        format!(
                            "expected a newline or `;` after the statement, found {}",
                            self.peek().describe()
                        ),
                    )
                    .primary(self.span(), "unexpected here")
                    .help("put each statement on its own line, or separate them with `;`");
                    self.report(d);
                }
                self.synchronize();
            }
            if self.pos == before {
                // No progress: skip the offending token.
                self.bump();
            }
            stmts.push(stmt);
        }
        stmts
    }

    /// `{ stmts }`
    pub(super) fn block(&mut self) -> Block {
        let open = self.span();
        if !self.expect(Punct::LBrace) {
            return Block {
                stmts: Vec::new(),
                span: open,
            };
        }
        let stmts = self.stmt_list(|p| p.at(Punct::RBrace));
        let end = self.span();
        self.expect_close(Punct::RBrace, open);
        Block {
            stmts,
            span: open.to(end),
        }
    }

    pub(super) fn stmt(&mut self) -> Stmt {
        let start = self.span();
        let kind = match self.peek().clone() {
            TokenKind::Kw(Kw::Const | Kw::Let) => self.let_stmt(false),
            TokenKind::Ident(w)
                if w == "var" && matches!(self.peek_nth(1), TokenKind::Ident(_)) =>
            {
                let span = self.span();
                self.report(
                    Diagnostic::error(codes::UNEXPECTED_TOKEN, "pico has no `var`")
                        .primary(span, "not a keyword in pico")
                        .help("use `let` for a binding you reassign, or `const`"),
                );
                self.last_error_pos = Some(self.pos);
                // Parse the rest as `let` so later code still gets checked.
                self.tokens[self.pos].kind = TokenKind::Kw(Kw::Let);
                self.let_stmt(false)
            }
            TokenKind::Kw(Kw::Comptime)
                if matches!(self.peek_nth(1), TokenKind::Kw(Kw::Const | Kw::Let)) =>
            {
                self.bump();
                self.let_stmt(true)
            }
            // `reactive name …` (spec §16.1; `reactive` is contextual).
            TokenKind::Ident(w)
                if w == "reactive" && matches!(self.peek_nth(1), TokenKind::Ident(_)) =>
            {
                self.let_stmt(false)
            }
            TokenKind::Kw(Kw::Using) => self.using_stmt(false),
            TokenKind::Kw(Kw::Await) if self.peek_nth(1).is_kw(Kw::Using) => {
                self.bump();
                self.using_stmt(true)
            }
            // A `{` at the start of a statement always opens a block (spec §3.2).
            TokenKind::Punct(Punct::LBrace) => {
                let b = self.block();
                let span = b.span;
                let block = Expr {
                    kind: ExprKind::Block(b),
                    span,
                };
                let expr = self.postfix_and_binary(block);
                StmtKind::Expr { expr, semi: false }
            }
            TokenKind::Kw(Kw::While) => self.while_stmt(),
            TokenKind::Kw(Kw::Do) => self.do_while_stmt(),
            TokenKind::Kw(Kw::For) => self.for_stmt(),
            _ if self.at_decl_start() => match self.decl() {
                Some(d) => StmtKind::Decl(d),
                None => {
                    self.synchronize();
                    StmtKind::Error
                }
            },
            _ => {
                let expr = self.expr();
                if matches!(expr.kind, ExprKind::Error) {
                    self.synchronize();
                    StmtKind::Error
                } else {
                    StmtKind::Expr { expr, semi: false }
                }
            }
        };
        Stmt {
            kind,
            span: start.to(self.prev_span()),
        }
    }

    /// `const pat: T = init` (after an optional `comptime`).
    pub(super) fn let_stmt(&mut self, comptime: bool) -> StmtKind {
        let kind = if self.eat_kw(Kw::Const) {
            BindingKind::Const
        } else if self.eat_word("reactive") {
            BindingKind::Reactive
        } else {
            self.bump();
            BindingKind::Let
        };
        let pat = self.binding_pattern();
        let ty = if self.eat(Punct::Colon) {
            Some(self.ty())
        } else {
            None
        };
        let init = if self.eat(Punct::Eq) {
            self.skip_newlines();
            Some(self.expr())
        } else {
            None
        };
        StmtKind::Let {
            kind,
            pat,
            ty,
            init,
            comptime,
        }
    }

    fn using_stmt(&mut self, is_await: bool) -> StmtKind {
        self.bump(); // using
        let name = self.ident().unwrap_or_else(|| self.error_ident());
        self.expect(Punct::Eq);
        let init = self.expr();
        StmtKind::Using {
            is_await,
            name,
            init,
        }
    }

    /// The body of `if`, `while`, `for`: a block, an expression, or a loop statement.
    pub(super) fn branch(&mut self) -> Expr {
        self.skip_newlines();
        if self.at(Punct::LBrace) {
            let b = self.block();
            let span = b.span;
            return Expr {
                kind: ExprKind::Block(b),
                span,
            };
        }
        if matches!(
            self.peek(),
            TokenKind::Kw(Kw::For | Kw::While | Kw::Do | Kw::Const | Kw::Let)
        ) {
            let s = self.stmt();
            let span = s.span;
            return Expr {
                kind: ExprKind::Block(Block {
                    stmts: vec![s],
                    span,
                }),
                span,
            };
        }
        self.expr()
    }

    fn paren_cond(&mut self) -> Expr {
        let open = self.span();
        if !self.eat(Punct::LParen) {
            let found = self.peek().describe();
            self.report(
                Diagnostic::error(
                    codes::UNEXPECTED_TOKEN,
                    format!("expected `(`, found {found}"),
                )
                .primary(open, "expected `(`")
                .help("conditions are written in parentheses: `while (x > 0) { … }`"),
            );
            self.last_error_pos = Some(self.pos);
            return self.expr();
        }
        let cond = self.expr();
        self.expect_close(Punct::RParen, open);
        cond
    }

    fn while_stmt(&mut self) -> StmtKind {
        self.bump();
        let cond = self.paren_cond();
        let body = Box::new(self.branch());
        StmtKind::While { cond, body }
    }

    fn do_while_stmt(&mut self) -> StmtKind {
        self.bump();
        self.skip_newlines();
        let body = self.block();
        self.eat_newlines_before(|k| k.is_kw(Kw::While));
        if !self.expect_kw(Kw::While) {
            self.synchronize();
            let span = self.span();
            let cond = Expr {
                kind: ExprKind::Error,
                span,
            };
            return StmtKind::DoWhile { body, cond };
        }
        let cond = self.paren_cond();
        StmtKind::DoWhile { body, cond }
    }

    fn for_stmt(&mut self) -> StmtKind {
        self.bump(); // for
        let is_await = self.eat_kw(Kw::Await);
        let open = self.span();
        self.expect(Punct::LParen);

        // for (const pat of iter)
        if matches!(self.peek(), TokenKind::Kw(Kw::Const | Kw::Let)) {
            let snap = self.snapshot();
            let kind = if self.eat_kw(Kw::Const) {
                BindingKind::Const
            } else {
                self.bump();
                BindingKind::Let
            };
            let pat = self.binding_pattern();
            if self.eat_word("of") {
                let iter = self.expr();
                self.expect_close(Punct::RParen, open);
                let body = Box::new(self.branch());
                return StmtKind::ForOf {
                    is_await,
                    kind,
                    pat,
                    iter,
                    body,
                };
            }
            if self.at_kw(Kw::In) {
                let span = self.bump().span;
                self.report(
                    Diagnostic::error(codes::UNEXPECTED_TOKEN, "pico has no `for-in` loop")
                        .primary(span, "expected `of`")
                        .help("iterate over keys with `for (const k of obj.keys())`"),
                );
                let iter = self.expr();
                self.expect_close(Punct::RParen, open);
                let body = Box::new(self.branch());
                return StmtKind::ForOf {
                    is_await,
                    kind,
                    pat,
                    iter,
                    body,
                };
            }
            self.restore(snap);
        }
        if is_await {
            self.error_expected("`const` or `let` (for await (const x of …))");
        }

        // C-style: for (init; cond; update)
        let init = if self.at(Punct::Semi) {
            None
        } else if matches!(self.peek(), TokenKind::Kw(Kw::Const | Kw::Let)) {
            let start = self.span();
            let kind = self.let_stmt(false);
            Some(Box::new(Stmt {
                kind,
                span: start.to(self.prev_span()),
            }))
        } else {
            let start = self.span();
            let expr = self.expr();
            Some(Box::new(Stmt {
                kind: StmtKind::Expr { expr, semi: true },
                span: start.to(self.prev_span()),
            }))
        };
        self.expect(Punct::Semi);
        let cond = if self.at(Punct::Semi) {
            None
        } else {
            Some(self.expr())
        };
        self.expect(Punct::Semi);
        let update = if self.at(Punct::RParen) {
            None
        } else {
            Some(self.expr())
        };
        self.expect_close(Punct::RParen, open);
        let body = Box::new(self.branch());
        StmtKind::For {
            init,
            cond,
            update,
            body,
        }
    }
}
