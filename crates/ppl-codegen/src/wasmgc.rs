//! The WasmGC backend (P8.7, decision 264): a WebAssembly module emitted straight from MIR,
//! whose ppl values are WasmGC values, so objects live on the browser's garbage-collected
//! heap instead of the ppl GC's linear memory. LLVM can't emit WasmGC, hence a backend of its
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
//! - The glue (`glue_gc.js`) prints, formats numbers (ppl's float format is JavaScript's) and
//!   reports panics; it reads strings through the module's `str_*` exports.

use ppl_mir::mir::{
    self, BinOp, Callee, Const, Operand, Place, Program, Proj, Rvalue, Terminator, UnOp,
};
use ppl_sema::hir::{Builtin, FnId, StructId};
use ppl_sema::{FloatTy, IntTy, Ty};
use std::collections::HashMap;
use wasm_encoder::{
    BlockType, CodeSection, CompositeInnerType, CompositeType, ConstExpr, DataCountSection,
    DataSection, EntityType, ExportKind, ExportSection, FieldType, Function, FunctionSection,
    GlobalSection, GlobalType, HeapType, Ieee32, Ieee64, ImportSection, InstructionSink, Module,
    RefType, StorageType, StructType, SubType, TypeSection, ValType,
};

type R<T> = Result<T, String>;

fn unsupported<T>(what: impl std::fmt::Display) -> R<T> {
    Err(format!("not supported by the WasmGC backend yet: {what}"))
}

/// The marker the golden harness looks for (such programs are pending, not failing).
pub const UNSUPPORTED: &str = "not supported by the WasmGC backend yet";

/// Placeholder for values of types this backend can't represent yet.
const ANYREF: ValType = ValType::Ref(RefType::ANYREF);

/// The string type's index (the first type of the rec group).
const STR: u32 = 0;

/// The JavaScript imports, in order (module `ppl`).
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
    ("pow_f64", &[ValT::F64, ValT::F64], &[ValT::F64]),
    ("clock_ms", &[], &[ValT::I64]),
];

/// The module's own helpers (after the imports), with their exports.
const HELPERS: &[&str] = &[
    "str_len",
    "str_byte",
    "str_new",
    "str_put",
    "str_concat",
    "str_eq",
    "str_chars",
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
    }
}

/// Emits the module for `p`.
pub fn emit(p: &Program) -> R<Vec<u8>> {
    let live = mir::live_functions(p);
    let mut g = Gen {
        p,
        structs: Vec::new(),
        struct_index: HashMap::new(),
        data: Vec::new(),
        fn_index: HashMap::new(),
    };
    // Every struct/class type the live code uses, with their fields' types.
    for (i, f) in p.fns.iter().enumerate() {
        if !live[i] {
            continue;
        }
        for l in &f.locals {
            g.collect(&l.ty)?;
        }
        g.collect(&f.ret)?;
    }
    for gl in &p.globals {
        let _ = g.collect(&gl.ty);
    }
    let mut next = (IMPORTS.len() + HELPERS.len()) as u32;
    for (i, l) in live.iter().enumerate() {
        if *l {
            g.fn_index.insert(FnId(i as u32), next);
            next += 1;
        }
    }
    let mut code = CodeSection::new();
    g.helpers(&mut code);
    let mut func_types = Vec::new();
    for (i, f) in p.fns.iter().enumerate() {
        if !live[i] {
            continue;
        }
        let (params, results) = g.signature(f)?;
        func_types.push((params, results));
        let body = g
            .function(FnId(i as u32), f)
            .map_err(|e| format!("{e} (in `{}`)", f.name))?;
        code.function(&body);
    }
    // Types: the rec group (strings, structs), then function types.
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
    for s in &g.structs {
        let fields: Vec<FieldType> = s
            .fields
            .iter()
            .map(|t| FieldType {
                element_type: StorageType::Val(g.wasm_ty(t).unwrap_or(ANYREF)),
                mutable: true,
            })
            .collect();
        group.push(SubType {
            is_final: s.base.is_none() && !s.is_class,
            supertype_idxs: s.base.into_iter().collect(),
            composite_type: CompositeType {
                inner: CompositeInnerType::Struct(StructType {
                    fields: fields.into(),
                }),
                shared: false,
                descriptor: None,
                describes: None,
            },
        });
    }
    let first_fn_type = group.len() as u32;
    types.ty().rec(group);
    let mut sigs: Vec<(Vec<ValType>, Vec<ValType>)> = Vec::new();
    let mut sig_index = |sig: (Vec<ValType>, Vec<ValType>), types: &mut TypeSection| -> u32 {
        if let Some(i) = sigs.iter().position(|s| *s == sig) {
            return first_fn_type + i as u32;
        }
        types.ty().function(sig.0.clone(), sig.1.clone());
        sigs.push(sig);
        first_fn_type + sigs.len() as u32 - 1
    };
    let mut imports = ImportSection::new();
    for (name, ps, rs) in IMPORTS {
        let t = sig_index(
            (
                ps.iter().map(|v| val(*v)).collect(),
                rs.iter().map(|v| val(*v)).collect(),
            ),
            &mut types,
        );
        imports.import("ppl", name, EntityType::Function(t));
    }
    let mut functions = FunctionSection::new();
    for h in HELPERS {
        functions.function(sig_index(helper_sig(h), &mut types));
    }
    for sig in func_types {
        functions.function(sig_index(sig, &mut types));
    }
    let mut globals = GlobalSection::new();
    for gl in &p.globals {
        // (A global of a type this backend can't hold yet is an `anyref` placeholder: only
        // `null` goes in.)
        let t = g.wasm_ty(&gl.ty).unwrap_or(ANYREF);
        globals.global(
            GlobalType {
                val_type: t,
                mutable: true,
                shared: false,
            },
            &zero_const(t),
        );
    }
    let mut exports = ExportSection::new();
    for (i, h) in HELPERS.iter().enumerate() {
        exports.export(h, ExportKind::Func, IMPORTS.len() as u32 + i as u32);
    }
    exports.export("start", ExportKind::Func, g.fn_index[&p.entry]);
    let mut data = DataSection::new();
    for d in &g.data {
        data.passive(d.iter().copied());
    }
    let mut m = Module::new();
    m.section(&types)
        .section(&imports)
        .section(&functions)
        .section(&globals)
        .section(&exports)
        .section(&DataCountSection {
            count: g.data.len() as u32,
        })
        .section(&code)
        .section(&data);
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
        "mul_ovf_s" | "mul_ovf_u" => (vec![I64, I64], vec![I32]),
        _ => unreachable!("helper {name}"),
    }
}

/// A struct or class type as a WasmGC struct.
struct GcStruct {
    fields: Vec<Ty>,
    /// The base class's struct type index (a class's fields start with its base's).
    base: Option<u32>,
    is_class: bool,
}

struct Gen<'p> {
    p: &'p Program,
    structs: Vec<GcStruct>,
    /// Struct/class type → its type index.
    struct_index: HashMap<Ty, u32>,
    /// Passive data segments (string constants).
    data: Vec<Vec<u8>>,
    fn_index: HashMap<FnId, u32>,
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
    scratch: HashMap<u8, u32>,
    nlocals: u32,
    extra: Vec<ValType>,
}

impl FnCx<'_> {
    /// A scratch local of type `t` (one per kind).
    fn scratch(&mut self, t: ValType) -> u32 {
        let key = match t {
            ValType::I32 => 0,
            ValType::I64 => 1,
            ValType::F32 => 2,
            ValType::F64 => 3,
            _ => 4,
        };
        if let Some(&i) = self.scratch.get(&key) {
            return i;
        }
        let i = self.nlocals;
        self.nlocals += 1;
        self.extra.push(if key == 4 { str_ref() } else { t });
        self.scratch.insert(key, i);
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
    /// Notes the struct types `t` needs.
    fn collect(&mut self, t: &Ty) -> R<()> {
        match t {
            Ty::Struct(sid, _, args) | Ty::Class(sid, _, args) => {
                if self.struct_index.contains_key(t) {
                    return Ok(());
                }
                let layout = self.p.layout(*sid);
                let owner = Ty::type_owner(*sid);
                let is_class = matches!(t, Ty::Class(..));
                // The base class first (its type is the supertype).
                let base = match (is_class, layout.meta.base) {
                    (true, Some(b)) => {
                        let bt =
                            Ty::Class(b, self.p.layout(b).name.as_str().into(), Vec::new().into());
                        self.collect(&bt)?;
                        self.struct_index.get(&bt).copied()
                    }
                    _ => None,
                };
                let idx = 1 + self.structs.len() as u32;
                self.struct_index.insert(t.clone(), idx);
                let fields: Vec<Ty> = layout
                    .fields
                    .iter()
                    .map(|(_, ft)| ft.subst(owner, args))
                    .collect();
                self.structs.push(GcStruct {
                    fields: fields.clone(),
                    base,
                    is_class,
                });
                for f in &fields {
                    // (Fields of types this backend can't hold yet are `anyref` placeholders:
                    // using them is what's unsupported.)
                    let _ = self.collect(f);
                }
                Ok(())
            }
            Ty::Ref(inner) => match &**inner {
                Ty::Struct(..) => self.collect(inner),
                _ => Ok(()),
            },
            Ty::Nullable(inner) => self.collect(inner),
            _ => Ok(()),
        }
    }

    /// A ppl type's wasm type (`None`: no value, or unsupported).
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
            Ty::Struct(..) | Ty::Class(..) => ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(*self.struct_index.get(&t.erase())?),
            }),
            Ty::Ref(inner) if matches!(*inner, Ty::Struct(..)) => self.wasm_ty(&inner)?,
            Ty::Nullable(inner) if matches!(*inner, Ty::Str | Ty::Struct(..) | Ty::Class(..)) => {
                self.wasm_ty(&inner)?
            }
            _ => return None,
        })
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

    fn struct_of(&self, t: &Ty) -> R<(u32, &GcStruct)> {
        let t = match t.erase() {
            Ty::Ref(inner) => *inner,
            Ty::Nullable(inner) => *inner,
            t => t,
        };
        match self.struct_index.get(&t) {
            Some(&i) => Ok((i, &self.structs[i as usize - 1])),
            None => unsupported(format!("fields of `{t}`")),
        }
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

    fn function(&mut self, _id: FnId, f: &mir::Function) -> R<Function> {
        let mut locals = Vec::new();
        let mut n = 0u32;
        let mut extra = Vec::new();
        // Parameters first, then the other locals.
        let is_param: Vec<bool> = (0..f.locals.len())
            .map(|i| f.params.iter().any(|p| p.0 as usize == i))
            .collect();
        let mut slots: Vec<Option<Slot>> = vec![None; f.locals.len()];
        for p in &f.params {
            if self.ty_or_err(f.local_ty(*p))?.is_some() {
                slots[p.0 as usize] = Some(Slot { index: n });
                n += 1;
            }
        }
        for (i, l) in f.locals.iter().enumerate() {
            if is_param[i] {
                continue;
            }
            if let Some(t) = self.ty_or_err(&l.ty)? {
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
            nlocals: n,
            extra,
        };
        // The body is built first (it may add scratch locals).
        let mut body = Function::new_with_locals_types(Vec::<ValType>::new());
        let mut code: Vec<u8> = Vec::new();
        {
            let mut sink = InstructionSink::new(&mut code);
            let nblocks = f.blocks.len() as u32;
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
                other => unsupported(format!("a `void` result of {other:?}")),
            };
        }
        self.store(fx, s, &st.place, |g, fx, s| {
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
            match pr {
                Proj::Field(i) => {
                    let (_, s) = self.struct_of(&t)?;
                    t = s.fields[*i as usize].clone();
                }
                _ => return unsupported("this kind of place (array elements, payloads)"),
            }
        }
        Ok(t)
    }

    /// Pushes the base of a place (a local's or global's value; a dereferenced struct
    /// reference is the struct).
    fn base_get(&self, fx: &FnCx, s: &mut InstructionSink, p: &Place) -> R<()> {
        match p.base {
            mir::Base::Local(l) | mir::Base::Deref(l) => {
                if let Some(slot) = fx.locals[l.0 as usize] {
                    s.local_get(slot.index);
                }
            }
            mir::Base::Global(g) => {
                s.global_get(g.0);
            }
        }
        Ok(())
    }

    /// Pushes a place's value (structs are copied: they're values).
    fn load(&self, fx: &mut FnCx, s: &mut InstructionSink, p: &Place, copy: bool) -> R<()> {
        let t = self.place_ty(fx, p)?;
        if no_value(&t) {
            return Ok(());
        }
        self.ty_or_err(&t)?;
        self.base_get(fx, s, p)?;
        let mut cur = match p.base {
            mir::Base::Local(l) => fx.f.local_ty(l).clone(),
            mir::Base::Global(g) => self.p.globals[g.0 as usize].ty.clone(),
            mir::Base::Deref(l) => fx.f.local_ty(l).clone(),
        };
        for pr in &p.proj {
            let Proj::Field(i) = pr else {
                unreachable!("checked")
            };
            let (idx, st) = self.struct_of(&cur)?;
            cur = st.fields[*i as usize].clone();
            s.struct_get(idx, *i);
        }
        if copy && matches!(t.erase(), Ty::Struct(..)) && !matches!(p.base, mir::Base::Deref(_)) {
            self.clone_struct(fx, s, &t)?;
        }
        Ok(())
    }

    /// Copies the struct on the stack (fields that are structs too).
    fn clone_struct(&self, fx: &mut FnCx, s: &mut InstructionSink, t: &Ty) -> R<()> {
        let (idx, st) = self.struct_of(t)?;
        let fields = st.fields.clone();
        let tmp = self.ref_scratch(fx, idx);
        s.local_set(tmp);
        // (A null struct stays null: `T?` of a struct.)
        s.local_get(tmp);
        s.ref_is_null();
        s.if_(BlockType::Result(ValType::Ref(RefType {
            nullable: true,
            heap_type: HeapType::Concrete(idx),
        })));
        s.ref_null(HeapType::Concrete(idx));
        s.else_();
        for (i, ft) in fields.iter().enumerate() {
            if no_value(ft) {
                continue;
            }
            s.local_get(tmp);
            s.struct_get(idx, i as u32);
            if matches!(ft.erase(), Ty::Struct(..)) {
                self.clone_struct(fx, s, ft)?;
                // (The nested copy used the scratch: restore it.)
            }
        }
        s.struct_new(idx);
        s.end();
        Ok(())
    }

    /// A scratch local holding a reference to struct type `idx` (one per type, per depth of
    /// use; nested copies reuse different types).
    fn ref_scratch(&self, fx: &mut FnCx, idx: u32) -> u32 {
        let key = 10 + idx as u8;
        if let Some(&i) = fx.scratch.get(&key) {
            return i;
        }
        let i = fx.nlocals;
        fx.nlocals += 1;
        fx.extra.push(ValType::Ref(RefType {
            nullable: true,
            heap_type: HeapType::Concrete(idx),
        }));
        fx.scratch.insert(key, i);
        i
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
            if let mir::Base::Deref(_) = p.base {
                return unsupported("assigning through a reference");
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
        let Proj::Field(i) = p.proj[p.proj.len() - 1] else {
            return unsupported("this kind of place");
        };
        let (idx, _) = self.struct_of(&parent_ty)?;
        self.load(fx, s, &parent, false)?;
        value(self, fx, s)?;
        s.struct_set(idx, i);
        Ok(())
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
        s.i32_const(loc.line as i32);
        s.i32_const(loc.col as i32);
        s.call(import_index("panic"));
        s.unreachable();
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
                        self.check(s, ppl_mir::mir::panics::SHIFT, loc);
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
                let fields = st.fields.clone();
                for (o, ft) in ops.iter().zip(&fields) {
                    if no_value(ft) {
                        continue;
                    }
                    self.operand(fx, s, o)?;
                }
                s.struct_new(idx);
            }
            Rvalue::New(ty) => {
                let (idx, _) = self.struct_of(ty)?;
                s.struct_new_default(idx);
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
                if !matches!(ot.erase(), Ty::Str | Ty::Struct(..) | Ty::Class(..)) {
                    return unsupported(format!("`{ot}?`"));
                }
                self.operand(fx, s, o)?;
            }
            Rvalue::IsNull(o) => {
                self.operand(fx, s, o)?;
                s.ref_is_null();
            }
            Rvalue::Unwrap(o) => {
                self.operand(fx, s, o)?;
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
            Rvalue::ExceptionPending => {
                // (Nothing throws in code this backend compiles yet.)
                s.i32_const(0);
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
        let key = 100 + n;
        if let Some(&i) = fx.scratch.get(&key) {
            return i;
        }
        let i = fx.nlocals;
        fx.nlocals += 1;
        fx.extra.push(ValType::I64);
        fx.scratch.insert(key, i);
        i
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
                    _ => return unsupported(format!("`{op:?}` on strings")),
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
        use ppl_mir::mir::panics;
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
        let key = 120 + n;
        if let Some(&i) = fx.scratch.get(&key) {
            return i;
        }
        let i = fx.nlocals;
        fx.nlocals += 1;
        fx.extra.push(ValType::I32);
        fx.scratch.insert(key, i);
        i
    }

    fn cast(
        &mut self,
        fx: &mut FnCx,
        s: &mut InstructionSink,
        from: &Ty,
        to: &Ty,
        loc: mir::Loc,
    ) -> R<()> {
        use ppl_mir::mir::panics;
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
                    self.operand(fx, s, v)?;
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
                            if no_value(callee_fn.local_ty(*p)) {
                                continue;
                            }
                            self.operand(fx, s, a)?;
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
                    Callee::Builtin(b) => {
                        return unsupported(format!("the builtin `{}`", b.name()));
                    }
                    Callee::Virtual { .. } => return unsupported("virtual methods"),
                    Callee::Indirect => return unsupported("function values"),
                    Callee::Iface { .. } => return unsupported("interface methods"),
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
                s.i32_const(loc.line as i32);
                s.i32_const(loc.col as i32);
                s.call(import_index("panic"));
                s.unreachable();
            }
            Terminator::Unreachable => {
                s.unreachable();
            }
            Terminator::Guard { .. } | Terminator::Unguard(_) | Terminator::Resume => {
                return unsupported("panic guards (`try`/`finally`)");
            }
            Terminator::Uncaught { .. } => return unsupported("uncaught exceptions"),
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
