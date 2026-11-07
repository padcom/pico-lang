//! Constant folding (spec §11.1): pure arithmetic over constants is evaluated at compile time,
//! and constants flow through locals and globals that are only ever assigned that constant
//! (`const KB = 1 << 10` makes `KB` the constant `1024` everywhere).
//!
//! An operation that would panic at run time (overflow, division by zero, a bad shift or
//! conversion) is a compile error instead, also in code that never runs (decision 219).

use crate::eval::{self, Prim, Scalar};
use crate::mir::{
    Base, BinOp, BlockId, Const, Function, Local, Operand, Place, Program, Rvalue, Statement,
    Terminator,
};
use pico_diag::{Diagnostic, Span, codes};
use std::collections::{HashMap, HashSet};

/// Folds the whole program in place; reports operations that always panic.
pub fn fold_program(p: &mut Program) -> Vec<Diagnostic> {
    let mut cx = Folder {
        globals: HashMap::new(),
        diags: Vec::new(),
        reported: HashSet::new(),
    };
    // Folding a function can make a global constant, which other functions then use.
    loop {
        for f in &mut p.fns {
            cx.function(f);
        }
        let globals = constant_globals(p);
        if globals.len() == cx.globals.len() {
            break;
        }
        cx.globals = globals;
    }
    for f in &mut p.fns {
        remove_dead_constants(f);
    }
    cx.diags
}

struct Folder {
    /// Globals whose every write stores this constant.
    globals: HashMap<u32, Const>,
    diags: Vec<Diagnostic>,
    reported: HashSet<Span>,
}

impl Folder {
    fn function(&mut self, f: &mut Function) {
        let mut single = single_def_locals(f);
        let mut known: HashMap<Local, Const> = HashMap::new();
        // Blocks aren't in execution order (loops), so iterate until nothing new is known.
        loop {
            let before = known.len();
            for b in &mut f.blocks {
                for st in &mut b.stmts {
                    self.statement(st, &known);
                    if let (Base::Local(l), true, Rvalue::Use(Operand::Const(c))) =
                        (st.place.base, st.place.proj.is_empty(), &st.rvalue)
                        && single.contains(&l)
                        && (scalar(c).is_some() || matches!(c, Const::Str(_)))
                    {
                        known.insert(l, c.clone());
                    }
                }
                b.term.for_each_operand(&mut |o| self.substitute(o, &known));
                if let Terminator::Branch {
                    cond: Operand::Const(Const::Bool(c)),
                    then,
                    else_,
                } = b.term
                {
                    b.term = Terminator::Goto(if c { then } else { else_ });
                }
            }
            // Folded branches can leave code unreachable, and its assignments then no longer
            // count (a short-circuit `&&` of constants).
            if clear_unreachable(f) {
                single = single_def_locals(f);
            } else if known.len() == before {
                break;
            }
        }
    }

    fn substitute(&self, o: &mut Operand, known: &HashMap<Local, Const>) {
        let Operand::Copy(place) = o else { return };
        if !place.proj.is_empty() {
            return;
        }
        let c = match place.base {
            Base::Local(l) => known.get(&l),
            Base::Global(g) => self.globals.get(&g.0),
            Base::Deref(_) => None,
        };
        if let Some(c) = c {
            *o = Operand::Const(c.clone());
        }
    }

    fn statement(&mut self, st: &mut Statement, known: &HashMap<Local, Const>) {
        st.rvalue
            .for_each_operand(&mut |o| self.substitute(o, known));
        // Strings (#55): `"a" + "b"`, and the string forms of integers, `bool`s and `char`s (as
        // `println` shows them; floats are left to the runtime's formatting).
        let text = match &st.rvalue {
            Rvalue::Binary(
                BinOp::Add,
                Operand::Const(Const::Str(a)),
                Operand::Const(Const::Str(b)),
            ) => Some(format!("{a}{b}")),
            Rvalue::ToStr(Operand::Const(c)) => match c {
                Const::Int(v, _) => Some(v.to_string()),
                Const::Bool(b) => Some(b.to_string()),
                Const::Char(c) => Some(c.to_string()),
                Const::Str(s) => Some(s.clone()),
                _ => None,
            },
            _ => None,
        };
        if let Some(t) = text {
            st.rvalue = Rvalue::Use(Operand::Const(Const::Str(t)));
            return;
        }
        let folded = match &st.rvalue {
            Rvalue::Binary(op, Operand::Const(a), Operand::Const(b)) => {
                match (scalar(a), scalar(b), Prim::of(&a.ty())) {
                    (Some(x), Some(y), Some(t)) => Some(eval::binary(*op, x, y, t)),
                    _ => None,
                }
            }
            Rvalue::Unary(op, Operand::Const(a)) => match (scalar(a), Prim::of(&a.ty())) {
                (Some(x), Some(t)) => Some(eval::unary(*op, x, t)),
                _ => None,
            },
            Rvalue::Cast(Operand::Const(a), to) => match (scalar(a), Prim::of(&to.erase())) {
                (Some(x), Some(t)) => eval::cast(x, t),
                _ => None,
            },
            _ => None,
        };
        match folded {
            Some(Ok(v)) => {
                if let Some(c) = to_const(v, &result_ty(&st.rvalue)) {
                    st.rvalue = Rvalue::Use(Operand::Const(c));
                }
            }
            Some(Err(msg)) => {
                if let Some(span) = st.loc.span
                    && self.reported.insert(span)
                {
                    self.diags.push(
                        Diagnostic::error(
                            codes::CONST_PANIC,
                            format!("this constant expression always panics: {msg}"),
                        )
                        .primary(span, "evaluated at compile time"),
                    );
                }
            }
            None => {}
        }
    }
}

/// The type of a foldable rvalue's result.
fn result_ty(rv: &Rvalue) -> pico_sema::Ty {
    match rv {
        Rvalue::Binary(op, ..) if op.is_comparison() => pico_sema::Ty::Bool,
        Rvalue::Binary(_, Operand::Const(a), _) | Rvalue::Unary(_, Operand::Const(a)) => a.ty(),
        Rvalue::Cast(_, t) => t.erase(),
        _ => unreachable!("not a folded rvalue"),
    }
}

fn scalar(c: &Const) -> Option<Scalar> {
    Some(match c {
        Const::Int(v, _) => Scalar::Int(*v),
        Const::Float(v, _) => Scalar::Float(*v),
        Const::Bool(b) => Scalar::Bool(*b),
        Const::Char(c) => Scalar::Char(*c),
        _ => return None,
    })
}

fn to_const(v: Scalar, ty: &pico_sema::Ty) -> Option<Const> {
    Some(match (v, Prim::of(ty)?) {
        (Scalar::Int(x), Prim::Int(it)) => Const::Int(x, it),
        (Scalar::Float(x), Prim::Float(ft)) => Const::Float(x, ft),
        (Scalar::Bool(b), Prim::Bool) => Const::Bool(b),
        (Scalar::Char(c), Prim::Char) => Const::Char(c),
        _ => return None,
    })
}

/// Locals (not parameters) assigned exactly once, as a whole, whose address is never taken.
fn single_def_locals(f: &Function) -> HashSet<Local> {
    let mut defs: HashMap<Local, u32> = HashMap::new();
    let mut blocked: HashSet<Local> = f.params.iter().copied().collect();
    let block_writes = |p: &Place, blocked: &mut HashSet<Local>| {
        if let Base::Local(l) = p.base {
            blocked.insert(l);
        }
    };
    for b in &f.blocks {
        for st in &b.stmts {
            match st.place.base {
                Base::Local(l) if st.place.proj.is_empty() => *defs.entry(l).or_default() += 1,
                _ => block_writes(&st.place, &mut blocked),
            }
            if let Some(p) = st.rvalue.referenced() {
                block_writes(p, &mut blocked);
            }
        }
        if let Terminator::Call {
            dest: Some(dest), ..
        } = &b.term
        {
            block_writes(dest, &mut blocked);
        }
    }
    defs.into_iter()
        .filter(|(l, n)| *n == 1 && !blocked.contains(l))
        .map(|(l, _)| l)
        .collect()
}

/// Globals whose every write (in any function) stores the same scalar constant.
fn constant_globals(p: &Program) -> HashMap<u32, Const> {
    let mut value: HashMap<u32, Option<Const>> = HashMap::new();
    let spoil = |g: u32, value: &mut HashMap<u32, Option<Const>>| {
        value.insert(g, None);
    };
    for f in &p.fns {
        for b in &f.blocks {
            for st in &b.stmts {
                if let Base::Global(g) = st.place.base {
                    match (&st.rvalue, st.place.proj.is_empty()) {
                        (Rvalue::Use(Operand::Const(c)), true)
                            if scalar(c).is_some() || matches!(c, Const::Str(_)) =>
                        {
                            let e = value.entry(g.0).or_insert_with(|| Some(c.clone()));
                            if e.as_ref() != Some(c) {
                                *e = None;
                            }
                        }
                        _ => spoil(g.0, &mut value),
                    }
                }
                if let Some(Place {
                    base: Base::Global(g),
                    ..
                }) = st.rvalue.referenced()
                {
                    spoil(g.0, &mut value);
                }
            }
            if let Terminator::Call {
                dest:
                    Some(Place {
                        base: Base::Global(g),
                        ..
                    }),
                ..
            } = &b.term
            {
                spoil(g.0, &mut value);
            }
        }
    }
    value
        .into_iter()
        .filter_map(|(g, c)| Some((g, c?)))
        .collect()
}

/// Drops `_l = const …` when nothing reads `_l` any more.
fn remove_dead_constants(f: &mut Function) {
    let mut used: HashSet<Local> = f.params.iter().copied().collect();
    let note = |p: &Place, used: &mut HashSet<Local>| used.extend(p.locals());
    for b in &mut f.blocks {
        for st in &mut b.stmts {
            // The destination counts unless it's a whole local.
            if !(matches!(st.place.base, Base::Local(_)) && st.place.proj.is_empty()) {
                note(&st.place, &mut used);
            }
            if let Some(p) = st.rvalue.referenced() {
                note(p, &mut used);
            }
            st.rvalue.for_each_operand(&mut |o| {
                if let Operand::Copy(p) = o {
                    used.extend(p.locals());
                }
            });
        }
        if let Terminator::Call { dest: Some(d), .. } = &b.term {
            note(d, &mut used);
        }
        b.term.for_each_operand(&mut |o| {
            if let Operand::Copy(p) = o {
                used.extend(p.locals());
            }
        });
    }
    for b in &mut f.blocks {
        b.stmts.retain(|st| {
            !matches!(
                (&st.place.base, st.place.proj.is_empty(), &st.rvalue),
                (Base::Local(l), true, Rvalue::Use(Operand::Const(_))) if !used.contains(l)
            )
        });
    }
}

/// Empties the blocks no path from the entry reaches; whether any were emptied.
fn clear_unreachable(f: &mut Function) -> bool {
    let mut seen = vec![false; f.blocks.len()];
    let mut work = vec![0usize];
    while let Some(b) = work.pop() {
        if std::mem::replace(&mut seen[b], true) {
            continue;
        }
        let next: Vec<BlockId> = match &f.blocks[b].term {
            Terminator::Goto(t) | Terminator::Unguard(t) => vec![*t],
            Terminator::Branch { then, else_, .. } => vec![*then, *else_],
            Terminator::Call { next, .. } => vec![*next],
            Terminator::Guard { body, handler } => vec![*body, *handler],
            Terminator::Return(_)
            | Terminator::TailCall { .. }
            | Terminator::Panic { .. }
            | Terminator::Resume
            | Terminator::Uncaught { .. }
            | Terminator::Unreachable => vec![],
        };
        work.extend(next.into_iter().map(|t| t.0 as usize));
    }
    let mut changed = false;
    for (b, reached) in f.blocks.iter_mut().zip(seen) {
        if !reached && !(b.stmts.is_empty() && matches!(b.term, Terminator::Unreachable)) {
            b.stmts.clear();
            b.term = Terminator::Unreachable;
            changed = true;
        }
    }
    changed
}
