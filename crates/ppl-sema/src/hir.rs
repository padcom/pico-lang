//! The high-level IR: the AST with every name resolved and loops desugared.
//!
//! Every expression has an [`ExprId`]; the type checker stores its results in tables indexed
//! by it. `for` loops become [`ExprKind::While`] (with an `update` step that `continue` runs),
//! and `for (x of a..b)` becomes a counting `while`.

pub use crate::ty::StructId;
use crate::ty::Ty;
use ppl_diag::{FileId, Span};
use ppl_syntax::ast::{BinOp, UnOp};
use ppl_syntax::token::NumSuffix;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExprId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FnId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlobalId(pub u32);

#[derive(Debug)]
pub struct Program {
    pub file: FileId,
    pub fns: Vec<Function>,
    /// The entry module's top-level code, as a function.
    pub entry: FnId,
    /// Top-level bindings of the entry module, visible to every function.
    pub globals: Vec<Binding>,
    /// Span of every expression, indexed by `ExprId`.
    pub expr_spans: Vec<Span>,
    pub structs: Vec<StructDef>,
}

#[derive(Debug)]
pub struct FieldDef {
    pub name: String,
    pub ty: Ty,
    pub span: Span,
}

#[derive(Debug)]
pub struct StructDef {
    pub name: String,
    pub name_span: Span,
    pub fields: Vec<FieldDef>,
    /// Methods by name; each is a function whose `this` is set.
    pub methods: Vec<(String, FnId)>,
}

impl StructDef {
    pub fn field(&self, name: &str) -> Option<(usize, &FieldDef)> {
        self.fields.iter().enumerate().find(|(_, f)| f.name == name)
    }

    pub fn method(&self, name: &str) -> Option<FnId> {
        self.methods
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, f)| *f)
    }
}

impl Program {
    pub fn func(&self, id: FnId) -> &Function {
        &self.fns[id.0 as usize]
    }

    pub fn global(&self, id: GlobalId) -> &Binding {
        &self.globals[id.0 as usize]
    }

    pub fn struct_def(&self, id: StructId) -> &StructDef {
        &self.structs[id.0 as usize]
    }

    pub fn struct_ty(&self, id: StructId) -> Ty {
        Ty::Struct(id, self.structs[id.0 as usize].name.as_str().into())
    }

    pub fn expr_count(&self) -> usize {
        self.expr_spans.len()
    }
}

/// A local variable, parameter or global.
#[derive(Debug, Clone)]
pub struct Binding {
    pub name: String,
    pub mutable: bool,
    /// The declared type, if any.
    pub ty: Option<Ty>,
    pub span: Span,
}

#[derive(Debug)]
pub struct Function {
    pub name: String,
    pub name_span: Span,
    pub params: Vec<LocalId>,
    pub ret: Option<Ty>,
    /// Span of the return type annotation, for messages.
    pub ret_span: Option<Span>,
    pub locals: Vec<Binding>,
    pub body: Block,
    pub span: Span,
    pub is_entry: bool,
    /// For methods: the `this` local and its struct. `this` is passed by reference.
    pub this: Option<(LocalId, StructId)>,
}

impl Function {
    pub fn local(&self, id: LocalId) -> &Binding {
        &self.locals[id.0 as usize]
    }
}

#[derive(Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    /// The last expression when it has no trailing `;`: the block's value.
    pub tail: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Debug)]
pub enum Stmt {
    Let {
        var: Var,
        init: Option<Expr>,
        span: Span,
    },
    Expr(Expr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Var {
    Local(LocalId),
    Global(GlobalId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    Print,
    Println,
    Assert,
    Panic,
}

impl Builtin {
    pub fn name(self) -> &'static str {
        match self {
            Builtin::Print => "print",
            Builtin::Println => "println",
            Builtin::Assert => "assert",
            Builtin::Panic => "panic",
        }
    }

    pub fn from_name(name: &str) -> Option<Builtin> {
        Some(match name {
            "print" => Builtin::Print,
            "println" => Builtin::Println,
            "assert" => Builtin::Assert,
            "panic" => Builtin::Panic,
            _ => return None,
        })
    }
}

#[derive(Debug)]
pub struct Expr {
    pub id: ExprId,
    pub kind: ExprKind,
    pub span: Span,
}

/// The target of an assignment or `++`/`--`.
#[derive(Debug)]
pub enum Target {
    Var(Var),
    /// `object.name = …`; `id` keys the field index in the type checker's results.
    Field {
        id: ExprId,
        object: Box<Expr>,
        name: String,
        name_span: Span,
    },
}

#[derive(Debug)]
pub struct FieldInit {
    pub name: String,
    pub name_span: Span,
    pub value: Expr,
}

#[derive(Debug)]
pub struct TemplatePart {
    pub expr: Expr,
    pub spec: Option<String>,
}

#[derive(Debug)]
pub enum ExprKind {
    Int(u64, Option<NumSuffix>),
    Float(f64, Option<NumSuffix>),
    Str(String),
    Bool(bool),
    Null,
    Var(Var),
    Fn(FnId),
    Builtin(Builtin),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `target = value` / `target op= value`.
    Assign {
        op: Option<BinOp>,
        target: Target,
        value: Box<Expr>,
    },
    /// `++x`, `p.x--`, …
    IncDec {
        target: Target,
        inc: bool,
        prefix: bool,
    },
    /// `Point { x: 1, y }` (`ty` given) or `{ x: 1, y }` (struct taken from the expected type).
    Object {
        ty: Option<StructId>,
        fields: Vec<FieldInit>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    Member {
        object: Box<Expr>,
        name: String,
        name_span: Span,
    },
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    },
    /// `quasis.len() == parts.len() + 1`
    Template {
        quasis: Vec<String>,
        parts: Vec<TemplatePart>,
    },
    If {
        cond: Box<Expr>,
        then: Box<Expr>,
        else_: Option<Box<Expr>>,
    },
    Block(Block),
    Loop(Block),
    /// `while` (`test_first`) or `do … while`. `continue` runs `update`, then the test.
    While {
        cond: Box<Expr>,
        body: Box<Expr>,
        update: Option<Box<Expr>>,
        test_first: bool,
    },
    Break(Option<Box<Expr>>),
    Continue,
    Return(Option<Box<Expr>>),
    As(Box<Expr>, Ty),
    Error,
}
