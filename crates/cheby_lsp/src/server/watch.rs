//! Disk changes: file watchers, and sweeps where there are none (A §8,
//! D-1374).
//!
//! At `initialized`, a client that registers watchers dynamically gets one
//! for `**/*.cheby` and `**/cheby.toml`, which covers every package inside
//! a workspace folder. A package outside them, such as the one above a
//! file Zed opened outside its worktree, gets a watcher of its own with
//! relative patterns when the client supports them (A §8.1). A std
//! checkout's session outside them registers `std/**/*.cheby`, and
//! `std/**/cheby.toml` and the root `cheby.toml`, whose coming ends the
//! checkout (D-1704, D-1729). Each reported path goes to every session
//! whose root holds it, so a nested package's file reaches the outer
//! package that also has it as a module, and starts a debounce. A checkout's session, rooted at the
//! whole repository, holds only the paths under its `std/`, so a change to
//! an example or a test starts no run of it, and a checkout nested in
//! another reaches only the session whose `std/` holds it (D-1704). A
//! `cheby.toml` that came or went routes the open documents again.
//!
//! A package with no watcher is swept every 2 s while it has an open
//! document: the `cheby-sweep` thread stats its manifest and the files
//! under `src/`, or for a checkout its root manifest and the files under
//! `std/`, reading none, and reports the paths whose modified time or
//! length changed, came or went. Open, close and save sweep as well (`docs`), with or without a
//! watcher.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use cheby_driver::analysis::PackageKind;
use crossbeam_channel::{Receiver, Sender, select, tick};
use lsp_types::{DidChangeWatchedFilesParams, FileChangeType};
use serde_json::{Value, json};

use super::effects::Effect;
use super::schedule::Urgency;
use super::sessions::STD_DIR;
use super::state::Watched;
use super::{MANIFEST, Server};
use crate::analysis_thread::Changed;
use crate::text::uri::{normalize, path_to_uri, uri_to_path};

/// How often a package with no watcher is swept (D-1374).
const SWEEP_EVERY: Duration = Duration::from_secs(2);

/// The deepest `src/` nesting a sweep walks, as package discovery does.
const MAX_DEPTH: usize = 64;

/// `WatchKind` 7: create, change and delete.
const ALL_KINDS: u8 = 7;

/// The id of the workspace folders' watcher.
const FOLDERS_ID: &str = "cheby-files";

/// The client's support for watchers, from `initialize`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WatchCaps {
    /// `workspace.didChangeWatchedFiles.dynamicRegistration`.
    pub dynamic: bool,
    /// `workspace.didChangeWatchedFiles.relativePatternSupport`.
    pub relative: bool,
}

impl WatchCaps {
    /// Reads the capabilities of `params`, the `initialize` parameters,
    /// leniently: a missing or odd value is `false`.
    pub(crate) fn from_params(params: &Value) -> Self {
        let flag = |name: &str| {
            params
                .pointer(&format!(
                    "/capabilities/workspace/didChangeWatchedFiles/{name}"
                ))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        Self {
            dynamic: flag("dynamicRegistration"),
            relative: flag("relativePatternSupport"),
        }
    }
}

/// The server's watchers and its sweep thread.
#[derive(Debug)]
pub(crate) struct Watch {
    caps: WatchCaps,
    /// The workspace folders' watcher is registered.
    folders: bool,
    /// The number of the next watcher of one package.
    next: u64,
    sweeper: Sweeper,
}

impl Watch {
    /// The watchers of a client with these `initialize` parameters, and a
    /// sweep thread with nothing to sweep yet.
    ///
    /// # Errors
    ///
    /// The error of a thread that cannot be started.
    pub(crate) fn new(params: &Value) -> std::io::Result<Self> {
        Ok(Self {
            caps: WatchCaps::from_params(params),
            folders: false,
            next: 1,
            sweeper: Sweeper::spawn()?,
        })
    }

    /// The sweep thread's findings, for the main loop.
    pub(crate) fn swept(&self) -> Receiver<Swept> {
        self.sweeper.found.clone()
    }
}

/// Paths of one package that a sweep found changed.
#[derive(Debug)]
pub(crate) struct Swept {
    /// The package root.
    pub root: PathBuf,
    /// Each path whose stamp changed, or that came or went.
    pub paths: Vec<PathBuf>,
}

impl Server {
    /// Registers the workspace folders' watcher, when the client can
    /// (A §8.1).
    pub(crate) fn watch_folders(&mut self) {
        if !self.watch.caps.dynamic {
            return;
        }
        self.watch.folders = true;
        let watchers = json!([
            { "globPattern": "**/*.cheby", "kind": ALL_KINDS },
            { "globPattern": "**/cheby.toml", "kind": ALL_KINDS },
        ]);
        self.register_watchers(FOLDERS_ID, &watchers);
    }

    /// How the package at `root`, or the std checkout there when
    /// `checkout`, will hear of disk changes: the folders' watcher when it
    /// lies in a workspace folder, else a watcher of its own with relative
    /// patterns, for a checkout `std/**/*.cheby` and the manifests that
    /// would end it, `std/**/cheby.toml` and the root `cheby.toml`
    /// (D-1704, D-1729), else sweeps.
    pub(crate) fn watch_package(&mut self, root: &Path, checkout: bool) -> Watched {
        if self.watch.folders && self.folders.iter().any(|folder| root.starts_with(folder)) {
            return Watched::Folders;
        }
        if !(self.watch.caps.dynamic && self.watch.caps.relative) {
            return Watched::Sweep;
        }
        let Ok(base) = path_to_uri(root) else {
            return Watched::Sweep;
        };
        let id = format!("{FOLDERS_ID}-{}", self.watch.next);
        self.watch.next = self.watch.next.saturating_add(1);
        let pattern = |pattern: &str| {
            json!({
                "globPattern": { "baseUri": base.as_str(), "pattern": pattern },
                "kind": ALL_KINDS,
            })
        };
        let watchers = if checkout {
            json!([
                pattern(&format!("{STD_DIR}/**/*.cheby")),
                pattern(&format!("{STD_DIR}/**/{MANIFEST}")),
                pattern(MANIFEST),
            ])
        } else {
            json!([pattern("**/*.cheby"), pattern("**/cheby.toml")])
        };
        self.register_watchers(&id, &watchers);
        Watched::Own(id)
    }

    /// Sends `client/registerCapability` for `workspace/didChangeWatchedFiles`.
    fn register_watchers(&mut self, id: &str, watchers: &Value) {
        self.apply(Effect::Request {
            method: "client/registerCapability",
            params: json!({
                "registrations": [{
                    "id": id,
                    "method": "workspace/didChangeWatchedFiles",
                    "registerOptions": { "watchers": watchers },
                }],
            }),
        });
    }

    /// Unregisters the watcher `id` of a session that ended.
    pub(crate) fn unwatch(&mut self, id: &str) {
        self.apply(Effect::Request {
            method: "client/unregisterCapability",
            params: json!({
                // The protocol's own spelling.
                "unregisterations": [{
                    "id": id,
                    "method": "workspace/didChangeWatchedFiles",
                }],
            }),
        });
    }

    /// `workspace/didChangeWatchedFiles` (A §8.2): each path is forgotten
    /// by every session whose root holds it, which then runs after the
    /// debounce. A change to an open document only refreshes the disk
    /// copy, since its text wins. A `cheby.toml` that came or went routes
    /// the open documents again.
    pub(crate) fn did_change_watched_files(&mut self, params: Value) {
        let Some(params) =
            self.params::<DidChangeWatchedFilesParams>("didChangeWatchedFiles", params)
        else {
            return;
        };
        let mut manifests = false;
        let mut changed: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
        for event in params.changes {
            let Some(path) = uri_to_path(&event.uri).map(|path| normalize(&path)) else {
                continue;
            };
            let manifest = path.file_name().is_some_and(|name| name == MANIFEST);
            manifests |= manifest && event.typ != FileChangeType::CHANGED;
            for (root, path) in self.holders(&path) {
                changed.entry(root).or_default().push(path);
            }
        }
        for (root, paths) in changed {
            let changed = Changed {
                sweep: false,
                paths: paths.clone(),
            };
            self.mark_dirty(&root, changed, Urgency::Debounce);
            self.child_on_save(&root, &paths);
        }
        if manifests {
            self.reroute();
        }
    }

    /// Each session root that holds `path`, with `path` as that session
    /// names it: a watcher may report a resolved path, such as
    /// `/private/tmp/…` for `/tmp/…` on macOS (A §6.1).
    fn holders(&self, path: &Path) -> Vec<(PathBuf, PathBuf)> {
        self.packages
            .values()
            .filter_map(|package| {
                let checkout = matches!(package.kind, PackageKind::StdCheckout { .. });
                let named = held(&package.root, package.real.as_deref(), checkout, path)?;
                Some((package.root.clone(), named))
            })
            .collect()
    }

    /// A sweep found changes under `root`: its session forgets them and
    /// runs after the debounce, and the constants child treats them as a
    /// watched change, since the sweep stands in for watchers (D-1374,
    /// D-1494). A manifest that came or went routes the open documents
    /// again. A checkout's root `cheby.toml` is swept for that alone: it is
    /// no file of the session's, as with a watcher (D-1704, D-1729).
    pub(crate) fn on_swept(&mut self, swept: Swept) {
        let Swept { root, paths } = swept;
        let Some(package) = self.packages.get(&root) else {
            return;
        };
        let checkout = matches!(package.kind, PackageKind::StdCheckout { .. });
        let manifests = paths
            .iter()
            .any(|path| path.file_name().is_some_and(|name| name == MANIFEST));
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|path| held(&root, None, checkout, path).is_some())
            .collect();
        if !paths.is_empty() {
            let changed = Changed {
                sweep: false,
                paths: paths.clone(),
            };
            self.mark_dirty(&root, changed, Urgency::Debounce);
            self.child_on_save(&root, &paths);
        }
        if manifests {
            self.reroute();
        }
    }

    /// Tells the sweep thread which packages to sweep: those with no
    /// watcher and an open document (A §8.3).
    pub(crate) fn sync_sweeps(&mut self) {
        let roots: Vec<SweepRoot> = self
            .packages
            .values()
            .filter(|package| package.watch == Watched::Sweep)
            .filter(|package| {
                self.docs
                    .values()
                    .any(|doc| doc.root.as_deref() == Some(package.root.as_path()))
            })
            .map(|package| SweepRoot {
                root: package.root.clone(),
                checkout: matches!(package.kind, PackageKind::StdCheckout { .. }),
            })
            .collect();
        self.watch.sweeper.set(roots);
    }
}

/// `path` as the session rooted at `root` names it, when that session holds
/// it: under `root`, or under `real`, the root with its links resolved,
/// and for a std checkout (`checkout`) under its `std/` only (D-1704).
fn held(root: &Path, real: Option<&Path>, checkout: bool, path: &Path) -> Option<PathBuf> {
    let (rest, named) = if let Ok(rest) = path.strip_prefix(root) {
        (rest, path.to_path_buf())
    } else {
        let rest = path.strip_prefix(real?).ok()?;
        (rest, root.join(rest))
    };
    (!checkout || rest.starts_with(STD_DIR)).then_some(named)
}

/// One session the sweep thread sweeps.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SweepRoot {
    /// The session's root.
    root: PathBuf,
    /// A std checkout, whose files under `std/` are swept (D-1704).
    checkout: bool,
}

/// The sweep thread's handles.
#[derive(Debug)]
struct Sweeper {
    /// The roots to sweep, sent whenever they change.
    roots: Sender<Vec<SweepRoot>>,
    /// What the thread found.
    found: Receiver<Swept>,
    /// The roots last sent.
    sent: Vec<SweepRoot>,
}

impl Sweeper {
    /// Starts the `cheby-sweep` thread. It ends when the sweeper is
    /// dropped.
    fn spawn() -> std::io::Result<Self> {
        let (roots_tx, roots_rx) = crossbeam_channel::unbounded::<Vec<SweepRoot>>();
        let (found_tx, found_rx) = crossbeam_channel::unbounded::<Swept>();
        std::thread::Builder::new()
            .name("cheby-sweep".to_owned())
            .spawn(move || sweep_loop(&roots_rx, &found_tx))?;
        Ok(Self {
            roots: roots_tx,
            found: found_rx,
            sent: Vec::new(),
        })
    }

    /// Sweeps `roots` from now on.
    fn set(&mut self, roots: Vec<SweepRoot>) {
        if roots != self.sent {
            // A thread that is gone sweeps nothing; the sweeps at open,
            // close and save still run.
            let _ = self.roots.send(roots.clone());
            self.sent = roots;
        }
    }
}

/// What a sweep compares per file, as the session's disk view does: the
/// modified time and the length.
type Stamps = BTreeMap<PathBuf, (Option<SystemTime>, u64)>;

/// The sweep thread's loop. A root's first stamps are taken when it is
/// added, so a change is one against what the session's run at that open
/// read.
fn sweep_loop(roots: &Receiver<Vec<SweepRoot>>, found: &Sender<Swept>) {
    let mut known: BTreeMap<SweepRoot, Stamps> = BTreeMap::new();
    let ticks = tick(SWEEP_EVERY);
    loop {
        select! {
            recv(roots) -> list => {
                let Ok(list) = list else {
                    return;
                };
                known.retain(|root, _| list.contains(root));
                for root in list {
                    known.entry(root).or_insert_with_key(stamps);
                }
            }
            recv(ticks) -> _ => {
                for (root, old) in &mut known {
                    let new = stamps(root);
                    let paths = differences(old, &new);
                    *old = new;
                    if paths.is_empty() {
                        continue;
                    }
                    let swept = Swept {
                        root: root.root.clone(),
                        paths,
                    };
                    if found.send(swept).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// The stamps of the package at `root`: its manifest, and every module and
/// `cheby.toml` under `src/`, following symbolic links as discovery does,
/// to [`MAX_DEPTH`]. For a std checkout, every `.cheby` file and
/// `cheby.toml` under `std/`, and the root `cheby.toml`, whose coming
/// ends the checkout (D-1704, D-1729).
fn stamps(root: &SweepRoot) -> Stamps {
    let mut found = Stamps::new();
    let manifest = root.root.join(MANIFEST);
    if let Ok(meta) = std::fs::metadata(&manifest) {
        found.insert(manifest, (meta.modified().ok(), meta.len()));
    }
    let files = if root.checkout { STD_DIR } else { "src" };
    walk(&root.root.join(files), 0, &mut found);
    found
}

/// Adds the stamps of the files under `dir` to `found`.
fn walk(dir: &Path, depth: usize, found: &mut Stamps) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            walk(&path, depth.saturating_add(1), found);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "cheby")
            || path.file_name().is_some_and(|name| name == MANIFEST)
        {
            found.insert(normalize(&path), (meta.modified().ok(), meta.len()));
        }
    }
}

/// Every path whose stamp differs between `old` and `new`, or that is in
/// one of them only.
fn differences(old: &Stamps, new: &Stamps) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = new
        .iter()
        .filter(|(path, stamp)| old.get(*path) != Some(*stamp))
        .map(|(path, _)| path.clone())
        .collect();
    paths.extend(old.keys().filter(|path| !new.contains_key(*path)).cloned());
    paths.sort();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_are_read_leniently() {
        let caps = |workspace: Value| {
            WatchCaps::from_params(&json!({ "capabilities": { "workspace": workspace } }))
        };
        assert_eq!(caps(json!({})), WatchCaps::default());
        assert_eq!(
            caps(json!({ "didChangeWatchedFiles": { "dynamicRegistration": true } })),
            WatchCaps {
                dynamic: true,
                relative: false,
            }
        );
        assert_eq!(
            caps(json!({ "didChangeWatchedFiles": {
                "dynamicRegistration": true, "relativePatternSupport": true,
            }})),
            WatchCaps {
                dynamic: true,
                relative: true,
            }
        );
        assert_eq!(
            caps(json!({ "didChangeWatchedFiles": { "dynamicRegistration": "yes" } })),
            WatchCaps::default()
        );
    }

    #[test]
    fn stamps_see_changes_arrivals_and_departures() {
        let dir = std::env::temp_dir().join(format!("cheby_lsp_sweep_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/geo")).unwrap();
        let dir = normalize(&std::path::absolute(&dir).unwrap());
        std::fs::write(dir.join("cheby.toml"), "name = \"app\"\n").unwrap();
        std::fs::write(dir.join("src/main.cheby"), "a").unwrap();
        std::fs::write(dir.join("src/geo/shapes.cheby"), "b").unwrap();
        std::fs::write(dir.join("src/notes.txt"), "c").unwrap();
        std::fs::write(dir.join("README.cheby"), "d").unwrap();
        let package = SweepRoot {
            root: dir.clone(),
            checkout: false,
        };
        let before = stamps(&package);
        let names: Vec<&Path> = before.keys().map(PathBuf::as_path).collect();
        assert_eq!(
            names,
            [
                dir.join("cheby.toml"),
                dir.join("src/geo/shapes.cheby"),
                dir.join("src/main.cheby"),
            ]
        );
        assert_eq!(
            differences(&before, &stamps(&package)),
            Vec::<PathBuf>::new()
        );
        // A longer text, a new module and a deleted one.
        std::fs::write(dir.join("src/main.cheby"), "aa").unwrap();
        std::fs::write(dir.join("src/new.cheby"), "e").unwrap();
        std::fs::remove_file(dir.join("src/geo/shapes.cheby")).unwrap();
        assert_eq!(
            differences(&before, &stamps(&package)),
            [
                dir.join("src/geo/shapes.cheby"),
                dir.join("src/main.cheby"),
                dir.join("src/new.cheby"),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A checkout's sweep stats the files under `std/` and the root
    /// manifest only: no `src/`, and no other directory of the repository
    /// (D-1704, D-1729).
    #[test]
    fn checkout_stamps_cover_std_and_the_root_manifest() {
        let dir = std::env::temp_dir().join(format!("cheby_lsp_sweep_co_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dir = normalize(&std::path::absolute(&dir).unwrap());
        let files = [
            ["cheby.toml"].as_slice(),
            &["src", "main.cheby"],
            &["docs", "examples", "x", "src", "main.cheby"],
            &["std", "list.cheby"],
            &["std", "json", "tree.cheby"],
            &["std", "notes.txt"],
        ];
        let at = |parts: &[&str]| parts.iter().fold(dir.clone(), |path, part| path.join(part));
        for parts in files {
            let path = at(parts);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "a").unwrap();
        }
        let checkout = SweepRoot {
            root: dir.clone(),
            checkout: true,
        };
        let before = stamps(&checkout);
        let names: Vec<&Path> = before.keys().map(PathBuf::as_path).collect();
        assert_eq!(
            names,
            [
                at(&["cheby.toml"]),
                at(&["std", "json", "tree.cheby"]),
                at(&["std", "list.cheby"])
            ]
        );
        // A change outside `std/` is no change of the checkout's.
        std::fs::write(at(&["docs", "examples", "x", "src", "main.cheby"]), "bb").unwrap();
        std::fs::write(at(&["src", "main.cheby"]), "bb").unwrap();
        assert_eq!(
            differences(&before, &stamps(&checkout)),
            Vec::<PathBuf>::new()
        );
        std::fs::write(at(&["std", "list.cheby"]), "bb").unwrap();
        assert_eq!(
            differences(&before, &stamps(&checkout)),
            [at(&["std", "list.cheby"])]
        );
        std::fs::remove_file(at(&["cheby.toml"])).unwrap();
        assert_eq!(
            differences(&before, &stamps(&checkout)),
            [at(&["cheby.toml"]), at(&["std", "list.cheby"])]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A checkout's session holds the paths under its `std/` only, by its
    /// root or its resolved root, while a package's holds every path under
    /// its root (D-1704).
    #[test]
    fn held_keeps_a_checkout_to_its_std() {
        let at = |text: &str| {
            text.split('/')
                .fold(PathBuf::from("/"), |path, part| path.join(part))
        };
        let root = at("co");
        let real = at("private/co");
        let rows = [
            // (path, held by a package at the root, held by a checkout there)
            (
                "co/std/list.cheby",
                Some("co/std/list.cheby"),
                Some("co/std/list.cheby"),
            ),
            (
                "co/std/json/tree.cheby",
                Some("co/std/json/tree.cheby"),
                Some("co/std/json/tree.cheby"),
            ),
            (
                "co/docs/examples/x/src/main.cheby",
                Some("co/docs/examples/x/src/main.cheby"),
                None,
            ),
            ("co/stdx/a.cheby", Some("co/stdx/a.cheby"), None),
            ("co/cheby.toml", Some("co/cheby.toml"), None),
            // A nested checkout's `std/` is not the outer one's.
            (
                "co/nested/std/list.cheby",
                Some("co/nested/std/list.cheby"),
                None,
            ),
            // A resolved path is named under the root.
            (
                "private/co/std/list.cheby",
                Some("co/std/list.cheby"),
                Some("co/std/list.cheby"),
            ),
            ("private/co/tests/x.cheby", Some("co/tests/x.cheby"), None),
            ("elsewhere/std/list.cheby", None, None),
        ];
        for (path, package, checkout) in rows {
            let path = at(path);
            assert_eq!(
                held(&root, Some(&real), false, &path),
                package.map(at),
                "{}",
                path.display()
            );
            assert_eq!(
                held(&root, Some(&real), true, &path),
                checkout.map(at),
                "{}",
                path.display()
            );
        }
    }
}
