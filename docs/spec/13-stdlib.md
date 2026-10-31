# 13. Standard library

The stdlib is **batteries included**, and its API is **Web/Deno-compatible** wherever a Web
standard exists. Porting TS code should mostly mean adding types. All I/O is **async only**.

## 13.1 Globals
`print`, `println`, `assert`, `panic`, `Math`, `JSON`, `Array`, `Map`, `Set`, `Promise`, `Error` and
the built-in error classes, `fetch`, `Request`, `Response`, `Headers`, `URL`, `URLSearchParams`,
`TextEncoder`, `TextDecoder`, `setTimeout`, `setInterval`, `clearTimeout`, `queueMicrotask`,
`structuredClone`, `crypto` (`getRandomValues`, `randomUUID`, `subtle`), `performance.now()`,
`AbortController`, `AbortSignal`, `Worker`.

```ts
const res = await fetch("https://api.example.com/users")
const users = await res.json<User[]>()        // typed, generated like JSON.parse<T>
```

## 13.2 Modules
| Module | Contents (draft) |
|--------|------------------|
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

## 13.4 Async-only I/O
No I/O function has a synchronous variant. Scripts use top-level `await`:
```ts
import { readText } from "std/fs"
const text = await readText("notes.txt")
for (const line of text.split("\n")) println(line)
```
