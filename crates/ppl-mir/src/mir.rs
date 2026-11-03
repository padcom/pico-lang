//! The mid-level IR: functions as control-flow graphs of basic blocks over typed locals.
//!
//! Integer arithmetic in [`Rvalue::Binary`] and [`Rvalue::Unary`] traps on overflow and
//! division by zero, and [`Rvalue::Cast`] traps when a narrowing conversion doesn't fit; each
//! statement carries the source location for the panic message. Calls are terminators so
//! exceptions can later lower to `invoke`.

use ppl_sema::hir::{Builtin, FnId, GlobalId};
use ppl_sema::ty::{EnumId, StructId};
use ppl_sema::{FloatTy, IntTy, Ty};
use std::fmt::{self, Write};

/// Panic messages shared by every backend, so interpreted and native runs print the same text.
pub mod panics {
    pub const OVERFLOW: &str = "integer overflow";
    pub const DIV_ZERO: &str = "division by zero";
    pub const SHIFT: &str = "shift amount out of range";
    /// Interpreter only (natively undefined behavior): pointer arithmetic that leaves the
    /// array a pointer points into.
    pub const POINTER_OUT_OF_BOUNDS: &str = "pointer arithmetic outside an array";
    pub const NEG_EXPONENT: &str = "negative exponent";
    pub const INT_CAST: &str = "integer conversion out of range";
    pub const FLOAT_CAST: &str = "float conversion out of range";
    pub const CHAR_CAST: &str = "invalid char code";
    pub const STACK_OVERFLOW: &str = "stack overflow";
    pub const INDEX: &str = "index out of bounds";
    pub const RANGE_STEP: &str = "range step must be positive";
    pub const NULL_UNWRAP: &str = "non-null assertion on a null value";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Local(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

/// A source position, for panic messages (line and column in the entry file, 0 elsewhere),
/// and the span of the code, for compile-time diagnostics (constant folding).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Loc {
    pub line: u32,
    pub col: u32,
    pub span: Option<ppl_diag::Span>,
}

#[derive(Clone, Debug)]
pub struct Program {
    /// The source file name, used in panic locations.
    pub file: String,
    pub fns: Vec<Function>,
    pub entry: FnId,
    /// Per file-based worker (spec §10.2), in order of their ids (from 1): the function its
    /// thread starts in (the library modules' code, then the worker module's).
    pub worker_entries: Vec<FnId>,
    /// Where inline workers' threads start (if the program has any).
    pub inline_worker_entry: Option<FnId>,
    pub globals: Vec<GlobalDecl>,
    pub structs: Vec<StructLayout>,
    pub enums: Vec<EnumLayout>,
    /// Interface method tables: for one implementing type and interface, the function for
    /// each interface method slot. Every entry takes the interface value as its first argument.
    pub itables: Vec<Vec<FnId>>,
    /// `@export`ed functions (spec §12.3, §12.4), each called through a wrapper.
    pub exports: Vec<Export>,
    /// Functions declared in `extern "C"` blocks: their bodies (a panic, for the
    /// interpreter) are replaced by calls of these C symbols in native code.
    pub externs: Vec<(FnId, String)>,
    /// `@link` libraries.
    pub links: Vec<String>,
    /// The source files by `FileId` (empty unless the driver filled it in): for debug info.
    pub sources: Vec<Source>,
}

/// A source file, for debug info: where a debugger finds it, and where its lines start.
#[derive(Clone, Debug, Default)]
pub struct Source {
    pub path: String,
    pub line_starts: Vec<u32>,
}

impl Source {
    /// The 1-based line and column of byte `offset`.
    pub fn line_col(&self, offset: u32) -> (u32, u32) {
        let line = self.line_starts.partition_point(|&s| s <= offset).max(1);
        let start = self.line_starts.get(line - 1).copied().unwrap_or(0);
        (line as u32, offset - start + 1)
    }
}

/// An `@export`ed function, as the outside sees it.
#[derive(Clone, Debug)]
pub struct Export {
    /// What the outside calls (decision 258): for JavaScript, a wrapper that takes structs
    /// through pointers (and returns them into a buffer given first), and everything else
    /// as is.
    pub wrapper: FnId,
    /// The exported function itself.
    pub func: FnId,
    pub name: String,
    pub abi: ppl_sema::hir::ExportAbi,
    pub params: Vec<(String, Ty)>,
    pub ret: Ty,
}

/// A class's runtime metadata: its base class and its vtable (empty for structs).
#[derive(Debug, Default, Clone)]
pub struct ClassMeta {
    pub base: Option<StructId>,
    pub vtable: Vec<FnId>,
}

/// An enum's variants and their payload fields, in declaration order.
#[derive(Clone, Debug)]
pub struct EnumLayout {
    pub name: String,
    pub variants: Vec<VariantLayout>,
}

#[derive(Clone, Debug)]
pub struct VariantLayout {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
    /// Declared with parentheses (`Unit()`), which the debug format shows.
    pub has_parens: bool,
}

/// Which field a projection names, for looking up its type.
#[derive(Clone, Copy, Debug)]
pub enum FieldRef {
    Struct(StructId, usize),
    Variant(EnumId, u32, usize),
}

/// A struct's or class's fields, in declaration order.
#[derive(Clone, Debug)]
pub struct StructLayout {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
    /// Per field: `@weak` (spec §9.3; a `C?` or `T[]?` field of a class that the collector
    /// clears when its target dies).
    pub weak: Vec<bool>,
    /// A class: instances live on the heap behind a header.
    pub is_class: bool,
    pub meta: ClassMeta,
}

/// The size and alignment of a C struct with these parts (LLVM's layout too).
pub fn struct_layout(parts: &[(u64, u64)]) -> (u64, u64) {
    let mut offset: u64 = 0;
    let mut align: u64 = 1;
    for (s, a) in parts {
        offset = offset.next_multiple_of(*a) + s;
        align = align.max(*a);
    }
    (offset.next_multiple_of(align), align)
}

/// Whether values of `t` take space (`void`, `never` and friends don't).
fn has_storage(t: &Ty) -> bool {
    !matches!(
        t.erase(),
        Ty::Void | Ty::Never | Ty::Error | Ty::Null | Ty::Param(..)
    )
}

impl Program {
    /// Size and alignment in bytes, as LLVM lays the type out on 64-bit targets.
    pub fn size_align(&self, t: &Ty) -> (u64, u64) {
        self.size_align_with(t, 8)
    }

    /// Size and alignment in bytes on a target whose pointers take `ptr` bytes (4 on wasm32).
    pub fn size_align_with(&self, t: &Ty, ptr: u64) -> (u64, u64) {
        match t {
            Ty::Int(it) => {
                let b = u64::from(it.bits() / 8);
                (b, b)
            }
            Ty::Float(FloatTy::F32) | Ty::Char => (4, 4),
            Ty::Float(FloatTy::F64) => (8, 8),
            Ty::Bool => (1, 1),
            Ty::Str | Ty::Array(_) | Ty::Ref(_) | Ty::Class(..) => (ptr, ptr),
            Ty::Iface(..) | Ty::Fn(..) => (2 * ptr, ptr),
            Ty::Nullable(inner) if inner.is_ref() => (ptr, ptr),
            Ty::Nullable(inner) => struct_layout(&[(1, 1), self.size_align_with(inner, ptr)]),
            Ty::Struct(sid, _, args) => {
                let owner = Ty::type_owner(*sid);
                let parts: Vec<(u64, u64)> = self
                    .layout(*sid)
                    .fields
                    .iter()
                    .map(|(_, t)| t.subst(owner, args))
                    .filter(has_storage)
                    .map(|t| self.size_align_with(&t, ptr))
                    .collect();
                struct_layout(&parts)
            }
            Ty::Tuple(ts) => {
                let parts: Vec<(u64, u64)> =
                    ts.iter().map(|t| self.size_align_with(t, ptr)).collect();
                struct_layout(&parts)
            }
            Ty::Range(it) => {
                let b = u64::from(it.bits() / 8);
                (3 * b, b)
            }
            Ty::Fixed(e, n) => {
                let (s, a) = self.size_align_with(e, ptr);
                (s * n, a)
            }
            Ty::FixedN(..) => unreachable!("a `comptime` length outside generic code"),
            Ty::Enum(eid, _) => {
                let words = self
                    .enum_layout(*eid)
                    .variants
                    .iter()
                    .map(|v| {
                        let parts: Vec<(u64, u64)> = v
                            .fields
                            .iter()
                            .map(|(_, t)| self.size_align_with(t, ptr))
                            .collect();
                        struct_layout(&parts).0.div_ceil(8)
                    })
                    .max()
                    .unwrap_or(0);
                if words == 0 {
                    (4, 4)
                } else {
                    (8 + 8 * words, 8)
                }
            }
            Ty::Lits(..)
            | Ty::Union(..)
            | Ty::Variant(..)
            | Ty::Distinct(..)
            | Ty::Immutable(_) => self.size_align_with(&t.erase(), ptr),
            Ty::Void | Ty::Never | Ty::Error | Ty::Null | Ty::Param(..) => (1, 1),
        }
    }

    pub fn func(&self, id: FnId) -> &Function {
        &self.fns[id.0 as usize]
    }

    pub fn layout(&self, id: StructId) -> &StructLayout {
        &self.structs[id.0 as usize]
    }

    pub fn enum_layout(&self, id: EnumId) -> &EnumLayout {
        &self.enums[id.0 as usize]
    }

    /// The type of a struct or variant field.
    pub fn field_ty(&self, f: FieldRef) -> Ty {
        match f {
            FieldRef::Struct(sid, i) => self.layout(sid).fields[i].1.clone(),
            FieldRef::Variant(eid, v, i) => self.enum_layout(eid).variants[v as usize].fields[i]
                .1
                .clone(),
        }
    }

    /// Whether values of `t` hold GC references, which the collector traces (spec §9.1).
    pub fn traced(&self, t: &Ty) -> bool {
        match &t.erase() {
            Ty::Str | Ty::Array(_) | Ty::Class(..) | Ty::Iface(..) | Ty::Fn(..) | Ty::Ref(_) => {
                true
            }
            Ty::Nullable(t) => self.traced(t),
            Ty::Struct(sid, _, args) => {
                let owner = Ty::type_owner(*sid);
                self.layout(*sid)
                    .fields
                    .iter()
                    .any(|(_, f)| self.traced(&f.subst(owner, args)))
            }
            Ty::Tuple(ts) => ts.iter().any(|t| self.traced(t)),
            Ty::Fixed(e, n) => *n > 0 && self.traced(e),
            Ty::Enum(eid, _) => self
                .enum_layout(*eid)
                .variants
                .iter()
                .any(|v| v.fields.iter().any(|(_, f)| self.traced(f))),
            _ => false,
        }
    }

    /// The type of `place` inside `f`.
    pub fn place_ty(&self, f: &Function, place: &Place) -> Ty {
        let mut t = match place.base {
            Base::Local(l) => f.local_ty(l).clone(),
            Base::Global(g) => self.globals[g.0 as usize].ty.clone(),
            Base::Deref(l) => match f.local_ty(l) {
                Ty::Ref(inner) => (**inner).clone(),
                other => unreachable!("deref of non-reference {other}"),
            },
        };
        for p in &place.proj {
            t = project(&t, p, |f| self.field_ty(f));
        }
        t
    }
}

/// The type of `p` applied to a value of type `t`; `field` gives a struct's field types.
pub fn project(t: &Ty, p: &Proj, field: impl Fn(FieldRef) -> Ty) -> Ty {
    let t = &t.erase();
    match (t, p) {
        (Ty::Struct(sid, _, args) | Ty::Class(sid, _, args), Proj::Field(i)) => {
            field(FieldRef::Struct(*sid, *i as usize)).subst(Ty::type_owner(*sid), args)
        }
        (Ty::Enum(eid, _), Proj::VariantField(v, i)) => {
            field(FieldRef::Variant(*eid, *v, *i as usize))
        }
        (Ty::Tuple(ts), Proj::Field(i)) => ts[*i as usize].clone(),
        (Ty::Range(it), Proj::Field(_)) => Ty::Int(*it),
        (Ty::Array(e) | Ty::Fixed(e, _), Proj::Index(_)) => (**e).clone(),
        (Ty::Nullable(t), Proj::Payload) => (**t).clone(),
        (other, p) => unreachable!("projection {p:?} of {other}"),
    }
}

#[derive(Clone, Debug)]
pub struct GlobalDecl {
    pub name: String,
    pub ty: Ty,
}

#[derive(Clone, Debug)]
pub struct LocalDecl {
    pub ty: Ty,
    /// The source name; `None` for temporaries.
    pub name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    pub params: Vec<Local>,
    pub ret: Ty,
    pub locals: Vec<LocalDecl>,
    pub blocks: Vec<BasicBlock>,
    /// The core library's code (or a generated adapter): kept only when something reaches it.
    pub library: bool,
}

impl Function {
    pub fn local_ty(&self, l: Local) -> &Ty {
        &self.locals[l.0 as usize].ty
    }

    pub fn block(&self, b: BlockId) -> &BasicBlock {
        &self.blocks[b.0 as usize]
    }
}

#[derive(Clone, Debug)]
pub struct BasicBlock {
    pub stmts: Vec<Statement>,
    pub term: Terminator,
}

/// Where a place starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    Local(Local),
    Global(GlobalId),
    /// The value a reference-typed local points to (`this` in methods).
    Deref(Local),
}

/// One step from a value to a part of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proj {
    /// A struct field, tuple element, or range part (0 start, 1 end, 2 step).
    Field(u32),
    /// The element of a fixed or growable array at the index held in a local (`int`). Lowering
    /// emits the bounds check before the place is used.
    Index(Local),
    /// The value inside a `T?` that is known not to be null.
    Payload,
    /// Field `.1` of variant `.0` of an enum value known to be that variant.
    VariantField(u32, u32),
}

/// A memory location: a base and a path of projections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub base: Base,
    pub proj: Vec<Proj>,
}

impl Place {
    pub fn local(l: Local) -> Place {
        Place {
            base: Base::Local(l),
            proj: Vec::new(),
        }
    }

    pub fn global(g: GlobalId) -> Place {
        Place {
            base: Base::Global(g),
            proj: Vec::new(),
        }
    }

    pub fn deref(l: Local) -> Place {
        Place {
            base: Base::Deref(l),
            proj: Vec::new(),
        }
    }

    pub fn field(&self, i: u32) -> Place {
        let mut p = self.clone();
        p.proj.push(Proj::Field(i));
        p
    }

    pub fn payload(&self) -> Place {
        let mut p = self.clone();
        p.proj.push(Proj::Payload);
        p
    }

    pub fn variant_field(&self, v: u32, i: u32) -> Place {
        let mut p = self.clone();
        p.proj.push(Proj::VariantField(v, i));
        p
    }

    pub fn index(&self, l: Local) -> Place {
        let mut p = self.clone();
        p.proj.push(Proj::Index(l));
        p
    }

    /// Whether the place goes through an array element (and so can be invalidated).
    pub fn has_index(&self) -> bool {
        self.proj.iter().any(|p| matches!(p, Proj::Index(_)))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Int(i128, IntTy),
    Float(f64, FloatTy),
    Bool(bool),
    Char(char),
    Str(String),
    /// `null` of the given `T?` type.
    Null(Ty),
    /// Any value of the type (an argument left out, whose default the callee computes).
    Undef(Ty),
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
            Const::Null(t) | Const::Undef(t) => t.clone(),
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
    /// The string form of a primitive (as `println` shows it): templates and `toString()`.
    ToStr(Operand),
    /// A struct value from its fields (in declaration order).
    Aggregate(Ty, Vec<Operand>),
    /// A reference to a place (a method receiver, or `ptr.of(place)`).
    Ref(Place),
    /// `p + n` / `p - n` (`sub`) on a `ptr<T>`: `n` (an integer) elements of `T` further.
    PtrOffset(Operand, Operand, bool),
    /// `sizeof<T>()` / `alignof<T>()` (`align`), a `usize`; see [`Program::size_align`].
    SizeOf(Ty, bool),
    /// `unsafe.cast<T>(x)`: the bits of a number as another number type of the same size.
    BitCast(Operand, Ty),
    /// `count` zeroed `T`s from an `Arena` (its handle): a `ptr<T>` to the first.
    RawArenaAlloc(Ty, Operand, Operand),
    /// The next message of a worker channel (its handle), as a `T`.
    WorkerTake(Ty, Operand),
    /// A template part with a format spec: `${x:08.2f}`.
    Format(Operand, ppl_sema::format::FormatSpec),
    /// `s.length`: a string's length in code points.
    StrLen(Operand),
    /// `s[i]`: the `char` at code-point index `i` (panics when out of bounds).
    StrCharAt(Operand, Operand),
    /// `s.slice(start, end)`: code points `start..end`, where negative indices count from the
    /// end and both are clamped to the string (JavaScript's `slice`).
    StrSlice(Operand, Operand, Operand),
    /// A tuple, fixed array or range value (the type) from its parts in order.
    Tuple(Ty, Vec<Operand>),
    /// `[value; n]`
    Repeat(Operand, u64),
    /// A new growable array with element type and initial elements.
    ArrayNew(Ty, Vec<Operand>),
    /// A new array with the elements `[start, end)` of another (`...rest` patterns).
    ArraySlice(Operand, Operand, Operand),
    /// `xs.length`
    ArrayLen(Operand),
    /// `xs.push(v)` (void).
    ArrayPush(Operand, Operand),
    /// `xs.pop()`: removes the last element, if any (`T?`).
    ArrayPop(Operand),
    /// `0 <= index < len` (both `int`).
    InBounds(Operand, Operand),
    /// Whether `a + b` overflows their integer type (range iteration).
    AddOverflows(Operand, Operand),
    /// `T` → `T?` (the type is the `T?`).
    WrapSome(Operand, Ty),
    /// Whether a `T?` is null.
    IsNull(Operand),
    /// The `T` inside a `T?` that lowering checked isn't null.
    Unwrap(Operand),
    /// An enum value: variant index and payload fields in declaration order.
    Variant(EnumId, u32, Vec<Operand>),
    /// The variant index of an enum value (`u32`).
    Tag(Operand),
    /// A new instance of a class, its fields not yet set (the constructor runs next).
    New(Ty),
    /// Whether a (possibly null) object reference is an instance of the class (or a subclass).
    InstanceOf(Operand, StructId),
    /// Starts showing an object in its default string form: `false` if it's already being
    /// shown further up (a cycle), otherwise it's marked (a `bool`).
    ShowEnter(Operand),
    /// Ends showing an object (after a `ShowEnter` that returned `true`).
    ShowLeave(Operand),
    /// An interface value from an object (or a struct, boxed) and an itable index.
    ToIface(Operand, u32),
    /// The object reference (or the boxed struct's reference) inside an interface value.
    IfaceData(Operand),
    /// A function value: the (lifted) function and the captured values, in order (its
    /// environment, which the function receives as its first parameter).
    Closure(FnId, Vec<Operand>),
    /// A function's address as a C function pointer (callbacks of extern functions).
    FnAddr(FnId),
    /// A new heap cell of the type holding the value (`Const::Void`: uninitialized); the
    /// result is a reference to it (captured `let` variables).
    BoxNew(Ty, Operand),
    /// The `u64` hash of a primitive or string (the same in every backend).
    Hash(Operand),
    /// Mixes a field's hash into an accumulated one.
    HashCombine(Operand, Operand),
    /// Makes the error the pending exception (`throw`): an `Error` object (tag 0), or a boxed
    /// lightweight throwable with its type's tag (`u32`).
    SetException(Operand, Operand),
    /// The pending exception's tag (0: an `Error` object).
    ExceptionTag,
    /// The pending exception (of the given type), which stops being pending.
    TakeException(Ty),
    /// Whether an exception is pending (checked after calls of throwing functions).
    ExceptionPending,
    /// `comptime e`: the result of the closure (called with an empty environment) evaluated
    /// inside the compiler, with an optional step limit. The driver replaces it with the
    /// value before any backend runs.
    Comptime(FnId, Option<u64>),
}

/// The operands of an rvalue, by reference or mutably (`iter` or `iter_mut`).
macro_rules! rvalue_operands {
    ($self:ident, $f:ident, $iter:ident) => {{
        use Rvalue::*;
        match $self {
            Use(a)
            | Unary(_, a)
            | Cast(a, _)
            | ToStr(a)
            | BitCast(a, _)
            | WorkerTake(_, a)
            | Format(a, _)
            | StrLen(a)
            | Repeat(a, _)
            | ArrayLen(a)
            | ArrayPop(a)
            | WrapSome(a, _)
            | IsNull(a)
            | Unwrap(a)
            | Tag(a)
            | InstanceOf(a, _)
            | ShowEnter(a)
            | ShowLeave(a)
            | ToIface(a, _)
            | IfaceData(a)
            | BoxNew(_, a)
            | Hash(a) => $f(a),
            Binary(_, a, b)
            | PtrOffset(a, b, _)
            | RawArenaAlloc(_, a, b)
            | StrCharAt(a, b)
            | ArrayPush(a, b)
            | InBounds(a, b)
            | AddOverflows(a, b)
            | HashCombine(a, b)
            | SetException(a, b) => {
                $f(a);
                $f(b);
            }
            StrSlice(a, b, c) | ArraySlice(a, b, c) => {
                $f(a);
                $f(b);
                $f(c);
            }
            Aggregate(_, ops)
            | Tuple(_, ops)
            | ArrayNew(_, ops)
            | Variant(_, _, ops)
            | Closure(_, ops) => ops.$iter().for_each($f),
            Ref(_) | SizeOf(..) | New(_) | ExceptionTag | TakeException(_) | ExceptionPending
            | Comptime(..) | FnAddr(_) => {}
        }
    }};
}

impl Rvalue {
    /// Calls `f` on every operand, in order.
    pub fn for_each_operand(&mut self, f: &mut impl FnMut(&mut Operand)) {
        rvalue_operands!(self, f, iter_mut)
    }

    /// Calls `f` on every operand, in order.
    pub fn operands(&self, f: &mut impl FnMut(&Operand)) {
        rvalue_operands!(self, f, iter)
    }

    /// The place a `Ref` takes the address of.
    pub fn referenced(&self) -> Option<&Place> {
        match self {
            Rvalue::Ref(p) => Some(p),
            _ => None,
        }
    }
}

impl Terminator {
    /// Calls `f` on every operand, in order.
    pub fn for_each_operand(&mut self, f: &mut impl FnMut(&mut Operand)) {
        match self {
            Terminator::Branch { cond, .. } => f(cond),
            Terminator::Return(Some(o)) | Terminator::Panic { msg: o, .. } => f(o),
            Terminator::Call { args, .. } | Terminator::TailCall { args, .. } => {
                args.iter_mut().for_each(f)
            }
            Terminator::Uncaught { class, msg } => {
                f(class);
                f(msg);
            }
            Terminator::Goto(_)
            | Terminator::Return(None)
            | Terminator::Guard { .. }
            | Terminator::Unguard(_)
            | Terminator::Resume
            | Terminator::Unreachable => {}
        }
    }

    /// Calls `f` on every operand, in order.
    pub fn operands(&self, f: &mut impl FnMut(&Operand)) {
        match self {
            Terminator::Branch { cond, .. } => f(cond),
            Terminator::Return(Some(o)) | Terminator::Panic { msg: o, .. } => f(o),
            Terminator::Call { args, .. } | Terminator::TailCall { args, .. } => {
                args.iter().for_each(f)
            }
            Terminator::Uncaught { class, msg } => {
                f(class);
                f(msg);
            }
            Terminator::Goto(_)
            | Terminator::Return(None)
            | Terminator::Guard { .. }
            | Terminator::Unguard(_)
            | Terminator::Resume
            | Terminator::Unreachable => {}
        }
    }
}

impl Place {
    /// Every local the place mentions: its base and its index locals.
    pub fn locals(&self) -> impl Iterator<Item = Local> + '_ {
        let base = match self.base {
            Base::Local(l) | Base::Deref(l) => Some(l),
            Base::Global(_) => None,
        };
        base.into_iter()
            .chain(self.proj.iter().filter_map(|p| match p {
                Proj::Index(l) => Some(*l),
                _ => None,
            }))
    }
}

#[derive(Clone, Debug)]
pub struct Statement {
    pub place: Place,
    pub rvalue: Rvalue,
    pub loc: Loc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Callee {
    Fn(FnId),
    Builtin(Builtin),
    /// A virtual method: the implementation in vtable `slot` of the receiver's runtime
    /// class (the first argument). `decl` is the statically known method (for its type).
    Virtual {
        slot: u32,
        decl: FnId,
    },
    /// A function value (the first argument): its function, given its environment and the
    /// rest of the arguments.
    Indirect,
    /// An interface method: entry `slot` of the itable in the interface value (the first
    /// argument). `decl` is the interface's method (for its type).
    Iface {
        slot: u32,
        decl: FnId,
    },
}

#[derive(Clone, Debug)]
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
    /// `return tailcall f(args)`: calls `callee` in place of the current call (same
    /// signature) and returns what it returns.
    TailCall {
        callee: FnId,
        args: Vec<Operand>,
        loc: Loc,
    },
    /// `panic(msg)` or a failed `assert`.
    Panic {
        msg: Operand,
        loc: Loc,
    },
    /// Enters a region protected against panics: a panic inside it (also in callees) jumps
    /// to `handler`, which runs the region's `finally` code and then `Resume`s.
    Guard {
        body: BlockId,
        handler: BlockId,
    },
    /// Leaves the innermost guarded region.
    Unguard(BlockId),
    /// Continues the panic that entered this handler.
    Resume,
    /// An exception escaped the top level: prints `uncaught <class>: <message>` and exits
    /// with code 1.
    Uncaught {
        class: Operand,
        msg: Operand,
    },
    Unreachable,
}

// ---------------------------------------------------------------- dump

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.base {
            Base::Local(l) => write!(f, "_{}", l.0)?,
            Base::Global(g) => write!(f, "@g{}", g.0)?,
            Base::Deref(l) => write!(f, "(*_{})", l.0)?,
        }
        for p in &self.proj {
            match p {
                Proj::Field(i) => write!(f, ".{i}")?,
                Proj::Index(l) => write!(f, "[_{}]", l.0)?,
                Proj::Payload => write!(f, ".some")?,
                Proj::VariantField(v, i) => write!(f, ".v{v}.{i}")?,
            }
        }
        Ok(())
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
            Const::Null(t) => write!(f, "null<{t}>"),
            Const::Undef(t) => write!(f, "undef<{t}>"),
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
            Rvalue::ToStr(a) => write!(f, "ToStr({a})"),
            Rvalue::Aggregate(t, ops) => {
                let sid = match t {
                    Ty::Struct(s, _, _) => *s,
                    _ => StructId(u32::MAX),
                };
                let ops: Vec<String> = ops.iter().map(|o| o.to_string()).collect();
                write!(f, "struct#{}{{{}}}", sid.0, ops.join(", "))
            }
            Rvalue::Ref(p) => write!(f, "&{p}"),
            Rvalue::PtrOffset(p, n, sub) => {
                write!(f, "PtrOffset({p}, {}{n})", if *sub { "-" } else { "" })
            }
            Rvalue::SizeOf(t, false) => write!(f, "SizeOf({t})"),
            Rvalue::SizeOf(t, true) => write!(f, "AlignOf({t})"),
            Rvalue::BitCast(a, t) => write!(f, "BitCast({a}, {t})"),
            Rvalue::RawArenaAlloc(t, a, n) => write!(f, "RawArenaAlloc<{t}>({a}, {n})"),
            Rvalue::WorkerTake(t, h) => write!(f, "WorkerTake<{t}>({h})"),
            Rvalue::Format(a, spec) => write!(f, "Format({a}, {spec:?})"),
            Rvalue::StrCharAt(a, i) => write!(f, "StrCharAt({a}, {i})"),
            Rvalue::StrSlice(a, i, j) => write!(f, "StrSlice({a}, {i}, {j})"),
            Rvalue::Tuple(t, ops) => {
                let ops: Vec<String> = ops.iter().map(|o| o.to_string()).collect();
                write!(f, "{t}({})", ops.join(", "))
            }
            Rvalue::Repeat(v, n) => write!(f, "[{v}; {n}]"),
            Rvalue::ArrayNew(t, ops) => {
                let ops: Vec<String> = ops.iter().map(|o| o.to_string()).collect();
                write!(f, "ArrayNew<{t}>[{}]", ops.join(", "))
            }
            Rvalue::ArraySlice(a, b, c) => write!(f, "ArraySlice({a}, {b}, {c})"),
            Rvalue::ArrayLen(a) => write!(f, "ArrayLen({a})"),
            Rvalue::ArrayPush(a, v) => write!(f, "ArrayPush({a}, {v})"),
            Rvalue::ArrayPop(a) => write!(f, "ArrayPop({a})"),
            Rvalue::InBounds(i, n) => write!(f, "InBounds({i}, {n})"),
            Rvalue::AddOverflows(a, b) => write!(f, "AddOverflows({a}, {b})"),
            Rvalue::WrapSome(a, t) => write!(f, "Some<{t}>({a})"),
            Rvalue::IsNull(a) => write!(f, "IsNull({a})"),
            Rvalue::ShowEnter(a) => write!(f, "ShowEnter({a})"),
            Rvalue::ShowLeave(a) => write!(f, "ShowLeave({a})"),
            Rvalue::Unwrap(a) => write!(f, "Unwrap({a})"),
            Rvalue::Variant(e, v, ops) => {
                let ops: Vec<String> = ops.iter().map(|o| o.to_string()).collect();
                write!(f, "enum#{}::{v}({})", e.0, ops.join(", "))
            }
            Rvalue::Tag(a) => write!(f, "Tag({a})"),
            Rvalue::New(t) => write!(f, "New({t})"),
            Rvalue::InstanceOf(a, c) => write!(f, "InstanceOf({a}, class#{})", c.0),
            Rvalue::ToIface(a, i) => write!(f, "ToIface({a}, itable#{i})"),
            Rvalue::IfaceData(a) => write!(f, "IfaceData({a})"),
            Rvalue::FnAddr(id) => write!(f, "FnAddr(fn#{})", id.0),
            Rvalue::Closure(id, ops) => {
                let ops: Vec<String> = ops.iter().map(|o| o.to_string()).collect();
                write!(f, "Closure(fn#{}, [{}])", id.0, ops.join(", "))
            }
            Rvalue::BoxNew(t, a) => write!(f, "BoxNew<{t}>({a})"),
            Rvalue::Comptime(id, _) => write!(f, "Comptime(fn#{})", id.0),
            Rvalue::Hash(a) => write!(f, "Hash({a})"),
            Rvalue::HashCombine(a, b) => write!(f, "HashCombine({a}, {b})"),
            Rvalue::SetException(a, t) => write!(f, "SetException({a}, {t})"),
            Rvalue::ExceptionTag => write!(f, "ExceptionTag"),
            Rvalue::TakeException(t) => write!(f, "TakeException<{t}>"),
            Rvalue::ExceptionPending => write!(f, "ExceptionPending"),
        }
    }
}

fn callee_name(p: &Program, c: Callee) -> String {
    match c {
        Callee::Fn(id) => p.func(id).name.clone(),
        Callee::Builtin(b) => format!("builtin {}", b.name()),
        Callee::Virtual { slot, decl } => format!("virtual[{slot}] {}", p.func(decl).name),
        Callee::Iface { slot, decl } => format!("iface[{slot}] {}", p.func(decl).name),
        Callee::Indirect => "indirect".to_string(),
    }
}

/// The textual MIR dump (`ppl build --emit mir`).
/// The functions the program can reach from its entry (calls, closures and function values,
/// vtables, itables). The prelude's unused code isn't, so backends and dumps skip it.
pub fn live_functions(p: &Program) -> Vec<bool> {
    let mut live = vec![false; p.fns.len()];
    let mut work = vec![p.entry];
    work.extend(p.worker_entries.iter().copied());
    work.extend(p.inline_worker_entry);
    work.extend(
        p.fns
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.library)
            .map(|(i, _)| FnId(i as u32)),
    );
    for s in &p.structs {
        work.extend(s.meta.vtable.iter().copied());
    }
    for t in &p.itables {
        work.extend(t.iter().copied());
    }
    while let Some(f) = work.pop() {
        let i = f.0 as usize;
        if i >= live.len() || live[i] {
            continue;
        }
        live[i] = true;
        for b in &p.fns[i].blocks {
            for st in &b.stmts {
                if let Rvalue::Closure(g, _) | Rvalue::Comptime(g, _) | Rvalue::FnAddr(g) =
                    &st.rvalue
                {
                    work.push(*g);
                }
            }
            match &b.term {
                Terminator::Call {
                    callee:
                        Callee::Fn(g) | Callee::Virtual { decl: g, .. } | Callee::Iface { decl: g, .. },
                    ..
                } => work.push(*g),
                Terminator::TailCall { callee, .. } => work.push(*callee),
                _ => {}
            }
        }
    }
    live
}

pub fn dump(p: &Program) -> String {
    let live = live_functions(p);
    let mut out = String::new();
    for (i, g) in p.globals.iter().enumerate() {
        let _ = writeln!(out, "global @g{i}: {} // {}", g.ty, g.name);
    }
    if !p.globals.is_empty() {
        out.push('\n');
    }
    let mut first = true;
    for (fi, func) in p.fns.iter().enumerate() {
        if !live[fi] {
            continue;
        }
        if !first {
            out.push('\n');
        }
        first = false;
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
                Terminator::TailCall { callee, args, .. } => {
                    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
                    format!("tailcall {}({})", p.func(*callee).name, args.join(", "))
                }
                Terminator::Uncaught { class, msg } => format!("uncaught {class}: {msg}"),
                Terminator::Guard { body, handler } => {
                    format!("guard -> bb{} (panic: bb{})", body.0, handler.0)
                }
                Terminator::Unguard(b) => format!("unguard -> bb{}", b.0),
                Terminator::Resume => "resume".to_string(),
                Terminator::Unreachable => "unreachable".to_string(),
            };
            let _ = writeln!(out, "    {term}");
        }
        out.push_str("}\n");
    }
    out
}
