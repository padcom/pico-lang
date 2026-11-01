# Design decisions

The decision log from the design interview. Each entry gives the choice and, where one was
stated, the reasoning. Entries are grouped by topic, and the spec chapters hold the details.

## Identity
| # | Topic | Decision |
|---|-------|----------|
| 1 | Name | **padcom programming language**, after the author's handle. |
| 2 | Extension | `.ppl`, which also reads as "People's Programming Language" because TS/JS are the most-used languages on GitHub. |
| 3 | Use cases | CLI tools, servers, games/graphics and browser/WASM, all first-class with **no domain bias**. |

## Implementation
| # | Topic | Decision |
|---|-------|----------|
| 4 | Compiler language | Rust. |
| 5 | Backend | LLVM only (via inkwell). It covers every native target plus wasm32. |
| 6 | Targets | x86_64 and arm64 on linux/macos/windows, wasm32 browser, wasm32 WASI. Embedded/riscv64 later. |
| 7 | Compiler UX | Elm/Rust-quality diagnostics, fast incremental builds, a single static binary (bundled lld), cross-compilation from any host. |
| 8 | Tooling | One `ppl` CLI: build, run (JIT/script mode), test (Jest/Vitest-style), fmt, add (package manager), REPL, LSP + VS Code extension. |
| 9 | First milestone | **Full spec first**, before any code. |

## Types
| # | Topic | Decision |
|---|-------|----------|
| 10 | Typing | TS-looking syntax with **nominal** types. |
| 11 | Interfaces | Nominal; a class conforms only through explicit `implements`. |
| 12 | Numbers | `i8..i64`, `u8..u64`, `f32`, `f64`; aliases `number` = f64 and `int` = i64. |
| 13 | Conversions | Only lossless widening is implicit; everything else needs `as`. |
| 14 | Overflow | Integer overflow **always traps**; explicit wrapping/saturating operations exist. |
| 15 | Null | A single `null`, no `undefined`, and no `any` (use `unknown`). |
| 16 | Strings | UTF-8 storage with a JS-like method API; `.length` counts code points. |
| 17 | Interpolation | Template literals with format specs (`${x:.2f}`) and tagged templates. |
| 18 | Unions | Union and literal types with narrowing (`typeof`, `instanceof`, discriminant fields). |
| 19 | Sum types | Payload `enum`s are the primitive; TS discriminated unions are supported and lower to them. |
| 20 | Value types | `struct`: stored inline, never GC-allocated on its own. |
| 21 | Generics | Monomorphization; `extends` constraints plus optional `where` clauses. |
| 22 | Collections | Growable `T[]`, fixed `[T; N]`, JS-like `Map`/`Set`, value tuples `[A, B]`. |
| 23 | Mutability | `const`/`let` as in TS, plus an opt-in **deep** `readonly`/`immutable` modifier enforced by the compiler. |
| 24 | Equality | Only strict `==`. It calls `equals()` when defined; otherwise identity for class refs and field by field for structs. |
| 25 | Operators | Operator overloading. |
| 26 | Reflection | Runtime reflection, `typeof`/`instanceof` narrowing, typed `JSON.parse<T>` generated at compile time. |

## Syntax & semantics
| # | Topic | Decision |
|---|-------|----------|
| 27 | Semicolons | Optional, with simple newline rules (Kotlin/Swift/Go-like, **not** JS ASI). |
| 28 | Entry point | Top-level statements; a file runs top to bottom. |
| 29 | Dropped | Loose equality, `var` and hoisting, `this` rebinding (`call`/`apply`/`bind`), `any`, `undefined`. |
| 30 | Expressions | Everything is an expression; the last expression of a block is its value, and `return` exits early. |
| 31 | Object literals | Both `Point { x, y }` and `{ x, y }` (when the expected type is known); `{` at statement start is a block. |
| 32 | Pattern matching | A `match` expression plus a modernized `switch` (patterns, no fallthrough), both exhaustive. |
| 33 | Functions | Named arguments and default/optional params (`c?: T` means `T \| null`). No extension methods, no pipe operator. |
| 34 | Classes | Inheritance, abstract, get/set, `static` members and static init blocks, constructor parameter properties. No mixins. |
| 35 | Visibility | Module members are private unless `export`ed; class members are **private by default**, otherwise `protected`/`public`. |
| 36 | Iteration | `for-of`, generators (`function*`), lazy iterators, ranges. |
| 37 | Ranges | Only exclusive `a..b`; stepping with `(a..b).step(n)`. |
| 38 | Printing | `print` / `println`. |
| 39 | Naming | TS conventions (camelCase values, PascalCase types), checked by lint only. |

## Errors
| # | Topic | Decision |
|---|-------|----------|
| 40 | Exceptions | **Checked** `throws E`. |
| 41 | Higher-order | Throws are inferred and polymorphic: `map` throws whatever its callback throws. |
| 42 | Panics | Overflow, out-of-bounds, failed asserts etc. are unchecked panics, catchable only at top level or a thread boundary. |
| 43 | Async errors | Written TS-style: `async function f(): Promise<T> throws E`. The checker tracks `E` on the promise, and `await` rethrows `E`, checked. |

## Memory & concurrency
| # | Topic | Decision |
|---|-------|----------|
| 44 | Memory | Tracing GC. |
| 45 | GC properties | Per-thread heaps, low pause (incremental/generational, manual collect/disable), arenas/off-heap escape hatch, WasmGC on the web. |
| 46 | No-GC | A `--no-gc` subset (structs, arenas, no class allocation) for embedded use. |
| 47 | Unsafe | Raw `ptr<T>` and pointer arithmetic only inside `unsafe { }`. |
| 48 | Concurrency | async/await on an event loop plus OS threads/workers. |
| 49 | Async impl | Stackless state machines; **eager** JS-like Promises. |
| 50 | Sharing | Threads are **strictly isolated**: message passing only, no shared memory. |

## Modules, interop, metaprogramming
| # | Topic | Decision |
|---|-------|----------|
| 51 | Modules | ES `import`/`export`, built-in package manager with a lockfile. |
| 52 | Stdlib | Batteries included: fs, path, process, net, http client and server, json, regex, crypto, time, testing. |
| 53 | C interop | `extern "C"` declarations and automatic import of C headers (`import * as sdl from "c:SDL2/SDL.h"`). |
| 54 | JS interop | wasm imports and exports with a generated `.d.ts`; existing `.d.ts` files can be read to type JS imports. |
| 55 | Comptime | Compile-time execution of ordinary ppl code. |
| 56 | Decorators | Ordinary ppl functions run at compile time that receive a type/AST and return modifications. |

## Round 2 (spec review)
| # | Topic | Decision |
|---|-------|----------|
| 58 | Member access | **Implicit members** inside methods (`x` means `this.x` unless a local shadows it), as in Java/C#/Kotlin. `this.` is still allowed. |
| 59 | Literal suffixes | `10u8`, `1.5f32`, etc. |
| 60 | Nullable shorthand | `T?` is shorthand for `T \| null`. |
| 61 | Distinct aliases | `type Meters = distinct f64` creates a new nominal type with zero runtime cost. |
| 62 | char | A separate `char` type (Unicode scalar value). |
| 64 | Char literals | No special syntax: a one-character string literal (`'a'` or `"a"`) has type `char` when the expected type is `char`, otherwise `string`. |
| 65 | Multiline strings | Triple-delimited `'''…'''`, `"""…"""` and ```` ```…``` ````, all indent-stripped. Only the backtick form interpolates. |
| 67 | String kinds | Only backtick strings interpolate `${…}`. `'…'` and `"…"` have no interpolation but process escapes (`\n`, `\u{…}`), as in JS. |
| 68 | Structs & GC | Structs may hold GC references (traced wherever they live). `@pod struct` enforces plain data (primitives/pod structs only) for FFI and GPU buffers. |
| 69 | Equality protocols | Methods `equals`, `hashCode`, `compareTo` (interfaces `Equatable`, `Hashable`, `Comparable`). Structs, enums and tuples derive them automatically; classes opt in with `@derive(Eq, Hash, Ord)`. |
| 66 | Regex literals | JS-style `/…/flags` regex literals, validated at compile time. |
| 63 | Format specs | Python/Rust-style mini-language after the top-level `:` in `${…}`: fill, align, sign, `#`, `0`, width, precision, type (`d x X o b e E f % ?`). |

## Round 5 (expressions & control flow)
| # | Topic | Decision |
|---|-------|----------|
| 70 | Loops | C-style `for(;;)`, `for-of`, `while`, `do-while`, and `loop` with `break value`. **No labeled** break/continue. |
| 71 | if without else | Used as a value it has type `T?` (null when the condition is false). |
| 72 | Match features | Guards (`if`), or-patterns and ranges, as-bindings (`c as Rect { w, h }`), `is` patterns with bindings and narrowing. |
| 73 | Operators | Keep `++`/`--` and all compound assignments, including `??=`, `&&=` and `\|\|=`. |

## Round 6 (functions)
| # | Topic | Decision |
|---|-------|----------|
| 74 | Closure capture | By value for `const`, by reference for `let`. Semantics identical to JS; a captured `let` is boxed only when a closure captures it. |
| 75 | Named args | Any parameter can be passed by name; positional arguments come first. Parameter names are part of the API. |
| 76 | Overloading | Real overloading by parameter types, resolved statically (most specific wins, ambiguity is an error). |
| 77 | Rest params | `...args: T[]` and spread `f(...arr)`. |

## Round 7 (classes & interfaces)
| # | Topic | Decision |
|---|-------|----------|
| 78 | Open/final | Classes can be extended by default (`final class` prevents it). Methods are **final by default** and must be `virtual` to be overridden (C#-style). |
| 79 | Override | The `override` keyword is required. |
| 80 | Default methods | Interfaces can have default method implementations, which replace mixins. |
| 81 | Constructors | TS `constructor`, overloadable, with delegation via `this(...)` and parameter properties. |

## Round 8 (open questions in ch. 3–6)
| # | Topic | Decision |
|---|-------|----------|
| 82 | Overridable keyword | `virtual`. |
| 83 | Sealed | No `sealed` classes. Exhaustiveness comes from enums and unions only. |
| 84 | private | Class-private: accessible only inside the class body. |
| 85 | Globals | Only `const` at module level; module-level `let` is forbidden. Mutable state lives in objects passed explicitly. |

## Round 9 (scripts & errors)
| # | Topic | Decision |
|---|-------|----------|
| 86 | Script body | In the **entry module**, top-level code behaves like the body of `main()`: `let` is allowed, and those bindings are locals that other modules can't see or export. Library modules allow only `const`. |
| 87 | Throwables | Subclasses of the built-in `Error` class (amended by #150: structs/enums implementing `Throwable` too). |
| 88 | try expression | `try … catch …` is an expression, e.g. `const n = try parse(s) catch (e: ParseError) 0`. |
| 89 | Error unions | `throws A \| B`; typed `catch` clauses narrow the union. (`try?`/`try!` were added later, see #141.) |
| 90 | Stack traces | Always captured on throw and on panic. |
| 91 | Throws inference | *(Confirmed in round 17)* Private functions and lambdas may omit `throws` (inferred); exported functions must declare it. |

## Round 10 (modules & packages)
| # | Topic | Decision |
|---|-------|----------|
| 92 | Manifest | `project.json` (JSON, package.json-like). |
| 93 | Package sources | A central ppl registry plus git URLs. |
| 94 | Specifiers | Extensionless relative imports: `./util` resolves `util.ppl`, then `util/index.ppl`. |
| 95 | Stdlib access | Common names are global (print/println, assert, panic, Math, JSON, Map, Set, Array, Promise, Error); the rest is imported from `std/…`. |

## Round 11 (memory & concurrency)
| # | Topic | Decision |
|---|-------|----------|
| 96 | Messages | Deep-copied into the receiver's heap. The compiler enforces **Sendable** (no closures, handles, pointers). |
| 97 | Worker API | Web Worker-like: `new Worker<In, Out>("./job")`, `postMessage`/`onmessage`, the same API on every target. |
| 98 | Arenas | Both a safe scoped `arena { … }` block (compiler forbids escapes) and an explicit `Arena` object inside `unsafe`. |
| 99 | Event loop | Implicit, like Node; top-level `await`; the process exits when nothing is pending. |

## Round 12 (comptime, decorators, reflection, cleanup)
| # | Topic | Decision |
|---|-------|----------|
| 100 | Comptime marking | Both automatic constant folding and an explicit `comptime` keyword (consts, blocks, params, `comptime if`). |
| 101 | Decorator powers | Add members, wrap/register functions, validate (emit diagnostics). They **can't** generate new top-level declarations. |
| 102 | Reflection cost | Whole-program, on demand: metadata only for types that can reach a reflection call. |
| 103 | Cleanup | TS `using` with `Disposable`/`AsyncDisposable`. No `defer`. |

## Round 13 (interop)
| # | Topic | Decision |
|---|-------|----------|
| 104 | FFI safety | FFI calls using only primitives, `@pod` structs and `string` are safe. Anything with pointers or callbacks needs `unsafe`. |
| 105 | C strings | A `string` in an `extern` signature is auto-converted to a temporary NUL-terminated copy. |
| 106 | JS calls | Both: typed imports through `.d.ts` (`js:` specifiers) and a dynamic `JsValue` escape hatch. |
| 107 | .d.ts mapping | Best effort: classes/interfaces become nominal handles, simple value shapes are converted to structs, everything else becomes `JsValue`. |

## Round 14 (stdlib & tooling)
| # | Topic | Decision |
|---|-------|----------|
| 108 | Stdlib style | Web/Deno-compatible: fetch, URL, Request/Response, Web Streams, timers, Web Crypto shape; `std/*` covers the rest. |
| 109 | I/O | **Async only**, with no `*Sync` variants. Scripts use top-level `await`. |
| 110 | Tests | Vitest-like `describe/test/expect` in `*.test.ppl` files. |
| 111 | Formatter | A few options (indent, lineWidth, quotes, semicolons) under `fmt` in `project.json`. |

## Round 15 (targets)
| # | Topic | Decision |
|---|-------|----------|
| 112 | Target names | LLVM triples, with short aliases (`windows-x64`, …). |
| 113 | Linking | Dynamic system libc on every platform; bundled stubs and lld make cross-compiling work without SDKs. |
| 114 | Windows | MinGW (GNU) ABI. |
| 115 | Min OS | Modern only: macOS 13+, Windows 10+, Linux 5.x / glibc 2.31. |

## Round 16 (open questions, part 1)
| # | Topic | Decision |
|---|-------|----------|
| 116 | compareTo | Returns `i32` (negative, zero or positive), as with JS sort comparators. |
| 117 | Enum patterns | Both positional `Circle(r)` and named `Circle { r }`. |
| 118 | Interface methods | Implementing an interface method doesn't make it virtual; subclasses can override it only if it's marked `virtual`. |
| 119 | Meta API | Both quasi-quote code templates (`${}` splicing, parsed and type-checked) and an AST builder API in `std/meta`. |

## Round 17 (open questions, part 2)
| # | Topic | Decision |
|---|-------|----------|
| 120 | Script functions | Named `function`s in the entry file can use top-level `let`s (closures, as in JS scripts). |
| 121 | Weak refs | `WeakRef`, `WeakMap`, `WeakSet` and `FinalizationRegistry` (JS-compatible, non-deterministic). |
| 122 | Package versions | Semver-compatible versions unify; different majors can coexist (Cargo-style). |

## Round 18 (open questions, part 3)
| # | Topic | Decision |
|---|-------|----------|
| 123 | Transferables | `postMessage(msg, transfer: [buf])` moves `ArrayBuffer`-like byte buffers without copying; the sender's handle becomes detached. Messages are otherwise deep-copied. |
| 124 | Inline workers | `new Worker((job: Job) => …)` from a **capture-free** function. |
| 125 | Pools | `parallelMap` and worker pools in the stdlib. |
| 126 | Cancellation | Web-standard `AbortController`/`AbortSignal`. |
| 127 | Browser fs/net | Importing `std/fs` or `std/net` for `wasm32-browser` is a compile error; use platform-specific files. |
| 128 | Date | A JS-compatible `Date` global is kept for porting; `std/time` is preferred. |
| 129 | Vector math | `Vec2/3/4`, `Mat4`, `Quat` ship in `std/math`. |
| 130 | musl | Extra `*-linux-musl` targets for fully static single-file binaries. |
| 131 | C export | `@export("c")` builds ppl shared/static libraries with a generated `.h`. |
| 132 | Debugging | DWARF (CodeView/PDB on Windows) plus a DAP debug adapter for VS Code. |

## Round 19 (open questions, part 4)
| # | Topic | Decision |
|---|-------|----------|
| 133 | Pointer-sized ints | `isize`/`usize` exist for FFI and unsafe code. `.length` and indices are `int` (i64), which is JS-friendly. |
| 134 | String indexing | `s[i]` returns a `char` (code point). It's O(n), sped up by a cached index; `s.bytes()` gives raw UTF-8. |
| 135 | Formatter semicolons | Keep the `semicolons` option. |
| 136 | Structs to JS | Structs cross into JS as plain object copies; classes become handle classes. |

## Round 20 (open questions, part 5)
| # | Topic | Decision |
|---|-------|----------|
| 137 | Async generators | `async function*` and `for await (… of …)`. |
| 138 | Destructuring defaults | `const { port = 8080 } = opts`, also in parameters. |
| 139 | Block comments | Not nested (as in JS). |
| 140 | Tuple spread | `f(...pair)` spreads a tuple into fixed-arity parameters. |
| 141 | try sugar | `try? e` turns an error into `null` (type `T?`); `try! e` turns an error into a panic. |
| 142 | switch case lists | `case "build" \| "b":`, the same or-pattern as `match`. |
| 143 | readonly methods | The `readonly` method modifier marks methods callable through `immutable T`; the compiler checks they don't mutate. |

## Round 21 (open questions, part 6)
| # | Topic | Decision |
|---|-------|----------|
| 144 | Package names | Scoped names are required: `scope/name` (e.g. `padcom/http-router`). |
| 145 | Workspaces | `"workspace": ["packages/*"]` in the root `project.json`, with one shared lockfile. |
| 146 | Overloads + defaults | Allowed; only calls that are actually ambiguous are errors. |
| 147 | Tail calls | `return tailcall f(…)` is guaranteed (LLVM `musttail`); if it can't be honored, that's a compile error. |

## Round 22 (closing open questions, part 1)
| # | Topic | Decision |
|---|-------|----------|
| 148 | Lockfile | `project.lock`. |
| 149 | Regex vs `/` | The JS rule: `/` starts a regex literal only where an expression can begin. |
| 150 | Lightweight errors | Structs and enums can be thrown when they `implement Throwable`. `Error` subclasses remain the default; lightweight throwables don't capture a stack trace, which is what makes them cheap. |
| 151 | Release traces | Release binaries keep raw addresses. `ppl symbolize` maps them using a separate debug file (`.dbg` / `.pdb` / `.dSYM`). Debug builds are symbolized in-process. |
| 152 | Comptime limits | 10M evaluation steps by default, configurable with `@comptimeLimit(n)`. |
| 153 | Comptime diagnostics | `decl.error/warn/note(msg, span?)`, attached to source spans. |

## Round 23 (closing open questions, part 2)
| # | Topic | Decision |
|---|-------|----------|
| 154 | no-gc strings | `str` slices (pointer + length) and `StrBuf<N>` fixed buffers, or arena allocation. In `--no-gc`, only lightweight `Throwable`s can be thrown. |
| 155 | glibc version | Given as a triple suffix: `x86_64-unknown-linux-gnu.2.35`. The default is 2.31. |
| 156 | Mobile | Android (`aarch64-linux-android`) and iOS (`aarch64-apple-ios`) come after v1. |
| 157 | C++ | Out of scope; use `extern "C"` shims. |
| 158 | Benchmarks | `bench()` in `std/test`, run by `ppl bench`. |
| 159 | Worker call | Function workers support `await w.call(arg)`; file workers use `postMessage`. |

## Round 24 (stdlib matrix)
| # | Topic | Decision |
|---|-------|----------|
| 160 | WASI workers | No workers on `wasm32-wasi` (wasi-threads is unstable). |
| 161 | External processes | A single cross-platform `Command` API in `std/process` (spawn, pipes, exit status, kill, env, cwd), available wherever the platform supports it. Today that's all native targets. It's a compile error on `wasm32-browser`; on `wasm32-wasi` it's unavailable until WASI gains process support. |

## Round 25 (registry)
| # | Topic | Decision |
|---|-------|----------|
| 162 | Registry | A git-backed index hosted on GitHub (like the crates.io index). Entries point at tagged git sources with content hashes. A web UI can come later. |

## Spec format
| # | Topic | Decision |
|---|-------|----------|
| 57 | Docs | Markdown: `PLAN.md` and `PROGRESS.md` at the repo root; `docs/DECISIONS.md` and `docs/spec/NN-chapter.md`. |

## Implementation decisions
Made while building the compiler. Entries marked fill gaps in the spec and are
waiting for the author's review.

| # | Topic | Decision |
|---|-------|----------|
| 163 | Error codes | `E00xx` lexical, `E01xx` syntax, `E02xx` names and modules, `E03xx` types (`E0308` mismatched types as in §14.2). The registry lives in `ppl-diag/src/codes.rs`, and codes are never reused. |
| 164 | Keywords | `do` is reserved (the §1.6 list had left it out, but `do … while` needs it). `type`, `of`, `from` and `arena` are contextual, as in TS. Reserved words are still allowed as property names after `.` (`map.get(k)`, `ages.set(…)`). |
| 165 | Regex vs `/` | A `/` starts a regex unless the previous significant token ends an expression: an identifier, a literal, `)`, `]`, `this`, `super`, `null`, `true`, `false`, or a postfix `!`/`++`/`--` after one of those. A `}` does **not** end an expression, so `} /re/` is a regex. Newlines don't change the state, which matches §1.2 (`x\n/ 2` divides). |
| 166 | Newline rule details | Refines §1.2. A line ending with `>`, `>>`, `>>>`, `?`, `!`, `++` or `--` is complete (generic closers, `T?`, postfix operators), so `let m: Map<K, V>` and `let s: string?` end their statements. Every other binary or assignment operator, `,`, `.`, `?.`, `:` and `=>` at the end continues the line. Leading tokens that continue: `.`, `?.`, `..`, binary operators other than `+`/`-`, assignment operators, `?`, `:`, `=>`, `\|` (multi-line unions), and `as`/`is`/`instanceof`/`throws`/`where`/`extends`/`implements`. Template substitutions behave like parentheses. The parser also skips terminators where a statement can't end: after `if (…)`/`while (…)`/`for (…)`, before `else`/`catch`/`finally`, after decorators, and inside object literals, struct literals and match bodies. |
| 167 | Operator precedence | §3.3 lists `&& \|\| ??` and `& ^ \|` on single rows. The parser uses the JS order inside those rows: `\|\|`/`??` < `&&` and `\|` < `^` < `&`. As the table says, unary operators bind tighter than `**` (`-2 ** 2` is `(-2) ** 2`), and `as` sits between them (`x as i32 + 1` is `(x as i32) + 1`). |
| 168 | `{` in expression position | An object literal if it's `{}` or starts with `...`, `name:`, `"key":`, `name,`, or is exactly `{ name }`. Otherwise it's a block. At the start of a statement `{` is always a block (§3.2). |
| 169 | Struct literals | `Path {` (with optional type arguments, `Pair<A, B> {`) directly after a dotted name on the same line is a struct/class literal. |
| 170 | Pattern contexts | In `match` arms, `catch` and `is`, `name: T` is a type-test pattern. In `case …:` a top-level `name:` is a plain binding, since the `:` ends the case. In bindings and parameters the `:` starts the type annotation. Inside a match guard, `x => …` ends the guard instead of starting an arrow function. Typed patterns take a type without a top-level union (`s: A \| B` would be ambiguous with or-patterns). |
| 171 | Statement separators | Two statements on one line need `;`, unless the first ends with `}` (`if (x) { … } f()`). |
| 172 | Array repeat | `[value; count]` builds a fixed array (the value form of `[T; N]`), as in Rust. The spec has the type but no literal for it. |
| 173 | Optional values | `T?` stays the only optional type; there's no `Option<T>`. The §3.5 example that used `Some(v)` was rewritten. |
| 174 | Decorator state | Decorators can attach **hidden statics** to the declaration they decorate (`decl.addStatic(name, init)`): private, once-initialized, per-thread state that only generated code references. This is how `@memoize` keeps its cache; decorators still can't add top-level declarations. |
| 175 | Map literals | When the expected type is `Map<string, V>`, an object literal with identifier or string keys builds that map (`env: { "LANG": "C" }`). |
| 176 | WasmGC | WasmGC remains the defined model for `wasm32-browser`. Until ppl's own WasmGC backend exists (P8.7), the target uses linear memory with the ppl GC; §9.2 notes this. |
| 177 | Iterable protocol | `interface Iterable<T> { iterator(): Iterator<T> }`: a plain method, usually a generator (`public *iterator()`). No symbols or computed member names. |
| 178 | Options objects | APIs that mirror a Web standard take the standard's options object as a struct built from an object literal (`fetch(url, { method: "POST", headers })`, `postMessage(msg, { transfer: [buf] })`), so ported code keeps its shape. ppl's own APIs (`Command`, `parallelMap`, `Arena`, …) use named arguments. |
| 179 | Number printing | `print`/`println` and templates show floats like JavaScript's `Number.prototype.toString`: the shortest digits that round-trip (at `f32` precision for `f32`), ties to the even digit, fixed notation for 1e-7 ≤ \|x\| < 1e21 and exponential (`1e+21`) otherwise, `-0` as `0`, `NaN`, `Infinity`. The interpreter and the C runtime implement the same algorithm and are cross-checked on thousands of values. |
| 180 | Panic report | A panic prints `panic: <message>` and `  at <file>:<line>:<col>` to stderr (after flushing stdout) and exits with code 101. Backends share the message texts (`ppl_mir::mir::panics`). |
| 181 | Format spec details | Filled in §1.4: compile-time checking against the value's type; sign-and-magnitude for `x`/`o`/`b`; `#` prefixes `0x`/`0o`/`0b` (lowercase prefix with `X`); Rust-style `e` (`1.2345e3`); shortest digits for `e`/`f`/`%` without precision; precision rounds half to even (Python/Rust, unlike JS `toFixed`), truncates strings, is rejected on integers; signs and `0` only for numbers; numbers right-aligned, other values left-aligned; `?` quotes and escapes strings and chars. |
| 182 | `const` structs | `const` only fixes the binding, also for structs (value types): fields of a `const` struct can be assigned and mutating methods called, like TS objects. Methods take `this` by reference. (Asked during P3.4; the alternative was Swift-style full immutability.) |
| 183 | Arrays, tuples, ranges (provisional, made in P3.5 without the author; please review) | `xs.pop()` returns `T?` (as TS). An array literal takes its kind from the expected type (tuple, `[T; N]`, `T[]`); without one it's a `T[]` whose element type is that of the first element that isn't an untyped literal (all literals: `int`, or `number` if any is a float); `[]` needs an expected type (E0315). Growable arrays are references and have no `==` (identity vs contents is undecided); tuples, fixed arrays and ranges compare element-wise. Fixed lengths and `[v; n]` counts are integer literals until comptime. `for-of` works over ranges, `T[]`, `[T; N]` and strings (by `char`), re-reading an array's length every iteration. `.step(n)` panics unless `n > 0`, and iteration stops instead of overflowing past the type's maximum. Ranges have `.start` and `.end`. `${v:?}` shows `[1, 2]`, `0..10`, `(0..10).step(2)`. |
| 184 | Null details (provisional, P3.6) | `T?` is a nullable pointer for reference types (`string`, arrays, later classes) and `{present, value}` for value types. Narrowing applies to variables (not fields): `x != null` / `x == null` in `if`, `while`, `&&`, `\|\|`, `!`, and early exits (`if (x == null) return` narrows the rest of the block). Assigning a variable ends its narrowing, and a loop first forgets narrowings of the variables it assigns. `a?.b.c()` short-circuits the whole chain. `print`/templates show a null as `null`. `x!` on null panics with "non-null assertion on a null value". `a ?? b` on a non-nullable `a` is allowed (`b` must still fit). |
| 185 | Enums and match details (provisional, P3.7) | An identifier pattern that names a variant of *any* enum is a variant pattern (`Empty`), otherwise a binding; qualified `Shape.Empty` always works. Variant patterns may be positional (`Circle(r)`, all fields) or named (`Circle { r }`, unnamed fields are wildcards); a bare `Circle` matches any `Circle`. Patterns other than `null`, `_` and bindings look inside a `T?` (so `Red` matches a non-null `Color?`). Non-exhaustive matches are E0317 and list up to 8 missing cases (`Rect(_, _)`, `false`, `null`, `P { y: false, .. }`, `_`); guarded arms don't count. `${v:?}` shows `Color.Red` and `Shape.Circle(r: 1)`. Enums compare variant and payload with `==`. An enum can't contain itself by value (E0210). Enum methods come later. |
| 186 | Advanced patterns (provisional, P3.8) | Or-alternatives must bind the same names (E0201 otherwise). Range patterns are exclusive over integers and chars; integer ranges never make a match exhaustive (no range splitting yet). Array patterns: `[a, b]` on tuples and fixed arrays needs the exact length and no rest; on `T[]`, `[first, ...rest]` / `[...init, last]` / `[a, ...]` bind a *copy* of the rest. Until unions (P3.12) a type test `s: string` holds when the value's type is `string`, or checks non-null on `string?`; anything else is E0318. `is` bindings are visible in the `if`/`while` body when the `is` sits in the condition directly or under `&&`; elsewhere they're local to the test. `x is Pattern` narrows a `T?` variable like `!= null`. A `switch` used as a statement needn't be exhaustive; one whose value is used must be. A bare type name in a pattern (`x is string`) is a type test. |
| 187 | Classes I details (provisional, P3.9) | Objects are heap records `{header, fields…}` (header reserved for vtables/class info). Unannotated instance fields need a literal initializer (`count = 0` → `int`); nullable fields start as `null`. Construction order: parameter properties, then field initializers in declaration order, then the constructor body. Static fields are globals initialized (with `static { }` blocks, in declaration order) **before any top-level code runs**, not at the class's position. Inside a class, static members are in scope by plain name. Constructors are public; `protected` behaves like `private` until inheritance. Definite assignment: every field must be assigned on every path to the end of the constructor and at every `return` (if/else and `match` arms intersect; loop bodies don't count) — E0320. Private access outside the class is E0319. `${obj:?}` shows `<ClassName>` (object graphs can be cyclic). |
| 188 | Inheritance details (provisional, P3.10) | Constructor overloads must differ in parameter count; `new`/`super(…)`/`this(…)` pick by argument count. `super(…)`/`this(…)` must be the first statement (E0326); without one, a subclass constructor calls the base's zero-argument constructor. A class without constructors gets one per base constructor (forwarding the arguments), or a zero-argument one at the root. Order in a subclass: `super(…)`, then its own parameter properties and field initializers, then the body; a `this(…)` delegate skips them (the delegate runs them). An override must keep the exact parameter and return types (E0325). `final` methods can't be overridden; `final class` can't be extended. Abstract methods compile to stubs that panic; concrete classes must override them all (E0323). `protected` members are visible in subclasses. Type tests (`d: Dog`) and class patterns downcast at runtime (`instanceof`), never count toward exhaustiveness, and a non-nullable type test also excludes `null`. Objects point at per-class metadata `{base, vtable…}`. |
| 189 | Interface implementation (provisional, P3.11) | An interface value is a fat pointer `{data, itable}`; `data` is the object, or a heap copy of a struct (so a struct's mutating methods called through the interface change the box, not the original). Every itable entry takes the interface value; implementer methods go through generated adapters (dispatching virtually when the method is `virtual`), missing ones use the interface's default. An interface's methods are its parents' (first parent first) then its own, so converting to the *first* parent (transitively) is free; converting to another parent isn't supported yet. A type gets defaults from all its interfaces; two different defaults for one method must be overridden (E0328); missing methods are E0327 (abstract classes may leave them); implementing methods of classes must be `public`. Enums can't implement interfaces until enum methods exist. |
| 190 | Unions and literal types (provisional, P3.12) | Literal types (`"north"`, `42`, `true`) and unions of literals of one base type are that base type at runtime; a `string` variable doesn't convert to `Dir`, a matching literal does. Other unions are tagged (an anonymous enum, member `i` in variant `i`); `T \| null` is `T?`, nested unions flatten, `"a" \| string` is `string`. `typeof x` is the ppl name of the runtime type (`"string"`, `"i64"`, `"f64"`, `"bool"`, a class name…; `"null"` for a null `T?`), not JS's `"number"`/`"object"`; `typeof x == "…"` narrows a union variable (and the `else` too when one member remains). Discriminated object unions need a field with a literal type in every member (the first such field); they become an anonymous enum whose variants are named by the discriminant value, built from object literals and narrowed by `e.kind == "…"`. Non-generic `type` aliases are implemented (resolved lazily, no recursion). Patterns on unions pick a member (type tests, literals, variants, struct and array patterns) and union members count for exhaustiveness. |
| 191 | Generic functions (provisional, P3.13) | A generic body is type-checked once against its bounds (`T extends …` and `where T: …`, joined with `&`); a type parameter supports only what its bounds provide. `Comparable<T>` (`compareTo`, `<`, `==`…) and `Equatable<T>` (`==`, `equals`) are built-in bounds that numbers, chars and strings meet (`Equatable` also everything with `==`); user types meet interface bounds by `implements`. Inference unifies parameter types with argument types: typed arguments first, then literals, then `[]`/`null`/objects, so `first([], 42)` gives `T = int`; uninferable parameters are E0329, unmet bounds E0330. Each instantiation is a separate MIR function named `f<i64>`; the generic itself lowers to an unreachable placeholder. Generic methods, types and default type parameters come in P3.14. |
