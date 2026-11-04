# 7. Errors

pico has two failure mechanisms:

| | Exceptions | Panics |
|---|---|---|
| For | expected failures (I/O, parsing, validation) | bugs (overflow, out-of-bounds, failed `assert`, `!` on null) |
| Declared | yes, `throws E` is **checked** | no |
| Caught by | `try`/`catch` | only at the top level or a thread boundary |
| Stack trace | always captured (except lightweight `Throwable`s) | always captured |

## 7.1 Error types
Two kinds of value can be thrown: subclasses of the built-in `Error` class (the default), and
**lightweight** structs/enums that `implement Throwable`.
```ts
class Error {
  constructor(public message: string, public cause: Error? = null) {}
  public get stack(): StackTrace { … }        // captured when the error is thrown
}

class IoError extends Error { constructor(public path: string, msg: string) { super(msg) } }   // a global; also has `kind` (§13.4)
class ParseError extends Error { constructor(public line: i32) { super(`parse error at ${line}`) } }
```
`throw` takes an `Error` subclass or a `Throwable`. `Error` subclasses **always** capture a stack
trace when they're thrown.

### Lightweight throwables
```ts
enum ParseFail implements Throwable { Eof, Unexpected(ch: char, at: int) }
function next(): Token throws ParseFail { … throw ParseFail.Eof }
```
`Throwable` requires only `message(): string`. Lightweight throwables are values, so there's no
allocation and no stack capture, which suits hot parsing loops and `--no-gc` code.

### Symbolization
Debug builds symbolize traces in-process. Release binaries print offsets into the executable
plus a build ID, and `pico symbolize trace.txt` resolves them from the separate debug file that
the release build produced next to the binary (`app.dbg` on Linux, `app.dSYM` on macOS; `.pdb`
on Windows is planned), checking the build ID:
```
stack trace (build 4c162639863f34213e87e7c053cfcff6; `pico symbolize` resolves it):
  0: app+0x1a560
  1: app+0x1ce18
```
becomes the debug build's trace with locations (inlined calls are frames too):
```
stack trace:
  0: inner at main.pi:1:34
  1: outer at main.pi:2:25
  2: <main> at main.pi:3:9
```
Decision 283.

## 7.2 Checked `throws`
A function that can throw must declare the errors it throws:
```ts
function readConfig(path: string): Config throws IoError | ParseError {
  const text = fs.readText(path)      // throws IoError
  parseConfig(text)                   // throws ParseError
}
```
- Calling a throwing function without handling or re-declaring every error type is a compile
  error.
- `throws A | B` is an error union. `throws Error` covers every subclass.
- A function with no `throws` clause can't throw.
- **Inference**: private (non-exported) functions and lambdas can leave out `throws`, and the
  compiler infers it. Exported functions must declare it.

### Throws polymorphism
A higher-order function is generic over what its callbacks throw, and the compiler infers this:
```ts
// declared in the stdlib:
map<U, E extends Error>(f: (x: T) => U throws E): U[] throws E

const nums = lines.map(l => parseInt(l))   // throws ParseError, so the call throws ParseError
const lens = lines.map(l => l.length)      // E = never, so the call can't throw
```

## 7.3 `try` / `catch` / `finally`
`try` is an expression. Its value comes from the `try` block, or from whichever `catch` ran.
```ts
const port = try parsePort(s) catch (e: ParseError) 8080

const config = try {
  readConfig(path)
} catch (e: IoError) {
  println(`cannot read ${e.path}`)
  Config.default()
} catch (e: ParseError) {
  throw new ConfigError("bad config", cause: e)
} finally {
  log.flush()
}
```
- `catch (e: T)` clauses are typed and tried in order. A clause can also use a pattern:
  `catch (e: IoError { path })`.
- Catching narrows the error union. Uncaught types propagate and must be declared by the
  enclosing function.
- `finally` always runs, and its value is ignored.
- An exception that escapes the top level prints `uncaught <Class>: <message>` to stderr and
  ends the program with exit code 1 (decision 197).

### `try?` and `try!`
```ts
const port = try? parsePort(s)          // port: u16?  (any error becomes null)
const n = try! parseInt("42")           // a thrown error becomes a panic: use when failure is a bug
```

## 7.4 Async
```ts
async function fetchUser(id: i64): Promise<User> throws HttpError { … }
const u = await fetchUser(1)       // rethrows HttpError (checked)
```
The checker tracks the `throws` set together with the `Promise`: the function above returns a
`Promise<User, HttpError>`. `await` rethrows it, so the caller has to handle it or declare it.

## 7.5 Panics
A panic means a bug. Overflow, out-of-bounds indexing, division by zero, `x!` on null, failed
`assert`, out-of-range `as`, and an explicit `panic("msg")` all panic. Panics:
- unwind the current thread and run `finally` blocks;
- can't be caught by `catch`;
- terminate the process when they happen on the main thread, printing the message and the stack trace;
- in a worker, surface to the parent as a `WorkerPanicked` event/error (§10).

The report on the main thread (decision 199):
```
panic: index out of bounds
  at src/main.pi:3:3
stack trace:
  0: inner
  1: outer
  2: <main>
```
