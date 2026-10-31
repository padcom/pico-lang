# 7. Errors

ppl has two failure mechanisms:

| | Exceptions | Panics |
|---|---|---|
| For | expected failures (I/O, parsing, validation) | bugs (overflow, out-of-bounds, failed `assert`, `!` on null) |
| Declared | yes, `throws E` is **checked** | no |
| Caught by | `try`/`catch` | only at the top level or a thread boundary |
| Stack trace | always captured | always captured |

## 7.1 Error types
Only subclasses of the built-in `Error` class can be thrown.
```ts
class Error {
  constructor(public message: string, public cause: Error? = null) {}
  public get stack(): StackTrace { … }        // captured when the error is thrown
}

class IoError extends Error { constructor(public path: string, msg: string) { super(msg) } }
class ParseError extends Error { constructor(public line: i32) { super(`parse error at ${line}`) } }
```
`throw` takes an expression whose type is a subclass of `Error`. A stack trace is **always**
captured when an error is thrown.

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

## 7.4 Async
```ts
async function fetchUser(id: i64): Promise<User> throws HttpError { … }
const u = await fetchUser(1)       // rethrows HttpError (checked)
```
The checker tracks the `throws` set together with the `Promise`. `await` rethrows it, so the
caller has to handle it or declare it.

## 7.5 Panics
A panic means a bug. Overflow, out-of-bounds indexing, division by zero, `x!` on null, failed
`assert`, out-of-range `as`, and an explicit `panic("msg")` all panic. Panics:
- unwind the current thread and run `finally` blocks;
- can't be caught by `catch`;
- terminate the process when they happen on the main thread, printing the message and the stack trace;
- in a worker, surface to the parent as a `WorkerPanicked` event/error (§10).

## Open questions
- `try?` (error → null) and `try!` (error → panic) sugar: not picked for now. Revisit later?
- Can `Error` subclasses be structs/enums later (a cheaper error representation)?
- How is the stack trace symbolized in release builds (embedded symbol tables vs external)?
