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
//! The formatter grows in parts (D-1029). This part lays out the file's
//! lines, imports and item headers; bodies, expressions, patterns, types
//! and statements are printed verbatim, with their own line structure, and
//! later parts lay them out.

pub mod doc;
mod layout;
mod source;

use std::fmt;
use std::sync::Arc;

use cheby_diag::{Diagnostic, FileId};

pub use layout::Coverage;

/// The line width `cheby fmt` uses. GAP(fmt-provisional-width): 100
/// columns until the user chooses the width at CP2 from the formatter's
/// measurement of 80, 100 and 120 (D-1030).
pub const PROVISIONAL_WIDTH: usize = 100;

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
mod tests;
