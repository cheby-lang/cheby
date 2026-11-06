//! Fresh temporary directories for the harnesses: the build cache of one
//! harness process (D-959) and the output directory of one `@aot` trial
//! (D-1025).
//!
//! Every harness process, the conformance, UI and REPL harnesses alike,
//! creates one fresh owner-only directory in the system's temporary
//! directory when it starts ([`HarnessCache::create`]) and sets
//! `CHEBY_CACHE` to it for every child it starts ([`HarnessCache::apply`]),
//! overriding the developer's own value. All the runs of one harness
//! process share it, so a harness never reads or fills the user's cache
//! and two harness processes running at once never share one. The
//! directory is deleted when the value is dropped, at the end of the run,
//! after failing trials too (docs/plan/testing.md §1).
//!
//! `CHEBY_TEST_KEEP_CACHE=<absolute directory>` makes the harness use that
//! directory instead, creating it owner-only if needed, and never delete
//! it, so two runs can share one cache, as the check that a second run of
//! the UI suite writes no new record does (D-1144).
//! `cargo xtask` and the `cheby_cli` tests that start `cheby` use
//! [`HarnessCache`] too (D-1128).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// The cache directory of a `cheby` command (D-959).
pub const CACHE_ENV: &str = "CHEBY_CACHE";

/// How many directories this process has asked for, so that two
/// directories of one process never get the same name.
static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A directory that did not exist before [`FreshDir::create_in`] made it,
/// readable, writable and searchable by its owner only (on Unix), and
/// deleted with everything in it when the value is dropped.
#[derive(Debug)]
pub struct FreshDir {
    path: PathBuf,
}

impl FreshDir {
    /// A fresh directory in the system's temporary directory, named
    /// `<prefix>-<process id>-<n>`.
    ///
    /// # Errors
    ///
    /// As [`FreshDir::create_in`].
    pub fn create(prefix: &str) -> Result<Self, String> {
        Self::create_in(&std::env::temp_dir(), prefix)
    }

    /// A fresh directory in `parent`, named `<prefix>-<process id>-<n>`.
    /// A name that exists already, left by an earlier process with the
    /// same id, is skipped, never reused: the directory is always new.
    /// A relative `parent` is taken from the current directory, so the
    /// path is absolute, as `CHEBY_CACHE` must be (D-959).
    ///
    /// # Errors
    ///
    /// Returns a message if `parent` cannot be made absolute or the
    /// directory cannot be created.
    pub fn create_in(parent: &Path, prefix: &str) -> Result<Self, String> {
        let parent = std::path::absolute(parent)
            .map_err(|e| format!("{}: cannot make the path absolute: {e}", parent.display()))?;
        let pid = std::process::id();
        let mut last_error = None;
        for _ in 0..100 {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("{prefix}-{pid}-{n}"));
            match owner_only_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_error = Some(e);
                }
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        Err(format!(
            "{}: no fresh name for `{prefix}` after 100 tries: {}",
            parent.display(),
            last_error.map_or_else(String::new, |e| e.to_string())
        ))
    }

    /// The directory's absolute path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FreshDir {
    fn drop(&mut self) {
        // Best effort: a directory left behind costs only disk space, and
        // its name is this process's own.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Creates `path`, which must not exist, owner-only on Unix.
fn owner_only_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    // Windows: the directory inherits the temporary directory's access
    // list, which is the user's own; the harnesses run nothing there
    // before M6 (D-935).
    #[cfg(not(unix))]
    {
        std::fs::DirBuilder::new().create(path)
    }
}

/// The variable that makes a harness use, and keep, a cache directory of
/// the developer's choice (D-1144).
pub const KEEP_CACHE_ENV: &str = "CHEBY_TEST_KEEP_CACHE";

/// The build cache of one harness process (D-959): a [`FreshDir`] in the
/// system's temporary directory that every child of the harness gets as
/// `CHEBY_CACHE`, or the directory `CHEBY_TEST_KEEP_CACHE` names.
#[derive(Debug)]
pub struct HarnessCache {
    /// The fresh directory, deleted on drop; `None` for a kept one.
    dir: Option<FreshDir>,
    path: PathBuf,
}

impl HarnessCache {
    /// Creates the harness's cache, `cheby-<harness>-cache-<pid>-<n>` in
    /// the system's temporary directory. When `CHEBY_TEST_KEEP_CACHE`
    /// names an absolute directory, the harness uses that one instead,
    /// creating it owner-only if needed, and never deletes it, so two runs
    /// can share one cache (D-1144).
    ///
    /// # Errors
    ///
    /// As [`FreshDir::create`], and when `CHEBY_TEST_KEEP_CACHE` is
    /// relative or its directory cannot be created. The harness then stops
    /// with status 2, as for its other setup errors.
    pub fn create(harness: &str) -> Result<Self, String> {
        Self::create_with(harness, std::env::var_os(KEEP_CACHE_ENV))
    }

    /// [`HarnessCache::create`] with `keep` as `CHEBY_TEST_KEEP_CACHE`'s
    /// value; unset or empty creates a fresh directory.
    ///
    /// # Errors
    ///
    /// As [`HarnessCache::create`].
    pub fn create_with(harness: &str, keep: Option<std::ffi::OsString>) -> Result<Self, String> {
        if let Some(keep) = keep.filter(|v| !v.is_empty()) {
            let path = PathBuf::from(keep);
            if !path.is_absolute() {
                return Err(format!(
                    "{KEEP_CACHE_ENV} must be an absolute path, but it is `{}`",
                    path.display()
                ));
            }
            owner_only_dir_all(&path)
                .map_err(|e| format!("cannot create {KEEP_CACHE_ENV} `{}`: {e}", path.display()))?;
            return Ok(Self { dir: None, path });
        }
        FreshDir::create(&format!("cheby-{harness}-cache"))
            .map(|dir| Self {
                path: dir.path().to_path_buf(),
                dir: Some(dir),
            })
            .map_err(|e| format!("cannot create the harness's {CACHE_ENV}: {e}"))
    }

    /// The cache directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the directory is kept after the run
    /// (`CHEBY_TEST_KEEP_CACHE`).
    #[must_use]
    pub const fn is_kept(&self) -> bool {
        self.dir.is_none()
    }

    /// Sets `CHEBY_CACHE` to the cache for `cmd`, whatever the harness's
    /// own environment says.
    pub fn apply(&self, cmd: &mut Command) {
        cmd.env(CACHE_ENV, self.path());
    }

    /// The line a harness prints when it starts, so a run's log says
    /// where its cache was.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.is_kept() {
            format!(
                "{CACHE_ENV} of every child: {} (kept, from {KEEP_CACHE_ENV})",
                self.path().display()
            )
        } else {
            format!(
                "{CACHE_ENV} of every child: {} (fresh, deleted at the end of the run, D-959)",
                self.path().display()
            )
        }
    }
}

/// Creates `path` and its missing parents, owner-only on Unix; an
/// existing directory is used as it is.
fn owner_only_dir_all(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn cache_dirs_are_fresh_absolute_and_removed_on_drop() {
        let first = HarnessCache::create("unit").unwrap();
        let second = HarnessCache::create("unit").unwrap();
        assert_ne!(first.path(), second.path());
        for cache in [&first, &second] {
            assert!(cache.path().is_absolute(), "{}", cache.path().display());
            assert!(cache.path().is_dir());
            let temp = std::path::absolute(std::env::temp_dir()).unwrap();
            assert!(cache.path().starts_with(&temp));
            let name = cache.path().file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("cheby-unit-cache-"), "{name}");
        }
        // Everything inside goes with it, as the cache a run filled does.
        std::fs::create_dir_all(first.path().join("v1/a")).unwrap();
        std::fs::write(first.path().join("v1/a/record"), b"x").unwrap();
        let (a, b) = (first.path().to_path_buf(), second.path().to_path_buf());
        drop(first);
        drop(second);
        assert!(!a.exists(), "{}", a.display());
        assert!(!b.exists(), "{}", b.display());
    }

    #[cfg(unix)]
    #[test]
    fn cache_dirs_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let cache = HarnessCache::create("unit-mode").unwrap();
        let mode = std::fs::metadata(cache.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
    }

    /// An existing name is skipped, never reused or emptied.
    #[test]
    fn cache_an_existing_name_is_skipped() {
        let parent = FreshDir::create("cheby-unit-parent").unwrap();
        let pid = std::process::id();
        let next = COUNTER.load(Ordering::Relaxed);
        // The next names this process would try, taken already.
        for n in next..next.saturating_add(3) {
            let taken = parent.path().join(format!("x-{pid}-{n}"));
            std::fs::create_dir(&taken).unwrap();
            std::fs::write(taken.join("keep"), b"k").unwrap();
        }
        let fresh = FreshDir::create_in(parent.path(), "x").unwrap();
        assert_eq!(std::fs::read_dir(fresh.path()).unwrap().count(), 0);
        let kept = std::fs::read_dir(parent.path())
            .unwrap()
            .filter(|e| e.as_ref().unwrap().path().join("keep").is_file())
            .count();
        // Other tests may take numbers meanwhile, but none of the taken
        // directories was emptied.
        assert!(kept >= 3, "{kept}");
    }

    /// `CHEBY_TEST_KEEP_CACHE` names a directory that is created
    /// owner-only, used and never deleted; a relative one is refused, and
    /// an empty one is unset.
    #[test]
    fn cache_kept_directory_is_used_and_kept() {
        let parent = FreshDir::create("cheby-unit-keep").unwrap();
        let kept = parent.path().join("a/b");
        let cache = HarnessCache::create_with("unit-keep", Some(kept.clone().into())).unwrap();
        assert_eq!(cache.path(), kept);
        assert!(cache.is_kept());
        assert!(cache.describe().contains("kept"));
        std::fs::write(kept.join("record"), b"x").unwrap();
        drop(cache);
        assert!(kept.join("record").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&kept).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{mode:o}");
        }
        // An existing directory is used as it is.
        let again = HarnessCache::create_with("unit-keep", Some(kept.clone().into())).unwrap();
        assert!(kept.join("record").is_file());
        drop(again);
        assert!(HarnessCache::create_with("unit-keep", Some("relative/dir".into())).is_err());
        let fresh = HarnessCache::create_with("unit-keep", Some("".into())).unwrap();
        assert!(!fresh.is_kept());
    }

    /// Every child gets the harness's cache, whatever the harness's own
    /// environment says.
    #[test]
    fn cache_is_set_for_a_child() {
        let cache = HarnessCache::create("unit-env").unwrap();
        let mut cmd = Command::new("cheby");
        cmd.env(CACHE_ENV, "/somewhere/else");
        cache.apply(&mut cmd);
        let value = cmd
            .get_envs()
            .find(|(k, _)| *k == OsStr::new(CACHE_ENV))
            .and_then(|(_, v)| v);
        assert_eq!(value, Some(cache.path().as_os_str()));
        let shown = cache.path().display().to_string();
        assert!(cache.describe().contains(&shown));
    }
}
