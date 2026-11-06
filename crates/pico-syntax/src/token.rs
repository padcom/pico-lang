//! Tokens produced by the lexer.

use pico_diag::Span;
use std::fmt;

macro_rules! keywords {
    ($($name:ident = $text:literal,)*) => {
        /// Reserved words (spec §1.6). Contextual words such as `type`, `of`, `from` and `arena`
        /// are identifiers that the parser recognizes by position.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Kw { $($name,)* }

        impl Kw {
            /// Every keyword (the VS Code grammar's test checks it has them all).
            pub const ALL: &'static [Kw] = &[$(Kw::$name,)*];

            pub fn as_str(self) -> &'static str {
                match self { $(Kw::$name => $text,)* }
            }

            pub fn parse(s: &str) -> Option<Kw> {
                match s { $($text => Some(Kw::$name),)* _ => None }
            }
        }
    };
}

keywords! {
    Abstract = "abstract", As = "as", Async = "async", Await = "await", Break = "break",
    Case = "case", Catch = "catch", Class = "class", Comptime = "comptime", Const = "const",
    Continue = "continue", Decorator = "decorator", Default = "default", Distinct = "distinct",
    Do = "do", Else = "else", Enum = "enum", Export = "export", Extends = "extends",
    Extern = "extern", False = "false", Final = "final", Finally = "finally", For = "for",
    Function = "function", If = "if", Immutable = "immutable",
    Implements = "implements", Import = "import", In = "in", Instanceof = "instanceof",
    Interface = "interface", Is = "is", Let = "let", Loop = "loop", Match = "match", New = "new",
    Null = "null", Operator = "operator", Override = "override", Private = "private",
    Protected = "protected", Public = "public", Readonly = "readonly", Return = "return",
    Static = "static", Struct = "struct", Super = "super", Switch = "switch",
    Tailcall = "tailcall", This = "this", Throw = "throw", Throws = "throws", True = "true",
    Try = "try", Typeof = "typeof", Unsafe = "unsafe", Using = "using", Virtual = "virtual",
    Where = "where", While = "while", Yield = "yield",
}

macro_rules! puncts {
    ($($name:ident = $text:literal,)*) => {
        /// Punctuators, listed longest first so the lexer can match greedily.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Punct { $($name,)* }

        impl Punct {
            pub fn as_str(self) -> &'static str {
                match self { $(Punct::$name => $text,)* }
            }

            pub const ALL: &'static [Punct] = &[$(Punct::$name,)*];
        }
    };
}

puncts! {
    UShrEq = ">>>=",
    Ellipsis = "...", StarStarEq = "**=", ShlEq = "<<=", ShrEq = ">>=", UShr = ">>>",
    AndAndEq = "&&=", OrOrEq = "||=", QQEq = "??=",
    FatArrow = "=>", EqEq = "==", NotEq = "!=", LtEq = "<=", GtEq = ">=", AndAnd = "&&",
    OrOr = "||", QQ = "??", QDot = "?.", PlusPlus = "++", MinusMinus = "--", PlusEq = "+=",
    MinusEq = "-=", StarEq = "*=", SlashEq = "/=", PercentEq = "%=", AndEq = "&=", OrEq = "|=",
    CaretEq = "^=", StarStar = "**", Shl = "<<", Shr = ">>", DotDot = "..",
    LParen = "(", RParen = ")", LBracket = "[", RBracket = "]", LBrace = "{", RBrace = "}",
    Comma = ",", Semi = ";", Colon = ":", Dot = ".", Question = "?", Bang = "!", Tilde = "~",
    Plus = "+", Minus = "-", Star = "*", Slash = "/", Percent = "%", And = "&", Or = "|",
    Caret = "^", Lt = "<", Gt = ">", Eq = "=", At = "@",
}

/// A numeric literal suffix such as `u8` or `f32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NumSuffix {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    Isize,
    Usize,
    F32,
    F64,
}

impl NumSuffix {
    pub fn parse(s: &str) -> Option<NumSuffix> {
        Some(match s {
            "i8" => NumSuffix::I8,
            "i16" => NumSuffix::I16,
            "i32" => NumSuffix::I32,
            "i64" => NumSuffix::I64,
            "u8" => NumSuffix::U8,
            "u16" => NumSuffix::U16,
            "u32" => NumSuffix::U32,
            "u64" => NumSuffix::U64,
            "isize" => NumSuffix::Isize,
            "usize" => NumSuffix::Usize,
            "f32" => NumSuffix::F32,
            "f64" => NumSuffix::F64,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            NumSuffix::I8 => "i8",
            NumSuffix::I16 => "i16",
            NumSuffix::I32 => "i32",
            NumSuffix::I64 => "i64",
            NumSuffix::U8 => "u8",
            NumSuffix::U16 => "u16",
            NumSuffix::U32 => "u32",
            NumSuffix::U64 => "u64",
            NumSuffix::Isize => "isize",
            NumSuffix::Usize => "usize",
            NumSuffix::F32 => "f32",
            NumSuffix::F64 => "f64",
        }
    }

    pub fn is_float(self) -> bool {
        matches!(self, NumSuffix::F32 | NumSuffix::F64)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    Ident(String),
    Kw(Kw),
    Int {
        value: u64,
        suffix: Option<NumSuffix>,
    },
    Float {
        value: f64,
        suffix: Option<NumSuffix>,
    },
    /// `'…'`, `"…"`, `'''…'''` or `"""…"""`, with escapes processed.
    Str(String),
    /// A template without substitutions: `` `text` ``.
    Template(String),
    /// `` `text${ ``
    TemplateHead(String),
    /// `}text${`. `spec` is the format spec of the substitution that just ended.
    TemplateMiddle {
        spec: Option<String>,
        text: String,
    },
    /// `` }text` ``
    TemplateTail {
        spec: Option<String>,
        text: String,
    },
    Regex {
        pattern: String,
        flags: String,
    },
    Punct(Punct),
    /// A line break. After the newline filter, only statement-terminating ones remain.
    Newline,
    Eof,
}

impl TokenKind {
    /// How the token is named in "expected X, found Y" messages.
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Ident(s) => format!("identifier `{s}`"),
            TokenKind::Kw(k) => format!("keyword `{}`", k.as_str()),
            TokenKind::Int { .. } => "integer literal".into(),
            TokenKind::Float { .. } => "float literal".into(),
            TokenKind::Str(_) => "string literal".into(),
            TokenKind::Template(_)
            | TokenKind::TemplateHead(_)
            | TokenKind::TemplateMiddle { .. }
            | TokenKind::TemplateTail { .. } => "template literal".into(),
            TokenKind::Regex { .. } => "regex literal".into(),
            TokenKind::Punct(p) => format!("`{}`", p.as_str()),
            TokenKind::Newline => "newline".into(),
            TokenKind::Eof => "end of file".into(),
        }
    }

    pub fn is_punct(&self, p: Punct) -> bool {
        matches!(self, TokenKind::Punct(q) if *q == p)
    }

    pub fn is_kw(&self, k: Kw) -> bool {
        matches!(self, TokenKind::Kw(q) if *q == k)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// A line break (or a multi-line comment) separates this token from the previous one.
    /// Survives newline filtering, so the parser can explain line continuations.
    pub newline_before: bool,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.kind.describe())
    }
}
