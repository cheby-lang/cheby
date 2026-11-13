//! `backend_query`: back-end facts for the language server's child process
//! (A §11.2, plan §1.8, D-1349, D-1410).
//!
//! The server never runs generated code (D-1349). It starts `cheby
//! __backend`, whose `cheby_lsp::child::main` calls [`backend_query`] once
//! and prints the result as one JSON document (`cheby_lsp::child::wire`).
//! One process runs one job (D-1410): [`BackendJob::Constants`] in W9, and
//! `Ownership`, which the W15 wave adds as a variant with its own section.
//!
//! The steps of the constants job:
//!
//! 1. The front end over the saved files at [`BackendRequest::root`], as
//!    `cheby check` runs it: `Mode::Check` and `CommandFamily::Check`, no
//!    build cache (D-1363), so no C compiles and no library opens
//!    (D-1410). The package's `[const-eval]` limits apply unchanged, so
//!    verdicts match `cheby check` (D-536, A §11.5).
//! 2. Any front-end error: [`FrontOutcome::Errors`], and the section is
//!    [`Section::Skipped`]. Constants are evaluated only when the front end
//!    has no error, as in `check_inner` (D-459, D-534).
//! 3. `backend::build` with `Roots::Constants`. On `Err`, the sorted
//!    diagnostics (E0069 from evaluation, E0075 to E0078) and no values,
//!    since `build` gives none on `Err` (D-1413). On `Ok`, every local
//!    constant's text from `cheby_consteval::debug_texts`, cut at
//!    [`BackendRequest::max_text`] characters (D-1413's 4,096), with the
//!    package's module files its initializer reaches, as D-538's seed
//!    computes them (D-1510).
//! 4. Every diagnostic is mapped to the [`Symbol`] of the constant whose
//!    initializer holds its primary span, so the server marks that one
//!    constant failed and keeps the others (D-1413, S-20).
//!
//! Every range and every hash is over normalized text: a leading BOM
//! removed and `\r\n` read as `\n` (D-1412, [`super::normalized_key`]).
//! The driver reads the bytes on disk, so ranges in a file with a BOM or a
//! `\r\n` go through [`super::NormMap::to_normalized_range`] before they
//! leave. Paths are relative to the root, as `cheby check` displays them.
//!
//! Nothing here prints: stdout belongs to the JSON document (A §11.3).

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::{Diagnostic, FileId, SourceMap, TextRange};
use cheby_iface::{DefId, DefKind, ModuleId, PackageId};
use cheby_package::{MANIFEST_FILE, Target};

use super::plain::{NormMap, Placed, PlainDiagnostic, normalized_key, plain};
use super::symbol::{Symbol, SymbolNs};
use crate::backend::{self, Roots};
use crate::cache::Hash128;
use crate::frontend::{Frontend, Mode};
use crate::{CheckOutput, Error};

/// What one child process computes (D-1410).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendRequest {
    /// The package root, the directory of its `cheby.toml`.
    pub root: PathBuf,
    /// The target to check for, as `cheby check --target` (D-550). `None`
    /// is the manifest's targets.
    pub target: Option<Target>,
    /// The one job of this process (D-1410).
    pub job: BackendJob,
    /// The cap on a constant's text, in characters (D-1413: 4,096).
    pub max_text: usize,
}

/// The job of one child process (D-1410). W15 adds `Ownership`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendJob {
    /// Evaluate every constant of the package, as `cheby check` does.
    Constants,
}

/// What [`backend_query`] found: plain data, ready for the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendResult {
    /// [`super::normalized_key`] of `cheby.toml` as read.
    pub manifest: Hash128,
    /// Every module file the front end read, in path order.
    pub files: Vec<BackendFile>,
    /// Whether the front end reported an error.
    pub front: FrontOutcome,
    /// The constants section, for [`BackendJob::Constants`].
    pub constants: Option<Section<ConstantsOut>>,
}

/// One file the child read, with the hash its ranges belong to (D-1412).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendFile {
    /// The path relative to the root, such as `src/main.cheby`.
    pub path: PathBuf,
    /// [`super::normalized_key`] of the text read.
    pub hash: Hash128,
}

/// How the front end ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontOutcome {
    /// No error: sections ran.
    Clean,
    /// An error, such as a saved file that does not check (plan §1.8):
    /// every section is [`Section::Skipped`].
    Errors,
}

/// One section's outcome (A §11.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Section<T> {
    /// The section ran.
    Ok(T),
    /// The section did not run, with the reason, such as "front-end
    /// errors".
    Skipped(String),
    /// The section failed inside, with the message, such as an internal
    /// error that stopped evaluation.
    Failed(String),
}

/// The constants section (A §11.2, D-1413).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConstantsOut {
    /// E0069 and E0075 to E0078, sorted as `cheby check` prints them.
    pub diagnostics: Vec<ConstDiagnostic>,
    /// The value of every evaluated local constant. Empty when evaluation
    /// failed, since `backend::build` gives no values on `Err` (D-1413).
    pub values: Vec<ConstValue>,
}

/// One back-end diagnostic, with its file and its constant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstDiagnostic {
    /// The file of the primary range, relative to the root; `cheby.toml`
    /// for a diagnostic with no span.
    pub path: PathBuf,
    /// The diagnostic. Its ranges, primary and related, are in normalized
    /// text, and a related label's path is relative to the root (D-1412).
    pub diagnostic: PlainDiagnostic,
    /// The constant whose initializer holds the primary range, which the
    /// server marks failed (D-1413). `None` when no constant does.
    pub symbol: Option<Symbol>,
}

/// One constant's value as `debug_texts` prints it (D-1413).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstValue {
    /// The constant (D-1365).
    pub symbol: Symbol,
    /// The text, cut at [`BackendRequest::max_text`] characters.
    pub text: String,
    /// The text was cut.
    pub truncated: bool,
    /// The package's module files that its initializer reaches through
    /// functions, constants and constructors, as D-538's seed computes
    /// them (D-593), its own included, in path order (D-1510). `None` when
    /// the build named none for it: every module then counts.
    pub modules: Option<Vec<PathBuf>>,
}

/// Why [`backend_query`] has no result: the wire's `status` other than
/// `ok` (A §11.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendError {
    /// No `cheby.toml` at the root: `no-package`.
    NoPackage,
    /// What `cheby check` treats as a tool error, such as an unreadable
    /// file or a target the manifest lacks: `tool-error`.
    Tool(String),
    /// An internal compiler error: `internal-error`.
    Internal(String),
}

/// The reason a section gives when the front end reported an error.
const FRONT_ERRORS: &str = "front-end errors";

/// Runs `request`'s job over the saved files at its root (module docs).
/// Reads no build cache (D-1363) and prints nothing.
///
/// # Errors
///
/// A [`BackendError`] when no section can run at all; a section's own
/// failure is [`Section::Failed`] inside an `Ok`.
pub fn backend_query(request: &BackendRequest) -> Result<BackendResult, BackendError> {
    if !request.root.join(MANIFEST_FILE).is_file() {
        return Err(BackendError::NoPackage);
    }
    // A panic in the driver is an internal error of this one job, which the
    // server reads as `internal-error` (A §11.3), never a dead child.
    catch_unwind(AssertUnwindSafe(|| run(request)))
        .unwrap_or_else(|payload| Err(BackendError::Internal(panic_message(&*payload))))
}

/// [`backend_query`] once the root has a manifest.
fn run(request: &BackendRequest) -> Result<BackendResult, BackendError> {
    let mut out = CheckOutput::default();
    // As `cheby check`: `Mode::Check`, so `CommandFamily::Check`, and no
    // cache, so no C compiles and no library opens (D-1410, A §10).
    let front = crate::front_end_for(&request.root, Mode::Check, request.target, &mut out, None)
        .map_err(|e| backend_error(&e))?;
    out.sources.sort(&mut out.diagnostics);
    let front = front.map(|(front, _)| front);
    let files = Files::new(&request.root, &out.sources, front.as_ref())?;
    let manifest = files.manifest_hash;
    let listed = files.listed();
    // Constants are evaluated only when the front end reported no error,
    // as `check_inner` has it (D-459, D-534).
    let Some(front) = front.filter(|_| !out.has_errors()) else {
        return Ok(BackendResult {
            manifest,
            files: listed,
            front: FrontOutcome::Errors,
            constants: match request.job {
                BackendJob::Constants => Some(Section::Skipped(FRONT_ERRORS.to_owned())),
            },
        });
    };
    let constants = match request.job {
        BackendJob::Constants => Some(constants(&front, &out.sources, &files, request.max_text)),
    };
    Ok(BackendResult {
        manifest,
        files: listed,
        front: FrontOutcome::Clean,
        constants,
    })
}

/// The constants section: `backend::build` with `Roots::Constants`, as
/// `cheby check` runs it (D-1413).
fn constants(
    front: &Frontend,
    sources: &SourceMap,
    files: &Files,
    max_text: usize,
) -> Section<ConstantsOut> {
    let consts = match local_consts(front) {
        Ok(consts) => consts,
        Err(message) => return Section::Failed(message),
    };
    let built = match backend::build(front, sources, Roots::Constants, false) {
        Ok(built) => built,
        Err(e) => return Section::Failed(e.message().to_owned()),
    };
    match built {
        // `build` gives no values on `Err` (D-1413): the diagnostics only,
        // each marking the constant it names.
        Err(mut diagnostics) => {
            sources.sort(&mut diagnostics);
            Section::Ok(ConstantsOut {
                diagnostics: diagnostics
                    .iter()
                    .map(|diag| files.diagnostic(diag, &consts))
                    .collect(),
                values: Vec::new(),
            })
        }
        Ok(built) => {
            // Only a constant with a slot was compiled: `debug_texts` takes
            // no other.
            let evaluated: Vec<&LocalConst> = consts
                .iter()
                .filter(|c| built.jit.const_slot(c.def).is_some())
                .collect();
            let defs: Vec<DefId> = evaluated.iter().map(|c| c.def).collect();
            let texts = match cheby_consteval::debug_texts(&built.jit, &defs) {
                Ok(texts) => texts,
                Err(ice) => return Section::Failed(ice.message().to_owned()),
            };
            let mut values: Vec<ConstValue> = evaluated
                .into_iter()
                .zip(texts)
                .filter_map(|(c, text)| {
                    let (text, truncated) = cut_text(text?, max_text);
                    let modules = built
                        .const_modules
                        .get(&c.def)
                        .map(|modules| files.module_paths(front, modules));
                    Some(ConstValue {
                        symbol: c.symbol.clone(),
                        text,
                        truncated,
                        modules,
                    })
                })
                .collect();
            values.sort_by(|a, b| a.symbol.cmp(&b.symbol));
            Section::Ok(ConstantsOut {
                diagnostics: Vec::new(),
                values,
            })
        }
    }
}

/// One constant of a local module: its handle, its name, and where it is
/// written, from its name to the end of its initializer.
struct LocalConst {
    def: DefId,
    symbol: Symbol,
    file: FileId,
    extent: TextRange,
}

/// Every constant of the package's own modules, in build order. A
/// constant with no current interface item has no [`Symbol`] (as
/// `Snapshot::key_of`) and is left out.
fn local_consts(front: &Frontend) -> Result<Vec<LocalConst>, String> {
    let names = &front.globals.names;
    let mut consts = Vec::new();
    for &module in &front.order {
        let Some(info) = front.modules.get(module) else {
            continue;
        };
        if info.package != PackageId::LOCAL {
            continue;
        }
        let checked = front
            .checked(module)
            .ok_or_else(|| format!("the module `{}` was not checked", info.path))?;
        let Some(iface) = front.set.get(module) else {
            continue;
        };
        for c in &checked.thir.consts {
            let Some(item) = iface.item(c.def.index) else {
                continue;
            };
            if item.kind != DefKind::Const
                || iface.is_hidden(c.def.index)
                || !iface.is_current(c.def.index)
            {
                continue;
            }
            let extent = c
                .body
                .expr(c.body.value)
                .filter(|value| value.span.file == c.span.file)
                .map_or(c.span.range, |value| c.span.range.cover(value.span.range));
            consts.push(LocalConst {
                def: c.def,
                symbol: Symbol::item(info.path.clone(), SymbolNs::Value, names.get(item.name)),
                file: c.span.file,
                extent,
            });
        }
    }
    Ok(consts)
}

/// `text` cut at `max` characters, and whether it was cut (D-1413). The
/// language server cuts a written initializer with it too, so both show
/// the same text (D-1496).
#[must_use]
pub fn cut_text(mut text: String, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((at, _)) => {
            text.truncate(at);
            (text, true)
        }
        None => (text, false),
    }
}

/// A file of the package as the child read it: `cheby.toml` or a module.
struct PackageFile {
    /// Relative to the root, as `cheby check` displays it.
    path: PathBuf,
    text: Arc<str>,
    /// From the bytes read to normalized text (D-1412).
    norm: NormMap,
    /// Whether this is a module file, listed in [`BackendResult::files`].
    module: bool,
}

/// The package's files of one run, by their id in its source map.
struct Files {
    by_id: BTreeMap<FileId, PackageFile>,
    /// The manifest's id, `None` if the source map lacks it.
    manifest: Option<FileId>,
    manifest_hash: Hash128,
}

impl Files {
    /// The package's files in `sources`: every file but std's, which the
    /// front end's module table names (no std file is read before it
    /// runs).
    fn new(
        root: &Path,
        sources: &SourceMap,
        front: Option<&Frontend>,
    ) -> Result<Self, BackendError> {
        let std_files: Vec<FileId> = front
            .map(|front| {
                front
                    .modules
                    .iter()
                    .filter(|(_, info)| info.package != PackageId::LOCAL)
                    .filter_map(|(_, info)| info.file)
                    .collect()
            })
            .unwrap_or_default();
        let mut by_id = BTreeMap::new();
        let mut manifest = None;
        for (id, file) in sources.files() {
            if std_files.contains(&id) {
                continue;
            }
            let is_manifest = file.path() == MANIFEST_FILE;
            if is_manifest {
                manifest = Some(id);
            }
            by_id.insert(
                id,
                PackageFile {
                    path: PathBuf::from(file.path()),
                    norm: NormMap::new(file.text()),
                    text: file.text_arc(),
                    module: !is_manifest,
                },
            );
        }
        let manifest_hash = if let Some(file) = manifest.and_then(|id| by_id.get(&id)) {
            normalized_key(&file.text)
        } else {
            let bytes = std::fs::read(root.join(MANIFEST_FILE))
                .map_err(|e| BackendError::Tool(format!("{MANIFEST_FILE}: {e}")))?;
            normalized_key(&String::from_utf8_lossy(&bytes))
        };
        Ok(Self {
            by_id,
            manifest,
            manifest_hash,
        })
    }

    /// The module files in path order, with their normalized hashes.
    fn listed(&self) -> Vec<BackendFile> {
        let mut listed: Vec<BackendFile> = self
            .by_id
            .values()
            .filter(|file| file.module)
            .map(|file| BackendFile {
                path: file.path.clone(),
                hash: normalized_key(&file.text),
            })
            .collect();
        listed.sort_by(|a, b| a.path.cmp(&b.path));
        listed
    }

    /// The paths of the package's module files among `modules`, sorted:
    /// std's and other packages' modules are left out, as
    /// [`BackendResult::files`] leaves them out (D-1510).
    fn module_paths(&self, front: &Frontend, modules: &[ModuleId]) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = modules
            .iter()
            .filter_map(|&module| front.modules.get(module)?.file)
            .filter_map(|file| self.by_id.get(&file))
            .filter(|file| file.module)
            .map(|file| file.path.clone())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// `diag` as plain data in normalized text, filed under its primary
    /// label's package file, or `cheby.toml` for no span or a span
    /// elsewhere (as the session files it), with the constant whose
    /// written extent holds its primary span.
    fn diagnostic(&self, diag: &Diagnostic, consts: &[LocalConst]) -> ConstDiagnostic {
        let primary = diag.primary_span();
        let owner = primary
            .map(|span| span.file)
            .filter(|file| self.by_id.contains_key(file))
            .or(self.manifest);
        let place = |file: FileId| {
            self.by_id.get(&file).map(|f| Placed {
                path: &f.path,
                text: &f.text,
            })
        };
        // An owner outside the package places nothing: `plain` keeps no
        // primary range then.
        let mut plain = plain(diag, owner.unwrap_or(FileId::new(u32::MAX)), place);
        let owner = owner.and_then(|id| self.by_id.get(&id));
        plain.range = plain
            .range
            .zip(owner)
            .and_then(|(range, file)| file.norm.to_normalized_range(range));
        let by_path = |path: &Path| self.by_id.values().find(|f| f.path == path);
        plain.related.retain_mut(|label| {
            let range = by_path(&label.path).and_then(|f| f.norm.to_normalized_range(label.range));
            range.map(|range| label.range = range).is_some()
        });
        plain.helps.retain_mut(|help| {
            help.edits.iter_mut().all(|edit| {
                let range =
                    by_path(&edit.path).and_then(|f| f.norm.to_normalized_range(edit.range));
                range.map(|range| edit.range = range).is_some()
            })
        });
        let symbol = primary.and_then(|span| {
            consts
                .iter()
                .find(|c| c.file == span.file && c.extent.contains_range(span.range))
                .map(|c| c.symbol.clone())
        });
        ConstDiagnostic {
            path: owner.map_or_else(|| PathBuf::from(MANIFEST_FILE), |f| f.path.clone()),
            diagnostic: plain,
            symbol,
        }
    }
}

/// A driver error as the wire's status: an internal compiler error or a
/// tool error (D-464).
fn backend_error(e: &Error) -> BackendError {
    if e.message().starts_with(crate::ICE_PREFIX) {
        BackendError::Internal(e.ice_message().to_owned())
    } else {
        BackendError::Tool(e.message().to_owned())
    }
}

/// The message of a panic's payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "the back end panicked".to_owned())
}

#[cfg(test)]
#[path = "backend_query_tests.rs"]
mod tests;
