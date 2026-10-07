//! `cheby.toml`, module discovery and, later, the import graph (§13.3,
//! §7.2).
//!
//! - [`Manifest::load`] reads and checks `cheby.toml` (B1). Problems are
//!   `E0036` diagnostics with a snippet of the manifest.
//! - [`discover`] maps the files under `src/` to module paths (B2), and
//!   reports invalid file and directory names as `E0037`.
//! - [`graph`] orders modules by their imports and finds import cycles
//!   (B3, D-145).
//!
//! Failures that are not about the package's contents, such as a
//! directory that cannot be read, are [`PackageError`]s, which the
//! command line prints as plain `error:` lines without a code (D-442).

mod discover;
pub mod graph;
mod manifest;
mod names;

use std::fmt;

pub use discover::{EXTENSION, ModuleFile, ModulePath, ROOT_FILE, SOURCE_DIR, discover};
pub use manifest::{Dependency, GitReference, MANIFEST_FILE, Manifest, Target, Targets, Version};
pub use names::{is_keyword_or_reserved, is_valid_name, suggest_name};

/// A failure to read the package that is not a diagnostic: an I/O error
/// or a file too large to compile. The message names the file by its
/// path relative to the package root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageError {
    message: String,
}

impl PackageError {
    /// An error with this message.
    #[must_use]
    pub const fn new(message: String) -> Self {
        Self { message }
    }

    /// An I/O error on `path`, a package-relative display path.
    #[must_use]
    pub fn io(path: &str, error: &std::io::Error) -> Self {
        Self::new(format!("{path}: {error}"))
    }

    /// A file over the size limit of the source map (4 GiB).
    #[must_use]
    pub fn too_large(path: &str) -> Self {
        Self::new(format!("{path}: the file is too large to compile"))
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PackageError {}

#[cfg(test)]
mod tests;
