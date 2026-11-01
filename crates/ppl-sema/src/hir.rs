//! The high-level IR: the AST with every name resolved and loops desugared.
//!
//! Every expression has an [`ExprId`]; the type checker stores its results in tables indexed
//! by it. `for` loops become [`ExprKind::While`] (with an `update` step that `continue` runs),
//! and `for-of` stays [`ExprKind::ForOf`] (its lowering depends on the iterated type).

use crate::ty::Ty;
pub use crate::ty::{EnumId, StructId};
use ppl_diag::{FileId, Span};
use ppl_syntax::ast::{BinOp, UnOp};
use ppl_syntax::token::NumSuffix;

use std::collections::HashMap;

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
    pub enums: Vec<EnumDef>,
    /// Functions that initialize classes' static fields; the entry function calls them first.
    pub static_inits: Vec<FnId>,
}

#[derive(Debug)]
pub struct VariantDef {
    pub name: String,
    pub span: Span,
    /// Payload fields; empty for a unit variant like `Empty`.
    pub fields: Vec<FieldDef>,
    /// `Circle(r: f64)` (even with no fields, `Unit()`) as opposed to `Empty`.
    pub has_parens: bool,
}

#[derive(Debug)]
pub struct EnumDef {
    pub name: String,
    pub name_span: Span,
    pub variants: Vec<VariantDef>,
    /// What the enum stands for, if it wasn't declared with `enum`.
    pub origin: EnumOrigin,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EnumOrigin {
    Declared,
    /// A union of these types: variant `i` holds member `i` in its one field.
    Union(Vec<Ty>),
    /// A discriminated object union: each variant is an object type whose field 0 is the
    /// discriminant (named here), set to the variant's name.
    Discriminated(String),
}

impl EnumDef {
    pub fn variant(&self, name: &str) -> Option<(u32, &VariantDef)> {
        self.variants
            .iter()
            .enumerate()
            .find(|(_, v)| v.name == name)
            .map(|(i, v)| (i as u32, v))
    }
}

impl VariantDef {
    pub fn field(&self, name: &str) -> Option<(usize, &FieldDef)> {
        self.fields.iter().enumerate().find(|(_, f)| f.name == name)
    }
}

#[derive(Debug)]
pub struct FieldDef {
    pub name: String,
    pub ty: Ty,
    pub span: Span,
    /// Assigned before the constructor body runs (an initializer or a parameter property),
    /// or nullable (implicitly `null`).
    pub preassigned: bool,
    /// `readonly`: assigned only by constructors.
    pub readonly: bool,
}

/// A struct or a class (`is_class`). Classes share the id space so most member code is shared.
#[derive(Debug)]
pub struct StructDef {
    pub name: String,
    pub name_span: Span,
    pub fields: Vec<FieldDef>,
    /// Methods by name; each is a function whose `this` is set.
    pub methods: Vec<(String, FnId)>,
    pub is_class: bool,
    /// An interface: `methods` are its methods (inherited ones first), each a default
    /// implementation or (in `abstract_fns`) a stub.
    pub is_interface: bool,
    /// Interfaces this type implements directly (`implements`), or an interface extends.
    pub implements: Vec<StructId>,
    /// Type parameters (`struct Pair<A, B>`); `Ty::Param(Ty::type_owner(id), i, _)` inside.
    pub generics: Vec<TypeParam>,
    /// The constructors (classes always have at least one, possibly synthesized), with
    /// their parameter types; overloads have different numbers of parameters.
    pub ctors: Vec<(FnId, Vec<Ty>)>,
    /// The class this one `extends`.
    pub base: Option<StructId>,
    pub is_abstract: bool,
    pub is_final: bool,
    /// Virtual method slots: the implementation in each slot, and slot numbers by name.
    pub vtable: Vec<FnId>,
    pub virtual_slots: Vec<(String, u32)>,
    /// Abstract methods (stubs that panic) and methods that can't be overridden further.
    pub abstract_fns: std::collections::HashSet<FnId>,
    pub final_methods: std::collections::HashSet<String>,
    /// `protected` members (accessible in subclasses).
    pub protected: std::collections::HashSet<String>,
    /// Where each inherited member was declared (own members aren't listed).
    pub declared_in: HashMap<String, StructId>,
    /// `@derive(Eq, Hash, Ord)` on a class: the derived protocols.
    pub derives: Vec<String>,
    /// Methods marked `readonly` (callable through `immutable`; they can't mutate `this`).
    pub readonly_methods: std::collections::HashSet<String>,
    pub getters: Vec<(String, FnId)>,
    pub setters: Vec<(String, FnId)>,
    pub static_fields: Vec<(String, GlobalId)>,
    pub static_methods: Vec<(String, FnId)>,
    /// Class members that aren't `public`/`protected` (classes are private by default).
    pub private: std::collections::HashSet<String>,
}

impl StructDef {
    pub fn new(name: String, name_span: Span, is_class: bool) -> StructDef {
        StructDef {
            name,
            name_span,
            fields: Vec::new(),
            methods: Vec::new(),
            is_class,
            is_interface: false,
            implements: Vec::new(),
            generics: Vec::new(),
            ctors: Vec::new(),
            base: None,
            is_abstract: false,
            is_final: false,
            vtable: Vec::new(),
            virtual_slots: Vec::new(),
            abstract_fns: Default::default(),
            final_methods: Default::default(),
            protected: Default::default(),
            declared_in: HashMap::new(),
            derives: Vec::new(),
            readonly_methods: Default::default(),
            getters: Vec::new(),
            setters: Vec::new(),
            static_fields: Vec::new(),
            static_methods: Vec::new(),
            private: Default::default(),
        }
    }

    pub fn field(&self, name: &str) -> Option<(usize, &FieldDef)> {
        self.fields.iter().enumerate().find(|(_, f)| f.name == name)
    }

    pub fn method(&self, name: &str) -> Option<FnId> {
        find(&self.methods, name)
    }

    /// Every method with this name (operators can be overloaded by operand type).
    pub fn methods_named(&self, name: &str) -> Vec<FnId> {
        self.methods
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, f)| *f)
            .collect()
    }

    pub fn getter(&self, name: &str) -> Option<FnId> {
        find(&self.getters, name)
    }

    pub fn setter(&self, name: &str) -> Option<FnId> {
        find(&self.setters, name)
    }

    pub fn static_field(&self, name: &str) -> Option<GlobalId> {
        self.static_fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, g)| *g)
    }

    pub fn static_method(&self, name: &str) -> Option<FnId> {
        find(&self.static_methods, name)
    }

    /// Whether code outside the class may use member `name`.
    pub fn is_public(&self, name: &str) -> bool {
        !self.private.contains(name) && !self.protected.contains(name)
    }

    /// The type as seen inside its own declaration: its parameters as the arguments.
    pub fn self_ty(&self, id: StructId) -> Ty {
        let owner = Ty::type_owner(id);
        let args: std::sync::Arc<[Ty]> = self
            .generics
            .iter()
            .enumerate()
            .map(|(i, p)| Ty::Param(owner, i as u32, p.name.as_str().into()))
            .collect();
        self.ty_with(id, args)
    }

    /// The type with these type arguments.
    pub fn ty_with(&self, id: StructId, args: std::sync::Arc<[Ty]>) -> Ty {
        let name: std::sync::Arc<str> = self.name.as_str().into();
        if self.is_class {
            Ty::Class(id, name, args)
        } else if self.is_interface {
            Ty::Iface(id, name, args)
        } else {
            Ty::Struct(id, name, args)
        }
    }

    /// A field's type for a value of type `of` (substituting the type arguments).
    pub fn field_ty(&self, id: StructId, i: usize, of: &Ty) -> Ty {
        self.fields[i].ty.subst(Ty::type_owner(id), of.type_args())
    }

    pub fn virtual_slot(&self, name: &str) -> Option<u32> {
        self.virtual_slots
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| *s)
    }

    /// The constructor taking `n` arguments.
    pub fn ctor_with(&self, n: usize) -> Option<FnId> {
        self.ctors
            .iter()
            .find(|(_, ps)| ps.len() == n)
            .map(|(f, _)| *f)
    }
}

fn find(list: &[(String, FnId)], name: &str) -> Option<FnId> {
    list.iter().find(|(n, _)| n == name).map(|(_, f)| *f)
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

    pub fn enum_def(&self, id: EnumId) -> &EnumDef {
        &self.enums[id.0 as usize]
    }

    pub fn enum_ty(&self, id: EnumId) -> Ty {
        Ty::Enum(id, self.enums[id.0 as usize].name.as_str().into())
    }

    /// Whether type `sid` (a struct, class or interface) conforms to interface `iface`:
    /// directly, through a base class, or through interface inheritance.
    pub fn implements(&self, sid: StructId, iface: StructId) -> bool {
        if sid == iface {
            return true;
        }
        let d = &self.structs[sid.0 as usize];
        d.implements.iter().any(|i| self.implements(*i, iface))
            || d.base.is_some_and(|b| self.implements(b, iface))
    }

    /// Whether class `sub` is `sup` or inherits from it.
    pub fn is_subclass(&self, mut sub: StructId, sup: StructId) -> bool {
        loop {
            if sub == sup {
                return true;
            }
            match self.structs[sub.0 as usize].base {
                Some(b) => sub = b,
                None => return false,
            }
        }
    }

    pub fn struct_ty(&self, id: StructId) -> Ty {
        self.structs[id.0 as usize].self_ty(id)
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
    /// Lives in a heap cell (the local holds a reference to it): a `let` that a closure
    /// captures, or a closure's reference to such a variable.
    pub boxed: bool,
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
    /// For methods: the `this` local and its struct or class. A struct's `this` is passed by
    /// reference, a class's is the object reference.
    pub this: Option<(LocalId, StructId)>,
    /// The struct or class whose code this is (methods, constructors, static members): it
    /// may use the type's private members.
    pub owner: Option<StructId>,
    /// Type parameters (`function max<T extends …>`); `Ty::Param(this fn, i, _)` in the body.
    pub generics: Vec<TypeParam>,
    /// A closure (arrow or function expression): what it captures, and the function it's in.
    pub closure: Option<ClosureInfo>,
    /// Each parameter's default (`p = 1`, and `null` for `p?: T`), evaluated in the callee
    /// (it may use earlier parameters) when the caller leaves the parameter out.
    pub defaults: Vec<Option<Expr>>,
    /// The last parameter is a rest parameter (`...xs: T[]`).
    pub rest: bool,
}

#[derive(Debug, Clone)]
pub struct ClosureInfo {
    pub parent: FnId,
    pub captures: Vec<Capture>,
}

/// A variable of the enclosing function that a closure uses: copied (`const`) or shared
/// through its box (`let`).
#[derive(Debug, Clone)]
pub struct Capture {
    pub outer: LocalId,
    pub inner: LocalId,
    pub by_ref: bool,
}

#[derive(Debug, Clone)]
pub struct TypeParam {
    pub name: String,
    pub span: Span,
    pub bounds: Vec<Bound>,
    /// `T = i64`: used when a type is written without its arguments.
    pub default: Option<Ty>,
}

/// What a type argument must provide.
#[derive(Debug, Clone, PartialEq)]
pub enum Bound {
    /// Implements this interface.
    Iface(StructId),
    /// The built-in `Comparable<T>`: `compareTo(o: T): i32` (numbers, chars, strings).
    Comparable,
    /// The built-in `Equatable<T>`: `==` / `equals(o: T): bool`.
    Equatable,
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
    /// `object[index] = …` (arrays, fixed arrays, tuples with a constant index).
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    },
}

#[derive(Debug)]
pub struct FieldInit {
    pub name: String,
    pub name_span: Span,
    pub value: Expr,
}

/// An argument of a variant constructor: `Circle(r: 1)` or `Rect(2, 3)`.
#[derive(Debug)]
pub struct VariantArg {
    pub name: Option<(String, Span)>,
    pub value: Expr,
}

#[derive(Debug)]
pub struct Pattern {
    /// Keys the type checker's results (shares the expression id space).
    pub id: ExprId,
    pub kind: PatKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum LitPat {
    /// The value, already negated for `-1`.
    Int(i128),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
}

#[derive(Debug)]
pub enum VariantPatArgs {
    /// `Empty` (also matches a payload variant, ignoring its fields).
    None,
    Positional(Vec<Pattern>),
    /// `Circle { r }`, `Circle { r: 0 }`: unnamed fields are wildcards.
    Named(Vec<FieldPat>),
}

#[derive(Debug)]
pub struct FieldPat {
    pub name: String,
    pub name_span: Span,
    pub pat: Pattern,
}

#[derive(Debug)]
pub enum PatKind {
    Wild,
    Bind(LocalId),
    Lit(LitPat),
    /// An enum variant. `enum_` is `None` when unqualified (`Circle(r)`): the scrutinee's type
    /// says which enum.
    Variant {
        enum_: Option<EnumId>,
        name: String,
        name_span: Span,
        args: VariantPatArgs,
    },
    /// `Point { x: 0, y }`: unnamed fields are wildcards.
    Struct {
        sid: StructId,
        fields: Vec<FieldPat>,
    },
    /// `a | b`: every alternative binds the same names (to the same locals).
    Or(Vec<Pattern>),
    /// `300..400` (exclusive), over integers or chars.
    Range(LitPat, LitPat),
    /// `[a, b]`, `[first, ...rest]`, `[...init, last]`: tuples, fixed and growable arrays.
    /// `rest` is `None` without `...`, `Some(None)` for an unnamed `...`.
    Slice {
        prefix: Vec<Pattern>,
        rest: Option<Option<LocalId>>,
        suffix: Vec<Pattern>,
    },
    /// `c as Rect { w, h }`: binds the whole value too.
    As(LocalId, Box<Pattern>),
    /// `s: string`, `p: Point { x }`, or a bare type name (`x is string`).
    Type {
        ty: Ty,
        bind: Option<LocalId>,
        fields: Option<Vec<FieldPat>>,
    },
}

impl Pattern {
    /// Whether matching a `T?` value tests that it isn't null and looks at the `T`: every
    /// pattern except `_`, bindings, `null`, or-patterns (each alternative decides) and type
    /// tests for the `T?` itself.
    pub fn looks_inside(&self, scrutinee: &Ty) -> bool {
        let Ty::Nullable(inner) = scrutinee else {
            return false;
        };
        match &self.kind {
            PatKind::Wild | PatKind::Bind(_) | PatKind::Lit(LitPat::Null) | PatKind::Or(_) => false,
            PatKind::As(_, p) => p.looks_inside(scrutinee),
            // A test for a non-nullable type never matches null.
            PatKind::Type { ty, .. } => {
                let _ = inner;
                !matches!(ty, Ty::Nullable(_))
            }
            _ => true,
        }
    }
}

#[derive(Debug)]
pub struct Arm {
    pub pat: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
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
    /// `new C(args)`
    New {
        class: StructId,
        args: Vec<Expr>,
        /// `new Stack<i64>()`; empty to infer them.
        type_args: Vec<Ty>,
    },
    /// `super(args)` / `this(args)` at the start of a constructor: runs a constructor of
    /// `class` (the base, or this class) on `this`.
    CtorCall {
        class: StructId,
        args: Vec<Expr>,
    },
    /// `super.method(args)`: a static call of the base class's implementation on `this`.
    SuperCall {
        method: FnId,
        args: Vec<Expr>,
    },
    /// `expr instanceof C`
    InstanceOf {
        expr: Box<Expr>,
        class: StructId,
    },
    /// `Point { x: 1, y }` (`ty` given) or `{ x: 1, y }` (struct taken from the expected type).
    Object {
        ty: Option<StructId>,
        fields: Vec<FieldInit>,
        /// `Pair<i64, string> { … }`; empty to infer them.
        type_args: Vec<Ty>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        /// Explicit type arguments: `max<i32>(a, b)`.
        type_args: Vec<Ty>,
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
    /// `[a, b, c]`: a growable array, fixed array or tuple, depending on the expected type.
    Array(Vec<Expr>),
    /// `[value; count]`: a fixed array.
    Repeat {
        value: Box<Expr>,
        count: u64,
    },
    /// `start..end`
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
    },
    /// `for (var of iter) body`; destructuring patterns become `let`s at the top of `body`.
    ForOf {
        var: LocalId,
        iter: Box<Expr>,
        body: Box<Expr>,
    },
    /// `Shape.Circle(r: 1)`, `Color.Red` (`args` is `None` without parentheses).
    Variant {
        enum_: EnumId,
        variant: u32,
        args: Option<Vec<VariantArg>>,
    },
    /// `match`, or a `switch`: `switch` is `Some(used)`, and a switch must be exhaustive only
    /// when its value is used.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<Arm>,
        switch: Option<bool>,
    },
    /// `expr is pattern`
    Is {
        expr: Box<Expr>,
        pat: Pattern,
    },
    /// `typeof e`: the name of `e`'s runtime type.
    TypeOf(Box<Expr>),
    /// `e!`: panics if `e` is null.
    NonNull(Box<Expr>),
    /// `base?.rest`: `tmp` holds the non-null `base`, and `rest` (the chain after `?.`) uses it.
    /// The result is null when `base` is.
    OptChain {
        tmp: LocalId,
        base: Box<Expr>,
        rest: Box<Expr>,
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
        /// `for (let i …)` variables that closures capture: each iteration gets a fresh box
        /// (before the update), as in JavaScript.
        rebox: Vec<LocalId>,
    },
    /// A closure value (the function is lifted; see `Function::closure`).
    Closure(FnId),
    /// `name: value` as a call argument.
    NamedArg {
        name: String,
        name_span: Span,
        value: Box<Expr>,
    },
    /// `...value` as a call argument (an array into a rest parameter, or a tuple's elements).
    Spread(Box<Expr>),
    /// A name that refers to several overloaded functions (resolved at the call).
    Overloaded(Vec<FnId>),
    /// `tailcall f(args)`: a call that must reuse the caller's stack frame.
    Tailcall(Box<Expr>),
    Break(Option<Box<Expr>>),
    Continue,
    Return(Option<Box<Expr>>),
    As(Box<Expr>, Ty),
    Error,
}

impl Expr {
    /// Calls `f` on every expression directly inside this one (not recursively), including
    /// those in statements, blocks and assignment targets.
    pub fn for_each_child(&self, f: &mut dyn FnMut(&Expr)) {
        fn block(b: &Block, f: &mut dyn FnMut(&Expr)) {
            for s in &b.stmts {
                match s {
                    Stmt::Let { init, .. } => {
                        if let Some(i) = init {
                            f(i);
                        }
                    }
                    Stmt::Expr(e) => f(e),
                }
            }
            if let Some(t) = &b.tail {
                f(t);
            }
        }
        fn target(t: &Target, f: &mut dyn FnMut(&Expr)) {
            match t {
                Target::Var(_) => {}
                Target::Field { object, .. } => f(object),
                Target::Index { object, index } => {
                    f(object);
                    f(index);
                }
            }
        }
        match &self.kind {
            ExprKind::Int(..)
            | ExprKind::Float(..)
            | ExprKind::Str(_)
            | ExprKind::Bool(_)
            | ExprKind::Null
            | ExprKind::Var(_)
            | ExprKind::Fn(_)
            | ExprKind::Builtin(_)
            | ExprKind::Continue
            | ExprKind::Closure(_)
            | ExprKind::Overloaded(_)
            | ExprKind::Error => {}
            ExprKind::NamedArg { value, .. } => f(value),
            ExprKind::Spread(e) | ExprKind::Tailcall(e) => f(e),
            ExprKind::Unary(_, e)
            | ExprKind::As(e, _)
            | ExprKind::NonNull(e)
            | ExprKind::TypeOf(e) => f(e),
            ExprKind::Binary(_, a, b) | ExprKind::Range { start: a, end: b } => {
                f(a);
                f(b);
            }
            ExprKind::Assign {
                target: t, value, ..
            } => {
                target(t, f);
                f(value);
            }
            ExprKind::IncDec { target: t, .. } => target(t, f),
            ExprKind::Object { fields, .. } => fields.iter().for_each(|x| f(&x.value)),
            ExprKind::New { args, .. }
            | ExprKind::CtorCall { args, .. }
            | ExprKind::SuperCall { args, .. } => args.iter().for_each(&mut *f),
            ExprKind::InstanceOf { expr, .. } => f(expr),
            ExprKind::Call { callee, args, .. } => {
                f(callee);
                args.iter().for_each(&mut *f);
            }
            ExprKind::Member { object, .. } => f(object),
            ExprKind::Index { object, index } => {
                f(object);
                f(index);
            }
            ExprKind::Array(elems) => elems.iter().for_each(f),
            ExprKind::Repeat { value, .. } => f(value),
            ExprKind::ForOf { iter, body, .. } => {
                f(iter);
                f(body);
            }
            ExprKind::Variant { args, .. } => {
                for a in args.iter().flatten() {
                    f(&a.value);
                }
            }
            ExprKind::Is { expr, .. } => f(expr),
            ExprKind::Match {
                scrutinee, arms, ..
            } => {
                f(scrutinee);
                for a in arms {
                    if let Some(g) = &a.guard {
                        f(g);
                    }
                    f(&a.body);
                }
            }
            ExprKind::OptChain { base, rest, .. } => {
                f(base);
                f(rest);
            }
            ExprKind::Template { parts, .. } => parts.iter().for_each(|p| f(&p.expr)),
            ExprKind::If { cond, then, else_ } => {
                f(cond);
                f(then);
                if let Some(e) = else_ {
                    f(e);
                }
            }
            ExprKind::Block(b) | ExprKind::Loop(b) => block(b, f),
            ExprKind::While {
                cond, body, update, ..
            } => {
                f(cond);
                f(body);
                if let Some(u) = update {
                    f(u);
                }
            }
            ExprKind::Break(v) | ExprKind::Return(v) => {
                if let Some(v) = v {
                    f(v);
                }
            }
        }
    }

    /// Variables assigned anywhere inside this expression (`x = …`, `x += …`, `x++`).
    pub fn assigned_vars(&self, out: &mut Vec<Var>) {
        match &self.kind {
            ExprKind::Assign {
                target: Target::Var(v),
                ..
            }
            | ExprKind::IncDec {
                target: Target::Var(v),
                ..
            } => out.push(*v),
            _ => {}
        }
        self.for_each_child(&mut |c| c.assigned_vars(out));
    }
}
