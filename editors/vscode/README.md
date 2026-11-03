# ppl for VS Code

Syntax highlighting for ppl (`.ppl` files and ` ```ppl ` blocks in Markdown): keywords,
built-in types, numbers with suffixes, strings and template strings with `${…}`, comments and
doc comments (`/** */`, `///`), decorators and declarations. Comment toggling, bracket
matching, auto-closing pairs and `/** */` continuation come from the language configuration.

The language server (`ppl lsp`: diagnostics, formatting, hover, go-to-definition, completion,
rename) follows in PLAN P10.6b–d.

## Install from the repository

```sh
scripts/vscode-package.sh --install    # builds target/vscode/ppl-<version>.vsix, installs it
```

then reload VS Code (Developer: Reload Window). Run it again after changing the extension.
(A symlink into `~/.vscode/extensions` isn't enough: VS Code only loads extensions registered
in its `extensions.json` and marks unknown folders obsolete.) To work on the extension, open
`editors/vscode` in VS Code and press F5 (an Extension Development Host).

## Editing the grammar

`syntaxes/ppl.tmLanguage.json` is generated: edit `syntaxes/generate.py`, then run
`python3 editors/vscode/syntaxes/generate.py` from the repository root.
`crates/ppl-tests/tests/vscode.rs` checks that the grammar has every keyword the lexer knows
and that its regular expressions compile.
