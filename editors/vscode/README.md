# ppl for VS Code

Syntax highlighting for ppl (`.ppl` files and ` ```ppl ` blocks in Markdown): keywords,
built-in types, numbers with suffixes, strings and template strings with `${…}`, comments and
doc comments (`/** */`, `///`), decorators and declarations. Comment toggling, bracket
matching, auto-closing pairs and `/** */` continuation come from the language configuration.

The language server (`ppl lsp`: diagnostics, formatting, hover, go-to-definition, completion,
rename) follows in PLAN P10.6b–d.

## Install from the repository

No build step yet (the extension has no code). Either link it into VS Code's extensions:

```sh
ln -s "$PWD/editors/vscode" ~/.vscode/extensions/padcom.ppl-0.1.0    # then reload VS Code
```

or open `editors/vscode` in VS Code and press F5 (an Extension Development Host).

## Editing the grammar

`syntaxes/ppl.tmLanguage.json` is generated: edit `syntaxes/generate.py`, then run
`python3 editors/vscode/syntaxes/generate.py` from the repository root.
`crates/ppl-tests/tests/vscode.rs` checks that the grammar has every keyword the lexer knows
and that its regular expressions compile.
