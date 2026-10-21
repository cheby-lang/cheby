//! Trimming (D-961): at most once a day, delete entries unused for five
//! days.
//!
//! "Unused" is read from an entry's modification time, which a read
//! refreshes when it is older than an hour (`store::touch_if_old`).
//! `v1/trimmed`'s modification time is the last trim. The caller decides
//! whether to trim at all: `cheby check`, `cheby build` and `cheby test`
//! do, `cheby run` and the REPL never do.
//!
//! GAP(cache-trim-scope): besides blobs and records, hints and files left
//! in `tmp/` by a writer that died are deleted on the same five-day rule;
//! directories are never removed, so a writer never races a deleted shard.

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::store::{BLOB_DIR, HINT_DIR, RECORD_DIR, TMP_DIR};
use super::{Hash128, Store, StoreError};

/// Entries unused for this long are deleted.
const UNUSED_FOR: Duration = Duration::from_hours(5 * 24);
/// Trimming runs at most once per this period.
const TRIM_EVERY: Duration = Duration::from_hours(24);
/// The marker whose modification time is the last trim.
const TRIMMED: &str = "trimmed";

/// What one call of [`Store::trim`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrimReport {
    /// Whether it trimmed: false when the last trim was less than a day
    /// ago.
    pub ran: bool,
    /// Blobs deleted.
    pub blobs: usize,
    /// Records deleted.
    pub records: usize,
    /// Hints deleted.
    pub hints: usize,
    /// Abandoned temporary files deleted.
    pub temporary: usize,
    /// Old blobs the caller's predicate kept.
    pub kept: usize,
}

impl Store {
    /// Deletes blobs, records and hints unused for five days before `now`,
    /// except blobs `keep` names, unless the last trim was less than a day
    /// before `now` (D-961). Files that vanish or cannot be deleted
    /// meanwhile are skipped.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] when the trim marker cannot be written.
    pub fn trim(
        &self,
        now: SystemTime,
        keep: &dyn Fn(Hash128) -> bool,
    ) -> Result<TrimReport, StoreError> {
        let v1 = self.layout_dir();
        let marker = v1.join(TRIMMED);
        let last = fs::metadata(&marker).and_then(|m| m.modified()).ok();
        if last.is_some_and(|t| now.duration_since(t).is_ok_and(|age| age < TRIM_EVERY)) {
            return Ok(TrimReport::default());
        }
        // Mark first, so that commands ending meanwhile do not trim too.
        let f = File::create(&marker).map_err(|e| StoreError::io(&marker, e))?;
        f.set_modified(now)
            .map_err(|e| StoreError::io(&marker, e))?;
        drop(f);

        let mut report = TrimReport {
            ran: true,
            ..TrimReport::default()
        };
        let old = |path: &Path| {
            fs::symlink_metadata(path)
                .and_then(|m| m.modified())
                .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age >= UNUSED_FOR))
        };
        for (dir, count) in [
            (BLOB_DIR, &mut report.blobs),
            (RECORD_DIR, &mut report.records),
            (HINT_DIR, &mut report.hints),
        ] {
            let is_blob = dir == BLOB_DIR;
            for shard in entries(&v1.join(dir)) {
                for path in entries(&shard) {
                    if !old(&path) {
                        continue;
                    }
                    if is_blob {
                        let name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .and_then(Hash128::from_hex);
                        if name.is_some_and(keep) {
                            report.kept = report.kept.saturating_add(1);
                            continue;
                        }
                    }
                    if fs::remove_file(&path).is_ok() {
                        *count = count.saturating_add(1);
                    }
                }
            }
        }
        for path in entries(&v1.join(TMP_DIR)) {
            if old(&path) && fs::remove_file(&path).is_ok() {
                report.temporary = report.temporary.saturating_add(1);
            }
        }
        Ok(report)
    }
}

/// The paths in `dir`, sorted; none when it cannot be read.
fn entries(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    paths.sort();
    paths
}
