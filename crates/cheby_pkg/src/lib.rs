//! Git dependencies (§13.4): the lockfile, version selection, fetching and
//! the commands `cheby add`, `cheby update` and `cheby fetch`.
//!
//! - [`lock`] reads and writes `cheby.lock` (D-1880).
//! - [`git`] runs the `git` found on `PATH` (D-1876).
//! - [`store`] keeps fetched sources per machine (D-1877) and checks them
//!   (D-1878).
//! - [`resolve`] selects one version of each package (D-192).
//! - [`commands`] is what the command line runs.
//!
//! Only [`commands`] uses the network. The commands that load a package
//! read the lockfile and the store (D-1881).

pub mod commands;
pub mod git;
mod hash;
pub mod lock;
mod manifest_edit;
pub mod resolve;
pub mod store;

use std::fmt;

pub use lock::{LOCK_FILE, LockedPackage, Lockfile};
pub use store::Store;

/// `text` as a TOML string. It holds no control character: URLs and the names
/// of tags have none.
fn toml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Why a command about dependencies failed: a tool error without a code
/// (D-442).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PkgError {
    message: String,
    help: Option<String>,
}

impl PkgError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            help: None,
        }
    }

    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// An I/O error while doing `what` to `path`.
    pub(crate) fn io(what: &str, path: &std::path::Path, e: &std::io::Error) -> Self {
        Self::new(format!("cannot {what} `{}`: {e}", path.display()))
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn help(&self) -> Option<&str> {
        self.help.as_deref()
    }
}

impl fmt::Display for PkgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PkgError {}
