//! Type syntax (spec §2): unions, `T?`, `T[]`, `[T; N]`, tuples, generics, function types,
//! object type literals, literal types, `immutable`, `distinct`.

use super::Parser;
use crate::ast::*;
use crate::token::{Kw, Punct, TokenKind};
use pico_diag::Span;

fn mk(kind: TypeKind, span: Span) -> Type {
    Type { kind, span }
}

impl Parser {
    /// A full type, including unions (`A | B`) and an optional leading `|`.
    pub(crate) fn ty(&mut self) -> Type {
        let start = self.span();
        self.eat(Punct::Or);
        let first = self.ty_no_union();
        if !self.at(Punct::Or) {
            return first;
        }
        let mut members = vec![first];
        while self.eat(Punct::Or) {
            members.push(self.ty_no_union());
        }
        let span = start.to(self.prev_span());
        mk(TypeKind::Union(members), span)
    }

    /// A type without a top-level `|` (used after `as`, in typed patterns, …).
    pub(crate) fn ty_no_union(&mut self) -> Type {
        let start = self.span();
        let first = self.ty_postfix();
        if !self.at(Punct::And) {
            return first;
        }
        let mut members = vec![first];
        while self.eat(Punct::And) {
            members.push(self.ty_postfix());
        }
        mk(TypeKind::Intersection(members), start.to(self.prev_span()))
    }

    fn ty_postfix(&mut self) -> Type {
        let mut t = self.ty_primary();
        loop {
            if self.at(Punct::LBracket) && self.peek_nth(1).is_punct(Punct::RBracket) {
                self.bump();
                self.bump();
                let span = t.span.to(self.prev_span());
                t = mk(TypeKind::Array(Box::new(t)), span);
            } else if self.at(Punct::Question) {
                self.bump();
                let span = t.span.to(self.prev_span());
                t = mk(TypeKind::Nullable(Box::new(t)), span);
            } else {
                return t;
            }
        }
    }

    fn ty_primary(&mut self) -> Type {
        let start = self.span();
        match self.peek().clone() {
            TokenKind::Kw(Kw::Immutable) => {
                self.bump();
                let inner = self.ty_postfix();
                mk(
                    TypeKind::Immutable(Box::new(inner)),
                    start.to(self.prev_span()),
                )
            }
            TokenKind::Kw(Kw::Distinct) => {
                self.bump();
                let inner = self.ty_postfix();
                mk(
                    TypeKind::Distinct(Box::new(inner)),
                    start.to(self.prev_span()),
                )
            }
            TokenKind::Ident(_) => {
                let mut path = vec![self.ident().unwrap()];
                while self.at(Punct::Dot) {
                    self.bump();
                    match self.ident() {
                        Some(i) => path.push(i),
                        None => break,
                    }
                }
                let args = if self.at(Punct::Lt) {
                    self.type_args().unwrap_or_default()
                } else {
                    Vec::new()
                };
                mk(TypeKind::Path { path, args }, start.to(self.prev_span()))
            }
            TokenKind::Kw(Kw::Null) => {
                self.bump();
                mk(TypeKind::Lit(Lit::Null), start)
            }
            TokenKind::Kw(Kw::True) => {
                self.bump();
                mk(TypeKind::Lit(Lit::Bool(true)), start)
            }
            TokenKind::Kw(Kw::False) => {
                self.bump();
                mk(TypeKind::Lit(Lit::Bool(false)), start)
            }
            TokenKind::Str(s) => {
                self.bump();
                mk(TypeKind::Lit(Lit::Str(s)), start)
            }
            TokenKind::Int { value, suffix } => {
                self.bump();
                mk(TypeKind::Lit(Lit::Int { value, suffix }), start)
            }
            TokenKind::Float { value, suffix } => {
                self.bump();
                mk(TypeKind::Lit(Lit::Float { value, suffix }), start)
            }
            TokenKind::Kw(Kw::Extern) => {
                self.bump();
                let abi = match self.peek().clone() {
                    TokenKind::Str(s) => {
                        self.bump();
                        s
                    }
                    _ => {
                        self.error_expected("an ABI string like \"C\"");
                        "C".into()
                    }
                };
                self.fn_type(start, Some(abi))
                    .unwrap_or_else(|| mk(TypeKind::Error, start.to(self.prev_span())))
            }
            TokenKind::Punct(Punct::LParen) => {
                if let Some(f) = self.speculate(|p| p.fn_type(start, None)) {
                    return f;
                }
                let open = self.bump().span;
                let inner = self.ty();
                self.expect_close(Punct::RParen, open);
                inner
            }
            TokenKind::Punct(Punct::LBracket) => {
                let open = self.bump().span;
                if self.eat(Punct::RBracket) {
                    return mk(TypeKind::Tuple(Vec::new()), open.to(self.prev_span()));
                }
                let first = self.ty();
                if self.eat(Punct::Semi) {
                    let len = self.expr();
                    self.expect_close(Punct::RBracket, open);
                    return mk(
                        TypeKind::Fixed {
                            elem: Box::new(first),
                            len: Box::new(len),
                        },
                        open.to(self.prev_span()),
                    );
                }
                let mut elems = vec![first];
                while self.eat(Punct::Comma) {
                    if self.at(Punct::RBracket) {
                        break;
                    }
                    elems.push(self.ty());
                }
                self.expect_close(Punct::RBracket, open);
                mk(TypeKind::Tuple(elems), open.to(self.prev_span()))
            }
            TokenKind::Punct(Punct::LBrace) => self.object_type(),
            _ => {
                self.error_expected("a type");
                mk(TypeKind::Error, start)
            }
        }
    }

    /// `(a: A, b?: B, ...c: C[]) => R throws E`; also accepts unnamed parameters `(A) => R`.
    fn fn_type(&mut self, start: Span, extern_abi: Option<String>) -> Option<Type> {
        let open = self.span();
        if !self.eat(Punct::LParen) {
            self.error_expected("`(`");
            return None;
        }
        let mut params = Vec::new();
        while !self.at(Punct::RParen) && !self.at_eof() {
            let rest = self.eat(Punct::Ellipsis);
            let named = matches!(self.peek(), TokenKind::Ident(_))
                && (self.peek_nth(1).is_punct(Punct::Colon)
                    || (self.peek_nth(1).is_punct(Punct::Question)
                        && self.peek_nth(2).is_punct(Punct::Colon)));
            let (name, optional) = if named {
                let n = self.ident();
                let opt = self.eat(Punct::Question);
                self.bump(); // :
                (n, opt)
            } else {
                (None, false)
            };
            let ty = self.ty();
            params.push(FnTypeParam {
                name,
                ty,
                optional,
                rest,
            });
            if !self.eat(Punct::Comma) {
                break;
            }
        }
        if !self.expect_close(Punct::RParen, open) {
            return None;
        }
        if !self.eat(Punct::FatArrow) {
            self.error_expected("`=>`");
            return None;
        }
        let ret = self.ty();
        let throws = if self.eat_kw(Kw::Throws) {
            Some(Box::new(self.ty()))
        } else {
            None
        };
        Some(mk(
            TypeKind::Function {
                params,
                ret: Box::new(ret),
                throws,
                extern_abi,
            },
            start.to(self.prev_span()),
        ))
    }

    /// `{ x: f64, y?: f64, readonly z: T }`, separated by `,`, `;` or newlines.
    fn object_type(&mut self) -> Type {
        let open = self.bump().span;
        let mut fields = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(Punct::RBrace) || self.at_eof() {
                break;
            }
            let readonly =
                self.at_kw(Kw::Readonly) && !self.peek_nth(1).is_punct(Punct::Colon) && {
                    self.bump();
                    true
                };
            let Some(name) = self.property_name() else {
                break;
            };
            let optional = self.eat(Punct::Question);
            self.expect(Punct::Colon);
            let ty = self.ty();
            fields.push(ObjectTypeField {
                name,
                ty,
                optional,
                readonly,
            });
            self.skip_newlines();
            if !(self.eat(Punct::Comma) || self.eat(Punct::Semi)) {
                break;
            }
        }
        self.skip_newlines();
        self.expect_close(Punct::RBrace, open);
        mk(TypeKind::Object(fields), open.to(self.prev_span()))
    }

    /// `<A, B>` in a type or call. Numeric literals become [`TypeKind::Const`]. Returns `None`
    /// (with an error) if the list doesn't close.
    pub(crate) fn type_args(&mut self) -> Option<Vec<Type>> {
        let open = self.span();
        if !self.eat(Punct::Lt) {
            return None;
        }
        let mut args = Vec::new();
        loop {
            let t = match self.peek() {
                TokenKind::Int { .. } | TokenKind::Punct(Punct::Minus) => {
                    let e = self.binary_const();
                    let span = e.span;
                    mk(TypeKind::Const(Box::new(e)), span)
                }
                _ => self.ty(),
            };
            if matches!(t.kind, TypeKind::Error) {
                return None;
            }
            args.push(t);
            if !self.eat(Punct::Comma) {
                break;
            }
        }
        if !self.eat_gt() {
            self.error_expected("`>`");
            let _ = open;
            return None;
        }
        Some(args)
    }

    /// A constant expression inside `<…>`: a literal with optional unary minus.
    fn binary_const(&mut self) -> Expr {
        let start = self.span();
        let neg = self.eat(Punct::Minus);
        let TokenKind::Int { value, suffix } = self.peek().clone() else {
            self.error_expected("an integer");
            return Expr {
                kind: ExprKind::Error,
                span: start,
            };
        };
        self.bump();
        let lit = Expr {
            kind: ExprKind::Lit(Lit::Int { value, suffix }),
            span: start.to(self.prev_span()),
        };
        if neg {
            Expr {
                kind: ExprKind::Unary {
                    op: UnOp::Neg,
                    expr: Box::new(lit),
                },
                span: start.to(self.prev_span()),
            }
        } else {
            lit
        }
    }

    /// Generic parameters: `<T extends Bound = Default, comptime N: i32>`.
    pub(crate) fn generics(&mut self) -> Vec<GenericParam> {
        let mut out = Vec::new();
        if !self.at(Punct::Lt) {
            return out;
        }
        self.bump();
        while !self.at(Punct::Gt) && !self.at_eof() {
            let start = self.span();
            let comptime = self.eat_kw(Kw::Comptime);
            let Some(name) = self.ident() else { break };
            let kind = if comptime {
                self.expect(Punct::Colon);
                GenericKind::Comptime { ty: self.ty() }
            } else {
                let bound = if self.eat_kw(Kw::Extends) {
                    Some(self.ty())
                } else {
                    None
                };
                let default = if self.eat(Punct::Eq) {
                    Some(self.ty())
                } else {
                    None
                };
                GenericKind::Type { bound, default }
            };
            out.push(GenericParam {
                name,
                kind,
                span: start.to(self.prev_span()),
            });
            if !self.eat(Punct::Comma) {
                break;
            }
        }
        if !self.eat_gt() {
            self.error_expected("`>`");
        }
        out
    }

    /// `where T: Bound, U: Other`
    pub(crate) fn where_clause(&mut self) -> Vec<WherePredicate> {
        let mut out = Vec::new();
        if !self.eat_newlines_before(|k| k.is_kw(Kw::Where)) && !self.at_kw(Kw::Where) {
            return out;
        }
        self.bump();
        loop {
            let start = self.span();
            let ty = self.ty_no_union();
            self.expect(Punct::Colon);
            let bound = self.ty_no_union();
            out.push(WherePredicate {
                ty,
                bound,
                span: start.to(self.prev_span()),
            });
            if !self.eat(Punct::Comma) {
                break;
            }
        }
        out
    }
}
