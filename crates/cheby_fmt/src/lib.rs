//! The formatter behind `cheby fmt` (§13.7, D-079, D-1030).
//!
//! [`format_source`] parses a file with the compiler's parser and prints
//! its lossless tree (D-272, D-280) in the one canonical style: a pretty
//! printer driven by one line width, with no options. Comments and single
//! blank lines are kept, runs of blank lines collapse to one, imports are
//! sorted within their block, files use LF and end with one newline, and
//! string literals are never split. A source with a grammar error is never
//! formatted.
//!
//! The formatter grows in parts (D-1029). The first part laid out the
//! file's lines, imports and item headers (`layout`); the second lays out
//! the expressions, statements, patterns and types below them
//! (`expr`); the third gives every comment and blank line between two
//! parts of a node its place (`gap`). Only a gap between a unary `-` and
//! a number (D-470) and an attribute with no item after it are still
//! printed verbatim, with their own line structure, by rule.

pub mod doc;
mod expr;
mod gap;
mod layout;
mod source;

use std::fmt;
use std::sync::Arc;

use cheby_diag::{Diagnostic, FileId};

pub use layout::Coverage;

/// The line width `cheby fmt` uses: 100 columns, decided at CP2 (D-1252)
/// from the formatter's measurement of 80, 100 and 120 (D-1030), which
/// kept D-1150's provisional width as the decided one.
pub const WIDTH: usize = 100;

/// A source that cannot be formatted because it has grammar errors.
#[derive(Clone, Debug)]
pub struct FormatError {
    errors: Vec<Diagnostic>,
}

impl FormatError {
    /// The grammar errors, in source order.
    #[must_use]
    pub fn errors(&self) -> &[Diagnostic] {
        &self.errors
    }

    /// The grammar errors, in source order.
    #[must_use]
    pub fn into_errors(self) -> Vec<Diagnostic> {
        self.errors
    }
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.errors.len();
        let s = if n == 1 { "" } else { "s" };
        write!(f, "the source has {n} syntax error{s}")
    }
}

impl std::error::Error for FormatError {}

/// Formats `text` at line width `width`.
///
/// # Errors
///
/// Returns the grammar errors if `text` has any; it is then not formatted.
pub fn format_source(text: &str, width: usize) -> Result<String, FormatError> {
    format_file(FileId::new(0), text, width)
}

/// Formats `text`, whose diagnostics name `file`, at line width `width`.
///
/// # Errors
///
/// Returns the grammar errors if `text` has any; it is then not formatted.
pub fn format_file(file: FileId, text: &str, width: usize) -> Result<String, FormatError> {
    run(file, text, width).map(|(text, _)| text)
}

/// Formats `text` as [`format_source`] does and counts the nodes the
/// printer laid out and those it printed verbatim.
///
/// # Errors
///
/// Returns the grammar errors if `text` has any.
pub fn coverage(text: &str, width: usize) -> Result<Coverage, FormatError> {
    run(FileId::new(0), text, width).map(|(_, coverage)| coverage)
}

fn run(file: FileId, text: &str, width: usize) -> Result<(String, Coverage), FormatError> {
    let parse = cheby_syntax::parse(file, Arc::from(text));
    if !parse.errors().is_empty() {
        return Err(FormatError {
            errors: parse.errors().to_vec(),
        });
    }
    let tree = parse.tree();
    let mut formatter = layout::Formatter::new(source::Source::new(tree));
    let doc = formatter.file(tree.root());
    Ok((doc::print(&doc, width), formatter.coverage))
}

#[cfg(test)]
mod comment_tests;
#[cfg(test)]
mod expr_tests;
#[cfg(test)]
mod tests;
