//! MIR interpreter for ppl (comptime and `ppl run --interp`).
//!
//! Semantics match the native backend exactly: integers trap on overflow, division by zero
//! and out-of-range conversions; floats follow IEEE-754 (computed at `f32` precision for
//! `f32`); printing uses [`format_float`] for numbers.

use ppl_mir::mir::{
    self, BinOp, Callee, Const, Loc, Operand, Place, Program, Rvalue, Terminator, UnOp, panics,
};
use ppl_sema::hir::{Builtin, FnId};
use ppl_sema::{FloatTy, IntTy, Ty};
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
    Void,
    Uninit,
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
        globals: vec![Value::Uninit; program.globals.len()],
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
    globals: Vec<Value>,
    depth: usize,
}

struct Frame<'p> {
    f: &'p mir::Function,
    locals: Vec<Value>,
}

impl Frame<'_> {
    fn ty(&self, p: &Program, place: &Place) -> Ty {
        match place {
            Place::Local(l) => self.f.local_ty(*l).clone(),
            Place::Global(g) => p.globals[g.0 as usize].ty.clone(),
        }
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
            locals: vec![Value::Uninit; f.locals.len()],
        };
        for (p, a) in f.params.iter().zip(args) {
            frame.locals[p.0 as usize] = a;
        }
        let r = self.exec(&mut frame);
        self.depth -= 1;
        r
    }

    fn read(&self, frame: &Frame, place: &Place) -> Value {
        match place {
            Place::Local(l) => frame.locals[l.0 as usize].clone(),
            Place::Global(g) => self.globals[g.0 as usize].clone(),
        }
    }

    fn write(&mut self, frame: &mut Frame, place: &Place, v: Value) {
        match place {
            Place::Local(l) => frame.locals[l.0 as usize] = v,
            Place::Global(g) => self.globals[g.0 as usize] = v,
        }
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
        (Value::Void, _) | (Value::Uninit, _) => String::new(),
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
    // The shortest correctly rounded scientific form that round-trips ("d.ddde±x"). Rounding
    // is half-to-even, so ties pick the even digit, as ECMAScript requires (and as the C
    // runtime's `%.*e` does).
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
    let (neg, sci) = match sci.strip_prefix('-') {
        Some(s) => (true, s),
        None => (false, sci.as_str()),
    };
    let (mant, exp) = sci.split_once('e').expect("scientific format");
    let mut digits: String = mant.chars().filter(|c| *c != '.').collect();
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let exp: i32 = exp.parse().expect("exponent");
    js_number_string(neg, &digits, exp + 1)
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
