//! Type checking (spec §2.1–§2.2, §3.1, §5.1) over the HIR.
//!
//! Bidirectional: [`Checker::check`] pushes an expected type down (so `const x: u8 = 1` types
//! the literal as `u8`), [`Checker::infer`] computes a type bottom-up. Implicit conversions
//! happen only when lossless; they're recorded in [`Types::coercions`] for lowering.
//!
//! Return types are inferred on demand: a call to a function whose return type is still
//! unknown checks that function first. Reaching a function that is already being checked
//! means it's recursive, and recursive functions must declare their return type.

use crate::exhaust::{self, Ctor, Pat};
use crate::hir::*;
use crate::ty::{EnumId, FloatTy, IntTy, LitVal, Ty};
use pico_diag::{Diagnostic, Span, codes};
use pico_syntax::ast::{BinOp, UnOp};
use pico_syntax::token::NumSuffix;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
pub struct FnSig {
    pub params: Vec<Ty>,
    pub ret: Ty,
}

/// The type checker's results.
#[derive(Debug, Default, Clone)]
pub struct Types {
    /// The type of every expression (indexed by `ExprId`).
    pub exprs: Vec<Ty>,
    /// Expressions whose value is implicitly widened to another type.
    pub coercions: HashMap<ExprId, Ty>,
    /// Local types, per function.
    pub locals: Vec<Vec<Ty>>,
    pub globals: Vec<Ty>,
    pub sigs: Vec<FnSig>,
    /// Parsed format specs of template parts, keyed by the part's expression.
    pub formats: HashMap<ExprId, crate::format::FormatSpec>,
    /// Field index of each `object.field` (member expressions and assignment targets).
    pub fields: HashMap<ExprId, u32>,
    /// For each struct literal, the field index of each initializer, in source order.
    pub objects: HashMap<ExprId, (StructId, Vec<u32>)>,
    /// Method calls: the call expression and the method it calls.
    pub methods: HashMap<ExprId, FnId>,
    /// Variable reads of a `T?` binding that narrowing typed as `T` (lowering unwraps them).
    pub narrowed: HashSet<ExprId>,
    /// Variant patterns: the enum and variant index.
    pub variants: HashMap<ExprId, (EnumId, u32)>,
    /// Named field patterns (`Circle { r }`, `Point { x, y }`): each field's index, in order.
    pub pat_fields: HashMap<ExprId, Vec<u32>>,
    /// Variant constructors: the field index of each argument, in source order.
    pub variant_args: HashMap<ExprId, Vec<u32>>,
    /// `obj.name` that calls a getter.
    pub getters: HashMap<ExprId, FnId>,
    /// Assignment targets (by target id) that call a setter.
    pub setters: HashMap<ExprId, FnId>,
    /// The constructor each `new` / `super(…)` / `this(…)` calls.
    pub ctor_calls: HashMap<ExprId, FnId>,
    /// Method calls that dispatch through the vtable, with their slot.
    pub virtual_calls: HashMap<ExprId, u32>,
    /// Type-test and class patterns that check the runtime class (downcasts).
    pub downcasts: HashSet<ExprId>,
    /// Method calls on interface values, with the method's slot in the interface.
    pub iface_calls: HashMap<ExprId, u32>,
    /// Member reads on discriminated unions: the variant and field they read.
    pub variant_fields: HashMap<ExprId, (u32, u32)>,
    /// Object literals that build a discriminated union's variant: enum, variant, and the
    /// field index of each initializer.
    pub object_variants: HashMap<ExprId, (EnumId, u32, Vec<u32>)>,
    /// Patterns that test which member a union value holds (by member index).
    pub union_tests: HashMap<ExprId, u32>,
    /// Calls of generic functions: the type arguments (may mention the caller's own type
    /// parameters; monomorphization substitutes them).
    pub instances: HashMap<ExprId, Vec<Ty>>,
    /// Calls of methods and constructors of generic types (and `new` of a generic class):
    /// the type's arguments.
    pub recv_args: HashMap<ExprId, Vec<Ty>>,
    /// How each call's arguments fill the callee's parameters.
    pub arg_slots: HashMap<ExprId, Vec<Slot>>,
    /// The overload each call of an overloaded name picked.
    pub overloads: HashMap<ExprId, FnId>,
    /// Assignments and `++`/`--` that write a reactive cell (spec §16.2): lowering calls
    /// `__set`.
    pub reactive_writes: HashSet<ExprId>,
    /// Names of reactive bindings read as their value (spec §16.2), with the cell's type: the
    /// expression's type is the value's, and lowering reads the cell with `__get`. (Other
    /// expressions of type `Reactive<T>` where a `T` is expected are coercions.)
    pub cell_reads: HashMap<ExprId, Ty>,
    /// The program's own code (outside the core library) reads or writes reactive state:
    /// lowering emits the deep-reactivity hooks (spec §16.6) and `__reactivity_on()` is true.
    pub uses_reactivity: bool,
    /// Operators and protocol uses that call a user method (`operator +`, `equals`,
    /// `compareTo`, `[]`, `[]=`), by the operator/assignment expression.
    pub op_calls: HashMap<ExprId, FnId>,
    /// Uses of a derived protocol (field by field): comparisons and `equals`/`compareTo`/
    /// `hashCode` calls on structs, enums, tuples and `@derive` classes.
    pub derived: HashMap<ExprId, Derived>,
    /// Method calls on a value whose type is a type parameter: the method's name.
    pub param_calls: HashMap<ExprId, String>,
    /// What each function may throw: its `throws` clause, or what was inferred for it.
    pub fn_throws: Vec<Vec<Ty>>,
    /// Method calls on built-in types that call a pico extension function (`xs.map(f)` →
    /// `__Array_map(xs, f)`); the type arguments are in `instances`.
    pub extensions: HashMap<ExprId, FnId>,
    /// `for-of` over a value with an `iterator()` method: the iterated expression and the
    /// method.
    pub iterables: HashMap<ExprId, FnId>,
    /// Template parts whose value implements `Formattable`: the part's expression and its
    /// `format(spec: string): string` method.
    pub formattables: HashMap<ExprId, FnId>,
}

/// A protocol implemented field by field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Derived {
    Equals,
    Compare,
    Hash,
}

/// Where a parameter's value comes from at a call.
#[derive(Clone, Debug, PartialEq)]
pub enum Slot {
    /// Argument `i` (its value, if it's `name: value`).
    Arg(usize),
    /// Left out: the callee evaluates the default.
    Default,
    /// The rest parameter: these arguments, collected into an array.
    Rest(Vec<usize>),
    /// The rest parameter: argument `i` is `...array`.
    RestSpread(usize),
    /// Element `k` of argument `i`, which spreads a tuple (`...pair`).
    TupleElem(usize, u32),
}

impl Types {
    /// Removes `immutable` from every recorded type.
    fn strip_immutable(&mut self) {
        let s = |t: &mut Ty| *t = t.strip_immutable();
        self.exprs.iter_mut().for_each(s);
        self.coercions.values_mut().for_each(s);
        self.locals.iter_mut().flatten().for_each(s);
        self.globals.iter_mut().for_each(s);
        for sig in &mut self.sigs {
            sig.params.iter_mut().for_each(s);
            s(&mut sig.ret);
        }
        for ts in self
            .instances
            .values_mut()
            .chain(self.recv_args.values_mut())
        {
            ts.iter_mut().for_each(s);
        }
    }

    /// These results with generic function `owner`'s type parameters replaced by `args`
    /// (what one monomorphized instance sees).
    pub fn substituted(&self, owner: u32, args: &[Ty]) -> Types {
        let s = |t: &Ty| t.subst(owner, args);
        Types {
            exprs: self.exprs.iter().map(s).collect(),
            coercions: self.coercions.iter().map(|(k, t)| (*k, s(t))).collect(),
            locals: self
                .locals
                .iter()
                .map(|ls| ls.iter().map(s).collect())
                .collect(),
            globals: self.globals.clone(),
            sigs: self
                .sigs
                .iter()
                .map(|g| FnSig {
                    params: g.params.iter().map(s).collect(),
                    ret: s(&g.ret),
                })
                .collect(),
            formats: self.formats.clone(),
            fields: self.fields.clone(),
            objects: self.objects.clone(),
            methods: self.methods.clone(),
            narrowed: self.narrowed.clone(),
            variants: self.variants.clone(),
            pat_fields: self.pat_fields.clone(),
            variant_args: self.variant_args.clone(),
            getters: self.getters.clone(),
            setters: self.setters.clone(),
            ctor_calls: self.ctor_calls.clone(),
            virtual_calls: self.virtual_calls.clone(),
            downcasts: self.downcasts.clone(),
            iface_calls: self.iface_calls.clone(),
            variant_fields: self.variant_fields.clone(),
            object_variants: self.object_variants.clone(),
            union_tests: self.union_tests.clone(),
            instances: self
                .instances
                .iter()
                .map(|(k, ts)| (*k, ts.iter().map(s).collect()))
                .collect(),
            recv_args: self
                .recv_args
                .iter()
                .map(|(k, ts)| (*k, ts.iter().map(s).collect()))
                .collect(),
            param_calls: self.param_calls.clone(),
            fn_throws: self.fn_throws.clone(),
            extensions: self.extensions.clone(),
            iterables: self.iterables.clone(),
            formattables: self.formattables.clone(),
            arg_slots: self.arg_slots.clone(),
            overloads: self.overloads.clone(),
            reactive_writes: self.reactive_writes.clone(),
            cell_reads: self.cell_reads.iter().map(|(k, t)| (*k, s(t))).collect(),
            uses_reactivity: self.uses_reactivity,
            op_calls: self.op_calls.clone(),
            derived: self.derived.clone(),
        }
    }
}

impl Types {
    pub fn expr(&self, id: ExprId) -> &Ty {
        &self.exprs[id.0 as usize]
    }

    /// The type an expression's value has after coercion.
    pub fn final_ty(&self, id: ExprId) -> &Ty {
        self.coercions
            .get(&id)
            .unwrap_or(&self.exprs[id.0 as usize])
    }
}

pub fn check(program: &Program) -> (Types, Vec<Diagnostic>) {
    let mut c = Checker {
        program,
        types: Types {
            exprs: vec![Ty::Error; program.expr_count()],
            coercions: HashMap::new(),
            locals: program
                .fns
                .iter()
                .map(|f| vec![Ty::Error; f.locals.len()])
                .collect(),
            globals: vec![Ty::Error; program.globals.len()],
            sigs: Vec::new(),
            formats: HashMap::new(),
            fields: HashMap::new(),
            objects: HashMap::new(),
            methods: HashMap::new(),
            narrowed: HashSet::new(),
            variants: HashMap::new(),
            pat_fields: HashMap::new(),
            variant_args: HashMap::new(),
            getters: HashMap::new(),
            setters: HashMap::new(),
            ctor_calls: HashMap::new(),
            virtual_calls: HashMap::new(),
            downcasts: HashSet::new(),
            iface_calls: HashMap::new(),
            variant_fields: HashMap::new(),
            object_variants: HashMap::new(),
            union_tests: HashMap::new(),
            instances: HashMap::new(),
            recv_args: HashMap::new(),
            arg_slots: HashMap::new(),
            overloads: HashMap::new(),
            reactive_writes: HashSet::new(),
            cell_reads: HashMap::new(),
            uses_reactivity: false,
            op_calls: HashMap::new(),
            derived: HashMap::new(),
            param_calls: HashMap::new(),
            fn_throws: program.fns.iter().map(|f| f.throws.clone()).collect(),
            extensions: HashMap::new(),
            iterables: HashMap::new(),
            formattables: HashMap::new(),
        },
        narrow: Vec::new(),
        readonly_cells: HashSet::new(),
        unsafe_depth: 0,
        inherit_unsafe: false,
        method_type_args: Vec::new(),
        checked_thunks: HashSet::new(),
        arenas: Vec::new(),
        recv_args: Vec::new(),
        pre_inferred: None,
        reuse: None,
        tail_position: false,
        tail_body: false,
        ret_hints: HashMap::new(),
        diags: Vec::new(),
        state: vec![FnState::Unchecked; program.fns.len()],
        global_known: vec![false; program.globals.len()],
        stack: Vec::new(),
    };
    for (i, g) in program.globals.iter().enumerate() {
        if let Some(t) = &g.ty {
            c.types.globals[i] = t.clone();
            c.global_known[i] = true;
        }
    }
    c.types.sigs = program
        .fns
        .iter()
        .map(|f| FnSig {
            params: f
                .params
                .iter()
                .map(|p| f.local(*p).ty.clone().unwrap_or(Ty::Error))
                .collect(),
            ret: f.ret.clone().unwrap_or(Ty::Error),
        })
        .collect();
    // Static initializers and the entry body first (they give globals their types), then
    // everything else.
    for id in &program.static_inits {
        c.check_fn(*id);
    }
    c.check_fn(program.entry);
    for i in 0..program.fns.len() {
        c.check_fn(FnId(i as u32));
    }
    c.check_overrides();
    if !c
        .diags
        .iter()
        .any(|d| d.is_error() && d.code != Some(codes::UNHANDLED_ERROR))
    {
        let more = crate::throws::check(program, &mut c.types);
        c.diags.extend(more);
    }
    // `immutable` only matters to the checks above.
    let mut types = c.types;
    types.strip_immutable();
    (types, c.diags)
}

#[derive(Clone, Copy, PartialEq)]
enum FnState {
    Unchecked,
    InProgress,
    Done,
}

/// An expected type, with the span that imposed it (for "expected because of this" labels).
#[derive(Clone)]
struct Expect {
    ty: Ty,
    origin: Option<Span>,
}

impl Expect {
    fn new(ty: Ty) -> Expect {
        Expect { ty, origin: None }
    }

    fn from(ty: Ty, origin: Option<Span>) -> Expect {
        Expect { ty, origin }
    }
}

enum Ret {
    Declared(Ty, Option<Span>),
    /// Return types seen so far, for inference.
    Infer(Vec<(Ty, Span)>),
}

struct LoopCtx {
    /// `loop { }` (can produce a value) as opposed to `while`/`for`.
    produces_value: bool,
    want: Option<Expect>,
    breaks: Vec<(Ty, Span)>,
}

struct FnCtx {
    id: FnId,
    ret: Ret,
    loops: Vec<LoopCtx>,
    /// A `readonly` method's `this` (immutable inside it).
    readonly_this: Option<LocalId>,
}

struct Checker<'p> {
    program: &'p Program,
    types: Types,
    diags: Vec<Diagnostic>,
    state: Vec<FnState>,
    global_known: Vec<bool>,
    stack: Vec<FnCtx>,
    /// The type arguments of the receiver whose field `field` looks up.
    recv_args: Vec<Ty>,
    /// A binary operator's left operand, already inferred while looking for a user operator.
    pre_inferred: Option<ExprId>,
    /// An expression already inferred that's checked again (an extension call's receiver).
    reuse: Option<ExprId>,
    /// The expression being checked is the function's result (where `tailcall` may be).
    tail_position: bool,
    /// The next block checked is a function body (its tail is the result).
    tail_body: bool,
    /// Lambdas' result types taken from the expected function type.
    ret_hints: HashMap<FnId, Ty>,
    /// Flow-sensitive narrowing: scopes mapping variables to the type they're known to have
    /// (`T` for a `T?` binding after `x != null`).
    narrow: Vec<HashMap<Var, Ty>>,
    /// `const`s initialized with a read-only `computed(getter)`: assigning them is E0357.
    readonly_cells: HashSet<Var>,
    /// Inside `unsafe { }` blocks or an `unsafe function` (spec §9.5) when non-zero.
    unsafe_depth: u32,
    /// The next function checked is a closure (it inherits the unsafe context).
    inherit_unsafe: bool,
    /// Explicit type arguments of the method call being checked (`b.map<string>(…)`).
    method_type_args: Vec<Ty>,
    /// Comptime thunks already checked (a local `comptime const` used by later comptime
    /// code refers to its thunk again).
    checked_thunks: HashSet<FnId>,
    /// The enclosing `arena { }` blocks: their function and own locals (`first..end`).
    arenas: Vec<(FnId, u32, u32)>,
}

/// Definite assignment of `this.field` in a constructor: the set of fields assigned on every
/// path, walking the body in order. `return`s (and the end) are where every field must be set.
struct Assigned<'a> {
    types: &'a Types,
    this: LocalId,
    own: Option<StructId>,
    all: &'a HashSet<u32>,
    exits: &'a mut Vec<(Span, HashSet<u32>)>,
}

impl Assigned<'_> {
    fn block(&mut self, b: &Block, mut s: HashSet<u32>) -> HashSet<u32> {
        for st in &b.stmts {
            s = match st {
                Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) => self.expr(e, s),
                Stmt::Let { init: None, .. } => s,
            };
        }
        match &b.tail {
            Some(t) => self.expr(t, s),
            None => s,
        }
    }

    fn expr(&mut self, e: &Expr, s: HashSet<u32>) -> HashSet<u32> {
        match &e.kind {
            ExprKind::Assign {
                target: Target::Field { id, object, .. },
                value,
                op,
            } if matches!(object.kind, ExprKind::Var(Var::Local(l)) if l == self.this) => {
                let mut s = self.expr(value, s);
                if op.is_none()
                    && let Some(i) = self.types.fields.get(id)
                {
                    s.insert(*i);
                }
                s
            }
            ExprKind::Block(b) => self.block(b, s),
            // `this(…)` runs a constructor of the same class, which assigns everything.
            ExprKind::CtorCall { class, args } if Some(*class) == self.own => {
                let mut s = s;
                for a in args {
                    s = self.expr(a, s);
                }
                let _ = s;
                self.all.clone()
            }
            ExprKind::If { cond, then, else_ } => {
                let s = self.expr(cond, s);
                let t = self.expr(then, s.clone());
                let e = match else_ {
                    Some(x) => self.expr(x, s),
                    None => s,
                };
                t.intersection(&e).copied().collect()
            }
            // A loop body may not run at all.
            ExprKind::While {
                cond,
                test_first: true,
                ..
            } => self.expr(cond, s),
            ExprKind::While { body, cond, .. } => {
                let s = self.expr(body, s);
                self.expr(cond, s)
            }
            ExprKind::ForOf { iter, .. } => self.expr(iter, s),
            ExprKind::Loop(b) => {
                // `loop` runs its body at least once; only `break` leaves it.
                let _ = self.block(b, s.clone());
                s
            }
            ExprKind::Return(v) => {
                let s = match v {
                    Some(v) => self.expr(v, s),
                    None => s,
                };
                if !s.is_superset(self.all) {
                    self.exits.push((e.span, s));
                }
                self.all.clone()
            }
            ExprKind::Match {
                scrutinee, arms, ..
            } => {
                let s = self.expr(scrutinee, s);
                let mut out: Option<HashSet<u32>> = None;
                for a in arms {
                    let r = self.expr(&a.body, s.clone());
                    out = Some(match out {
                        None => r,
                        Some(o) => o.intersection(&r).copied().collect(),
                    });
                }
                out.unwrap_or(s)
            }
            _ => {
                let mut s = Some(s);
                e.for_each_child(&mut |c| {
                    let cur = s.take().expect("state");
                    s = Some(self.expr(c, cur));
                });
                s.expect("state")
            }
        }
    }
}

/// Variables and the types they're known to have.
type Known = Vec<(Var, Ty)>;

/// What a condition tells about variables when it's true and when it's false.
#[derive(Default)]
struct Facts {
    when_true: Vec<(Var, Ty)>,
    when_false: Vec<(Var, Ty)>,
}

/// An integer literal without a suffix (possibly negated) adapts to its context.
fn untyped_lit(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(_, None) | ExprKind::Float(_, None) => true,
        ExprKind::Unary(UnOp::Neg | UnOp::Plus, inner) => untyped_lit(inner),
        _ => false,
    }
}

const NULL_HELP: &str = "the value may be null: check it first (`if (x != null) …`), give a default with `??`, or assert with `!`";

/// Expressions whose type comes from what's expected (literals, `[]`, `null`, `{ … }`):
/// generic inference looks at the other arguments first.
fn needs_context(e: &Expr) -> bool {
    untyped_lit(e)
        || matches!(&e.kind, ExprKind::Null | ExprKind::Object { ty: None, .. })
        || matches!(&e.kind, ExprKind::Array(xs) if xs.is_empty())
}

/// The constant a `comptime` argument stands for, as a literal type (or a `comptime`
/// parameter of the code around it).
fn const_of(e: &Expr) -> Option<Ty> {
    let lit = |v: LitVal| {
        let base = match v {
            LitVal::Int(_) => Ty::INT,
            LitVal::Str(_) => Ty::Str,
            LitVal::Bool(_) => Ty::Bool,
        };
        Some(Ty::Lits(Box::new(base), vec![v].into()))
    };
    match &e.kind {
        ExprKind::Int(v, _) => lit(LitVal::Int(*v as i128)),
        ExprKind::Unary(UnOp::Neg, inner) => match &inner.kind {
            ExprKind::Int(v, _) => lit(LitVal::Int(-(*v as i128))),
            _ => None,
        },
        ExprKind::Bool(b) => lit(LitVal::Bool(*b)),
        ExprKind::Str(s) => lit(LitVal::Str(s.as_str().into())),
        ExprKind::ConstParam(t) => Some(t.clone()),
        _ => None,
    }
}

/// Whether every type parameter of `owner` in `p` already has a value.
fn subst_complete(p: &Ty, owner: u32, subst: &[Option<Ty>]) -> bool {
    match p {
        Ty::Param(o, i, _) if *o == owner => subst[*i as usize].is_some(),
        Ty::Array(t) | Ty::Fixed(t, _) | Ty::Nullable(t) | Ty::Ref(t) => {
            subst_complete(t, owner, subst)
        }
        Ty::FixedN(t, n) => subst_complete(t, owner, subst) && subst_complete(n, owner, subst),
        Ty::Tuple(ts) => ts.iter().all(|t| subst_complete(t, owner, subst)),
        Ty::Fn(ps, r, ts) => {
            ps.iter()
                .chain(ts.iter())
                .all(|t| subst_complete(t, owner, subst))
                && subst_complete(r, owner, subst)
        }
        Ty::Struct(_, _, a) | Ty::Class(_, _, a) | Ty::Iface(_, _, a) => {
            a.iter().all(|t| subst_complete(t, owner, subst))
        }
        _ => true,
    }
}

fn is_float_lit(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Float(..) => true,
        ExprKind::Unary(UnOp::Neg | UnOp::Plus, inner) => is_float_lit(inner),
        _ => false,
    }
}

fn suffix_ty(s: NumSuffix) -> Ty {
    match s {
        NumSuffix::I8 => Ty::Int(IntTy::I8),
        NumSuffix::I16 => Ty::Int(IntTy::I16),
        NumSuffix::I32 => Ty::Int(IntTy::I32),
        NumSuffix::I64 => Ty::Int(IntTy::I64),
        NumSuffix::U8 => Ty::Int(IntTy::U8),
        NumSuffix::U16 => Ty::Int(IntTy::U16),
        NumSuffix::U32 => Ty::Int(IntTy::U32),
        NumSuffix::U64 => Ty::Int(IntTy::U64),
        NumSuffix::Isize => Ty::Int(IntTy::Isize),
        NumSuffix::Usize => Ty::Int(IntTy::Usize),
        NumSuffix::F32 => Ty::Float(FloatTy::F32),
        NumSuffix::F64 => Ty::Float(FloatTy::F64),
    }
}

/// Whether values of `t` can be read from and written as JSON (spec §11.4; `Err`: the first
/// type that can't). `seen` holds the structs and classes being checked (recursive types).
pub fn json_ok(p: &Program, t: &Ty, seen: &mut Vec<Ty>) -> Result<(), Ty> {
    match t {
        Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::Str | Ty::Error => Ok(()),
        Ty::Nullable(e) | Ty::Array(e) => json_ok(p, e, seen),
        Ty::Struct(..) | Ty::Class(..) if seen.contains(t) => Ok(()),
        Ty::Struct(sid, _, args) | Ty::Class(sid, _, args) => {
            seen.push(t.clone());
            let owner = Ty::type_owner(*sid);
            for f in &p.struct_def(*sid).fields {
                json_ok(p, &f.ty.subst(owner, args), seen)?;
            }
            Ok(())
        }
        other => Err(other.clone()),
    }
}

/// The error for `JSON.parse<t>`/`JSON.stringify` when `bad` (in `t`) has no JSON form.
pub fn json_error(t: &Ty, bad: &Ty, span: Span) -> Diagnostic {
    let what = if bad == t {
        format!("`{bad}`")
    } else {
        format!("`{t}` (it holds a `{bad}`)")
    };
    Diagnostic::error(
        codes::INVALID_OPERANDS,
        format!("{what} can't be converted to or from JSON"),
    )
    .primary(span, "no JSON form")
    .note("JSON supports numbers, `bool`, `char`, `string`, arrays, nullable values, structs and classes")
}

/// Whether an argument of type `t` can instantiate generic parameter type `p` by shape:
/// a bare type parameter takes anything; otherwise the type constructors must match.
fn shape_fits(p: &Ty, t: &Ty) -> bool {
    if !p.has_any_params() {
        return p == t || t.widens_to(p) || t.is_error();
    }
    match (p, t) {
        (Ty::Param(..), _) => true,
        (_, Ty::Error | Ty::Never) => true,
        (Ty::Array(a), Ty::Array(b)) => shape_fits(a, b),
        (Ty::Nullable(a), Ty::Nullable(b)) => shape_fits(a, b),
        (Ty::Nullable(a), b) => shape_fits(a, b),
        (Ty::Fixed(a, _), Ty::Fixed(b, _)) => shape_fits(a, b),
        (Ty::Tuple(a), Ty::Tuple(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| shape_fits(x, y))
        }
        (Ty::Class(s1, _, a1), Ty::Class(s2, _, a2))
        | (Ty::Struct(s1, _, a1), Ty::Struct(s2, _, a2)) => {
            s1 == s2
                && a1.len() == a2.len()
                && a1.iter().zip(a2.iter()).all(|(x, y)| shape_fits(x, y))
        }
        (Ty::Fn(p1, _, _), Ty::Fn(p2, _, _)) => p1.len() == p2.len(),
        // Other generic shapes (interfaces, maps…) are left to the full check.
        (Ty::Array(_) | Ty::Fixed(..) | Ty::Tuple(_) | Ty::Fn(..), _) => false,
        (Ty::Class(..) | Ty::Struct(..), Ty::Param(..)) => true,
        (Ty::Class(..) | Ty::Struct(..), _) => false,
        _ => true,
    }
}

/// Overload ranking: whether parameter type `x` is at least as specific as `y` (equal, a
/// widening, concrete over generic, or the same shape with more specific parts; a bare type
/// parameter is the most general).
fn at_least_as_specific(x: &Ty, y: &Ty) -> bool {
    if x == y || x.widens_to(y) {
        return true;
    }
    match (x, y) {
        (_, Ty::Param(..)) => true,
        (Ty::Param(..), _) => false,
        _ if y.has_any_params() && !x.has_any_params() => true,
        (Ty::Array(a), Ty::Array(b)) | (Ty::Nullable(a), Ty::Nullable(b)) => {
            at_least_as_specific(a, b)
        }
        (Ty::Class(s1, _, a1), Ty::Class(s2, _, a2))
        | (Ty::Struct(s1, _, a1), Ty::Struct(s2, _, a2)) => {
            s1 == s2
                && a1
                    .iter()
                    .zip(a2.iter())
                    .all(|(p, q)| at_least_as_specific(p, q))
        }
        (Ty::Tuple(a), Ty::Tuple(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(p, q)| at_least_as_specific(p, q))
        }
        _ => false,
    }
}

fn is_printable(t: &Ty) -> bool {
    match t {
        Ty::Nullable(inner)
        | Ty::Lits(inner, _)
        | Ty::Distinct(_, _, inner)
        | Ty::Immutable(inner) => is_printable(inner),
        Ty::Union(_, ms) => ms.iter().all(is_printable),
        _ => matches!(
            t,
            Ty::Int(_)
                | Ty::Float(_)
                | Ty::Bool
                | Ty::Char
                | Ty::Str
                | Ty::Error
                | Ty::Never
                // An instance shows its value (compound ones in the debug format).
                | Ty::Param(..)
                // Its `toString()`, or `Name (field = value, …)` (spec §6.1).
                | Ty::Class(..)
        ),
    }
}

impl<'p> Checker<'p> {
    fn ctx(&mut self) -> &mut FnCtx {
        self.stack.last_mut().expect("inside a function")
    }

    fn func(&self) -> &'p Function {
        self.program
            .func(self.stack.last().expect("inside a function").id)
    }

    fn set(&mut self, e: &Expr, t: Ty) -> Ty {
        self.types.exprs[e.id.0 as usize] = t.clone();
        t
    }

    fn mismatch(&mut self, span: Span, want: &Expect, found: &Ty) {
        if want.ty.is_error() || found.is_error() {
            return;
        }
        let mut d = Diagnostic::error(codes::MISMATCHED_TYPES, "mismatched types")
            .primary(span, format!("expected `{}`, found `{found}`", want.ty));
        if let Some(o) = want.origin {
            d = d.secondary(o, "expected because of this");
        }
        if want.ty.is_numeric() && found.is_numeric() {
            d = d.help(format!(
                "`{found}` doesn't convert to `{}` without possible loss; convert explicitly with `as {}`",
                want.ty, want.ty
            ));
        }
        if let Ty::Nullable(inner) = found
            && inner.widens_to(&want.ty)
        {
            d = d.help(NULL_HELP);
        }
        self.diags.push(d);
    }

    /// Whether a `from` reference converts to `to` as an upcast (a subclass to a base class,
    /// possibly into a nullable).
    fn upcasts(&self, from: &Ty, to: &Ty) -> bool {
        match (from, to) {
            // A function throwing less (or subclasses) fits where one throwing more is expected.
            // So does one returning a subclass (same representation: a reference), such as
            // `Promise<T, never>` where `Promise<T, Error>` is expected.
            (Ty::Fn(p1, r1, t1), Ty::Fn(p2, r2, t2))
                if p1 == p2 && (r1 == r2 || self.upcasts(r1, r2)) =>
            {
                t1.iter().all(|t| {
                    t2.iter().any(|d| match (t, d) {
                        (Ty::Class(a, ..), Ty::Class(b, ..)) => self.program.is_subclass(*a, *b),
                        (t, d) => t == d,
                    })
                })
            }
            (Ty::Class(a, _, _), Ty::Class(b, _, _)) => self.program.is_subclass(*a, *b),
            // To an interface the type implements (nominally).
            (Ty::Class(a, _, _) | Ty::Struct(a, _, _), Ty::Iface(i, _, _)) => {
                self.program.implements(*a, *i)
            }
            // Interface inheritance: only along first parents, whose methods are a prefix.
            (Ty::Iface(a, _, _), Ty::Iface(b, _, _)) => self.iface_prefix(*a, *b),
            (Ty::Class(..) | Ty::Struct(..) | Ty::Iface(..), Ty::Nullable(t)) => {
                self.upcasts(from, t)
            }
            (Ty::Nullable(a), Ty::Nullable(b)) => self.upcasts(a, b),
            _ => false,
        }
    }

    /// Whether interface `b` is `a` or reached from it through first parents.
    fn iface_prefix(&self, a: StructId, b: StructId) -> bool {
        a == b
            || self
                .program
                .struct_def(a)
                .implements
                .first()
                .is_some_and(|p| self.iface_prefix(*p, b))
    }

    /// Accepts `found` where `want` is expected, recording a widening if needed.
    fn coerce(&mut self, e: &Expr, found: Ty, want: &Expect) -> Ty {
        if found == want.ty || found == Ty::Never || found.is_error() || want.ty.is_error() {
            return want.ty.clone();
        }
        let into_union = match &want.ty {
            Ty::Union(_, ms) => self.union_member(&found, ms).is_some(),
            Ty::Nullable(inner) => match &**inner {
                Ty::Union(_, ms) => self.union_member(&found, ms).is_some(),
                _ => false,
            },
            _ => false,
        };
        if found.widens_to(&want.ty) || self.upcasts(&found, &want.ty) || into_union {
            self.types.coercions.insert(e.id, want.ty.clone());
            return want.ty.clone();
        }
        // A cell where its value is expected: a read (then the value's own widening).
        if let Some(inner) = self.reactive_arg(&found, e.span)
            && self.reactive_arg(&want.ty, e.span).is_none()
            && (inner == want.ty || inner.widens_to(&want.ty) || self.upcasts(&inner, &want.ty))
        {
            self.types.uses_reactivity = true;
            self.types.coercions.insert(e.id, want.ty.clone());
            return want.ty.clone();
        }
        self.mismatch(e.span, want, &found);
        Ty::Error
    }

    fn check(&mut self, e: &Expr, want: &Expect) -> Ty {
        let found = self.infer_with(e, Some(want));
        self.coerce(e, found, want)
    }

    fn infer(&mut self, e: &Expr) -> Ty {
        self.infer_with(e, None)
    }

    // ------------------------------------------------------------ narrowing

    fn narrowed(&self, v: Var) -> Option<&Ty> {
        self.narrow.iter().rev().find_map(|s| s.get(&v))
    }

    /// Runs `f` with `facts` known.
    fn with_facts<T>(&mut self, facts: Vec<(Var, Ty)>, f: impl FnOnce(&mut Self) -> T) -> T {
        self.narrow.push(facts.into_iter().collect());
        let r = f(self);
        self.narrow.pop();
        r
    }

    /// An assignment ends every narrowing of `v`.
    fn forget(&mut self, v: Var) {
        for s in &mut self.narrow {
            s.remove(&v);
        }
    }

    /// Before a loop: narrowings of variables the loop assigns don't hold on later iterations.
    fn forget_assigned(&mut self, exprs: &[&Expr]) {
        let mut vars = Vec::new();
        for e in exprs {
            e.assigned_vars(&mut vars);
        }
        for v in vars {
            self.forget(v);
        }
    }

    /// What `typeof v == lit` / `v.kind == lit` says about `v`: when true, and when false
    /// (only if a single other case remains).
    fn tag_facts(&self, probe: &Expr, lit: &str) -> (Known, Known) {
        let none = (Vec::new(), Vec::new());
        match &probe.kind {
            ExprKind::TypeOf(inner) => {
                let ExprKind::Var(v) = inner.kind else {
                    return none;
                };
                let Ty::Union(_, ms) = self.types.expr(inner.id) else {
                    return none;
                };
                let Some(i) = ms.iter().position(|m| m.erase().to_string() == lit) else {
                    return none;
                };
                let rest: Vec<&Ty> = ms
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, t)| t)
                    .collect();
                let no = match rest.as_slice() {
                    [one] => vec![(v, (*one).clone())],
                    _ => Vec::new(),
                };
                (vec![(v, ms[i].clone())], no)
            }
            ExprKind::Member { object, name, .. } => {
                let ExprKind::Var(v) = object.kind else {
                    return none;
                };
                let Ty::Enum(eid, ename) = self.types.expr(object.id) else {
                    return none;
                };
                let def = self.program.enum_def(*eid);
                if !matches!(&def.origin, crate::hir::EnumOrigin::Discriminated(d) if d == name) {
                    return none;
                }
                let Some((vi, _)) = def.variant(lit) else {
                    return none;
                };
                let variant = |i: u32| (v, Ty::Variant(*eid, i, ename.clone()));
                let no = if def.variants.len() == 2 {
                    vec![variant(1 - vi)]
                } else {
                    Vec::new()
                };
                (vec![variant(vi)], no)
            }
            _ => none,
        }
    }

    /// The type of a simple expression without checking it (variables and fields), to pick
    /// a code path before checking.
    fn peek_type(&self, e: &Expr) -> Option<Ty> {
        match &e.kind {
            ExprKind::Var(Var::Local(l)) => {
                let f = self.stack.last()?.id;
                Some(self.types.locals[f.0 as usize][l.0 as usize].clone())
            }
            ExprKind::Var(Var::Global(g)) => Some(self.types.globals[g.0 as usize].clone()),
            _ => None,
        }
    }

    /// The facts of an already checked condition.
    fn facts(&self, cond: &Expr) -> Facts {
        match &cond.kind {
            ExprKind::Unary(UnOp::Not, inner) => {
                let f = self.facts(inner);
                Facts {
                    when_true: f.when_false,
                    when_false: f.when_true,
                }
            }
            ExprKind::Binary(op @ (BinOp::Eq | BinOp::Ne), l, r)
                if matches!(
                    (&l.kind, &r.kind),
                    (ExprKind::Var(_), ExprKind::Null) | (ExprKind::Null, ExprKind::Var(_))
                ) =>
            {
                let var = match (&l.kind, &r.kind) {
                    (ExprKind::Var(v), ExprKind::Null) => Some((*v, l)),
                    (ExprKind::Null, ExprKind::Var(v)) => Some((*v, r)),
                    _ => None,
                };
                let Some((v, ve)) = var else {
                    return Facts::default();
                };
                let Ty::Nullable(inner) = self.types.expr(ve.id) else {
                    return Facts::default();
                };
                let fact = vec![(v, (**inner).clone())];
                if *op == BinOp::Ne {
                    Facts {
                        when_true: fact,
                        when_false: Vec::new(),
                    }
                } else {
                    Facts {
                        when_true: Vec::new(),
                        when_false: fact,
                    }
                }
            }
            // `typeof x == "string"` on a union; `e.kind == "click"` on a discriminated union.
            ExprKind::Binary(op @ (BinOp::Eq | BinOp::Ne), l, r)
                if matches!(
                    (&l.kind, &r.kind),
                    (_, ExprKind::Str(_)) | (ExprKind::Str(_), _)
                ) =>
            {
                let (probe, lit) = match (&l.kind, &r.kind) {
                    (_, ExprKind::Str(s)) => (l, s),
                    (ExprKind::Str(s), _) => (r, s),
                    _ => unreachable!(),
                };
                let (yes, no) = self.tag_facts(probe, lit);
                if *op == BinOp::Eq {
                    Facts {
                        when_true: yes,
                        when_false: no,
                    }
                } else {
                    Facts {
                        when_true: no,
                        when_false: yes,
                    }
                }
            }
            // `x instanceof Dog` narrows `x` to `Dog`.
            ExprKind::InstanceOf { expr, class } => match expr.kind {
                ExprKind::Var(v) => Facts {
                    when_true: vec![(v, self.program.struct_ty(*class))],
                    when_false: Vec::new(),
                },
                _ => Facts::default(),
            },
            // `x is Circle { .. }` on a `Shape?` says `x` isn't null; `x is null` the opposite.
            ExprKind::Is { expr, pat } => {
                let ExprKind::Var(v) = expr.kind else {
                    return Facts::default();
                };
                let vt = self.types.expr(expr.id).clone();
                let Ty::Nullable(inner) = &vt else {
                    return Facts::default();
                };
                let fact = vec![(v, (**inner).clone())];
                if pat.looks_inside(&vt) {
                    Facts {
                        when_true: fact,
                        when_false: Vec::new(),
                    }
                } else if matches!(pat.kind, PatKind::Lit(LitPat::Null)) {
                    Facts {
                        when_true: Vec::new(),
                        when_false: fact,
                    }
                } else {
                    Facts::default()
                }
            }
            ExprKind::Binary(BinOp::And, l, r) => {
                let (a, b) = (self.facts(l), self.facts(r));
                Facts {
                    when_true: a.when_true.into_iter().chain(b.when_true).collect(),
                    when_false: Vec::new(),
                }
            }
            ExprKind::Binary(BinOp::Or, l, r) => {
                let (a, b) = (self.facts(l), self.facts(r));
                Facts {
                    when_true: Vec::new(),
                    when_false: a.when_false.into_iter().chain(b.when_false).collect(),
                }
            }
            _ => Facts::default(),
        }
    }

    // ------------------------------------------------------------ functions

    fn check_fn(&mut self, id: FnId) {
        let saved = self.unsafe_depth;
        let inherit = std::mem::take(&mut self.inherit_unsafe);
        self.unsafe_depth = if self.program.func(id).is_unsafe {
            1
        } else if inherit {
            saved
        } else {
            0
        };
        self.check_fn_body(id);
        self.unsafe_depth = saved;
    }

    /// Whether values of `t` may hold references (what the collector traces).
    fn holds_refs(&self, t: &Ty) -> bool {
        self.program.holds_refs(t)
    }

    /// Whether values of `t` can be sent to a worker (spec §10.3): primitives, strings and
    /// what's built from Sendable parts (structs, enums, tuples, arrays, classes by their
    /// fields, so `Map`/`Set` too); not functions, interfaces, pointers or type parameters
    /// without the bound.
    fn sendable(&self, t: &Ty, seen: &mut Vec<StructId>) -> bool {
        match t {
            Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::Str | Ty::Void | Ty::Null => true,
            Ty::Never | Ty::Error | Ty::Range(_) => true,
            Ty::Array(e) | Ty::Fixed(e, _) | Ty::FixedN(e, _) | Ty::Nullable(e) => {
                self.sendable(e, seen)
            }
            Ty::Tuple(ts) => ts.iter().all(|t| self.sendable(t, seen)),
            Ty::Struct(sid, _, args) | Ty::Class(sid, _, args) => {
                if seen.contains(sid) {
                    return true;
                }
                seen.push(*sid);
                let def = self.program.struct_def(*sid);
                let owner = Ty::type_owner(*sid);
                def.fields
                    .iter()
                    .all(|f| f.weak || self.sendable(&f.ty.subst(owner, args), seen))
            }
            Ty::Enum(eid, _) | Ty::Union(eid, _) | Ty::Variant(eid, _, _) => self.program.enums
                [eid.0 as usize]
                .variants
                .iter()
                .all(|v| v.fields.iter().all(|f| self.sendable(&f.ty, seen))),
            Ty::Lits(b, _) | Ty::Distinct(_, _, b) | Ty::Immutable(b) => self.sendable(b, seen),
            Ty::Param(..) => self.bounds_of(t).contains(&Bound::Sendable),
            Ty::Fn(..) | Ty::Iface(..) | Ty::Ref(_) => false,
        }
    }

    /// Whether `v` lives outside the innermost enclosing `arena` block of this function
    /// (globals always do; inside a lambda nothing is known).
    fn outside_arena(&self, v: Var) -> bool {
        let f = self.stack.last().expect("inside a function").id;
        match v {
            Var::Global(_) => true,
            Var::Local(l) => self
                .arenas
                .iter()
                .rev()
                .find(|a| a.0 == f)
                .is_some_and(|a| l.0 < a.1 || l.0 >= a.2),
        }
    }

    fn arena_escape(&mut self, span: Span, what: &str) {
        // (A function's result may be checked twice.)
        if self.diags.iter().any(|d| {
            d.code == Some(codes::ARENA_ESCAPE)
                && d.labels.iter().any(|l| l.primary && l.span == span)
        }) {
            return;
        }
        self.diags.push(
            Diagnostic::error(codes::ARENA_ESCAPE, "an arena reference escapes the block")
                .primary(span, what.to_string())
                .note("only values without references (numbers, plain structs, …) can leave an `arena` block"),
        );
    }

    /// An unsafe operation (spec §9.5) outside `unsafe` code is an error.
    fn require_unsafe(&mut self, span: Span, what: &str) {
        if self.unsafe_depth == 0 {
            self.diags.push(
                Diagnostic::error(codes::UNSAFE_NEEDED, format!("{what} needs `unsafe`"))
                    .primary(span, "outside an `unsafe` block or function")
                    .help("wrap it in `unsafe { … }`, or declare the function `unsafe`"),
            );
        }
    }

    fn check_fn_body(&mut self, id: FnId) {
        match self.state[id.0 as usize] {
            FnState::Done => return,
            FnState::InProgress => return,
            FnState::Unchecked => {}
        }
        self.state[id.0 as usize] = FnState::InProgress;
        let f = self.program.func(id);
        for (i, l) in f.locals.iter().enumerate() {
            if let Some(t) = &l.ty {
                self.types.locals[id.0 as usize][i] = t.clone();
            }
        }
        // A generator's step function returns through `yield`; `return` ends it.
        let hint = self.ret_hints.get(&id).cloned();
        let declared = f.ret.clone().or(hint);
        let ret = match &declared {
            _ if f.generator => Ret::Declared(Ty::Void, None),
            Some(t) => Ret::Declared(t.clone(), f.ret_span),
            None => Ret::Infer(Vec::new()),
        };
        let readonly_this = f.this.and_then(|(this, sid)| {
            let short = f.name.rsplit('.').next().unwrap_or(&f.name);
            self.program
                .struct_def(sid)
                .readonly_methods
                .contains(short)
                .then_some(this)
        });
        self.stack.push(FnCtx {
            id,
            ret,
            loops: Vec::new(),
            readonly_this,
        });
        let body_ty = match (&declared, f.is_entry) {
            _ if f.generator => self.block(&f.body, None),
            (_, true) | (Some(Ty::Void), _) => {
                // The value of the last expression is discarded.
                self.check_defaults(f);
                self.block(&f.body, None)
            }
            (Some(t), false) => {
                let want = Expect::from(t.clone(), f.ret_span);
                self.check_defaults(f);
                self.tail_body = true;
                let bt = self.block(&f.body, Some(&want));
                if f.body.tail.is_none() && bt != Ty::Never && *t != Ty::Void && !t.is_error() {
                    self.diags.push(
                        Diagnostic::error(codes::MISMATCHED_TYPES, "mismatched types")
                            .primary(
                                f.body.span,
                                format!("expected `{t}`, but the body ends without a value"),
                            )
                            .secondary(
                                f.ret_span.unwrap_or(f.name_span),
                                "the return type is declared here",
                            )
                            .help("make the last expression the result, or add a `return`"),
                    );
                }
                bt
            }
            (None, false) => {
                self.check_defaults(f);
                self.tail_body = true;
                self.block(&f.body, None)
            }
        };
        if let Some((this, sid)) = f.this
            && f.name.ends_with(".constructor")
            && self.program.struct_def(sid).is_class
        {
            self.definite_assignment(f, this, sid);
        }
        let ctx = self.stack.pop().unwrap();
        if let Ret::Infer(mut rets) = ctx.ret {
            rets.push((
                body_ty,
                f.body.tail.as_ref().map_or(f.body.span, |t| t.span),
            ));
            let ret = self.unify_all(&rets, "the function returns values of different types");
            self.types.sigs[id.0 as usize].ret = ret;
        }
        self.state[id.0 as usize] = FnState::Done;
        if f.infer_throws {
            // What it throws: what its body lets escape (callees with inferred clauses first).
            for c in crate::throws::callees(self.program, &self.types, id) {
                self.check_fn(c);
            }
            self.types.fn_throws[id.0 as usize] =
                crate::throws::uncaught(self.program, &self.types, id);
        }
    }

    /// An override (or an interface method's implementation) must take and return the same
    /// types as the method it overrides.
    fn check_overrides(&mut self) {
        let mut pairs = Vec::new();
        for (i, def) in self.program.structs.iter().enumerate() {
            if let Some(base) = def.base {
                let bdef = self.program.struct_def(base);
                for (slot, f) in def.vtable.iter().enumerate() {
                    if let Some(bf) = bdef.vtable.get(slot)
                        && f != bf
                    {
                        pairs.push((*f, *bf));
                    }
                }
            }
            if def.is_interface {
                continue;
            }
            for (j, idef) in self.program.structs.iter().enumerate() {
                if !idef.is_interface
                    || !self
                        .program
                        .implements(StructId(i as u32), StructId(j as u32))
                {
                    continue;
                }
                for (name, ifn) in &idef.methods {
                    if let Some(f) = def.method(name) {
                        pairs.push((f, *ifn));
                    }
                }
            }
        }
        for (f, bf) in pairs {
            let (a, b) = (
                &self.types.sigs[f.0 as usize],
                &self.types.sigs[bf.0 as usize],
            );
            if a.params != b.params || (a.ret != b.ret && !a.ret.is_error() && !b.ret.is_error()) {
                let fun = self.program.func(f);
                let bfun = self.program.func(bf);
                let show = |s: &FnSig| {
                    let ps: Vec<String> = s.params.iter().map(|p| p.to_string()).collect();
                    format!("({}): {}", ps.join(", "), s.ret)
                };
                self.diags.push(
                    Diagnostic::error(
                        codes::OVERRIDE_SIGNATURE,
                        format!("`{}` doesn't match the method it implements", fun.name),
                    )
                    .primary(fun.name_span, format!("has type `{}`", show(a)))
                    .secondary(bfun.name_span, format!("`{}` is `{}`", bfun.name, show(b))),
                );
            }
        }
    }

    /// Parameter defaults, checked against the parameter types (an untyped parameter with a
    /// default takes the default's type).
    fn check_defaults(&mut self, f: &Function) {
        let id = self.stack.last().expect("inside a function").id;
        for (k, d) in f.defaults.iter().enumerate() {
            let Some(d) = d else { continue };
            let p = f.params[k];
            match &f.local(p).ty {
                Some(t) => {
                    self.check(d, &Expect::from(t.clone(), Some(f.local(p).span)));
                }
                None => {
                    let t = self.infer(d);
                    self.types.locals[id.0 as usize][p.0 as usize] = t.clone();
                    self.types.sigs[id.0 as usize].params[k] = t;
                }
            }
        }
    }

    /// Every field must be assigned on every path through a constructor (spec §6.1).
    fn definite_assignment(&mut self, f: &Function, this: LocalId, sid: StructId) {
        let def = self.program.struct_def(sid);
        let all: HashSet<u32> = (0..def.fields.len() as u32).collect();
        let pre: HashSet<u32> = def
            .fields
            .iter()
            .enumerate()
            .filter(|(_, fd)| fd.preassigned || fd.ty.is_error())
            .map(|(i, _)| i as u32)
            .collect();
        let mut missing_at: Vec<(Span, HashSet<u32>)> = Vec::new();
        let mut walk = Assigned {
            types: &self.types,
            this,
            own: Some(sid),
            all: &all,
            exits: &mut missing_at,
        };
        let end = walk.block(&f.body, pre);
        if !end.is_superset(&all) {
            missing_at.push((f.body.span.shrink_to_hi(), end));
        }
        for (span, have) in missing_at {
            let mut names: Vec<&str> = def
                .fields
                .iter()
                .enumerate()
                .filter(|(i, _)| !have.contains(&(*i as u32)))
                .map(|(_, fd)| fd.name.as_str())
                .collect();
            names.dedup();
            if names.is_empty() {
                continue;
            }
            let list: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
            let first = def.field(names[0]).map(|(_, fd)| fd.span);
            let mut d = Diagnostic::error(
                codes::UNASSIGNED_FIELD,
                format!(
                    "field{} {} not assigned by the constructor",
                    if names.len() == 1 { "" } else { "s" },
                    list.join(", ")
                ),
            )
            .primary(span, "the constructor can finish here");
            if let Some(fs) = first {
                d = d.secondary(fs, "declared here");
            }
            d = d.help("assign every field in each constructor, or give it an initializer");
            self.diags.push(d);
        }
    }

    /// Joins several types: identical, or all widening to one of them. `never` disappears.
    fn unify_all(&mut self, tys: &[(Ty, Span)], msg: &str) -> Ty {
        let live: Vec<&(Ty, Span)> = tys.iter().filter(|(t, _)| *t != Ty::Never).collect();
        let Some(first) = live.first() else {
            return Ty::Never;
        };
        let mut acc = first.0.clone();
        for (t, _) in &live[1..] {
            if t.widens_to(&acc) {
                continue;
            }
            if acc.widens_to(t) {
                acc = t.clone();
                continue;
            }
            if t.is_error() || acc.is_error() {
                return Ty::Error;
            }
            // `T` and `null` join to `T?`.
            if *t == Ty::Null && acc != Ty::Void {
                acc = acc.nullable();
                continue;
            }
            if acc == Ty::Null && *t != Ty::Void {
                acc = t.clone().nullable();
                continue;
            }
            let mut d = Diagnostic::error(codes::MISMATCHED_TYPES, msg.to_string());
            for (t, s) in &live {
                d = d.secondary(*s, format!("`{t}`"));
            }
            d.labels[0].primary = true;
            self.diags.push(d);
            return Ty::Error;
        }
        acc
    }

    // ------------------------------------------------------------ blocks and statements

    fn block(&mut self, b: &Block, want: Option<&Expect>) -> Ty {
        // Only the function body's own tail is in tail position.
        let tail_body = std::mem::take(&mut self.tail_body);
        self.narrow.push(HashMap::new());
        let mut diverges = false;
        for s in &b.stmts {
            match s {
                Stmt::Let { var, init, span } => self.let_stmt(*var, init.as_ref(), *span),
                Stmt::Expr(e) => {
                    // A `try` statement's value is unused, so its branches needn't agree.
                    let t = if matches!(e.kind, ExprKind::Try { .. }) {
                        self.check(e, &Expect::new(Ty::Void))
                    } else {
                        self.infer(e)
                    };
                    if t == Ty::Never {
                        diverges = true;
                    }
                    self.early_exit_facts(e);
                }
            }
        }
        let t = match &b.tail {
            Some(t) => {
                self.tail_position = tail_body && matches!(t.kind, ExprKind::Tailcall(_));
                match want {
                    Some(w) => self.check(t, w),
                    None => self.infer(t),
                }
            }
            None if diverges => Ty::Never,
            None => Ty::Void,
        };
        self.narrow.pop();
        t
    }

    /// `if (x == null) return` narrows `x` for the rest of the block (and `if (c) … else
    /// return` keeps what `c` says).
    fn early_exit_facts(&mut self, e: &Expr) {
        let ExprKind::If { cond, then, else_ } = &e.kind else {
            return;
        };
        let then_exits = *self.types.expr(then.id) == Ty::Never;
        let else_exits = else_
            .as_ref()
            .is_some_and(|x| *self.types.expr(x.id) == Ty::Never);
        let facts = self.facts(cond);
        let known = match (then_exits, else_exits) {
            (true, false) => facts.when_false,
            (false, true) => facts.when_true,
            _ => return,
        };
        let scope = self.narrow.last_mut().expect("block scope");
        scope.extend(known);
    }

    fn var_binding(&self, var: Var) -> &'p Binding {
        match var {
            Var::Local(l) => self.func().local(l),
            Var::Global(g) => self.program.global(g),
        }
    }

    fn var_ty(&mut self, var: Var, span: Span) -> Ty {
        match var {
            Var::Local(l) => {
                let f = self.stack.last().unwrap().id;
                self.types.locals[f.0 as usize][l.0 as usize].clone()
            }
            Var::Global(g) => {
                if !self.global_known[g.0 as usize] {
                    let b = self.program.global(g);
                    self.diags.push(
                        Diagnostic::error(
                            codes::MISMATCHED_TYPES,
                            format!("the type of `{}` isn't known here yet", b.name),
                        )
                        .primary(
                            span,
                            "used before the top-level code reaches its declaration",
                        )
                        .secondary(b.span, "declared here")
                        .help(format!("add a type annotation: `{}: T`", b.name)),
                    );
                    self.global_known[g.0 as usize] = true;
                }
                self.types.globals[g.0 as usize].clone()
            }
        }
    }

    fn set_var_ty(&mut self, var: Var, t: Ty) {
        match var {
            Var::Local(l) => {
                let f = self.stack.last().unwrap().id;
                self.types.locals[f.0 as usize][l.0 as usize] = t;
            }
            Var::Global(g) => {
                self.types.globals[g.0 as usize] = t;
                self.global_known[g.0 as usize] = true;
            }
        }
    }

    fn let_stmt(&mut self, var: Var, init: Option<&Expr>, span: Span) {
        let b = self.var_binding(var);
        let t = match (&b.ty, init) {
            (Some(ann), Some(init)) => {
                let origin = Some(b.span);
                self.check(init, &Expect::from(ann.clone(), origin));
                ann.clone()
            }
            (Some(ann), None) => ann.clone(),
            (None, Some(init)) => {
                let t = self.infer(init);
                match t {
                    Ty::Null => {
                        self.diags.push(
                            Diagnostic::error(
                                codes::MISMATCHED_TYPES,
                                "can't infer a type from `null`",
                            )
                            .primary(init.span, "only `null`")
                            .help(format!("declare the type: `{}: T? = null`", b.name)),
                        );
                        Ty::Error
                    }
                    Ty::Void => {
                        self.diags.push(
                            Diagnostic::error(
                                codes::MISMATCHED_TYPES,
                                "this expression has no value",
                            )
                            .primary(init.span, "has type `void`"),
                        );
                        Ty::Error
                    }
                    t => t,
                }
            }
            (None, None) => {
                self.diags.push(
                    Diagnostic::error(
                        codes::MISMATCHED_TYPES,
                        format!("`{}` needs a type or a value", b.name),
                    )
                    .primary(span, "neither a type annotation nor an initializer")
                    .help(format!("write `{}: T` or `{} = value`", b.name, b.name)),
                );
                Ty::Error
            }
        };
        if self.reactive_arg(&t, span).is_some() {
            if b.mutable {
                self.diags.push(
                    Diagnostic::error(codes::REACTIVE, format!("a `let` can't hold a `{t}`"))
                        .primary(b.span, "declared with `let`")
                        .help("declare it with `const`: assigning it writes the cell (spec §16.2)"),
                );
            }
            // `const c = computed(getter)`: assigning `c` would panic, so it's an error.
            if let Some(init) = init
                && let ExprKind::Call { callee, .. } = &init.kind
            {
                let f = match &callee.kind {
                    ExprKind::Fn(id) => Some(*id),
                    _ => self.types.overloads.get(&init.id).copied(),
                };
                if f.is_some() && f == self.program.readonly_computed {
                    self.readonly_cells.insert(var);
                }
            }
        }
        self.set_var_ty(var, t);
    }

    // ------------------------------------------------------------ expressions

    fn infer_with(&mut self, e: &Expr, want: Option<&Expect>) -> Ty {
        if self.reuse == Some(e.id) {
            self.reuse = None;
            let t = self.types.exprs[e.id.0 as usize].clone();
            return self.read_cell(e, t, want);
        }
        let t = self.expr_kind(e, want);
        // What's read through an `immutable` view is immutable too (deeply).
        let t = match &e.kind {
            ExprKind::Member { object, .. } | ExprKind::Index { object, .. }
                if self.through_immutable(object) =>
            {
                self.immutable(t)
            }
            _ => t,
        };
        let t = self.set(e, t);
        self.read_cell(e, t, want)
    }

    /// `T` for a `Reactive<T>` (spec §16), outside the core library (where it's an ordinary
    /// class).
    fn reactive_arg(&self, t: &Ty, span: Span) -> Option<Ty> {
        match t {
            Ty::Class(sid, _, args)
                if Some(*sid) == self.program.reactive_class
                    && !self.program.prelude_files.contains(&span.file) =>
            {
                args.first().cloned()
            }
            _ => None,
        }
    }

    /// Naming a reactive binding (a variable or a field holding a `Reactive<T>`) reads its
    /// value, unless a `Reactive<T>` is expected (spec §16.2): lowering turns it into a
    /// tracked `__get()`.
    fn read_cell(&mut self, e: &Expr, t: Ty, want: Option<&Expect>) -> Ty {
        if !matches!(e.kind, ExprKind::Var(_) | ExprKind::Member { .. }) {
            return t;
        }
        let Some(inner) = self.reactive_arg(&t, e.span) else {
            return t;
        };
        if want.is_some_and(|w| self.reactive_arg(&w.ty, e.span).is_some()) {
            return t;
        }
        self.types.cell_reads.insert(e.id, t);
        self.types.uses_reactivity = true;
        self.types.exprs[e.id.0 as usize] = inner.clone();
        inner
    }

    /// `immutable t`, or just `t` when it holds no references (a copy can't reach anything).
    fn immutable(&self, t: Ty) -> Ty {
        match t {
            Ty::Immutable(_) | Ty::Error => t,
            t if self.pure_value(&t) => t,
            t => Ty::Immutable(Box::new(t)),
        }
    }

    /// Whether values of `t` contain no references (copies are fully independent).
    fn pure_value(&self, t: &Ty) -> bool {
        match t {
            Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::Str | Ty::Null | Ty::Range(_) => {
                true
            }
            Ty::Lits(b, _) | Ty::Distinct(_, _, b) | Ty::Nullable(b) | Ty::Fixed(b, _) => {
                self.pure_value(b)
            }
            Ty::Tuple(ts) => ts.iter().all(|t| self.pure_value(t)),
            Ty::Struct(sid, _, args) => self
                .program
                .struct_def(*sid)
                .fields
                .iter()
                .all(|f| self.pure_value(&f.ty.subst(Ty::type_owner(*sid), args))),
            Ty::Enum(eid, _) => self
                .program
                .enum_def(*eid)
                .variants
                .iter()
                .all(|v| v.fields.iter().all(|f| self.pure_value(&f.ty))),
            Ty::Union(_, ms) => ms.iter().all(|m| self.pure_value(m)),
            _ => false,
        }
    }

    /// Whether `e` reaches its value through an `immutable` view (or the `this` of a
    /// `readonly` method), so it mustn't be mutated.
    fn through_immutable(&self, e: &Expr) -> bool {
        if matches!(self.types.expr(e.id), Ty::Immutable(_)) {
            return true;
        }
        match &e.kind {
            ExprKind::Var(Var::Local(l)) => self
                .stack
                .last()
                .is_some_and(|c| c.readonly_this == Some(*l)),
            ExprKind::Member { object, .. } | ExprKind::Index { object, .. } => {
                self.through_immutable(object)
            }
            _ => false,
        }
    }

    /// A `readonly` field can only be assigned by its type's constructors.
    fn check_readonly_field(&mut self, ot: &Ty, name: &str, span: Span) {
        let (Ty::Struct(sid, _, _) | Ty::Class(sid, _, _)) = ot else {
            return;
        };
        let def = self.program.struct_def(*sid);
        let Some((_, fd)) = def.field(name) else {
            return;
        };
        if !fd.readonly {
            return;
        }
        let f = self.func();
        let in_ctor = f.name.ends_with(".constructor") && f.owner == Some(*sid);
        if !in_ctor {
            self.diags.push(
                Diagnostic::error(codes::READONLY_FIELD, format!("`{name}` is `readonly`"))
                    .primary(span, "assigned outside a constructor")
                    .secondary(fd.span, "declared `readonly` here"),
            );
        }
    }

    fn immutable_mutation(&mut self, span: Span, what: &str) {
        self.diags.push(
            Diagnostic::error(codes::IMMUTABLE_MUTATION, format!("can't {what} through an `immutable` view"))
                .primary(span, "the value is immutable here")
                .note("`immutable T` is deep: nothing reachable through it can change, and only `readonly` methods can be called"),
        );
    }

    fn expr_kind(&mut self, e: &Expr, want: Option<&Expect>) -> Ty {
        // A literal checked against literal types or a union (`T?` of one too) picks the
        // member it fits.
        fn lit_target(t: &Ty) -> bool {
            match t {
                Ty::Lits(..) | Ty::Union(..) => true,
                Ty::Nullable(inner) => matches!(**inner, Ty::Lits(..) | Ty::Union(..)),
                _ => false,
            }
        }
        match &e.kind {
            ExprKind::Int(v, None) if want.is_some_and(|w| lit_target(&w.ty)) => {
                let w = want.expect("checked");
                self.lit_in(e, &LitVal::Int(*v as i128), &w.ty)
            }
            ExprKind::Int(v, suffix) => self.int_lit(e, *v as i128, *suffix, want),
            ExprKind::Float(_, suffix) => match (suffix, want.map(|w| &w.ty)) {
                (Some(s), _) => suffix_ty(*s),
                (None, Some(d @ Ty::Distinct(_, _, b))) if matches!(**b, Ty::Float(_)) => d.clone(),
                (None, Some(Ty::Float(f))) => Ty::Float(*f),
                (None, Some(Ty::Nullable(inner))) if matches!(**inner, Ty::Float(_)) => {
                    (**inner).clone()
                }
                _ => Ty::NUMBER,
            },
            ExprKind::Str(s) if want.is_some_and(|w| lit_target(&w.ty)) => {
                let w = want.expect("checked");
                self.lit_in(e, &LitVal::Str(s.as_str().into()), &w.ty)
            }
            ExprKind::Bool(b) if want.is_some_and(|w| lit_target(&w.ty)) => {
                let w = want.expect("checked");
                self.lit_in(e, &LitVal::Bool(*b), &w.ty)
            }
            ExprKind::TypeOf(inner) => {
                self.infer(inner);
                Ty::Str
            }
            ExprKind::Arena { body, first, end } => {
                let f = self.stack.last().expect("inside a function").id;
                self.arenas.push((f, *first, *end));
                let t = self.block(body, want);
                self.arenas.pop();
                if self.holds_refs(&t) {
                    let at = body.tail.as_ref().map_or(body.span, |t| t.span);
                    self.arena_escape(at, &format!("the block's value (`{t}`) holds references"));
                }
                t
            }
            ExprKind::RawArenaAlloc { ty, arena, count } => {
                self.require_unsafe(e.span, "arena allocation");
                self.check(arena, &Expect::new(Ty::INT));
                self.check(count, &Expect::new(Ty::INT));
                // (Generic code is unchecked: `Arena.alloc<T>`.)
                if self.holds_refs(ty) && !matches!(ty, Ty::Param(..)) {
                    self.diags.push(
                        Diagnostic::error(
                            codes::POD_REFERENCE,
                            format!("`{ty}` holds references, which arena memory doesn't trace"),
                        )
                        .primary(e.span, "allocate plain data here"),
                    );
                }
                Ty::Ref(Box::new(ty.clone()))
            }
            ExprKind::Unsafe(b) => {
                self.unsafe_depth += 1;
                let t = self.block(b, want);
                self.unsafe_depth -= 1;
                t
            }
            ExprKind::WorkerTake { ty, handle } => {
                self.check(handle, &Expect::new(Ty::INT));
                ty.clone()
            }
            ExprKind::ThrowUnchecked(inner) | ExprKind::ExitUncaught(inner) => {
                if let Some(err) = self.program.error_class {
                    let et = Ty::Class(err, "Error".into(), std::sync::Arc::from([]));
                    self.check(inner, &Expect::new(et));
                }
                Ty::Never
            }
            ExprKind::PtrOf(inner) => {
                self.require_unsafe(e.span, "`ptr.of`");
                let t = self.infer(inner);
                let place = matches!(
                    inner.kind,
                    ExprKind::Var(_) | ExprKind::Member { .. } | ExprKind::Index { .. }
                );
                if !place && !t.is_error() {
                    self.diags.push(
                        Diagnostic::error(
                            codes::MISMATCHED_TYPES,
                            "`ptr.of` needs a variable, a field or an element",
                        )
                        .primary(inner.span, "not a place in memory"),
                    );
                    return Ty::Error;
                }
                if t.is_error() {
                    return Ty::Error;
                }
                Ty::Ref(Box::new(t.erase()))
            }
            ExprKind::SizeOf { align, .. } => {
                self.require_unsafe(e.span, if *align { "`alignof`" } else { "`sizeof`" });
                Ty::Int(IntTy::Usize)
            }
            ExprKind::BitCast(inner, to) => {
                self.require_unsafe(e.span, "`unsafe.cast`");
                let from = self.infer(inner);
                let bits = |t: &Ty| match t.erase() {
                    Ty::Int(it) => Some(it.bits()),
                    Ty::Float(FloatTy::F32) => Some(32),
                    Ty::Float(FloatTy::F64) => Some(64),
                    _ => None,
                };
                match (bits(&from), bits(to)) {
                    (Some(a), Some(b)) if a == b => {}
                    _ if from.is_error() || to.is_error() => {}
                    _ => self.diags.push(
                        Diagnostic::error(
                            codes::INVALID_CAST,
                            format!("`unsafe.cast` from `{from}` to `{to}`"),
                        )
                        .primary(e.span, "not two number types of the same size"),
                    ),
                }
                to.clone()
            }
            ExprKind::Str(s) => {
                let wants_char = matches!(want.map(|w| &w.ty), Some(Ty::Char));
                if wants_char {
                    if s.chars().count() == 1 {
                        return Ty::Char;
                    }
                    self.diags.push(
                        Diagnostic::error(codes::MISMATCHED_TYPES, "mismatched types")
                            .primary(
                                e.span,
                                format!(
                                    "expected `char`, found a string of {} characters",
                                    s.chars().count()
                                ),
                            )
                            .note(
                                "a `char` literal is a string literal with exactly one character",
                            ),
                    );
                    return Ty::Error;
                }
                Ty::Str
            }
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Null => Ty::Null,
            ExprKind::Var(v) => {
                if let Some(t) = self.narrowed(*v).cloned() {
                    self.types.narrowed.insert(e.id);
                    return t;
                }
                self.var_ty(*v, e.span)
            }
            ExprKind::Fn(id) if self.program.func(*id).generics.is_empty() => {
                // A named function used as a value.
                let g = self.program.func(*id);
                if (g.ret.is_none() || g.infer_throws)
                    && self.state[id.0 as usize] == FnState::Unchecked
                {
                    self.check_fn(*id);
                }
                let sig = self.types.sigs[id.0 as usize].clone();
                let ts = self.types.fn_throws[id.0 as usize].clone();
                Ty::Fn(sig.params.into(), Box::new(sig.ret), ts.into())
            }
            ExprKind::FnAddr(id) if self.program.func(*id).generics.is_empty() => {
                // A C function pointer to a top-level function (spec §12.1).
                let g = self.program.func(*id);
                if (g.ret.is_none() || g.infer_throws)
                    && self.state[id.0 as usize] == FnState::Unchecked
                {
                    self.check_fn(*id);
                }
                let sig = self.types.sigs[id.0 as usize].clone();
                let ts = self.types.fn_throws[id.0 as usize].clone();
                if !ts.is_empty() {
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNHANDLED_ERROR,
                            "a function C calls back can't throw",
                        )
                        .primary(e.span, "this function throws")
                        .help("catch its errors inside it"),
                    );
                }
                Ty::Ref(Box::new(Ty::Fn(
                    sig.params.into(),
                    Box::new(sig.ret),
                    Vec::new().into(),
                )))
            }
            ExprKind::Fn(_) | ExprKind::FnAddr(_) | ExprKind::Builtin(_) => {
                self.diags.push(
                    Diagnostic::error(
                        codes::NOT_IMPLEMENTED,
                        "generic and built-in functions as values aren't implemented yet",
                    )
                    .primary(e.span, "call it instead, or wrap it in a closure"),
                );
                Ty::Error
            }
            ExprKind::Closure(id) => self.closure(e, *id, want),
            ExprKind::Comptime { f, .. } => {
                // Checked as a lambda of no parameters returning what's wanted; what it
                // throws is inferred (`{error}` in the throws list) and fails the build.
                let want = want.map(|w| {
                    Expect::from(
                        Ty::Fn(
                            std::sync::Arc::from([]),
                            Box::new(w.ty.clone()),
                            std::sync::Arc::from([Ty::Error]),
                        ),
                        w.origin,
                    )
                });
                if self.checked_thunks.insert(*f) {
                    self.closure(e, *f, want.as_ref());
                }
                self.types.sigs[f.0 as usize].ret.clone()
            }
            ExprKind::Overloaded(_) => {
                self.diags.push(
                    Diagnostic::error(
                        codes::NOT_IMPLEMENTED,
                        "overloaded functions as values aren't implemented yet",
                    )
                    .primary(e.span, "call it instead"),
                );
                Ty::Error
            }
            ExprKind::NamedArg { value, .. } | ExprKind::Spread(value) => {
                self.infer(value);
                self.diags.push(
                    Diagnostic::error(
                        codes::INVALID_OPERANDS,
                        "named and spread arguments only work in calls",
                    )
                    .primary(e.span, "not a call argument"),
                );
                Ty::Error
            }
            ExprKind::Tailcall(inner) => self.tailcall(e, inner, want),
            ExprKind::TryOpt(inner) => match self.infer(inner) {
                t @ (Ty::Void | Ty::Never | Ty::Error | Ty::Nullable(_)) => t,
                t => t.nullable(),
            },
            ExprKind::TryBang(inner) => match want {
                Some(w) => self.check(inner, w),
                None => self.infer(inner),
            },
            ExprKind::Yield(inner) => {
                let id = self.stack.last().expect("inside a function").id;
                let elem = match &self.types.sigs[id.0 as usize].ret {
                    Ty::Nullable(t) => (**t).clone(),
                    t => t.clone(),
                };
                self.check(inner, &Expect::new(elem));
                Ty::Void
            }
            ExprKind::Throw(inner) => {
                let t = self.infer(inner);
                let ok = match (&t, self.program.error_class) {
                    (Ty::Class(sid, ..), Some(err)) => self.program.is_subclass(*sid, err),
                    _ => t.is_error() || self.program.throw_tag(&t).is_some(),
                };
                if !ok {
                    self.diags.push(
                        Diagnostic::error(codes::NOT_THROWABLE, format!("`{t}` can't be thrown"))
                            .primary(inner.span, "not an `Error`")
                            .help("throw an `Error` (or subclass) instance, or a value of a struct or enum that implements `Throwable`"),
                    );
                }
                Ty::Never
            }
            ExprKind::Try {
                body,
                catches,
                finally,
            } => {
                let statement = want.is_some_and(|w| w.ty == Ty::Void);
                let want = want.filter(|w| w.ty != Ty::Void);
                let mut results: Vec<(&Expr, Ty)> = Vec::new();
                let t = match want {
                    Some(w) => self.check(body, w),
                    None => self.infer(body),
                };
                results.push((body, t));
                let err = match self.program.error_class {
                    Some(sid) => self.program.struct_def(sid).self_ty(sid),
                    None => Ty::Error,
                };
                for c in catches {
                    // `catch (e: ParseFail)` catches a lightweight throwable; other clauses
                    // look at an `Error`.
                    match &c.pat.kind {
                        PatKind::Type { ty, .. } if self.program.throw_tag(ty).is_some() => {
                            let ty = ty.clone();
                            self.check_pattern(&c.pat, &ty)
                        }
                        _ => self.check_pattern(&c.pat, &err),
                    }
                    let t = match want {
                        Some(w) => self.check(&c.body, w),
                        None => self.infer(&c.body),
                    };
                    results.push((&c.body, t));
                }
                if let Some(f) = finally {
                    self.infer(f);
                }
                match want {
                    _ if statement => Ty::Void,
                    Some(w) => w.ty.clone(),
                    None => self.unify_exprs(&results, "`try` and `catch` have different types"),
                }
            }
            ExprKind::Unary(op, inner) => self.unary(e, *op, inner, want),
            ExprKind::Binary(op, l, r) => self.binary(e, *op, l, r, want),
            ExprKind::Assign {
                op: None,
                target: Target::Index { object, index },
                value,
            } if {
                let ot = self.peek_type(object);
                matches!(ot, Some(Ty::Struct(..) | Ty::Class(..)))
            } =>
            {
                // `obj[i] = v` through `operator []=`.
                let ot = self.infer(object);
                match self.operator_for(&ot, "op[]=", Some(index)) {
                    Some(m) => {
                        self.types.op_calls.insert(e.id, m);
                        let args = [index, value];
                        let sig = self.types.sigs[m.0 as usize].clone();
                        if sig.params.len() == 2 {
                            self.check(args[0], &Expect::new(sig.params[0].clone()));
                            self.check(args[1], &Expect::new(sig.params[1].clone()));
                        }
                    }
                    None => {
                        self.infer(index);
                        self.infer(value);
                        self.diags.push(
                            Diagnostic::error(
                                codes::INVALID_OPERANDS,
                                format!("`{ot}` has no `[]=` operator"),
                            )
                            .primary(e.span, "can't assign to an element"),
                        );
                    }
                }
                Ty::Void
            }
            ExprKind::Assign { op, target, value }
                if !self.creates_cell(value) && self.reactive_target(target, e.span).is_some() =>
            {
                let inner = self.reactive_target(target, e.span).expect("checked");
                self.write_cell(e, target);
                match op {
                    None => {
                        self.check(value, &Expect::new(inner));
                    }
                    Some(BinOp::And | BinOp::Or | BinOp::Nullish) => {
                        self.infer(value);
                        self.diags.push(
                            Diagnostic::error(
                                codes::NOT_IMPLEMENTED,
                                "`&&=`, `||=` and `??=` on reactive state",
                            )
                            .primary(e.span, "not implemented yet"),
                        );
                    }
                    Some(op) => {
                        self.compound(e, *op, inner, value);
                    }
                }
                Ty::Void
            }
            ExprKind::Assign { op, target, value } => {
                let (vt, origin) = self.target_ty(target);
                self.check_assignable(target, e.span);
                if !self.arenas.is_empty() && self.holds_refs(&vt) {
                    let root = match target {
                        Target::Var(v) => Some(*v),
                        Target::Field { object, .. } | Target::Index { object, .. } => {
                            root_var(object)
                        }
                    };
                    if let Some(v) = root
                        && self.outside_arena(v)
                    {
                        self.arena_escape(e.span, "a reference is stored outside the block");
                    }
                }
                match op {
                    None => {
                        self.check(value, &Expect::from(vt, origin));
                    }
                    Some(op) => {
                        self.compound(e, *op, vt, value);
                    }
                }
                // The value is checked first: `cur = cur.next` reads the narrowed `cur`.
                if let Target::Var(v) = target {
                    self.forget(*v);
                }
                Ty::Void
            }
            ExprKind::Object {
                ty,
                fields,
                type_args,
            } => self.object(e, *ty, fields, type_args, want),
            ExprKind::IncDec { target, .. } => {
                if let Target::Var(v) = target {
                    self.forget(*v);
                }
                if let Some(inner) = self.reactive_target(target, e.span) {
                    self.write_cell(e, target);
                    if !inner.is_int() && !inner.is_error() {
                        self.invalid_operand(e.span, "`++`/`--`", &inner, "an integer");
                        return Ty::Error;
                    }
                    return inner;
                }
                self.check_assignable(target, e.span);
                let (t, _) = self.target_ty(target);
                if !t.is_int() && !t.is_error() {
                    self.invalid_operand(e.span, "`++`/`--`", &t, "an integer");
                    return Ty::Error;
                }
                t
            }
            ExprKind::Call {
                callee,
                args,
                type_args,
            } => {
                if let ExprKind::Overloaded(cands) = &callee.kind {
                    self.set(callee, Ty::Void);
                    let Some(id) = self.pick_overload(e, cands, args) else {
                        return Ty::Error;
                    };
                    self.types.overloads.insert(e.id, id);
                    if !self.program.func(id).generics.is_empty() {
                        return self.call_generic(e, id, args, type_args);
                    }
                    return self.call_fn(e, id, args);
                }
                if let ExprKind::Fn(id) = callee.kind
                    && !self.program.func(id).generics.is_empty()
                {
                    self.set(callee, Ty::Void);
                    return self.call_generic(e, id, args, type_args);
                }
                if !type_args.is_empty() && matches!(callee.kind, ExprKind::Member { .. }) {
                    self.method_type_args = type_args.clone();
                } else if let Some(t) = type_args.first() {
                    let _ = t;
                    self.diags.push(
                        Diagnostic::error(
                            codes::WRONG_ARG_COUNT,
                            "type arguments for a function that isn't generic",
                        )
                        .primary(callee.span, "takes no type arguments"),
                    );
                }
                self.call(e, callee, args)
            }
            ExprKind::Member {
                object,
                name,
                name_span,
            } => {
                let ot = self.infer(object).strip_immutable();
                match (&ot, name.as_str()) {
                    (Ty::Str | Ty::Array(_) | Ty::Fixed(..) | Ty::FixedN(..), "length") => Ty::INT,
                    (Ty::Range(it), "start" | "end") => Ty::Int(*it),
                    (Ty::Error, _) => Ty::Error,
                    (Ty::Struct(sid, _, args), _) => {
                        self.recv_args = args.to_vec();
                        self.field(e.id, *sid, name, *name_span)
                    }
                    (Ty::Class(sid, _, args), _) => {
                        self.recv_args = args.to_vec();
                        self.class_member(e.id, *sid, name, *name_span)
                    }
                    (Ty::Variant(eid, v, _), _) => {
                        let var = &self.program.enum_def(*eid).variants[*v as usize];
                        match var.field(name) {
                            Some((i, f)) => {
                                self.types.variant_fields.insert(e.id, (*v, i as u32));
                                f.ty.clone()
                            }
                            None => {
                                self.diags.push(
                                    Diagnostic::error(
                                        codes::UNKNOWN_MEMBER,
                                        format!("`{ot}` with `{}` has no field `{name}`", var.name),
                                    )
                                    .primary(*name_span, "unknown field"),
                                );
                                Ty::Error
                            }
                        }
                    }
                    (Ty::Enum(eid, _), _)
                        if matches!(&self.program.enum_def(*eid).origin,
                            crate::hir::EnumOrigin::Discriminated(d) if d == name) =>
                    {
                        // The discriminant is field 0 of every variant.
                        let def = self.program.enum_def(*eid);
                        let ty = def
                            .variants
                            .first()
                            .map_or(Ty::Error, |v| v.fields[0].ty.clone());
                        let all: Vec<LitVal> = def
                            .variants
                            .iter()
                            .filter_map(|v| match &v.fields[0].ty {
                                Ty::Lits(_, ls) => ls.first().cloned(),
                                _ => None,
                            })
                            .collect();
                        self.types.variant_fields.insert(e.id, (0, 0));
                        match ty {
                            Ty::Lits(b, _) => Ty::Lits(b, all.into()),
                            t => t,
                        }
                    }
                    (Ty::Enum(eid, _), _)
                        if matches!(
                            self.program.enum_def(*eid).origin,
                            crate::hir::EnumOrigin::Discriminated(_)
                        ) =>
                    {
                        self.diags.push(
                            Diagnostic::error(
                                codes::UNKNOWN_MEMBER,
                                format!("`{ot}` has no member `{name}` in every case"),
                            )
                            .primary(*name_span, "check the discriminant first")
                            .help("narrow it, e.g. `if (e.kind == \"click\") e.x`"),
                        );
                        Ty::Error
                    }
                    _ => {
                        let mut d = Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!("`{ot}` has no member `{name}`"),
                        )
                        .primary(*name_span, "unknown member");
                        if let Ty::Nullable(_) = ot {
                            d = d.help(format!(
                                "the value may be null: use `?.{name}`, or check it with `!= null` first"
                            ));
                        }
                        self.diags.push(d);
                        Ty::Error
                    }
                }
            }
            ExprKind::Index { object, index } => {
                let ot = self.infer(object).strip_immutable();
                match ot {
                    Ty::Str => {
                        self.check(index, &Expect::new(Ty::INT));
                        Ty::Char
                    }
                    Ty::Array(t) | Ty::Fixed(t, _) | Ty::FixedN(t, _) => {
                        self.check(index, &Expect::new(Ty::INT));
                        *t
                    }
                    Ty::Tuple(ts) => self.tuple_index(&ts, index),
                    Ty::Error => {
                        self.infer(index);
                        Ty::Error
                    }
                    t @ (Ty::Struct(..) | Ty::Class(..))
                        if self.operator_for(&t, "op[]", None).is_some() =>
                    {
                        let m = self.operator_for(&t, "op[]", Some(index)).expect("checked");
                        self.types.op_calls.insert(e.id, m);
                        self.call_fn(e, m, std::slice::from_ref(index))
                    }
                    _ => {
                        self.infer(index);
                        self.diags.push(
                            Diagnostic::error(
                                codes::INVALID_OPERANDS,
                                format!("`{ot}` can't be indexed"),
                            )
                            .primary(object.span, format!("has type `{ot}`")),
                        );
                        Ty::Error
                    }
                }
            }
            ExprKind::Template { parts, .. } => {
                for p in parts {
                    let t = self.infer(&p.expr);
                    // `Formattable` values format themselves (`${x}`, `${x:spec}`); `:?` stays
                    // the debug form.
                    if !p.spec.as_deref().is_some_and(|s| s.ends_with('?'))
                        && let Some(m) = self.formattable_method(&t)
                    {
                        self.types.formattables.insert(p.expr.id, m);
                        continue;
                    }
                    let compound = crate::format::is_compound(&t);
                    let debug = compound && p.spec.as_deref().is_some_and(|s| s.ends_with('?'));
                    if !is_printable(&t) && !debug {
                        if compound {
                            self.diags.push(
                                Diagnostic::error(
                                    codes::INVALID_OPERANDS,
                                    format!("`{t}` has no string form"),
                                )
                                .primary(p.expr.span, "a compound value in a template")
                                .help("show its fields with the debug format: `${value:?}`"),
                            );
                            continue;
                        }
                        self.diags.push(
                            Diagnostic::error(
                                codes::INVALID_OPERANDS,
                                format!("`{t}` can't be put in a template"),
                            )
                            .primary(p.expr.span, "has no string form"),
                        );
                        continue;
                    }
                    if let Some(spec) = &p.spec {
                        self.format_spec(&p.expr, spec);
                    }
                }
                Ty::Str
            }
            ExprKind::If { cond, then, else_ } => self.if_expr(cond, then, else_.as_deref(), want),
            ExprKind::Block(b) => self.block(b, want),
            ExprKind::Loop(b) => {
                self.forget_assigned(&[e]);
                let want = want.filter(|w| w.ty != Ty::Void).cloned();
                self.ctx().loops.push(LoopCtx {
                    produces_value: true,
                    want: want.clone(),
                    breaks: Vec::new(),
                });
                self.block(b, None);
                let breaks = self.ctx().loops.pop().unwrap().breaks;
                if breaks.is_empty() {
                    return Ty::Never;
                }
                if let Some(w) = want {
                    // Break values were already checked against the expectation.
                    return w.ty;
                }
                self.unify_all(&breaks, "`break` values have different types")
            }
            ExprKind::While {
                cond, body, update, ..
            } => {
                self.forget_assigned(&[e]);
                self.check(cond, &Expect::new(Ty::Bool));
                let facts = self.facts(cond);
                self.ctx().loops.push(LoopCtx {
                    produces_value: false,
                    want: None,
                    breaks: Vec::new(),
                });
                self.with_facts(facts.when_true, |c| {
                    c.infer(body);
                    if let Some(u) = update {
                        c.infer(u);
                    }
                });
                self.ctx().loops.pop();
                Ty::Void
            }
            ExprKind::Break(value) => {
                // The innermost loop; `want` of an enclosing `loop` is passed via break types.
                let want = self.ctx().loops.last().and_then(|l| l.want.clone());
                let t = match (value, &want) {
                    (Some(v), Some(w)) => self.check(v, w),
                    (Some(v), None) => self.infer(v),
                    (None, _) => Ty::Void,
                };
                if let Some(l) = self.ctx().loops.last_mut()
                    && l.produces_value
                {
                    l.breaks
                        .push((t, value.as_ref().map_or(e.span, |v| v.span)));
                }
                Ty::Never
            }
            ExprKind::Continue => Ty::Never,
            ExprKind::Return(value) => {
                self.ret(e, value.as_deref());
                let f = self.stack.last().expect("inside a function").id;
                if let Some(v) = value
                    && self.arenas.iter().any(|a| a.0 == f)
                    && self.holds_refs(&self.types.expr(v.id).clone())
                {
                    self.arena_escape(e.span, "a reference is returned from inside the block");
                }
                Ty::Never
            }
            ExprKind::As(inner, to) => {
                let from = self.infer(inner);
                // Distinct types convert like their base types.
                let ok = match (&from.erase(), &to.erase()) {
                    (a, b) if a == b => true,
                    (Ty::Int(_) | Ty::Float(_), Ty::Int(_) | Ty::Float(_)) => true,
                    (Ty::Char, Ty::Int(_)) => true,
                    (Ty::Int(IntTy::U8 | IntTy::U32), Ty::Char) => true,
                    // Raw pointers to any other pointee (`ptr<void>` for C; spec §12.1).
                    (Ty::Ref(_), Ty::Ref(_)) => true,
                    (Ty::Error, _) | (_, Ty::Error) => true,
                    _ => false,
                };
                if !ok {
                    self.diags.push(
                        Diagnostic::error(
                            codes::INVALID_CAST,
                            format!("can't convert `{from}` to `{to}` with `as`"),
                        )
                        .primary(e.span, "invalid conversion")
                        .note("`as` converts between numeric types (and `char` ↔ integer codes)"),
                    );
                    return Ty::Error;
                }
                to.clone()
            }
            ExprKind::Array(elems) => self.array_lit(e, elems, want),
            ExprKind::Repeat { value, count } => self.repeat_lit(e, value, *count, want),
            ExprKind::RepeatN { value, count } => {
                let want_elem = match want.map(|w| &w.ty) {
                    Some(Ty::FixedN(t, n)) if **n == *count => Some((**t).clone()),
                    _ => None,
                };
                let t = match want_elem {
                    Some(t) => {
                        self.check(value, &Expect::new(t.clone()));
                        t
                    }
                    None => self.infer(value),
                };
                if t.is_error() {
                    return Ty::Error;
                }
                Ty::FixedN(Box::new(t), Box::new(count.clone()))
            }
            ExprKind::ConstParam(t) => self.const_param_ty(t),
            ExprKind::TypeInfo { member, elem, .. } => match member {
                TypeMember::Name | TypeMember::Kind => Ty::Str,
                TypeMember::Fields | TypeMember::Methods => Ty::Array(Box::new(elem.clone())),
            },
            ExprKind::TypeValue(t) => {
                self.diags.push(
                    Diagnostic::error(codes::NOT_A_VALUE, format!("`{t}` is a type, not a value"))
                        .primary(e.span, "type used as a value"),
                );
                Ty::Error
            }
            ExprKind::Range { start, end } => self.range(e, start, end, want),
            ExprKind::ForOf { var, iter, body } => {
                self.forget_assigned(&[body]);
                let it = self.infer(iter);
                let elem = match &it {
                    Ty::Range(i) => Ty::Int(*i),
                    Ty::Array(t) | Ty::Fixed(t, _) | Ty::FixedN(t, _) => (**t).clone(),
                    Ty::Str => Ty::Char,
                    Ty::Error => Ty::Error,
                    Ty::Class(sid, _, args) if Some(*sid) == self.program.iterator_class => {
                        args.first().cloned().unwrap_or(Ty::Error)
                    }
                    other if self.iterable_elem(iter, other).is_some() => {
                        self.iterable_elem(iter, other).expect("checked")
                    }
                    other => {
                        self.diags.push(
                            Diagnostic::error(
                                codes::INVALID_OPERANDS,
                                format!("`{other}` isn't iterable"),
                            )
                            .primary(iter.span, format!("has type `{other}`"))
                            .note("`for-of` works over arrays, fixed arrays, ranges, strings, iterators and types with an `iterator()` method"),
                        );
                        Ty::Error
                    }
                };
                self.set_var_ty(Var::Local(*var), elem);
                self.ctx().loops.push(LoopCtx {
                    produces_value: false,
                    want: None,
                    breaks: Vec::new(),
                });
                self.infer(body);
                self.ctx().loops.pop();
                Ty::Void
            }
            ExprKind::Variant {
                enum_,
                variant,
                args,
            } => self.variant(e, *enum_, *variant, args.as_deref()),
            ExprKind::New {
                class,
                args,
                type_args,
                ..
            } if !self.program.struct_def(*class).generics.is_empty() => {
                // A generic class: its type arguments are given, expected, or inferred from
                // the constructor's arguments.
                let def = self.program.struct_def(*class);
                let Some(ctor) = self.pick_ctor(*class, args) else {
                    for a in args {
                        self.infer(a);
                    }
                    self.diags.push(
                        Diagnostic::error(
                            codes::WRONG_ARG_COUNT,
                            format!(
                                "`{}` has no constructor taking {} arguments",
                                def.name,
                                args.len()
                            ),
                        )
                        .primary(e.span, "no matching constructor"),
                    );
                    return Ty::Error;
                };
                let explicit = match (type_args.is_empty(), want.map(|w| &w.ty)) {
                    (false, _) => type_args.clone(),
                    (true, Some(Ty::Class(s, _, a))) if s == class => a.to_vec(),
                    _ => Vec::new(),
                };
                let generics = def.generics.clone();
                self.types.ctor_calls.insert(e.id, ctor);
                let (_, targs) = self.call_inferring(
                    e,
                    ctor,
                    &args.iter().collect::<Vec<_>>(),
                    &explicit,
                    Ty::type_owner(*class),
                    &generics,
                    None,
                );
                self.types.recv_args.insert(e.id, targs.clone());
                self.program
                    .struct_def(*class)
                    .ty_with(*class, targs.into())
            }
            ExprKind::CtorCall { class, args }
                if !self.program.struct_def(*class).generics.is_empty() =>
            {
                // Inside a generic class: the same type arguments as `this`.
                let def = self.program.struct_def(*class);
                let Some(ctor) = self.pick_ctor(*class, args) else {
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Void;
                };
                let recv = def.self_ty(*class).type_args().to_vec();
                self.types.ctor_calls.insert(e.id, ctor);
                let (_, _) = self.call_inferring(
                    e,
                    ctor,
                    &args.iter().collect::<Vec<_>>(),
                    &[],
                    ctor.0,
                    &[],
                    Some((Ty::type_owner(*class), &recv)),
                );
                self.types.recv_args.insert(e.id, recv);
                Ty::Void
            }
            ExprKind::New { class, args, .. } | ExprKind::CtorCall { class, args } => {
                let def = self.program.struct_def(*class);
                match self.pick_ctor(*class, args) {
                    Some(ctor) => {
                        self.types.ctor_calls.insert(e.id, ctor);
                        self.call_fn(e, ctor, args);
                    }
                    None => {
                        let arities: Vec<String> =
                            def.ctors.iter().map(|(_, p)| p.len().to_string()).collect();
                        self.diags.push(
                            Diagnostic::error(
                                codes::WRONG_ARG_COUNT,
                                format!(
                                    "`{}` has no constructor taking {} argument{}",
                                    def.name,
                                    args.len(),
                                    if args.len() == 1 { "" } else { "s" }
                                ),
                            )
                            .primary(e.span, "no matching constructor")
                            .note(format!(
                                "its constructors take {} arguments",
                                arities.join(" or ")
                            )),
                        );
                        for a in args {
                            self.infer(a);
                        }
                    }
                }
                match e.kind {
                    ExprKind::New { .. } => self.program.struct_ty(*class),
                    _ => Ty::Void,
                }
            }
            ExprKind::SuperCall { method, args } => self.call_fn(e, *method, args),
            ExprKind::InstanceOf { expr, class } => {
                let t = self.infer(expr);
                let ok = match &t {
                    Ty::Class(..) | Ty::Error => true,
                    Ty::Nullable(inner) => matches!(**inner, Ty::Class(..)),
                    _ => false,
                };
                if !ok {
                    self.invalid_operand(expr.span, "`instanceof`", &t, "a class instance");
                }
                let _ = class;
                Ty::Bool
            }
            ExprKind::Match {
                scrutinee,
                arms,
                switch,
            } => self.match_expr(e, scrutinee, arms, *switch, want),
            ExprKind::Is { expr, pat } => {
                let t = self.infer(expr);
                self.check_pattern(pat, &t);
                Ty::Bool
            }
            ExprKind::NonNull(inner) => match self.infer(inner) {
                Ty::Nullable(t) => *t,
                Ty::Null => {
                    self.diags.push(
                        Diagnostic::error(codes::INVALID_OPERANDS, "`null!` always panics")
                            .primary(e.span, "this value is always null"),
                    );
                    Ty::Error
                }
                t => t,
            },
            ExprKind::OptChain { tmp, base, rest } => {
                let bt = self.infer(base);
                let (inner, nullable) = match bt {
                    Ty::Nullable(t) => (*t, true),
                    t => (t, false),
                };
                self.set_var_ty(Var::Local(*tmp), inner);
                let rt = self.infer(rest);
                match rt {
                    Ty::Void | Ty::Never | Ty::Error => rt,
                    t if nullable => t.nullable(),
                    t => t,
                }
            }
            ExprKind::Error => Ty::Error,
        }
    }

    /// `Shape.Circle(r: 1)`, `Shape.Rect(2, 3)`, `Color.Red`.
    fn variant(&mut self, e: &Expr, eid: EnumId, vi: u32, args: Option<&[VariantArg]>) -> Ty {
        let def = self.program.enum_def(eid);
        let v = &def.variants[vi as usize];
        let ty = self.program.enum_ty(eid);
        let Some(args) = args else {
            if !v.fields.is_empty() || v.has_parens {
                let fields: Vec<String> = v
                    .fields
                    .iter()
                    .map(|f| format!("{}: {}", f.name, f.ty))
                    .collect();
                self.diags.push(
                    Diagnostic::error(
                        codes::WRONG_ARG_COUNT,
                        format!("`{}.{}` needs its fields", def.name, v.name),
                    )
                    .primary(e.span, "no arguments given")
                    .secondary(
                        v.span,
                        format!("declared as `{}({})`", v.name, fields.join(", ")),
                    ),
                );
            }
            return ty;
        };
        if !v.has_parens {
            self.diags.push(
                Diagnostic::error(
                    codes::WRONG_ARG_COUNT,
                    format!("`{}.{}` has no fields", def.name, v.name),
                )
                .primary(e.span, "called with parentheses")
                .help(format!("write `{}.{}`", def.name, v.name)),
            );
            for a in args {
                self.infer(&a.value);
            }
            return ty;
        }
        let mut seen: Vec<Option<Span>> = vec![None; v.fields.len()];
        let mut order = Vec::new();
        let mut next_positional = 0;
        let mut named_seen = false;
        for a in args {
            let idx = match &a.name {
                None => {
                    if named_seen {
                        self.diags.push(
                            Diagnostic::error(
                                codes::POSITIONAL_AFTER_NAMED,
                                "positional argument after a named one",
                            )
                            .primary(a.value.span, "move it before the named arguments"),
                        );
                    }
                    let i = next_positional;
                    next_positional += 1;
                    if i >= v.fields.len() {
                        self.diags.push(
                            Diagnostic::error(
                                codes::WRONG_ARG_COUNT,
                                format!(
                                    "`{}.{}` takes {} argument{}",
                                    def.name,
                                    v.name,
                                    v.fields.len(),
                                    if v.fields.len() == 1 { "" } else { "s" }
                                ),
                            )
                            .primary(a.value.span, "extra argument"),
                        );
                        None
                    } else {
                        Some(i)
                    }
                }
                Some((name, span)) => {
                    named_seen = true;
                    match v.field(name) {
                        Some((i, _)) => Some(i),
                        None => {
                            self.diags.push(
                                Diagnostic::error(
                                    codes::UNKNOWN_MEMBER,
                                    format!("`{}.{}` has no field `{name}`", def.name, v.name),
                                )
                                .primary(*span, "unknown field")
                                .secondary(v.span, "variant declared here"),
                            );
                            None
                        }
                    }
                }
            };
            match idx {
                Some(i) => {
                    let span = a.name.as_ref().map_or(a.value.span, |(_, s)| *s);
                    if let Some(prev) = seen[i] {
                        self.diags.push(
                            Diagnostic::error(
                                codes::DUPLICATE_FIELD,
                                format!("field `{}` is given twice", v.fields[i].name),
                            )
                            .primary(span, "given again here")
                            .secondary(prev, "first given here"),
                        );
                    }
                    seen[i] = Some(span);
                    order.push(i as u32);
                    let f = &v.fields[i];
                    self.check(&a.value, &Expect::from(f.ty.clone(), Some(f.span)));
                }
                None => {
                    order.push(u32::MAX);
                    self.infer(&a.value);
                }
            }
        }
        let missing: Vec<String> = v
            .fields
            .iter()
            .zip(&seen)
            .filter(|(_, s)| s.is_none())
            .map(|(f, _)| format!("`{}`", f.name))
            .collect();
        if !missing.is_empty() {
            self.diags.push(
                Diagnostic::error(
                    codes::MISSING_FIELDS,
                    format!(
                        "missing field{} {}",
                        if missing.len() == 1 { "" } else { "s" },
                        missing.join(", ")
                    ),
                )
                .primary(
                    e.span,
                    format!("`{}.{}` needs every field", def.name, v.name),
                )
                .secondary(v.span, "variant declared here"),
            );
        }
        self.types.variant_args.insert(e.id, order);
        ty
    }

    fn match_expr(
        &mut self,
        e: &Expr,
        scrutinee: &Expr,
        arms: &[Arm],
        switch: Option<bool>,
        want: Option<&Expect>,
    ) -> Ty {
        let st = self.infer(scrutinee);
        let want = want.filter(|w| w.ty != Ty::Void);
        // A switch statement needn't cover every case (and has no value).
        let statement = switch == Some(false) && want.is_none();
        let mut results: Vec<(&Expr, Ty)> = Vec::new();
        for arm in arms {
            self.check_pattern(&arm.pat, &st);
            if let Some(g) = &arm.guard {
                self.check(g, &Expect::new(Ty::Bool));
            }
            let facts = arm
                .guard
                .as_ref()
                .map(|g| self.facts(g).when_true)
                .unwrap_or_default();
            let t = self.with_facts(facts, |c| match want {
                Some(w) => c.check(&arm.body, w),
                None => c.infer(&arm.body),
            });
            results.push((&arm.body, t));
        }
        if !st.is_error() && !statement {
            let rows: Vec<Pat> = arms
                .iter()
                .filter(|a| a.guard.is_none())
                .flat_map(|a| self.to_pat(&a.pat, &st))
                .collect();
            let ex = exhaust::Checker {
                program: self.program,
            };
            let missing = ex.missing(&st, &rows, 8);
            if !missing.is_empty() {
                let list: Vec<String> = missing.iter().map(|m| format!("`{m}`")).collect();
                self.diags.push(
                    Diagnostic::error(
                        codes::NON_EXHAUSTIVE,
                        format!("non-exhaustive match: missing {}", list.join(", ")),
                    )
                    .primary(
                        scrutinee.span,
                        format!("`{st}` isn't covered by every case"),
                    )
                    .help("add arms for the missing cases, or a wildcard arm `_ => …`")
                    .note("arms with an `if` guard don't count, since the guard can be false"),
                );
            }
        }
        let _ = e;
        if statement {
            return Ty::Void;
        }
        if let Some(w) = want {
            return w.ty.clone();
        }
        self.unify_exprs(&results, "`match` arms have different types")
    }

    /// Joins the types of several expressions (match arms), recording widenings, and turning
    /// `T` + `null` into `T?`.
    fn unify_exprs(&mut self, items: &[(&Expr, Ty)], msg: &str) -> Ty {
        let live: Vec<&(&Expr, Ty)> = items.iter().filter(|(_, t)| *t != Ty::Never).collect();
        let Some(first) = live.first() else {
            return Ty::Never;
        };
        if live.iter().any(|(_, t)| t.is_error()) {
            return Ty::Error;
        }
        let mut acc = first.1.clone();
        for (_, t) in &live[1..] {
            if t.widens_to(&acc) {
                continue;
            }
            if acc.widens_to(t) {
                acc = t.clone();
                continue;
            }
            if *t == Ty::Null && acc != Ty::Void {
                acc = acc.nullable();
                continue;
            }
            if acc == Ty::Null && *t != Ty::Void {
                acc = t.clone().nullable();
                continue;
            }
            let mut d = Diagnostic::error(codes::MISMATCHED_TYPES, msg.to_string());
            for (x, t) in &live {
                d = d.secondary(x.span, format!("`{t}`"));
            }
            d.labels[0].primary = true;
            self.diags.push(d);
            return Ty::Error;
        }
        for (x, t) in &live {
            if *t != acc {
                self.types.coercions.insert(x.id, acc.clone());
            }
        }
        acc
    }

    fn pattern_mismatch(&mut self, span: Span, ty: &Ty, what: &str) {
        if ty.is_error() {
            return;
        }
        self.diags.push(
            Diagnostic::error(
                codes::PATTERN_MISMATCH,
                format!("this pattern can't match a `{ty}`"),
            )
            .primary(span, format!("{what} pattern"))
            .note(format!("the matched value has type `{ty}`")),
        );
    }

    /// For a pattern on a union: the member it looks at (type tests, literals, variants,
    /// struct and array patterns), or `None` for `_`, bindings and the like.
    fn union_member_for(&mut self, p: &Pattern, ms: &[Ty]) -> Option<Option<usize>> {
        let pick = |pred: &dyn Fn(&Ty) -> bool| ms.iter().position(pred);
        Some(match &p.kind {
            PatKind::Wild | PatKind::Bind(_) | PatKind::Or(_) | PatKind::As(..) => return None,
            PatKind::Type { ty, .. } => pick(&|m| m == ty),
            PatKind::Lit(l) => {
                let lit = match l {
                    LitPat::Str(s) => Some(LitVal::Str(s.as_str().into())),
                    LitPat::Int(v) => Some(LitVal::Int(*v)),
                    LitPat::Bool(b) => Some(LitVal::Bool(*b)),
                    _ => None,
                };
                let in_lits =
                    |m: &Ty| matches!((m, &lit), (Ty::Lits(_, ls), Some(l)) if ls.contains(l));
                pick(&in_lits).or_else(|| {
                    pick(&|m: &Ty| {
                        matches!(
                            (l, m.erase()),
                            (LitPat::Str(_), Ty::Str | Ty::Char)
                                | (LitPat::Int(_), Ty::Int(_) | Ty::Float(_))
                                | (LitPat::Float(_), Ty::Float(_))
                                | (LitPat::Bool(_), Ty::Bool)
                        ) && !matches!(m, Ty::Lits(..))
                    })
                })
            }
            PatKind::Range(..) => pick(&|m| matches!(m, Ty::Int(_) | Ty::Char)),
            PatKind::Variant { enum_, name, .. } => pick(&|m| match m {
                Ty::Enum(e, _) => match enum_ {
                    Some(x) => x == e,
                    None => self.program.enum_def(*e).variant(name).is_some(),
                },
                _ => false,
            }),
            PatKind::Struct { sid, .. } => {
                pick(&|m| matches!(m, Ty::Struct(s, _, _) | Ty::Class(s, _, _) if s == sid))
            }
            PatKind::Slice { .. } => {
                pick(&|m| matches!(m, Ty::Array(_) | Ty::Tuple(_) | Ty::Fixed(..)))
            }
        })
    }

    /// Checks a pattern against the type of the value it matches, typing its bindings.
    fn check_pattern(&mut self, p: &Pattern, ty: &Ty) {
        let union = match ty {
            Ty::Union(_, ms) => Some(ms.clone()),
            _ => None,
        };
        if let Some(ms) = union
            && let Some(member) = self.union_member_for(p, &ms)
        {
            match member {
                Some(i) => {
                    self.types.union_tests.insert(p.id, i as u32);
                    let m = ms[i].clone();
                    return self.check_pattern(p, &m);
                }
                None => {
                    self.pattern_mismatch(p.span, ty, "this");
                    return;
                }
            }
        }
        self.types.exprs[p.id.0 as usize] = ty.clone();
        let inner = match ty {
            Ty::Nullable(t) if p.looks_inside(ty) => (**t).clone(),
            _ => ty.clone(),
        };
        match &p.kind {
            PatKind::Wild => {}
            PatKind::Bind(l) => self.bind_local(*l, ty, p.span),
            PatKind::Or(alts) => {
                for a in alts {
                    self.check_pattern(a, ty);
                }
            }
            PatKind::As(l, sub) => {
                self.bind_local(*l, &inner, p.span);
                self.check_pattern(sub, ty);
            }
            PatKind::Range(lo, hi) => {
                for l in [lo, hi] {
                    let ok = match (l, &inner) {
                        (LitPat::Int(v), Ty::Int(it)) => {
                            if !it.contains(*v) && *v != it.max() + 1 {
                                self.lit_out_of_range(p.span, *v, *it);
                            }
                            true
                        }
                        (LitPat::Str(s), Ty::Char) => s.chars().count() == 1,
                        (_, Ty::Error) => true,
                        _ => false,
                    };
                    if !ok {
                        self.pattern_mismatch(p.span, ty, "a range");
                        break;
                    }
                }
            }
            PatKind::Slice {
                prefix,
                rest,
                suffix,
            } => self.slice_pattern(p, ty, &inner, prefix, *rest, suffix),
            PatKind::Type {
                ty: want,
                bind,
                fields,
            } => {
                // Until unions (P3.12), a type test holds, checks for null, or (for classes)
                // checks the runtime class.
                let downcast = matches!((want, &inner), (Ty::Class(a, _, _), Ty::Class(b, _, _))
                    if a != b && self.program.is_subclass(*a, *b));
                if downcast {
                    self.types.downcasts.insert(p.id);
                }
                let ok = *want == inner || downcast || want.is_error() || inner.is_error();
                if !ok {
                    self.diags.push(
                        Diagnostic::error(
                            codes::PATTERN_MISMATCH,
                            format!("a `{ty}` is never a `{want}`"),
                        )
                        .primary(p.span, "this type test can't succeed"),
                    );
                }
                let bt = if ok { want.clone() } else { Ty::Error };
                if let Some(l) = bind {
                    self.bind_local(*l, &bt, p.span);
                }
                if let Some(fps) = fields {
                    match &bt {
                        Ty::Struct(sid, _, _) | Ty::Class(sid, _, _) => {
                            let def = self.program.struct_def(*sid);
                            let field_tys: Vec<(String, Ty)> = def
                                .fields
                                .iter()
                                .map(|f| (f.name.clone(), f.ty.clone()))
                                .collect();
                            let what = format!("`{}`", def.name);
                            self.field_patterns(p, fps, &field_tys, &what);
                        }
                        _ => {
                            for f in fps {
                                self.check_pattern(&f.pat, &Ty::Error);
                            }
                            if !bt.is_error() {
                                self.pattern_mismatch(p.span, ty, "a field");
                            }
                        }
                    }
                }
            }
            PatKind::Lit(l) => {
                if let Ty::Lits(_, ls) = &inner {
                    let lit = match l {
                        LitPat::Str(s) => Some(LitVal::Str(s.as_str().into())),
                        LitPat::Int(v) => Some(LitVal::Int(*v)),
                        LitPat::Bool(b) => Some(LitVal::Bool(*b)),
                        _ => None,
                    };
                    if !lit.is_some_and(|x| ls.contains(&x)) {
                        self.pattern_mismatch(p.span, ty, "a literal");
                    }
                    return;
                }
                let ok = match (l, &inner) {
                    (LitPat::Int(v), Ty::Int(it)) => {
                        if !it.contains(*v) {
                            self.lit_out_of_range(p.span, *v, *it);
                        }
                        true
                    }
                    (LitPat::Int(_) | LitPat::Float(_), Ty::Float(_)) => true,
                    (LitPat::Str(_), Ty::Str) => true,
                    (LitPat::Str(s), Ty::Char) => s.chars().count() == 1,
                    (LitPat::Bool(_), Ty::Bool) => true,
                    (LitPat::Null, Ty::Nullable(_)) => true,
                    (_, Ty::Error) => true,
                    _ => false,
                };
                if !ok {
                    self.pattern_mismatch(p.span, ty, "a literal");
                }
            }
            PatKind::Variant {
                enum_,
                name,
                name_span,
                args,
            } => {
                let eid = match &inner {
                    Ty::Enum(id, _) => *id,
                    Ty::Error => return self.wild_subpatterns(args),
                    _ => {
                        self.pattern_mismatch(p.span, ty, "a variant");
                        return self.wild_subpatterns(args);
                    }
                };
                if let Some(named) = enum_
                    && *named != eid
                {
                    self.pattern_mismatch(
                        p.span,
                        ty,
                        &format!("a `{}`", self.program.enum_def(*named).name),
                    );
                    return self.wild_subpatterns(args);
                }
                let def = self.program.enum_def(eid);
                let Some((vi, v)) = def.variant(name) else {
                    let names: Vec<&str> = def.variants.iter().map(|v| v.name.as_str()).collect();
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!("enum `{}` has no variant `{name}`", def.name),
                        )
                        .primary(*name_span, "unknown variant")
                        .note(format!("its variants are: {}", names.join(", "))),
                    );
                    return self.wild_subpatterns(args);
                };
                self.types.variants.insert(p.id, (eid, vi));
                let field_tys: Vec<(String, Ty)> = v
                    .fields
                    .iter()
                    .map(|f| (f.name.clone(), f.ty.clone()))
                    .collect();
                match args {
                    VariantPatArgs::None => {}
                    VariantPatArgs::Positional(ps) => {
                        if ps.len() != field_tys.len() {
                            self.diags.push(
                                Diagnostic::error(
                                    codes::WRONG_LENGTH,
                                    format!(
                                        "`{name}` has {} field{}, but the pattern has {}",
                                        field_tys.len(),
                                        if field_tys.len() == 1 { "" } else { "s" },
                                        ps.len()
                                    ),
                                )
                                .primary(p.span, "wrong number of fields")
                                .help("use `_` for fields you don't need"),
                            );
                        }
                        for (i, sub) in ps.iter().enumerate() {
                            match field_tys.get(i) {
                                Some((_, t)) => self.check_pattern(sub, t),
                                None => self.check_pattern(sub, &Ty::Error),
                            }
                        }
                    }
                    VariantPatArgs::Named(fps) => {
                        let what = format!("`{name}`");
                        self.field_patterns(p, fps, &field_tys, &what);
                    }
                }
            }
            PatKind::Struct { sid, fields } => {
                let st = self.program.struct_ty(*sid);
                if let (Ty::Class(a, _, _), Ty::Class(b, _, _)) = (&st, &inner)
                    && a != b
                    && self.program.is_subclass(*a, *b)
                {
                    self.types.downcasts.insert(p.id);
                } else if inner != st && !inner.is_error() {
                    self.pattern_mismatch(p.span, ty, &format!("a `{st}`"));
                    for f in fields {
                        self.check_pattern(&f.pat, &Ty::Error);
                    }
                    return;
                }
                let def = self.program.struct_def(*sid);
                let field_tys: Vec<(String, Ty)> = def
                    .fields
                    .iter()
                    .map(|f| {
                        (
                            f.name.clone(),
                            f.ty.subst(Ty::type_owner(*sid), inner.type_args()),
                        )
                    })
                    .collect();
                let what = format!("`{}`", def.name);
                self.field_patterns(p, fields, &field_tys, &what);
            }
        }
    }

    /// `[a, b]`, `[first, ...rest]` on a tuple, fixed array or growable array.
    fn slice_pattern(
        &mut self,
        p: &Pattern,
        ty: &Ty,
        inner: &Ty,
        prefix: &[Pattern],
        rest: Option<Option<LocalId>>,
        suffix: &[Pattern],
    ) {
        let n = prefix.len() + suffix.len();
        let subs: Vec<&Pattern> = prefix.iter().chain(suffix).collect();
        let fail = |c: &mut Self| {
            for sub in &subs {
                c.check_pattern(sub, &Ty::Error);
            }
            if let Some(Some(l)) = rest {
                c.bind_local(l, &Ty::Error, p.span);
            }
        };
        match inner {
            Ty::Array(t) => {
                for sub in &subs {
                    self.check_pattern(sub, t);
                }
                if let Some(Some(l)) = rest {
                    self.bind_local(l, inner, p.span);
                }
            }
            Ty::Tuple(ts) if rest.is_none() && ts.len() == n => {
                for (sub, t) in subs.iter().zip(ts) {
                    self.check_pattern(sub, t);
                }
            }
            Ty::Fixed(t, len) if rest.is_none() && *len as usize == n => {
                for sub in &subs {
                    self.check_pattern(sub, t);
                }
            }
            Ty::Tuple(_) | Ty::Fixed(..) => {
                let len = match inner {
                    Ty::Tuple(ts) => ts.len(),
                    Ty::Fixed(_, n) => *n as usize,
                    _ => unreachable!(),
                };
                let msg = if rest.is_some() {
                    format!("`...rest` only works on growable arrays, not `{inner}`")
                } else {
                    format!("`{inner}` has {len} elements, but the pattern has {n}")
                };
                self.diags.push(
                    Diagnostic::error(codes::WRONG_LENGTH, msg)
                        .primary(p.span, "wrong number of elements"),
                );
                fail(self);
            }
            other => {
                if !other.is_error() {
                    self.pattern_mismatch(p.span, ty, "an array");
                }
                fail(self);
            }
        }
    }

    /// Types a pattern binding. In an or-pattern every alternative binds the same local, so
    /// they must agree.
    fn bind_local(&mut self, l: LocalId, t: &Ty, span: Span) {
        let f = self.stack.last().unwrap().id;
        let cur = &self.types.locals[f.0 as usize][l.0 as usize];
        if !cur.is_error() && cur != t && !t.is_error() {
            let prev = cur.clone();
            self.diags.push(
                Diagnostic::error(
                    codes::MISMATCHED_TYPES,
                    "a binding has different types in different alternatives",
                )
                .primary(
                    span,
                    format!("`{t}` here, `{prev}` in an earlier alternative"),
                ),
            );
            return;
        }
        self.set_var_ty(Var::Local(l), t.clone());
    }

    fn field_patterns(
        &mut self,
        p: &Pattern,
        fps: &[FieldPat],
        fields: &[(String, Ty)],
        what: &str,
    ) {
        let mut idx = Vec::new();
        for fp in fps {
            match fields.iter().position(|(n, _)| *n == fp.name) {
                Some(i) => {
                    idx.push(i as u32);
                    self.check_pattern(&fp.pat, &fields[i].1);
                }
                None => {
                    idx.push(u32::MAX);
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!("{what} has no field `{}`", fp.name),
                        )
                        .primary(fp.name_span, "unknown field"),
                    );
                    self.check_pattern(&fp.pat, &Ty::Error);
                }
            }
        }
        self.types.pat_fields.insert(p.id, idx);
    }

    /// Types the bindings of sub-patterns of a pattern that failed to check.
    fn wild_subpatterns(&mut self, args: &VariantPatArgs) {
        match args {
            VariantPatArgs::None => {}
            VariantPatArgs::Positional(ps) => {
                for sub in ps {
                    self.check_pattern(sub, &Ty::Error);
                }
            }
            VariantPatArgs::Named(fps) => {
                for f in fps {
                    self.check_pattern(&f.pat, &Ty::Error);
                }
            }
        }
    }

    /// A checked pattern, reduced for the exhaustiveness checker: one entry per alternative
    /// of an or-pattern (expanded through nested patterns, capped).
    fn to_pat(&self, p: &Pattern, ty: &Ty) -> Vec<Pat> {
        const CAP: usize = 256;
        if let (Some(i), Ty::Union(_, ms)) = (self.types.union_tests.get(&p.id), ty) {
            let m = &ms[*i as usize];
            return self
                .to_pat(p, m)
                .into_iter()
                .map(|x| Pat::Ctor(Ctor::Variant(*i), vec![x]))
                .collect();
        }
        let inner = match ty {
            Ty::Nullable(t) if p.looks_inside(ty) => (**t).clone(),
            _ => ty.clone(),
        };
        let leaf = |c: Ctor| vec![Pat::Ctor(c, Vec::new())];
        // All combinations of the sub-patterns' alternatives.
        let product = |cols: Vec<Vec<Pat>>, c: Ctor| -> Vec<Pat> {
            let mut acc: Vec<Vec<Pat>> = vec![Vec::new()];
            for col in cols {
                let mut next = Vec::new();
                for prefix in &acc {
                    for alt in &col {
                        if next.len() >= CAP {
                            break;
                        }
                        let mut v = prefix.clone();
                        v.push(alt.clone());
                        next.push(v);
                    }
                }
                acc = next;
            }
            acc.into_iter()
                .map(|args| Pat::Ctor(c.clone(), args))
                .collect()
        };
        let struct_fields = |sid: StructId| -> Vec<Ty> {
            self.program
                .struct_def(sid)
                .fields
                .iter()
                .map(|f| f.ty.subst(Ty::type_owner(sid), inner.type_args()))
                .collect()
        };
        let pats = match &p.kind {
            // A runtime class check never covers the base type on its own.
            _ if self.types.downcasts.contains(&p.id) => {
                leaf(Ctor::Range(format!("downcast#{}", p.id.0)))
            }
            PatKind::Wild | PatKind::Bind(_) => return vec![Pat::Wild],
            PatKind::Or(alts) => {
                return alts
                    .iter()
                    .flat_map(|a| self.to_pat(a, ty))
                    .take(CAP)
                    .collect();
            }
            PatKind::As(_, sub) => return self.to_pat(sub, ty),
            PatKind::Lit(l) => match l {
                LitPat::Null => leaf(Ctor::Null),
                LitPat::Bool(b) => leaf(Ctor::Bool(*b)),
                LitPat::Int(v) if inner.is_int() => leaf(Ctor::Int(*v)),
                LitPat::Int(v) => leaf(Ctor::Float((*v as f64).to_bits())),
                LitPat::Float(f) => leaf(Ctor::Float(f.to_bits())),
                LitPat::Str(s) => leaf(Ctor::Str(s.clone())),
            },
            PatKind::Range(lo, hi) => leaf(Ctor::Range(format!("{lo:?}..{hi:?}"))),
            PatKind::Variant { args, .. } => {
                let Some(&(eid, vi)) = self.types.variants.get(&p.id) else {
                    return vec![Pat::Wild];
                };
                let tys: Vec<Ty> = self.program.enum_def(eid).variants[vi as usize]
                    .fields
                    .iter()
                    .map(|f| f.ty.clone())
                    .collect();
                let mut cols: Vec<Vec<Pat>> = vec![vec![Pat::Wild]; tys.len()];
                match args {
                    VariantPatArgs::None => {}
                    VariantPatArgs::Positional(ps) => {
                        for (i, (sub, t)) in ps.iter().zip(&tys).enumerate() {
                            cols[i] = self.to_pat(sub, t);
                        }
                    }
                    VariantPatArgs::Named(fps) => self.named_subpats(p, fps, &tys, &mut cols),
                }
                product(cols, Ctor::Variant(vi))
            }
            PatKind::Struct { sid, fields: fps } => {
                let tys = struct_fields(*sid);
                let mut cols = vec![vec![Pat::Wild]; tys.len()];
                self.named_subpats(p, fps, &tys, &mut cols);
                product(cols, Ctor::Single)
            }
            PatKind::Type {
                ty: Ty::Struct(sid, _, _) | Ty::Class(sid, _, _),
                fields: Some(fps),
                ..
            } => {
                let tys = struct_fields(*sid);
                let mut cols = vec![vec![Pat::Wild]; tys.len()];
                self.named_subpats(p, fps, &tys, &mut cols);
                product(cols, Ctor::Single)
            }
            PatKind::Type { .. } => vec![Pat::Wild],
            PatKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let n = prefix.len() + suffix.len();
                let elem_tys: Vec<Ty> = match &inner {
                    Ty::Tuple(ts) if ts.len() == n && rest.is_none() => ts.clone(),
                    Ty::Fixed(t, len) if *len as usize == n && rest.is_none() => {
                        vec![(**t).clone(); n]
                    }
                    Ty::Array(t) => vec![(**t).clone(); prefix.len() + suffix.len()],
                    _ => return vec![Pat::Wild],
                };
                let cols: Vec<Vec<Pat>> = prefix
                    .iter()
                    .chain(suffix)
                    .zip(&elem_tys)
                    .map(|(sub, t)| self.to_pat(sub, t))
                    .collect();
                let c = match &inner {
                    Ty::Array(_) => Ctor::Slice {
                        prefix: prefix.len(),
                        suffix: suffix.len(),
                        rest: rest.is_some(),
                    },
                    _ => Ctor::Single,
                };
                product(cols, c)
            }
        };
        if p.looks_inside(ty) {
            pats.into_iter()
                .map(|x| Pat::Ctor(Ctor::NonNull, vec![x]))
                .collect()
        } else {
            pats
        }
    }

    fn named_subpats(&self, p: &Pattern, fps: &[FieldPat], tys: &[Ty], cols: &mut [Vec<Pat>]) {
        let Some(idx) = self.types.pat_fields.get(&p.id) else {
            return;
        };
        for (fp, i) in fps.iter().zip(idx) {
            if let Some(t) = tys.get(*i as usize) {
                cols[*i as usize] = self.to_pat(&fp.pat, t);
            }
        }
    }

    /// A closure: its captures take the enclosing function's types, untyped parameters the
    /// expected function type's, then the body is checked.
    fn closure(&mut self, e: &Expr, id: FnId, want: Option<&Expect>) -> Ty {
        let f = self.program.func(id);
        let info = f.closure.as_ref().expect("a closure");
        let outer = self.stack.last().expect("inside a function").id;
        for c in &info.captures {
            let t = self.types.locals[outer.0 as usize][c.outer.0 as usize].clone();
            self.types.locals[id.0 as usize][c.inner.0 as usize] = t;
        }
        // (Also where a nullable function is expected: `w.onmessage = (m) => …`.)
        let expected = match want.map(|w| match &w.ty {
            Ty::Nullable(t) => &**t,
            t => t,
        }) {
            Some(Ty::Fn(ps, r, ts)) if ps.len() == f.params.len() => {
                Some((ps.clone(), r.clone(), ts.clone()))
            }
            // A union with one function member of this arity (`string | (m: M) => string`).
            Some(Ty::Union(_, ms)) => {
                let fns: Vec<&Ty> = ms
                    .iter()
                    .filter(|m| matches!(m, Ty::Fn(ps, ..) if ps.len() == f.params.len()))
                    .collect();
                match fns.as_slice() {
                    [Ty::Fn(ps, r, ts)] => Some((ps.clone(), r.clone(), ts.clone())),
                    _ => None,
                }
            }
            _ => None,
        };
        for (i, p) in f.params.iter().enumerate() {
            if f.local(*p).ty.is_some() {
                continue;
            }
            let t = match &expected {
                Some((ps, _, _)) => ps[i].clone(),
                None => {
                    let b = f.local(*p);
                    self.diags.push(
                        Diagnostic::error(
                            codes::MISSING_PARAM_TYPE,
                            format!("parameter `{}` needs a type", b.name),
                        )
                        .primary(b.span, "add a type annotation, e.g. `: i32`")
                        .note("an arrow's parameter types can come from where it's passed, but nothing says here"),
                    );
                    Ty::Error
                }
            };
            self.types.locals[id.0 as usize][p.0 as usize] = t.clone();
            self.types.sigs[id.0 as usize].params[i] = t;
        }
        if f.generator {
            // `T?`: what each step yields, `null` at the end.
            self.types.sigs[id.0 as usize].ret = match &expected {
                Some((_, r, _)) => (**r).clone(),
                None => {
                    self.diags.push(
                        Diagnostic::error(
                            codes::MISMATCHED_TYPES,
                            "a generator needs its `Iterator<T>` return type",
                        )
                        .primary(e.span, "declare `: Iterator<T>`"),
                    );
                    Ty::Error
                }
            };
        }
        // An untyped lambda's result type comes from the expected one, when that's known
        // (`void`: its value is discarded, as in TS).
        if f.ret.is_none()
            && !f.generator
            && let Some((_, r, _)) = &expected
            && !r.to_string().contains("{error}")
        {
            self.ret_hints.insert(id, (**r).clone());
            self.types.sigs[id.0 as usize].ret = (**r).clone();
        }
        // Checked right here (it sees the enclosing function's current narrowings too).
        let saved = std::mem::take(&mut self.narrow);
        self.inherit_unsafe = true;
        self.check_fn(id);
        self.narrow = saved;
        let sig = self.types.sigs[id.0 as usize].clone();
        let inferred = self.types.fn_throws[id.0 as usize].clone();
        let throws: std::sync::Arc<[Ty]> = match &expected {
            // Generic over what it throws (or not known yet): what it throws decides.
            Some((_, _, et)) if et.iter().any(|t| matches!(t, Ty::Param(..) | Ty::Error)) => {
                inferred.into()
            }
            Some((_, _, et)) => {
                for t in &inferred {
                    let ok = et.iter().any(|d| match (d, t) {
                        (Ty::Class(d, ..), Ty::Class(c, ..)) => self.program.is_subclass(*c, *d),
                        (d, t) => d == t,
                    });
                    let name = &self.program.func(id).name;
                    if !ok
                        && let Some(f) = name
                            .strip_prefix("<async ")
                            .or_else(|| name.strip_prefix("<async* "))
                            .and_then(|n| n.strip_suffix('>'))
                    {
                        // An async function's body (spec §7.4): what it throws travels with
                        // its promise, so it must be declared.
                        self.diags.push(
                            Diagnostic::error(
                                codes::UNHANDLED_ERROR,
                                format!("unhandled error `{t}`"),
                            )
                            .primary(self.program.func(id).name_span, format!("`{f}` can throw `{t}`"))
                            .help(format!("catch it, or add `throws {t}` to `{f}` (its promise rejects with it)")),
                        );
                    } else if !ok {
                        let want = want.map(|w| w.ty.to_string()).unwrap_or_default();
                        self.diags.push(
                            Diagnostic::error(
                                codes::UNHANDLED_ERROR,
                                format!("unhandled error `{t}`"),
                            )
                            .primary(e.span, format!("this lambda can throw `{t}`"))
                            .note(format!("it's used as `{want}`, which doesn't throw `{t}`"))
                            .help("catch the error inside the lambda"),
                        );
                    }
                }
                et.clone()
            }
            None => inferred.into(),
        };
        Ty::Fn(sig.params.into(), Box::new(sig.ret), throws)
    }

    /// `tailcall f(args)`: only as the result of the function, calling a function with the
    /// same parameter and return types (so the call can reuse the frame).
    fn tailcall(&mut self, e: &Expr, inner: &Expr, want: Option<&Expect>) -> Ty {
        let in_tail = std::mem::take(&mut self.tail_position);
        let t = self.infer_with(inner, want);
        let target = match &inner.kind {
            ExprKind::Call { callee, .. } => match &callee.kind {
                ExprKind::Fn(id) => Some(*id),
                ExprKind::Overloaded(_) => self.types.overloads.get(&inner.id).copied(),
                _ => None,
            },
            _ => None,
        };
        let me = self.stack.last().expect("inside a function").id;
        let problem = match target {
            _ if !in_tail => Some(
                "`tailcall` must be the function's result (`return tailcall f(…)`)".to_string(),
            ),
            None => Some("`tailcall` needs a direct call of a named function".to_string()),
            Some(id) => {
                let (a, b) = (
                    &self.types.sigs[id.0 as usize],
                    &self.types.sigs[me.0 as usize],
                );
                let f = self.program.func(id);
                let g = self.program.func(me);
                if a.params != b.params || a.ret != b.ret {
                    Some(format!(
                        "`{}` must take and return the same types as this function",
                        f.name
                    ))
                } else if !f.generics.is_empty()
                    || f.rest
                    || f.defaults.iter().any(Option::is_some)
                    || g.rest
                    || g.defaults.iter().any(Option::is_some)
                    || f.this.is_some()
                    || g.this.is_some()
                    || f.closure.is_some()
                    || g.closure.is_some()
                {
                    Some("`tailcall` works between plain functions without defaults or rest parameters".to_string())
                } else {
                    None
                }
            }
        };
        if let Some(msg) = problem {
            self.diags.push(
                Diagnostic::error(codes::BAD_TAILCALL, msg)
                    .primary(e.span, "can't be a guaranteed tail call"),
            );
        }
        t
    }

    /// `a ?? b`: `b` is used when `a` is null.
    fn nullish(&mut self, l: &Expr, r: &Expr, want: Option<&Expect>) -> Ty {
        let lt = match want {
            Some(w) if !matches!(w.ty, Ty::Void | Ty::Nullable(_)) => {
                self.infer_with(l, Some(&Expect::new(w.ty.clone().nullable())))
            }
            _ => self.infer(l),
        };
        match lt {
            Ty::Nullable(inner) => {
                // `a ?? b` with a nullable `b` stays nullable.
                let rt = self.infer_with(r, Some(&Expect::new((*inner).clone())));
                if rt == *inner || rt == Ty::Never || rt.is_error() {
                    return *inner;
                }
                if rt.widens_to(&inner) {
                    self.types.coercions.insert(r.id, (*inner).clone());
                    return *inner;
                }
                let n = Ty::Nullable(inner);
                if rt.widens_to(&n) {
                    if rt != n {
                        self.types.coercions.insert(r.id, n.clone());
                    }
                    return n;
                }
                let want = match &n {
                    Ty::Nullable(t) => (**t).clone(),
                    t => t.clone(),
                };
                self.mismatch(r.span, &Expect::new(want), &rt);
                Ty::Error
            }
            Ty::Error => {
                self.infer(r);
                Ty::Error
            }
            Ty::Null => self.infer(r),
            t => {
                // Not nullable: `b` is never used, but it still has to fit.
                self.check(r, &Expect::new(t.clone()));
                t
            }
        }
    }

    /// `t[i]` on a tuple: `i` must be an integer literal below the tuple's length.
    fn tuple_index(&mut self, ts: &[Ty], index: &Expr) -> Ty {
        match index.kind {
            ExprKind::Int(v, _) if (v as usize) < ts.len() => {
                self.set(index, Ty::INT);
                ts[v as usize].clone()
            }
            ExprKind::Int(v, _) => {
                self.set(index, Ty::INT);
                self.diags.push(
                    Diagnostic::error(
                        codes::TUPLE_INDEX,
                        format!("tuple index {v} is out of range"),
                    )
                    .primary(
                        index.span,
                        format!(
                            "the tuple has {} element{}",
                            ts.len(),
                            if ts.len() == 1 { "" } else { "s" }
                        ),
                    ),
                );
                Ty::Error
            }
            _ => {
                self.infer(index);
                self.diags.push(
                    Diagnostic::error(codes::TUPLE_INDEX, "tuple index must be a constant")
                        .primary(index.span, "not an integer literal")
                        .note("each tuple element has its own type, so the index must be known")
                        .help("use an integer literal, `t[0]`, or destructure: `const [a, b] = t`"),
                );
                Ty::Error
            }
        }
    }

    fn wrong_length(&mut self, e: &Expr, want: &Expect, expected: usize, found: usize) {
        let mut d = Diagnostic::error(
            codes::WRONG_LENGTH,
            format!(
                "expected {expected} element{}, found {found}",
                if expected == 1 { "" } else { "s" }
            ),
        )
        .primary(e.span, format!("this literal has {found}"));
        if let Some(o) = want.origin {
            d = d.secondary(o, format!("`{}` expected because of this", want.ty));
        }
        self.diags.push(d);
    }

    /// `[a, b, c]`: a tuple, fixed array or growable array as the expected type says;
    /// without one, a growable array whose element type comes from the elements.
    fn array_lit(&mut self, e: &Expr, elems: &[Expr], want: Option<&Expect>) -> Ty {
        let want = want.map(|w| match &w.ty {
            Ty::Nullable(inner) => Expect::from((**inner).clone(), w.origin),
            _ => w.clone(),
        });
        // A union with one array-like member (`string | [string, string][]`): the literal
        // is that member.
        let want = want.map(|w| match &w.ty {
            Ty::Union(_, ms) => {
                let arrays: Vec<&Ty> = ms
                    .iter()
                    .filter(|m| matches!(m, Ty::Array(_) | Ty::Tuple(_) | Ty::Fixed(..)))
                    .collect();
                match arrays.as_slice() {
                    [one] => Expect::from((*one).clone(), w.origin),
                    _ => w,
                }
            }
            _ => w,
        });
        match want.as_ref().map(|w| (&w.ty, w)) {
            Some((Ty::Tuple(ts), w)) => {
                if ts.len() != elems.len() {
                    self.wrong_length(e, w, ts.len(), elems.len());
                    for x in elems {
                        self.infer(x);
                    }
                    return Ty::Error;
                }
                for (x, t) in elems.iter().zip(ts) {
                    self.check(x, &Expect::new(t.clone()));
                }
                w.ty.clone()
            }
            Some((Ty::Fixed(t, n), w)) => {
                if *n as usize != elems.len() {
                    self.wrong_length(e, w, *n as usize, elems.len());
                }
                for x in elems {
                    self.check(x, &Expect::new((**t).clone()));
                }
                w.ty.clone()
            }
            Some((Ty::Array(t), w)) => {
                for x in elems {
                    self.check(x, &Expect::new((**t).clone()));
                }
                w.ty.clone()
            }
            _ => {
                if elems.is_empty() {
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_ELEMENT_TYPE,
                            "can't infer the element type of an empty array",
                        )
                        .primary(e.span, "no elements and no expected type")
                        .help("annotate the binding: `const xs: i32[] = []`"),
                    );
                    return Ty::Error;
                }
                // The first element that isn't an untyped literal decides the type, so
                // `[1, x]` takes `x`'s type; all literals: `int`, or `number` with a float.
                let elem = match elems.iter().find(|x| !untyped_lit(x)) {
                    Some(x) => self.infer(x),
                    None if elems.iter().any(is_float_lit) => Ty::NUMBER,
                    None => Ty::INT,
                };
                if elem.is_error() {
                    for x in elems {
                        self.infer(x);
                    }
                    return Ty::Error;
                }
                let want = Expect::new(elem.clone());
                for x in elems {
                    self.check(x, &want);
                }
                Ty::Array(Box::new(elem))
            }
        }
    }

    /// `[value; count]`: a fixed array.
    fn repeat_lit(&mut self, e: &Expr, value: &Expr, count: u64, want: Option<&Expect>) -> Ty {
        match want.map(|w| (&w.ty, w)) {
            Some((Ty::Fixed(t, n), w)) => {
                if *n != count {
                    self.wrong_length(e, w, *n as usize, count as usize);
                }
                self.check(value, &Expect::new((**t).clone()));
                w.ty.clone()
            }
            _ => {
                let t = self.infer(value);
                if t.is_error() {
                    return Ty::Error;
                }
                Ty::Fixed(Box::new(t), count)
            }
        }
    }

    /// `start..end`: both bounds join to one integer type.
    fn range(&mut self, e: &Expr, start: &Expr, end: &Expr, want: Option<&Expect>) -> Ty {
        let hint = want.and_then(|w| match &w.ty {
            Ty::Range(it) => Some(Expect::new(Ty::Int(*it))),
            _ => None,
        });
        let (lt, rt) = self.operands(start, end, hint.as_ref());
        let t = if lt == rt || rt.is_error() {
            lt
        } else if lt.is_error() {
            rt
        } else if rt.widens_to(&lt) {
            self.types.coercions.insert(end.id, lt.clone());
            lt
        } else if lt.widens_to(&rt) {
            self.types.coercions.insert(start.id, rt.clone());
            rt
        } else {
            let mut d = Diagnostic::error(
                codes::MISMATCHED_TYPES,
                "the bounds of a range have different types",
            )
            .secondary(start.span, format!("`{lt}`"))
            .secondary(end.span, format!("`{rt}`"))
            .help("convert one side explicitly with `as`");
            d.labels[1].primary = true;
            self.diags.push(d);
            return Ty::Error;
        };
        match t {
            Ty::Int(it) => Ty::Range(it),
            Ty::Error => Ty::Error,
            other => {
                self.invalid_operand(e.span, "`..`", &other, "integer bounds");
                Ty::Error
            }
        }
    }

    /// `T` when an assignment target holds a reactive cell (a write, spec §16.2). Looks at
    /// the target's type without checking it (`write_cell` and `target_ty` do).
    fn reactive_target(&mut self, t: &Target, span: Span) -> Option<Ty> {
        let ty = match t {
            Target::Var(v) => {
                let span = self.var_binding(*v).span;
                self.var_ty(*v, span)
            }
            Target::Field { object, name, .. } => {
                let ot = self.peek_type(object)?.strip_immutable();
                let sid = match ot {
                    Ty::Class(sid, _, _) | Ty::Struct(sid, _, _) => sid,
                    _ => return None,
                };
                let def = self.program.struct_def(sid);
                let (_, f) = def.field(name)?;
                let args = ot.type_args().to_vec();
                f.ty.subst(Ty::type_owner(sid), &args)
            }
            Target::Index { .. } => return None,
        };
        self.reactive_arg(&ty, span)
    }

    /// `__reactive(v)`: a field initializer storing a new cell (not a write of one).
    fn creates_cell(&self, value: &Expr) -> bool {
        let ExprKind::Call { callee, .. } = &value.kind else {
            return false;
        };
        matches!(&callee.kind, ExprKind::Fn(id)
            if self.program.func(*id).name == "__reactive"
                && self.program.prelude_files.contains(&self.program.func(*id).span.file))
    }

    /// Records a write of a reactive cell, checking the target (and that it isn't a read-only
    /// `computed`).
    fn write_cell(&mut self, e: &Expr, target: &Target) {
        self.target_ty(target);
        self.types.reactive_writes.insert(e.id);
        self.types.uses_reactivity = true;
        if let Target::Var(v) = target
            && self.readonly_cells.contains(v)
        {
            let b = self.var_binding(*v);
            let (name, decl) = (b.name.clone(), b.span);
            self.diags.push(
                Diagnostic::error(
                    codes::REACTIVE,
                    format!("`{name}` is a read-only `computed`"),
                )
                .primary(e.span, "can't be assigned")
                .secondary(decl, "declared with `computed(getter)` here")
                .help("pass `{ get, set }` to `computed` to make it writable"),
            );
        }
    }

    /// Assignments to a `const` binding (other than writes of a reactive cell, which the
    /// resolver can't tell apart).
    fn check_assignable(&mut self, t: &Target, span: Span) {
        let Target::Var(v) = t else {
            return;
        };
        let b = self.var_binding(*v);
        if b.mutable {
            return;
        }
        let (name, decl) = (b.name.clone(), b.span);
        self.diags.push(
            Diagnostic::error(codes::ASSIGN_TO_CONST, format!("cannot assign to `{name}`"))
                .primary(span, "assignment to a constant")
                .secondary(decl, "declared with `const` here")
                .help(format!(
                    "declare it with `let` to reassign it: `let {name} = …`"
                )),
        );
    }

    /// The type of an assignment target, and the span that declared it.
    fn target_ty(&mut self, t: &Target) -> (Ty, Option<Span>) {
        match t {
            Target::Var(v) => {
                let span = self.var_binding(*v).span;
                (self.var_ty(*v, span), Some(span))
            }
            Target::Field {
                id,
                object,
                name,
                name_span,
            } => {
                let ot = self.infer(object);
                if self.through_immutable(object) {
                    self.immutable_mutation(*name_span, "assign a field");
                }
                let ot = ot.strip_immutable();
                self.check_readonly_field(&ot, name, *name_span);
                self.recv_args = ot.type_args().to_vec();
                match ot {
                    Ty::Struct(sid, _, _) => {
                        let t = self.field(*id, sid, name, *name_span);
                        let span = self
                            .program
                            .struct_def(sid)
                            .field(name)
                            .map(|(_, f)| f.span);
                        (t, span)
                    }
                    Ty::Class(sid, _, _) => {
                        let def = self.program.struct_def(sid);
                        if def.field(name).is_none()
                            && let Some(f) = def.setter(name)
                        {
                            self.check_access(sid, name, *name_span);
                            self.types.setters.insert(*id, f);
                            let t = self.types.sigs[f.0 as usize]
                                .params
                                .first()
                                .cloned()
                                .unwrap_or(Ty::Error);
                            return (t, None);
                        }
                        if def.field(name).is_none() && def.getter(name).is_some() {
                            self.diags.push(
                                Diagnostic::error(
                                    codes::ASSIGN_TO_CONST,
                                    format!("`{name}` has a getter but no setter"),
                                )
                                .primary(*name_span, "can't be assigned"),
                            );
                            return (Ty::Error, None);
                        }
                        let t = self.field(*id, sid, name, *name_span);
                        let span = def.field(name).map(|(_, f)| f.span);
                        (t, span)
                    }
                    Ty::Error => (Ty::Error, None),
                    other => {
                        self.diags.push(
                            Diagnostic::error(
                                codes::UNKNOWN_MEMBER,
                                format!("`{other}` has no field `{name}`"),
                            )
                            .primary(*name_span, "unknown field"),
                        );
                        (Ty::Error, None)
                    }
                }
            }
            Target::Index { object, index } => {
                let ot = self.infer(object);
                if self.through_immutable(object) {
                    self.immutable_mutation(object.span, "assign an element");
                }
                let ot = ot.strip_immutable();
                match ot {
                    Ty::Array(t) | Ty::Fixed(t, _) | Ty::FixedN(t, _) => {
                        self.check(index, &Expect::new(Ty::INT));
                        (*t, None)
                    }
                    Ty::Tuple(ts) => (self.tuple_index(&ts, index), None),
                    Ty::Error => {
                        self.infer(index);
                        (Ty::Error, None)
                    }
                    Ty::Str => {
                        self.infer(index);
                        self.diags.push(
                            Diagnostic::error(codes::INVALID_OPERANDS, "strings are immutable")
                                .primary(object.span, "can't assign to a character of a string")
                                .help("build a new string instead, e.g. with a template"),
                        );
                        (Ty::Error, None)
                    }
                    other => {
                        self.infer(index);
                        self.diags.push(
                            Diagnostic::error(
                                codes::INVALID_OPERANDS,
                                format!("`{other}` can't be indexed"),
                            )
                            .primary(object.span, format!("has type `{other}`")),
                        );
                        (Ty::Error, None)
                    }
                }
            }
        }
    }

    /// A class member read (`obj.name`): a field or a getter.
    fn class_member(&mut self, id: ExprId, sid: StructId, name: &str, span: Span) -> Ty {
        let def = self.program.struct_def(sid);
        if def.field(name).is_none()
            && let Some(g) = def.getter(name)
        {
            self.check_access(sid, name, span);
            self.types.getters.insert(id, g);
            let f = self.program.func(g);
            if f.ret.is_none() && self.state[g.0 as usize] == FnState::Unchecked {
                self.check_fn(g);
            }
            // A generic class's getter: its result in the receiver's type arguments.
            let args = std::mem::take(&mut self.recv_args);
            return self.types.sigs[g.0 as usize]
                .ret
                .subst(Ty::type_owner(sid), &args);
        }
        self.field(id, sid, name, span)
    }

    /// Reports use of a private class member outside the class.
    fn check_access(&mut self, sid: StructId, name: &str, span: Span) {
        let def = self.program.struct_def(sid);
        if !def.is_class || def.is_public(name) {
            return;
        }
        // Private: only the declaring class. Protected: it and its subclasses.
        let declared = def.declared_in.get(name).copied().unwrap_or(sid);
        let owner = self.func().owner;
        let allowed = match owner {
            Some(o) if def.protected.contains(name) => self.program.is_subclass(o, declared),
            Some(o) => o == declared,
            None => false,
        };
        if allowed {
            return;
        }
        let what = if def.protected.contains(name) {
            "protected"
        } else {
            "private"
        };
        let dname = self.program.struct_def(declared).name.clone();
        if what == "protected" {
            self.diags.push(
                Diagnostic::error(
                    codes::PRIVATE_MEMBER,
                    format!("`{name}` is protected in `{dname}`"),
                )
                .primary(span, "used outside the class and its subclasses"),
            );
            return;
        }
        let def = self.program.struct_def(declared);
        self.diags.push(
            Diagnostic::error(
                codes::PRIVATE_MEMBER,
                format!("`{name}` is private to `{}`", def.name),
            )
            .primary(span, "used outside the class")
            .secondary(def.name_span, format!("`{}` is declared here", def.name))
            .help(format!(
                "class members are private by default; declare it `public {name}` to use it here"
            )),
        );
    }

    /// Looks up a struct field for `object.name` and records its index under `id`.
    fn field(&mut self, id: ExprId, sid: StructId, name: &str, span: Span) -> Ty {
        let args = std::mem::take(&mut self.recv_args);
        let def = self.program.struct_def(sid);
        match def.field(name) {
            Some((i, f)) => {
                let t = f.ty.subst(Ty::type_owner(sid), &args);
                self.types.fields.insert(id, i as u32);
                self.check_access(sid, name, span);
                t
            }
            None => {
                let mut d = Diagnostic::error(
                    codes::UNKNOWN_MEMBER,
                    format!("`{}` has no field `{name}`", def.name),
                )
                .primary(span, "unknown field")
                .secondary(def.name_span, format!("`{}` is declared here", def.name));
                if def.method(name).is_some() {
                    d = d.help(format!("`{name}` is a method; call it: `{name}()`"));
                } else if !def.fields.is_empty() {
                    let names: Vec<&str> = def.fields.iter().map(|f| f.name.as_str()).collect();
                    d = d.note(format!("its fields are: {}", names.join(", ")));
                }
                self.diags.push(d);
                Ty::Error
            }
        }
    }

    /// A struct literal: every field exactly once.
    fn object(
        &mut self,
        e: &Expr,
        ty: Option<StructId>,
        fields: &[FieldInit],
        type_args: &[Ty],
        want: Option<&Expect>,
    ) -> Ty {
        if ty.is_none()
            && let Some(Ty::Enum(eid, ename)) = want.map(|w| &w.ty)
            && let crate::hir::EnumOrigin::Discriminated(disc) = &self.program.enum_def(*eid).origin
        {
            return self.discriminated_object(e, *eid, ename, disc, fields);
        }
        // A union with one struct member (`Duration | DurationParts`): the literal is that
        // member.
        let union_want = match want.map(|w| (&w.ty, w)) {
            Some((Ty::Union(_, ms), w)) => {
                let structs: Vec<&Ty> = ms.iter().filter(|m| matches!(m, Ty::Struct(..))).collect();
                match structs.as_slice() {
                    [one] => Some(Expect::from((*one).clone(), w.origin)),
                    _ => None,
                }
            }
            _ => None,
        };
        let want = union_want.as_ref().or(want);
        let sid = match (ty, want.map(|w| &w.ty)) {
            (Some(s), _) => s,
            (None, Some(Ty::Struct(s, _, _))) => *s,
            // (`{ … }` where a `S?` is expected builds an `S`.)
            (None, Some(Ty::Nullable(inner))) if matches!(**inner, Ty::Struct(..)) => {
                let Ty::Struct(s, _, _) = &**inner else {
                    unreachable!("checked")
                };
                *s
            }
            _ => {
                for f in fields {
                    self.infer(&f.value);
                }
                self.diags.push(
                    Diagnostic::error(codes::UNKNOWN_OBJECT_TYPE, "can't tell which struct this object literal builds")
                        .primary(e.span, "no struct type is expected here")
                        .help("name the type: `Point { x: 1, y: 2 }`, or annotate the binding: `const p: Point = { … }`"),
                );
                return Ty::Error;
            }
        };
        let def = self.program.struct_def(sid);
        // A generic struct's type arguments: given, expected, or inferred from the fields.
        let owner = Ty::type_owner(sid);
        let targs: Vec<Ty> = if def.generics.is_empty() {
            Vec::new()
        } else if !type_args.is_empty() {
            type_args.to_vec()
        } else if let Some(Ty::Struct(s, _, a)) = want.map(|w| &w.ty)
            && *s == sid
        {
            a.to_vec()
        } else {
            let mut subst: Vec<Option<Ty>> = vec![None; def.generics.len()];
            for late in [false, true] {
                for f in fields {
                    if needs_context(&f.value) != late {
                        continue;
                    }
                    if let Some((_, fd)) = def.field(&f.name)
                        && fd.ty.has_params(owner)
                        && !subst_complete(&fd.ty, owner, &subst)
                    {
                        let t = self.infer(&f.value);
                        self.unify(owner, &fd.ty, &t, &mut subst);
                    }
                }
            }
            subst
                .into_iter()
                .enumerate()
                .map(|(i, t)| {
                    t.or_else(|| def.generics[i].default.clone())
                        .unwrap_or_else(|| {
                            self.diags.push(
                                Diagnostic::error(
                                    codes::CANT_INFER,
                                    format!(
                                        "can't infer the type argument `{}`",
                                        def.generics[i].name
                                    ),
                                )
                                .primary(e.span, "not determined by the fields")
                                .help(format!("write `{}<…> {{ … }}`", def.name)),
                            );
                            Ty::Error
                        })
                })
                .collect()
        };
        let mut seen: Vec<Option<Span>> = vec![None; def.fields.len()];
        let mut order = Vec::new();
        for f in fields {
            match def.field(&f.name) {
                Some((i, fd)) => {
                    let fty = fd.ty.subst(owner, &targs);
                    if let Some(prev) = seen[i] {
                        self.diags.push(
                            Diagnostic::error(
                                codes::DUPLICATE_FIELD,
                                format!("field `{}` is given twice", f.name),
                            )
                            .primary(f.name_span, "given again here")
                            .secondary(prev, "first given here"),
                        );
                    }
                    seen[i] = Some(f.name_span);
                    order.push(i as u32);
                    let found = self.types.expr(f.value.id).clone();
                    if found.is_error() || !fty.has_any_params() && found == Ty::Error {
                        self.check(&f.value, &Expect::from(fty, Some(fd.span)));
                    } else if self.types.exprs[f.value.id.0 as usize] != Ty::Error
                        && !def.generics.is_empty()
                        && type_args.is_empty()
                    {
                        // Already inferred while finding the type arguments.
                        self.coerce(&f.value, found, &Expect::from(fty, Some(fd.span)));
                    } else {
                        self.check(&f.value, &Expect::from(fty, Some(fd.span)));
                    }
                }
                None => {
                    self.infer(&f.value);
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!("`{}` has no field `{}`", def.name, f.name),
                        )
                        .primary(f.name_span, "unknown field")
                        .secondary(def.name_span, format!("`{}` is declared here", def.name)),
                    );
                    order.push(u32::MAX);
                }
            }
        }
        let missing: Vec<&str> = def
            .fields
            .iter()
            .zip(&seen)
            // An optional struct field (`p?: T`) is `null` if left out.
            .filter(|(f, s)| s.is_none() && !(f.preassigned && !def.is_class))
            .map(|(f, _)| f.name.as_str())
            .collect();
        if !missing.is_empty() {
            let list = missing
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", ");
            self.diags.push(
                Diagnostic::error(
                    codes::MISSING_FIELDS,
                    format!(
                        "missing field{} {list}",
                        if missing.len() == 1 { "" } else { "s" }
                    ),
                )
                .primary(e.span, format!("`{}` needs every field", def.name))
                .secondary(def.name_span, format!("`{}` is declared here", def.name)),
            );
        }
        self.types.objects.insert(e.id, (sid, order));
        def.ty_with(sid, targs.into())
    }

    /// `{ kind: "click", x: 1, y: 2 }` for a discriminated union: the discriminant picks the
    /// variant, whose fields must all be given.
    fn discriminated_object(
        &mut self,
        e: &Expr,
        eid: EnumId,
        ename: &std::sync::Arc<str>,
        disc: &str,
        fields: &[FieldInit],
    ) -> Ty {
        let def = self.program.enum_def(eid);
        let tag = fields.iter().find(|f| f.name == disc);
        let vi = match tag.map(|f| &f.value.kind) {
            Some(ExprKind::Str(s)) => def.variant(s).map(|(i, _)| i),
            _ => None,
        };
        let Some(vi) = vi else {
            for f in fields {
                self.infer(&f.value);
            }
            let names: Vec<String> = def
                .variants
                .iter()
                .map(|v| format!("{:?}", v.name))
                .collect();
            self.diags.push(
                Diagnostic::error(
                    codes::UNKNOWN_OBJECT_TYPE,
                    format!("this object doesn't say which `{ename}` it is"),
                )
                .primary(e.span, format!("needs `{disc}: …` with a literal"))
                .note(format!("`{disc}` is one of {}", names.join(", "))),
            );
            return Ty::Error;
        };
        let var = &def.variants[vi as usize];
        let mut seen = vec![false; var.fields.len()];
        let mut order = Vec::new();
        for f in fields {
            match var.field(&f.name) {
                Some((i, fd)) => {
                    seen[i] = true;
                    order.push(i as u32);
                    self.check(&f.value, &Expect::from(fd.ty.clone(), Some(fd.span)));
                }
                None => {
                    order.push(u32::MAX);
                    self.infer(&f.value);
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!(
                                "`{ename}` with `{disc}: {:?}` has no field `{}`",
                                var.name, f.name
                            ),
                        )
                        .primary(f.name_span, "unknown field"),
                    );
                }
            }
        }
        let missing: Vec<String> = var
            .fields
            .iter()
            .zip(&seen)
            .filter(|(fd, s)| !**s && !fd.preassigned)
            .map(|(fd, _)| format!("`{}`", fd.name))
            .collect();
        if !missing.is_empty() {
            self.diags.push(
                Diagnostic::error(
                    codes::MISSING_FIELDS,
                    format!("missing {}", missing.join(", ")),
                )
                .primary(e.span, "every field is needed"),
            );
        }
        self.types.object_variants.insert(e.id, (eid, vi, order));
        Ty::Enum(eid, ename.clone())
    }

    /// Whether `==` is defined: primitives, strings, and structs whose fields all are.
    /// Whether values of `t` have a `hashCode` (built in, derived or written).
    fn hashable(&self, t: &Ty) -> bool {
        match t {
            Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::Str | Ty::Error => true,
            Ty::Struct(..) | Ty::Enum(..) | Ty::Tuple(_) | Ty::Fixed(..) => self.equatable(t),
            Ty::Class(sid, _, _) => self.program.struct_def(*sid).method("hashCode").is_some(),
            Ty::Lits(b, _) | Ty::Distinct(_, _, b) | Ty::Immutable(b) => self.hashable(b),
            Ty::Param(..) => self.bounds_of(t).contains(&Bound::Hashable),
            _ => false,
        }
    }

    fn equatable(&self, t: &Ty) -> bool {
        match t {
            Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::Str | Ty::Error => true,
            // Pointers compare by address.
            Ty::Ref(_) => true,
            Ty::Struct(sid, _, args) => self
                .program
                .struct_def(*sid)
                .fields
                .iter()
                .all(|f| self.equatable(&f.ty.subst(Ty::type_owner(*sid), args))),
            Ty::Tuple(ts) => ts.iter().all(|t| self.equatable(t)),
            Ty::Nullable(t) => self.equatable(t),
            Ty::Lits(b, _) | Ty::Distinct(_, _, b) | Ty::Immutable(b) => self.equatable(b),
            Ty::Union(_, ms) => ms.iter().all(|m| self.equatable(m)),
            Ty::Variant(..) => true,
            Ty::Param(..) => self
                .bounds_of(t)
                .iter()
                .any(|b| matches!(b, Bound::Comparable | Bound::Equatable | Bound::Hashable)),
            // Classes without `equals` compare by identity.
            Ty::Class(..) => true,
            Ty::Enum(id, _) => self
                .program
                .enum_def(*id)
                .variants
                .iter()
                .all(|v| v.fields.iter().all(|f| self.equatable(&f.ty))),
            Ty::Fixed(t, _) => self.equatable(t),
            Ty::Range(_) => true,
            _ => false,
        }
    }

    /// The `format` method of a struct or class implementing the core `Formattable`
    /// interface.
    fn formattable_method(&self, t: &Ty) -> Option<FnId> {
        let (Ty::Struct(sid, _, _) | Ty::Class(sid, _, _)) = t else {
            return None;
        };
        let iface = self
            .program
            .structs
            .iter()
            .position(|d| d.is_interface && d.name == "Formattable")
            .map(|i| StructId(i as u32))?;
        if *sid == iface || !self.program.implements(*sid, iface) {
            return None;
        }
        self.program.struct_def(*sid).method("format")
    }

    /// Parses and checks a template part's format spec against the part's type.
    fn format_spec(&mut self, e: &Expr, spec: &str) {
        let t = self.types.final_ty(e.id).clone();
        if t.is_error() {
            return;
        }
        match crate::format::parse(spec).and_then(|f| crate::format::check(&f, &t).map(|()| f)) {
            Ok(f) => {
                self.types.formats.insert(e.id, f);
            }
            Err(why) => self.diags.push(
                Diagnostic::error(
                    codes::INVALID_FORMAT_SPEC,
                    format!("invalid format spec `{spec}`"),
                )
                .primary(e.span, format!("formatted with `:{spec}`"))
                .note(why),
            ),
        }
    }

    /// A literal where a literal type or a union is expected: the literal type (or union
    /// member) it belongs to.
    fn lit_in(&mut self, e: &Expr, lit: &LitVal, want: &Ty) -> Ty {
        let want = match want {
            Ty::Nullable(inner) => &**inner,
            w => w,
        };
        let base = match lit {
            LitVal::Str(_) => Ty::Str,
            LitVal::Int(_) => Ty::INT,
            LitVal::Bool(_) => Ty::Bool,
        };
        let fits = |t: &Ty| match (t, lit) {
            (Ty::Lits(_, ls), l) => ls.contains(l),
            (Ty::Str, LitVal::Str(_)) | (Ty::Bool, LitVal::Bool(_)) => true,
            (Ty::Char, LitVal::Str(s)) => s.chars().count() == 1,
            (Ty::Int(it), LitVal::Int(v)) => it.contains(*v),
            (Ty::Float(_), LitVal::Int(_)) => true,
            _ => false,
        };
        let pick = match want {
            Ty::Lits(..) => fits(want).then(|| want.clone()),
            Ty::Union(_, ms) => ms
                .iter()
                .find(|m| matches!(m, Ty::Lits(..)) && fits(m))
                .or_else(|| ms.iter().find(|m| fits(m)))
                .cloned(),
            _ => None,
        };
        match pick {
            Some(Ty::Char) => Ty::Char,
            Some(Ty::Float(f)) => Ty::Float(f),
            Some(Ty::Int(it)) => Ty::Int(it),
            Some(t) => t,
            None => {
                let shown = match lit {
                    LitVal::Str(s) => format!("{s:?}"),
                    LitVal::Int(v) => v.to_string(),
                    LitVal::Bool(b) => b.to_string(),
                };
                self.diags.push(
                    Diagnostic::error(codes::MISMATCHED_TYPES, "mismatched types")
                        .primary(e.span, format!("`{shown}` isn't a `{want}`")),
                );
                let _ = base;
                Ty::Error
            }
        }
    }

    /// The member of a union that a `from` value converts into: the same type, else the first
    /// it widens or upcasts to.
    fn union_member(&self, from: &Ty, members: &[Ty]) -> Option<usize> {
        members.iter().position(|m| m == from).or_else(|| {
            members
                .iter()
                .position(|m| from.widens_to(m) || self.upcasts(from, m))
        })
    }

    fn int_lit(
        &mut self,
        e: &Expr,
        v: i128,
        suffix: Option<NumSuffix>,
        want: Option<&Expect>,
    ) -> Ty {
        let t = match (suffix, want.map(|w| &w.ty)) {
            (Some(s), _) => suffix_ty(s),
            (None, Some(t @ (Ty::Int(_) | Ty::Float(_)))) => t.clone(),
            (None, Some(Ty::Nullable(inner))) if inner.is_numeric() => (**inner).clone(),
            // A literal can be a value of a numeric distinct type.
            (None, Some(d @ Ty::Distinct(_, _, b))) if b.is_numeric() => d.clone(),
            _ => Ty::INT,
        };
        if let Ty::Int(it) = t.erase()
            && !it.contains(v)
        {
            self.lit_out_of_range(e.span, v, it);
            return Ty::Error;
        }
        t
    }

    fn lit_out_of_range(&mut self, span: Span, v: i128, it: IntTy) {
        self.diags.push(
            Diagnostic::error(
                codes::LITERAL_OUT_OF_RANGE,
                format!("literal out of range for `{}`", it.name()),
            )
            .primary(span, format!("`{v}` doesn't fit"))
            .note(format!(
                "`{}` holds values from {} to {}",
                it.name(),
                it.min(),
                it.max()
            )),
        );
    }

    fn invalid_operand(&mut self, span: Span, op: &str, t: &Ty, needs: &str) {
        if t.is_error() {
            return;
        }
        self.diags.push(
            Diagnostic::error(
                codes::INVALID_OPERANDS,
                format!("{op} can't be applied to `{t}`"),
            )
            .primary(span, format!("has type `{t}`"))
            .note(format!("{op} needs {needs}")),
        );
        if let Ty::Nullable(_) = t {
            let d = self.diags.pop().expect("just pushed");
            self.diags.push(d.help(NULL_HELP));
        }
    }

    fn unary(&mut self, e: &Expr, op: UnOp, inner: &Expr, want: Option<&Expect>) -> Ty {
        // `-128i8`, `-9223372036854775808`: the range check sees the negated value.
        if op == UnOp::Neg
            && let ExprKind::Int(v, suffix) = inner.kind
        {
            let t = match (suffix, want.map(|w| &w.ty)) {
                (Some(s), _) => suffix_ty(s),
                (None, Some(t @ (Ty::Int(_) | Ty::Float(_)))) => t.clone(),
                _ => Ty::INT,
            };
            self.set(inner, t.clone());
            return match t {
                Ty::Int(it) if !it.signed() => {
                    self.invalid_operand(e.span, "unary `-`", &t, "a signed integer or a float");
                    Ty::Error
                }
                Ty::Int(it) if !it.contains(-(v as i128)) => {
                    self.lit_out_of_range(e.span, -(v as i128), it);
                    Ty::Error
                }
                t => t,
            };
        }
        let t = match want {
            Some(w) if matches!(op, UnOp::Neg | UnOp::Plus | UnOp::BitNot) && w.ty.is_numeric() => {
                self.infer_with(inner, Some(w))
            }
            _ => self.infer(inner),
        };
        if matches!(t, Ty::Struct(..) | Ty::Class(..)) {
            let sym = match op {
                UnOp::Neg => "-",
                UnOp::Not => "!",
                UnOp::BitNot => "~",
                _ => "+",
            };
            let name = format!("op{sym}u");
            if let Some(m) = self.operator_for(&t, &name, None) {
                self.types.op_calls.insert(e.id, m);
                return self.call_fn(e, m, &[]);
            }
        }
        match op {
            UnOp::Neg => match &t {
                Ty::Int(it) if it.signed() => t,
                Ty::Float(_) | Ty::Error => t,
                _ => {
                    self.invalid_operand(e.span, "unary `-`", &t, "a signed integer or a float");
                    Ty::Error
                }
            },
            UnOp::Plus => {
                if t.is_numeric() || t.is_error() {
                    t
                } else {
                    self.invalid_operand(e.span, "unary `+`", &t, "a number");
                    Ty::Error
                }
            }
            UnOp::Not => {
                if matches!(t, Ty::Bool | Ty::Error) {
                    Ty::Bool
                } else {
                    self.invalid_operand(e.span, "`!`", &t, "a `bool`");
                    Ty::Error
                }
            }
            UnOp::BitNot => {
                if t.is_int() || t.is_error() {
                    t
                } else {
                    self.invalid_operand(e.span, "`~`", &t, "an integer");
                    Ty::Error
                }
            }
            _ => unreachable!("handled by the resolver"),
        }
    }

    /// Types both operands of a binary operator and joins them to one type.
    fn operands(&mut self, l: &Expr, r: &Expr, hint: Option<&Expect>) -> (Ty, Ty) {
        if self.pre_inferred.take() == Some(l.id) {
            let lt = self.types.expr(l.id).clone();
            let rt = self.infer_with(r, Some(&Expect::new(lt.clone())));
            return (lt, rt);
        }
        if untyped_lit(l) && !untyped_lit(r) {
            let rt = self.infer_with(r, hint);
            let lt = self.check_or_join(l, &rt);
            return (lt, rt);
        }
        let lt = self.infer_with(l, hint);
        let rt = self.infer_with(r, Some(&Expect::new(lt.clone())));
        (lt, rt)
    }

    fn check_or_join(&mut self, e: &Expr, to: &Ty) -> Ty {
        let t = self.infer_with(e, Some(&Expect::new(to.clone())));
        if t.widens_to(to) && t != *to {
            self.types.coercions.insert(e.id, to.clone());
            return to.clone();
        }
        t
    }

    /// The common type of two operands, inserting a widening on one side if needed.
    fn join(&mut self, op_span: Span, op: BinOp, l: &Expr, lt: &Ty, r: &Expr, rt: &Ty) -> Ty {
        if lt == rt || rt.is_error() {
            return lt.clone();
        }
        if lt.is_error() {
            return rt.clone();
        }
        if rt.widens_to(lt) {
            self.types.coercions.insert(r.id, lt.clone());
            return lt.clone();
        }
        if lt.widens_to(rt) {
            self.types.coercions.insert(l.id, rt.clone());
            return rt.clone();
        }
        // Anything compares with `null` (as a `T?`, which a plain `T` never is null as).
        if matches!(op, BinOp::Eq | BinOp::Ne) && (*lt == Ty::Null || *rt == Ty::Null) {
            let t = if *lt == Ty::Null { rt } else { lt }.clone().nullable();
            for (x, xt) in [(l, lt), (r, rt)] {
                if *xt != t {
                    self.types.coercions.insert(x.id, t.clone());
                }
            }
            return t;
        }
        // A union member compared with the union.
        if let Ty::Union(_, ms) = lt
            && self.union_member(rt, ms).is_some()
        {
            self.types.coercions.insert(r.id, lt.clone());
            return lt.clone();
        }
        if let Ty::Union(_, ms) = rt
            && self.union_member(lt, ms).is_some()
        {
            self.types.coercions.insert(l.id, rt.clone());
            return rt.clone();
        }
        let mut d = Diagnostic::error(
            codes::INVALID_OPERANDS,
            format!("`{}` can't be applied to `{lt}` and `{rt}`", op.as_str()),
        )
        .secondary(l.span, format!("`{lt}`"))
        .secondary(r.span, format!("`{rt}`"));
        d.labels[1].primary = true;
        if lt.is_numeric() && rt.is_numeric() {
            d = d.help("convert one side explicitly with `as`");
        }
        let _ = op_span;
        self.diags.push(d);
        Ty::Error
    }

    fn binary(&mut self, e: &Expr, op: BinOp, l: &Expr, r: &Expr, want: Option<&Expect>) -> Ty {
        use BinOp::*;
        // A user type on the left: its operators and protocols.
        if !matches!(op, And | Or | Nullish | Shl | Shr | UShr) && !needs_context(l) {
            let hint =
                want.filter(|w| w.ty.is_numeric() && !matches!(op, Eq | Ne | Lt | Le | Gt | Ge));
            let lt = self.infer_with(l, hint);
            if let Some(t) = self.user_binary(e, op, l, &lt, r) {
                return t;
            }
            // Pointer arithmetic: `p + n` / `p - n` moves by `n` elements.
            if matches!(lt, Ty::Ref(_)) && matches!(op, Add | Sub) {
                self.require_unsafe(e.span, "pointer arithmetic");
                let rt = self.infer_with(r, Some(&Expect::new(Ty::INT)));
                if !rt.is_int() && !rt.is_error() {
                    self.invalid_operand(r.span, "a pointer offset", &rt, "an integer");
                }
                return lt;
            }
            self.pre_inferred = Some(l.id);
        }
        match op {
            And | Or => {
                self.check(l, &Expect::new(Ty::Bool));
                let f = self.facts(l);
                let known = if op == And { f.when_true } else { f.when_false };
                self.with_facts(known, |c| c.check(r, &Expect::new(Ty::Bool)));
                Ty::Bool
            }
            Nullish => self.nullish(l, r, want),
            Shl | Shr | UShr => {
                let hint = want.filter(|w| w.ty.is_int());
                let lt = self.infer_with(l, hint);
                let rt = self.infer_with(r, Some(&Expect::new(Ty::Int(IntTy::U32))));
                if !lt.is_int() && !lt.is_error() {
                    self.invalid_operand(l.span, &format!("`{}`", op.as_str()), &lt, "an integer");
                    return Ty::Error;
                }
                if !rt.is_int() && !rt.is_error() {
                    self.invalid_operand(r.span, "a shift amount", &rt, "an integer");
                }
                lt
            }
            Add | Sub | Mul | Div | Rem | Pow | BitAnd | BitOr | BitXor => {
                let hint = want.filter(|w| w.ty.is_numeric());
                let (lt, rt) = self.operands(l, r, hint);
                // `"n = " + n` / `n + " items"`: the other side's string form (spec §3.2).
                let is_str = |t: &Ty| t.erase() == Ty::Str;
                if op == Add && (is_str(&lt) || is_str(&rt)) {
                    for (x, xt) in [(l, &lt), (r, &rt)] {
                        if !is_printable(xt) {
                            self.invalid_operand(x.span, "`+`", xt, "a value with a string form");
                        }
                    }
                    return Ty::Str;
                }
                let t = self.join(e.span, op, l, &lt, r, &rt);
                let ok = match op {
                    Add => t.is_numeric() || t == Ty::Str,
                    BitAnd | BitOr | BitXor => t.is_int(),
                    _ => t.is_numeric(),
                };
                if !ok && !t.is_error() {
                    let needs = match op {
                        Add => "numbers or strings",
                        BitAnd | BitOr | BitXor => "integers",
                        _ => "numbers",
                    };
                    self.invalid_operand(e.span, &format!("`{}`", op.as_str()), &t, needs);
                    return Ty::Error;
                }
                t
            }
            Lt | Le | Gt | Ge | Eq | Ne => {
                let (lt, rt) = self.operands(l, r, None);
                let t = self.join(e.span, op, l, &lt, r, &rt);
                // Anything nullable compares with `null`.
                let with_null =
                    matches!(l.kind, ExprKind::Null) || matches!(r.kind, ExprKind::Null);
                let ok = match op {
                    Eq | Ne => with_null || self.equatable(&t),
                    _ => {
                        matches!(
                            t.erase(),
                            Ty::Int(_) | Ty::Float(_) | Ty::Char | Ty::Str | Ty::Error
                        ) || self.bounds_of(&t).contains(&Bound::Comparable)
                    }
                };
                if !ok {
                    self.invalid_operand(
                        e.span,
                        &format!("`{}`", op.as_str()),
                        &t,
                        "comparable values",
                    );
                }
                Ty::Bool
            }
        }
    }

    /// The user method implementing operator `name` on `t` whose parameter fits `arg`
    /// (operators may be overloaded by the right operand's type).
    fn operator_for(&mut self, t: &Ty, name: &str, arg: Option<&Expr>) -> Option<FnId> {
        let (Ty::Struct(sid, _, _) | Ty::Class(sid, _, _)) = t else {
            return None;
        };
        let cands = self.program.struct_def(*sid).methods_named(name);
        match (cands.as_slice(), arg) {
            ([], _) => None,
            ([one], _) | ([one, ..], None) => Some(*one),
            (many, Some(a)) => {
                let at = if needs_context(a) {
                    None
                } else {
                    Some(self.infer(a))
                };
                many.iter()
                    .find(|f| {
                        let p = &self.types.sigs[f.0 as usize].params;
                        match (&at, p.first()) {
                            (Some(at), Some(p)) => at == p || at.widens_to(p),
                            (None, Some(p)) => self.literal_fits(a, p),
                            _ => false,
                        }
                    })
                    .copied()
                    .or(Some(many[0]))
            }
        }
    }

    /// Whether values of `t` compare field by field (derived `compareTo`).
    fn derives_ord(&self, t: &Ty) -> bool {
        match t {
            Ty::Struct(..) | Ty::Enum(..) | Ty::Tuple(_) | Ty::Fixed(..) | Ty::Range(_) => true,
            Ty::Nullable(inner) => self.derives_ord(inner),
            _ => false,
        }
    }

    /// Operators on structs and classes: `operator` methods, `==` through `equals`, and
    /// comparisons through `compareTo` (written or derived). `None`: not a user operator.
    fn user_binary(&mut self, e: &Expr, op: BinOp, l: &Expr, lt: &Ty, r: &Expr) -> Option<Ty> {
        use BinOp::*;
        let user = matches!(lt, Ty::Struct(..) | Ty::Class(..));
        match op {
            Add | Sub | Mul | Div | Rem | Pow | BitAnd | BitOr | BitXor if user => {
                let name = format!("op{}", op.as_str());
                let Some(m) = self.operator_for(lt, &name, Some(r)) else {
                    self.infer(r);
                    self.diags.push(
                        Diagnostic::error(
                            codes::INVALID_OPERANDS,
                            format!("`{lt}` has no `{}` operator", op.as_str()),
                        )
                        .primary(e.span, "not defined for this type")
                        .help(format!(
                            "declare `operator {}(o: …): …` in the type",
                            op.as_str()
                        )),
                    );
                    return Some(Ty::Error);
                };
                self.types.op_calls.insert(e.id, m);
                Some(self.call_fn(e, m, std::slice::from_ref(r)))
            }
            Lt | Le | Gt | Ge if user || self.derives_ord(lt) => {
                let user_cmp = match lt {
                    Ty::Struct(sid, _, _) | Ty::Class(sid, _, _) => {
                        self.program.struct_def(*sid).method("compareTo")
                    }
                    _ => None,
                };
                match user_cmp {
                    Some(m) => {
                        self.types.op_calls.insert(e.id, m);
                        self.call_fn(e, m, std::slice::from_ref(r));
                    }
                    None if self.derives_ord(lt) => {
                        self.check(r, &Expect::new(lt.clone()));
                        self.types.derived.insert(e.id, Derived::Compare);
                    }
                    None => {
                        self.infer(r);
                        self.diags.push(
                            Diagnostic::error(
                                codes::INVALID_OPERANDS,
                                format!("`{lt}` can't be ordered"),
                            )
                            .primary(e.span, "no `compareTo`")
                            .help("implement `compareTo(o: …): i32`, or use `@derive(Ord)`"),
                        );
                    }
                }
                let _ = l;
                Some(Ty::Bool)
            }
            Eq | Ne if user => {
                let def_eq = match lt {
                    Ty::Struct(sid, _, _) | Ty::Class(sid, _, _) => {
                        self.program.struct_def(*sid).method("equals")
                    }
                    _ => None,
                };
                let m = def_eq?;
                self.types.op_calls.insert(e.id, m);
                self.call_fn(e, m, std::slice::from_ref(r));
                Some(Ty::Bool)
            }
            _ => None,
        }
    }

    /// `x op= value`
    fn compound(&mut self, e: &Expr, op: BinOp, vt: Ty, value: &Expr) {
        use BinOp::*;
        // Derived from the type's binary operator.
        if matches!(vt, Ty::Struct(..) | Ty::Class(..))
            && !matches!(op, And | Or | Nullish)
            && let Some(m) = self.operator_for(&vt, &format!("op{}", op.as_str()), Some(value))
        {
            self.types.op_calls.insert(e.id, m);
            let r = self.call_fn(e, m, std::slice::from_ref(value));
            if r != vt && !r.is_error() {
                self.mismatch(e.span, &Expect::new(vt), &r);
            }
            return;
        }
        match op {
            And | Or => {
                if !matches!(vt, Ty::Bool | Ty::Error) {
                    self.invalid_operand(e.span, &format!("`{}=`", op.as_str()), &vt, "a `bool`");
                }
                self.check(value, &Expect::new(Ty::Bool));
            }
            Shl | Shr | UShr => {
                if !vt.is_int() && !vt.is_error() {
                    self.invalid_operand(e.span, &format!("`{}=`", op.as_str()), &vt, "an integer");
                }
                let rt = self.infer_with(value, Some(&Expect::new(Ty::Int(IntTy::U32))));
                if !rt.is_int() && !rt.is_error() {
                    self.invalid_operand(value.span, "a shift amount", &rt, "an integer");
                }
            }
            Nullish => {
                if !matches!(vt, Ty::Nullable(_) | Ty::Error) {
                    self.invalid_operand(e.span, "`??=`", &vt, "a nullable (`T?`) target");
                }
                self.check(value, &Expect::new(vt.clone()));
            }
            Add if vt.erase() == Ty::Str => {
                // `s += v`: the value's string form.
                let t = self.infer_with(value, Some(&Expect::new(vt.clone())));
                if !is_printable(&t) {
                    self.invalid_operand(value.span, "`+=`", &t, "a value with a string form");
                }
            }
            _ => {
                self.check(value, &Expect::new(vt.clone()));
                let ok = match op {
                    Add => vt.is_numeric() || vt == Ty::Str,
                    BitAnd | BitOr | BitXor => vt.is_int(),
                    _ => vt.is_numeric(),
                };
                if !ok {
                    self.invalid_operand(e.span, &format!("`{}=`", op.as_str()), &vt, "numbers");
                }
            }
        }
    }

    fn if_expr(
        &mut self,
        cond: &Expr,
        then: &Expr,
        else_: Option<&Expr>,
        want: Option<&Expect>,
    ) -> Ty {
        self.check(cond, &Expect::new(Ty::Bool));
        let facts = self.facts(cond);
        let (ft, ff) = (facts.when_true, facts.when_false);
        match else_ {
            Some(els) => match want {
                Some(w) if !matches!(w.ty, Ty::Void) => {
                    self.with_facts(ft, |c| c.check(then, w));
                    self.with_facts(ff, |c| c.check(els, w));
                    w.ty.clone()
                }
                _ => {
                    let tt = self.with_facts(ft, |c| c.infer(then));
                    let et = self.with_facts(ff, |c| c.infer(els));
                    if want.is_some_and(|w| w.ty == Ty::Void) {
                        return Ty::Void;
                    }
                    self.unify_branches(then, &tt, els, &et)
                }
            },
            None => {
                let inner = match want.map(|w| &w.ty) {
                    Some(Ty::Nullable(t)) => Some(Expect::new((**t).clone())),
                    _ => None,
                };
                let tt = self.with_facts(ft, |c| match &inner {
                    Some(w) => c.check(then, w),
                    None => c.infer(then),
                });
                match tt {
                    Ty::Void | Ty::Never => Ty::Void,
                    t => t.nullable(),
                }
            }
        }
    }

    fn unify_branches(&mut self, then: &Expr, tt: &Ty, els: &Expr, et: &Ty) -> Ty {
        match (tt, et) {
            (Ty::Never, t) | (t, Ty::Never) => t.clone(),
            (a, b) if a == b => a.clone(),
            // `if (c) null else x`: `x`'s type made nullable.
            (Ty::Null, t) | (t, Ty::Null) if !matches!(t, Ty::Void | Ty::Error) => {
                let n = t.clone().nullable();
                for (e, ty) in [(then, tt), (els, et)] {
                    if *ty != n {
                        self.types.coercions.insert(e.id, n.clone());
                    }
                }
                n
            }
            (a, b) if b.widens_to(a) => {
                self.types.coercions.insert(els.id, a.clone());
                a.clone()
            }
            (a, b) if a.widens_to(b) => {
                self.types.coercions.insert(then.id, b.clone());
                b.clone()
            }
            (a, b) if a.is_error() || b.is_error() => Ty::Error,
            (a, b) => {
                let mut d = Diagnostic::error(
                    codes::MISMATCHED_TYPES,
                    "`if` and `else` have different types",
                )
                .secondary(then.span, format!("`{a}`"))
                .secondary(els.span, format!("`{b}`"));
                d.labels[1].primary = true;
                self.diags.push(d);
                Ty::Error
            }
        }
    }

    fn ret(&mut self, e: &Expr, value: Option<&Expr>) {
        if self.func().is_entry {
            if let Some(v) = value {
                self.infer(v);
            }
            return;
        }
        let declared = match &self.ctx().ret {
            Ret::Declared(t, origin) => Some((t.clone(), *origin)),
            Ret::Infer(_) => None,
        };
        match (declared, value) {
            (Some((t, origin)), Some(v)) => {
                self.tail_position = matches!(v.kind, ExprKind::Tailcall(_));
                if t == Ty::Void {
                    self.infer(v);
                } else {
                    self.check(v, &Expect::from(t, origin));
                }
            }
            (Some((t, origin)), None) => {
                if t != Ty::Void && !t.is_error() {
                    let mut d =
                        Diagnostic::error(codes::MISMATCHED_TYPES, "`return` without a value")
                            .primary(e.span, format!("expected a `{t}` value"));
                    if let Some(o) = origin {
                        d = d.secondary(o, "the return type is declared here");
                    }
                    self.diags.push(d);
                }
            }
            (None, v) => {
                let t = match v {
                    Some(v) => self.infer(v),
                    None => Ty::Void,
                };
                if let Ret::Infer(rets) = &mut self.ctx().ret {
                    rets.push((t, v.map_or(e.span, |v| v.span)));
                }
            }
        }
    }

    /// Checks a call of the function `id` (a free function or a method; `this` isn't an
    /// argument) and returns its result type.
    /// The constructor of `class` that `args` call: the one taking exactly that many
    /// positional arguments, the only one, else the first whose defaults, rest parameter
    /// and parameter names allow them (`map_args` then checks the details).
    fn pick_ctor(&self, class: StructId, args: &[Expr]) -> Option<FnId> {
        let def = self.program.struct_def(class);
        let plain = args
            .iter()
            .all(|a| !matches!(a.kind, ExprKind::NamedArg { .. } | ExprKind::Spread(_)));
        if plain && let Some(f) = def.ctor_with(args.len()) {
            return Some(f);
        }
        let spread = args.iter().any(|a| matches!(a.kind, ExprKind::Spread(_)));
        let named: Vec<&str> = args
            .iter()
            .filter_map(|a| match &a.kind {
                ExprKind::NamedArg { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        let positional = args.len() - named.len();
        if def.ctors.len() == 1 {
            // The only one: the argument checks then say what's wrong.
            return Some(def.ctors[0].0);
        }
        def.ctors.iter().map(|(f, _)| *f).find(|&id| {
            let f = self.program.func(id);
            let total = f.params.len();
            let fixed = if f.rest { total - 1 } else { total };
            let required = (0..fixed)
                .filter(|k| f.defaults.get(*k).is_none_or(Option::is_none))
                .count();
            let names_ok = named
                .iter()
                .all(|n| f.params.iter().any(|p| f.local(*p).name == *n));
            names_ok
                && (f.rest || (!spread && positional <= fixed))
                && (spread || positional + named.len() >= required)
        })
    }

    fn call_fn(&mut self, e: &Expr, id: FnId, args: &[Expr]) -> Ty {
        let f = self.program.func(id);
        // Parameters typed by their defaults need the callee checked first.
        let untyped = f.params.iter().any(|p| f.local(*p).ty.is_none());
        if untyped && f.ret.is_some() && self.state[id.0 as usize] == FnState::Unchecked {
            self.check_fn(id);
        }
        if f.ret.is_none() {
            match self.state[id.0 as usize] {
                FnState::InProgress => {
                    self.diags.push(
                        Diagnostic::error(
                            codes::RECURSIVE_NEEDS_RETURN_TYPE,
                            format!("recursive function `{}` needs a return type", f.name),
                        )
                        .primary(f.name_span, "add a return type, e.g. `: i64`")
                        .secondary(e.span, "recursive call here")
                        .note("the return type of a recursive function can't be inferred"),
                    );
                    self.types.sigs[id.0 as usize].ret = Ty::Error;
                    self.state[id.0 as usize] = FnState::Done;
                }
                FnState::Unchecked => self.check_fn(id),
                FnState::Done => {}
            }
        }
        let sig = self.types.sigs[id.0 as usize].clone();
        if let Some(slots) = self.map_args(e, id, &sig.params, args, false) {
            self.check_slots(id, &sig.params, args, &slots);
            self.types.arg_slots.insert(e.id, slots);
        }
        sig.ret
    }

    /// The expression an argument passes (inside `name: value` and `...value`).
    fn arg_value(a: &Expr) -> &Expr {
        match &a.kind {
            ExprKind::NamedArg { value, .. } | ExprKind::Spread(value) => value,
            _ => a,
        }
    }

    /// Matches a call's arguments to the parameters of `id` (positional, then named; rest
    /// arguments; `...tuple` and `...array` spreads; defaults for the rest). With `quiet`,
    /// only says whether it fits.
    fn map_args(
        &mut self,
        e: &Expr,
        id: FnId,
        params: &[Ty],
        args: &[Expr],
        quiet: bool,
    ) -> Option<Vec<Slot>> {
        let f = self.program.func(id);
        let mark = self.diags.len();
        let n = params.len();
        let fixed = if f.rest { n.saturating_sub(1) } else { n };
        let mut slots: Vec<Option<Slot>> = vec![None; n];
        let mut next = 0;
        let mut rest_args = Vec::new();
        let mut rest_spread = None;
        let mut named_seen = false;
        let names: Vec<&str> = f.params.iter().map(|p| f.local(*p).name.as_str()).collect();
        let fail = |c: &mut Self, span: Span, msg: String, label: &str| {
            c.diags.push(
                Diagnostic::error(codes::WRONG_ARG_COUNT, msg).primary(span, label.to_string()),
            );
        };
        for (i, a) in args.iter().enumerate() {
            match &a.kind {
                ExprKind::NamedArg {
                    name, name_span, ..
                } => {
                    named_seen = true;
                    match names.iter().position(|n| n == name) {
                        Some(k) if slots[k].is_some() => fail(
                            self,
                            *name_span,
                            format!("`{name}` is given twice"),
                            "already given",
                        ),
                        Some(k) => slots[k] = Some(Slot::Arg(i)),
                        None => fail(
                            self,
                            *name_span,
                            format!("`{}` has no parameter `{name}`", f.name),
                            "unknown parameter",
                        ),
                    }
                }
                _ if named_seen => {
                    self.diags.push(
                        Diagnostic::error(
                            codes::POSITIONAL_AFTER_NAMED,
                            "positional argument after a named one",
                        )
                        .primary(a.span, "move it before the named arguments"),
                    );
                }
                ExprKind::Spread(inner) => match self.infer(inner) {
                    Ty::Tuple(ts) => {
                        for k in 0..ts.len() {
                            if next < fixed {
                                slots[next] = Some(Slot::TupleElem(i, k as u32));
                                next += 1;
                            } else {
                                fail(
                                    self,
                                    a.span,
                                    format!(
                                        "`{}` takes {fixed} positional argument{}",
                                        f.name,
                                        if fixed == 1 { "" } else { "s" }
                                    ),
                                    "the tuple has too many elements",
                                );
                                break;
                            }
                        }
                    }
                    Ty::Array(_) if f.rest && next >= fixed && rest_args.is_empty() => {
                        rest_spread = Some(i)
                    }
                    Ty::Error => {}
                    t => fail(
                        self,
                        a.span,
                        format!("can't spread a `{t}` here"),
                        "spread a tuple into parameters, or an array into a rest parameter",
                    ),
                },
                _ if next < fixed => {
                    slots[next] = Some(Slot::Arg(i));
                    next += 1;
                }
                _ if f.rest => rest_args.push(i),
                _ => fail(
                    self,
                    e.span,
                    format!(
                        "`{}` takes {n} argument{}, but {} were given",
                        f.name,
                        if n == 1 { "" } else { "s" },
                        args.len()
                    ),
                    "extra argument",
                ),
            }
        }
        if f.rest && n > 0 {
            slots[n - 1] = Some(match rest_spread {
                Some(i) => Slot::RestSpread(i),
                None => Slot::Rest(rest_args),
            });
        }
        #[allow(clippy::needless_range_loop)]
        for k in 0..n {
            if slots[k].is_none() {
                if f.defaults.get(k).is_some_and(Option::is_some) {
                    slots[k] = Some(Slot::Default);
                } else {
                    let missing = names.get(k).copied().unwrap_or("?").to_string();
                    fail(
                        self,
                        e.span,
                        format!("missing argument `{missing}` for `{}`", f.name),
                        "not enough arguments",
                    );
                }
            }
        }
        if self.diags.len() > mark {
            if quiet {
                self.diags.truncate(mark);
            }
            for a in args {
                if !quiet && !matches!(a.kind, ExprKind::Spread(_)) {
                    self.infer(Self::arg_value(a));
                }
            }
            return None;
        }
        Some(slots.into_iter().map(|s| s.expect("filled")).collect())
    }

    /// Checks the arguments that fill each parameter.
    fn check_slots(&mut self, id: FnId, params: &[Ty], args: &[Expr], slots: &[Slot]) {
        let f = self.program.func(id);
        for (k, slot) in slots.iter().enumerate() {
            let p = &params[k];
            let origin = f.params.get(k).map(|pid| f.local(*pid).span);
            match slot {
                Slot::Arg(i) => {
                    self.check(Self::arg_value(&args[*i]), &Expect::from(p.clone(), origin));
                }
                Slot::Rest(is) => {
                    let elem = match p {
                        Ty::Array(t) => (**t).clone(),
                        _ => Ty::Error,
                    };
                    for i in is {
                        self.check(&args[*i], &Expect::from(elem.clone(), origin));
                    }
                }
                Slot::RestSpread(i) | Slot::TupleElem(i, _) => {
                    let inner = Self::arg_value(&args[*i]);
                    let t = self.types.expr(inner.id).clone();
                    let elem = match (slot, &t) {
                        (Slot::TupleElem(_, k), Ty::Tuple(ts)) => ts[*k as usize].clone(),
                        _ => t,
                    };
                    if elem != *p && !elem.widens_to(p) && !elem.is_error() {
                        self.mismatch(inner.span, &Expect::from(p.clone(), origin), &elem);
                    }
                }
                Slot::Default => {}
            }
        }
    }

    /// Picks the overload a call means: the applicable candidates (by argument count, names
    /// and types), then the most specific (each of its parameter types converts to the
    /// other's). Several equally specific ones are an error.
    fn pick_overload(&mut self, e: &Expr, cands: &[FnId], args: &[Expr]) -> Option<FnId> {
        for c in cands {
            if self.program.func(*c).ret.is_none() && self.state[c.0 as usize] == FnState::Unchecked
            {
                self.check_fn(*c);
            }
        }
        // Argument types (literals are flexible: they fit any type that can hold them).
        let tys: Vec<Option<Ty>> = args
            .iter()
            .map(|a| {
                let v = Self::arg_value(a);
                (!needs_context(v)).then(|| self.infer(v))
            })
            .collect();
        let mut ok: Vec<(FnId, Vec<Ty>)> = Vec::new();
        for c in cands {
            let sig = self.types.sigs[c.0 as usize].clone();
            let Some(slots) = self.map_args(e, *c, &sig.params, args, true) else {
                continue;
            };
            let mut fits = true;
            let mut used = Vec::new();
            for (k, slot) in slots.iter().enumerate() {
                let p = &sig.params[k];
                if let Slot::Arg(i) = slot {
                    used.push(p.clone());
                    let fit = match &tys[*i] {
                        Some(t) => {
                            t == p
                                || t.widens_to(p)
                                || self.upcasts(t, p)
                                || (p.has_any_params() && shape_fits(p, t))
                                || t.is_error()
                                // A closure checked against a `void` function type drops
                                // its value (`() => f(x)` where `() => void` is expected).
                                || matches!((&Self::arg_value(&args[*i]).kind, t, p),
                                    (ExprKind::Closure(_), Ty::Fn(p1, _, t1), Ty::Fn(p2, r2, _))
                                        if p1 == p2 && **r2 == Ty::Void
                                            && self.upcasts(&Ty::Fn(p1.clone(), Box::new(Ty::Void), t1.clone()), p))
                        }
                        None => self.literal_fits(Self::arg_value(&args[*i]), p),
                    };
                    fits &= fit;
                }
            }
            if fits {
                ok.push((*c, used));
            }
        }
        // A literal argument prefers its own type (`3` → `int` over `number`).
        let lit_ty: Vec<Option<Ty>> = args
            .iter()
            .map(|a| match &Self::arg_value(a).kind {
                ExprKind::Int(_, None) => Some(Ty::INT),
                ExprKind::Float(_, None) => Some(Ty::NUMBER),
                _ => None,
            })
            .collect();
        let more_specific = |a: &[Ty], b: &[Ty]| {
            a.iter().zip(b).enumerate().all(|(i, (x, y))| {
                at_least_as_specific(x, y)
                    || lit_ty
                        .get(i)
                        .cloned()
                        .flatten()
                        .is_some_and(|l| *x == l && *y != l)
            })
        };
        let best: Vec<&(FnId, Vec<Ty>)> = ok
            .iter()
            .filter(|(f, a)| ok.iter().all(|(g, b)| f == g || more_specific(a, b)))
            .collect();
        match (best.as_slice(), ok.len()) {
            ([one], _) => Some(one.0),
            (_, 0) => {
                let name = self.program.func(cands[0]).name.clone();
                self.diags.push(
                    Diagnostic::error(
                        codes::NO_OVERLOAD,
                        format!("no overload of `{name}` takes these arguments"),
                    )
                    .primary(e.span, "no matching overload"),
                );
                None
            }
            _ => {
                let name = self.program.func(cands[0]).name.clone();
                let mut d = Diagnostic::error(
                    codes::AMBIGUOUS_OVERLOAD,
                    format!("ambiguous call of `{name}`"),
                )
                .primary(e.span, "several overloads fit equally well");
                for (f, _) in &ok {
                    d = d.secondary(self.program.func(*f).name_span, "this one fits");
                }
                self.diags.push(d);
                None
            }
        }
    }

    /// Whether an untyped literal (or `[]`, `null`, `{…}`) could be a `t`.
    fn literal_fits(&self, e: &Expr, t: &Ty) -> bool {
        match (&e.kind, t.erase()) {
            (_, Ty::Error) => true,
            (ExprKind::Int(v, _), Ty::Int(it)) => it.contains(*v as i128),
            (ExprKind::Int(..) | ExprKind::Float(..), Ty::Float(_)) => true,
            (ExprKind::Unary(UnOp::Neg, inner), _) => self.literal_fits(inner, t),
            (ExprKind::Null, Ty::Nullable(_)) => true,
            (ExprKind::Array(xs), Ty::Array(_)) if xs.is_empty() => true,
            (ExprKind::Object { .. }, Ty::Struct(..) | Ty::Enum(..)) => true,
            (_, Ty::Param(..)) => true,
            _ => false,
        }
    }

    fn call(&mut self, e: &Expr, callee: &Expr, args: &[Expr]) -> Ty {
        match &callee.kind {
            ExprKind::Fn(id) => {
                self.set(callee, Ty::Void);
                if self.program.func(*id).is_unsafe {
                    self.require_unsafe(e.span, "calling an `unsafe function`");
                }
                self.call_fn(e, *id, args)
            }
            ExprKind::Builtin(b) => {
                self.set(callee, Ty::Void);
                self.builtin(e, *b, args)
            }
            ExprKind::Member {
                object,
                name,
                name_span,
            } => {
                let explicit = std::mem::take(&mut self.method_type_args);
                let t = self.infer(object);
                let imm = self.through_immutable(object);
                let t = t.strip_immutable();
                self.set(callee, Ty::Void);
                if imm {
                    let readonly = match &t {
                        Ty::Struct(sid, _, _) | Ty::Class(sid, _, _) => {
                            let def = self.program.struct_def(*sid);
                            def.method(name).is_none() || def.readonly_methods.contains(name)
                        }
                        Ty::Array(_) => !matches!(name.as_str(), "push" | "pop"),
                        _ => true,
                    };
                    if !readonly {
                        self.immutable_mutation(
                            *name_span,
                            &format!("call `{name}` (not `readonly`)"),
                        );
                    }
                }
                if let (Ty::Array(elem), "push") = (&t, name.as_str())
                    && !self.arenas.is_empty()
                    && self.holds_refs(elem)
                    && root_var(object).is_some_and(|v| self.outside_arena(v))
                {
                    self.arena_escape(
                        e.span,
                        "a reference is pushed onto an array outside the block",
                    );
                }
                if let Some(r) = self.builtin_method(e, &t, name, *name_span, args) {
                    return r;
                }
                if let Some(r) = self.extension_call(e, object, &t, name, args) {
                    return r;
                }
                if let Ty::Param(..) = t {
                    if let Some(r) = self.param_method(e, &t, name, *name_span, args) {
                        return r;
                    }
                    for a in args {
                        self.infer(a);
                    }
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!("`{t}` has no method `{name}`"),
                        )
                        .primary(*name_span, "not provided by its bounds")
                        .help(format!("add a bound: `{t} extends …`")),
                    );
                    return Ty::Error;
                }
                if let Ty::Iface(sid, _, _) = &t {
                    let def = self.program.struct_def(*sid);
                    if let Some(slot) = def.methods.iter().position(|(n, _)| n == name) {
                        let m = def.methods[slot].1;
                        self.types.iface_calls.insert(e.id, slot as u32);
                        return self.call_fn(e, m, args);
                    }
                }
                if let Ty::Struct(sid, sname, _)
                | Ty::Class(sid, sname, _)
                | Ty::Iface(sid, sname, _) = &t
                {
                    let def = self.program.struct_def(*sid);
                    if let Some(m) = def.method(name) {
                        self.check_access(*sid, name, *name_span);
                        self.types.methods.insert(e.id, m);
                        if let Some(slot) = def.virtual_slot(name) {
                            self.types.virtual_calls.insert(e.id, slot);
                        }
                        let own = self.program.func(m).generics.clone();
                        if !def.generics.is_empty() || !own.is_empty() {
                            // Seen through the receiver's type arguments, then inferring the
                            // method's own.
                            let recv = t.type_args().to_vec();
                            let (ret, targs) = self.call_inferring(
                                e,
                                m,
                                &args.iter().collect::<Vec<_>>(),
                                &explicit,
                                m.0,
                                &own,
                                Some((Ty::type_owner(*sid), &recv)),
                            );
                            self.types.recv_args.insert(e.id, recv);
                            self.types.instances.insert(e.id, targs);
                            return ret;
                        }
                        if !explicit.is_empty() {
                            self.diags.push(
                                Diagnostic::error(
                                    codes::WRONG_ARG_COUNT,
                                    "type arguments for a method that isn't generic",
                                )
                                .primary(*name_span, "takes no type arguments"),
                            );
                        }
                        return self.call_fn(e, m, args);
                    }
                    if name == "toString" && matches!(t, Ty::Class(..)) && args.is_empty() {
                        // The default string form.
                        return Ty::Str;
                    }
                    let what = if def.field(name).is_some() {
                        "is a field, not a method"
                    } else {
                        "isn't declared"
                    };
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!("`{sname}` has no method `{name}`"),
                        )
                        .primary(*name_span, format!("`{name}` {what}")),
                    );
                    for a in args {
                        self.infer(a);
                    }
                    return Ty::Error;
                }
                // `x.toString()` on primitives.
                if name == "toString" && is_printable(&t) {
                    if !args.is_empty() {
                        self.diags.push(
                            Diagnostic::error(
                                codes::WRONG_ARG_COUNT,
                                "`toString` takes no arguments",
                            )
                            .primary(e.span, "wrong number of arguments"),
                        );
                    }
                    return Ty::Str;
                }
                for a in args {
                    self.infer(a);
                }
                if !t.is_error() {
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_MEMBER,
                            format!("`{t}` has no method `{name}`"),
                        )
                        .primary(*name_span, "unknown member"),
                    );
                }
                Ty::Error
            }
            _ => {
                let t = self.infer(callee);
                if let Ty::Fn(ps, r, _) = &t {
                    // A function value. Trailing `void` parameters may be left out (as in
                    // TS: `resolve()` for a `Promise<void>`).
                    let given_ok = args.len() < ps.len()
                        && ps[args.len()..].iter().all(|p| matches!(p, Ty::Void));
                    if args.len() != ps.len() && !given_ok {
                        self.diags.push(
                            Diagnostic::error(
                                codes::WRONG_ARG_COUNT,
                                format!(
                                    "this function takes {} argument{}, but {} were given",
                                    ps.len(),
                                    if ps.len() == 1 { "" } else { "s" },
                                    args.len()
                                ),
                            )
                            .primary(e.span, "wrong number of arguments"),
                        );
                        for a in args {
                            self.infer(a);
                        }
                        return (**r).clone();
                    }
                    for (a, p) in args.iter().zip(ps.iter()) {
                        self.check(a, &Expect::new(p.clone()));
                    }
                    return (**r).clone();
                }
                for a in args {
                    self.infer(a);
                }
                if !t.is_error() {
                    self.diags.push(
                        Diagnostic::error(codes::NOT_CALLABLE, format!("`{t}` isn't a function"))
                            .primary(callee.span, "called here"),
                    );
                }
                Ty::Error
            }
        }
    }

    /// Methods of built-in types (`xs.push(v)`, `r.step(n)`); `None` if `name` isn't one.
    fn builtin_method(
        &mut self,
        e: &Expr,
        t: &Ty,
        name: &str,
        name_span: Span,
        args: &[Expr],
    ) -> Option<Ty> {
        let _ = name_span;
        // Derived protocols (unless the type writes its own).
        let own = match t {
            Ty::Struct(sid, _, _) | Ty::Class(sid, _, _) => {
                self.program.struct_def(*sid).method(name).is_some()
            }
            _ => false,
        };
        // (Classes write theirs, or get them from `@derive`.)
        let derives = matches!(
            t,
            Ty::Struct(..) | Ty::Enum(..) | Ty::Tuple(_) | Ty::Fixed(..)
        );
        let derived = match name {
            "equals" if !own && derives => Some((Derived::Equals, Ty::Bool, 1)),
            "compareTo" if !own && derives => Some((Derived::Compare, Ty::Int(IntTy::I32), 1)),
            "hashCode" if !own && derives => Some((Derived::Hash, Ty::Int(IntTy::U64), 0)),
            _ => None,
        };
        if let Some((d, ret, n)) = derived {
            if args.len() != n {
                self.diags.push(
                    Diagnostic::error(
                        codes::WRONG_ARG_COUNT,
                        format!(
                            "`{name}` takes {n} argument{}",
                            if n == 1 { "" } else { "s" }
                        ),
                    )
                    .primary(e.span, "wrong number of arguments"),
                );
            }
            for a in args {
                self.check(a, &Expect::new(t.clone()));
            }
            self.types.derived.insert(e.id, d);
            return Some(ret);
        }
        let (arity_ok, expected, ret) = match (t, name) {
            (Ty::Ref(elem), "read") => {
                self.require_unsafe(e.span, "reading through a pointer");
                for a in args {
                    self.infer(a);
                }
                (args.is_empty(), "no arguments", (**elem).clone())
            }
            (Ty::Ref(elem), "write") => {
                self.require_unsafe(e.span, "writing through a pointer");
                let want = Expect::new((**elem).clone());
                for a in args {
                    self.check(a, &want);
                }
                (args.len() == 1, "1 argument", Ty::Void)
            }
            (Ty::Array(elem), "push") => {
                let want = Expect::new((**elem).clone());
                for a in args {
                    self.check(a, &want);
                }
                (!args.is_empty(), "at least 1 argument", Ty::Void)
            }
            (Ty::Array(elem), "pop") => {
                for a in args {
                    self.infer(a);
                }
                (args.is_empty(), "no arguments", (**elem).clone().nullable())
            }
            (Ty::Str, "slice") => {
                for a in args {
                    self.check(a, &Expect::new(Ty::INT));
                }
                (matches!(args.len(), 1 | 2), "1 or 2 arguments", Ty::Str)
            }
            (Ty::Range(it), "step") => {
                let want = Expect::new(Ty::Int(*it));
                for a in args {
                    self.check(a, &want);
                }
                (args.len() == 1, "1 argument", Ty::Range(*it))
            }
            _ => return None,
        };
        if !arity_ok {
            self.diags.push(
                Diagnostic::error(
                    codes::WRONG_ARG_COUNT,
                    format!("`{name}` takes {expected}, but {} were given", args.len()),
                )
                .primary(e.span, "wrong number of arguments"),
            );
        }
        Some(ret)
    }

    /// The element type of a value with an `iterator(): Iterator<T>` method (recording the
    /// method for lowering).
    fn iterable_elem(&mut self, iter: &Expr, t: &Ty) -> Option<Ty> {
        let (sid, args) = match t {
            Ty::Class(s, _, a) | Ty::Struct(s, _, a) => (*s, a.clone()),
            _ => return None,
        };
        let m = self.program.struct_def(sid).method("iterator")?;
        if !self.program.func(m).params.is_empty() {
            return None;
        }
        if self.program.func(m).ret.is_none() {
            self.check_fn(m);
        }
        let ret = self.types.sigs[m.0 as usize]
            .ret
            .subst(Ty::type_owner(sid), &args);
        match ret {
            Ty::Class(i, _, a) if Some(i) == self.program.iterator_class => {
                self.types.iterables.insert(iter.id, m);
                a.first().cloned()
            }
            _ => None,
        }
    }

    /// `xs.map(f)` on a built-in type: a call of the pico extension `__Array_map(xs, f)` (the
    /// overload whose parameter count fits).
    fn extension_call(
        &mut self,
        e: &Expr,
        object: &Expr,
        t: &Ty,
        name: &str,
        args: &[Expr],
    ) -> Option<Ty> {
        let kind = match t {
            Ty::Array(_) => "Array",
            Ty::Str => "String",
            _ => return None,
        };
        let ids = self
            .program
            .extensions
            .get(&(kind.to_string(), name.to_string()))?;
        let id = *ids
            .iter()
            .find(|f| self.program.func(**f).params.len() == args.len() + 1)
            .or(ids.first())?;
        let mut all: Vec<&Expr> = vec![object];
        all.extend(args.iter());
        let generics = self.program.func(id).generics.clone();
        self.reuse = Some(object.id);
        let (ret, targs) = self.call_inferring(e, id, &all, &[], id.0, &generics, None);
        self.reuse = None;
        if !generics.is_empty() {
            self.types.instances.insert(e.id, targs);
        }
        self.types.extensions.insert(e.id, id);
        Some(ret)
    }

    /// The bounds of a type parameter.
    fn bounds_of(&self, t: &Ty) -> &'p [Bound] {
        match t {
            Ty::Param(owner, i, _) if owner & 0x8000_0000 != 0 => self
                .program
                .struct_def(StructId(owner & 0x7fff_ffff))
                .generics
                .get(*i as usize)
                .map_or(&[][..], |p| &p.bounds[..]),
            Ty::Param(owner, i, _) => self
                .program
                .func(FnId(*owner))
                .generics
                .get(*i as usize)
                .map_or(&[][..], |p| &p.bounds[..]),
            _ => &[],
        }
    }

    /// Whether `t` meets `bound` (a type parameter meets the bounds it declares).
    fn satisfies(&self, t: &Ty, bound: &Bound) -> bool {
        if t.is_error() {
            return true;
        }
        if let Ty::Param(..) = t {
            let own = self.bounds_of(t);
            return own.contains(bound)
                || (*bound == Bound::Equatable
                    && (own.contains(&Bound::Comparable) || own.contains(&Bound::Hashable)));
        }
        match bound {
            Bound::Comparable => {
                matches!(t.erase(), Ty::Int(_) | Ty::Float(_) | Ty::Char | Ty::Str)
                    || self.derives_ord(t)
                    || matches!(t, Ty::Class(sid, _, _) if self.program.struct_def(*sid).method("compareTo").is_some())
            }
            Bound::Equatable => self.equatable(t),
            Bound::Hashable => self.hashable(t),
            Bound::Object => matches!(t.erase(), Ty::Class(..) | Ty::Array(_)),
            Bound::Sendable => self.sendable(t, &mut Vec::new()),
            Bound::Error => match (t, self.program.error_class) {
                (Ty::Never, _) => true,
                (Ty::Class(s, ..), Some(err)) => self.program.is_subclass(*s, err),
                _ => false,
            },
            Bound::Iface(i) => match t {
                Ty::Struct(s, _, _) | Ty::Class(s, _, _) | Ty::Iface(s, _, _) => {
                    self.program.implements(*s, *i)
                }
                _ => false,
            },
        }
    }

    /// Infers type parameters by matching a parameter type against an argument type.
    fn unify(&self, owner: u32, p: &Ty, a: &Ty, subst: &mut [Option<Ty>]) {
        match (p, a) {
            (Ty::Param(o, i, _), _) if *o == owner => {
                let slot = &mut subst[*i as usize];
                match slot {
                    None if !matches!(a, Ty::Never | Ty::Error | Ty::Null) => {
                        *slot = Some(a.clone())
                    }
                    // A later argument may widen an earlier guess: `max(1i32, x)` with `x: i64`.
                    Some(prev) if prev.widens_to(a) && prev != a => *slot = Some(a.clone()),
                    _ => {}
                }
            }
            (Ty::Array(p), Ty::Array(a))
            | (Ty::Fixed(p, _), Ty::Fixed(a, _))
            | (Ty::FixedN(p, _), Ty::FixedN(a, _)) => self.unify(owner, p, a, subst),
            // `[T; N]` against `[f32; 3]`: `N = 3`.
            (Ty::FixedN(p, n), Ty::Fixed(a, k)) => {
                self.unify(owner, p, a, subst);
                if let Ty::Param(o, i, _) = &**n
                    && *o == owner
                    && let Some(slot @ None) = subst.get_mut(*i as usize)
                {
                    *slot = Some(Ty::Lits(
                        Box::new(Ty::INT),
                        vec![LitVal::Int(*k as i128)].into(),
                    ));
                }
            }
            (Ty::Nullable(p), Ty::Nullable(a)) => self.unify(owner, p, a, subst),
            (Ty::Nullable(p), a) => self.unify(owner, p, a, subst),
            (Ty::Fn(ps, pr, pt), Ty::Fn(xs, xr, xt)) if ps.len() == xs.len() => {
                for (p, a) in ps.iter().zip(xs.iter()) {
                    self.unify(owner, p, a, subst);
                }
                self.unify(owner, pr, xr, subst);
                // `throws E`: E is what the argument throws beyond the other listed errors
                // (`never` for nothing, `Error` when it's several classes).
                for p in pt.iter() {
                    if let Ty::Param(o, i, _) = p
                        && *o == owner
                        && subst[*i as usize].is_none()
                    {
                        let rest: Vec<&Ty> = xt.iter().filter(|t| !pt.contains(t)).collect();
                        subst[*i as usize] = Some(match rest.as_slice() {
                            [] => Ty::Never,
                            [t] => (*t).clone(),
                            _ => match self.program.error_class {
                                Some(err) => self.program.struct_def(err).self_ty(err),
                                None => Ty::Error,
                            },
                        });
                    }
                }
            }
            (Ty::Tuple(ps), Ty::Tuple(xs)) if ps.len() == xs.len() => {
                for (p, a) in ps.iter().zip(xs) {
                    self.unify(owner, p, a, subst);
                }
            }
            (
                Ty::Struct(s1, _, ps) | Ty::Class(s1, _, ps) | Ty::Iface(s1, _, ps),
                Ty::Struct(s2, _, xs) | Ty::Class(s2, _, xs) | Ty::Iface(s2, _, xs),
            ) if s1 == s2 && ps.len() == xs.len() => {
                for (p, a) in ps.iter().zip(xs.iter()) {
                    self.unify(owner, p, a, subst);
                }
            }
            _ => {}
        }
    }

    /// A call of a generic function: type arguments are given or inferred from the arguments
    /// (typed ones first, then literals, which adapt to what's known), then the bounds are
    /// checked and the instance recorded.
    /// The type of a `comptime` parameter used as a value.
    fn const_param_ty(&self, t: &Ty) -> Ty {
        match t {
            Ty::Param(o, i, _) if *o & 0x8000_0000 == 0 => self
                .program
                .func(FnId(*o))
                .generics
                .get(*i as usize)
                .and_then(|g| g.comptime.clone())
                .unwrap_or(Ty::Error),
            _ => Ty::Error,
        }
    }

    /// Type argument `t` for parameter `g`: a `comptime` parameter takes a constant of its
    /// type (made a literal type of exactly that type, so equal constants share an instance).
    fn type_arg(&mut self, span: Span, g: &TypeParam, t: &Ty) -> Ty {
        let Some(ct) = &g.comptime else {
            return t.clone();
        };
        if g.is_len
            && let Ty::Lits(_, vs) = t
            && let [LitVal::Int(v)] = &vs[..]
            && *v < 0
        {
            self.diags.push(
                Diagnostic::error(
                    codes::MISMATCHED_TYPES,
                    format!("`{}` is an array length, so it can't be negative", g.name),
                )
                .primary(span, format!("`{}` = {v}", g.name))
                .secondary(g.span, "declared here"),
            );
            return Ty::Error;
        }
        let fits = |v: &LitVal| match (v, ct) {
            (LitVal::Int(v), Ty::Int(it)) => (it.min()..=it.max()).contains(v),
            (LitVal::Bool(_), Ty::Bool) | (LitVal::Str(_), Ty::Str) => true,
            _ => false,
        };
        match t {
            Ty::Error => Ty::Error,
            Ty::Lits(_, vs) if vs.len() == 1 && fits(&vs[0]) => {
                Ty::Lits(Box::new(ct.clone()), vs.clone())
            }
            Ty::Param(..) if self.const_param_ty(t) == *ct => t.clone(),
            Ty::Lits(_, vs) if vs.len() == 1 => {
                self.diags.push(
                    Diagnostic::error(
                        codes::MISMATCHED_TYPES,
                        format!("`{}` isn't a value of type `{ct}`", vs[0]),
                    )
                    .primary(span, format!("`{}` takes a `{ct}`", g.name))
                    .secondary(g.span, "declared here"),
                );
                Ty::Error
            }
            _ => {
                self.not_constant(span, &g.name);
                Ty::Error
            }
        }
    }

    fn not_constant(&mut self, span: Span, name: &str) {
        self.diags.push(
            Diagnostic::error(
                codes::COMPTIME_ARG,
                format!("the `comptime` parameter `{name}` takes a constant"),
            )
            .primary(span, "not known at compile time")
            .help("pass a literal, or a `comptime` parameter of the code around it"),
        );
    }

    fn call_generic(&mut self, e: &Expr, id: FnId, args: &[Expr], explicit: &[Ty]) -> Ty {
        let f = self.program.func(id);
        let params = f.generics.clone();
        let args: Vec<&Expr> = args.iter().collect();
        let (ret, targs) = self.call_inferring(e, id, &args, explicit, id.0, &params, None);
        // `JSON.parse<T>` / `JSON.stringify(v)`: `T` needs a JSON form (generic code is
        // checked per instance when it's lowered).
        if matches!(f.name.as_str(), "parse" | "stringify")
            && let Some(owner) = f.owner
            && self.program.struct_def(owner).name == "JSON"
            && self.program.structs.iter().position(|d| d.name == "JSON") == Some(owner.0 as usize)
            && let Some(t) = targs.first()
            && !t.has_any_params()
            && let Err(bad) = json_ok(self.program, t, &mut Vec::new())
        {
            self.diags.push(json_error(t, &bad, e.span));
        }
        self.types.instances.insert(e.id, targs);
        ret
    }

    /// Checks a call of function `id`, inferring the type parameters `params` of `owner`
    /// (the function's own, or a generic class's for its constructor) after substituting
    /// `pre` (the receiver's type arguments, for methods of generic types). Returns the result
    /// type and the inferred arguments.
    #[allow(clippy::too_many_arguments)]
    fn call_inferring(
        &mut self,
        e: &Expr,
        id: FnId,
        args: &[&Expr],
        explicit: &[Ty],
        owner: u32,
        generics: &[TypeParam],
        pre: Option<(u32, &[Ty])>,
    ) -> (Ty, Vec<Ty>) {
        let f = self.program.func(id);
        let n = generics.len();
        if f.ret.is_none() && self.state[id.0 as usize] == FnState::Unchecked {
            self.check_fn(id);
        }
        let mut sig = self.types.sigs[id.0 as usize].clone();
        if let Some((o, a)) = pre {
            sig = FnSig {
                params: sig.params.iter().map(|p| p.subst(o, a)).collect(),
                ret: sig.ret.subst(o, a),
            };
        }
        // `comptime T: type` arguments are types, not runtime arguments: set them aside
        // (the slots skip them).
        let type_params: Vec<(usize, usize)> = generics
            .iter()
            .enumerate()
            .filter(|(_, g)| g.comptime.is_none())
            .filter_map(|(i, g)| g.value_param.map(|k| (i, k)))
            .collect();
        let skipped: HashSet<usize> = type_params.iter().map(|(_, k)| *k).collect();
        let mut type_values: Vec<(usize, Ty)> = Vec::new();
        let kept: Vec<&Expr>;
        let args = if type_params.is_empty() {
            args
        } else {
            for (i, k) in &type_params {
                let Some(a) = args.get(*k) else { continue };
                self.set(a, Ty::Void);
                let t = match &a.kind {
                    ExprKind::TypeValue(t) => t.clone(),
                    _ => {
                        self.diags.push(
                            Diagnostic::error(
                                codes::COMPTIME_ARG,
                                format!(
                                    "the `comptime` parameter `{}` takes a type",
                                    generics[*i].name
                                ),
                            )
                            .primary(a.span, "not a type"),
                        );
                        Ty::Error
                    }
                };
                type_values.push((*i, t));
            }
            self.types.arg_slots.entry(e.id).or_insert_with(|| {
                (0..args.len())
                    .filter(|j| !skipped.contains(j))
                    .map(Slot::Arg)
                    .collect()
            });
            kept = args
                .iter()
                .enumerate()
                .filter(|(j, _)| !skipped.contains(j))
                .map(|(_, a)| *a)
                .collect();
            &kept[..]
        };
        // `name: value` arguments (each parameter given once; no rest or defaults): in
        // parameter order, recording the slots so lowering evaluates them that way.
        let reordered;
        let mut args = args;
        if args
            .iter()
            .any(|a| matches!(a.kind, ExprKind::NamedArg { .. }))
            && !f.rest
            && !args.iter().any(|a| matches!(a.kind, ExprKind::Spread(_)))
        {
            let names: Vec<&str> = f.params.iter().map(|p| f.local(*p).name.as_str()).collect();
            let mut order: Vec<Option<usize>> = vec![None; names.len()];
            let mut next = 0;
            let mut ok = true;
            for (i, a) in args.iter().enumerate() {
                let at = match &a.kind {
                    ExprKind::NamedArg {
                        name, name_span, ..
                    } => match names.iter().position(|n| n == name) {
                        Some(k) => k,
                        None => {
                            self.diags.push(
                                Diagnostic::error(
                                    codes::WRONG_ARG_COUNT,
                                    format!("`{}` has no parameter `{name}`", f.name),
                                )
                                .primary(*name_span, "unknown parameter"),
                            );
                            ok = false;
                            continue;
                        }
                    },
                    _ => {
                        next += 1;
                        next - 1
                    }
                };
                match order.get_mut(at) {
                    Some(slot @ None) => *slot = Some(i),
                    _ => ok = false,
                }
            }
            if ok && order.iter().all(Option::is_some) {
                let order: Vec<usize> = order.into_iter().map(|o| o.expect("checked")).collect();
                self.types
                    .arg_slots
                    .insert(e.id, order.iter().map(|i| Slot::Arg(*i)).collect());
                reordered = order
                    .iter()
                    .map(|i| Self::arg_value(args[*i]))
                    .collect::<Vec<&Expr>>();
                args = &reordered;
            }
        }
        let mut subst: Vec<Option<Ty>> = vec![None; n];
        for (i, t) in type_values {
            subst[i] = Some(t);
        }
        // The hidden parameters of `comptime` value parameters come last; they aren't
        // written as type arguments.
        let n_shown = generics.iter().filter(|g| g.value_param.is_none()).count();
        // Missing trailing type arguments take their defaults (`Promise<int>`).
        let filled: Vec<Ty>;
        let explicit = if !explicit.is_empty()
            && explicit.len() < n_shown
            && generics[explicit.len()..n_shown]
                .iter()
                .all(|g| g.default.is_some())
        {
            filled = explicit
                .iter()
                .cloned()
                .chain(
                    generics[explicit.len()..n_shown]
                        .iter()
                        .map(|g| g.default.clone().expect("checked")),
                )
                .collect();
            &filled[..]
        } else {
            explicit
        };
        if !explicit.is_empty() {
            let n = n_shown;
            if explicit.len() != n {
                self.diags.push(
                    Diagnostic::error(
                        codes::WRONG_ARG_COUNT,
                        format!(
                            "`{}` takes {n} type argument{}, but {} were given",
                            f.name,
                            if n == 1 { "" } else { "s" },
                            explicit.len()
                        ),
                    )
                    .primary(e.span, "wrong number of type arguments"),
                );
            }
            for (i, t) in explicit.iter().enumerate().take(n) {
                subst[i] = Some(self.type_arg(e.span, &generics[i], t));
            }
        }
        // Trailing parameters with defaults may be left out (positional arguments only):
        // the callee evaluates them.
        // (Also when all are given: the callee takes a "given" flag per defaulted one.)
        if args.len() <= sig.params.len()
            && f.defaults.iter().any(Option::is_some)
            && !self.types.arg_slots.contains_key(&e.id)
            && !args
                .iter()
                .any(|a| matches!(a.kind, ExprKind::NamedArg { .. } | ExprKind::Spread(_)))
            && (args.len()..sig.params.len())
                .all(|k| f.defaults.get(k).is_some_and(Option::is_some))
        {
            let slots = (0..sig.params.len())
                .map(|k| {
                    if k < args.len() {
                        Slot::Arg(k)
                    } else {
                        Slot::Default
                    }
                })
                .collect();
            self.types.arg_slots.insert(e.id, slots);
            sig.params.truncate(args.len());
        }
        if args.len() != sig.params.len() {
            self.diags.push(
                Diagnostic::error(
                    codes::WRONG_ARG_COUNT,
                    format!(
                        "`{}` takes {} argument{}, but {} were given",
                        f.name,
                        sig.params.len(),
                        if sig.params.len() == 1 { "" } else { "s" },
                        args.len()
                    ),
                )
                .primary(e.span, "wrong number of arguments")
                .secondary(f.name_span, "declared here"),
            );
            for a in args {
                self.infer(a);
            }
            return (Ty::Error, vec![Ty::Error; n]);
        }
        let pairs: Vec<(&Expr, &Ty)> = args.iter().copied().zip(&sig.params).collect();
        let mut done = vec![false; pairs.len()];
        let mut inferred = vec![false; pairs.len()];
        // Typed arguments first, so literals can adapt: `max(x, 1)` with `x: i32`.
        // Then literals (they can decide a type), then `[]`, `null` and object literals.
        for pass in 0..3 {
            for (k, (a, p)) in pairs.iter().enumerate() {
                // Lambdas last: their parameter types come from what's known by then.
                let lambda = matches!(a.kind, ExprKind::Closure(_));
                let skip = match pass {
                    0 => needs_context(a) || lambda,
                    1 => needs_context(a) && !untyped_lit(a) || lambda,
                    _ => false,
                };
                if done[k] || skip {
                    continue;
                }
                let known = p.subst(
                    owner,
                    &subst
                        .iter()
                        .map(|t| t.clone().unwrap_or(Ty::Error))
                        .collect::<Vec<_>>(),
                );
                if !p.has_params(owner)
                    || !known.has_params(owner) && subst_complete(p, owner, &subst)
                {
                    self.check(a, &Expect::new(known));
                } else {
                    let at = if lambda {
                        self.infer_with(a, Some(&Expect::new(known)))
                    } else if let (
                        ExprKind::Object {
                            ty: None, fields, ..
                        },
                        Ty::Struct(sid, _, _),
                    ) = (&a.kind, *p)
                    {
                        // `f({ get: …, set: … })` for `f<T>(o: Options<T>)`: the literal
                        // builds the parameter's struct, its type arguments inferred from
                        // the fields (as for `Options { … }`).
                        let t = self.object(a, Some(*sid), fields, &[], None);
                        self.set(a, t)
                    } else {
                        self.infer(a)
                    };
                    self.unify(owner, p, &at, &mut subst);
                    inferred[k] = true;
                }
                done[k] = true;
            }
        }
        // `comptime` value parameters: the argument's constant decides the instance.
        for (i, g) in generics.iter().enumerate() {
            let Some(k) = g.value_param else { continue };
            if g.comptime.is_none() {
                continue;
            }
            // (Its position among the runtime arguments.)
            let k = k - skipped.iter().filter(|s| **s < k).count();
            let Some((a, _)) = pairs.get(k) else { continue };
            let a = Self::arg_value(a);
            subst[i] = Some(match const_of(a) {
                Some(c) => self.type_arg(a.span, g, &c),
                None => {
                    self.not_constant(a.span, &g.name);
                    Ty::Error
                }
            });
        }
        let mut args_ty: Vec<Ty> = Vec::new();
        for (i, t) in subst.iter().enumerate() {
            match t {
                Some(t) => args_ty.push(t.clone()),
                None if generics[i].default.is_some() => {
                    args_ty.push(generics[i].default.clone().expect("checked"))
                }
                // An error parameter nothing determines: nothing is thrown.
                None if generics[i].bounds.contains(&Bound::Error) => args_ty.push(Ty::Never),
                None => {
                    let name = &generics[i].name;
                    self.diags.push(
                        Diagnostic::error(
                            codes::CANT_INFER,
                            format!("can't infer the type argument `{name}`"),
                        )
                        .primary(
                            e.span,
                            format!("`{name}` isn't determined by the arguments"),
                        )
                        .help(format!("pass it explicitly: `{}<…>(…)`", f.name)),
                    );
                    args_ty.push(Ty::Error);
                }
            }
        }
        // Arguments inferred into a parameter type: convert them to the final types.
        for (k, (a, p)) in pairs.iter().enumerate() {
            if inferred[k] {
                let want = p.subst(owner, &args_ty);
                let found = self.types.expr(a.id).clone();
                if found != want && !found.is_error() && !want.is_error() {
                    self.coerce(a, found, &Expect::new(want));
                }
            }
        }
        for (i, tp) in generics.iter().enumerate() {
            for b in &tp.bounds {
                if !self.satisfies(&args_ty[i], b) {
                    let what = match b {
                        Bound::Comparable => "`Comparable`".to_string(),
                        Bound::Equatable => "`Equatable`".to_string(),
                        Bound::Error => "`Error`".to_string(),
                        Bound::Hashable => "`Hashable`".to_string(),
                        Bound::Object => "`object`".to_string(),
                        Bound::Sendable => "`Sendable`".to_string(),
                        Bound::Iface(s) => format!("`{}`", self.program.struct_def(*s).name),
                    };
                    let (fails, must) = if *b == Bound::Object {
                        (
                            "isn't a class or an array".to_string(),
                            "be a class or an array (`object`)".to_string(),
                        )
                    } else {
                        (
                            format!("doesn't implement {what}"),
                            format!("implement {what}"),
                        )
                    };
                    self.diags.push(
                        Diagnostic::error(
                            codes::UNSATISFIED_BOUND,
                            format!("`{}` {fails}", args_ty[i]),
                        )
                        .primary(e.span, format!("`{}` = `{}` here", tp.name, args_ty[i]))
                        .secondary(tp.span, format!("`{}` must {must}", tp.name)),
                    );
                }
            }
        }
        (sig.ret.subst(owner, &args_ty), args_ty)
    }

    /// A method call on a value of a type parameter: `a.compareTo(b)` and the methods of
    /// interface bounds.
    fn param_method(
        &mut self,
        e: &Expr,
        t: &Ty,
        name: &str,
        name_span: Span,
        args: &[Expr],
    ) -> Option<Ty> {
        let bounds = self.bounds_of(t);
        let builtin = match name {
            "compareTo" if bounds.contains(&Bound::Comparable) => Some((Ty::Int(IntTy::I32), 1)),
            "hashCode" if bounds.contains(&Bound::Hashable) => Some((Ty::Int(IntTy::U64), 0)),
            "equals"
                if bounds.iter().any(|b| {
                    matches!(b, Bound::Comparable | Bound::Equatable | Bound::Hashable)
                }) =>
            {
                Some((Ty::Bool, 1))
            }
            _ => None,
        };
        if let Some((ret, n)) = builtin {
            if args.len() != n {
                self.diags.push(
                    Diagnostic::error(
                        codes::WRONG_ARG_COUNT,
                        format!(
                            "`{name}` takes {n} argument{}",
                            if n == 1 { "" } else { "s" }
                        ),
                    )
                    .primary(e.span, "wrong number of arguments"),
                );
            }
            for a in args {
                self.check(a, &Expect::new(t.clone()));
            }
            self.types.param_calls.insert(e.id, name.to_string());
            return Some(ret);
        }
        for b in bounds {
            if let Bound::Iface(i) = b
                && let Some(m) = self.program.struct_def(*i).method(name)
            {
                self.types.param_calls.insert(e.id, name.to_string());
                return Some(self.call_fn(e, m, args));
            }
        }
        let _ = name_span;
        None
    }

    fn builtin(&mut self, e: &Expr, b: Builtin, args: &[Expr]) -> Ty {
        let arity = |n: &[usize]| n.contains(&args.len());
        let (ok, ret) = match b {
            Builtin::Print | Builtin::Println => {
                for a in args {
                    let t = self.infer(a);
                    if !is_printable(&t) {
                        self.diags.push(
                            Diagnostic::error(
                                codes::INVALID_OPERANDS,
                                format!("`{t}` can't be printed"),
                            )
                            .primary(a.span, "no string form"),
                        );
                    }
                }
                (
                    if b == Builtin::Print {
                        arity(&[1])
                    } else {
                        arity(&[0, 1])
                    },
                    Ty::Void,
                )
            }
            Builtin::Assert => {
                if let Some(c) = args.first() {
                    self.check(c, &Expect::new(Ty::Bool));
                }
                if let Some(m) = args.get(1) {
                    self.check(m, &Expect::new(Ty::Str));
                }
                (arity(&[1, 2]), Ty::Void)
            }
            Builtin::Panic => {
                if let Some(m) = args.first() {
                    self.check(m, &Expect::new(Ty::Str));
                }
                (arity(&[1]), Ty::Never)
            }
            Builtin::GcStat => {
                if let Some(i) = args.first() {
                    self.check(i, &Expect::new(Ty::INT));
                }
                (arity(&[1]), Ty::INT)
            }
            Builtin::RawArenaNew => {
                if let Some(c) = args.first() {
                    self.check(c, &Expect::new(Ty::INT));
                }
                (arity(&[1]), Ty::INT)
            }
            Builtin::RawArenaFree | Builtin::LoopWait => {
                if let Some(c) = args.first() {
                    self.check(c, &Expect::new(Ty::INT));
                }
                (arity(&[1]), Ty::Void)
            }
            Builtin::ClockMs => (arity(&[0]), Ty::INT),
            Builtin::ParseF64 => {
                if let Some(s) = args.first() {
                    self.check(s, &Expect::new(Ty::Str));
                }
                (arity(&[1]), Ty::Float(FloatTy::F64))
            }
            Builtin::CatchPanic => {
                let f = Ty::Fn(
                    std::sync::Arc::from([]),
                    Box::new(Ty::Void),
                    std::sync::Arc::from([]),
                );
                if let Some(a) = args.first() {
                    self.check(a, &Expect::new(f));
                }
                (arity(&[1]), Ty::Str.nullable())
            }
            Builtin::PanicTaken => (arity(&[0]), Ty::Str),
            Builtin::Math => {
                for (i, a) in args.iter().enumerate() {
                    let t = if i == 0 {
                        Ty::INT
                    } else {
                        Ty::Float(FloatTy::F64)
                    };
                    self.check(a, &Expect::new(t));
                }
                (arity(&[3]), Ty::Float(FloatTy::F64))
            }
            Builtin::ComptimeInfo => {
                if let Some(k) = args.first() {
                    self.check(k, &Expect::new(Ty::INT));
                }
                (arity(&[1]), Ty::Str)
            }
            Builtin::ComptimeReadFile => {
                if let Some(p) = args.first() {
                    self.check(p, &Expect::new(Ty::Str));
                }
                (arity(&[1]), Ty::Str)
            }
            Builtin::MetaEmit => {
                for (i, a) in args.iter().enumerate() {
                    let t = if i == 0 { Ty::INT } else { Ty::Str };
                    self.check(a, &Expect::new(t));
                }
                (arity(&[4]), Ty::Void)
            }
            Builtin::WorkerOp => {
                for a in args {
                    self.check(a, &Expect::new(Ty::INT));
                }
                (arity(&[2]), Ty::INT)
            }
            Builtin::WorkerPost => {
                if let Some(h) = args.first() {
                    self.check(h, &Expect::new(Ty::INT));
                }
                if let Some(v) = args.get(1) {
                    self.infer(v);
                }
                if let Some(t) = args.get(2) {
                    self.check(t, &Expect::new(Ty::Array(Box::new(Ty::INT))));
                }
                (arity(&[2, 3]), Ty::Void)
            }
            Builtin::BufOp => {
                for a in args {
                    self.check(a, &Expect::new(Ty::INT));
                }
                (arity(&[4]), Ty::INT)
            }
            Builtin::Io => {
                for (i, a) in args.iter().enumerate() {
                    self.check(a, &Expect::new(if i < 4 { Ty::INT } else { Ty::Str }));
                }
                (arity(&[6]), Ty::INT)
            }
            Builtin::IoText => {
                if let Some(h) = args.first() {
                    self.check(h, &Expect::new(Ty::INT));
                }
                (arity(&[1]), Ty::Str)
            }
            Builtin::WorkerError => {
                if let Some(h) = args.first() {
                    self.check(h, &Expect::new(Ty::INT));
                }
                (arity(&[1]), Ty::Str)
            }
            Builtin::ArenaEnter | Builtin::ArenaExit | Builtin::ArenaExitRescue => {
                (arity(&[0]), Ty::Void)
            }
            Builtin::RefId => {
                if let Some(x) = args.first() {
                    let t = self.infer(x);
                    if !self.satisfies(&t, &Bound::Object) {
                        self.diags.push(
                            Diagnostic::error(
                                codes::MISMATCHED_TYPES,
                                format!("`{t}` isn't an object"),
                            )
                            .primary(x.span, "expected a class instance or an array"),
                        );
                    }
                }
                (arity(&[1]), Ty::INT)
            }
            Builtin::ReactivityOn => (arity(&[0]), Ty::Bool),
            Builtin::Identical => {
                if let [a, b] = args {
                    let t = self.infer(a);
                    self.check(b, &Expect::new(t));
                }
                (arity(&[2]), Ty::Bool)
            }
            Builtin::GcCollect
            | Builtin::GcCollectYoung
            | Builtin::GcPause
            | Builtin::GcResume
            | Builtin::GcDumpRoots => (arity(&[0]), Ty::Void),
        };
        if !ok {
            let expected = match b {
                Builtin::Print
                | Builtin::ParseF64
                | Builtin::Panic
                | Builtin::GcStat
                | Builtin::RefId
                | Builtin::RawArenaNew
                | Builtin::RawArenaFree
                | Builtin::LoopWait
                | Builtin::WorkerError
                | Builtin::ComptimeInfo
                | Builtin::ComptimeReadFile => "1 argument",
                Builtin::WorkerOp | Builtin::Identical => "2 arguments",
                Builtin::Math => "3 arguments",
                Builtin::CatchPanic => "1 argument",
                Builtin::PanicTaken => "no arguments",
                Builtin::WorkerPost => "2 or 3 arguments",
                Builtin::BufOp | Builtin::MetaEmit => "4 arguments",
                Builtin::Io => "6 arguments",
                Builtin::IoText => "1 argument",
                Builtin::ClockMs | Builtin::ReactivityOn => "no arguments",
                Builtin::ArenaEnter | Builtin::ArenaExit | Builtin::ArenaExitRescue => {
                    "no arguments"
                }
                Builtin::GcCollect
                | Builtin::GcCollectYoung
                | Builtin::GcPause
                | Builtin::GcResume
                | Builtin::GcDumpRoots => "no arguments",
                Builtin::Println => "0 or 1 arguments",
                Builtin::Assert => "1 or 2 arguments",
            };
            self.diags.push(
                Diagnostic::error(
                    codes::WRONG_ARG_COUNT,
                    format!(
                        "`{}` takes {expected}, but {} were given",
                        b.name(),
                        args.len()
                    ),
                )
                .primary(e.span, "wrong number of arguments"),
            );
        }
        ret
    }
}

/// The variable a place expression starts from (`a` in `a.b[i].c`).
fn root_var(e: &Expr) -> Option<Var> {
    match &e.kind {
        ExprKind::Var(v) => Some(*v),
        ExprKind::Member { object, .. } | ExprKind::Index { object, .. } => root_var(object),
        ExprKind::NonNull(inner) => root_var(inner),
        _ => None,
    }
}
