//! Byte offsets, ranges and spans.
//!
//! Spans inside the compiler are byte offsets into a file's text (D-421).
//! Lines and code-point columns are computed only when a diagnostic is
//! rendered ([`crate::LineIndex`]).

use std::fmt;

/// A byte offset into a source file. Files larger than `u32::MAX` bytes are
/// rejected by [`crate::SourceMap::add_file`], so every offset fits.
pub type TextSize = u32;

/// A half-open byte range `start..end` with `start <= end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TextRange {
    start: TextSize,
    end: TextSize,
}

impl TextRange {
    /// The range `start..end`. The bounds are swapped if `end < start`, so
    /// the invariant `start <= end` always holds.
    #[must_use]
    pub const fn new(start: TextSize, end: TextSize) -> Self {
        if end < start {
            Self {
                start: end,
                end: start,
            }
        } else {
            Self { start, end }
        }
    }

    /// The empty range at `offset`.
    #[must_use]
    pub const fn empty(offset: TextSize) -> Self {
        Self {
            start: offset,
            end: offset,
        }
    }

    /// The range of `len` bytes starting at `start`, cut at `u32::MAX`.
    #[must_use]
    pub const fn at(start: TextSize, len: TextSize) -> Self {
        Self {
            start,
            end: start.saturating_add(len),
        }
    }

    #[must_use]
    pub const fn start(self) -> TextSize {
        self.start
    }

    #[must_use]
    pub const fn end(self) -> TextSize {
        self.end
    }

    #[must_use]
    pub const fn len(self) -> TextSize {
        self.end.saturating_sub(self.start)
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// Whether `offset` is inside the range (`start <= offset < end`).
    #[must_use]
    pub const fn contains(self, offset: TextSize) -> bool {
        self.start <= offset && offset < self.end
    }

    /// Whether `other` lies entirely inside this range.
    #[must_use]
    pub const fn contains_range(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    /// The smallest range that covers both ranges.
    #[must_use]
    pub const fn cover(self, other: Self) -> Self {
        let start = if self.start < other.start {
            self.start
        } else {
            other.start
        };
        let end = if self.end > other.end {
            self.end
        } else {
            other.end
        };
        Self { start, end }
    }

    /// The text of this range, or `None` if it is out of bounds or not on
    /// character boundaries.
    #[must_use]
    pub fn slice(self, text: &str) -> Option<&str> {
        let start = usize::try_from(self.start).ok()?;
        let end = usize::try_from(self.end).ok()?;
        text.get(start..end)
    }
}

impl fmt::Display for TextRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

/// Identifies a file in a [`crate::SourceMap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(u32);

impl FileId {
    #[must_use]
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// A byte range in a particular file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    pub file: FileId,
    pub range: TextRange,
}

impl Span {
    #[must_use]
    pub const fn new(file: FileId, range: TextRange) -> Self {
        Self { file, range }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_keep_their_invariant() {
        let r = TextRange::new(5, 2);
        assert_eq!((r.start(), r.end(), r.len()), (2, 5, 3));
        assert!(TextRange::empty(4).is_empty());
        assert_eq!(TextRange::at(u32::MAX - 1, 5).end(), u32::MAX);
        assert!(r.contains(2) && r.contains(4) && !r.contains(5));
        assert!(r.contains_range(TextRange::new(3, 5)));
        assert_eq!(r.cover(TextRange::new(7, 9)), TextRange::new(2, 9));
        assert_eq!(r.to_string(), "2..5");
    }

    #[test]
    fn slices_only_on_char_boundaries() {
        let text = "aé b";
        assert_eq!(TextRange::new(0, 3).slice(text), Some("aé"));
        assert_eq!(TextRange::new(0, 2).slice(text), None);
        assert_eq!(TextRange::new(4, 9).slice(text), None);
    }
}
