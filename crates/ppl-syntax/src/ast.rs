//! The abstract syntax tree. It covers the whole language; the parser fills it in.

use crate::token::NumSuffix;
use ppl_diag::Span;

#[derive(Clone, Debug, PartialEq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

/// A dotted name: `Shape.Circle`, `json.name`.
pub type Path = Vec<Ident>;

#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

// ---------------------------------------------------------------- literals

#[derive(Clone, Debug, PartialEq)]
pub enum Lit {
    Int {
        value: u64,
        suffix: Option<NumSuffix>,
    },
    Float {
        value: f64,
        suffix: Option<NumSuffix>,
    },
    Str(String),
    Bool(bool),
    Null,
    Regex {
        pattern: String,
        flags: String,
    },
}

// ---------------------------------------------------------------- types

#[derive(Clone, Debug, PartialEq)]
pub struct Type {
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypeKind {
    /// `T`, `std.Map<K, V>`, `ptr<u8>`.
    Path {
        path: Path,
        args: Vec<Type>,
    },
    /// `T[]`
    Array(Box<Type>),
    /// `[T; N]`
    Fixed {
        elem: Box<Type>,
        len: Box<Expr>,
    },
    /// `[A, B]`
    Tuple(Vec<Type>),
    /// `T?`
    Nullable(Box<Type>),
    Union(Vec<Type>),
    Intersection(Vec<Type>),
    /// `(x: T) => U throws E`, or `extern "C" (…) => R`.
    Function {
        params: Vec<FnTypeParam>,
        ret: Box<Type>,
        throws: Option<Box<Type>>,
        extern_abi: Option<String>,
    },
    /// `{ x: f64, y?: f64 }`
    Object(Vec<ObjectTypeField>),
    /// `"north"`, `42`, `true`, `null`.
    Lit(Lit),
    /// `immutable T`
    Immutable(Box<Type>),
    /// `distinct T` (only on the right of `type X =`).
    Distinct(Box<Type>),
    /// A constant expression used as a type argument: `zeros<4>()`.
    Const(Box<Expr>),
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FnTypeParam {
    pub name: Option<Ident>,
    pub ty: Type,
    pub optional: bool,
    pub rest: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectTypeField {
    pub name: Ident,
    pub ty: Type,
    pub optional: bool,
    pub readonly: bool,
}

// ---------------------------------------------------------------- expressions

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Plus,
    Not,
    BitNot,
    PreInc,
    PreDec,
    PostInc,
    PostDec,
    Typeof,
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
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    BitAnd,
    BitOr,
    BitXor,
    And,
    Or,
    Nullish,
}

impl BinOp {
    pub fn as_str(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Pow => "**",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::UShr => ">>>",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
            BinOp::And => "&&",
            BinOp::Or => "||",
            BinOp::Nullish => "??",
        }
    }
}

impl UnOp {
    pub fn as_str(self) -> &'static str {
        match self {
            UnOp::Neg => "neg",
            UnOp::Plus => "pos",
            UnOp::Not => "!",
            UnOp::BitNot => "~",
            UnOp::PreInc => "pre++",
            UnOp::PreDec => "pre--",
            UnOp::PostInc => "post++",
            UnOp::PostDec => "post--",
            UnOp::Typeof => "typeof",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arg {
    pub name: Option<Ident>,
    pub spread: bool,
    pub value: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ArrayElem {
    Expr(Expr),
    Spread(Expr),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PropKey {
    Ident(Ident),
    Str(String, Span),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ObjField {
    KeyValue { key: PropKey, value: Expr },
    Shorthand(Ident),
    Spread(Expr),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TemplateExpr {
    pub expr: Expr,
    pub spec: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MatchArm {
    pub pat: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SwitchCase {
    /// `None` for `default`.
    pub pat: Option<Pattern>,
    pub guard: Option<Expr>,
    pub body: Block,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Catch {
    /// `e: IoError`, `e: IoError { path }`, or just `e`.
    pub pat: Pattern,
    pub body: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    Lit(Lit),
    Ident(String),
    This,
    Super,
    /// `` tag`a${x:.2f}b` ``: `quasis.len() == exprs.len() + 1`.
    Template {
        tag: Option<Box<Expr>>,
        quasis: Vec<String>,
        exprs: Vec<TemplateExpr>,
    },
    Array(Vec<ArrayElem>),
    /// `[value; count]`
    ArrayRepeat {
        value: Box<Expr>,
        count: Box<Expr>,
    },
    /// `{ a: 1 }` or `Point { x, y }` (with `ty`).
    Object {
        ty: Option<Box<Type>>,
        fields: Vec<ObjField>,
    },
    Paren(Box<Expr>),
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `op` is `None` for plain `=`, else the operator of a compound assignment.
    Assign {
        op: Option<BinOp>,
        target: Box<Expr>,
        value: Box<Expr>,
    },
    Ternary {
        cond: Box<Expr>,
        then: Box<Expr>,
        else_: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        type_args: Vec<Type>,
        args: Vec<Arg>,
    },
    New {
        callee: Box<Expr>,
        type_args: Vec<Type>,
        args: Vec<Arg>,
    },
    Member {
        object: Box<Expr>,
        prop: Ident,
        optional: bool,
    },
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    },
    NonNull(Box<Expr>),
    As {
        expr: Box<Expr>,
        ty: Box<Type>,
    },
    Is {
        expr: Box<Expr>,
        pat: Box<Pattern>,
    },
    Instanceof {
        expr: Box<Expr>,
        ty: Box<Type>,
    },
    /// `a..b`
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
    },
    /// Arrow functions and function expressions.
    Function(Box<FnDecl>),
    Block(Block),
    If {
        cond: Box<Expr>,
        then: Box<Expr>,
        else_: Option<Box<Expr>>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    Switch {
        scrutinee: Box<Expr>,
        cases: Vec<SwitchCase>,
    },
    Loop(Block),
    Try {
        body: Box<Expr>,
        catches: Vec<Catch>,
        finally: Option<Block>,
    },
    /// `try? e`
    TryOpt(Box<Expr>),
    /// `try! e`
    TryBang(Box<Expr>),
    Throw(Box<Expr>),
    Return(Option<Box<Expr>>),
    Break(Option<Box<Expr>>),
    Continue,
    Await(Box<Expr>),
    Yield {
        expr: Option<Box<Expr>>,
        delegate: bool,
    },
    Tailcall(Box<Expr>),
    /// `comptime e`, `comptime { … }`, `comptime if …`
    Comptime(Box<Expr>),
    Unsafe(Block),
    Arena(Block),
    Error,
}

// ---------------------------------------------------------------- patterns

#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldPat {
    pub name: Ident,
    /// `y: py` or `x: 0`; `None` for shorthand `x`.
    pub pat: Option<Pattern>,
    /// `port = 8080` (destructuring default).
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatternKind {
    Wildcard,
    /// A binding, or a constant/variant that name resolution finds in scope.
    Ident(Ident),
    /// `Shape.Circle`
    Path(Path),
    Lit(Lit),
    /// `0..10`
    Range {
        start: Box<Pattern>,
        end: Box<Pattern>,
    },
    /// `Circle(r)`
    TupleStruct {
        path: Path,
        elems: Vec<Pattern>,
    },
    /// `{ x, y: py, ...rest }`, `Point { x: 0, y }`.
    Struct {
        path: Option<Path>,
        fields: Vec<FieldPat>,
        rest: Option<Option<Ident>>,
    },
    /// `[a, b]`, `[first, ...rest]`
    Array(Vec<Pattern>),
    /// `...rest` inside an array pattern.
    Rest(Option<Ident>),
    Or(Vec<Pattern>),
    /// `s: string`, `e: IoError { path }`
    Typed {
        name: Ident,
        ty: Type,
        fields: Option<Vec<FieldPat>>,
    },
    /// `c as Rect { w, h }`
    As {
        name: Ident,
        pat: Box<Pattern>,
    },
    /// `a = 1` inside a destructuring array pattern.
    Default {
        pat: Box<Pattern>,
        default: Box<Expr>,
    },
    Error,
}

// ---------------------------------------------------------------- statements

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingKind {
    Const,
    Let,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StmtKind {
    /// `semi` records a trailing `;`, which makes a block's last expression `void`.
    Expr {
        expr: Expr,
        semi: bool,
    },
    Let {
        kind: BindingKind,
        pat: Pattern,
        ty: Option<Type>,
        init: Option<Expr>,
        comptime: bool,
    },
    Using {
        is_await: bool,
        name: Ident,
        init: Expr,
    },
    While {
        cond: Expr,
        body: Box<Expr>,
    },
    DoWhile {
        body: Block,
        cond: Expr,
    },
    For {
        init: Option<Box<Stmt>>,
        cond: Option<Expr>,
        update: Option<Expr>,
        body: Box<Expr>,
    },
    ForOf {
        is_await: bool,
        kind: BindingKind,
        pat: Pattern,
        iter: Expr,
        body: Box<Expr>,
    },
    Decl(Decl),
    Error,
}

// ---------------------------------------------------------------- declarations

#[derive(Clone, Debug, PartialEq)]
pub struct Decorator {
    pub path: Path,
    pub args: Option<Vec<Arg>>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Decl {
    pub kind: DeclKind,
    pub decorators: Vec<Decorator>,
    pub exported: bool,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeclKind {
    Function(FnDecl),
    Struct(StructDecl),
    Enum(EnumDecl),
    Class(ClassDecl),
    Interface(InterfaceDecl),
    TypeAlias {
        name: Ident,
        generics: Vec<GenericParam>,
        ty: Type,
    },
    Import(ImportDecl),
    /// `export { a, b as c }` or `export { a } from "./x"`.
    ExportList {
        names: Vec<ImportName>,
        from: Option<String>,
    },
    /// `export * from "./x"`
    ExportAll {
        from: String,
    },
    /// `extern "C" { … }`
    Extern {
        abi: String,
        items: Vec<Decl>,
    },
    /// An exported `const`/`let` (`export const X = …`).
    Binding(Box<Stmt>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FnFlags {
    pub is_async: bool,
    pub generator: bool,
    pub is_unsafe: bool,
    pub decorator: bool,
    pub arrow: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FnBody {
    Block(Block),
    /// Arrow function with an expression body.
    Expr(Box<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct FnDecl {
    pub name: Option<Ident>,
    pub flags: FnFlags,
    pub generics: Vec<GenericParam>,
    pub params: Vec<Param>,
    pub ret: Option<Type>,
    pub throws: Option<Type>,
    pub where_clause: Vec<WherePredicate>,
    /// `None` for declarations without a body (abstract, interface, extern).
    pub body: Option<FnBody>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub decorators: Vec<Decorator>,
    /// Constructor parameter properties: `public name: string`.
    pub mods: Modifiers,
    pub pat: Pattern,
    pub ty: Option<Type>,
    pub optional: bool,
    pub rest: bool,
    pub comptime: bool,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GenericKind {
    /// `T extends Bound = Default`
    Type {
        bound: Option<Type>,
        default: Option<Type>,
    },
    /// `comptime N: i32`
    Comptime { ty: Type },
}

#[derive(Clone, Debug, PartialEq)]
pub struct GenericParam {
    pub name: Ident,
    pub kind: GenericKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WherePredicate {
    pub ty: Type,
    pub bound: Type,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Protected,
    Private,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub visibility: Option<Visibility>,
    pub is_static: bool,
    pub readonly: bool,
    pub virtual_: bool,
    pub override_: bool,
    pub abstract_: bool,
    pub final_: bool,
    pub is_async: bool,
}

impl Modifiers {
    pub fn is_empty(&self) -> bool {
        *self == Modifiers::default()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub kind: MemberKind,
    pub decorators: Vec<Decorator>,
    pub mods: Modifiers,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MemberKind {
    Field {
        name: Ident,
        ty: Option<Type>,
        optional: bool,
        init: Option<Expr>,
    },
    Method(FnDecl),
    Getter(FnDecl),
    Setter(FnDecl),
    Constructor(FnDecl),
    /// `operator +(o: Vec2): Vec2 { … }`; `op` is `"+"`, `"[]"`, `"[]="`, …
    Operator {
        op: String,
        func: FnDecl,
    },
    StaticBlock(Block),
}

#[derive(Clone, Debug, PartialEq)]
pub struct StructDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub implements: Vec<Type>,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    pub name: Ident,
    /// `Circle(r: f64)`; `None` for `Empty`.
    pub fields: Option<Vec<Param>>,
    pub decorators: Vec<Decorator>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub implements: Vec<Type>,
    pub variants: Vec<Variant>,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClassDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub extends: Option<Type>,
    pub implements: Vec<Type>,
    pub members: Vec<Member>,
    pub abstract_: bool,
    pub final_: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InterfaceDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub extends: Vec<Type>,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportName {
    pub name: Ident,
    pub alias: Option<Ident>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ImportKind {
    /// `import { a, b as c } from "…"`
    Named(Vec<ImportName>),
    /// `import * as m from "…"`
    Namespace(Ident),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportDecl {
    pub kind: ImportKind,
    pub from: String,
    pub from_span: Span,
}
