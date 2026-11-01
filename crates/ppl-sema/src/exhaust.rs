//! Exhaustiveness checking for `match` (spec §3.5), after Maranget's "Warnings for pattern
//! matching": a pattern vector is *useful* with respect to a matrix of earlier rows if some
//! value matches it and no earlier row. A match is exhaustive when the wildcard isn't useful
//! after all its (unguarded) arms; each witness the algorithm returns is a missing case.

use crate::hir::Program;
use crate::ty::Ty;

/// A constructor: what the head of a value can be.
#[derive(Clone, Debug, PartialEq)]
pub enum Ctor {
    /// An enum variant, by index.
    Variant(u32),
    Bool(bool),
    /// The only constructor of a struct or tuple.
    Single,
    Null,
    /// A non-null `T?`, with the `T` as its one argument.
    NonNull,
    /// Literals of types with infinitely many values (never complete).
    Int(i128),
    Float(u64),
    Str(String),
}

/// A pattern reduced to constructors and wildcards.
#[derive(Clone, Debug)]
pub enum Pat {
    Wild,
    Ctor(Ctor, Vec<Pat>),
}

pub struct Checker<'p> {
    pub program: &'p Program,
}

impl Checker<'_> {
    /// The argument types of `c` applied to a value of type `t`.
    fn arg_tys(&self, t: &Ty, c: &Ctor) -> Vec<Ty> {
        match (t, c) {
            (Ty::Enum(id, _), Ctor::Variant(v)) => self.program.enum_def(*id).variants[*v as usize]
                .fields
                .iter()
                .map(|f| f.ty.clone())
                .collect(),
            (Ty::Struct(id, _), Ctor::Single) => self
                .program
                .struct_def(*id)
                .fields
                .iter()
                .map(|f| f.ty.clone())
                .collect(),
            (Ty::Tuple(ts), Ctor::Single) => ts.clone(),
            (Ty::Nullable(inner), Ctor::NonNull) => vec![(**inner).clone()],
            _ => Vec::new(),
        }
    }

    /// Every constructor of `t`, or `None` if there are infinitely many.
    fn all_ctors(&self, t: &Ty) -> Option<Vec<Ctor>> {
        Some(match t {
            Ty::Enum(id, _) => (0..self.program.enum_def(*id).variants.len())
                .map(|i| Ctor::Variant(i as u32))
                .collect(),
            Ty::Bool => vec![Ctor::Bool(false), Ctor::Bool(true)],
            Ty::Nullable(_) => vec![Ctor::Null, Ctor::NonNull],
            Ty::Struct(..) | Ty::Tuple(_) => vec![Ctor::Single],
            _ => return None,
        })
    }

    /// Rows whose head matches `c`, with the head replaced by its arguments.
    fn specialize(&self, rows: &[Vec<Pat>], c: &Ctor, arity: usize) -> Vec<Vec<Pat>> {
        rows.iter()
            .filter_map(|r| match &r[0] {
                Pat::Wild => {
                    let mut out = vec![Pat::Wild; arity];
                    out.extend_from_slice(&r[1..]);
                    Some(out)
                }
                Pat::Ctor(rc, args) if rc == c => {
                    let mut out = args.clone();
                    out.extend_from_slice(&r[1..]);
                    Some(out)
                }
                Pat::Ctor(..) => None,
            })
            .collect()
    }

    /// A value matching `q` but no row of `rows`, if there is one (as a pattern vector).
    pub fn useful(&self, rows: &[Vec<Pat>], q: &[Pat], tys: &[Ty]) -> Option<Vec<Pat>> {
        if q.is_empty() {
            return rows.is_empty().then(Vec::new);
        }
        let t = &tys[0];
        let rebuild = |c: Ctor, arity: usize, mut w: Vec<Pat>| {
            let rest = w.split_off(arity);
            let mut out = vec![Pat::Ctor(c, w)];
            out.extend(rest);
            out
        };
        let sub_tys = |c: &Ctor| {
            let mut ts = self.arg_tys(t, c);
            ts.extend_from_slice(&tys[1..]);
            ts
        };
        match &q[0] {
            Pat::Ctor(c, args) => {
                let arity = args.len();
                let rows = self.specialize(rows, c, arity);
                let mut q2 = args.clone();
                q2.extend_from_slice(&q[1..]);
                self.useful(&rows, &q2, &sub_tys(c))
                    .map(|w| rebuild(c.clone(), arity, w))
            }
            Pat::Wild => {
                let used: Vec<&Ctor> = rows
                    .iter()
                    .filter_map(|r| match &r[0] {
                        Pat::Ctor(c, _) => Some(c),
                        Pat::Wild => None,
                    })
                    .collect();
                let all = self.all_ctors(t);
                if let Some(all) = &all
                    && all.iter().all(|c| used.contains(&c))
                {
                    // Every constructor appears: try each.
                    for c in all {
                        let arity = self.arg_tys(t, c).len();
                        let rows = self.specialize(rows, c, arity);
                        let mut q2 = vec![Pat::Wild; arity];
                        q2.extend_from_slice(&q[1..]);
                        if let Some(w) = self.useful(&rows, &q2, &sub_tys(c)) {
                            return Some(rebuild(c.clone(), arity, w));
                        }
                    }
                    return None;
                }
                // Some constructor is missing: only the wildcard rows can match it.
                let default: Vec<Vec<Pat>> = rows
                    .iter()
                    .filter(|r| matches!(r[0], Pat::Wild))
                    .map(|r| r[1..].to_vec())
                    .collect();
                let w = self.useful(&default, &q[1..], &tys[1..])?;
                let head = match all {
                    Some(all) if !used.is_empty() => {
                        let missing = all
                            .into_iter()
                            .find(|c| !used.contains(&c))
                            .expect("a constructor is missing");
                        let arity = self.arg_tys(t, &missing).len();
                        Pat::Ctor(missing, vec![Pat::Wild; arity])
                    }
                    _ => Pat::Wild,
                };
                let mut out = vec![head];
                out.extend(w);
                Some(out)
            }
        }
    }

    /// The missing cases of a match on `t` with these arm patterns (at most `limit`).
    pub fn missing(&self, t: &Ty, arms: &[Pat], limit: usize) -> Vec<String> {
        let mut rows: Vec<Vec<Pat>> = arms.iter().map(|p| vec![p.clone()]).collect();
        let mut out = Vec::new();
        while out.len() < limit {
            let Some(w) = self.useful(&rows, &[Pat::Wild], std::slice::from_ref(t)) else {
                break;
            };
            out.push(self.show(t, &w[0]));
            rows.push(w);
        }
        out
    }

    /// A witness as source text: `Circle(_)`, `Empty`, `false`, `Point { .. }`, `_`.
    pub fn show(&self, t: &Ty, p: &Pat) -> String {
        match (p, t) {
            (Pat::Wild, _) => "_".into(),
            (Pat::Ctor(Ctor::Variant(v), args), Ty::Enum(id, _)) => {
                let def = &self.program.enum_def(*id).variants[*v as usize];
                if def.fields.is_empty() && !def.has_parens {
                    return def.name.clone();
                }
                let parts: Vec<String> = args
                    .iter()
                    .zip(&def.fields)
                    .map(|(a, f)| self.show(&f.ty, a))
                    .collect();
                format!("{}({})", def.name, parts.join(", "))
            }
            (Pat::Ctor(Ctor::Bool(b), _), _) => b.to_string(),
            (Pat::Ctor(Ctor::Null, _), _) => "null".into(),
            (Pat::Ctor(Ctor::NonNull, args), Ty::Nullable(inner)) => self.show(inner, &args[0]),
            (Pat::Ctor(Ctor::Single, args), Ty::Tuple(ts)) => {
                let parts: Vec<String> =
                    args.iter().zip(ts).map(|(a, t)| self.show(t, a)).collect();
                format!("[{}]", parts.join(", "))
            }
            (Pat::Ctor(Ctor::Single, args), Ty::Struct(id, name)) => {
                let def = self.program.struct_def(*id);
                let parts: Vec<String> = args
                    .iter()
                    .zip(&def.fields)
                    .filter(|(a, _)| !matches!(a, Pat::Wild))
                    .map(|(a, f)| format!("{}: {}", f.name, self.show(&f.ty, a)))
                    .collect();
                if parts.is_empty() {
                    format!("{name} {{ .. }}")
                } else {
                    format!("{name} {{ {}, .. }}", parts.join(", "))
                }
            }
            (Pat::Ctor(Ctor::Int(v), _), _) => v.to_string(),
            (Pat::Ctor(Ctor::Float(b), _), _) => f64::from_bits(*b).to_string(),
            (Pat::Ctor(Ctor::Str(s), _), _) => format!("{s:?}"),
            _ => "_".into(),
        }
    }
}
