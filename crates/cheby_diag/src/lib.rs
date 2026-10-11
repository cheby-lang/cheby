//! Spans, source map, diagnostics and their rendering (§13.8).
//!
//! - [`TextRange`], [`FileId`] and [`Span`]: byte ranges in files. Spans stay
//!   byte offsets inside the compiler (D-421).
//! - [`SourceMap`]: every file of a build with its display path and line
//!   index. Lines and code-point columns are computed only for output.
//! - [`Diagnostic`]: a [`Code`], a message, primary and secondary labels,
//!   notes, and help lines with optional suggested edits (D-418).
//! - [`codes`]: the registry of codes (D-419), each explained by a file in
//!   `docs/diagnostics/` that is embedded here for `cheby explain` (D-420).
//! - [`render`] and [`render_all`]: the plain-text layout of D-418, which
//!   the UI-test goldens pin. [`render_with`] and [`render_all_with`] add
//!   color escapes for a terminal, and [`strip_colors`] removes them again.

pub mod codes;
mod diagnostic;
mod render;
mod source;
mod text;

pub use codes::{Code, CodeInfo};
pub use diagnostic::{Diagnostic, Edit, Help, Label, LabelStyle, Severity};
pub use render::{
    Colors, render, render_all, render_all_with, render_tool_error, render_tool_help, render_with,
    strip_colors,
};
pub use source::{FileTooLarge, LineCol, LineIndex, Location, SourceFile, SourceMap, display_path};
pub use text::{FileId, Span, TextRange, TextSize};
