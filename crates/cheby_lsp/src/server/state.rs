//! The main loop's state: open documents and packages (A §3.1).
//!
//! Everything here is plain data. A package holds its newest snapshot for
//! the requests of later waves, and nothing that reads a handle.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use cheby_driver::analysis::{Hash128, PackageKind, Snapshot};

use super::schedule::{JobTexts, RunState};
use super::sessions::DocRoute;
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
    /// The key of the session that analyzes the document: its package's
    /// root, or the document's own path in single-file mode (D-1371), and
    /// `None` for a std file (A §6.4).
    pub root: Option<PathBuf>,
    /// Where the document routes by its own path (A §6.4).
    pub route: DocRoute,
    /// A std document was edited, so its positions no longer match std's
    /// snapshot (A §6.3).
    pub diverged: bool,
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
    /// `didOpen` or `didClose` forgot this publish, so the path's next
    /// publish is sent even when it equals this one (D-1442).
    pub forgotten: bool,
}

/// One package, served by one session on the analysis thread (D-1375),
/// or one file in single-file mode (D-1371).
#[derive(Debug)]
pub(crate) struct Package {
    /// The directory of its `cheby.toml`, or in single-file mode the file,
    /// which keys the session.
    pub root: PathBuf,
    /// A package, or single-file mode on the file at `root`.
    pub kind: PackageKind,
    /// `root` with its symbolic links resolved, when that differs: a
    /// watcher may report `/private/tmp/…` for `/tmp/…` on macOS (A §6.1).
    pub real: Option<PathBuf>,
    /// Debouncing and running.
    pub run: RunState,
    /// What changed on disk since its last job.
    pub changed: Changed,
    /// The superseded flag of the job in flight.
    pub superseded: Arc<AtomicBool>,
    /// The open documents' versions of the job in flight.
    pub job_versions: BTreeMap<PathBuf, i32>,
    /// The open documents' edit stamps of the job in flight: a report
    /// covers a document only while its stamp is the one the job read,
    /// since a reopened document starts at version 0 again (cp-notes, CP1).
    pub job_stamps: BTreeMap<PathBuf, u64>,
    /// The stamp of the newest edit of one of its documents.
    pub last_edit: u64,
    /// What each file was last published with.
    pub published: BTreeMap<PathBuf, Published>,
    /// The newest snapshot, for requests.
    pub snapshot: Option<Arc<Snapshot>>,
    /// The texts of the last Q run, when it gave no snapshot and no
    /// snapshot came since: a request on exactly these texts gets its
    /// fallback at once, without a run that would fail again (A §3.7).
    pub failed: Option<JobTexts>,
    /// The tool errors last shown, so that each list is shown once.
    pub shown_tool: Vec<String>,
    /// A failed run was shown with `window/showMessage` (A §3.7).
    pub shown_failure: bool,
    /// The root is a workspace folder: the session starts at `initialized`
    /// and lives while the folder does (D-1375).
    pub folder: bool,
    /// When the package's last open document closed, for a session that
    /// ends 120 s later (D-1375). `None` while a document is open, and for
    /// a folder's session.
    pub idle_since: Option<Instant>,
    /// How the package learns of disk changes (A §8.1, §8.3).
    pub watch: Watched,
}

/// How a package learns of disk changes (D-1374).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Watched {
    /// The workspace folders' watcher covers it.
    Folders,
    /// A watcher of its own, with relative patterns, by registration id.
    Own(String),
    /// No watcher: a sweep every 2 s while it has an open document.
    Sweep,
    /// Nothing to watch: a single-file session reads its one document from
    /// the editor, and the routing at each open, close and save finds a
    /// `cheby.toml` that appears above it (A §8.3).
    Editor,
}

impl Package {
    /// A package at `root`, or a single-file session keyed by it, with
    /// nothing run yet.
    pub(crate) fn new(root: PathBuf, kind: PackageKind) -> Self {
        Self {
            root,
            kind,
            real: None,
            run: RunState::Idle,
            changed: Changed::default(),
            superseded: Arc::new(AtomicBool::new(false)),
            job_versions: BTreeMap::new(),
            job_stamps: BTreeMap::new(),
            last_edit: 0,
            published: BTreeMap::new(),
            snapshot: None,
            failed: None,
            shown_tool: Vec::new(),
            shown_failure: false,
            folder: false,
            idle_since: None,
            watch: Watched::Sweep,
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
