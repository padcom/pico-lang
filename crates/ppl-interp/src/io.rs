//! File-system jobs for `std/fs` (`__io`, `__io_text`): the operations of the C runtime's
//! `ppl_io.c` with `std::fs`, same results and error kinds. A job runs when it's submitted;
//! its result waits in a queue until the event loop polls it, so promises still settle on a
//! later turn of the loop.
//!
//! `std/process` uses the same channel: reading and writing pipes and waiting for children
//! block, so those jobs run on threads of their own and their results wake the loop.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use crate::{Interp, Value};

const K_NOT_FOUND: i64 = 1;
const K_ALREADY_EXISTS: i64 = 2;
const K_PERMISSION_DENIED: i64 = 3;
const K_NOT_A_DIRECTORY: i64 = 4;
const K_IS_A_DIRECTORY: i64 = 5;
const K_DIRECTORY_NOT_EMPTY: i64 = 6;
const K_INVALID_DATA: i64 = 7;
const K_OTHER: i64 = 8;
const K_CONN_REFUSED: i64 = 9;
const K_CONN_RESET: i64 = 10;
const K_ADDR_IN_USE: i64 = 11;
const K_ADDR_NOT_AVAIL: i64 = 12;
const K_TIMED_OUT: i64 = 13;
const K_HOST_NOT_FOUND: i64 = 14;

#[derive(Default)]
pub(crate) struct IoState {
    next_id: i64,
    done: VecDeque<Job>,
    taken: Vec<Job>,
    files: HashMap<i64, fs::File>,
    next_fd: i64,
    watchers: HashMap<i64, Watcher>,
    next_watcher: i64,
    /// Jobs finished by threads of their own (pipes, children), moved to `done` when polled.
    finished: Arc<Mutex<VecDeque<Job>>>,
    /// Children by pid, until they're waited for.
    children: HashMap<i64, Arc<Mutex<Child>>>,
    /// Pipe ends by id (`0` is this process's stdin; children's from 1000).
    pipes: HashMap<i64, Pipe>,
    next_pipe: i64,
    /// The program's environment, once it's looked at (changes stay inside the program,
    /// as the interpreter shares its process with other programs).
    env: Option<BTreeMap<String, String>>,
    /// `std/net` sockets, shared with the threads that accept and connect.
    net: Arc<Mutex<Net>>,
}

/// Sockets by id (from 1 000 000, apart from pipes).
#[derive(Default)]
struct Net {
    next: i64,
    socks: HashMap<i64, Sock>,
}

impl Net {
    fn add(&mut self, s: Sock) -> i64 {
        self.next = self.next.max(999_999) + 1;
        self.socks.insert(self.next, s);
        self.next
    }
}

#[derive(Clone)]
enum Sock {
    Listener(Arc<TcpListener>),
    Stream(Arc<TcpStream>),
    Udp(Arc<UdpSocket>),
}

/// "host port".
fn addr_text(a: &SocketAddr) -> String {
    format!("{} {}", a.ip(), a.port())
}

/// "local-host local-port remote-host remote-port".
fn ends_text(s: &TcpStream) -> String {
    let none = SocketAddr::from(([0, 0, 0, 0], 0));
    format!(
        "{} {}",
        addr_text(&s.local_addr().unwrap_or(none)),
        addr_text(&s.peer_addr().unwrap_or(none))
    )
}

fn lookup(host: &str, port: i64) -> Result<Vec<SocketAddr>, (i64, i64)> {
    match (host, port as u16).to_socket_addrs() {
        Ok(a) => {
            let v: Vec<SocketAddr> = a.collect();
            if v.is_empty() {
                Err((-K_HOST_NOT_FOUND, 0))
            } else {
                Ok(v)
            }
        }
        Err(_) => Err((-K_HOST_NOT_FOUND, 0)),
    }
}

/// Tries each address in turn; the last failure if none works.
fn first_ok<T>(
    addrs: &[SocketAddr],
    f: impl Fn(&SocketAddr) -> std::io::Result<T>,
) -> Result<T, (i64, i64)> {
    let mut last = (-K_OTHER, 0);
    for a in addrs {
        match f(a) {
            Ok(x) => return Ok(x),
            Err(e) => last = error(&e),
        }
    }
    Err(last)
}

type Reader = Arc<Mutex<Box<dyn Read + Send>>>;
type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

#[derive(Clone)]
enum Pipe {
    Read(Reader),
    Write(Writer),
}

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// A `watch` (op 15): the tree's last snapshot and the "next" job (op 16) waiting on it.
/// The tree is scanned again when a job arrives and at each poll while one waits, and the
/// differences become its events (as in `ppl_io.c`).
struct Watcher {
    root: String,
    recursive: bool,
    snap: Vec<Entry>,
    waiting: Option<i64>,
}

/// A snapshot entry: relative path, kind, size, modification time (ns), inode.
#[derive(PartialEq)]
struct Entry {
    rel: String,
    kind: char,
    size: u64,
    mtime: u128,
    ino: u64,
}

fn entry(rel: String, m: &fs::Metadata) -> Entry {
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(m);
    #[cfg(not(unix))]
    let ino = 0;
    Entry {
        rel,
        kind: kind_char(m.file_type()),
        size: m.len(),
        mtime: m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos()),
        ino,
    }
}

fn scan_dir(out: &mut Vec<Entry>, root: &str, rel: &str, recursive: bool) {
    let dir = if rel.is_empty() {
        root.to_string()
    } else {
        format!("{root}/{rel}")
    };
    let Ok(rd) = fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let child = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        if let Ok(m) = fs::symlink_metadata(format!("{root}/{child}")) {
            let is_dir = m.is_dir();
            out.push(entry(child.clone(), &m));
            if recursive && is_dir {
                scan_dir(out, root, &child, recursive);
            }
        }
    }
}

/// The tree at `root` (its entries, or the file itself as ""), sorted by path.
fn scan(root: &str, recursive: bool) -> std::io::Result<Vec<Entry>> {
    let m = fs::metadata(root)?;
    let mut out = Vec::new();
    if m.is_dir() {
        scan_dir(&mut out, root, "", recursive);
    } else {
        out.push(entry(String::new(), &m));
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

/// The events from `a` to `b` ("c|m|r <path>\0" each).
fn diff(a: &[Entry], b: &[Entry]) -> String {
    let mut text = String::new();
    let mut ev = |k: char, rel: &str| {
        text.push(k);
        text.push(' ');
        text.push_str(rel);
        text.push('\0');
    };
    let (mut i, mut k) = (0, 0);
    while i < a.len() || k < b.len() {
        let c = if i == a.len() {
            std::cmp::Ordering::Greater
        } else if k == b.len() {
            std::cmp::Ordering::Less
        } else {
            a[i].rel.cmp(&b[k].rel)
        };
        match c {
            std::cmp::Ordering::Less => {
                ev('r', &a[i].rel);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                ev('c', &b[k].rel);
                k += 1;
            }
            std::cmp::Ordering::Equal => {
                let (x, y) = (&a[i], &b[k]);
                if x.kind != y.kind || x.ino != y.ino {
                    ev('r', &x.rel);
                    ev('c', &y.rel);
                } else if x.kind != 'd' && (x.size != y.size || x.mtime != y.mtime) {
                    ev('m', &y.rel);
                }
                i += 1;
                k += 1;
            }
        }
    }
    text
}

impl IoState {
    /// Whether a "next" job waits on a watcher (the loop then polls instead of sleeping).
    pub(crate) fn watching(&self) -> bool {
        self.watchers.values().any(|w| w.waiting.is_some())
    }

    /// Scans watcher `id` if a job waits on it; finishes the job if anything changed.
    fn deliver(&mut self, id: i64) {
        let Some(w) = self.watchers.get_mut(&id) else {
            return;
        };
        let Some(job) = w.waiting else { return };
        let now = scan(&w.root, w.recursive).unwrap_or_default();
        let text = diff(&w.snap, &now);
        if text.is_empty() {
            return;
        }
        w.snap = now;
        w.waiting = None;
        self.done.push_back(Job {
            id: job,
            r: 0,
            r2: 0,
            text,
            ..Default::default()
        });
    }
}

#[derive(Default)]
struct Job {
    id: i64,
    r: i64,
    r2: i64,
    text: String,
    /// Bytes read, which become a buffer (its handle in `r`, their count in `r2`) when the
    /// job is polled.
    bytes: Option<Vec<u8>>,
}

/// `(r, r2)` for an error: the negated kind and the OS error number.
fn error(e: &std::io::Error) -> (i64, i64) {
    let k = match e.kind() {
        ErrorKind::NotFound => K_NOT_FOUND,
        ErrorKind::AlreadyExists => K_ALREADY_EXISTS,
        ErrorKind::PermissionDenied => K_PERMISSION_DENIED,
        ErrorKind::NotADirectory => K_NOT_A_DIRECTORY,
        ErrorKind::IsADirectory => K_IS_A_DIRECTORY,
        ErrorKind::DirectoryNotEmpty => K_DIRECTORY_NOT_EMPTY,
        ErrorKind::ConnectionRefused => K_CONN_REFUSED,
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            K_CONN_RESET
        }
        ErrorKind::AddrInUse => K_ADDR_IN_USE,
        ErrorKind::AddrNotAvailable => K_ADDR_NOT_AVAIL,
        ErrorKind::TimedOut => K_TIMED_OUT,
        _ => K_OTHER,
    };
    (-k, i64::from(e.raw_os_error().unwrap_or(0)))
}

fn kind_char(t: fs::FileType) -> char {
    if t.is_file() {
        'f'
    } else if t.is_dir() {
        'd'
    } else if t.is_symlink() {
        'l'
    } else {
        'o'
    }
}

fn read_all(path: &str) -> std::io::Result<Vec<u8>> {
    if fs::metadata(path)?.is_dir() {
        return Err(std::io::Error::from(ErrorKind::IsADirectory));
    }
    fs::read(path)
}

impl Interp<'_, '_> {
    pub(crate) fn io(&mut self, vals: &[Value]) -> Value {
        let n = |i: usize| match vals.get(i) {
            Some(Value::Int(x)) => *x as i64,
            _ => 0,
        };
        let s = |i: usize| match vals.get(i) {
            Some(Value::Str(x)) => x.to_string(),
            _ => String::new(),
        };
        let (op, a, b, c) = (n(0), n(1), n(2), n(3));
        if (20..=23).contains(&op) || op == 25 {
            return Value::Int(i128::from(self.process_job(op, a, b, c, &s(5))));
        }
        if (30..=36).contains(&op) {
            return Value::Int(i128::from(self.net_job(op, a, b, c, &s(4), &s(5))));
        }
        if op == 133 {
            // Clocks in nanoseconds: the wall clock, or a monotonic one.
            let ns = if a == 1 {
                let start = self.shared.start.get_or_init(std::time::Instant::now);
                start.elapsed().as_nanos() as i128
            } else {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos() as i128)
            };
            return Value::Int(ns);
        }
        if op == 140 {
            // Crypto: the runtime's own code (ppl_crypto.c over Mbed TLS), through FFI.
            let data = self.buffer_bytes(b, c);
            let (r, r2) = match crypto_op(a, &data) {
                Ok(bytes) => {
                    let n = bytes.len() as i64;
                    (self.shared.buffers.lock().expect("buffers").add(bytes), n)
                }
                Err(e) => (-1, e),
            };
            let st = &mut self.io;
            st.next_id += 1;
            st.taken.push(Job {
                id: st.next_id,
                r,
                r2,
                text: String::new(),
                bytes: None,
            });
            return Value::Int(i128::from(st.next_id));
        }
        if op == 134 || op == 135 {
            // Read a file now (time zone data), or a symbolic link's target.
            let path = self.at(&s(4));
            let (r, text) = if op == 134 {
                match fs::read(&path) {
                    Ok(bytes) => {
                        let n = bytes.len() as i64;
                        let h = self.shared.buffers.lock().expect("buffers").add(bytes);
                        ((h, n), String::new())
                    }
                    Err(_) => ((-1, 0), String::new()),
                }
            } else {
                let t = fs::read_link(&path)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                ((0, 0), t)
            };
            let st = &mut self.io;
            st.next_id += 1;
            st.taken.push(Job {
                id: st.next_id,
                r: r.0,
                r2: r.1,
                text,
                bytes: None,
            });
            return Value::Int(i128::from(st.next_id));
        }
        if (130..=132).contains(&op) {
            // TextEncoder / TextDecoder, random bytes.
            let (r, r2, text, bytes) = if op == 132 {
                let mut bytes = vec![0u8; a.max(0) as usize];
                fs::File::open("/dev/urandom")
                    .and_then(|mut f| f.read_exact(&mut bytes))
                    .expect("/dev/urandom");
                let n = bytes.len() as i64;
                (0, n, String::new(), Some(bytes))
            } else if op == 130 {
                let bytes = s(4).into_bytes();
                let n = bytes.len() as i64;
                (0, n, String::new(), Some(bytes))
            } else {
                let data = self.buffer_bytes(b, c);
                match String::from_utf8(data) {
                    Ok(t) => (0, 0, t, None),
                    Err(e) if a == 1 => (
                        -K_INVALID_DATA,
                        e.utf8_error().valid_up_to() as i64,
                        String::new(),
                        None,
                    ),
                    Err(e) => (
                        0,
                        0,
                        String::from_utf8_lossy(e.as_bytes()).into_owned(),
                        None,
                    ),
                }
            };
            let r = match bytes {
                Some(bytes) => self.shared.buffers.lock().expect("buffers").add(bytes),
                None => r,
            };
            let st = &mut self.io;
            st.next_id += 1;
            st.taken.push(Job {
                id: st.next_id,
                r,
                r2,
                text,
                bytes: None,
            });
            return Value::Int(i128::from(st.next_id));
        }
        if op == 120 || op == 121 {
            let mut net = self.io.net.lock().expect("net");
            if op == 121 {
                if let Some(Sock::Stream(st)) = net.socks.remove(&a) {
                    // SAFETY: frees the session of a descriptor that no job uses any more.
                    unsafe { ppl_tls_free(std::os::fd::AsRawFd::as_raw_fd(&*st)) };
                }
            } else if let Some(Sock::Stream(st)) = net.socks.get(&a) {
                if b != 0 {
                    // SAFETY: the descriptor is open (the stream is in the table).
                    unsafe { ppl_tls_close_notify(std::os::fd::AsRawFd::as_raw_fd(&**st)) };
                }
                let how = match b {
                    0 => std::net::Shutdown::Read,
                    1 => std::net::Shutdown::Write,
                    _ => std::net::Shutdown::Both,
                };
                let _ = st.shutdown(how);
            }
            return Value::Int(0);
        }
        if (110..=119).contains(&op) {
            return Value::Int(i128::from(self.process_op(op, a, b, c, &s(4), &s(5))));
        }
        if op == 16 {
            // A watcher's next events: now if there are any, else at a later poll.
            let st = &mut self.io;
            st.next_id += 1;
            let id = st.next_id;
            match st.watchers.get_mut(&a) {
                Some(w) if w.waiting.is_none() => {
                    w.waiting = Some(id);
                    st.deliver(a);
                }
                _ => st.done.push_back(Job {
                    id,
                    r: 1,
                    r2: 0,
                    text: String::new(),
                    ..Default::default()
                }),
            }
            return Value::Int(i128::from(id));
        }
        if op < 100 {
            // (The second string is a path only for `rename`.)
            let path = self.at(&s(4));
            let other = if op == 8 { self.at(&s(5)) } else { s(5) };
            let (r, r2, text) = self.run_io(op, a, b, c, &path, &other);
            let st = &mut self.io;
            st.next_id += 1;
            let id = st.next_id;
            st.done.push_back(Job {
                id,
                r,
                r2,
                text,
                bytes: None,
            });
            return Value::Int(i128::from(id));
        }
        if op == 100 {
            let finished: Vec<Job> = self.io.finished.lock().expect("jobs").drain(..).collect();
            for mut j in finished {
                if let Some(bytes) = j.bytes.take() {
                    j.r2 = bytes.len() as i64;
                    j.r = self.shared.buffers.lock().expect("buffers").add(bytes);
                }
                self.io.done.push_back(j);
            }
        }
        let cwd = self.shared.cwd.clone();
        let st = &mut self.io;
        Value::Int(i128::from(match op {
            100 if st.done.is_empty() && st.watching() => {
                let ids: Vec<i64> = st.watchers.keys().copied().collect();
                for id in ids {
                    st.deliver(id);
                }
                match st.done.pop_front() {
                    Some(j) => {
                        let id = j.id;
                        st.taken.push(j);
                        id
                    }
                    None => -1,
                }
            }
            100 => match st.done.pop_front() {
                Some(j) => {
                    let id = j.id;
                    st.taken.push(j);
                    id
                }
                None => -1,
            },
            101 => st.done.len() as i64,
            105 => {
                st.next_id += 1;
                let text = match &cwd {
                    Some(d) => d.to_string_lossy().into_owned(),
                    None => std::env::current_dir()
                        .map(|d| d.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                };
                st.taken.push(Job {
                    id: st.next_id,
                    r: 0,
                    r2: 0,
                    text,
                    ..Default::default()
                });
                st.next_id
            }
            102 => st.taken.iter().find(|j| j.id == a).map_or(0, |j| j.r),
            103 => st.taken.iter().find(|j| j.id == a).map_or(0, |j| j.r2),
            104 => {
                st.taken.retain(|j| j.id != a);
                0
            }
            106 => {
                if let Some(w) = st.watchers.remove(&a)
                    && let Some(job) = w.waiting
                {
                    st.done.push_back(Job {
                        id: job,
                        r: 1,
                        r2: 0,
                        text: String::new(),
                        ..Default::default()
                    });
                }
                0
            }
            _ => -1,
        }))
    }

    pub(crate) fn io_text(&mut self, vals: &[Value]) -> Value {
        let id = match vals.first() {
            Some(Value::Int(x)) => *x as i64,
            _ => 0,
        };
        let text = self
            .io
            .taken
            .iter()
            .find(|j| j.id == id)
            .map(|j| j.text.clone())
            .unwrap_or_default();
        Value::Str(text.as_str().into())
    }

    /// `p` against the program's working directory (if it has its own).
    fn at(&self, p: &str) -> String {
        match &self.shared.cwd {
            Some(d) if !p.is_empty() && !p.starts_with('/') => {
                d.join(p).to_string_lossy().into_owned()
            }
            _ => p.to_string(),
        }
    }

    /// Runs job `op`: `(r, r2, text)`.
    fn run_io(&mut self, op: i64, a: i64, b: i64, c: i64, s: &str, t: &str) -> (i64, i64, String) {
        let ok = (0, 0, String::new());
        let err = |e: std::io::Error| {
            let (r, r2) = error(&e);
            (r, r2, String::new())
        };
        match op {
            1 => match read_all(s) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(text) => (0, 0, text),
                    Err(_) => (-K_INVALID_DATA, 0, String::new()),
                },
                Err(e) => err(e),
            },
            2 => match read_all(s) {
                Ok(bytes) => {
                    let len = bytes.len() as i64;
                    let h = self.shared.buffers.lock().expect("buffers").add(bytes);
                    (h, len, String::new())
                }
                Err(e) => err(e),
            },
            3 | 4 => {
                let bytes: Vec<u8> = if op == 3 {
                    t.as_bytes().to_vec()
                } else {
                    self.buffer_bytes(b, c)
                };
                let file = fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .append(a != 0)
                    .truncate(a == 0)
                    .open(s);
                match file.and_then(|mut f| f.write_all(&bytes)) {
                    Ok(()) => ok,
                    Err(e) => err(e),
                }
            }
            5 => {
                let m = if a != 0 {
                    fs::symlink_metadata(s)
                } else {
                    fs::metadata(s)
                };
                match m {
                    Ok(m) => {
                        let mtime = m
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map_or(0, |d| d.as_millis() as i64);
                        (
                            0,
                            0,
                            format!("{} {} {mtime}", kind_char(m.file_type()), m.len()),
                        )
                    }
                    Err(e) => err(e),
                }
            }
            6 => {
                let r = if a != 0 {
                    fs::create_dir_all(s)
                } else {
                    fs::create_dir(s)
                };
                r.map_or_else(err, |()| ok)
            }
            7 => match fs::symlink_metadata(s) {
                Ok(m) => {
                    let r = if !m.is_dir() {
                        fs::remove_file(s)
                    } else if a != 0 {
                        fs::remove_dir_all(s)
                    } else {
                        fs::remove_dir(s)
                    };
                    r.map_or_else(err, |()| ok)
                }
                Err(e) => err(e),
            },
            8 => fs::rename(s, t).map_or_else(err, |()| ok),
            9 => match fs::read_dir(s) {
                Ok(entries) => {
                    let mut names: Vec<(Vec<u8>, char)> = Vec::new();
                    for e in entries.flatten() {
                        let k = e.file_type().map_or('o', kind_char);
                        names.push((e.file_name().to_string_lossy().as_bytes().to_vec(), k));
                    }
                    names.sort();
                    let mut text = String::new();
                    for (n, k) in names {
                        text.push(k);
                        text.push(' ');
                        text.push_str(&String::from_utf8_lossy(&n));
                        text.push('\0');
                    }
                    (0, 0, text)
                }
                Err(e) => err(e),
            },
            10 => {
                let opened = fs::OpenOptions::new()
                    .read(a & 1 != 0)
                    .write(a & 2 != 0)
                    .append(a & 4 != 0)
                    .create(a & 8 != 0)
                    .truncate(a & 16 != 0)
                    .create_new(a & 32 != 0)
                    .open(s);
                match opened {
                    Ok(f) => {
                        let st = &mut self.io;
                        st.next_fd += 1;
                        st.files.insert(st.next_fd, f);
                        (st.next_fd, 0, String::new())
                    }
                    Err(e) => err(e),
                }
            }
            11 => {
                let Some(f) = self.io.files.get_mut(&a) else {
                    return (-K_OTHER, 9, String::new());
                };
                let mut buf = vec![0u8; b.max(1) as usize];
                match f.read(&mut buf) {
                    Ok(k) => {
                        buf.truncate(k);
                        let h = self.shared.buffers.lock().expect("buffers").add(buf);
                        (h, k as i64, String::new())
                    }
                    Err(e) => err(e),
                }
            }
            12 => {
                let bytes = self.buffer_bytes(b, c);
                let Some(f) = self.io.files.get_mut(&a) else {
                    return (-K_OTHER, 9, String::new());
                };
                match f.write_all(&bytes) {
                    Ok(()) => (bytes.len() as i64, 0, String::new()),
                    Err(e) => err(e),
                }
            }
            13 => match self.io.files.remove(&a) {
                Some(_) => ok,
                None => (-K_OTHER, 9, String::new()),
            },
            14 => {
                let Some(f) = self.io.files.get_mut(&a) else {
                    return (-K_OTHER, 9, String::new());
                };
                match f.seek(SeekFrom::Start(b.max(0) as u64)) {
                    Ok(p) => (p as i64, 0, String::new()),
                    Err(e) => err(e),
                }
            }
            15 => match scan(s, a != 0) {
                Ok(snap) => {
                    let st = &mut self.io;
                    st.next_watcher += 1;
                    let id = st.next_watcher;
                    st.watchers.insert(
                        id,
                        Watcher {
                            root: s.to_string(),
                            recursive: a != 0,
                            snap,
                            waiting: None,
                        },
                    );
                    (id, 0, String::new())
                }
                Err(e) => err(e),
            },
            _ => (-K_OTHER, 22, String::new()),
        }
    }

    /// The first `len` bytes of buffer `handle`.
    fn buffer_bytes(&self, handle: i64, len: i64) -> Vec<u8> {
        let bufs = self.shared.buffers.lock().expect("buffers");
        let mem = bufs.mem.get(&handle).map(Vec::as_slice).unwrap_or(&[]);
        mem[..(len.max(0) as usize).min(mem.len())].to_vec()
    }
}

/// Fields of a spawn request (op 110): the working directory, `argc` arguments, `envc`
/// variables, separated by NULs.
fn spawn_fields(t: &str, argc: usize, envc: usize) -> (String, Vec<String>, Vec<(String, String)>) {
    let mut parts = t.split('\0').map(str::to_string);
    let dir = parts.next().unwrap_or_default();
    let args: Vec<String> = (0..argc)
        .map(|_| parts.next().unwrap_or_default())
        .collect();
    let env = (0..envc)
        .map(|_| {
            let kv = parts.next().unwrap_or_default();
            match kv.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (kv, String::new()),
            }
        })
        .collect();
    (dir, args, env)
}

fn stdio(mode: i64) -> Stdio {
    match mode {
        1 => Stdio::piped(),
        2 => Stdio::null(),
        _ => Stdio::inherit(),
    }
}

/// How a child ended: (exit code, or 128 + the signal; the signal or 0).
fn exit_of(status: std::process::ExitStatus) -> (i64, i64) {
    #[cfg(unix)]
    if let Some(sig) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return (128 + i64::from(sig), i64::from(sig));
    }
    (i64::from(status.code().unwrap_or(1)), 0)
}

impl Interp<'_, '_> {
    fn env_map(&mut self) -> &mut BTreeMap<String, String> {
        self.io.env.get_or_insert_with(|| {
            std::env::vars_os()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect()
        })
    }

    /// A job of `std/process` (ops 20–23 on threads of their own, 25 at once).
    fn process_job(&mut self, op: i64, a: i64, b: i64, c: i64, t: &str) -> i64 {
        self.io.next_id += 1;
        let id = self.io.next_id;
        let bad = |id| Job {
            id,
            r: -K_OTHER,
            r2: 9,
            ..Default::default()
        };
        let data = if op != 21 && op != 25 {
            Vec::new()
        } else if c >= 0 {
            self.buffer_bytes(b, c)
        } else {
            t.as_bytes().to_vec()
        };
        if op == 25 {
            // This process's stdout (the program's output) or stderr.
            if a == 2 {
                let _ = std::io::stderr().write_all(&data);
            } else {
                self.emit(&String::from_utf8_lossy(&data));
            }
            let r = data.len() as i64;
            self.io.done.push_back(Job {
                id,
                r,
                ..Default::default()
            });
            return id;
        }
        if a == 0 && !self.io.pipes.contains_key(&0) {
            self.io.pipes.insert(
                0,
                Pipe::Read(Arc::new(Mutex::new(Box::new(std::io::stdin())))),
            );
        }
        let pipe = match self.io.pipes.get(&a).cloned() {
            Some(p) => Some(p),
            // A TCP stream of `std/net`.
            None => match self.io.net.lock().expect("net").socks.get(&a) {
                // Through its TLS session, if it has one.
                Some(Sock::Stream(st)) if tls_has(st) => Some(if op == 21 {
                    Pipe::Write(Arc::new(Mutex::new(
                        Box::new(TlsIo(st.clone())) as Box<dyn Write + Send>
                    )))
                } else {
                    Pipe::Read(Arc::new(Mutex::new(
                        Box::new(TlsIo(st.clone())) as Box<dyn Read + Send>
                    )))
                }),
                Some(Sock::Stream(st)) => st.try_clone().ok().map(|st| {
                    if op == 21 {
                        Pipe::Write(Arc::new(Mutex::new(Box::new(st) as Box<dyn Write + Send>)))
                    } else {
                        Pipe::Read(Arc::new(Mutex::new(Box::new(st) as Box<dyn Read + Send>)))
                    }
                }),
                _ => None,
            },
        };
        let child = self.io.children.get(&a).cloned();
        let finished = self.io.finished.clone();
        let waker = self.waker.clone();
        let work: Box<dyn FnOnce() -> Job + Send> = match (op, pipe, child) {
            (20, Some(Pipe::Read(r)), _) => Box::new(move || {
                let mut buf = vec![0u8; b.max(1) as usize];
                let got = r.lock().expect("pipe").read(&mut buf);
                match got {
                    Ok(k) => {
                        buf.truncate(k);
                        Job {
                            id,
                            bytes: Some(buf),
                            ..Default::default()
                        }
                    }
                    Err(e) => {
                        let (r, r2) = error(&e);
                        Job {
                            id,
                            r,
                            r2,
                            ..Default::default()
                        }
                    }
                }
            }),
            (21, Some(Pipe::Write(w)), _) => Box::new(move || {
                let res = w.lock().expect("pipe").write_all(&data);
                match res.and_then(|()| w.lock().expect("pipe").flush()) {
                    Ok(()) => Job {
                        id,
                        r: data.len() as i64,
                        ..Default::default()
                    },
                    Err(e) => {
                        let (r, r2) = error(&e);
                        Job {
                            id,
                            r,
                            r2,
                            ..Default::default()
                        }
                    }
                }
            }),
            (22, Some(Pipe::Read(r)), _) => Box::new(move || {
                let mut buf = Vec::new();
                let got = r.lock().expect("pipe").read_to_end(&mut buf);
                match got {
                    Ok(_) => match String::from_utf8(buf) {
                        Ok(text) => Job {
                            id,
                            text,
                            ..Default::default()
                        },
                        Err(_) => Job {
                            id,
                            r: -K_INVALID_DATA,
                            ..Default::default()
                        },
                    },
                    Err(e) => {
                        let (r, r2) = error(&e);
                        Job {
                            id,
                            r,
                            r2,
                            ..Default::default()
                        }
                    }
                }
            }),
            (23, _, Some(ch)) => {
                self.io.children.remove(&a);
                Box::new(move || {
                    // Polled, so `kill` can take the lock in between (and never signals a
                    // reaped pid).
                    loop {
                        let st = ch.lock().expect("child").try_wait();
                        match st {
                            Ok(Some(status)) => {
                                let (r, r2) = exit_of(status);
                                return Job {
                                    id,
                                    r,
                                    r2,
                                    ..Default::default()
                                };
                            }
                            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(1)),
                            Err(e) => {
                                let (r, r2) = error(&e);
                                return Job {
                                    id,
                                    r,
                                    r2,
                                    ..Default::default()
                                };
                            }
                        }
                    }
                })
            }
            _ => {
                self.io.done.push_back(bad(id));
                return id;
            }
        };
        std::thread::spawn(move || {
            let job = work();
            finished.lock().expect("jobs").push_back(job);
            waker.wake();
        });
        id
    }

    /// A job of `std/net` (ops 30–35, as in `ppl_io.c`), on a thread of its own.
    fn net_job(&mut self, op: i64, a: i64, b: i64, c: i64, s: &str, t: &str) -> i64 {
        self.io.next_id += 1;
        let id = self.io.next_id;
        let data = if op != 34 {
            Vec::new()
        } else if c >= 0 {
            self.buffer_bytes(b, c)
        } else {
            t.as_bytes().to_vec()
        };
        let s = s.to_string();
        let t_text = t.to_string();
        let net = self.io.net.clone();
        let finished = self.io.finished.clone();
        let waker = self.waker.clone();
        let fail = move |(r, r2): (i64, i64)| Job {
            id,
            r,
            r2,
            ..Default::default()
        };
        let sock = net.lock().expect("net").socks.get(&a).cloned();
        let work = move || -> Job {
            match op {
                30 => {
                    let addrs = match lookup(&s, a) {
                        Ok(v) => v,
                        Err(e) => return fail(e),
                    };
                    let made = if b == 1 {
                        first_ok(&addrs, |x| UdpSocket::bind(x)).map(|u| {
                            let t = u.local_addr().map(|x| addr_text(&x)).unwrap_or_default();
                            (Sock::Udp(Arc::new(u)), t)
                        })
                    } else {
                        first_ok(&addrs, |x| TcpListener::bind(x)).map(|l| {
                            let t = l.local_addr().map(|x| addr_text(&x)).unwrap_or_default();
                            (Sock::Listener(Arc::new(l)), t)
                        })
                    };
                    match made {
                        Ok((sock, text)) => Job {
                            id,
                            r: net.lock().expect("net").add(sock),
                            text,
                            ..Default::default()
                        },
                        Err(e) => fail(e),
                    }
                }
                31 => match sock {
                    Some(Sock::Listener(l)) => match l.accept() {
                        Ok((st, _)) => {
                            let text = ends_text(&st);
                            Job {
                                id,
                                r: net.lock().expect("net").add(Sock::Stream(Arc::new(st))),
                                text,
                                ..Default::default()
                            }
                        }
                        Err(e) => fail(error(&e)),
                    },
                    _ => fail((-K_OTHER, 9)),
                },
                36 => match sock {
                    Some(Sock::Stream(st)) => {
                        // A TLS handshake: `t` holds a PEM text, a NUL and a second one.
                        let (p1, p2) = t_text.split_once('\0').unwrap_or((t_text.as_str(), ""));
                        let host = std::ffi::CString::new(s.clone()).unwrap_or_default();
                        let mut err = vec![0u8; 512];
                        let fd = std::os::fd::AsRawFd::as_raw_fd(&*st);
                        // SAFETY: valid pointers and lengths for the duration of the call.
                        let r = unsafe {
                            ppl_tls_start(
                                fd,
                                b as i32,
                                host.as_ptr(),
                                p1.as_ptr(),
                                p1.len(),
                                p2.as_ptr(),
                                p2.len(),
                                err.as_mut_ptr(),
                                err.len(),
                            )
                        };
                        if r == 0 {
                            Job {
                                id,
                                ..Default::default()
                            }
                        } else {
                            let end = err.iter().position(|&c| c == 0).unwrap_or(err.len());
                            Job {
                                id,
                                r: -15,
                                text: String::from_utf8_lossy(&err[..end]).into_owned(),
                                ..Default::default()
                            }
                        }
                    }
                    _ => fail((-K_OTHER, 9)),
                },
                32 => {
                    let addrs = match lookup(&s, a) {
                        Ok(v) => v,
                        Err(e) => return fail(e),
                    };
                    match first_ok(&addrs, |x| TcpStream::connect(x)) {
                        Ok(st) => {
                            let text = ends_text(&st);
                            Job {
                                id,
                                r: net.lock().expect("net").add(Sock::Stream(Arc::new(st))),
                                text,
                                ..Default::default()
                            }
                        }
                        Err(e) => fail(e),
                    }
                }
                33 => match sock {
                    Some(Sock::Udp(u)) => {
                        let mut buf = vec![0u8; b.max(1) as usize];
                        match u.recv_from(&mut buf) {
                            Ok((k, from)) => {
                                buf.truncate(k);
                                Job {
                                    id,
                                    text: addr_text(&from),
                                    bytes: Some(buf),
                                    ..Default::default()
                                }
                            }
                            Err(e) => fail(error(&e)),
                        }
                    }
                    _ => fail((-K_OTHER, 9)),
                },
                34 => match sock {
                    Some(Sock::Udp(u)) => {
                        let (port, host) = s.split_once(' ').unwrap_or((s.as_str(), ""));
                        let addrs = match lookup(host, port.parse().unwrap_or(0)) {
                            Ok(v) => v,
                            Err(e) => return fail(e),
                        };
                        let mine = u.local_addr().map(|x| x.is_ipv4()).unwrap_or(true);
                        let to = addrs
                            .iter()
                            .find(|x| x.is_ipv4() == mine)
                            .unwrap_or(&addrs[0]);
                        match u.send_to(&data, to) {
                            Ok(k) => Job {
                                id,
                                r: k as i64,
                                ..Default::default()
                            },
                            Err(e) => fail(error(&e)),
                        }
                    }
                    _ => fail((-K_OTHER, 9)),
                },
                _ => match lookup(&s, 0) {
                    Ok(addrs) => {
                        let mut seen: Vec<String> = Vec::new();
                        for x in addrs {
                            let ip = x.ip().to_string();
                            if !seen.contains(&ip) {
                                seen.push(ip);
                            }
                        }
                        let mut text = String::new();
                        for ip in seen {
                            text.push_str(&ip);
                            text.push('\0');
                        }
                        Job {
                            id,
                            text,
                            ..Default::default()
                        }
                    }
                    Err(e) => fail(e),
                },
            }
        };
        std::thread::spawn(move || {
            let job = work();
            finished.lock().expect("jobs").push_back(job);
            waker.wake();
        });
        id
    }

    /// A control operation of `std/process` (ops 110–119).
    fn process_op(&mut self, op: i64, a: i64, b: i64, c: i64, s: &str, t: &str) -> i64 {
        let result = |st: &mut IoState, r: i64, r2: i64, text: String| {
            st.next_id += 1;
            let id = st.next_id;
            st.taken.push(Job {
                id,
                r,
                r2,
                text,
                bytes: None,
            });
            id
        };
        match op {
            110 => {
                let (dir, args, vars) = spawn_fields(t, a.max(0) as usize, b.max(0) as usize);
                let mut cmd = std::process::Command::new(s);
                cmd.args(&args);
                if !dir.is_empty() {
                    cmd.current_dir(self.at(&dir));
                } else if let Some(d) = &self.shared.cwd {
                    cmd.current_dir(d);
                }
                if c & 64 != 0 {
                    cmd.env_clear();
                } else if self.io.env.is_some() {
                    let env = self.env_map().clone();
                    cmd.env_clear().envs(env);
                }
                cmd.envs(vars);
                cmd.stdin(stdio(c & 3))
                    .stdout(stdio((c >> 2) & 3))
                    .stderr(stdio((c >> 4) & 3));
                match cmd.spawn() {
                    Ok(mut child) => {
                        let pid = i64::from(child.id());
                        let st = &mut self.io;
                        let mut fd = |p: Option<Pipe>| match p {
                            Some(p) => {
                                st.next_pipe = st.next_pipe.max(999) + 1;
                                st.pipes.insert(st.next_pipe, p);
                                st.next_pipe
                            }
                            None => -1,
                        };
                        let i = fd(child.stdin.take().map(|w| {
                            Pipe::Write(Arc::new(Mutex::new(Box::new(w) as Box<dyn Write + Send>)))
                        }));
                        let o = fd(child.stdout.take().map(|r| {
                            Pipe::Read(Arc::new(Mutex::new(Box::new(r) as Box<dyn Read + Send>)))
                        }));
                        let e = fd(child.stderr.take().map(|r| {
                            Pipe::Read(Arc::new(Mutex::new(Box::new(r) as Box<dyn Read + Send>)))
                        }));
                        st.children.insert(pid, Arc::new(Mutex::new(child)));
                        result(st, pid, 0, format!("{i} {o} {e}"))
                    }
                    Err(e) => {
                        let (r, r2) = error(&e);
                        result(&mut self.io, r, r2, "-1 -1 -1".into())
                    }
                }
            }
            111 => {
                if let Some(ch) = self.io.children.get(&a) {
                    let mut ch = ch.lock().expect("child");
                    if let Ok(None) = ch.try_wait() {
                        #[cfg(unix)]
                        // SAFETY: the child isn't reaped (we hold its lock), so `a` is its pid.
                        unsafe {
                            kill(a as i32, b as i32);
                        }
                        #[cfg(not(unix))]
                        let _ = ch.kill();
                    }
                }
                0
            }
            112 => {
                self.io.pipes.remove(&a);
                0
            }
            113 => {
                let mut text = String::new();
                for arg in &self.shared.args {
                    text.push_str(arg);
                    text.push('\0');
                }
                result(&mut self.io, 0, 0, text)
            }
            114 => {
                let v = self.env_map().get(s).cloned();
                let found = i64::from(v.is_some());
                result(&mut self.io, found, 0, v.unwrap_or_default())
            }
            115 => {
                self.env_map().insert(s.to_string(), t.to_string());
                0
            }
            116 => {
                self.env_map().remove(s);
                0
            }
            117 => {
                let mut text = String::new();
                for (k, v) in self.env_map().iter() {
                    text.push_str(&format!("{k}={v}\0"));
                }
                result(&mut self.io, 0, 0, text)
            }
            119 => i64::from(std::process::id()),
            _ => -1,
        }
    }
}

unsafe extern "C" {
    fn ppl_crypto_op(
        op: i64,
        input: *const u8,
        n: usize,
        out: *mut *mut u8,
        out_len: *mut usize,
    ) -> i64;
    fn ppl_crypto_free(p: *mut std::ffi::c_void);
}

/// Runs crypto operation `op` of the C runtime on `input`: the output, or its error code.
fn crypto_op(op: i64, input: &[u8]) -> Result<Vec<u8>, i64> {
    let mut out: *mut u8 = std::ptr::null_mut();
    let mut len = 0usize;
    // SAFETY: `input` is valid for its length; the function sets `out`/`len` to a malloc'd
    // buffer (or null) that we copy and then free.
    let e = unsafe { ppl_crypto_op(op, input.as_ptr(), input.len(), &mut out, &mut len) };
    if e != 0 {
        return Err(e);
    }
    // SAFETY: on success `out` points at `len` initialized bytes.
    let bytes = unsafe { std::slice::from_raw_parts(out, len) }.to_vec();
    // SAFETY: allocated by the C side, freed once.
    unsafe { ppl_crypto_free(out.cast()) };
    Ok(bytes)
}

unsafe extern "C" {
    fn ppl_tls_has(fd: i32) -> i32;
    fn ppl_tls_start(
        fd: i32,
        mode: i32,
        host: *const std::ffi::c_char,
        pem1: *const u8,
        n1: usize,
        pem2: *const u8,
        n2: usize,
        err: *mut u8,
        errcap: usize,
    ) -> i32;
    fn ppl_tls_read(fd: i32, buf: *mut u8, n: usize) -> isize;
    fn ppl_tls_write(fd: i32, buf: *const u8, n: usize) -> isize;
    fn ppl_tls_close_notify(fd: i32);
    fn ppl_tls_free(fd: i32);
}

fn tls_has(st: &TcpStream) -> bool {
    // SAFETY: only looks the descriptor up.
    unsafe { ppl_tls_has(std::os::fd::AsRawFd::as_raw_fd(st)) != 0 }
}

/// A TLS connection's reads and writes, through the C runtime's session for its socket.
struct TlsIo(Arc<TcpStream>);

impl Read for TlsIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let fd = std::os::fd::AsRawFd::as_raw_fd(&*self.0);
        // SAFETY: `buf` is valid for writes of its length.
        let k = unsafe { ppl_tls_read(fd, buf.as_mut_ptr(), buf.len()) };
        if k < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(k as usize)
        }
    }
}

impl Write for TlsIo {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let fd = std::os::fd::AsRawFd::as_raw_fd(&*self.0);
        // SAFETY: `buf` is valid for reads of its length.
        let k = unsafe { ppl_tls_write(fd, buf.as_ptr(), buf.len()) };
        if k < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(k as usize)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
