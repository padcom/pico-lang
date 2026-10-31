# 1. Lexical structure

## 1.1 Source text
- Source files are UTF-8 and use the `.ppl` extension.
- Identifiers follow the TS/JS rules (Unicode ID_Start / ID_Continue, `$`, `_`).

## 1.2 Statement termination
Semicolons are optional. ppl does **not** use JavaScript's automatic semicolon insertion rules.
It uses a simpler newline rule:

> A newline ends a statement **unless** the newline is inside `(` `)` or `[` `]`, the line ends
> with a binary operator, `,`, `.` or `=>`, or the next line starts with `.`, `?.`, or a binary
> operator that cannot start an expression (`&&`, `||`, `??`, `*`, `/`, `|>`-like operators…).

```ts
const total = items
  .filter(i => i.active)
  .map(i => i.price)      // the leading `.` continues the expression

const a = 1; const b = 2  // `;` separates statements on one line
```

Lines that start with `-`, `+`, `(` or `[` begin a **new** statement. The two-line program
`f\n(x)` is therefore two statements, unlike in JavaScript.

## 1.3 Literals
```ts
42  0xFF  0o17  0b1010  1_000_000       // integer literals
3.14  1e-9  .5                           // float literals
"double"  'single'                       // strings (identical meaning)
`template ${expr} and ${x:.2f}`          // template literal with format spec
sql`SELECT * FROM t WHERE id = ${id}`    // tagged template
true  false  null
10u8  300i16  1.5f32  2f64              // literal suffixes
c'a'  c'\u{1F600}'                       // char literal (draft syntax)
```
Untyped numeric literals take their type from context. With no context, an integer literal is
`int` (i64) and a float literal is `number` (f64).

## 1.4 Format specs in templates
Inside `${ … }`, a format spec can follow a `:` at the top level of the expression (a `:` inside
nested parens, ternaries or object literals doesn't count):
`${x:.2f}`, `${n:08d}`, `${n:x}`, `${s:>10}`, `${v:?}` (debug representation).

Grammar (a Python/Rust-style mini-language):
```
spec      := [[fill] align] [sign] ["#"] ["0"] [width] ["." precision] [type]
fill      := any char except '}'
align     := "<" | ">" | "^"
sign      := "+" | "-" | " "
width     := digits
precision := digits
type      := "d" | "x" | "X" | "o" | "b" | "e" | "E" | "f" | "%" | "?"
```
| Example | Output |
|---------|--------|
| `${3.14159:.2f}` | `3.14` |
| `${42:08}` | `00000042` |
| `${255:#x}` | `0xff` |
| `${"hi":*^6}` | `**hi**` |
| `${0.25:.0%}` | `25%` |
| `${point:?}` | `Point { x: 1, y: 2 }` |

A type can customize how it formats by implementing a `Formattable` interface (draft name).

## 1.5 Comments
`// line`, `/* block */`, `/** doc comment */`.

## 1.6 Reserved words (draft)
`as async await break case catch class const continue default else enum export extends extern
false finally for function get if implements import in instanceof interface let match new null
operator private protected public readonly return set static struct super switch this throw
throws true try typeof unsafe where while yield comptime immutable`

## Open questions
- Nested block comments?
- Char literal syntax: `c'a'` (draft) vs something else.
- Raw/multiline string forms beyond template literals?
