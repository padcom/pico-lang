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
            out.push(above(l).trim_start_matches('/').trim_start().to_string());
        }
        out.reverse();
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
            let t = t.strip_prefix('*').map(str::trim_start).unwrap_or(t);
            out.push(t.to_string());
        }
        while out.first().is_some_and(|s| s.is_empty()) {
            out.remove(0);
        }
        while out.last().is_some_and(|s| s.is_empty()) {
            out.pop();
        }
    }
    out.join("\n")
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
}
