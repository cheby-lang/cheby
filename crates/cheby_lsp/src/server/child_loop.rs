//! The main loop's side of children (`child`, `progress`, D-1349, D-1411,
//! D-1415): the `Server` methods that feed `Children` what it reads of
//! the server and apply what it answers.
//!
//! - The loop's `select!` reads `Children::done` into
//!   [`Server::on_child_done`], and its timer includes
//!   `Children::deadline` until `shutdown`.
//! - Each turn ends with [`Server::schedule_children`], then
//!   [`Server::send_idle`].
//! - [`Server::child_on_live`] follows each Q run, and
//!   [`Server::child_on_report`] each published report, with what the
//!   server knows of it: the session's kind, whether a local module
//!   declares a constant, and whether a file that keeps it unclean is
//!   saved. [`Server::child_on_save`] follows `didSave` and a watched
//!   change on disk, [`Server::child_on_settings`] each settings refresh,
//!   a change of `analysis.target` included (D-1498),
//!   [`Server::end_children`] a session's end, and
//!   [`Server::kill_children`] `shutdown` and the loop's end.
//! - `publish` appends `Children::diagnostics` after a file's own list,
//!   as `report_back_end` appends them in `cheby check`, and
//!   `republish_child` publishes report files again when only the child's
//!   lists changed. Hover and the constant inlay row read
//!   `Children::constants` through `RequestCx`, and `cheby/status` is
//!   [`Server::on_status`].
//! - The constant inlay row shows only values that read current
//!   (D-1679), so the client is asked for its hints again
//!   (`workspace/inlayHint/refresh`) when the set of current values of a
//!   package changes: when a run's values arrive or its marks change,
//!   which [`Server::apply_outbox`] and [`Server::child_on_report`] see
//!   as a new `ConstValues`, and when an edit, a close, a save or a new
//!   snapshot changes the text a value is read against
//!   ([`Server::recheck_constant_hints`]). Only with the row on and a
//!   client that takes the request, and at most once per change.

use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cheby_driver::analysis::{
    DefKind, Hash128, Live, PackageKind, Report, Severity, Snapshot, Symbol, normalized_key,
};
use lsp_server::ResponseError;
use lsp_types::Uri;
use serde_json::{Value, json};

use super::child::{ChildDone, ChildEnv, ConstValues, EditorText, Outbox, ReportFacts};
use super::effects::Effect;
use super::progress::IdleParams;
use super::schedule::RunState;
use super::state::Doc;
use super::{ClientCaps, Outgoing, Server, Settings, codes};
use crate::ServerOptions;
use crate::text::uri::{normalize, path_to_uri, uri_to_path};

/// The [`ChildEnv`] of the server's fields, borrowed apart from
/// `children` so that both can be used at once.
fn child_env<'a>(
    options: &'a ServerOptions,
    settings: &Settings,
    caps: &ClientCaps,
    analyzing: &'a BTreeSet<PathBuf>,
) -> ChildEnv<'a> {
    ChildEnv {
        exe: options.child.as_deref(),
        evaluate: settings.constants.evaluate,
        timeout: Duration::from_secs(settings.constants.child_timeout_seconds),
        target: settings.analysis.target,
        progress: caps.work_done_progress,
        analyzing,
    }
}

/// Whether a local module of `snapshot` declares a constant (A §11.4).
fn has_constants(snapshot: &Snapshot) -> bool {
    snapshot.local_modules().any(|module| {
        module
            .interface()
            .is_some_and(|iface| iface.items.iter().any(|item| item.kind == DefKind::Const))
    })
}

/// What the constant inlay row of one package was last computed from
/// (D-1679): the values, held so that any later change to them makes a
/// new `Arc`, and those of them that read current then.
#[derive(Debug)]
pub(crate) struct ShownConstants {
    /// The package's values when last looked at.
    values: Arc<ConstValues>,
    /// The values that read current then, with their texts and cuts.
    current: BTreeMap<Symbol, (String, bool)>,
}

/// What to look at again for the constant inlay row.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Recheck<'a> {
    /// Every package whose values changed since they were last looked at.
    Values,
    /// The package at this root, whose text in the editor may have
    /// changed: an edit, a close, a save or a new snapshot.
    Text(&'a Path),
}

/// The package's text as the editor holds it now (D-1510): an open
/// document's live text, else the newest snapshot's, which read the
/// file from disk. Each hash is computed on first need, so an edit that
/// no value depends on hashes nothing else.
struct LiveEditor<'a> {
    snapshot: Option<&'a Snapshot>,
    docs: &'a BTreeMap<PathBuf, Doc>,
    modules: OnceCell<BTreeSet<PathBuf>>,
    hashes: RefCell<BTreeMap<PathBuf, Option<Hash128>>>,
}

impl<'a> LiveEditor<'a> {
    fn new(snapshot: Option<&'a Snapshot>, docs: &'a BTreeMap<PathBuf, Doc>) -> Self {
        Self {
            snapshot,
            docs,
            modules: OnceCell::new(),
            hashes: RefCell::new(BTreeMap::new()),
        }
    }

    fn module_paths(&self) -> &BTreeSet<PathBuf> {
        self.modules.get_or_init(|| {
            self.snapshot
                .map_or_else(BTreeSet::new, EditorText::modules)
        })
    }
}

impl EditorText for LiveEditor<'_> {
    fn hash(&self, path: &Path) -> Option<Hash128> {
        if let Some(&hash) = self.hashes.borrow().get(path) {
            return hash;
        }
        let hash = if self.module_paths().contains(path) {
            match self.docs.get(path) {
                Some(doc) => Some(normalized_key(&doc.text)),
                None => self.snapshot.and_then(|snapshot| snapshot.hash(path)),
            }
        } else {
            None
        };
        self.hashes.borrow_mut().insert(path.to_path_buf(), hash);
        hash
    }

    fn modules(&self) -> BTreeSet<PathBuf> {
        self.module_paths().clone()
    }
}

impl Server {
    /// Whether the constant inlay row is on and the client takes
    /// `workspace/inlayHint/refresh` (D-1679).
    fn constant_hints_tracked(&self) -> bool {
        let inlays = &self.settings.inlay_hints;
        self.caps.inlay_refresh && inlays.enable && inlays.constants
    }

    /// Looks again at the packages `which` names, and asks the client for
    /// its hints again, once, when the set of current values of one of
    /// them changed since it was last looked at (D-1679).
    pub(crate) fn recheck_constant_hints(&mut self, which: Recheck<'_>) {
        if self.recompute_constant_hints(which) {
            self.request("workspace/inlayHint/refresh", Value::Null, Outgoing::Ignore);
        }
    }

    /// Computes again what the constant inlay row of the packages `which`
    /// names would show, and keeps it. True when it changed for one of
    /// them. Nothing is kept while the row is off or the client cannot be
    /// asked again.
    pub(crate) fn recompute_constant_hints(&mut self, which: Recheck<'_>) -> bool {
        if !self.constant_hints_tracked() {
            self.shown_constants.clear();
            return false;
        }
        let packages = &self.packages;
        self.shown_constants
            .retain(|root, _| packages.contains_key(root));
        let roots: Vec<PathBuf> = match which {
            Recheck::Values => self.packages.keys().cloned().collect(),
            Recheck::Text(root) => vec![root.to_path_buf()],
        };
        let mut changed = false;
        for root in roots {
            let values = self.children.constants(&root);
            let shown = self.shown_constants.get(&root);
            let same_values = match (shown, &values) {
                (Some(shown), Some(values)) => Arc::ptr_eq(&shown.values, values),
                (None, None) => true,
                (Some(_), None) | (None, Some(_)) => false,
            };
            if same_values && matches!(which, Recheck::Values) {
                continue;
            }
            let current = values.as_deref().map_or_else(BTreeMap::new, |values| {
                let snapshot = self
                    .packages
                    .get(&root)
                    .and_then(|package| package.snapshot.as_deref());
                values.current_values(&LiveEditor::new(snapshot, &self.docs))
            });
            let before = shown.map(|shown| &shown.current);
            changed |= before.map_or(!current.is_empty(), |before| *before != current);
            match values {
                Some(values) => {
                    self.shown_constants
                        .insert(root, ShownConstants { values, current });
                }
                None => {
                    self.shown_constants.remove(&root);
                }
            }
        }
        changed
    }

    /// Whether `path` is an open document edited since it was opened or
    /// saved. A closed file is read from disk.
    fn unsaved(&self, path: &Path) -> bool {
        self.docs.get(path).is_some_and(|doc| doc.dirty)
    }

    /// The packages whose analysis job runs, or is due.
    fn analyzing(&self) -> BTreeSet<PathBuf> {
        self.packages
            .iter()
            .filter(|(root, package)| {
                self.running.as_deref() == Some(root.as_path())
                    || !matches!(package.run, RunState::Idle)
            })
            .map(|(root, _)| root.clone())
            .collect()
    }

    /// Sends what a step of `Children` asked for.
    fn apply_outbox(&mut self, out: Outbox) {
        for effect in out.effects {
            self.apply(effect);
        }
        for (root, paths) in out.republish {
            let files = self.children.report_files(&root, &paths);
            self.republish_child(&root, &files);
        }
        // A run's values, or its marks, may have changed (D-1679).
        self.recheck_constant_hints(Recheck::Values);
    }

    /// A report of the package at `root` was published: keep its files,
    /// and on the session's first clean report, or a clean report after a
    /// save that found it unclean through unsaved buffers only, make a
    /// constants run due (D-1411). `Report::clean` already requires no
    /// broken file (plan §1.2), and the newest snapshot's broken files
    /// count as unclean ones.
    pub(crate) fn child_on_report(&mut self, root: &Path, report: &Report) {
        let package = self.packages.get(root);
        let is_package =
            package.is_some_and(|package| matches!(package.kind, PackageKind::Package));
        let snapshot = package.and_then(|package| package.snapshot.as_ref());
        let has_constants = snapshot.is_none_or(|snapshot| has_constants(snapshot));
        let errors = report
            .files
            .iter()
            .filter(|(_, file)| {
                file.diagnostics
                    .iter()
                    .any(|d| d.severity == Severity::Error)
            })
            .map(|(path, _)| path);
        let broken = snapshot
            .into_iter()
            .flat_map(|snapshot| snapshot.status().broken.iter());
        // The files whose saved text does not check (D-1501).
        let saved_errors: BTreeSet<PathBuf> = if report.clean {
            BTreeSet::new()
        } else {
            errors
                .chain(broken)
                .filter(|path| !self.unsaved(path))
                .cloned()
                .collect()
        };
        let disk_unclean = !report.clean && (!report.tool.is_empty() || !saved_errors.is_empty());
        let facts = ReportFacts {
            is_package,
            has_constants,
            disk_unclean,
            saved_errors,
        };
        let analyzing = BTreeSet::new();
        let env = child_env(&self.options, &self.settings, &self.caps, &analyzing);
        self.children
            .on_report(root, facts, report, &env, Instant::now());
        // Errors on disk mark every value stale (D-1503).
        self.recheck_constant_hints(Recheck::Values);
    }

    /// A Q run of the package at `root` ended: while it has a broken
    /// file, no constants run starts (plan §1.2). Called before
    /// [`Server::child_on_report`] for the same run.
    pub(crate) fn child_on_live(&mut self, root: &Path, live: &Live) {
        if !live.broken.is_empty() {
            let saved: Vec<PathBuf> = live
                .broken
                .iter()
                .filter(|path| !self.unsaved(path))
                .cloned()
                .collect();
            self.children.on_broken(root, &saved);
        }
        // A new snapshot: the text of a closed file may have changed.
        self.recheck_constant_hints(Recheck::Text(root));
    }

    /// The settings were refreshed: apply `constants.*` to the children,
    /// rerun the constants when `retarget`, a change of `analysis.target`
    /// (D-1498), and owe every package a `cheby/idle`, so a client that
    /// waits for one after a settings change gets it even when nothing
    /// starts (D-1443).
    pub(crate) fn child_on_settings(&mut self, retarget: bool) {
        if self.shutting_down {
            return;
        }
        let analyzing = self.analyzing();
        let env = child_env(&self.options, &self.settings, &self.caps, &analyzing);
        let mut out = Outbox::default();
        let now = Instant::now();
        self.children.on_settings(&env, now, &mut out);
        if retarget {
            self.children.on_target(&env, now, &mut out);
        }
        let roots: Vec<PathBuf> = self.packages.keys().cloned().collect();
        for root in &roots {
            self.children.owe_idle(root);
        }
        self.apply_outbox(out);
    }

    /// `didSave`, or a watched change on disk, of `saved` in the package
    /// at `root`: when the package's text on disk changed, kill its
    /// constants child and make a run due after `COALESCE` (D-1411,
    /// D-1494). It also owes a `cheby/idle`, even if nothing runs.
    pub(crate) fn child_on_save(&mut self, root: &Path, saved: &[PathBuf]) {
        let analyzing = BTreeSet::new();
        let env = child_env(&self.options, &self.settings, &self.caps, &analyzing);
        let mut out = Outbox::default();
        self.children
            .on_save(root, saved, &env, Instant::now(), &mut out);
        self.apply_outbox(out);
    }

    /// Starts what is due, at most one constants child in the process, and
    /// kills what passed its time limit (A §11.4, §11.5).
    pub(crate) fn schedule_children(&mut self) {
        if self.shutting_down {
            return;
        }
        let analyzing = self.analyzing();
        let env = child_env(&self.options, &self.settings, &self.caps, &analyzing);
        let mut out = Outbox::default();
        self.children.schedule(&env, Instant::now(), &mut out);
        self.apply_outbox(out);
    }

    /// A child's output ended: keep its results, republish the files whose
    /// child list changed, and end its progress. Output of a run that was
    /// killed is dropped (A §11.6).
    pub(crate) fn on_child_done(&mut self, done: ChildDone) {
        let mut out = Outbox::default();
        self.children.on_done(done, Instant::now(), &mut out);
        self.apply_outbox(out);
    }

    /// Kills every child: `shutdown`, and the loop's end (A §11.5).
    pub(crate) fn kill_children(&mut self) {
        self.children.kill_all();
    }

    /// The session at `root` ended: kill its children, end their
    /// progress, and forget their results.
    pub(crate) fn end_children(&mut self, root: &Path) {
        let mut out = Outbox::default();
        self.children.end(root, &mut out);
        self.apply_outbox(out);
    }

    /// The `generation` of `root` for `cheby/idle` and `cheby/status`:
    /// how many snapshots the root has had since the server started, 0
    /// for none, which never goes back, not even when its session is
    /// rebuilt (D-1495) or ends and starts again (D-1499).
    fn generation(&self, root: &Path) -> u64 {
        self.generations.get(root).copied().unwrap_or(0)
    }

    /// Sends `cheby/idle` for each package that became idle since its last
    /// one: no analysis job running or debouncing, no request parked on
    /// it, no child running or due. Only with `experimental.chebyIdle`
    /// (D-1415).
    pub(crate) fn send_idle(&mut self) {
        if !self.caps.idle_notification {
            return;
        }
        let parked: BTreeSet<&Path> = self
            .parked
            .iter()
            .map(|parked| parked.root.as_path())
            .collect();
        let analyzing = self.analyzing();
        let states: Vec<(PathBuf, bool)> = self
            .packages
            .keys()
            .map(|root| {
                let busy = analyzing.contains(root) || parked.contains(root.as_path());
                (root.clone(), busy)
            })
            .collect();
        for root in self.children.idle_turn(states) {
            let Ok(uri) = path_to_uri(&root) else {
                continue;
            };
            let generation = self.generation(&root);
            let params = IdleParams {
                root: uri.as_str().to_owned(),
                generation,
            };
            self.apply(Effect::Notify {
                method: "cheby/idle",
                params: serde_json::to_value(params).unwrap_or_default(),
            });
        }
    }

    /// `cheby/status` (`progress`'s module docs). `params` may name a
    /// `root` URI, else every package is listed.
    ///
    /// # Errors
    ///
    /// `InvalidParams` for parameters of another shape.
    pub(crate) fn on_status(&mut self, params: Value) -> Result<Value, ResponseError> {
        let invalid = |message: &str| ResponseError {
            code: codes::INVALID_PARAMS,
            message: format!("cheby/status: {message}"),
            data: None,
        };
        let only = match params {
            Value::Null => None,
            Value::Object(map) => match map.get("root") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) => {
                    let uri: Uri = text.parse().map_err(|_| invalid("`root` is not a URI"))?;
                    let path = uri_to_path(&uri).ok_or_else(|| invalid("`root` is not a file"))?;
                    Some(normalize(&path))
                }
                Some(_) => return Err(invalid("`root` is not a string")),
            },
            Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) => {
                return Err(invalid("expected an object or null"));
            }
        };
        let now = Instant::now();
        let analyzing = self.analyzing();
        let env = child_env(&self.options, &self.settings, &self.caps, &analyzing);
        let packages: Vec<Value> = self
            .packages
            .iter()
            .filter(|(root, _)| only.as_ref().is_none_or(|only| only == *root))
            .map(|(root, package)| {
                let snapshot = package.snapshot.as_ref();
                json!({
                    "root": path_to_uri(root).map(|uri| uri.as_str().to_owned()).ok(),
                    "kind": match package.kind {
                        PackageKind::Package => "package",
                        PackageKind::SingleFile { .. } => "singleFile",
                    },
                    "generation": self.generation(root),
                    "broken": snapshot.map_or_else(Vec::new, |s| {
                        s.status()
                            .broken
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect()
                    }),
                    "run": match package.run {
                        RunState::Idle => "idle",
                        RunState::Debouncing { .. } => "debouncing",
                        RunState::Running { .. } => "running",
                    },
                    "parked": self.parked.iter().filter(|p| &p.root == root).count(),
                    "children": self.children.status(
                        root,
                        matches!(package.kind, PackageKind::Package),
                        snapshot.map(|snapshot| -> &dyn EditorText { &**snapshot }),
                        &env,
                        now,
                    ),
                })
            })
            .collect();
        Ok(json!({
            "platform": std::env::consts::OS,
            "idleNotification": self.caps.idle_notification,
            "workDoneProgress": self.caps.work_done_progress,
            "packages": packages,
        }))
    }
}
