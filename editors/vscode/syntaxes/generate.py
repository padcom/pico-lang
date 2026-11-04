# Generates the TextMate grammars (pico.tmLanguage.json, pico.markdown.tmLanguage.json): edit
# this, then run `python3 editors/vscode/syntaxes/generate.py` from the repository root.
# crates/pico-tests/tests/vscode.rs checks the result (keywords, regular expressions).
import json
ID = r"[A-Za-z_$][A-Za-z0-9_$]*"
KW = {
  "keyword.control.pico": "await break case catch continue default do else finally for if loop match return switch tailcall throw try while yield",
  "keyword.control.import.pico": "import export",
  "storage.type.pico": "class const decorator enum extern function interface let struct get set",
  "storage.modifier.pico": "abstract async comptime distinct final immutable override private protected public readonly static throws unsafe using virtual",
  "keyword.operator.expression.pico": "as extends implements in instanceof is new operator typeof where",
}
PRIM = "int number i8 i16 i32 i64 u8 u16 u32 u64 isize usize f32 f64 bool char string void never object c_char c_schar c_uchar c_short c_ushort c_int c_uint c_long c_ulong c_longlong c_ulonglong c_float c_double"
def words(ws): return r"(?<![\w$.])(" + "|".join(ws.split()) + r")(?![\w$])"
esc = {"name": "constant.character.escape.pico", "match": r"\\(x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f]+\}|.)"}
g = {
  "$schema": "https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json",
  "name": "pico",
  "scopeName": "source.pico",
  "fileTypes": ["pi"],
  "patterns": [{"include": "#code"}],
  "repository": {
    "code": {"patterns": [
      {"include": "#comments"}, {"include": "#strings"}, {"include": "#decorators"},
      {"include": "#declarations"}, {"include": "#keywords"}, {"include": "#constants"},
      {"include": "#types"}, {"include": "#numbers"}, {"include": "#calls"},
      {"include": "#operators"}, {"include": "#braces"}, {"include": "#identifiers"}]},
    "comments": {"patterns": [
      {"name": "comment.block.documentation.pico", "begin": r"/\*\*(?!/)", "end": r"\*/",
       "beginCaptures": {"0": {"name": "punctuation.definition.comment.pico"}},
       "endCaptures": {"0": {"name": "punctuation.definition.comment.pico"}}},
      {"name": "comment.block.pico", "begin": r"/\*", "end": r"\*/",
       "beginCaptures": {"0": {"name": "punctuation.definition.comment.pico"}},
       "endCaptures": {"0": {"name": "punctuation.definition.comment.pico"}}},
      {"name": "comment.line.documentation.pico", "match": r"///.*$"},
      {"name": "comment.line.double-slash.pico", "match": r"//.*$"}]},
    "strings": {"patterns": [
      {"name": "string.quoted.double.pico", "begin": '"', "end": r'"|$',
       "patterns": [esc]},
      {"name": "string.quoted.single.pico", "begin": "'", "end": r"'|$",
       "patterns": [esc]},
      {"name": "string.template.pico", "begin": "`", "end": "`",
       "patterns": [esc, {"include": "#interpolation"}]}]},
    "interpolation": {
      "name": "meta.template.expression.pico", "contentName": "meta.embedded.line.pico",
      "begin": r"\$\{", "end": r"\}",
      "beginCaptures": {"0": {"name": "punctuation.definition.template-expression.begin.pico"}},
      "endCaptures": {"0": {"name": "punctuation.definition.template-expression.end.pico"}},
      "patterns": [{"include": "#code"}]},
    "braces": {"begin": r"\{", "end": r"\}", "patterns": [{"include": "#code"}]},
    "decorators": {"match": r"(@)(" + ID + ")",
      "captures": {"1": {"name": "punctuation.decorator.pico"}, "2": {"name": "entity.name.function.decorator.pico"}}},
    "declarations": {"patterns": [
      {"match": r"(?<![\w$])(class|interface|struct|enum|decorator)\s+(" + ID + ")",
       "captures": {"1": {"name": "storage.type.pico"}, "2": {"name": "entity.name.type.pico"}}},
      {"match": r"(?<![\w$])(type)\s+(" + ID + r")(?=\s*(<[^=]*>)?\s*=)",
       "captures": {"1": {"name": "storage.type.pico"}, "2": {"name": "entity.name.type.alias.pico"}}},
      {"match": r"(?<![\w$])(function)(\s*\*)?\s+(" + ID + ")",
       "captures": {"1": {"name": "storage.type.function.pico"}, "2": {"name": "keyword.generator.asterisk.pico"}, "3": {"name": "entity.name.function.pico"}}},
      {"match": r"(?<![\w$])(const|let)\s+(" + ID + ")",
       "captures": {"1": {"name": "storage.type.pico"}, "2": {"name": "variable.other.declaration.pico"}}}]},
    "keywords": {"patterns": [{"name": k, "match": words(v)} for k, v in KW.items()] + [
      # Contextual words (identifiers elsewhere): `for (x of xs)`, `import … from "m"`,
      # `arena { … }`.
      {"name": "keyword.control.pico", "match": r"(?<=\s)of(?=\s)"},
      {"name": "keyword.control.import.pico", "match": r"(?<![\w$.])from(?=\s*[\"'])"},
      {"name": "keyword.control.pico", "match": r"(?<![\w$.])arena(?=\s*\{)"}]},
    "constants": {"patterns": [
      {"name": "constant.language.pico", "match": words("true false null")},
      {"name": "variable.language.pico", "match": words("this super")}]},
    "types": {"patterns": [
      {"name": "support.type.primitive.pico", "match": words(PRIM)},
      {"name": "entity.name.type.pico", "match": r"(?<![\w$])[A-Z][A-Za-z0-9_$]*(?![\w$])"}]},
    "numbers": {"name": "constant.numeric.pico",
      "match": r"(?<![\w$])(0[xX][0-9A-Fa-f_]+|0[oO][0-7_]+|0[bB][01_]+|[0-9][0-9_]*(\.[0-9][0-9_]*)?([eE][+-]?[0-9]+)?)(i8|i16|i32|i64|u8|u16|u32|u64|isize|usize|f32|f64)?(?![\w$])"},
    "calls": {"match": "(" + ID + r")(?=\s*(<[A-Za-z0-9_$<>,\s\[\]?|]*>)?\()",
      "captures": {"1": {"name": "entity.name.function.pico"}}},
    # Last: what the rules above didn't claim (keywords, types and calls win at the same
    # position). Scoped like TypeScript's, so themes color them, also inside `${…}`.
    "identifiers": {"patterns": [
      {"name": "variable.other.property.pico", "match": r"(?<=[^.]\.)" + ID + r"(?![\w$])"},
      {"name": "variable.other.object.pico", "match": ID + r"(?=\s*\??\.(?!\.))"},
      {"name": "variable.other.readwrite.pico", "match": ID}]},
    "operators": {"patterns": [
      {"name": "storage.type.function.arrow.pico", "match": "=>"},
      {"name": "keyword.operator.spread.pico", "match": r"\.\.\.|\.\."},
      {"name": "keyword.operator.optional.pico", "match": r"\?\?=?|\?\."},
      {"name": "keyword.operator.comparison.pico", "match": r"===?|!==?|<=|>="},
      {"name": "keyword.operator.logical.pico", "match": r"&&=?|\|\|=?|!"},
      {"name": "keyword.operator.assignment.pico", "match": r"(\*\*|<<|>>>|>>|[-+*/%&|^])?="},
      {"name": "keyword.operator.arithmetic.pico", "match": r"\+\+|--|\*\*|[-+*/%]"},
      {"name": "keyword.operator.bitwise.pico", "match": r"<<|>>>|>>|[&|^~]"}]},
  },
}
open("editors/vscode/syntaxes/pico.tmLanguage.json", "w").write(json.dumps(g, indent=2) + "\n")
md = {
  "scopeName": "markdown.pico.codeblock",
  "injectionSelector": "L:text.html.markdown",
  "patterns": [{"include": "#pico-code-block"}],
  "repository": {"pico-code-block": {
    "name": "markup.fenced_code.block.markdown",
    "begin": r"(^|\G)(\s*)(`{3,}|~{3,})\s*(?i:(pico)(\s+[^`~]*)?$)",
    "end": r"(^|\G)(\2|\s{0,3})(\3)\s*$",
    "beginCaptures": {"3": {"name": "punctuation.definition.markdown"}, "4": {"name": "fenced_code.block.language.markdown"}},
    "endCaptures": {"3": {"name": "punctuation.definition.markdown"}},
    "patterns": [{"begin": r"(^|\G)(\s*)(.*)", "while": r"(^|\G)(?!\s*([`~]{3,})\s*$)",
                  "contentName": "meta.embedded.block.pico", "patterns": [{"include": "source.pico"}]}]}},
}
open("editors/vscode/syntaxes/pico.markdown.tmLanguage.json", "w").write(json.dumps(md, indent=2) + "\n")
