//! `CHEBY_CACHE_LOG` (D-973): every lookup of a command as a hit or a
//! miss, and every package module it parsed (D-1135),
//! written sorted when the command ends, for the cache-hit tests.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use super::{Key, StoreError};

/// Collects `<artifact> <module or constant path> hit|miss` lines and
/// writes them, sorted, to the file `CHEBY_CACHE_LOG` names. Without a
/// file it records nothing. Lookups on several threads may record at once.
/// Borrowing's records add `facts`, `masks` and `key` lines (K4b).
#[derive(Debug, Default)]
pub struct CacheLog {
    path: Option<PathBuf>,
    /// Shared with the logs [`CacheLog::share`] made.
    lines: Arc<Mutex<Vec<String>>>,
}

impl CacheLog {
    /// A log written to `path`, or a log that records nothing.
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            lines: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A log that records into this one's lines, for a part of the
    /// command that outlives a borrow of the cache (borrowing's records,
    /// which the front end hands to the back end). Only this log's
    /// [`CacheLog::finish`] writes the file.
    pub(crate) fn share(&self) -> Self {
        Self {
            path: self.path.clone(),
            lines: Arc::clone(&self.lines),
        }
    }

    /// Records the key a unit's code would be stored under, as a line
    /// `key <artifact> <module> <32 hex digits>` (`artifact` names the
    /// record kind and the build kind, such as `code.run`): what K4c's
    /// lookups will use, logged before any code is stored (D-969, D-973).
    pub(crate) fn key(&self, artifact: &str, subject: &str, key: &Key) {
        if self.path.is_none() {
            return;
        }
        let line = format!("key {artifact} {subject} {}", key.hash().to_hex());
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
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
    /// `parse <module>` (D-1135). Std modules are not
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

    /// Records that the command parsed std module `module`, as a line
    /// `parse.std <module>` (GAP(std-cache-log-lines), refining D-1135:
    /// `parse` stays package-only, and std's parses get their own name).
    /// A std module is listed once per command, though std's first use
    /// parses it in a front end of its own beside the command's.
    pub fn parsed_std(&self, module: &str) {
        if self.path.is_none() {
            return;
        }
        let line = format!("parse.std {module}");
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if !lines.contains(&line) {
            lines.push(line);
        }
    }

    /// How many lines the log holds now, for [`CacheLog::rollback`].
    pub(crate) fn mark(&self) -> usize {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Drops the lines recorded after `mark` (a command that starts again,
    /// D-1133, D-1135: only the run that finished is logged).
    pub(crate) fn rollback(&self, mark: usize) {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .truncate(mark);
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
