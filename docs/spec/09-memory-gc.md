# 9. Memory & GC

## 9.1 Model
- Class instances, arrays, strings, closures and boxed values live on a **GC-managed heap**.
- Structs, tuples, fixed arrays and primitives are values stored inline. Any GC references inside
  them are traced.
- `@pod struct`s contain no references and have a C-compatible layout.

## 9.2 The collector
- **Per-thread heaps.** Every thread/worker owns its heap and collects it independently. There is
  never a global stop-the-world pause.
- **Low pause.** A generational collector (bump-allocated nursery plus an incrementally marked old
  generation) aims for sub-millisecond pauses at game frame rates.
- **Control:**
  ```ts
  import { gc } from "std/gc"
  gc.collect()             // full collection now
  gc.collectYoung()        // nursery only
  gc.pause(); …; gc.resume()  // no collections in a frame-critical section
  gc.stats()               // heap size, pause times
  ```
- **Web**: `wasm32-browser` uses **WasmGC**, so the browser's collector manages ppl objects and they
  can be handed to JS without copying. WASI and native targets use the ppl collector.

## 9.3 Arenas
Two forms are available.

**Scoped arena (safe).** Every allocation made inside the block goes to an arena that's freed in
one step when the block exits. The compiler rejects any reference that would outlive the block.
Only values (structs, primitives, copies) can leave it.
```ts
const total = arena {
  const parts = text.split(",")             // allocated in the arena
  parts.map(p => parseInt(p)).sum()         // an i64 escapes, which is fine
}
```

**Arena object (unsafe).** Explicit lifetime control for advanced uses:
```ts
unsafe {
  const a = new Arena(capacity: 1 << 20)
  const p: ptr<Particle> = a.alloc<Particle>(count: 10_000)
  …
  a.free()                                   // dangling pointers after this are UB
}
```

## 9.4 Unsafe code
Raw pointers `ptr<T>`, pointer arithmetic, `unsafe.cast<T>(x)`, `sizeof<T>()` and `alignof<T>()`
are allowed only inside `unsafe { }` blocks or `unsafe function`s.
```ts
unsafe {
  const p = ptr.of(buffer[0])
  (p + 4).write(0xFF as u8)
}
```

## 9.5 `--no-gc` mode
For embedded targets, compiling with `--no-gc` restricts the language to structs, enums, fixed
arrays, primitives, arenas and `unsafe`. Class allocation, growable arrays, `string`
concatenation and closures that capture by reference are compile errors. Exceptions need an
allocation strategy (still open).

## Open questions
- Finalizers / `using` declarations (TS 5.2 `using x = …` with `Symbol.dispose`) for files and
  sockets?
- Weak references (`WeakRef`, `WeakMap`)?
- Can a string in `--no-gc` mode be a non-growable `str` slice?
