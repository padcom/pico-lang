//! Format specs in template literals (spec §1.4): `${x:08.2f}`.
//!
//! ```text
//! spec      := [[fill] align] [sign] ["#"] ["0"] [width] ["." precision] [type]
//! align     := "<" | ">" | "^"
//! sign      := "+" | "-" | " "
//! type      := "d" | "x" | "X" | "o" | "b" | "e" | "E" | "f" | "%" | "?"
//! ```
//!
//! Specs are parsed and checked against the value's type at compile time; both backends then
//! format with the same rules (see `ppl_interp::format_spec` and `runtime/src/ppl_rt.c`).

use crate::ty::Ty;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Align {
    Left,
    Right,
    Center,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sign {
    /// `-` (the default): only negative numbers get a sign.
    Minus,
    /// `+`: always show the sign.
    Plus,
    /// ` `: a space for non-negative numbers.
    Space,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// No type: the value's normal text.
    Default,
    /// `d`
    Dec,
    /// `x`
    Hex,
    /// `X`
    HexUpper,
    /// `o`
    Oct,
    /// `b`
    Bin,
    /// `e`
    Exp,
    /// `E`
    ExpUpper,
    /// `f`
    Fixed,
    /// `%`
    Percent,
    /// `?`
    Debug,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FormatSpec {
    pub fill: char,
    pub align: Option<Align>,
    pub sign: Sign,
    /// `#`: `0x`/`0o`/`0b` prefixes.
    pub alternate: bool,
    /// `0`: pad numbers with zeros after the sign.
    pub zero: bool,
    pub width: Option<u32>,
    pub precision: Option<u32>,
    pub kind: Kind,
}

impl Default for FormatSpec {
    fn default() -> Self {
        FormatSpec {
            fill: ' ',
            align: None,
            sign: Sign::Minus,
            alternate: false,
            zero: false,
            width: None,
            precision: None,
            kind: Kind::Default,
        }
    }
}

/// Parses a spec. The error explains what's wrong.
pub fn parse(spec: &str) -> Result<FormatSpec, String> {
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;
    let mut f = FormatSpec::default();
    let align_of = |c: char| match c {
        '<' => Some(Align::Left),
        '>' => Some(Align::Right),
        '^' => Some(Align::Center),
        _ => None,
    };
    if chars.len() >= 2
        && let Some(a) = align_of(chars[1])
    {
        f.fill = chars[0];
        f.align = Some(a);
        i = 2;
    } else if let Some(a) = chars.first().copied().and_then(align_of) {
        f.align = Some(a);
        i = 1;
    }
    match chars.get(i) {
        Some('+') => {
            f.sign = Sign::Plus;
            i += 1;
        }
        Some('-') => i += 1,
        Some(' ') => {
            f.sign = Sign::Space;
            i += 1;
        }
        _ => {}
    }
    if chars.get(i) == Some(&'#') {
        f.alternate = true;
        i += 1;
    }
    if chars.get(i) == Some(&'0') {
        f.zero = true;
        i += 1;
    }
    let number = |i: &mut usize| -> Option<u32> {
        let start = *i;
        while chars.get(*i).is_some_and(|c| c.is_ascii_digit()) {
            *i += 1;
        }
        (*i > start).then(|| {
            chars[start..*i]
                .iter()
                .collect::<String>()
                .parse()
                .unwrap_or(u32::MAX)
        })
    };
    f.width = number(&mut i);
    if f.width.is_some_and(|w| w > 1000) {
        return Err("the width is too large (at most 1000)".into());
    }
    if chars.get(i) == Some(&'.') {
        i += 1;
        f.precision = number(&mut i);
        if f.precision.is_none() {
            return Err("expected digits after `.`".into());
        }
        if f.precision.is_some_and(|p| p > 100) {
            return Err("the precision is too large (at most 100)".into());
        }
    }
    if let Some(&c) = chars.get(i) {
        f.kind = match c {
            'd' => Kind::Dec,
            'x' => Kind::Hex,
            'X' => Kind::HexUpper,
            'o' => Kind::Oct,
            'b' => Kind::Bin,
            'e' => Kind::Exp,
            'E' => Kind::ExpUpper,
            'f' => Kind::Fixed,
            '%' => Kind::Percent,
            '?' => Kind::Debug,
            _ => {
                return Err(format!(
                    "unknown format type `{c}` (expected one of d x X o b e E f % ?)"
                ));
            }
        };
        i += 1;
    }
    if i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        return Err(format!("unexpected `{rest}` at the end of the spec"));
    }
    Ok(f)
}

/// Checks that `spec` makes sense for a value of type `ty`.
/// Types shown only in the debug format: structs, tuples, arrays and ranges.
pub fn is_compound(ty: &Ty) -> bool {
    if let Ty::Nullable(inner) = ty {
        return is_compound(inner);
    }
    matches!(
        ty,
        Ty::Struct(..) | Ty::Enum(..) | Ty::Tuple(_) | Ty::Fixed(..) | Ty::Array(_) | Ty::Range(_)
    )
}

pub fn check(spec: &FormatSpec, ty: &Ty) -> Result<(), String> {
    if is_compound(ty) {
        if spec.kind != Kind::Debug {
            return Err(format!("`{ty}` only has the debug format `?`"));
        }
        if spec.zero || spec.sign != Sign::Minus || spec.precision.is_some() {
            return Err(
                "structs, tuples, arrays and ranges take only fill, alignment and width".into(),
            );
        }
        return Ok(());
    }
    let numeric = ty.is_numeric();
    let is_int = ty.is_int();
    let integer_kind = matches!(
        spec.kind,
        Kind::Dec | Kind::Hex | Kind::HexUpper | Kind::Oct | Kind::Bin
    );
    let float_kind = matches!(
        spec.kind,
        Kind::Exp | Kind::ExpUpper | Kind::Fixed | Kind::Percent
    );
    if integer_kind && !is_int {
        return Err(format!(
            "`{}` needs an integer, not `{ty}`",
            kind_char(spec.kind)
        ));
    }
    if float_kind && !numeric {
        return Err(format!(
            "`{}` needs a number, not `{ty}`",
            kind_char(spec.kind)
        ));
    }
    if spec.alternate
        && !matches!(
            spec.kind,
            Kind::Hex | Kind::HexUpper | Kind::Oct | Kind::Bin
        )
    {
        return Err("`#` only applies to `x`, `X`, `o` and `b`".into());
    }
    if (spec.zero || spec.sign != Sign::Minus) && !numeric {
        return Err(format!("signs and zero padding need a number, not `{ty}`"));
    }
    if spec.precision.is_some() && is_int && !float_kind {
        return Err("integers don't take a precision (use `.Nf` to format as a float)".into());
    }
    if spec.precision.is_some() && matches!(ty, Ty::Bool | Ty::Char) {
        return Err(format!("`{ty}` doesn't take a precision"));
    }
    Ok(())
}

fn kind_char(k: Kind) -> char {
    match k {
        Kind::Default => ' ',
        Kind::Dec => 'd',
        Kind::Hex => 'x',
        Kind::HexUpper => 'X',
        Kind::Oct => 'o',
        Kind::Bin => 'b',
        Kind::Exp => 'e',
        Kind::ExpUpper => 'E',
        Kind::Fixed => 'f',
        Kind::Percent => '%',
        Kind::Debug => '?',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_spec_table() {
        let p = |s| parse(s).unwrap();
        assert_eq!(
            p(".2f"),
            FormatSpec {
                precision: Some(2),
                kind: Kind::Fixed,
                ..Default::default()
            }
        );
        assert_eq!(
            p("08"),
            FormatSpec {
                zero: true,
                width: Some(8),
                ..Default::default()
            }
        );
        assert_eq!(
            p("#x"),
            FormatSpec {
                alternate: true,
                kind: Kind::Hex,
                ..Default::default()
            }
        );
        assert_eq!(
            p("*^6"),
            FormatSpec {
                fill: '*',
                align: Some(Align::Center),
                width: Some(6),
                ..Default::default()
            }
        );
        assert_eq!(
            p(".0%"),
            FormatSpec {
                precision: Some(0),
                kind: Kind::Percent,
                ..Default::default()
            }
        );
        assert_eq!(
            p("?"),
            FormatSpec {
                kind: Kind::Debug,
                ..Default::default()
            }
        );
        assert_eq!(
            p(">10"),
            FormatSpec {
                align: Some(Align::Right),
                width: Some(10),
                ..Default::default()
            }
        );
        assert_eq!(
            p("+"),
            FormatSpec {
                sign: Sign::Plus,
                ..Default::default()
            }
        );
        // A fill character that is itself an align character.
        assert_eq!(
            p("<<5"),
            FormatSpec {
                fill: '<',
                align: Some(Align::Left),
                width: Some(5),
                ..Default::default()
            }
        );
    }

    #[test]
    fn rejects_bad_specs() {
        assert!(parse(".2q").is_err());
        assert!(parse("5.").is_err());
        assert!(parse("x2").is_err());
        let s = |t: &str| parse(t).unwrap();
        assert!(check(&s("x"), &Ty::Str).is_err());
        assert!(check(&s(".2"), &Ty::INT).is_err());
        assert!(check(&s("+"), &Ty::Str).is_err());
        assert!(check(&s("#f"), &Ty::NUMBER).is_err());
        assert!(check(&s(".2f"), &Ty::INT).is_ok());
        assert!(check(&s(".3"), &Ty::Str).is_ok());
    }
}
