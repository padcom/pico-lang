//! LLVM code generation for ppl (via inkwell).
//!
//! Every MIR local becomes an `alloca` (LLVM's mem2reg cleans this up when optimizing).
//! Integer arithmetic uses the `llvm.*.with.overflow` intrinsics and branches to a call to
//! `ppl_panic` on overflow; division, shifts and conversions get explicit checks. Strings are
//! pointers to the runtime's `PplStr`; literals are writable globals with the same layout.
//!
//! Functions are named `ppl.<name>` with internal linkage, so user names never clash with C
//! symbols; the entry function is the external `ppl_entry` that the runtime's `main` calls.

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
    BinOp, BlockId, Callee, Const, Loc, Operand, Place, Program, Rvalue, Terminator, UnOp, panics,
};
use ppl_sema::format::{Align, FormatSpec, Kind, Sign};
use ppl_sema::hir::Builtin;
use ppl_sema::{FloatTy, IntTy, Ty};
use std::collections::HashMap;
use std::path::Path;

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
    };
    g.declare();
    for i in 0..program.fns.len() {
        g.function(i);
    }
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
}

/// Per-function state.
struct FnCx<'ctx, 'p> {
    func: &'p ppl_mir::mir::Function,
    llfn: FunctionValue<'ctx>,
    locals: Vec<Option<PointerValue<'ctx>>>,
    blocks: Vec<BasicBlock<'ctx>>,
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
            Ty::Str => self.ptr().into(),
            Ty::Void | Ty::Never | Ty::Error | Ty::Null | Ty::Nullable(_) => return None,
        })
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
            "ppl_ipow_s" | "ppl_ipow_u" => {
                i64t.fn_type(&[i64t.into(), i64t.into(), i32t.into(), ptr.into()], false)
            }
            _ => unreachable!("unknown runtime function {name}"),
        };
        let f = self.m.add_function(name, fn_ty, Some(Linkage::External));
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
        let locals: Vec<Option<PointerValue>> = func
            .locals
            .iter()
            .enumerate()
            .map(|(i, l)| {
                self.ty(&l.ty).map(|t| {
                    let name = l.name.clone().unwrap_or_else(|| format!("t{i}"));
                    self.b.build_alloca(t, &name).unwrap()
                })
            })
            .collect();
        let mut arg = 0;
        for p in &func.params {
            if let Some(slot) = locals[p.0 as usize] {
                let v = llfn.get_nth_param(arg).unwrap();
                arg += 1;
                self.b.build_store(slot, v).unwrap();
            }
        }
        self.b.build_unconditional_branch(blocks[0]).unwrap();
        let mut fx = FnCx {
            func,
            llfn,
            locals,
            blocks,
        };
        for (i, bb) in func.blocks.iter().enumerate() {
            self.b.position_at_end(fx.blocks[i]);
            for s in &bb.stmts {
                let v = self.rvalue(&mut fx, &s.rvalue, s.loc);
                if let Some(v) = v {
                    self.store(&fx, &s.place, v);
                }
            }
            self.terminator(&mut fx, &bb.term);
        }
    }

    fn place_ptr(&self, fx: &FnCx<'ctx, '_>, p: &Place) -> Option<PointerValue<'ctx>> {
        match p {
            Place::Local(l) => fx.locals[l.0 as usize],
            Place::Global(g) => self.globals[g.0 as usize].map(|g| g.as_pointer_value()),
        }
    }

    fn place_ty(&self, fx: &FnCx<'ctx, '_>, p: &Place) -> Ty {
        match p {
            Place::Local(l) => fx.func.local_ty(*l).clone(),
            Place::Global(g) => self.p.globals[g.0 as usize].ty.clone(),
        }
    }

    fn store(&self, fx: &FnCx<'ctx, '_>, p: &Place, v: BasicValueEnum<'ctx>) {
        if let Some(ptr) = self.place_ptr(fx, p) {
            self.b.build_store(ptr, v).unwrap();
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
                Some(self.format(&t, v, spec))
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
        let flag = self.b.build_alloca(self.i32(), "pow.overflow").unwrap();
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
                loc: _,
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
                    Callee::Builtin(_) => unreachable!("lowered to control flow"),
                }
                self.b.build_unconditional_branch(next).unwrap();
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
