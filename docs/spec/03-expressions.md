# 3. Expressions

## 3.1 Everything is an expression
Blocks, `if`, `match`, `switch`, `loop` and `try` all produce values. The value of a block is its
last expression. A block ending in a declaration or a `;`-terminated statement has type `void`.
```ts
const grade = if (score > 90) "A" else if (score > 75) "B" else "C"

const area = {
  const w = 3
  const h = 4
  w * h                      // the block's value: 12
}
```
`return` exits the enclosing function early.

### `if` without `else`
Used as a value, `if (c) e` has type `T?`: it evaluates to `null` when the condition is false.
```ts
const bonus = if (vip) 100    // bonus: int?
```

## 3.2 Object literals
```ts
const p = Point { x: 1, y: 2 }   // explicit type
const q: Point = { x: 1, y: 2 }  // expected type known
const r = Point { ...p, y: 5 }   // spread: every field of `p`, then `y`
```
A spread `...v` in a struct literal takes a value of the same struct type and sets every field;
entries are evaluated left to right and later ones win (`{ x: 9, ...p }` keeps `p.x`), so no
field is missing (decision 313). In a map literal it copies a map's entries (§2.3).
A `{` at the start of a statement always opens a block. Elsewhere, `{` opens an **object
literal** if it's `{}`, starts with `...`, `name:`, `"key":`, `name,` or `[…]:` (a computed
map key, §2.3), or is exactly `{ name }`; otherwise it opens a block (`const area = { const w = 3; w * h }`).

`Name {` (or `Name<A, B> {`) directly after a dotted name, on the same line, is a struct or class
literal.

## 3.3 Operators
In order of precedence, highest first:

| Operators | Notes |
|-----------|-------|
| `.` `?.` `()` `[]` `!` (postfix) | member access, call, index, non-null assert |
| `++` `--` (postfix and prefix), `-` `!` `~` (unary), `await` | |
| `as` | numeric/distinct conversion |
| `**` | right-associative |
| `*` `/` `%` | |
| `+` `-` | |
| `<<` `>>` `>>>` | |
| `..` | exclusive range |
| `<` `<=` `>` `>=` `instanceof` `is` | |
| `==` `!=` | strict only (no `===`) |
| `&` `^` `\|` | |
| `&&` `\|\|` `??` | `??` can't be mixed with `&&`/`\|\|` without parens |
| `?:` | ternary |
| `=` `+=` `-=` `*=` `/=` `%=` `**=` `<<=` `>>=` `&=` `\|=` `^=` `&&=` `\|\|=` `??=` | assignment |

Rows that list several operators keep JavaScript's order inside the row: `||` and `??` bind
loosest, then `&&`; among the bitwise operators `|` is loosest, then `^`, then `&`. Unary
operators bind tighter than `**`, so `-2 ** 2` is `(-2) ** 2`, and `as` sits between them
(`x as i32 + 1` is `(x as i32) + 1`).

`+` with a string on either side concatenates the other side's string form (any value that can
go in a template: numbers, `bool`, `char`, nullables, unions of those, class instances (§6.1)),
so `"n = " + 1` is `"n = 1"` and `1 + 2 + "x"` is `"3x"`. `s += v` does the same. Arrays, structs
and other compound values need an explicit form (`${v:?}`).

`++`/`--` and compound assignment work as in TS. Overflow from `++` panics like any other
integer overflow.

### Operator overloading
Classes and structs can define operators:
```ts
struct Vec2 {
  x: f32
  y: f32
  operator +(o: Vec2): Vec2 { Vec2 { x: x + o.x, y: y + o.y } }
  operator *(k: f32): Vec2 { Vec2 { x: x * k, y: y * k } }
  operator [](i: i32): f32 { if (i == 0) x else y }
  operator -(): Vec2 { Vec2 { x: -x, y: -y } }        // unary
}
```
Overloadable: arithmetic, bitwise, comparisons (through `compareTo`), `==` (through `equals`), `[]` and
`[]=`, and unary `-` `!` `~`. `&&`, `||`, `??`, `=` and `.` can't be overloaded. Compound
assignment (`+=`) is derived from the binary operator.

## 3.4 Equality
`==` calls `equals` when one is defined. Without it, structs, enums and tuples compare field by
field and classes compare by identity. See §2.8.

## 3.5 Pattern matching
### `match` expression
```ts
const desc = match (shape) {
  Circle { r } if r > 10   => "big circle",
  Circle { r }             => `circle ${r}`,
  c as Rect { w, h }       => `rect ${w}x${h} (${area(c)})`,   // binds the whole value as `c`
  Empty                    => "nothing",
}

const kind = match (code) {
  200 | 201 | 204 => "ok",
  300..400        => "redirect",
  _               => "other",
}
```
Patterns:

| Pattern | Example |
|---------|---------|
| literal | `42`, `"north"`, `true`, `null` |
| wildcard | `_` |
| binding | `x` |
| enum variant | `Circle { r }` (named) or `Circle(r)` (positional); both forms are allowed |
| struct/class | `Point { x: 0, y }` (also matches subclasses with `instanceof` semantics) |
| tuple / array | `[a, b]`, `[first, ...rest]` |
| or | `p1 \| p2` |
| range | `0..10` |
| type test | `s: string` |
| as-binding | `name as pattern` |
| guard | `pattern if cond` |

A match must be **exhaustive**. A missing case is a compile error that lists the uncovered
patterns.

### Modernized `switch`
`switch` works the same way, written in statement style. It never falls through, `case` takes
patterns, and a switch whose value is used must be exhaustive. Because `:` ends a case, a
top-level `case x:` is a name pattern, not a type test; write a type test in parentheses,
`case (s: string):`, or use `match`. Any pattern can be parenthesized.
```ts
switch (cmd) {
  case "build" | "b": build()
  case "test":       test()
  case Run { file }: run(file)
  default:           usage()
}
```

### `is` patterns
`expr is Pattern` is a boolean test. Bindings it introduces are in scope where the test is known to
be true:
```ts
if (shape is Circle { r }) println(r)
if (!(x is string)) return
x.toUpperCase()                     // x narrowed to string here
while (queue.next() is Job { id }) process(id)
```

## 3.6 Ternary, optional chaining, nullish
These work as in TS: `c ? a : b`, `a?.b?.c()`, `a ?? b`, `a!`.
