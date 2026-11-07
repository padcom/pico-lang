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
  gc.resetStats()          // restart the pause statistics (e.g. after loading)
  ```
- `gc.dumpRoots()` (debugging) prints the GC roots that hold references: per active function
  (innermost first) its named locals, then the globals. `GcStats` has `heapSize`, `usedBytes`,
  `allocatedBytes`, `collections`, `youngCollections`, and `lastPause`/`maxPause`/`totalPause` in
  nanoseconds. The interpreter has no collector: its statistics are zero.
- **Web**: `wasm32-browser` uses **WasmGC**, so the browser's collector manages pico objects and they
  can be handed to JS without copying. WASI and native targets use the pico collector.
  *Interim:* LLVM can't emit WasmGC, so until pico's own WasmGC backend exists, `wasm32-browser`
  builds use linear memory with the pico collector (as on WASI). Values then cross to JS as copies
  or handles instead of shared GC objects.

## 9.3 Weak references & finalization
These are JS-compatible: `WeakRef<T>`, `WeakMap<K, V>`, `WeakSet<T>` and `FinalizationRegistry<T>`.
Finalization callbacks are **non-deterministic**. They run on the owning thread's event loop at
some point after collection. Use `using` (§11.5) for deterministic cleanup.
```ts
const cache = new WeakMap<Texture, Image>()
const ref = new WeakRef<Texture>(tex)
ref.deref()?.bind()                   // `null` once the texture was collected

const registry = new FinalizationRegistry<string>((name: string) => log(`freed ${name}`))
registry.register(tex, tex.name)
registry.registerWithToken(tex, tex.name, owner)
registry.unregister(owner)
```
- They are core-library globals. Targets and keys are `object`s (classes or arrays, §2):
  `WeakRef<T extends object>` (`deref(): T?`), `WeakMap<K extends object, V>` (`has`, `get`
  (`V?`), `set`, `delete`), `WeakSet<T extends object>` (`has`, `add`, `delete`).
  `FinalizationRegistry<T>(callback)` has `register(target, held)`,
  `registerWithToken(target, held, token)` (pico has no method overloading yet) and
  `unregister(token): bool`.
- A weak reference is cleared when a collection finds its target unreachable (a nursery
  collection for young targets, the end of a major marking for old ones); `gc.collect()`
  clears every one whose target is unreachable.
- `WeakMap` holds its values strongly: a value that refers to its own key keeps the entry
  alive (no ephemerons).
- Until there is an event loop (§10), the due finalization callbacks run once the entry
  module's top-level code has finished (after a full collection); a program without a
  `FinalizationRegistry` does no work at exit.
- The interpreter has no collector: a target dies with its last reference, and objects in a
  reference cycle are never cleared there.

## 9.4 Arenas
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

- Allocations inside are those made *while* the block runs, including in the functions it
  calls; storage that belongs to an older object (a growing outer array) stays on the heap.
- Static rules (E0344): the block's value must not hold references, and inside it (outside
  lambdas) no reference may be assigned to a variable declared outside it or a global,
  stored into a field or element of something rooted outside it, pushed onto such an array,
  or `return`ed.
- What the compiler can't see (a callee stashing a reference) is caught at run time: storing
  an arena reference into an older object panics, and so does closing the arena while a
  variable or global still points into it ("a reference into an arena outlives the arena").
  The interpreter doesn't check at run time.
- `return`, `break`, `continue`, exceptions and panics all close the arena. An exception
  leaving the block is copied out of the arena first (with what it reaches).
- Arena objects are roots for the collector while the arena is open, and are never
  collected individually.

**Arena object (unsafe).** Explicit lifetime control for advanced uses:
```ts
unsafe {
  const a = new Arena(capacity: 1 << 20)
  const p: ptr<Particle> = a.alloc<Particle>(count: 10_000)
  …
  a.free()                                   // dangling pointers after this are UB
}
```
`new Arena(capacity)` reserves memory (it grows by more chunks when needed);
`alloc<T>(count)` returns zeroed, untraced room for plain data (no references); `free()`
releases it all.

## 9.5 Unsafe code
Raw pointers `ptr<T>`, pointer arithmetic, `unsafe.cast<T>(x)`, `sizeof<T>()` and `alignof<T>()`
are allowed only inside `unsafe { }` blocks or `unsafe function`s.
```ts
unsafe {
  const p = ptr.of(buffer[0])
  (p + 4).write(0xFF as u8)
}
```
- `ptr.of(place)` points at a variable, a field or an element (not at a class value, which is
  a reference to a managed object: E0308); `ptr.bytes(view)` is a `ptr<u8>` to a `Uint8Array`'s
  or an `ArrayBuffer`'s bytes (`byteOffset` included), for C, valid while the buffer is alive
  and not transferred (native code only); `p.read()` and `p.write(v)`
  access the value; `p + n` / `p - n` move by `n` elements (`n` any integer); pointers compare
  with `==` by address. `sizeof<T>()` / `alignof<T>()` are `usize`s (the native layout, also
  in the interpreter). `unsafe.cast<T>(x)` reinterprets the bits of a number as another number
  type of the same size.
- An `unsafe function` may use all of this in its body; calling one, or using it as a
  value (`const f = poke`, #4), needs `unsafe` too. A
  lambda inside `unsafe` code is unsafe code. Outside it, each operation is an error (E0342);
  `ptr<T>` values may still be stored and passed around.
- Pointers aren't owners: a pointer into an array's storage doesn't keep it alive (pointers are
  traced conservatively, but that's no promise), and growing the array leaves it dangling.
  Pointer arithmetic outside the pointed-to array, and using a dangling pointer, are undefined
  behavior (the interpreter panics where it can tell).
- `@pod struct`s (§9.1) may contain numbers, `bool`, `char`, `ptr<T>`, fixed arrays of those
  and other `@pod` structs (E0343 otherwise); they're laid out like C structs (fields in order,
  natural alignment). Generic `@pod` structs aren't supported yet.

## 9.6 `--no-gc` mode
For embedded targets, compiling with `--no-gc` restricts the language to structs, enums, fixed
arrays, primitives, arenas and `unsafe`. Class allocation, growable arrays, `string`
concatenation and closures that capture by reference are compile errors.
- **Strings**: string literals and `str` (a pointer + length view) are available. Build strings with
  `StrBuf<N>` (a fixed-capacity inline buffer) or allocate them in an arena.
- **Errors**: only lightweight `Throwable` structs/enums can be thrown (§7.1).
