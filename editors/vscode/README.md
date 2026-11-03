# ppl for VS Code

- **Syntax highlighting** for `.ppl` files and ` ```ppl ` blocks in Markdown: keywords, built-in
  types, numbers with suffixes, strings and template strings with `${…}`, comments and doc
  comments (`/** */`, `///`), decorators and declarations. Comment toggling, bracket matching,
  auto-closing pairs and `/** */` continuation come from the language configuration.
- **The language server** (`ppl lsp`): errors and warnings as you type (also those in imported
  modules), Format Document (the `ppl fmt` formatter with `project.json`'s options), hover
  (the type or signature and the doc comment) and Go to Definition (also into imported modules
  and the core library), also in code with errors, and completion (names in scope, members
  after `.`, the names a module exports inside `import { … }`), Go to Implementations (Ctrl+F12: the types implementing an interface or extending a class, the methods implementing or overriding a method, in the whole project), Rename Symbol (F2: every use in
  the project's modules, methods with their overrides and implementations) and the outline
  (document symbols, also for Go to Symbol and breadcrumbs). Lint warnings (`ppl lint`'s, spec
  §14.7, configured in `project.json`'s `"lint"`) appear with the errors.

## Debugging

Press F5 in a `.ppl` file (or add a `ppl` launch configuration: `program` is the `.ppl` file,
also `args`, `cwd`, `env`, `stopOnEntry`). The extension starts `ppl dap`, which builds the
program (a debug build, cached while nothing changes) and runs it under lldb-dap, so lldb must
be installed (`lldb-dap`, or a versioned `lldb-dap-23` as apt.llvm.org installs it; Xcode's on
macOS; `PPL_LLDB_DAP` names another). Breakpoints, stepping, the call stack and variables work;
strings show as text, arrays and maps and sets with their elements, enums by variant. A program
that doesn't compile shows the compiler's errors instead of starting.

## Settings

| Setting | Default | |
|---------|---------|---|
| `ppl.server.path` | `ppl` | the ppl binary (looked up on PATH: VS Code started from a desktop launcher may not have your shell's PATH, so set an absolute path if the server doesn't start) |
| `ppl.server.args` | `[]` | arguments before `lsp` |
| `ppl.trace.server` | `off` | `messages`/`verbose`: the protocol in the "ppl language server" output |

The command **ppl: Restart Language Server** restarts it (after rebuilding `ppl`, say).

## Install from the repository

```sh
scripts/vscode-package.sh --install    # builds target/vscode/ppl-<version>.vsix, installs it
```

then reload VS Code (Developer: Reload Window). Run it again after changing the extension. It
installs the npm dependencies (`vscode-languageclient`) from `package-lock.json` first, so it
needs Node.js and npm.
(A symlink into `~/.vscode/extensions` isn't enough: VS Code only loads extensions registered
in its `extensions.json` and marks unknown folders obsolete.) To work on the extension, open
`editors/vscode` in VS Code and press F5 (an Extension Development Host).

## Editing the grammar

`syntaxes/ppl.tmLanguage.json` is generated: edit `syntaxes/generate.py`, then run
`python3 editors/vscode/syntaxes/generate.py` from the repository root.
`crates/ppl-tests/tests/vscode.rs` checks that the grammar has every keyword the lexer knows
and that its regular expressions compile.
