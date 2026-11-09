//! Std for requests (A §6.2, §7, D-1369, D-1370, D-1372).
//!
//! - The std base: every shipped std module checked once per process,
//!   with its plain [`StdIndex`]. Hover reads the index for docs of std
//!   items (D-1385), `workspace/symbol` for std's items, and a request on
//!   an opened std document reads the base's own snapshot.
//! - Std's sources on disk, written once per toolchain by
//!   `materialize_std`, so a location in std is a plain file.
//!
//! Both are made on first need. `run` starts the base on a thread of its
//! own as the server starts ([`warm`]), so the first request seldom waits.
//! A request that needs them before then waits for them, which keeps its
//! answer the same whenever it comes. A failure is logged once per server,
//! and the requests that needed std then answer without it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use cheby_driver::analysis::{Globals, StdBase, StdIndex, materialize_std};
use cheby_driver::cache::BuildId;

use super::Analyzed;
use crate::text::uri::normalize;

/// The warming thread's stack: the check recurses over trees, as the
/// compiler does (D-601). It reserves address space only.
const STACK_SIZE: usize = 256 << 20;

/// The std base of the process, with its snapshot as requests read it.
#[derive(Debug)]
pub(crate) struct StdReady {
    /// Every shipped std module, checked, and its index.
    pub base: Arc<StdBase>,
    /// The base's snapshot, for requests on an opened std document.
    pub analyzed: Arc<Analyzed>,
}

/// The std base, built once per process. It reads no setting and no file,
/// so every server of the process shares it.
static BASE: OnceLock<Result<StdReady, String>> = OnceLock::new();

/// The std base, built on the first call. Later callers wait for it.
fn base() -> &'static Result<StdReady, String> {
    BASE.get_or_init(|| {
        let base = StdBase::build(Arc::new(Globals::new()))
            .map_err(|e| format!("cannot check the standard library: {e}"))?;
        let analyzed = Arc::new(Analyzed::new(Arc::clone(base.snapshot())));
        Ok(StdReady { base, analyzed })
    })
}

/// Builds the std base on a thread of its own, so that it is ready by the
/// first request that needs it. A thread that cannot start is no error:
/// the first such request builds it.
pub(crate) fn warm() {
    let _ = std::thread::Builder::new()
        .name("cheby-std".to_owned())
        .stack_size(STACK_SIZE)
        .spawn(|| {
            let _ = base();
        });
}

/// One server's view of std: the shared base, and std's sources for this
/// toolchain's build.
#[derive(Debug)]
pub(crate) struct StdService {
    /// The build whose sources are written. `None` in tests that read no
    /// std.
    build: Option<BuildId>,
    /// The directory std's sources were written to, made on first need.
    dir: OnceLock<Result<PathBuf, String>>,
    /// A failure was logged.
    reported: AtomicBool,
}

impl StdService {
    /// Std for a server of `build`.
    pub(crate) const fn new(build: BuildId) -> Self {
        Self {
            build: Some(build),
            dir: OnceLock::new(),
            reported: AtomicBool::new(false),
        }
    }

    /// No std at all: every request answers without it.
    #[cfg(test)]
    pub(crate) const fn none() -> Self {
        Self {
            build: None,
            dir: OnceLock::new(),
            reported: AtomicBool::new(false),
        }
    }

    /// The std base, waiting for it when it is being built. `None` when it
    /// failed.
    pub(crate) fn ready(&self) -> Option<&'static StdReady> {
        self.build?;
        base().as_ref().ok()
    }

    /// The index of std's items.
    pub(crate) fn index(&self) -> Option<&'static StdIndex> {
        self.ready().map(|ready| ready.base.index())
    }

    /// The directory of std's sources, whose `std/…` files a std location
    /// names, written on the first call. Normalized as routing names it
    /// (`server::sessions::std_dirs`).
    pub(crate) fn dir(&self) -> Option<&Path> {
        let build = self.build?;
        self.dir
            .get_or_init(|| {
                materialize_std(build)
                    .map(|dir| normalize(&std::path::absolute(&dir).unwrap_or(dir)))
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .ok()
            .map(PathBuf::as_path)
    }

    /// The first failure of std that this server has not logged yet.
    pub(crate) fn unreported_failure(&self) -> Option<String> {
        let failure = BASE
            .get()
            .and_then(|base| base.as_ref().err())
            .or_else(|| self.dir.get().and_then(|dir| dir.as_ref().err()))?;
        (!self.reported.swap(true, Ordering::Relaxed)).then(|| failure.clone())
    }
}
