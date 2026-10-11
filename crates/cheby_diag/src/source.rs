//! The source map: every file of a build with its display path, text and
//! line index.
//!
//! Positions follow D-421: lines and columns count from 1, columns count
//! Unicode code points, and paths are package-relative with `/`.

use std::fmt;
use std::path::{Component, Path};
use std::sync::Arc;

use crate::diagnostic::Diagnostic;
use crate::text::{FileId, Span, TextRange, TextSize};

/// The UTF-8 byte-order mark, ignored at the start of a file (§2.1).
const BOM: &str = "\u{feff}";

/// All source files of a build, indexed by [`FileId`].
#[derive(Debug, Default, Clone)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

/// A file was larger than the `u32` offsets of spans allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTooLarge {
    pub path: String,
    pub len: usize,
}

impl fmt::Display for FileTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is {} bytes long, larger than the 4 GiB a source file may have",
            self.path, self.len
        )
    }
}

impl std::error::Error for FileTooLarge {}

impl SourceMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a file. `path` is the path shown in diagnostics: relative to the
    /// package root and `/`-separated (D-421), see [`display_path`].
    ///
    /// # Errors
    ///
    /// Returns [`FileTooLarge`] if the text does not fit in `u32` offsets,
    /// or if the map already holds `u32::MAX` files.
    pub fn add_file(
        &mut self,
        path: impl Into<String>,
        text: impl Into<Arc<str>>,
    ) -> Result<FileId, FileTooLarge> {
        let path = path.into();
        let text = text.into();
        let too_large = || FileTooLarge {
            path: path.clone(),
            len: text.len(),
        };
        if u32::try_from(text.len()).is_err() {
            return Err(too_large());
        }
        let id = FileId::new(u32::try_from(self.files.len()).map_err(|_| too_large())?);
        let line_index = LineIndex::new(&text);
        self.files.push(SourceFile {
            path,
            text,
            line_index,
        });
        Ok(id)
    }

    #[must_use]
    pub fn file(&self, id: FileId) -> Option<&SourceFile> {
        self.files.get(usize::try_from(id.index()).ok()?)
    }

    /// Every file with its id, in the order they were added.
    pub fn files(&self) -> impl Iterator<Item = (FileId, &SourceFile)> {
        self.files
            .iter()
            .zip(0u32..)
            .map(|(file, index)| (FileId::new(index), file))
    }

    /// The position of the start of `span`, or `None` for an unknown file.
    #[must_use]
    pub fn location(&self, span: Span) -> Option<Location> {
        let file = self.file(span.file)?;
        let LineCol { line, col } = file.line_col(span.range.start());
        Some(Location {
            path: file.path.clone(),
            line,
            col,
        })
    }

    /// Sorts diagnostics into the deterministic output order: diagnostics
    /// without a span first, then by path of the primary span, start, end,
    /// code and message (D-264).
    pub fn sort(&self, diags: &mut [Diagnostic]) {
        diags.sort_by_cached_key(|d| {
            let place = d.primary_span().and_then(|span| {
                let file = self.file(span.file)?;
                Some((file.path.clone(), span.range.start(), span.range.end()))
            });
            (place, d.code, d.message.clone())
        });
    }
}

/// One source file.
#[derive(Debug, Clone)]
pub struct SourceFile {
    path: String,
    text: Arc<str>,
    line_index: LineIndex,
}

impl SourceFile {
    /// The display path (D-421).
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The text, shared, for example with the syntax tree.
    #[must_use]
    pub fn text_arc(&self) -> Arc<str> {
        Arc::clone(&self.text)
    }

    #[must_use]
    pub fn line_index(&self) -> &LineIndex {
        &self.line_index
    }

    /// The line and column of `offset` in this file.
    #[must_use]
    pub fn line_col(&self, offset: TextSize) -> LineCol {
        self.line_index.line_col(&self.text, offset)
    }
}

/// A 1-based line and a 1-based column in Unicode code points (D-421).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineCol {
    pub line: u32,
    pub col: u32,
}

/// A position as shown in a diagnostic: `path:line:col`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Location {
    pub path: String,
    pub line: u32,
    pub col: u32,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.path, self.line, self.col)
    }
}

/// The start offset of every line of a text.
///
/// Lines end at `\n`; the `\r` of a `\r\n` belongs to the terminator. A lone
/// `\r` is not a line break (it is a compile error, §2.1). The position
/// after a final `\n` is on an empty last line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    starts: Vec<TextSize>,
}

impl LineIndex {
    #[must_use]
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .zip(1u32..)
                .filter(|&(byte, _)| byte == b'\n')
                .map(|(_, after)| after),
        );
        Self { starts }
    }

    /// The number of lines, counting the empty line after a final `\n`.
    #[must_use]
    pub fn line_count(&self) -> u32 {
        u32::try_from(self.starts.len()).unwrap_or(u32::MAX)
    }

    /// The line and column of `offset`. An offset past the end is clamped to
    /// the end, and an offset inside a character is moved to its start. A
    /// byte-order mark at the start of the file is not counted (§2.1).
    #[must_use]
    pub fn line_col(&self, text: &str, offset: TextSize) -> LineCol {
        let offset = floor_char_boundary(text, offset);
        let index = self
            .starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let line_start = self.starts.get(index).copied().unwrap_or(0);
        let content_start = if index == 0 && text.starts_with(BOM) {
            to_size(BOM.len()).min(offset)
        } else {
            line_start
        };
        let before = TextRange::new(content_start, offset)
            .slice(text)
            .unwrap_or("");
        LineCol {
            line: to_size(index).saturating_add(1),
            col: to_size(before.chars().count()).saturating_add(1),
        }
    }

    /// The content of 1-based `line`, without its terminator and, on the
    /// first line, without a byte-order mark. `None` if there is no such
    /// line.
    #[must_use]
    pub fn line_range(&self, text: &str, line: u32) -> Option<TextRange> {
        let index = usize::try_from(line.checked_sub(1)?).ok()?;
        let mut start = *self.starts.get(index)?;
        let text_len = to_size(text.len());
        let mut end = match self.starts.get(index.saturating_add(1)) {
            Some(&next) => next.saturating_sub(1),
            None => text_len,
        };
        let line_text = TextRange::new(start, end).slice(text)?;
        if line_text.ends_with('\r') && end < text_len {
            end = end.saturating_sub(1);
        }
        if index == 0 && line_text.starts_with(BOM) {
            start = to_size(BOM.len()).min(end);
        }
        Some(TextRange::new(start, end))
    }

    /// The 1-based line that contains `offset` (clamped to the text).
    #[must_use]
    pub fn line_of(&self, offset: TextSize) -> u32 {
        to_size(self.starts.partition_point(|&start| start <= offset)).max(1)
    }
}

/// Moves `offset` back to a character boundary of `text`, clamped to its
/// length.
pub(crate) fn floor_char_boundary(text: &str, offset: TextSize) -> TextSize {
    let mut at = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at = at.saturating_sub(1);
    }
    to_size(at)
}

/// Converts a length that is known to fit (files are at most `u32::MAX`
/// bytes) to a [`TextSize`], saturating otherwise.
pub(crate) fn to_size(n: usize) -> TextSize {
    TextSize::try_from(n).unwrap_or(TextSize::MAX)
}

/// The display path of `path` for diagnostics: relative to `root` when
/// `path` is inside it, with `/` between components on every platform
/// (D-421).
#[must_use]
pub fn display_path(path: &Path, root: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::ParentDir => parts.push("..".to_owned()),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => parts.push(String::new()),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lc(line: u32, col: u32) -> LineCol {
        LineCol { line, col }
    }

    #[test]
    fn columns_count_code_points() {
        let text = "let é = \"日本\"\nx";
        let index = LineIndex::new(text);
        assert_eq!(index.line_col(text, 0), lc(1, 1));
        assert_eq!(index.line_col(text, 4), lc(1, 5));
        // After `é` (2 bytes), ` ` is column 6.
        assert_eq!(index.line_col(text, 6), lc(1, 6));
        // Inside `日` moves to its start.
        let inside = u32::try_from(text.find('日').unwrap() + 1).unwrap();
        assert_eq!(index.line_col(text, inside), lc(1, 10));
        let x = u32::try_from(text.find('x').unwrap()).unwrap();
        assert_eq!(index.line_col(text, x), lc(2, 1));
    }

    #[test]
    fn crlf_and_lone_cr() {
        let text = "a\r\nb\rc\n";
        let index = LineIndex::new(text);
        assert_eq!(index.line_count(), 3);
        assert_eq!(index.line_col(text, 3), lc(2, 1));
        assert_eq!(index.line_col(text, 5), lc(2, 3));
        assert_eq!(index.line_range(text, 1), Some(TextRange::new(0, 1)));
        assert_eq!(index.line_range(text, 2), Some(TextRange::new(3, 6)));
        assert_eq!(index.line_range(text, 3), Some(TextRange::new(7, 7)));
        assert_eq!(index.line_range(text, 4), None);
        assert_eq!(index.line_range(text, 0), None);
    }

    #[test]
    fn end_of_file_positions() {
        let text = "ab\n";
        let index = LineIndex::new(text);
        assert_eq!(index.line_col(text, 3), lc(2, 1));
        assert_eq!(index.line_col(text, 99), lc(2, 1));
        let text = "ab";
        assert_eq!(LineIndex::new(text).line_col(text, 2), lc(1, 3));
        assert_eq!(LineIndex::new("").line_col("", 0), lc(1, 1));
    }

    #[test]
    fn byte_order_mark_is_not_counted() {
        let text = "\u{feff}fn f\n";
        let index = LineIndex::new(text);
        assert_eq!(index.line_col(text, 3), lc(1, 1));
        assert_eq!(index.line_col(text, 6), lc(1, 4));
        assert_eq!(index.line_col(text, 0), lc(1, 1));
        assert_eq!(index.line_range(text, 1), Some(TextRange::new(3, 7)));
    }

    #[test]
    fn locations_and_paths() {
        let mut map = SourceMap::new();
        let id = map.add_file("src/main.cheby", "a\nbc").unwrap();
        let loc = map.location(Span::new(id, TextRange::empty(3))).unwrap();
        assert_eq!(loc.to_string(), "src/main.cheby:2:2");
        assert!(
            map.location(Span::new(FileId::new(7), TextRange::empty(0)))
                .is_none()
        );
        let root = Path::new("pkg");
        assert_eq!(
            display_path(&root.join("src").join("web").join("a.cheby"), root),
            "src/web/a.cheby"
        );
    }
}
