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

Details of the rule:
- A line ending with `>`, `>>`, `>>>`, `?`, `!`, `++` or `--` is **complete**, because those
  tokens close generics (`let m: Map<K, V>`), mark nullable types (`let s: string?`) or are
  postfix operators. To split a ternary, start the next line with `?` or `:`.
- Every other binary or assignment operator, `,`, `.`, `?.`, `:` and `=>` at the end of a line
  continues it.
- A line starting with `.`, `?.`, `..`, a binary operator other than `+`/`-`, an assignment
  operator, `?`, `:`, `=>`, `|` (multi-line unions), or `as`/`is`/`instanceof`/`throws`/`where`/
  `extends`/`implements` continues the previous line. That includes `/`: a regex literal can't
  start a line that follows an expression.
- Template substitutions `${ … }` behave like parentheses.
- Where a statement can't end, a newline is simply skipped: after `if (…)`, `while (…)` and
  `for (…)`, before `else`/`catch`/`finally`, after decorators, and inside object literals,
  struct literals and `match` bodies.
- Two statements on one line need a `;`, unless the first ends with `}`
  (`if (x) { … } f()`).

## 1.3 Literals
```ts
42  0xFF  0o17  0b1010  1_000_000       // integer literals
3.14  1e-9  .5                           // float literals
"double"  'single'                       // strings: escapes yes, interpolation no
`template ${expr} and ${x:.2f}`          // only backtick strings interpolate
sql`SELECT * FROM t WHERE id = ${id}`    // tagged template
true  false  null
10u8  300i16  1.5f32  2f64              // literal suffixes
/\d+(\.\d+)?/g                           // regex literal, validated at compile time
                                         // (`/` starts a regex only where an expression can begin)
```
A `/` starts a regex literal unless the previous token ends an expression: an identifier, a
literal, `)`, `]`, `this`, `super`, `null`, `true`, `false`, or a postfix `!`/`++`/`--` after one
of those. After `}`, `/` starts a regex.

Untyped numeric literals take their type from context. With no context, an integer literal is
`int` (i64) and a float literal is `number` (f64).

A one-character string literal has type `char` when the expected type is `char`:
```ts
const c: char = 'a'          // char
const s = 'a'                // string
const e: char = "ab"         // error: more than one character
```

Multiline literals use a tripled delimiter (`'''`, `"""` or ```` ``` ````) and strip indentation.
Leading whitespace common to all lines is removed, as are the first and last line breaks. Only the
backtick form interpolates:
````ts
const help = """
    Usage: ppl <command>
      build   compile the project
    """                        // "Usage: ppl <command>\n  build   compile the project"

const page = ```
    <h1>${title}</h1>
    <p>${body}</p>
    ```
````

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

Rules:
- The spec is parsed and checked against the value's type at **compile time**; an invalid spec
  is an error.
- `d x X o b` need an integer. Negative numbers keep their sign (`${-255:x}` is `-ff`), and `#`
  adds `0x`, `0o` or `0b` (`${255:#X}` is `0xFF`).
- `e E f %` need a number. Without a precision they use the shortest round-trip digits: `e` in
  Rust style (`${1234.5:e}` is `1.2345e3`), `f` and `%` in fixed notation.
- A precision rounds half to even on the exact binary value (`${0.125:.2f}` is `0.12`), as in
  Python and Rust. It isn't allowed on integers (use `.Nf`), and it truncates strings
  (`${"zażółć":.4}` is `zażó`).
- Signs (`+`, ` `) and `0` padding apply only to numbers. Numbers align right by default; strings,
  `char`s and `bool`s align left. Width counts code points.
- `?` shows strings and `char`s quoted, with `\\`, the quote, `\n`, `\r`, `\t`, `\0` and
  `\u{…}` escapes.

A class instance shows its string form (§6.1: `toString()` or `Name (field = value, …)`); its
debug form `${obj:?}` is still `<Name>`. A type can customize how it formats by implementing a
`Formattable` interface (draft name).

## 1.5 Comments
`// line`, `/* block */` (not nested, as in JS), `/** doc comment */`.

## 1.6 Reserved words (draft)
`as async await break case catch class const continue default else enum export extends extern
false finally for function get if implements import in instanceof interface let match new null
operator private protected public readonly return set static struct super switch this throw
throws true try typeof unsafe where while yield comptime immutable tailcall virtual override
abstract final loop using distinct decorator is do`

`type`, `of`, `from` and `arena` are **contextual**: they're keywords only where the grammar
expects them (`type X = …`, `for (x of …)`, `import … from`, `arena { … }`) and ordinary
identifiers everywhere else.
