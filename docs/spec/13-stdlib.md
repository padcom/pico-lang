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

## 13.3 Async-only I/O
No I/O function has a synchronous variant. Scripts use top-level `await`:
```ts
import { readText } from "std/fs"
const text = await readText("notes.txt")
for (const line of text.split("\n")) println(line)
```

## Open questions
- Which targets lack which modules? (`std/fs` and `std/net` in wasm32-browser: unavailable, or
  a virtual FS?)
- Is `std/math`'s vector library in core, or a separate package?
- Is a `Date` global kept for compatibility, or only `std/time`?
