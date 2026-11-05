//! Declarations (spec §4.4, §5, §6, §8, §11.2, §12.1).

use super::Parser;
use crate::ast::*;
use crate::token::{Kw, Punct, TokenKind};
use pico_diag::{Diagnostic, Span, codes};

/// Which member forms a body allows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Body {
    Class,
    Struct,
    Interface,
    Enum,
}

impl Parser {
    /// Whether the current token starts a declaration (as opposed to an expression statement).
    pub(super) fn at_decl_start(&self) -> bool {
        match self.peek() {
            TokenKind::Kw(
                Kw::Struct
                | Kw::Enum
                | Kw::Class
                | Kw::Interface
                | Kw::Import
                | Kw::Export
                | Kw::Extern,
            ) => true,
            TokenKind::Punct(Punct::At) => true,
            // `function name` is a declaration; `function (` is an expression.
            TokenKind::Kw(Kw::Function) => {
                matches!(self.peek_nth(1), TokenKind::Ident(_))
                    || (self.peek_nth(1).is_punct(Punct::Star)
                        && matches!(self.peek_nth(2), TokenKind::Ident(_)))
            }
            TokenKind::Kw(Kw::Async | Kw::Unsafe | Kw::Decorator) => {
                self.peek_nth(1).is_kw(Kw::Function)
            }
            TokenKind::Kw(Kw::Abstract | Kw::Final) => self.peek_nth(1).is_kw(Kw::Class),
            TokenKind::Ident(w) if w == "type" => matches!(self.peek_nth(1), TokenKind::Ident(_)),
            _ => false,
        }
    }

    /// `@name`, `@a.b(args)`; newlines after each decorator are skipped.
    fn decorators(&mut self) -> Vec<Decorator> {
        let mut out = Vec::new();
        while self.at(Punct::At) {
            let start = self.bump().span;
            let mut path = Vec::new();
            match self.property_name() {
                Some(i) => path.push(i),
                None => break,
            }
            while self.at(Punct::Dot) {
                self.bump();
                match self.property_name() {
                    Some(i) => path.push(i),
                    None => break,
                }
            }
            let args = if self.at(Punct::LParen) {
                Some(self.call_args())
            } else {
                None
            };
            out.push(Decorator {
                path,
                args,
                span: start.to(self.prev_span()),
            });
            self.skip_newlines();
        }
        out
    }

    pub(super) fn decl(&mut self) -> Option<Decl> {
        let start = self.span();
        let decorators = self.decorators();
        let exported = self.eat_kw(Kw::Export);
        if exported {
            match self.peek() {
                TokenKind::Punct(Punct::LBrace) => {
                    let names = self.import_names();
                    let from = if self.eat_word("from") {
                        self.module_specifier().map(|(s, _)| s)
                    } else {
                        None
                    };
                    return Some(self.finish(
                        DeclKind::ExportList { names, from },
                        decorators,
                        false,
                        start,
                    ));
                }
                TokenKind::Punct(Punct::Star) => {
                    self.bump();
                    if !self.eat_word("from") {
                        self.error_expected("`from`");
                    }
                    let (from, _) = self.module_specifier()?;
                    return Some(self.finish(
                        DeclKind::ExportAll { from },
                        decorators,
                        false,
                        start,
                    ));
                }
                TokenKind::Kw(Kw::Const | Kw::Let) => {
                    let s = self.stmt();
                    return Some(self.finish(
                        DeclKind::Binding(Box::new(s)),
                        decorators,
                        true,
                        start,
                    ));
                }
                TokenKind::Ident(w)
                    if w == "reactive" && matches!(self.peek_nth(1), TokenKind::Ident(_)) =>
                {
                    let s = self.stmt();
                    return Some(self.finish(
                        DeclKind::Binding(Box::new(s)),
                        decorators,
                        true,
                        start,
                    ));
                }
                TokenKind::Kw(Kw::Comptime) => {
                    let s = self.stmt();
                    return Some(self.finish(
                        DeclKind::Binding(Box::new(s)),
                        decorators,
                        true,
                        start,
                    ));
                }
                _ => {}
            }
        }
        // Decorated bindings: `@comptimeLimit(…) comptime const X = …`.
        if !decorators.is_empty()
            && (matches!(self.peek(), TokenKind::Kw(Kw::Const | Kw::Let))
                || (self.at_kw(Kw::Comptime)
                    && matches!(self.peek_nth(1), TokenKind::Kw(Kw::Const | Kw::Let))))
        {
            let s = self.stmt();
            return Some(self.finish(DeclKind::Binding(Box::new(s)), decorators, exported, start));
        }
        let kind = match self.peek().clone() {
            TokenKind::Kw(Kw::Function | Kw::Async | Kw::Unsafe | Kw::Decorator) => {
                DeclKind::Function(self.function_decl())
            }
            TokenKind::Kw(Kw::Struct) => self.struct_decl(),
            TokenKind::Kw(Kw::Enum) => self.enum_decl(),
            TokenKind::Kw(Kw::Class | Kw::Abstract | Kw::Final) => self.class_decl(),
            TokenKind::Kw(Kw::Interface) => self.interface_decl(),
            TokenKind::Kw(Kw::Import) => self.import_decl()?,
            TokenKind::Kw(Kw::Extern) => self.extern_decl(),
            TokenKind::Ident(w) if w == "type" => self.type_alias(),
            _ => {
                self.error_expected("a declaration");
                return None;
            }
        };
        Some(self.finish(kind, decorators, exported, start))
    }

    fn finish(
        &self,
        kind: DeclKind,
        decorators: Vec<Decorator>,
        exported: bool,
        start: Span,
    ) -> Decl {
        Decl {
            kind,
            decorators,
            exported,
            span: start.to(self.prev_span()),
        }
    }

    // ------------------------------------------------------------ functions

    /// `[async|unsafe|decorator] function [*] name<G>(params): R throws E where …` without body.
    pub(super) fn fn_signature(&mut self, mut flags: FnFlags) -> FnDecl {
        let start = self.span();
        loop {
            if self.eat_kw(Kw::Async) {
                flags.is_async = true;
            } else if self.eat_kw(Kw::Unsafe) {
                flags.is_unsafe = true;
            } else if self.eat_kw(Kw::Decorator) {
                flags.decorator = true;
            } else {
                break;
            }
        }
        self.expect_kw(Kw::Function);
        flags.generator = self.eat(Punct::Star);
        let name = if matches!(self.peek(), TokenKind::Ident(_)) {
            self.ident()
        } else {
            None
        };
        self.fn_rest(name, flags, start)
    }

    /// Everything after the name: generics, parameters, return type, throws, where.
    fn fn_rest(&mut self, name: Option<Ident>, flags: FnFlags, start: Span) -> FnDecl {
        let generics = self.generics();
        let params = self.params().unwrap_or_default();
        let ret = if self.eat(Punct::Colon) {
            Some(self.ty())
        } else {
            None
        };
        let throws = if self.eat_newlines_before(|k| k.is_kw(Kw::Throws)) || self.at_kw(Kw::Throws)
        {
            self.bump();
            Some(self.ty())
        } else {
            None
        };
        let where_clause = self.where_clause();
        FnDecl {
            name,
            flags,
            generics,
            params,
            ret,
            throws,
            where_clause,
            body: None,
            span: start.to(self.prev_span()),
        }
    }

    /// An optional block body. A body may start on the next line.
    fn fn_body(&mut self, f: &mut FnDecl) {
        if self.eat_newlines_before(|k| k.is_punct(Punct::LBrace)) || self.at(Punct::LBrace) {
            f.body = Some(FnBody::Block(self.block()));
        }
        f.span = f.span.to(self.prev_span());
    }

    fn function_decl(&mut self) -> FnDecl {
        let mut f = self.fn_signature(FnFlags::default());
        self.fn_body(&mut f);
        f
    }

    /// `(a: T, b = 1, c?: U, ...rest: V[], { x, y }: P)`. Returns `None` if the list is
    /// malformed (used by arrow-function speculation).
    pub(super) fn params(&mut self) -> Option<Vec<Param>> {
        let open = self.span();
        if !self.eat(Punct::LParen) {
            self.error_expected("`(`");
            return None;
        }
        let mut out = Vec::new();
        while !self.at(Punct::RParen) && !self.at_eof() {
            out.push(self.param()?);
            if !self.eat(Punct::Comma) {
                break;
            }
        }
        if !self.expect_close(Punct::RParen, open) {
            return None;
        }
        Some(out)
    }

    fn param(&mut self) -> Option<Param> {
        let start = self.span();
        let decorators = self.decorators();
        let mods = self.modifiers(true);
        let comptime = self.eat_kw(Kw::Comptime);
        let rest = self.eat(Punct::Ellipsis);
        let pat = match self.peek() {
            TokenKind::Ident(_) | TokenKind::Punct(Punct::LBrace | Punct::LBracket) => {
                self.binding_pattern()
            }
            // `this` as a parameter name isn't allowed; any other token fails the list.
            _ => {
                self.error_expected("a parameter name");
                return None;
            }
        };
        let optional = self.eat(Punct::Question);
        let ty = if self.eat(Punct::Colon) {
            Some(self.ty())
        } else {
            None
        };
        let default = if self.eat(Punct::Eq) {
            Some(self.expr())
        } else {
            None
        };
        Some(Param {
            decorators,
            mods,
            pat,
            ty,
            optional,
            rest,
            comptime,
            default,
            span: start.to(self.prev_span()),
        })
    }

    // ------------------------------------------------------------ modifiers and members

    /// Member/parameter modifiers. A modifier word followed by `(`, `:`, `=`, `?`, `<`, `,` or
    /// `)` is a name, not a modifier (`get(i)`, `static: bool`).
    fn modifiers(&mut self, param: bool) -> Modifiers {
        let mut m = Modifiers::default();
        while let TokenKind::Kw(k) = self.peek().clone() {
            let next = self.peek_nth(1);
            let used_as_name = matches!(
                next,
                TokenKind::Punct(
                    Punct::LParen
                        | Punct::Colon
                        | Punct::Eq
                        | Punct::Question
                        | Punct::Lt
                        | Punct::Comma
                        | Punct::RParen
                )
            );
            if used_as_name {
                break;
            }
            let ok = match k {
                Kw::Public => m.visibility.replace(Visibility::Public).is_none(),
                Kw::Private => m.visibility.replace(Visibility::Private).is_none(),
                Kw::Protected => m.visibility.replace(Visibility::Protected).is_none(),
                Kw::Readonly => !std::mem::replace(&mut m.readonly, true),
                Kw::Static if !param && !next.is_punct(Punct::LBrace) => {
                    !std::mem::replace(&mut m.is_static, true)
                }
                Kw::Virtual if !param => !std::mem::replace(&mut m.virtual_, true),
                Kw::Override if !param => !std::mem::replace(&mut m.override_, true),
                Kw::Abstract if !param => !std::mem::replace(&mut m.abstract_, true),
                Kw::Final if !param => !std::mem::replace(&mut m.final_, true),
                Kw::Async if !param => !std::mem::replace(&mut m.is_async, true),
                _ => break,
            };
            let span = self.bump().span;
            if !ok {
                self.report(
                    Diagnostic::error(
                        codes::INVALID_MODIFIER,
                        format!("duplicate modifier `{}`", k.as_str()),
                    )
                    .primary(span, "already given"),
                );
            }
        }
        // `reactive name` (contextual, after the other modifiers).
        if !param && self.at_word("reactive") && matches!(self.peek_nth(1), TokenKind::Ident(_)) {
            self.bump();
            m.reactive = true;
        }
        m
    }

    /// `{ members }` of a class, struct, interface, or the member part of an enum.
    fn members_until_close(&mut self, body: Body, open: Span, members: &mut Vec<Member>) {
        loop {
            self.skip_separators();
            if self.at(Punct::RBrace) || self.at_eof() {
                break;
            }
            let before = self.pos;
            match self.member(body) {
                Some(m) => members.push(m),
                None => {
                    self.recover_member();
                }
            }
            if self.pos == before {
                self.bump();
            }
        }
        self.expect_close(Punct::RBrace, open);
    }

    fn recover_member(&mut self) {
        let mut depth = 0;
        loop {
            match self.peek() {
                TokenKind::Eof => return,
                TokenKind::Punct(Punct::RBrace) if depth == 0 => return,
                TokenKind::Newline | TokenKind::Punct(Punct::Semi) if depth == 0 => {
                    self.bump();
                    return;
                }
                TokenKind::Punct(Punct::LBrace | Punct::LParen | Punct::LBracket) => depth += 1,
                TokenKind::Punct(Punct::RBrace | Punct::RParen | Punct::RBracket) => {
                    depth = (depth - 1).max(0)
                }
                _ => {}
            }
            self.bump();
        }
    }

    fn member(&mut self, body: Body) -> Option<Member> {
        let start = self.span();
        let decorators = self.decorators();
        let mods = self.modifiers(false);
        let kind = self.member_kind(body, mods)?;
        Some(Member {
            kind,
            decorators,
            mods,
            span: start.to(self.prev_span()),
        })
    }

    fn member_kind(&mut self, body: Body, mods: Modifiers) -> Option<MemberKind> {
        let start = self.span();
        let fflags = FnFlags {
            is_async: mods.is_async,
            ..Default::default()
        };
        // static { … }
        if self.at_kw(Kw::Static) && self.peek_nth(1).is_punct(Punct::LBrace) {
            self.bump();
            return Some(MemberKind::StaticBlock(self.block()));
        }
        // constructor(…)
        if self.at_word("constructor") && self.peek_nth(1).is_punct(Punct::LParen) {
            self.bump();
            let mut f = self.fn_rest(None, fflags, start);
            self.fn_body(&mut f);
            return Some(MemberKind::Constructor(f));
        }
        // get x() / set x(v)
        if matches!(self.peek(), TokenKind::Kw(Kw::Get | Kw::Set))
            && matches!(self.peek_nth(1), TokenKind::Ident(_) | TokenKind::Kw(_))
        {
            let getter = self.bump().kind.is_kw(Kw::Get);
            let name = self.property_name();
            let mut f = self.fn_rest(name, fflags, start);
            self.fn_body(&mut f);
            return Some(if getter {
                MemberKind::Getter(f)
            } else {
                MemberKind::Setter(f)
            });
        }
        // operator +(…)
        if self.at_kw(Kw::Operator) {
            self.bump();
            let op = self.operator_name()?;
            let mut f = self.fn_rest(None, fflags, start);
            self.fn_body(&mut f);
            return Some(MemberKind::Operator { op, func: f });
        }
        // *gen()
        let generator = self.eat(Punct::Star);
        let name = self.property_name()?;
        if self.at(Punct::LParen) || self.at(Punct::Lt) {
            let mut f = self.fn_rest(
                Some(name),
                FnFlags {
                    generator,
                    ..fflags
                },
                start,
            );
            self.fn_body(&mut f);
            return Some(MemberKind::Method(f));
        }
        if generator {
            self.error_expected("`(` after a generator method name");
            return None;
        }
        if body == Body::Enum {
            self.error_expected("`(` (enum members after the variants are methods)");
            return None;
        }
        // Field: name[?][: T][= init]
        let optional = self.eat(Punct::Question);
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
        if ty.is_none() && init.is_none() && !optional {
            self.error_expected("`:`, `=` or `(` after the member name");
            return None;
        }
        Some(MemberKind::Field {
            name,
            ty,
            optional,
            init,
        })
    }

    /// The operator after `operator`: `+`, `-`, `[]`, `[]=`, `==`, …
    fn operator_name(&mut self) -> Option<String> {
        if self.at(Punct::LBracket) && self.peek_nth(1).is_punct(Punct::RBracket) {
            self.bump();
            self.bump();
            if self.at(Punct::Eq) && !self.peek_nth(1).is_punct(Punct::Eq) {
                self.bump();
                return Some("[]=".into());
            }
            return Some("[]".into());
        }
        match self.peek().clone() {
            TokenKind::Punct(
                p @ (Punct::Plus
                | Punct::Minus
                | Punct::Star
                | Punct::Slash
                | Punct::Percent
                | Punct::StarStar
                | Punct::EqEq
                | Punct::NotEq
                | Punct::Lt
                | Punct::LtEq
                | Punct::Gt
                | Punct::GtEq
                | Punct::And
                | Punct::Or
                | Punct::Caret
                | Punct::Shl
                | Punct::Shr
                | Punct::UShr
                | Punct::Bang
                | Punct::Tilde),
            ) => {
                self.bump();
                Some(p.as_str().into())
            }
            _ => {
                self.error_expected("an overloadable operator");
                None
            }
        }
    }

    fn heritage(&mut self, kw: Kw) -> Vec<Type> {
        let mut out = Vec::new();
        if self.eat_newlines_before(|k| k.is_kw(kw)) || self.at_kw(kw) {
            self.bump();
            loop {
                out.push(self.ty_no_union());
                if !self.eat(Punct::Comma) {
                    break;
                }
            }
        }
        out
    }

    fn open_body(&mut self) -> Option<Span> {
        self.skip_newlines();
        let open = self.span();
        self.expect(Punct::LBrace).then_some(open)
    }

    fn struct_decl(&mut self) -> DeclKind {
        self.bump(); // struct
        let name = self.ident().unwrap_or_else(|| self.error_ident());
        let generics = self.generics();
        let implements = self.heritage(Kw::Implements);
        let mut members = Vec::new();
        if let Some(open) = self.open_body() {
            self.members_until_close(Body::Struct, open, &mut members);
        }
        DeclKind::Struct(StructDecl {
            name,
            generics,
            implements,
            members,
        })
    }

    fn class_decl(&mut self) -> DeclKind {
        let mut abstract_ = false;
        let mut final_ = false;
        loop {
            if self.eat_kw(Kw::Abstract) {
                abstract_ = true;
            } else if self.eat_kw(Kw::Final) {
                final_ = true;
            } else {
                break;
            }
        }
        self.expect_kw(Kw::Class);
        let name = self.ident().unwrap_or_else(|| self.error_ident());
        let generics = self.generics();
        let extends = self.heritage(Kw::Extends);
        if extends.len() > 1 {
            self.error_at(
                codes::UNEXPECTED_TOKEN,
                extends[1].span,
                "a class can extend only one class",
                "second base class",
            );
        }
        let implements = self.heritage(Kw::Implements);
        let mut members = Vec::new();
        if let Some(open) = self.open_body() {
            self.members_until_close(Body::Class, open, &mut members);
        }
        DeclKind::Class(ClassDecl {
            name,
            generics,
            extends: extends.into_iter().next(),
            implements,
            members,
            abstract_,
            final_,
        })
    }

    fn interface_decl(&mut self) -> DeclKind {
        self.bump(); // interface
        let name = self.ident().unwrap_or_else(|| self.error_ident());
        let generics = self.generics();
        let extends = self.heritage(Kw::Extends);
        let mut members = Vec::new();
        if let Some(open) = self.open_body() {
            self.members_until_close(Body::Interface, open, &mut members);
        }
        DeclKind::Interface(InterfaceDecl {
            name,
            generics,
            extends,
            members,
        })
    }

    /// Variants come first (`A`, `B(x: T)`, separated by commas); methods may follow.
    fn enum_decl(&mut self) -> DeclKind {
        self.bump(); // enum
        let name = self.ident().unwrap_or_else(|| self.error_ident());
        let generics = self.generics();
        let implements = self.heritage(Kw::Implements);
        let mut variants = Vec::new();
        let mut members = Vec::new();
        let Some(open) = self.open_body() else {
            return DeclKind::Enum(EnumDecl {
                name,
                generics,
                implements,
                variants,
                members,
            });
        };
        loop {
            self.skip_newlines();
            if self.at(Punct::RBrace) || self.at_eof() {
                break;
            }
            // A variant is a name optionally followed by `(fields)`, then `,`, newline or `}`;
            // anything else (a `{` body, a `:` return type, a modifier) starts the methods.
            let is_variant = if self.looks_like_variant() {
                let start = self.span();
                let decorators = self.decorators();
                let name = self.ident().unwrap_or_else(|| self.error_ident());
                let fields = if self.at(Punct::LParen) {
                    match self.params() {
                        Some(f) => Some(f),
                        None => {
                            self.recover_in_braces();
                            Some(Vec::new())
                        }
                    }
                } else {
                    None
                };
                Some(Variant {
                    name,
                    fields,
                    decorators,
                    span: start.to(self.prev_span()),
                })
            } else {
                None
            };
            match is_variant {
                Some(v) => {
                    variants.push(v);
                    self.skip_newlines();
                    if !self.eat(Punct::Comma) && !self.eat(Punct::Semi) {
                        // Methods may follow the last variant without a separator.
                    }
                }
                None => {
                    self.members_until_close(Body::Enum, open, &mut members);
                    return DeclKind::Enum(EnumDecl {
                        name,
                        generics,
                        implements,
                        variants,
                        members,
                    });
                }
            }
        }
        self.expect_close(Punct::RBrace, open);
        DeclKind::Enum(EnumDecl {
            name,
            generics,
            implements,
            variants,
            members,
        })
    }

    /// Scans ahead (without consuming) to tell a variant from a method.
    fn looks_like_variant(&self) -> bool {
        let mut i = 0;
        while self.peek_nth(i).is_punct(Punct::At) {
            // Skip a decorator: `@` path [`(` … `)`].
            i += 1;
            while matches!(self.peek_nth(i), TokenKind::Ident(_) | TokenKind::Kw(_))
                || self.peek_nth(i).is_punct(Punct::Dot)
            {
                i += 1;
            }
            if self.peek_nth(i).is_punct(Punct::LParen) {
                i = self.skip_parens(i);
            }
            while matches!(self.peek_nth(i), TokenKind::Newline) {
                i += 1;
            }
        }
        if !matches!(self.peek_nth(i), TokenKind::Ident(_)) {
            return false;
        }
        i += 1;
        if self.peek_nth(i).is_punct(Punct::LParen) {
            i = self.skip_parens(i);
        }
        matches!(
            self.peek_nth(i),
            TokenKind::Newline
                | TokenKind::Eof
                | TokenKind::Punct(Punct::Comma | Punct::RBrace | Punct::Semi)
        )
    }

    /// Given the offset of a `(`, returns the offset just past its matching `)`.
    fn skip_parens(&self, mut i: usize) -> usize {
        let mut depth = 0;
        loop {
            match self.peek_nth(i) {
                TokenKind::Eof => return i,
                TokenKind::Punct(Punct::LParen) => depth += 1,
                TokenKind::Punct(Punct::RParen) => {
                    depth -= 1;
                    if depth == 0 {
                        return i + 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// `type Name<G> = T`
    fn type_alias(&mut self) -> DeclKind {
        self.bump(); // type
        let name = self.ident().unwrap_or_else(|| self.error_ident());
        let generics = self.generics();
        self.expect(Punct::Eq);
        self.skip_newlines();
        let ty = self.ty();
        DeclKind::TypeAlias { name, generics, ty }
    }

    // ------------------------------------------------------------ modules

    fn module_specifier(&mut self) -> Option<(String, Span)> {
        match self.peek().clone() {
            TokenKind::Str(s) => {
                let span = self.bump().span;
                Some((s, span))
            }
            _ => {
                self.error_expected("a module path string");
                None
            }
        }
    }

    /// `{ a, b as c }`
    fn import_names(&mut self) -> Vec<ImportName> {
        let open = self.bump().span; // {
        let mut names = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(Punct::RBrace) || self.at_eof() {
                break;
            }
            let Some(name) = self.property_name() else {
                break;
            };
            let alias = if self.eat_kw(Kw::As) {
                self.ident()
            } else {
                None
            };
            names.push(ImportName { name, alias });
            self.skip_newlines();
            if !self.eat(Punct::Comma) {
                break;
            }
        }
        self.skip_newlines();
        self.expect_close(Punct::RBrace, open);
        names
    }

    fn import_decl(&mut self) -> Option<DeclKind> {
        self.bump(); // import
        let kind = if self.eat(Punct::Star) {
            self.expect_kw(Kw::As);
            ImportKind::Namespace(self.ident()?)
        } else if self.at(Punct::LBrace) {
            ImportKind::Named(self.import_names())
        } else {
            self.error_expected("`{` or `*`");
            return None;
        };
        if !self.eat_word("from") {
            self.error_expected("`from`");
            return None;
        }
        let (from, from_span) = self.module_specifier()?;
        Some(DeclKind::Import(ImportDecl {
            kind,
            from,
            from_span,
        }))
    }

    /// `extern "C" { function f(x: i32): i32 … }`
    fn extern_decl(&mut self) -> DeclKind {
        self.bump(); // extern
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
        let mut items = Vec::new();
        if let Some(open) = self.open_body() {
            loop {
                self.skip_separators();
                if self.at(Punct::RBrace) || self.at_eof() {
                    break;
                }
                let before = self.pos;
                match self.decl() {
                    Some(d) => items.push(d),
                    None => self.recover_member(),
                }
                if self.pos == before {
                    self.bump();
                }
            }
            self.expect_close(Punct::RBrace, open);
        }
        DeclKind::Extern { abi, items }
    }
}
