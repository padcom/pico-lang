//! MIR interpreter for ppl (comptime and `ppl run --interp`).
//!
//! Semantics match the native backend exactly: integers trap on overflow, division by zero
//! and out-of-range conversions; floats follow IEEE-754 (computed at `f32` precision for
//! `f32`); printing uses [`format_float`] for numbers.

// (Links the C runtime's crypto layer, which `--interp` calls for `std/crypto` and TLS.)
use ppl_mir::eval::{self, Prim, Scalar, from_bits, to_bits};
use ppl_mir::mir::{
    self, Base, BinOp, Callee, Const, Loc, Operand, Place, Program, Proj, Rvalue, Terminator,
    panics,
};
use ppl_runtime as _;
use ppl_sema::hir::{Builtin, FnId};
use ppl_sema::{FloatTy, Ty};
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;
pub mod comptime;
mod io;
mod workers;

/// Call depth after which the interpreter reports a stack overflow.
pub const MAX_DEPTH: usize = 20_000;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// An integer, always within its type's range.
    Int(i128),
    Float(f64),
    Bool(bool),
    Char(char),
    Str(Rc<str>),
    /// A struct, tuple, fixed array or range (start, end, step) value: its parts in order.
    /// Cloning copies it deeply.
    Struct(Vec<Value>),
    /// An enum value: the variant index and its payload fields.
    Enum(u32, Vec<Value>),
    /// A class instance: shared, so cloning copies the reference.
    Object(Rc<Obj>),
    /// An interface value: the object (or a reference to a boxed struct) and its itable.
    Iface(Rc<(Value, u32)>),
    /// A function value: the function and its environment (a reference to the captures).
    Closure(Rc<(FnId, Value)>),
    /// A growable array: shared, so cloning copies the reference.
    Array(Rc<RefCell<Vec<Value>>>),
    /// A reference into a variable: the cell and the path inside it.
    Ref(Cell, Rc<[Step]>),
    /// The content of a `@weak` field (spec §9.3): reading it gives the target, or `null` once
    /// nothing else holds it.
    Weak(WeakTarget),
    /// `null`. A non-null `T?` is just the `T` value.
    Null,
    Void,
    Uninit,
}

/// How a function's code ends: returning, or handing over to a tail call.
enum Flow {
    Return(Value),
    Tail(FnId, Vec<Value>),
}

/// A class instance's storage.
#[derive(Debug)]
pub struct Obj {
    pub class: u32,
    pub fields: RefCell<Vec<Value>>,
}

/// Objects are equal only to themselves (identity).
impl PartialEq for Obj {
    fn eq(&self, other: &Obj) -> bool {
        std::ptr::eq(self, other)
    }
}

/// A weak reference to an object or an array. There's no collector: a target is gone when its
/// last strong reference is (so a cycle is never cleared).
#[derive(Clone, Debug)]
pub enum WeakTarget {
    Obj(std::rc::Weak<Obj>),
    Array(std::rc::Weak<RefCell<Vec<Value>>>),
}

impl PartialEq for WeakTarget {
    fn eq(&self, other: &WeakTarget) -> bool {
        match (self, other) {
            (WeakTarget::Obj(a), WeakTarget::Obj(b)) => a.ptr_eq(b),
            (WeakTarget::Array(a), WeakTarget::Array(b)) => a.ptr_eq(b),
            _ => false,
        }
    }
}

/// A zeroed value of plain data (what fresh arena memory holds natively).
fn zero_value(p: &Program, t: &Ty) -> Value {
    match t {
        Ty::Int(_) => Value::Int(0),
        Ty::Float(_) => Value::Float(0.0),
        Ty::Bool => Value::Bool(false),
        Ty::Char => Value::Char('\0'),
        Ty::Fixed(e, n) => Value::Struct(vec![zero_value(p, e); *n as usize]),
        Ty::Tuple(ts) => Value::Struct(ts.iter().map(|t| zero_value(p, t)).collect()),
        Ty::Struct(sid, _, args) => {
            let owner = Ty::type_owner(*sid);
            Value::Struct(
                p.layout(*sid)
                    .fields
                    .iter()
                    .map(|(_, f)| zero_value(p, &f.subst(owner, args)))
                    .collect(),
            )
        }
        _ => Value::Null,
    }
}

/// A value as a `@weak` field holds it.
fn downgrade(v: Value) -> Value {
    match v {
        Value::Object(o) => Value::Weak(WeakTarget::Obj(Rc::downgrade(&o))),
        Value::Array(a) => Value::Weak(WeakTarget::Array(Rc::downgrade(&a))),
        v => v,
    }
}

/// A `@weak` field's value as code sees it.
fn upgrade(v: &Value) -> Value {
    match v {
        Value::Weak(WeakTarget::Obj(o)) => o.upgrade().map_or(Value::Null, Value::Object),
        Value::Weak(WeakTarget::Array(a)) => a.upgrade().map_or(Value::Null, Value::Array),
        v => v.clone(),
    }
}

/// One resolved step of a path into a value: a part of an inline value, or an element of a
/// growable array.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    Part(usize),
    Elem(usize),
    /// A field of the object a class reference points to.
    ObjField(usize),
    /// A `@weak` field of an object.
    WeakField(usize),
}

/// Storage for one variable. Locals and globals live in cells so references can point at them.
pub type Cell = Rc<RefCell<Value>>;

/// Whether a root holds a reference that `gc.dumpRoots()` shows (matching the native type
/// maps: enum payloads and closure environments are left out).
fn visible_ref(p: &Program, v: &Value, t: &Ty) -> bool {
    match (v, &t.erase()) {
        (Value::Uninit | Value::Null | Value::Void, _) => false,
        (_, Ty::Str | Ty::Array(_) | Ty::Class(..) | Ty::Iface(..) | Ty::Ref(_)) => true,
        (_, Ty::Nullable(inner)) => visible_ref(p, v, inner),
        (Value::Struct(parts), Ty::Struct(sid, _, args)) => {
            let owner = Ty::type_owner(*sid);
            p.layout(*sid)
                .fields
                .iter()
                .zip(parts)
                .any(|((_, f), v)| visible_ref(p, v, &f.subst(owner, args)))
        }
        (Value::Struct(parts), Ty::Tuple(ts)) => {
            ts.iter().zip(parts).any(|(t, v)| visible_ref(p, v, t))
        }
        (Value::Struct(parts), Ty::Fixed(e, _)) => parts.iter().any(|v| visible_ref(p, v, e)),
        _ => false,
    }
}

fn cell(v: Value) -> Cell {
    Rc::new(RefCell::new(v))
}

/// A panic: the message and where it happened.
#[derive(Debug, Clone, PartialEq)]
pub struct Panic {
    pub msg: String,
    pub file: String,
    pub loc: Loc,
    /// An uncaught exception rather than a panic (`msg` is `<class>: <message>`).
    pub uncaught: bool,
    /// The functions active when it happened, innermost first.
    pub trace: Vec<String>,
    /// Not a panic: `exit(code)` from `std/process` ends the program (no `finally` runs).
    pub exit: Option<i32>,
}

impl Panic {
    /// The text printed to stderr (shared format with the native runtime).
    pub fn report(&self) -> String {
        if self.exit.is_some() {
            return String::new();
        }
        if self.uncaught {
            return format!("uncaught {}\n", self.msg);
        }
        let mut s = format!(
            "panic: {}\n  at {}:{}:{}\n",
            self.msg, self.file, self.loc.line, self.loc.col
        );
        if !self.trace.is_empty() {
            s.push_str("stack trace:\n");
            for (i, f) in self.trace.iter().enumerate() {
                s.push_str(&format!("  {i}: {f}\n"));
            }
        }
        s
    }
}

/// Runs the program's entry function, writing program output to `out`. Workers run on
/// threads of their own, each with its own interpreter (its own globals).
pub fn run(program: &Program, out: &mut dyn Write) -> Result<(), Panic> {
    run_in(program, out, None)
}

/// [`run`] with working directory `cwd` for the program's file-system operations (instead of
/// the process's, which is shared by every thread).
pub fn run_in(
    program: &Program,
    out: &mut dyn Write,
    cwd: Option<std::path::PathBuf>,
) -> Result<(), Panic> {
    run_with(program, out, cwd, Vec::new())
}

/// [`run_in`] with the program's arguments (`args` in `std/process`).
pub fn run_with(
    program: &Program,
    out: &mut dyn Write,
    cwd: Option<std::path::PathBuf>,
    args: Vec<String>,
) -> Result<(), Panic> {
    let shared = Arc::new(workers::Shared {
        cwd,
        args,
        ..Default::default()
    });
    let _ = shared.start.set(std::time::Instant::now());
    let mut it = Interp::new(program, Some(out), shared.clone(), None);
    let r = it
        .call(program.entry, Vec::new(), Loc::default())
        .map(|_| ());
    // The program ends: so do its workers.
    shared.stop_all();
    it.flush_output();
    r
}

struct Interp<'p, 'o> {
    p: &'p Program,
    /// The main thread's output (workers' output reaches it through `shared`).
    out: Option<&'o mut dyn Write>,
    shared: Arc<workers::Shared>,
    /// This thread as a worker, and the workers it started.
    this_worker: Option<Arc<workers::IWorker>>,
    children: Vec<i64>,
    waker: Arc<workers::Waker>,
    globals: Vec<Cell>,
    depth: usize,
    /// The pending exception, and its tag (0: an `Error` object).
    exception: Option<Value>,
    exception_tag: u32,
    /// The objects being shown in their default string form (to stop at cycles).
    showing: Vec<*const Obj>,
    /// File-system jobs (`std/fs`).
    io: io::IoState,
    /// The names of the active functions (for stack traces).
    stack: Vec<String>,
    /// Panics whose `finally` handlers are running, innermost last.
    unwinding: Vec<Panic>,
    /// The active functions' locals, innermost last (for `gc.dumpRoots()`).
    frames: Vec<(&'p mir::Function, Rc<Vec<Cell>>)>,
    /// The start of `__clock_ms()`.
    started: std::time::Instant,
    /// Evaluating `comptime` code inside the compiler.
    comptime: Option<comptime::State>,
    /// A global read before it was set (comptime only: it's only set when the program runs).
    uninit_global: std::cell::Cell<Option<u32>>,
}

struct Frame<'p> {
    f: &'p mir::Function,
    locals: Rc<Vec<Cell>>,
}

impl Frame<'_> {
    fn ty(&self, p: &Program, place: &Place) -> Ty {
        p.place_ty(self.f, place)
    }
}

impl Interp<'_, '_> {
    fn panic(&self, msg: &str, loc: Loc) -> Panic {
        Panic {
            msg: msg.to_string(),
            file: self.p.file.clone(),
            loc,
            uncaught: false,
            trace: self.stack.iter().rev().cloned().collect(),
            exit: None,
        }
    }

    fn call(&mut self, id: FnId, args: Vec<Value>, loc: Loc) -> Result<Value, Panic> {
        if self.terminated() {
            return Err(workers::terminated_panic());
        }
        if self.depth >= MAX_DEPTH {
            return Err(self.panic(panics::STACK_OVERFLOW, loc));
        }
        self.depth += 1;
        // A `tailcall` replaces the frame instead of nesting, so its depth doesn't grow.
        let (mut id, mut args) = (id, args);
        self.stack.push(String::new());
        let r = loop {
            let f = self.p.func(id);
            *self.stack.last_mut().expect("pushed") = f.name.clone();
            let mut frame = Frame {
                f,
                locals: Rc::new((0..f.locals.len()).map(|_| cell(Value::Uninit)).collect()),
            };
            for (p, a) in f.params.iter().zip(args) {
                *frame.locals[p.0 as usize].borrow_mut() = a;
            }
            self.frames.push((f, frame.locals.clone()));
            let r = self.exec(&mut frame);
            self.frames.pop();
            match r {
                Ok(Flow::Return(v)) => break Ok(v),
                Ok(Flow::Tail(next, a)) => {
                    id = next;
                    args = a;
                }
                Err(p) => break Err(p),
            }
        };
        self.stack.pop();
        self.depth -= 1;
        r
    }

    /// The cell a place lives in and the full resolved path inside it.
    fn locate(&self, frame: &Frame, place: &Place) -> (Cell, Vec<Step>) {
        let mut path = Vec::new();
        let cell = match place.base {
            Base::Local(l) => frame.locals[l.0 as usize].clone(),
            Base::Global(g) => self.globals[g.0 as usize].clone(),
            Base::Deref(l) => match &*frame.locals[l.0 as usize].borrow() {
                Value::Ref(c, p) => {
                    path.extend(p.iter().copied());
                    c.clone()
                }
                v => unreachable!("deref of {v:?}"),
            },
        };
        // The type decides whether an index addresses inline parts or a heap array.
        let mut ty = match place.base {
            Base::Local(l) => frame.f.local_ty(l).clone(),
            Base::Global(g) => self.p.globals[g.0 as usize].ty.clone(),
            Base::Deref(l) => match frame.f.local_ty(l) {
                Ty::Ref(t) => (**t).clone(),
                t => unreachable!("deref of {t}"),
            },
        };
        for pr in &place.proj {
            if *pr == Proj::Payload {
                ty = mir::project(&ty, pr, |f| self.p.field_ty(f));
                continue;
            }
            let step = match pr {
                Proj::Field(i) if matches!(ty, Ty::Class(..)) => match ty {
                    Ty::Class(sid, ..) if self.p.layout(sid).weak[*i as usize] => {
                        Step::WeakField(*i as usize)
                    }
                    _ => Step::ObjField(*i as usize),
                },
                Proj::Field(i) | Proj::VariantField(_, i) => Step::Part(*i as usize),
                Proj::Payload => unreachable!(),
                Proj::Index(l) => {
                    let Value::Int(i) = *frame.locals[l.0 as usize].borrow() else {
                        unreachable!("index local holds an int")
                    };
                    if matches!(ty, Ty::Array(_)) {
                        Step::Elem(i as usize)
                    } else {
                        Step::Part(i as usize)
                    }
                }
            };
            path.push(step);
            ty = mir::project(&ty, pr, |f| self.p.field_ty(f));
        }
        (cell, path)
    }

    fn read(&self, frame: &Frame, place: &Place) -> Value {
        let (c, path) = self.locate(frame, place);
        let v = c.borrow();
        read_path(&v, &path)
    }

    fn write(&mut self, frame: &mut Frame, place: &Place, v: Value) {
        let (c, path) = self.locate(frame, place);
        let mut root = c.borrow_mut();
        write_path(&mut root, &path, v);
    }

    fn operand(&self, frame: &Frame, o: &Operand) -> Value {
        match o {
            Operand::Copy(p) => self.read(frame, p),
            Operand::Const(c) => match c {
                Const::Int(v, _) => Value::Int(*v),
                Const::Float(v, FloatTy::F32) => Value::Float(*v as f32 as f64),
                Const::Float(v, FloatTy::F64) => Value::Float(*v),
                Const::Bool(b) => Value::Bool(*b),
                Const::Char(c) => Value::Char(*c),
                Const::Str(s) => Value::Str(s.as_str().into()),
                Const::Null(_) => Value::Null,
                Const::Undef(_) => Value::Uninit,
                Const::Void => Value::Void,
            },
        }
    }

    fn operand_ty(&self, frame: &Frame, o: &Operand) -> Ty {
        match o {
            Operand::Copy(p) => frame.ty(self.p, p),
            Operand::Const(c) => c.ty(),
        }
    }

    /// Runs a frame; a panic inside a guarded region continues at the region's handler.
    fn exec(&mut self, frame: &mut Frame) -> Result<Flow, Panic> {
        let mut bb = mir::BlockId(0);
        let mut guards: Vec<mir::BlockId> = Vec::new();
        loop {
            match self.run_blocks(frame, &mut bb, &mut guards) {
                Err(p) if !guards.is_empty() && p.exit.is_none() => {
                    self.unwinding.push(p);
                    bb = guards.pop().expect("checked");
                }
                r => return r,
            }
        }
    }

    fn run_blocks(
        &mut self,
        frame: &mut Frame,
        bb: &mut mir::BlockId,
        guards: &mut Vec<mir::BlockId>,
    ) -> Result<Flow, Panic> {
        loop {
            let block = frame.f.block(*bb);
            if let Some(c) = &mut self.comptime {
                c.steps += block.stmts.len() as u64 + 1;
                if c.steps > c.limit {
                    let loc = block.stmts.first().map_or(Loc::default(), |s| s.loc);
                    return Err(self.panic(comptime::LIMIT_PANIC, loc));
                }
            }
            for s in &block.stmts {
                if self.comptime.is_some() {
                    s.rvalue.operands(&mut |o| self.note_uninit(o));
                    if !s.place.proj.is_empty() {
                        self.note_uninit_place(&s.place);
                    }
                    self.check_uninit(s.loc)?;
                }
                let v = self.rvalue(frame, &s.rvalue, s.loc)?;
                self.write(frame, &s.place, v);
            }
            if self.comptime.is_some() {
                block.term.operands(&mut |o| self.note_uninit(o));
                if let Terminator::Call { dest: Some(d), .. } = &block.term
                    && !d.proj.is_empty()
                {
                    self.note_uninit_place(d);
                }
                let loc = match &block.term {
                    Terminator::Call { loc, .. }
                    | Terminator::TailCall { loc, .. }
                    | Terminator::Panic { loc, .. } => *loc,
                    _ => block.stmts.last().map_or(Loc::default(), |s| s.loc),
                };
                self.check_uninit(loc)?;
            }
            match &block.term {
                Terminator::Goto(t) => *bb = *t,
                Terminator::Branch { cond, then, else_ } => {
                    *bb = match self.operand(frame, cond) {
                        Value::Bool(true) => *then,
                        _ => *else_,
                    }
                }
                Terminator::Return(v) => {
                    return Ok(Flow::Return(
                        v.as_ref().map_or(Value::Void, |o| self.operand(frame, o)),
                    ));
                }
                Terminator::TailCall { callee, args, .. } => {
                    let vals = args.iter().map(|a| self.operand(frame, a)).collect();
                    return Ok(Flow::Tail(*callee, vals));
                }
                Terminator::Call {
                    callee,
                    args,
                    dest,
                    next,
                    loc,
                } => {
                    let vals: Vec<Value> = args.iter().map(|a| self.operand(frame, a)).collect();
                    let r = match callee {
                        Callee::Fn(id) => self.call(*id, vals, *loc)?,
                        Callee::Indirect => {
                            let Value::Closure(c) = &vals[0] else {
                                unreachable!("call of {:?}", vals[0])
                            };
                            let (f, env) = (c.0, c.1.clone());
                            let mut args = vec![env];
                            args.extend(vals[1..].iter().cloned());
                            self.call(f, args, *loc)?
                        }
                        Callee::Iface { slot, .. } => {
                            let Value::Iface(iv) = &vals[0] else {
                                unreachable!("interface call on {:?}", vals[0])
                            };
                            let f = self.p.itables[iv.1 as usize][*slot as usize];
                            self.call(f, vals, *loc)?
                        }
                        Callee::Virtual { slot, .. } => {
                            let Value::Object(o) = &vals[0] else {
                                unreachable!("virtual call on {:?}", vals[0])
                            };
                            let class = ppl_sema::ty::StructId(o.class);
                            let f = self.p.layout(class).meta.vtable[*slot as usize];
                            self.call(f, vals, *loc)?
                        }
                        Callee::Builtin(b) => {
                            let tys: Vec<Ty> =
                                args.iter().map(|a| self.operand_ty(frame, a)).collect();
                            self.builtin(*b, &vals, &tys, *loc)?
                        }
                    };
                    if let Some(d) = dest {
                        self.write(frame, d, r);
                    }
                    *bb = *next;
                }
                Terminator::Panic { msg, loc } => {
                    let m = match self.operand(frame, msg) {
                        Value::Str(s) => s.to_string(),
                        _ => "panic".into(),
                    };
                    return Err(self.panic(&m, *loc));
                }
                Terminator::Guard { body, handler } => {
                    guards.push(*handler);
                    *bb = *body;
                }
                Terminator::Unguard(next) => {
                    guards.pop();
                    *bb = *next;
                }
                Terminator::Resume => {
                    return Err(self.unwinding.pop().expect("a panic being unwound"));
                }
                Terminator::Uncaught { class, msg } => {
                    let text = |v: Value| match v {
                        Value::Str(s) => s.to_string(),
                        _ => String::new(),
                    };
                    let c = text(self.operand(frame, class));
                    let m = text(self.operand(frame, msg));
                    let mut p = self.panic(&format!("{c}: {m}"), Loc::default());
                    p.uncaught = true;
                    return Err(p);
                }
                Terminator::Unreachable => unreachable!("reached an unreachable block"),
            }
        }
    }

    /// Notes a read of a global that isn't set (comptime: only the running program sets it).
    fn note_uninit(&self, o: &Operand) {
        if let Operand::Copy(p) = o {
            self.note_uninit_place(p);
        }
    }

    fn note_uninit_place(&self, p: &Place) {
        if let Base::Global(g) = p.base
            && matches!(*self.globals[g.0 as usize].borrow(), Value::Uninit)
        {
            self.uninit_global.set(Some(g.0));
        }
    }

    /// A comptime read of a global that only the running program sets.
    fn check_uninit(&self, loc: Loc) -> Result<(), Panic> {
        match self.uninit_global.take() {
            Some(g) => Err(self.panic(
                &format!(
                    "comptime code reads `{}`, which is only set when the program runs",
                    self.p.globals[g as usize].name
                ),
                loc,
            )),
            None => Ok(()),
        }
    }

    fn builtin(
        &mut self,
        b: Builtin,
        vals: &[Value],
        tys: &[Ty],
        loc: Loc,
    ) -> Result<Value, Panic> {
        if let Some(r) = self.comptime_builtin(b, vals, loc) {
            return r;
        }
        if b == Builtin::Io
            && let [Value::Int(118), Value::Int(code), ..] = vals
        {
            self.flush_output();
            let mut p = self.panic("exit", loc);
            p.exit = Some(*code as i32);
            return Err(p);
        }
        Ok(self.runtime_builtin(b, vals, tys))
    }

    fn runtime_builtin(&mut self, b: Builtin, vals: &[Value], tys: &[Ty]) -> Value {
        let text: String = vals.iter().zip(tys).map(|(v, t)| display(v, t)).collect();
        let _ = match b {
            Builtin::Println => {
                self.emit(&format!("{text}\n"));
                Ok(())
            }
            Builtin::Print => {
                self.emit(&text);
                Ok(())
            }
            Builtin::GcDumpRoots => {
                let text = self.dump_roots();
                self.emit(&text);
                Ok::<(), std::io::Error>(())
            }
            // No collector here: nothing to do, and every statistic is zero.
            Builtin::GcStat => return Value::Int(0),
            // Arenas only change where memory comes from.
            Builtin::ArenaEnter
            | Builtin::ArenaExit
            | Builtin::ArenaExitRescue
            | Builtin::RawArenaFree => Ok(()),
            Builtin::RawArenaNew => return Value::Int(0),
            Builtin::WorkerOp => return self.worker_op(vals),
            Builtin::WorkerPost => {
                self.worker_post(&vals[0], vals[1].clone(), vals.get(2));
                Ok(())
            }
            Builtin::WorkerError => return self.worker_error(&vals[0]),
            Builtin::BufOp => return self.buf_op(vals),
            Builtin::Io => return self.io(vals),
            Builtin::IoText => return self.io_text(vals),
            Builtin::ClockMs => {
                return Value::Int(self.started.elapsed().as_millis() as i128);
            }
            Builtin::PanicTaken => {
                // The handler of `__catch_panic` stops the panic being unwound.
                let p = self.unwinding.pop().expect("a panic being unwound");
                return Value::Str(p.msg.into());
            }
            Builtin::Math => {
                let f = |i: usize| match vals.get(i) {
                    Some(Value::Float(x)) => *x,
                    _ => f64::NAN,
                };
                let op = match vals.first() {
                    Some(Value::Int(x)) => *x as i64,
                    _ => -1,
                };
                return Value::Float(math_op(op, f(1), f(2)));
            }
            Builtin::ParseF64 => {
                return Value::Float(match &vals[0] {
                    Value::Str(s) => s.parse::<f64>().unwrap_or(f64::NAN),
                    _ => f64::NAN,
                });
            }
            Builtin::LoopWait => {
                if let Value::Int(ms) = vals[0] {
                    // A watcher waiting for changes is polled every few milliseconds.
                    let ms = ms as i64;
                    let ms = if self.io.watching() && !(0..=5).contains(&ms) {
                        5
                    } else {
                        ms
                    };
                    self.waker.wait(ms);
                }
                Ok(())
            }
            Builtin::RefId => {
                return Value::Int(match &vals[0] {
                    Value::Object(o) => Rc::as_ptr(o) as usize as i128,
                    Value::Array(a) => Rc::as_ptr(a) as usize as i128,
                    v => unreachable!("identity of {v:?}"),
                });
            }
            Builtin::GcCollect | Builtin::GcCollectYoung | Builtin::GcPause | Builtin::GcResume => {
                Ok(())
            }
            Builtin::Assert | Builtin::Panic | Builtin::CatchPanic => {
                unreachable!("lowered to control flow")
            }
            Builtin::ComptimeInfo | Builtin::ComptimeReadFile | Builtin::MetaEmit => {
                unreachable!("handled by comptime_builtin")
            }
        };
        Value::Void
    }

    /// What the native `ppl_gc_dump_roots` prints: per active function (innermost first),
    /// the named locals holding a reference, then the globals.
    fn dump_roots(&self) -> String {
        let mut out = String::from("gc roots:\n");
        let line = |out: &mut String, title: &str, names: Vec<&str>| {
            if !names.is_empty() {
                out.push_str(&format!("  {title}: {}\n", names.join(", ")));
            }
        };
        for (f, locals) in self.frames.iter().rev() {
            let names = f
                .locals
                .iter()
                .zip(locals.iter())
                .filter_map(|(l, v)| {
                    let name = l.name.as_deref()?;
                    visible_ref(self.p, &v.borrow(), &l.ty).then_some(name)
                })
                .collect();
            line(&mut out, &f.name, names);
        }
        let names = self
            .p
            .globals
            .iter()
            .zip(&self.globals)
            .filter(|(g, v)| !g.name.starts_with("__") && visible_ref(self.p, &v.borrow(), &g.ty))
            .map(|(g, _)| g.name.as_str())
            .collect();
        line(&mut out, "globals", names);
        out
    }

    fn rvalue(&mut self, frame: &Frame, rv: &Rvalue, loc: Loc) -> Result<Value, Panic> {
        match rv {
            Rvalue::Use(o) => Ok(self.operand(frame, o)),
            Rvalue::Unary(op, o) => {
                let t = self.operand_ty(frame, o);
                let v = self.operand(frame, o);
                let (Some(x), Some(p)) = (scalar(&v), Prim::of(&t)) else {
                    unreachable!("type-checked unary operator")
                };
                eval::unary(*op, x, p)
                    .map(from_scalar)
                    .map_err(|m| self.panic(m, loc))
            }
            Rvalue::Binary(op, a, b) => {
                let t = self.operand_ty(frame, a);
                let (x, y) = (self.operand(frame, a), self.operand(frame, b));
                self.binary(*op, x, y, &t, loc)
            }
            Rvalue::Cast(o, to) => {
                let from = self.operand_ty(frame, o);
                let v = self.operand(frame, o);
                self.cast(v, &from, to, loc)
            }
            Rvalue::ToStr(o) => {
                let t = self.operand_ty(frame, o);
                Ok(Value::Str(display(&self.operand(frame, o), &t).into()))
            }
            Rvalue::Aggregate(_, ops) => Ok(Value::Struct(
                ops.iter().map(|o| self.operand(frame, o)).collect(),
            )),
            Rvalue::Ref(place) => {
                let (c, path) = self.locate(frame, place);
                Ok(Value::Ref(c, path.into()))
            }
            // A pointer is a place: moving it moves the last index (array elements, or the
            // elements of a fixed array).
            Rvalue::PtrOffset(p, n, sub) => {
                let Value::Ref(c, path) = self.operand(frame, p) else {
                    unreachable!("a pointer")
                };
                let Value::Int(n) = self.operand(frame, n) else {
                    unreachable!("an integer offset")
                };
                let n = if *sub { -n } else { n };
                if n == 0 {
                    return Ok(Value::Ref(c, path));
                }
                let mut steps = path.to_vec();
                let moved = match steps.last_mut() {
                    Some(Step::Elem(i) | Step::Part(i)) => {
                        usize::try_from(*i as i128 + n).ok().map(|j| *i = j)
                    }
                    _ => None,
                };
                if moved.is_none() {
                    return Err(self.panic(panics::POINTER_OUT_OF_BOUNDS, loc));
                }
                Ok(Value::Ref(c, steps.into()))
            }
            Rvalue::WorkerTake(t, h) => {
                let h = self.operand(frame, h);
                Ok(self.worker_take(&h, t))
            }
            // `count` zeroed values in a fresh array; the pointer is its first element.
            Rvalue::RawArenaAlloc(t, _, n) => {
                let Value::Int(n) = self.operand(frame, n) else {
                    unreachable!("an int count")
                };
                let zero = zero_value(self.p, t);
                let storage = vec![zero; usize::try_from(n).unwrap_or(0)];
                let cell: Cell =
                    Rc::new(RefCell::new(Value::Array(Rc::new(RefCell::new(storage)))));
                Ok(Value::Ref(cell, Rc::from([Step::Elem(0)])))
            }
            Rvalue::SizeOf(t, align) => {
                let (size, alignment) = self.p.size_align(t);
                Ok(Value::Int(i128::from(if *align {
                    alignment
                } else {
                    size
                })))
            }
            Rvalue::BitCast(o, to) => {
                let from = self.operand_ty(frame, o);
                let bits: u128 = match (self.operand(frame, o), &from) {
                    (Value::Int(x), Ty::Int(it)) => to_bits(x, *it),
                    (Value::Float(x), Ty::Float(FloatTy::F32)) => u128::from((x as f32).to_bits()),
                    (Value::Float(x), _) => u128::from(x.to_bits()),
                    (v, t) => unreachable!("bit cast of {v:?}: {t}"),
                };
                Ok(match to {
                    Ty::Int(it) => Value::Int(from_bits(bits, *it)),
                    Ty::Float(FloatTy::F32) => Value::Float(f64::from(f32::from_bits(bits as u32))),
                    _ => Value::Float(f64::from_bits(bits as u64)),
                })
            }
            Rvalue::Format(o, spec)
                if ppl_sema::format::is_compound(&self.operand_ty(frame, o)) =>
            {
                let t = self.operand_ty(frame, o);
                let text = debug_value(self.p, &self.operand(frame, o), &t);
                Ok(Value::Str(pad(false, "", &text, false, spec).into()))
            }
            Rvalue::Format(o, spec) => {
                let t = self.operand_ty(frame, o);
                Ok(Value::Str(
                    format_spec(&self.operand(frame, o), &t, spec).into(),
                ))
            }
            Rvalue::StrLen(o) => match self.operand(frame, o) {
                Value::Str(s) => Ok(Value::Int(s.chars().count() as i128)),
                v => unreachable!("length of {v:?}"),
            },
            Rvalue::StrCharAt(o, i) => match (self.operand(frame, o), self.operand(frame, i)) {
                (Value::Str(s), Value::Int(i)) => usize::try_from(i)
                    .ok()
                    .and_then(|i| s.chars().nth(i))
                    .map(Value::Char)
                    .ok_or_else(|| self.panic(panics::INDEX, loc)),
                v => unreachable!("indexing {v:?}"),
            },
            Rvalue::StrSlice(o, a, b) => {
                match (
                    self.operand(frame, o),
                    self.operand(frame, a),
                    self.operand(frame, b),
                ) {
                    (Value::Str(s), Value::Int(a), Value::Int(b)) => {
                        let n = s.chars().count() as i128;
                        let norm = |i: i128| if i < 0 { (n + i).max(0) } else { i.min(n) };
                        let (a, b) = (norm(a), norm(b));
                        let out: String = if b > a {
                            s.chars().skip(a as usize).take((b - a) as usize).collect()
                        } else {
                            String::new()
                        };
                        Ok(Value::Str(out.into()))
                    }
                    v => unreachable!("slice of {v:?}"),
                }
            }
            Rvalue::Tuple(_, ops) => Ok(Value::Struct(
                ops.iter().map(|o| self.operand(frame, o)).collect(),
            )),
            Rvalue::Repeat(o, n) => {
                let v = self.operand(frame, o);
                Ok(Value::Struct(vec![v; *n as usize]))
            }
            Rvalue::ArrayNew(_, ops) => Ok(Value::Array(Rc::new(RefCell::new(
                ops.iter().map(|o| self.operand(frame, o)).collect(),
            )))),
            Rvalue::ArraySlice(a, b, c) => {
                match (
                    self.operand(frame, a),
                    self.operand(frame, b),
                    self.operand(frame, c),
                ) {
                    (Value::Array(a), Value::Int(b), Value::Int(c)) => {
                        let part = a.borrow()[b as usize..c as usize].to_vec();
                        Ok(Value::Array(Rc::new(RefCell::new(part))))
                    }
                    v => unreachable!("slice of {v:?}"),
                }
            }
            Rvalue::ArrayLen(o) => match self.operand(frame, o) {
                Value::Array(a) => Ok(Value::Int(a.borrow().len() as i128)),
                v => unreachable!("length of {v:?}"),
            },
            Rvalue::ArrayPush(a, v) => {
                let v = self.operand(frame, v);
                match self.operand(frame, a) {
                    Value::Array(a) => a.borrow_mut().push(v),
                    v => unreachable!("push to {v:?}"),
                }
                Ok(Value::Void)
            }
            Rvalue::ArrayPop(a) => match self.operand(frame, a) {
                Value::Array(a) => Ok(a.borrow_mut().pop().unwrap_or(Value::Null)),
                v => unreachable!("pop from {v:?}"),
            },
            Rvalue::InBounds(i, n) => match (self.operand(frame, i), self.operand(frame, n)) {
                (Value::Int(i), Value::Int(n)) => Ok(Value::Bool(0 <= i && i < n)),
                v => unreachable!("bounds check of {v:?}"),
            },
            Rvalue::WrapSome(o, _) | Rvalue::Unwrap(o) => Ok(self.operand(frame, o)),
            Rvalue::Variant(_, v, ops) => Ok(Value::Enum(
                *v,
                ops.iter().map(|o| self.operand(frame, o)).collect(),
            )),
            Rvalue::ToIface(o, it) => {
                let v = self.operand(frame, o);
                // A struct is boxed: the interface value refers to a copy of it.
                let data = match v {
                    Value::Object(_) => v,
                    other => Value::Ref(cell(other), Rc::from(Vec::new())),
                };
                Ok(Value::Iface(Rc::new((data, *it))))
            }
            Rvalue::Closure(f, ops) => {
                let caps: Vec<Value> = ops.iter().map(|o| self.operand(frame, o)).collect();
                let env = Value::Ref(cell(Value::Struct(caps)), Rc::from(Vec::new()));
                Ok(Value::Closure(Rc::new((*f, env))))
            }
            Rvalue::FnAddr(_) => Err(self.panic(
                "C function pointers need a native build (extern functions don't run in the interpreter)",
                loc,
            )),
            // Comptime code inside comptime code: just run it.
            Rvalue::Comptime(f, _) => {
                let env = Value::Ref(cell(Value::Struct(Vec::new())), Rc::from(Vec::new()));
                self.call(*f, vec![env], loc)
            }
            Rvalue::BoxNew(_, o) => {
                let v = match o {
                    Operand::Const(Const::Void) => Value::Uninit,
                    o => self.operand(frame, o),
                };
                Ok(Value::Ref(cell(v), Rc::from(Vec::new())))
            }
            Rvalue::Hash(o) => {
                let t = self.operand_ty(frame, o);
                Ok(Value::Int(hash_value(&self.operand(frame, o), &t) as i128))
            }
            Rvalue::SetException(o, t) => {
                let tag = match self.operand(frame, t) {
                    Value::Int(t) => t as u32,
                    _ => 0,
                };
                self.exception = Some(self.operand(frame, o));
                self.exception_tag = tag;
                Ok(Value::Void)
            }
            Rvalue::TakeException(_) => Ok(self.exception.take().expect("a pending exception")),
            Rvalue::ExceptionTag => Ok(Value::Int(self.exception_tag as i128)),
            Rvalue::ExceptionPending => Ok(Value::Bool(self.exception.is_some())),
            Rvalue::HashCombine(a, b) => match (self.operand(frame, a), self.operand(frame, b)) {
                (Value::Int(x), Value::Int(y)) => {
                    Ok(Value::Int(hash_combine(x as u64, y as u64) as i128))
                }
                v => unreachable!("hash of {v:?}"),
            },
            Rvalue::IfaceData(o) => match self.operand(frame, o) {
                Value::Iface(iv) => Ok(iv.0.clone()),
                v => unreachable!("data of {v:?}"),
            },
            Rvalue::InstanceOf(o, target) => match self.operand(frame, o) {
                Value::Object(obj) => {
                    let mut c = Some(ppl_sema::ty::StructId(obj.class));
                    while let Some(k) = c {
                        if k == *target {
                            return Ok(Value::Bool(true));
                        }
                        c = self.p.layout(k).meta.base;
                    }
                    Ok(Value::Bool(false))
                }
                _ => Ok(Value::Bool(false)),
            },
            Rvalue::New(t) => {
                let Ty::Class(sid, _, _) = t else {
                    unreachable!("new of {t}")
                };
                Ok(Value::Object(Rc::new(Obj {
                    class: sid.0,
                    fields: RefCell::new(vec![Value::Uninit; self.p.layout(*sid).fields.len()]),
                })))
            }
            Rvalue::Tag(o) => match self.operand(frame, o) {
                Value::Enum(v, _) => Ok(Value::Int(v as i128)),
                v => unreachable!("tag of {v:?}"),
            },
            Rvalue::IsNull(o) => Ok(Value::Bool(self.operand(frame, o) == Value::Null)),
            Rvalue::ShowEnter(o) => match self.operand(frame, o) {
                Value::Object(obj) => {
                    let id = Rc::as_ptr(&obj);
                    if self.showing.contains(&id) {
                        return Ok(Value::Bool(false));
                    }
                    self.showing.push(id);
                    Ok(Value::Bool(true))
                }
                v => unreachable!("showing {v:?}"),
            },
            Rvalue::ShowLeave(o) => match self.operand(frame, o) {
                Value::Object(obj) => {
                    let id = Rc::as_ptr(&obj);
                    if let Some(i) = self.showing.iter().rposition(|p| *p == id) {
                        self.showing.remove(i);
                    }
                    Ok(Value::Void)
                }
                v => unreachable!("showing {v:?}"),
            },
            Rvalue::AddOverflows(a, b) => {
                let Ty::Int(it) = self.operand_ty(frame, a) else {
                    unreachable!("integer addition")
                };
                match (self.operand(frame, a), self.operand(frame, b)) {
                    (Value::Int(x), Value::Int(y)) => Ok(Value::Bool(!it.contains(x + y))),
                    v => unreachable!("addition of {v:?}"),
                }
            }
        }
    }

    fn binary(&self, op: BinOp, x: Value, y: Value, t: &Ty, loc: Loc) -> Result<Value, Panic> {
        use BinOp::*;
        match (x, y, t) {
            (a, b, Ty::Nullable(_)) => Ok(match op {
                Eq => Value::Bool(values_eq(&a, &b)),
                Ne => Value::Bool(!values_eq(&a, &b)),
                _ => unreachable!("type-checked nullable operator"),
            }),
            (x @ (Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Char(_)), y, t) => {
                let (Some(x), Some(y), Some(p)) = (scalar(&x), scalar(&y), Prim::of(t)) else {
                    unreachable!("binary {op:?} on {x:?}, {y:?} ({t})")
                };
                eval::binary(op, x, y, p)
                    .map(from_scalar)
                    .map_err(|m| self.panic(m, loc))
            }
            (
                a @ (Value::Struct(_) | Value::Enum(..) | Value::Object(_) | Value::Ref(..)),
                b,
                _,
            ) => Ok(match op {
                Eq => Value::Bool(values_eq(&a, &b)),
                Ne => Value::Bool(!values_eq(&a, &b)),
                _ => unreachable!("type-checked struct operator"),
            }),
            (Value::Str(a), Value::Str(b), _) => Ok(match op {
                Add => Value::Str(format!("{a}{b}").into()),
                // Byte order of UTF-8 is code-point order.
                Eq => Value::Bool(a == b),
                Ne => Value::Bool(a != b),
                Lt => Value::Bool(a < b),
                Le => Value::Bool(a <= b),
                Gt => Value::Bool(a > b),
                Ge => Value::Bool(a >= b),
                _ => unreachable!("type-checked string operator"),
            }),
            (x, y, t) => unreachable!("binary {op:?} on {x:?}, {y:?} ({t})"),
        }
    }

    fn cast(&self, v: Value, from: &Ty, to: &Ty, loc: Loc) -> Result<Value, Panic> {
        if let (Some(x), Some(p)) = (scalar(&v), Prim::of(to))
            && let Some(r) = eval::cast(x, p)
        {
            return r.map(from_scalar).map_err(|m| self.panic(m, loc));
        }
        if from == to {
            return Ok(v);
        }
        unreachable!("cast {v:?} from {from} to {to}")
    }
}

fn scalar(v: &Value) -> Option<Scalar> {
    Some(match v {
        Value::Int(x) => Scalar::Int(*x),
        Value::Float(x) => Scalar::Float(*x),
        Value::Bool(b) => Scalar::Bool(*b),
        Value::Char(c) => Scalar::Char(*c),
        _ => return None,
    })
}

fn from_scalar(s: Scalar) -> Value {
    match s {
        Scalar::Int(x) => Value::Int(x),
        Scalar::Float(x) => Value::Float(x),
        Scalar::Bool(b) => Value::Bool(b),
        Scalar::Char(c) => Value::Char(c),
    }
}

fn read_path(v: &Value, path: &[Step]) -> Value {
    match path.split_first() {
        None => v.clone(),
        Some((Step::Part(i), rest)) => match v {
            Value::Struct(parts) | Value::Enum(_, parts) => read_path(&parts[*i], rest),
            other => unreachable!("part {i} of {other:?}"),
        },
        Some((Step::Elem(i), rest)) => match v {
            Value::Array(a) => read_path(&a.borrow()[*i], rest),
            other => unreachable!("element {i} of {other:?}"),
        },
        Some((Step::ObjField(i), rest)) => match v {
            Value::Object(o) => read_path(&o.fields.borrow()[*i], rest),
            other => unreachable!("field {i} of {other:?}"),
        },
        Some((Step::WeakField(i), rest)) => match v {
            Value::Object(o) => read_path(&upgrade(&o.fields.borrow()[*i]), rest),
            other => unreachable!("field {i} of {other:?}"),
        },
    }
}

fn write_path(v: &mut Value, path: &[Step], new: Value) {
    match path.split_first() {
        None => *v = new,
        Some((Step::Part(i), rest)) => match v {
            Value::Struct(parts) | Value::Enum(_, parts) => write_path(&mut parts[*i], rest, new),
            other => unreachable!("part {i} of {other:?}"),
        },
        Some((Step::Elem(i), rest)) => match v {
            Value::Array(a) => {
                let a = a.clone();
                write_path(&mut a.borrow_mut()[*i], rest, new);
            }
            other => unreachable!("element {i} of {other:?}"),
        },
        Some((Step::ObjField(i), rest)) => match v {
            Value::Object(o) => {
                let o = o.clone();
                write_path(&mut o.fields.borrow_mut()[*i], rest, new);
            }
            other => unreachable!("field {i} of {other:?}"),
        },
        Some((Step::WeakField(i), rest)) => match v {
            Value::Object(o) if rest.is_empty() => o.fields.borrow_mut()[*i] = downgrade(new),
            // Inside the target (shared, so writing through a strong copy is the same).
            Value::Object(o) => write_path(&mut upgrade(&o.fields.borrow()[*i]), rest, new),
            other => unreachable!("field {i} of {other:?}"),
        },
    }
}

/// The hash of a primitive or string. Must match `ppl_hash_*` in the C runtime.
fn hash_value(v: &Value, t: &Ty) -> u64 {
    match (v, t.erase()) {
        (Value::Int(x), _) => splitmix64(*x as i64 as u64),
        (Value::Float(x), Ty::Float(FloatTy::F32)) => splitmix64((*x as f32 as f64).to_bits()),
        (Value::Float(x), _) => splitmix64(x.to_bits()),
        (Value::Bool(b), _) => splitmix64(u64::from(*b)),
        (Value::Char(c), _) => splitmix64(u64::from(*c as u32)),
        (Value::Str(s), _) => {
            // FNV-1a over the UTF-8 bytes.
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in s.bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            h
        }
        _ => 0,
    }
}

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn hash_combine(a: u64, b: u64) -> u64 {
    a ^ b
        .wrapping_add(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(a << 6)
        .wrapping_add(a >> 2)
}

/// How `print`/`println` and templates show a value.
pub fn display(v: &Value, t: &Ty) -> String {
    match (v, t) {
        (Value::Int(x), _) => x.to_string(),
        (Value::Float(x), Ty::Float(FloatTy::F32)) => format_float(*x, true),
        (Value::Float(x), _) => format_float(*x, false),
        (Value::Bool(b), _) => b.to_string(),
        (Value::Char(c), _) => c.to_string(),
        (Value::Str(s), _) => s.to_string(),
        (Value::Null, _) => "null".into(),
        (
            Value::Struct(_)
            | Value::Enum(..)
            | Value::Object(_)
            | Value::Iface(_)
            | Value::Closure(_)
            | Value::Array(_)
            | Value::Ref(..)
            | Value::Weak(_)
            | Value::Void
            | Value::Uninit,
            _,
        ) => String::new(),
    }
}

/// Field-wise equality (floats compare as IEEE values, so `NaN != NaN`).
fn values_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Struct(x), Value::Struct(y)) => x.iter().zip(y).all(|(p, q)| values_eq(p, q)),
        (Value::Enum(t, x), Value::Enum(u, y)) => {
            t == u && x.iter().zip(y).all(|(p, q)| values_eq(p, q))
        }
        (Value::Float(x), Value::Float(y)) => x == y,
        // Pointers: the same place.
        (Value::Ref(c, p), Value::Ref(d, q)) => Rc::ptr_eq(c, d) && p == q,
        _ => a == b,
    }
}

/// The debug form (`${v:?}`): `Point { x: 1, y: 2 }`, `[1, "a"]`, `0..10`,
/// `(0..10).step(2)`; parts in their own debug form. Must match the native helpers.
fn debug_value(p: &Program, v: &Value, t: &Ty) -> String {
    let list = |items: &mut dyn Iterator<Item = String>| {
        format!("[{}]", items.collect::<Vec<_>>().join(", "))
    };
    match (v, t) {
        (Value::Struct(fields), Ty::Struct(sid, _, args)) => {
            let layout = p.layout(*sid);
            let owner = Ty::type_owner(*sid);
            if fields.is_empty() {
                return format!("{} {{}}", layout.name);
            }
            let parts: Vec<String> = layout
                .fields
                .iter()
                .zip(fields)
                .map(|((name, fty), fv)| {
                    format!("{name}: {}", debug_value(p, fv, &fty.subst(owner, args)))
                })
                .collect();
            format!("{} {{ {} }}", layout.name, parts.join(", "))
        }
        (Value::Enum(vi, fields), Ty::Enum(eid, _)) => {
            let layout = p.enum_layout(*eid);
            let v = &layout.variants[*vi as usize];
            if v.fields.is_empty() && !v.has_parens {
                return format!("{}.{}", layout.name, v.name);
            }
            let parts: Vec<String> = v
                .fields
                .iter()
                .zip(fields)
                .map(|((name, fty), fv)| format!("{name}: {}", debug_value(p, fv, fty)))
                .collect();
            format!("{}.{}({})", layout.name, v.name, parts.join(", "))
        }
        (Value::Struct(parts), Ty::Tuple(ts)) => {
            list(&mut parts.iter().zip(ts).map(|(v, t)| debug_value(p, v, t)))
        }
        (Value::Struct(parts), Ty::Fixed(et, _)) => {
            list(&mut parts.iter().map(|v| debug_value(p, v, et)))
        }
        (Value::Array(a), Ty::Array(et)) => {
            list(&mut a.borrow().iter().map(|v| debug_value(p, v, et)))
        }
        (Value::Null, _) => "null".into(),
        (v, Ty::Nullable(inner)) => debug_value(p, v, inner),
        (Value::Object(_), Ty::Class(sid, _, _)) => format!("<{}>", p.layout(*sid).name),
        (Value::Struct(parts), Ty::Range(_)) => match &parts[..] {
            [Value::Int(a), Value::Int(b), Value::Int(1)] => format!("{a}..{b}"),
            [Value::Int(a), Value::Int(b), Value::Int(s)] => format!("({a}..{b}).step({s})"),
            other => unreachable!("range {other:?}"),
        },
        _ => {
            let debug = FormatSpec {
                kind: Kind::Debug,
                ..FormatSpec::default()
            };
            format_spec(v, t, &debug)
        }
    }
}

/// Formats a float like JavaScript's `Number.prototype.toString`: the shortest digits that
/// round-trip (at `f32` precision for `f32`; ties go to the even digit), in fixed notation for
/// 1e-7 ≤ |x| < 1e21 and exponential notation otherwise. `-0` prints as `0`.
pub fn format_float(x: f64, is_f32: bool) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if x == 0.0 {
        return "0".into();
    }
    let neg = x < 0.0;
    let (digits, n) = shortest_digits(x.abs(), is_f32);
    js_number_string(neg, &digits, n)
}

/// The shortest digits `d1…dk` and decimal exponent `n` with `|x| = 0.d1…dk × 10^n` that
/// round-trip. Uses the shortest correctly rounded `%.*e` (rounding half to even, so ties pick
/// the even digit, as ECMAScript requires, and as the C runtime does). `x` must be finite.
fn shortest_digits(x: f64, is_f32: bool) -> (String, i32) {
    if x == 0.0 {
        return ("0".into(), 1);
    }
    let max = if is_f32 { 9 } else { 17 };
    let sci = (1..=max)
        .map(|p| {
            if is_f32 {
                format!("{:.*e}", p - 1, x as f32)
            } else {
                format!("{:.*e}", p - 1, x)
            }
        })
        .find(|s| {
            if is_f32 {
                s.parse::<f32>().ok() == Some(x as f32)
            } else {
                s.parse::<f64>().ok() == Some(x)
            }
        })
        .expect("17 significant digits always round-trip");
    let sci = sci.trim_start_matches('-');
    let (mant, exp) = sci.split_once('e').expect("scientific format");
    let mut digits: String = mant.chars().filter(|c| *c != '.').collect();
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let exp: i32 = exp.parse().expect("exponent");
    (digits, exp + 1)
}

// ---------------------------------------------------------------- format specs

use ppl_sema::format::{Align, FormatSpec, Kind, Sign};

/// Formats a template part with a spec (spec §1.4). Must match `ppl_fmt_*` in the C runtime.
pub fn format_spec(v: &Value, t: &Ty, spec: &FormatSpec) -> String {
    let is_f32 = matches!(t, Ty::Float(FloatTy::F32));
    let (neg, prefix, body, numeric) = match v {
        Value::Int(x) => {
            let mag = x.unsigned_abs();
            let body = match spec.kind {
                Kind::Hex => format!("{mag:x}"),
                Kind::HexUpper => format!("{mag:X}"),
                Kind::Oct => format!("{mag:o}"),
                Kind::Bin => format!("{mag:b}"),
                Kind::Exp | Kind::ExpUpper | Kind::Fixed | Kind::Percent => {
                    let (neg, body) = float_body(*x as f64, false, spec);
                    return pad(neg, "", &body, true, spec);
                }
                _ => mag.to_string(),
            };
            let prefix = match (spec.alternate, spec.kind) {
                (true, Kind::Hex | Kind::HexUpper) => "0x",
                (true, Kind::Oct) => "0o",
                (true, Kind::Bin) => "0b",
                _ => "",
            };
            (*x < 0, prefix, body, true)
        }
        Value::Float(x) => {
            let (neg, body) = float_body(*x, is_f32, spec);
            (neg, "", body, true)
        }
        Value::Str(s) => {
            let body = if spec.kind == Kind::Debug {
                quote(s, '"')
            } else {
                match spec.precision {
                    Some(p) => s.chars().take(p as usize).collect(),
                    None => s.to_string(),
                }
            };
            (false, "", body, false)
        }
        Value::Char(c) => {
            let body = if spec.kind == Kind::Debug {
                quote(&c.to_string(), '\'')
            } else {
                c.to_string()
            };
            (false, "", body, false)
        }
        Value::Bool(b) => (false, "", b.to_string(), false),
        Value::Null => (false, "", "null".into(), false),
        Value::Struct(_)
        | Value::Enum(..)
        | Value::Object(_)
        | Value::Iface(_)
        | Value::Closure(_)
        | Value::Array(_)
        | Value::Ref(..)
        | Value::Weak(_)
        | Value::Void
        | Value::Uninit => (false, "", String::new(), false),
    };
    pad(neg, prefix, &body, numeric, spec)
}

/// The unsigned text of a float for `spec`, and whether it's negative.
fn float_body(x: f64, is_f32: bool, spec: &FormatSpec) -> (bool, String) {
    if x.is_nan() {
        return (false, "NaN".into());
    }
    let neg = x < 0.0;
    let a = x.abs();
    if a.is_infinite() {
        return (neg, "Infinity".into());
    }
    let p = spec.precision.map(|p| p as usize);
    let body = match spec.kind {
        Kind::Fixed => match p {
            Some(p) => format!("{a:.p$}"),
            None => shortest_fixed(a, is_f32),
        },
        Kind::Exp | Kind::ExpUpper => {
            let s = match p {
                Some(p) => format!("{a:.p$e}"),
                None => shortest_exp(a, is_f32),
            };
            if spec.kind == Kind::ExpUpper {
                s.replace('e', "E")
            } else {
                s
            }
        }
        Kind::Percent => {
            let y = a * 100.0;
            let s = match p {
                Some(p) => format!("{y:.p$}"),
                None => shortest_fixed(y, false),
            };
            s + "%"
        }
        _ => match p {
            Some(p) => format!("{a:.p$}"),
            None => format_float(a, is_f32),
        },
    };
    (neg, body)
}

/// Shortest round-trip digits in fixed notation (never an exponent).
fn shortest_fixed(a: f64, is_f32: bool) -> String {
    let (digits, n) = shortest_digits(a, is_f32);
    let k = digits.len() as i32;
    if n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else if n >= k {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    }
}

/// Shortest round-trip digits in exponential notation, Rust style: `1.2345e3`, `1e-7`.
fn shortest_exp(a: f64, is_f32: bool) -> String {
    let (digits, n) = shortest_digits(a, is_f32);
    let mant = if digits.len() > 1 {
        format!("{}.{}", &digits[..1], &digits[1..])
    } else {
        digits
    };
    format!("{mant}e{}", n - 1)
}

/// `"text"` / `'c'` with escapes: `\\`, the quote, `\n`, `\r`, `\t`, `\0`, and `\u{…}` for
/// other control characters.
fn quote(s: &str, q: char) -> String {
    let mut out = String::new();
    out.push(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7F => {
                out.push_str(&format!("\\u{{{:x}}}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

fn pad(neg: bool, prefix: &str, body: &str, numeric: bool, spec: &FormatSpec) -> String {
    let sign = match (neg, spec.sign) {
        (true, _) => "-",
        (false, Sign::Plus) if numeric => "+",
        (false, Sign::Space) if numeric => " ",
        _ => "",
    };
    let len = sign.chars().count() + prefix.chars().count() + body.chars().count();
    let width = spec.width.unwrap_or(0) as usize;
    if len >= width {
        return format!("{sign}{prefix}{body}");
    }
    let missing = width - len;
    if spec.zero && numeric {
        return format!("{sign}{prefix}{}{body}", "0".repeat(missing));
    }
    let align = spec
        .align
        .unwrap_or(if numeric { Align::Right } else { Align::Left });
    let (left, right) = match align {
        Align::Left => (0, missing),
        Align::Right => (missing, 0),
        Align::Center => (missing / 2, missing - missing / 2),
    };
    let fill = |n: usize| spec.fill.to_string().repeat(n);
    format!("{}{sign}{prefix}{body}{}", fill(left), fill(right))
}

/// Lays out `0.d1d2…dk × 10^n` the way ECMAScript's Number::toString does.
fn js_number_string(neg: bool, digits: &str, n: i32) -> String {
    let k = digits.len() as i32;
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    if k <= n && n <= 21 {
        s.push_str(digits);
        s.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        s.push_str(&digits[..n as usize]);
        s.push('.');
        s.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        s.push_str("0.");
        s.extend(std::iter::repeat_n('0', (-n) as usize));
        s.push_str(digits);
    } else {
        s.push_str(&digits[..1]);
        if k > 1 {
            s.push('.');
            s.push_str(&digits[1..]);
        }
        s.push('e');
        let e = n - 1;
        s.push(if e < 0 { '-' } else { '+' });
        s.push_str(&e.abs().to_string());
    }
    s
}

/// `Math`'s floating-point functions by number (the same table as `ppl_math` in the C
/// runtime; both use the platform's libm, so results agree bit for bit).
fn math_op(op: i64, x: f64, y: f64) -> f64 {
    match op {
        0 => x.sqrt(),
        1 => x.sin(),
        2 => x.cos(),
        3 => x.tan(),
        4 => x.asin(),
        5 => x.acos(),
        6 => x.atan(),
        7 => x.atan2(y),
        8 => x.exp(),
        9 => x.ln(),
        10 => x.log2(),
        11 => x.log10(),
        12 => x.powf(y),
        13 => x.floor(),
        14 => x.ceil(),
        15 => x.trunc(),
        16 => x.cbrt(),
        17 => x.hypot(y),
        18 => x.sinh(),
        19 => x.cosh(),
        20 => x.tanh(),
        21 => x.asinh(),
        22 => x.acosh(),
        23 => x.atanh(),
        24 => x.exp_m1(),
        25 => x.ln_1p(),
        26 => x % y,
        // f32 rounding (`Math.fround`).
        27 => x as f32 as f64,
        // A double's bits in two exact 32-bit halves (std/js).
        28 => f64::from_bits(((x as u64) << 32) | (y as u64 & 0xffff_ffff)),
        29 => (x.to_bits() >> 32) as f64,
        30 => (x.to_bits() & 0xffff_ffff) as f64,
        _ => f64::NAN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::excessive_precision)] // the tie cases need their exact binary values
    fn float_formatting_matches_js() {
        let cases: &[(f64, &str)] = &[
            (1.0, "1"),
            (0.1 + 0.2, "0.30000000000000004"),
            (2.75, "2.75"),
            (-2.5, "-2.5"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (123456789.0, "123456789"),
            (0.000001, "0.000001"),
            (0.0000001, "1e-7"),
            (1.5e-10, "1.5e-10"),
            (-0.0, "0"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
        ];
        for (x, want) in cases {
            assert_eq!(format_float(*x, false), *want, "for {x:e}");
        }
        assert_eq!(format_float(0.1f32 as f64, true), "0.1");
        assert_eq!(format_float(16777217.0f32 as f64, true), "16777216");
        // Ties between two shortest candidates go to the even digit (ECMAScript).
        assert_eq!(format_float(3684605.25, true), "3684605.2");
        assert_eq!(
            format_float(-167581363823776.125, false),
            "-167581363823776.12"
        );
    }
}
