//! The diagnostic type: a code, a message, labelled spans, notes and help
//! lines with optional suggested edits (§13.8, D-418, D-419).

use crate::codes::Code;
use crate::text::Span;

/// Whether a diagnostic stops compilation. It follows from the code's
/// letter: `E` is an error, `W` a warning (D-419).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    /// The word used in the rendered header.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// How a label is drawn: `^` under a primary span, `-` under a secondary
/// one (D-418).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LabelStyle {
    Primary,
    Secondary,
}

/// A span with an optional short message (empty for none).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Label {
    pub span: Span,
    pub message: String,
    pub style: LabelStyle,
}

/// A text replacement, part of a suggestion.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Edit {
    pub span: Span,
    pub replacement: String,
}

/// A `help` line. `edits` is the machine-applicable form of the suggestion,
/// if there is one; only `message` is rendered (D-418).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Help {
    pub message: String,
    pub edits: Vec<Edit>,
}

/// A compiler diagnostic.
///
/// Built with [`Diagnostic::new`] and the `with_*` methods:
///
/// ```
/// use cheby_diag::{codes, Diagnostic, FileId, Span, TextRange};
///
/// let file = FileId::new(0);
/// let diag = Diagnostic::new(codes::SYNTAX_ERROR, "expected `)`, found `fn`")
///     .with_primary(Span::new(file, TextRange::at(10, 2)), "expected `)`")
///     .with_secondary(Span::new(file, TextRange::at(3, 1)), "to close this `(`");
/// assert_eq!(diag.labels.len(), 2);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Diagnostic {
    pub code: Code,
    pub message: String,
    pub labels: Vec<Label>,
    pub notes: Vec<String>,
    pub helps: Vec<Help>,
}

impl Diagnostic {
    #[must_use]
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            labels: Vec::new(),
            notes: Vec::new(),
            helps: Vec::new(),
        }
    }

    #[must_use]
    pub const fn severity(&self) -> Severity {
        self.code.severity()
    }

    #[must_use]
    pub const fn is_error(&self) -> bool {
        matches!(self.code.severity(), Severity::Error)
    }

    /// Adds a primary label, drawn with `^`. An empty `message` draws only
    /// the markers. The first primary label gives the location line.
    #[must_use]
    pub fn with_primary(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label {
            span,
            message: message.into(),
            style: LabelStyle::Primary,
        });
        self
    }

    /// Adds a secondary label, drawn with `-`.
    #[must_use]
    pub fn with_secondary(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label {
            span,
            message: message.into(),
            style: LabelStyle::Secondary,
        });
        self
    }

    /// Adds a `= note: …` line.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    /// Adds a `= help: …` line without an edit.
    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.helps.push(Help {
            message: help.into(),
            edits: Vec::new(),
        });
        self
    }

    /// Adds a suggested fix: a `= help: …` line plus the edit that applies
    /// it, replacing `span` with `replacement`. The message should show the
    /// fix in words, because only the message is rendered (D-418).
    #[must_use]
    pub fn with_suggestion(
        mut self,
        help: impl Into<String>,
        span: Span,
        replacement: impl Into<String>,
    ) -> Self {
        self.helps.push(Help {
            message: help.into(),
            edits: vec![Edit {
                span,
                replacement: replacement.into(),
            }],
        });
        self
    }

    /// The span of the first primary label.
    #[must_use]
    pub fn primary_span(&self) -> Option<Span> {
        self.labels
            .iter()
            .find(|label| label.style == LabelStyle::Primary)
            .map(|label| label.span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codes;
    use crate::text::{FileId, TextRange};

    #[test]
    fn builder_collects_parts() {
        let span = Span::new(FileId::new(0), TextRange::new(1, 2));
        let other = Span::new(FileId::new(0), TextRange::new(5, 6));
        let d = Diagnostic::new(codes::TUPLE_SHAPE, "m")
            .with_secondary(other, "b")
            .with_primary(span, "a")
            .with_note("n")
            .with_suggestion("h", span, "x");
        assert_eq!(d.severity(), Severity::Warning);
        assert!(!d.is_error());
        assert_eq!(d.primary_span(), Some(span));
        assert_eq!(d.helps.first().map(|h| h.edits.len()), Some(1));
        assert_eq!(d.notes, vec!["n".to_owned()]);
    }
}
