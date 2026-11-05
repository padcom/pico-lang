# 16. Reactivity

Reactive state in the style of Vue: values that know who reads them, values computed from them,
and effects that run again when they change. Decisions 288–293 and 299–309.

```ts
reactive price = 10
reactive qty = 2
const total = computed(() => price * qty)

const log = watchEffect(() => println(`total: ${total}`))   // prints "total: 20" now
qty = 3                                                      // prints "total: 30"
log.dispose()
qty = 4                                                      // prints nothing
```

## 16.1 Reactive declarations
`reactive` declares a binding whose value is held in a **reactive cell**, of type `Reactive<T>`:
```ts
reactive count = 0                  // a local or module-level binding: Reactive<int>
reactive user: User = User.guest()  // with a type, as for let
export reactive theme = "light"     // exported like a let binding (§8)

class Counter {
  reactive count = 0                // a reactive field
  private reactive step = 1
  static reactive instances = 0
}
```
- `reactive` is a **contextual** keyword: it starts a declaration when it's followed by a name at
  the start of a statement or class member, and is an ordinary identifier everywhere else.
- A reactive binding needs an initial value, a reactive field too (E0357): its cell exists
  before the constructor runs, so assigning it in the constructor is a write.
- `T` comes from the type annotation or the initializer, as for `let` (§4). Reactive bindings
  can't be destructuring patterns, `comptime`, or fields of structs (structs are values, §2).

## 16.2 Reading and writing
A **reactive binding** is a `reactive` declaration, or a `const`, parameter or field whose type
is `Reactive<T>` (such as the result of `computed`). Access is **transparent** (decision 288):
- **Naming** a reactive binding (`count`, `this.count`, `counter.count`) reads its value, an
  expression of type `T`. Inside a computed getter or an effect, the read is **tracked**
  (§16.5).
- **Assigning** writes its value: `count = 2`, `count += 1`, `count++`. Assignment to a reactive
  binding always writes the cell and never rebinds it, even for a `const`.
- A write stores the value and then **triggers** what depends on the binding, unless the new
  value is **identical** to the old one (decision 302): the same reference for classes, arrays,
  maps and sets; the same value for primitives and strings; identical fields for structs (never
  through `equals`). Function values are never identical, so writing one always triggers.
- Where the **expected type** is `Reactive<T>`, naming a reactive binding gives the cell itself
  instead of its value (decision 301). That's how reactive state is passed to and returned from
  functions:
```ts
function increment(c: Reactive<int>) { c += 1 }     // c is a reactive binding
function useDouble(c: Reactive<int>): Reactive<int> { return computed(() => c * 2) }

reactive n = 1
increment(n)               // passes the cell: n is now 2
const d = useDouble(n)     // d: Reactive<int>
println(d)                 // 4
const snapshot = n         // no expected type: the value (int), not the cell
const alias: Reactive<int> = n                       // the same cell
```
- Any other expression of type `Reactive<T>` (a call of `computed`, or of a function returning
  `Reactive<T>`) gives the cell; where a `T` is expected, it's read.
- For now, `Reactive<T>` is allowed only as the type of a reactive binding (`const`, parameter,
  field) and as a function's return type: not in `let` bindings (rebinding and writing would be
  ambiguous; E0357), array elements, type arguments, unions or nullables.
- Reactive values don't cross into C, JavaScript (`@export`) or `comptime` code.

## 16.3 `computed`
```ts
reactive first = "Ada"
reactive last = "Lovelace"

const full = computed(() => `${first} ${last}`)     // Reactive<string>, read-only
println(full)                                         // runs the getter: "Ada Lovelace"
println(full)                                         // cached: the getter doesn't run
last = "Byron"                                        // marks full dirty; nothing runs
println(full)                                         // recomputed: "Ada Byron"

const name = computed({
  get: () => `${first} ${last}`,
  set: (v: string) => {
    const parts = v.split(" ")
    first = parts[0]
    last = parts[1]
  },
})
name = "Grace Hopper"                                 // calls set
```
- `computed(getter: () => T): Reactive<T>` and `computed(options: ComputedOptions<T>):
  Reactive<T>`, where `ComputedOptions<T>` is a struct `{ get: () => T, set: (v: T) => void }`
  built from the object literal.
- **Lazy** (decision 291): the getter first runs when the value is first read. Its result is
  cached until something it read changes, which only marks it **dirty**; the getter runs again at
  the next read. Its dependencies are the ones its last run read (they can differ between runs).
- Assigning to a read-only computed is a compile error (E0357) where the binding is a `const`
  initialized with `computed(getter)`; elsewhere (through a parameter, a field) it panics.
  Assigning to a writable one calls `set` with the value; `set`'s own writes trigger as usual.
- A getter must not write reactive state: a write during a getter panics. A computed that reads
  itself while it's computing (directly or through others) panics.
- Getters and setters can't throw (decision 307): their types have no `throws` (§7.2), so a
  getter that calls something throwing catches it itself (E0336 otherwise).

## 16.4 `watchEffect`
```ts
reactive count = 0
const handle = watchEffect(() => println(`count is ${count}`))   // "count is 0", now
count = 1                     // "count is 1", before the assignment completes
handle.dispose()
count = 2                     // nothing

{
  using w = watchEffect(() => save(count))   // stops at the end of the block (§11.5)
}
```
- `watchEffect(fn: () => void): WatchHandle` runs `fn` at once, tracking what it reads, and
  again after every write that triggers it. `WatchHandle` is `Disposable` (decision 293):
  `dispose()` stops the effect and drops its dependencies; disposing twice does nothing, and
  disposing an effect during its own run stops it once the run ends.
- **Synchronous** (decision 291): the effects a write triggers run before the write completes,
  not in a microtask.
- An effect that depends on a computed runs only when the computed's value changes: when the
  computed becomes dirty, it's recomputed first, and the effect runs only if the new value isn't
  identical to the old one (§16.2).
- `fn` is synchronous. An `async` function can be passed, but only what it reads before its
  first `await` is tracked.
- `fn` can't throw (decision 307): an effect runs during whichever write triggers it, where no
  `throws` could be checked, so it catches what it calls itself (E0336 otherwise).

## 16.5 Tracking and triggering
- **Tracking**: a read is tracked while a computed getter or an effect runs, including in the
  functions it calls. Reads anywhere else aren't tracked.
- **Flush**: a write that triggers effects (directly, or through computeds that become dirty)
  queues them. The queue runs before the outermost write completes, in the order the effects were
  created; an effect runs at most once per write that queued it. Writes made while an effect runs
  queue what they trigger: in a flush they join it, and during an effect's first run they're
  flushed right after that run, before `watchEffect` returns.
- An effect isn't triggered by its own writes. Effects that keep triggering each other panic once
  one has run 100 times in a single flush (`reactive effects didn't settle`).
- A **panic** in an effect or a getter ends the program, as anywhere (§7); effects and getters
  can't throw exceptions (§16.3, §16.4).

## 16.6 Deep reactivity
Reactivity is **deep** (decision 290): mutating what reactive state holds triggers what depends
on it, as writing the binding does, through **any reference** to it (decision 299):
```ts
class Todo { constructor(public title: string, public done: bool) {} }

reactive todos: Todo[] = []
const left = computed(() => todos.filter(t => !t.done).length)

function add(list: Todo[], title: string) { list.push(new Todo(title, false)) }

println(left)          // 0
add(todos, "write")    // a plain Todo[] parameter: the push still triggers
println(left)          // 1
todos[0].done = true   // a field of an element triggers too
println(left)          // 0
```
- Reading an object (a class instance), an array or a `Map` while tracking (in a computed
  getter or an effect: a field, an element, `length`, a `Map` lookup, iterating it, an array
  method) makes it a **reactive object** and records a dependency on it. Values read through
  reactive state are such reads, and so is any other object read there (decision 309).
- **Mutating** a reactive object triggers what depends on it, through whichever reference it's
  done: assigning a field or an element (also `+=`, `++`); the arrays' mutating methods (`push`,
  `pop`, `sort`, `reverse`); `Map`'s `set`, `delete` and `clear` (and so `Set`'s `add`,
  `delete` and `clear`, which use a map).
- The granularity is the object: reading any field depends on all of it, and mutating any field
  triggers everything that read it.
- Structs are values: assigning a field of a struct held by a reactive binding (`point.x = 1`)
  writes the binding.
- Only programs whose own code uses reactive state pay for this: there, reads and mutations of
  objects test a flag (about half a nanosecond each); other programs don't (decision 309).

## 16.7 Effect scopes
Effects and computeds stop when the **effect scope** that created them is disposed (decisions
293, 300):
```ts
const scope = effectScope(() => {
  const doubled = computed(() => count * 2)
  watchEffect(() => println(doubled))
})
scope.dispose()            // stops the effect and the computed

using s = effectScope(() => setUpWidgets())       // or with using (§11.5)
```
- `effectScope(fn: () => void): EffectScope` runs `fn` at once. Every `watchEffect` and
  `computed` created while it runs (in the functions it calls too) belongs to the scope, and so
  does every effect scope created in it, which is disposed with it. `EffectScope` is
  `Disposable`.
- An effect or computed created outside any scope lives until it's disposed (effects) or the
  program ends.
- A computed whose scope is disposed stops tracking: each read then runs its getter, without
  caching.

## 16.8 Workers
`Reactive<T>` isn't `Sendable` (§10.3, decision 292): reactive state can't be part of a message to
or from a worker, or of what an inline worker captures. Each worker has its own reactive state,
and tracking never crosses threads. A reactive object sent to a worker arrives as a plain copy.

## 16.9 Library
`computed`, `watchEffect`, `effectScope` and the types `Reactive<T>`, `WatchHandle` and
`EffectScope` are in the core library: available without an import, and shadowed by a
declaration of the same name.
