//! Arithmetic on primitive values, shared by constant folding and the interpreter so both
//! compute (and panic) exactly as native code does.

use crate::mir::{BinOp, UnOp, panics};
use pico_sema::{FloatTy, IntTy};

/// A primitive value: integers of any type are held as `i128`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scalar {
    Int(i128),
    Float(f64),
    Bool(bool),
    Char(char),
}

/// What a primitive operand's type is, as far as arithmetic cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prim {
    Int(IntTy),
    Float(FloatTy),
    Bool,
    Char,
}

impl Prim {
    /// The primitive behind `t` (not erased: callers pass erased MIR types).
    pub fn of(t: &pico_sema::Ty) -> Option<Prim> {
        use pico_sema::Ty;
        Some(match t {
            Ty::Int(it) => Prim::Int(*it),
            Ty::Float(ft) => Prim::Float(*ft),
            Ty::Bool => Prim::Bool,
            Ty::Char => Prim::Char,
            _ => return None,
        })
    }
}

/// The result of a failed operation: the panic message.
pub type Trap = &'static str;

/// `a op b` where `t` is the type of `a` (shifts take any integer amount).
pub fn binary(op: BinOp, a: Scalar, b: Scalar, t: Prim) -> Result<Scalar, Trap> {
    use BinOp::*;
    use Scalar::*;
    Ok(match (a, b, t) {
        (Int(a), Int(b), Prim::Int(it)) => {
            let r = match op {
                Add => a + b,
                Sub => a - b,
                Mul => a.checked_mul(b).ok_or(panics::OVERFLOW)?,
                Div | Rem => {
                    if b == 0 {
                        return Err(panics::DIV_ZERO);
                    }
                    if op == Div { a / b } else { a % b }
                }
                Pow => {
                    if b < 0 {
                        return Err(panics::NEG_EXPONENT);
                    }
                    int_pow(a, b, it).ok_or(panics::OVERFLOW)?
                }
                Shl | Shr | UShr => {
                    if b < 0 || b >= it.bits() as i128 {
                        return Err(panics::SHIFT);
                    }
                    let bits = to_bits(a, it);
                    return Ok(Int(match op {
                        Shl => from_bits(bits << b, it),
                        UShr => from_bits(bits >> b, it),
                        // Arithmetic shift: shift the signed value.
                        _ if it.signed() => a >> b,
                        _ => (bits >> b) as i128,
                    }));
                }
                BitAnd => return Ok(Int(from_bits(to_bits(a, it) & to_bits(b, it), it))),
                BitOr => return Ok(Int(from_bits(to_bits(a, it) | to_bits(b, it), it))),
                BitXor => return Ok(Int(from_bits(to_bits(a, it) ^ to_bits(b, it), it))),
                Eq => return Ok(Bool(a == b)),
                Ne => return Ok(Bool(a != b)),
                Lt => return Ok(Bool(a < b)),
                Le => return Ok(Bool(a <= b)),
                Gt => return Ok(Bool(a > b)),
                Ge => return Ok(Bool(a >= b)),
            };
            int(r, it)?
        }
        (Float(a), Float(b), Prim::Float(ft)) => match op {
            Add => Float(round(a + b, ft)),
            Sub => Float(round(a - b, ft)),
            Mul => Float(round(a * b, ft)),
            Div => Float(round(a / b, ft)),
            Rem => Float(round(a % b, ft)),
            Pow => Float(match ft {
                FloatTy::F32 => (a as f32).powf(b as f32) as f64,
                FloatTy::F64 => a.powf(b),
            }),
            Eq => Bool(a == b),
            Ne => Bool(a != b),
            Lt => Bool(a < b),
            Le => Bool(a <= b),
            Gt => Bool(a > b),
            Ge => Bool(a >= b),
            _ => unreachable!("type-checked float operator {op:?}"),
        },
        (Bool(a), Bool(b), _) => Bool(match op {
            Eq => a == b,
            Ne => a != b,
            BitAnd => a & b,
            BitOr => a | b,
            BitXor => a ^ b,
            _ => unreachable!("type-checked bool operator {op:?}"),
        }),
        (Char(a), Char(b), _) => Bool(match op {
            Eq => a == b,
            Ne => a != b,
            Lt => a < b,
            Le => a <= b,
            Gt => a > b,
            Ge => a >= b,
            _ => unreachable!("type-checked char operator {op:?}"),
        }),
        (a, b, t) => unreachable!("binary {op:?} on {a:?}, {b:?} ({t:?})"),
    })
}

/// `op a` where `t` is the type of `a`.
pub fn unary(op: UnOp, a: Scalar, t: Prim) -> Result<Scalar, Trap> {
    Ok(match (op, a, t) {
        (UnOp::Not, Scalar::Bool(b), _) => Scalar::Bool(!b),
        (UnOp::Neg, Scalar::Float(x), Prim::Float(ft)) => Scalar::Float(round(-x, ft)),
        (UnOp::Neg, Scalar::Int(x), Prim::Int(it)) => int(-x, it)?,
        (UnOp::BitNot, Scalar::Int(x), Prim::Int(it)) => {
            Scalar::Int(from_bits(!to_bits(x, it), it))
        }
        (op, a, t) => unreachable!("unary {op:?} on {a:?} ({t:?})"),
    })
}

/// `a as to` (numeric and char conversions); `None` when `to` isn't a primitive conversion.
pub fn cast(a: Scalar, to: Prim) -> Option<Result<Scalar, Trap>> {
    Some(match (a, to) {
        (Scalar::Int(x), Prim::Int(it)) => {
            if it.contains(x) {
                Ok(Scalar::Int(x))
            } else {
                Err(panics::INT_CAST)
            }
        }
        (Scalar::Int(x), Prim::Float(ft)) => Ok(Scalar::Float(round(x as f64, ft))),
        (Scalar::Float(x), Prim::Int(it)) => {
            let t = x.trunc();
            // Both bounds are exact powers of two (`max as f64` would round up).
            let (lo, hi_excl) = (it.min() as f64, (it.max() + 1) as f64);
            if x.is_nan() || t < lo || t >= hi_excl {
                Err(panics::FLOAT_CAST)
            } else {
                Ok(Scalar::Int(t as i128))
            }
        }
        (Scalar::Float(x), Prim::Float(ft)) => Ok(Scalar::Float(round(x, ft))),
        (Scalar::Char(c), Prim::Int(it)) => {
            let x = c as u32 as i128;
            if it.contains(x) {
                Ok(Scalar::Int(x))
            } else {
                Err(panics::INT_CAST)
            }
        }
        (Scalar::Int(x), Prim::Char) => u32::try_from(x)
            .ok()
            .and_then(char::from_u32)
            .map(Scalar::Char)
            .ok_or(panics::CHAR_CAST),
        (v @ Scalar::Char(_), Prim::Char) | (v @ Scalar::Bool(_), Prim::Bool) => Ok(v),
        _ => return None,
    })
}

fn int(v: i128, it: IntTy) -> Result<Scalar, Trap> {
    if it.contains(v) {
        Ok(Scalar::Int(v))
    } else {
        Err(panics::OVERFLOW)
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

/// `x` rounded to the precision of `ft`.
pub fn round(x: f64, ft: FloatTy) -> f64 {
    match ft {
        FloatTy::F32 => x as f32 as f64,
        FloatTy::F64 => x,
    }
}

/// Two's-complement bit pattern of `v` in `it`'s width.
pub fn to_bits(v: i128, it: IntTy) -> u128 {
    let mask = if it.bits() == 128 {
        u128::MAX
    } else {
        (1u128 << it.bits()) - 1
    };
    (v as u128) & mask
}

/// The value of `it` with this bit pattern (the low bits of `bits`).
pub fn from_bits(bits: u128, it: IntTy) -> i128 {
    let w = it.bits();
    let mask = (1u128 << w) - 1;
    let b = bits & mask;
    if it.signed() && b >> (w - 1) == 1 {
        (b as i128) - (1i128 << w)
    } else {
        b as i128
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_arithmetic_traps_like_native_code() {
        let u8 = Prim::Int(IntTy::U8);
        let (a, b) = (Scalar::Int(255), Scalar::Int(1));
        assert_eq!(binary(BinOp::Add, a, b, u8), Err(panics::OVERFLOW));
        assert_eq!(binary(BinOp::Shl, a, b, u8), Ok(Scalar::Int(254)));
        assert_eq!(
            binary(BinOp::Shl, a, Scalar::Int(8), u8),
            Err(panics::SHIFT)
        );
        assert_eq!(
            binary(BinOp::Div, a, Scalar::Int(0), u8),
            Err(panics::DIV_ZERO)
        );
        let i8 = Prim::Int(IntTy::I8);
        assert_eq!(
            binary(BinOp::Shr, Scalar::Int(-128), Scalar::Int(7), i8),
            Ok(Scalar::Int(-1))
        );
        assert_eq!(
            binary(BinOp::UShr, Scalar::Int(-128), Scalar::Int(7), i8),
            Ok(Scalar::Int(1))
        );
        assert_eq!(
            unary(UnOp::Neg, Scalar::Int(-128), i8),
            Err(panics::OVERFLOW)
        );
        assert_eq!(
            cast(Scalar::Int(300), Prim::Int(IntTy::U8)),
            Some(Err(panics::INT_CAST))
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
