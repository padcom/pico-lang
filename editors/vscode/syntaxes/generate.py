# Generates the TextMate grammars (ppl.tmLanguage.json, ppl.markdown.tmLanguage.json): edit
# this, then run `python3 editors/vscode/syntaxes/generate.py` from the repository root.
# crates/ppl-tests/tests/vscode.rs checks the result (keywords, regular expressions).
import json
ID = r"[A-Za-z_$][A-Za-z0-9_$]*"
KW = {
  "keyword.control.ppl": "await break case catch continue default do else finally for if loop match return switch tailcall throw try while yield",
  "keyword.control.import.ppl": "import export",
  "storage.type.ppl": "class const decorator enum extern function interface let struct get set",
  "storage.modifier.ppl": "abstract async comptime distinct final immutable override private protected public readonly static throws unsafe using virtual",
  "keyword.operator.expression.ppl": "as extends implements in instanceof is new operator typeof where",
}
PRIM = "int number i8 i16 i32 i64 u8 u16 u32 u64 isize usize f32 f64 bool char string void never object c_char c_schar c_uchar c_short c_ushort c_int c_uint c_long c_ulong c_longlong c_ulonglong c_float c_double"
def words(ws): return r"(?<![\w$.])(" + "|".join(ws.split()) + r")(?![\w$])"
esc = {"name": "constant.character.escape.ppl", "match": r"\\(x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f]+\}|.)"}
g = {
  "$schema": "https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json",
  "name": "ppl",
  "scopeName": "source.ppl",
  "fileTypes": ["ppl"],
  "patterns": [{"include": "#code"}],
  "repository": {
    "code": {"patterns": [
      {"include": "#comments"}, {"include": "#strings"}, {"include": "#decorators"},
      {"include": "#declarations"}, {"include": "#keywords"}, {"include": "#constants"},
      {"include": "#types"}, {"include": "#numbers"}, {"include": "#calls"},
      {"include": "#operators"}, {"include": "#braces"}, {"include": "#identifiers"}]},
    "comments": {"patterns": [
      {"name": "comment.block.documentation.ppl", "begin": r"/\*\*(?!/)", "end": r"\*/",
       "beginCaptures": {"0": {"name": "punctuation.definition.comment.ppl"}},
       "endCaptures": {"0": {"name": "punctuation.definition.comment.ppl"}}},
      {"name": "comment.block.ppl", "begin": r"/\*", "end": r"\*/",
       "beginCaptures": {"0": {"name": "punctuation.definition.comment.ppl"}},
       "endCaptures": {"0": {"name": "punctuation.definition.comment.ppl"}}},
      {"name": "comment.line.documentation.ppl", "match": r"///.*$"},
      {"name": "comment.line.double-slash.ppl", "match": r"//.*$"}]},
    "strings": {"patterns": [
      {"name": "string.quoted.double.ppl", "begin": '"', "end": r'"|$',
       "patterns": [esc]},
      {"name": "string.quoted.single.ppl", "begin": "'", "end": r"'|$",
       "patterns": [esc]},
      {"name": "string.template.ppl", "begin": "`", "end": "`",
       "patterns": [esc, {"include": "#interpolation"}]}]},
    "interpolation": {
      "name": "meta.template.expression.ppl", "contentName": "meta.embedded.line.ppl",
      "begin": r"\$\{", "end": r"\}",
      "beginCaptures": {"0": {"name": "punctuation.definition.template-expression.begin.ppl"}},
      "endCaptures": {"0": {"name": "punctuation.definition.template-expression.end.ppl"}},
      "patterns": [{"include": "#code"}]},
    "braces": {"begin": r"\{", "end": r"\}", "patterns": [{"include": "#code"}]},
    "decorators": {"match": r"(@)(" + ID + ")",
      "captures": {"1": {"name": "punctuation.decorator.ppl"}, "2": {"name": "entity.name.function.decorator.ppl"}}},
    "declarations": {"patterns": [
      {"match": r"(?<![\w$])(class|interface|struct|enum|decorator)\s+(" + ID + ")",
       "captures": {"1": {"name": "storage.type.ppl"}, "2": {"name": "entity.name.type.ppl"}}},
      {"match": r"(?<![\w$])(type)\s+(" + ID + r")(?=\s*(<[^=]*>)?\s*=)",
       "captures": {"1": {"name": "storage.type.ppl"}, "2": {"name": "entity.name.type.alias.ppl"}}},
      {"match": r"(?<![\w$])(function)(\s*\*)?\s+(" + ID + ")",
       "captures": {"1": {"name": "storage.type.function.ppl"}, "2": {"name": "keyword.generator.asterisk.ppl"}, "3": {"name": "entity.name.function.ppl"}}},
      {"match": r"(?<![\w$])(const|let)\s+(" + ID + ")",
       "captures": {"1": {"name": "storage.type.ppl"}, "2": {"name": "variable.other.declaration.ppl"}}}]},
    "keywords": {"patterns": [{"name": k, "match": words(v)} for k, v in KW.items()] + [
      # Contextual words (identifiers elsewhere): `for (x of xs)`, `import … from "m"`,
      # `arena { … }`.
      {"name": "keyword.control.ppl", "match": r"(?<=\s)of(?=\s)"},
      {"name": "keyword.control.import.ppl", "match": r"(?<![\w$.])from(?=\s*[\"'])"},
      {"name": "keyword.control.ppl", "match": r"(?<![\w$.])arena(?=\s*\{)"}]},
    "constants": {"patterns": [
      {"name": "constant.language.ppl", "match": words("true false null")},
      {"name": "variable.language.ppl", "match": words("this super")}]},
    "types": {"patterns": [
      {"name": "support.type.primitive.ppl", "match": words(PRIM)},
      {"name": "entity.name.type.ppl", "match": r"(?<![\w$])[A-Z][A-Za-z0-9_$]*(?![\w$])"}]},
    "numbers": {"name": "constant.numeric.ppl",
      "match": r"(?<![\w$])(0[xX][0-9A-Fa-f_]+|0[oO][0-7_]+|0[bB][01_]+|[0-9][0-9_]*(\.[0-9][0-9_]*)?([eE][+-]?[0-9]+)?)(i8|i16|i32|i64|u8|u16|u32|u64|isize|usize|f32|f64)?(?![\w$])"},
    "calls": {"match": "(" + ID + r")(?=\s*(<[A-Za-z0-9_$<>,\s\[\]?|]*>)?\()",
      "captures": {"1": {"name": "entity.name.function.ppl"}}},
    # Last: what the rules above didn't claim (keywords, types and calls win at the same
    # position). Scoped like TypeScript's, so themes color them, also inside `${…}`.
    "identifiers": {"patterns": [
      {"name": "variable.other.property.ppl", "match": r"(?<=[^.]\.)" + ID + r"(?![\w$])"},
      {"name": "variable.other.object.ppl", "match": ID + r"(?=\s*\??\.(?!\.))"},
      {"name": "variable.other.readwrite.ppl", "match": ID}]},
    "operators": {"patterns": [
      {"name": "storage.type.function.arrow.ppl", "match": "=>"},
      {"name": "keyword.operator.spread.ppl", "match": r"\.\.\.|\.\."},
      {"name": "keyword.operator.optional.ppl", "match": r"\?\?=?|\?\."},
      {"name": "keyword.operator.comparison.ppl", "match": r"===?|!==?|<=|>="},
      {"name": "keyword.operator.logical.ppl", "match": r"&&=?|\|\|=?|!"},
      {"name": "keyword.operator.assignment.ppl", "match": r"(\*\*|<<|>>>|>>|[-+*/%&|^])?="},
      {"name": "keyword.operator.arithmetic.ppl", "match": r"\+\+|--|\*\*|[-+*/%]"},
      {"name": "keyword.operator.bitwise.ppl", "match": r"<<|>>>|>>|[&|^~]"}]},
  },
}
open("editors/vscode/syntaxes/ppl.tmLanguage.json", "w").write(json.dumps(g, indent=2) + "\n")
md = {
  "scopeName": "markdown.ppl.codeblock",
  "injectionSelector": "L:text.html.markdown",
  "patterns": [{"include": "#ppl-code-block"}],
  "repository": {"ppl-code-block": {
    "name": "markup.fenced_code.block.markdown",
    "begin": r"(^|\G)(\s*)(`{3,}|~{3,})\s*(?i:(ppl)(\s+[^`~]*)?$)",
    "end": r"(^|\G)(\2|\s{0,3})(\3)\s*$",
    "beginCaptures": {"3": {"name": "punctuation.definition.markdown"}, "4": {"name": "fenced_code.block.language.markdown"}},
    "endCaptures": {"3": {"name": "punctuation.definition.markdown"}},
    "patterns": [{"begin": r"(^|\G)(\s*)(.*)", "while": r"(^|\G)(?!\s*([`~]{3,})\s*$)",
                  "contentName": "meta.embedded.block.ppl", "patterns": [{"include": "source.ppl"}]}]}},
}
open("editors/vscode/syntaxes/ppl.markdown.tmLanguage.json", "w").write(json.dumps(md, indent=2) + "\n")
