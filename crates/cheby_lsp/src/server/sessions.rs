//! Routing and sessions (A §6.3 to §6.5, D-1371, D-1375).
//!
//! A document routes by its own path, never by `rootUri` or the workspace
//! folders: Zed gives a file opened outside its worktree a server rooted
//! at the file's directory, with no workspace folders (cp-notes, CP1). A
//! file under the materialized std directory is std's (A §6.3). Any other
//! belongs to the nearest `cheby.toml` above it, as the CLI finds it
//! (D-1375), when it is a module of that package or its manifest. A
//! `.cheby` file with no package, or one inside a package that is not a
//! module, is for single-file mode (D-1371). Any other file, such as a
//! `cheby.toml` with no package of its own, is no Cheby source: no session
//! analyzes it, and it is never published (D-1446).
//!
//! One session serves each package root (D-1375). A workspace folder that
//! is a package root gets its session at `initialized` and keeps it while
//! the folder lasts. Any other package gets one at its first open document
//! and ends 120 s after its last one closes, once no job or request needs
//! it: each file it published then gets an empty list, since nothing will
//! refresh it. The workspace folders only seed sessions; there is no deep
//! scan, so opening a tree of 32 packages starts none of the others
//! (A §6.5). A nested package's files are published by the nearest
//! package only (`publish`).
//!
//! A document for single-file mode gets a session of its own, keyed by its
//! path, whose jobs go to the analysis thread as a package's do (D-1371,
//! A §6.4). It publishes the document as a package publishes an open one
//! (D-1373, D-1442), and ends when the document closes, with one empty
//! publish and no grace period (D-1446), since nothing refreshes a closed
//! file outside every package.
//! When a `cheby.toml` appears above the file, the routing at the next
//! open, close, save or watched manifest moves the document into that
//! package, and the single-file session ends.
//!
//! A `.cheby` file with no package above it that lies under the `std/` of
//! a checkout of this repository, a directory that holds
//! `crates/cheby_driver/src/std_lib.rs`, belongs to that checkout's
//! session, which checks it as a module of package `std` (D-1699,
//! D-1700). The nearest checkout above the file wins, and the disk is
//! read at each route with no cache, as for a package (D-1707). The
//! session is rooted at the checkout, starts at its first open document,
//! never at `initialized`, and ends 120 s after its last one closes, as a
//! package's does (D-1704). It also ends, and its documents route again,
//! when its `std_lib.rs` is gone or a `cheby.toml` appears at its `std/`
//! or above (D-1699). At each start it compares the checkout's `std/` on
//! disk with the std this binary embeds, and says so once when they
//! differ (D-1705).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use cheby_driver::analysis::PackageKind;
use cheby_driver::cache::{BuildId, CacheEnv, Hash128, cache_root};
use cheby_driver::std_lib::{STAGED_STD_MODULES, STD_MODULES, source_hashes};
use lsp_types::{DidChangeWorkspaceFoldersParams, MessageType, PublishDiagnosticsParams};
use serde_json::Value;

use super::docs::SWEEP;
use super::effects::Effect;
use super::schedule::{RunState, Urgency};
use super::state::{Package, Watched};
use super::{MANIFEST, Server};
use crate::analysis_thread::{Changed, Work};
use crate::text::uri::{normalize, uri_to_path};

/// How long a session with no open document outlives its last one
/// (D-1375). Tests wait a moment only.
const LINGER: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(120)
};

/// D-1705's `window/showMessage`, a `Warning` sent at each start of a std
/// checkout's session whose `std/` is not the binary's, which also sets
/// `stdDiffers: true` on that package's `cheby/status` entry
/// (`ensure_package`). D's goldens pin the text.
pub(crate) const STD_DIFFERS: &str = "This checkout's std/ is not the std that this cheby binary \
    embeds, so an error may come from an older compiler and a rebuild may clear it.";

/// The directory of a checkout that holds std's sources, written in lower
/// case (D-1707).
pub(crate) const STD_DIR: &str = "std";

/// The file that marks a checkout of this repository, under the checkout:
/// the one that embeds std (D-450, D-1699).
const MARKER: [&str; 4] = ["crates", "cheby_driver", "src", "std_lib.rs"];

/// The deepest nesting of `std/` that the comparison walks, as package
/// discovery does.
const MAX_DEPTH: usize = 64;

/// Where a document routes by its own path (A §6.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DocRoute {
    /// A materialized std file: never published, and answered from std's
    /// snapshot (A §6.3).
    Std,
    /// A module, or the manifest, of the package at this root.
    Package(PathBuf),
    /// Single-file mode (D-1371), in a session of its own. `strict` when
    /// no package is anywhere above the file, which gets all of a run's
    /// diagnostics minus W0005; otherwise the file sits in a package
    /// without being one of its modules, and gets phase-A diagnostics
    /// only.
    SingleFile {
        /// The file, which keys its session.
        file: PathBuf,
        /// No `cheby.toml` above the file.
        strict: bool,
    },
    /// A `.cheby` file under the `std/` of a checkout of this repository
    /// (D-1699), checked as package `std` in the session of the checkout,
    /// which keys it (D-1700, D-1704).
    StdCheckout {
        /// The checkout, the directory that holds `std/`.
        checkout: PathBuf,
        /// The file.
        file: PathBuf,
    },
    /// No session's: a file that is not std's, not a package's, and not a
    /// `.cheby` file, such as a `cheby.toml` whose package is gone.
    Unowned,
}

impl DocRoute {
    /// The key of the session that analyzes the document: the package
    /// root, the checkout, or the file itself in single-file mode.
    pub(crate) fn root(&self) -> Option<&Path> {
        match self {
            Self::Package(root) | Self::StdCheckout { checkout: root, .. } => Some(root),
            Self::SingleFile { file, .. } => Some(file),
            Self::Std | Self::Unowned => None,
        }
    }

    /// What that session analyzes. `None` for std and an unowned file,
    /// which have no session.
    pub(crate) fn kind(&self) -> Option<PackageKind> {
        match self {
            Self::Package(_) => Some(PackageKind::Package),
            Self::SingleFile { file, strict } => Some(PackageKind::SingleFile {
                file: file.clone(),
                strict: *strict,
            }),
            Self::StdCheckout { checkout, .. } => Some(PackageKind::StdCheckout {
                checkout: checkout.clone(),
            }),
            Self::Std | Self::Unowned => None,
        }
    }
}

/// Routes `path`, a normalized absolute path, by A §6.4's table: under one
/// of `std_dirs` is std; else the nearest package root above it owns it
/// when it is a module or the manifest there; else a `.cheby` file goes to
/// the nearest std checkout above it (D-1699), else to single-file mode,
/// and any other file to no session.
pub(crate) fn route(path: &Path, std_dirs: &[PathBuf]) -> DocRoute {
    if std_dirs.iter().any(|dir| path.starts_with(dir)) {
        return DocRoute::Std;
    }
    let file = path.to_path_buf();
    let source = path
        .extension()
        .is_some_and(|ext| ext == cheby_driver::EXTENSION);
    match path.parent().and_then(cheby_driver::find_package_root) {
        Some(root) if is_package_file(&root, path) => DocRoute::Package(root),
        _ if !source => DocRoute::Unowned,
        Some(_) => DocRoute::SingleFile {
            file,
            strict: false,
        },
        None => match std_checkout(path) {
            Some(checkout) => DocRoute::StdCheckout { checkout, file },
            None => DocRoute::SingleFile { file, strict: true },
        },
    }
}

/// The checkout of this repository whose `std/` holds `path`, which has no
/// `cheby.toml` above it: the nearest directory above a `std` component of
/// the path that holds [`MARKER`], tested on disk with no cache (D-1699,
/// D-1707).
fn std_checkout(path: &Path) -> Option<PathBuf> {
    checkout_dirs(path)
        .into_iter()
        .find(|dir| marker(dir).is_file())
}

/// The directories that may be a std checkout holding `path`, nearest
/// first: the parent of each component of the path above the file that is
/// [`STD_DIR`] as written (D-1707). Lexical, so an unsaved buffer counts.
fn checkout_dirs(path: &Path) -> Vec<PathBuf> {
    path.ancestors()
        .skip(1)
        .filter(|dir| dir.file_name().is_some_and(|name| name == STD_DIR))
        .filter_map(Path::parent)
        .map(Path::to_path_buf)
        .collect()
}

/// The std checkout whose `std/` is `path` or holds it, a file or a
/// folder, for D-1706's refusal of a move there: the nearest directory
/// that holds [`MARKER`] among the parent of `path` when `path` is a
/// [`STD_DIR`] itself and [`checkout_dirs`], tested on disk with no
/// cache, as [`std_checkout`] finds one for a file (D-1699, D-1707).
/// `None` when a `cheby.toml` is at `path` or above it, where a package
/// owns the path, as routing gives it. Lexical before the disk, so a
/// folder routes as a file does, with no session needed.
fn std_checkout_holding(path: &Path) -> Option<PathBuf> {
    if cheby_driver::find_package_root(path).is_some() {
        return None;
    }
    path.file_name()
        .filter(|name| *name == STD_DIR)
        .and_then(|_| path.parent())
        .map(Path::to_path_buf)
        .into_iter()
        .chain(checkout_dirs(path))
        .find(|dir| marker(dir).is_file())
}

/// The marker file of a checkout at `dir`, built from components.
fn marker(dir: &Path) -> PathBuf {
    MARKER
        .iter()
        .fold(dir.to_path_buf(), |path, part| path.join(part))
}

/// Whether `checkout` is still a std checkout, so that its session goes on
/// (D-1699): its marker file is there, and no `cheby.toml` is at its
/// `std/` or above.
fn is_checkout(checkout: &Path) -> bool {
    marker(checkout).is_file() && cheby_driver::find_package_root(&checkout.join(STD_DIR)).is_none()
}

/// Whether the `std/` of the checkout at `checkout` on disk is not the std
/// that this binary embeds (D-1705): its module set, every `.cheby` file
/// under `std/`, is not `STD_MODULES` and `STAGED_STD_MODULES`, which
/// `build.rs` hashed, or a file's bytes do not have its module's
/// `std_lib::source_hashes`. A file or directory that cannot be read
/// differs.
pub(crate) fn std_differs(checkout: &Path) -> bool {
    let mut found = BTreeMap::new();
    if !std_files(&checkout.join(STD_DIR), "", 0, &mut found) {
        return true;
    }
    let embedded: BTreeSet<&str> = STD_MODULES
        .iter()
        .chain(STAGED_STD_MODULES)
        .map(|module| module.path)
        .collect();
    // Both are sorted by path.
    if !found
        .keys()
        .map(String::as_str)
        .eq(embedded.iter().copied())
    {
        return true;
    }
    found.iter().any(|(module, file)| {
        let Ok(bytes) = std::fs::read(file) else {
            return true;
        };
        source_hashes(module).is_none_or(|hashes| hashes.sip128 != Hash128::of(&bytes))
    })
}

/// Adds each `.cheby` file under `dir` to `found` by its module path, with
/// `prefix` before it, as `build.rs` names std's modules: `::` between
/// directory names, the extension dropped, and a name that starts with
/// `.` or is not Unicode skipped. False when a directory cannot be read.
fn std_files(
    dir: &Path,
    prefix: &str,
    depth: usize,
    found: &mut BTreeMap<String, PathBuf>,
) -> bool {
    if depth > MAX_DEPTH {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    // `found` is sorted, so the order of the entries reaches nothing.
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            let inner = format!("{prefix}{name}::");
            if !std_files(&path, &inner, depth.saturating_add(1), found) {
                return false;
            }
        } else if let Some(stem) = name.strip_suffix(".cheby") {
            found.insert(format!("{prefix}{stem}"), path.clone());
        }
    }
    true
}

/// Whether `path` is the manifest of the package at `root`, or a file that
/// `cheby_package::module_file` takes as one of its modules: lexical, as
/// that function is, so an unsaved buffer counts.
fn is_package_file(root: &Path, path: &Path) -> bool {
    // The package's name only names the module, which is not needed here.
    path.strip_prefix(root).is_ok_and(|relative| {
        relative == Path::new(MANIFEST) || cheby_driver::module_file(root, "main", path).is_some()
    })
}

/// The directories std's sources may be materialized in for this build
/// (A §6.2): under the cache root, and the temporary fallback. Each comes
/// as given and resolved, since a client may name either, such as
/// `/private/tmp` for `/tmp` on macOS. The directories need not exist
/// yet, since std is materialized after the server starts: the deepest
/// one of their ancestors that exists is resolved, and the rest appended.
pub(crate) fn std_dirs(build: BuildId) -> Vec<PathBuf> {
    let hex = build.to_hex();
    let mut dirs = Vec::new();
    if let Ok(Some(root)) = cache_root(&CacheEnv::from_process()) {
        dirs.push(root.join("lsp").join("std-src").join(&hex));
    }
    dirs.push(std::env::temp_dir().join(format!("cheby-std-{hex}")));
    let mut all = Vec::new();
    for dir in dirs {
        let dir = normalize(&std::path::absolute(&dir).unwrap_or(dir));
        if let Some(real) = resolved(&dir) {
            let real = normalize(&real);
            if real != dir && !all.contains(&real) {
                all.push(real);
            }
        }
        if !all.contains(&dir) {
            all.push(dir);
        }
    }
    all
}

/// `path` with the symbolic links of its deepest existing ancestor
/// resolved, and the part below that ancestor appended as it is.
fn resolved(path: &Path) -> Option<PathBuf> {
    path.ancestors().find_map(|ancestor| {
        let real = ancestor.canonicalize().ok()?;
        let rest = path.strip_prefix(ancestor).ok()?;
        Some(if rest.as_os_str().is_empty() {
            real
        } else {
            real.join(rest)
        })
    })
}

impl Server {
    /// Where the document at `path` routes now.
    pub(crate) fn route_of(&self, path: &Path) -> DocRoute {
        route(path, &self.std_dirs)
    }

    /// Whether a move of `path`, a file or a folder, renames a module of a
    /// std checkout: `path` is a checkout's `std/` or lies under it, and
    /// is not under the materialized std (D-1369). D-1706 refuses such a
    /// move from routing alone, whether or not the checkout's session
    /// lives, since a folder routes to no session and the session starts
    /// only at an open document (D-1704).
    pub(crate) fn is_std_checkout_move(&self, path: &Path) -> bool {
        !self.std_dirs.iter().any(|dir| path.starts_with(dir))
            && std_checkout_holding(path).is_some()
    }

    /// Whether the session at `root` owns `path`, so that it alone
    /// publishes it (A §6.5).
    pub(crate) fn owns(&self, root: &Path, path: &Path) -> bool {
        self.route_of(path).root() == Some(root)
    }

    /// A session for the package at `root`, for the std checkout at `root`,
    /// or for single-file mode on the file at `root`, made if it has none,
    /// with its way of hearing of disk changes (A §8.1). A single-file
    /// session whose file now has a package above it, or no longer has one,
    /// takes the new `kind`, and its next job replaces the analysis
    /// thread's session (D-1371), while a package's session at a root that
    /// is now a std checkout, or the other way, ends and is made again. A
    /// new checkout session compares the checkout's `std/` with the
    /// binary's, and when they differ shows D-1705's warning, at each
    /// start, and keeps the answer for `cheby/status`.
    pub(crate) fn ensure_package(&mut self, root: &Path, kind: PackageKind) {
        let checkout = |kind: &PackageKind| matches!(kind, PackageKind::StdCheckout { .. });
        if let Some(package) = self.packages.get_mut(root) {
            // A package root that became a std checkout, or the other way,
            // as a `cheby.toml` came or went: a session of the other kind
            // ends, and a new one starts, with its own watcher, lifetime and
            // comparison (D-1699, D-1704).
            if checkout(&package.kind) == checkout(&kind) {
                package.kind = kind;
                return;
            }
            self.end_session(root);
        }
        let (single, checkout) = match &kind {
            PackageKind::SingleFile { .. } => (true, false),
            PackageKind::StdCheckout { .. } => (false, true),
            PackageKind::Package => (false, false),
        };
        let mut package = Package::new(root.to_path_buf(), kind);
        package.real = root
            .canonicalize()
            .ok()
            .map(|real| normalize(&real))
            .filter(|real| real != root);
        package.watch = if single {
            Watched::Editor
        } else {
            self.watch_package(root, checkout)
        };
        package.std_differs = checkout && std_differs(root);
        let differs = package.std_differs;
        self.packages.insert(root.to_path_buf(), package);
        if differs {
            self.apply(Effect::show(MessageType::WARNING, STD_DIFFERS));
        }
    }

    /// The session for the document route `route`, made if it has none.
    pub(crate) fn ensure_route(&mut self, route: &DocRoute) {
        if let (Some(root), Some(kind)) = (route.root(), route.kind()) {
            let root = root.to_path_buf();
            self.ensure_package(&root, kind);
        }
    }

    /// Records whether the package at `root` has an open document: a
    /// session that has none and is not a folder's starts its 120 s
    /// (D-1375), and one that has one again keeps going.
    pub(crate) fn settle(&mut self, root: &Path) {
        let open = self
            .docs
            .values()
            .any(|doc| doc.root.as_deref() == Some(root));
        let Some(package) = self.packages.get_mut(root) else {
            return;
        };
        if open || package.folder {
            package.idle_since = None;
        } else if package.idle_since.is_none() {
            package.idle_since = Some(Instant::now());
        }
    }

    /// When the next idle session may end, for the loop's timer. A session
    /// that still has a job or a parked request does not count: its event
    /// or its request wakes the loop.
    pub(crate) fn session_deadline(&self) -> Option<Instant> {
        self.packages
            .values()
            .filter(|package| self.may_end(package))
            .filter_map(|package| package.idle_since?.checked_add(LINGER))
            .min()
    }

    /// Whether nothing but its 120 s keeps `package`'s session.
    fn may_end(&self, package: &Package) -> bool {
        package.idle_since.is_some()
            && package.run == RunState::Idle
            && self.running.as_deref() != Some(package.root.as_path())
            && !self.parked.iter().any(|parked| parked.root == package.root)
    }

    /// Ends each session whose 120 s are over (D-1375).
    pub(crate) fn end_idle_sessions(&mut self) {
        let now = Instant::now();
        let due: Vec<PathBuf> = self
            .packages
            .values()
            .filter(|package| self.may_end(package))
            .filter(|package| {
                package
                    .idle_since
                    .and_then(|since| since.checked_add(LINGER))
                    .is_some_and(|end| end <= now)
            })
            .map(|package| package.root.clone())
            .collect();
        for root in due {
            self.end_session(&root);
        }
    }

    /// Ends the session at `root`: each file it published gets an empty
    /// list, unless another live session owns it now, its watcher is
    /// unregistered, and the analysis thread drops its `Session` (A §6.5).
    /// A request still waiting on it gets its fallback.
    pub(crate) fn end_session(&mut self, root: &Path) {
        let Some(package) = self.packages.remove(root) else {
            return;
        };
        self.served.remove(root);
        self.end_children(root);
        // A job of this root still on the thread runs first, since the
        // thread takes its work in order. Its events then reach no session
        // (`on_orphan`), and its D run is skipped.
        if self.running.as_deref() == Some(root) {
            self.orphan_job = true;
            package.superseded.store(true, Ordering::Relaxed);
        }
        let _ = self.jobs.send(Work::Drop(root.to_path_buf()));
        self.wake(root);
        for path in package.published.keys() {
            let elsewhere = self
                .route_of(path)
                .root()
                .is_some_and(|owner| owner != root && self.packages.contains_key(owner));
            if elsewhere {
                continue;
            }
            let Ok(uri) = self.uris.uri_of(path) else {
                continue;
            };
            let params = PublishDiagnosticsParams {
                uri,
                diagnostics: Vec::new(),
                version: self.docs.get(path).map(|doc| doc.version),
            };
            self.apply(Effect::Notify {
                method: "textDocument/publishDiagnostics",
                params: serde_json::to_value(params).unwrap_or_default(),
            });
        }
        if let Watched::Own(id) = package.watch {
            self.unwatch(&id);
        }
    }

    /// Routes every open document again, after a `cheby.toml` came or
    /// went (A §8.2), and at each open, close and save, which a client
    /// with no watcher may be the only news of one from: a document whose
    /// route changed leaves its old session and joins its new one, and its
    /// last publish is forgotten. A session whose `cheby.toml` is gone
    /// ends (A §2.6), and so does a single-file session whose document
    /// left it (D-1371), and a std checkout's session whose marker file is
    /// gone or that a `cheby.toml` now holds (D-1699). A workspace folder
    /// whose `cheby.toml` is back gets its session again (D-1375).
    pub(crate) fn reroute(&mut self) {
        let paths: Vec<PathBuf> = self.docs.keys().cloned().collect();
        for path in paths {
            let route = self.route_of(&path);
            if self.docs.get(&path).is_none_or(|doc| doc.route == route) {
                continue;
            }
            let stamp = self.next_stamp();
            let Some(doc) = self.docs.get_mut(&path) else {
                continue;
            };
            let old = doc.root.take();
            doc.root = route.root().map(Path::to_path_buf);
            let route_kind = route.kind();
            doc.route = route;
            // A new identity of its text: a request parked on the old
            // session is answered `ContentModified` (D-1367).
            doc.stamp = stamp;
            let new = doc.root.clone();
            self.forget_publish(&path);
            // A package whose manifest went is no package now: its session
            // ends below, and the rest of the session's files are cleared.
            // A single-file session the document left ends below as well,
            // and so does a checkout that is no checkout now.
            let stays = |old: &PathBuf| {
                old.join(MANIFEST).is_file()
                    || self.packages.get(old).is_some_and(|package| {
                        matches!(package.kind, PackageKind::StdCheckout { .. }) && is_checkout(old)
                    })
            };
            if let Some(old) = old.filter(stays) {
                self.touch(&old, stamp);
                self.mark_dirty(&old, SWEEP, Urgency::Flush);
                self.settle(&old);
            }
            if let (Some(new), Some(kind)) = (new, route_kind) {
                self.ensure_package(&new, kind);
                self.touch(&new, stamp);
                self.mark_dirty(&new, SWEEP, Urgency::Flush);
                self.settle(&new);
            }
        }
        let gone: Vec<PathBuf> = self
            .packages
            .values()
            .filter(|package| match package.kind {
                PackageKind::Package => !package.root.join(MANIFEST).is_file(),
                PackageKind::SingleFile { .. } => !self
                    .docs
                    .values()
                    .any(|doc| doc.root.as_ref() == Some(&package.root)),
                PackageKind::StdCheckout { .. } => !is_checkout(&package.root),
            })
            .map(|package| package.root.clone())
            .collect();
        for root in gone {
            self.end_session(&root);
        }
        // A workspace folder whose `cheby.toml` came back is a package
        // root again, with its eager session (D-1375). A document that
        // rejoined it above made a session that is not yet the folder's.
        let back: Vec<PathBuf> = self
            .folders
            .iter()
            .filter(|folder| self.packages.get(*folder).is_none_or(|p| !p.folder))
            .cloned()
            .collect();
        for folder in back {
            self.start_folder(&folder);
        }
    }

    /// `didOpen` and `didClose` forget the path's last publish, so its next
    /// one is sent even when unchanged (D-1442).
    pub(crate) fn forget_publish(&mut self, path: &Path) {
        for package in self.packages.values_mut() {
            if let Some(published) = package.published.get_mut(path) {
                published.forgotten = true;
            }
        }
    }

    /// `workspace/didChangeWorkspaceFolders`: an added folder that is a
    /// package root gets a session now, and a removed one's session ends
    /// as any other once it has no open document (A §6.5).
    pub(crate) fn did_change_workspace_folders(&mut self, params: Value) {
        let Some(params) =
            self.params::<DidChangeWorkspaceFoldersParams>("didChangeWorkspaceFolders", params)
        else {
            return;
        };
        let path_of = |uri: &lsp_types::Uri| uri_to_path(uri).map(|path| normalize(&path));
        for removed in &params.event.removed {
            let Some(folder) = path_of(&removed.uri) else {
                continue;
            };
            self.folders.retain(|known| *known != folder);
            if let Some(package) = self.packages.get_mut(&folder) {
                package.folder = false;
                self.settle(&folder);
            }
            // The folders' watcher no longer covers the packages in it.
            let uncovered: Vec<PathBuf> = self
                .packages
                .values()
                .filter(|package| package.watch == Watched::Folders)
                .filter(|package| package.root.starts_with(&folder))
                .map(|package| package.root.clone())
                .collect();
            for root in uncovered {
                let checkout = self
                    .packages
                    .get(&root)
                    .is_some_and(|package| matches!(package.kind, PackageKind::StdCheckout { .. }));
                let watch = self.watch_package(&root, checkout);
                if let Some(package) = self.packages.get_mut(&root) {
                    package.watch = watch;
                }
            }
        }
        for added in &params.event.added {
            let Some(folder) = path_of(&added.uri) else {
                continue;
            };
            if !self.folders.contains(&folder) {
                self.folders.push(folder.clone());
            }
            self.start_folder(&folder);
        }
    }

    /// Starts the session of a workspace folder that is a package root, so
    /// that its closed files are published before anything opens (D-1375).
    pub(crate) fn start_folder(&mut self, folder: &Path) {
        if !folder.join(MANIFEST).is_file() {
            return;
        }
        self.ensure_package(folder, PackageKind::Package);
        if let Some(package) = self.packages.get_mut(folder) {
            package.folder = true;
        }
        self.settle(folder);
        self.mark_dirty(folder, Changed::default(), Urgency::Flush);
    }

    /// The order of the next edit.
    pub(crate) fn next_stamp(&mut self) -> u64 {
        self.stamp = self.stamp.saturating_add(1);
        self.stamp
    }

    /// Records `stamp` as the package's newest edit.
    pub(crate) fn touch(&mut self, root: &Path, stamp: u64) {
        if let Some(package) = self.packages.get_mut(root) {
            package.last_edit = stamp;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree in a fresh temporary directory, removed on drop.
    struct Tree(PathBuf);

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Tree {
        fn new(name: &str, files: &[&str]) -> Self {
            let dir =
                std::env::temp_dir().join(format!("cheby_lsp_route_{}_{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for file in files {
                let path = dir.join(file);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, "").unwrap();
            }
            Self(normalize(&std::path::absolute(dir).unwrap()))
        }
    }

    /// A §6.4's table, with a nested package and std. No file here is
    /// checked, so the table runs on every platform.
    #[test]
    fn route_table() {
        let tree = Tree::new(
            "table",
            &[
                "app/cheby.toml",
                "app/src/main.cheby",
                "app/src/geo/shapes.cheby",
                "app/notes.cheby",
                "app/src/Bad-Name.cheby",
                "app/src/bad-dir/x.cheby",
                "app/src/inner/cheby.toml",
                "app/src/inner/src/lib.cheby",
                "app/src/inner/tools/gen.cheby",
                "loose/script.cheby",
                "std-src/std/io.cheby",
            ],
        );
        let at = |file: &str| tree.0.join(file);
        let std_dirs = [at("std-src")];
        let app = DocRoute::Package(at("app"));
        let inner = DocRoute::Package(at("app/src/inner"));
        let strict = |file: &str| DocRoute::SingleFile {
            file: at(file),
            strict: true,
        };
        let in_package = |file: &str| DocRoute::SingleFile {
            file: at(file),
            strict: false,
        };
        let table = [
            ("app/cheby.toml", app.clone()),
            ("app/src/main.cheby", app.clone()),
            ("app/src/geo/shapes.cheby", app.clone()),
            // Not yet on disk: an unsaved buffer under `src/` is a module.
            ("app/src/new.cheby", app),
            ("app/notes.cheby", in_package("app/notes.cheby")),
            (
                "app/src/Bad-Name.cheby",
                in_package("app/src/Bad-Name.cheby"),
            ),
            (
                "app/src/bad-dir/x.cheby",
                in_package("app/src/bad-dir/x.cheby"),
            ),
            // No Cheby source: no session, and no publish.
            ("app/src/main.txt", DocRoute::Unowned),
            // The nearest `cheby.toml` wins (D-1375).
            ("app/src/inner/cheby.toml", inner.clone()),
            ("app/src/inner/src/lib.cheby", inner),
            (
                "app/src/inner/tools/gen.cheby",
                in_package("app/src/inner/tools/gen.cheby"),
            ),
            ("loose/script.cheby", strict("loose/script.cheby")),
            ("loose/unsaved.cheby", strict("loose/unsaved.cheby")),
            // A manifest with no package of its own, not on disk: in no
            // package, or in one that does not take it.
            ("loose/cheby.toml", DocRoute::Unowned),
            ("app/docs/cheby.toml", DocRoute::Unowned),
            // Std is std before any package is looked for (A §6.3).
            ("std-src/std/io.cheby", DocRoute::Std),
        ];
        check_routes(&tree, &std_dirs, table);
    }

    /// D-1699's and D-1707's rows of the table: a std checkout's files,
    /// nested checkouts, and the paths that are not a checkout's. No file
    /// here is checked, so the table runs on every platform.
    #[test]
    fn route_table_checkouts() {
        let tree = Tree::new(
            "checkouts",
            &[
                // A checkout, and its files.
                "co/crates/cheby_driver/src/std_lib.rs",
                "co/std/list.cheby",
                "co/std/json/tree.cheby",
                "co/std/Bad.cheby",
                "co/std/unshipped_probe.cheby",
                "co/std/notes.md",
                "co/Std/list.cheby",
                "co/docs/examples/x/cheby.toml",
                "co/docs/examples/x/src/main.cheby",
                "co/tests/ui/x.cheby",
                // A checkout nested in the first one's `std/`, and a
                // `std` directory with no marker inside the first one's.
                "co/std/inner/crates/cheby_driver/src/std_lib.rs",
                "co/std/inner/std/list.cheby",
                "co/std/plain/std/list.cheby",
                // A `std/` with no marker.
                "bare/std/list.cheby",
                // A marker under a package: the package wins (D-1699).
                "pkg/cheby.toml",
                "pkg/crates/cheby_driver/src/std_lib.rs",
                "pkg/std/list.cheby",
                "pkg/src/crates/cheby_driver/src/std_lib.rs",
                "pkg/src/std/list.cheby",
            ],
        );
        let at = |file: &str| tree.0.join(file);
        let strict = |file: &str| DocRoute::SingleFile {
            file: at(file),
            strict: true,
        };
        let checkout = |dir: &str, file: &str| DocRoute::StdCheckout {
            checkout: at(dir),
            file: at(file),
        };
        let table = [
            // A std checkout's files (D-1699), nested ones included, and
            // an unsaved buffer (D-1707).
            ("co/std/list.cheby", checkout("co", "co/std/list.cheby")),
            (
                "co/std/json/tree.cheby",
                checkout("co", "co/std/json/tree.cheby"),
            ),
            (
                "co/std/unsaved.cheby",
                checkout("co", "co/std/unsaved.cheby"),
            ),
            // A path that names no module, and a planned module's: the
            // checkout's session, which gives them phase A (D-1700).
            ("co/std/Bad.cheby", checkout("co", "co/std/Bad.cheby")),
            (
                "co/std/unshipped_probe.cheby",
                checkout("co", "co/std/unshipped_probe.cheby"),
            ),
            // No Cheby source: no session (D-1446).
            ("co/std/notes.md", DocRoute::Unowned),
            // `std` matches as written (D-1707).
            ("co/Std/list.cheby", strict("co/Std/list.cheby")),
            // The rest of the repository is not the checkout's.
            (
                "co/docs/examples/x/src/main.cheby",
                DocRoute::Package(at("co/docs/examples/x")),
            ),
            ("co/tests/ui/x.cheby", strict("co/tests/ui/x.cheby")),
            // Two `std` components: the nearest checkout wins, and one with
            // no marker is passed over.
            (
                "co/std/inner/std/list.cheby",
                checkout("co/std/inner", "co/std/inner/std/list.cheby"),
            ),
            (
                "co/std/plain/std/list.cheby",
                checkout("co", "co/std/plain/std/list.cheby"),
            ),
            // No marker: single-file mode.
            ("bare/std/list.cheby", strict("bare/std/list.cheby")),
            ("pkg/src/std/list.cheby", DocRoute::Package(at("pkg"))),
            // A `cheby.toml` above the file: the package's rules, a
            // single file in a package (D-1699).
            (
                "pkg/std/list.cheby",
                DocRoute::SingleFile {
                    file: at("pkg/std/list.cheby"),
                    strict: false,
                },
            ),
        ];
        check_routes(&tree, &[], table);
    }

    /// Routes each file of `table` in `tree` and compares the answer.
    fn check_routes<const N: usize>(
        tree: &Tree,
        std_dirs: &[PathBuf],
        table: [(&str, DocRoute); N],
    ) {
        let at = |file: &str| tree.0.join(file);
        for (file, want) in table {
            let got = route(&at(file), std_dirs);
            // A single-file session is keyed by the file itself, and a
            // checkout's by the checkout.
            match &got {
                DocRoute::SingleFile { .. } => {
                    assert_eq!(got.root(), Some(at(file).as_path()), "{file}");
                }
                DocRoute::StdCheckout { checkout, .. } => {
                    assert_eq!(got.root(), Some(checkout.as_path()), "{file}");
                    assert_eq!(
                        got.kind(),
                        Some(PackageKind::StdCheckout {
                            checkout: checkout.clone()
                        }),
                        "{file}"
                    );
                }
                DocRoute::Std | DocRoute::Package(_) | DocRoute::Unowned => {}
            }
            assert_eq!(got, want, "{file}");
        }
    }

    /// The directories above a `std` component, nearest first, read
    /// lexically, on paths as `normalize` gives them (D-1707). Drive paths
    /// written with `/` parse on every platform, a drive letter included; a
    /// verbatim prefix parses only on Windows, where it must give what its
    /// plain spelling gives, and elsewhere both are one component.
    #[test]
    fn checkout_dirs_are_lexical() {
        let dirs = |text: &str| checkout_dirs(&normalize(Path::new(text)));
        let one = |text: &str| vec![normalize(Path::new(text))];
        assert_eq!(dirs("/co/std/list.cheby"), one("/co"));
        assert_eq!(dirs("/co/std/json/tree.cheby"), one("/co"));
        assert_eq!(
            dirs("/a/std/b/std/list.cheby"),
            [normalize(Path::new("/a/std/b")), normalize(Path::new("/a"))]
        );
        assert_eq!(dirs("/co/Std/list.cheby"), Vec::<PathBuf>::new());
        assert_eq!(dirs("/co/std"), Vec::<PathBuf>::new());
        assert_eq!(dirs("/co/x/../std/list.cheby"), one("/co"));
        // Windows drive paths.
        assert_eq!(dirs("C:/co/std/list.cheby"), one("C:/co"));
        assert_eq!(dirs("c:/co/std/json/tree.cheby"), one("c:/co"));
        assert_eq!(
            dirs(r"\\?\C:\co\std\list.cheby"),
            dirs(r"C:\co\std\list.cheby")
        );
        assert_eq!(
            dirs(r"\\?\c:\co\std\list.cheby"),
            dirs(r"C:\co\std\list.cheby")
        );
        if cfg!(windows) {
            assert_eq!(dirs(r"\\?\c:\co\std\list.cheby"), one(r"C:\co"));
        }
    }

    /// D-1699's unit test: the marker file is where the server looks, so a
    /// move of `std_lib.rs` fails here.
    #[test]
    fn marker_is_where_the_server_looks() {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        assert!(
            marker(&repository).is_file(),
            "{}",
            marker(&repository).display()
        );
        assert!(is_checkout(&normalize(
            &std::path::absolute(&repository).unwrap()
        )));
    }

    /// A copy of this repository's `std/` is the binary's std; a changed
    /// file, an added module and a removed one each make it differ
    /// (D-1705).
    #[test]
    fn std_differs_by_hash_and_module_set() {
        let dir = std::env::temp_dir().join(format!("cheby_lsp_differs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let std_dir = dir.join(STD_DIR);
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        copy_tree(&repository.join(STD_DIR), &std_dir);
        assert!(!std_differs(&dir));
        // A changed file.
        let list = std_dir.join("list.cheby");
        let text = std::fs::read(&list).unwrap();
        std::fs::write(&list, [text.as_slice(), b"\n"].concat()).unwrap();
        assert!(std_differs(&dir));
        std::fs::write(&list, &text).unwrap();
        assert!(!std_differs(&dir));
        // An added module, nested.
        let extra = std_dir.join("json").join("extra.cheby");
        std::fs::write(&extra, "//! Extra.\n").unwrap();
        assert!(std_differs(&dir));
        std::fs::remove_file(&extra).unwrap();
        // A file that is no module, as `build.rs` skips it, changes nothing.
        std::fs::write(std_dir.join(".hidden.cheby"), "x").unwrap();
        std::fs::write(std_dir.join("notes.md"), "x").unwrap();
        assert!(!std_differs(&dir));
        // A removed module.
        std::fs::remove_file(std_dir.join("bool.cheby")).unwrap();
        assert!(std_differs(&dir));
        // No `std/` at all.
        std::fs::remove_dir_all(&std_dir).unwrap();
        assert!(std_differs(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Copies the files under `from` to `to`.
    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let path = entry.unwrap().path();
            let target = to.join(path.file_name().unwrap());
            if path.is_dir() {
                copy_tree(&path, &target);
            } else {
                std::fs::copy(&path, &target).unwrap();
            }
        }
    }

    #[test]
    fn std_dirs_name_this_build() {
        let build = BuildId([7; 16]);
        let dirs = std_dirs(build);
        assert_ne!(dirs, Vec::<PathBuf>::new());
        for dir in &dirs {
            assert!(dir.is_absolute(), "{}", dir.display());
            let name = dir.file_name().and_then(|n| n.to_str()).unwrap();
            assert!(name.contains(&build.to_hex()), "{}", dir.display());
        }
    }
}
