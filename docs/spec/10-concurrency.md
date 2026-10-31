# 10. Concurrency

## 10.1 async / await
- `async function f(): Promise<T> throws E` starts running **eagerly** when called, as in JS.
- `await` suspends until the promise settles and rethrows `E` (checked).
- Async functions and generators compile to **stackless state machines**.
- **Top-level `await`** is allowed in the entry module.
- The event loop is **implicit**, as in Node: the process exits once nothing is pending (timers,
  I/O, promises, live workers).
```ts
const [a, b] = await Promise.all([fetchUser(1), fetchUser(2)])
setTimeout(() => println("later"), 100)
```
On native targets the loop uses epoll (Linux), kqueue (macOS) and IOCP (Windows). On wasm it uses
the host's loop (browser) or WASI poll.

## 10.2 Workers (threads)
Threads are **strictly isolated**: each worker has its own heap and its own module instances,
and workers share nothing. The API is the same as Web Workers, so code runs unchanged in the
browser.
```ts
// main.ppl
const w = new Worker<Job, Result>("./job")
w.onmessage = (r: Result) => println(`done: ${r.value}`)
w.onerror = (e: WorkerPanicked) => println(`worker died: ${e.message}`)
w.postMessage(Job { id: 1, data: [1, 2, 3] })

// job.ppl
import { self } from "std/worker"
self.onmessage = (job: Job) => {
  self.postMessage(Result { value: job.data.sum() })
}
```
- `Worker<In, Out>` is typed by the message types it receives and sends.
- A panic in a worker ends that worker and is delivered to `onerror`.
- `w.terminate()` stops a worker.

## 10.3 Messages
Messages are **deep-copied** into the receiver's heap. The compiler checks that the message type
is **Sendable**, meaning it's built only from:
- primitives, strings, structs, enums, tuples, arrays, Map/Set;
- classes whose fields are all Sendable (copied field by field, with the object graph preserved,
  cycles included).

Closures, `Worker` handles, file/socket handles, `ptr<T>` and arena references are **not**
Sendable. Sending one is a compile error.

### Transferables
Large byte buffers can be **moved** instead of copied:
```ts
const buf = new ArrayBuffer(64 << 20)
w.postMessage(Frame { pixels: buf }, transfer: [buf])
buf.byteLength            // 0: buf is detached in the sender
```
Only `ArrayBuffer` and its typed-array views (`Uint8Array`, `Float32Array`, …) are transferable.
They live off the GC heap (malloc'd), so moving one only hands over a pointer.

## 10.4 Inline workers & pools
```ts
const w = new Worker((n: i64): i64 => fib(n))     // capture-free function: compile error if it captures
const r = await w.call(40)

import { parallelMap, WorkerPool } from "std/worker"
const hashes = await parallelMap(files, (f: string) => sha256File(f), workers: 8)
const pool = new WorkerPool<Job, Result>("./job", size: navigator.hardwareConcurrency)
```
An inline worker body is compiled as its own entry point. Capturing anything is a compile error.

## 10.5 Cancellation
Cancellation uses Web-standard `AbortController`/`AbortSignal`. `fetch`, timers, fs, net and worker
calls accept `signal`. An aborted operation throws `AbortError`, which is checked like any other
error.
```ts
const ac = new AbortController()
setTimeout(() => ac.abort(), 5000)
const res = await fetch(url, signal: ac.signal)    // throws HttpError | AbortError
```

## Open questions
- `w.call()` for request/response over a worker: a built-in or a library pattern?
