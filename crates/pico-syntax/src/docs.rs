//! Doc comments and the comments and decorators attached to a declaration, read from the
//! source text above it (the parser doesn't keep comments). Used by the language server's
//! hovers and by the lint (spec §14.7).

/// The doc comment above the declaration that starts on the line of byte `offset` of `text`
/// (`/** … */` or `///` lines; decorators in between are skipped), as Markdown, or `""`.
pub fn doc_comment(text: &str, offset: u32) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut line = line_of(text, offset);
    let above = |l: usize| lines[l].trim();
    while line > 0 && above(line - 1).starts_with('@') {
        line -= 1;
    }
    if line == 0 {
        return String::new();
    }
    let mut out: Vec<String> = Vec::new();
    let prev = above(line - 1);
    if prev.starts_with("///") {
        let mut l = line;
        while l > 0 && above(l - 1).starts_with("///") {
            l -= 1;
            out.push(above(l).trim_start_matches('/').to_string());
        }
        out.reverse();
        return unindent(out);
    } else if prev.ends_with("*/") {
        let mut l = line;
        let mut block = Vec::new();
        loop {
            if l == 0 {
                return String::new();
            }
            l -= 1;
            let t = above(l);
            block.push(t);
            if t.starts_with("/**") {
                break;
            }
            if t.starts_with("/*") || (l + 1 < line && t.ends_with("*/")) {
                return String::new();
            }
        }
        block.reverse();
        for t in block {
            let t = t.strip_prefix("/**").unwrap_or(t);
            let t = t.strip_suffix("*/").unwrap_or(t).trim();
            let t = t.strip_prefix('*').unwrap_or(t);
            out.push(t.to_string());
        }
        // (The block's own lines were trimmed: what's after each `*` is all that's left.)
        return unindent(out);
    }
    out.join("\n")
}

/// A doc comment's lines (what follows each `///` or `*`) as Markdown: outside code fences
/// each line is trimmed; inside them only the one space after the prefix goes, so the code
/// keeps its indentation. Blank lines at either end are dropped.
fn unindent(lines: Vec<String>) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut fenced = false;
    for l in lines {
        let t = l.trim();
        if t.starts_with("```") {
            fenced = !fenced;
            out.push(t.to_string());
        } else if fenced {
            out.push(l.strip_prefix(' ').unwrap_or(&l).trim_end().to_string());
        } else {
            out.push(t.to_string());
        }
    }
    while out.first().is_some_and(|s| s.is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|s| s.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// A usage example in a doc comment: `@example` (with an optional caption after it) and the
/// fenced code block that follows it (decision 348).
#[derive(Clone, Debug, PartialEq)]
pub struct Example {
    pub caption: String,
    pub code: String,
}

/// A doc comment without its examples, and the examples. An `@example` takes the code fence
/// after it; one without a fence takes the lines up to the next tag or blank line.
pub fn split_examples(md: &str) -> (String, Vec<Example>) {
    let lines: Vec<&str> = md.split('\n').collect();
    let mut rest: Vec<&str> = Vec::new();
    let mut examples = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(caption) = lines[i].strip_prefix("@example") else {
            rest.push(lines[i]);
            i += 1;
            continue;
        };
        let caption = caption.trim().to_string();
        i += 1;
        let mut code = Vec::new();
        if lines.get(i).is_some_and(|l| l.starts_with("```")) {
            i += 1;
            while i < lines.len() && !lines[i].starts_with("```") {
                code.push(lines[i]);
                i += 1;
            }
            i += 1;
        } else {
            while i < lines.len() && !lines[i].is_empty() && !lines[i].starts_with('@') {
                code.push(lines[i]);
                i += 1;
            }
        }
        examples.push(Example {
            caption,
            code: code.join("\n"),
        });
    }
    while rest.last().is_some_and(|s| s.is_empty()) {
        rest.pop();
    }
    (rest.join("\n"), examples)
}

/// A doc comment for a hover or a completion: each example under an **Example** heading, as a
/// `ts` code block.
pub fn for_display(md: &str) -> String {
    let (text, examples) = split_examples(md);
    let mut out = text;
    for e in examples {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("**Example**");
        if !e.caption.is_empty() {
            out.push_str(": ");
            out.push_str(&e.caption);
        }
        out.push_str("\n```ts\n");
        out.push_str(&e.code);
        out.push_str("\n```");
    }
    out
}

/// The first line (0-based) of what belongs to the declaration or statement starting on the
/// line of `offset`: the comments and decorators directly above it, and its own line.
pub fn attached_start_line(text: &str, offset: u32) -> usize {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut line = line_of(text, offset);
    loop {
        if line == 0 {
            return 0;
        }
        let t = lines[line - 1].trim();
        if t.starts_with('@') || t.starts_with("//") {
            line -= 1;
        } else if t.ends_with("*/") {
            // A block comment: up to its start.
            let mut l = line - 1;
            while l > 0 && !lines[l].contains("/*") {
                l -= 1;
            }
            line = l;
        } else {
            return line;
        }
    }
}

/// The 0-based line of byte `offset`.
pub fn line_of(text: &str, offset: u32) -> usize {
    let end = (offset as usize).min(text.len());
    text.as_bytes()[..end]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_comments_and_attached_lines() {
        let t = "const a = 1\n\n/**\n * Adds.\n */\n@inline\nfunction add() {}\n/// One.\n/// Two.\nfunction b() {}\n// plain\nfunction c() {}\n";
        let at = |s: &str| t.find(s).unwrap() as u32;
        assert_eq!(doc_comment(t, at("function add")), "Adds.");
        assert_eq!(doc_comment(t, at("function b")), "One.\nTwo.");
        assert_eq!(doc_comment(t, at("function c")), "");
        assert_eq!(doc_comment(t, at("const a")), "");
        assert_eq!(attached_start_line(t, at("function add")), 2);
        assert_eq!(attached_start_line(t, at("function c")), 10);
        assert_eq!(attached_start_line(t, at("function b")), 7);
        assert_eq!(attached_start_line(t, at("const a")), 0);
    }

    #[test]
    fn examples_keep_their_indentation() {
        let t = "/// Joins.\n///\n/// @example Two parts\n/// ```ts\n/// if (ok) {\n///   println(join(\"a\", \"b\"))\n/// }\n/// ```\nfunction join() {}\n/**\n * Splits.\n *\n * @example\n * ```ts\n * for (const p of split(\"a/b\")) {\n *   println(p)\n * }\n * ```\n */\nfunction split() {}\n";
        let at = |s: &str| t.find(s).unwrap() as u32;
        let join = doc_comment(t, at("function join"));
        let (text, ex) = split_examples(&join);
        assert_eq!(text, "Joins.");
        assert_eq!(
            ex,
            [Example {
                caption: "Two parts".into(),
                code: "if (ok) {\n  println(join(\"a\", \"b\"))\n}".into()
            }]
        );
        let (text, ex) = split_examples(&doc_comment(t, at("function split")));
        assert_eq!(text, "Splits.");
        assert_eq!(
            ex[0].code,
            "for (const p of split(\"a/b\")) {\n  println(p)\n}"
        );
        assert_eq!(
            for_display(&join),
            "Joins.\n\n**Example**: Two parts\n```ts\nif (ok) {\n  println(join(\"a\", \"b\"))\n}\n```"
        );
        // Without a fence: the lines up to a blank line.
        let (text, ex) = split_examples("Adds.\n\n@example\nadd(1, 2)\n\nMore.");
        assert_eq!(text, "Adds.\n\n\nMore.");
        assert_eq!(ex[0].code, "add(1, 2)");
    }
}
