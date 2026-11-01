//! File-system jobs for `std/fs` (`__io`, `__io_text`): the operations of the C runtime's
//! `ppl_io.c` with `std::fs`, same results and error kinds. A job runs when it's submitted;
//! its result waits in a queue until the event loop polls it, so promises still settle on a
//! later turn of the loop.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
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

#[derive(Default)]
pub(crate) struct IoState {
    next_id: i64,
    done: VecDeque<Job>,
    taken: Vec<Job>,
    files: HashMap<i64, fs::File>,
    next_fd: i64,
}

struct Job {
    id: i64,
    r: i64,
    r2: i64,
    text: String,
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
        if op < 100 {
            // (The second string is a path only for `rename`.)
            let path = self.at(&s(4));
            let other = if op == 8 { self.at(&s(5)) } else { s(5) };
            let (r, r2, text) = self.run_io(op, a, b, c, &path, &other);
            let st = &mut self.io;
            st.next_id += 1;
            let id = st.next_id;
            st.done.push_back(Job { id, r, r2, text });
            return Value::Int(i128::from(id));
        }
        let cwd = self.shared.cwd.clone();
        let st = &mut self.io;
        Value::Int(i128::from(match op {
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
                });
                st.next_id
            }
            102 => st.taken.iter().find(|j| j.id == a).map_or(0, |j| j.r),
            103 => st.taken.iter().find(|j| j.id == a).map_or(0, |j| j.r2),
            104 => {
                st.taken.retain(|j| j.id != a);
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
