# 13. Standard library

The stdlib is **batteries included**, and its API is **Web/Deno-compatible** wherever a Web
standard exists. Porting TS code should mostly mean adding types. All I/O is **async only**.

## 13.1 Globals
`print`, `println`, `assert`, `panic`, `Math`, `JSON`, `Array`, `Map`, `Set`, `Promise`, `Error` and
the built-in error classes, `fetch`, `Request`, `Response`, `Headers`, `URL`, `URLSearchParams`,
`TextEncoder`, `TextDecoder`, `setTimeout`, `setInterval`, `clearTimeout`, `queueMicrotask`,
`structuredClone`, `crypto` (`getRandomValues`, `randomUUID`, `subtle`), `performance.now()`,
`AbortController`, `AbortSignal`, `Worker`, `WeakRef`, `WeakMap`, `WeakSet`,
`FinalizationRegistry`, `Arena` (§9.4) and `parseInt(s): int` (base 10, JavaScript's parsing; a
string without leading digits panics, since `int` has no `NaN`). Integer arrays have `sum()`.

APIs that mirror a Web standard take that standard's **options object**, as a struct built from
an object literal (`fetch(url, { method: "POST", headers })`, `postMessage(m, { transfer })`).
ppl's own APIs take options as named arguments (`new Command("git", args, cwd: repo)`).

```ts
const res = await fetch("https://api.example.com/users")
const users = await res.json<User[]>()        // typed, generated like JSON.parse<T>
```

## 13.2 Modules
| Module | Contents (draft) |
|--------|------------------|
| `std/timers` | `delay(ms, signal?)`: a cancellable promise timer (§10.5) |
| `std/fs` | `readText`, `readBytes`, `writeText`, `writeBytes`, `open` (`File implements AsyncDisposable`), `readDir` (async iterator), `stat`, `mkdir`, `remove`, `rename`, `watch` |
| `std/path` | `join`, `resolve`, `dirname`, `basename`, `extname`, `relative`, `sep` |
| `std/process` | `args`, `env`, `exit`, `cwd`, `spawn`/`Command` (async), `stdin`/`stdout`/`stderr` streams |
| `std/net` | TCP/UDP sockets, `listen`, `connect`, DNS |
| `std/http` | `serve(handler)` on top of Web `Request`/`Response` (Deno.serve-like), WebSocket |
| `std/streams` | Web Streams: `ReadableStream`, `WritableStream`, `TransformStream` |
| `std/regex` | `RegExp` (also produced by regex literals) |
| `std/crypto` | hashes, HMAC, AEAD, signatures (Web Crypto shape) |
| `std/time` | `Instant`, `Duration`, `DateTime`, time zones (Temporal-inspired) |
| `std/gc` | GC control (§9) |
| `std/worker` | `self` inside workers (§10) |
| `std/reflect` | runtime reflection (§11) |
| `std/meta` | decorator/comptime API (§11) |
| `std/js` | `JsValue`, `js.global` (wasm32-browser) |
| `std/test` | `describe`, `test`, `expect`, mocks (§14) |
| `std/math` | vectors/matrices (`Vec2/3/4`, `Mat4`, `Quat` with operator overloads), bigint, random |
| `std/collections` | `Deque`, `PriorityQueue`, `SortedMap`, `BitSet` |
| `std/encoding` | base64, hex, utf-16, csv, toml |
| `std/fmt` | `Formattable`, padding/number formatting helpers |

## 13.3 Target availability
`std/fs`, `std/net` and `std/process` aren't available on `wasm32-browser`, and importing them there
is a **compile error**. Use platform-specific files (`storage.wasm.ppl` / `storage.ppl`) to provide
alternatives.

A JS-compatible `Date` global is kept so ported code works, but new code should use `std/time`.
`std/math` includes `Vec2`, `Vec3`, `Vec4`, `Mat4` and `Quat` as structs with operator overloads.

### Availability matrix
| Module / feature | native | wasm32-wasi | wasm32-browser |
|------------------|:------:|:-----------:|:--------------:|
| `std/fs` | ✅ | ✅ (preopened dirs) | ❌ compile error |
| `std/net` | ✅ | ✅ (WASI p2 sockets) | ❌ compile error |
| `std/process`: args, env, exit, cwd, stdio | ✅ | ✅ | ❌ compile error |
| `std/process`: `Command` (spawn) | ✅ | ❌ (no WASI support yet) | ❌ |
| `Worker`, `std/worker` | ✅ | ❌ (wasi-threads unstable) | ✅ (Web Workers) |
| `fetch`, `std/http` client | ✅ | ✅ (wasi-http) | ✅ (browser fetch) |
| `std/http` `serve` | ✅ | ✅ (wasi-http incoming) | ❌ |
| `std/js` | ❌ | ❌ | ✅ |
| everything else | ✅ | ✅ | ✅ |

Using an unavailable feature is a **compile error** that names the target. `comptime if
(target…)` or platform-specific files work around it.

### External processes
One cross-platform API runs wherever the platform supports processes, which is every native
target. Arguments are passed as an array, never through a shell, so there are no quoting
differences between Windows and POSIX.
```ts
import { Command } from "std/process"

const out = await new Command("git", ["status", "--short"], cwd: repo).output()
println(out.stdout)                         // string; out.code: i32; out.stderr

const child = new Command("ffmpeg", args, stdin: "pipe", stdout: "pipe", env: { "LANG": "C" }).spawn()
await child.stdin.write(bytes)
for await (const chunk of child.stdout) handle(chunk)
const status = await child.status()         // throws ProcessError when spawning fails
child.kill()                                // SIGTERM / TerminateProcess
```
`Command.shell("ls | wc -l")` runs through the platform shell when that's explicitly wanted.

```text
new Command(program, args = [], cwd?: string, env?: Map<string, string>, clearEnv = false,
            stdin: Stdio = "inherit", stdout: Stdio = "inherit", stderr: Stdio = "inherit")
type Stdio = "inherit" | "pipe" | "null"
Command.shell(script, …the same options)       // /bin/sh -c script (cmd.exe /c on Windows)
cmd.spawn(): Child                              // never throws; failures surface in status()
cmd.output(): Promise<Output, ProcessError>     // stdout and stderr piped, read as UTF-8
cmd.status(): Promise<ExitStatus, ProcessError>
```
`env` adds to or replaces this process's variables (with `clearEnv`, it's all the child gets);
a relative `cwd` is resolved against the working directory. A `Child` has `pid` (0 when it
couldn't start), `stdin` (a `Writer`), `stdout` and `stderr` (`Reader`s), `status()` (the same
promise on every call), `output()` (everything its piped streams still have, plus its status)
and `kill(signal = "SIGTERM")` (`"SIGKILL"`, `"SIGINT"`, `"SIGHUP"`; nothing happens once it has
ended). Using a stream that isn't piped panics. `ExitStatus` is `{ success, code, signal }`
(`code` is 128 + the signal's number when a signal ended the child, `signal` is `null`
otherwise); `Output` adds `stdout` and `stderr` strings. `ProcessError` (`command`, `kind` as
`IoError`'s, `message`: `cannot run nope: no such file or directory`) also reports output that
isn't UTF-8 (`InvalidData`).

A `Reader` reads chunks of bytes: `read(n = 65536): Promise<Uint8Array?, IoError>` (`null` at
the end), `for await (const chunk of r)`, or `text()` for the rest as UTF-8; one read at a
time (a second one while the first waits panics). A `Writer` has `write(data: Uint8Array)` and
`writeText(text)` (both `Promise<int, IoError>`, the bytes written), which happen in the order
they're made, and `close()`, which ends the child's input after them. A child that stops
reading makes writes reject (`Other`, os error 32) instead of ending the program.

This process: `args: string[]` (the arguments after the program), `pid`, `cwd()`, `env`
(`get(name): string?`, `has`, `set`, `delete`, `toObject(): Map<string, string>`; children
started later see changes), `exit(code = 0)` (ends the program at once: no `finally` blocks,
pending promises dropped) and `stdin` (a `Reader`), `stdout`, `stderr` (`Writer`s whose writes
are in order with `print`).

## 13.4 Async-only I/O
No I/O function has a synchronous variant. Scripts use top-level `await`:
```ts
import { readText } from "std/fs"
const text = await readText("notes.txt")
for (const line of text.split("\n")) println(line)
```

### `std/fs`
Each operation runs on the runtime's I/O threads and settles its promise on the event loop.
Failures reject with the global `IoError` (`path`, `message`, and a portable `kind`:
`NotFound`, `AlreadyExists`, `PermissionDenied`, `NotADirectory`, `IsADirectory`,
`DirectoryNotEmpty`, `InvalidData`, `Other`); messages read `no such file or directory: a.txt`.
```text
readText(path): Promise<string, IoError>              // UTF-8; other bytes: InvalidData
readBytes(path): Promise<Uint8Array, IoError>
writeText(path, text, append = false), writeBytes(path, data: Uint8Array, append = false)
stat(path, followLinks = true): Promise<FileInfo, IoError>  // isFile, isDirectory, isSymlink, size, mtime (ms)
exists(path): Promise<bool>
mkdir(path, recursive = false), remove(path, recursive = false), rename(from, to)
readDir(path): AsyncIterator<DirEntry> throws IoError  // name, isFile, isDirectory, isSymlink; name order
open(path, read = true, write = false, append = false, create = false, truncate = false,
     createNew = false): Promise<File, IoError>
```
A `File` (`AsyncDisposable`) has `read(n): Promise<Uint8Array?>` (`null` at the end), `write(data):
Promise<int>`, `seek(position): Promise<int>` (from the start), `close()` (twice is fine) and
`path`. Using it after `close` panics.

```text
watch(path, recursive = true): Promise<Watcher, IoError>
```
A `Watcher` (`Disposable`, an async iterator: `next(): Promise<WatchEvent?>`, `for await`) reports
`WatchEvent`s (`kind`: `"create"`, `"modify"` or `"remove"`; `path`: the watched path joined with
the entry's path, or the watched path itself when watching a file). Events are differences between
snapshots of the tree (kind, size, modification time, inode per entry), compared when an event is
asked for and again while one waits, in path order. So changes made while nobody waits arrive
together as one difference (a file created and written is one `create`; created and removed,
nothing), a rename is a `remove` and a `create`, and directories are never `modify`d.
`close()` (twice is fine) makes waiting and later `next()` calls return `null`. The OS
notifications (kqueue, inotify) only wake the watcher up; it also looks every 250 ms.

### `std/path`
POSIX paths with Node's `path.posix` semantics: `sep`, `isAbsolute`, `normalize` (`a//b/../c/` →
`a/c/`), `join(...parts)`, `resolve(...parts)` (from the working directory; no trailing slash),
`dirname`, `basename(p, ext = "")`, `extname` (`.bashrc` has none), `relative(from, to)`.
Windows paths come with the Windows target.

### `std/net`
TCP, UDP and DNS. Every operation that can wait (accepting, connecting, reading, writing,
sending, receiving, resolving) runs on the runtime's I/O threads and settles its promise on the
event loop. Failures reject with `NetError` (`address` as `host:port`, `message`, and a portable
`kind`: `ConnectionRefused`, `ConnectionReset`, `AddrInUse`, `AddrNotAvailable`, `TimedOut`,
`HostNotFound`, `InvalidData`, `PermissionDenied`, `Other`); messages read
`connection refused: 127.0.0.1:9`.
```ts
import { listen, connect } from "std/net"

const server = await listen(port: 9000)          // hostname = "127.0.0.1"; port 0 picks one
for await (const conn of server) echo(conn)       // conns until server.close()

const conn = await connect(9000, hostname: "localhost")
await conn.writeText("hi")
conn.closeWrite()
println(await conn.text())
conn.close()
```
```text
listen(port = 0, hostname = "127.0.0.1"): Promise<Listener, NetError>   // "0.0.0.0"/"::": every interface
connect(port, hostname = "127.0.0.1"): Promise<Conn, NetError>          // tries each address in turn
bind(port = 0, hostname = "127.0.0.1"): Promise<UdpSocket, NetError>
resolve(hostname): Promise<string[], NetError>                          // IP addresses, no repeats
type Addr = { hostname: string, port: int }                             // hostname is an IP address
```
A `Listener` has `addr`, `accept(): Promise<Conn?, NetError>` (one at a time; `null` once
closed), `for await`, and `close()` (twice is fine; a waiting `accept` gives `null`; accepted
connections stay open). A `Conn` has `localAddr`, `remoteAddr`, `read(n = 65536):
Promise<Uint8Array?, NetError>` (`null` once the other side finished writing or this side is
closed), `for await` over chunks, `text()` (the rest as UTF-8), `write(data)` and
`writeText(text)` (`Promise<int, NetError>`, in the order they're made), `closeWrite()` (the
other side reads the end after earlier writes; reading goes on) and `close()` (after earlier
writes; a waiting read gives `null`). One read at a time; a second while one waits panics, as
does writing after `closeWrite`/`close`. A `UdpSocket` has `addr`, `send(data, port, hostname =
"127.0.0.1")`, `sendText(text, port, hostname)` (one datagram each), `receive(n = 65536):
Promise<Datagram?, NetError>` (`{ data: Uint8Array, addr }`; bytes past `n` are dropped; `null`
once closed) and `close()`. Web Streams (`conn.readable`, `conn.writable`) come with
`std/streams`.
