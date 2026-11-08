//! Tests of the analysis API (D-1361, D-1362, D-1364, D-1365, D-1358): a
//! session's runs against `cheby check`, what a run keeps, symbols, cycle
//! cuts, containment and the life of snapshots.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use cheby_diag::{Colors, Diagnostic, FileId, LabelStyle, SourceMap};
use cheby_iface::{DefId, DefKind, Globals, ItemSig, ModuleId};
use cheby_package::{MANIFEST_FILE, ModulePath};

use crate::analysis::{
    FileReport, Live, Overlay, OverlayFile, PlainDiagnostic, PlainEdit, PlainHelp, PlainLabel,
    Report, Session, SessionConfig, Snapshot, Symbol, SymbolNs, SymbolTarget,
};
use crate::frontend::{Retain, RunExtras, run_analysis};

/// The codes the back end reports, which a session never does (the UI
/// harness's list).
const BACK_END_CODES: [&str; 6] = ["E0053", "E0069", "E0075", "E0076", "E0077", "E0078"];

const MANIFEST: &str = "name = \"app\"\nversion = \"0.1.0\"\n";
const UI_MANIFEST: &str = "name = \"ui_test\"\nversion = \"0.1.0\"\n";

/// A package in a fresh temporary directory, removed on drop.
struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A package `name` with these files, under the temporary directory.
fn package(name: &str, files: &[(&str, &str)]) -> Temp {
    let dir = std::env::temp_dir().join(format!("cheby_analysis_{}_{name}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    for (path, text) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    Temp(std::path::absolute(dir).unwrap())
}

/// The example packages, sorted.
fn examples() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join(MANIFEST_FILE).is_file())
        .map(|p| std::path::absolute(p).unwrap())
        .collect();
    dirs.sort();
    dirs
}

/// Every UI test: a `.cheby` file or a package directory, with its mode.
fn ui_cases() -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                if path.join(MANIFEST_FILE).is_file() {
                    out.push(path);
                } else {
                    walk(&path, out);
                }
            } else if path.extension().is_some_and(|e| e == "cheby") {
                out.push(path);
            }
        }
    }
    let mut cases = Vec::new();
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ui"),
        &mut cases,
    );
    cases
        .into_iter()
        .map(|case| {
            let main = if case.is_dir() {
                case.join("src/main.cheby")
            } else {
                case.clone()
            };
            let source = std::fs::read(&main).unwrap();
            let header = String::from_utf8_lossy(&source);
            let mode = header
                .lines()
                .take_while(|l| l.starts_with("//"))
                .map(|l| l.trim_start_matches('/').trim())
                .find_map(|l| l.strip_prefix("@mode:"))
                .map_or_else(String::new, |m| m.trim().to_owned());
            (case, mode)
        })
        .collect()
}

/// The package of a UI test: the directory itself, or a temporary
/// package with the file as `src/main.cheby`, as the UI harness makes.
fn ui_package(case: &Path, tag: &str) -> (PathBuf, Option<Temp>) {
    if case.is_dir() {
        return (std::path::absolute(case).unwrap(), None);
    }
    let text = std::fs::read(case).unwrap();
    let dir = std::env::temp_dir().join(format!("cheby_analysis_{}_{tag}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join(MANIFEST_FILE), UI_MANIFEST).unwrap();
    std::fs::write(dir.join("src/main.cheby"), text).unwrap();
    let dir = std::path::absolute(dir).unwrap();
    (dir.clone(), Some(Temp(dir)))
}

/// A session over `root` with fresh `Globals`.
fn session(root: &Path) -> Session {
    session_with(root, Arc::new(Globals::new()))
}

/// A session over `root` with these `Globals`.
fn session_with(root: &Path, globals: Arc<Globals>) -> Session {
    Session::new(SessionConfig {
        root: root.to_path_buf(),
        target: None,
        globals,
    })
}

/// An overlay of these open documents, stamped in order.
fn overlay(root: &Path, files: &[(&str, &str)]) -> Overlay {
    Overlay {
        files: files
            .iter()
            .zip(1_u64..)
            .map(|(&(path, text), stamp)| {
                (
                    root.join(path),
                    OverlayFile {
                        text: Arc::from(text),
                        version: 1,
                        stamp,
                    },
                )
            })
            .collect(),
    }
}

/// The non-empty lists of `files`, by path.
fn lists(files: &BTreeMap<PathBuf, Arc<FileReport>>) -> BTreeMap<PathBuf, Vec<PlainDiagnostic>> {
    files
        .iter()
        .filter(|(_, report)| !report.diagnostics.is_empty())
        .map(|(path, report)| (path.clone(), report.diagnostics.clone()))
        .collect()
}

/// `cheby check`'s diagnostics of the package at `root`, back-end codes
/// left out, as plain data by owning file, as a session files them: by the
/// primary label's package file, else `cheby.toml`. Built straight from
/// check's [`Diagnostic`]s, not through `plain`, so `live_matches_check`
/// tests `plain` against check rather than against itself. A package file
/// is `cheby.toml` or one under `src/`; std files are never placed.
fn check_lists(
    root: &Path,
    sources: &SourceMap,
    diagnostics: &[Diagnostic],
) -> BTreeMap<PathBuf, Vec<PlainDiagnostic>> {
    let package_file = |file: FileId| {
        let path = sources.file(file)?.path();
        (path == MANIFEST_FILE || path.starts_with("src/")).then(|| root.join(path))
    };
    let manifest = sources
        .files()
        .find(|(_, f)| f.path() == MANIFEST_FILE)
        .map(|(file, _)| file)
        .unwrap();
    let paths: BTreeMap<FileId, PathBuf> = sources
        .files()
        .filter_map(|(file, _)| Some((file, package_file(file)?)))
        .collect();
    let mut out: BTreeMap<PathBuf, Vec<PlainDiagnostic>> = BTreeMap::new();
    for diag in diagnostics {
        let primary = diag
            .labels
            .iter()
            .position(|label| label.style == LabelStyle::Primary);
        let primary_label = primary.and_then(|i| diag.labels.get(i));
        let owner = primary_label
            .map(|label| label.span.file)
            .filter(|file| paths.contains_key(file))
            .unwrap_or(manifest);
        let range = primary_label
            .filter(|label| paths.contains_key(&label.span.file))
            .map(|label| label.span.range);
        let related = diag
            .labels
            .iter()
            .enumerate()
            .filter(|&(i, _)| Some(i) != primary)
            .filter_map(|(_, label)| {
                Some(PlainLabel {
                    path: paths.get(&label.span.file)?.clone(),
                    range: label.span.range,
                    message: label.message.clone(),
                })
            })
            .collect();
        let helps = diag
            .helps
            .iter()
            .filter_map(|help| {
                let edits = help
                    .edits
                    .iter()
                    .map(|edit| {
                        Some(PlainEdit {
                            path: paths.get(&edit.span.file)?.clone(),
                            range: edit.span.range,
                            replacement: edit.replacement.clone(),
                        })
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(PlainHelp {
                    message: help.message.clone(),
                    edits,
                })
            })
            .collect();
        out.entry(paths.get(&owner).unwrap().clone())
            .or_default()
            .push(PlainDiagnostic {
                code: diag.code,
                severity: diag.severity(),
                message: diag.message.clone(),
                range,
                primary_message: primary_label.map(|l| l.message.clone()).unwrap_or_default(),
                related,
                notes: diag.notes.clone(),
                helps,
            });
    }
    out
}

/// How a package's live run compares with `cheby check`.
#[derive(Debug, PartialEq, Eq)]
enum Compared {
    /// The run is the check: every file's list equals check's.
    Report,
    /// Phase-A errors: each published file's list equals check's, and
    /// check has no error elsewhere.
    PhaseA,
    /// A cut cycle: the D run of W4 publishes, check has E0040.
    Cut,
}

/// `cheby check`'s diagnostics on the package at `root` up to its back
/// end, with its sources: check's list is the front end's, then the back
/// end's, whose codes only the back end reports (E0069 is the front end's
/// too, so codes alone cannot split the list).
fn checked(root: &Path) -> Result<(SourceMap, Vec<Diagnostic>), String> {
    let out = crate::check(root).map_err(|e| format!("check failed: {e}"))?;
    let mut front = crate::CheckOutput::default();
    crate::front_end_for(root, crate::frontend::Mode::Check, None, &mut front, None)
        .map_err(|e| format!("the front end failed: {e}"))?;
    front.sources.sort(&mut front.diagnostics);
    let (head, tail) = out
        .diagnostics
        .split_at(front.diagnostics.len().min(out.diagnostics.len()));
    if head != front.diagnostics.as_slice() {
        return Err("check's list does not start with its front end's".to_owned());
    }
    if let Some(d) = tail
        .iter()
        .find(|d| !BACK_END_CODES.contains(&d.code.to_string().as_str()))
    {
        return Err(format!(
            "check's back end reported {}: {}",
            d.code, d.message
        ));
    }
    let head = head.to_vec();
    Ok((out.sources, head))
}

/// Compares `live` with `cheby check` on the package at `root`.
fn compare(root: &Path, live: &Live) -> Result<Compared, String> {
    let (sources, diagnostics) = checked(root)?;
    let expected = check_lists(root, &sources, &diagnostics);
    if let Some(failed) = &live.failed {
        return Err(format!("the run failed: {failed}"));
    }
    if let Some(report) = &live.report {
        let found = lists(&report.files);
        return if found == expected {
            Ok(Compared::Report)
        } else {
            Err(format!("expected {expected:#?}\nfound {found:#?}"))
        };
    }
    let phase_a = lists(&live.phase_a);
    let has_errors =
        |list: &[PlainDiagnostic]| list.iter().any(|d| d.code.to_string().starts_with('E'));
    if phase_a.values().any(|list| has_errors(list)) {
        let kept: BTreeMap<PathBuf, Vec<PlainDiagnostic>> = expected
            .iter()
            .filter(|(path, _)| live.phase_a.contains_key(*path))
            .map(|(path, list)| (path.clone(), list.clone()))
            .collect();
        let elsewhere = expected
            .iter()
            .any(|(path, list)| !live.phase_a.contains_key(path) && has_errors(list));
        return if kept == phase_a && !elsewhere {
            Ok(Compared::PhaseA)
        } else {
            Err(format!("expected {expected:#?}\nphase A {phase_a:#?}"))
        };
    }
    let cut = live
        .snapshot
        .as_ref()
        .is_some_and(|s| !s.status().cut_imports.is_empty());
    let e0040 = diagnostics.iter().any(|d| d.code.to_string() == "E0040");
    if cut && e0040 {
        Ok(Compared::Cut)
    } else {
        Err(format!(
            "no report, no phase-A error and no cut: {expected:#?}"
        ))
    }
}

/// D-1361: with an empty overlay, a session's live run gives what
/// `cheby check` gives, compared field by field (code, severity, message,
/// every label's path, range and message, notes, helps): on every example
/// and every `check-fail` UI test. What check's back end adds after its
/// front end, the constants' diagnostics, is left out.
#[test]
#[cfg_attr(
    windows,
    ignore = "check() evaluates constants with JIT code; Windows support is M6 (D-545)"
)]
fn live_matches_check() {
    let mut packages: Vec<(PathBuf, Option<Temp>)> =
        examples().into_iter().map(|dir| (dir, None)).collect();
    for (n, (case, mode)) in ui_cases().into_iter().enumerate() {
        if mode == "check-fail" {
            packages.push(ui_package(&case, &format!("live_{n}")));
        }
    }
    let mut failures = Vec::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (root, _temp) in &packages {
        let live = session(root).analyze_live(&Overlay::default());
        let compared = live
            .map_err(|e| format!("the session failed: {e}"))
            .and_then(|live| compare(root, &live));
        match compared {
            Ok(kind) => *counts.entry(format!("{kind:?}")).or_default() += 1,
            Err(why) => failures.push(format!("{}: {why}", root.display())),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    assert!(packages.len() > 300, "{counts:?}");
    assert!(counts.get("Report").is_some_and(|&n| n > 200), "{counts:?}");
    assert!(counts.get("PhaseA").is_some_and(|&n| n > 10), "{counts:?}");
}

/// Renders the diagnostics of a front-end run of the package at `root`
/// with `retain` and no cut, as `cheby check` prints them, or the error.
fn rendered(root: &Path, retain: Retain) -> String {
    let mut sources = SourceMap::new();
    let mut diagnostics = Vec::new();
    let package = match crate::load_package(root, &mut sources, &mut diagnostics) {
        Ok(package) => package,
        Err(e) => return format!("load: {e}"),
    };
    if let Some(package) = package {
        let extras = RunExtras {
            globals: Arc::new(Globals::new()),
            retain,
            cut_cycles: None,
        };
        if let Err(e) = run_analysis(package, &mut sources, &mut diagnostics, extras) {
            return format!("run: {e}");
        }
    }
    sources.sort(&mut diagnostics);
    crate::render_diagnostics(&diagnostics, &sources, Colors::Off, false)
}

/// D-1380, D-1361: recording facts, keeping trees and containment change
/// no diagnostic: every example and every UI test renders the same under
/// `Retain::LIVE` as under `Retain::BUILD`.
#[test]
fn recording_changes_nothing() {
    let mut packages: Vec<(PathBuf, Option<Temp>)> =
        examples().into_iter().map(|dir| (dir, None)).collect();
    let examples = packages.len();
    for (n, (case, _)) in ui_cases().into_iter().enumerate() {
        packages.push(ui_package(&case, &format!("record_{n}")));
    }
    assert_eq!(examples, 32);
    assert!(packages.len() > 900, "{}", packages.len());
    let failures: Vec<String> = std::thread::scope(|scope| {
        let chunks: Vec<_> = packages
            .chunks(packages.len().div_ceil(8))
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .filter_map(|(root, _)| {
                            let build = rendered(root, Retain::BUILD);
                            let live = rendered(root, Retain::LIVE);
                            (build != live).then(|| {
                                format!("{}:\nBUILD\n{build}\nLIVE\n{live}", root.display())
                            })
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        chunks.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The kind of symbol a type parameter of an item of kind `kind` has.
fn ns_of(kind: DefKind) -> SymbolNs {
    match kind {
        DefKind::Type | DefKind::Alias | DefKind::Interface => SymbolNs::Type,
        _ => SymbolNs::Value,
    }
}

/// Checks that every import alias of `module` maps to its target in the
/// namespaces where it is bound, and to nothing in the others: a module's
/// alias is a value (D-062), an item's is in its item's namespaces
/// (D-111). Gives the number of aliases found.
fn aliases_round_trip(snapshot: &Snapshot, module: crate::analysis::ModuleView<'_>) -> usize {
    let names = &snapshot.globals().names;
    let path = module.path().unwrap().clone();
    let mut aliases = 0_usize;
    for (import, request) in module.import_requests().iter().enumerate() {
        if let Some((alias, _)) = request.alias
            && request.items.is_none()
        {
            let alias = names.get(alias);
            let symbol = Symbol::import_alias(path.clone(), SymbolNs::Value, alias);
            assert_eq!(
                snapshot.find(&symbol),
                Some(SymbolTarget::ImportAlias {
                    module: module.id(),
                    import,
                    item: None,
                })
            );
            for ns in [SymbolNs::Type, SymbolNs::Module, SymbolNs::Test] {
                let symbol = Symbol::import_alias(path.clone(), ns, alias);
                assert_eq!(snapshot.find(&symbol), None, "{symbol:?}");
            }
            aliases = aliases.saturating_add(1);
        }
        let target = module
            .imports()
            .get(import)
            .and_then(|t| t.module())
            .and_then(|t| snapshot.interfaces().get(t));
        for (item, request_item) in request.items.iter().flatten().enumerate() {
            let Some((alias, _)) = request_item.alias else {
                continue;
            };
            let target = target.unwrap();
            let alias = names.get(alias);
            for (ns, space) in [
                (SymbolNs::Value, cheby_iface::Namespace::Value),
                (SymbolNs::Type, cheby_iface::Namespace::Type),
            ] {
                let symbol = Symbol::import_alias(path.clone(), ns, alias);
                let expected =
                    target
                        .lookup(space, request_item.name)
                        .map(|_| SymbolTarget::ImportAlias {
                            module: module.id(),
                            import,
                            item: Some(item),
                        });
                assert_eq!(snapshot.find(&symbol), expected, "{symbol:?}");
                aliases = aliases.saturating_add(usize::from(expected.is_some()));
            }
        }
    }
    aliases
}

/// D-1365: in the snapshot of every example, every module and every item
/// of every interface (std's and the builtin module's included) maps to a
/// `Symbol` and back to itself, and so does every named field, type
/// parameter and import alias. A symbol of nothing finds nothing.
#[test]
fn symbol_round_trips() {
    let mut checked = 0_usize;
    let mut aliases = 0_usize;
    for root in examples() {
        let live = session(&root).analyze_live(&Overlay::default()).unwrap();
        let snapshot = live.snapshot.unwrap();
        let names = &snapshot.globals().names;
        for module in snapshot.all_modules() {
            let path = module.path().unwrap().clone();
            assert_eq!(
                snapshot.find(&Symbol::of_module(path.clone())),
                Some(SymbolTarget::Module(module.id()))
            );
            let Some(iface) = module.interface() else {
                continue;
            };
            for (index, item) in iface.items.iter_enumerated() {
                let def = DefId::new(module.id(), index);
                let key = snapshot
                    .key_of(def)
                    .unwrap_or_else(|| panic!("{path}: {index:?}"));
                assert_eq!(
                    snapshot.find(&key),
                    Some(SymbolTarget::Item(def)),
                    "{key:?}"
                );
                checked += 1;
                let name = names.get(item.name);
                if let ItemSig::Ctor(ctor) = &item.sig
                    && let cheby_iface::CtorFields::Named(fields) = &ctor.fields
                {
                    let ty = DefId::new(module.id(), ctor.ty);
                    let ty_name = names.get(iface.item(ctor.ty).unwrap().name);
                    for &(field, _) in fields {
                        let symbol = Symbol::field(path.clone(), ty_name, names.get(field));
                        assert_eq!(
                            snapshot.find(&symbol),
                            Some(SymbolTarget::Field { ty, name: field })
                        );
                    }
                }
                let generics = match &item.sig {
                    ItemSig::Fn(f) => Some(&f.generics),
                    ItemSig::Type(t) => Some(&t.generics),
                    ItemSig::Alias(a) => Some(&a.generics),
                    _ => None,
                };
                for (param, generic) in generics.iter().flat_map(|g| g.params.iter_enumerated()) {
                    let symbol = Symbol::type_param(
                        path.clone(),
                        ns_of(item.kind),
                        name,
                        names.get(generic.name),
                    );
                    assert_eq!(
                        snapshot.find(&symbol),
                        Some(SymbolTarget::TypeParam { item: def, param })
                    );
                }
            }
            aliases += aliases_round_trip(&snapshot, module);
        }
        let main = ModulePath::root(
            &snapshot
                .table()
                .get(ModuleId::from_u32(1))
                .unwrap()
                .path
                .segments()[0],
        );
        for missing in [
            Symbol::item(main.clone(), SymbolNs::Value, "no_such_item_anywhere"),
            Symbol::item(main.child("no_such_module"), SymbolNs::Value, "main"),
            Symbol::field(main.clone(), "NoSuchType", "x"),
        ] {
            assert_eq!(snapshot.find(&missing), None, "{missing:?}");
        }
    }
    assert!(checked > 10_000, "{checked}");
    assert!(aliases > 0, "{aliases}");
}

/// The lists of a session's live run, by path: the report's, or phase A's.
fn published(live: &Live) -> BTreeMap<PathBuf, Vec<PlainDiagnostic>> {
    live.report
        .as_ref()
        .map_or_else(|| lists(&live.phase_a), |r: &Report| lists(&r.files))
}

/// A §1.6: sessions that share one `Globals` table, warmed by every other
/// example first, publish what sessions with fresh tables publish.
#[test]
fn shared_globals_change_nothing() {
    let shared = Arc::new(Globals::new());
    let roots = examples();
    for root in roots.iter().rev() {
        session_with(root, Arc::clone(&shared))
            .analyze_live(&Overlay::default())
            .unwrap();
    }
    for root in &roots {
        let fresh = session(root).analyze_live(&Overlay::default()).unwrap();
        let again = session_with(root, Arc::clone(&shared))
            .analyze_live(&Overlay::default())
            .unwrap();
        assert_eq!(published(&fresh), published(&again), "{}", root.display());
        assert_eq!(fresh.report.is_some(), again.report.is_some());
        assert_eq!(
            fresh.report.as_ref().map(|r| r.inputs),
            again.report.as_ref().map(|r| r.inputs)
        );
        assert_eq!(
            fresh.snapshot.unwrap().baseline(),
            again.snapshot.unwrap().baseline()
        );
    }
}

/// A §2.3: every range a session publishes lies in the live text of its
/// path, inside the package, with an open document edited in each package:
/// every example and every `check-fail` UI test.
#[test]
fn plain_has_no_foreign_range() {
    let mut packages: Vec<(PathBuf, Option<Temp>)> =
        examples().into_iter().map(|dir| (dir, None)).collect();
    for (n, (case, mode)) in ui_cases().into_iter().enumerate() {
        if mode == "check-fail" {
            packages.push(ui_package(&case, &format!("foreign_{n}")));
        }
    }
    let mut ranges = 0_usize;
    for (root, _temp) in &packages {
        let main = root.join("src/main.cheby");
        let edited = format!("\n\n{}", std::fs::read_to_string(&main).unwrap_or_default());
        let overlay = overlay(root, &[("src/main.cheby", &edited)]);
        let live = session(root).analyze_live(&overlay).unwrap();
        let text_of = |path: &Path| -> String {
            overlay.files.get(path).map_or_else(
                || std::fs::read_to_string(path).unwrap(),
                |open| open.text.to_string(),
            )
        };
        let files = live
            .report
            .as_ref()
            .map_or(&live.phase_a, |report| &report.files);
        for (path, report) in files {
            assert!(path.starts_with(root), "{}", path.display());
            assert_eq!(*report.text, text_of(path), "{}", path.display());
            for diag in &report.diagnostics {
                let mut placed: Vec<(&Path, cheby_diag::TextRange)> = diag
                    .related
                    .iter()
                    .map(|l| (l.path.as_path(), l.range))
                    .chain(
                        diag.helps
                            .iter()
                            .flat_map(|h| &h.edits)
                            .map(|e| (e.path.as_path(), e.range)),
                    )
                    .collect();
                placed.extend(diag.range.map(|range| (path.as_path(), range)));
                for (at, range) in placed {
                    assert!(at.starts_with(root), "{}", at.display());
                    assert!(
                        range.slice(&text_of(at)).is_some(),
                        "{}: {range:?} in {}",
                        path.display(),
                        at.display()
                    );
                    ranges += 1;
                }
            }
        }
    }
    assert!(ranges > 500, "{ranges}");
}

const A: &str = "//! A.\n\nimport app::b\n\npub type Pair {\n  Pair(Int, Int),\n}\n\n\
                 pub fn f() -> Int {\n  1\n}\n\nfn uses_b() -> Int {\n  b::g()\n}\n";
const B: &str = "//! B.\n\nimport app::a\n\npub fn g() -> Int {\n  2\n}\n\n\
                 fn uses_a() -> Int {\n  a::f()\n}\n";
const MAIN_A: &str = "//! Root.\n\nimport app::a\n\nfn main() {\n  let _ = a::f()\n}\n";

/// The names of the functions in the THIR of `module` of `snapshot`.
fn fns(snapshot: &Snapshot, module: &str) -> Option<Vec<String>> {
    let view = view(snapshot, module);
    let thir = view.thir()?;
    let iface = view.interface().unwrap();
    Some(
        thir.fns
            .iter()
            .map(|f| {
                let name = iface.item(f.def.index).unwrap().name;
                snapshot.globals().names.get(name).to_owned()
            })
            .collect(),
    )
}

/// The view of package module `module` of `snapshot`: `main` is the root.
fn view<'s>(snapshot: &'s Snapshot, module: &str) -> crate::analysis::ModuleView<'s> {
    let path = match module {
        "main" => ModulePath::root("app"),
        _ => ModulePath::root("app").child(module),
    };
    snapshot
        .module(snapshot.table().find(&path).unwrap())
        .unwrap()
}

/// R-1, D-1358: a clean package with an import cycle. The Q run cuts the
/// one import of one edge and checks every module, so the snapshot has the types, THIR and
/// facts of each. The run is not the check, so it publishes no report.
#[test]
fn cycle_keeps_types_in_snapshot() {
    let temp = package(
        "cycle",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN_A),
            ("src/a.cheby", A),
            ("src/b.cheby", B),
        ],
    );
    let live = session(&temp.0).analyze_live(&Overlay::default()).unwrap();
    assert_eq!(live.failed, None);
    assert!(live.report.is_none());
    // Nothing is broken: phase A publishes `cheby.toml`'s empty list only.
    assert_eq!(
        live.phase_a.keys().collect::<Vec<_>>(),
        [&temp.0.join(MANIFEST_FILE)]
    );
    let snapshot = live.snapshot.unwrap();
    assert_eq!(snapshot.status().cut_imports.len(), 1);
    assert!(snapshot.status().broken.is_empty());
    for module in snapshot.local_modules() {
        assert!(module.thir().is_some(), "{module:?}");
        assert!(
            module.resolved().is_some() && module.tree().is_some(),
            "{module:?}"
        );
        assert!(
            module.facts().is_some_and(|f| f.lost.is_empty()),
            "{module:?}"
        );
    }
    let a = view(&snapshot, "a");
    let names = &snapshot.globals().names;
    let pair = a
        .interface()
        .unwrap()
        .lookup(cheby_iface::Namespace::Type, names.lookup("Pair").unwrap());
    assert!(pair.is_some());
    assert_eq!(fns(&snapshot, "main").unwrap(), ["main"]);
    assert!(fns(&snapshot, "a").unwrap().contains(&"uses_b".to_owned()));
    // Std keeps its interfaces only.
    for module in snapshot.all_modules().filter(|m| !m.is_local()) {
        assert!(
            module.tree().is_none() && module.thir().is_none(),
            "{module:?}"
        );
    }
    assert!(snapshot.all_ids().len() > 3);
    // The cut import of `b` no longer reaches `a`.
    assert_eq!(snapshot.importers(a.id()), [view(&snapshot, "main").id()]);
    assert_eq!(snapshot.importers(view(&snapshot, "b").id()), [a.id()]);
}

/// R-6, D-1386: a satisfaction query on a local type, with a std module
/// as `home` and a local view as `from`. A view of another snapshot gives
/// no input.
#[test]
fn query_input_answers_on_a_local_type() {
    let main = "//! Root.\n\ntype Point {\n  x: Int,\n  y: Int,\n}\n\n\
                fn show(p: Point) -> String {\n  \"{p.x}, {p.y}\"\n}\n\n\
                fn main() {\n  let _ = show(Point { x: 1, y: 2 })\n}\n";
    let temp = package(
        "query",
        &[(MANIFEST_FILE, MANIFEST), ("src/main.cheby", main)],
    );
    let mut session = session(&temp.0);
    let snapshot = session
        .analyze_live(&Overlay::default())
        .unwrap()
        .snapshot
        .unwrap();
    let from = view(&snapshot, "main");
    let names = &snapshot.globals().names;
    let point = from
        .interface()
        .unwrap()
        .lookup(cheby_iface::Namespace::Type, names.lookup("Point").unwrap())
        .unwrap();
    let ty = snapshot
        .globals()
        .types
        .named(DefId::new(from.id(), point), []);
    let home = snapshot
        .all_modules()
        .find(|m| !m.is_local() && m.interface().is_some() && m.id() != ModuleId::BUILTIN)
        .unwrap()
        .id();
    let cx = snapshot.query_input(home, from).unwrap();
    assert_eq!(cx.input.module, home);
    let listed = crate::analysis::interfaces_of(
        &cx,
        &cheby_iface::Generics::empty(),
        ty,
        &[cheby_iface::builtin::SHOW],
    )
    .unwrap();
    assert!(
        matches!(
            listed.as_slice(),
            [(iface, crate::analysis::Verdict::Yes { .. })] if *iface == cheby_iface::builtin::SHOW
        ),
        "{listed:?}"
    );
    let other = session
        .analyze_live(&Overlay::default())
        .unwrap()
        .snapshot
        .unwrap();
    assert!(other.query_input(home, from).is_none());
}

/// A §2.7: a package with no `src/` is one `cheby check` rejects. The run
/// publishes the message in `tool`, and the report is not clean.
#[test]
fn clean_is_false_without_src() {
    let temp = package("no_src", &[(MANIFEST_FILE, MANIFEST)]);
    assert!(crate::check(&temp.0).is_err());
    let live = session(&temp.0).analyze_live(&Overlay::default()).unwrap();
    assert_eq!(live.failed, None);
    let report = live.report.unwrap();
    assert!(!report.clean);
    assert_eq!(report.tool.len(), 1, "{:?}", report.tool);
}

/// D-1358: the cut is an import out of the module edited last.
#[test]
fn cut_prefers_newest_edit() {
    let temp = package(
        "newest",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN_A),
            ("src/a.cheby", A),
            ("src/b.cheby", B),
        ],
    );
    let root = &temp.0;
    let mut session = session(root);
    for (first, last) in [("a", "b"), ("b", "a")] {
        let first_path = format!("src/{first}.cheby");
        let last_path = format!("src/{last}.cheby");
        let text = |m: &str| if m == "a" { A } else { B };
        let overlay = overlay(
            root,
            &[
                (first_path.as_str(), text(first)),
                (last_path.as_str(), text(last)),
            ],
        );
        let live = session.analyze_live(&overlay).unwrap();
        let snapshot = live.snapshot.unwrap();
        let cut = &snapshot.status().cut_imports;
        assert_eq!(cut.len(), 1);
        let (path, range) = cut.first().unwrap();
        assert_eq!(*path, root.join(&last_path));
        let source = snapshot.source(snapshot.file_of(path).unwrap()).unwrap();
        assert_eq!(
            range.slice(source.text()),
            Some(format!("import app::{first}").as_str())
        );
    }
}

const MAIN_UTIL: &str = "//! Root.\n\nimport app::util\n\nfn main() {\n  let _ = util::ok()\n}\n";
const LONE: &str = "//! Lone.\n\npub fn lone() -> Int {\n  3\n}\n";

/// F §2.3, layer 1: an `Ice` in one body loses that body only. The run is
/// not the check, so it publishes no report, and the snapshot keeps every
/// module. The `cfg(test)` switch `ice_by_test` fakes the loss after the
/// recording check, so this tests what the run does with a loss, not the
/// check's own containment. A type error in another module keeps the
/// dropped-body gate quiet, so only the body's loss withholds the report.
#[test]
fn ice_in_one_body_is_contained() {
    let util =
        "//! Util.\n\npub fn ok() -> Int {\n  1\n}\n\npub fn ice_by_test() -> Int {\n  2\n}\n";
    let bad = "//! Bad.\n\npub fn bad() -> Int {\n  \"x\"\n}\n";
    let temp = package(
        "body_ice",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN_UTIL),
            ("src/util.cheby", util),
            ("src/bad.cheby", bad),
        ],
    );
    let live = session(&temp.0).analyze_live(&Overlay::default()).unwrap();
    assert_eq!(live.failed, None);
    assert!(live.report.is_none());
    let snapshot = live.snapshot.unwrap();
    assert!(!snapshot.silent_drop());
    assert!(snapshot.status().broken.is_empty());
    assert_eq!(snapshot.status().cut_imports, []);
    assert_eq!(snapshot.lost().len(), 0);
    let facts = view(&snapshot, "util").facts().unwrap();
    assert_eq!(facts.lost.len(), 1);
    assert_eq!(fns(&snapshot, "util").unwrap(), ["ok"]);
    assert_eq!(fns(&snapshot, "main").unwrap(), ["main"]);
}

/// F §2.3, layer 3: an `Ice` in one module before it publishes its
/// interface (the switch `module_ice_by_test`) loses it and the modules
/// that import it, and no other.
#[test]
fn ice_in_one_module_is_contained() {
    let util =
        "//! Util.\n\npub fn ok() -> Int {\n  1\n}\n\nfn module_ice_by_test() -> Int {\n  2\n}\n";
    let temp = package(
        "module_ice",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN_UTIL),
            ("src/util.cheby", util),
            ("src/lone.cheby", LONE),
        ],
    );
    let live = session(&temp.0).analyze_live(&Overlay::default()).unwrap();
    assert_eq!(live.failed, None);
    assert!(live.report.is_none());
    let snapshot = live.snapshot.unwrap();
    let lost = snapshot.lost();
    assert_eq!(lost.len(), 1);
    assert_eq!(lost.first().unwrap().0, view(&snapshot, "util").id());
    assert!(fns(&snapshot, "util").is_none());
    assert!(fns(&snapshot, "main").is_none());
    assert_eq!(fns(&snapshot, "lone").unwrap(), ["lone"]);
}

/// R-14: a panic in a run (the switch `panic_by_test`) ends that run
/// only. The session publishes phase A, and its next run is whole.
#[test]
fn panic_in_run_keeps_session() {
    let panicking =
        "//! Lone.\n\npub fn lone() -> Int {\n  3\n}\n\nfn panic_by_test() -> Int {\n  0\n}\n";
    let temp = package(
        "panic",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", "//! Root.\n\nfn main() {\n  Nil\n}\n"),
            ("src/lone.cheby", panicking),
        ],
    );
    let mut session = session(&temp.0);
    let live = session.analyze_live(&Overlay::default()).unwrap();
    assert!(live.snapshot.is_none());
    let failed = live.failed.unwrap();
    assert!(failed.contains("a run panicked by a test"), "{failed}");
    assert!(live.phase_a.contains_key(&temp.0.join(MANIFEST_FILE)));
    let fixed = overlay(&temp.0, &[("src/lone.cheby", LONE)]);
    let live = session.analyze_live(&fixed).unwrap();
    assert_eq!(live.failed, None);
    let report = live.report.unwrap();
    assert!(report.clean);
    assert_eq!(
        report
            .files
            .get(&temp.0.join("src/lone.cheby"))
            .unwrap()
            .version,
        Some(1)
    );
    let snapshot = live.snapshot.unwrap();
    assert_eq!(snapshot.id().generation, 2);
    assert_eq!(fns(&snapshot, "lone").unwrap(), ["lone"]);
}

/// D-1361: a session keeps no snapshot. After 200 runs the last snapshot
/// has exactly one strong reference, the caller's, and every earlier one
/// is gone.
#[test]
fn snapshot_count_stays_one() {
    let temp = package(
        "count",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", "//! Root.\n\nfn main() {\n  Nil\n}\n"),
        ],
    );
    let mut session = session(&temp.0);
    let mut earlier: Vec<Weak<Snapshot>> = Vec::new();
    let mut last = None;
    for n in 0..200_i32 {
        let text = format!("//! Root.\n\nfn main() {{\n  Nil\n}}\n\nfn f() -> Int {{\n  {n}\n}}\n");
        let mut overlay = overlay(&temp.0, &[("src/main.cheby", &text)]);
        for file in overlay.files.values_mut() {
            file.version = n;
        }
        let live = session.analyze_live(&overlay).unwrap();
        let snapshot = live.snapshot.unwrap();
        assert_eq!(snapshot.version_of(&temp.0.join("src/main.cheby")), Some(n));
        if let Some(previous) = last.replace(snapshot) {
            earlier.push(Arc::downgrade(&previous));
        }
    }
    let last = last.unwrap();
    assert_eq!(Arc::strong_count(&last), 1);
    assert_eq!(earlier.len(), 199);
    assert!(earlier.iter().all(|weak| weak.upgrade().is_none()));
}

/// A snapshot crosses threads: the server's query pool reads it (A §3.4).
#[test]
fn snapshot_is_send_and_sync() {
    fn shared<T: Send + Sync>() {}
    shared::<Snapshot>();
    shared::<Live>();
}

/// A session reads disk once, until a sweep or an invalidation sees a
/// change: an edited file, a new module, a deleted one.
#[test]
fn sweep_sees_disk_changes() {
    let temp = package(
        "sweep",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", "//! Root.\n\nfn main() {\n  Nil\n}\n"),
        ],
    );
    let root = &temp.0;
    let main = root.join("src/main.cheby");
    let mut session = session(root);
    let live = session.analyze_live(&Overlay::default()).unwrap();
    assert_eq!(live.files.len(), 2);
    assert!(!session.sweep());
    // Held until a sweep sees it.
    std::fs::write(&main, "//! Root.\n\nfn main() {\n  1\n}\n").unwrap();
    let held = session.analyze_live(&Overlay::default()).unwrap();
    assert!(held.report.unwrap().clean);
    assert!(session.sweep());
    let fresh = session.analyze_live(&Overlay::default()).unwrap();
    assert!(!fresh.report.unwrap().clean);
    // A new module joins the walk.
    std::fs::write(root.join("src/util.cheby"), LONE).unwrap();
    assert!(session.sweep());
    let live = session.analyze_live(&Overlay::default()).unwrap();
    assert!(live.files.contains(&root.join("src/util.cheby")));
    // An invalidated path is read again.
    std::fs::remove_file(root.join("src/util.cheby")).unwrap();
    session.invalidate(&[root.join("src/util.cheby")]);
    let live = session.analyze_live(&Overlay::default()).unwrap();
    assert_eq!(live.files.len(), 2);
}
