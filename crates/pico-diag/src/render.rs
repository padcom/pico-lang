//! Plain-text rendering of diagnostics in the layout of spec §14.2.
//!
//! ```text
//! error[E0308]: mismatched types
//!   ┌─ src/main.pi:4:19
//!   │
//! 4 │ const m: Meters = new Feet(3)
//!   │          ------   ^^^^^^^^^^^ expected `Meters`, found `Feet`
//!   │          │
//!   │          expected because of this annotation
//!   │
//!   = note: …
//! ```

use crate::{Diagnostic, FileId, Label, SourceMap};
use std::collections::BTreeMap;
use std::fmt::Write;

/// Renders one diagnostic. The output ends with a newline.
pub fn render(diag: &Diagnostic, sm: &SourceMap) -> String {
    let mut out = String::new();
    match diag.code {
        Some(code) => writeln!(
            out,
            "{}[{}]: {}",
            diag.severity.as_str(),
            code,
            diag.message
        ),
        None => writeln!(out, "{}: {}", diag.severity.as_str(), diag.message),
    }
    .unwrap();

    // Group labels by file, the main span's file first.
    let main_file = diag.main_span().map(|s| s.file);
    let mut files: Vec<FileId> = Vec::new();
    if let Some(f) = main_file {
        files.push(f);
    }
    for l in &diag.labels {
        if !files.contains(&l.span.file) {
            files.push(l.span.file);
        }
    }

    // The gutter is as wide as the largest line number shown anywhere in the diagnostic.
    let max_line = diag
        .labels
        .iter()
        .map(|l| sm.file(l.span.file).line_col(l.span.start).0)
        .max()
        .unwrap_or(1);
    let width = max_line.to_string().len();
    let pad = " ".repeat(width + 1);

    for file in files {
        let labels: Vec<&Label> = diag.labels.iter().filter(|l| l.span.file == file).collect();
        render_file(&mut out, sm, file, &labels, diag, width, &pad);
    }

    if !diag.labels.is_empty() && (!diag.notes.is_empty() || !diag.help.is_empty()) {
        writeln!(out, "{pad}│").unwrap();
    }
    for n in &diag.notes {
        writeln!(out, "{pad}= note: {}", indent_continuation(n, &pad)).unwrap();
    }
    for h in &diag.help {
        writeln!(out, "{pad}= help: {}", indent_continuation(h, &pad)).unwrap();
    }
    out
}

/// Renders several diagnostics separated by blank lines.
pub fn render_all(diags: &[Diagnostic], sm: &SourceMap) -> String {
    diags
        .iter()
        .map(|d| render(d, sm))
        .collect::<Vec<_>>()
        .join("\n")
}

fn indent_continuation(text: &str, pad: &str) -> String {
    text.replace('\n', &format!("\n{pad}        "))
}

/// A label resolved to its line and character columns (0-based columns).
struct Placed<'a> {
    label: &'a Label,
    start_col: usize,
    end_col: usize,
}

fn render_file(
    out: &mut String,
    sm: &SourceMap,
    file: FileId,
    labels: &[&Label],
    diag: &Diagnostic,
    width: usize,
    pad: &str,
) {
    let src = sm.file(file);
    // Location: the main span if it's in this file, else the first label here.
    let loc_span = diag
        .main_span()
        .filter(|s| s.file == file)
        .unwrap_or(labels[0].span);
    let (line, col) = src.line_col(loc_span.start);
    writeln!(out, "{pad}┌─ {}:{}:{}", src.name, line, col).unwrap();
    writeln!(out, "{pad}│").unwrap();

    let mut by_line: BTreeMap<usize, Vec<Placed>> = BTreeMap::new();
    for &label in labels {
        let line = src.line_index(label.span.start);
        let line_start = src.line_start(line);
        let text = src.line_text(line);
        let start_byte = (label.span.start - line_start) as usize;
        let end_byte =
            ((label.span.end.max(label.span.start) - line_start) as usize).min(text.len());
        let start_col = char_col(text, start_byte);
        let end_col = char_col(text, end_byte.max(start_byte)).max(start_col + 1);
        by_line.entry(line).or_default().push(Placed {
            label,
            start_col,
            end_col,
        });
    }

    let mut prev: Option<usize> = None;
    for (line, mut placed) in by_line {
        if let Some(p) = prev {
            if line > p + 2 {
                writeln!(out, "{pad}·").unwrap();
            } else if line == p + 2 {
                write_source_line(out, src.line_text(p + 1), p + 2, width);
            }
        }
        write_source_line(out, src.line_text(line), line + 1, width);
        placed.sort_by_key(|p| (p.start_col, p.end_col));
        write_annotations(out, &placed, pad);
        prev = Some(line);
    }
}

fn char_col(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    text[..byte].chars().count()
}

fn write_source_line(out: &mut String, text: &str, number: usize, width: usize) {
    let text = text.replace('\t', " ");
    if text.is_empty() {
        writeln!(out, "{number:>width$} │").unwrap();
    } else {
        writeln!(out, "{number:>width$} │ {text}").unwrap();
    }
}

fn write_annotations(out: &mut String, placed: &[Placed], pad: &str) {
    // Line 1: all underlines, plus the rightmost label's message inline.
    let mut canvas: Vec<char> = Vec::new();
    for p in placed {
        let ch = if p.label.primary { '^' } else { '-' };
        if canvas.len() < p.end_col {
            canvas.resize(p.end_col, ' ');
        }
        for c in &mut canvas[p.start_col..p.end_col] {
            *c = ch;
        }
    }
    let (last, rest) = placed.split_last().expect("at least one label");
    let mut line: String = canvas.iter().collect();
    if !last.label.message.is_empty() {
        line.push(' ');
        line.push_str(&last.label.message);
    }
    writeln!(out, "{pad}│ {}", line.trim_end()).unwrap();

    // The other labels hang below on `│` connectors, right to left.
    let hanging: Vec<&Placed> = rest
        .iter()
        .filter(|p| !p.label.message.is_empty())
        .collect();
    if hanging.is_empty() {
        return;
    }
    let connectors = |upto: usize| -> Vec<char> {
        let mut c: Vec<char> = Vec::new();
        for p in &hanging[..upto] {
            if c.len() <= p.start_col {
                c.resize(p.start_col + 1, ' ');
            }
            c[p.start_col] = '│';
        }
        c
    };
    let all: String = connectors(hanging.len()).iter().collect();
    writeln!(out, "{pad}│ {}", all.trim_end()).unwrap();
    for i in (0..hanging.len()).rev() {
        let mut c = connectors(i);
        let p = hanging[i];
        if c.len() < p.start_col {
            c.resize(p.start_col, ' ');
        }
        c.truncate(p.start_col);
        let mut s: String = c.iter().collect();
        s.push_str(&p.label.message);
        writeln!(out, "{pad}│ {s}").unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Diagnostic, Span, codes};

    fn span_of(sm: &SourceMap, file: FileId, needle: &str) -> Span {
        let start = sm.file(file).text.find(needle).expect("needle") as u32;
        Span::new(file, start, start + needle.len() as u32)
    }

    #[test]
    fn spec_14_2_example() {
        let mut sm = SourceMap::new();
        let f = sm.add(
            "src/main.pi",
            "class Meters { constructor(public value: f64) {} }\n\
             class Feet   { constructor(public value: f64) {} }\n\
             \n\
             const m: Meters = new Feet(3)\n",
        );
        let d = Diagnostic::error(codes::MISMATCHED_TYPES, "mismatched types")
            .primary(
                span_of(&sm, f, "new Feet(3)"),
                "expected `Meters`, found `Feet`",
            )
            .secondary(
                span_of(&sm, f, "Meters ="),
                "expected because of this annotation",
            );
        // Trim the secondary label to just `Meters`.
        let mut d = d;
        d.labels[1].span.end -= 2;
        let d = d
            .note("`Meters` and `Feet` have the same shape, but pico types are nominal")
            .help("convert explicitly: `Meters.fromFeet(new Feet(3))`");
        insta::assert_snapshot!(render(&d, &sm));
    }

    #[test]
    fn three_labels_on_one_line_and_gap() {
        let mut sm = SourceMap::new();
        let f = sm.add("a.pi", "let a = f(x, y, z)\n\n\n\n\nconst b = 1\n");
        let d = Diagnostic::error(codes::UNEXPECTED_TOKEN, "demo")
            .secondary(span_of(&sm, f, "x"), "first")
            .secondary(span_of(&sm, f, "y"), "second")
            .primary(span_of(&sm, f, "z"), "third")
            .secondary(span_of(&sm, f, "b"), "later");
        insta::assert_snapshot!(render(&d, &sm));
    }

    #[test]
    fn no_labels_and_no_code() {
        let sm = SourceMap::new();
        let d = Diagnostic::warning("something odd").note("just a note");
        assert_eq!(
            render(&d, &sm),
            "warning: something odd\n  = note: just a note\n"
        );
    }

    #[test]
    fn empty_span_at_end_of_line() {
        let mut sm = SourceMap::new();
        let f = sm.add("e.pi", "const s = \"abc\n");
        let end = sm.file(f).line_text(0).len() as u32;
        let d = Diagnostic::error(codes::UNTERMINATED_STRING, "unterminated string literal")
            .primary(Span::new(f, 10, end), "string starts here");
        insta::assert_snapshot!(render(&d, &sm));
    }
}
