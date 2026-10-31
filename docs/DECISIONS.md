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
| 87 | Throwables | Only subclasses of the built-in `Error` class. |
| 88 | try expression | `try … catch …` is an expression, e.g. `const n = try parse(s) catch (e: ParseError) 0`. |
| 89 | Error unions | `throws A \| B`; typed `catch` clauses narrow the union. No `try?`/`try!` sugar for now. |
| 90 | Stack traces | Always captured on throw and on panic. |
| 91 | Throws inference | *(Proposed, awaiting confirmation)* Private functions and lambdas may omit `throws` (inferred); exported functions must declare it. |

## Spec format
| # | Topic | Decision |
|---|-------|----------|
| 57 | Docs | Markdown in `docs/`: `PLAN.md`, `DECISIONS.md`, and `spec/NN-chapter.md`. |
