//! Lexer, parser and AST for ppl.

pub mod lexer;
pub mod token;

pub use lexer::lex;
pub use token::{Kw, NumSuffix, Punct, Token, TokenKind};
