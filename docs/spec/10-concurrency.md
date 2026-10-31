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

## Open questions
- Transferables (moving large buffers without a copy), still not decided. Deep copy only for now.
- Inline workers (`new Worker((msg) => …)` from a closure without captures)?
- Worker pools / `parallelMap` in the stdlib?
- `Promise` cancellation (`AbortSignal`)?
