//! `CHEBY_CACHE_LOG` (D-973): every lookup of a command as a hit or a
//! miss, and every package module it parsed (GAP(cache-log-parses)),
//! written sorted when the command ends, for the cache-hit tests.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use super::StoreError;

/// Collects `<artifact> <module or constant path> hit|miss` lines and
/// writes them, sorted, to the file `CHEBY_CACHE_LOG` names. Without a
/// file it records nothing. Lookups on several threads may record at once.
#[derive(Debug, Default)]
pub struct CacheLog {
    path: Option<PathBuf>,
    lines: Mutex<Vec<String>>,
}

impl CacheLog {
    /// A log written to `path`, or a log that records nothing.
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            lines: Mutex::new(Vec::new()),
        }
    }

    /// The log `CHEBY_CACHE_LOG`'s value names; unset or empty records
    /// nothing.
    #[must_use]
    pub fn from_var(value: Option<OsString>) -> Self {
        Self::new(value.filter(|v| !v.is_empty()).map(PathBuf::from))
    }

    /// Whether lookups are recorded.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.path.is_some()
    }

    /// Records one lookup of `artifact` (a key kind's tag, such as `code`)
    /// for `subject` (a module or constant path).
    pub fn record(&self, artifact: &str, subject: &str, hit: bool) {
        if self.path.is_none() {
            return;
        }
        let verdict = if hit { "hit" } else { "miss" };
        let line = format!("{artifact} {subject} {verdict}");
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }

    /// Records that the command parsed package module `module`, as a line
    /// `parse <module>` (GAP(cache-log-parses)). Std modules are not
    /// listed (D-980).
    pub fn parsed(&self, module: &str) {
        if self.path.is_none() {
            return;
        }
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(format!("parse {module}"));
    }

    /// The recorded lines, sorted.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut lines = self
            .lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        lines.sort();
        lines
    }

    /// Writes the sorted lines, one per line, to the log's file; does
    /// nothing without one. The command calls it when it ends.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] when the file cannot be written.
    pub fn finish(&self) -> Result<(), StoreError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut text = String::new();
        for line in self.lines() {
            text.push_str(&line);
            text.push('\n');
        }
        std::fs::write(path, text).map_err(|e| StoreError::io(path, e))
    }
}
