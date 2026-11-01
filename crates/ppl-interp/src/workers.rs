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
    /// A reference into a cell (a closure's captures): the cell (by index) and the path.
    Ref(usize, Vec<Step>),
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
    cells: Vec<Sv>,
}

struct Encoder<'a> {
    g: Graph,
    /// `ArrayBuffer`s (by class): their bytes are copied, or moved when transferred.
    buffer_class: Option<u32>,
    buffers: &'a Mutex<Buffers>,
    transfer: Vec<i64>,
    objs: HashMap<*const Obj, usize>,
    arrs: HashMap<*const RefCell<Vec<Value>>, usize>,
    cells: HashMap<*const RefCell<Value>, usize>,
}

impl Encoder<'_> {
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
                let mut fields: Vec<Sv> = o.fields.borrow().iter().map(|f| self.value(f)).collect();
                // An `ArrayBuffer` not transferred: the copy gets its own bytes.
                if Some(o.class) == self.buffer_class
                    && let Some(Sv::Int(h)) = fields.first()
                    && *h != 0
                    && !self.transfer.contains(&(*h as i64))
                {
                    let mut bufs = self.buffers.lock().expect("buffers");
                    let copy = bufs.mem.get(&(*h as i64)).cloned().unwrap_or_default();
                    fields[0] = Sv::Int(i128::from(bufs.add(copy)));
                }
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
            Value::Ref(c, path) => {
                let key = Rc::as_ptr(c);
                let i = match self.cells.get(&key) {
                    Some(&i) => i,
                    None => {
                        let i = self.g.cells.len();
                        self.cells.insert(key, i);
                        self.g.cells.push(Sv::Uninit);
                        let v = self.value(&c.borrow());
                        self.g.cells[i] = v;
                        i
                    }
                };
                Sv::Ref(i, path.to_vec())
            }
            Value::Void => Sv::Void,
            Value::Uninit => Sv::Uninit,
            // Not Sendable (the type checker keeps them out); weak references aren't sent.
            Value::Null | Value::Iface(_) | Value::Weak(_) => Sv::Null,
        }
    }
}

pub fn encode(
    v: &Value,
    buffer_class: Option<u32>,
    buffers: &Mutex<Buffers>,
    transfer: Vec<i64>,
) -> Graph {
    let mut e = Encoder {
        g: Graph::default(),
        buffer_class,
        buffers,
        transfer,
        objs: HashMap::new(),
        arrs: HashMap::new(),
        cells: HashMap::new(),
    };
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
    let cells: Vec<Cell> = g.cells.iter().map(|_| cell(Value::Uninit)).collect();
    fn val(v: &Sv, objs: &[Rc<Obj>], arrs: &[Rc<RefCell<Vec<Value>>>], cells: &[Cell]) -> Value {
        let val =
            |v: &Sv, objs: &[Rc<Obj>], arrs: &[Rc<RefCell<Vec<Value>>>]| val(v, objs, arrs, cells);
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
            Sv::Ref(i, path) => Value::Ref(cells[*i].clone(), path.clone().into()),
            Sv::Null => Value::Null,
            Sv::Void => Value::Void,
            Sv::Uninit => Value::Uninit,
        }
    }
    for (o, (_, fields)) in objs.iter().zip(&g.objs) {
        *o.fields.borrow_mut() = fields
            .iter()
            .map(|f| val(f, &objs, &arrs, &cells))
            .collect();
    }
    for (a, elems) in arrs.iter().zip(&g.arrs) {
        *a.borrow_mut() = elems.iter().map(|e| val(e, &objs, &arrs, &cells)).collect();
    }
    for (c, v) in cells.iter().zip(&g.cells) {
        *c.borrow_mut() = val(v, &objs, &arrs, &cells);
    }
    match &g.root {
        Some(r) => val(r, &objs, &arrs, &cells),
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
    /// Doesn't keep the parent's loop running (an idle inline worker).
    pub unref: AtomicBool,
    pub waker: Arc<Waker>,
    pub parent_waker: Arc<Waker>,
}

/// `ArrayBuffer` memory (spec §10.3), shared by every thread so a transfer moves a handle.
#[derive(Default)]
pub struct Buffers {
    pub mem: HashMap<i64, Vec<u8>>,
    next: i64,
}

impl Buffers {
    pub fn add(&mut self, bytes: Vec<u8>) -> i64 {
        self.next += 1;
        self.mem.insert(self.next, bytes);
        self.next
    }
}

/// Everything a run's threads share: their output (in order), the workers and the buffers.
#[derive(Default)]
pub struct Shared {
    pub buffers: Mutex<Buffers>,
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
            comptime: None,
            uninit_global: std::cell::Cell::new(None),
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
            unref: AtomicBool::new(false),
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
                // (An interpreter bug in a worker reaches the parent as a failure.)
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &w.entry {
                    Entry::Module(f) => it.call(*f, Vec::new(), Loc::default()),
                }))
                .unwrap_or_else(|_| {
                    let mut p = terminated_panic();
                    p.msg = "internal interpreter error in a worker".into();
                    Err(p)
                });
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
                    self.shared.worker(**h).is_some_and(|w| {
                        w.live.load(Ordering::SeqCst) && !w.unref.load(Ordering::SeqCst)
                    })
                })
                .count() as i64,
            7 => i64::from(self.this_worker.is_some()),
            8 => match self.p.inline_worker_entry {
                Some(f) => self.spawn(Entry::Module(f)),
                None => -1,
            },
            9 | 10 => {
                if let Some(w) = self.shared.worker(arg) {
                    w.unref.store(op == 9, Ordering::SeqCst);
                }
                0
            }
            11 => std::thread::available_parallelism().map_or(1, |n| n.get() as i64),
            _ => 0,
        }))
    }

    pub(crate) fn worker_post(&mut self, h: &Value, v: Value, transfer: Option<&Value>) {
        let Value::Int(h) = *h else { return };
        let transfer: Vec<i64> = match transfer {
            Some(Value::Array(a)) => a
                .borrow()
                .iter()
                .filter_map(|x| match x {
                    Value::Int(i) => Some(*i as i64),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let buffer_class = self
            .p
            .structs
            .iter()
            .position(|s| s.is_class && s.name == "ArrayBuffer")
            .map(|i| i as u32);
        let msg = IMsg::Data(encode(&v, buffer_class, &self.shared.buffers, transfer));
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

impl Interp<'_, '_> {
    /// `__buf_op` (see `ppl_buf_op` in the runtime): 1 alloc, 2 free, 3 copy a range,
    /// 11–18 read a little-endian unsigned integer of 1–8 bytes, 21–28 write one.
    pub(crate) fn buf_op(&mut self, vals: &[Value]) -> Value {
        let n = |i: usize| match vals[i] {
            Value::Int(x) => x as i64,
            _ => 0,
        };
        let (op, a, b, c) = (n(0), n(1), n(2), n(3));
        let mut bufs = self.shared.buffers.lock().expect("buffers");
        Value::Int(i128::from(match op {
            1 => bufs.add(vec![0; a.max(0) as usize]),
            2 => {
                bufs.mem.remove(&a);
                0
            }
            3 => {
                let src = bufs.mem.get(&a).cloned().unwrap_or_default();
                let (from, len) = (b.max(0) as usize, c.max(0) as usize);
                let part = src
                    .get(from..from + len)
                    .map(<[u8]>::to_vec)
                    .unwrap_or_default();
                bufs.add(part)
            }
            11..=18 => {
                let w = (op - 10) as usize;
                let mem = bufs.mem.get(&a).map(Vec::as_slice).unwrap_or(&[]);
                let mut x: u64 = 0;
                for k in (0..w).rev() {
                    x = (x << 8) | u64::from(mem.get(b as usize + k).copied().unwrap_or(0));
                }
                x as i64
            }
            21..=28 => {
                let w = (op - 20) as usize;
                if let Some(mem) = bufs.mem.get_mut(&a) {
                    for k in 0..w {
                        if let Some(byte) = mem.get_mut(b as usize + k) {
                            *byte = ((c as u64) >> (8 * k)) as u8;
                        }
                    }
                }
                0
            }
            _ => 0,
        }))
    }
}
