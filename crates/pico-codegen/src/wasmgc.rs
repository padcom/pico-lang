//! The WasmGC backend (P8.7, decision 264): a WebAssembly module emitted straight from MIR,
//! whose pico values are WasmGC values, so objects live on the browser's garbage-collected
//! heap instead of the pico GC's linear memory. LLVM can't emit WasmGC, hence a backend of its
//! own (`wasm-encoder`).
//!
//! Slice P8.7a covers numbers, `bool`, `char`, strings, structs and classes without virtual
//! dispatch, globals, calls and control flow: everything else is reported as "not supported by
//! the WasmGC backend yet", which the golden harness counts as pending.
//!
//! - Integers up to 32 bits are `i32` (sign- or zero-extended as their type is), wider ones
//!   `i64`; arithmetic checks overflow, division by zero and shift amounts as native code does.
//! - Strings are `(array (mut i8))` of UTF-8; structs and classes are GC structs (a struct is
//!   copied when it's read from a place, so it keeps value semantics; a reference to a struct
//!   is the struct itself).
//! - Control flow: each function is a loop around a `br_table` over its MIR blocks.
//! - The glue (`glue_gc.js`) prints, formats numbers (pico's float format is JavaScript's) and
//!   reports panics; it reads strings through the module's `str_*` exports.

use pico_mir::mir::{
    self, BinOp, Callee, Const, Operand, Place, Program, Proj, Rvalue, Terminator, UnOp,
};
use pico_sema::hir::{Builtin, FnId, StructId};
use pico_sema::ty::EnumId;
use pico_sema::{FloatTy, IntTy, Ty};
use std::cell::RefCell;
use std::collections::HashMap;
use wasm_encoder::{
    BlockType, CodeSection, CompositeInnerType, CompositeType, ConstExpr, DataCountSection,
    DataSection, ElementSection, Elements, EntityType, ExportKind, ExportSection, FieldType,
    Function, FunctionSection, GlobalSection, GlobalType, HeapType, Ieee32, Ieee64, ImportSection,
    InstructionSink, Module, NameMap, NameSection, RefType, StorageType, StructType, SubType,
    TableSection, TableType, TagKind, TagSection, TagType, TypeSection, ValType,
};

type R<T> = Result<T, String>;

fn unsupported<T>(what: impl std::fmt::Display) -> R<T> {
    Err(format!("not supported by the WasmGC backend yet: {what}"))
}

/// The marker the golden harness looks for (such programs are pending, not failing).
pub const UNSUPPORTED: &str = "not supported by the WasmGC backend yet";

/// Placeholder for values of types this backend can't represent yet.
const ANYREF: ValType = ValType::Ref(RefType::ANYREF);

/// Globals after the program's: the pending exception and its tag, the panic's message, line
/// and column, the event loop's wait.
/// Then a synchronous `__io` job's results (`r`, `r2`, text) and the `ArrayBuffer` table.
const FIXED_GLOBALS: usize = 10;

/// The string type's index (the first type of the rec group).
const STR: u32 = 0;

/// The JavaScript imports, in order (module `pico`).
const IMPORTS: &[(&str, &[ValT], &[ValT])] = &[
    ("print_str", &[ValT::Str], &[]),
    ("print_i64", &[ValT::I64, ValT::I32], &[]),
    ("print_f64", &[ValT::F64], &[]),
    ("print_f32", &[ValT::F32], &[]),
    ("print_char", &[ValT::I32], &[]),
    ("print_nl", &[], &[]),
    ("fmt_i64", &[ValT::I64, ValT::I32], &[ValT::Str]),
    ("fmt_f64", &[ValT::F64], &[ValT::Str]),
    ("fmt_f32", &[ValT::F32], &[ValT::Str]),
    ("fmt_char", &[ValT::I32], &[ValT::Str]),
    ("panic", &[ValT::Str, ValT::I32, ValT::I32], &[]),
    ("uncaught", &[ValT::Str, ValT::Str], &[]),
    ("show_enter", &[ValT::Any], &[ValT::I32]),
    ("show_leave", &[ValT::Any], &[]),
    ("pow_f64", &[ValT::F64, ValT::F64], &[ValT::F64]),
    ("clock_ms", &[], &[ValT::I64]),
    ("clock_ns", &[ValT::I32], &[ValT::I64]),
    (
        "decode_utf8",
        &[ValT::Str, ValT::I32, ValT::I32, ValT::I32],
        &[ValT::Str],
    ),
    ("decode_bad_at", &[], &[ValT::I64]),
    ("fill_random", &[ValT::Str], &[]),
    ("parse_f64", &[ValT::Str], &[ValT::F64]),
    ("ref_id", &[ValT::Any], &[ValT::I64]),
    ("math", &[ValT::I32, ValT::F64, ValT::F64], &[ValT::F64]),
    // std/js (`__io` 150–168 but 163, decision 259): op, a, b, c, s, t.
    (
        "js_op",
        &[
            ValT::I32,
            ValT::I64,
            ValT::I64,
            ValT::I64,
            ValT::Str,
            ValT::Str,
        ],
        &[ValT::I64],
    ),
    // `String(value)` of a handle (`__io` 163).
    ("js_text", &[ValT::I64], &[ValT::Str]),
    // Format specs (`${x:08.2f}`): the value, then the spec's eight fields (as `pico_rt.h`'s
    // `PicoFmt`: fill, align, sign, alternate, zero, width, precision, kind).
    (
        "fmt_spec_i64",
        &[ValT::I64, ValT::I32, I, I, I, I, I, I, I, I],
        &[ValT::Str],
    ),
    (
        "fmt_spec_f64",
        &[ValT::F64, ValT::I32, I, I, I, I, I, I, I, I],
        &[ValT::Str],
    ),
    (
        "fmt_spec_str",
        &[ValT::Str, I, I, I, I, I, I, I, I],
        &[ValT::Str],
    ),
    (
        "fmt_spec_char",
        &[ValT::I32, I, I, I, I, I, I, I, I],
        &[ValT::Str],
    ),
    (
        "fmt_spec_bool",
        &[ValT::I32, I, I, I, I, I, I, I, I],
        &[ValT::Str],
    ),
];

const I: ValT = ValT::I32;

/// The module's own helpers (after the imports), with their exports.
const HELPERS: &[&str] = &[
    "str_len",
    "str_byte",
    "str_new",
    "str_put",
    "str_concat",
    "str_eq",
    "str_chars",
    "str_cmp",
    "mul_ovf_s",
    "mul_ovf_u",
];

#[derive(Clone, Copy, PartialEq)]
enum ValT {
    I32,
    I64,
    F32,
    F64,
    Str,
    Any,
}

fn import_index(name: &str) -> u32 {
    IMPORTS
        .iter()
        .position(|(n, ..)| *n == name)
        .expect("an import") as u32
}

fn helper_index(name: &str) -> u32 {
    IMPORTS.len() as u32 + HELPERS.iter().position(|n| *n == name).expect("a helper") as u32
}

fn str_ref() -> ValType {
    ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(STR),
    })
}

fn val(v: ValT) -> ValType {
    match v {
        ValT::I32 => ValType::I32,
        ValT::I64 => ValType::I64,
        ValT::F32 => ValType::F32,
        ValT::F64 => ValType::F64,
        ValT::Str => str_ref(),
        ValT::Any => ANYREF,
    }
}

/// Emits the module for `p`.
/// `tzdata` is the bundled tz database (`third_party/tzdata/tzdata.bin`), embedded when the
/// program can look zones up.
pub fn emit(p: &Program, tzdata: &[u8]) -> R<Vec<u8>> {
    let live = mir::live_functions(p);
    let mut g = Gen {
        p,
        reg: RefCell::new(Registry::default()),
        data: Vec::new(),
        fn_index: HashMap::new(),
        extra: Vec::new(),
        extra_keys: HashMap::new(),
        first_extra: 0,
        closure_targets: std::collections::HashSet::new(),
        itable_base: Vec::new(),
        vt_globals: Vec::new(),
        refd: Vec::new(),
        tzdata: tzdata.to_vec(),
    };
    let mut next = (IMPORTS.len() + HELPERS.len()) as u32;
    for (i, l) in live.iter().enumerate() {
        if *l {
            g.fn_index.insert(FnId(i as u32), next);
            next += 1;
        }
    }
    g.first_extra = next;
    // Closure targets take their environment as `anyref` (one signature per function type).
    for (i, f) in p.fns.iter().enumerate() {
        if !live[i] {
            continue;
        }
        for b in &f.blocks {
            for st in &b.stmts {
                if let Rvalue::Closure(c, _) = &st.rvalue {
                    g.closure_targets.insert(*c);
                }
            }
        }
    }
    // Interface method tables, flattened into the funcref table.
    let mut base = 0u32;
    for t in &p.itables {
        g.itable_base.push(base);
        base += t.len() as u32;
    }
    let mut code = CodeSection::new();
    g.helpers(&mut code);
    let mut func_types = Vec::new();
    for (i, f) in p.fns.iter().enumerate() {
        if !live[i] {
            continue;
        }
        func_types.push(g.fn_sig(FnId(i as u32))?);
        let body = g
            .function(FnId(i as u32), f)
            .map_err(|e| format!("{e} (in `{}`)", f.name))?;
        code.function(&body);
    }
    // Vtables (filled before the entry runs), then the start function; the event loop's
    // `resume` and `pending`.
    let start = g.start_fn()?;
    let (resume, pending) = g.loop_fns();
    let js_exports = g.js_exports()?;
    // On-demand helpers (array operations, copies, thunks), after the program's functions.
    let extra = std::mem::take(&mut g.extra);
    for (_, _, body) in &extra {
        code.function(body.as_ref().expect("built"));
    }
    // Every function's type, and the globals' (they may register more types).
    let mut import_types = Vec::new();
    for (_, ps, rs) in IMPORTS {
        import_types.push(g.func_type(
            ps.iter().map(|v| val(*v)).collect(),
            rs.iter().map(|v| val(*v)).collect(),
        ));
    }
    let panic_tag_type = g.func_type(Vec::new(), Vec::new());
    let mut fn_types = Vec::new();
    for h in HELPERS {
        let (ps, rs) = helper_sig(h);
        fn_types.push(g.func_type(ps, rs));
    }
    for (ps, rs) in func_types {
        fn_types.push(g.func_type(ps, rs));
    }
    for (ps, rs, _) in &extra {
        fn_types.push(g.func_type(ps.clone(), rs.clone()));
    }
    let mut global_types: Vec<ValType> = p
        .globals
        .iter()
        .map(|gl| g.wasm_ty(&gl.ty).unwrap_or(ANYREF))
        .collect();
    let vt = g.vt_type();
    global_types.push(ANYREF);
    global_types.push(ValType::I32);
    global_types.extend([str_ref(), ValType::I32, ValType::I32, ValType::I64]);
    let buffers = g.buffer_table();
    global_types.extend([ValType::I64, ValType::I64, str_ref(), ref_to(buffers)]);
    global_types.extend(g.vt_globals.iter().map(|_| ref_to(vt)));
    // The rec group: strings, then the registry's types. Struct fields can register more, so
    // they're resolved until nothing new comes.
    let mut resolved: Vec<CompositeInnerType> = Vec::new();
    let mut k = 0;
    loop {
        let def = {
            let r = g.reg.borrow();
            match r.defs.get(k) {
                Some(d) => d.clone(),
                None => break,
            }
        };
        resolved.push(g.composite(&def));
        k += 1;
    }
    let mut types = TypeSection::new();
    let mut group = vec![SubType {
        is_final: true,
        supertype_idxs: Vec::new(),
        composite_type: CompositeType {
            inner: CompositeInnerType::Array(wasm_encoder::ArrayType(FieldType {
                element_type: StorageType::I8,
                mutable: true,
            })),
            shared: false,
            descriptor: None,
            describes: None,
        },
    }];
    for (def, inner) in g.reg.borrow().defs.iter().zip(resolved) {
        let (is_final, base) = match def {
            Def::Struct { base, is_final, .. } => (*is_final, *base),
            Def::Enum => (false, None),
            Def::Variant(e, _, _) => (true, Some(*e)),
            _ => (true, None),
        };
        group.push(SubType {
            is_final,
            supertype_idxs: base.into_iter().collect(),
            composite_type: CompositeType {
                inner,
                shared: false,
                descriptor: None,
                describes: None,
            },
        });
    }
    types.ty().rec(group);
    let mut imports = ImportSection::new();
    for ((name, ..), t) in IMPORTS.iter().zip(import_types) {
        imports.import("pico", name, EntityType::Function(t));
    }
    let mut functions = FunctionSection::new();
    for t in fn_types {
        functions.function(t);
    }
    let mut tables = TableSection::new();
    let table_len = base;
    if table_len > 0 {
        tables.table(TableType {
            element_type: RefType::FUNCREF,
            minimum: table_len as u64,
            maximum: Some(table_len as u64),
            table64: false,
            shared: false,
        });
    }
    let mut globals = GlobalSection::new();
    // (A global of a type this backend can't hold yet is an `anyref` placeholder: only `null`
    // goes in.)
    let wait = g.wait_global() as usize;
    for (i, t) in global_types.into_iter().enumerate() {
        globals.global(
            GlobalType {
                val_type: t,
                mutable: true,
                shared: false,
            },
            &if i == wait {
                ConstExpr::i64_const(-1)
            } else {
                zero_const(t)
            },
        );
    }
    let mut exports = ExportSection::new();
    for (i, h) in HELPERS.iter().enumerate() {
        exports.export(h, ExportKind::Func, IMPORTS.len() as u32 + i as u32);
    }
    for (name, f) in &js_exports {
        exports.export(name, ExportKind::Func, *f);
    }
    exports.export("start", ExportKind::Func, start);
    exports.export("resume", ExportKind::Func, resume);
    exports.export("pending", ExportKind::Func, pending);
    exports.export("panic", ExportKind::Tag, 0);
    let pg = g.panic_global();
    exports.export("panic_msg", ExportKind::Global, pg);
    exports.export("panic_line", ExportKind::Global, pg + 1);
    exports.export("panic_col", ExportKind::Global, pg + 2);
    let mut tags = TagSection::new();
    tags.tag(TagType {
        kind: TagKind::Exception,
        func_type_idx: panic_tag_type,
    });
    let mut elements = ElementSection::new();
    if table_len > 0 {
        let funcs: Vec<u32> = p
            .itables
            .iter()
            .flatten()
            .map(|f| g.fn_index.get(f).copied().unwrap_or(0))
            .collect();
        elements.active(
            Some(0),
            &ConstExpr::i32_const(0),
            Elements::Functions(funcs.into()),
        );
    }
    if !g.refd.is_empty() {
        let funcs: Vec<u32> = g.refd.clone();
        elements.declared(Elements::Functions(funcs.into()));
    }
    let mut data = DataSection::new();
    for d in &g.data {
        data.passive(d.iter().copied());
    }
    let mut m = Module::new();
    m.section(&types)
        .section(&imports)
        .section(&functions)
        .section(&tables)
        .section(&tags)
        .section(&globals)
        .section(&exports)
        .section(&elements)
        .section(&DataCountSection {
            count: g.data.len() as u32,
        })
        .section(&code)
        .section(&data);
    // Function names, for stack traces.
    let mut names = NameMap::new();
    for (i, (n, ..)) in IMPORTS.iter().enumerate() {
        names.append(i as u32, n);
    }
    for (i, h) in HELPERS.iter().enumerate() {
        names.append((IMPORTS.len() + i) as u32, h);
    }
    let mut by_index: Vec<(u32, &str)> = g
        .fn_index
        .iter()
        .map(|(f, i)| (*i, p.fns[f.0 as usize].name.as_str()))
        .collect();
    by_index.sort();
    for (i, n) in by_index {
        names.append(i, n);
    }
    let mut keys: Vec<(&u32, &String)> = g.extra_keys.iter().map(|(k, v)| (v, k)).collect();
    keys.sort();
    for (i, k) in keys {
        names.append(*i, k);
    }
    let mut ns = NameSection::new();
    ns.functions(&names);
    m.section(&ns);
    Ok(m.finish())
}

fn zero_const(t: ValType) -> ConstExpr {
    match t {
        ValType::I32 => ConstExpr::i32_const(0),
        ValType::I64 => ConstExpr::i64_const(0),
        ValType::F32 => ConstExpr::f32_const(Ieee32::from(0.0f32)),
        ValType::F64 => ConstExpr::f64_const(Ieee64::from(0.0f64)),
        ValType::Ref(r) => ConstExpr::ref_null(r.heap_type),
        ValType::V128 => ConstExpr::i32_const(0),
    }
}

fn helper_sig(name: &str) -> (Vec<ValType>, Vec<ValType>) {
    use ValType::*;
    let s = str_ref();
    match name {
        "str_len" => (vec![s], vec![I32]),
        "str_byte" => (vec![s, I32], vec![I32]),
        "str_new" => (vec![I32], vec![s]),
        "str_put" => (vec![s, I32, I32], vec![]),
        "str_concat" => (vec![s, s], vec![s]),
        "str_eq" => (vec![s, s], vec![I32]),
        "str_chars" => (vec![s], vec![I64]),
        "str_cmp" => (vec![s, s], vec![I32]),
        "mul_ovf_s" | "mul_ovf_u" => (vec![I64, I64], vec![I32]),
        _ => unreachable!("helper {name}"),
    }
}

/// A GC type the module defines (after the string type).
#[derive(Clone)]
enum Def {
    /// A struct, class, tuple or range: its fields' pico types (a class's start with its base
    /// class's, whose struct is its supertype).
    Struct {
        fields: Vec<Ty>,
        base: Option<u32>,
        is_final: bool,
        /// A class: a vtable field comes first.
        class: bool,
        /// Per field: stored as `anyref` (a generic class's field whose type comes from a type
        /// parameter and is a reference: instances that differ only there share the type).
        erased: Vec<bool>,
    },
    /// A GC array of `E`: a fixed array, or a growable array's storage.
    Raw(ValType),
    /// A growable array: `{ data: (ref null raw), len: i32 }`.
    Grow(u32),
    /// A `T?` of a value type: a box struct `{ value }` (`null` is a null reference); also a
    /// captured variable's cell.
    Opt(ValType),
    /// A function type.
    Func(Vec<ValType>, Vec<ValType>),
    /// An enum's (or union's) values: `{ tag: i32 }`, the supertype of its variants'.
    Enum,
    /// One variant: `{ tag, fields… }` (the enum's struct type index, the enum, the variant).
    Variant(u32, EnumId, u32),
    /// A function value: `{ fn: (ref null func type), env: anyref }`.
    Clo(u32),
    /// An interface value: `{ data: anyref, itable: i32 }` (the table's offset).
    Iface,
    /// Vtables: `(array (ref null func))`.
    Vt,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Ty(Ty),
    Raw(ValType),
    Opt(ValType),
    Func(Vec<ValType>, Vec<ValType>),
    Enum(EnumId),
    Variant(EnumId, u32),
    Iface,
    Vt,
    Clo(u32),
    Gen(StructId, Vec<Option<ValType>>),
}

/// The GC types registered so far; `defs[i]` is type `i + 1`.
#[derive(Default)]
struct Registry {
    defs: Vec<Def>,
    index: HashMap<Key, u32>,
}

/// A helper function made on demand: its signature and body (`None` while it's built).
type Extra = (Vec<ValType>, Vec<ValType>, Option<Function>);

fn ref_to(idx: u32) -> ValType {
    ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(idx),
    })
}

struct Gen<'p> {
    p: &'p Program,
    reg: RefCell<Registry>,
    /// Passive data segments (string constants).
    data: Vec<Vec<u8>>,
    fn_index: HashMap<FnId, u32>,
    extra: Vec<Extra>,
    extra_keys: HashMap<String, u32>,
    /// The function index of the first on-demand helper.
    first_extra: u32,
    /// Functions made into closures: they take their environment as `anyref`.
    closure_targets: std::collections::HashSet<FnId>,
    /// Each interface method table's offset in the funcref table.
    itable_base: Vec<u32>,
    /// The classes whose vtable a global holds (global `globals.len() + k`).
    vt_globals: Vec<StructId>,
    /// Functions `ref.func` names (declared in an element segment).
    refd: Vec<u32>,
    /// The bundled tz database (a data segment once a zone lookup needs it).
    tzdata: Vec<u8>,
}

/// Where a MIR local lives.
#[derive(Clone, Copy)]
struct Slot {
    index: u32,
}

struct FnCx<'f> {
    f: &'f mir::Function,
    locals: Vec<Option<Slot>>,
    /// The block counter (`$pc`) and scratch locals.
    pc: u32,
    /// Scratch locals: one per wasm type (`scratch`), and numbered ones (`tmp`).
    scratch: HashMap<ValType, u32>,
    tmps: HashMap<u32, u32>,
    /// A function with panic guards: its guard stack (handler blocks) and depth locals.
    guards: Option<(u32, u32)>,
    /// Each local's wasm type (`anyref` for one that takes exceptions, which may be boxed
    /// throwables rather than `Error`s).
    local_types: Vec<Option<ValType>>,
    nlocals: u32,
    extra: Vec<ValType>,
}

impl FnCx<'_> {
    /// A scratch local of type `t` (one per type).
    fn scratch(&mut self, t: ValType) -> u32 {
        if let Some(&i) = self.scratch.get(&t) {
            return i;
        }
        let i = self.nlocals;
        self.nlocals += 1;
        self.extra.push(t);
        self.scratch.insert(t, i);
        i
    }

    /// Numbered scratch local `key` of type `t` (several of one type at once).
    fn tmp(&mut self, key: u32, t: ValType) -> u32 {
        if let Some(&i) = self.tmps.get(&key) {
            return i;
        }
        let i = self.nlocals;
        self.nlocals += 1;
        self.extra.push(t);
        self.tmps.insert(key, i);
        i
    }
}

fn int_bits(it: IntTy) -> u32 {
    it.bits()
}

fn wide(it: IntTy) -> bool {
    int_bits(it) > 32
}

impl<'p> Gen<'p> {
    /// Registers a GC type under `key` (`make` builds its definition; it may register the
    /// types it refers to first, which keeps supertypes before their subtypes).
    fn register(&self, key: Key, make: impl FnOnce(&Self) -> Def) -> u32 {
        if let Some(&i) = self.reg.borrow().index.get(&key) {
            return i;
        }
        let def = make(self);
        let mut r = self.reg.borrow_mut();
        if let Some(&i) = r.index.get(&key) {
            return i;
        }
        r.defs.push(def);
        let i = r.defs.len() as u32;
        r.index.insert(key, i);
        i
    }

    /// Whether wasm field `i` of struct type `idx` is erased to `anyref`.
    fn erased_field(&self, idx: u32, i: u32) -> bool {
        let r = self.reg.borrow();
        match r.defs.get(idx as usize - 1) {
            Some(Def::Struct { erased, class, .. }) => (i as usize)
                .checked_sub(*class as usize)
                .is_some_and(|k| erased.get(k).copied().unwrap_or(false)),
            _ => false,
        }
    }

    /// The type index of a struct, class, tuple or range type.
    fn struct_type(&self, t: &Ty) -> u32 {
        let t = t.erase();
        match &t {
            Ty::Struct(sid, _, args) | Ty::Class(sid, _, args) => {
                let is_class = matches!(t, Ty::Class(..));
                let layout = self.p.layout(*sid);
                let owner = Ty::type_owner(*sid);
                let fields: Vec<Ty> = layout
                    .fields
                    .iter()
                    .map(|(_, ft)| ft.subst(owner, args))
                    .collect();
                let base = layout.meta.base.filter(|_| is_class);
                // A generic class: fields typed by its parameters, when references, are erased
                // to `anyref`, and the type is keyed by that layout (so `Promise<int, never>`
                // fits where `Promise<int, Error>` is expected).
                let generic = is_class && !args.is_empty();
                let erased: Vec<bool> = layout
                    .fields
                    .iter()
                    .zip(&fields)
                    .map(|((_, declared), ft)| {
                        generic
                            && declared != ft
                            && !matches!(
                                ft.erase(),
                                Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char
                            )
                    })
                    .collect();
                let key = if generic {
                    Key::Gen(
                        *sid,
                        fields
                            .iter()
                            .zip(&erased)
                            .map(|(ft, e)| if *e { None } else { self.wasm_ty(ft) })
                            .collect(),
                    )
                } else {
                    Key::Ty(t.clone())
                };
                self.register(key, |g| Def::Struct {
                    fields,
                    erased,
                    class: is_class,
                    base: base.map(|b| {
                        g.struct_type(&Ty::Class(
                            b,
                            g.p.layout(b).name.as_str().into(),
                            Vec::new().into(),
                        ))
                    }),
                    is_final: !is_class,
                })
            }
            Ty::Tuple(ts) => self.register(Key::Ty(t.clone()), |_| Def::Struct {
                erased: vec![false; ts.len()],
                fields: ts.clone(),
                base: None,
                is_final: true,
                class: false,
            }),
            Ty::Range(it) => self.register(Key::Ty(t.clone()), |_| Def::Struct {
                erased: vec![false; 3],
                fields: vec![Ty::Int(*it); 3],
                base: None,
                is_final: true,
                class: false,
            }),
            _ => unreachable!("struct_type of {t}"),
        }
    }

    /// The root of a class's hierarchy (the class itself without a base).
    fn root_class(&self, t: &Ty) -> Ty {
        let Ty::Class(sid, ..) = t else {
            return t.clone();
        };
        let mut c = *sid;
        while let Some(b) = self.p.layout(c).meta.base {
            c = b;
        }
        if c == *sid {
            t.clone()
        } else {
            Ty::Class(c, self.p.layout(c).name.as_str().into(), Vec::new().into())
        }
    }

    /// An array element's storage type (`void` elements are `anyref` placeholders).
    fn elem_ty(&self, t: &Ty) -> Option<ValType> {
        if no_value(t) {
            Some(ANYREF)
        } else {
            self.wasm_ty(t)
        }
    }

    fn elem_storage(&self, t: &Ty) -> R<ValType> {
        if no_value(t) {
            return Ok(ANYREF);
        }
        Ok(self.ty_or_err(t)?.expect("a value"))
    }

    fn raw_type(&self, e: ValType) -> u32 {
        self.register(Key::Raw(e), |_| Def::Raw(e))
    }

    /// A function type's index.
    fn func_type(&self, ps: Vec<ValType>, rs: Vec<ValType>) -> u32 {
        self.register(Key::Func(ps.clone(), rs.clone()), |_| Def::Func(ps, rs))
    }

    /// The function type behind function values of `(ps) => r`: the environment (`anyref`)
    /// first.
    fn closure_sig(&self, ps: &[Ty], r: &Ty) -> Option<u32> {
        let mut params = vec![ANYREF];
        for t in ps {
            if !no_value(t) {
                params.push(self.wasm_ty(t)?);
            }
        }
        let rs = if no_value(r) {
            Vec::new()
        } else {
            vec![self.wasm_ty(r)?]
        };
        Some(self.func_type(params, rs))
    }

    fn enum_type(&self, eid: EnumId) -> u32 {
        self.register(Key::Enum(eid), |_| Def::Enum)
    }

    fn variant_type(&self, eid: EnumId, v: u32) -> u32 {
        let base = self.enum_type(eid);
        self.register(Key::Variant(eid, v), |_| Def::Variant(base, eid, v))
    }

    fn vt_type(&self) -> u32 {
        self.register(Key::Vt, |_| Def::Vt)
    }

    /// The global holding class `sid`'s vtable.
    fn vt_global(&mut self, sid: StructId) -> u32 {
        let k = match self.vt_globals.iter().position(|c| *c == sid) {
            Some(k) => k,
            None => {
                self.vt_globals.push(sid);
                self.vt_globals.len() - 1
            }
        };
        (self.p.globals.len() + FIXED_GLOBALS + k) as u32
    }

    /// The panic's message, line and column (exported for the glue), then the event loop's
    /// next wait (ms, -1: none; decision 268).
    fn panic_global(&self) -> u32 {
        self.p.globals.len() as u32 + 2
    }

    fn wait_global(&self) -> u32 {
        self.p.globals.len() as u32 + 5
    }

    /// A synchronous `__io` job's `r`, `r2` and text (there's one job at a time: ops answer
    /// at once, the job id is always 1).
    fn job_global(&self) -> u32 {
        self.p.globals.len() as u32 + 6
    }

    /// `ArrayBuffer` memory: a table of byte arrays (handle `index + 1 << 32`, so a handle
    /// plus an offset still says both; decision 269).
    fn buffers_global(&self) -> u32 {
        self.p.globals.len() as u32 + 9
    }

    /// The buffer table's type (a growable array of byte arrays).
    fn buffer_table(&self) -> u32 {
        self.wasm_ty(&Ty::Array(Box::new(Ty::Str)))
            .and_then(heap_index)
            .expect("an array")
    }

    /// `pico_buf_op` on the buffer table: 1 allocate `a` bytes (a handle), 2 free `a`, 3 a copy
    /// of `c` bytes of `a` from `b`, 4 move `c` bytes from address `b` to address `a` (a handle
    /// plus an offset), 11–18 read and 21–28 write a little-endian integer at `a + b`.
    fn buf_fn(&mut self) -> R<u32> {
        use ValType::*;
        let table_ty = Ty::Array(Box::new(Ty::Str));
        let grow = self.buffer_table();
        let raw = self.raw_type(str_ref());
        let push = self.array_fn(&table_ty, "push")?;
        let g = self.buffers_global();
        Ok(self.extra_fn(
            "buf op".into(),
            vec![I64, I64, I64, I64],
            vec![I64],
            move |_| {
                // locals: 4 table, 5 bytes, 6 index, 7 position, 8 i, 9 x, 10 copy
                let mut f = Function::new_with_locals_types(vec![
                    ref_to(grow),
                    str_ref(),
                    I32,
                    I32,
                    I32,
                    I64,
                    str_ref(),
                ]);
                let mut s = f.instructions();
                s.global_get(g).ref_is_null();
                s.if_(BlockType::Empty);
                s.array_new_fixed(raw, 0)
                    .i32_const(0)
                    .struct_new(grow)
                    .global_set(g);
                s.end();
                s.global_get(g).local_set(4);
                // A new handle for the bytes in local 10.
                let add = |s: &mut InstructionSink| {
                    s.local_get(4).local_get(10).call(push);
                    s.local_get(4)
                        .struct_get(grow, 1)
                        .i64_extend_i32_u()
                        .i64_const(32)
                        .i64_shl();
                    s.return_();
                };
                s.local_get(0).i64_const(1).i64_eq();
                s.if_(BlockType::Empty);
                s.local_get(1)
                    .i32_wrap_i64()
                    .array_new_default(STR)
                    .local_set(10);
                add(&mut s);
                s.end();
                s.local_get(1)
                    .i64_const(32)
                    .i64_shr_u()
                    .i32_wrap_i64()
                    .i32_const(1)
                    .i32_sub()
                    .local_set(6);
                s.local_get(4)
                    .struct_get(grow, 0)
                    .local_get(6)
                    .array_get(raw)
                    .local_set(5);
                s.local_get(1)
                    .i32_wrap_i64()
                    .local_get(2)
                    .i32_wrap_i64()
                    .i32_add()
                    .local_set(7);
                s.local_get(0).i64_const(2).i64_eq();
                s.if_(BlockType::Empty);
                s.local_get(4)
                    .struct_get(grow, 0)
                    .local_get(6)
                    .ref_null(HeapType::Concrete(STR))
                    .array_set(raw);
                s.i64_const(0).return_();
                s.end();
                // 4: move `c` bytes from address `b` to address `a` (local 5 is `a`'s bytes);
                // `array.copy` handles overlapping ranges.
                s.local_get(0).i64_const(4).i64_eq();
                s.if_(BlockType::Empty);
                s.local_get(5).local_get(1).i32_wrap_i64();
                s.local_get(4)
                    .struct_get(grow, 0)
                    .local_get(2)
                    .i64_const(32)
                    .i64_shr_u()
                    .i32_wrap_i64()
                    .i32_const(1)
                    .i32_sub()
                    .array_get(raw);
                s.local_get(2).i32_wrap_i64();
                s.local_get(3).i32_wrap_i64();
                s.array_copy(STR, STR);
                s.i64_const(0).return_();
                s.end();
                s.local_get(0).i64_const(3).i64_eq();
                s.if_(BlockType::Empty);
                s.local_get(3)
                    .i32_wrap_i64()
                    .array_new_default(STR)
                    .local_set(10);
                s.local_get(10)
                    .i32_const(0)
                    .local_get(5)
                    .local_get(7)
                    .local_get(3)
                    .i32_wrap_i64();
                s.array_copy(STR, STR);
                add(&mut s);
                s.end();
                // Reads: bytes k-1 … 0 (k = op - 10).
                s.local_get(0)
                    .i64_const(11)
                    .i64_ge_s()
                    .local_get(0)
                    .i64_const(18)
                    .i64_le_s()
                    .i32_and();
                s.if_(BlockType::Empty);
                s.local_get(0)
                    .i32_wrap_i64()
                    .i32_const(11)
                    .i32_sub()
                    .local_set(8);
                s.block(BlockType::Empty).loop_(BlockType::Empty);
                s.local_get(8).i32_const(0).i32_lt_s().br_if(1);
                s.local_get(9).i64_const(8).i64_shl();
                s.local_get(5)
                    .local_get(7)
                    .local_get(8)
                    .i32_add()
                    .array_get_u(STR)
                    .i64_extend_i32_u();
                s.i64_or().local_set(9);
                s.local_get(8).i32_const(1).i32_sub().local_set(8);
                s.br(0).end().end();
                s.local_get(9).return_();
                s.end();
                // Writes: bytes 0 … k-1 (k = op - 20).
                s.local_get(0)
                    .i32_wrap_i64()
                    .i32_const(20)
                    .i32_sub()
                    .local_set(8);
                s.block(BlockType::Empty).loop_(BlockType::Empty);
                s.local_get(8).i32_eqz().br_if(1);
                s.local_get(8).i32_const(1).i32_sub().local_set(8);
                s.local_get(5).local_get(7).local_get(8).i32_add();
                s.local_get(3)
                    .local_get(8)
                    .i64_extend_i32_u()
                    .i64_const(8)
                    .i64_mul()
                    .i64_shr_u();
                s.i32_wrap_i64().array_set(STR);
                s.br(0).end().end();
                s.i64_const(0).end();
                f
            },
        ))
    }

    /// A synchronous `__io` job: its results go to the job globals.
    fn io_job(
        &mut self,
        fx: &mut FnCx,
        s: &mut InstructionSink,
        op: i128,
        args: &[Operand],
    ) -> R<()> {
        let j = self.job_global();
        let set_text = |g: &mut Self, s: &mut InstructionSink, text: &str| {
            let d = g.string_data(text);
            s.i32_const(0)
                .i32_const(text.len() as i32)
                .array_new_data(STR, d);
            s.global_set(j + 2);
        };
        match op {
            // TextEncoder: the string's bytes in a new buffer (r, r2: its handle and length).
            130 => {
                let buf = self.buf_fn()?;
                let bytes = self.buf_bytes_fn();
                let str_tmp = fx.tmp(4_000_000, str_ref());
                self.operand(fx, s, &args[4])?;
                s.local_tee(str_tmp)
                    .array_len()
                    .i64_extend_i32_u()
                    .global_set(j + 1);
                s.i64_const(1)
                    .global_get(j + 1)
                    .i64_const(0)
                    .i64_const(0)
                    .call(buf)
                    .global_set(j);
                s.global_get(j)
                    .call(bytes)
                    .i32_const(0)
                    .local_get(str_tmp)
                    .i32_const(0);
                s.global_get(j + 1).i32_wrap_i64().array_copy(STR, STR);
            }
            // TextDecoder: `c` bytes at `b` as text (`a`: fatal), through the glue.
            131 => {
                let bytes = self.buf_bytes_fn();
                let text = fx.tmp(4_000_001, str_ref());
                self.operand(fx, s, &args[2])?;
                s.call(bytes);
                self.operand(fx, s, &args[2])?;
                s.i32_wrap_i64();
                self.operand(fx, s, &args[3])?;
                s.i32_wrap_i64();
                self.operand(fx, s, &args[1])?;
                s.i32_wrap_i64();
                s.call(import_index("decode_utf8"))
                    .local_tee(text)
                    .ref_is_null();
                s.if_(BlockType::Empty);
                s.i64_const(-7).global_set(j);
                s.call(import_index("decode_bad_at")).global_set(j + 1);
                set_text(self, s, "");
                s.else_();
                s.i64_const(0)
                    .global_set(j)
                    .local_get(text)
                    .global_set(j + 2);
                s.end();
            }
            // The bundled tz database's zone `s` (r: a buffer, or -1; r2: its size).
            136 => {
                let f = self.tz_fn()?;
                self.operand(fx, s, &args[4])?;
                s.call(f).global_set(j);
            }
            // `a` random bytes in a new buffer.
            132 => {
                let buf = self.buf_fn()?;
                let bytes = self.buf_bytes_fn();
                s.i64_const(1);
                self.operand(fx, s, &args[1])?;
                s.i64_const(0).i64_const(0).call(buf).global_set(j);
                s.global_get(j)
                    .call(bytes)
                    .call(import_index("fill_random"));
            }
            // The working directory: the root (there's no file system).
            105 => set_text(self, s, "/"),
            // No environment variables, files, links or local zone here.
            114 | 135 | 137 => {
                s.i64_const(0).global_set(j);
                set_text(self, s, "");
            }
            _ => {
                s.i64_const(-1).global_set(j);
                s.i64_const(0).global_set(j + 1);
            }
        }
        Ok(())
    }

    /// Looks a zone up in the bundled tz database (`pico_tzdata.c`'s format): a buffer handle
    /// with its TZif bytes (the size in `r2`), or -1.
    fn tz_fn(&mut self) -> R<u32> {
        use ValType::*;
        let blob = self.tzdata.clone();
        let d = self
            .data
            .iter()
            .position(|x| *x == blob)
            .unwrap_or_else(|| {
                self.data.push(blob.clone());
                self.data.len() - 1
            }) as u32;
        let size = blob.len() as i32;
        let buf = self.buf_fn()?;
        let bytes = self.buf_bytes_fn();
        let j = self.job_global();
        let u = self.extra_fn("u32le".into(), vec![str_ref(), I32], vec![I32], |_| {
            let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
            let mut s = f.instructions();
            for k in 0..4 {
                s.local_get(0)
                    .local_get(1)
                    .i32_const(k)
                    .i32_add()
                    .array_get_u(STR);
                if k > 0 {
                    s.i32_const(8 * k).i32_shl().i32_or();
                }
            }
            s.end();
            f
        });
        Ok(
            self.extra_fn("tz bundled".into(), vec![str_ref()], vec![I64], move |_| {
                // locals: 1 blob, 2 count, 3 entry, 4 name length here, 5 i, 6 k, 7 n, 8 size
                let mut f = Function::new_with_locals_types(vec![
                    str_ref(),
                    I32,
                    I32,
                    I32,
                    I32,
                    I32,
                    I32,
                    I32,
                ]);
                let mut s = f.instructions();
                s.i32_const(0)
                    .i32_const(size)
                    .array_new_data(STR, d)
                    .local_set(1);
                s.local_get(1).array_len().i32_const(12).i32_lt_u();
                s.if_(BlockType::Empty).i64_const(-1).return_().end();
                s.local_get(1).i32_const(8).call(u).local_set(2);
                s.i32_const(12).local_set(3);
                s.local_get(0).array_len().local_set(7);
                s.block(BlockType::Empty).loop_(BlockType::Empty);
                s.local_get(5).local_get(2).i32_ge_u().br_if(1);
                // nl = u16 at e
                s.local_get(1).local_get(3).array_get_u(STR);
                s.local_get(1)
                    .local_get(3)
                    .i32_const(1)
                    .i32_add()
                    .array_get_u(STR)
                    .i32_const(8)
                    .i32_shl()
                    .i32_or();
                s.local_set(4);
                s.local_get(4).local_get(7).i32_eq();
                s.if_(BlockType::Empty);
                s.i32_const(0).local_set(6);
                s.block(BlockType::Empty).loop_(BlockType::Empty);
                s.local_get(6).local_get(7).i32_ge_u().br_if(1);
                s.local_get(1)
                    .local_get(3)
                    .i32_const(2)
                    .i32_add()
                    .local_get(6)
                    .i32_add()
                    .array_get_u(STR);
                s.local_get(0)
                    .local_get(6)
                    .array_get_u(STR)
                    .i32_ne()
                    .br_if(1);
                s.local_get(6).i32_const(1).i32_add().local_set(6);
                s.br(0).end().end();
                // All bytes equal: copy the zone into a new buffer.
                s.local_get(6).local_get(7).i32_eq();
                s.if_(BlockType::Empty);
                s.local_get(1)
                    .local_get(3)
                    .i32_const(6)
                    .i32_add()
                    .local_get(4)
                    .i32_add()
                    .call(u)
                    .local_set(8);
                s.local_get(8).i64_extend_i32_u().global_set(j + 1);
                s.i64_const(1)
                    .local_get(8)
                    .i64_extend_i32_u()
                    .i64_const(0)
                    .i64_const(0)
                    .call(buf);
                s.global_set(j);
                s.global_get(j).call(bytes).i32_const(0).local_get(1);
                s.local_get(1)
                    .local_get(3)
                    .i32_const(2)
                    .i32_add()
                    .local_get(4)
                    .i32_add()
                    .call(u);
                s.local_get(8).array_copy(STR, STR);
                s.global_get(j).return_();
                s.end();
                s.end();
                s.local_get(3)
                    .i32_const(10)
                    .i32_add()
                    .local_get(4)
                    .i32_add()
                    .local_set(3);
                s.local_get(5).i32_const(1).i32_add().local_set(5);
                s.br(0).end().end();
                s.i64_const(-1).end();
                f
            }),
        )
    }

    /// The byte array of a buffer handle (an offset in its low bits is ignored).
    fn buf_bytes_fn(&mut self) -> u32 {
        let grow = self.buffer_table();
        let raw = self.raw_type(str_ref());
        let g = self.buffers_global();
        self.extra_fn(
            "buf bytes".into(),
            vec![ValType::I64],
            vec![str_ref()],
            move |_| {
                let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
                let mut s = f.instructions();
                s.global_get(g).struct_get(grow, 0);
                s.local_get(0)
                    .i64_const(32)
                    .i64_shr_u()
                    .i32_wrap_i64()
                    .i32_const(1)
                    .i32_sub();
                s.array_get(raw).end();
                f
            },
        )
    }

    /// The pending exception (`anyref`, null when none) and its tag (`i32`): globals right
    /// after the program's (decision 267).
    fn exc_global(&self) -> u32 {
        self.p.globals.len() as u32
    }

    /// A function's wasm signature (a closure target takes its environment as `anyref`).
    fn fn_sig(&self, id: FnId) -> R<(Vec<ValType>, Vec<ValType>)> {
        let f = &self.p.fns[id.0 as usize];
        let (mut ps, rs) = self.signature(f)?;
        if self.closure_targets.contains(&id) {
            if f.params.first().is_some_and(|p| !no_value(f.local_ty(*p))) {
                ps[0] = ANYREF;
            } else {
                ps.insert(0, ANYREF);
            }
        }
        Ok((ps, rs))
    }

    /// The wasm definition of a registered type.
    fn composite(&self, def: &Def) -> CompositeInnerType {
        let field = |v: ValType| FieldType {
            element_type: StorageType::Val(v),
            mutable: true,
        };
        let structure =
            |fs: Vec<FieldType>| CompositeInnerType::Struct(StructType { fields: fs.into() });
        match def {
            Def::Struct {
                fields,
                class,
                erased,
                ..
            } => {
                let mut fs = Vec::new();
                if *class {
                    fs.push(field(ref_to(self.vt_type())));
                }
                // (A `void` field is an `anyref` placeholder, so indices stay MIR's.)
                fs.extend(fields.iter().zip(erased).map(|(t, e)| {
                    field(if *e {
                        ANYREF
                    } else {
                        self.wasm_ty(t).unwrap_or(ANYREF)
                    })
                }));
                structure(fs)
            }
            Def::Raw(e) => CompositeInnerType::Array(wasm_encoder::ArrayType(field(*e))),
            Def::Grow(raw) => structure(vec![field(ref_to(*raw)), field(ValType::I32)]),
            Def::Opt(v) => structure(vec![field(*v)]),
            Def::Func(ps, rs) => {
                CompositeInnerType::Func(wasm_encoder::FuncType::new(ps.clone(), rs.clone()))
            }
            Def::Enum => structure(vec![field(ValType::I32)]),
            Def::Variant(_, eid, v) => {
                let mut fs = vec![field(ValType::I32)];
                for (_, t) in &self.p.enum_layout(*eid).variants[*v as usize].fields {
                    fs.push(field(self.wasm_ty(t).unwrap_or(ANYREF)));
                }
                structure(fs)
            }
            Def::Clo(sig) => structure(vec![field(ref_to(*sig)), field(ANYREF)]),
            Def::Iface => structure(vec![field(ANYREF), field(ValType::I32)]),
            Def::Vt => CompositeInnerType::Array(wasm_encoder::ArrayType(field(ValType::Ref(
                RefType::FUNCREF,
            )))),
        }
    }

    /// The function `start` exports: fills the vtables, then runs the entry.
    fn start_fn(&mut self) -> R<u32> {
        let vt = self.vt_type();
        let entry = self.fn_index[&self.p.entry];
        // Each class's slots, each a function of its slot's root signature (a thunk where the
        // class's method differs: its receiver is the class itself).
        let mut tables: Vec<(u32, Vec<Option<u32>>)> = Vec::new();
        let mut k = 0;
        while k < self.vt_globals.len() {
            let sid = self.vt_globals[k];
            let g = (self.p.globals.len() + FIXED_GLOBALS + k) as u32;
            let slots = self.p.layout(sid).meta.vtable.clone();
            let mut entries = Vec::new();
            for (slot, f) in slots.iter().enumerate() {
                entries.push(self.slot_fn(sid, slot as u32, *f)?);
            }
            tables.push((g, entries));
            k += 1;
        }
        for (_, entries) in &tables {
            for f in entries.iter().flatten() {
                if !self.refd.contains(f) {
                    self.refd.push(*f);
                }
            }
        }
        Ok(self.extra_fn("start".into(), vec![], vec![], move |_| {
            let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
            let mut s = f.instructions();
            for (g, entries) in &tables {
                for e in entries {
                    match e {
                        Some(i) => s.ref_func(*i),
                        None => s.ref_null(HeapType::FUNC),
                    };
                }
                s.array_new_fixed(vt, entries.len() as u32).global_set(*g);
            }
            s.call(entry).end();
            f
        }))
    }

    /// `resume` (runs the core library's event loop again) and `pending` (the loop's next wait
    /// in ms, or -1, which it resets).
    fn loop_fns(&mut self) -> (u32, u32) {
        let run = self
            .p
            .fns
            .iter()
            .position(|f| f.library && f.name == "__runEventLoop")
            .and_then(|i| self.fn_index.get(&FnId(i as u32)).copied());
        let resume = self.extra_fn("resume".into(), vec![], vec![], move |_| {
            let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
            let mut s = f.instructions();
            if let Some(r) = run {
                s.call(r);
            }
            s.end();
            f
        });
        let w = self.wait_global();
        let pending = self.extra_fn("pending".into(), vec![], vec![ValType::I64], move |_| {
            let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
            let mut s = f.instructions();
            s.global_get(w).i64_const(-1).global_set(w).end();
            f
        });
        (resume, pending)
    }

    /// The root of a vtable slot: the topmost class whose vtable has it.
    fn slot_root(&self, sid: StructId, slot: u32) -> FnId {
        let mut c = sid;
        while let Some(b) = self.p.layout(c).meta.base
            && self.p.layout(b).meta.vtable.len() > slot as usize
        {
            c = b;
        }
        self.p.layout(c).meta.vtable[slot as usize]
    }

    /// What class `sid`'s vtable holds in `slot`: method `f`, or a thunk with the slot root's
    /// signature (`None`: a method this program never runs).
    fn slot_fn(&mut self, sid: StructId, slot: u32, f: FnId) -> R<Option<u32>> {
        let Some(&idx) = self.fn_index.get(&f) else {
            return Ok(None);
        };
        let root = self.slot_root(sid, slot);
        let (rps, rrs) = self.fn_sig(root)?;
        let (ps, _) = self.fn_sig(f)?;
        if ps == rps {
            return Ok(Some(idx));
        }
        let recv = ps[0];
        let n = rps.len() as u32;
        Ok(Some(self.extra_fn(
            format!("thunk {}/{slot}", f.0),
            rps,
            rrs,
            move |_| {
                let mut fun = Function::new_with_locals_types(Vec::<ValType>::new());
                let mut s = fun.instructions();
                for i in 0..n {
                    s.local_get(i);
                    if i == 0
                        && let ValType::Ref(r) = recv
                    {
                        s.ref_cast_nullable(r.heap_type);
                    }
                }
                s.call(idx).end();
                fun
            },
        )))
    }

    /// A pico type's wasm type (`None`: no value, or unsupported). Registers the GC types it
    /// needs.
    fn wasm_ty(&self, t: &Ty) -> Option<ValType> {
        Some(match t.erase() {
            Ty::Int(it) => {
                if wide(it) {
                    ValType::I64
                } else {
                    ValType::I32
                }
            }
            Ty::Bool | Ty::Char => ValType::I32,
            Ty::Float(FloatTy::F32) => ValType::F32,
            Ty::Float(_) => ValType::F64,
            Ty::Str => str_ref(),
            // A class value is typed as its hierarchy's root: any subclass fits (field access
            // casts to the class itself).
            t @ Ty::Class(..) => ref_to(self.struct_type(&self.root_class(&t))),
            t @ (Ty::Struct(..) | Ty::Tuple(_) | Ty::Range(_)) => ref_to(self.struct_type(&t)),
            Ty::Fixed(elem, _) => {
                let e = self.elem_ty(&elem)?;
                ref_to(self.raw_type(e))
            }
            t @ Ty::Array(_) => {
                let Ty::Array(elem) = &t else { unreachable!() };
                let e = self.elem_ty(elem)?;
                let raw = self.raw_type(e);
                ref_to(self.register(Key::Ty(t.clone()), |_| Def::Grow(raw)))
            }
            // A reference to a value aggregate is the aggregate; to anything else, a cell.
            Ty::Ref(inner) if Self::is_value_aggregate(&inner) => self.wasm_ty(&inner)?,
            Ty::Ref(inner) => {
                let v = self.wasm_ty(&inner)?;
                ref_to(self.register(Key::Opt(v), |_| Def::Opt(v)))
            }
            Ty::Enum(eid, _) => ref_to(self.enum_type(eid)),
            Ty::Iface(..) => ref_to(self.register(Key::Iface, |_| Def::Iface)),
            Ty::Fn(ps, r, _) => {
                let sig = self.closure_sig(&ps, &r)?;
                // (One per wasm signature: function values of covariant types share it.)
                ref_to(self.register(Key::Clo(sig), |_| Def::Clo(sig)))
            }
            Ty::Nullable(inner) => {
                if is_ref_like(&inner) {
                    self.wasm_ty(&inner)?
                } else {
                    let v = self.wasm_ty(&inner)?;
                    ref_to(self.register(Key::Opt(v), |_| Def::Opt(v)))
                }
            }
            _ => return None,
        })
    }

    /// The box type of a `T?` of a value type.
    fn opt_type(&self, inner: &Ty) -> R<u32> {
        if no_value(inner) {
            return unsupported(format!("`{inner}?`"));
        }
        let v = self.ty_or_err(inner)?.expect("a value");
        Ok(self.register(Key::Opt(v), |_| Def::Opt(v)))
    }

    fn ty_or_err(&self, t: &Ty) -> R<Option<ValType>> {
        if no_value(t) {
            return Ok(None);
        }
        match self.wasm_ty(t) {
            Some(v) => Ok(Some(v)),
            None => unsupported(format!("values of type `{t}`")),
        }
    }

    fn signature(&self, f: &mir::Function) -> R<(Vec<ValType>, Vec<ValType>)> {
        let mut ps = Vec::new();
        for l in &f.params {
            if let Some(t) = self.ty_or_err(f.local_ty(*l))? {
                ps.push(t);
            }
        }
        let rs = self.ty_or_err(&f.ret)?.into_iter().collect();
        Ok((ps, rs))
    }

    /// A struct, class, tuple or range type's index and its fields' types (through a
    /// reference or a `T?`).
    fn struct_of(&self, t: &Ty) -> R<(u32, Vec<Ty>)> {
        let t = match t.erase() {
            Ty::Ref(inner) => *inner,
            Ty::Nullable(inner) => *inner,
            t => t,
        };
        if !matches!(
            t,
            Ty::Struct(..) | Ty::Class(..) | Ty::Tuple(_) | Ty::Range(_)
        ) {
            return unsupported(format!("fields of `{t}`"));
        }
        let i = self.struct_type(&t);
        // (This type's own field types: instances of a generic class may share the struct.)
        if let Ty::Struct(sid, _, args) | Ty::Class(sid, _, args) = &t {
            let owner = Ty::type_owner(*sid);
            return Ok((
                i,
                self.p
                    .layout(*sid)
                    .fields
                    .iter()
                    .map(|(_, ft)| ft.subst(owner, args))
                    .collect(),
            ));
        }
        let r = self.reg.borrow();
        match &r.defs[i as usize - 1] {
            Def::Struct { fields, .. } => Ok((i, fields.clone())),
            _ => unreachable!(),
        }
    }

    /// The runtime's hash functions (`pico_hash_u64`, `pico_hash_str`, `pico_hash_combine`).
    fn hash_fn(&mut self, which: &str) -> u32 {
        use ValType::*;
        match which {
            "u64" => self.extra_fn("hash u64".into(), vec![I64], vec![I64], |_| {
                let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
                let mut s = f.instructions();
                s.local_get(0)
                    .i64_const(0x9e3779b97f4a7c15u64 as i64)
                    .i64_add()
                    .local_set(0);
                s.local_get(0)
                    .local_get(0)
                    .i64_const(30)
                    .i64_shr_u()
                    .i64_xor();
                s.i64_const(0xbf58476d1ce4e5b9u64 as i64)
                    .i64_mul()
                    .local_set(0);
                s.local_get(0)
                    .local_get(0)
                    .i64_const(27)
                    .i64_shr_u()
                    .i64_xor();
                s.i64_const(0x94d049bb133111ebu64 as i64)
                    .i64_mul()
                    .local_set(0);
                s.local_get(0)
                    .local_get(0)
                    .i64_const(31)
                    .i64_shr_u()
                    .i64_xor()
                    .end();
                f
            }),
            "str" => self.extra_fn("hash str".into(), vec![str_ref()], vec![I64], |_| {
                // FNV-1a over the bytes. locals: 1 h, 2 i, 3 n
                let mut f = Function::new_with_locals_types(vec![I64, I32, I32]);
                let mut s = f.instructions();
                s.i64_const(0xcbf29ce484222325u64 as i64).local_set(1);
                s.local_get(0).array_len().local_set(3);
                s.block(BlockType::Empty).loop_(BlockType::Empty);
                s.local_get(2).local_get(3).i32_ge_u().br_if(1);
                s.local_get(1)
                    .local_get(0)
                    .local_get(2)
                    .array_get_u(STR)
                    .i64_extend_i32_u()
                    .i64_xor();
                s.i64_const(0x100000001b3).i64_mul().local_set(1);
                s.local_get(2).i32_const(1).i32_add().local_set(2);
                s.br(0).end().end();
                s.local_get(1).end();
                f
            }),
            _ => self.extra_fn("hash combine".into(), vec![I64, I64], vec![I64], |_| {
                // a ^ (b + 0x9e3779b97f4a7c15 + (a << 6) + (a >> 2))
                let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
                let mut s = f.instructions();
                s.local_get(0);
                s.local_get(1)
                    .i64_const(0x9e3779b97f4a7c15u64 as i64)
                    .i64_add();
                s.local_get(0).i64_const(6).i64_shl().i64_add();
                s.local_get(0).i64_const(2).i64_shr_u().i64_add();
                s.i64_xor().end();
                f
            }),
        }
    }

    /// A growable array's operation (`push`, `pop`, `slice`) for array type `at`.
    fn array_fn(&mut self, at: &Ty, op: &str) -> R<u32> {
        let Ty::Array(elem) = at.erase() else {
            return unsupported(format!("`{op}` on `{at}`"));
        };
        let e = self.elem_storage(&elem)?;
        let raw = self.raw_type(e);
        let gv = self.wasm_ty(at).expect("an array");
        let grow = heap_index(gv).expect("a ref");
        let key = format!("array {op} {at}");
        use ValType::*;
        Ok(match op {
            "push" => self.extra_fn(key, vec![gv, e], vec![], |_| {
                // locals: 2 data, 3 len, 4 the new storage
                let mut f = Function::new_with_locals_types(vec![ref_to(raw), I32, ref_to(raw)]);
                let mut s = f.instructions();
                s.local_get(0).struct_get(grow, 0).local_set(2);
                s.local_get(0).struct_get(grow, 1).local_set(3);
                // Full (or no storage yet): twice the room.
                s.local_get(2).ref_is_null();
                s.if_(BlockType::Result(I32)).i32_const(1).else_();
                s.local_get(3).local_get(2).array_len().i32_ge_u().end();
                s.if_(BlockType::Empty);
                // (`select` takes its first operand when the condition holds: 4 below 2.)
                s.i32_const(4)
                    .local_get(3)
                    .i32_const(1)
                    .i32_shl()
                    .local_get(3)
                    .i32_const(2)
                    .i32_lt_u();
                s.select().array_new_default(raw).local_set(4);
                s.local_get(2).ref_is_null().i32_eqz();
                s.if_(BlockType::Empty);
                s.local_get(4)
                    .i32_const(0)
                    .local_get(2)
                    .i32_const(0)
                    .local_get(3)
                    .array_copy(raw, raw);
                s.end();
                s.local_get(0).local_get(4).struct_set(grow, 0);
                s.local_get(4).local_set(2);
                s.end();
                s.local_get(2).local_get(3).local_get(1).array_set(raw);
                s.local_get(0)
                    .local_get(3)
                    .i32_const(1)
                    .i32_add()
                    .struct_set(grow, 1);
                s.end();
                f
            }),
            "pop" => {
                let boxed = !is_ref_like(&elem);
                let opt = if boxed {
                    Some(self.opt_type(&elem)?)
                } else {
                    None
                };
                let rt = match opt {
                    Some(b) => ref_to(b),
                    None => e,
                };
                self.extra_fn(key, vec![gv], vec![rt], |_| {
                    // locals: 1 len
                    let mut f = Function::new_with_locals_types(vec![I32]);
                    let mut s = f.instructions();
                    s.local_get(0).struct_get(grow, 1).local_tee(1).i32_eqz();
                    s.if_(BlockType::Empty);
                    s.ref_null(HeapType::Concrete(heap_index(rt).expect("a ref")))
                        .return_();
                    s.end();
                    s.local_get(0)
                        .local_get(1)
                        .i32_const(1)
                        .i32_sub()
                        .local_tee(1)
                        .struct_set(grow, 1);
                    s.local_get(0)
                        .struct_get(grow, 0)
                        .local_get(1)
                        .array_get(raw);
                    if let Some(b) = opt {
                        s.struct_new(b);
                    }
                    s.end();
                    f
                })
            }
            _ => self.extra_fn(key, vec![gv, I64, I64], vec![gv], |_| {
                // `[from, to)` clamped to the array. locals: 3 from, 4 to, 5 n, 6 storage
                let mut f = Function::new_with_locals_types(vec![I32, I32, I32, ref_to(raw)]);
                let mut s = f.instructions();
                s.local_get(0).struct_get(grow, 1).local_set(5);
                for (arg, local) in [(1u32, 3u32), (2, 4)] {
                    // Clamped to [0, n].
                    s.local_get(arg).i64_const(0).i64_lt_s();
                    s.if_(BlockType::Empty).i64_const(0).local_set(arg).end();
                    s.local_get(arg).local_get(5).i64_extend_i32_u().i64_gt_s();
                    s.if_(BlockType::Empty)
                        .local_get(5)
                        .i64_extend_i32_u()
                        .local_set(arg)
                        .end();
                    s.local_get(arg).i32_wrap_i64().local_set(local);
                }
                s.local_get(4)
                    .local_get(3)
                    .i32_sub()
                    .i32_const(0)
                    .local_get(4)
                    .local_get(3)
                    .i32_gt_s()
                    .select()
                    .local_set(5);
                s.local_get(5).array_new_default(raw).local_set(6);
                s.local_get(6)
                    .i32_const(0)
                    .local_get(0)
                    .struct_get(grow, 0)
                    .local_get(3)
                    .local_get(5)
                    .array_copy(raw, raw);
                s.local_get(6).local_get(5).struct_new(grow).end();
                f
            }),
        })
    }

    /// A string helper (`char_at`: the code point at a code-point index, or -1; `slice`:
    /// JavaScript's `slice` on code points).
    fn str_fn(&mut self, op: &str) -> u32 {
        use ValType::*;
        let sr = str_ref();
        // The byte offset of code point `c` (the length past the end).
        let off = self.extra_fn("str off".into(), vec![sr, I32], vec![I32], |_| {
            // locals: 2 i, 3 count, 4 len
            let mut f = Function::new_with_locals_types(vec![I32, I32, I32]);
            let mut s = f.instructions();
            s.local_get(0).array_len().local_set(4);
            s.block(BlockType::Empty).loop_(BlockType::Empty);
            s.local_get(2).local_get(4).i32_ge_u().br_if(1);
            s.local_get(0)
                .local_get(2)
                .array_get_u(STR)
                .i32_const(0xc0)
                .i32_and()
                .i32_const(0x80)
                .i32_ne();
            s.if_(BlockType::Empty);
            s.local_get(3)
                .local_get(1)
                .i32_eq()
                .if_(BlockType::Empty)
                .local_get(2)
                .return_()
                .end();
            s.local_get(3).i32_const(1).i32_add().local_set(3);
            s.end();
            s.local_get(2).i32_const(1).i32_add().local_set(2);
            s.br(0).end().end();
            s.local_get(4).end();
            f
        });
        let chars = helper_index("str_chars");
        match op {
            "char_at" => self.extra_fn("str char_at".into(), vec![sr, I64], vec![I32], |_| {
                // locals: 2 offset, 3 the first byte, 4 code point
                let mut f = Function::new_with_locals_types(vec![I32, I32, I32]);
                let mut s = f.instructions();
                s.local_get(1).i64_const(0).i64_lt_s();
                s.local_get(1).local_get(0).call(chars).i64_ge_s().i32_or();
                s.if_(BlockType::Empty).i32_const(-1).return_().end();
                s.local_get(0)
                    .local_get(1)
                    .i32_wrap_i64()
                    .call(off)
                    .local_set(2);
                s.local_get(0).local_get(2).array_get_u(STR).local_tee(3);
                s.i32_const(0x80)
                    .i32_lt_u()
                    .if_(BlockType::Empty)
                    .local_get(3)
                    .return_()
                    .end();
                // Continuation bytes: 1 (0xC0..), 2 (0xE0..) or 3 (0xF0..).
                let cont = |s: &mut InstructionSink, k: i32| {
                    s.local_get(4).i32_const(6).i32_shl();
                    s.local_get(0)
                        .local_get(2)
                        .i32_const(k)
                        .i32_add()
                        .array_get_u(STR);
                    s.i32_const(0x3f).i32_and().i32_or().local_set(4);
                };
                s.local_get(3).i32_const(0xe0).i32_lt_u();
                s.if_(BlockType::Empty);
                s.local_get(3).i32_const(0x1f).i32_and().local_set(4);
                cont(&mut s, 1);
                s.local_get(4).return_();
                s.end();
                s.local_get(3).i32_const(0xf0).i32_lt_u();
                s.if_(BlockType::Empty);
                s.local_get(3).i32_const(0x0f).i32_and().local_set(4);
                cont(&mut s, 1);
                cont(&mut s, 2);
                s.local_get(4).return_();
                s.end();
                s.local_get(3).i32_const(0x07).i32_and().local_set(4);
                cont(&mut s, 1);
                cont(&mut s, 2);
                cont(&mut s, 3);
                s.local_get(4).end();
                f
            }),
            _ => self.extra_fn("str slice".into(), vec![sr, I64, I64], vec![sr], |_| {
                // locals: 3 n, 4 from (bytes), 5 to (bytes), 6 the copy
                let mut f = Function::new_with_locals_types(vec![I64, I32, I32, sr]);
                let mut s = f.instructions();
                s.local_get(0).call(chars).local_set(3);
                // Each index: negative counts from the end; clamped to [0, n].
                for arg in [1u32, 2] {
                    s.local_get(arg).i64_const(0).i64_lt_s();
                    s.if_(BlockType::Empty)
                        .local_get(arg)
                        .local_get(3)
                        .i64_add()
                        .local_set(arg)
                        .end();
                    s.local_get(arg).i64_const(0).i64_lt_s();
                    s.if_(BlockType::Empty).i64_const(0).local_set(arg).end();
                    s.local_get(arg).local_get(3).i64_gt_s();
                    s.if_(BlockType::Empty).local_get(3).local_set(arg).end();
                }
                s.local_get(2).local_get(1).i64_lt_s();
                s.if_(BlockType::Empty).local_get(1).local_set(2).end();
                s.local_get(0)
                    .local_get(1)
                    .i32_wrap_i64()
                    .call(off)
                    .local_set(4);
                s.local_get(0)
                    .local_get(2)
                    .i32_wrap_i64()
                    .call(off)
                    .local_set(5);
                s.local_get(5)
                    .local_get(4)
                    .i32_sub()
                    .array_new_default(STR)
                    .local_set(6);
                s.local_get(6)
                    .i32_const(0)
                    .local_get(0)
                    .local_get(4)
                    .local_get(5)
                    .local_get(4)
                    .i32_sub();
                s.array_copy(STR, STR);
                s.local_get(6).end();
                f
            }),
        }
    }

    /// `@export("js")` functions (as `js_<name>`: the glue converts numbers and strings,
    /// references pass as they are), and for the structs they take or return, a constructor
    /// (`js_new_<id>`, the fields in order) and field getters (`js_get_<id>_<field>`).
    fn js_exports(&mut self) -> R<Vec<(String, u32)>> {
        let mut out = Vec::new();
        let mut structs: Vec<Ty> = Vec::new();
        fn visit(g: &Gen, t: &Ty, structs: &mut Vec<Ty>) {
            if let Ty::Struct(sid, _, args) = t.erase()
                && !structs.contains(&t.erase())
            {
                structs.push(t.erase());
                let owner = Ty::type_owner(sid);
                for (_, ft) in &g.p.layout(sid).fields {
                    visit(g, &ft.subst(owner, &args), structs);
                }
            }
        }
        for e in &self.p.exports {
            if e.abi != pico_sema::hir::ExportAbi::Js {
                continue;
            }
            let Some(&f) = self.fn_index.get(&e.func) else {
                continue;
            };
            out.push((format!("js_{}", e.name), f));
            for (_, t) in &e.params {
                visit(self, t, &mut structs);
            }
            visit(self, &e.ret, &mut structs);
        }
        for t in structs {
            let Ty::Struct(sid, _, _) = &t else {
                unreachable!()
            };
            let (idx, fields) = self.struct_of(&t)?;
            let sty = ref_to(idx);
            let ftys: Vec<Option<ValType>> = fields
                .iter()
                .map(|f| if no_value(f) { None } else { self.wasm_ty(f) })
                .collect();
            let params: Vec<ValType> = ftys.iter().flatten().copied().collect();
            let make = self.extra_fn(format!("js new {t}"), params, vec![sty], |_| {
                let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
                let mut s = f.instructions();
                let mut k = 0;
                for ft in &ftys {
                    if ft.is_some() {
                        s.local_get(k);
                        k += 1;
                    } else {
                        s.ref_null(RefType::ANYREF.heap_type);
                    }
                }
                s.struct_new(idx).end();
                f
            });
            out.push((format!("js_new_{}", sid.0), make));
            for (i, ft) in ftys.iter().enumerate() {
                let Some(ft) = *ft else { continue };
                let get = self.extra_fn(format!("js get {t} {i}"), vec![sty], vec![ft], |_| {
                    let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
                    let mut s = f.instructions();
                    s.local_get(0).struct_get(idx, i as u32).end();
                    f
                });
                out.push((format!("js_get_{}_{i}", sid.0), get));
            }
        }
        Ok(out)
    }

    /// A helper function made once per `key`: its index (built by `build` the first time).
    fn extra_fn(
        &mut self,
        key: String,
        params: Vec<ValType>,
        results: Vec<ValType>,
        build: impl FnOnce(&mut Self) -> Function,
    ) -> u32 {
        if let Some(&i) = self.extra_keys.get(&key) {
            return i;
        }
        let k = self.extra.len();
        let i = self.first_extra + k as u32;
        self.extra_keys.insert(key, i);
        self.extra.push((params, results, None));
        let f = build(self);
        self.extra[k].2 = Some(f);
        i
    }

    /// A string constant's data segment.
    fn string_data(&mut self, s: &str) -> u32 {
        let bytes = s.as_bytes().to_vec();
        if let Some(i) = self.data.iter().position(|d| *d == bytes) {
            return i as u32;
        }
        self.data.push(bytes);
        self.data.len() as u32 - 1
    }

    // ------------------------------------------------------------ functions

    fn function(&mut self, id: FnId, f: &mir::Function) -> R<Function> {
        let mut locals = Vec::new();
        let mut n = 0u32;
        let mut extra = Vec::new();
        // Parameters first, then the other locals.
        let is_param: Vec<bool> = (0..f.locals.len())
            .map(|i| f.params.iter().any(|p| p.0 as usize == i))
            .collect();
        let mut slots: Vec<Option<Slot>> = vec![None; f.locals.len()];
        // A closure target gets its environment as `anyref` (wasm parameter 0), cast into a
        // local of its real type first thing.
        let closure = self.closure_targets.contains(&id);
        // Locals taking a pending exception hold `anyref` (it may be a boxed throwable).
        let taken: std::collections::HashSet<u32> = f
            .blocks
            .iter()
            .flat_map(|b| &b.stmts)
            .filter(|st| matches!(st.rvalue, Rvalue::TakeException(_)) && st.place.proj.is_empty())
            .filter_map(|st| match st.place.base {
                mir::Base::Local(l) => Some(l.0),
                _ => None,
            })
            .collect();
        let mut local_types: Vec<Option<ValType>> = Vec::new();
        for (i, l) in f.locals.iter().enumerate() {
            local_types.push(if taken.contains(&(i as u32)) {
                Some(ANYREF)
            } else {
                self.ty_or_err(&l.ty)?
            });
        }
        let mut env_cast: Option<(u32, ValType)> = None;
        let mut params = f.params.iter().peekable();
        if closure {
            n = 1;
            if let Some(p0) = f.params.first()
                && let Some(t) = self.ty_or_err(f.local_ty(*p0))?
            {
                params.next();
                env_cast = Some((p0.0, t));
            }
        }
        for p in params {
            if self.ty_or_err(f.local_ty(*p))?.is_some() {
                slots[p.0 as usize] = Some(Slot { index: n });
                n += 1;
            }
        }
        if let Some((l, t)) = env_cast {
            slots[l as usize] = Some(Slot { index: n });
            extra.push(t);
            n += 1;
        }
        for (i, _) in f.locals.iter().enumerate() {
            if is_param[i] {
                continue;
            }
            if let Some(t) = local_types[i] {
                slots[i] = Some(Slot { index: n });
                extra.push(t);
                n += 1;
            }
        }
        locals.append(&mut slots);
        let pc = n;
        extra.push(ValType::I32);
        n += 1;
        let mut fx = FnCx {
            f,
            locals,
            pc,
            scratch: HashMap::new(),
            tmps: HashMap::new(),
            guards: None,
            local_types,
            nlocals: n,
            extra,
        };
        // The body is built first (it may add scratch locals).
        let mut body = Function::new_with_locals_types(Vec::<ValType>::new());
        let mut code: Vec<u8> = Vec::new();
        {
            let mut sink = InstructionSink::new(&mut code);
            if let Some((l, ValType::Ref(r))) = env_cast {
                sink.local_get(0);
                sink.ref_cast_nullable(r.heap_type);
                sink.local_set(fx.locals[l as usize].expect("a slot").index);
            }
            let nblocks = f.blocks.len() as u32;
            // Panic guards: the dispatch loop runs inside a `try_table`; a caught panic goes
            // to the innermost guard's handler (popped), or on when this frame has none left.
            let guarded = f
                .blocks
                .iter()
                .any(|b| matches!(b.term, Terminator::Guard { .. }));
            if guarded {
                let raw = self.raw_type(ValType::I32);
                let stack = fx.tmp(3_000_000, ref_to(raw));
                let depth = fx.tmp(3_000_001, ValType::I32);
                fx.guards = Some((stack, depth));
                sink.i32_const(64).array_new_default(raw).local_set(stack);
                sink.loop_(BlockType::Empty); // retry
                sink.block(BlockType::Empty); // caught
                sink.try_table(
                    BlockType::Empty,
                    [wasm_encoder::Catch::One { tag: 0, label: 0 }],
                );
            }
            sink.loop_(BlockType::Empty);
            for _ in 0..nblocks {
                sink.block(BlockType::Empty);
            }
            sink.local_get(fx.pc);
            sink.br_table(0..nblocks, 0);
            for (k, b) in f.blocks.iter().enumerate() {
                sink.end();
                // Inside: the blocks after this one, then the loop.
                let to_loop = nblocks - 1 - k as u32;
                for st in &b.stmts {
                    self.statement(&mut fx, &mut sink, st)?;
                }
                self.terminator(&mut fx, &mut sink, &b.term, to_loop)?;
            }
            sink.end(); // the loop
            sink.unreachable();
            if let Some((stack, depth)) = fx.guards {
                sink.end(); // try_table
                sink.unreachable();
                sink.end(); // caught
                sink.local_get(depth).i32_eqz();
                sink.if_(BlockType::Empty).throw(0).end();
                sink.local_get(depth)
                    .i32_const(1)
                    .i32_sub()
                    .local_set(depth);
                sink.local_get(stack).local_get(depth);
                sink.array_get(self.raw_type(ValType::I32)).local_set(fx.pc);
                sink.br(0); // retry
                sink.end();
                sink.unreachable();
            }
            sink.end();
        }
        let _ = &mut body;
        let mut out = Function::new_with_locals_types(fx.extra.clone());
        out.raw(code);
        Ok(out)
    }

    fn statement(&mut self, fx: &mut FnCx, s: &mut InstructionSink, st: &mir::Statement) -> R<()> {
        let t = self.place_ty(fx, &st.place)?;
        if no_value(&t) {
            // (Nothing to store: a `void` value.)
            return match &st.rvalue {
                Rvalue::Use(o) if no_value(&self.operand_ty(fx, o)?) => Ok(()),
                Rvalue::ShowLeave(o) => {
                    self.operand(fx, s, o)?;
                    s.call(import_index("show_leave"));
                    Ok(())
                }
                Rvalue::SetException(e, tag) => {
                    let g = self.exc_global();
                    self.operand(fx, s, e)?;
                    s.global_set(g);
                    self.operand(fx, s, tag)?;
                    s.global_set(g + 1);
                    Ok(())
                }
                Rvalue::ArrayPush(a, v) => {
                    let at = self.operand_ty(fx, a)?;
                    let f = self.array_fn(&at, "push")?;
                    self.operand(fx, s, a)?;
                    let Ty::Array(elem) = at.erase() else {
                        unreachable!()
                    };
                    if no_value(&elem) {
                        s.ref_null(RefType::ANYREF.heap_type);
                    } else {
                        self.operand_to(fx, s, v, &elem)?;
                    }
                    s.call(f);
                    Ok(())
                }
                other => {
                    let name = format!("{other:?}");
                    let name = name.split(['(', ' ']).next().unwrap_or("").to_string();
                    unsupported(format!("`{name}` (in MIR)"))
                }
            };
        }
        // (A local's wasm type may differ from its pico type's: `local_types`.)
        let dest = match (&st.place.base, st.place.proj.is_empty()) {
            (mir::Base::Local(l), true) => fx.local_types[l.0 as usize],
            _ => self.wasm_ty(&t),
        };
        self.store(fx, s, &st.place, |g, fx, s| {
            if let Rvalue::Use(o) = &st.rvalue {
                g.operand(fx, s, o)?;
                let from = match o {
                    Operand::Copy(p)
                        if p.proj.is_empty() && matches!(p.base, mir::Base::Local(_)) =>
                    {
                        let mir::Base::Local(l) = p.base else {
                            unreachable!()
                        };
                        fx.local_types[l.0 as usize]
                    }
                    o => {
                        let ot = g.operand_ty(fx, o)?;
                        g.wasm_ty(&ot)
                    }
                };
                g.coerce(s, from, dest);
                return Ok(());
            }
            g.rvalue(fx, s, &st.rvalue, &t, st.loc).map(|_| ())
        })
    }

    /// The type of a place.
    fn place_ty(&self, fx: &FnCx, p: &Place) -> R<Ty> {
        let mut t = match p.base {
            mir::Base::Local(l) => fx.f.local_ty(l).clone(),
            mir::Base::Global(g) => self.p.globals[g.0 as usize].ty.clone(),
            mir::Base::Deref(l) => match fx.f.local_ty(l) {
                Ty::Ref(inner) => (**inner).clone(),
                t => return unsupported(format!("dereferencing `{t}`")),
            },
        };
        for pr in &p.proj {
            t = self.proj_ty(&t, pr)?;
        }
        Ok(t)
    }

    /// The type one projection leads to.
    fn proj_ty(&self, t: &Ty, pr: &Proj) -> R<Ty> {
        Ok(match (pr, t.erase()) {
            (Proj::Field(i), _) => self.struct_of(t)?.1[*i as usize].clone(),
            (Proj::Index(_), Ty::Array(elem) | Ty::Fixed(elem, _)) => *elem,
            (Proj::Payload, Ty::Nullable(inner)) => *inner,
            (Proj::VariantField(v, i), Ty::Enum(eid, _)) => {
                self.p.enum_layout(eid).variants[*v as usize].fields[*i as usize]
                    .1
                    .clone()
            }
            (pr, t) => return unsupported(format!("the place `{pr:?}` of `{t}`")),
        })
    }

    /// Pushes what is needed under the value of one projection of the (pushed) `t`: the
    /// struct for a field, the storage and the index for an element; returns how to finish a
    /// get or a set.
    fn proj_prefix(&mut self, fx: &FnCx, s: &mut InstructionSink, t: &Ty, pr: &Proj) -> R<Access> {
        Ok(match (pr, t.erase()) {
            // (A class's fields come after its vtable.)
            (Proj::Field(i), inner) => {
                let class = matches!(inner, Ty::Class(..))
                    || matches!(&inner, Ty::Nullable(c) | Ty::Ref(c) if matches!(**c, Ty::Class(..)));
                let idx = self.struct_of(t)?.0;
                if class {
                    // (Typed as the root class: cast to this one.)
                    s.ref_cast_nullable(HeapType::Concrete(idx));
                }
                Access::Field(idx, *i + class as u32)
            }
            (Proj::VariantField(v, i), Ty::Enum(eid, _)) => {
                // A field every variant has (a discriminated union's discriminant) may be read
                // before the variant is known: through a getter dispatching on the tag.
                let layout = self.p.enum_layout(eid);
                let want = layout.variants[*v as usize].fields[*i as usize].1.clone();
                let shared = layout.variants.len() > 1
                    && layout.variants.iter().all(|var| {
                        var.fields
                            .get(*i as usize)
                            .is_some_and(|(_, t)| self.wasm_ty(t) == self.wasm_ty(&want))
                    });
                if shared {
                    Access::Getter(self.shared_field_fn(eid, *i, &want)?)
                } else {
                    let vt = self.variant_type(eid, *v);
                    s.ref_cast_nullable(HeapType::Concrete(vt));
                    Access::Field(vt, *i + 1)
                }
            }
            (Proj::Index(l), Ty::Array(elem)) => {
                let e = self.elem_storage(&elem)?;
                let raw = self.raw_type(e);
                let grow = self.wasm_ty(t).and_then(heap_index).expect("an array");
                s.struct_get(grow, 0);
                self.index_local(fx, s, *l);
                Access::Elem(raw)
            }
            (Proj::Index(l), Ty::Fixed(elem, _)) => {
                let e = self.elem_storage(&elem)?;
                let raw = self.raw_type(e);
                self.index_local(fx, s, *l);
                Access::Elem(raw)
            }
            (Proj::Payload, Ty::Nullable(inner)) => {
                if is_ref_like(&inner) {
                    Access::Same
                } else {
                    Access::Field(self.opt_type(&inner)?, 0)
                }
            }
            (pr, t) => return unsupported(format!("the place `{pr:?}` of `{t}`")),
        })
    }

    /// An index held in a local (`int`) as an `i32`.
    fn index_local(&self, fx: &FnCx, s: &mut InstructionSink, l: mir::Local) {
        if let Some(slot) = fx.locals[l.0 as usize] {
            s.local_get(slot.index);
            if !matches!(fx.f.local_ty(l).erase(), Ty::Int(it) if !wide(it)) {
                s.i32_wrap_i64();
            }
        }
    }

    /// Pushes the base of a place (a local's or global's value; a dereferenced struct
    /// reference is the struct).
    fn base_get(&self, fx: &FnCx, s: &mut InstructionSink, p: &Place) -> R<()> {
        match p.base {
            mir::Base::Local(l) => {
                if let Some(slot) = fx.locals[l.0 as usize] {
                    s.local_get(slot.index);
                }
            }
            mir::Base::Deref(l) => {
                if let Some(slot) = fx.locals[l.0 as usize] {
                    s.local_get(slot.index);
                    // A cell: its value (a value aggregate is reached directly).
                    if let Ty::Ref(inner) = fx.f.local_ty(l).erase()
                        && !Self::is_value_aggregate(&inner)
                    {
                        let cell = self
                            .wasm_ty(&Ty::Ref(inner))
                            .and_then(heap_index)
                            .expect("a cell");
                        s.struct_get(cell, 0);
                    }
                }
            }
            mir::Base::Global(g) => {
                s.global_get(g.0);
            }
        }
        Ok(())
    }

    /// Pushes a place's value (structs are copied: they're values).
    fn load(&mut self, fx: &mut FnCx, s: &mut InstructionSink, p: &Place, copy: bool) -> R<()> {
        let t = self.place_ty(fx, p)?;
        if no_value(&t) {
            return Ok(());
        }
        self.ty_or_err(&t)?;
        self.base_get(fx, s, p)?;
        let mut cur = match p.base {
            mir::Base::Local(l) => fx.f.local_ty(l).clone(),
            mir::Base::Global(g) => self.p.globals[g.0 as usize].ty.clone(),
            mir::Base::Deref(l) => match fx.f.local_ty(l).erase() {
                Ty::Ref(inner) => *inner,
                t => t,
            },
        };
        for pr in &p.proj {
            match self.proj_prefix(fx, s, &cur, pr)? {
                Access::Field(idx, i) => {
                    s.struct_get(idx, i);
                    // An erased field: back to its type (this access's: instances share it).
                    if self.erased_field(idx, i)
                        && let Some(ValType::Ref(r)) = self.wasm_ty(&self.proj_ty(&cur, pr)?)
                    {
                        s.ref_cast_nullable(r.heap_type);
                    }
                }
                Access::Elem(raw) => {
                    s.array_get(raw);
                }
                Access::Same => {}
                Access::Getter(f) => {
                    s.call(f);
                }
            }
            cur = self.proj_ty(&cur, pr)?;
        }
        if copy
            && Self::is_value_aggregate(&t)
            && !(p.proj.is_empty() && matches!(p.base, mir::Base::Deref(_)))
        {
            self.clone_struct(fx, s, &t)?;
        }
        Ok(())
    }

    /// Copies the value aggregate on the stack (a struct, tuple, range or fixed array; what
    /// they hold of those too), so it keeps value semantics.
    fn clone_struct(&mut self, _fx: &mut FnCx, s: &mut InstructionSink, t: &Ty) -> R<()> {
        let f = self.clone_fn(t)?;
        s.call(f);
        Ok(())
    }

    /// Whether values of `t` are copied when read from a place.
    fn is_value_aggregate(t: &Ty) -> bool {
        matches!(
            t.erase(),
            Ty::Struct(..) | Ty::Tuple(_) | Ty::Range(_) | Ty::Fixed(..)
        )
    }

    /// The copy function of a value aggregate type (null stays null).
    fn clone_fn(&mut self, t: &Ty) -> R<u32> {
        let t = t.erase();
        let vt = self.ty_or_err(&t)?.expect("a value");
        let key = format!("clone {t}");
        if let Ty::Fixed(elem, _) = &t {
            let elem = (**elem).clone();
            let e = self.elem_storage(&elem)?;
            let raw = self.raw_type(e);
            let inner = if Self::is_value_aggregate(&elem) {
                Some(self.clone_fn(&elem)?)
            } else {
                None
            };
            return Ok(self.extra_fn(key, vec![vt], vec![vt], |_| {
                // locals: 1 the copy, 2 i, 3 n
                let mut f = Function::new_with_locals_types(vec![vt, ValType::I32, ValType::I32]);
                let mut s = f.instructions();
                s.local_get(0).ref_is_null();
                s.if_(BlockType::Empty)
                    .ref_null(HeapType::Concrete(raw))
                    .return_()
                    .end();
                s.local_get(0)
                    .array_len()
                    .local_tee(3)
                    .array_new_default(raw)
                    .local_set(1);
                match inner {
                    None => {
                        s.local_get(1)
                            .i32_const(0)
                            .local_get(0)
                            .i32_const(0)
                            .local_get(3);
                        s.array_copy(raw, raw);
                    }
                    Some(c) => {
                        s.block(BlockType::Empty).loop_(BlockType::Empty);
                        s.local_get(2).local_get(3).i32_ge_u().br_if(1);
                        s.local_get(1).local_get(2);
                        s.local_get(0).local_get(2).array_get(raw).call(c);
                        s.array_set(raw);
                        s.local_get(2).i32_const(1).i32_add().local_set(2);
                        s.br(0).end().end();
                    }
                }
                s.local_get(1).end();
                f
            }));
        }
        let (idx, fields) = self.struct_of(&t)?;
        let mut inner: Vec<Option<u32>> = Vec::new();
        for ft in &fields {
            inner.push(if Self::is_value_aggregate(ft) {
                Some(self.clone_fn(ft)?)
            } else {
                None
            });
        }
        Ok(self.extra_fn(key, vec![vt], vec![vt], |_| {
            let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
            let mut s = f.instructions();
            s.local_get(0).ref_is_null();
            s.if_(BlockType::Empty)
                .ref_null(HeapType::Concrete(idx))
                .return_()
                .end();
            for (i, c) in inner.iter().enumerate() {
                s.local_get(0).struct_get(idx, i as u32);
                if let Some(c) = c {
                    s.call(*c);
                }
            }
            s.struct_new(idx).end();
            f
        }))
    }

    /// A scratch local holding a reference to struct type `idx` (one per type, per depth of
    /// use; nested copies reuse different types).
    fn ref_scratch(&self, fx: &mut FnCx, idx: u32) -> u32 {
        fx.tmp(1_000_000 + idx, ref_to(idx))
    }

    /// Stores what `value` pushes into a place.
    fn store(
        &mut self,
        fx: &mut FnCx,
        s: &mut InstructionSink,
        p: &Place,
        value: impl FnOnce(&mut Self, &mut FnCx, &mut InstructionSink) -> R<()>,
    ) -> R<()> {
        if p.proj.is_empty() {
            if let mir::Base::Deref(l) = p.base {
                let slot = fx.locals[l.0 as usize].expect("a reference").index;
                let Ty::Ref(inner) = fx.f.local_ty(l).erase() else {
                    return unsupported("dereferencing a non-reference");
                };
                if Self::is_value_aggregate(&inner) {
                    // Into the aggregate it refers to: every part copied over.
                    let f = self.assign_fn(&inner)?;
                    s.local_get(slot);
                    value(self, fx, s)?;
                    s.call(f);
                } else {
                    let cell = self
                        .wasm_ty(&Ty::Ref(inner))
                        .and_then(heap_index)
                        .expect("a cell");
                    s.local_get(slot);
                    value(self, fx, s)?;
                    s.struct_set(cell, 0);
                }
                return Ok(());
            }
            value(self, fx, s)?;
            match p.base {
                mir::Base::Local(l) => {
                    if let Some(slot) = fx.locals[l.0 as usize] {
                        s.local_set(slot.index);
                    }
                }
                mir::Base::Global(g) => {
                    s.global_set(g.0);
                }
                mir::Base::Deref(_) => unreachable!(),
            }
            return Ok(());
        }
        // The struct holding the field, then the value: `struct.set`.
        let parent = Place {
            base: p.base,
            proj: p.proj[..p.proj.len() - 1].to_vec(),
        };
        let parent_ty = self.place_ty(fx, &parent)?;
        let last = &p.proj[p.proj.len() - 1];
        if matches!(last, Proj::Payload)
            && matches!(parent_ty.erase(), Ty::Nullable(ref i) if is_ref_like(i))
        {
            // (The payload of a nullable reference is the reference itself.)
            return self.store(fx, s, &parent, value);
        }
        self.load(fx, s, &parent, false)?;
        match self.proj_prefix(fx, s, &parent_ty, last)? {
            Access::Field(idx, i) => {
                value(self, fx, s)?;
                s.struct_set(idx, i);
            }
            Access::Elem(raw) => {
                value(self, fx, s)?;
                s.array_set(raw);
            }
            Access::Same => unreachable!("handled above"),
            Access::Getter(_) => return unsupported("assigning a union's shared field"),
        }
        Ok(())
    }

    /// Pushes an operand as a value of type `to`: a reference of another wasm type is cast
    /// (a narrowed class, a taken exception, a covariant function value's result).
    fn operand_to(
        &mut self,
        fx: &mut FnCx,
        s: &mut InstructionSink,
        o: &Operand,
        to: &Ty,
    ) -> R<()> {
        // Two instances of a generic class are unrelated struct types here.
        let ot = self.operand_ty(fx, o)?;
        if let (Ty::Class(a, _, aa), Ty::Class(b, _, ba)) = (ot.erase(), to.erase())
            && a == b
            && aa != ba
            && self.wasm_ty(&ot) != self.wasm_ty(to)
        {
            return unsupported(format!(
                "`{ot}` where `{to}` is expected (generic class covariance)"
            ));
        }
        // (Function values whose types differ in a generic class instance: distinct structs.)
        if matches!((ot.erase(), to.erase()), (Ty::Fn(..), Ty::Fn(..)))
            && self.wasm_ty(&ot) != self.wasm_ty(to)
        {
            return unsupported(format!(
                "`{ot}` where `{to}` is expected (function value covariance)"
            ));
        }
        self.operand(fx, s, o)?;
        let from = match o {
            Operand::Copy(p) if p.proj.is_empty() && matches!(p.base, mir::Base::Local(_)) => {
                let mir::Base::Local(l) = p.base else {
                    unreachable!()
                };
                fx.local_types[l.0 as usize]
            }
            o => {
                let t = self.operand_ty(fx, o)?;
                self.wasm_ty(&t)
            }
        };
        self.coerce(s, from, self.wasm_ty(to));
        Ok(())
    }

    /// Casts the reference on the stack (of wasm type `from`) to `to` when they differ.
    fn coerce(&self, s: &mut InstructionSink, from: Option<ValType>, to: Option<ValType>) {
        if let (Some(f), Some(ValType::Ref(t))) = (from, to)
            && f != ValType::Ref(t)
            && matches!(f, ValType::Ref(_))
        {
            s.ref_cast_nullable(t.heap_type);
        }
    }

    fn operand(&mut self, fx: &mut FnCx, s: &mut InstructionSink, o: &Operand) -> R<()> {
        match o {
            Operand::Copy(p) => self.load(fx, s, p, true),
            Operand::Const(c) => self.constant(s, c),
        }
    }

    fn operand_ty(&self, fx: &FnCx, o: &Operand) -> R<Ty> {
        match o {
            Operand::Copy(p) => self.place_ty(fx, p),
            Operand::Const(c) => Ok(c.ty()),
        }
    }

    fn constant(&mut self, s: &mut InstructionSink, c: &Const) -> R<()> {
        match c {
            Const::Int(v, it) => {
                if wide(*it) {
                    s.i64_const(*v as i64);
                } else {
                    s.i32_const(*v as i32);
                }
            }
            Const::Float(v, FloatTy::F32) => {
                s.f32_const(Ieee32::from(*v as f32));
            }
            Const::Float(v, _) => {
                s.f64_const(Ieee64::from(*v));
            }
            Const::Bool(b) => {
                s.i32_const(*b as i32);
            }
            Const::Char(c) => {
                s.i32_const(*c as i32);
            }
            Const::Str(text) => {
                let d = self.string_data(text);
                s.i32_const(0);
                s.i32_const(text.len() as i32);
                s.array_new_data(STR, d);
            }
            // Any value of a value aggregate is a fresh default one: a callee may assign into it
            // through a reference (a defaulted parameter).
            Const::Undef(t) if Self::is_value_aggregate(t) => {
                let v = self.ty_or_err(t)?.expect("a value");
                let idx = heap_index(v).expect("a ref");
                match t.erase() {
                    Ty::Fixed(_, n) => {
                        s.i32_const(n as i32);
                        s.array_new_default(idx);
                    }
                    _ => {
                        s.struct_new_default(idx);
                    }
                }
            }
            Const::Null(t) | Const::Undef(t) => {
                if no_value(t) {
                    return Ok(());
                }
                match self.wasm_ty(t) {
                    Some(ValType::Ref(r)) => {
                        s.ref_null(r.heap_type);
                    }
                    // (Into a placeholder: see `ANYREF`.)
                    None if matches!(t, Ty::Nullable(_)) => {
                        s.ref_null(RefType::ANYREF.heap_type);
                    }
                    // A `null` (or an omitted argument) of a type left generic: the bottom
                    // reference fits any reference slot.
                    None => {
                        s.ref_null(HeapType::Abstract {
                            shared: false,
                            ty: wasm_encoder::AbstractHeapType::None,
                        });
                    }
                    Some(ValType::I32) => {
                        s.i32_const(0);
                    }
                    Some(ValType::I64) => {
                        s.i64_const(0);
                    }
                    Some(ValType::F32) => {
                        s.f32_const(Ieee32::from(0.0f32));
                    }
                    Some(ValType::F64) => {
                        s.f64_const(Ieee64::from(0.0f64));
                    }
                    _ => return unsupported(format!("`null` of `{t}`")),
                }
            }
            Const::Void => {}
        }
        Ok(())
    }

    /// Panics with `msg` at `loc` when the `i32` on the stack is true.
    fn check(&mut self, s: &mut InstructionSink, msg: &str, loc: mir::Loc) {
        s.if_(BlockType::Empty);
        self.panic_const(s, msg, loc);
        s.end();
    }

    fn panic_const(&mut self, s: &mut InstructionSink, msg: &str, loc: mir::Loc) {
        let d = self.string_data(msg);
        s.i32_const(0);
        s.i32_const(msg.len() as i32);
        s.array_new_data(STR, d);
        self.throw_panic(s, loc);
    }

    /// Panics with the message on the stack: it goes to the globals the glue (or a panic
    /// guard) reads, and the `panic` tag is thrown (decision 268).
    fn throw_panic(&self, s: &mut InstructionSink, loc: mir::Loc) {
        let g = self.panic_global();
        s.global_set(g);
        s.i32_const(loc.line as i32);
        s.global_set(g + 1);
        s.i32_const(loc.col as i32);
        s.global_set(g + 2);
        s.throw(0);
    }

    /// Pushes an rvalue (of type `t`); `Ok(None)` when it produces nothing.
    fn rvalue(
        &mut self,
        fx: &mut FnCx,
        s: &mut InstructionSink,
        rv: &Rvalue,
        t: &Ty,
        loc: mir::Loc,
    ) -> R<Option<()>> {
        match rv {
            Rvalue::Use(o) => {
                self.operand(fx, s, o)?;
            }
            Rvalue::Binary(op, a, b) => {
                let at = self.operand_ty(fx, a)?;
                let bt = self.operand_ty(fx, b)?;
                self.operand(fx, s, a)?;
                self.operand(fx, s, b)?;
                // A shift amount of another integer type: as the shifted value's width (it's
                // checked against the width first, unsigned, so negative amounts fail too).
                if let (BinOp::Shl | BinOp::Shr | BinOp::UShr, Ty::Int(ai), Ty::Int(bi)) =
                    (op, at.erase(), bt.erase())
                    && wide(ai) != wide(bi)
                {
                    if wide(ai) {
                        self.extend(s, bi);
                    } else {
                        // (A 64-bit amount too large for 32 bits is out of range anyway.)
                        let v = self.i64_tmp(fx, 8);
                        s.local_tee(v);
                        s.i64_const(u32::MAX as i64);
                        s.i64_gt_u();
                        self.check(s, pico_mir::mir::panics::SHIFT, loc);
                        s.local_get(v);
                        s.i32_wrap_i64();
                    }
                }
                self.binary(fx, s, *op, &at, loc)?;
            }
            Rvalue::Unary(op, a) => {
                let at = self.operand_ty(fx, a)?;
                self.unary(fx, s, *op, a, &at, loc)?;
            }
            Rvalue::Cast(o, to) => {
                let from = self.operand_ty(fx, o)?;
                self.operand(fx, s, o)?;
                self.cast(fx, s, &from, to, loc)?;
            }
            Rvalue::ToStr(o) => {
                let ot = self.operand_ty(fx, o)?;
                self.operand(fx, s, o)?;
                self.string_form(s, &ot)?;
            }
            Rvalue::StrLen(o) => {
                self.operand(fx, s, o)?;
                s.call(helper_index("str_chars"));
            }
            Rvalue::Aggregate(ty, ops) => {
                let (idx, st) = self.struct_of(ty)?;
                let fields = st.clone();
                for (o, ft) in ops.iter().zip(&fields) {
                    if no_value(ft) {
                        s.ref_null(RefType::ANYREF.heap_type);
                        continue;
                    }
                    self.operand_to(fx, s, o, ft)?;
                }
                s.struct_new(idx);
            }
            Rvalue::New(ty) => {
                let (idx, _) = self.struct_of(ty)?;
                let Ty::Class(sid, ..) = ty.erase() else {
                    return unsupported(format!("`new` of `{ty}`"));
                };
                let g = self.vt_global(sid);
                let tmp = self.ref_scratch(fx, idx);
                s.struct_new_default(idx);
                s.local_tee(tmp);
                s.global_get(g);
                s.struct_set(idx, 0);
                s.local_get(tmp);
            }
            Rvalue::InstanceOf(o, sid) => {
                self.operand(fx, s, o)?;
                let cls = self.struct_type(&Ty::Class(
                    *sid,
                    self.p.layout(*sid).name.as_str().into(),
                    Vec::new().into(),
                ));
                s.ref_test_non_null(HeapType::Concrete(cls));
            }
            Rvalue::ShowEnter(o) => {
                // (The glue keeps the objects being shown: a cycle shows as `(…)`.)
                self.operand(fx, s, o)?;
                s.call(import_index("show_enter"));
            }
            Rvalue::Closure(f, ops) => {
                let ft = self
                    .wasm_ty(t)
                    .and_then(heap_index)
                    .expect("a function value");
                let callee = &self.p.fns[f.0 as usize];
                if ops.is_empty() {
                    s.ref_null(RefType::ANYREF.heap_type);
                } else {
                    // The environment: the lifted function's first parameter is `ptr<[…]>`.
                    let env_ty = match callee.params.first().map(|p| callee.local_ty(*p).erase()) {
                        Some(Ty::Ref(inner)) => *inner,
                        other => {
                            return unsupported(format!("a closure environment of `{other:?}`"));
                        }
                    };
                    let (env, _) = self.struct_of(&env_ty)?;
                    for o in ops {
                        self.operand(fx, s, o)?;
                    }
                    s.struct_new(env);
                }
                let Some(&fi) = self.fn_index.get(f) else {
                    return unsupported("a closure of a dropped function");
                };
                if !self.refd.contains(&fi) {
                    self.refd.push(fi);
                }
                // (`{ fn, env }`: the env was pushed first, so swap through a scratch local.)
                let env_tmp = fx.scratch(ANYREF);
                s.local_set(env_tmp);
                s.ref_func(fi);
                s.local_get(env_tmp);
                s.struct_new(ft);
            }
            Rvalue::BoxNew(ty, o) => {
                let uninit = matches!(o, Operand::Const(Const::Void));
                if Self::is_value_aggregate(ty) {
                    if uninit {
                        let v = self.ty_or_err(ty)?.expect("a value");
                        match ty.erase() {
                            Ty::Fixed(_, n) => {
                                s.i32_const(n as i32);
                                s.array_new_default(heap_index(v).expect("a ref"));
                            }
                            _ => {
                                s.struct_new_default(heap_index(v).expect("a ref"));
                            }
                        }
                    } else {
                        self.operand(fx, s, o)?;
                    }
                } else {
                    let cell = self
                        .wasm_ty(&Ty::Ref(Box::new(ty.clone())))
                        .and_then(heap_index)
                        .expect("a cell");
                    if uninit {
                        s.struct_new_default(cell);
                    } else {
                        self.operand(fx, s, o)?;
                        s.struct_new(cell);
                    }
                }
            }
            Rvalue::Variant(eid, v, ops) => {
                let vt = self.variant_type(*eid, *v);
                s.i32_const(*v as i32);
                let fields: Vec<Ty> = self.p.enum_layout(*eid).variants[*v as usize]
                    .fields
                    .iter()
                    .map(|(_, t)| t.clone())
                    .collect();
                for (o, ft) in ops.iter().zip(&fields) {
                    if no_value(ft) {
                        s.ref_null(RefType::ANYREF.heap_type);
                    } else {
                        self.operand_to(fx, s, o, ft)?;
                    }
                }
                s.struct_new(vt);
            }
            Rvalue::Tag(o) => {
                let ot = self.operand_ty(fx, o)?;
                let Ty::Enum(eid, _) = ot.erase() else {
                    return unsupported(format!("the tag of `{ot}`"));
                };
                self.operand(fx, s, o)?;
                s.struct_get(self.enum_type(eid), 0);
            }
            Rvalue::ToIface(o, i) => {
                let it = self.register(Key::Iface, |_| Def::Iface);
                self.operand(fx, s, o)?;
                s.i32_const(self.itable_base[*i as usize] as i32);
                s.struct_new(it);
            }
            Rvalue::IfaceData(o) => {
                let it = self.register(Key::Iface, |_| Def::Iface);
                self.operand(fx, s, o)?;
                s.struct_get(it, 0);
                match self.wasm_ty(t) {
                    Some(ValType::Ref(r)) => {
                        s.ref_cast_nullable(r.heap_type);
                    }
                    _ => return unsupported(format!("interface data of `{t}`")),
                }
            }
            Rvalue::Ref(p) => {
                let pt = self.place_ty(fx, p)?;
                if !matches!(pt.erase(), Ty::Struct(..)) {
                    return unsupported(format!("references to `{pt}`"));
                }
                // The struct itself (structs are references here).
                self.load(fx, s, p, false)?;
            }
            Rvalue::WrapSome(o, _) => {
                let ot = self.operand_ty(fx, o)?;
                self.operand(fx, s, o)?;
                if !is_ref_like(&ot) {
                    let b = self.opt_type(&ot)?;
                    s.struct_new(b);
                }
            }
            Rvalue::IsNull(o) => {
                self.operand(fx, s, o)?;
                s.ref_is_null();
            }
            Rvalue::Unwrap(o) => {
                let ot = self.operand_ty(fx, o)?;
                self.operand(fx, s, o)?;
                if let Ty::Nullable(inner) = ot.erase()
                    && !is_ref_like(&inner)
                {
                    let b = self.opt_type(&inner)?;
                    s.struct_get(b, 0);
                }
            }
            Rvalue::Tuple(ty, ops) => match ty.erase() {
                Ty::Fixed(elem, _) => {
                    let e = self.elem_storage(&elem)?;
                    let raw = self.raw_type(e);
                    for o in ops {
                        self.operand_to(fx, s, o, &elem)?;
                    }
                    s.array_new_fixed(raw, ops.len() as u32);
                }
                _ => {
                    let (idx, fields) = self.struct_of(ty)?;
                    for (o, ft) in ops.iter().zip(&fields) {
                        if no_value(ft) {
                            s.ref_null(RefType::ANYREF.heap_type);
                        } else {
                            self.operand_to(fx, s, o, ft)?;
                        }
                    }
                    s.struct_new(idx);
                }
            },
            Rvalue::Repeat(o, n) => {
                let ot = self.operand_ty(fx, o)?;
                if Self::is_value_aggregate(&ot) {
                    return unsupported("`[value; n]` of structs");
                }
                let e = self.ty_or_err(&ot)?.expect("a value");
                let raw = self.raw_type(e);
                self.operand(fx, s, o)?;
                s.i32_const(*n as i32);
                s.array_new(raw);
            }
            Rvalue::ArrayNew(elem, ops) => {
                let e = self.elem_storage(elem)?;
                let raw = self.raw_type(e);
                let grow = heap_index(
                    self.wasm_ty(&Ty::Array(Box::new(elem.clone())))
                        .expect("an array"),
                )
                .expect("a ref");
                for o in ops {
                    if no_value(elem) {
                        s.ref_null(RefType::ANYREF.heap_type);
                    } else {
                        self.operand_to(fx, s, o, elem)?;
                    }
                }
                s.array_new_fixed(raw, ops.len() as u32);
                s.i32_const(ops.len() as i32);
                s.struct_new(grow);
            }
            Rvalue::ArrayLen(a) => {
                let at = self.operand_ty(fx, a)?;
                self.operand(fx, s, a)?;
                match at.erase() {
                    Ty::Fixed(..) => {
                        s.array_len();
                    }
                    _ => {
                        let grow = self.wasm_ty(&at).and_then(heap_index).expect("an array");
                        s.struct_get(grow, 1);
                    }
                }
                s.i64_extend_i32_u();
            }
            Rvalue::ArrayPop(a) => {
                let at = self.operand_ty(fx, a)?;
                let f = self.array_fn(&at, "pop")?;
                self.operand(fx, s, a)?;
                s.call(f);
            }
            Rvalue::ArraySlice(a, from, to) => {
                let at = self.operand_ty(fx, a)?;
                let f = self.array_fn(&at, "slice")?;
                self.operand(fx, s, a)?;
                self.operand(fx, s, from)?;
                self.operand(fx, s, to)?;
                s.call(f);
            }
            Rvalue::Format(o, spec) => {
                let ot = self.operand_ty(fx, o)?;
                self.operand(fx, s, o)?;
                use pico_sema::format::Kind;
                let compound = pico_sema::format::is_compound(&ot)
                    || (matches!(ot.erase(), Ty::Nullable(_)) && spec.kind == Kind::Debug);
                if compound {
                    // The debug text, then width, fill and alignment on the whole of it.
                    let f = self.debug_fn(&ot)?;
                    s.call(f);
                    let outer = pico_sema::format::FormatSpec {
                        kind: Kind::Default,
                        ..*spec
                    };
                    spec_args(s, &outer);
                    s.call(import_index("fmt_spec_str"));
                } else {
                    self.format_prim(s, &ot, spec)?;
                }
            }
            Rvalue::StrCharAt(a, i) => {
                let f = self.str_fn("char_at");
                self.operand(fx, s, a)?;
                self.operand(fx, s, i)?;
                s.call(f);
                let v = fx.scratch(ValType::I32);
                s.local_tee(v);
                s.i32_const(0);
                s.i32_lt_s();
                self.check(s, pico_mir::mir::panics::INDEX, loc);
                s.local_get(v);
            }
            Rvalue::StrSlice(a, from, to) => {
                let f = self.str_fn("slice");
                self.operand(fx, s, a)?;
                self.operand(fx, s, from)?;
                self.operand(fx, s, to)?;
                s.call(f);
            }
            Rvalue::InBounds(i, n) => {
                // 0 <= i < n, both `int`.
                self.operand(fx, s, i)?;
                self.operand(fx, s, n)?;
                s.i64_lt_u();
            }
            Rvalue::AddOverflows(a, b) => {
                let at = self.operand_ty(fx, a)?;
                let Ty::Int(it) = at.erase() else {
                    return unsupported("AddOverflows");
                };
                self.operand(fx, s, a)?;
                self.operand(fx, s, b)?;
                self.add_overflows(fx, s, it, false)?;
            }
            Rvalue::Hash(o) => {
                let ot = self.operand_ty(fx, o)?;
                self.operand(fx, s, o)?;
                match ot.erase() {
                    Ty::Str => {
                        let f = self.hash_fn("str");
                        s.call(f);
                    }
                    t => {
                        match t {
                            Ty::Int(it) if wide(it) => {}
                            Ty::Int(it) => self.extend(s, it),
                            Ty::Float(FloatTy::F32) => {
                                s.f64_promote_f32();
                                s.i64_reinterpret_f64();
                            }
                            Ty::Float(_) => {
                                s.i64_reinterpret_f64();
                            }
                            Ty::Bool | Ty::Char => {
                                s.i64_extend_i32_u();
                            }
                            t => return unsupported(format!("hashing `{t}`")),
                        }
                        let f = self.hash_fn("u64");
                        s.call(f);
                    }
                }
            }
            Rvalue::HashCombine(a, b) => {
                self.operand(fx, s, a)?;
                self.operand(fx, s, b)?;
                let f = self.hash_fn("combine");
                s.call(f);
            }
            Rvalue::ExceptionPending => {
                s.global_get(self.exc_global());
                s.ref_is_null();
                s.i32_eqz();
            }
            Rvalue::ExceptionTag => {
                s.global_get(self.exc_global() + 1);
            }
            Rvalue::TakeException(_) => {
                // (As `anyref`: see `local_types`.)
                let g = self.exc_global();
                s.global_get(g);
                s.ref_null(RefType::ANYREF.heap_type);
                s.global_set(g);
            }
            other => {
                let name = format!("{other:?}");
                let name = name.split(['(', ' ']).next().unwrap_or("");
                return unsupported(format!("`{name}` (in MIR)"));
            }
        }
        let _ = t;
        Ok(Some(()))
    }

    fn unary(
        &mut self,
        fx: &mut FnCx,
        s: &mut InstructionSink,
        op: UnOp,
        a: &Operand,
        t: &Ty,
        loc: mir::Loc,
    ) -> R<()> {
        match (op, t.erase()) {
            (UnOp::Not, Ty::Bool) => {
                self.operand(fx, s, a)?;
                s.i32_eqz();
            }
            (UnOp::Neg, Ty::Float(FloatTy::F32)) => {
                self.operand(fx, s, a)?;
                s.f32_neg();
            }
            (UnOp::Neg, Ty::Float(_)) => {
                self.operand(fx, s, a)?;
                s.f64_neg();
            }
            (UnOp::Neg, Ty::Int(it)) => {
                // 0 - a, checked.
                if wide(it) {
                    s.i64_const(0);
                } else {
                    s.i32_const(0);
                }
                self.operand(fx, s, a)?;
                self.int_binary(fx, s, BinOp::Sub, it, loc)?;
            }
            (UnOp::BitNot, Ty::Int(it)) => {
                self.operand(fx, s, a)?;
                if wide(it) {
                    s.i64_const(-1);
                    s.i64_xor();
                } else {
                    s.i32_const(-1);
                    s.i32_xor();
                    self.normalize(s, it);
                }
            }
            (op, t) => return unsupported(format!("`{op:?}` on `{t}`")),
        }
        Ok(())
    }

    /// Brings an `i32` holding an integer of a narrow type back to its canonical form
    /// (sign- or zero-extended from its width).
    fn normalize(&self, s: &mut InstructionSink, it: IntTy) {
        match int_bits(it) {
            8 if it.signed() => {
                s.i32_extend8_s();
            }
            16 if it.signed() => {
                s.i32_extend16_s();
            }
            8 => {
                s.i32_const(0xff);
                s.i32_and();
            }
            16 => {
                s.i32_const(0xffff);
                s.i32_and();
            }
            _ => {}
        }
    }

    /// Whether `a + b` (on the stack) overflows `it`; pushes the `i32` flag (and drops the
    /// operands). With `keep`, the sum stays under the flag.
    fn add_overflows(
        &mut self,
        fx: &mut FnCx,
        s: &mut InstructionSink,
        it: IntTy,
        keep: bool,
    ) -> R<()> {
        let _ = keep;
        if wide(it) {
            let b = fx.scratch(ValType::I64);
            s.local_set(b);
            let a = self.i64_tmp(fx, 0);
            s.local_set(a);
            let r = self.i64_tmp(fx, 1);
            s.local_get(a);
            s.local_get(b);
            s.i64_add();
            s.local_set(r);
            if it.signed() {
                // ((a ^ r) & (b ^ r)) < 0
                s.local_get(a);
                s.local_get(r);
                s.i64_xor();
                s.local_get(b);
                s.local_get(r);
                s.i64_xor();
                s.i64_and();
                s.i64_const(0);
                s.i64_lt_s();
            } else {
                s.local_get(r);
                s.local_get(a);
                s.i64_lt_u();
            }
        } else {
            // In 64 bits, then the type's range.
            let b = fx.scratch(ValType::I32);
            s.local_set(b);
            self.extend(s, it);
            s.local_get(b);
            self.extend(s, it);
            s.i64_add();
            self.out_of_range(fx, s, it);
        }
        Ok(())
    }

    /// Distinct `i64` scratch locals (beyond the shared one).
    fn i64_tmp(&self, fx: &mut FnCx, n: u8) -> u32 {
        fx.tmp(100 + n as u32, ValType::I64)
    }

    /// `i32` of a narrow integer → its `i64` value.
    fn extend(&self, s: &mut InstructionSink, it: IntTy) {
        if it.signed() {
            s.i64_extend_i32_s();
        } else {
            s.i64_extend_i32_u();
        }
    }

    /// Consumes an `i64` value: pushes whether it's outside narrow type `it`'s range.
    fn out_of_range(&self, fx: &mut FnCx, s: &mut InstructionSink, it: IntTy) {
        let v = self.i64_tmp(fx, 2);
        s.local_set(v);
        s.local_get(v);
        s.i64_const(it.min() as i64);
        s.i64_lt_s();
        s.local_get(v);
        s.i64_const(it.max() as i64);
        s.i64_gt_s();
        s.i32_or();
    }

    /// Like [`Self::out_of_range`], leaving the `i32` result (the value wrapped) under it.
    fn narrow_checked(
        &mut self,
        fx: &mut FnCx,
        s: &mut InstructionSink,
        it: IntTy,
        loc: mir::Loc,
        msg: &str,
    ) {
        let v = self.i64_tmp(fx, 3);
        s.local_tee(v);
        self.out_of_range(fx, s, it);
        self.check(s, msg, loc);
        s.local_get(v);
        s.i32_wrap_i64();
    }

    fn binary(
        &mut self,
        fx: &mut FnCx,
        s: &mut InstructionSink,
        op: BinOp,
        t: &Ty,
        loc: mir::Loc,
    ) -> R<()> {
        use BinOp::*;
        match t.erase() {
            Ty::Int(it) => self.int_binary(fx, s, op, it, loc),
            Ty::Float(ft) => {
                let f32_ = ft == FloatTy::F32;
                match (op, f32_) {
                    (Add, false) => s.f64_add(),
                    (Sub, false) => s.f64_sub(),
                    (Mul, false) => s.f64_mul(),
                    (Div, false) => s.f64_div(),
                    (Add, true) => s.f32_add(),
                    (Sub, true) => s.f32_sub(),
                    (Mul, true) => s.f32_mul(),
                    (Div, true) => s.f32_div(),
                    (Eq, false) => s.f64_eq(),
                    (Ne, false) => s.f64_ne(),
                    (Lt, false) => s.f64_lt(),
                    (Le, false) => s.f64_le(),
                    (Gt, false) => s.f64_gt(),
                    (Ge, false) => s.f64_ge(),
                    (Eq, true) => s.f32_eq(),
                    (Ne, true) => s.f32_ne(),
                    (Lt, true) => s.f32_lt(),
                    (Le, true) => s.f32_le(),
                    (Gt, true) => s.f32_gt(),
                    (Ge, true) => s.f32_ge(),
                    (Pow, false) => s.call(import_index("pow_f64")),
                    _ => return unsupported(format!("`{op:?}` on `{t}`")),
                };
                Ok(())
            }
            Ty::Bool | Ty::Char => {
                match op {
                    Eq => s.i32_eq(),
                    Ne => s.i32_ne(),
                    Lt => s.i32_lt_u(),
                    Le => s.i32_le_u(),
                    Gt => s.i32_gt_u(),
                    Ge => s.i32_ge_u(),
                    BitAnd => s.i32_and(),
                    BitOr => s.i32_or(),
                    BitXor => s.i32_xor(),
                    _ => return unsupported(format!("`{op:?}` on `{t}`")),
                };
                Ok(())
            }
            Ty::Str => {
                match op {
                    Add => {
                        s.call(helper_index("str_concat"));
                    }
                    Eq => {
                        s.call(helper_index("str_eq"));
                    }
                    Ne => {
                        s.call(helper_index("str_eq"));
                        s.i32_eqz();
                    }
                    Lt | Le | Gt | Ge => {
                        s.call(helper_index("str_cmp"));
                        s.i32_const(0);
                        match op {
                            Lt => s.i32_lt_s(),
                            Le => s.i32_le_s(),
                            Gt => s.i32_gt_s(),
                            _ => s.i32_ge_s(),
                        };
                    }
                    _ => return unsupported(format!("`{op:?}` on strings")),
                }
                Ok(())
            }
            t2 @ (Ty::Nullable(_)
            | Ty::Struct(..)
            | Ty::Tuple(_)
            | Ty::Range(_)
            | Ty::Fixed(..)
            | Ty::Enum(..))
                if matches!(op, Eq | Ne) =>
            {
                let f = self.eq_fn(&t2)?;
                s.call(f);
                if op == Ne {
                    s.i32_eqz();
                }
                Ok(())
            }
            Ty::Class(..) => {
                match op {
                    Eq => s.ref_eq(),
                    Ne => {
                        s.ref_eq();
                        s.i32_eqz()
                    }
                    _ => return unsupported(format!("`{op:?}` on objects")),
                };
                Ok(())
            }
            t => unsupported(format!("`{op:?}` on `{t}`")),
        }
    }

    fn int_binary(
        &mut self,
        fx: &mut FnCx,
        s: &mut InstructionSink,
        op: BinOp,
        it: IntTy,
        loc: mir::Loc,
    ) -> R<()> {
        use BinOp::*;
        use pico_mir::mir::panics;
        let signed = it.signed();
        if wide(it) {
            match op {
                Add | Sub => {
                    let b = self.i64_tmp(fx, 4);
                    s.local_set(b);
                    let a = self.i64_tmp(fx, 5);
                    s.local_set(a);
                    let r = self.i64_tmp(fx, 6);
                    s.local_get(a);
                    s.local_get(b);
                    if op == Add {
                        s.i64_add();
                    } else {
                        s.i64_sub();
                    }
                    s.local_set(r);
                    match (op, signed) {
                        (Add, true) => {
                            s.local_get(a);
                            s.local_get(r);
                            s.i64_xor();
                            s.local_get(b);
                            s.local_get(r);
                            s.i64_xor();
                            s.i64_and();
                            s.i64_const(0);
                            s.i64_lt_s();
                        }
                        (Add, false) => {
                            s.local_get(r);
                            s.local_get(a);
                            s.i64_lt_u();
                        }
                        (_, true) => {
                            s.local_get(a);
                            s.local_get(b);
                            s.i64_xor();
                            s.local_get(a);
                            s.local_get(r);
                            s.i64_xor();
                            s.i64_and();
                            s.i64_const(0);
                            s.i64_lt_s();
                        }
                        (_, false) => {
                            s.local_get(a);
                            s.local_get(b);
                            s.i64_lt_u();
                        }
                    }
                    self.check(s, panics::OVERFLOW, loc);
                    s.local_get(r);
                }
                Mul => {
                    let b = self.i64_tmp(fx, 4);
                    s.local_set(b);
                    let a = self.i64_tmp(fx, 5);
                    s.local_set(a);
                    s.local_get(a);
                    s.local_get(b);
                    s.call(helper_index(if signed { "mul_ovf_s" } else { "mul_ovf_u" }));
                    self.check(s, panics::OVERFLOW, loc);
                    s.local_get(a);
                    s.local_get(b);
                    s.i64_mul();
                }
                Div | Rem => {
                    let b = self.i64_tmp(fx, 4);
                    s.local_set(b);
                    let a = self.i64_tmp(fx, 5);
                    s.local_set(a);
                    s.local_get(b);
                    s.i64_eqz();
                    self.check(s, panics::DIV_ZERO, loc);
                    if signed && op == Div {
                        s.local_get(a);
                        s.i64_const(i64::MIN);
                        s.i64_eq();
                        s.local_get(b);
                        s.i64_const(-1);
                        s.i64_eq();
                        s.i32_and();
                        self.check(s, panics::OVERFLOW, loc);
                    }
                    s.local_get(a);
                    s.local_get(b);
                    match (op, signed) {
                        (Div, true) => s.i64_div_s(),
                        (Div, false) => s.i64_div_u(),
                        (_, true) => s.i64_rem_s(),
                        (_, false) => s.i64_rem_u(),
                    };
                }
                Shl | Shr | UShr => {
                    // (The amount has the same type here: MIR keeps the right operand's.)
                    let b = self.i64_tmp(fx, 4);
                    s.local_tee(b);
                    s.i64_const(64);
                    s.i64_ge_u();
                    self.check(s, panics::SHIFT, loc);
                    s.local_get(b);
                    match op {
                        Shl => s.i64_shl(),
                        Shr if signed => s.i64_shr_s(),
                        _ => s.i64_shr_u(),
                    };
                }
                BitAnd => {
                    s.i64_and();
                }
                BitOr => {
                    s.i64_or();
                }
                BitXor => {
                    s.i64_xor();
                }
                Eq => {
                    s.i64_eq();
                }
                Ne => {
                    s.i64_ne();
                }
                Lt => {
                    if signed {
                        s.i64_lt_s()
                    } else {
                        s.i64_lt_u()
                    };
                }
                Le => {
                    if signed {
                        s.i64_le_s()
                    } else {
                        s.i64_le_u()
                    };
                }
                Gt => {
                    if signed {
                        s.i64_gt_s()
                    } else {
                        s.i64_gt_u()
                    };
                }
                Ge => {
                    if signed {
                        s.i64_ge_s()
                    } else {
                        s.i64_ge_u()
                    };
                }
                Pow => return unsupported("integer `**`"),
            }
            return Ok(());
        }
        // Narrow integers (i32 and below): through 64 bits where it can overflow.
        match op {
            Add | Sub | Mul => {
                let b = fx.scratch(ValType::I32);
                s.local_set(b);
                self.extend(s, it);
                s.local_get(b);
                self.extend(s, it);
                match op {
                    Add => s.i64_add(),
                    Sub => s.i64_sub(),
                    _ => s.i64_mul(),
                };
                self.narrow_checked(fx, s, it, loc, panics::OVERFLOW);
            }
            Div | Rem => {
                let b = fx.scratch(ValType::I32);
                s.local_set(b);
                let a = self.i32_tmp(fx, 1);
                s.local_set(a);
                s.local_get(b);
                s.i32_eqz();
                self.check(s, panics::DIV_ZERO, loc);
                s.local_get(a);
                self.extend(s, it);
                s.local_get(b);
                self.extend(s, it);
                match (op, signed) {
                    (Div, true) => s.i64_div_s(),
                    (Div, false) => s.i64_div_u(),
                    (_, true) => s.i64_rem_s(),
                    (_, false) => s.i64_rem_u(),
                };
                self.narrow_checked(fx, s, it, loc, panics::OVERFLOW);
            }
            Shl | Shr | UShr => {
                let b = fx.scratch(ValType::I32);
                s.local_tee(b);
                s.i32_const(int_bits(it) as i32);
                s.i32_ge_u();
                self.check(s, panics::SHIFT, loc);
                s.local_get(b);
                match op {
                    Shl => s.i32_shl(),
                    Shr if signed => s.i32_shr_s(),
                    _ => {
                        // (A narrow signed value is sign-extended: mask it to its width first.)
                        if signed && int_bits(it) < 32 {
                            return unsupported("`>>>` on narrow signed integers");
                        }
                        s.i32_shr_u()
                    }
                };
                self.normalize(s, it);
            }
            BitAnd => {
                s.i32_and();
            }
            BitOr => {
                s.i32_or();
            }
            BitXor => {
                s.i32_xor();
            }
            Eq => {
                s.i32_eq();
            }
            Ne => {
                s.i32_ne();
            }
            Lt => {
                if signed {
                    s.i32_lt_s()
                } else {
                    s.i32_lt_u()
                };
            }
            Le => {
                if signed {
                    s.i32_le_s()
                } else {
                    s.i32_le_u()
                };
            }
            Gt => {
                if signed {
                    s.i32_gt_s()
                } else {
                    s.i32_gt_u()
                };
            }
            Ge => {
                if signed {
                    s.i32_ge_s()
                } else {
                    s.i32_ge_u()
                };
            }
            Pow => return unsupported("integer `**`"),
        }
        Ok(())
    }

    fn i32_tmp(&self, fx: &mut FnCx, n: u8) -> u32 {
        fx.tmp(200 + n as u32, ValType::I32)
    }

    fn cast(
        &mut self,
        fx: &mut FnCx,
        s: &mut InstructionSink,
        from: &Ty,
        to: &Ty,
        loc: mir::Loc,
    ) -> R<()> {
        use pico_mir::mir::panics;
        let from_int = match from.erase() {
            Ty::Int(it) => Some(it),
            Ty::Char => Some(IntTy::U32),
            _ => None,
        };
        match (from_int, from.erase(), to.erase()) {
            (Some(fi), _, Ty::Int(ti)) => {
                // To an `i64` holding the value (unsigned 64-bit values above `i64::MAX` are
                // checked apart).
                let fits = ti.min() <= fi.min() && fi.max() <= ti.max();
                if wide(fi) {
                    if fits {
                        if !wide(ti) {
                            s.i32_wrap_i64();
                        }
                        return Ok(());
                    }
                    let v = self.i64_tmp(fx, 7);
                    s.local_set(v);
                    if fi == IntTy::U64 || fi == IntTy::Usize {
                        // Unsigned: v <=u max(ti).
                        s.local_get(v);
                        s.i64_const(ti.max().min(u64::MAX as i128) as u64 as i64);
                        s.i64_gt_u();
                    } else {
                        s.local_get(v);
                        s.i64_const(ti.min() as i64);
                        s.i64_lt_s();
                        s.local_get(v);
                        s.i64_const(ti.max().min(i64::MAX as i128) as i64);
                        s.i64_gt_s();
                        s.i32_or();
                    }
                    self.check(s, panics::INT_CAST, loc);
                    s.local_get(v);
                    if !wide(ti) {
                        s.i32_wrap_i64();
                    }
                } else {
                    self.extend(s, fi);
                    if fits {
                        if !wide(ti) {
                            s.i32_wrap_i64();
                        }
                        return Ok(());
                    }
                    if wide(ti) {
                        // Only negatives into unsigned 64 bits fail.
                        let v = self.i64_tmp(fx, 7);
                        s.local_tee(v);
                        s.i64_const(0);
                        s.i64_lt_s();
                        self.check(s, panics::INT_CAST, loc);
                        s.local_get(v);
                    } else {
                        self.narrow_checked(fx, s, ti, loc, panics::INT_CAST);
                    }
                }
                Ok(())
            }
            (Some(fi), _, Ty::Char) => {
                if wide(fi) {
                    s.i32_wrap_i64();
                }
                let v = fx.scratch(ValType::I32);
                s.local_tee(v);
                s.i32_const(0x10ffff);
                s.i32_gt_u();
                s.local_get(v);
                s.i32_const(0xd800);
                s.i32_ge_u();
                s.local_get(v);
                s.i32_const(0xdfff);
                s.i32_le_u();
                s.i32_and();
                s.i32_or();
                self.check(s, panics::CHAR_CAST, loc);
                s.local_get(v);
                Ok(())
            }
            (Some(fi), _, Ty::Float(ft)) => {
                if !wide(fi) {
                    self.extend(s, fi);
                }
                match (fi.signed(), ft) {
                    (true, FloatTy::F32) => s.f32_convert_i64_s(),
                    (false, FloatTy::F32) => s.f32_convert_i64_u(),
                    (true, _) => s.f64_convert_i64_s(),
                    (false, _) => s.f64_convert_i64_u(),
                };
                Ok(())
            }
            (None, Ty::Float(f1), Ty::Float(f2)) => {
                match (f1, f2) {
                    (FloatTy::F32, FloatTy::F64) => {
                        s.f64_promote_f32();
                    }
                    (FloatTy::F64, FloatTy::F32) => {
                        s.f32_demote_f64();
                    }
                    _ => {}
                }
                Ok(())
            }
            (None, Ty::Float(f1), Ty::Int(ti)) => {
                if f1 == FloatTy::F32 {
                    s.f64_promote_f32();
                }
                let v = fx.scratch(ValType::F64);
                s.local_tee(v);
                // In range (NaN isn't): min - 1 < x < max + 1.
                let lo = if ti.signed() {
                    ti.min() as f64 - 1.0
                } else {
                    -1.0
                };
                let hi = ti.max() as f64 + 1.0;
                s.f64_const(Ieee64::from(lo));
                s.f64_gt();
                s.local_get(v);
                s.f64_const(Ieee64::from(hi));
                s.f64_lt();
                s.i32_and();
                s.i32_eqz();
                self.check(s, panics::FLOAT_CAST, loc);
                s.local_get(v);
                if ti.signed() {
                    s.i64_trunc_sat_f64_s();
                } else {
                    s.i64_trunc_sat_f64_u();
                }
                if !wide(ti) {
                    s.i32_wrap_i64();
                }
                Ok(())
            }
            (_, f, t) if f == t => Ok(()),
            (_, f, t) => unsupported(format!("`{f}` as `{t}`")),
        }
    }

    /// A primitive on the stack formatted with a spec.
    fn format_prim(
        &mut self,
        s: &mut InstructionSink,
        t: &Ty,
        spec: &pico_sema::format::FormatSpec,
    ) -> R<()> {
        match t.erase() {
            Ty::Int(it) => {
                if !wide(it) {
                    self.extend(s, it);
                }
                s.i32_const(!it.signed() as i32);
                spec_args(s, spec);
                s.call(import_index("fmt_spec_i64"));
            }
            Ty::Float(ft) => {
                if ft == FloatTy::F32 {
                    s.f64_promote_f32();
                }
                s.i32_const((ft == FloatTy::F32) as i32);
                spec_args(s, spec);
                s.call(import_index("fmt_spec_f64"));
            }
            Ty::Str => {
                spec_args(s, spec);
                s.call(import_index("fmt_spec_str"));
            }
            Ty::Char => {
                spec_args(s, spec);
                s.call(import_index("fmt_spec_char"));
            }
            Ty::Bool => {
                spec_args(s, spec);
                s.call(import_index("fmt_spec_bool"));
            }
            t => return unsupported(format!("a format spec on `{t}`")),
        }
        Ok(())
    }

    /// The debug text function of a type (`${x:?}`): `(value) -> string`.
    fn debug_fn(&mut self, t: &Ty) -> R<u32> {
        use pico_sema::format::{FormatSpec, Kind};
        let t = t.erase();
        let vt = self.ty_or_err(&t)?.expect("a value");
        let key = format!("debug {t}");
        if let Some(&i) = self.extra_keys.get(&key) {
            return Ok(i);
        }
        let debug = FormatSpec {
            kind: Kind::Debug,
            ..FormatSpec::default()
        };
        // What the parts need first (so the closure only emits).
        enum Part {
            Lit(u32, usize),
            /// A value at a field (struct index, field) or element, and its text function.
            Field(u32, u32, Ty),
        }
        match &t {
            Ty::Class(sid, ..) => {
                let name = format!("<{}>", self.p.layout(*sid).name);
                let d = self.string_data(&name);
                return Ok(self.extra_fn(key, vec![vt], vec![str_ref()], move |_| {
                    let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
                    let mut s = f.instructions();
                    s.i32_const(0)
                        .i32_const(name.len() as i32)
                        .array_new_data(STR, d)
                        .end();
                    f
                }));
            }
            Ty::Nullable(inner) => {
                let inner = (**inner).clone();
                let inner_fn = self.part_fn(&inner)?;
                let d = self.string_data("null");
                let unbox = if is_ref_like(&inner) {
                    None
                } else {
                    Some(self.opt_type(&inner)?)
                };
                return Ok(self.extra_fn(key, vec![vt], vec![str_ref()], move |_| {
                    let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
                    let mut s = f.instructions();
                    s.local_get(0).ref_is_null();
                    s.if_(BlockType::Result(str_ref()));
                    s.i32_const(0).i32_const(4).array_new_data(STR, d);
                    s.else_().local_get(0);
                    if let Some(b) = unbox {
                        s.struct_get(b, 0);
                    }
                    s.call(inner_fn).end().end();
                    f
                }));
            }
            Ty::Struct(..) | Ty::Tuple(_) => {
                let (idx, fields) = self.struct_of(&t)?;
                let names: Vec<String> = match &t {
                    Ty::Struct(sid2, ..) => self
                        .p
                        .layout(*sid2)
                        .fields
                        .iter()
                        .map(|(n, _)| n.clone())
                        .collect(),
                    _ => Vec::new(),
                };
                let tuple = matches!(t, Ty::Tuple(_));
                let mut parts = Vec::new();
                let lit = |g: &mut Self, text: String, parts: &mut Vec<Part>| {
                    let d = g.string_data(&text);
                    parts.push(Part::Lit(d, text.len()));
                };
                let sname = match &t {
                    Ty::Struct(sid2, ..) => self.p.layout(*sid2).name.clone(),
                    _ => String::new(),
                };
                if tuple {
                    lit(self, "[".into(), &mut parts);
                } else if fields.is_empty() {
                    lit(self, format!("{sname} {{}}"), &mut parts);
                } else {
                    lit(self, format!("{sname} {{ "), &mut parts);
                }
                for (i, ft) in fields.iter().enumerate() {
                    let sep = match (tuple, i) {
                        (true, 0) => String::new(),
                        (true, _) => ", ".into(),
                        (false, 0) => format!("{}: ", names[i]),
                        (false, _) => format!(", {}: ", names[i]),
                    };
                    if !sep.is_empty() {
                        lit(self, sep, &mut parts);
                    }
                    if !no_value(ft) {
                        parts.push(Part::Field(idx, i as u32, ft.clone()));
                    }
                }
                if tuple {
                    lit(self, "]".into(), &mut parts);
                } else if !fields.is_empty() {
                    lit(self, " }".into(), &mut parts);
                }
                let mut fns = Vec::new();
                for p in &parts {
                    if let Part::Field(_, _, ft) = p {
                        fns.push(self.part_fn(ft)?);
                    }
                }
                let concat = helper_index("str_concat");
                return Ok(self.extra_fn(key, vec![vt], vec![str_ref()], move |_| {
                    let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
                    let mut s = f.instructions();
                    let mut first = true;
                    let mut k = 0;
                    for p in &parts {
                        match p {
                            Part::Lit(d, n) => {
                                s.i32_const(0).i32_const(*n as i32).array_new_data(STR, *d);
                            }
                            Part::Field(idx, i, _) => {
                                s.local_get(0).struct_get(*idx, *i).call(fns[k]);
                                k += 1;
                            }
                        }
                        if !first {
                            s.call(concat);
                        }
                        first = false;
                    }
                    s.end();
                    f
                }));
            }
            Ty::Array(elem) | Ty::Fixed(elem, _) => {
                let elem = (**elem).clone();
                let e = self.elem_storage(&elem)?;
                let raw = self.raw_type(e);
                let grow = match &t {
                    Ty::Array(_) => Some(heap_index(vt).expect("a ref")),
                    _ => None,
                };
                let ef = self.part_fn(&elem)?;
                let open = self.string_data("[");
                let close = self.string_data("]");
                let comma = self.string_data(", ");
                let concat = helper_index("str_concat");
                return Ok(self.extra_fn(key, vec![vt], vec![str_ref()], move |_| {
                    // locals: 1 data, 2 n, 3 i, 4 text
                    let mut f = Function::new_with_locals_types(vec![
                        ref_to(raw),
                        ValType::I32,
                        ValType::I32,
                        str_ref(),
                    ]);
                    let mut s = f.instructions();
                    match grow {
                        Some(g) => {
                            s.local_get(0).struct_get(g, 0).local_set(1);
                            s.local_get(0).struct_get(g, 1).local_set(2);
                        }
                        None => {
                            s.local_get(0).local_set(1);
                            s.local_get(0).array_len().local_set(2);
                        }
                    }
                    s.i32_const(0)
                        .i32_const(1)
                        .array_new_data(STR, open)
                        .local_set(4);
                    s.block(BlockType::Empty).loop_(BlockType::Empty);
                    s.local_get(3).local_get(2).i32_ge_u().br_if(1);
                    s.local_get(3).if_(BlockType::Empty);
                    s.local_get(4)
                        .i32_const(0)
                        .i32_const(2)
                        .array_new_data(STR, comma)
                        .call(concat)
                        .local_set(4);
                    s.end();
                    s.local_get(4)
                        .local_get(1)
                        .local_get(3)
                        .array_get(raw)
                        .call(ef)
                        .call(concat)
                        .local_set(4);
                    s.local_get(3).i32_const(1).i32_add().local_set(3);
                    s.br(0).end().end();
                    s.local_get(4)
                        .i32_const(0)
                        .i32_const(1)
                        .array_new_data(STR, close)
                        .call(concat)
                        .end();
                    f
                }));
            }
            Ty::Enum(eid, _) => {
                let eid = *eid;
                let base = self.enum_type(eid);
                let layout = self.p.enum_layout(eid).clone();
                // Per variant: its literal pieces and fields (`Shape.Circle(r: 1)`).
                let mut per: Vec<(u32, Vec<Part>)> = Vec::new();
                for (v, var) in layout.variants.iter().enumerate() {
                    let vt2 = self.variant_type(eid, v as u32);
                    let mut parts = Vec::new();
                    let lit = |g: &mut Self, text: String, parts: &mut Vec<Part>| {
                        let d = g.string_data(&text);
                        parts.push(Part::Lit(d, text.len()));
                    };
                    if var.fields.is_empty() && !var.has_parens {
                        lit(self, format!("{}.{}", layout.name, var.name), &mut parts);
                    } else {
                        lit(self, format!("{}.{}(", layout.name, var.name), &mut parts);
                        for (i, (fname, ft)) in var.fields.iter().enumerate() {
                            let sep = if i == 0 {
                                format!("{fname}: ")
                            } else {
                                format!(", {fname}: ")
                            };
                            lit(self, sep, &mut parts);
                            if !no_value(ft) {
                                parts.push(Part::Field(vt2, i as u32 + 1, ft.clone()));
                            }
                        }
                        lit(self, ")".into(), &mut parts);
                    }
                    per.push((vt2, parts));
                }
                let mut fns: Vec<Vec<u32>> = Vec::new();
                for (_, parts) in &per {
                    let mut v = Vec::new();
                    for p in parts {
                        if let Part::Field(_, _, ft) = p {
                            v.push(self.part_fn(ft)?);
                        }
                    }
                    fns.push(v);
                }
                let concat = helper_index("str_concat");
                return Ok(self.extra_fn(key, vec![vt], vec![str_ref()], move |_| {
                    let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
                    let mut s = f.instructions();
                    for (v, (_, parts)) in per.iter().enumerate() {
                        s.local_get(0)
                            .struct_get(base, 0)
                            .i32_const(v as i32)
                            .i32_eq();
                        s.if_(BlockType::Empty);
                        let mut first = true;
                        let mut k = 0;
                        for p in parts {
                            match p {
                                Part::Lit(d, n) => {
                                    s.i32_const(0).i32_const(*n as i32).array_new_data(STR, *d);
                                }
                                Part::Field(vt2, i, _) => {
                                    s.local_get(0).ref_cast_non_null(HeapType::Concrete(*vt2));
                                    s.struct_get(*vt2, *i).call(fns[v][k]);
                                    k += 1;
                                }
                            }
                            if !first {
                                s.call(concat);
                            }
                            first = false;
                        }
                        s.return_().end();
                    }
                    s.unreachable().end();
                    f
                }));
            }
            Ty::Range(it) => {
                let it = *it;
                let idx = self.struct_type(&t);
                let dots = self.string_data("..");
                let open = self.string_data("(");
                let mid = self.string_data(").step(");
                let close = self.string_data(")");
                let concat = helper_index("str_concat");
                let w = wide(it);
                return Ok(self.extra_fn(key, vec![vt], vec![str_ref()], move |_| {
                    let mut f = Function::new_with_locals_types(vec![str_ref()]);
                    let mut s = f.instructions();
                    let part = |s: &mut InstructionSink, i: u32| {
                        s.local_get(0).struct_get(idx, i);
                        if !w {
                            if it.signed() {
                                s.i64_extend_i32_s();
                            } else {
                                s.i64_extend_i32_u();
                            }
                        }
                        s.i32_const(!it.signed() as i32)
                            .call(import_index("fmt_i64"));
                    };
                    part(&mut s, 0);
                    s.i32_const(0)
                        .i32_const(2)
                        .array_new_data(STR, dots)
                        .call(concat);
                    part(&mut s, 1);
                    s.call(concat).local_set(1);
                    // A step other than 1: `(a..b).step(s)`.
                    s.local_get(0).struct_get(idx, 2);
                    if w {
                        s.i64_const(1).i64_eq();
                    } else {
                        s.i32_const(1).i32_eq();
                    }
                    s.if_(BlockType::Result(str_ref()));
                    s.local_get(1);
                    s.else_();
                    s.i32_const(0)
                        .i32_const(1)
                        .array_new_data(STR, open)
                        .local_get(1)
                        .call(concat);
                    s.i32_const(0)
                        .i32_const(7)
                        .array_new_data(STR, mid)
                        .call(concat);
                    part(&mut s, 2);
                    s.call(concat);
                    s.i32_const(0)
                        .i32_const(1)
                        .array_new_data(STR, close)
                        .call(concat);
                    s.end().end();
                    f
                }));
            }
            _ => {}
        }
        // A primitive: its debug format.
        let _ = &debug;
        let prim = t.clone();
        match prim {
            Ty::Int(_) | Ty::Float(_) | Ty::Str | Ty::Char | Ty::Bool => {}
            t => return unsupported(format!("the debug form of `{t}`")),
        }
        let mut body = Vec::new();
        {
            let mut sink = InstructionSink::new(&mut body);
            sink.local_get(0);
            self.format_prim(&mut sink, &prim, &debug)?;
            sink.end();
        }
        Ok(self.extra_fn(key, vec![vt], vec![str_ref()], move |_| {
            let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
            f.raw(body);
            f
        }))
    }

    /// Reads field `i` that every variant of enum `eid` has, whatever the variant.
    fn shared_field_fn(&mut self, eid: EnumId, i: u32, t: &Ty) -> R<u32> {
        let et = ref_to(self.enum_type(eid));
        let rt = self.ty_or_err(t)?.expect("a value");
        let base = self.enum_type(eid);
        let n = self.p.enum_layout(eid).variants.len() as u32;
        let vts: Vec<u32> = (0..n).map(|v| self.variant_type(eid, v)).collect();
        Ok(self.extra_fn(
            format!("shared field {}.{i}", eid.0),
            vec![et],
            vec![rt],
            move |_| {
                let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
                let mut s = f.instructions();
                for (v, vt) in vts.iter().enumerate() {
                    s.local_get(0)
                        .struct_get(base, 0)
                        .i32_const(v as i32)
                        .i32_eq();
                    s.if_(BlockType::Empty);
                    s.local_get(0)
                        .ref_cast_non_null(HeapType::Concrete(*vt))
                        .struct_get(*vt, i + 1);
                    s.return_().end();
                }
                s.unreachable().end();
                f
            },
        ))
    }

    /// Copies a value aggregate's parts into another of its type: `(dst, src)` (assigning
    /// through a reference to a struct: the struct itself changes).
    fn assign_fn(&mut self, t: &Ty) -> R<u32> {
        let t = t.erase();
        let vt = self.ty_or_err(&t)?.expect("a value");
        let key = format!("assign {t}");
        if let Ty::Fixed(elem, _) = &t {
            let e = self.elem_storage(elem)?;
            let raw = self.raw_type(e);
            return Ok(self.extra_fn(key, vec![vt, vt], vec![], move |_| {
                let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
                let mut s = f.instructions();
                s.local_get(0)
                    .i32_const(0)
                    .local_get(1)
                    .i32_const(0)
                    .local_get(1)
                    .array_len();
                s.array_copy(raw, raw).end();
                f
            }));
        }
        let (idx, fields) = self.struct_of(&t)?;
        let n = fields.len() as u32;
        Ok(self.extra_fn(key, vec![vt, vt], vec![], move |_| {
            let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
            let mut s = f.instructions();
            for i in 0..n {
                s.local_get(0)
                    .local_get(1)
                    .struct_get(idx, i)
                    .struct_set(idx, i);
            }
            s.end();
            f
        }))
    }

    /// Value equality (`==`) of a type: `(a, b) -> i32`.
    fn eq_fn(&mut self, t: &Ty) -> R<u32> {
        let t = t.erase();
        let vt = self.ty_or_err(&t)?.expect("a value");
        let key = format!("eq {t}");
        if let Some(&i) = self.extra_keys.get(&key) {
            return Ok(i);
        }
        // How two parts compare: an instruction or a function.
        #[derive(Clone, Copy)]
        enum Cmp {
            I32,
            I64,
            F32,
            F64,
            Ref,
            Call(u32),
        }
        let cmp_of = |g: &mut Self, part: &Ty| -> R<Cmp> {
            Ok(match part.erase() {
                Ty::Int(it) if wide(it) => Cmp::I64,
                Ty::Int(_) | Ty::Bool | Ty::Char => Cmp::I32,
                Ty::Float(FloatTy::F32) => Cmp::F32,
                Ty::Float(_) => Cmp::F64,
                Ty::Str => Cmp::Call(helper_index("str_eq")),
                Ty::Class(..) => Cmp::Ref,
                p => Cmp::Call(g.eq_fn(&p)?),
            })
        };
        let emit_cmp = |s: &mut InstructionSink, c: Cmp| {
            match c {
                Cmp::I32 => s.i32_eq(),
                Cmp::I64 => s.i64_eq(),
                Cmp::F32 => s.f32_eq(),
                Cmp::F64 => s.f64_eq(),
                Cmp::Ref => s.ref_eq(),
                Cmp::Call(f) => s.call(f),
            };
        };
        match &t {
            Ty::Nullable(inner) => {
                let inner = (**inner).clone();
                let c = cmp_of(self, &inner)?;
                let unbox = if is_ref_like(&inner) {
                    None
                } else {
                    Some(self.opt_type(&inner)?)
                };
                Ok(
                    self.extra_fn(key, vec![vt, vt], vec![ValType::I32], move |_| {
                        let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
                        let mut s = f.instructions();
                        // Both null: equal; one null: not.
                        s.local_get(0)
                            .ref_is_null()
                            .local_get(1)
                            .ref_is_null()
                            .i32_or();
                        s.if_(BlockType::Empty);
                        s.local_get(0)
                            .ref_is_null()
                            .local_get(1)
                            .ref_is_null()
                            .i32_and()
                            .return_();
                        s.end();
                        for i in 0..2 {
                            s.local_get(i);
                            if let Some(b) = unbox {
                                s.struct_get(b, 0);
                            }
                        }
                        emit_cmp(&mut s, c);
                        s.end();
                        f
                    }),
                )
            }
            Ty::Fixed(elem, _) => {
                let elem = (**elem).clone();
                let e = self.elem_storage(&elem)?;
                let raw = self.raw_type(e);
                let c = cmp_of(self, &elem)?;
                Ok(
                    self.extra_fn(key, vec![vt, vt], vec![ValType::I32], move |_| {
                        // locals: 2 i, 3 n
                        let mut f =
                            Function::new_with_locals_types(vec![ValType::I32, ValType::I32]);
                        let mut s = f.instructions();
                        s.local_get(0)
                            .array_len()
                            .local_tee(3)
                            .local_get(1)
                            .array_len()
                            .i32_ne();
                        s.if_(BlockType::Empty).i32_const(0).return_().end();
                        s.block(BlockType::Empty).loop_(BlockType::Empty);
                        s.local_get(2).local_get(3).i32_ge_u().br_if(1);
                        s.local_get(0).local_get(2).array_get(raw);
                        s.local_get(1).local_get(2).array_get(raw);
                        emit_cmp(&mut s, c);
                        s.i32_eqz()
                            .if_(BlockType::Empty)
                            .i32_const(0)
                            .return_()
                            .end();
                        s.local_get(2).i32_const(1).i32_add().local_set(2);
                        s.br(0).end().end();
                        s.i32_const(1).end();
                        f
                    }),
                )
            }
            Ty::Struct(..) | Ty::Tuple(_) | Ty::Range(_) => {
                let (idx, fields) = self.struct_of(&t)?;
                let mut cmps = Vec::new();
                for (i, ft) in fields.iter().enumerate() {
                    if !no_value(ft) {
                        cmps.push((i as u32, cmp_of(self, ft)?));
                    }
                }
                Ok(
                    self.extra_fn(key, vec![vt, vt], vec![ValType::I32], move |_| {
                        let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
                        let mut s = f.instructions();
                        for (i, c) in &cmps {
                            s.local_get(0).struct_get(idx, *i);
                            s.local_get(1).struct_get(idx, *i);
                            emit_cmp(&mut s, *c);
                            s.i32_eqz()
                                .if_(BlockType::Empty)
                                .i32_const(0)
                                .return_()
                                .end();
                        }
                        s.i32_const(1).end();
                        f
                    }),
                )
            }
            Ty::Enum(eid, _) => {
                let eid = *eid;
                let base = self.enum_type(eid);
                let nv = self.p.enum_layout(eid).variants.len();
                let mut per: Vec<(u32, Vec<(u32, Cmp)>)> = Vec::new();
                for v in 0..nv {
                    let vt2 = self.variant_type(eid, v as u32);
                    let fts: Vec<Ty> = self.p.enum_layout(eid).variants[v]
                        .fields
                        .iter()
                        .map(|(_, t)| t.clone())
                        .collect();
                    let mut cs = Vec::new();
                    for (i, ft) in fts.iter().enumerate() {
                        if !no_value(ft) {
                            cs.push((i as u32 + 1, cmp_of(self, ft)?));
                        }
                    }
                    per.push((vt2, cs));
                }
                Ok(
                    self.extra_fn(key, vec![vt, vt], vec![ValType::I32], move |_| {
                        let mut f = Function::new_with_locals_types(Vec::<ValType>::new());
                        let mut s = f.instructions();
                        s.local_get(0)
                            .struct_get(base, 0)
                            .local_get(1)
                            .struct_get(base, 0)
                            .i32_ne();
                        s.if_(BlockType::Empty).i32_const(0).return_().end();
                        for (v, (vt2, cs)) in per.iter().enumerate() {
                            if cs.is_empty() {
                                continue;
                            }
                            s.local_get(0)
                                .struct_get(base, 0)
                                .i32_const(v as i32)
                                .i32_eq();
                            s.if_(BlockType::Empty);
                            for (i, c) in cs {
                                s.local_get(0).ref_cast_non_null(HeapType::Concrete(*vt2));
                                s.struct_get(*vt2, *i);
                                s.local_get(1).ref_cast_non_null(HeapType::Concrete(*vt2));
                                s.struct_get(*vt2, *i);
                                emit_cmp(&mut s, *c);
                                s.i32_eqz()
                                    .if_(BlockType::Empty)
                                    .i32_const(0)
                                    .return_()
                                    .end();
                            }
                            s.end();
                        }
                        s.i32_const(1).end();
                        f
                    }),
                )
            }
            t => unsupported(format!("`==` on `{t}`")),
        }
    }

    /// The text function of a part of a compound value in its debug form.
    fn part_fn(&mut self, t: &Ty) -> R<u32> {
        self.debug_fn(t)
    }

    /// The string form of a primitive on the stack.
    fn string_form(&mut self, s: &mut InstructionSink, t: &Ty) -> R<()> {
        match t.erase() {
            Ty::Int(it) => {
                if !wide(it) {
                    self.extend(s, it);
                }
                s.i32_const(!it.signed() as i32);
                s.call(import_index("fmt_i64"));
            }
            Ty::Float(FloatTy::F32) => {
                s.call(import_index("fmt_f32"));
            }
            Ty::Float(_) => {
                s.call(import_index("fmt_f64"));
            }
            Ty::Char => {
                s.call(import_index("fmt_char"));
            }
            Ty::Bool => {
                let t = self.string_data("true");
                let f = self.string_data("false");
                s.if_(BlockType::Result(str_ref()));
                s.i32_const(0);
                s.i32_const(4);
                s.array_new_data(STR, t);
                s.else_();
                s.i32_const(0);
                s.i32_const(5);
                s.array_new_data(STR, f);
                s.end();
            }
            Ty::Str => {}
            t => return unsupported(format!("the string form of `{t}`")),
        }
        Ok(())
    }

    fn print(&mut self, s: &mut InstructionSink, t: &Ty) -> R<()> {
        match t.erase() {
            Ty::Int(it) => {
                if !wide(it) {
                    self.extend(s, it);
                }
                s.i32_const(!it.signed() as i32);
                s.call(import_index("print_i64"));
            }
            Ty::Float(FloatTy::F32) => {
                s.call(import_index("print_f32"));
            }
            Ty::Float(_) => {
                s.call(import_index("print_f64"));
            }
            Ty::Char => {
                s.call(import_index("print_char"));
            }
            Ty::Bool | Ty::Str => {
                self.string_form(s, t)?;
                s.call(import_index("print_str"));
            }
            t => return unsupported(format!("printing `{t}`")),
        }
        Ok(())
    }

    // ------------------------------------------------------------ terminators

    fn goto(&self, fx: &FnCx, s: &mut InstructionSink, b: mir::BlockId, to_loop: u32) {
        s.i32_const(b.0 as i32);
        s.local_set(fx.pc);
        s.br(to_loop);
    }

    fn terminator(
        &mut self,
        fx: &mut FnCx,
        s: &mut InstructionSink,
        t: &Terminator,
        to_loop: u32,
    ) -> R<()> {
        match t {
            Terminator::Goto(b) => self.goto(fx, s, *b, to_loop),
            Terminator::Branch { cond, then, else_ } => {
                self.operand(fx, s, cond)?;
                s.if_(BlockType::Empty);
                s.i32_const(then.0 as i32);
                s.local_set(fx.pc);
                s.else_();
                s.i32_const(else_.0 as i32);
                s.local_set(fx.pc);
                s.end();
                s.br(to_loop);
            }
            Terminator::Return(v) => {
                if let Some(v) = v {
                    let ret = fx.f.ret.clone();
                    self.operand_to(fx, s, v, &ret)?;
                }
                s.return_();
            }
            Terminator::Call {
                callee,
                args,
                dest,
                next,
                loc,
            } => {
                match callee {
                    Callee::Fn(f) => {
                        if self.p.externs.iter().any(|(e, _)| e == f) {
                            return unsupported("extern \"C\" functions");
                        }
                        let callee_fn = &self.p.fns[f.0 as usize];
                        for (a, p) in args.iter().zip(&callee_fn.params) {
                            let pt = callee_fn.local_ty(*p).clone();
                            if no_value(&pt) {
                                continue;
                            }
                            self.operand_to(fx, s, a, &pt)?;
                        }
                        let Some(&idx) = self.fn_index.get(f) else {
                            return unsupported("a call of a dropped function");
                        };
                        s.call(idx);
                        self.call_result(fx, s, dest.as_ref(), &callee_fn.ret.clone())?;
                    }
                    Callee::Builtin(b @ (Builtin::Print | Builtin::Println)) => {
                        for a in args {
                            let at = self.operand_ty(fx, a)?;
                            if no_value(&at) {
                                continue;
                            }
                            self.operand(fx, s, a)?;
                            self.print(s, &at)?;
                        }
                        if *b == Builtin::Println {
                            s.call(import_index("print_nl"));
                        }
                    }
                    Callee::Builtin(
                        Builtin::GcCollect
                        | Builtin::GcCollectYoung
                        | Builtin::GcPause
                        | Builtin::GcResume,
                    ) => {}
                    Callee::Builtin(Builtin::ClockMs) => {
                        s.call(import_index("clock_ms"));
                        self.call_result(fx, s, dest.as_ref(), &Ty::Int(IntTy::I64))?;
                    }
                    Callee::Builtin(Builtin::PanicTaken) => {
                        s.global_get(self.panic_global());
                        self.call_result(fx, s, dest.as_ref(), &Ty::Str)?;
                    }
                    Callee::Builtin(Builtin::LoopWait) => {}
                    Callee::Builtin(Builtin::GcStat) => {
                        s.i64_const(0);
                        self.call_result(fx, s, dest.as_ref(), &Ty::Int(IntTy::I64))?;
                    }
                    Callee::Builtin(Builtin::GcDumpRoots) => {}
                    Callee::Builtin(Builtin::WorkerOp) => {
                        // No workers here: no messages, no live workers, not a worker.
                        let op = match args.first() {
                            Some(Operand::Const(Const::Int(op, _))) => *op,
                            _ => -1,
                        };
                        match op {
                            2 => s.i64_const(-1),
                            // (Ops on a worker or a message can't happen without them.)
                            _ => s.i64_const(0),
                        };
                        self.call_result(fx, s, dest.as_ref(), &Ty::Int(IntTy::I64))?;
                    }
                    Callee::Builtin(Builtin::Io) => {
                        let op = match args.first() {
                            Some(Operand::Const(Const::Int(op, _))) => *op,
                            _ => -1,
                        };
                        match op {
                            // The loop is about to wait: the host waits instead.
                            138 => {
                                self.operand(fx, s, &args[1])?;
                                s.global_set(self.wait_global());
                                s.i64_const(1);
                            }
                            // No I/O jobs.
                            100 => {
                                s.i64_const(-1);
                            }
                            101 | 104 => {
                                s.i64_const(0);
                            }
                            // Clocks (ns): the wall clock, or since the program started.
                            133 => {
                                self.operand(fx, s, &args[1])?;
                                s.i32_wrap_i64();
                                s.call(import_index("clock_ns"));
                            }
                            // A job's results (always job 1: see `job_global`).
                            102 | 103 => {
                                s.global_get(self.job_global() + (op - 102) as u32);
                            }
                            // std/js: through the glue.
                            163 => {
                                self.operand(fx, s, &args[1])?;
                                s.call(import_index("js_text"));
                                s.global_set(self.job_global() + 2);
                                s.i64_const(1);
                            }
                            150..=168 => {
                                s.i32_const(op as i32);
                                for a in &args[1..6] {
                                    self.operand(fx, s, a)?;
                                }
                                s.call(import_index("js_op"));
                            }
                            130 | 131 | 132 | 105 | 114 | 134 | 135 | 136 | 137 => {
                                self.io_job(fx, s, op, args)?;
                                s.i64_const(1);
                            }
                            op => return unsupported(format!("I/O operation {op} (`__io`)")),
                        }
                        self.call_result(fx, s, dest.as_ref(), &Ty::Int(IntTy::I64))?;
                    }
                    Callee::Builtin(Builtin::IoText) => {
                        s.global_get(self.job_global() + 2);
                        self.call_result(fx, s, dest.as_ref(), &Ty::Str)?;
                    }
                    Callee::Builtin(Builtin::BufOp) => {
                        let f = self.buf_fn()?;
                        for a in args.iter().take(4) {
                            self.operand(fx, s, a)?;
                        }
                        s.call(f);
                        self.call_result(fx, s, dest.as_ref(), &Ty::Int(IntTy::I64))?;
                    }
                    Callee::Builtin(Builtin::Math) => {
                        // `__math(op, x, y)`: the runtime's op table (`pico_math`), mostly in
                        // wasm, the rest through JavaScript's `Math`.
                        let op = match args.first() {
                            Some(Operand::Const(Const::Int(op, _))) => Some(*op),
                            _ => None,
                        };
                        let native = |s: &mut InstructionSink, op: i128| {
                            match op {
                                0 => s.f64_sqrt(),
                                13 => s.f64_floor(),
                                14 => s.f64_ceil(),
                                _ => s.f64_trunc(),
                            };
                        };
                        self.operand(fx, s, &args[1])?;
                        match op {
                            Some(o) if matches!(o, 0 | 13 | 14 | 15) => {
                                native(s, o);
                            }
                            Some(27) => {
                                s.f32_demote_f64().f64_promote_f32();
                            }
                            _ => {
                                let tmp = fx.scratch(ValType::F64);
                                s.local_set(tmp);
                                self.operand(fx, s, &args[0])?;
                                s.i32_wrap_i64();
                                s.local_get(tmp);
                                self.operand(fx, s, &args[2])?;
                                s.call(import_index("math"));
                            }
                        }
                        self.call_result(fx, s, dest.as_ref(), &Ty::Float(FloatTy::F64))?;
                    }
                    Callee::Builtin(Builtin::ParseF64) => {
                        self.operand(fx, s, &args[0])?;
                        s.call(import_index("parse_f64"));
                        self.call_result(fx, s, dest.as_ref(), &Ty::Float(FloatTy::F64))?;
                    }
                    Callee::Builtin(Builtin::RefId) => {
                        self.operand(fx, s, &args[0])?;
                        s.call(import_index("ref_id"));
                        self.call_result(fx, s, dest.as_ref(), &Ty::Int(IntTy::I64))?;
                    }
                    Callee::Builtin(b) => {
                        return unsupported(format!("the builtin `{}`", b.name()));
                    }
                    Callee::Virtual { slot, decl } => {
                        let decl_fn = &self.p.fns[decl.0 as usize];
                        let recv_ty = self.operand_ty(fx, &args[0])?;
                        let root_ty = self.root_class(&recv_ty.erase());
                        let (cls, _) = self.struct_of(&root_ty)?;
                        let Ty::Class(sid, ..) = recv_ty.erase() else {
                            return unsupported(format!("a virtual call on `{recv_ty}`"));
                        };
                        let root = self.slot_root(sid, *slot);
                        let (rps, rrs) = self.fn_sig(root)?;
                        let sig = self.func_type(rps, rrs.clone());
                        let recv = self.ref_scratch(fx, cls);
                        self.operand(fx, s, &args[0])?;
                        s.local_set(recv);
                        s.local_get(recv);
                        for (a, p) in args.iter().zip(&decl_fn.params).skip(1) {
                            if no_value(decl_fn.local_ty(*p)) {
                                continue;
                            }
                            self.operand(fx, s, a)?;
                        }
                        s.local_get(recv);
                        s.struct_get(cls, 0);
                        s.i32_const(*slot as i32);
                        s.array_get(self.vt_type());
                        s.ref_cast_non_null(HeapType::Concrete(sig));
                        s.call_ref(sig);
                        // (The root may return a base class of what this override does.)
                        let ret = decl_fn.ret.clone();
                        if let (Some(ValType::Ref(want)), Some(got)) =
                            (self.wasm_ty(&ret), rrs.first())
                            && ValType::Ref(want) != *got
                        {
                            s.ref_cast_nullable(want.heap_type);
                        }
                        self.call_result(fx, s, dest.as_ref(), &ret)?;
                    }
                    Callee::Indirect => {
                        let ft = self.operand_ty(fx, &args[0])?;
                        let Ty::Fn(ps, ret, _) = ft.erase() else {
                            return unsupported(format!("calling `{ft}`"));
                        };
                        let clo = self
                            .wasm_ty(&ft)
                            .and_then(heap_index)
                            .expect("a function value");
                        let sig = self.closure_sig(&ps, &ret).expect("its signature");
                        let tmp = self.ref_scratch(fx, clo);
                        self.operand(fx, s, &args[0])?;
                        s.local_tee(tmp);
                        s.struct_get(clo, 1);
                        for (a, pt) in args.iter().skip(1).zip(ps.iter()) {
                            if no_value(pt) {
                                continue;
                            }
                            self.operand(fx, s, a)?;
                        }
                        s.local_get(tmp);
                        s.struct_get(clo, 0);
                        s.call_ref(sig);
                        self.call_result(fx, s, dest.as_ref(), &ret)?;
                    }
                    Callee::Iface { slot, decl } => {
                        let decl_fn = &self.p.fns[decl.0 as usize];
                        let (ps, rs) = self.fn_sig(*decl)?;
                        let sig = self.func_type(ps, rs);
                        let it = self.register(Key::Iface, |_| Def::Iface);
                        let tmp = self.ref_scratch(fx, it);
                        self.operand(fx, s, &args[0])?;
                        s.local_tee(tmp);
                        for (a, p) in args.iter().zip(&decl_fn.params).skip(1) {
                            if no_value(decl_fn.local_ty(*p)) {
                                continue;
                            }
                            self.operand(fx, s, a)?;
                        }
                        s.local_get(tmp);
                        s.struct_get(it, 1);
                        s.i32_const(*slot as i32);
                        s.i32_add();
                        s.call_indirect(0, sig);
                        let ret = decl_fn.ret.clone();
                        self.call_result(fx, s, dest.as_ref(), &ret)?;
                    }
                }
                let _ = loc;
                self.goto(fx, s, *next, to_loop);
            }
            Terminator::TailCall { callee, args, .. } => {
                let callee_fn = &self.p.fns[callee.0 as usize];
                for (a, p) in args.iter().zip(&callee_fn.params) {
                    if no_value(callee_fn.local_ty(*p)) {
                        continue;
                    }
                    self.operand(fx, s, a)?;
                }
                s.return_call(self.fn_index[callee]);
            }
            Terminator::Panic { msg, loc } => {
                self.operand(fx, s, msg)?;
                self.throw_panic(s, *loc);
            }
            Terminator::Unreachable => {
                s.unreachable();
            }
            Terminator::Guard { body, handler } => {
                // The handler goes on this frame's guard stack (see `function`).
                let (stack, depth) = fx.guards.expect("a function with guards");
                s.local_get(stack)
                    .local_get(depth)
                    .i32_const(handler.0 as i32);
                s.array_set(self.raw_type(ValType::I32));
                s.local_get(depth).i32_const(1).i32_add().local_set(depth);
                self.goto(fx, s, *body, to_loop);
            }
            Terminator::Unguard(next) => {
                let (_, depth) = fx.guards.expect("a function with guards");
                s.local_get(depth).i32_const(1).i32_sub().local_set(depth);
                self.goto(fx, s, *next, to_loop);
            }
            Terminator::Resume => {
                // The panic goes on (its message is still in the globals).
                s.throw(0);
            }
            Terminator::Uncaught { class, msg } => {
                self.operand(fx, s, class)?;
                self.operand(fx, s, msg)?;
                s.call(import_index("uncaught"));
                s.unreachable();
            }
        }
        Ok(())
    }

    fn call_result(
        &mut self,
        fx: &mut FnCx,
        s: &mut InstructionSink,
        dest: Option<&Place>,
        ret: &Ty,
    ) -> R<()> {
        if no_value(ret) {
            return Ok(());
        }
        if let Some(d) = dest {
            let to = match (&d.base, d.proj.is_empty()) {
                (mir::Base::Local(l), true) => fx.local_types[l.0 as usize],
                _ => {
                    let dt = self.place_ty(fx, d)?;
                    self.wasm_ty(&dt)
                }
            };
            self.coerce(s, self.wasm_ty(ret), to);
        }
        match dest {
            Some(d) => {
                // (The value is on the stack: store it, through a scratch local for fields.)
                if d.proj.is_empty() && !matches!(d.base, mir::Base::Deref(_)) {
                    match d.base {
                        mir::Base::Local(l) => match fx.locals[l.0 as usize] {
                            Some(slot) => {
                                s.local_set(slot.index);
                            }
                            None => {
                                s.drop();
                            }
                        },
                        mir::Base::Global(g) => {
                            s.global_set(g.0);
                        }
                        mir::Base::Deref(_) => unreachable!(),
                    }
                } else {
                    let t = self.wasm_ty(ret).expect("checked");
                    let tmp = match t {
                        ValType::Ref(RefType {
                            heap_type: HeapType::Concrete(i),
                            ..
                        }) if i != STR => self.ref_scratch(fx, i),
                        t => fx.scratch(t),
                    };
                    s.local_set(tmp);
                    self.store(fx, s, d, |_, _, s| {
                        s.local_get(tmp);
                        Ok(())
                    })?;
                }
            }
            None => {
                s.drop();
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ helpers

    fn helpers(&self, code: &mut CodeSection) {
        use ValType::*;
        for h in HELPERS {
            let mut f = Function::new_with_locals_types(match *h {
                "str_concat" => vec![str_ref(), I32, I32],
                "str_eq" => vec![I32, I32],
                "str_chars" => vec![I32, I64, I32],
                "str_cmp" => vec![I32, I32, I32],
                _ => vec![],
            });
            let mut s = f.instructions();
            match *h {
                "str_len" => {
                    s.local_get(0).array_len();
                }
                "str_byte" => {
                    s.local_get(0).local_get(1).array_get_u(STR);
                }
                "str_new" => {
                    s.local_get(0).array_new_default(STR);
                }
                "str_put" => {
                    s.local_get(0).local_get(1).local_get(2).array_set(STR);
                }
                "str_concat" => {
                    // locals: 2 the result, 3 len(a), 4 len(b)
                    s.local_get(0).array_len().local_set(3);
                    s.local_get(1).array_len().local_set(4);
                    s.local_get(3)
                        .local_get(4)
                        .i32_add()
                        .array_new_default(STR)
                        .local_set(2);
                    s.local_get(2)
                        .i32_const(0)
                        .local_get(0)
                        .i32_const(0)
                        .local_get(3);
                    s.array_copy(STR, STR);
                    s.local_get(2)
                        .local_get(3)
                        .local_get(1)
                        .i32_const(0)
                        .local_get(4);
                    s.array_copy(STR, STR);
                    s.local_get(2);
                }
                "str_eq" => {
                    // locals: 2 i, 3 n
                    s.local_get(0).array_len().local_tee(3);
                    s.local_get(1).array_len().i32_ne();
                    s.if_(BlockType::Empty).i32_const(0).return_().end();
                    s.block(BlockType::Empty).loop_(BlockType::Empty);
                    s.local_get(2).local_get(3).i32_ge_u().br_if(1);
                    s.local_get(0).local_get(2).array_get_u(STR);
                    s.local_get(1).local_get(2).array_get_u(STR);
                    s.i32_ne()
                        .if_(BlockType::Empty)
                        .i32_const(0)
                        .return_()
                        .end();
                    s.local_get(2).i32_const(1).i32_add().local_set(2);
                    s.br(0).end().end();
                    s.i32_const(1);
                }
                "str_chars" => {
                    // Code points: bytes that aren't continuation bytes. locals: 1 i, 2 n, 3 len
                    s.local_get(0).array_len().local_set(3);
                    s.block(BlockType::Empty).loop_(BlockType::Empty);
                    s.local_get(1).local_get(3).i32_ge_u().br_if(1);
                    s.local_get(0).local_get(1).array_get_u(STR);
                    s.i32_const(0xc0).i32_and().i32_const(0x80).i32_ne();
                    s.i64_extend_i32_u().local_get(2).i64_add().local_set(2);
                    s.local_get(1).i32_const(1).i32_add().local_set(1);
                    s.br(0).end().end();
                    s.local_get(2);
                }
                "str_cmp" => {
                    // Byte order is code point order in UTF-8: -1, 0 or 1. locals: 2 i, 3 n
                    s.local_get(0).array_len().local_get(1).array_len();
                    s.local_get(0)
                        .array_len()
                        .local_get(1)
                        .array_len()
                        .i32_lt_u()
                        .select()
                        .local_set(3);
                    s.block(BlockType::Empty).loop_(BlockType::Empty);
                    s.local_get(2).local_get(3).i32_ge_u().br_if(1);
                    s.local_get(0).local_get(2).array_get_u(STR);
                    s.local_get(1).local_get(2).array_get_u(STR);
                    s.i32_sub().local_tee(4).if_(BlockType::Empty);
                    s.i32_const(-1)
                        .i32_const(1)
                        .local_get(4)
                        .i32_const(0)
                        .i32_lt_s()
                        .select()
                        .return_();
                    s.end();
                    s.local_get(2).i32_const(1).i32_add().local_set(2);
                    s.br(0).end().end();
                    s.local_get(0)
                        .array_len()
                        .local_get(1)
                        .array_len()
                        .i32_sub();
                    s.local_tee(4).i32_const(0).i32_ne();
                    s.i32_const(-1)
                        .i32_const(1)
                        .local_get(4)
                        .i32_const(0)
                        .i32_lt_s()
                        .select();
                    s.i32_mul();
                }
                "mul_ovf_s" => {
                    // a == 0: no; a == -1: b == MIN; else (a * b) / a != b.
                    s.local_get(0).i64_eqz();
                    s.if_(BlockType::Empty).i32_const(0).return_().end();
                    s.local_get(0).i64_const(-1).i64_eq();
                    s.if_(BlockType::Empty)
                        .local_get(1)
                        .i64_const(i64::MIN)
                        .i64_eq()
                        .return_()
                        .end();
                    s.local_get(0)
                        .local_get(1)
                        .i64_mul()
                        .local_get(0)
                        .i64_div_s();
                    s.local_get(1).i64_ne();
                }
                "mul_ovf_u" => {
                    s.local_get(0).i64_eqz();
                    s.if_(BlockType::Empty).i32_const(0).return_().end();
                    s.local_get(0)
                        .local_get(1)
                        .i64_mul()
                        .local_get(0)
                        .i64_div_u();
                    s.local_get(1).i64_ne();
                }
                _ => unreachable!(),
            }
            s.end();
            code.function(&f);
        }
    }
}

/// Pushes a format spec's eight fields (see the `fmt_spec_*` imports).
fn spec_args(s: &mut InstructionSink, spec: &pico_sema::format::FormatSpec) {
    use pico_sema::format::{Align, Kind, Sign};
    let fields = [
        spec.fill as i32,
        match spec.align {
            None => 0,
            Some(Align::Left) => 1,
            Some(Align::Right) => 2,
            Some(Align::Center) => 3,
        },
        match spec.sign {
            Sign::Minus => 0,
            Sign::Plus => 1,
            Sign::Space => 2,
        },
        spec.alternate as i32,
        spec.zero as i32,
        spec.width.map_or(-1, |w| w as i32),
        spec.precision.map_or(-1, |p| p as i32),
        match spec.kind {
            Kind::Default => 0,
            Kind::Dec => 1,
            Kind::Hex => 2,
            Kind::HexUpper => 3,
            Kind::Oct => 4,
            Kind::Bin => 5,
            Kind::Exp => 6,
            Kind::ExpUpper => 7,
            Kind::Fixed => 8,
            Kind::Percent => 9,
            Kind::Debug => 10,
        },
    ];
    for v in fields {
        s.i32_const(v);
    }
}

/// How a projection ends: a struct field, an array element, or nothing (a nullable
/// reference's payload is the reference).
enum Access {
    Field(u32, u32),
    Elem(u32),
    Same,
    /// Read through a function (a field every variant of a union has: its discriminant).
    Getter(u32),
}

/// The type index of a concrete reference type.
fn heap_index(v: ValType) -> Option<u32> {
    match v {
        ValType::Ref(RefType {
            heap_type: HeapType::Concrete(i),
            ..
        }) => Some(i),
        _ => None,
    }
}

/// Types held by reference here, whose `T?` is a nullable reference (not a box).
fn is_ref_like(t: &Ty) -> bool {
    matches!(
        t.erase(),
        Ty::Str
            | Ty::Struct(..)
            | Ty::Class(..)
            | Ty::Array(_)
            | Ty::Fixed(..)
            | Ty::Tuple(_)
            | Ty::Range(_)
            | Ty::Enum(..)
            | Ty::Iface(..)
            | Ty::Fn(..)
    )
}

/// Types without a value (nothing to store or pass).
fn no_value(t: &Ty) -> bool {
    matches!(t.erase(), Ty::Void | Ty::Never)
}

#[allow(dead_code)]
fn struct_id(t: &Ty) -> Option<StructId> {
    match t {
        Ty::Struct(s, ..) | Ty::Class(s, ..) => Some(*s),
        _ => None,
    }
}
