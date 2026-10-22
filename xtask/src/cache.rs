//! The build cache of every `cheby` that `cargo xtask` starts (D-959's
//! harness rule; GAP(cache-tests-isolated)): one fresh owner-only
//! directory per `xtask` process, shared by all its children, so no
//! command reads or fills the developer's cache. `CHEBY_TEST_KEEP_CACHE`
//! names a directory to use and keep instead (GAP(harness-kept-cache)).
//! The directory is created on the first child that needs it and deleted
//! when the command ends ([`finish`]).
//!
//! `check-determinism` gives its two builds a fresh cache each instead, to
//! compare them (D-972).

use std::process::Command;
use std::sync::OnceLock;

use cheby_test_support::cache::HarnessCache;

/// The process's cache, once a child needs it.
static CACHE: OnceLock<Result<HarnessCache, String>> = OnceLock::new();

/// Sets `CHEBY_CACHE` of `cmd`, a `cheby` command, to the process's cache.
///
/// # Errors
///
/// When the cache cannot be created.
pub(crate) fn apply(cmd: &mut Command) -> Result<(), String> {
    match CACHE.get_or_init(|| HarnessCache::create("xtask")) {
        Ok(cache) => {
            cache.apply(cmd);
            Ok(())
        }
        Err(e) => Err(e.clone()),
    }
}

/// Deletes the process's cache, unless `CHEBY_TEST_KEEP_CACHE` named it.
/// A static is never dropped, so the command calls this when it ends.
pub(crate) fn finish() {
    if let Some(Ok(cache)) = CACHE.get()
        && !cache.is_kept()
    {
        let _ = std::fs::remove_dir_all(cache.path());
    }
}
