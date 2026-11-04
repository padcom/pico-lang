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
- The loop starts once the entry module's top-level code has finished. Each turn runs all
  microtasks (`queueMicrotask`, promise reactions; including ones they queue), then one timer:
  the earliest due, ties in creation order, so the microtasks a timer queues run before the
  next timer, as in browsers and Node. `setTimeout(f, ms = 0)` / `setInterval(f, ms)` return an
  `int` id for `clearTimeout` / `clearInterval`; an interval is rescheduled `ms` after its
  callback starts. When nothing is pending, the due finalization callbacks (§9.3) run, and the
  loop continues if they scheduled more.
- `Promise<T, E = never>`: `E` is what awaiting it can throw. `async function f(): Promise<T>
  throws E` has the type `() => Promise<T, E>` (several errors: `Error`); its result type
  must be written. Every `await` and reaction resumes one microtask after the promise settles,
  so the ordering matches JavaScript's. An error the body neither catches nor declares is an
  error (E0336); `await` outside an async function or the entry's top level is E0345.
- Promises: `new Promise<T>((resolve, reject) => …)`, `then(f)`, `settled`,
  `Promise.resolve(v)`, `Promise.reject<T>(e)` (a `Promise<T, Error>`), `Promise.all`, `race`
  and `allSettled` (`Settled<T> { status, value, reason }`). A rejected promise that nothing
  awaits or reacts to by the time the microtasks are done ends the program like an uncaught
  exception (exit code 1).
- Top-level `await` runs the event loop until the promise settles. A trailing `void`
  parameter may be left out when calling a function value (`resolve()`), and a lambda whose
  expected result is `void` discards its value, as in TS.

## 10.2 Workers (threads)
Threads are **strictly isolated**: each worker has its own heap and its own module instances,
and workers share nothing. The API is the same as Web Workers, so code runs unchanged in the
browser.
```ts
// main.pi
const w = new Worker<Job, Result>("./job")
w.onmessage = (r: Result) => println(`done: ${r.value}`)
w.onerror = (e: WorkerPanicked) => println(`worker died: ${e.message}`)
w.postMessage(Job { id: 1, data: [1, 2, 3] })

// job.pi
import { self } from "std/worker"
self.onmessage = (job: Job) => {
  self.postMessage(Result { value: job.data.sum() })
}
```
- `Worker<In, Out>` is typed by the message types it receives and sends.
- A panic in a worker ends that worker and is delivered to `onerror`.
- `w.terminate()` stops a worker.
- The module path is a string literal written in `new Worker<…>("…")` (the compiler builds the
  worker module into the program). A worker thread runs the library modules' top-level code
  (its own instances), then its module's, then its own event loop. `self.onmessage = (m: T) => …`
  receives the parent's messages (the parameter type says what they are) and keeps the worker
  alive; `self.postMessage(v)` sends to the parent and `self.close()` stops listening.
- An uncaught error ends a worker like a panic (`WorkerPanicked`, message
  `uncaught <Class>: <message>`); without an `onerror` it ends the program like an uncaught
  error. The program keeps running while any worker it started is alive (so a listening worker
  must be terminated, as in Node).

## 10.3 Messages
Messages are **deep-copied** into the receiver's heap. The compiler checks that the message type
is **Sendable**, meaning it's built only from:
- primitives, strings, structs, enums, tuples, arrays, Map/Set;
- classes whose fields are all Sendable (copied field by field, with the object graph preserved,
  cycles included).

Closures, `Worker` handles, file/socket handles, `ptr<T>` and arena references are **not**
Sendable. Sending one is a compile error. The check is the built-in bound `T extends Sendable`
(`Worker<In extends Sendable, Out extends Sendable>`, `self.postMessage<T extends Sendable>`);
interface values aren't Sendable either, and weak references arrive cleared.

### Transferables
Large byte buffers can be **moved** instead of copied:
```ts
const buf = new ArrayBuffer(64 << 20)
w.postMessage(Frame { pixels: buf }, { transfer: [buf] })
buf.byteLength            // 0: buf is detached in the sender
```
Only `ArrayBuffer` and its typed-array views (`Uint8Array`, `Float32Array`, …) are transferable.
They live off the GC heap (malloc'd), so moving one only hands over a pointer.
- `postMessage(message, options?: TransferOptions)`; `{ transfer: [buf] }` lists the buffers to
  move. A transferred buffer is detached in the sender (`byteLength` 0, `detached`, its views'
  `length` 0); any other buffer in a message is copied (its bytes too).
- Views: `Uint8Array`, `Int32Array`, `Uint32Array`, `Float32Array`, `Float64Array`, with
  `new View(length)`, `new View(buffer, byteOffset)`, `new View(buffer, byteOffset, length)`,
  `get`/`set`/`[]`, `length`, `byteLength`, `buffer`, `byteOffset`, `BYTES_PER_ELEMENT`.
  `ArrayBuffer` has `byteLength`, `detached` and `slice(begin, end)`; its memory is freed by a
  finalizer once it's collected (not yet for buffers received from another thread).

## 10.4 Inline workers & pools
```ts
const w = new Worker((n: i64): i64 => fib(n))     // capture-free function: compile error if it captures
const r = await w.call(40)

import { parallelMap, WorkerPool } from "std/worker"
const hashes = await parallelMap(files, (f: string) => sha256File(f), workers: 8)
const pool = new WorkerPool<Job, Result>("./job", size: navigator.hardwareConcurrency)
```
An inline worker body is compiled as its own entry point. Capturing anything is a compile error.
A worker built from a function supports **`await w.call(arg)`**, which returns the function's result
(and rethrows its errors, checked). File-based workers use `postMessage`/`onmessage`.
- `Worker<In, Out, E = never>`: an inline worker's `call` returns a `Promise<Out, E>`. Its thread
  runs the library modules' code (not the entry module's), so using the entry module's variables
  counts as capturing (E0346; a lambda given to `parallelMap` likewise). An inline worker keeps
  the program running only while calls are pending.
- `parallelMap(items, f, workers: n)` (from `std/worker`) returns `Promise<U[], E>` with the
  results in order; its workers end when it settles. `new WorkerPool<In, Out>("./job", size: n)`
  has `run(job): Promise<Out>` (to the least busy worker; each job must be answered by exactly
  one message), `size` and `terminate()`. `navigator.hardwareConcurrency` is the number of
  processors.

## 10.5 Cancellation
Cancellation uses Web-standard `AbortController`/`AbortSignal`. `fetch`, timers, fs, net and worker
calls accept `signal`. An aborted operation throws `AbortError`, which is checked like any other
error.
```ts
const ac = new AbortController()
setTimeout(() => ac.abort(), 5000)
const res = await fetch(url, { signal: ac.signal })    // throws HttpError | AbortError
```
- `AbortController` (`signal`, `abort(reason?: Error)`), `AbortSignal` (`aborted`, `reason`,
  `onabort`, `addEventListener("abort", f)`, `throwIfAborted()`, static `abort(reason?)` and
  `timeout(ms)`) and `AbortError` are globals. The `reason` is always an `AbortError`; the
  reason passed to `abort` becomes its `cause` (Web code may abort with any value; pico keeps the
  thrown type precise instead).
- Timers: `import { delay } from "std/timers"`; `delay(ms, signal?)` is a
  `Promise<void, AbortError>` that rejects (and clears its timer) when the signal aborts, at once
  if it already has. The callback timers (`setTimeout`) take no signal, as on the Web.
