//! The mid-level IR: functions as control-flow graphs of basic blocks over typed locals.
//!
//! Integer arithmetic in [`Rvalue::Binary`] and [`Rvalue::Unary`] traps on overflow and
//! division by zero, and [`Rvalue::Cast`] traps when a narrowing conversion doesn't fit; each
//! statement carries the source location for the panic message. Calls are terminators so
//! exceptions can later lower to `invoke`.

use ppl_sema::hir::{Builtin, FnId, GlobalId};
use ppl_sema::{FloatTy, IntTy, Ty};
use std::fmt::{self, Write};

/// Panic messages shared by every backend, so interpreted and native runs print the same text.
pub mod panics {
    pub const OVERFLOW: &str = "integer overflow";
    pub const DIV_ZERO: &str = "division by zero";
    pub const SHIFT: &str = "shift amount out of range";
    pub const NEG_EXPONENT: &str = "negative exponent";
    pub const INT_CAST: &str = "integer conversion out of range";
    pub const FLOAT_CAST: &str = "float conversion out of range";
    pub const CHAR_CAST: &str = "invalid char code";
    pub const STACK_OVERFLOW: &str = "stack overflow";
    pub const INDEX: &str = "index out of bounds";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Local(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

/// A source position, for panic messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Loc {
    pub line: u32,
    pub col: u32,
}

#[derive(Debug)]
pub struct Program {
    /// The source file name, used in panic locations.
    pub file: String,
    pub fns: Vec<Function>,
    pub entry: FnId,
    pub globals: Vec<GlobalDecl>,
}

impl Program {
    pub fn func(&self, id: FnId) -> &Function {
        &self.fns[id.0 as usize]
    }
}

#[derive(Debug)]
pub struct GlobalDecl {
    pub name: String,
    pub ty: Ty,
}

#[derive(Debug)]
pub struct LocalDecl {
    pub ty: Ty,
    /// The source name; `None` for temporaries.
    pub name: Option<String>,
}

#[derive(Debug)]
pub struct Function {
    pub name: String,
    pub params: Vec<Local>,
    pub ret: Ty,
    pub locals: Vec<LocalDecl>,
    pub blocks: Vec<BasicBlock>,
}

impl Function {
    pub fn local_ty(&self, l: Local) -> &Ty {
        &self.locals[l.0 as usize].ty
    }

    pub fn block(&self, b: BlockId) -> &BasicBlock {
        &self.blocks[b.0 as usize]
    }
}

#[derive(Debug)]
pub struct BasicBlock {
    pub stmts: Vec<Statement>,
    pub term: Terminator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Local(Local),
    Global(GlobalId),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Int(i128, IntTy),
    Float(f64, FloatTy),
    Bool(bool),
    Char(char),
    Str(String),
    Void,
}

impl Const {
    pub fn ty(&self) -> Ty {
        match self {
            Const::Int(_, t) => Ty::Int(*t),
            Const::Float(_, t) => Ty::Float(*t),
            Const::Bool(_) => Ty::Bool,
            Const::Char(_) => Ty::Char,
            Const::Str(_) => Ty::Str,
            Const::Void => Ty::Void,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Operand {
    Copy(Place),
    Const(Const),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Shl,
    Shr,
    UShr,
    BitAnd,
    BitOr,
    BitXor,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl BinOp {
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    BitNot,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Rvalue {
    Use(Operand),
    Binary(BinOp, Operand, Operand),
    Unary(UnOp, Operand),
    /// Numeric/char conversion to the given type.
    Cast(Operand, Ty),
    /// `s.length`: a string's length in code points.
    StrLen(Operand),
    /// `s[i]`: the `char` at code-point index `i` (panics when out of bounds).
    StrCharAt(Operand, Operand),
}

#[derive(Debug)]
pub struct Statement {
    pub place: Place,
    pub rvalue: Rvalue,
    pub loc: Loc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Callee {
    Fn(FnId),
    Builtin(Builtin),
}

#[derive(Debug)]
pub enum Terminator {
    Goto(BlockId),
    Branch {
        cond: Operand,
        then: BlockId,
        else_: BlockId,
    },
    Return(Option<Operand>),
    Call {
        callee: Callee,
        args: Vec<Operand>,
        dest: Option<Place>,
        next: BlockId,
        loc: Loc,
    },
    /// `panic(msg)` or a failed `assert`.
    Panic {
        msg: Operand,
        loc: Loc,
    },
    Unreachable,
}

// ---------------------------------------------------------------- dump

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Place::Local(l) => write!(f, "_{}", l.0),
            Place::Global(g) => write!(f, "@g{}", g.0),
        }
    }
}

impl fmt::Display for Const {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Const::Int(v, t) => write!(f, "{v}_{}", t.name()),
            Const::Float(v, t) => write!(f, "{v:?}_{}", t.name()),
            Const::Bool(b) => write!(f, "{b}"),
            Const::Char(c) => write!(f, "{c:?}"),
            Const::Str(s) => write!(f, "{s:?}"),
            Const::Void => write!(f, "()"),
        }
    }
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Copy(p) => write!(f, "{p}"),
            Operand::Const(c) => write!(f, "const {c}"),
        }
    }
}

impl fmt::Display for Rvalue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rvalue::Use(o) => write!(f, "{o}"),
            Rvalue::Binary(op, a, b) => write!(f, "{op:?}({a}, {b})"),
            Rvalue::Unary(op, a) => write!(f, "{op:?}({a})"),
            Rvalue::Cast(a, t) => write!(f, "{a} as {t}"),
            Rvalue::StrLen(a) => write!(f, "StrLen({a})"),
            Rvalue::StrCharAt(a, i) => write!(f, "StrCharAt({a}, {i})"),
        }
    }
}

fn callee_name(p: &Program, c: Callee) -> String {
    match c {
        Callee::Fn(id) => p.func(id).name.clone(),
        Callee::Builtin(b) => format!("builtin {}", b.name()),
    }
}

/// The textual MIR dump (`ppl build --emit mir`).
pub fn dump(p: &Program) -> String {
    let mut out = String::new();
    for (i, g) in p.globals.iter().enumerate() {
        let _ = writeln!(out, "global @g{i}: {} // {}", g.ty, g.name);
    }
    if !p.globals.is_empty() {
        out.push('\n');
    }
    for (fi, func) in p.fns.iter().enumerate() {
        if fi > 0 {
            out.push('\n');
        }
        let params: Vec<String> = func
            .params
            .iter()
            .map(|l| format!("_{}: {}", l.0, func.local_ty(*l)))
            .collect();
        let _ = writeln!(
            out,
            "fn {}({}) -> {} {{",
            func.name,
            params.join(", "),
            func.ret
        );
        for (i, l) in func.locals.iter().enumerate() {
            if func.params.contains(&Local(i as u32)) {
                continue;
            }
            match &l.name {
                Some(n) => {
                    let _ = writeln!(out, "    let _{i}: {} // {n}", l.ty);
                }
                None => {
                    let _ = writeln!(out, "    let _{i}: {}", l.ty);
                }
            }
        }
        for (bi, b) in func.blocks.iter().enumerate() {
            let _ = writeln!(out, "  bb{bi}:");
            for s in &b.stmts {
                let _ = writeln!(out, "    {} = {}", s.place, s.rvalue);
            }
            let term = match &b.term {
                Terminator::Goto(t) => format!("goto bb{}", t.0),
                Terminator::Branch { cond, then, else_ } => {
                    format!(
                        "branch {cond} -> [true: bb{}, false: bb{}]",
                        then.0, else_.0
                    )
                }
                Terminator::Return(Some(o)) => format!("return {o}"),
                Terminator::Return(None) => "return".to_string(),
                Terminator::Call {
                    callee,
                    args,
                    dest,
                    next,
                    ..
                } => {
                    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
                    let call = format!("{}({})", callee_name(p, *callee), args.join(", "));
                    match dest {
                        Some(d) => format!("{d} = call {call} -> bb{}", next.0),
                        None => format!("call {call} -> bb{}", next.0),
                    }
                }
                Terminator::Panic { msg, loc } => format!("panic {msg} @{}:{}", loc.line, loc.col),
                Terminator::Unreachable => "unreachable".to_string(),
            };
            let _ = writeln!(out, "    {term}");
        }
        out.push_str("}\n");
    }
    out
}
