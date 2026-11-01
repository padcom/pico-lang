//! LLVM code generation for ppl (via inkwell).
//!
//! Every MIR local becomes an `alloca` (LLVM's mem2reg cleans this up when optimizing).
//! Integer arithmetic uses the `llvm.*.with.overflow` intrinsics and branches to a call to
//! `ppl_panic` on overflow; division, shifts and conversions get explicit checks. Strings are
//! pointers to the runtime's `PplStr`; literals are writable globals with the same layout.
//!
//! Functions are named `ppl.<name>` with internal linkage, so user names never clash with C
//! symbols; the entry function is the external `ppl_entry` that the runtime's `main` calls.
//!
//! GC support (shadow-stack frames, type maps, safepoints, write barriers) is in [`gc`].

use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::intrinsics::Intrinsic;
use inkwell::module::{Linkage, Module};
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine,
};
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, IntType};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValue, BasicValueEnum, FunctionValue, GlobalValue, IntValue,
    PointerValue,
};
use inkwell::{AddressSpace, FloatPredicate, IntPredicate, OptimizationLevel};
use ppl_mir::mir::{
    self, Base, BinOp, BlockId, Callee, Const, Loc, Operand, Place, Program, Proj, Rvalue,
    Terminator, UnOp, panics,
};
use ppl_sema::format::{Align, FormatSpec, Kind, Sign};
use ppl_sema::hir::Builtin;
use ppl_sema::{FloatTy, IntTy, Ty};
use std::collections::HashMap;
use std::path::Path;

mod gc;

/// Code generation settings.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub optimize: bool,
}

/// The textual LLVM IR for `program` (`ppl build --emit llvm-ir`).
pub fn emit_llvm_ir(program: &Program) -> String {
    let cx = Context::create();
    let m = generate(&cx, program);
    m.print_to_string().to_string()
}

/// Compiles `program` to a native object file for the host.
pub fn emit_object(program: &Program, path: &Path, opts: Options) -> Result<(), String> {
    Target::initialize_native(&InitializationConfig::default())?;
    let triple = TargetMachine::get_default_triple();
    let target = Target::from_triple(&triple).map_err(|e| e.to_string())?;
    let level = if opts.optimize {
        OptimizationLevel::Default
    } else {
        OptimizationLevel::None
    };
    let tm = target
        .create_target_machine(
            &triple,
            &TargetMachine::get_host_cpu_name().to_string(),
            &TargetMachine::get_host_cpu_features().to_string(),
            level,
            RelocMode::PIC,
            CodeModel::Default,
        )
        .ok_or("cannot create a target machine for the host")?;
    let cx = Context::create();
    let m = generate(&cx, program);
    // Frame pointers let a panic walk the stack; debug builds also export ppl's functions, so
    // the trace can name every frame (`dladdr`).
    let mut f = m.get_first_function();
    while let Some(func) = f {
        if func.count_basic_blocks() > 0 {
            func.add_attribute(
                inkwell::attributes::AttributeLoc::Function,
                cx.create_string_attribute("frame-pointer", "all"),
            );
        }
        if !opts.optimize && func.get_name().to_bytes().starts_with(b"ppl.") {
            func.set_linkage(Linkage::External);
        }
        f = func.get_next_function();
    }
    m.set_triple(&triple);
    m.set_data_layout(&tm.get_target_data().get_data_layout());
    m.verify()
        .map_err(|e| format!("LLVM verification failed:\n{}", e.to_string()))?;
    if opts.optimize {
        m.run_passes(
            "default<O2>",
            &tm,
            inkwell::passes::PassBuilderOptions::create(),
        )
        .map_err(|e| e.to_string())?;
    }
    tm.write_to_file(&m, FileType::Object, path)
        .map_err(|e| e.to_string())
}

fn generate<'ctx>(cx: &'ctx Context, program: &Program) -> Module<'ctx> {
    let m = cx.create_module(&program.file);
    let mut g = Gen {
        cx,
        m,
        b: cx.create_builder(),
        p: program,
        fns: Vec::new(),
        globals: Vec::new(),
        cstrings: HashMap::new(),
        file: None,
        structs: Vec::new(),
        enums: Vec::new(),
        debug_fns: HashMap::new(),
        eq_fns: HashMap::new(),
        class_infos: Vec::new(),
        generic_structs: Default::default(),
        itables: Vec::new(),
        exception: None,
        layouts: Default::default(),
    };
    // Struct types first (named, so they can refer to each other), then their bodies.
    g.structs = program
        .structs
        .iter()
        .map(|s| cx.opaque_struct_type(&format!("ppl.{}", s.name)))
        .collect();
    g.enums = program
        .enums
        .iter()
        .map(|e| cx.opaque_struct_type(&format!("ppl.enum.{}", e.name)))
        .collect();
    for (i, s) in program.structs.iter().enumerate() {
        let mut fields: Vec<BasicTypeEnum> = s.fields.iter().filter_map(|(_, t)| g.ty(t)).collect();
        if s.is_class {
            // Objects start with a header word (the class's metadata, from P3.10 on).
            fields.insert(0, g.ptr().into());
        }
        g.structs[i].set_body(&fields, false);
    }
    // An enum is `{ i32 tag, [k x i64] payload }`, the payload big enough for every variant.
    for (i, e) in program.enums.iter().enumerate() {
        let words = e
            .variants
            .iter()
            .map(|v| {
                let parts: Vec<(u64, u64)> =
                    v.fields.iter().map(|(_, t)| g.size_align(t)).collect();
                struct_layout(&parts).0.div_ceil(8)
            })
            .max()
            .unwrap_or(0);
        let tag = cx.i32_type().into();
        if words == 0 {
            g.enums[i].set_body(&[tag], false);
        } else {
            let payload = cx.i64_type().array_type(words as u32).into();
            g.enums[i].set_body(&[tag, payload], false);
        }
    }
    g.declare();
    g.class_infos();
    let live = ppl_mir::mir::live_functions(program);
    for (i, l) in live.iter().enumerate() {
        if *l {
            g.function(i);
        }
    }
    // Unreachable functions (the core library's unused code) are dropped.
    for (i, l) in live.iter().enumerate() {
        if !*l {
            // SAFETY: nothing references an unreachable function.
            unsafe { g.fns[i].delete() };
        }
    }
    g.register_gc_globals();
    g.m
}

struct Gen<'ctx, 'p> {
    cx: &'ctx Context,
    m: Module<'ctx>,
    b: Builder<'ctx>,
    p: &'p Program,
    fns: Vec<FunctionValue<'ctx>>,
    globals: Vec<Option<GlobalValue<'ctx>>>,
    cstrings: HashMap<String, GlobalValue<'ctx>>,
    file: Option<GlobalValue<'ctx>>,
    /// LLVM struct types, by `StructId`.
    structs: Vec<inkwell::types::StructType<'ctx>>,
    /// Generated `${v:?}` helpers, by type.
    debug_fns: HashMap<Ty, FunctionValue<'ctx>>,
    /// LLVM enum types, by `EnumId`.
    enums: Vec<inkwell::types::StructType<'ctx>>,
    /// Generated `==` helpers for enums, by `EnumId`.
    eq_fns: HashMap<u32, FunctionValue<'ctx>>,
    /// Per class: `{ ptr base_info, [n x ptr] vtable }`, which object headers point to.
    class_infos: Vec<Option<GlobalValue<'ctx>>>,
    /// The pending exception (null when none); created when first used.
    exception: Option<GlobalValue<'ctx>>,
    /// LLVM types of instantiated generic structs and classes.
    generic_structs: std::cell::RefCell<HashMap<Ty, inkwell::types::StructType<'ctx>>>,
    /// Interface method tables, by itable index.
    itables: Vec<GlobalValue<'ctx>>,
    /// `PplLayout` constants by type (`true`: a class's objects).
    layouts: std::cell::RefCell<HashMap<(Ty, bool), PointerValue<'ctx>>>,
}

/// A variant's name, fields and whether it was declared with parentheses.
type VariantInfo = (String, Vec<(String, Ty)>, bool);

/// Size and alignment of a C-layout struct with these parts.
use ppl_mir::mir::struct_layout;

/// Per-function state.
struct FnCx<'ctx, 'p> {
    func: &'p ppl_mir::mir::Function,
    llfn: FunctionValue<'ctx>,
    locals: Vec<Option<PointerValue<'ctx>>>,
    blocks: Vec<BasicBlock<'ctx>>,
    /// The shadow-stack frame, when locals hold references.
    frame: Option<(PointerValue<'ctx>, inkwell::types::StructType<'ctx>)>,
}

impl<'ctx> Gen<'ctx, '_> {
    // ------------------------------------------------------------ types

    fn ty(&self, t: &Ty) -> Option<BasicTypeEnum<'ctx>> {
        Some(match t {
            Ty::Int(it) => self.int_ty(it.bits()).into(),
            Ty::Float(FloatTy::F32) => self.cx.f32_type().into(),
            Ty::Float(FloatTy::F64) => self.cx.f64_type().into(),
            Ty::Bool => self.cx.bool_type().into(),
            Ty::Char => self.cx.i32_type().into(),
            Ty::Str | Ty::Ref(_) | Ty::Array(_) | Ty::Class(..) => self.ptr().into(),
            // `{ fn, env }`, like an interface value.
            Ty::Iface(..) | Ty::Fn(..) => self.iface_ty().into(),
            Ty::Struct(sid, _, args) if args.is_empty() => self.structs[sid.0 as usize].into(),
            Ty::Struct(..) => self.struct_llty(t).into(),
            Ty::Enum(eid, _) => self.enums[eid.0 as usize].into(),
            Ty::Fixed(e, n) => self.elem_ty(e).array_type(*n as u32).into(),
            Ty::Tuple(ts) => {
                let fields: Vec<BasicTypeEnum> = ts.iter().map(|t| self.elem_ty(t)).collect();
                self.cx.struct_type(&fields, false).into()
            }
            Ty::Range(it) => {
                let t = self.int_ty(it.bits());
                self.cx
                    .struct_type(&[t.into(), t.into(), t.into()], false)
                    .into()
            }
            // A reference type's null is the null pointer; values get a presence flag.
            Ty::Nullable(t) if t.is_ref() => self.ptr().into(),
            Ty::Nullable(t) => {
                let inner = self.elem_ty(t);
                self.cx
                    .struct_type(&[self.cx.bool_type().into(), inner], false)
                    .into()
            }
            Ty::Lits(..)
            | Ty::Union(..)
            | Ty::Variant(..)
            | Ty::Distinct(..)
            | Ty::Immutable(_) => {
                return self.ty(&t.erase());
            }
            // Monomorphization removed every type parameter.
            Ty::Void | Ty::Never | Ty::Error | Ty::Null | Ty::Param(..) => return None,
        })
    }

    /// Size and alignment in bytes, as LLVM lays the type out on 64-bit targets.
    fn size_align(&self, t: &Ty) -> (u64, u64) {
        self.p.size_align(t)
    }

    /// The payload layout of one variant.
    fn variant_struct(
        &self,
        eid: ppl_sema::ty::EnumId,
        v: u32,
    ) -> inkwell::types::StructType<'ctx> {
        let fields: Vec<BasicTypeEnum> = self.p.enum_layout(eid).variants[v as usize]
            .fields
            .iter()
            .map(|(_, t)| self.elem_ty(t))
            .collect();
        self.cx.struct_type(&fields, false)
    }

    /// A pointer to field `i` of variant `v` inside the enum value at `ptr`.
    fn variant_field_ptr(
        &self,
        eid: ppl_sema::ty::EnumId,
        v: u32,
        i: u32,
        ptr: PointerValue<'ctx>,
    ) -> PointerValue<'ctx> {
        let et = self.enums[eid.0 as usize];
        let payload = self.b.build_struct_gep(et, ptr, 1, "payload").unwrap();
        self.b
            .build_struct_gep(self.variant_struct(eid, v), payload, i, "")
            .unwrap()
    }

    /// The LLVM struct type of a struct or class type (a class's object, with its header):
    /// one per instantiation of a generic type.
    fn struct_llty(&self, t: &Ty) -> inkwell::types::StructType<'ctx> {
        let (sid, args) = match t {
            Ty::Struct(s, _, a) | Ty::Class(s, _, a) => (*s, a),
            other => unreachable!("struct type of {other}"),
        };
        if args.is_empty() {
            return self.structs[sid.0 as usize];
        }
        if let Some(st) = self.generic_structs.borrow().get(t) {
            return *st;
        }
        let layout = self.p.layout(sid);
        let st = self.cx.opaque_struct_type(&format!("ppl.{t}"));
        self.generic_structs.borrow_mut().insert(t.clone(), st);
        let owner = Ty::type_owner(sid);
        let mut fields: Vec<BasicTypeEnum> = layout
            .fields
            .iter()
            .filter_map(|(_, f)| self.ty(&f.subst(owner, args)))
            .collect();
        if layout.is_class {
            fields.insert(0, self.ptr().into());
        }
        st.set_body(&fields, false);
        st
    }

    /// The type of an element or tuple part (a placeholder byte for types without values).
    fn elem_ty(&self, t: &Ty) -> BasicTypeEnum<'ctx> {
        self.ty(t).unwrap_or_else(|| self.cx.i8_type().into())
    }

    /// The `PplArray` header: `{ len, cap, data }`.
    fn array_header(&self) -> inkwell::types::StructType<'ctx> {
        let i64t = self.i64();
        self.cx
            .struct_type(&[i64t.into(), i64t.into(), self.ptr().into()], false)
    }

    /// `a.len` of a `PplArray*`.
    fn array_len(&self, a: PointerValue<'ctx>) -> IntValue<'ctx> {
        let p = self
            .b
            .build_struct_gep(self.array_header(), a, 0, "len")
            .unwrap();
        self.b
            .build_load(self.i64(), p, "")
            .unwrap()
            .into_int_value()
    }

    /// `a.data` of a `PplArray*`.
    fn array_data(&self, a: PointerValue<'ctx>) -> PointerValue<'ctx> {
        let p = self
            .b
            .build_struct_gep(self.array_header(), a, 2, "data")
            .unwrap();
        self.b
            .build_load(self.ptr(), p, "")
            .unwrap()
            .into_pointer_value()
    }

    /// A pointer to element `i` of a growable array.
    fn array_elem(
        &self,
        a: PointerValue<'ctx>,
        elem: &Ty,
        i: IntValue<'ctx>,
    ) -> PointerValue<'ctx> {
        let data = self.array_data(a);
        unsafe {
            self.b
                .build_in_bounds_gep(self.elem_ty(elem), data, &[i], "elem")
                .unwrap()
        }
    }

    fn size_of(&self, t: &Ty) -> IntValue<'ctx> {
        self.elem_ty(t).size_of().expect("sized type")
    }

    /// A stack slot in the current function's entry block (an `alloca` inside a loop would
    /// grow the stack on every iteration).
    fn entry_alloca(&self, t: BasicTypeEnum<'ctx>, name: &str) -> PointerValue<'ctx> {
        let f = self
            .b
            .get_insert_block()
            .and_then(|b| b.get_parent())
            .expect("inside a function");
        let entry = f.get_first_basic_block().expect("entry block");
        let b = self.cx.create_builder();
        match entry.get_first_instruction() {
            Some(i) => b.position_before(&i),
            None => b.position_at_end(entry),
        }
        b.build_alloca(t, name).unwrap()
    }

    fn int_ty(&self, bits: u32) -> IntType<'ctx> {
        match bits {
            8 => self.cx.i8_type(),
            16 => self.cx.i16_type(),
            32 => self.cx.i32_type(),
            64 => self.cx.i64_type(),
            128 => self.cx.i128_type(),
            _ => unreachable!("no {bits}-bit integer type"),
        }
    }

    fn ptr(&self) -> inkwell::types::PointerType<'ctx> {
        self.cx.ptr_type(AddressSpace::default())
    }

    /// The global holding the pending exception.
    fn exception_slot(&mut self) -> inkwell::values::PointerValue<'ctx> {
        if let Some(g) = self.exception {
            return g.as_pointer_value();
        }
        let ptr = self.ptr();
        let g = self.m.add_global(ptr, None, "ppl.exception");
        g.set_linkage(Linkage::Internal);
        g.set_initializer(&ptr.const_null());
        self.exception = Some(g);
        g.as_pointer_value()
    }

    /// The global holding the pending exception's tag.
    fn exception_tag_slot(&mut self) -> inkwell::values::PointerValue<'ctx> {
        if let Some(g) = self.m.get_global("ppl.exception_tag") {
            return g.as_pointer_value();
        }
        let t = self.i32();
        let g = self.m.add_global(t, None, "ppl.exception_tag");
        g.set_linkage(Linkage::Internal);
        g.set_initializer(&t.const_zero());
        g.as_pointer_value()
    }

    fn i32(&self) -> IntType<'ctx> {
        self.cx.i32_type()
    }

    fn i64(&self) -> IntType<'ctx> {
        self.cx.i64_type()
    }

    // ------------------------------------------------------------ declarations

    fn declare(&mut self) {
        for (i, f) in self.p.fns.iter().enumerate() {
            let params: Vec<BasicMetadataTypeEnum> = f
                .params
                .iter()
                .filter_map(|l| self.ty(f.local_ty(*l)))
                .map(Into::into)
                .collect();
            let fn_ty = match self.ty(&f.ret) {
                Some(t) => t.fn_type(&params, false),
                None => self.cx.void_type().fn_type(&params, false),
            };
            let is_entry = i == self.p.entry.0 as usize;
            let (name, linkage) = if is_entry {
                ("ppl_entry".to_string(), Linkage::External)
            } else {
                (format!("ppl.{}", f.name), Linkage::Internal)
            };
            self.fns
                .push(self.m.add_function(&name, fn_ty, Some(linkage)));
        }
        for (i, gdecl) in self.p.globals.iter().enumerate() {
            let g = self.ty(&gdecl.ty).map(|t| {
                let g = self
                    .m
                    .add_global(t, None, &format!("ppl.global.{}", gdecl.name));
                g.set_linkage(Linkage::Internal);
                g.set_initializer(&t.const_zero());
                g
            });
            let _ = i;
            self.globals.push(g);
        }
    }

    /// The class metadata globals (after functions are declared, for the vtables).
    fn class_infos(&mut self) {
        let ptr = self.ptr();
        let mut infos = Vec::new();
        for s in &self.p.structs {
            infos.push(s.is_class.then(|| {
                let ty = self.cx.struct_type(
                    &[
                        ptr.into(),
                        ptr.array_type(s.meta.vtable.len() as u32).into(),
                    ],
                    false,
                );
                let g = self
                    .m
                    .add_global(ty, None, &format!("ppl.classinfo.{}", s.name));
                g.set_linkage(Linkage::Internal);
                g.set_constant(true);
                g
            }));
        }
        for (i, s) in self.p.structs.iter().enumerate() {
            let Some(g) = infos[i] else { continue };
            let base = match s.meta.base.and_then(|b| infos[b.0 as usize]) {
                Some(b) => b.as_pointer_value(),
                None => ptr.const_null(),
            };
            let slots: Vec<PointerValue> = s
                .meta
                .vtable
                .iter()
                .map(|f| self.fns[f.0 as usize].as_global_value().as_pointer_value())
                .collect();
            let value = self
                .cx
                .const_struct(&[base.into(), ptr.const_array(&slots).into()], false);
            g.set_initializer(&value);
        }
        self.class_infos = infos;
        for (i, entries) in self.p.itables.iter().enumerate() {
            let slots: Vec<PointerValue> = entries
                .iter()
                .map(|f| self.fns[f.0 as usize].as_global_value().as_pointer_value())
                .collect();
            let arr = ptr.const_array(&slots);
            let g = self
                .m
                .add_global(arr.get_type(), None, &format!("ppl.itable.{i}"));
            g.set_linkage(Linkage::Internal);
            g.set_constant(true);
            g.set_initializer(&arr);
            self.itables.push(g);
        }
    }

    /// An interface value's LLVM type: `{ ptr data, ptr itable }`.
    fn iface_ty(&self) -> inkwell::types::StructType<'ctx> {
        self.cx
            .struct_type(&[self.ptr().into(), self.ptr().into()], false)
    }

    /// A runtime function, declared on first use.
    fn rt(&self, name: &str) -> FunctionValue<'ctx> {
        if let Some(f) = self.m.get_function(name) {
            return f;
        }
        let (i32t, i64t, ptr, void) = (self.i32(), self.i64(), self.ptr(), self.cx.void_type());
        let fn_ty = match name {
            "ppl_print_i64" | "ppl_print_u64" => void.fn_type(&[i64t.into()], false),
            "ppl_print_f64" => void.fn_type(&[self.cx.f64_type().into()], false),
            "ppl_print_f32" => void.fn_type(&[self.cx.f32_type().into()], false),
            "ppl_print_bool" | "ppl_print_char" => void.fn_type(&[i32t.into()], false),
            "ppl_print_string" => void.fn_type(&[ptr.into()], false),
            "ppl_print_newline" => void.fn_type(&[], false),
            "ppl_panic" => void.fn_type(
                &[
                    ptr.into(),
                    i64t.into(),
                    ptr.into(),
                    i64t.into(),
                    i32t.into(),
                    i32t.into(),
                ],
                false,
            ),
            "ppl_panic_str" => void.fn_type(
                &[
                    ptr.into(),
                    ptr.into(),
                    i64t.into(),
                    i32t.into(),
                    i32t.into(),
                ],
                false,
            ),
            "ppl_str_concat" => ptr.fn_type(&[ptr.into(), ptr.into()], false),
            "ppl_str_slice" => ptr.fn_type(&[ptr.into(), i64t.into(), i64t.into()], false),
            "ppl_uncaught" => void.fn_type(&[ptr.into(), ptr.into()], false),
            "ppl_fmt_i64" | "ppl_fmt_u64" => ptr.fn_type(&[i64t.into(), ptr.into()], false),
            "ppl_fmt_f64" => {
                ptr.fn_type(&[self.cx.f64_type().into(), i32t.into(), ptr.into()], false)
            }
            "ppl_fmt_str" => ptr.fn_type(&[ptr.into(), ptr.into()], false),
            "ppl_fmt_char" | "ppl_fmt_bool" => ptr.fn_type(&[i32t.into(), ptr.into()], false),
            "ppl_str_from_i64" | "ppl_str_from_u64" => ptr.fn_type(&[i64t.into()], false),
            "ppl_str_from_f64" => ptr.fn_type(&[self.cx.f64_type().into()], false),
            "ppl_str_from_f32" => ptr.fn_type(&[self.cx.f32_type().into()], false),
            "ppl_str_from_bool" | "ppl_str_from_char" => ptr.fn_type(&[i32t.into()], false),
            "ppl_str_eq" | "ppl_str_cmp" => i32t.fn_type(&[ptr.into(), ptr.into()], false),
            "ppl_str_char_at" => i32t.fn_type(
                &[
                    ptr.into(),
                    i64t.into(),
                    ptr.into(),
                    i64t.into(),
                    i32t.into(),
                    i32t.into(),
                ],
                false,
            ),
            "ppl_array_new" => ptr.fn_type(&[i64t.into(), i64t.into(), ptr.into()], false),
            "ppl_gc_alloc" => ptr.fn_type(&[i64t.into(), ptr.into()], false),
            "ppl_gc_write" => void.fn_type(&[ptr.into(), ptr.into()], false),
            "ppl_gc_set_globals" => void.fn_type(&[ptr.into()], false),
            "ppl_gc_safepoint"
            | "ppl_gc_collect"
            | "ppl_gc_collect_young"
            | "ppl_gc_pause"
            | "ppl_gc_resume"
            | "ppl_gc_dump_roots" => void.fn_type(&[], false),
            "ppl_gc_stat" => i64t.fn_type(&[i64t.into()], false),
            "ppl_arena_enter" => void.fn_type(&[], false),
            "ppl_arena_exit_rescue" => void.fn_type(
                &[
                    ptr.into(),
                    ptr.into(),
                    i64t.into(),
                    i32t.into(),
                    i32t.into(),
                ],
                false,
            ),
            "ppl_arena_exit" => {
                void.fn_type(&[ptr.into(), i64t.into(), i32t.into(), i32t.into()], false)
            }
            "ppl_raw_arena_new" => i64t.fn_type(&[i64t.into()], false),
            "ppl_raw_arena_free" => void.fn_type(&[i64t.into()], false),
            "ppl_raw_arena_alloc" => ptr.fn_type(&[i64t.into(), i64t.into(), i64t.into()], false),
            "ppl_instance_of" => i32t.fn_type(&[ptr.into(), ptr.into()], false),
            "ppl_hash_u64" => i64t.fn_type(&[i64t.into()], false),
            "ppl_hash_str" => i64t.fn_type(&[ptr.into()], false),
            "ppl_hash_combine" => i64t.fn_type(&[i64t.into(), i64t.into()], false),
            "ppl_array_push" => ptr.fn_type(&[ptr.into(), i64t.into()], false),
            "ppl_array_pop" => i32t.fn_type(&[ptr.into()], false),
            "ppl_array_slice" => {
                ptr.fn_type(&[ptr.into(), i64t.into(), i64t.into(), i64t.into()], false)
            }
            "ppl_ipow_s" | "ppl_ipow_u" => {
                i64t.fn_type(&[i64t.into(), i64t.into(), i32t.into(), ptr.into()], false)
            }
            "ppl_guard_push" => void.fn_type(&[ptr.into()], false),
            "ppl_guard_pop" | "ppl_resume" => void.fn_type(&[], false),
            "_setjmp" => i32t.fn_type(&[ptr.into()], false),
            _ => unreachable!("unknown runtime function {name}"),
        };
        let f = self.m.add_function(name, fn_ty, Some(Linkage::External));
        let attr = |n: &str| {
            self.cx
                .create_enum_attribute(inkwell::attributes::Attribute::get_named_enum_kind_id(n), 0)
        };
        if name == "_setjmp" {
            f.add_attribute(
                inkwell::attributes::AttributeLoc::Function,
                attr("returns_twice"),
            );
        }
        if name == "ppl_resume" {
            f.add_attribute(
                inkwell::attributes::AttributeLoc::Function,
                attr("noreturn"),
            );
        }
        if name.starts_with("ppl_panic") {
            let noreturn = self.cx.create_enum_attribute(
                inkwell::attributes::Attribute::get_named_enum_kind_id("noreturn"),
                0,
            );
            let cold = self.cx.create_enum_attribute(
                inkwell::attributes::Attribute::get_named_enum_kind_id("cold"),
                0,
            );
            f.add_attribute(inkwell::attributes::AttributeLoc::Function, noreturn);
            f.add_attribute(inkwell::attributes::AttributeLoc::Function, cold);
        }
        f
    }

    fn intrinsic(&self, name: &str, tys: &[BasicTypeEnum<'ctx>]) -> FunctionValue<'ctx> {
        Intrinsic::find(name)
            .unwrap_or_else(|| panic!("no LLVM intrinsic {name}"))
            .get_declaration(&self.m, tys)
            .unwrap_or_else(|| panic!("cannot declare {name}"))
    }

    /// A private constant byte string (no terminator), for panic messages and file names.
    fn cstring(&mut self, s: &str) -> GlobalValue<'ctx> {
        if let Some(g) = self.cstrings.get(s) {
            return *g;
        }
        let bytes = self.cx.const_string(s.as_bytes(), false);
        let g = self.m.add_global(bytes.get_type(), None, "ppl.cstr");
        g.set_linkage(Linkage::Private);
        g.set_constant(true);
        g.set_unnamed_addr(true);
        g.set_initializer(&bytes);
        self.cstrings.insert(s.to_string(), g);
        g
    }

    /// A string literal: a writable global laid out like `PplStr`.
    fn str_literal(&mut self, s: &str) -> PointerValue<'ctx> {
        let i64t = self.i64();
        let data = self.cx.const_string(s.as_bytes(), false);
        let fields: Vec<BasicValueEnum> = vec![
            i64t.const_int(s.len() as u64, false).into(),
            i64t.const_int(s.chars().count() as u64, false).into(),
            i64t.const_zero().into(),
            i64t.const_zero().into(),
            data.into(),
        ];
        let value = self.cx.const_struct(&fields, false);
        let g = self.m.add_global(value.get_type(), None, "ppl.str");
        g.set_linkage(Linkage::Private);
        g.set_initializer(&value);
        g.set_alignment(8);
        g.as_pointer_value()
    }

    // ------------------------------------------------------------ functions

    fn function(&mut self, index: usize) {
        let func = &self.p.fns[index];
        let llfn = self.fns[index];
        let blocks: Vec<BasicBlock> = (0..func.blocks.len())
            .map(|i| self.cx.append_basic_block(llfn, &format!("bb{i}")))
            .collect();
        let entry = self.cx.prepend_basic_block(blocks[0], "start");
        self.b.position_at_end(entry);
        // After a `longjmp` to a panic guard, values the optimizer kept in registers would be
        // stale: functions with guards aren't optimized.
        if func
            .blocks
            .iter()
            .any(|b| matches!(b.term, Terminator::Guard { .. }))
        {
            for n in ["optnone", "noinline"] {
                let a = self.cx.create_enum_attribute(
                    inkwell::attributes::Attribute::get_named_enum_kind_id(n),
                    0,
                );
                llfn.add_attribute(inkwell::attributes::AttributeLoc::Function, a);
            }
        }
        let mut locals: Vec<Option<PointerValue>> = func
            .locals
            .iter()
            .enumerate()
            .map(|(i, l)| {
                if self.p.traced(&l.ty) {
                    return None;
                }
                self.ty(&l.ty).map(|t| {
                    let name = l.name.clone().unwrap_or_else(|| format!("t{i}"));
                    self.b.build_alloca(t, &name).unwrap()
                })
            })
            .collect();
        // Locals holding references live in the shadow-stack frame.
        let frame = self.gc_frame(func, &mut locals);
        let mut arg = 0;
        for p in &func.params {
            if let Some(slot) = locals[p.0 as usize] {
                let v = llfn.get_nth_param(arg).unwrap();
                arg += 1;
                self.b.build_store(slot, v).unwrap();
            }
        }
        self.safepoint(llfn);
        self.b.build_unconditional_branch(blocks[0]).unwrap();
        let mut fx = FnCx {
            func,
            llfn,
            locals,
            blocks,
            frame,
        };
        let headers = gc::loop_headers(func);
        for (i, bb) in func.blocks.iter().enumerate() {
            self.b.position_at_end(fx.blocks[i]);
            if headers[i] {
                self.safepoint(llfn);
            }
            for s in &bb.stmts {
                let v = self.rvalue(&mut fx, &s.rvalue, s.loc);
                if let Some(v) = v {
                    self.store(&fx, &s.place, v);
                }
            }
            self.terminator(&mut fx, &bb.term);
        }
    }

    /// A pointer to a place: the base's storage, then one GEP per field.
    fn place_ptr(&self, fx: &FnCx<'ctx, '_>, p: &Place) -> Option<PointerValue<'ctx>> {
        let (mut ptr, mut ty) = match p.base {
            Base::Local(l) => (fx.locals[l.0 as usize]?, fx.func.local_ty(l).clone()),
            Base::Global(g) => (
                self.globals[g.0 as usize]?.as_pointer_value(),
                self.p.globals[g.0 as usize].ty.clone(),
            ),
            Base::Deref(l) => {
                let slot = fx.locals[l.0 as usize]?;
                let target = self
                    .b
                    .build_load(self.ptr(), slot, "")
                    .unwrap()
                    .into_pointer_value();
                let Ty::Ref(inner) = fx.func.local_ty(l) else {
                    unreachable!("deref of non-reference")
                };
                (target, (**inner).clone())
            }
        };
        for pr in &p.proj {
            ptr = match (pr, &ty) {
                (Proj::Field(i), Ty::Class(..)) => {
                    // Through the object reference; field 0 is the header.
                    let obj = self
                        .b
                        .build_load(self.ptr(), ptr, "")
                        .unwrap()
                        .into_pointer_value();
                    self.b
                        .build_struct_gep(self.struct_llty(&ty), obj, i + 1, "")
                        .unwrap()
                }
                (Proj::Field(i), _) => {
                    let st = self.ty(&ty).expect("aggregate").into_struct_type();
                    self.b.build_struct_gep(st, ptr, *i, "").unwrap()
                }
                (Proj::Index(l), Ty::Fixed(..)) => {
                    let i = self.load_index(fx, *l);
                    let at = self.ty(&ty).expect("array");
                    let zero = self.i64().const_zero();
                    unsafe { self.b.build_in_bounds_gep(at, ptr, &[zero, i], "").unwrap() }
                }
                (Proj::Index(l), Ty::Array(elem)) => {
                    let i = self.load_index(fx, *l);
                    let a = self
                        .b
                        .build_load(self.ptr(), ptr, "")
                        .unwrap()
                        .into_pointer_value();
                    self.array_elem(a, elem, i)
                }
                (Proj::VariantField(v, i), Ty::Enum(eid, _)) => {
                    self.variant_field_ptr(*eid, *v, *i, ptr)
                }
                (Proj::Payload, Ty::Nullable(inner)) if inner.is_ref() => ptr,
                (Proj::Payload, Ty::Nullable(_)) => {
                    let st = self.ty(&ty).expect("nullable").into_struct_type();
                    self.b.build_struct_gep(st, ptr, 1, "").unwrap()
                }
                (pr, t) => unreachable!("projection {pr:?} of {t}"),
            };
            ty = mir::project(&ty, pr, |f| self.p.field_ty(f));
        }
        Some(ptr)
    }

    fn load_index(&self, fx: &FnCx<'ctx, '_>, l: ppl_mir::mir::Local) -> IntValue<'ctx> {
        let slot = fx.locals[l.0 as usize].expect("index local");
        self.b
            .build_load(self.i64(), slot, "idx")
            .unwrap()
            .into_int_value()
    }

    fn place_ty(&self, fx: &FnCx<'ctx, '_>, p: &Place) -> Ty {
        self.p.place_ty(fx.func, p)
    }

    fn store(&self, fx: &FnCx<'ctx, '_>, p: &Place, v: BasicValueEnum<'ctx>) {
        if let Some(ptr) = self.place_ptr(fx, p) {
            self.b.build_store(ptr, v).unwrap();
            let t = self.place_ty(fx, p);
            if self.p.traced(&t) && self.place_in_heap(fx, p) {
                self.barrier(ptr, &t);
            }
        }
    }

    fn operand_ty(&self, fx: &FnCx<'ctx, '_>, o: &Operand) -> Ty {
        match o {
            Operand::Copy(p) => self.place_ty(fx, p),
            Operand::Const(c) => c.ty(),
        }
    }

    fn operand(&mut self, fx: &FnCx<'ctx, '_>, o: &Operand) -> Option<BasicValueEnum<'ctx>> {
        match o {
            Operand::Copy(p) => {
                let ty = self.ty(&self.place_ty(fx, p))?;
                let ptr = self.place_ptr(fx, p)?;
                Some(self.b.build_load(ty, ptr, "").unwrap())
            }
            Operand::Const(c) => Some(match c {
                Const::Int(v, it) => self.int_ty(it.bits()).const_int(*v as u64, false).into(),
                Const::Float(v, FloatTy::F32) => self.cx.f32_type().const_float(*v).into(),
                Const::Float(v, FloatTy::F64) => self.cx.f64_type().const_float(*v).into(),
                Const::Bool(b) => self.cx.bool_type().const_int(u64::from(*b), false).into(),
                Const::Char(c) => self.i32().const_int(*c as u64, false).into(),
                Const::Str(s) => self.str_literal(s).into(),
                Const::Null(t) => self.ty(t)?.const_zero(),
                Const::Undef(t) => self.ty(t)?.const_zero(),
                Const::Void => return None,
            }),
        }
    }

    // ------------------------------------------------------------ panics and checks

    fn file_global(&mut self) -> GlobalValue<'ctx> {
        if let Some(f) = self.file {
            return f;
        }
        let name = self.p.file.clone();
        let g = self.cstring(&name);
        self.file = Some(g);
        g
    }

    fn loc_args(&mut self, loc: Loc) -> [BasicMetadataValueEnum<'ctx>; 4] {
        let file = self.file_global();
        [
            file.as_pointer_value().into(),
            self.i64().const_int(self.p.file.len() as u64, false).into(),
            self.i32().const_int(loc.line as u64, false).into(),
            self.i32().const_int(loc.col as u64, false).into(),
        ]
    }

    /// Emits a call to `ppl_panic` with a fixed message, then `unreachable`.
    fn panic_here(&mut self, msg: &str, loc: Loc) {
        let m = self.cstring(msg);
        let [f, fl, l, c] = self.loc_args(loc);
        let args = [
            m.as_pointer_value().into(),
            self.i64().const_int(msg.len() as u64, false).into(),
            f,
            fl,
            l,
            c,
        ];
        self.b.build_call(self.rt("ppl_panic"), &args, "").unwrap();
        self.b.build_unreachable().unwrap();
    }

    /// Panics with `msg` when `bad` is true; continues in a fresh block otherwise.
    fn check(&mut self, fx: &FnCx<'ctx, '_>, bad: IntValue<'ctx>, msg: &str, loc: Loc) {
        let fail = self.cx.append_basic_block(fx.llfn, "panic");
        let ok = self.cx.append_basic_block(fx.llfn, "ok");
        self.b.build_conditional_branch(bad, fail, ok).unwrap();
        self.b.position_at_end(fail);
        self.panic_here(msg, loc);
        self.b.position_at_end(ok);
    }

    // ------------------------------------------------------------ rvalues

    fn rvalue(
        &mut self,
        fx: &mut FnCx<'ctx, '_>,
        rv: &Rvalue,
        loc: Loc,
    ) -> Option<BasicValueEnum<'ctx>> {
        match rv {
            Rvalue::Use(o) => self.operand(fx, o),
            Rvalue::Unary(op, o) => {
                let t = self.operand_ty(fx, o);
                let v = self.operand(fx, o)?;
                Some(match (op, &t) {
                    (UnOp::Not, _) => self.b.build_not(v.into_int_value(), "").unwrap().into(),
                    (UnOp::BitNot, _) => self.b.build_not(v.into_int_value(), "").unwrap().into(),
                    (UnOp::Neg, Ty::Float(_)) => self
                        .b
                        .build_float_neg(v.into_float_value(), "")
                        .unwrap()
                        .into(),
                    (UnOp::Neg, Ty::Int(it)) => {
                        let zero = v.into_int_value().get_type().const_zero();
                        self.overflowing(fx, "sub", *it, zero, v.into_int_value(), loc)
                            .into()
                    }
                    _ => unreachable!("type-checked unary"),
                })
            }
            Rvalue::Binary(op, a, b) => {
                let t = self.operand_ty(fx, a);
                let bt = self.operand_ty(fx, b);
                let x = self.operand(fx, a)?;
                let y = self.operand(fx, b)?;
                Some(self.binary(fx, *op, &t, &bt, x, y, loc))
            }
            Rvalue::Cast(o, to) => {
                let from = self.operand_ty(fx, o);
                let v = self.operand(fx, o)?;
                Some(self.cast(fx, v, &from, to, loc))
            }
            Rvalue::PtrOffset(p, n, sub) => {
                let Ty::Ref(elem) = self.operand_ty(fx, p) else {
                    unreachable!("a pointer")
                };
                let nt = self.operand_ty(fx, n);
                let p = self.operand(fx, p)?.into_pointer_value();
                let n = self.operand(fx, n)?.into_int_value();
                let signed = matches!(nt.erase(), Ty::Int(it) if it.signed());
                let mut n = self
                    .b
                    .build_int_cast_sign_flag(n, self.i64(), signed, "")
                    .unwrap();
                if *sub {
                    n = self.b.build_int_neg(n, "").unwrap();
                }
                let et = self.elem_ty(&elem);
                Some(unsafe { self.b.build_gep(et, p, &[n], "ptr").unwrap() }.into())
            }
            Rvalue::RawArenaAlloc(t, h, n) => {
                let (size, align) = self.size_align(t);
                let h = self.operand(fx, h)?.into_int_value();
                let n = self.operand(fx, n)?.into_int_value();
                let i64t = self.i64();
                let bytes = self
                    .b
                    .build_int_mul(n, i64t.const_int(size, false), "bytes")
                    .unwrap();
                let r = self
                    .b
                    .build_call(
                        self.rt("ppl_raw_arena_alloc"),
                        &[h.into(), bytes.into(), i64t.const_int(align, false).into()],
                        "",
                    )
                    .unwrap();
                Some(basic(r))
            }
            Rvalue::SizeOf(t, align) => {
                let (size, alignment) = self.size_align(t);
                let v = if *align { alignment } else { size };
                Some(self.i64().const_int(v, false).into())
            }
            Rvalue::BitCast(o, to) => {
                let v = self.operand(fx, o)?;
                let t = self.ty(to).expect("a number type");
                Some(self.b.build_bit_cast(v, t, "bits").unwrap())
            }
            Rvalue::ToStr(o) => {
                let t = self.operand_ty(fx, o);
                let v = self.operand(fx, o)?;
                let (name, arg) = self.primitive_arg(&t, v, "ppl_str_from")?;
                Some(basic(
                    self.b.build_call(self.rt(&name), &[arg], "").unwrap(),
                ))
            }
            Rvalue::Format(o, spec) => {
                let t = self.operand_ty(fx, o);
                let v = self.operand(fx, o)?;
                if ppl_sema::format::is_compound(&t) {
                    let text = self.debug_value(&t, v);
                    // Width, fill and alignment apply to the whole text.
                    let outer = FormatSpec {
                        kind: Kind::Default,
                        ..*spec
                    };
                    return Some(self.format(&Ty::Str, text, &outer));
                }
                Some(self.format(&t, v, spec))
            }
            Rvalue::Aggregate(t, ops) => {
                let st = self.struct_llty(t);
                let Ty::Struct(sid, _, args) = t else {
                    unreachable!("aggregate of {t}")
                };
                let mut agg = st.get_undef();
                let mut index = 0;
                let owner = Ty::type_owner(*sid);
                for (o, (_, fty)) in ops.iter().zip(&self.p.layout(*sid).fields) {
                    if self.ty(&fty.subst(owner, args)).is_none() {
                        continue;
                    }
                    let v = self.operand(fx, o).expect("field value");
                    agg = self
                        .b
                        .build_insert_value(agg, v, index, "")
                        .unwrap()
                        .into_struct_value();
                    index += 1;
                }
                Some(agg.into())
            }
            Rvalue::Ref(place) => self.place_ptr(fx, place).map(Into::into),
            Rvalue::Tuple(t, ops) => {
                let lt = self.ty(t).expect("aggregate type");
                let mut agg = match t {
                    Ty::Fixed(..) => lt.into_array_type().get_undef().as_basic_value_enum(),
                    _ => lt.into_struct_type().get_undef().as_basic_value_enum(),
                };
                for (i, o) in ops.iter().enumerate() {
                    let Some(v) = self.operand(fx, o) else {
                        continue;
                    };
                    agg = match agg {
                        BasicValueEnum::ArrayValue(a) => self
                            .b
                            .build_insert_value(a, v, i as u32, "")
                            .unwrap()
                            .as_basic_value_enum(),
                        BasicValueEnum::StructValue(st) => self
                            .b
                            .build_insert_value(st, v, i as u32, "")
                            .unwrap()
                            .as_basic_value_enum(),
                        _ => unreachable!(),
                    };
                }
                Some(agg)
            }
            Rvalue::Repeat(o, n) => {
                let et = self.operand_ty(fx, o);
                let v = self.operand(fx, o);
                let at = self.elem_ty(&et).array_type(*n as u32);
                let mut agg = at.get_undef();
                if let Some(v) = v {
                    for i in 0..*n {
                        agg = self
                            .b
                            .build_insert_value(agg, v, i as u32, "")
                            .unwrap()
                            .into_array_value();
                    }
                }
                Some(agg.into())
            }
            Rvalue::ArrayNew(elem, ops) => {
                let size = self.size_of(elem);
                let n = self.i64().const_int(ops.len() as u64, false);
                let layout = self.layout(elem);
                let a = basic(
                    self.b
                        .build_call(
                            self.rt("ppl_array_new"),
                            &[size.into(), n.into(), layout.into()],
                            "",
                        )
                        .unwrap(),
                )
                .into_pointer_value();
                for (i, o) in ops.iter().enumerate() {
                    if let Some(v) = self.operand(fx, o) {
                        let idx = self.i64().const_int(i as u64, false);
                        let p = self.array_elem(a, elem, idx);
                        self.b.build_store(p, v).unwrap();
                    }
                }
                Some(a.into())
            }
            Rvalue::ArraySlice(a, b, c) => {
                let Ty::Array(elem) = self.operand_ty(fx, a) else {
                    unreachable!("slice of non-array")
                };
                let arr = self.operand(fx, a)?;
                let start = self.operand(fx, b)?;
                let end = self.operand(fx, c)?;
                let size = self.size_of(&elem);
                Some(basic(
                    self.b
                        .build_call(
                            self.rt("ppl_array_slice"),
                            &[arr.into(), size.into(), start.into(), end.into()],
                            "",
                        )
                        .unwrap(),
                ))
            }
            Rvalue::ArrayLen(o) => {
                let a = self.operand(fx, o)?.into_pointer_value();
                Some(self.array_len(a).into())
            }
            Rvalue::ArrayPush(a, v) => {
                let Ty::Array(elem) = self.operand_ty(fx, a) else {
                    unreachable!("push to non-array")
                };
                let arr = self.operand(fx, a)?;
                let val = self.operand(fx, v);
                let size = self.size_of(&elem);
                let slot = basic(
                    self.b
                        .build_call(self.rt("ppl_array_push"), &[arr.into(), size.into()], "")
                        .unwrap(),
                )
                .into_pointer_value();
                if let Some(val) = val {
                    self.b.build_store(slot, val).unwrap();
                    if self.p.traced(&elem) {
                        self.barrier(slot, &elem);
                    }
                }
                None
            }
            Rvalue::ArrayPop(a) => {
                let Ty::Array(elem) = self.operand_ty(fx, a) else {
                    unreachable!("pop from non-array")
                };
                let arr = self.operand(fx, a)?.into_pointer_value();
                let popped = basic(
                    self.b
                        .build_call(self.rt("ppl_array_pop"), &[arr.into()], "")
                        .unwrap(),
                )
                .into_int_value();
                let popped = self
                    .b
                    .build_int_compare(IntPredicate::NE, popped, self.i32().const_zero(), "")
                    .unwrap();
                // The removed element is at the new length (still inside the allocation, which
                // always has room for at least 4 elements, so the load is safe when empty too).
                let len = self.array_len(arr);
                let p = self.array_elem(arr, &elem, len);
                let v = self.b.build_load(self.elem_ty(&elem), p, "").unwrap();
                Some(self.wrap_flagged(&elem, popped, v))
            }
            Rvalue::Variant(eid, v, ops) => {
                let et = self.enums[eid.0 as usize];
                let slot = self.entry_alloca(et.into(), "variant");
                let tag = self.b.build_struct_gep(et, slot, 0, "tag").unwrap();
                self.b
                    .build_store(tag, self.i32().const_int(u64::from(*v), false))
                    .unwrap();
                for (i, o) in ops.iter().enumerate() {
                    if let Some(val) = self.operand(fx, o) {
                        let p = self.variant_field_ptr(*eid, *v, i as u32, slot);
                        self.b.build_store(p, val).unwrap();
                    }
                }
                Some(self.b.build_load(et, slot, "").unwrap())
            }
            Rvalue::New(t) => {
                let Ty::Class(sid, _, _) = t else {
                    unreachable!("new of {t}")
                };
                let size = self.struct_llty(t).size_of().expect("sized class");
                let layout = self.object_layout(t);
                let obj = basic(
                    self.b
                        .build_call(
                            self.rt("ppl_gc_alloc"),
                            &[size.into(), layout.into()],
                            "obj",
                        )
                        .unwrap(),
                )
                .into_pointer_value();
                // The header points at the class's metadata.
                let info = self.class_infos[sid.0 as usize].expect("class info");
                self.b.build_store(obj, info.as_pointer_value()).unwrap();
                Some(obj.into())
            }
            Rvalue::ToIface(o, it) => {
                let t = self.operand_ty(fx, o);
                let v = self.operand(fx, o)?;
                let data = match t {
                    Ty::Class(..) => v.into_pointer_value(),
                    _ => {
                        // Box the struct.
                        let size = self.size_of(&t);
                        let layout = self.layout(&t);
                        let b = basic(
                            self.b
                                .build_call(
                                    self.rt("ppl_gc_alloc"),
                                    &[size.into(), layout.into()],
                                    "box",
                                )
                                .unwrap(),
                        )
                        .into_pointer_value();
                        self.b.build_store(b, v).unwrap();
                        b
                    }
                };
                let fat = self.iface_ty().get_undef();
                let fat = self.b.build_insert_value(fat, data, 0, "").unwrap();
                let table = self.itables[*it as usize].as_pointer_value();
                Some(
                    self.b
                        .build_insert_value(fat, table, 1, "")
                        .unwrap()
                        .as_basic_value_enum(),
                )
            }
            Rvalue::Closure(f, ops) => {
                // The environment: the captures in a heap tuple (none: a null pointer).
                let env = if ops.is_empty() {
                    self.ptr().const_null()
                } else {
                    let tys: Vec<Ty> = ops.iter().map(|o| self.operand_ty(fx, o)).collect();
                    let tt = Ty::Tuple(tys);
                    let st = self.ty(&tt).expect("tuple").into_struct_type();
                    let size = self.size_of(&tt);
                    let layout = self.layout(&tt);
                    let env = basic(
                        self.b
                            .build_call(
                                self.rt("ppl_gc_alloc"),
                                &[size.into(), layout.into()],
                                "env",
                            )
                            .unwrap(),
                    )
                    .into_pointer_value();
                    for (i, o) in ops.iter().enumerate() {
                        if let Some(v) = self.operand(fx, o) {
                            let p = self.b.build_struct_gep(st, env, i as u32, "").unwrap();
                            self.b.build_store(p, v).unwrap();
                        }
                    }
                    env
                };
                let fnptr = self.fns[f.0 as usize].as_global_value().as_pointer_value();
                let v = self.iface_ty().get_undef();
                let v = self.b.build_insert_value(v, fnptr, 0, "").unwrap();
                Some(
                    self.b
                        .build_insert_value(v, env, 1, "")
                        .unwrap()
                        .as_basic_value_enum(),
                )
            }
            Rvalue::BoxNew(t, o) => {
                let size = self.size_of(t);
                let layout = self.layout(t);
                let cell = basic(
                    self.b
                        .build_call(
                            self.rt("ppl_gc_alloc"),
                            &[size.into(), layout.into()],
                            "cell",
                        )
                        .unwrap(),
                )
                .into_pointer_value();
                if let Some(v) = self.operand(fx, o) {
                    self.b.build_store(cell, v).unwrap();
                }
                Some(cell.into())
            }
            Rvalue::Hash(o) => {
                let t = self.operand_ty(fx, o).erase();
                let v = self.operand(fx, o)?;
                let i64t = self.i64();
                let r = match &t {
                    Ty::Str => self
                        .b
                        .build_call(self.rt("ppl_hash_str"), &[v.into()], "")
                        .unwrap(),
                    _ => {
                        let bits = match &t {
                            Ty::Int(it) => self
                                .b
                                .build_int_cast_sign_flag(v.into_int_value(), i64t, it.signed(), "")
                                .unwrap(),
                            Ty::Float(ft) => {
                                let d = match ft {
                                    FloatTy::F64 => v.into_float_value(),
                                    FloatTy::F32 => self
                                        .b
                                        .build_float_ext(
                                            v.into_float_value(),
                                            self.cx.f64_type(),
                                            "",
                                        )
                                        .unwrap(),
                                };
                                self.b.build_bit_cast(d, i64t, "").unwrap().into_int_value()
                            }
                            _ => self
                                .b
                                .build_int_z_extend(v.into_int_value(), i64t, "")
                                .unwrap(),
                        };
                        self.b
                            .build_call(self.rt("ppl_hash_u64"), &[bits.into()], "")
                            .unwrap()
                    }
                };
                Some(basic(r))
            }
            Rvalue::HashCombine(a, b) => {
                let x = self.operand(fx, a)?;
                let y = self.operand(fx, b)?;
                Some(basic(
                    self.b
                        .build_call(self.rt("ppl_hash_combine"), &[x.into(), y.into()], "")
                        .unwrap(),
                ))
            }
            Rvalue::IfaceData(o) => {
                let v = self.operand(fx, o)?;
                Some(
                    self.b
                        .build_extract_value(v.into_struct_value(), 0, "data")
                        .unwrap(),
                )
            }
            Rvalue::SetException(o, t) => {
                let v = self.operand(fx, o)?;
                let tag = self.operand(fx, t)?;
                let slot = self.exception_slot();
                self.b.build_store(slot, v).unwrap();
                let tslot = self.exception_tag_slot();
                self.b.build_store(tslot, tag).unwrap();
                None
            }
            Rvalue::ExceptionTag => {
                let tslot = self.exception_tag_slot();
                Some(self.b.build_load(self.i32(), tslot, "tag").unwrap())
            }
            Rvalue::TakeException(_) => {
                let slot = self.exception_slot();
                let ptr = self.ptr();
                let v = self.b.build_load(ptr, slot, "exc").unwrap();
                self.b.build_store(slot, ptr.const_null()).unwrap();
                Some(v)
            }
            Rvalue::ExceptionPending => {
                let slot = self.exception_slot();
                let ptr = self.ptr();
                let v = self
                    .b
                    .build_load(ptr, slot, "exc")
                    .unwrap()
                    .into_pointer_value();
                Some(self.b.build_is_not_null(v, "pending").unwrap().into())
            }
            Rvalue::InstanceOf(o, sid) => {
                let v = self.operand(fx, o)?;
                let info = self.class_infos[sid.0 as usize].expect("class info");
                let r = basic(
                    self.b
                        .build_call(
                            self.rt("ppl_instance_of"),
                            &[v.into(), info.as_pointer_value().into()],
                            "",
                        )
                        .unwrap(),
                )
                .into_int_value();
                Some(
                    self.b
                        .build_int_compare(IntPredicate::NE, r, self.i32().const_zero(), "")
                        .unwrap()
                        .into(),
                )
            }
            Rvalue::Tag(o) => {
                let v = self.operand(fx, o)?;
                Some(
                    self.b
                        .build_extract_value(v.into_struct_value(), 0, "tag")
                        .unwrap(),
                )
            }
            Rvalue::WrapSome(o, t) => {
                let v = self.operand(fx, o)?;
                let Ty::Nullable(inner) = t else {
                    unreachable!("wrap into {t}")
                };
                let yes = self.cx.bool_type().const_int(1, false);
                Some(self.wrap_flagged(inner, yes, v))
            }
            Rvalue::IsNull(o) => {
                let t = self.operand_ty(fx, o);
                let v = self.operand(fx, o)?;
                Some(self.is_null(&t, v).into())
            }
            Rvalue::Unwrap(o) => {
                let t = self.operand_ty(fx, o);
                let v = self.operand(fx, o)?;
                Some(self.payload(&t, v))
            }
            Rvalue::InBounds(i, n) => {
                let i = self.operand(fx, i)?.into_int_value();
                let n = self.operand(fx, n)?.into_int_value();
                Some(
                    self.b
                        .build_int_compare(IntPredicate::ULT, i, n, "")
                        .unwrap()
                        .into(),
                )
            }
            Rvalue::AddOverflows(a, b) => {
                let Ty::Int(it) = self.operand_ty(fx, a) else {
                    unreachable!("integer addition")
                };
                let x = self.operand(fx, a)?.into_int_value();
                let y = self.operand(fx, b)?.into_int_value();
                let name = format!(
                    "llvm.{}add.with.overflow",
                    if it.signed() { "s" } else { "u" }
                );
                let f = self.intrinsic(&name, &[x.get_type().into()]);
                let r = basic(self.b.build_call(f, &[x.into(), y.into()], "").unwrap())
                    .into_struct_value();
                Some(self.b.build_extract_value(r, 1, "").unwrap())
            }
            Rvalue::StrSlice(o, a, b) => {
                let s = self.operand(fx, o)?;
                let a = self.operand(fx, a)?;
                let b = self.operand(fx, b)?;
                Some(basic(
                    self.b
                        .build_call(
                            self.rt("ppl_str_slice"),
                            &[s.into(), a.into(), b.into()],
                            "",
                        )
                        .unwrap(),
                ))
            }
            Rvalue::StrLen(o) => {
                let s = self.operand(fx, o)?.into_pointer_value();
                Some(self.str_len(s).into())
            }
            Rvalue::StrCharAt(o, i) => {
                let s = self.operand(fx, o)?.into_pointer_value();
                let i = self.operand(fx, i)?.into_int_value();
                // Unsigned compare: negative indices are out of bounds too.
                let len = self.str_len(s);
                let bad = self
                    .b
                    .build_int_compare(IntPredicate::UGE, i, len, "")
                    .unwrap();
                self.check(fx, bad, panics::INDEX, loc);
                let [f, fl, l, c] = self.loc_args(loc);
                let r = self
                    .b
                    .build_call(
                        self.rt("ppl_str_char_at"),
                        &[s.into(), i.into(), f, fl, l, c],
                        "",
                    )
                    .unwrap();
                Some(basic(r))
            }
        }
    }

    /// A `T?` from a presence flag and a value (null when `present` is false).
    fn wrap_flagged(
        &self,
        inner: &Ty,
        present: IntValue<'ctx>,
        v: BasicValueEnum<'ctx>,
    ) -> BasicValueEnum<'ctx> {
        if inner.is_ref() {
            let null = self.ptr().const_null();
            return self.b.build_select(present, v, null.into(), "").unwrap();
        }
        let st = self
            .ty(&Ty::Nullable(Box::new(inner.clone())))
            .expect("nullable")
            .into_struct_type();
        let s = self
            .b
            .build_insert_value(st.get_undef(), present, 0, "")
            .unwrap();
        self.b
            .build_insert_value(s, v, 1, "")
            .unwrap()
            .as_basic_value_enum()
    }

    fn is_null(&self, t: &Ty, v: BasicValueEnum<'ctx>) -> IntValue<'ctx> {
        match t {
            Ty::Nullable(inner) if inner.is_ref() => {
                self.b.build_is_null(v.into_pointer_value(), "").unwrap()
            }
            Ty::Nullable(_) => {
                let flag = self
                    .b
                    .build_extract_value(v.into_struct_value(), 0, "")
                    .unwrap()
                    .into_int_value();
                self.b.build_not(flag, "").unwrap()
            }
            _ => self.cx.bool_type().const_zero(),
        }
    }

    fn payload(&self, t: &Ty, v: BasicValueEnum<'ctx>) -> BasicValueEnum<'ctx> {
        match t {
            Ty::Nullable(inner) if !inner.is_ref() => self
                .b
                .build_extract_value(v.into_struct_value(), 1, "")
                .unwrap(),
            _ => v,
        }
    }

    /// `==` on values of type `t`: field by field for structs, tuples, fixed arrays, ranges.
    fn value_eq(
        &mut self,
        t: &Ty,
        x: BasicValueEnum<'ctx>,
        y: BasicValueEnum<'ctx>,
    ) -> IntValue<'ctx> {
        let parts: Vec<Ty> = match t {
            Ty::Struct(sid, _, args) => self
                .p
                .layout(*sid)
                .fields
                .iter()
                .map(|(_, t)| t.subst(Ty::type_owner(*sid), args))
                .filter(|t| self.ty(t).is_some())
                .collect(),
            Ty::Tuple(ts) => ts.clone(),
            Ty::Range(it) => vec![Ty::Int(*it); 3],
            Ty::Fixed(e, n) => vec![(**e).clone(); *n as usize],
            Ty::Str => return self.str_binary(BinOp::Eq, x, y).into_int_value(),
            // Identity (pointers: the address).
            Ty::Class(..) | Ty::Ref(_) => {
                let (a, b) = (
                    self.b
                        .build_ptr_to_int(x.into_pointer_value(), self.i64(), "")
                        .unwrap(),
                    self.b
                        .build_ptr_to_int(y.into_pointer_value(), self.i64(), "")
                        .unwrap(),
                );
                return self
                    .b
                    .build_int_compare(IntPredicate::EQ, a, b, "")
                    .unwrap();
            }
            Ty::Enum(eid, _) => {
                let f = self.enum_eq_fn(*eid);
                let et = self.enums[eid.0 as usize];
                let a = self.entry_alloca(et.into(), "eq.a");
                let b = self.entry_alloca(et.into(), "eq.b");
                self.b.build_store(a, x).unwrap();
                self.b.build_store(b, y).unwrap();
                return basic(self.b.build_call(f, &[a.into(), b.into()], "").unwrap())
                    .into_int_value();
            }
            // References: the runtime's comparisons are null-safe.
            Ty::Nullable(inner) if inner.is_ref() => return self.value_eq(inner, x, y),
            Ty::Nullable(inner) => {
                // Same presence, and equal payloads if present (a null's payload is zero).
                let (nx, ny) = (self.is_null(t, x), self.is_null(t, y));
                let same = self
                    .b
                    .build_int_compare(IntPredicate::EQ, nx, ny, "")
                    .unwrap();
                let (px, py) = (self.payload(t, x), self.payload(t, y));
                let peq = self.value_eq(inner, px, py);
                let ok = self.b.build_or(nx, peq, "").unwrap();
                return self.b.build_and(same, ok, "").unwrap();
            }
            Ty::Float(_) => {
                return self
                    .b
                    .build_float_compare(
                        FloatPredicate::OEQ,
                        x.into_float_value(),
                        y.into_float_value(),
                        "",
                    )
                    .unwrap();
            }
            _ => {
                return self
                    .b
                    .build_int_compare(IntPredicate::EQ, x.into_int_value(), y.into_int_value(), "")
                    .unwrap();
            }
        };
        let mut acc = self.cx.bool_type().const_int(1, false);
        for (i, pt) in parts.iter().enumerate() {
            let (a, b) = match (x, y) {
                (BasicValueEnum::ArrayValue(a), BasicValueEnum::ArrayValue(b)) => (
                    self.b.build_extract_value(a, i as u32, "").unwrap(),
                    self.b.build_extract_value(b, i as u32, "").unwrap(),
                ),
                _ => (
                    self.b
                        .build_extract_value(x.into_struct_value(), i as u32, "")
                        .unwrap(),
                    self.b
                        .build_extract_value(y.into_struct_value(), i as u32, "")
                        .unwrap(),
                ),
            };
            let eq = self.value_eq(pt, a, b);
            acc = self.b.build_and(acc, eq, "").unwrap();
        }
        acc
    }

    /// `ppl.eq.<Name>(ptr, ptr) -> i1`: same variant and equal payload fields.
    fn enum_eq_fn(&mut self, eid: ppl_sema::ty::EnumId) -> FunctionValue<'ctx> {
        if let Some(f) = self.eq_fns.get(&eid.0) {
            return *f;
        }
        let layout_name = self.p.enum_layout(eid).name.clone();
        let fn_ty = self
            .cx
            .bool_type()
            .fn_type(&[self.ptr().into(), self.ptr().into()], false);
        let f = self.m.add_function(
            &format!("ppl.eq.{layout_name}"),
            fn_ty,
            Some(Linkage::Internal),
        );
        self.eq_fns.insert(eid.0, f);
        let saved = self.b.get_insert_block();
        let entry = self.cx.append_basic_block(f, "entry");
        let yes = self.cx.append_basic_block(f, "equal");
        let no = self.cx.append_basic_block(f, "different");
        self.b.position_at_end(entry);
        let (a, b) = (
            f.get_nth_param(0).unwrap().into_pointer_value(),
            f.get_nth_param(1).unwrap().into_pointer_value(),
        );
        let et = self.enums[eid.0 as usize];
        let load_tag = |g: &Self, p: PointerValue<'ctx>| {
            let tp = g.b.build_struct_gep(et, p, 0, "").unwrap();
            g.b.build_load(g.i32(), tp, "").unwrap().into_int_value()
        };
        let (ta, tb) = (load_tag(self, a), load_tag(self, b));
        let same = self
            .b
            .build_int_compare(IntPredicate::EQ, ta, tb, "")
            .unwrap();
        let dispatch = self.cx.append_basic_block(f, "dispatch");
        self.b.build_conditional_branch(same, dispatch, no).unwrap();
        let variants: Vec<Vec<Ty>> = self
            .p
            .enum_layout(eid)
            .variants
            .iter()
            .map(|v| v.fields.iter().map(|(_, t)| t.clone()).collect())
            .collect();
        let mut cases = Vec::new();
        for (vi, fields) in variants.iter().enumerate() {
            if fields.is_empty() {
                continue;
            }
            let bb = self.cx.append_basic_block(f, "variant");
            cases.push((self.i32().const_int(vi as u64, false), bb));
            self.b.position_at_end(bb);
            let mut acc = self.cx.bool_type().const_int(1, false);
            for (i, ft) in fields.iter().enumerate() {
                let Some(lt) = self.ty(ft) else { continue };
                let pa = self.variant_field_ptr(eid, vi as u32, i as u32, a);
                let pb = self.variant_field_ptr(eid, vi as u32, i as u32, b);
                let x = self.b.build_load(lt, pa, "").unwrap();
                let y = self.b.build_load(lt, pb, "").unwrap();
                let eq = self.value_eq(ft, x, y);
                acc = self.b.build_and(acc, eq, "").unwrap();
            }
            self.b.build_return(Some(&acc)).unwrap();
        }
        self.b.position_at_end(dispatch);
        self.b.build_switch(ta, yes, &cases).unwrap();
        self.b.position_at_end(yes);
        self.b
            .build_return(Some(&self.cx.bool_type().const_int(1, false)))
            .unwrap();
        self.b.position_at_end(no);
        self.b
            .build_return(Some(&self.cx.bool_type().const_zero()))
            .unwrap();
        if let Some(bb) = saved {
            self.b.position_at_end(bb);
        }
        f
    }

    /// The debug text of an enum at `this`, returned from inside the helper `f`:
    /// `Shape.Circle(r: 1)`, `Color.Red`.
    fn debug_enum_body(
        &mut self,
        eid: ppl_sema::ty::EnumId,
        f: FunctionValue<'ctx>,
        this: PointerValue<'ctx>,
    ) {
        let layout = self.p.enum_layout(eid);
        let name = layout.name.clone();
        let variants: Vec<VariantInfo> = layout
            .variants
            .iter()
            .map(|v| (v.name.clone(), v.fields.clone(), v.has_parens))
            .collect();
        let et = self.enums[eid.0 as usize];
        let tp = self.b.build_struct_gep(et, this, 0, "").unwrap();
        let tag = self
            .b
            .build_load(self.i32(), tp, "")
            .unwrap()
            .into_int_value();
        let dispatch = self.b.get_insert_block().unwrap();
        let mut cases = Vec::new();
        let mut default = None;
        for (vi, (vname, fields, parens)) in variants.iter().enumerate() {
            let bb = self.cx.append_basic_block(f, "variant");
            if default.is_none() {
                default = Some(bb);
            }
            cases.push((self.i32().const_int(vi as u64, false), bb));
            self.b.position_at_end(bb);
            let text: BasicValueEnum = if fields.is_empty() && !parens {
                self.str_literal(&format!("{name}.{vname}")).into()
            } else {
                let mut acc: BasicValueEnum = self.str_literal(&format!("{name}.{vname}(")).into();
                for (i, (fname, ft)) in fields.iter().enumerate() {
                    let sep = if i == 0 {
                        format!("{fname}: ")
                    } else {
                        format!(", {fname}: ")
                    };
                    let sep = self.str_literal(&sep).into();
                    acc = self.concat(acc, sep);
                    let p = self.variant_field_ptr(eid, vi as u32, i as u32, this);
                    let t = self.debug_at(ft, p);
                    acc = self.concat(acc, t);
                }
                let close = self.str_literal(")").into();
                self.concat(acc, close)
            };
            self.b.build_return(Some(&text)).unwrap();
        }
        self.b.position_at_end(dispatch);
        match default {
            Some(d) => {
                self.b.build_switch(tag, d, &cases).unwrap();
            }
            None => {
                self.b.build_unreachable().unwrap();
            }
        }
    }

    /// The debug text of a value, via a generated `ppl.debug.<n>` helper.
    fn debug_value(&mut self, t: &Ty, v: BasicValueEnum<'ctx>) -> BasicValueEnum<'ctx> {
        if let Ty::Class(sid, _, _) = t {
            let _ = v;
            let name = format!("<{}>", self.p.layout(*sid).name);
            return self.str_literal(&name).into();
        }
        if let Ty::Nullable(_) = t {
            let tmp = self.entry_alloca(self.ty(t).expect("nullable"), "debug.arg");
            self.b.build_store(tmp, v).unwrap();
            return self.debug_at(t, tmp);
        }
        let f = self.debug_fn(t);
        let tmp = self.entry_alloca(self.ty(t).expect("debuggable type"), "debug.arg");
        self.b.build_store(tmp, v).unwrap();
        basic(self.b.build_call(f, &[tmp.into()], "").unwrap())
    }

    fn concat(&self, a: BasicValueEnum<'ctx>, b: BasicValueEnum<'ctx>) -> BasicValueEnum<'ctx> {
        basic(
            self.b
                .build_call(self.rt("ppl_str_concat"), &[a.into(), b.into()], "")
                .unwrap(),
        )
    }

    /// The debug text of the value at `p` (of type `t`).
    fn debug_at(&mut self, t: &Ty, p: PointerValue<'ctx>) -> BasicValueEnum<'ctx> {
        if let Ty::Class(sid, _, _) = t {
            let name = format!("<{}>", self.p.layout(*sid).name);
            return self.str_literal(&name).into();
        }
        if let Ty::Nullable(inner) = t {
            // "null", or the payload's debug text.
            let lt = self.ty(t).expect("nullable");
            let v = self.b.build_load(lt, p, "").unwrap();
            let is_null = self.is_null(t, v);
            let f = self
                .b
                .get_insert_block()
                .and_then(|b| b.get_parent())
                .expect("inside a function");
            let null_bb = self.cx.append_basic_block(f, "null");
            let some_bb = self.cx.append_basic_block(f, "some");
            let join = self.cx.append_basic_block(f, "join");
            self.b
                .build_conditional_branch(is_null, null_bb, some_bb)
                .unwrap();
            self.b.position_at_end(null_bb);
            let null_text: BasicValueEnum = self.str_literal("null").into();
            self.b.build_unconditional_branch(join).unwrap();
            self.b.position_at_end(some_bb);
            let pp = if inner.is_ref() {
                p
            } else {
                self.b
                    .build_struct_gep(lt.into_struct_type(), p, 1, "")
                    .unwrap()
            };
            let text = self.debug_at(inner, pp);
            let some_end = self.b.get_insert_block().unwrap();
            self.b.build_unconditional_branch(join).unwrap();
            self.b.position_at_end(join);
            let phi = self.b.build_phi(self.ptr(), "").unwrap();
            phi.add_incoming(&[(&null_text, null_bb), (&text, some_end)]);
            return phi.as_basic_value();
        }
        if ppl_sema::format::is_compound(t) {
            let g = self.debug_fn(t);
            return basic(self.b.build_call(g, &[p.into()], "").unwrap());
        }
        let Some(lt) = self.ty(t) else {
            return self.str_literal("").into();
        };
        let v = self.b.build_load(lt, p, "").unwrap();
        let debug = FormatSpec {
            kind: Kind::Debug,
            ..FormatSpec::default()
        };
        self.format(t, v, &debug)
    }

    /// `ppl.debug.<n>(ptr) -> PplStr*` for a struct, tuple, array or range: the same text as
    /// the interpreter's `debug_value`.
    fn debug_fn(&mut self, t: &Ty) -> FunctionValue<'ctx> {
        if let Some(f) = self.debug_fns.get(t) {
            return *f;
        }
        let fn_ty = self.ptr().fn_type(&[self.ptr().into()], false);
        let name = match t {
            Ty::Struct(sid, _, _) => format!("ppl.debug.{}", self.p.layout(*sid).name),
            _ => format!("ppl.debug.{}", self.debug_fns.len()),
        };
        let f = self.m.add_function(&name, fn_ty, Some(Linkage::Internal));
        self.debug_fns.insert(t.clone(), f);
        let saved = self.b.get_insert_block();
        let entry = self.cx.append_basic_block(f, "entry");
        self.b.position_at_end(entry);
        let this = f.get_nth_param(0).unwrap().into_pointer_value();
        let lt = self.ty(t).expect("debuggable type");
        if let Ty::Enum(eid, _) = t {
            self.debug_enum_body(*eid, f, this);
            if let Some(bb) = saved {
                self.b.position_at_end(bb);
            }
            return f;
        }
        let text = match t {
            Ty::Struct(sid, _, args) => {
                let layout_name = self.p.layout(*sid).name.clone();
                let owner = Ty::type_owner(*sid);
                let fields: Vec<(String, Ty)> = self
                    .p
                    .layout(*sid)
                    .fields
                    .iter()
                    .map(|(n, t)| (n.clone(), t.subst(owner, args)))
                    .collect();
                let st = lt.into_struct_type();
                let mut acc: BasicValueEnum = if fields.is_empty() {
                    self.str_literal(&format!("{layout_name} {{}}")).into()
                } else {
                    self.str_literal(&format!("{layout_name} {{ ")).into()
                };
                let mut index = 0;
                for (i, (name, fty)) in fields.iter().enumerate() {
                    let sep = if i == 0 {
                        format!("{name}: ")
                    } else {
                        format!(", {name}: ")
                    };
                    let sep = self.str_literal(&sep).into();
                    acc = self.concat(acc, sep);
                    if self.ty(fty).is_none() {
                        continue;
                    }
                    let p = self.b.build_struct_gep(st, this, index, "").unwrap();
                    index += 1;
                    let text = self.debug_at(fty, p);
                    acc = self.concat(acc, text);
                }
                if !fields.is_empty() {
                    let close = self.str_literal(" }").into();
                    acc = self.concat(acc, close);
                }
                acc
            }
            Ty::Tuple(ts) => {
                let st = lt.into_struct_type();
                let mut acc: BasicValueEnum = self.str_literal("[").into();
                for (i, et) in ts.iter().enumerate() {
                    if i > 0 {
                        let sep = self.str_literal(", ").into();
                        acc = self.concat(acc, sep);
                    }
                    let p = self.b.build_struct_gep(st, this, i as u32, "").unwrap();
                    let text = self.debug_at(et, p);
                    acc = self.concat(acc, text);
                }
                let close = self.str_literal("]").into();
                self.concat(acc, close)
            }
            Ty::Range(it) => {
                let st = lt.into_struct_type();
                let part = |g: &mut Self, i: u32| {
                    let p = g.b.build_struct_gep(st, this, i, "").unwrap();
                    let v = g.b.build_load(g.int_ty(it.bits()), p, "").unwrap();
                    let (name, arg) = g
                        .primitive_arg(&Ty::Int(*it), v, "ppl_str_from")
                        .expect("integer");
                    (v, basic(g.b.build_call(g.rt(&name), &[arg], "").unwrap()))
                };
                let (_, a) = part(self, 0);
                let (_, b) = part(self, 1);
                let (sv, s) = part(self, 2);
                let dots = self.str_literal("..").into();
                let ab = self.concat(a, dots);
                let ab = self.concat(ab, b);
                let open = self.str_literal("(").into();
                let stepped = self.concat(open, ab);
                let mid = self.str_literal(").step(").into();
                let stepped = self.concat(stepped, mid);
                let stepped = self.concat(stepped, s);
                let close = self.str_literal(")").into();
                let stepped = self.concat(stepped, close);
                let one = sv.into_int_value().get_type().const_int(1, false);
                let is_one = self
                    .b
                    .build_int_compare(IntPredicate::EQ, sv.into_int_value(), one, "")
                    .unwrap();
                self.b.build_select(is_one, ab, stepped, "").unwrap()
            }
            Ty::Fixed(et, _) | Ty::Array(et) => {
                let (data, len) = match t {
                    Ty::Fixed(_, n) => (this, self.i64().const_int(*n, false)),
                    _ => {
                        let a = self
                            .b
                            .build_load(self.ptr(), this, "")
                            .unwrap()
                            .into_pointer_value();
                        (self.array_data(a), self.array_len(a))
                    }
                };
                let acc = self.b.build_alloca(self.ptr(), "acc").unwrap();
                let i = self.b.build_alloca(self.i64(), "i").unwrap();
                let open: BasicValueEnum = self.str_literal("[").into();
                self.b.build_store(acc, open).unwrap();
                self.b.build_store(i, self.i64().const_zero()).unwrap();
                let cond = self.cx.append_basic_block(f, "cond");
                let body = self.cx.append_basic_block(f, "body");
                let done = self.cx.append_basic_block(f, "done");
                self.b.build_unconditional_branch(cond).unwrap();
                self.b.position_at_end(cond);
                let iv = self
                    .b
                    .build_load(self.i64(), i, "")
                    .unwrap()
                    .into_int_value();
                let more = self
                    .b
                    .build_int_compare(IntPredicate::ULT, iv, len, "")
                    .unwrap();
                self.b.build_conditional_branch(more, body, done).unwrap();
                self.b.position_at_end(body);
                let first = self
                    .b
                    .build_int_compare(IntPredicate::EQ, iv, self.i64().const_zero(), "")
                    .unwrap();
                let empty: BasicValueEnum = self.str_literal("").into();
                let comma: BasicValueEnum = self.str_literal(", ").into();
                let sep = self.b.build_select(first, empty, comma, "").unwrap();
                let cur = self.b.build_load(self.ptr(), acc, "").unwrap();
                let cur = self.concat(cur, sep);
                let et_ll = self.elem_ty(et);
                let p = unsafe { self.b.build_in_bounds_gep(et_ll, data, &[iv], "").unwrap() };
                let text = self.debug_at(et, p);
                let cur = self.concat(cur, text);
                self.b.build_store(acc, cur).unwrap();
                let next = self
                    .b
                    .build_int_add(iv, self.i64().const_int(1, false), "")
                    .unwrap();
                self.b.build_store(i, next).unwrap();
                self.b.build_unconditional_branch(cond).unwrap();
                self.b.position_at_end(done);
                let cur = self.b.build_load(self.ptr(), acc, "").unwrap();
                let close = self.str_literal("]").into();
                self.concat(cur, close)
            }
            _ => unreachable!("debug helper for {t}"),
        };
        self.b.build_return(Some(&text)).unwrap();
        if let Some(bb) = saved {
            self.b.position_at_end(bb);
        }
        f
    }

    /// A constant `PplFmt` (see `ppl_rt.h`) for a spec.
    fn fmt_global(&self, spec: &FormatSpec) -> PointerValue<'ctx> {
        let i32t = self.i32();
        let c = |v: i64| i32t.const_int(v as u64, true);
        let fields: Vec<BasicValueEnum> = vec![
            c(spec.fill as i64).into(),
            c(match spec.align {
                None => 0,
                Some(Align::Left) => 1,
                Some(Align::Right) => 2,
                Some(Align::Center) => 3,
            })
            .into(),
            c(match spec.sign {
                Sign::Minus => 0,
                Sign::Plus => 1,
                Sign::Space => 2,
            })
            .into(),
            c(spec.alternate as i64).into(),
            c(spec.zero as i64).into(),
            c(spec.width.map_or(-1, i64::from)).into(),
            c(spec.precision.map_or(-1, i64::from)).into(),
            c(match spec.kind {
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
            })
            .into(),
        ];
        let value = self.cx.const_struct(&fields, false);
        let g = self.m.add_global(value.get_type(), None, "ppl.fmt");
        g.set_linkage(Linkage::Private);
        g.set_constant(true);
        g.set_initializer(&value);
        g.as_pointer_value()
    }

    fn format(
        &mut self,
        t: &Ty,
        v: BasicValueEnum<'ctx>,
        spec: &FormatSpec,
    ) -> BasicValueEnum<'ctx> {
        let f = self.fmt_global(spec).into();
        let call = |g: &Self, name: &str, args: &[BasicMetadataValueEnum<'ctx>]| {
            basic(g.b.build_call(g.rt(name), args, "").unwrap())
        };
        match t {
            Ty::Int(it) => {
                let wide = self
                    .b
                    .build_int_cast_sign_flag(v.into_int_value(), self.i64(), it.signed(), "")
                    .unwrap();
                call(
                    self,
                    if it.signed() {
                        "ppl_fmt_i64"
                    } else {
                        "ppl_fmt_u64"
                    },
                    &[wide.into(), f],
                )
            }
            Ty::Float(ft) => {
                let d = match ft {
                    FloatTy::F64 => v.into_float_value(),
                    FloatTy::F32 => self
                        .b
                        .build_float_ext(v.into_float_value(), self.cx.f64_type(), "")
                        .unwrap(),
                };
                let is_f32 = self.i32().const_int(u64::from(*ft == FloatTy::F32), false);
                call(self, "ppl_fmt_f64", &[d.into(), is_f32.into(), f])
            }
            Ty::Str => call(self, "ppl_fmt_str", &[v.into(), f]),
            Ty::Char => call(self, "ppl_fmt_char", &[v.into(), f]),
            Ty::Bool => {
                let w = self
                    .b
                    .build_int_z_extend(v.into_int_value(), self.i32(), "")
                    .unwrap();
                call(self, "ppl_fmt_bool", &[w.into(), f])
            }
            _ => unreachable!("format spec on {t}"),
        }
    }

    /// Loads `PplStr.char_len` (the second field).
    fn str_len(&self, s: PointerValue<'ctx>) -> IntValue<'ctx> {
        let i64t = self.i64();
        let header = self
            .cx
            .struct_type(&[i64t.into(), i64t.into(), i64t.into(), i64t.into()], false);
        let p = self.b.build_struct_gep(header, s, 1, "char_len").unwrap();
        self.b.build_load(i64t, p, "").unwrap().into_int_value()
    }

    fn str_binary(
        &mut self,
        op: BinOp,
        a: BasicValueEnum<'ctx>,
        b: BasicValueEnum<'ctx>,
    ) -> BasicValueEnum<'ctx> {
        let zero = self.i32().const_zero();
        match op {
            BinOp::Add => basic(
                self.b
                    .build_call(self.rt("ppl_str_concat"), &[a.into(), b.into()], "")
                    .unwrap(),
            ),
            BinOp::Eq | BinOp::Ne => {
                let eq = basic(
                    self.b
                        .build_call(self.rt("ppl_str_eq"), &[a.into(), b.into()], "")
                        .unwrap(),
                )
                .into_int_value();
                let pred = if op == BinOp::Eq {
                    IntPredicate::NE
                } else {
                    IntPredicate::EQ
                };
                self.b.build_int_compare(pred, eq, zero, "").unwrap().into()
            }
            _ => {
                let c = basic(
                    self.b
                        .build_call(self.rt("ppl_str_cmp"), &[a.into(), b.into()], "")
                        .unwrap(),
                )
                .into_int_value();
                self.b
                    .build_int_compare(int_predicate(op, true), c, zero, "")
                    .unwrap()
                    .into()
            }
        }
    }

    /// `a op b` with an overflow check (`op` is `add`, `sub` or `mul`).
    fn overflowing(
        &mut self,
        fx: &FnCx<'ctx, '_>,
        op: &str,
        it: IntTy,
        a: IntValue<'ctx>,
        b: IntValue<'ctx>,
        loc: Loc,
    ) -> IntValue<'ctx> {
        let name = format!(
            "llvm.{}{op}.with.overflow",
            if it.signed() { "s" } else { "u" }
        );
        let f = self.intrinsic(&name, &[a.get_type().into()]);
        let r = self.b.build_call(f, &[a.into(), b.into()], "").unwrap();
        let r = match r.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(v) => v.into_struct_value(),
            _ => unreachable!(),
        };
        let value = self
            .b
            .build_extract_value(r, 0, "")
            .unwrap()
            .into_int_value();
        let overflow = self
            .b
            .build_extract_value(r, 1, "")
            .unwrap()
            .into_int_value();
        self.check(fx, overflow, panics::OVERFLOW, loc);
        value
    }

    #[allow(clippy::too_many_arguments)]
    fn binary(
        &mut self,
        fx: &FnCx<'ctx, '_>,
        op: BinOp,
        t: &Ty,
        rhs_ty: &Ty,
        x: BasicValueEnum<'ctx>,
        y: BasicValueEnum<'ctx>,
        loc: Loc,
    ) -> BasicValueEnum<'ctx> {
        use BinOp::*;
        match t {
            Ty::Int(it) => {
                let (a, b) = (x.into_int_value(), y.into_int_value());
                let it = *it;
                let signed = it.signed();
                match op {
                    Add => self.overflowing(fx, "add", it, a, b, loc).into(),
                    Sub => self.overflowing(fx, "sub", it, a, b, loc).into(),
                    Mul => self.overflowing(fx, "mul", it, a, b, loc).into(),
                    Div | Rem => {
                        let ity = a.get_type();
                        let zero = self
                            .b
                            .build_int_compare(IntPredicate::EQ, b, ity.const_zero(), "")
                            .unwrap();
                        self.check(fx, zero, panics::DIV_ZERO, loc);
                        if !signed {
                            return if op == Div {
                                self.b.build_int_unsigned_div(a, b, "").unwrap().into()
                            } else {
                                self.b.build_int_unsigned_rem(a, b, "").unwrap().into()
                            };
                        }
                        let minus_one = ity.const_all_ones();
                        let b_m1 = self
                            .b
                            .build_int_compare(IntPredicate::EQ, b, minus_one, "")
                            .unwrap();
                        if op == Div {
                            let min = ity.const_int(1u64 << (it.bits() - 1), false);
                            let a_min = self
                                .b
                                .build_int_compare(IntPredicate::EQ, a, min, "")
                                .unwrap();
                            let both = self.b.build_and(a_min, b_m1, "").unwrap();
                            self.check(fx, both, panics::OVERFLOW, loc);
                            self.b.build_int_signed_div(a, b, "").unwrap().into()
                        } else {
                            // `MIN % -1` is UB in LLVM; the result is 0 (as `x % 1`).
                            let safe = self
                                .b
                                .build_select(b_m1, ity.const_int(1, false), b, "")
                                .unwrap()
                                .into_int_value();
                            self.b.build_int_signed_rem(a, safe, "").unwrap().into()
                        }
                    }
                    Pow => self.int_pow(fx, it, a, b, loc).into(),
                    Shl | Shr | UShr => {
                        let Ty::Int(rit) = rhs_ty else { unreachable!() };
                        // Compare the amount as unsigned, so negative amounts fail too.
                        let bits = b.get_type().const_int(it.bits() as u64, false);
                        let bad = self
                            .b
                            .build_int_compare(IntPredicate::UGE, b, bits, "")
                            .unwrap();
                        let _ = rit;
                        self.check(fx, bad, panics::SHIFT, loc);
                        let amount = self
                            .b
                            .build_int_cast_sign_flag(b, a.get_type(), false, "")
                            .unwrap();
                        match op {
                            Shl => self.b.build_left_shift(a, amount, "").unwrap().into(),
                            Shr => self
                                .b
                                .build_right_shift(a, amount, signed, "")
                                .unwrap()
                                .into(),
                            _ => self
                                .b
                                .build_right_shift(a, amount, false, "")
                                .unwrap()
                                .into(),
                        }
                    }
                    BitAnd => self.b.build_and(a, b, "").unwrap().into(),
                    BitOr => self.b.build_or(a, b, "").unwrap().into(),
                    BitXor => self.b.build_xor(a, b, "").unwrap().into(),
                    _ => {
                        let pred = int_predicate(op, signed);
                        self.b.build_int_compare(pred, a, b, "").unwrap().into()
                    }
                }
            }
            Ty::Float(ft) => {
                let (a, b) = (x.into_float_value(), y.into_float_value());
                match op {
                    Add => self.b.build_float_add(a, b, "").unwrap().into(),
                    Sub => self.b.build_float_sub(a, b, "").unwrap().into(),
                    Mul => self.b.build_float_mul(a, b, "").unwrap().into(),
                    Div => self.b.build_float_div(a, b, "").unwrap().into(),
                    Rem => self.b.build_float_rem(a, b, "").unwrap().into(),
                    Pow => {
                        let f = self.intrinsic("llvm.pow", &[a.get_type().into()]);
                        let _ = ft;
                        match self
                            .b
                            .build_call(f, &[a.into(), b.into()], "")
                            .unwrap()
                            .try_as_basic_value()
                        {
                            inkwell::values::ValueKind::Basic(v) => v,
                            _ => unreachable!(),
                        }
                    }
                    _ => {
                        let pred = match op {
                            Eq => FloatPredicate::OEQ,
                            Ne => FloatPredicate::UNE,
                            Lt => FloatPredicate::OLT,
                            Le => FloatPredicate::OLE,
                            Gt => FloatPredicate::OGT,
                            Ge => FloatPredicate::OGE,
                            _ => unreachable!("type-checked float op"),
                        };
                        self.b.build_float_compare(pred, a, b, "").unwrap().into()
                    }
                }
            }
            Ty::Str => self.str_binary(op, x, y),
            Ty::Struct(..)
            | Ty::Class(..)
            | Ty::Ref(_)
            | Ty::Enum(..)
            | Ty::Tuple(_)
            | Ty::Fixed(..)
            | Ty::Range(_)
            | Ty::Nullable(_) => {
                let eq = self.value_eq(t, x, y);
                if op == BinOp::Ne {
                    self.b.build_not(eq, "").unwrap().into()
                } else {
                    eq.into()
                }
            }
            Ty::Bool if matches!(op, BitAnd | BitOr | BitXor) => {
                let (a, b) = (x.into_int_value(), y.into_int_value());
                match op {
                    BitAnd => self.b.build_and(a, b, "").unwrap().into(),
                    BitOr => self.b.build_or(a, b, "").unwrap().into(),
                    _ => self.b.build_xor(a, b, "").unwrap().into(),
                }
            }
            Ty::Bool | Ty::Char => {
                let pred = int_predicate(op, false);
                self.b
                    .build_int_compare(pred, x.into_int_value(), y.into_int_value(), "")
                    .unwrap()
                    .into()
            }
            _ => unreachable!("binary {op:?} on {t}"),
        }
    }

    fn int_pow(
        &mut self,
        fx: &FnCx<'ctx, '_>,
        it: IntTy,
        a: IntValue<'ctx>,
        b: IntValue<'ctx>,
        loc: Loc,
    ) -> IntValue<'ctx> {
        let i64t = self.i64();
        let signed = it.signed();
        if signed {
            let neg = self
                .b
                .build_int_compare(IntPredicate::SLT, b, b.get_type().const_zero(), "")
                .unwrap();
            self.check(fx, neg, panics::NEG_EXPONENT, loc);
        }
        let a64 = self
            .b
            .build_int_cast_sign_flag(a, i64t, signed, "")
            .unwrap();
        let b64 = self
            .b
            .build_int_cast_sign_flag(b, i64t, signed, "")
            .unwrap();
        let flag = self.entry_alloca(self.i32().into(), "pow.overflow");
        let f = self.rt(if signed { "ppl_ipow_s" } else { "ppl_ipow_u" });
        let bits = self.i32().const_int(it.bits() as u64, false);
        let r = self
            .b
            .build_call(f, &[a64.into(), b64.into(), bits.into(), flag.into()], "")
            .unwrap();
        let r = match r.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(v) => v.into_int_value(),
            _ => unreachable!(),
        };
        let ov = self
            .b
            .build_load(self.i32(), flag, "")
            .unwrap()
            .into_int_value();
        let bad = self
            .b
            .build_int_compare(IntPredicate::NE, ov, self.i32().const_zero(), "")
            .unwrap();
        self.check(fx, bad, panics::OVERFLOW, loc);
        self.b
            .build_int_cast_sign_flag(r, a.get_type(), signed, "")
            .unwrap()
    }

    fn cast(
        &mut self,
        fx: &FnCx<'ctx, '_>,
        v: BasicValueEnum<'ctx>,
        from: &Ty,
        to: &Ty,
        loc: Loc,
    ) -> BasicValueEnum<'ctx> {
        let from_int = match from {
            Ty::Int(it) => Some(*it),
            Ty::Char => Some(IntTy::U32),
            _ => None,
        };
        match (from_int, from, to) {
            (Some(fi), _, Ty::Int(ti)) => {
                self.int_to_int(fx, v.into_int_value(), fi, *ti, loc).into()
            }
            (Some(fi), _, Ty::Char) => {
                let x = v.into_int_value();
                if fi.bits() > 8 {
                    let x32 = self
                        .b
                        .build_int_cast_sign_flag(x, self.i32(), fi.signed(), "")
                        .unwrap();
                    let i32t = self.i32();
                    let too_big = self
                        .b
                        .build_int_compare(
                            IntPredicate::UGT,
                            x32,
                            i32t.const_int(0x10FFFF, false),
                            "",
                        )
                        .unwrap();
                    let lo = self
                        .b
                        .build_int_compare(
                            IntPredicate::UGE,
                            x32,
                            i32t.const_int(0xD800, false),
                            "",
                        )
                        .unwrap();
                    let hi = self
                        .b
                        .build_int_compare(
                            IntPredicate::ULE,
                            x32,
                            i32t.const_int(0xDFFF, false),
                            "",
                        )
                        .unwrap();
                    let surrogate = self.b.build_and(lo, hi, "").unwrap();
                    let bad = self.b.build_or(too_big, surrogate, "").unwrap();
                    self.check(fx, bad, panics::CHAR_CAST, loc);
                    x32.into()
                } else {
                    self.b.build_int_z_extend(x, self.i32(), "").unwrap().into()
                }
            }
            (Some(fi), _, Ty::Float(ft)) => {
                let fty = if *ft == FloatTy::F32 {
                    self.cx.f32_type()
                } else {
                    self.cx.f64_type()
                };
                let x = v.into_int_value();
                if fi.signed() {
                    self.b.build_signed_int_to_float(x, fty, "").unwrap().into()
                } else {
                    self.b
                        .build_unsigned_int_to_float(x, fty, "")
                        .unwrap()
                        .into()
                }
            }
            (None, Ty::Float(_), Ty::Int(ti)) => {
                let x = v.into_float_value();
                let fty = x.get_type();
                let trunc = self.intrinsic("llvm.trunc", &[fty.into()]);
                let t = match self
                    .b
                    .build_call(trunc, &[x.into()], "")
                    .unwrap()
                    .try_as_basic_value()
                {
                    inkwell::values::ValueKind::Basic(v) => v.into_float_value(),
                    _ => unreachable!(),
                };
                // Exact power-of-two bounds: min ≤ t < max + 1 (false for NaN).
                let lo = fty.const_float(ti.min() as f64);
                let hi = fty.const_float((ti.max() + 1) as f64);
                let ge = self
                    .b
                    .build_float_compare(FloatPredicate::OGE, t, lo, "")
                    .unwrap();
                let lt = self
                    .b
                    .build_float_compare(FloatPredicate::OLT, t, hi, "")
                    .unwrap();
                let ok = self.b.build_and(ge, lt, "").unwrap();
                let bad = self.b.build_not(ok, "").unwrap();
                self.check(fx, bad, panics::FLOAT_CAST, loc);
                let ity = self.int_ty(ti.bits());
                if ti.signed() {
                    self.b.build_float_to_signed_int(x, ity, "").unwrap().into()
                } else {
                    self.b
                        .build_float_to_unsigned_int(x, ity, "")
                        .unwrap()
                        .into()
                }
            }
            (None, Ty::Float(_), Ty::Float(ft)) => {
                let fty = if *ft == FloatTy::F32 {
                    self.cx.f32_type()
                } else {
                    self.cx.f64_type()
                };
                self.b
                    .build_float_cast(v.into_float_value(), fty, "")
                    .unwrap()
                    .into()
            }
            _ if from == to => v,
            _ => unreachable!("cast from {from} to {to}"),
        }
    }

    /// Integer conversion with a range check, done in 128 bits.
    fn int_to_int(
        &mut self,
        fx: &FnCx<'ctx, '_>,
        x: IntValue<'ctx>,
        fi: IntTy,
        ti: IntTy,
        loc: Loc,
    ) -> IntValue<'ctx> {
        let target = self.int_ty(ti.bits());
        if ti.min() <= fi.min() && fi.max() <= ti.max() {
            return self
                .b
                .build_int_cast_sign_flag(x, target, fi.signed(), "")
                .unwrap();
        }
        let i128t = self.cx.i128_type();
        let wide = self
            .b
            .build_int_cast_sign_flag(x, i128t, fi.signed(), "")
            .unwrap();
        let c = |v: i128| i128t.const_int_arbitrary_precision(&[v as u64, (v >> 64) as u64]);
        let lo = self
            .b
            .build_int_compare(IntPredicate::SLT, wide, c(ti.min()), "")
            .unwrap();
        let hi = self
            .b
            .build_int_compare(IntPredicate::SGT, wide, c(ti.max()), "")
            .unwrap();
        let bad = self.b.build_or(lo, hi, "").unwrap();
        self.check(fx, bad, panics::INT_CAST, loc);
        self.b.build_int_truncate(wide, target, "").unwrap()
    }

    // ------------------------------------------------------------ terminators

    fn terminator(&mut self, fx: &mut FnCx<'ctx, '_>, t: &Terminator) {
        let bb = |b: &BlockId| fx.blocks[b.0 as usize];
        match t {
            Terminator::Goto(target) => {
                self.b.build_unconditional_branch(bb(target)).unwrap();
            }
            Terminator::Branch { cond, then, else_ } => {
                let (then, else_) = (bb(then), bb(else_));
                let c = self
                    .operand(fx, cond)
                    .expect("bool condition")
                    .into_int_value();
                self.b.build_conditional_branch(c, then, else_).unwrap();
            }
            Terminator::Return(v) => {
                let v = v.as_ref().and_then(|o| self.operand(fx, o));
                self.gc_pop(fx);
                match v {
                    Some(v) => self.b.build_return(Some(&v as &dyn BasicValue)).unwrap(),
                    None => self.b.build_return(None).unwrap(),
                };
            }
            Terminator::Call {
                callee,
                args,
                dest,
                next,
                loc,
            } => {
                let next = bb(next);
                match callee {
                    Callee::Fn(id) => {
                        let vals: Vec<BasicMetadataValueEnum> = args
                            .iter()
                            .filter_map(|a| self.operand(fx, a))
                            .map(Into::into)
                            .collect();
                        let r = self
                            .b
                            .build_call(self.fns[id.0 as usize], &vals, "")
                            .unwrap();
                        if let (Some(d), inkwell::values::ValueKind::Basic(v)) =
                            (dest, r.try_as_basic_value())
                        {
                            self.store(fx, d, v);
                        }
                    }
                    Callee::Indirect => {
                        let Ty::Fn(ps, r, _) = self.operand_ty(fx, &args[0]) else {
                            unreachable!("indirect call of a non-function")
                        };
                        let vals: Vec<BasicMetadataValueEnum> = args
                            .iter()
                            .filter_map(|a| self.operand(fx, a))
                            .map(Into::into)
                            .collect();
                        let closure = vals[0].into_struct_value();
                        let fnptr = self
                            .b
                            .build_extract_value(closure, 0, "fn")
                            .unwrap()
                            .into_pointer_value();
                        let env = self.b.build_extract_value(closure, 1, "env").unwrap();
                        let mut call_args: Vec<BasicMetadataValueEnum> = vec![env.into()];
                        call_args.extend(vals[1..].iter().copied());
                        let mut ptys: Vec<BasicMetadataTypeEnum> = vec![self.ptr().into()];
                        ptys.extend(
                            ps.iter()
                                .filter_map(|t| self.ty(t))
                                .map(BasicMetadataTypeEnum::from),
                        );
                        let fn_ty = match self.ty(&r) {
                            Some(t) => t.fn_type(&ptys, false),
                            None => self.cx.void_type().fn_type(&ptys, false),
                        };
                        let res = self
                            .b
                            .build_indirect_call(fn_ty, fnptr, &call_args, "")
                            .unwrap();
                        if let (Some(d), inkwell::values::ValueKind::Basic(v)) =
                            (dest, res.try_as_basic_value())
                        {
                            self.store(fx, d, v);
                        }
                    }
                    Callee::Iface { slot, decl } => {
                        let vals: Vec<BasicMetadataValueEnum> = args
                            .iter()
                            .filter_map(|a| self.operand(fx, a))
                            .map(Into::into)
                            .collect();
                        let fat = vals[0].into_struct_value();
                        let table = self
                            .b
                            .build_extract_value(fat, 1, "itable")
                            .unwrap()
                            .into_pointer_value();
                        let idx = self.i64().const_int(u64::from(*slot), false);
                        let slot_ptr =
                            unsafe { self.b.build_gep(self.ptr(), table, &[idx], "slot").unwrap() };
                        let fnptr = self
                            .b
                            .build_load(self.ptr(), slot_ptr, "method")
                            .unwrap()
                            .into_pointer_value();
                        let fn_ty = self.fns[decl.0 as usize].get_type();
                        let r = self.b.build_indirect_call(fn_ty, fnptr, &vals, "").unwrap();
                        if let (Some(d), inkwell::values::ValueKind::Basic(v)) =
                            (dest, r.try_as_basic_value())
                        {
                            self.store(fx, d, v);
                        }
                    }
                    Callee::Virtual { slot, decl } => {
                        let vals: Vec<BasicMetadataValueEnum> = args
                            .iter()
                            .filter_map(|a| self.operand(fx, a))
                            .map(Into::into)
                            .collect();
                        // header → class info → vtable[slot] (info word 0 is the base).
                        let obj = vals[0].into_pointer_value();
                        let info = self
                            .b
                            .build_load(self.ptr(), obj, "info")
                            .unwrap()
                            .into_pointer_value();
                        let idx = self.i64().const_int(1 + u64::from(*slot), false);
                        let slot_ptr =
                            unsafe { self.b.build_gep(self.ptr(), info, &[idx], "slot").unwrap() };
                        let fnptr = self
                            .b
                            .build_load(self.ptr(), slot_ptr, "method")
                            .unwrap()
                            .into_pointer_value();
                        let fn_ty = self.fns[decl.0 as usize].get_type();
                        let r = self.b.build_indirect_call(fn_ty, fnptr, &vals, "").unwrap();
                        if let (Some(d), inkwell::values::ValueKind::Basic(v)) =
                            (dest, r.try_as_basic_value())
                        {
                            self.store(fx, d, v);
                        }
                    }
                    Callee::Builtin(b @ (Builtin::Print | Builtin::Println)) => {
                        for a in args {
                            let t = self.operand_ty(fx, a);
                            if let Some(v) = self.operand(fx, a) {
                                self.print(&t, v);
                            }
                        }
                        if *b == Builtin::Println {
                            self.b
                                .build_call(self.rt("ppl_print_newline"), &[], "")
                                .unwrap();
                        }
                    }
                    Callee::Builtin(Builtin::ArenaEnter) => {
                        self.b
                            .build_call(self.rt("ppl_arena_enter"), &[], "")
                            .unwrap();
                    }
                    Callee::Builtin(Builtin::ArenaExit) => {
                        let args = self.loc_args(*loc);
                        self.b
                            .build_call(self.rt("ppl_arena_exit"), &args, "")
                            .unwrap();
                    }
                    Callee::Builtin(Builtin::ArenaExitRescue) => {
                        let slot = self.exception_slot();
                        let mut args = vec![slot.into()];
                        args.extend(self.loc_args(*loc));
                        self.b
                            .build_call(self.rt("ppl_arena_exit_rescue"), &args, "")
                            .unwrap();
                    }
                    Callee::Builtin(b @ (Builtin::RawArenaNew | Builtin::RawArenaFree)) => {
                        let h = self.operand(fx, &args[0]).expect("an int");
                        let name = if *b == Builtin::RawArenaNew {
                            "ppl_raw_arena_new"
                        } else {
                            "ppl_raw_arena_free"
                        };
                        let r = self.b.build_call(self.rt(name), &[h.into()], "").unwrap();
                        if let (Some(d), inkwell::values::ValueKind::Basic(v)) =
                            (dest, r.try_as_basic_value())
                        {
                            self.store(fx, d, v);
                        }
                    }
                    Callee::Builtin(Builtin::RefId) => {
                        let x = self.operand(fx, &args[0]).expect("a reference");
                        let r = self
                            .b
                            .build_ptr_to_int(x.into_pointer_value(), self.i64(), "id")
                            .unwrap();
                        if let Some(d) = dest {
                            self.store(fx, d, r.into());
                        }
                    }
                    Callee::Builtin(Builtin::GcStat) => {
                        let i = self.operand(fx, &args[0]).expect("int");
                        let r = basic(
                            self.b
                                .build_call(self.rt("ppl_gc_stat"), &[i.into()], "")
                                .unwrap(),
                        );
                        if let Some(d) = dest {
                            self.store(fx, d, r);
                        }
                    }
                    Callee::Builtin(
                        b @ (Builtin::GcCollect
                        | Builtin::GcCollectYoung
                        | Builtin::GcPause
                        | Builtin::GcResume
                        | Builtin::GcDumpRoots),
                    ) => {
                        let name = match b {
                            Builtin::GcCollect => "ppl_gc_collect",
                            Builtin::GcCollectYoung => "ppl_gc_collect_young",
                            Builtin::GcPause => "ppl_gc_pause",
                            Builtin::GcResume => "ppl_gc_resume",
                            _ => "ppl_gc_dump_roots",
                        };
                        self.b.build_call(self.rt(name), &[], "").unwrap();
                    }
                    Callee::Builtin(_) => unreachable!("lowered to control flow"),
                }
                self.b.build_unconditional_branch(next).unwrap();
            }
            Terminator::Guard { body, handler } => {
                // A jmp_buf (large enough for every supported target) in this frame.
                let buf_ty = self.i64().array_type(64);
                let buf = self.entry_alloca(buf_ty.into(), "guard");
                self.b
                    .build_call(self.rt("ppl_guard_push"), &[buf.into()], "")
                    .unwrap();
                let r = basic(
                    self.b
                        .build_call(self.rt("_setjmp"), &[buf.into()], "")
                        .unwrap(),
                )
                .into_int_value();
                let normal = self
                    .b
                    .build_int_compare(IntPredicate::EQ, r, self.i32().const_zero(), "")
                    .unwrap();
                self.b
                    .build_conditional_branch(
                        normal,
                        fx.blocks[body.0 as usize],
                        fx.blocks[handler.0 as usize],
                    )
                    .unwrap();
            }
            Terminator::Unguard(next) => {
                self.b
                    .build_call(self.rt("ppl_guard_pop"), &[], "")
                    .unwrap();
                self.b
                    .build_unconditional_branch(fx.blocks[next.0 as usize])
                    .unwrap();
            }
            Terminator::Resume => {
                self.b.build_call(self.rt("ppl_resume"), &[], "").unwrap();
                self.b.build_unreachable().unwrap();
            }
            Terminator::Uncaught { class, msg } => {
                let c = self.operand(fx, class).expect("class name");
                let m = self.operand(fx, msg).expect("message");
                self.b
                    .build_call(self.rt("ppl_uncaught"), &[c.into(), m.into()], "")
                    .unwrap();
                self.b.build_unreachable().unwrap();
            }
            Terminator::Panic { msg, loc } => {
                let m = self.operand(fx, msg).expect("string message");
                let [f, fl, l, c] = self.loc_args(*loc);
                self.b
                    .build_call(self.rt("ppl_panic_str"), &[m.into(), f, fl, l, c], "")
                    .unwrap();
                self.b.build_unreachable().unwrap();
            }
            Terminator::Unreachable => {
                self.b.build_unreachable().unwrap();
            }
            Terminator::TailCall { callee, args, .. } => {
                let vals: Vec<BasicMetadataValueEnum> = args
                    .iter()
                    .filter_map(|a| self.operand(fx, a))
                    .map(Into::into)
                    .collect();
                // The callee gets the arguments in registers and roots them itself.
                self.gc_pop(fx);
                let call = self
                    .b
                    .build_call(self.fns[callee.0 as usize], &vals, "")
                    .unwrap();
                // Guaranteed: LLVM refuses a `musttail` it can't honor.
                call.set_tail_call_kind(
                    inkwell::values::LLVMTailCallKind::LLVMTailCallKindMustTail,
                );
                match call.try_as_basic_value() {
                    inkwell::values::ValueKind::Basic(v) => {
                        self.b.build_return(Some(&v as &dyn BasicValue)).unwrap()
                    }
                    _ => self.b.build_return(None).unwrap(),
                };
            }
        }
    }

    /// The runtime function `<prefix>_<kind>` for a primitive, and the argument it takes
    /// (integers widened to 64 bits, `bool` as `i32`).
    fn primitive_arg(
        &self,
        t: &Ty,
        v: BasicValueEnum<'ctx>,
        prefix: &str,
    ) -> Option<(String, BasicMetadataValueEnum<'ctx>)> {
        let (kind, arg): (&str, BasicMetadataValueEnum) = match t {
            Ty::Int(it) => {
                let wide = self
                    .b
                    .build_int_cast_sign_flag(v.into_int_value(), self.i64(), it.signed(), "")
                    .unwrap();
                (if it.signed() { "i64" } else { "u64" }, wide.into())
            }
            Ty::Float(FloatTy::F64) => ("f64", v.into()),
            Ty::Float(FloatTy::F32) => ("f32", v.into()),
            Ty::Bool => {
                let w = self
                    .b
                    .build_int_z_extend(v.into_int_value(), self.i32(), "")
                    .unwrap();
                ("bool", w.into())
            }
            Ty::Char => ("char", v.into()),
            Ty::Str => ("string", v.into()),
            _ => return None,
        };
        Some((format!("{prefix}_{kind}"), arg))
    }

    fn print(&mut self, t: &Ty, v: BasicValueEnum<'ctx>) {
        if let Some((name, arg)) = self.primitive_arg(t, v, "ppl_print") {
            self.b.build_call(self.rt(&name), &[arg], "").unwrap();
        }
    }
}

/// The value of a call that returns one.
fn basic(call: inkwell::values::CallSiteValue<'_>) -> BasicValueEnum<'_> {
    match call.try_as_basic_value() {
        inkwell::values::ValueKind::Basic(v) => v,
        _ => unreachable!("call returns void"),
    }
}

fn int_predicate(op: BinOp, signed: bool) -> IntPredicate {
    match (op, signed) {
        (BinOp::Eq, _) => IntPredicate::EQ,
        (BinOp::Ne, _) => IntPredicate::NE,
        (BinOp::Lt, true) => IntPredicate::SLT,
        (BinOp::Le, true) => IntPredicate::SLE,
        (BinOp::Gt, true) => IntPredicate::SGT,
        (BinOp::Ge, true) => IntPredicate::SGE,
        (BinOp::Lt, false) => IntPredicate::ULT,
        (BinOp::Le, false) => IntPredicate::ULE,
        (BinOp::Gt, false) => IntPredicate::UGT,
        (BinOp::Ge, false) => IntPredicate::UGE,
        _ => unreachable!("not a comparison: {op:?}"),
    }
}
