//! Where the cache lives, who may write it, and the temporary fallback
//! (D-959).
//!
//! `CHEBY_CACHE`, when set and not empty, names the root and must be
//! absolute. Otherwise the root is the OS cache directory's `cheby`:
//! `$XDG_CACHE_HOME/cheby` (an absolute `XDG_CACHE_HOME` only, as the XDG
//! specification says) or `$HOME/.cache/cheby` on Linux and other Unix
//! systems, `$HOME/Library/Caches/cheby` on macOS and `%LocalAppData%\cheby`
//! on Windows.
//!
//! GAP(cache-location-env): an empty variable counts as unset, a relative
//! `XDG_CACHE_HOME` is ignored, and an environment that names no home
//! directory (no `HOME`, or no `LocalAppData` on Windows) falls back to
//! the temporary store like a root that cannot be created.
//!
//! On Unix the root must be owned by the effective user and writable by
//! nobody else; anything else is a tool error, never a fallback, because
//! `cheby run` executes code from the cache. On Windows that check waits
//! for M6 (GAP(cache-permissions-windows)).

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::store::{Store, create_private_dir_all};
use super::{BuildId, StoreError};

/// The environment the location reads, passed as a value so that tests
/// never set variables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheEnv {
    /// `CHEBY_CACHE`.
    pub cheby_cache: Option<OsString>,
    /// `XDG_CACHE_HOME`.
    pub xdg_cache_home: Option<OsString>,
    /// `HOME`.
    pub home: Option<OsString>,
    /// `LocalAppData` (Windows).
    pub local_app_data: Option<OsString>,
}

impl CacheEnv {
    /// The variables of this process.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            cheby_cache: std::env::var_os("CHEBY_CACHE"),
            xdg_cache_home: std::env::var_os("XDG_CACHE_HOME"),
            home: std::env::var_os("HOME"),
            local_app_data: std::env::var_os("LOCALAPPDATA"),
        }
    }
}

/// Why a command uses a temporary cache (D-959), for the note the
/// command prints on standard error.
#[derive(Debug)]
pub struct Fallback {
    /// The root that could not be used, or `None` when no default could
    /// be found because the environment names no home directory.
    pub root: Option<PathBuf>,
    /// Why, as a phrase: the I/O error or the missing variable.
    pub reason: String,
}

impl fmt::Display for Fallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.root {
            Some(root) => write!(f, "cannot use `{}`: {}", root.display(), self.reason),
            None => f.write_str(&self.reason),
        }
    }
}

/// The operating systems whose default locations differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Os {
    /// Linux and every Unix other than macOS.
    Linux,
    /// macOS.
    Mac,
    /// Windows.
    Windows,
}

impl Os {
    /// The system this toolchain runs on.
    pub(crate) const HOST: Self = if cfg!(target_os = "macos") {
        Self::Mac
    } else if cfg!(windows) {
        Self::Windows
    } else {
        Self::Linux
    };
}

/// The cache root `env` names on this system: `Ok(None)` when it names
/// none, because neither `CHEBY_CACHE` nor the home directory is set.
///
/// # Errors
///
/// [`StoreError::RelativeCacheVar`] when `CHEBY_CACHE` is relative.
pub fn cache_root(env: &CacheEnv) -> Result<Option<PathBuf>, StoreError> {
    cache_root_on(env, Os::HOST)
}

/// [`cache_root`] for `os`.
pub(crate) fn cache_root_on(env: &CacheEnv, os: Os) -> Result<Option<PathBuf>, StoreError> {
    if let Some(var) = non_empty(env.cheby_cache.as_ref()) {
        let path = PathBuf::from(var);
        return if path.is_absolute() {
            Ok(Some(path))
        } else {
            Err(StoreError::RelativeCacheVar(path))
        };
    }
    let base = match os {
        Os::Linux => non_empty(env.xdg_cache_home.as_ref())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| non_empty(env.home.as_ref()).map(|h| Path::new(h).join(".cache"))),
        Os::Mac => {
            non_empty(env.home.as_ref()).map(|h| Path::new(h).join("Library").join("Caches"))
        }
        Os::Windows => non_empty(env.local_app_data.as_ref()).map(PathBuf::from),
    };
    Ok(base.map(|b| b.join("cheby")))
}

/// The variable's value when it is set and not empty.
fn non_empty(v: Option<&OsString>) -> Option<&OsString> {
    v.filter(|s| !s.is_empty())
}

/// The variable that names the default location on `os`, for messages.
const fn home_var(os: Os) -> &'static str {
    match os {
        Os::Linux | Os::Mac => "HOME",
        Os::Windows => "LocalAppData",
    }
}

impl Store {
    /// Opens the cache that `env` names (D-959). When the root cannot be
    /// created or written, or the environment names none, it opens a
    /// fresh owner-only store in the system's temporary directory instead,
    /// which is deleted when the store is dropped, and returns the reason
    /// for the caller's note.
    ///
    /// # Errors
    ///
    /// [`StoreError::RelativeCacheVar`] and [`StoreError::UnsafeRoot`],
    /// which never fall back, and the error of the temporary store when
    /// that fails too.
    pub fn open_or_temporary(
        env: &CacheEnv,
        build: BuildId,
    ) -> Result<(Self, Option<Fallback>), StoreError> {
        let fallback = match cache_root(env)? {
            None => Fallback {
                root: None,
                reason: format!("neither CHEBY_CACHE nor {} is set", home_var(Os::HOST)),
            },
            Some(root) => match Self::open(&root, build) {
                Ok(store) => return Ok((store, None)),
                Err(e @ (StoreError::RelativeCacheVar(_) | StoreError::UnsafeRoot { .. })) => {
                    return Err(e);
                }
                Err(StoreError::Io { path, error }) => Fallback {
                    root: Some(root),
                    reason: format!("{}: {error}", path.display()),
                },
            },
        };
        let store = Self::temporary(&std::env::temp_dir(), build)?;
        Ok((store, Some(fallback)))
    }

    /// A fresh owner-only store in a new directory under `base`, deleted
    /// when it is dropped.
    pub(crate) fn temporary(base: &Path, build: BuildId) -> Result<Self, StoreError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        create_private_dir_all(base)?;
        let mut last = None;
        for _ in 0..64 {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = base.join(format!("cheby-cache-{}-{n}", std::process::id()));
            let mut builder = fs::DirBuilder::new();
            set_private(&mut builder);
            match builder.create(&dir) {
                Ok(()) => return Self::open_with(&dir, build, true),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some((dir, e)),
                Err(e) => return Err(StoreError::io(dir, e)),
            }
        }
        let (dir, error) =
            last.unwrap_or_else(|| (base.to_path_buf(), io::Error::other("no attempt")));
        Err(StoreError::io(dir, error))
    }
}

/// Makes `builder` create an owner-only directory on Unix.
#[cfg(unix)]
fn set_private(builder: &mut fs::DirBuilder) {
    use std::os::unix::fs::DirBuilderExt as _;
    builder.mode(0o700);
}

/// Windows: the parent's access list applies (GAP(cache-permissions-windows)).
#[cfg(not(unix))]
fn set_private(_builder: &mut fs::DirBuilder) {}

/// Checks that only the effective user can write `root` (D-959).
#[cfg(unix)]
pub(crate) fn check_root(root: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = fs::metadata(root).map_err(|e| StoreError::io(root, e))?;
    // SAFETY: `geteuid` takes no arguments, cannot fail and touches no
    // memory of ours.
    let euid = unsafe { libc::geteuid() };
    root_problem(meta.uid(), meta.mode(), euid).map_or(Ok(()), |why| {
        Err(StoreError::UnsafeRoot {
            root: root.to_path_buf(),
            why,
        })
    })
}

/// GAP(cache-permissions-windows): Windows checks no owner or access list
/// until M6.
#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the Unix check returns errors; M6 adds Windows's"
)]
pub(crate) fn check_root(_root: &Path) -> Result<(), StoreError> {
    Ok(())
}

/// What makes a root owned by `uid` with `mode` unsafe for the user
/// `euid`, if anything.
#[cfg_attr(not(unix), expect(dead_code, reason = "Unix only until M6"))]
pub(crate) fn root_problem(uid: u32, mode: u32, euid: u32) -> Option<String> {
    if uid != euid {
        Some(format!(
            "it is owned by user {uid}, not by the current user {euid}"
        ))
    } else if mode & 0o022 != 0 {
        Some(format!(
            "its mode {:o} lets other users write it; `chmod go-w` it",
            mode & 0o7777
        ))
    } else {
        None
    }
}
