//! Semantic types (spec §2.1–§2.2).

use std::fmt;
use std::sync::Arc;

/// Identifies a `struct` declaration in the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StructId(pub u32);

/// Identifies an `enum` declaration in the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EnumId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntTy {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    Isize,
    Usize,
}

impl IntTy {
    pub fn signed(self) -> bool {
        matches!(
            self,
            IntTy::I8 | IntTy::I16 | IntTy::I32 | IntTy::I64 | IntTy::Isize
        )
    }

    /// Width in bits. `isize`/`usize` are 64-bit on every current target.
    pub fn bits(self) -> u32 {
        match self {
            IntTy::I8 | IntTy::U8 => 8,
            IntTy::I16 | IntTy::U16 => 16,
            IntTy::I32 | IntTy::U32 => 32,
            IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize => 64,
        }
    }

    pub fn min(self) -> i128 {
        if self.signed() {
            -(1i128 << (self.bits() - 1))
        } else {
            0
        }
    }

    pub fn max(self) -> i128 {
        if self.signed() {
            (1i128 << (self.bits() - 1)) - 1
        } else {
            (1i128 << self.bits()) - 1
        }
    }

    pub fn contains(self, v: i128) -> bool {
        (self.min()..=self.max()).contains(&v)
    }

    pub fn name(self) -> &'static str {
        match self {
            IntTy::I8 => "i8",
            IntTy::I16 => "i16",
            IntTy::I32 => "i32",
            IntTy::I64 => "i64",
            IntTy::U8 => "u8",
            IntTy::U16 => "u16",
            IntTy::U32 => "u32",
            IntTy::U64 => "u64",
            IntTy::Isize => "isize",
            IntTy::Usize => "usize",
        }
    }

    fn pointer_sized(self) -> bool {
        matches!(self, IntTy::Isize | IntTy::Usize)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FloatTy {
    F32,
    F64,
}

impl FloatTy {
    pub fn name(self) -> &'static str {
        match self {
            FloatTy::F32 => "f32",
            FloatTy::F64 => "f64",
        }
    }

    /// Bits of mantissa precision (including the implicit bit).
    fn precision(self) -> u32 {
        match self {
            FloatTy::F32 => 24,
            FloatTy::F64 => 53,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    Int(IntTy),
    Float(FloatTy),
    Bool,
    Char,
    Str,
    Void,
    Never,
    /// The type of the `null` literal.
    Null,
    /// A user-defined struct (the name is kept for messages).
    Struct(StructId, Arc<str>),
    /// A class instance: a reference to a heap object (shares the id space with structs).
    Class(StructId, Arc<str>),
    /// A user-defined enum (a tagged union).
    Enum(EnumId, Arc<str>),
    /// Internal: a reference to a value (method receivers). Not writable in source.
    Ref(Box<Ty>),
    /// `T[]` / `Array<T>`: a growable, heap-allocated array (a reference type).
    Array(Box<Ty>),
    /// `[T; N]`: a fixed-size array stored inline (a value type).
    Fixed(Box<Ty>, u64),
    /// `[A, B]`: a tuple (a value type).
    Tuple(Vec<Ty>),
    /// `a..b` (with an optional `.step(n)`): start, end and step of an integer type.
    Range(IntTy),
    /// `T?`
    Nullable(Box<Ty>),
    /// A type error was already reported; suppresses follow-up errors.
    Error,
}

impl Ty {
    pub const INT: Ty = Ty::Int(IntTy::I64);
    pub const NUMBER: Ty = Ty::Float(FloatTy::F64);

    /// A built-in type name (`int` and `number` are aliases).
    pub fn from_name(name: &str) -> Option<Ty> {
        Some(match name {
            "i8" => Ty::Int(IntTy::I8),
            "i16" => Ty::Int(IntTy::I16),
            "i32" => Ty::Int(IntTy::I32),
            "i64" | "int" => Ty::Int(IntTy::I64),
            "u8" => Ty::Int(IntTy::U8),
            "u16" => Ty::Int(IntTy::U16),
            "u32" => Ty::Int(IntTy::U32),
            "u64" => Ty::Int(IntTy::U64),
            "isize" => Ty::Int(IntTy::Isize),
            "usize" => Ty::Int(IntTy::Usize),
            "f32" => Ty::Float(FloatTy::F32),
            "f64" | "number" => Ty::Float(FloatTy::F64),
            "bool" => Ty::Bool,
            "char" => Ty::Char,
            "string" => Ty::Str,
            "void" => Ty::Void,
            "never" => Ty::Never,
            _ => return None,
        })
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, Ty::Int(_) | Ty::Float(_))
    }

    pub fn is_int(&self) -> bool {
        matches!(self, Ty::Int(_))
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Ty::Error)
    }

    /// Reference types are pointers at runtime, so `T?` is a nullable pointer; other types
    /// get a presence flag.
    pub fn is_ref(&self) -> bool {
        matches!(self, Ty::Str | Ty::Array(_) | Ty::Class(..))
    }

    /// The element type of an indexable collection (`T[]`, `[T; N]`).
    pub fn elem(&self) -> Option<&Ty> {
        match self {
            Ty::Array(t) | Ty::Fixed(t, _) => Some(t),
            _ => None,
        }
    }

    /// `T` → `T?` (idempotent; `void` and `never` stay as they are).
    pub fn nullable(self) -> Ty {
        match self {
            Ty::Nullable(_) | Ty::Null | Ty::Error | Ty::Void | Ty::Never => self,
            t => Ty::Nullable(Box::new(t)),
        }
    }

    /// Whether a value of type `self` converts to `to` implicitly (spec §2.2: only when every
    /// value is exactly representable in the target).
    pub fn widens_to(&self, to: &Ty) -> bool {
        match (self, to) {
            (a, b) if a == b => true,
            (Ty::Never, _) | (Ty::Error, _) | (_, Ty::Error) => true,
            (Ty::Int(a), Ty::Int(b)) => {
                !a.pointer_sized() && !b.pointer_sized() && b.min() <= a.min() && a.max() <= b.max()
            }
            (Ty::Int(a), Ty::Float(f)) => {
                !a.pointer_sized() && a.bits() - u32::from(a.signed()) <= f.precision()
            }
            (Ty::Float(FloatTy::F32), Ty::Float(FloatTy::F64)) => true,
            (Ty::Null, Ty::Nullable(_)) => true,
            (Ty::Nullable(a), Ty::Nullable(b)) => a.widens_to(b),
            (t, Ty::Nullable(inner)) => t.widens_to(inner),
            _ => false,
        }
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Int(i) => f.write_str(i.name()),
            Ty::Float(x) => f.write_str(x.name()),
            Ty::Bool => f.write_str("bool"),
            Ty::Char => f.write_str("char"),
            Ty::Str => f.write_str("string"),
            Ty::Void => f.write_str("void"),
            Ty::Never => f.write_str("never"),
            Ty::Null => f.write_str("null"),
            Ty::Struct(_, name) | Ty::Enum(_, name) | Ty::Class(_, name) => f.write_str(name),
            Ty::Ref(t) => write!(f, "&{t}"),
            Ty::Array(t) if matches!(**t, Ty::Nullable(_)) => write!(f, "({t})[]"),
            Ty::Array(t) => write!(f, "{t}[]"),
            Ty::Fixed(t, n) => write!(f, "[{t}; {n}]"),
            Ty::Tuple(ts) => {
                let parts: Vec<String> = ts.iter().map(|t| t.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Ty::Range(it) => write!(f, "Range<{}>", it.name()),
            Ty::Nullable(t) => write!(f, "{t}?"),
            Ty::Error => f.write_str("{error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossless_widening() {
        let t = |s| Ty::from_name(s).unwrap();
        assert!(t("i32").widens_to(&t("i64")));
        assert!(t("u8").widens_to(&t("i16")));
        assert!(t("u32").widens_to(&t("f64")));
        assert!(t("i32").widens_to(&t("f64")));
        assert!(t("f32").widens_to(&t("f64")));
        assert!(t("i16").widens_to(&t("f32")));
        assert!(!t("i64").widens_to(&t("i32")));
        assert!(!t("i8").widens_to(&t("u16")));
        assert!(!t("i64").widens_to(&t("f64")));
        assert!(!t("i32").widens_to(&t("f32")));
        assert!(!t("usize").widens_to(&t("u64")));
        assert!(t("i32").widens_to(&t("i32").nullable()));
    }

    #[test]
    fn ranges() {
        assert_eq!(IntTy::I8.min(), -128);
        assert_eq!(IntTy::U64.max(), u64::MAX as i128);
        assert!(!IntTy::U8.contains(256));
    }
}
