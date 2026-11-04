use super::*;
use crate::token::{Kw, NumSuffix, Punct, TokenKind as T};

fn lex_ok(src: &str) -> Vec<T> {
    let (tokens, diags) = lex(FileId(0), src);
    assert!(
        diags.is_empty(),
        "unexpected diagnostics for {src:?}: {diags:#?}"
    );
    tokens
        .into_iter()
        .map(|t| t.kind)
        .filter(|k| !matches!(k, T::Eof))
        .collect()
}

fn one(src: &str) -> T {
    let v = lex_ok(src);
    assert_eq!(v.len(), 1, "expected one token for {src:?}, got {v:?}");
    v.into_iter().next().unwrap()
}

fn errors(src: &str) -> Vec<String> {
    let (_, diags) = lex(FileId(0), src);
    diags.iter().map(|d| d.code.unwrap().to_string()).collect()
}

fn int(value: u64, suffix: Option<NumSuffix>) -> T {
    T::Int { value, suffix }
}

fn float(value: f64, suffix: Option<NumSuffix>) -> T {
    T::Float { value, suffix }
}

fn p(p: Punct) -> T {
    T::Punct(p)
}

fn id(s: &str) -> T {
    T::Ident(s.into())
}

fn s(s: &str) -> T {
    T::Str(s.into())
}

// ---- P1.4: core tokens ----

#[test]
fn integer_literals() {
    assert_eq!(one("42"), int(42, None));
    assert_eq!(one("0xFF"), int(255, None));
    assert_eq!(one("0o17"), int(15, None));
    assert_eq!(one("0b1010"), int(10, None));
    assert_eq!(one("1_000_000"), int(1_000_000, None));
    assert_eq!(one("0xFF_FF"), int(0xFFFF, None));
    assert_eq!(one("18446744073709551615"), int(u64::MAX, None));
}

#[test]
fn float_literals() {
    assert_eq!(one("2.75"), float(2.75, None));
    assert_eq!(one("1e-9"), float(1e-9, None));
    assert_eq!(one("2.5E+3"), float(2500.0, None));
    assert_eq!(one(".5"), float(0.5, None));
    assert_eq!(one("1_000.25"), float(1000.25, None));
}

#[test]
fn literal_suffixes() {
    assert_eq!(one("10u8"), int(10, Some(NumSuffix::U8)));
    assert_eq!(one("300i16"), int(300, Some(NumSuffix::I16)));
    assert_eq!(one("1.5f32"), float(1.5, Some(NumSuffix::F32)));
    assert_eq!(one("2f64"), float(2.0, Some(NumSuffix::F64)));
    assert_eq!(one("0xFFu8"), int(255, Some(NumSuffix::U8)));
    assert_eq!(one("7usize"), int(7, Some(NumSuffix::Usize)));
}

#[test]
fn numbers_next_to_dots() {
    assert_eq!(
        lex_ok("0..10"),
        vec![int(0, None), p(Punct::DotDot), int(10, None)]
    );
    assert_eq!(
        lex_ok("1.toString"),
        vec![int(1, None), p(Punct::Dot), id("toString")]
    );
    assert_eq!(
        lex_ok("255u8.wrappingAdd"),
        vec![
            int(255, Some(NumSuffix::U8)),
            p(Punct::Dot),
            id("wrappingAdd")
        ]
    );
}

#[test]
fn number_errors() {
    assert_eq!(errors("10q"), vec!["E0004"]);
    assert_eq!(errors("1.5u8"), vec!["E0004"]);
    assert_eq!(errors("0x"), vec!["E0004"]);
    assert_eq!(errors("1e"), vec!["E0004"]);
    assert_eq!(errors("99999999999999999999"), vec!["E0005"]);
}

#[test]
fn strings_and_escapes() {
    assert_eq!(one(r#""double""#), s("double"));
    assert_eq!(one("'single'"), s("single"));
    assert_eq!(one(r#""a\nb\t\\\"\'""#), s("a\nb\t\\\"'"));
    assert_eq!(one(r#""\x41\u{1F642}ą\0""#), s("A🙂ą\0"));
    assert_eq!(one(r#""no ${interp}""#), s("no ${interp}"));
    assert_eq!(one("\"line \\\ncontinued\""), s("line continued"));
    assert_eq!(one("\"\""), s(""));
}

#[test]
fn string_errors() {
    assert_eq!(errors("\"abc"), vec!["E0001"]);
    assert_eq!(errors("'abc\nx'"), vec!["E0001", "E0001"]);
    assert_eq!(errors(r#""\q""#), vec!["E0003"]);
    assert_eq!(errors(r#""\xFF""#), vec!["E0003"]);
    assert_eq!(errors(r#""\u{110000}""#), vec!["E0003"]);
}

#[test]
fn identifiers_and_keywords() {
    assert_eq!(
        lex_ok("const żółw $x _y type of"),
        vec![
            T::Kw(Kw::Const),
            id("żółw"),
            id("$x"),
            id("_y"),
            id("type"),
            id("of")
        ]
    );
    assert_eq!(one("comptime"), T::Kw(Kw::Comptime));
    assert_eq!(one("do"), T::Kw(Kw::Do));
}

#[test]
fn punctuators_longest_match() {
    assert_eq!(
        lex_ok("a >>>= b ?? c ?. d ... e ** f"),
        vec![
            id("a"),
            p(Punct::UShrEq),
            id("b"),
            p(Punct::QQ),
            id("c"),
            p(Punct::QDot),
            id("d"),
            p(Punct::Ellipsis),
            id("e"),
            p(Punct::StarStar),
            id("f"),
        ]
    );
    // `?.` followed by a digit is a ternary.
    assert_eq!(
        lex_ok("a?.5:b"),
        vec![
            id("a"),
            p(Punct::Question),
            float(0.5, None),
            p(Punct::Colon),
            id("b")
        ]
    );
}

#[test]
fn comments_and_newlines() {
    assert_eq!(
        lex_ok("a // c\n\n\nb /* x */ c /* multi\nline */ d /** doc */"),
        vec![id("a"), T::Newline, id("b"), id("c"), T::Newline, id("d")]
    );
    assert_eq!(errors("a /* never closed"), vec!["E0002"]);
    assert_eq!(lex_ok("#!/usr/bin/env pico\nx"), vec![id("x")]);
}

#[test]
fn unexpected_character() {
    assert_eq!(errors("a … b"), vec!["E0006"]);
}

#[test]
fn spans_are_byte_ranges() {
    let (tokens, _) = lex(FileId(0), "ab  \"żó\"");
    assert_eq!((tokens[0].span.start, tokens[0].span.end), (0, 2));
    assert_eq!((tokens[1].span.start, tokens[1].span.end), (4, 10));
}

// ---- P1.5: templates, multiline strings, regex ----

fn head(t: &str) -> T {
    T::TemplateHead(t.into())
}

fn mid(spec: Option<&str>, t: &str) -> T {
    T::TemplateMiddle {
        spec: spec.map(Into::into),
        text: t.into(),
    }
}

fn tail(spec: Option<&str>, t: &str) -> T {
    T::TemplateTail {
        spec: spec.map(Into::into),
        text: t.into(),
    }
}

#[test]
fn simple_templates() {
    assert_eq!(one("`plain`"), T::Template("plain".into()));
    assert_eq!(
        lex_ok("`a${x}b${y}c`"),
        vec![head("a"), id("x"), mid(None, "b"), id("y"), tail(None, "c")]
    );
    assert_eq!(one(r"`\` \${no}`"), T::Template("` ${no}".into()));
    assert_eq!(lex_ok("`$${x}`"), vec![head("$"), id("x"), tail(None, "")]);
}

#[test]
fn nested_templates() {
    assert_eq!(
        lex_ok("`a${`b${c}`}`"),
        vec![
            head("a"),
            head("b"),
            id("c"),
            tail(None, ""),
            tail(None, "")
        ]
    );
    // Braces inside a substitution don't end it.
    assert_eq!(
        lex_ok("`${ {x: 1}.x }`"),
        vec![
            head(""),
            p(Punct::LBrace),
            id("x"),
            p(Punct::Colon),
            int(1, None),
            p(Punct::RBrace),
            p(Punct::Dot),
            id("x"),
            tail(None, ""),
        ]
    );
}

#[test]
fn format_specs() {
    assert_eq!(
        lex_ok("`${x:.2f}`"),
        vec![head(""), id("x"), tail(Some(".2f"), "")]
    );
    assert_eq!(
        lex_ok("`${n:08} and ${s:*^6}!`"),
        vec![
            head(""),
            id("n"),
            mid(Some("08"), " and "),
            id("s"),
            tail(Some("*^6"), "!")
        ]
    );
    assert_eq!(
        lex_ok("`${cents % 100:02}`"),
        vec![
            head(""),
            id("cents"),
            p(Punct::Percent),
            int(100, None),
            tail(Some("02"), "")
        ]
    );
}

#[test]
fn colons_that_are_not_format_specs() {
    // Ternary.
    assert_eq!(
        lex_ok("`${a ? b : c}`"),
        vec![
            head(""),
            id("a"),
            p(Punct::Question),
            id("b"),
            p(Punct::Colon),
            id("c"),
            tail(None, "")
        ]
    );
    // Ternary followed by a spec.
    assert_eq!(
        lex_ok("`${a ? b : c:>4}`"),
        vec![
            head(""),
            id("a"),
            p(Punct::Question),
            id("b"),
            p(Punct::Colon),
            id("c"),
            tail(Some(">4"), ""),
        ]
    );
    // Named argument inside parens.
    assert_eq!(
        lex_ok("`${f(x: 1)}`"),
        vec![
            head(""),
            id("f"),
            p(Punct::LParen),
            id("x"),
            p(Punct::Colon),
            int(1, None),
            p(Punct::RParen),
            tail(None, ""),
        ]
    );
}

#[test]
fn template_errors() {
    assert_eq!(errors("`abc"), vec!["E0007"]);
    assert_eq!(errors("`a${x"), vec!["E0007"]);
}

#[test]
fn dedent_rules() {
    assert_eq!(dedent("\n    a\n      b\n    "), "a\n  b");
    assert_eq!(dedent("\n  a\n\n  b\n"), "a\n\nb");
    assert_eq!(dedent("inline"), "inline");
    assert_eq!(dedent("\n\ta\n\t\tb\n\t"), "a\n\tb");
    assert_eq!(dedent("   "), "");
}

#[test]
fn multiline_strings() {
    let src = "const help = \"\"\"\n    Usage: pico <command>\n      build   compile the project\n    \"\"\"";
    assert_eq!(
        lex_ok(src)[3],
        s("Usage: pico <command>\n  build   compile the project")
    );
    assert_eq!(one("'''\n  a\\tb\n  '''"), s("a\tb"));
    assert_eq!(
        one("\"\"\"one \"quoted\" line\"\"\""),
        s("one \"quoted\" line")
    );
    assert_eq!(errors("\"\"\"\n never closed"), vec!["E0001"]);
}

#[test]
fn multiline_templates() {
    let src = "```\n    <h1>${title}</h1>\n    <p>${body}</p>\n    ```";
    assert_eq!(
        lex_ok(src),
        vec![
            head("<h1>"),
            id("title"),
            mid(None, "</h1>\n<p>"),
            id("body"),
            tail(None, "</p>")
        ]
    );
    assert_eq!(one("```\n  a ` b\n  ```"), T::Template("a ` b".into()));
}

#[test]
fn regex_vs_division() {
    assert_eq!(
        lex_ok("a / b / c"),
        vec![id("a"), p(Punct::Slash), id("b"), p(Punct::Slash), id("c")]
    );
    assert_eq!(
        lex_ok(r"x = /\d+(\.\d+)?/g"),
        vec![
            id("x"),
            p(Punct::Eq),
            T::Regex {
                pattern: r"\d+(\.\d+)?".into(),
                flags: "g".into()
            }
        ]
    );
    assert_eq!(
        lex_ok("f(/[/]x/)"),
        vec![
            id("f"),
            p(Punct::LParen),
            T::Regex {
                pattern: "[/]x".into(),
                flags: "".into()
            },
            p(Punct::RParen)
        ]
    );
    // After `)` or a literal it's division; postfix `!` keeps that.
    assert_eq!(
        lex_ok("(a) / 2 / x! / y"),
        vec![
            p(Punct::LParen),
            id("a"),
            p(Punct::RParen),
            p(Punct::Slash),
            int(2, None),
            p(Punct::Slash),
            id("x"),
            p(Punct::Bang),
            p(Punct::Slash),
            id("y"),
        ]
    );
    // Prefix `!` starts an expression.
    assert_eq!(
        lex_ok("!/a/.test(s)")[..2],
        [
            p(Punct::Bang),
            T::Regex {
                pattern: "a".into(),
                flags: "".into()
            }
        ]
    );
    // A newline doesn't reset the state: `x\n/2` is a division (spec §1.2).
    assert_eq!(
        lex_ok("x\n/ 2"),
        vec![id("x"), T::Newline, p(Punct::Slash), int(2, None)]
    );
    assert_eq!(errors("x = /abc"), vec!["E0008"]);
}
