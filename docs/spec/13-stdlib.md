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

## 13.5 Streams and encoding
### `TextEncoder` and `TextDecoder`
Globals, as WHATWG Encoding, UTF-8 only. `new TextEncoder().encode(text): Uint8Array`.
`new TextDecoder(label = "utf-8", { fatal, ignoreBOM })` (another label panics);
`decode(bytes = empty, { stream }): string throws TypeError`: each invalid sequence becomes
U+FFFD (the WHATWG "maximal subpart" rule) or, with `fatal`, throws `TypeError` (`The encoded
data was not valid utf-8 (at byte 1)`); a leading BOM is dropped unless `ignoreBOM`; with
`stream`, an incomplete sequence at the end waits for the next call. `TypeError` and `RangeError`
are core error classes. `Uint8Array` has `subarray(begin, end?)`, `slice(begin, end?)` (negative
positions count from the end) and `Uint8Array.from(values: int[])`.

### `std/encoding`
`base64.encode(data, url = false)` (RFC 4648; `url` uses `-`/`_` without padding) and
`base64.decode(text): Uint8Array throws TypeError` (either alphabet, padding optional,
whitespace skipped); `hex.encode(data)` (lowercase) and `hex.decode(text) throws TypeError`.

### `std/streams`
WHATWG Streams with chunk-counting queues, globals as on the Web (`std/streams` exports them
too, with `textEncoderStream()` and `textDecoderStream()`): `ReadableStream<T>`, `WritableStream<T>`,
`TransformStream<I, O>`, their default controllers, `ReadableStreamDefaultReader<T>`
(`read(): Promise<ReadResult<T>, Error>` with `{ done, value }`, `cancel`, `closed`,
`releaseLock`) and `WritableStreamDefaultWriter<T>` (`write`, `close`, `abort`, `ready`,
`desiredSize`, `closed`, `releaseLock`).
```ts
import { ReadableStream, TransformStream, WritableStream } from "std/streams"

const upper = new TransformStream<string, string>({ transform: (s, c) => c.enqueue(s.toUpperCase()) })
const sink = new WritableStream<string>({ write: (chunk, c) => println(chunk) })
await ReadableStream.from(["a", "b"]).pipeThrough(upper).pipeTo(sink)
for await (const chunk of conn.readable) handle(chunk)   // without a reader
```
Sources, sinks and transformers are structs of callbacks (`UnderlyingSource<T>`: `start`,
`pull`, `cancel`; `UnderlyingSink<T>`: `start`, `write`, `close`, `abort`; `Transformer<I, O>`:
`start`, `transform`, `flush`). Callbacks finish when they return; work that finishes later is
given as a promise through `pullAsync`, `writeAsync`, `closeAsync` and `transformAsync` (ppl has
no async lambdas yet). Strategies are `{ highWaterMark }` (default 1; a transform's readable
side 0). `pipeTo(dest, { preventClose, preventAbort, preventCancel })` writes each chunk after
the last one was taken; `pipeThrough(t)` returns `t.readable`. `ReadableStream.from(items)`
streams an array. `textEncoderStream()` and `textDecoderStream()` are transform streams from
strings to UTF-8 bytes and back. `std/net`'s `Conn` and `std/process`'s `Reader`/`Writer` have
`readable`/`writable` streams of `Uint8Array` (closing a connection's writable finishes writing;
once both directions are finished the connection closes itself).

## 13.6 HTTP
### `URL` and `URLSearchParams`
Globals, as WHATWG URL: `new URL(input, base?)` (throws `TypeError: Invalid URL: …`),
`URL.parse` (`null` instead), `URL.canParse`; `href`, `protocol`, `username`, `password`, `host`,
`hostname`, `port` (`""` for the scheme's default), `pathname`, `search`, `hash`, `origin`,
`searchParams` (changing it changes the URL), `toString()`, `toJSON()`. Special schemes (http,
https, ws, wss, ftp, file) get an authority, dot-segment removal and default ports; others an
opaque path. Not covered yet: IDNA (non-ASCII hosts are percent-encoded), IPv4 number forms,
setters. `URLSearchParams(init: string | [string, string][])` has `append`, `delete`, `get`,
`getAll`, `has`, `set`, `sort`, `size`, `keys`, `entries`, `for…of` and `toString()`
(`application/x-www-form-urlencoded`).

### `fetch`, `Headers`, `Request`, `Response`
Globals, as WHATWG Fetch. `fetch(input: string | URL | Request, init = {}): Promise<Response,
Error>` speaks HTTP/1.1 (`Connection: close`), follows up to 20 redirects (`redirect: "follow"`,
or `"manual"`, `"error"`; 303, and 301/302 after a POST, become a GET) and rejects with a
`TypeError` on network failures (`fetch failed: connection refused: 127.0.0.1:9`, with the
`NetError` as `cause`), never for HTTP error statuses. Only `http:` so far (`https:` rejects;
P7.5b). `RequestInit` is `{ method, headers, body, redirect }`, `ResponseInit` `{ status,
statusText, headers }`; a body (`BodyInit`) is a `string` (`text/plain;charset=UTF-8`), a
`Uint8Array`, a `ReadableStream<Uint8Array>` (sent chunked) or `URLSearchParams` (form
encoded). Bodies have `body` (a stream), `bodyUsed`, `bytes()`, `arrayBuffer()`, `text()` and
`json<T>()` (as `JSON.parse<T>`), each readable once. `Response.json<T>(data, init)` and
`Response.redirect(url, status = 302)` build responses; `ok`, `status`, `statusText`, `url`,
`redirected`. `Headers(init: [string, string][] | Headers)` keeps names lowercase; `get` joins
repeated values with `, ` (`getSetCookie()` keeps cookies apart); iteration is sorted by name.
Bodies are read whole before `fetch` resolves (and before a handler runs).

### `std/http`
```ts
import { serve } from "std/http"

async function handle(req: Request): Promise<Response> {
  new Response(`hello ${new URL(req.url).pathname}`)
}
const server = await serve({ port: 8080 }, handle)   // ServeOptions { port = 8000, hostname = "127.0.0.1" }
await server.shutdown()                             // stop accepting, close idle connections
await server.finished
```
`serve(options, handler: (req: Request) => Promise<Response, Error>): Promise<HttpServer,
NetError>` resolves once it listens (`server.addr`). Connections stay open between requests
(HTTP/1.1 keep-alive) unless either side sends `Connection: close`; a handler that throws gets
a `500` response, a malformed request a `400`. Responses with a known length get
`content-length`, stream bodies `transfer-encoding: chunked`; `HEAD` responses carry headers
only. TLS and WebSocket upgrades come with P7.5b and P7.5c.

### `WebSocket`
A global, as WHATWG WebSockets (RFC 6455): `new WebSocket(url, protocols = [])` (`ws:`; `wss:`
throws a `TypeError` until TLS, P7.5b) connects in the background. `readyState` (`CONNECTING`
0, `OPEN` 1, `CLOSING` 2, `CLOSED` 3), `protocol`, `url`; callbacks `onopen(Event)`,
`onmessage(MessageEvent)` (`data: string | Uint8Array`: text or binary; fragments are joined),
`onerror(Event)` and `onclose(CloseEvent)` (`code`, `reason`, `wasClean`; 1006 when the
connection was lost); `send(data: string | Uint8Array)` (panics while connecting) and
`close(code = 1000, reason = "")` (codes 1000 or 3000–4999, reason ≤ 123 bytes). Pings are
answered; client frames are masked with random keys. On the server:
```ts
import { serve, upgradeWebSocket } from "std/http"

async function handle(req: Request): Promise<Response> throws TypeError {
  const { socket, response } = upgradeWebSocket(req)   // throws TypeError if not an upgrade
  socket.onmessage = (e) => socket.send(e.data)
  response                                            // 101 Switching Protocols
}
```
`upgradeWebSocket(req, protocol = "")` checks `upgrade: websocket` and the key, and returns
`{ socket, response }`; once `serve` has sent the 101 response, the connection belongs to the
socket (`onopen` runs then). No extensions (`permessage-deflate`).

## 13.7 Regular expressions
`RegExp` (a global, also exported by `std/regex`) has ECMAScript syntax and semantics:
alternation, greedy and lazy quantifiers (`*`, `+`, `?`, `{n}`, `{n,}`, `{n,m}`, each with `?`),
groups (capturing, `(?:…)`, named `(?<name>…)`), backreferences (`\1`, `\k<name>`), lookahead
and lookbehind (`(?=…)`, `(?!…)`, `(?<=…)`, `(?<!…)`), classes (`[a-z]`, `[^…]`, `\d \w \s \D \W
\S`, `.`), anchors (`^ $ \b \B`) and escapes (`\n \t \xHH \uHHHH \u{H…} \cX`). Captures inside a
repeated group are reset at each repetition and an iteration that matches nothing ends the
loop, as in JavaScript. Flags: `g` (global), `i` (case-insensitive, simple case folding), `m`
(`^`/`$` at line ends), `s` (`.` matches line ends), `y` (sticky), `u`, `v`, `d` (accepted).
Matching is by code point, as if `u` were always on (`.` is one character), and indices count
code points, as ppl strings do.

A regex literal (`/\d+/g`) is checked at compile time (E0010, with JavaScript's message:
`Invalid regular expression: /(abc/: Unterminated group`) and can't fail at runtime;
`new RegExp(pattern, flags = "")` throws a `SyntaxError` instead. A `RegExp` has `source`,
`flags`, `global`, `ignoreCase`, `multiline`, `dotAll`, `unicode`, `sticky`, `hasIndices`,
`lastIndex`, `exec(s): RegExpMatch?` (from `lastIndex` with `g`/`y`, which it updates), `test(s)`
and `toString()`. A `RegExpMatch` is array-like: `m[0]` is the match, `m[i]` group `i` (`string?`:
`null` when the group didn't take part), `length`, `index`, `input`, `groups` (named groups:
`Map<string, string?>?`), `toArray()`, `for…of`.

String methods take a `string` or a `RegExp`: `replace(search, replacement)` (the first
occurrence; every match with `g`), `replaceAll(search, replacement)` (a `RegExp` must have `g`,
or it panics), `split(separator, limit = -1)` (a regex's groups are spliced in), and only a
`RegExp`: `match(re)` (`exec`'s match without `g`; with `g`, every match's text as the elements
and `index` -1), `matchAll(re): RegExpMatch[]`, `search(re): int`. A replacement is a string with
`$&`, `$1`…`$99`, `$<name>`, `` $` ``, `$'` and `$$`, or a function `(m: RegExpMatch) => string`.

## 13.8 Time
`std/time` follows Temporal, with exact times in `int` nanoseconds (so instants span 1678–2262):

- `Duration`: an exact length (`ofSeconds(n)`, `ofMilliseconds(n)`, … `ofDays(n)` as 24 hours,
  `of({ hours: 1, minutes: 30 })`, `parse("PT1H30M")`), with `plus`, `minus`, `times`,
  `dividedBy`, `negated`, `abs`, `sign`, `totalSeconds()`… and ISO 8601 `toString()` (hours are
  the largest unit: `PT36H`).
- `Instant`: a point on the UTC time line (`now()`, `fromEpochMilliseconds(ms)`,
  `parse("2024-03-10T12:30:00Z")`, which needs `Z` or an offset), with `plus`/`minus` a
  `Duration`, `since`/`until`, comparisons, `inZone(zone)` and `toString()` in UTC.
- `TimeZone`: `utc()`, `local()` (`TZ`, else `/etc/localtime`), `of(id)` for IANA ids
  (`Europe/Berlin`, from the system's tz database: `/usr/share/zoneinfo` or `TZDIR`, with each
  file's POSIX rule beyond its last transition) and offsets (`+05:30`), `fixed(seconds)`; it
  gives the offset, abbreviation and DST flag at an instant. An unknown id throws `RangeError`.
- `DateTime`: an instant in a zone with its ISO calendar fields (`year`, `month` 1–12, `day`,
  `hour`, …, `dayOfWeek` 1 = Monday, `dayOfYear`, `weekOfYear`, `offset`).
  `DateTime.of(y, m, d, h, mi, s, ns, zone, disambiguation)` resolves wall-clock times that a
  zone skips or repeats like Temporal (`"compatible"`: later in a gap, earlier in an overlap;
  `"earlier"`, `"later"`). `plus`/`minus` take a `Duration` (exact) or `DurationParts`
  (`{ months: 1, days: 2, hours: 3 }`: years and months keep the day, clamped to the month's
  end; days keep the wall-clock time; smaller units are exact). Also `with({ day: 1 })`,
  `withZone`, `startOfDay`, `parse` (`…+01:00[Europe/Berlin]`; a mismatched offset throws),
  `toString()` (with `[zone]`), `toIsoString()` and `format(pattern)` (`yyyy-MM-dd HH:mm:ss`,
  `EEE`, `MMMM`, `a`, `XXX`, `z`, `VV`, `'quoted'`; see the doc comment).

The `Date` global is JavaScript's: a `number` of milliseconds (`NaN` when invalid) seen in
local time or UTC, with the same constructor forms, getters, setters, `Date.UTC`, `Date.parse`
(ISO 8601 and the `toString`/`toUTCString` forms) and string forms, except that `toString`
shows the zone's abbreviation (`GMT+0100 (CET)`) and `toISOString` of an invalid date panics.
`performance.now()` is milliseconds since the program started (`performance.timeOrigin()` when
it did, since 1970).

## 13.9 Math
`Math` (a global) is JavaScript's on `number`s: the constants (`PI`, `E`, …), `abs`, `sign`,
`floor`, `ceil`, `round` (half up, as in JavaScript), `trunc`, `fround`, `sqrt`, `cbrt`, `pow`,
`exp`, `expm1`, `log`, `log1p`, `log2`, `log10`, the trigonometric and hyperbolic functions,
`atan2`, `hypot(...)`, `min(...)`, `max(...)`, `clz32`, `imul` and `random()` (xoshiro128**
seeded from the system: not for cryptography). Library functions are the platform's libm in
both backends.

`std/math` has `number` structs with operators (OpenGL conventions: right-handed, column-major
matrices acting on column vectors, so `a * b` applies `b` first): `Vec2`, `Vec3`, `Vec4` (`+ - *
/`, unary `-`, `[i]`, `dot`, `cross`, `length`, `normalize`, `distance`, `lerp`, `approxEquals`,
statics `of`, `zero`, `one`, `unitX`…), `Quat` (`fromAxisAngle`, `fromEuler`, `*`, `rotate(v)`,
`slerp`, `conjugate`, `inverse`, `toMat4`) and `Mat4` (`identity`, `fromRows`,
`fromColumnMajor`, `translation`, `scaling`, `rotation(axis, angle)`, `rotationX/Y/Z`,
`fromQuat`, `perspective`, `orthographic`, `lookAt`, `*` with a `Mat4` or `Vec4`,
`transformPoint`, `transformDirection`, `transpose`, `determinant`, `invert(): Mat4?`,
`toArray`). `BigInt` is an arbitrary-precision integer class (`BigInt.of(n)`, `BigInt.parse(s,
radix)` throwing `SyntaxError`, `+ - * / %` truncating like JavaScript, `pow`, `abs`,
`compareTo`, `equals`, `toInt`, `toNumber`, `toString()`, `toStringRadix(r)`). `Random` is a
seedable generator (`new Random(seed?)`: `float`, `int(lo, hi)` without bias, `bool(p)`,
`gaussian`, `pick`, `shuffle`, `nextU32`); a seed gives the same sequence on every target. Also
`clamp`, `lerp`, `degToRad`, `radToDeg`.

## 13.10 Collections
`std/collections` has `Deque<T>` (`pushBack`/`push`, `pushFront`/`unshift`, `popBack`/`pop`,
`popFront`/`shift` giving `T?`, `at(i)` with negative indices, `peekFront`, `peekBack`,
`length`, amortized O(1) at both ends), `PriorityQueue<T>` (a binary heap built with
`compare(a, b): int`, smallest first: `push`, `pop`, `peek`, `size`, `toSortedArray`, `for-of`
in priority order), `SortedMap<K extends Comparable<K>, V>` (`get`, `set`, `has`, `delete`,
`firstKey`, `lastKey`, `floorKey`, `ceilingKey`, `range(lo, hi)` for `lo <= k < hi`, `keys`,
`values`, `entries`, `for-of` in key order; binary search over sorted arrays, so inserting is
O(n)) and `BitSet` (`add`, `delete`, `has`, `set(i, on)`, `flip`, `size` (set bits), `length`,
`and`, `or`, `xor`, `andNot`, `equals`, `toArray`, `for-of` ascending). Iteration ends at the first
`null` (§5.7), so collections of nullable elements are walked through `toArray()`.
