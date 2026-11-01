//! MIR interpreter for ppl (comptime and `ppl run --interp`).
//!
//! Semantics match the native backend exactly: integers trap on overflow, division by zero
//! and out-of-range conversions; floats follow IEEE-754 (computed at `f32` precision for
//! `f32`); printing uses [`format_float`] for numbers.

use ppl_mir::mir::{
    self, Base, BinOp, Callee, Const, Loc, Operand, Place, Program, Proj, Rvalue, Terminator, UnOp,
    panics,
};
use ppl_sema::hir::{Builtin, FnId};
use ppl_sema::{FloatTy, IntTy, Ty};
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

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
    /// A growable array: shared, so cloning copies the reference.
    Array(Rc<RefCell<Vec<Value>>>),
    /// A reference into a variable: the cell and the path inside it.
    Ref(Cell, Rc<[Step]>),
    /// `null`. A non-null `T?` is just the `T` value.
    Null,
    Void,
    Uninit,
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

/// One resolved step of a path into a value: a part of an inline value, or an element of a
/// growable array.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    Part(usize),
    Elem(usize),
    /// A field of the object a class reference points to.
    ObjField(usize),
}

/// Storage for one variable. Locals and globals live in cells so references can point at them.
pub type Cell = Rc<RefCell<Value>>;

fn cell(v: Value) -> Cell {
    Rc::new(RefCell::new(v))
}

/// A panic: the message and where it happened.
#[derive(Debug, Clone, PartialEq)]
pub struct Panic {
    pub msg: String,
    pub file: String,
    pub loc: Loc,
}

impl Panic {
    /// The text printed to stderr (shared format with the native runtime).
    pub fn report(&self) -> String {
        format!(
            "panic: {}\n  at {}:{}:{}\n",
            self.msg, self.file, self.loc.line, self.loc.col
        )
    }
}

/// Runs the program's entry function, writing program output to `out`.
pub fn run(program: &Program, out: &mut dyn Write) -> Result<(), Panic> {
    let mut it = Interp {
        p: program,
        out,
        globals: (0..program.globals.len())
            .map(|_| cell(Value::Uninit))
            .collect(),
        depth: 0,
    };
    let r = it
        .call(program.entry, Vec::new(), Loc::default())
        .map(|_| ());
    let _ = it.out.flush();
    r
}

struct Interp<'p, 'o> {
    p: &'p Program,
    out: &'o mut dyn Write,
    globals: Vec<Cell>,
    depth: usize,
}

struct Frame<'p> {
    f: &'p mir::Function,
    locals: Vec<Cell>,
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
        }
    }

    fn call(&mut self, id: FnId, args: Vec<Value>, loc: Loc) -> Result<Value, Panic> {
        if self.depth >= MAX_DEPTH {
            return Err(self.panic(panics::STACK_OVERFLOW, loc));
        }
        self.depth += 1;
        let f = self.p.func(id);
        let mut frame = Frame {
            f,
            locals: (0..f.locals.len()).map(|_| cell(Value::Uninit)).collect(),
        };
        for (p, a) in f.params.iter().zip(args) {
            *frame.locals[p.0 as usize].borrow_mut() = a;
        }
        let r = self.exec(&mut frame);
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
                Proj::Field(i) if matches!(ty, Ty::Class(..)) => Step::ObjField(*i as usize),
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

    fn exec(&mut self, frame: &mut Frame) -> Result<Value, Panic> {
        let mut bb = mir::BlockId(0);
        loop {
            let block = frame.f.block(bb);
            for s in &block.stmts {
                let v = self.rvalue(frame, &s.rvalue, s.loc)?;
                self.write(frame, &s.place, v);
            }
            match &block.term {
                Terminator::Goto(t) => bb = *t,
                Terminator::Branch { cond, then, else_ } => {
                    bb = match self.operand(frame, cond) {
                        Value::Bool(true) => *then,
                        _ => *else_,
                    }
                }
                Terminator::Return(v) => {
                    return Ok(v.as_ref().map_or(Value::Void, |o| self.operand(frame, o)));
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
                            self.builtin(*b, &vals, &tys);
                            Value::Void
                        }
                    };
                    if let Some(d) = dest {
                        self.write(frame, d, r);
                    }
                    bb = *next;
                }
                Terminator::Panic { msg, loc } => {
                    let m = match self.operand(frame, msg) {
                        Value::Str(s) => s.to_string(),
                        _ => "panic".into(),
                    };
                    return Err(self.panic(&m, *loc));
                }
                Terminator::Unreachable => unreachable!("reached an unreachable block"),
            }
        }
    }

    fn builtin(&mut self, b: Builtin, vals: &[Value], tys: &[Ty]) {
        let text: String = vals.iter().zip(tys).map(|(v, t)| display(v, t)).collect();
        let _ = match b {
            Builtin::Println => writeln!(self.out, "{text}"),
            Builtin::Print => write!(self.out, "{text}"),
            _ => unreachable!("lowered to control flow"),
        };
    }

    fn rvalue(&mut self, frame: &Frame, rv: &Rvalue, loc: Loc) -> Result<Value, Panic> {
        match rv {
            Rvalue::Use(o) => Ok(self.operand(frame, o)),
            Rvalue::Unary(op, o) => {
                let t = self.operand_ty(frame, o);
                let v = self.operand(frame, o);
                match (op, v, &t) {
                    (UnOp::Not, Value::Bool(b), _) => Ok(Value::Bool(!b)),
                    (UnOp::Neg, Value::Float(x), Ty::Float(ft)) => Ok(Value::Float(round(-x, *ft))),
                    (UnOp::Neg, Value::Int(x), Ty::Int(it)) => self.int(-x, *it, loc),
                    (UnOp::BitNot, Value::Int(x), Ty::Int(it)) => {
                        Ok(Value::Int(from_bits(!to_bits(x, *it), *it)))
                    }
                    _ => unreachable!("type-checked unary operator"),
                }
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
            Rvalue::New(sid) => Ok(Value::Object(Rc::new(Obj {
                class: sid.0,
                fields: RefCell::new(vec![Value::Uninit; self.p.layout(*sid).fields.len()]),
            }))),
            Rvalue::Tag(o) => match self.operand(frame, o) {
                Value::Enum(v, _) => Ok(Value::Int(v as i128)),
                v => unreachable!("tag of {v:?}"),
            },
            Rvalue::IsNull(o) => Ok(Value::Bool(self.operand(frame, o) == Value::Null)),
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

    fn int(&self, v: i128, it: IntTy, loc: Loc) -> Result<Value, Panic> {
        if it.contains(v) {
            Ok(Value::Int(v))
        } else {
            Err(self.panic(panics::OVERFLOW, loc))
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
            (Value::Int(a), Value::Int(b), Ty::Int(it)) => {
                let it = *it;
                let r = match op {
                    Add => a + b,
                    Sub => a - b,
                    Mul => a
                        .checked_mul(b)
                        .ok_or_else(|| self.panic(panics::OVERFLOW, loc))?,
                    Div | Rem => {
                        if b == 0 {
                            return Err(self.panic(panics::DIV_ZERO, loc));
                        }
                        if op == Div { a / b } else { a % b }
                    }
                    Pow => {
                        if b < 0 {
                            return Err(self.panic(panics::NEG_EXPONENT, loc));
                        }
                        int_pow(a, b, it).ok_or_else(|| self.panic(panics::OVERFLOW, loc))?
                    }
                    Shl | Shr | UShr => {
                        if b < 0 || b >= it.bits() as i128 {
                            return Err(self.panic(panics::SHIFT, loc));
                        }
                        let bits = to_bits(a, it);
                        let r = match op {
                            Shl => bits << b,
                            UShr => bits >> b,
                            // Arithmetic shift: shift the signed value.
                            _ => {
                                return Ok(Value::Int(if it.signed() {
                                    a >> b
                                } else {
                                    (bits >> b) as i128
                                }));
                            }
                        };
                        return Ok(Value::Int(from_bits(r, it)));
                    }
                    BitAnd => {
                        return Ok(Value::Int(from_bits(to_bits(a, it) & to_bits(b, it), it)));
                    }
                    BitOr => return Ok(Value::Int(from_bits(to_bits(a, it) | to_bits(b, it), it))),
                    BitXor => {
                        return Ok(Value::Int(from_bits(to_bits(a, it) ^ to_bits(b, it), it)));
                    }
                    Eq => return Ok(Value::Bool(a == b)),
                    Ne => return Ok(Value::Bool(a != b)),
                    Lt => return Ok(Value::Bool(a < b)),
                    Le => return Ok(Value::Bool(a <= b)),
                    Gt => return Ok(Value::Bool(a > b)),
                    Ge => return Ok(Value::Bool(a >= b)),
                };
                self.int(r, it, loc)
            }
            (Value::Float(a), Value::Float(b), Ty::Float(ft)) => Ok(match op {
                Add => Value::Float(round(a + b, *ft)),
                Sub => Value::Float(round(a - b, *ft)),
                Mul => Value::Float(round(a * b, *ft)),
                Div => Value::Float(round(a / b, *ft)),
                Rem => Value::Float(round(a % b, *ft)),
                Pow => Value::Float(match ft {
                    FloatTy::F32 => (a as f32).powf(b as f32) as f64,
                    FloatTy::F64 => a.powf(b),
                }),
                Eq => Value::Bool(a == b),
                Ne => Value::Bool(a != b),
                Lt => Value::Bool(a < b),
                Le => Value::Bool(a <= b),
                Gt => Value::Bool(a > b),
                Ge => Value::Bool(a >= b),
                _ => unreachable!("type-checked float operator"),
            }),
            (Value::Bool(a), Value::Bool(b), _) => Ok(Value::Bool(match op {
                Eq => a == b,
                Ne => a != b,
                _ => unreachable!("type-checked bool operator"),
            })),
            (a @ (Value::Struct(_) | Value::Enum(..) | Value::Object(_)), b, _) => Ok(match op {
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
            (Value::Char(a), Value::Char(b), _) => Ok(Value::Bool(match op {
                Eq => a == b,
                Ne => a != b,
                Lt => a < b,
                Le => a <= b,
                Gt => a > b,
                Ge => a >= b,
                _ => unreachable!("type-checked char operator"),
            })),
            (x, y, t) => unreachable!("binary {op:?} on {x:?}, {y:?} ({t})"),
        }
    }

    fn cast(&self, v: Value, from: &Ty, to: &Ty, loc: Loc) -> Result<Value, Panic> {
        match (v, to) {
            (Value::Int(x), Ty::Int(it)) => {
                if it.contains(x) {
                    Ok(Value::Int(x))
                } else {
                    Err(self.panic(panics::INT_CAST, loc))
                }
            }
            (Value::Int(x), Ty::Float(ft)) => Ok(Value::Float(round(x as f64, *ft))),
            (Value::Float(x), Ty::Int(it)) => {
                let t = x.trunc();
                // Both bounds are exact powers of two (`max as f64` would round up).
                let (lo, hi_excl) = (it.min() as f64, (it.max() + 1) as f64);
                if x.is_nan() || t < lo || t >= hi_excl {
                    return Err(self.panic(panics::FLOAT_CAST, loc));
                }
                Ok(Value::Int(t as i128))
            }
            (Value::Float(x), Ty::Float(ft)) => Ok(Value::Float(round(x, *ft))),
            (Value::Char(c), Ty::Int(it)) => {
                let x = c as u32 as i128;
                if it.contains(x) {
                    Ok(Value::Int(x))
                } else {
                    Err(self.panic(panics::INT_CAST, loc))
                }
            }
            (Value::Int(x), Ty::Char) => u32::try_from(x)
                .ok()
                .and_then(char::from_u32)
                .map(Value::Char)
                .ok_or_else(|| self.panic(panics::CHAR_CAST, loc)),
            (v, _) if from == to => Ok(v),
            (v, t) => unreachable!("cast {v:?} from {from} to {t}"),
        }
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
    }
}

/// `a ** e` by squaring; `None` if the result doesn't fit in `it`.
fn int_pow(a: i128, mut e: i128, it: IntTy) -> Option<i128> {
    let (mut base, mut acc) = (a, 1i128);
    while e > 0 {
        if e & 1 == 1 {
            acc = acc.checked_mul(base).filter(|v| it.contains(*v))?;
        }
        e >>= 1;
        if e > 0 {
            // |base| ≥ 2 and base² out of range means the result is out of range too.
            base = base
                .checked_mul(base)
                .filter(|v| it.contains(*v) || base.abs() < 2)?;
        }
    }
    Some(acc)
}

fn round(x: f64, ft: FloatTy) -> f64 {
    match ft {
        FloatTy::F32 => x as f32 as f64,
        FloatTy::F64 => x,
    }
}

/// Two's-complement bit pattern of `v` in `it`'s width.
fn to_bits(v: i128, it: IntTy) -> u128 {
    let mask = if it.bits() == 128 {
        u128::MAX
    } else {
        (1u128 << it.bits()) - 1
    };
    (v as u128) & mask
}

fn from_bits(bits: u128, it: IntTy) -> i128 {
    let w = it.bits();
    let mask = (1u128 << w) - 1;
    let b = bits & mask;
    if it.signed() && b >> (w - 1) == 1 {
        (b as i128) - (1i128 << w)
    } else {
        b as i128
    }
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
            | Value::Array(_)
            | Value::Ref(..)
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
        (Value::Struct(fields), Ty::Struct(sid, _)) => {
            let layout = p.layout(*sid);
            if fields.is_empty() {
                return format!("{} {{}}", layout.name);
            }
            let parts: Vec<String> = layout
                .fields
                .iter()
                .zip(fields)
                .map(|((name, fty), fv)| format!("{name}: {}", debug_value(p, fv, fty)))
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
        (Value::Object(_), Ty::Class(sid, _)) => format!("<{}>", p.layout(*sid).name),
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
        | Value::Array(_)
        | Value::Ref(..)
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

    #[test]
    fn integer_power() {
        assert_eq!(int_pow(2, 62, IntTy::I64), Some(1 << 62));
        assert_eq!(int_pow(2, 63, IntTy::I64), None);
        assert_eq!(int_pow(-2, 63, IntTy::I64), Some(i64::MIN as i128));
        assert_eq!(int_pow(-1, 1_000_000_000_001, IntTy::I8), Some(-1));
        assert_eq!(int_pow(3, 0, IntTy::U8), Some(1));
        assert_eq!(int_pow(10, 3, IntTy::U8), None);
    }

    #[test]
    fn bit_patterns() {
        assert_eq!(to_bits(-1, IntTy::I8), 0xFF);
        assert_eq!(from_bits(0xFF, IntTy::I8), -1);
        assert_eq!(from_bits(0x80, IntTy::U8), 128);
    }
}
