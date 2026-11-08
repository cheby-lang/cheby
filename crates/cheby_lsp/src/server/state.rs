//! The main loop's state: open documents and packages (A §3.1).
//!
//! Everything here is plain data. A package holds its newest snapshot for
//! the requests of later waves, and nothing that reads a handle.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use cheby_driver::analysis::{Hash128, Snapshot};

use super::schedule::RunState;
use crate::analysis_thread::Changed;

/// One open document.
#[derive(Debug)]
pub(crate) struct Doc {
    /// The client's version of the text.
    pub version: i32,
    /// The text, as the client holds it.
    pub text: String,
    /// `text` shared with the analysis thread, made once per change.
    pub shared: Option<Arc<str>>,
    /// The order of the document's last edit across all documents.
    pub stamp: u64,
    /// The root of the package the document belongs to, `None` for a file
    /// with no `cheby.toml` above it.
    pub root: Option<PathBuf>,
}

impl Doc {
    /// `text` as one shared string, copied at most once per change.
    pub(crate) fn shared(&mut self) -> Arc<str> {
        if let Some(text) = &self.shared {
            return Arc::clone(text);
        }
        let text: Arc<str> = Arc::from(self.text.as_str());
        self.shared = Some(Arc::clone(&text));
        text
    }
}

/// What was last published for one file (A §8.4).
#[derive(Debug)]
pub(crate) struct Published {
    /// The version it was published with, `None` for a closed file.
    pub version: Option<i32>,
    /// The hash of the diagnostics as sent.
    pub sent: Hash128,
    /// The diagnostics as sent, for a republish with a new version.
    pub diagnostics: Vec<lsp_types::Diagnostic>,
}

/// One package, served by one session on the analysis thread (D-1375).
#[derive(Debug)]
pub(crate) struct Package {
    /// The directory of its `cheby.toml`.
    pub root: PathBuf,
    /// Debouncing and running.
    pub run: RunState,
    /// What changed on disk since its last job.
    pub changed: Changed,
    /// The superseded flag of the job in flight.
    pub superseded: Arc<AtomicBool>,
    /// The open documents' versions of the job in flight.
    pub job_versions: BTreeMap<PathBuf, i32>,
    /// The stamp of the newest edit of one of its documents.
    pub last_edit: u64,
    /// What each file was last published with.
    pub published: BTreeMap<PathBuf, Published>,
    /// The newest snapshot, for requests.
    pub snapshot: Option<Arc<Snapshot>>,
    /// The tool errors last shown, so that each list is shown once.
    pub shown_tool: Vec<String>,
    /// A failed run was shown with `window/showMessage` (A §3.7).
    pub shown_failure: bool,
}

impl Package {
    /// A package at `root` with nothing run yet.
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            run: RunState::Idle,
            changed: Changed::default(),
            superseded: Arc::new(AtomicBool::new(false)),
            job_versions: BTreeMap::new(),
            last_edit: 0,
            published: BTreeMap::new(),
            snapshot: None,
            shown_tool: Vec::new(),
            shown_failure: false,
        }
    }
}

/// The root of the package that `path` belongs to: the nearest directory
/// above it, or `path` itself for a directory, that holds a `cheby.toml`,
/// as the CLI finds it (D-1375).
pub(crate) fn package_root(path: &Path) -> Option<PathBuf> {
    let start = if path.is_dir() { path } else { path.parent()? };
    cheby_driver::find_package_root(start)
}

/// Messages already logged, so that each is logged once.
#[derive(Debug, Default)]
pub(crate) struct Once {
    seen: BTreeSet<String>,
}

impl Once {
    /// True the first time `message` is seen.
    pub(crate) fn first(&mut self, message: &str) -> bool {
        self.seen.insert(message.to_owned())
    }
}
