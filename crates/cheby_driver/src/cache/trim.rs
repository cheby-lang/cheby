//! Trimming (D-961): at most once a day, delete entries unused for five
//! days.
//!
//! "Unused" is read from an entry's modification time, which a read
//! refreshes when it is older than an hour (`store::touch_if_old`).
//! `v1/trimmed`'s modification time is the last trim. The caller decides
//! whether to trim at all: `cheby check`, `cheby build` and `cheby test`
//! do, `cheby run` and the REPL never do.
//!
//! [`Store::trim_keeping_indexed`] keeps the blobs a recently used package
//! index names (D-961, D-962): the indexes in `hint/` are read only when a
//! trim is due.
//!
//! D-1119: besides blobs and records, hints and files left
//! in `tmp/` by a writer that died are deleted on the same five-day rule;
//! directories are never removed, so a writer never races a deleted shard.

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::index::PackageIndex;
use super::store::{BLOB_DIR, FileKind, HINT_DIR, RECORD_DIR, TMP_DIR};
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
        if !self.mark_trim(now)? {
            return Ok(TrimReport::default());
        }
        Ok(self.sweep(now, keep))
    }

    /// [`Store::trim`] keeping the blobs that a recently used package
    /// index names (D-961, D-962): an index in `v1/hint/` unused for less
    /// than five days. The indexes are read only when the trim is due.
    ///
    /// # Errors
    ///
    /// As [`Store::trim`].
    pub fn trim_keeping_indexed(&self, now: SystemTime) -> Result<TrimReport, StoreError> {
        if !self.mark_trim(now)? {
            return Ok(TrimReport::default());
        }
        let mut named = std::collections::BTreeSet::new();
        for shard in entries(&self.layout_dir().join(HINT_DIR)) {
            for path in entries(&shard) {
                let recent = fs::symlink_metadata(&path)
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| now.duration_since(t).map_or(true, |age| age < UNUSED_FOR));
                if !recent {
                    continue;
                }
                let Some(payload) = fs::read(&path)
                    .ok()
                    .filter(|bytes| super::store::header_ok(bytes, FileKind::Hint, self.build_id()))
                    .and_then(|bytes| bytes.get(super::store::HEADER_LEN..).map(<[u8]>::to_vec))
                else {
                    continue;
                };
                if PackageIndex::is_index(&payload)
                    && let Some(index) = PackageIndex::decode(&payload)
                {
                    named.extend(index.blobs());
                }
            }
        }
        Ok(self.sweep(now, &|blob| named.contains(&blob)))
    }

    /// Whether a trim is due at `now`, the last one being a day ago or
    /// more; if so, marks `now` as the last trim first, so that commands
    /// ending meanwhile do not trim too.
    fn mark_trim(&self, now: SystemTime) -> Result<bool, StoreError> {
        let marker = self.layout_dir().join(TRIMMED);
        let last = fs::metadata(&marker).and_then(|m| m.modified()).ok();
        if last.is_some_and(|t| now.duration_since(t).is_ok_and(|age| age < TRIM_EVERY)) {
            return Ok(false);
        }
        let f = File::create(&marker).map_err(|e| StoreError::io(&marker, e))?;
        f.set_modified(now)
            .map_err(|e| StoreError::io(&marker, e))?;
        drop(f);
        Ok(true)
    }

    /// Deletes what is unused for five days before `now`, but blobs `keep`
    /// names.
    fn sweep(&self, now: SystemTime, keep: &dyn Fn(Hash128) -> bool) -> TrimReport {
        let v1 = self.layout_dir();
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
        report
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
