# 2. Types

## 2.1 Primitive types
| Type | Description |
|------|-------------|
| `i8 i16 i32 i64` | signed integers |
| `u8 u16 u32 u64` | unsigned integers |
| `f32 f64` | IEEE-754 floats |
| `int` | alias for `i64` |
| `number` | alias for `f64` |
| `bool` | `true` / `false` |
| `char` | a Unicode scalar value |
| `string` | immutable UTF-8 text; `.length` counts code points |
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
const d = 255u8 + 1u8     // panic: overflow
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

## 2.4 Nullability
`T | null` is the only optional type, and `T?` is shorthand for it (`string?`). Narrowing works with `!= null`,
`?.`, `??`, and `!` (a non-null assertion that panics when the value is null).

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
- Structs can implement interfaces but cannot be inherited from.

## 2.8 Tuples and arrays
| Syntax | Meaning |
|--------|---------|
| `T[]` / `Array<T>` | growable, GC-managed array |
| `[T; N]` | fixed-size inline array (value type) |
| `[A, B]` | tuple (value type) |
| `Map<K, V>`, `Set<T>` | hash collections using `equals`/`hashCode` |

### Equality, hashing, ordering
| Interface | Method | Used by |
|-----------|--------|---------|
| `Equatable<T>` | `equals(o: T): bool` | `==`, `!=` |
| `Hashable` | `hashCode(): u64` | `Map`, `Set` keys |
| `Comparable<T>` | `compareTo(o: T): i32` | `<`, `<=`, `>`, `>=`, `sort` |

Structs, enums and tuples get all three automatically (field by field, in declaration order).
Classes opt in with `@derive(Eq, Hash, Ord)`, or implement the methods by hand. A class without
`equals` compares by identity.
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
  mutated through it, and only methods marked `immutable`/`readonly` can be called.
```ts
function render(scene: immutable Scene) { scene.nodes.push(x) }  // error
```

## Open questions
- Exact syntax for the `immutable` modifier on methods (`readonly foo()`?).
