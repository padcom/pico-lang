//! GC support in generated code (spec §9): type maps (`PplLayout`), shadow-stack frames,
//! safepoints, write barriers and the table of global roots. See `runtime/include/ppl_rt.h`.

use super::*;

/// A reference word that `gc.dumpRoots()` ignores (`PPL_REF_HIDDEN`).
const REF_HIDDEN: u32 = 0x8000_0000;

impl<'ctx> Gen<'ctx, '_> {
    /// Offsets of the parts of a C-layout struct, as [`struct_layout`] places them.
    pub(crate) fn part_offsets(parts: &[(u64, u64)]) -> Vec<u64> {
        let mut offset: u64 = 0;
        parts
            .iter()
            .map(|(s, a)| {
                let at = offset.next_multiple_of(*a);
                offset = at + s;
                at
            })
            .collect()
    }

    /// The offsets of the words of a `t` value at `base` that may hold references.
    fn ref_words(&self, t: &Ty, base: u64, out: &mut Vec<u32>) {
        match t {
            Ty::Str | Ty::Array(_) | Ty::Class(..) | Ty::Ref(_) => out.push(base as u32),
            // `{ data, itable }`.
            Ty::Iface(..) => out.push(base as u32),
            // `{ fn, env }`: a closure without captures has no environment, which the
            // interpreter can't tell apart, so roots dumps leave environments out.
            Ty::Fn(..) => out.push((base + 8) as u32 | REF_HIDDEN),
            Ty::Nullable(inner) if inner.is_ref() => self.ref_words(inner, base, out),
            Ty::Nullable(inner) => {
                let at = Self::part_offsets(&[(1, 1), self.size_align(inner)])[1];
                self.ref_words(inner, base + at, out);
            }
            Ty::Struct(sid, _, args) => {
                let owner = Ty::type_owner(*sid);
                let fields: Vec<Ty> = self
                    .p
                    .layout(*sid)
                    .fields
                    .iter()
                    .map(|(_, f)| f.subst(owner, args))
                    .filter(|f| self.ty(f).is_some())
                    .collect();
                self.parts_ref_words(&fields, base, out);
            }
            Ty::Tuple(ts) => self.parts_ref_words(ts, base, out),
            Ty::Fixed(e, n) if self.p.traced(e) => {
                let size = self.size_align(e).0;
                for i in 0..*n {
                    self.ref_words(e, base + i * size, out);
                }
            }
            // The payload words depend on the variant: all of them are scanned, and the
            // collector ignores words that aren't heap pointers.
            Ty::Enum(..) if self.p.traced(t) => {
                let words = (self.size_align(t).0 - 8) / 8;
                for w in 0..words {
                    out.push((base + 8 + 8 * w) as u32 | REF_HIDDEN);
                }
            }
            Ty::Lits(..)
            | Ty::Union(..)
            | Ty::Variant(..)
            | Ty::Distinct(..)
            | Ty::Immutable(_) => self.ref_words(&t.erase(), base, out),
            _ => {}
        }
    }

    fn parts_ref_words(&self, parts: &[Ty], base: u64, out: &mut Vec<u32>) {
        let sizes: Vec<(u64, u64)> = parts.iter().map(|t| self.size_align(t)).collect();
        for (t, at) in parts.iter().zip(Self::part_offsets(&sizes)) {
            self.ref_words(t, base + at, out);
        }
    }

    /// A `PplLayout` constant, or null without references (strong or weak).
    fn layout_global(&self, size: u64, refs: &[u32]) -> PointerValue<'ctx> {
        self.layout_global_weak(size, refs, &[])
    }

    /// A `PplLayout` constant with `weak` words after the strong ones (`nweak`).
    fn layout_global_weak(&self, size: u64, refs: &[u32], weak: &[u32]) -> PointerValue<'ctx> {
        if refs.is_empty() && weak.is_empty() {
            return self.ptr().const_null();
        }
        let (i32t, i64t) = (self.i32(), self.i64());
        let words: Vec<IntValue> = refs
            .iter()
            .chain(weak)
            .map(|r| i32t.const_int(u64::from(*r), false))
            .collect();
        let value = self.cx.const_struct(
            &[
                i64t.const_int(size, false).into(),
                i32t.const_int(refs.len() as u64, false).into(),
                i32t.const_int(weak.len() as u64, false).into(),
                i32t.const_array(&words).into(),
            ],
            false,
        );
        let g = self.m.add_global(value.get_type(), None, "ppl.layout");
        g.set_linkage(Linkage::Private);
        g.set_constant(true);
        g.set_initializer(&value);
        g.set_alignment(8);
        g.as_pointer_value()
    }

    /// The layout of a `t` value (inline, or in a heap box or array storage).
    pub(crate) fn layout(&self, t: &Ty) -> PointerValue<'ctx> {
        let key = (t.clone(), false);
        if let Some(l) = self.layouts.borrow().get(&key) {
            return *l;
        }
        let mut refs = Vec::new();
        if self.p.traced(t) {
            self.ref_words(t, 0, &mut refs);
        }
        let l = self.layout_global(self.size_align(t).0.max(1), &refs);
        self.layouts.borrow_mut().insert(key, l);
        l
    }

    /// The layout of a class's objects: the header word, then the fields.
    pub(crate) fn object_layout(&self, class: &Ty) -> PointerValue<'ctx> {
        let key = (class.clone(), true);
        if let Some(l) = self.layouts.borrow().get(&key) {
            return *l;
        }
        let Ty::Class(sid, _, args) = class else {
            unreachable!("object layout of {class}")
        };
        let owner = Ty::type_owner(*sid);
        let def = self.p.layout(*sid);
        let mut parts = vec![(Ty::Ref(Box::new(Ty::Void)), false)];
        parts.extend(
            def.fields
                .iter()
                .zip(&def.weak)
                .map(|((_, f), w)| (f.subst(owner, args), *w))
                .filter(|(f, _)| self.ty(f).is_some()),
        );
        let (mut refs, mut weak) = (Vec::new(), Vec::new());
        let sizes: Vec<(u64, u64)> = parts.iter().map(|(t, _)| self.size_align(t)).collect();
        for ((t, w), at) in parts.iter().zip(Self::part_offsets(&sizes)).skip(1) {
            // A weak `C?` / `T[]?` is one pointer, cleared by the collector (spec §9.3);
            // weak fields of other types hold strongly.
            match t {
                Ty::Nullable(inner) if *w && matches!(**inner, Ty::Class(..) | Ty::Array(_)) => {
                    weak.push(at as u32)
                }
                _ => self.ref_words(t, at, &mut refs),
            }
        }
        let l = self.layout_global_weak(struct_layout(&sizes).0, &refs, &weak);
        self.layouts.borrow_mut().insert(key, l);
        l
    }

    /// A NUL-terminated constant string.
    pub(crate) fn cstr0(&self, s: &str) -> PointerValue<'ctx> {
        let bytes = self.cx.const_string(s.as_bytes(), true);
        let g = self.m.add_global(bytes.get_type(), None, "ppl.name");
        g.set_linkage(Linkage::Private);
        g.set_constant(true);
        g.set_unnamed_addr(true);
        g.set_initializer(&bytes);
        g.as_pointer_value()
    }

    fn runtime_global(&self, name: &str, t: BasicTypeEnum<'ctx>) -> PointerValue<'ctx> {
        match self.m.get_global(name) {
            Some(g) => g.as_pointer_value(),
            None => {
                let g = self.m.add_global(t, None, name);
                g.set_linkage(Linkage::External);
                g.as_pointer_value()
            }
        }
    }

    fn gc_top(&self) -> PointerValue<'ctx> {
        self.runtime_global("ppl_gc_top", self.ptr().into())
    }

    /// Builds the function's shadow-stack frame in the entry block (where the builder is):
    /// the locals that hold references move into it, the frame is zeroed and pushed.
    pub(crate) fn gc_frame(
        &self,
        func: &ppl_mir::mir::Function,
        locals: &mut [Option<PointerValue<'ctx>>],
    ) -> Option<(PointerValue<'ctx>, inkwell::types::StructType<'ctx>)> {
        let rooted: Vec<usize> = func
            .locals
            .iter()
            .enumerate()
            .filter(|(_, l)| self.ty(&l.ty).is_some() && self.p.traced(&l.ty))
            .map(|(i, _)| i)
            .collect();
        if rooted.is_empty() {
            return None;
        }
        let ptr = self.ptr();
        let mut fields: Vec<BasicTypeEnum> = vec![ptr.into(), ptr.into()];
        fields.extend(rooted.iter().map(|i| self.elem_ty(&func.locals[*i].ty)));
        let frame_ty = self.cx.struct_type(&fields, false);
        let frame = self.b.build_alloca(frame_ty, "gc.frame").unwrap();
        let mut sizes = vec![(8, 8), (8, 8)];
        sizes.extend(rooted.iter().map(|i| self.size_align(&func.locals[*i].ty)));
        let offsets = Self::part_offsets(&sizes);
        // The map: function name, then per root its name, layout and offset.
        let i64t = self.i64();
        let entry_ty = self
            .cx
            .struct_type(&[ptr.into(), ptr.into(), i64t.into()], false);
        let entries: Vec<inkwell::values::StructValue> = rooted
            .iter()
            .zip(&offsets[2..])
            .map(|(i, at)| {
                let l = &func.locals[*i];
                let name = match &l.name {
                    Some(n) => self.cstr0(n),
                    None => ptr.const_null(),
                };
                entry_ty.const_named_struct(&[
                    name.into(),
                    self.layout(&l.ty).into(),
                    i64t.const_int(*at, false).into(),
                ])
            })
            .collect();
        let map = self.cx.const_struct(
            &[
                self.cstr0(&func.name).into(),
                i64t.const_int(rooted.len() as u64, false).into(),
                entry_ty.const_array(&entries).into(),
            ],
            false,
        );
        let g = self.m.add_global(map.get_type(), None, "ppl.frame_map");
        g.set_linkage(Linkage::Private);
        g.set_constant(true);
        g.set_initializer(&map);
        for (k, i) in rooted.iter().enumerate() {
            locals[*i] = Some(
                self.b
                    .build_struct_gep(frame_ty, frame, 2 + k as u32, "")
                    .unwrap(),
            );
        }
        let size = frame_ty.size_of().expect("sized frame");
        self.b
            .build_memset(frame, 8, self.cx.i8_type().const_zero(), size)
            .unwrap();
        let map_slot = self.b.build_struct_gep(frame_ty, frame, 1, "").unwrap();
        self.b.build_store(map_slot, g.as_pointer_value()).unwrap();
        let top = self.gc_top();
        let prev = self.b.build_load(ptr, top, "gc.prev").unwrap();
        self.b.build_store(frame, prev).unwrap();
        self.b.build_store(top, frame).unwrap();
        Some((frame, frame_ty))
    }

    /// Pops the current function's frame (before it returns).
    pub(crate) fn gc_pop(&self, fx: &FnCx<'ctx, '_>) {
        if let Some((frame, _)) = fx.frame {
            let prev = self.b.build_load(self.ptr(), frame, "gc.prev").unwrap();
            self.b.build_store(self.gc_top(), prev).unwrap();
        }
    }

    /// Collects if a collection is due: a check of `ppl_gc_requested`.
    pub(crate) fn safepoint(&self, llfn: FunctionValue<'ctx>) {
        let flag = self.runtime_global("ppl_gc_requested", self.i32().into());
        let v = self
            .b
            .build_load(self.i32(), flag, "gc.requested")
            .unwrap()
            .into_int_value();
        let due = self
            .b
            .build_int_compare(IntPredicate::NE, v, self.i32().const_zero(), "")
            .unwrap();
        let slow = self.cx.append_basic_block(llfn, "gc.collect");
        let cont = self.cx.append_basic_block(llfn, "gc.continue");
        self.b.build_conditional_branch(due, slow, cont).unwrap();
        self.b.position_at_end(slow);
        self.b
            .build_call(self.rt("ppl_gc_safepoint"), &[], "")
            .unwrap();
        self.b.build_unconditional_branch(cont).unwrap();
        self.b.position_at_end(cont);
    }

    /// The write barrier, after a `t` value (holding references) was stored at `slot`.
    pub(crate) fn barrier(&self, slot: PointerValue<'ctx>, t: &Ty) {
        let layout = self.layout(t);
        self.b
            .build_call(self.rt("ppl_gc_write"), &[slot.into(), layout.into()], "")
            .unwrap();
    }

    /// Whether storing to `p` may write into a heap object.
    pub(crate) fn place_in_heap(&self, fx: &FnCx<'ctx, '_>, p: &Place) -> bool {
        let mut ty = match p.base {
            Base::Deref(_) => return true,
            Base::Local(l) => fx.func.local_ty(l).clone(),
            Base::Global(g) => self.p.globals[g.0 as usize].ty.clone(),
        };
        for pr in &p.proj {
            if matches!(
                (pr, &ty.erase()),
                (Proj::Field(_), Ty::Class(..)) | (Proj::Index(_), Ty::Array(_))
            ) {
                return true;
            }
            ty = mir::project(&ty, pr, |f| self.p.field_ty(f));
        }
        false
    }

    /// The table of global roots (once every function is generated, so the pending
    /// exception's global exists if anything uses it), registered at the start of the
    /// entry function.
    pub(crate) fn register_gc_globals(&self) {
        let (ptr, i64t) = (self.ptr(), self.i64());
        let entry_ty = self
            .cx
            .struct_type(&[ptr.into(), ptr.into(), ptr.into()], false);
        let mut entries = Vec::new();
        for (i, g) in self.p.globals.iter().enumerate() {
            if let Some(gv) = self.globals[i]
                && self.p.traced(&g.ty)
            {
                // The core library's internal globals (`__x`) aren't shown by dumps.
                let name = if g.name.starts_with("__") {
                    ptr.const_null()
                } else {
                    self.cstr0(&g.name)
                };
                entries.push(entry_ty.const_named_struct(&[
                    name.into(),
                    self.layout(&g.ty).into(),
                    gv.as_pointer_value().into(),
                ]));
            }
        }
        if let Some(e) = self.exception {
            entries.push(entry_ty.const_named_struct(&[
                ptr.const_null().into(),
                self.layout(&Ty::Str).into(),
                e.as_pointer_value().into(),
            ]));
        }
        let table = self.cx.const_struct(
            &[
                i64t.const_int(entries.len() as u64, false).into(),
                entry_ty.const_array(&entries).into(),
            ],
            false,
        );
        let g = self.m.add_global(table.get_type(), None, "ppl.gc_globals");
        g.set_linkage(Linkage::Private);
        g.set_constant(true);
        g.set_initializer(&table);
        let entry = self.fns[self.p.entry.0 as usize]
            .get_first_basic_block()
            .expect("entry block");
        let b = self.cx.create_builder();
        match entry.get_first_instruction() {
            Some(i) => b.position_before(&i),
            None => b.position_at_end(entry),
        }
        b.build_call(
            self.rt("ppl_gc_set_globals"),
            &[g.as_pointer_value().into()],
            "",
        )
        .unwrap();
    }
}

/// The blocks that are targets of a back edge (loop headers), which get safepoints.
pub(crate) fn loop_headers(func: &ppl_mir::mir::Function) -> Vec<bool> {
    let n = func.blocks.len();
    let mut headers = vec![false; n];
    // 0: unvisited, 1: on the DFS stack, 2: done.
    let mut state = vec![0u8; n];
    let mut stack: Vec<(usize, Vec<usize>)> = Vec::new();
    if n == 0 {
        return headers;
    }
    state[0] = 1;
    stack.push((0, successors(&func.blocks[0].term)));
    while let Some((b, succs)) = stack.last_mut() {
        let b = *b;
        match succs.pop() {
            Some(s) => match state[s] {
                0 => {
                    state[s] = 1;
                    let next = successors(&func.blocks[s].term);
                    stack.push((s, next));
                }
                1 => headers[s] = true,
                _ => {}
            },
            None => {
                state[b] = 2;
                stack.pop();
            }
        }
    }
    headers
}

fn successors(t: &Terminator) -> Vec<usize> {
    match t {
        Terminator::Goto(b) | Terminator::Unguard(b) => vec![b.0 as usize],
        Terminator::Branch { then, else_, .. } => vec![then.0 as usize, else_.0 as usize],
        Terminator::Call { next, .. } => vec![next.0 as usize],
        Terminator::Guard { body, handler } => vec![body.0 as usize, handler.0 as usize],
        Terminator::Return(_)
        | Terminator::TailCall { .. }
        | Terminator::Panic { .. }
        | Terminator::Resume
        | Terminator::Uncaught { .. }
        | Terminator::Unreachable => Vec::new(),
    }
}
