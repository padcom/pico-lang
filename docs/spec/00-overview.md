# 0. Overview

The Pico Programming Language (pico) is a statically and nominally typed language that uses a
garbage collector and borrows its syntax from TypeScript. Programs are compiled ahead of time
through LLVM to native executables or WebAssembly modules.

## Design principles
1. **Familiar first.** If TypeScript already has a sensible spelling for something, pico uses it.
2. **Sound.** No `any`, no `undefined`, no implicit lossy conversions, checked exceptions.
3. **Native performance.** Sized numbers, inline `struct`s, monomorphized generics, and integer
   overflow that traps instead of silently wrapping.
4. **No domain bias.** CLI tools, servers, games and the browser are all first-class.
5. **One tool.** `pico` builds, runs, tests, formats and manages packages.

## A taste
```ts
import { readFile } from "std/fs"

struct Vec2 {
  x: f32
  y: f32
  operator +(o: Vec2): Vec2 { Vec2 { x: x + o.x, y: y + o.y } }
}

enum Shape {
  Circle(r: f64),
  Rect(w: f64, h: f64),
}

function area(s: Shape): f64 {
  match (s) {
    Circle { r } => Math.PI * r * r,
    Rect { w, h } => w * h,
  }
}

async function loadConfig(path: string): Promise<Config> throws IoError | JsonError {
  const text = await readFile(path)
  JSON.parse<Config>(text)
}

println(`area = ${area(Shape.Circle(r: 2)):.2f}`)
```

## Chapters
| # | Chapter |
|---|---------|
| 01 | [Lexical structure](01-lexical.md) |
| 02 | [Types](02-types.md) |
| 03 | [Expressions](03-expressions.md) |
| 04 | [Statements & declarations](04-statements-and-declarations.md) |
| 05 | [Functions](05-functions.md) |
| 06 | [Classes & interfaces](06-classes-interfaces.md) |
| 07 | [Errors](07-errors.md) |
| 08 | [Modules & packages](08-modules-packages.md) |
| 09 | [Memory & GC](09-memory-gc.md) |
| 10 | [Concurrency](10-concurrency.md) |
| 11 | [Comptime, decorators, reflection](11-comptime-decorators-reflection.md) |
| 12 | [Interop](12-interop.md) |
| 13 | [Standard library](13-stdlib.md) |
| 14 | [Tooling & CLI](14-tooling-cli.md) |
| 15 | [Targets](15-targets.md) |
