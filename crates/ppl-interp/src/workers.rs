//! Workers in the interpreter (spec §10.2): each runs on a thread of its own with its own
//! [`Interp`] (so its own globals: module instances), and messages are deep-copied through
//! [`Sv`], a thread-safe copy of a value graph (sharing and cycles kept). Mirrors
//! `runtime/src/ppl_worker.c`.

use super::*;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

/// A value detached from any thread: objects and arrays by index into the message's tables.
#[derive(Clone, Debug)]
pub enum Sv {
    Int(i128),
    Float(f64),
    Bool(bool),
    Char(char),
    Str(String),
    Struct(Vec<Sv>),
    Enum(u32, Vec<Sv>),
    Obj(usize),
    Arr(usize),
    Closure(FnId, Box<Sv>),
    Null,
    Void,
    Uninit,
}

/// A message: the value, and the objects and arrays it reaches.
#[derive(Debug, Default)]
pub struct Graph {
    pub root: Option<Sv>,
    objs: Vec<(u32, Vec<Sv>)>,
    arrs: Vec<Vec<Sv>>,
}

#[derive(Default)]
struct Encoder {
    g: Graph,
    objs: HashMap<*const Obj, usize>,
    arrs: HashMap<*const RefCell<Vec<Value>>, usize>,
}

impl Encoder {
    fn value(&mut self, v: &Value) -> Sv {
        match v {
            Value::Int(x) => Sv::Int(*x),
            Value::Float(x) => Sv::Float(*x),
            Value::Bool(b) => Sv::Bool(*b),
            Value::Char(c) => Sv::Char(*c),
            Value::Str(s) => Sv::Str(s.to_string()),
            Value::Struct(ps) => Sv::Struct(ps.iter().map(|p| self.value(p)).collect()),
            Value::Enum(i, ps) => Sv::Enum(*i, ps.iter().map(|p| self.value(p)).collect()),
            Value::Object(o) => {
                let key = Rc::as_ptr(o);
                if let Some(&i) = self.objs.get(&key) {
                    return Sv::Obj(i);
                }
                let i = self.g.objs.len();
                self.objs.insert(key, i);
                self.g.objs.push((o.class, Vec::new()));
                let fields: Vec<Sv> = o.fields.borrow().iter().map(|f| self.value(f)).collect();
                self.g.objs[i].1 = fields;
                Sv::Obj(i)
            }
            Value::Array(a) => {
                let key = Rc::as_ptr(a);
                if let Some(&i) = self.arrs.get(&key) {
                    return Sv::Arr(i);
                }
                let i = self.g.arrs.len();
                self.arrs.insert(key, i);
                self.g.arrs.push(Vec::new());
                let elems: Vec<Sv> = a.borrow().iter().map(|e| self.value(e)).collect();
                self.g.arrs[i] = elems;
                Sv::Arr(i)
            }
            // (A capture-free function and what its environment holds: inline workers.)
            Value::Closure(c) => Sv::Closure(c.0, Box::new(self.value(&c.1))),
            Value::Void => Sv::Void,
            Value::Uninit => Sv::Uninit,
            // Not Sendable (the type checker keeps them out); weak references aren't sent.
            Value::Null | Value::Iface(_) | Value::Ref(..) | Value::Weak(_) => Sv::Null,
        }
    }
}

pub fn encode(v: &Value) -> Graph {
    let mut e = Encoder::default();
    let root = e.value(v);
    e.g.root = Some(root);
    e.g
}

/// The message as values of this thread.
pub fn decode(g: &Graph) -> Value {
    let objs: Vec<Rc<Obj>> = g
        .objs
        .iter()
        .map(|(class, _)| {
            Rc::new(Obj {
                class: *class,
                fields: RefCell::new(Vec::new()),
            })
        })
        .collect();
    let arrs: Vec<Rc<RefCell<Vec<Value>>>> = g
        .arrs
        .iter()
        .map(|_| Rc::new(RefCell::new(Vec::new())))
        .collect();
    fn val(v: &Sv, objs: &[Rc<Obj>], arrs: &[Rc<RefCell<Vec<Value>>>]) -> Value {
        match v {
            Sv::Int(x) => Value::Int(*x),
            Sv::Float(x) => Value::Float(*x),
            Sv::Bool(b) => Value::Bool(*b),
            Sv::Char(c) => Value::Char(*c),
            Sv::Str(s) => Value::Str(s.as_str().into()),
            Sv::Struct(ps) => Value::Struct(ps.iter().map(|p| val(p, objs, arrs)).collect()),
            Sv::Enum(i, ps) => Value::Enum(*i, ps.iter().map(|p| val(p, objs, arrs)).collect()),
            Sv::Obj(i) => Value::Object(objs[*i].clone()),
            Sv::Arr(i) => Value::Array(arrs[*i].clone()),
            Sv::Closure(f, env) => Value::Closure(Rc::new((*f, val(env, objs, arrs)))),
            Sv::Null => Value::Null,
            Sv::Void => Value::Void,
            Sv::Uninit => Value::Uninit,
        }
    }
    for (o, (_, fields)) in objs.iter().zip(&g.objs) {
        *o.fields.borrow_mut() = fields.iter().map(|f| val(f, &objs, &arrs)).collect();
    }
    for (a, elems) in arrs.iter().zip(&g.arrs) {
        *a.borrow_mut() = elems.iter().map(|e| val(e, &objs, &arrs)).collect();
    }
    match &g.root {
        Some(r) => val(r, &objs, &arrs),
        None => Value::Uninit,
    }
}

/// Wakes a thread waiting in its event loop.
#[derive(Default)]
pub struct Waker {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Waker {
    pub fn wake(&self) {
        *self.flag.lock().expect("waker") = true;
        self.cv.notify_all();
    }

    /// Waits up to `ms` milliseconds (`< 0`: until woken).
    pub fn wait(&self, ms: i64) {
        if ms == 0 {
            return;
        }
        let mut woken = self.flag.lock().expect("waker");
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(ms.max(0) as u64);
        while !*woken {
            if ms < 0 {
                woken = self.cv.wait(woken).expect("waker");
            } else {
                let now = std::time::Instant::now();
                if now >= deadline {
                    break;
                }
                woken = self
                    .cv
                    .wait_timeout(woken, deadline - now)
                    .expect("waker")
                    .0;
            }
        }
        *woken = false;
    }
}

pub enum IMsg {
    Data(Graph),
    Error(String),
    Exit,
}

// (The graphs only hold `Sv`s: plain data.)
unsafe impl Send for IMsg {}

/// What a worker thread runs.
#[derive(Clone)]
pub enum Entry {
    /// A worker module's entry function.
    Module(FnId),
}

pub struct IWorker {
    pub entry: Entry,
    pub to_worker: Mutex<VecDeque<IMsg>>,
    pub to_parent: Mutex<VecDeque<IMsg>>,
    pub terminate: AtomicBool,
    /// Not exited or terminated (as far as the parent knows).
    pub live: AtomicBool,
    pub waker: Arc<Waker>,
    pub parent_waker: Arc<Waker>,
}

/// Everything a run's threads share: their output (in order) and the workers.
#[derive(Default)]
pub struct Shared {
    pub out: Mutex<Vec<u8>>,
    pub workers: Mutex<Vec<Arc<IWorker>>>,
    /// The program, for worker threads (copied when the first one starts).
    pub program: Mutex<Option<Arc<Program>>>,
}

impl Shared {
    /// The program is over: every worker stops.
    pub fn stop_all(&self) {
        for w in self.workers.lock().expect("workers").iter() {
            w.terminate.store(true, Ordering::SeqCst);
            w.waker.wake();
        }
    }

    fn worker(&self, h: i64) -> Option<Arc<IWorker>> {
        let ws = self.workers.lock().expect("workers");
        usize::try_from(h - 1).ok().and_then(|i| ws.get(i).cloned())
    }
}

/// How a terminated worker's code ends (not reported to anyone).
pub fn terminated_panic() -> Panic {
    Panic {
        msg: "\u{0}terminated".into(),
        file: String::new(),
        loc: Loc::default(),
        uncaught: false,
        trace: Vec::new(),
    }
}

impl<'p, 'o> Interp<'p, 'o> {
    pub(crate) fn new(
        program: &'p Program,
        out: Option<&'o mut dyn Write>,
        shared: Arc<Shared>,
        this_worker: Option<Arc<IWorker>>,
    ) -> Self {
        let waker = this_worker
            .as_ref()
            .map_or_else(|| Arc::new(Waker::default()), |w| w.waker.clone());
        Interp {
            p: program,
            out,
            shared,
            this_worker,
            children: Vec::new(),
            waker,
            globals: (0..program.globals.len())
                .map(|_| cell(Value::Uninit))
                .collect(),
            depth: 0,
            exception: None,
            exception_tag: 0,
            stack: Vec::new(),
            frames: Vec::new(),
            unwinding: Vec::new(),
            started: std::time::Instant::now(),
        }
    }

    /// Program output: in order across threads; the main thread writes it out.
    pub(crate) fn emit(&mut self, text: &str) {
        self.shared
            .out
            .lock()
            .expect("output")
            .extend_from_slice(text.as_bytes());
        self.flush_output();
    }

    pub(crate) fn flush_output(&mut self) {
        if let Some(out) = self.out.as_mut() {
            let pending = std::mem::take(&mut *self.shared.out.lock().expect("output"));
            let _ = out.write_all(&pending);
            let _ = out.flush();
        }
    }

    pub(crate) fn terminated(&self) -> bool {
        self.this_worker
            .as_ref()
            .is_some_and(|w| w.terminate.load(Ordering::SeqCst))
    }

    /// The channel messages from `h` arrive on (0: the parent).
    fn inbox(&self, h: i64) -> Option<(Arc<IWorker>, bool)> {
        if h == 0 {
            return self.this_worker.clone().map(|w| (w, true));
        }
        let w = self.shared.worker(h)?;
        w.live.load(Ordering::SeqCst).then_some((w, false))
    }

    fn with_inbox<R>(&self, h: i64, f: impl FnOnce(&mut VecDeque<IMsg>) -> R) -> Option<R> {
        let (w, from_parent) = self.inbox(h)?;
        let mut q = if from_parent {
            w.to_worker.lock().expect("queue")
        } else {
            w.to_parent.lock().expect("queue")
        };
        Some(f(&mut q))
    }

    fn spawn(&mut self, entry: Entry) -> i64 {
        let w = Arc::new(IWorker {
            entry,
            to_worker: Mutex::new(VecDeque::new()),
            to_parent: Mutex::new(VecDeque::new()),
            terminate: AtomicBool::new(false),
            live: AtomicBool::new(true),
            waker: Arc::new(Waker::default()),
            parent_waker: self.waker.clone(),
        });
        let h = {
            let mut ws = self.shared.workers.lock().expect("workers");
            ws.push(w.clone());
            ws.len() as i64
        };
        self.children.push(h);
        let program = self
            .shared
            .program
            .lock()
            .expect("program")
            .get_or_insert_with(|| Arc::new(self.p.clone()))
            .clone();
        let shared = self.shared.clone();
        let spawned = std::thread::Builder::new()
            .name("ppl-worker".into())
            .stack_size(1 << 30)
            .spawn(move || {
                let program: &Program = &program;
                let mut it = Interp::new(program, None, shared, Some(w.clone()));
                let r = match &w.entry {
                    Entry::Module(f) => it.call(*f, Vec::new(), Loc::default()),
                };
                let msg = match r {
                    Err(p) if p.msg.starts_with('\u{0}') => None,
                    Err(p) if p.uncaught => Some(IMsg::Error(format!("uncaught {}", p.msg))),
                    Err(p) => Some(IMsg::Error(p.msg)),
                    Ok(_) => Some(IMsg::Exit),
                };
                if let Some(m) = msg {
                    w.to_parent.lock().expect("queue").push_back(m);
                    w.parent_waker.wake();
                }
            });
        spawned.expect("spawn worker thread");
        h
    }

    pub(crate) fn worker_op(&mut self, vals: &[Value]) -> Value {
        let (Value::Int(op), Value::Int(arg)) = (&vals[0], &vals[1]) else {
            unreachable!("int operands")
        };
        let (op, arg) = (*op as i64, *arg as i64);
        Value::Int(i128::from(match op {
            1 => self.spawn(Entry::Module(self.p.worker_entries[(arg - 1) as usize])),
            2 => {
                let ready =
                    |it: &Self, h: i64| it.with_inbox(h, |q| !q.is_empty()).unwrap_or(false);
                if ready(self, 0) {
                    0
                } else {
                    self.children
                        .iter()
                        .copied()
                        .find(|h| ready(self, *h))
                        .unwrap_or(-1)
                }
            }
            3 => self
                .with_inbox(arg, |q| match q.front() {
                    Some(IMsg::Data(_)) => 0,
                    Some(IMsg::Error(_)) => 1,
                    Some(IMsg::Exit) => 2,
                    None => -1,
                })
                .unwrap_or(-1),
            4 => match self.shared.worker(arg) {
                Some(w) if w.live.swap(false, Ordering::SeqCst) => {
                    w.terminate.store(true, Ordering::SeqCst);
                    w.waker.wake();
                    1
                }
                _ => 0,
            },
            5 => {
                let gone = self
                    .with_inbox(arg, |q| {
                        q.pop_front().is_some_and(|m| !matches!(m, IMsg::Data(_)))
                    })
                    .unwrap_or(false);
                if gone
                    && arg != 0
                    && let Some(w) = self.shared.worker(arg)
                {
                    w.live.store(false, Ordering::SeqCst);
                }
                0
            }
            6 => self
                .children
                .iter()
                .filter(|h| {
                    self.shared
                        .worker(**h)
                        .is_some_and(|w| w.live.load(Ordering::SeqCst))
                })
                .count() as i64,
            7 => i64::from(self.this_worker.is_some()),
            _ => 0,
        }))
    }

    pub(crate) fn worker_post(&mut self, h: &Value, v: Value, _t: &Ty) {
        let Value::Int(h) = *h else { return };
        let msg = IMsg::Data(encode(&v));
        if h == 0 {
            if let Some(w) = &self.this_worker {
                w.to_parent.lock().expect("queue").push_back(msg);
                w.parent_waker.wake();
            }
        } else if let Some(w) = self.shared.worker(h as i64)
            && w.live.load(Ordering::SeqCst)
        {
            w.to_worker.lock().expect("queue").push_back(msg);
            w.waker.wake();
        }
    }

    pub(crate) fn worker_take(&mut self, h: &Value, _t: &Ty) -> Value {
        let Value::Int(h) = *h else {
            return Value::Uninit;
        };
        match self.with_inbox(h as i64, |q| q.pop_front()).flatten() {
            Some(IMsg::Data(g)) => decode(&g),
            _ => Value::Uninit,
        }
    }

    pub(crate) fn worker_error(&mut self, h: &Value) -> Value {
        let Value::Int(h) = *h else {
            return Value::Str("".into());
        };
        let text = match self.with_inbox(h as i64, |q| q.pop_front()).flatten() {
            Some(IMsg::Error(t)) => t,
            _ => String::new(),
        };
        if let Some(w) = self.shared.worker(h as i64) {
            w.live.store(false, Ordering::SeqCst);
        }
        Value::Str(text.as_str().into())
    }
}
