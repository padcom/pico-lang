# Progress

> **Starting a new session?** CLAUDE.md already tells the agent what to do, so *"continue"* is
> enough. Or say it explicitly: *"Do the next task from PROGRESS.md."*

## Next task
**P8.2: Linux gnu and musl** (see [PLAN.md](PLAN.md#p82-linux-gnu-and-musl)). P8.1 is done (decision 244): `ppl build --target`, lld linked into ppl (`crates/ppl-lld`), the runtime compiled per target by clang (`crates/ppl-driver/src/link.rs`), glibc still from the system (`/usr/lib/<multiarch>` or the Debian cross sysroot `/usr/x86_64-linux-gnu`). P8.2 replaces that with generated glibc 2.31 stubs (and headers old enough: with glibc 2.41's headers the runtime references `__isoc23_strtol`, a 2.38 symbol), enforces the `.2.xx` suffix, and adds musl (static; ask before installing musl sources/packages). epoll is already in the event loop since P8.0.

Notes:
- Pipeline: `ppl_driver::check_source` runs parse → resolve (HIR) → typecheck (`Types`) → MIR;
  `run_interp` and `build_native` run it. New features touch: resolver (`ppl-sema/src/resolve.rs`,
  replace an E0900 "not implemented"), typeck, MIR lowering, the interpreter **and** codegen (+ C
  runtime when needed). Golden fixtures run interpreted and native; they must agree.
- Value representation so far: primitives in registers/allocas, strings as `PplStr*` (see
  `runtime/include/ppl_rt.h`). Structs (P3.4) are the first aggregate: decide their MIR/LLVM
  layout (inline values, field places) and make `Place` support fields.
- Integer literals without context default to `int`; there are no inference variables, so
  `const g = if (c) 1 else 2` is `int` even if later returned as `i32` (by design, §1.3).
- Commit only through a `scripts/test.sh` exit-status check. When editing files with scripts,
  assert that anchors exist and slices are non-empty (an empty-slice replace once corrupted
  `ppl_rt.c`; it was restored from git).

## Current state
| Area | State |
|------|-------|
| Spec | ✅ 16 chapters, 229 decisions, no open questions |
| Compiler | **M1 reached**: parse → resolve → typecheck → MIR → interpreter / LLVM native (primitives, functions, loops, panics) |
| Toolchain | Rust 1.99.0 (rust-toolchain.toml), LLVM 23 (Homebrew `llvm@23` 23.1.3 on macOS, apt.llvm.org 23.1.5 on Linux), inkwell 0.11 `llvm23-1` |
| Last green `scripts/test.sh` | 2026-11-02 (P8.0, Linux arm64) |
| Host | Linux arm64 VM from P8.0 on (macOS arm64 before); CI: macOS + Linux arm64 |

## Session protocol
1. Read this file, then the task in PLAN.md and its **Spec** sections. Nothing else unless you need it.
2. Implement it. Add golden tests (`tests/run`, `tests/fail`) or unit tests.
3. Run `scripts/test.sh`. Once it's green, commit as `P<x.y>: <summary>`.
4. Update this file: tick the box, add a log line, set **Next task**, and note any new
   decisions or known issues. Commit again (or amend into the same commit).
5. If a task is too large, split it in PLAN.md (`P3.7a`, `P3.7b`) and record that here.

## Known issues / notes for the next session
- **TLS (P7.5b, decision 243)**: `runtime/src/ppl_tls.c` (sessions keyed by fd; `ppl_tls_start/read/write/close_notify/free`), native hooks in `ppl_io.c` (`write_all`, jobs 20/22 reads, job 36 handshake on a solo thread, 120 close_notify, 121 free), interpreter `TlsIo` and job 36 in `io.rs`. ppl: `Conn.__startTls` (`<core>/net.ppl`), https in `__fetchOnce`, `ServeOptions.cert/key` (`stdlib/http.ppl`), `wss:` in `<core>/websocket.ppl`, `RequestInit.caCerts`, `WebSocketOptions`. Test CA and certificate (valid until 2126) in `tests/run/http/data/` (read with `comptime.readFile`). Real sites work with the system roots (checked by hand against example.com and google.com). Not yet: client certificates, ALPN, Keychain/Windows stores, timeouts (a silent peer stalls a handshake), Windows.
- **Crypto (P7.8, decision 242)**: `third_party/mbedtls` (3.6.7, unmodified; `README.ppl.md`), `runtime/include/ppl_mbedtls_config.h`, `runtime/src/ppl_crypto.c` + `ppl_crypto.h` (`ppl_crypto_op`: op table in the file's header comment), built by `crates/ppl-runtime/build.rs` (`libppl_mbedtls.a`, `libppl_crypto.a`; `ppl_runtime::crypto_libs()` for the native link; Cargo links them into the compiler for the interpreter, `ppl-interp` depends on `ppl-runtime`), `__io` 140 (`ppl_io.c`, `io.rs` `crypto_op` FFI). ppl: `<core>/crypto.ppl` (`CryptoError`, `__cryptoOp`, `CryptoKey`, `Algorithm`, `SubtleCrypto`, the `crypto` global), `stdlib/crypto.ppl`. Inputs are packed byte by byte in ppl (slow for megabytes: a bulk buffer copy op would help). A struct method's closure keeps a *reference* to `this`, so an async method on a short-lived struct temporary could outlive it (the `crypto` global is fine). Not yet: Ed25519, RSA, key formats other than raw (spki/pkcs8/jwk), AES-CBC/CTR, streaming hashes, `wrapKey`, `deriveKey`.
- **Testing (P7.10, decision 241)**: `stdlib/test.ppl` (registry `__Run`, runner `__runAll`, matchers `Expect`/`IntExpect`/`NumberExpect`/`StringExpect`/`BoolExpect`/`ArrayExpect`/`FnExpect`/`PromiseExpect`, benches), CLI `ppl test`/`ppl bench` (`TestRun` in `crates/ppl-cli/src/main.rs`), builtins `__catch_panic`/`__panic_taken` (`Builtin::CatchPanic` lowered to a `Guard` region in `lower.rs`; `ppl_panic_taken` in `ppl_rt.c`). Stdlib tests: `stdlib/tests/*.test.ppl`, run by `crates/ppl-cli/tests/stdlib_tests.rs` (native and interpreted). Golden: run/test/runner. The old golden fixtures (`tests/run/**`) stay: they also test the compiler; only module-level stdlib checks moved. Not yet: coverage, async hooks, async lambdas as tests, `test.each`, snapshot matchers, mocks (`vi.fn`), `toThrow(Class)`, identity `toBe` for classes, per-test timeouts, a panic after an `await` in an async test (it ends the file: no catching across the event loop), worker-based isolation (processes instead), `--watch` re-runs everything (polling).
- **Formattable (P7.9c, decision 240)**: core `stdlib/core/fmt.ppl` (the interface), typeck `formattable_method` → `Types.formattables`, lowering `format_call` (virtual for classes). `stdlib/fmt.ppl` (`FormatSpec`, `formatInt/Number/String/Bool`, `pad`, `groupDigits`; precision switches are generated text: 0–20). `DateTime`, `Vec2/3/4`, `Quat` implement it (`std/math` imports `std/fmt`). Not yet: `Formattable` in `print(x)`/`println(x)` of a bare value, string `+`, `toString()`; generic `T extends Formattable` parameters in templates; `Mat4`/`BigInt` specs.
- **Collections (P7.9b, decision 239)**: `stdlib/collections.ppl` (`Deque`, `PriorityQueue`, `SortedMap`, `BitSet`, helpers `__insertAt`/`__removeAt`). Arrays have no `splice` yet. `string` satisfies `Comparable` but `s.compareTo(t)` isn't callable (E0309): comparators use `<`/`>`. Not yet: a balanced-tree `SortedMap` (inserts are O(n)), `SortedSet`, `MultiMap`, `LinkedList`, `LruCache`.
- **Math (P7.9a, decision 238)**: `stdlib/core/math.ppl` (`Math`, `__Xoshiro128`), `stdlib/math.ppl` (`Vec2/3/4`, `Quat`, `Mat4`, `Random`, `BigInt`, `clamp`/`lerp`/`degToRad`/`radToDeg`). Builtin `__math(op, x, y)` (`Builtin::Math`; `ppl_math` in `ppl_rt.c`, `math_op` in `ppl-interp/src/lib.rs`: keep the op tables in step). Structs got `static` methods/getters (resolver `struct_members`, `names_class`). Not yet: method overloading (so `Math.abs` is `number`-only; spec §5.4 allows overloads but only free functions have them), wrapping/saturating integer methods (spec §2), `bigint` literals, BigInt bit operations/shifts, Karatsuba (multiplication is O(n²), division O(n² log BASE)), f32 vector types, `Mat3`. A struct's `toString()` isn't its string form yet (P7.9c `Formattable`): tests call `v.toString()`. A class `toString` with parameters (even defaulted) isn't used by `${x}`.
- **Time (P7.7, decision 237)**: `stdlib/core/time.ppl` (civil-date math `__daysFromCivil`/`__civilFromDays`, `__PosixTz`, `__Zone` from TZif via `__Zone.fromTzif`, `__zoneNamed`/`__localZone`, ISO parser `__parseIso`, `Date`, `performance`), `stdlib/time.ppl` (`Duration`, `Instant`, `TimeZone`, `DateTime`, `DurationParts`, `DateTimeFields`). `__io` 133 clocks (monotonic is since program start: `ppl_start_ns` in `ppl_rt.c`'s `main`, `Shared.start` in the interpreter), 134 read a file now, 135 readlink. Typing: literals into `U?` unions, object literals into a union's single struct member. Not yet: plain date/time types (`PlainDate`…), calendar `Duration`s and `since` with largest units (`P1Y2M`), `round`, `Intl`/locale formats, `Date.toLocale*String`, a bundled tz database (WASI/Windows), leap seconds, nanosecond range beyond 1678–2262. Zones are cached per id; `TZ` is read again at every local-time use (each `Date` getter calls `getenv`: slow in hot loops).
- **Regex (P7.6, decision 236)**: engine in `stdlib/core/regex.ppl` (`__RParser` → `__RNode` tree → `__RProgram` instructions → `__RMachine` backtracking VM; `RegExp`, `RegExpMatch`, `SyntaxError`), string methods in `stdlib/core/string.ppl` (`replace`/`replaceAll`/`split` take `string | RegExp`; `match`, `matchAll`, `search`), `std/regex` re-exports. Compile-time check: `ppl-sema/src/regex.rs` (`validate`, unit tests; keep it in step with `__RParser`), E0010, literals → `RegExp.__literal`. Typing: closures get parameter types from a union's single function member; native extension calls pass default flags (`split(sep)` without `limit`). Not yet: Unicode property escapes (`\p{L}`), `v`-flag set operations, `d`-flag `indices`, full case folding (only single-code-point lower/upper), `RegExp` from a `RegExp`, `Symbol.replace`-style protocols, a step limit against catastrophic backtracking, literal caching (each evaluation compiles the pattern again: hoist literals out of hot loops). A function type in a `match` pattern needs an alias (`f: ((m) => T) => …` parses `=>` as part of the type).
- **WebSocket (P7.5c, decision 235)**: `stdlib/core/websocket.ppl` (`__sha1`, `__wsAccept`, `__randomBytes` via `__io` 132, `Event`/`MessageEvent`/`CloseEvent`, `WebSocket`), `upgradeWebSocket` in `stdlib/http.ppl` (serve hands the conn over when `Response.__upgrade` is set). `Base64`/`Hex` moved to `<core>/encoding.ppl`. Resolver: a program's own type alias now shadows a core type of the same name (`type Event = …`). Core statics are getters (static fields run at every program's start). Not yet: wss (P7.5b), `EventTarget`/`addEventListener`, `binaryType`/`Blob`, `bufferedAmount`, permessage-deflate, max message size, UTF-8 validation of text frames (bad bytes become U+FFFD instead of failing the connection).
- **HTTP (P7.5a, decision 234)**: `stdlib/core/url.ppl` (URL, URLSearchParams), `stdlib/core/fetch.ppl` (Headers, Request, Response, `__HttpReader`/`__parseHead`/`__writeMessage`, `fetch`), `stdlib/http.ppl` (`serve`, `HttpServer`). Streams and std/net's implementation now live in core (`<core>/streams.ppl`, `<core>/net.ppl`), re-exported by `std/streams`/`std/net`. New: `String.lastIndexOf`; array literals typed through a union's single array member; function values covariant in object-reference returns (lower.rs keeps the pointer). Not yet: https (P7.5b), WebSocket (P7.5c), streaming bodies (read whole first), client keep-alive/pooling, request timeouts/`AbortSignal`, `Request.clone`, URL setters/IDNA, `Response.json` of object literals (needs a declared type), multipart/form-data. Known bug: `match` on a union that includes `null` (`string | Uint8Array | null`) says the type tests can't succeed (narrow with `if (x != null)` first). Byte handling is per-byte `__buf_op` calls (slow for big bodies).
- **Streams/encoding (P7.4, decision 233)**: `stdlib/streams.ppl` (Readable/Writable/TransformStream, controllers, reader/writer, `pipeTo`/`pipeThrough`, `ReadableStream.from`, `textEncoderStream()`/`textDecoderStream()`), `stdlib/core/text.ppl` (TextEncoder/TextDecoder via `__io` 130/131 in `ppl_io.c`'s `decode_utf8` and `io.rs`), `stdlib/encoding.ppl` (base64, hex), `Uint8Array.subarray/slice/from`, core `TypeError`/`RangeError`. `Conn`, process `Reader`/`Writer` have `readable`/`writable`. Not yet: `tee`, byte streams/BYOB, `size()` strategies, `AbortSignal` for pipes, async iteration of a stream with a reader's lock (for-await just reads), `ReadableStream.from` of async iterables, encodings other than UTF-8, utf-16/csv/toml in std/encoding. Encoding ops live in the POSIX part of `ppl_io.c` (the Windows stub returns -1: fix in P8.3). Strings are built by concatenation (base64/hex O(n²)).
- **std/net (P7.3, decision 232)**: `stdlib/net.ppl` (`listen`/`Listener`, `connect`/`Conn`, `bind`/`UdpSocket`, `resolve`, `NetError`); native section "std/net" in `runtime/src/ppl_io.c` (`net_run`: jobs 30–35 on solo threads; TCP reads/writes reuse pipe jobs 20–22; 120 shutdown, 121 close), interpreter `net_job` in `crates/ppl-interp/src/io.rs` (`IoState.net`, sockets shared with job threads; `process_job` finds TCP streams there). Closing wakes waiting jobs (shutdown, or a self-connect / self-datagram) and the last job closes the fd. Not yet: `timeouts/keepalive/nodelay options, IPv6-only tests, Windows (P8.3), WASI sockets (P8.5), accepting from several `accept()` calls at once.
- **std/process (P7.2, decision 231)**: `stdlib/process.ppl`; native in `runtime/src/ppl_io.c` (section "std/process": `spawn_child` with `posix_spawnp`, `child_wait`, `process_op`; blocking jobs 20–23 run on a thread each via `run_solo`), `ppl_argc/ppl_argv` saved in `ppl_rt.c`'s `main`; interpreter in `crates/ppl-interp/src/io.rs` (`process_job` threads push to `IoState.finished` and wake the loop; `process_op`; its own env copy), `Panic.exit` for `exit(code)` (no `finally`; harness/CLI turn it into the exit code), `ppl run --interp file -- args` now passes args. Constructors take defaults/named/rest (`typeck::pick_ctor`). Not yet: **Map literals** (decision 175, `env: { "LANG": "C" }` is E0900/E0313, so the spec/TOUR examples don't compile yet; tests build a `Map`), Windows (`CreateProcessW`, P8.3), `TextEncoder`/`TextDecoder` and Web Streams for the pipes (P7.4), `exit` from a worker (interpreter: only ends that worker), PATH lookup with a changed `PATH` in `env` (native looks in ours, the interpreter in the child's), a child's inherited stdout in the interpreter bypasses the captured output (golden tests pipe everything they print), fds of pipes can leak into children spawned at the same moment from another worker (no `pipe2` on macOS).
- **std/fs (P7.1a, decision 229)**: `stdlib/fs.ppl`, `stdlib/path.ppl`, `runtime/src/ppl_io.c` (4 pool threads, per-thread completion queues waking the loop), `crates/ppl-interp/src/io.rs` (same ops with std::fs, run at submit, delivered on the next loop turn; `Shared.cwd` is a virtual working directory), loop polling in `stdlib/core/event.ppl` (`__ioSubmit`). The golden harness gives every run a fresh temp working directory. POSIX only: Windows `ppl_io` is a stub (P8.3). `watch` (P7.1b, decision 230): watchers in `ppl_io.c` (one thread each, snapshot diffs, kqueue/inotify wake-ups; the inotify code runs in run/fs/watch on Linux since P8.0) and `io.rs` (polled from op 100; `__loop_wait` is capped at 5 ms while a watcher waits); not yet: Windows, a watcher that's never closed keeps its thread (it ends with the process), big trees rescan fully each time (kqueue opens one descriptor per entry up to 256 entries, then directories only). Not yet: symlink creation/readlink, permissions/chmod, copy, streaming `readText` for big files, `TextEncoder`/`TextDecoder` (so `File` has no text methods), async closures (tests use helper functions).
- **JSON (P6.8, decision 228)**: `stdlib/core/json.ppl`, `Synth::JsonRead/JsonWrite` (`json_read`, `json_write` in `ppl-mir/src/lower.rs`), `typeck::json_ok`. Not yet: `Map<string, V>`, enums, pretty printing (`stringify(v, indent)`), cycle detection in `stringify`, class field initializers as defaults (a missing field with an initializer is an error unless nullable), `res.json<T>()` (P7.x http). The reader indexes `text[i]` (code points with a cached index) and builds strings by concatenation: fine for configs, slow for megabytes.
- **Runtime reflection (P6.7, decision 227)**: `stdlib/core/reflect.ppl` + `stdlib/reflect.ppl`; resolver `reflect_call`; lowering `Synth::Info/Get/Set/FieldShow` (`info_body`, `describe`, `access_body` in `ppl-mir/src/lower.rs`). Not yet: `unknown` (so no untyped `get`), field/method decorators (not implemented in §11.2 either), method signatures in `MethodInfo` (name only), caching of `TypeInfo` objects (each `typeOf` call builds new ones), subclass dispatch for generic classes. Panics from accessors point at the class declaration. JSON (P6.8) is compile-time generated and shouldn't need this metadata.
- **Class string form (decision 226, done before P6.7)**: `Cx::show_fn` in `ppl-mir/src/lower.rs` requests a per-class-type "show function" (`Synth::Show`, placeholder ids from `SYNTH_BASE`, built in the instance loop and appended after the worker entries); `FnBuilder::show_body` builds it, `show_value(…, quote)` formats fields. Not yet: `${obj:?}` and arrays/structs holding objects still show `<Name>` (the backends' debug formatters don't call show functions), so `${[p]:?}` is `[<P>]`; `Map`/`Set` show their internal fields; generic classes don't dispatch to subclasses (static class only); a panic caught inside a `toString` leaves its object marked as being shown (it then shows as `Name (…)`).
- **Comptime (P6.2a, decision 220)**: `Rvalue::Comptime(thunk, limit)` is replaced in `ppl_interp::comptime::run` (called from `check_source_with`, then folding runs again). A local `comptime const` used by later comptime code reruns its thunk (`Found::Thunk`; the thunk is type-checked once, `checked_thunks`). Comptime parameters (P6.2b, decision 221): `TypeParam.comptime`/`value_param`/`is_len`, `ExprKind::ConstParam`/`RepeatN`, `Ty::FixedN`; constants are `Ty::Lits` type arguments. Comptime parameters on generic *types* (`struct Vec<comptime N>`) aren't supported yet. Type reflection (P6.3, decision 222): `ExprKind::TypeInfo`/`TypeValue`, lowered to constants in `Lower::type_info`; `comptime T: type` value parameters are dropped from the runtime parameters via `arg_slots` (so they don't combine with named arguments or defaults yet). Decorators on bindings other than `@comptimeLimit` are E0900, and only top-level bindings carry decorators (`Modules.binding_decorators`, keyed by statement span). The package root for `comptime.readFile` is the entry file's directory until packages exist (P9?). A 10M-step loop takes ~1.3 s in a debug build.
- **Decorators (P6.4, decision 223)**: `ppl-driver/src/decorators.rs` (`expand`, called by `check_source_with` before resolution), `stdlib/meta.ppl`, `Builtin::MetaEmit`. `@derive` is `stdlib/core/derive.ppl` (P6.6, decision 225); the meta classes are core (`__ClassDecl`, …) re-exported by `std/meta`, so every program's prelude has them (they show in LLVM IR snapshots as class infos). Templates are code strings and `ast` (P6.5, decision 224) is a class of static methods returning code text. Not yet: decorators on methods, fields, parameters and enums (E0900); decorator arguments that use the using module's names (they're evaluated in the decorator's module); re-exported decorators (`export { memoize } from …`). Generated code with mistakes is reported in files like `<body made by @memoize>`. Native code calling a comptime-only builtin (`__meta_emit` outside comptime) hits `unreachable!` in codegen (the interpreter panics properly).
- **Constant folding (P6.1, decision 219)**: runs at the end of `ppl_mir::lower`. Not folded: strings (`"a" + "b"`), `ToStr` of constants, aggregates, `InBounds`/`AddOverflows`; no interprocedural folding. A comptime interpreter (P6.2) can reuse `ppl_mir::eval`.
- **GC (P4.x)**: `runtime/src/ppl_gc.c` and `crates/ppl-codegen/src/gc.rs` (decisions 206–209).
  Generational (non-moving nursery + incremental old generation, Dijkstra barrier, cards for
  large objects, lazy sweeping). **Pause benchmark** `bench/gc_pause.ppl` (release build, macOS
  arm64, 200k-entity world updated 4000×/frame for 600 frames, 1.36 GB allocated, ~141 MiB heap):
  frame-loop **max pause 0.34–0.77 ms** over 5 runs, typical nursery pause ~0.2–0.4 ms, GC time
  ~2.5 ms/frame. The setup phase alone has one ~0.7–1 ms nursery pause (scanning a freshly grown
  1.6 MB array). `PPL_GC_TRACE=1` prints each pause.
  Weak refs (P4.4): `@weak` fields (core library only) → `PplLayout.nweak`; every nursery
  collection walks the whole list of objects with weak words (fine for now; a generational
  split of that list would help programs with very many weak refs). No ephemerons in `WeakMap`.
  Finalizers run at the end of the entry's code (`__runFinalizers`), to move onto the event
  loop in P5.1.
- **Unsafe (P4.5, decision 210)**: `ptr<T>` = `Ty::Ref`. An `unsafe function` used as a value
  (`const f = poke`) can then be called without `unsafe` (not checked yet). FFI (P? interop)
  will need `ptr<void>`, null pointers and casts between pointer types.
- **Workers (P5.5, decision 217)**: check threading changes with ThreadSanitizer:
  `ppl build --emit llvm-ir x.ppl > x.ll` then
  `/opt/homebrew/opt/llvm@23/bin/clang -g -O1 -fsanitize=thread -Iruntime/include x.ll runtime/src/*.c`.
  Every mutable C static must be `_Thread_local` (except the worker table). Expected TSan
  report: `ppl_worker_op` terminate writing a worker's `ppl_gc_requested` (intentional).
  Received `ArrayBuffer` copies leak their memory (not registered for finalization). Thread-local
  access costs ~12% of GC time on `bench/gc_pause.ppl` (max pause unchanged).
- **Event loop (P5.1, decision 213)**: `stdlib/core/event.ppl` + `runtime/src/ppl_loop.c`.
  Timer tests use gaps of ≥ 10 ms (they run real time, also under `PPL_GC_STRESS`).
- **async (P5.2, decision 214)**: desugaring in resolve.rs (`desugar_async`, `lower_await`,
  `build` AST helpers); `Promise` in `stdlib/core/promise.ppl`. Node 24 is installed
  (`node`), handy for checking orderings. Not yet: async lambdas (`async () => …`), `await`
  of non-promises, promise `catch`/`finally` methods, throwables inside async code.
- **Arenas (P4.6, decision 212)**: Arena objects are scanned at every nursery collection (cost grows with
  open arena size). Exceptions leaving an arena are deep-copied out; other escapes panic
  natively only. Arenas inside generators and `async` code are untested.
- `ppl run` builds natively; `ppl run --interp` uses the interpreter. `// native-only:` in a
  run fixture skips the interpreter.
  Collection only at safepoints; layouts mirror codegen's `size_align` (wasm32's 4-byte
  pointers will need both updated in P8.5). The interpreter has no collector.
  `PPL_GC_STRESS=1` (also run by the golden harness) collects at every safepoint.
- **Decisions 197–205 (P3.19–P3.26) are provisional** too: exceptions via error-return lowering,
  throws inference/polymorphism, panic unwinding with setjmp guards, the core library in ppl
  (`stdlib/core`), `Iterator<T>` as a class (no generic interfaces yet), and module resolution.
- Modules: the resolver keys top-level names by module (`Names<T>`, `ModCtx` in resolve.rs); the
  driver's `modules.rs` loads files and strips import/export declarations. MIR `Loc`s have no
  file, so panics inside non-entry modules report `0:0` (and the entry's file name).
- Generic interfaces are the main missing piece for a real `Iterable<T>`/`Iterator<T>`.
- The core library lives in `stdlib/core/*.ppl` (`ppl_driver::PRELUDE`, `include_str!`). Built-in type methods are `__Array_x`/`__String_x` functions (`Program.extensions`, `Types.extensions`). `mir::live_functions` drops unreachable `library` functions in dumps and codegen. String building is O(n²) (concatenation); a string builder would help. `Map.keys()` etc. return arrays until P3.23 gives iterators.
- LLVM can't emit WasmGC, so the browser target starts on linear memory (PLAN P8.6 and P8.7).
- lld (P8.1): linked into ppl from `liblld-23-dev` (required on Linux; on macOS optional through `PPL_LLD_PREFIX`, and macOS builds still link with `cc` until P8.4). Builds link no compiler builtins: if a runtime change makes lld report an undefined `__…` helper (`__udivti3`, `__aarch64_ldadd…`), avoid it or bundle compiler-rt. The runtime cache is `$TMPDIR/ppl-cache/runtime/<triple>-<hash>`; on Linux, the `cc`-built `libppl_rt.a` is now only used by the interpreter and `ppl-runtime`'s C tests.
- Math on glibc (P8.0): Rust's `f64` methods don't all call the system libm (`cbrt` is Rust's own, correctly rounded), so `ppl_math` corrects glibc's `cbrt` (up to ~2 ulps off) to the nearest double. Other functions agreed in the golden tests; if one ever differs between backends on Linux, fix it the same way.
- Exceptions use error-return lowering (decision 197): `Rvalue::SetException/TakeException/
  ExceptionPending`, `Terminator::Uncaught`; `FnBuilder::terminate` adds the pending check after
  calls of throwing functions; the throws analysis is `ppl-sema/src/throws.rs`. The prelude
  (`ppl_driver::PRELUDE`) is a second source file; lowering gives its spans no location.
  The pending slot is a plain global: make it thread-local with threads (P5).
- **Decisions 183+ are provisional** (made during an unattended session): pop → `T?`,
  array-literal inference, no `==` on `T[]`, range step rules, debug format of arrays, null
  representation and narrowing rules. Ask the author to confirm.
- Narrowing is variables-only; `p.x != null` doesn't narrow `p.x` (fields can change through
  aliases). `unify_all` (return/break joins) doesn't make `T` + `null` into `T?` yet; `if/else`
  does. Closures (P3.15) must make captured-and-assigned variables non-narrowable.
- A method called on an array element (`xs[0].bump()`) gets a raw pointer into the array's
  storage natively; if the method grows the same array, the pointer dangles. Revisit with the GC.
- `==` on large fixed arrays is unrolled per element in codegen.
- Enum methods (`enum Token { … name(): string { … } }`) report E0900; `this` is struct-only
  (`Function.this: (LocalId, StructId)`), so enums can't implement interfaces yet either.
- Interfaces are StructDefs with `is_interface` (`Ty::Iface`); itables are built at the end
  of lowering (`Cx::build_itables`), with MIR-only adapter functions appended to `fns`.
- §3.5's `switch (cmd) { case "build" … case Run { file } … }` could now use a union
  (`string | Run`); `tests/run/patterns.ppl` still splits it. `unknown` doesn't exist yet.
- Generic types: `Ty::type_owner(sid)` (high bit set) owns a type's parameters; methods and
  constructors of generic types lower only as instances (placeholders otherwise); MIR
  `Rvalue::New/Aggregate` carry the full type; codegen caches LLVM types per instantiation
  (`struct_llty`). Missing: generic enums/interfaces (no `Option<T>`/user `Comparable<T>`).
- Closures: resolver keeps a stack of enclosing `FnState`s (`outer_fns`) and captures on
  lookup (`capture`, `capture_this`); `Binding.boxed` marks heap-celled variables (MIR local
  `Ty::Ref(T)`, accessed through `Place::deref`). Closures in generic code are instantiated
  with the enclosing instance's substitution (`Cx.cur_subst`). `get`/`set` are lexer keywords
  — they should probably become contextual (they block `const get = …`).
- `ppl run` caches executables in $TMPDIR/ppl-cache; the key now includes the compiler binary
  (size + mtime). Before that fix, a rebuilt compiler could run a stale executable — if a native
  result ever looks impossible, `ppl build -o` and run that directly.
- Monomorphization: `Cx.types` is a `Cow<Types>`; an instance is lowered with
  `Types::substituted(fn, args)` (a full copy — fine for now, quadratic-ish for big programs).
  Instances are MIR functions `program.fns.len() + i` (before itable adapters).
- Lowering erases `Ty::Lits/Union/Variant` (`Ty::erase`, `erase_program`) at the end; inside
  lowering, typeck types are unerased, MIR place types become erased only afterwards
  (`mir::project` erases defensively).
- Classes reuse `StructDef` (`is_class`), `Ty::Class(StructId)`; a class method's `this` is
  the object reference (not `Ty::Ref`). Ctors are named `<Class>.constructor`; typeck runs
  definite assignment on those. Subclasses copy the base's fields first (same indices), so
  base methods work on subclass objects; `declared_in` tracks where inherited members come
  from (visibility). Virtual dispatch: `Callee::Virtual { slot, decl }`.
- `match` lowers to sequential arm tests (no decision tree yet); unreachable arms aren't warned
  about (the usefulness checker could do it: `exhaust::Checker::useful` per arm).

## Checklist

### Phase 1: Foundation
- [x] P1.1 Toolchain setup
- [x] P1.2 Workspace skeleton
- [x] P1.3 Diagnostics
- [x] P1.4 Lexer: core tokens
- [x] P1.5 Lexer: templates, multiline strings, regex
- [x] P1.6 Statement-termination rule
- [x] P1.7 AST and pretty-printer
- [x] P1.8 Parser: expressions
- [x] P1.9 Parser: statements and blocks
- [x] P1.10 Parser: declarations and types
- [x] P1.11 Parser: patterns, match, switch, try
- [x] P1.12 Parser error recovery
- [x] P1.13 Golden-test harness

### Phase 2: First executable
- [x] P2.1 Name resolution
- [x] P2.2 Type checker: primitives and functions
- [x] P2.3 MIR design and lowering
- [x] P2.4 MIR interpreter
- [x] P2.5 C runtime v0
- [x] P2.6 LLVM codegen v0
- [x] P2.7 `ppl build` / `ppl run` end to end (**M1: hello world**)

### Phase 3: Core language
- [x] P3.1 Allocator v0 and string runtime
- [x] P3.2 char type and template literals
- [x] P3.3 Format specs
- [x] P3.4 Structs
- [x] P3.5 Arrays, fixed arrays, tuples, ranges
- [x] P3.6 Null and narrowing infrastructure
- [x] P3.7 Enums and basic match
- [x] P3.8 Advanced patterns and switch
- [x] P3.9 Classes I
- [x] P3.10 Classes II: inheritance
- [x] P3.11 Interfaces
- [x] P3.12 Unions and literal types
- [x] P3.13 Generics I: functions
- [x] P3.14 Generics II: types and where
- [x] P3.15 Closures
- [x] P3.16 Function features
- [x] P3.17 Operators and equality protocols
- [x] P3.18 Distinct types, aliases, immutability
- [x] P3.19 Exceptions I
- [x] P3.20 Exceptions II
- [x] P3.20b Lightweight throwables (split from P3.20)
- [x] P3.21 Panics and stack traces
- [x] P3.22 Core collections and Array API
- [x] P3.23 Iterators and generators
- [x] P3.24 Modules
- [x] P3.25 Destructuring and spread
- [x] P3.26 `using` and Disposable

### Phase 4: Memory management
- [x] P4.1 Shadow stack and type maps
- [x] P4.2 Mark-sweep GC
- [x] P4.3 Generational and incremental GC
- [x] P4.4 Weak references and finalization
- [x] P4.5 Unsafe and pointers
- [x] P4.6 Arenas

### Phase 5: Async and concurrency
- [x] P5.1 Event loop
- [x] P5.2 async/await
- [x] P5.3 Async generators and `await using`
- [x] P5.4 Cancellation
- [x] P5.5 Workers
- [x] P5.6 Transferables, inline workers, pools

### Phase 6: Comptime and metaprogramming
- [x] P6.1 Constant folding
- [x] P6.2a `comptime` evaluation
- [x] P6.2b `comptime` parameters
- [x] P6.3 Types as comptime values
- [x] P6.4 Decorators and std/meta
- [x] P6.5 Quasi-quote templates and the AST builder
- [x] P6.6 `@derive` in ppl
- [x] P6.7 Runtime reflection
- [x] P6.8 JSON

### Phase 7: Standard library
- [x] P7.1a std/fs and std/path (all but `watch`)
- [x] P7.1b fs.watch
- [x] P7.2 std/process
- [x] P7.3 std/net
- [x] P7.4 Web Streams and encoding
- [x] P7.5 fetch and std/http (split)
  - [x] P7.5a URL, fetch and serve over HTTP
  - [x] P7.5b TLS (https)
  - [x] P7.5c WebSocket
- [x] P7.6 Regex
- [x] P7.7 Time
- [x] P7.8 Crypto
- [x] P7.9 Math, collections, fmt (split)
  - [x] P7.9a Math and std/math
  - [x] P7.9b std/collections
  - [x] P7.9c std/fmt
- [x] P7.10 std/test and `ppl test`

### Phase 8: Targets
- [x] P8.0 Linux host bring-up
- [x] P8.1 Cross-compilation infrastructure
- [ ] P8.2 Linux gnu and musl
- [ ] P8.3 Windows (MinGW)
- [ ] P8.4 macOS cross builds
- [ ] P8.5 wasm32-wasi
- [ ] P8.6 wasm32-browser (linear memory)
- [ ] P8.7 WasmGC backend
- [ ] P8.8 JS imports from .d.ts
- [ ] P8.9 Platform files and conditionals

### Phase 9: C interop
- [ ] P9.1 extern "C"
- [ ] P9.2 C header import
- [ ] P9.3 Export to C

### Phase 10: Tooling and ecosystem
- [ ] P10.1 Formatter
- [ ] P10.2 Lint
- [ ] P10.3 Package manager
- [ ] P10.4 Registry
- [ ] P10.5 Incremental compilation
- [ ] P10.6 LSP and VS Code extension
- [ ] P10.7 REPL and JIT
- [ ] P10.8 Debugging
- [ ] P10.9 Documentation tooling
- [ ] P10.10 Release packaging

### Phase 11: Later
- [ ] P11.1 `--no-gc` subset + bare metal
- [ ] P11.2 riscv64
- [ ] P11.3 Android
- [ ] P11.4 iOS
- [ ] P11.5 Self-hosting investigation

## Log
| Date | Task | Notes |
|------|------|-------|
| 2026-10-31 | Spec | Design interview completed. Spec chapters 00–15 and DECISIONS.md #1–162 written. |
| 2026-10-31 | Plan | PLAN.md split into 103 session-sized tasks, and PROGRESS.md created. |
| 2026-10-31 | Setup | CLAUDE.md added (session start, auto-commit rules). |
| 2026-10-31 | P1.1 | Rust 1.99.0 + LLVM 23.1.3 were already installed (keg-only). Pinned toolchain, added scripts/env.sh and docs/DEVELOPMENT.md. Used LLVM 23 instead of 18 (inkwell 0.11 supports it); inkwell smoke test OK. |
| 2026-10-31 | Tour | docs/TOUR.md: an example of every feature. Open questions for the user: `Some(v)` in §3.5 (no Option type), undeclared `cache` in the §11.2 `@memoize` example, `env: { "LANG": "C" }` object literal (§13.3), option bags vs named args for Web APIs, the WasmGC wording in §9.2 vs PLAN. |
| 2026-10-31 | P1.2 | Cargo workspace (edition 2024, resolver 3) with 10 crates, `ppl` binary (clap) with `--version` + integration test, scripts/test.sh (fmt, clippy -D warnings, test). |
| 2026-10-31 | P1.3 | ppl-diag: SourceMap/FileId/Span, Diagnostic builder, error-code registry (decision 163), plain renderer matching §14.2 (snapshots). Fixed the spec example's column (4:20 → 4:19). |
| 2026-10-31 | P1.4 | Lexer (with P1.5): tokens, keywords (decision 164: `do` added, contextual `type`/`of`/`from`/`arena`), numbers with suffixes, strings/escapes, comments, newline tokens. |
| 2026-10-31 | P1.5 | Templates via a substitution stack, format-spec capture at a top-level `:` (ternary-aware), triple-quoted dedent, regex literals (decision 165). Committed together with P1.4. |
| 2026-10-31 | P1.6 | newline::filter with bracket stack; trailing/leading continuation sets (decision 166, provisional: generic closers and `?` end a line); table-driven tests for every §1.2 example. |
| 2026-10-31 | P1.7 | ast.rs covers the whole language (exprs incl. control flow as expressions, patterns, stmts, decls, members); dump.rs S-expression printer with width-based breaking; unit tests on hand-built ASTs. `ppl parse --dump` gets wired in P1.8. |
| 2026-10-31 | P1.8–P1.11 | Parser (one module, committed together): Pratt expressions, statements, declarations, types, patterns, match/switch/try. `ppl parse --dump`. 14 insta snapshot groups; `tests/spec_samples.rs` parses every ```ts block in docs/spec and docs/TOUR.md (2 spec blocks skipped with reasons). Decisions 167–172. |
| 2026-10-31 | P1.12 | Recovery: statement-level sync (no separator error after a failed statement, stops at statement keywords on a new line), bracket-depth-safe member/arm recovery, unclosed `(`/`[` end before a declaration keyword (newline filter), enum variant/method lookahead. Targeted messages: `var`, `for-in`, `===`, conditions without parens, trailing-operator line continuation. 6 multi-error fixtures in tests/fail/parse with `// error:` directives + snapshots. |
| 2026-10-31 | Review | Author answered all pending questions. Confirmed 166–168, 170, 172 (now in spec §1.2/§1.3/§2.8/§3.2/§3.3/§3.5); new decisions 173–178: no Option type, decorator hidden statics (`addStatic`), Map literals, WasmGC interim note, `Iterable.iterator()`, Web-standard options objects. Parser: parenthesized patterns (`case (s: string):`). Spec and TOUR examples updated. |
| 2026-10-31 | P1.13 | `ppl-tests`: discovery (multi-file dirs with main.ppl), directive parser (expect/exit/panic/error/at, malformed directives fail), LCS diff, pass/pending/fail report. `ppl-driver` got `check_source`/`check_file` and `Stage` (code ranges to stages); the harness marks checks for unbuilt stages as pending. Fixtures: run/{hello,fib,loops,overflow_panic}, fail/types/nominal. Broke two fixtures on purpose (wrong code + position; non-compiling run test), checked the report was readable, restored them. **Phase 1 complete.** |
| 2026-10-31 | P2.1 | ppl-sema: Ty, HIR (ExprIds, resolved Vars, desugared for/for-of-range/do-while), resolver: block scopes with TDZ-style use-before-declaration, order-independent top-level functions, entry globals, built-ins, const assignment, break/continue placement, did-you-mean suggestions. E02xx codes; E0900 'not implemented' makes golden fixtures pending. Driver sorts diagnostics by position. 6 fail fixtures in tests/fail/names. |
| 2026-10-31 | P2.2 | typeck.rs: bidirectional checking with literal typing from context (range-checked, incl. negated literals), lossless widening recorded as coercions, operator typing, if/blocks/loop-break values, never/void, if-without-else → T?, char inference from 1-char literals, `as` rules, on-demand return-type inference with recursion detection (E0306), built-in signatures. 6 fail fixtures in tests/fail/types. |
| 2026-10-31 | P2.3 | ppl-mir: CFG of basic blocks over typed locals; statements carry Locs (integer ops trap implicitly); terminators goto/branch/return/call/panic/unreachable; &&/|| and compound &&= lowered to control flow; dead code after divergence goes to unreachable blocks. Lowering reports E0900 for strings/templates/nullable values for now. `ppl build --emit mir`; snapshots in ppl-driver/tests/mir.rs (loops, if expressions, loop/break value). |
| 2026-10-31 | P2.4 | ppl-interp: Value/Frame interpreter over MIR; integer ops trap (overflow, div0, shift range, negative exponent, casts) with shared messages in ppl_mir::mir::panics; f32 computed at f32 precision; JS-style float printing (format_float, unit-tested); depth limit → stack overflow panic; runs on a 1 GiB-stack thread (driver::run_interp). `ppl run --interp`; panic report 'panic: msg\\n  at file:line:col', exit 101. Harness checks stdout/exit/panic. 10 new run fixtures. |
| 2026-10-31 | P2.5 | runtime/ (C11): ppl_rt.h + ppl_rt.c with print functions, JS-style number formatting, ppl_panic (format + exit 101), main → ppl_entry. ppl-runtime builds libppl_rt.a via cc (no link directives into the compiler), macOS min 13.0 (decision 180/179). Tests link C programs: panic format, and C vs interpreter float formatting on ~6000 values — found Rust's {:e} picks the odd digit on ties; switched the interpreter to the same shortest-correctly-rounded loop (ties to even, as ECMAScript). |
| 2026-10-31 | P2.6 | ppl-codegen (inkwell 0.11/LLVM 23): allocas per MIR local, overflow intrinsics → cold noreturn ppl_panic, div0/MIN÷-1/shift/cast checks (casts range-checked in i128; float→int with exact power-of-two bounds — same bug fixed in the interpreter, new fixture), integer ** via runtime ppl_ipow_s/u, strings as PplStr* (runtime header gained PplStr, ppl_print_string, ppl_panic_str), internal `ppl.<name>` functions + external ppl_entry, object emission for the host. `ppl build --emit llvm-ir`; IR snapshots in ppl-driver/tests/llvm_ir.rs. |
| 2026-10-31 | P2.7 | Driver: build_native (LLVM object + cc link with libppl_rt.a), content-hashed run cache in $TMPDIR/ppl-cache. CLI: `ppl build [-o] [--release] [--emit mir|llvm-ir]`, `ppl run [--interp] [--release] [-- args]`. Harness runs every tests/run fixture interpreted AND native against the same directives: 31 pass. --release (O2) output matches the interpreter. **Milestone M1: hello world native.** |
| 2026-10-31 | P3.1 | Runtime: ppl_alloc (malloc-backed, never frees, aborts on OOM), PplStr ops ppl_str_new/concat/eq/cmp/char_at (cached position → sequential s[i] O(1); ASCII fast path). MIR Rvalue::StrLen/StrCharAt; string +, +=, ==, <… in interpreter and codegen (codegen does the bounds check so the message stays in ppl_mir::panics). Fixtures: strings (Polish diacritics, emoji), string_index_panic, string_scan (10k chars). Fixed: if-without-else in statement position no longer flagged as a nullable value. |
| 2026-10-31 | P3.2 | Templates lower to concatenation of quasis and Rvalue::ToStr(part) (runtime ppl_str_from_{i64,u64,f64,f32,bool,char}, interpreter shares display()); `x.toString()` on primitives; triple-quoted and backtick multiline literals verified end to end. Fixtures: run/templates (nesting, $${}, multiline dedent), run/chars, fail/types/char_literal (`const c: char = "ab"`). loops.ppl no longer pending. |
| 2026-10-31 | P3.3 | ppl_sema::format: spec parser + type check (E0310); Types.formats; MIR Rvalue::Format; interpreter format_spec and C ppl_fmt_{i64,u64,f64,str,char,bool} with a constant PplFmt global from codegen; shortest_digits shared by e/f/%. Fixtures: run/format_specs (all §1.4 examples except `point:?`, every tour example, 40 cases, identical natively), fail/types/format_specs. Decision 181 + spec §1.4 rules. Note: a runtime edit briefly corrupted ppl_rt.c (empty-slice replace); restored from git before anything was committed. |
| 2026-10-31 | P3.4 | Structs end to end: Ty::Struct (Arc name), HIR StructDef/Target/Object, resolver (pass 1 structs, fields+methods, by-value cycle check E0210, methods with hidden `this`, implicit members, field targets), typeck (field indices, struct literals with E0311/E0312/E0313, method calls via shared call_fn, field-wise ==, `?` only), MIR places with projections + Deref base, Aggregate/Ref rvalues, StructLayouts; interpreter locals as Rc<RefCell> cells with Value::Ref; codegen named LLVM struct types, GEP places, insertvalue aggregates, struct_eq, generated ppl.debug.<Name> helpers. Decision 182 (asked): const fixes only the binding. Fixtures: run/structs (copy semantics, mutation via methods, nesting, ==, debug), fail/types/structs, fail/names/recursive_struct; `${point:?}` added to format_specs. |
| 2026-11-01 | P3.5 | Ty::Array/Fixed/Tuple/Range; MIR `Proj::{Field, Index(local)}` with bounds checks emitted by lowering (after an assignment's value, so the value can't invalidate them); rvalues Tuple/Repeat/ArrayNew/ArrayLen/ArrayPush/ArrayPop/InBounds/AddOverflows; HIR ForOf (ranges, arrays, fixed arrays, strings) and let/for-of destructuring of array patterns (incl. top level); runtime PplArray (ppl_array_new/push/pop); interpreter paths through shared arrays (`Step::Part/Elem`); generalized `${v:?}` debug helpers. E0314–E0316. Decision 183 (provisional). Fixtures: run/{arrays,tuples_fixed,ranges,array_index_panic,range_step_panic}, fail/types/arrays. |
| 2026-11-01 | P3.6 | `T?` as nullable pointer (refs) or `{i1, T}`; MIR Const::Null, Proj::Payload, WrapSome/IsNull/Unwrap; convert() handles T→T?, T?→U?, null; `??`, `??=`, `!` (panic), `?.` chains (resolver rewrites the outermost link onto a hidden local: HIR OptChain); `x == null` lowers to IsNull; if-without-else values; printing/debug of nulls; pop() returns T?. Typeck narrowing scopes with facts from conditions, `&&`/`\|\|`, early exits; assignments/loops forget. Null-safe ppl_str_eq; entry-block allocas for debug/pow temporaries. Decision 184. Fixtures: run/{nullable,null_unwrap_panic}, fail/types/nullable. |
| 2026-11-01 | P3.7 | Ty::Enum, HIR EnumDef/VariantDef, patterns (Wild/Bind/Lit/Variant positional+named/Struct) and Arm with guard; resolver: `Enum.Variant` construction (named + positional args), variant-name identifier patterns, enum cycle check generalized; typeck: constructor arg mapping (E0311/E0312/E0103), pattern checking (E0318), Maranget usefulness with witnesses (`exhaust.rs`, E0317 lists missing cases), unify_exprs for arms; MIR Proj::VariantField, Rvalue::Variant/Tag, sequential match lowering; interpreter Value::Enum; codegen `{i32, [k x i64]}` enums with own size/align computation, generated ppl.eq.<Enum> and debug helpers. Decision 185. Fixtures: run/enums, fail/types/match, fail/names/enums. |
| 2026-11-01 | P3.8 | HIR PatKind Or/Range/Slice/As/Type + `Pattern::looks_inside`; ExprKind::Is and Match.switch. Resolver: or-alternatives share locals (Binds with reuse), type names as type-test patterns, condition-scoped `is` bindings (`if`/`while`, through `&&`/parens), switch statements vs values. Typeck: bind_local (or-alternatives agree), slice/type/range checks, `is` facts for narrowing, switch statements skip exhaustiveness. Exhaustiveness: or-expansion (capped product), Ctor::Slice with length constructors (`[_, _, ...]` witnesses), fixed arrays. Lowering: single match_pattern (test + bind), or via shared success block, ranges, slices (Proj::Index with const locals, ArraySlice for rest). Runtime ppl_array_slice. Decision 186. Fixtures: run/patterns (every §3.5 example), fail/types/patterns, fail/names/or_patterns. |
| 2026-11-01 | P3.9 | Classes: StructDef gains is_class/ctor/getters/setters/statics/private set, Function.owner; resolver class_members (fields with literal-typed inits, param properties, accessors, static fields as globals + static init functions called first in the entry, static methods), synthesized constructors with a prologue, `new`, `Class.static` access, private static checks; typeck visibility (E0319) for fields/methods/accessors, getters/setters (incl. compound assignment), identity `==`, definite assignment walk (E0320), static inits checked first; MIR Rvalue::New, class field projections through the reference; interpreter Value::Object (Rc<Obj>) and Step::ObjField; codegen `{ptr header, fields}` objects via ppl_alloc_zeroed. Fixed: assignment checks its value before ending the target's narrowing (`cur = cur.next`). Decision 187. Fixtures: run/classes, fail/types/classes, fail/names/classes; fail/types/nominal now runs. |
| 2026-11-01 | P3.10 | Inheritance: classes ordered base-first (cycle/unknown/final-base errors), `inherit` copies fields/methods/accessors/vtable/visibility; vtables with required `override` (E0321), final-by-default and `final` (E0322), abstract stubs + E0323/E0324; constructor overloads by arity, `super(…)`/`this(…)` as HIR CtorCall (first statement only, E0326), forwarding default constructors, `super.m()` (SuperCall), `instanceof` + narrowing, upcasts (typeck `upcasts`, no-op conversions), protected access, override signature check (E0325), downcasting type/class patterns (`downcasts`, InstanceOf test into a subclass-typed temp). MIR ClassMeta (base, vtable), Callee::Virtual, Rvalue::InstanceOf; interpreter dispatch by object class; codegen `ppl.classinfo.<C>` globals `{base, [n x ptr]}`, header store, indirect calls, runtime ppl_instance_of. Fixed: a diverging body in a value-returning function ends in `unreachable`. Decision 188. Fixtures: run/inheritance, fail/names/{inheritance,ctor_call}, fail/types/inheritance. |
| 2026-11-01 | P3.11 | Interfaces: Ty::Iface (StructDef.is_interface, `implements` lists), resolver orders interfaces by `extends`, inherited-first method lists with defaults/abstract stubs, conformance (E0327 missing, E0328 conflicting defaults, public implementations); typeck nominal upcasts to interfaces (class/struct, first-parent interface chains), interface call slots, implementation signature checks (E0325). Lowering: ToIface (boxing structs), IfaceData, Callee::Iface, itables + adapters (virtual when needed). Interpreter Value::Iface; codegen `{ptr, ptr}` fat pointers and `ppl.itable.N` globals. Decision 189. Fixtures: run/interfaces, fail/types/interfaces (structural match without `implements`), fail/names/interfaces. |
| 2026-11-01 | P3.12 | Ty::Lits/Union/Variant + LitVal; type aliases (lazy, cycle check); resolver union normalization (flatten, null → `T?`, literal merging), interned anonymous enums for unions (EnumOrigin::Union), discriminated object unions (EnumOrigin::Discriminated, discriminant = field 0), `typeof` (HIR TypeOf). Typeck: literals into literal types/union members (`lit_in`), union coercions and joins, discriminant member access and `e.kind == …` / `typeof x == …` narrowing (`tag_facts`), discriminated object literals, union patterns (`union_tests`), exhaustiveness for literal unions and union members. Lowering: Variant construction into unions, type names by tag, union stringification, narrowed reads into members, `erase_program`. Decision 190. Fixtures: run/unions (every §2.5/§2.6 example), fail/types/unions. |
| 2026-11-01 | P3.13 | Generic functions: Ty::Param(fn, index, name) + subst/has_params; HIR TypeParam/Bound (Comparable, Equatable, interfaces; `extends`, `where`, `&`), explicit type args on calls; typeck call_generic (3-pass inference + unify, E0329/E0330), operations on parameters via bounds (`compareTo`/`equals`, comparisons, interface methods → param_calls), Types::substituted; lowering worklist of instances (`f<i64>`), placeholder for generic bodies, param_call per concrete type (primitive compare → -1/0/1, struct/class/interface methods). MIR snapshot `generic_instances`. Decision 191. Fixtures: run/generics (max<T> over ints, floats, strings, chars), fail/types/generics. |
| 2026-11-01 | P3.14 | Generic structs/classes: type args in Ty::Struct/Class/Iface (mechanical change), StructDef.generics with defaults, identity instantiation for `this`; resolver `Name<…>` in types/`new`/literals, generic methods, global annotations deferred until types are declared; typeck field types substituted by receiver args, call_inferring (shared by generic fns, methods of generic types, generic constructors via `new` with expected-type args), struct literal inference, unify through type args; lowering multi-owner substitutions (`Stack<i64>.push`, `Box<i64>.map<string>`), method_target; MIR New/Aggregate(Ty), projections substitute; codegen per-instantiation LLVM types. Decision 192. Fixtures: run/generic_types (Stack<T>, Pair<A, B>, defaults, generic method, Node<T> list), fail/names/generic_types, fail/types/generic_types. |
| 2026-11-01 | P3.15 | Closures: Ty::Fn and `(x: T) => U` types; resolver lifts arrows/function expressions (ExprKind::Closure, Function.closure with captures), capture lookup through enclosing functions (by value for const, boxing captured lets/params), arrow `this` capture, per-iteration rebox for captured C-style `for (let …)` variables; typeck closure checking with contextual parameter types, named functions as values, calls through function values; MIR Rvalue::Closure/BoxNew, Callee::Indirect, env as first parameter, boxed locals via Ref/deref, function-value adapters; interpreter Value::Closure; codegen `{fnptr, env}` with heap environments and indirect calls. Entry function id is now reserved before top-level resolution. Decision 193. Fixtures: run/closures (§5.5 counter, loop-variable closures, makeAdder, shared lets, method arrows), fail/types/closures. |
| 2026-11-01 | P3.16 | Named arguments, defaults (callee-evaluated with hidden flags; may use earlier params), optional `?` params, rest params and `...array`/`...tuple` spreads via typeck `Slot`s (`map_args`/`check_slots`) and lowering `call_operands`; overload sets for top-level functions with applicability + specificity resolution (E0332/E0333, same-signature duplicates still E0203); `tailcall` (E0331) as MIR Terminator::TailCall → LLVM musttail / interpreter frame reuse (1,000,000-deep recursion runs). Const::Undef for omitted arguments. Decision 194. Fixtures: run/function_features, fail/types/function_features. |
| 2026-11-01 | P3.17 | Operators: `operator` members as methods `op+`/`op-u`/`op[]`/`op[]=` (resolver), typeck user_binary/unary/index/index-assign/compound dispatch (op_calls), `==` → user `equals`, comparisons → user or derived `compareTo` (derived), derived `equals`/`compareTo`/`hashCode` method calls; `@derive(Eq, Hash, Ord)` on classes; lowering user_call/receiver, compare_values/cmp_into (structs, tuples, fixed, ranges, enums, nullables, classes), eq_values (derived class equality), hash_value/hash_parts; MIR Rvalue::Hash/HashCombine with matching interpreter and C runtime (ppl_hash_u64/str/combine); bool `&`/`\|`/`^`. Fixed: run cache keyed on the compiler binary. Decision 195. Fixtures: run/operators (Vec2 arithmetic, sorting structs and classes by compareTo, derive), fail/types/operators; bound fixtures now use classes. |
| 2026-11-01 | P3.18 | Ty::Distinct (from `type X = distinct T`, erased to the base; `as` conversions; literals adopt numeric distinct types), Ty::Immutable (typeck-only, stripped from Types before lowering): deep views with pure-value normalization, `through_immutable` checks for field/element assignment and non-readonly method calls (incl. push/pop), `readonly` methods (immutable `this`), `readonly` fields (E0334) incl. parameter properties; struct members accept `readonly`. Lowering erases literal types. Decision 196. Fixtures: run/distinct, fail/types/immutability (mutation through `immutable`, `Meters`/`Feet` mixing). Type aliases were done in P3.12. |
| 2026-11-01 | P3.19 | Error-return exceptions: prelude `Error` class, `throw`, `try`/`catch`/`finally` as expression (and statement), typed/pattern catch clauses, `throws` on functions, methods and constructors; checked-throws pass (E0336) with catch narrowing; non-`Error` throws E0337; top-level uncaught → `uncaught Class: msg`, exit 1 (interp `Panic.uncaught`, runtime `ppl_uncaught`). `finally` runs on return/break/continue. Decision 197. Fixtures: run/errors/exceptions, fail/types/{exceptions,throw}, fail/names/throws_clause. IR/MIR snapshots now include the prelude. |
| 2026-11-01 | P3.20 | Throws inference (lambdas, top-level fns; `Types.fn_throws`, fixpoint in `throws::check`), `Ty::Fn` carries throws (resolved `throws` in fn types, display, subclass-aware upcast), `E extends Error` (`Bound::Error`) bound from callbacks in `unify`, lambdas deferred to the last inference pass, `try?`/`try!`, checks after every indirect call. Lightweight throwables split into P3.20b. Decision 198. Fixtures: run/errors/{exceptions_inference,try_bang_panic}, fail/types/exceptions (rewritten: methods declare, top-level infers; map/parseInt precision). |
| 2026-11-01 | P3.21 | Panic unwinding through `finally`: MIR `Guard`/`Unguard`/`Resume` terminators (try-with-finally), interpreter handler stacks, native `_setjmp` + runtime guard stack (`ppl_guard_push/pop`, `ppl_resume`), `optnone` for guarded functions. Stack traces: interpreter call-name stack; native `backtrace` + `dladdr` (addr-1), frame pointers everywhere, debug builds export `ppl.*` symbols. Decision 199. Tests: run/errors/panic_finally, driver panic_trace snapshots (native macOS and interpreter, identical). |
| 2026-11-01 | P3.22 | Core library in ppl (`stdlib/core`: error, array, string, map/set) loaded as prelude files; extension methods on arrays/strings via `__Array_*`/`__String_*` functions (generic, throws-polymorphic); `Hashable<T>` bound and `hashCode()` on type parameters; type params in templates; getters/setters of generic class instances; `StrSlice` primitive (`ppl_str_slice`); live-function analysis (library code only when reached). Decision 200. Tests: run/stdlib/{collections,throwing_callbacks}, fail/types/stdlib_throws; IR/MIR snapshots shrank. |
| 2026-11-01 | P3.23 | `Iterator<T>` core class (step closure) with lazy adapters; `function*`/`*method` desugared in the resolver to `new Iterator(step)`, the step closure lowered as a MIR state machine (frame in the closure environment, `Cx::gen_frames`, dispatch/save blocks filled after the body); `for-of` over iterators and `iterator()` methods (`Types.iterables`, `for_of_iterator`); `Map`/`Set`/array iterators; null comparisons always allowed; T+null joins; lambda result hints. Decision 201. Tests: run/iterators/generators (fib().take(10) etc.), fail/types/generators, fail/names/yield. |
| 2026-11-01 | P3.24 | Modules: driver loader (`modules.rs`: relative/extensionless/`index.ppl`/`std/` resolution, DFS init order, export/import tables, `export const` unwrapping), resolver namespaces per module (`Names<T>`, `ModCtx`, `resolve_modules`), `* as` namespaces for values and types, re-exports, E0338 library `let`, E0339 init cycles, E0340 not exported, E0341 unknown module; harness resolves imports from each fixture's directory; `stdlib/greeting.ppl` as a first `std/` module. Decision 202. Fixtures: run/modules/{basic,cycle}, fail/modules/{init_cycle,library_let,exports,missing}. |
| 2026-11-01 | P3.20b | Lightweight throwables: prelude `Throwable` interface, throwable enums (`EnumDef.throwable`), `Program::throwables`/`throw_tag`, boxed values with a tag slot (`SetException(v, tag)`, `ExceptionTag`; interp field, native `ppl.exception_tag` global), catch dispatch by tag (`catch_pattern`), shared `exception_text` for uncaught/`try!`, throws analysis `Catch` kinds. Decision 203. Tests: run/errors/throwables, fail/types/{throwables,throw_value}. |
| 2026-11-01 | P3.25 | Destructuring desugared in the resolver: object patterns with defaults, array `...rest` (slice), destructured parameters (`split_destructured`, `destructured_params`), destructuring assignment (`assign_destructured`), array spread via `concat`/`slice`. Decision 204 (object `...rest` still missing). Tests: run/destructuring (§4.1, §5.3, §5.7 examples), fail/{names,types}/destructuring. |
| 2026-11-01 | P3.26 | `using` desugared in `block_stmts` to `const` + `try { rest } finally { x?.dispose() }` (reverse order, exceptions, early exits, panics); prelude `Disposable`. Decision 205. Tests: run/using, fail/{types,names}/using. **Phase 3 complete.** |
| 2026-11-01 | P4.1 | GC roots: `PplLayout` type maps (codegen `gc.rs`: reference offsets mirroring `size_align`, enum payloads conservative), shadow-stack frames (traced locals in one zeroed frame struct, pushed at entry, popped at return/tailcall, restored by panic guards), global-roots table, safepoints at entry and loop headers (`ppl_gc_requested`), write-barrier calls on heap stores, `ppl_gc_alloc(size, layout)` everywhere (still malloc), `PplArray.elem`; `__gc_*` builtins, `std/gc` (`gc`, `GcStats`); interpreter frame stack and `dumpRoots`. Decision 206. Test: run/gc/roots. |
| 2026-11-01 | P4.2 | Mark-sweep heap in `ppl_gc.c`: 64 KiB blocks, 27 size classes (bump then free lists), large-object block runs, block hash table for pointer validation (interior pointers), mark stack, sweep with spare-block reuse, threshold 2× survivors (≥ 8 MiB), pause/time stats, `pause`/`resume`; `PPL_GC_STRESS=1` and a third harness run per fixture. Decision 207. Test: run/gc/stress (200k iterations: 38 MB allocated in an 8 MB heap, 5 collections, max pause 0.46 ms). |
| 2026-11-01 | P4.3 | Generational + incremental GC: nursery of young objects (list-swept, promoted in place), remembered set + cards for large objects, write barrier with layouts (Dijkstra shading during marking), incremental marking with alternating colors and chunked large objects, finishing pause (nursery + root rescan + drain), lazy sweeping, radix block table; `collectYoung`, `pause`/`resume`, `resetStats`; `PPL_GC_STRESS` overlaps cycles with the mutator, `PPL_GC_TRACE`. Decision 208. Tests: run/gc/{generations,control}; bench/gc_pause.ppl: frame max pause 0.34–0.77 ms (see notes). |
| 2026-11-01 | P4.4 | `WeakRef`, `WeakMap`, `WeakSet`, `FinalizationRegistry` in `stdlib/core/weak.ppl` over a core-only `@weak` field (`StructLayout.weak`, `PplLayout.nweak`; the collector clears dead targets after nursery marking and at the end of major marking), `T extends object` bound, `__ref_id` builtin; interpreter weak fields as `rc::Weak`; finalizers at exit through `__runFinalizers` (dropped when unused). Decision 209. Tests: run/gc/{weak,finalization}, fail/gc/weak_targets. |
| 2026-11-01 | P4.5 | `unsafe { }`, `unsafe function`, `ptr<T>` (as `Ty::Ref`), `ptr.of`, `read`/`write`, pointer `+`/`-`/`==`, `sizeof`/`alignof` (`mir::Program::size_align`, moved from codegen), `unsafe.cast` bit casts, `@pod` structs; E0342 (needs `unsafe`), E0343 (non-plain `@pod` field). Decision 210. Tests: run/unsafe/pointers, fail/unsafe/{outside,pod}. |
| 2026-11-01 | P4.6 | Scoped `arena { }` (runtime arena chunks in the block table, roots for the GC, barrier + exit escape checks, exception evacuation, guard restore; lowering via the `finally` exit list; static escape checks E0344), `Arena` class (raw chunks), explicit method type arguments and named arguments for generic calls, `parseInt`, `int[].sum()`, harness `// native-only:`. Decisions 211–212. Tests: run/arena/{scoped,raw,escape_panic,escape_frame}, fail/arena/escape. Phase 4 done. |
| 2026-11-01 | P5.1 | Event loop in the core library (`event.ppl`: timers, intervals, microtasks, JS ordering; finalizers when idle), `__clock_ms`/`__loop_wait` builtins over `ppl_loop.c` (kqueue/epoll), exit hook `__runEventLoop` (dropped when unused). Decision 213. Test: run/async/timers. |
| 2026-11-01 | P5.2 | async functions/methods desugared to generator steps over `Promise<T, E>` (core library: executor ctor, `then`, `resolve`/`reject`/`all`/`race`/`allSettled`), checked rethrow, top-level `await`, unhandled rejections (exit 1), E0345; omitted trailing `void` args, `void`-expected lambdas. Decision 214. Tests: run/async/{ordering (= Node), basics, unhandled_rejection}, fail/async/{unhandled, await_outside}. |
| 2026-11-01 | P5.3 | `async function*` over a core `AsyncIterator<T, E>`, `for await`, `await using` + `AsyncDisposable`; async results held until the step finishes; panic guards re-armed after `yield`; `x != null` for any type. Decision 215. Tests: run/async/{generators (§5.7 example), await_using (= Node)}, run/errors/generator_finally_panic. |
| 2026-11-01 | P5.4 | `AbortController`/`AbortSignal`/`AbortError` (core), `std/timers` `delay(ms, signal?)` rejecting with `AbortError`. Decision 216. Test: run/async/abort. |
| 2026-11-01 | P5.5 | Workers: thread-local runtime/program state, worker modules as extra roots with their own entry functions, `ppl_worker.c` (threads, queues, wakers, generic deep copy), `Worker`/`WorkerPanicked`/`std/worker`, `Sendable` bound, interpreter threads. Decision 217. Tests: run/workers/{pingpong, errors, graphs}, fail/workers/not_sendable. |
| 2026-11-01 | P5.6 | `ArrayBuffer` + typed arrays (off-heap, copied or transferred/detached in messages), inline workers (`new Worker(f)`, `call`, unref when idle), `parallelMap`, `WorkerPool`, `navigator.hardwareConcurrency`; static getters, generic-call defaults. Decision 218. Tests: run/workers/{transfer, parallel (hashing), pool}, fail/workers/inline_captures. Phase 5 done. |
| 2026-11-01 | P6.1 | MIR constant folding (`ppl-mir/src/fold.rs`): int/float/bool/char `Binary`/`Unary`/`Cast`, propagation through single-assignment locals and constant globals, constant branches → gotos, unreachable blocks emptied, dead constant temps dropped. Arithmetic moved to `ppl_mir::eval` (interpreter uses it too). Folded panics are E0347 (decision 219); MIR `Loc` now carries a span. Tests: mir snapshot `constant_folding`, fail/consts/const_panics; four run panic fixtures now use parameters. |
| 2026-11-01 | P6.2a | `comptime` expressions/blocks/consts/`if`, run in `ppl-interp` (`ppl_interp::comptime`: comptime mode with step limit, forbidden builtins, unset-global check; values written back as MIR), `@comptimeLimit`, `comptime.readFile`, `target`/`build` (`stdlib/core/comptime.ppl`), compile-time log. Shared operand walkers (`Rvalue::operands`, `Terminator::operands`). E0348–E0352. Decision 220. Split P6.2 → P6.2a/P6.2b (comptime parameters). Tests: run/comptime/{crc_table, values, target, shadowing}, fail/comptime/{step_limit, failures, captures}, driver test of the compile-time log. Fixed TOUR's CRC example precedence (`(c & 1) != 0`). |
| 2026-11-01 | P6.2b | Comptime parameters: `<comptime N: i32>` and `comptime n: i32` (hidden trailing type parameter), constants as literal-type arguments (one instance per constant), `[T; N]` lengths (`Ty::FixedN`, inferred from fixed-array arguments), E0353. Local `comptime const`s usable from later comptime code. Decision 221. Tests: run/comptime/{params, local_consts}, fail/comptime/{params, params_var}. |
| 2026-11-01 | P6.3 | Types as comptime values: `<comptime T: type>` and `comptime T: type` parameters (type arguments written as values), `T.name`/`kind`/`fields`/`methods` (`FieldInfo`, `MethodInfo` in core) as per-instance constants, private members only in the declaring file; `vec<comptime T: type, comptime N: i32>`. Decision 222. Tests: run/comptime/types, fail/comptime/types. |
| 2026-11-01 | P6.4 | Decorators: `decorator function`s run in a separate comptime program before resolution (`ppl-driver/src/decorators.rs`), `std/meta` (`ClassDecl`, `FunctionDecl`, `Args`) recording requests through `__meta_emit`; added members, hidden statics, wrapped bodies, `error`/`warn`/`note` (E0354). Generated code in files mapped to modules (`Modules::file_modules`). CLI prints warnings on success. Spec/TOUR memoize example fixed. Decision 223. Tests: run/decorators/{basic (memoize, mustHaveId, traced across modules), members}, fail/decorators/{reported, wrong_kind}. |
| 2026-11-01 | P6.5 | Quasi-quote templates (code text parsed where spliced, type-checked with the program; errors labeled at the generating decorator via `Expansion::annotate`) and the `ast` builder in `std/meta` (static methods returning code text). `FunctionDecl.isAsync`; generated struct members drop `public`. Decision 224. Tests: run/decorators/to_string (`toString()` from a template, `describe()` from the builder, `@logged`), fail/decorators/bad_template. |
| 2026-11-01 | P6.6 | `@derive(Eq, Hash, Ord)` as a core-library decorator (`stdlib/core/derive.ppl`); built-in derive and `StructDef.derives` removed. Meta classes moved to the core library (`stdlib/core/meta.ppl`, re-exported by `std/meta`), core decorators visible everywhere, bare-name decorator arguments passed as strings, prelude fallback for re-exported names. Generic `==`/`<` on class instances now call their `equals`/`compareTo`. Decision 225. Tests: run/decorators/derive (Map keys, sort, `==`/`<`), fail/decorators/derive_unknown; P3.17's operators.ppl unchanged; three LLVM IR snapshots gained the new core class infos. |
| 2026-11-01 | Class strings | Classes convert to strings silently (templates, `print`, `toString()`, `string + v`): `toString(): string` or `Name (field = value, …)`, runtime class, cycle guard; `+`/`+=` convert any printable value next to a string (decision 226, spec §1.4/§3.3/§6.1). |
| 2026-11-01 | P6.7 | Runtime reflection: `std/reflect` (`reflect.typeOf(v)` runtime class / `typeOf<T>()`, `TypeInfo<T>`, `Field<T>` with typed `get<V>`/`set<V>`/`show`, methods, interfaces, base, decorators as name + argument source); metadata synthesized on demand in MIR; class metadata only for used classes (binary smaller without reflection). Decision 227. |
| 2026-11-01 | P6.8 | JSON: `JSON.parse<T>` / `JSON.stringify` generated per type in MIR over a ppl reader/writer, precise errors (`field 'port': expected u16, got "x"`, `missing field`, `invalid JSON at L:C`), `@json.name`, optional struct fields (`p?: T`), `__parse_f64` builtin. Decision 228. |
| 2026-11-02 | P7.1a | `std/fs` (readText/readBytes/writeText/writeBytes/stat/exists/mkdir/remove/rename/readDir/open + `File` read/write/seek/close, `IoError` with portable kinds) on a runtime I/O thread pool polled by the event loop; `std/path` (POSIX, as Node's `path.posix`); golden runs in fresh temp directories. P7.1 split (watch → P7.1b). Decision 229. |
| 2026-11-02 | P7.1b | `fs.watch(path, recursive = true)` → `Watcher` (async iterator of `WatchEvent { kind, path }`, `close`/`using`): snapshot diffs while a `next()` waits, kqueue/inotify wake-ups natively, polling in the interpreter. Decision 230 (provisional: API shape). Test: run/fs/watch. |
| 2026-11-02 | P7.2 | `std/process`: `args`, `env` (get/has/set/delete/toObject), `exit` (skips `finally`), `cwd`, `pid`, `stdin`/`stdout`/`stderr`; `Command` (named options: `cwd`, `env`, `clearEnv`, `stdin`/`stdout`/`stderr` `"inherit" \| "pipe" \| "null"`; `spawn`/`output`/`status`, `Command.shell`), `Child` (`Reader`/`Writer` pipes, `status`, `output`, `kill`), `ProcessError`, `ExitStatus`/`Output`. posix_spawn natively, `std::process` in the interpreter; blocking jobs on threads of their own. Constructors now take default, optional and rest parameters and named arguments. Decision 231. Tests: run/process/{pipes (echo, cat with pipes, shell, failure, kill), self}, run/ctor_defaults, fail/types/ctor_defaults. |
| 2026-11-02 | P7.3 | `std/net`: TCP `listen`/`connect` (`Listener` async iterator of `Conn`; `Conn` read/`for await`/text/write/writeText/closeWrite/close, `localAddr`/`remoteAddr`), UDP `bind` (`UdpSocket` send/sendText/receive/close, `Datagram`), DNS `resolve`, `NetError` with portable kinds. Blocking socket jobs on runtime threads natively, `std::net` in the interpreter. Spec §13.4 `std/net`, decision 232. Tests: run/net/{echo (server and clients in one process), udp (datagrams, DNS)}. |
| 2026-11-02 | P7.4 | Web Streams (`std/streams`: ReadableStream/WritableStream/TransformStream with controllers, readers, writers, backpressure, pipeTo/pipeThrough, `from`, text encoder/decoder streams), `TextEncoder`/`TextDecoder` globals (UTF-8, fatal/BOM/stream), `std/encoding` (base64 incl. URL-safe, hex), `Uint8Array.subarray/slice/from`, `TypeError`/`RangeError`; `readable`/`writable` on net `Conn` and process pipes. Spec §13.5, decision 233. Tests: run/streams/{pipe, encoding}, run/net/streams (TOUR's echo with `pipeTo`), run/process/streams. |
| 2026-11-02 | P7.5a | P7.5 split (a: HTTP, b: TLS — waits for the author's library choice, c: WebSocket). `URL`/`URLSearchParams` (checked against Node), `Headers`, `Request`, `Response` (`json<T>()`, `Response.json`, `redirect`), `fetch` over HTTP/1.1 (redirects, chunked, HEAD), `std/http` `serve` (keep-alive, 400/500, chunked stream bodies, `shutdown`/`finished`). Streams and net moved into core. Typing: union array-literal context, function return covariance. Decision 234. Tests: run/http/{url, serve, keepalive}, run/fn_return_covariance. |
| 2026-11-02 | P7.5c | `WebSocket` global (ws:, text/binary, fragments, ping/pong, closing handshake, masking with random keys), `std/http` `upgradeWebSocket` with the connection handed over by `serve`, SHA-1 in core, `__io` 132 random bytes, base64/hex moved into core; local type aliases shadow core types. Spec §13.6, decision 235. Test: run/http/websocket (incl. RFC 6455's accept example). |
| 2026-11-02 | P7.6 | `RegExp` engine in ppl (backtracking VM: groups, named groups, backrefs, lookahead/behind, lazy/greedy, flags gimsyu), compile-time validation of literals (E0010, JS messages, Rust port of the parser), `RegExpMatch`, `SyntaxError`, string `replace`/`replaceAll`/`split` with regexes and replacement patterns/functions, `match`/`matchAll`/`search`; `std/regex`. 35 exec cases and the string methods agree with Node. Spec §13.7, decision 236. Tests: run/regex/{engine, strings}, fail/regex/invalid, regex.rs unit tests. |
| 2026-11-02 | P7.7 | `std/time` (`Duration`, `Instant`, `TimeZone` from the system tz database incl. POSIX footer rules, `DateTime` with DST disambiguation, calendar arithmetic, ISO parse/print, `format` patterns), `Date` global (JS-compatible, checked against Node in two zones), `performance.now()`. Typing: literals into nullable unions, object literals into a union's struct member. Spec §13.8, decision 237. Tests: run/time/{date, std_time}. |
| 2026-11-02 | P7.9a | P7.9 split (a/b/c). `Math` global (all of JavaScript's functions and constants, agreeing with Node bit for bit; `__math` builtin over libm), `std/math` (Vec2/3/4, Quat, Mat4 with projections/lookAt/invert, BigInt checked against Node's bigint, seeded Random), static methods/getters on structs, lexer fix for `operator /`. Spec §13.9, decision 238. Tests: run/math/{global, std_math}. |
| 2026-11-02 | P7.9b | `std/collections`: `Deque` (two-array, amortized O(1) ends, indexing), `PriorityQueue` (binary heap with a comparator), `SortedMap` (sorted arrays: floor/ceiling/range queries), `BitSet` (set algebra). Spec §13.10, decision 239. Test: run/collections/collections. |
| 2026-11-02 | P7.9c | `Formattable` core interface (`${x}`/`${x:spec}` call `format(spec)` on structs and classes that implement it), `std/fmt` (runtime spec parsing and formatting matching the compiler's, `pad`, `groupDigits`); `DateTime`, vectors and quaternions are Formattable. Spec §1.4/§13.11, decision 240. Tests: run/fmt/{formattable, std_fmt}. |
| 2026-11-02 | P7.10 | `std/test` (describe/test/it/skip/only/todo, hooks, typed `expect` overloads with `.not`, `toThrow`, `toPanic`, `resolves`/`rejects`, async tests, `bench`), `ppl test` (discovery, a process per file in parallel, `-t`, `--interp`, `--watch`, `-j`) and `ppl bench`; `__catch_panic` builtin; stdlib tests as `stdlib/tests/*.test.ppl` run by cargo. Overload resolution by shape/specificity, generic getters substituted, native debug format of `T?`. Spec §14.3, decision 241. Tests: run/test/runner, ppl-cli stdlib_tests. |
| 2026-11-02 | P7.8 | Mbed TLS 3.6.7 vendored (author's choice); `crypto` global (`getRandomValues`, `randomUUID`, Web Crypto `subtle`: digest, HMAC, AES-GCM, ECDSA, ECDH, HKDF, PBKDF2) and `std/crypto` (hashes incl. SHA-3, HMAC, HKDF, PBKDF2, AES-GCM, ChaCha20-Poly1305, P-256/P-384/X25519 keys, deterministic ECDSA, ECDH) over one C layer shared by both backends. Async struct methods; struct `this` captured by reference. Spec §13.12, decision 242. Tests: run/crypto/{kat (FIPS/RFC/NIST vectors), subtle}, stdlib/tests/crypto. |
| 2026-11-02 | P7.5b | TLS over Mbed TLS: `fetch` https: (certificate and host checks against the system roots, `caCerts`), `serve` with `cert`/`key`, `wss:`; one C session layer keyed by socket fd used by both backends (concurrent read/write via non-blocking sockets), `NetError` kind `TlsError`. Spec §13.6, decision 243. Test: run/http/https (local CA, IP SAN, big bodies both ways, untrusted rejection, wss echo). **Phase 7 complete.** |
| 2026-11-02 | — | Move to Linux prepared: `env.sh` finds apt.llvm.org LLVM 23 and `~/.cargo/bin`, DEVELOPMENT.md Linux setup, CI workflow (macOS + Linux arm64, Linux allowed to fail until P8.0), PLAN: new P8.0 (Linux host bring-up), P8.1/P8.2/P8.4 "Done when" reworded for a Linux arm64 host. Fixed a lost wakeup in `ppl_worker_post` (it read `started` before pushing, so a worker starting in between slept forever; hung run/workers/transfer intermittently). |
| 2026-11-02 | P8.0 | Linux arm64 host: LLVM 23.1.5 (apt.llvm.org) installed; `scripts/test.sh` green. Fixes: `_POSIX_C_SOURCE` in `ppl_gc.c` (glibc hides `clock_gettime` under `-std=c11`); `-lm` moved into `host_link_flags`; GCC folded the weak `const` `ppl_arraybuffer_class` to NULL, so copied `ArrayBuffer`s were shared with workers (now non-const); glibc's `cbrt` corrected to the nearest double (`Math.cbrt(27)` was 3.0000000000000004; matches Rust's on 2M inputs); panic traces skip unnamed frames outside the executable (glibc's `__libc_start_call_main`); `panic_trace` native test enabled on Linux; CI's Linux job no longer allowed to fail. Checked by hand: `fetch` of a public https site uses the Debian CA bundle. |
| 2026-11-02 | P8.1 | Cross-compilation: `Target` (§15.1 triples and aliases, glibc suffix, default 2.31) and `ppl build --target`; codegen for any triple (baseline CPU when cross); lld linked in-process (`ppl-lld`: C++ shim over `lld::lldMain`, ELF); runtime + Mbed TLS compiled per target by clang 23 and cached; every linux-gnu build (host included) linked by lld without `cc`. `-mno-outline-atomics` on aarch64 and `MBEDTLS_NO_UDBL_DIVISION`, since no compiler builtins are linked. Decision 244. Test: `ppl-driver/tests/cross.rs` builds linux-x64 from arm64, checks the ELF machine and runs it under qemu-user; the golden tests now go through lld on Linux. CI installs liblld, the amd64 cross glibc and qemu. |
