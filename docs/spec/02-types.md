# 2. Types

## 2.1 Primitive types
| Type | Description |
|------|-------------|
| `i8 i16 i32 i64` | signed integers |
| `u8 u16 u32 u64` | unsigned integers |
| `f32 f64` | IEEE-754 floats |
| `isize usize` | pointer-sized integers (FFI and unsafe code) |
| `int` | alias for `i64`; the type of `.length` and indices |
| `number` | alias for `f64` |
| `bool` | `true` / `false` |
| `char` | a Unicode scalar value |
| `string` | immutable UTF-8 text; `.length` counts code points; `s[i]` returns a `char` (O(n) with a cached index); `s.bytes()` gives the UTF-8 bytes |
| `null` | the only "absent" value |
| `unknown` | top type; must be narrowed before use |
| `never` | bottom type |
| `void` | the type of expressions with no useful value |

There is no `any` and no `undefined`.

## 2.2 Numeric rules
- **Implicit conversion only when it's lossless**: `i32 → i64`, `u8 → u16/i16/…`, `f32 → f64`,
  `i32 → f64`, `u32 → f64`. A conversion is lossless when every value of the source type is exactly
  representable in the target.
- Every other conversion needs `as`: `x as i32`. A float→int `as` that's out of range panics.
- **Integer overflow always panics**, in debug and release builds alike. For other behavior there
  are explicit methods: `a.wrappingAdd(b)`, `a.saturatingAdd(b)`, `a.checkedAdd(b): i32 | null`.
- Integer division by zero panics.

```ts
const a: i32 = 10
const b: i64 = a          // ok: widening
const c: i16 = a as i16   // explicit narrowing (panics if out of range)
const d = 255u8 + 1u8     // compile error E0347: constant overflow (decision 219)
function inc(x: u8): u8 { x + 1u8 }
inc(255u8)                // panic: overflow
```

## 2.3 Nominal typing
Two classes with identical shapes are still **different types**. Assignability follows only the
declared `extends`/`implements` relationships.

```ts
class Meters { constructor(public value: f64) {} }
class Feet   { constructor(public value: f64) {} }
const m: Meters = new Feet(3)   // error: Feet is not Meters
```
Object type literals (`{ x: f64, y: f64 }`) are anonymous types. A plain `{ x, y }` literal is
accepted when the expected type is a named struct/class/record type, and it constructs that type.

When the expected type is `Map<string, V>`, an object literal with identifier or string keys
builds that map: `env: { "LANG": "C", PATH: p }`.

## 2.4 Nullability
`T | null` is the only optional type, and `T?` is shorthand for it (`string?`). Narrowing works with `!= null`,
`?.`, `??`, and `!` (a non-null assertion that panics when the value is null).

Narrowing is flow-sensitive and applies to variables: after `if (x == null) return`, or inside
`if (x != null && …)`, `x` has type `T`. Assigning the variable ends the narrowing, and so does a
loop that assigns it. `a?.b.c()` is `null` as a whole when `a` is null. A null prints as `null`
(decision 184).

## 2.5 Unions and literal types
```ts
type Dir = "north" | "south" | "east" | "west"
type Id = i64 | string
function show(id: Id): string {
  if (typeof id == "string") id else id.toString()
}
```
Narrowing works with `typeof`, `instanceof`, `==` against literals, discriminant fields and
`match`.

## 2.6 Enums (sum types)
Payload enums are the primitive sum type:
```ts
enum Shape {
  Circle(r: f64),
  Rect(w: f64, h: f64),
  Empty,
}
const s = Shape.Circle(r: 1)
```
Discriminated object unions are also supported. They lower to an anonymous enum:
```ts
type Event = { kind: "click", x: i32, y: i32 } | { kind: "key", code: u32 }
```
Both are tagged unions in memory and can be matched exhaustively.

## 2.7 Structs (value types)
```ts
struct Vec3 {
  x: f32
  y: f32
  z: f32
  length(): f32 { Math.sqrt(x*x + y*y + z*z) }   // members are implicit, `this.` is optional
}
```
- Structs are stored inline (on the stack, inside arrays, inside other objects) and copied on
  assignment.
- A struct is never GC-allocated on its own; it is boxed only when converted to `unknown` or an
  interface type.
- Structs may contain GC references such as strings or class instances, and the GC traces them
  wherever the struct lives. `@pod struct` restricts a struct to plain data (primitives and other
  pod structs) with a C-compatible layout, for FFI and GPU buffers.
- `==` compares fields unless `equals` is defined.
- Methods receive `this` by reference, so a method that assigns to fields changes the value it
  was called on. Everything else (assignment, arguments, return values) copies.
- `const` only stops the binding from being reassigned: `const p = Point { … }` still allows
  `p.x = 5` and mutating method calls, as with TS objects. Use `immutable T` (§2.11) for a deep
  read-only view.
- A struct literal must give every field exactly once. `${p:?}` shows `Point { x: 1, y: 2 }`;
  structs have no other string form until they implement `Formattable`.
- Structs can implement interfaces but cannot be inherited from.

## 2.8 Tuples and arrays
| Syntax | Meaning |
|--------|---------|
| `T[]` / `Array<T>` | growable, GC-managed array |
| `[T; N]` | fixed-size inline array (value type); `[value; N]` builds one filled with `value` |
| `[A, B]` | tuple (value type) |
| `Map<K, V>`, `Set<T>` | hash collections using `equals`/`hashCode` |

- An array literal's kind comes from the expected type: `const p: [string, i32] = ["a", 1]` is a
  tuple, `const g: [u8; 4] = [1, 2, 3, 4]` a fixed array, otherwise it's a `T[]`. Without an
  expected type the element type comes from the first element that isn't an untyped literal
  (all literals: `int`, or `number` if one is a float), so `[]` needs an annotation.
- Tuples are indexed with a constant (`pair[1]`) or destructured (`const [k, v] = pair`).
  Indexing an array out of bounds panics (`index out of bounds`). `xs.pop()` returns `T?`.
- `a..b` is a `Range` of an integer type with `.start`, `.end` and `.step(n)`; `n` must be
  positive (otherwise a panic), and iteration stops at the end without overflowing (decision 183).

### Equality, hashing, ordering
| Interface | Method | Used by |
|-----------|--------|---------|
| `Equatable<T>` | `equals(o: T): bool` | `==`, `!=` |
| `Hashable` | `hashCode(): u64` | `Map`, `Set` keys |
| `Comparable<T>` | `compareTo(o: T): i32` | `<`, `<=`, `>`, `>=`, `sort` |

Structs, enums and tuples get all three automatically (field by field, in declaration order).
Classes opt in with `@derive(Eq, Hash, Ord)`, or implement the methods by hand. A class without
`equals` compares by identity. `@derive` is an ordinary decorator of the core library
(§11.2) that writes those methods from the class's own fields, so generic code (`Map` keys,
`sort`) and `==`/`<` use them like hand-written ones.

The built-in bound `T extends object` (as in TypeScript) admits class types and arrays, i.e.
references to heap objects: the targets of `WeakRef` and the keys of `WeakMap` (§9.3).
```ts
@derive(Eq, Hash)
class UserId { constructor(public value: i64) {} }
```

## 2.9 Generics
Generics are **monomorphized**: every instantiation is compiled separately.
```ts
function max<T extends Comparable<T>>(a: T, b: T): T { if (a.compareTo(b) >= 0) a else b }

function sum<T>(xs: T[]): T where T: Add<T> & Zero { … }
```

## 2.10 Distinct types
`type Meters = distinct f64` creates a new nominal type with the same representation as `f64` and
no runtime cost. Converting between the two needs `as`.
```ts
type Meters = distinct f64
type Feet = distinct f64
const m: Meters = 3.0 as Meters
const f: Feet = m          // error
```
A plain `type X = Y` is still just an alias.

## 2.11 Immutability
- `const` stops a binding from being reassigned, as in TS.
- `readonly` on a field stops that field from being reassigned (shallow).
- `immutable T` is a **deep** read-only view: no field anywhere in the reachable object graph can be
  mutated through it, and only methods marked `readonly` can be called. The compiler checks that a
  `readonly` method doesn't mutate `this` (deeply) and calls only other `readonly` methods on it.
```ts
class Scene { public readonly count(): int { nodes.length } }
```
```ts
function render(scene: immutable Scene) { scene.nodes.push(x) }  // error
```
