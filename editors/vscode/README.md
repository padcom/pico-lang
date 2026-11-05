# Pico for VS Code

Support for the [Pico Programming Language](https://padcom.github.io/pico-lang/): TypeScript-style
syntax, compiled through LLVM to native code and WebAssembly.

```ts
reactive count = 0
const doubled = computed(() => count * 2)

const res = await fetch("https://api.example.com/users", {
  headers: { "Accept": "application/json" },
})
```

## Requirements

The extension runs the `pico` compiler, which isn't bundled: install a release from the
[downloads](https://github.com/padcom/pico-lang/releases/latest) (macOS, Linux and Windows) and put
its `bin` directory on your PATH, or point `pico.server.path` at it. Debugging also needs lldb
(`lldb-dap`; Xcode's on macOS).

## Features

- **Syntax highlighting** for `.pi` files and ` ```pico ` blocks in Markdown, with comment toggling,
  bracket matching, auto-closing pairs and `/** */` continuation.
- **Errors and lint warnings as you type**, also those in imported modules.
- **Completion**: names in scope, members after `.`, the names a module exports inside
  `import { … }`.
- **Hover** with the type or signature and the doc comment, also in code that has errors.
- **Go to Definition** (into imported modules and the core library), **Go to Implementations**
  (Ctrl+F12) and **Rename Symbol** (F2: every use in the project, methods with their overrides).
- **Format Document** with the `pico fmt` formatter (options from `project.json`).
- **Outline**, breadcrumbs and Go to Symbol.

## Debugging

Press F5: it debugs the `entry` of the nearest `project.json`, or the open file when there's no
project. The program is built in debug mode (cached while nothing changes) and runs under
lldb-dap. Breakpoints, stepping, the call stack, watches and variables work: strings as text,
arrays, maps and sets with their elements, enums by variant, reactive values with their current
value, and the program's globals. A program that doesn't compile shows the compiler's errors
instead. Run Build Task offers `pico: build`.

A `pico` launch configuration takes `program` (a `.pi` file, a project directory or an
executable), `args`, `cwd`, `env` and `stopOnEntry`.

## Settings

| Setting | Default | |
|---------|---------|---|
| `pico.server.path` | `pico` | the `pico` binary. VS Code started from a desktop launcher may not have your shell's PATH: set an absolute path if the server doesn't start |
| `pico.server.args` | `[]` | arguments before `lsp` |
| `pico.trace.server` | `off` | `messages`/`verbose`: the protocol in the "pico language server" output |

The command **pico: Restart Language Server** restarts it, after updating `pico` for example.

## Links

[Website](https://padcom.github.io/pico-lang/) ·
[Language tour](https://github.com/padcom/pico-lang/blob/main/docs/TOUR.md) ·
[Specification](https://github.com/padcom/pico-lang/tree/main/docs/spec) ·
[Issues](https://github.com/padcom/pico-lang/issues)

## Working on the extension

From the repository: `scripts/vscode-package.sh --install` builds and installs
`target/vscode/pico-<version>.vsix` (it needs Node.js and npm), then reload the window. To debug
the extension itself, open `editors/vscode` and press F5. `syntaxes/pico.tmLanguage.json` is
generated: edit `syntaxes/generate.py` and run `python3 editors/vscode/syntaxes/generate.py`
from the repository root (`crates/pico-tests/tests/vscode.rs` checks it against the lexer).
