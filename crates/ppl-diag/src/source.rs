//! Source files, file ids, spans and line/column lookup.

use std::fmt;

/// Identifies a file inside a [`SourceMap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u32);

/// A half-open byte range `start..end` inside one file.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(file: FileId, start: u32, end: u32) -> Span {
        debug_assert!(start <= end, "span start {start} > end {end}");
        Span { file, start, end }
    }

    /// The smallest span covering both `self` and `other` (same file).
    pub fn to(self, other: Span) -> Span {
        Span::new(
            self.file,
            self.start.min(other.start),
            self.end.max(other.end),
        )
    }

    /// An empty span at the start of `self`.
    pub fn shrink_to_lo(self) -> Span {
        Span::new(self.file, self.start, self.start)
    }

    /// An empty span at the end of `self`.
    pub fn shrink_to_hi(self) -> Span {
        Span::new(self.file, self.end, self.end)
    }

    pub fn len(self) -> u32 {
        self.end - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

/// One source file: its display name, text and line index.
pub struct SourceFile {
    pub name: String,
    pub text: String,
    line_starts: Vec<u32>,
}

impl SourceFile {
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> SourceFile {
        let text = text.into();
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        SourceFile {
            name: name.into(),
            text,
            line_starts,
        }
    }

    /// 0-based line index containing byte `offset`.
    pub fn line_index(&self, offset: u32) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        }
    }

    /// 1-based line and 1-based column (in chars) of byte `offset`.
    pub fn line_col(&self, offset: u32) -> (usize, usize) {
        let line = self.line_index(offset);
        let start = self.line_starts[line] as usize;
        let offset = (offset as usize).min(self.text.len());
        let col = self.text[start..offset].chars().count() + 1;
        (line + 1, col)
    }

    /// Text of the 0-based line `index`, without the line break.
    pub fn line_text(&self, index: usize) -> &str {
        let start = self.line_starts[index] as usize;
        let end = self
            .line_starts
            .get(index + 1)
            .map_or(self.text.len(), |&e| e as usize);
        self.text[start..end].trim_end_matches(['\n', '\r'])
    }

    /// Byte offset of the start of the 0-based line `index`.
    pub fn line_start(&self, index: usize) -> u32 {
        self.line_starts[index]
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }
}

/// All the files of one compilation.
#[derive(Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> SourceMap {
        SourceMap::default()
    }

    pub fn add(&mut self, name: impl Into<String>, text: impl Into<String>) -> FileId {
        self.files.push(SourceFile::new(name, text));
        FileId(self.files.len() as u32 - 1)
    }

    pub fn file(&self, id: FileId) -> &SourceFile {
        &self.files[id.0 as usize]
    }

    /// The source text covered by `span`.
    pub fn snippet(&self, span: Span) -> &str {
        &self.file(span.file).text[span.start as usize..span.end as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_counts_chars_not_bytes() {
        let f = SourceFile::new("t", "ab\nżółw x\n");
        assert_eq!(f.line_col(0), (1, 1));
        assert_eq!(f.line_col(3), (2, 1));
        let x = f.text.find('x').unwrap() as u32;
        assert_eq!(f.line_col(x), (2, 6));
        assert_eq!(f.line_text(1), "żółw x");
        assert_eq!(f.line_count(), 3);
    }
}
