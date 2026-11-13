//! Tests of `child` and `progress`: the constants job's state machine
//! with canned documents, on every platform, and with real processes from
//! a shell script that stands in for `cheby`, where processes are run
//! (D-545).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cheby_driver::analysis::{
    Code, FileReport, Hash128, ModulePath, Report, Symbol, SymbolNs, Target, TextOrigin, exact_key,
    normalized_key,
};
use serde_json::Value;

use super::*;
use crate::child::wire::{
    ConstantsSection, Envelope, FileHash, Front, Section, Sections, Status, SymbolData,
    WireDiagnostic, WireHash, WireRange, WireSeverity, WireValue,
};
use crate::server::progress::Progress;

const MAIN: &str = "pub const limit: Int = 20\npub const boom: Int = 1\n";

/// A package directory of its own, emptied first.
fn package(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cheby_lsp_child_{}_{name}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    dir
}

fn report(root: &Path, clean: bool, inputs: u8) -> Report {
    let file = FileReport {
        text: Arc::from(MAIN),
        origin: TextOrigin::Disk,
        version: None,
        hash: exact_key(MAIN),
        diagnostics: Vec::new(),
    };
    Report {
        files: BTreeMap::from([(root.join("src").join("main.cheby"), Arc::new(file))]),
        held: BTreeSet::new(),
        tool: Vec::new(),
        inputs: Hash128([inputs; 16]),
        clean,
        substituted: false,
    }
}

fn symbol(name: &str) -> Symbol {
    Symbol::item(ModulePath::root("app"), SymbolNs::Value, name)
}

fn value(name: &str, text: &str) -> WireValue {
    WireValue {
        symbol: SymbolData::from(&symbol(name)),
        text: text.to_owned(),
        truncated: false,
        modules: None,
    }
}

fn panicked(name: &str) -> WireDiagnostic {
    WireDiagnostic {
        code: "E0075".to_owned(),
        severity: WireSeverity::Error,
        message: format!("constant `{name}` panicked"),
        path: "src/main.cheby".to_owned(),
        range: Some(WireRange(47, 51)),
        primary_message: String::new(),
        related: Vec::new(),
        notes: Vec::new(),
        helps: Vec::new(),
        symbol: Some(SymbolData::from(&symbol(name))),
    }
}

/// An `ok` document over [`MAIN`] with these results.
fn document(diagnostics: Vec<WireDiagnostic>, values: Vec<WireValue>) -> Envelope {
    Envelope {
        schema: crate::child::wire::SCHEMA,
        status: Status::Ok,
        manifest: Some(WireHash(Hash128([7; 16]))),
        files: vec![FileHash {
            path: "src/main.cheby".to_owned(),
            hash: WireHash(normalized_key(MAIN)),
        }],
        front: Some(Front::Clean),
        sections: Sections {
            constants: Some(Section::Ok(ConstantsSection {
                diagnostics,
                values,
            })),
            ownership: None,
        },
    }
}

/// Writes the fixture file of `root` with `documents`, one per run.
fn write_fixture(root: &Path, documents: &[Envelope]) -> PathBuf {
    let path = root.join("fixture.json");
    std::fs::write(&path, serde_json::to_string(documents).unwrap()).unwrap();
    path
}

fn with_fixture(fixture: PathBuf) -> Children {
    let mut children = Children::new();
    children.fixture = Some(fixture);
    children
}

fn env(analyzing: &BTreeSet<PathBuf>) -> ChildEnv<'_> {
    ChildEnv {
        exe: None,
        evaluate: true,
        timeout: Duration::from_secs(60),
        target: None,
        progress: true,
        analyzing,
    }
}

/// The `window/showMessage` notifications in `out`. Only the process tests use it.
#[cfg(unix)]
fn shows(out: &Outbox) -> usize {
    out.effects
        .iter()
        .filter(|e| {
            matches!(
                e,
                Effect::Notify {
                    method: "window/showMessage",
                    ..
                }
            )
        })
        .count()
}

/// The editor's text of the package at `root`, as hover reads it from a
/// snapshot: `src/main.cheby` alone, with the normalized hash `hash`
/// (D-1510).
fn editor(root: &Path, hash: Hash128) -> BTreeMap<PathBuf, Hash128> {
    BTreeMap::from([(main_path(root), hash)])
}

fn main_path(root: &Path) -> PathBuf {
    root.join("src").join("main.cheby")
}

/// Writes `text` to `src/main.cheby` on disk, as a save does: the text
/// the child reads changes, so the save makes a run due (D-1494).
fn write_main(root: &Path, text: &str) {
    std::fs::write(main_path(root), text).unwrap();
}

#[test]
fn first_clean_report_runs_and_keeps_results() {
    let root = package("first_clean");
    let fixture = write_fixture(
        &root,
        &[document(vec![panicked("boom")], vec![value("limit", "20")])],
    );
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let now = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        now,
    );
    assert!(
        children.busy(&root),
        "the first clean report makes a run due"
    );
    assert_eq!(children.deadline(), Some(now));
    let mut out = Outbox::default();
    children.schedule(&env(&none), now, &mut out);
    assert!(!children.busy(&root));
    assert_eq!(
        out.republish.get(&root),
        Some(&BTreeSet::from([main_path(&root)]))
    );
    let shown = children.diagnostics(&root, &main_path(&root), normalized_key(MAIN));
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].code, Code::parse("E0075").unwrap());
    // An edit of the file hides its child diagnostics (D-1412).
    let edited = normalized_key("pub const limit: Int = 21\n");
    assert_eq!(
        children.diagnostics(&root, &main_path(&root), edited).len(),
        0
    );
    // An error marks evaluation failed: no values came (D-1413).
    let values = children.constants(&root).unwrap();
    assert_eq!(
        values.reading(&symbol("boom"), &editor(&root, normalized_key(MAIN))),
        ConstReading::Failed {
            code: Code::parse("E0075").unwrap()
        }
    );
    // A constant with no value after a failed run says so, not that a
    // save computes it (D-1490).
    assert_eq!(
        values.reading(&symbol("limit"), &editor(&root, normalized_key(MAIN))),
        ConstReading::AfterFailure
    );
    // A later clean report with no save runs nothing.
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 2),
        &env(&none),
        now,
    );
    assert!(!children.busy(&root));
}

#[test]
fn one_failing_constant_keeps_other_values() {
    let root = package("keeps_others");
    let fixture = write_fixture(
        &root,
        &[
            document(Vec::new(), vec![value("limit", "20"), value("boom", "1")]),
            document(vec![panicked("boom")], Vec::new()),
        ],
    );
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let t0 = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        t0,
    );
    children.schedule(&env(&none), t0, &mut Outbox::default());
    let current = normalized_key(MAIN);
    let first = children.constants(&root).unwrap();
    assert_eq!(
        first.reading(&symbol("limit"), &editor(&root, current)),
        ConstReading::Value {
            text: "20".to_owned(),
            truncated: false,
            stale: false,
            evaluating: false,
        }
    );
    // A value from text that changed since is stale.
    assert_eq!(
        first.reading(&symbol("limit"), &editor(&root, Hash128([1; 16]))),
        ConstReading::Value {
            text: "20".to_owned(),
            truncated: false,
            stale: true,
            evaluating: false,
        }
    );
    let mut out = Outbox::default();
    write_main(&root, MAIN);
    children.on_save(&root, &[main_path(&root)], &env(&none), t0, &mut out);
    let t1 = t0.checked_add(COALESCE).unwrap();
    // Not due before the coalescing wait.
    children.schedule(&env(&none), t0, &mut out);
    assert!(children.busy(&root));
    assert_eq!(children.deadline(), Some(t1));
    children.schedule(&env(&none), t1, &mut out);
    assert!(!children.busy(&root));
    let second = children.constants(&root).unwrap();
    assert_eq!(
        second.reading(&symbol("limit"), &editor(&root, current)),
        ConstReading::Value {
            text: "20".to_owned(),
            truncated: false,
            stale: true,
            evaluating: false,
        }
    );
    assert_eq!(
        second.reading(&symbol("boom"), &editor(&root, current)),
        ConstReading::Failed {
            code: Code::parse("E0075").unwrap()
        }
    );
    // Hover's reader still holds the first values, unchanged.
    assert!(!first.stale);
}

/// D-1494: with no child running, a save makes a run due only when the
/// text on disk differs from the last run's, and a save of unchanged text
/// leaves a due run where it was.
#[test]
fn unchanged_save_starts_nothing() {
    let root = package("unchanged_save");
    let fixture = write_fixture(&root, &[document(Vec::new(), vec![value("limit", "20")])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let t0 = Instant::now();
    write_main(&root, MAIN);
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        t0,
    );
    children.schedule(&env(&none), t0, &mut Outbox::default());
    assert_eq!(children.next_run, 1);
    let mut out = Outbox::default();
    children.on_save(&root, &[main_path(&root)], &env(&none), t0, &mut out);
    assert!(!children.busy(&root), "the text the last run read");
    // A watched change to a file the child does not read changes nothing.
    let notes = root.join("notes.txt");
    std::fs::write(&notes, "notes").unwrap();
    children.on_save(&root, &[notes], &env(&none), t0, &mut out);
    assert!(!children.busy(&root));
    // A new module file is read, so creating it makes a run due.
    let other = root.join("src").join("other.cheby");
    std::fs::write(&other, "pub const one: Int = 1\n").unwrap();
    let t1 = t0.checked_add(COALESCE).unwrap();
    children.on_save(&root, &[other], &env(&none), t0, &mut out);
    assert_eq!(children.deadline(), Some(t1));
    // An unchanged save while it is due moves nothing.
    let later = t0.checked_add(Duration::from_millis(200)).unwrap();
    children.on_save(&root, &[main_path(&root)], &env(&none), later, &mut out);
    assert_eq!(children.deadline(), Some(t1));
    children.schedule(&env(&none), t1, &mut out);
    assert_eq!(children.next_run, 2);
    // A changed text runs again.
    write_main(&root, &format!("{MAIN}// edited\n"));
    children.on_save(&root, &[main_path(&root)], &env(&none), t1, &mut out);
    assert!(children.busy(&root));
}

/// D-1494: a save's echo, of the text that made the run due, leaves the
/// due run where that save put it, however many echoes come.
#[test]
fn an_echo_of_the_due_save_moves_nothing() {
    let root = package("echo_due");
    let fixture = write_fixture(&root, &[document(Vec::new(), vec![value("limit", "20")])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let t0 = Instant::now();
    write_main(&root, MAIN);
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        t0,
    );
    children.schedule(&env(&none), t0, &mut Outbox::default());
    assert_eq!(children.next_run, 1);
    let mut out = Outbox::default();
    write_main(&root, &format!("{MAIN}// edited\n"));
    children.on_save(&root, &[main_path(&root)], &env(&none), t0, &mut out);
    let due = t0.checked_add(COALESCE).unwrap();
    assert_eq!(children.deadline(), Some(due));
    for millis in [50, 100, 200, 250] {
        let echo = t0.checked_add(Duration::from_millis(millis)).unwrap();
        children.on_save(&root, &[main_path(&root)], &env(&none), echo, &mut out);
        assert_eq!(children.deadline(), Some(due), "an echo at {millis} ms");
    }
    // A save that changes the text again still coalesces.
    write_main(&root, &format!("{MAIN}// edited twice\n"));
    let later = t0.checked_add(Duration::from_millis(250)).unwrap();
    children.on_save(&root, &[main_path(&root)], &env(&none), later, &mut out);
    assert_eq!(children.deadline(), later.checked_add(COALESCE));
}

#[test]
fn unclean_report_defers_the_run() {
    let root = package("unclean");
    let fixture = write_fixture(&root, &[document(Vec::new(), vec![value("limit", "20")])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let t0 = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, false, 1),
        &env(&none),
        t0,
    );
    assert!(!children.busy(&root));
    let mut out = Outbox::default();
    children.on_save(&root, &[main_path(&root)], &env(&none), t0, &mut out);
    let t1 = t0.checked_add(COALESCE).unwrap();
    children.schedule(&env(&none), t1, &mut out);
    assert!(!children.busy(&root), "an unclean report starts nothing");
    assert!(children.packages[&root].pending);
    assert_eq!(
        children
            .constants(&root)
            .unwrap()
            .reading(&symbol("limit"), &editor(&root, normalized_key(MAIN))),
        ConstReading::NotYet
    );
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 2),
        &env(&none),
        t1,
    );
    assert!(children.busy(&root), "the next clean report starts it");
    children.schedule(&env(&none), t1, &mut out);
    assert!(matches!(
        children
            .constants(&root)
            .unwrap()
            .reading(&symbol("limit"), &editor(&root, normalized_key(MAIN))),
        ConstReading::Value { .. }
    ));
}

#[test]
fn due_run_waits_for_its_analysis_without_spinning() {
    let root = package("waits");
    let fixture = write_fixture(&root, &[document(Vec::new(), Vec::new())]);
    let mut children = with_fixture(fixture);
    let analyzing = BTreeSet::from([root.clone()]);
    let t0 = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&analyzing),
        t0,
    );
    let mut out = Outbox::default();
    children.schedule(&env(&analyzing), t0, &mut out);
    assert!(children.busy(&root));
    // Already due when the loop last looked: only an event can start it.
    assert_eq!(children.deadline(), None);
    let none = BTreeSet::new();
    children.schedule(&env(&none), t0, &mut out);
    assert!(!children.busy(&root));
}

#[test]
fn single_file_and_evaluate_off_never_evaluate() {
    let root = package("never");
    let fixture = write_fixture(&root, &[document(Vec::new(), Vec::new())]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let now = Instant::now();
    children.on_report(
        &root,
        ReportFacts::SINGLE_FILE,
        &report(&root, true, 1),
        &env(&none),
        now,
    );
    assert!(!children.busy(&root));
    assert!(children.constants(&root).is_none());
    let off = ChildEnv {
        evaluate: false,
        ..env(&none)
    };
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &off,
        now,
    );
    assert!(!children.busy(&root));
    assert!(children.constants(&root).is_none());
    // No executable and no fixture: no run either.
    let mut bare = Children::new();
    bare.fixture = None;
    bare.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        now,
    );
    assert!(!bare.busy(&root));
    assert!(bare.constants(&root).is_none());
}

#[test]
fn fixture_is_ignored_in_a_release_build() {
    // S-24: the variable is taken only when `debug` holds, which
    // `Children::new` sets from `cfg!(debug_assertions)`.
    let var = Some(std::ffi::OsString::from("canned.json"));
    assert_eq!(fixture_from(false, var.clone()), None);
    assert_eq!(fixture_from(true, var), Some(PathBuf::from("canned.json")));
    assert_eq!(fixture_from(true, None), None);
}

/// D-1493: raising `constants.childTimeoutSeconds` above the limit that
/// cut a run short makes it due at once; the same limit or a lower one
/// does not, nor does a cut by the output cap.
#[test]
fn raising_the_time_limit_lifts_a_cut() {
    let root = package("lift");
    let fixture = write_fixture(&root, &[document(Vec::new(), vec![value("limit", "20")])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let t0 = Instant::now();
    let short = ChildEnv {
        timeout: Duration::from_secs(5),
        ..env(&none)
    };
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &short,
        t0,
    );
    children.packages.get_mut(&root).unwrap().constants = ChildState::Idle;
    children.packages.get_mut(&root).unwrap().last_cut = Some(Cut {
        key: Hash128([1; 16]),
        limit: Some(Duration::from_secs(5)),
    });
    let mut out = Outbox::default();
    // The same limit, or a lower one, changes nothing.
    children.on_settings(&short, t0, &mut out);
    assert!(!children.busy(&root));
    // A higher one runs the same key again (A §11.5).
    children.on_settings(&env(&none), t0, &mut out);
    assert!(children.busy(&root));
    assert_eq!(children.packages[&root].last_cut, None);
    children.schedule(&env(&none), t0, &mut out);
    assert_eq!(children.next_run, 1);
    // A cut by the output cap is not a setting's to lift.
    children.packages.get_mut(&root).unwrap().last_cut = Some(Cut {
        key: Hash128([1; 16]),
        limit: None,
    });
    let long = ChildEnv {
        timeout: Duration::from_secs(600),
        ..env(&none)
    };
    children.on_settings(&long, t0, &mut out);
    assert!(!children.busy(&root));
}

#[test]
fn evaluate_setting_applies_at_once() {
    let root = package("toggle");
    let fixture = write_fixture(&root, &[document(vec![panicked("boom")], vec![])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let t0 = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        t0,
    );
    children.schedule(&env(&none), t0, &mut Outbox::default());
    assert_eq!(
        children
            .diagnostics(&root, &main_path(&root), normalized_key(MAIN))
            .len(),
        1
    );
    let off = ChildEnv {
        evaluate: false,
        ..env(&none)
    };
    let mut out = Outbox::default();
    children.on_settings(&off, t0, &mut out);
    assert!(children.constants(&root).is_none(), "hover shows no value");
    assert_eq!(
        children.diagnostics(&root, &main_path(&root), normalized_key(MAIN)),
        &[]
    );
    assert_eq!(
        out.republish.get(&root),
        Some(&BTreeSet::from([main_path(&root)]))
    );
    children.on_settings(&env(&none), t0, &mut out);
    assert!(children.constants(&root).is_some());
    assert!(children.busy(&root), "turned on, a run comes due");
    children.schedule(&env(&none), t0, &mut out);
    assert_eq!(children.next_run, 2);
}

#[test]
fn package_with_no_constants_starts_no_child() {
    let root = package("no_constants");
    let fixture = write_fixture(&root, &[document(vec![panicked("boom")], vec![])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let t0 = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        t0,
    );
    children.schedule(&env(&none), t0, &mut Outbox::default());
    assert_eq!(children.next_run, 1);
    // The constants are gone: no run, and the old results go too.
    let facts = ReportFacts {
        has_constants: false,
        ..ReportFacts::PACKAGE
    };
    write_main(&root, "fn main() {}\n");
    children.on_report(&root, facts, &report(&root, true, 2), &env(&none), t0);
    let mut out = Outbox::default();
    children.on_save(&root, &[main_path(&root)], &env(&none), t0, &mut out);
    let t1 = t0.checked_add(COALESCE).unwrap();
    children.schedule(&env(&none), t1, &mut out);
    assert_eq!(children.next_run, 1, "no child");
    assert!(!children.busy(&root));
    assert!(out.effects.is_empty(), "no progress");
    assert_eq!(
        children.diagnostics(&root, &main_path(&root), normalized_key(MAIN)),
        &[]
    );
    assert_eq!(
        children
            .constants(&root)
            .unwrap()
            .reading(&symbol("boom"), &editor(&root, normalized_key(MAIN))),
        ConstReading::NotYet
    );
    assert_eq!(
        children.status(&root, true, None, &env(&none), t1)["notEvaluated"],
        "the package has no constants"
    );
}

#[test]
fn unclean_disk_drops_the_pending_run() {
    let root = package("unclean_disk");
    let fixture = write_fixture(&root, &[document(Vec::new(), vec![value("limit", "20")])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let t0 = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        t0,
    );
    children.schedule(&env(&none), t0, &mut Outbox::default());
    // A save of a file that does not check.
    let facts = ReportFacts {
        disk_unclean: true,
        ..ReportFacts::PACKAGE
    };
    children.on_report(&root, facts, &report(&root, false, 2), &env(&none), t0);
    let mut out = Outbox::default();
    children.on_save(&root, &[main_path(&root)], &env(&none), t0, &mut out);
    let t1 = t0.checked_add(COALESCE).unwrap();
    children.schedule(&env(&none), t1, &mut out);
    assert!(!children.packages[&root].pending);
    // Fixing the buffer without a save starts nothing: disk is broken.
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 3),
        &env(&none),
        t1,
    );
    assert!(!children.busy(&root));
    assert_eq!(children.next_run, 1);
}

#[test]
fn nothing_comes_due_after_shutdown() {
    let root = package("closed");
    let fixture = write_fixture(&root, &[document(Vec::new(), Vec::new())]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let t0 = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        t0,
    );
    children.schedule(&env(&none), t0, &mut Outbox::default());
    children.kill_all();
    let mut out = Outbox::default();
    children.on_save(&root, &[main_path(&root)], &env(&none), t0, &mut out);
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 2),
        &env(&none),
        t0,
    );
    assert!(!children.busy(&root));
    assert_eq!(children.deadline(), None);
}

#[test]
fn bom_and_crlf_texts_keep_child_diagnostics() {
    let root = package("normalized");
    let fixture = write_fixture(&root, &[document(vec![panicked("boom")], Vec::new())]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let now = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        now,
    );
    children.schedule(&env(&none), now, &mut Outbox::default());
    // The child hashed normalized text, as the server does (D-1412).
    for live in [format!("\u{feff}{MAIN}"), MAIN.replace('\n', "\r\n")] {
        let shown = children.diagnostics(&root, &main_path(&root), normalized_key(&live));
        assert_eq!(shown.len(), 1, "{live:?}");
    }
}

#[test]
fn failed_documents_keep_results_and_mark_values_stale() {
    let root = package("failed_doc");
    let mut skipped = document(Vec::new(), Vec::new());
    skipped.front = Some(Front::Errors);
    skipped.sections.constants = Some(Section::Skipped {
        reason: "front-end errors".to_owned(),
    });
    let refused = Envelope {
        schema: crate::child::wire::SCHEMA,
        status: Status::InternalError {
            message: "boom".to_owned(),
        },
        manifest: None,
        files: Vec::new(),
        front: None,
        sections: Sections::default(),
    };
    let fixture = write_fixture(
        &root,
        &[
            document(vec![panicked("boom")], vec![value("limit", "20")]),
            skipped,
            refused,
        ],
    );
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let mut now = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        now,
    );
    children.schedule(&env(&none), now, &mut Outbox::default());
    for outcome in ["skipped", "failed"] {
        let mut out = Outbox::default();
        write_main(&root, &format!("{MAIN}// {outcome}\n"));
        children.on_save(&root, &[main_path(&root)], &env(&none), now, &mut out);
        now = now.checked_add(COALESCE).unwrap();
        children.schedule(&env(&none), now, &mut out);
        let package = &children.packages[&root];
        assert!(package.last.as_ref().unwrap().outcome.starts_with(outcome));
        assert!(package.values.stale);
        assert!(out.republish.is_empty(), "the shown list stays");
        assert_eq!(
            children
                .diagnostics(&root, &main_path(&root), normalized_key(MAIN))
                .len(),
            1
        );
    }
}

#[test]
fn output_of_a_killed_run_is_dropped() {
    let root = package("dropped");
    let mut children = Children::new();
    children
        .packages
        .insert(root.clone(), PackageChild::default());
    let mut out = Outbox::default();
    children.on_done(
        ChildDone {
            root: root.clone(),
            job: ChildJob::Constants,
            run: 4,
            outcome: ChildOutcome::Exited {
                status: None,
                stdout: document(Vec::new(), vec![value("limit", "1")])
                    .to_json()
                    .unwrap()
                    .into_bytes(),
                stderr: String::new(),
            },
        },
        Instant::now(),
        &mut out,
    );
    assert!(out.effects.is_empty());
    assert!(children.packages[&root].values.values.is_empty());
}

#[test]
fn progress_tokens_begin_and_end() {
    let mut progress = Progress::default();
    let (token, effects) = progress.begin(false, "Evaluating constants");
    assert!(token.is_none() && effects.is_empty());
    let (token, effects) = progress.begin(true, "Evaluating constants");
    let token = token.unwrap();
    assert_eq!(token.0, "cheby/child/0");
    assert!(matches!(
        effects.as_slice(),
        [
            Effect::Request {
                method: "window/workDoneProgress/create",
                ..
            },
            Effect::Notify {
                method: "$/progress",
                ..
            },
        ]
    ));
    let end = progress.end(token, "done");
    let [Effect::Notify { params, .. }] = end.as_slice() else {
        panic!("one notification");
    };
    assert_eq!(params["value"]["kind"], "end");
    assert_eq!(params["token"], "cheby/child/0");
}

#[test]
fn idle_is_owed_after_busy_or_save_and_paid_once() {
    let root = package("idle");
    let other = package("idle_other");
    let mut children = Children::new();
    assert!(
        children
            .idle_turn([(root.clone(), false), (other.clone(), false)])
            .is_empty(),
        "nothing happened, so nothing is owed"
    );
    assert_eq!(
        children.idle_turn([(root.clone(), true)]),
        Vec::<PathBuf>::new()
    );
    assert_eq!(
        children.idle_turn([(root.clone(), false), (other.clone(), false)]),
        vec![root.clone()]
    );
    assert_eq!(
        children.idle_turn([(root.clone(), false)]),
        Vec::<PathBuf>::new()
    );
    // A save that starts nothing still owes one (D-1443).
    children.owe_idle(&other);
    assert_eq!(
        children.idle_turn([(root.clone(), false), (other.clone(), false)]),
        vec![other.clone()]
    );
    // A due child keeps the package busy.
    children.packages.insert(
        root.clone(),
        PackageChild {
            constants: ChildState::Waiting {
                due: Instant::now(),
            },
            ..PackageChild::default()
        },
    );
    assert_eq!(
        children.idle_turn([(root.clone(), false)]),
        Vec::<PathBuf>::new()
    );
    children.end(&root, &mut Outbox::default());
    assert!(
        children.idle_owed.is_empty(),
        "an ended session owes nothing"
    );
}

#[test]
fn status_names_the_state_and_why_not() {
    let root = package("status");
    let fixture = write_fixture(&root, &[document(Vec::new(), vec![value("limit", "20")])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let now = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        now,
    );
    let status = children.status(&root, true, None, &env(&none), now);
    assert_eq!(status["constants"]["state"], "waiting");
    children.schedule(&env(&none), now, &mut Outbox::default());
    let status = children.status(&root, true, None, &env(&none), now);
    assert_eq!(status["constants"]["state"], "idle");
    assert_eq!(status["constants"]["values"], 1);
    assert!(
        status["constants"]["last"]["outcome"]
            .as_str()
            .unwrap()
            .starts_with("ok")
    );
    assert_eq!(status["notEvaluated"], Value::Null);
    let off = ChildEnv {
        evaluate: false,
        ..env(&none)
    };
    assert_eq!(
        children.status(&root, true, None, &off, now)["notEvaluated"],
        "`constants.evaluate` is off"
    );
    assert_eq!(
        children.status(&root, false, None, &env(&none), now)["notEvaluated"],
        "single-file mode evaluates no constants (D-1371)"
    );
}

/// The text of `src/util.cheby` in [`two_modules`].
const UTIL: &str = "pub const base: Int = 2\n";

/// An `ok` document over [`MAIN`] and `src/util.cheby` holding [`UTIL`],
/// with `values`.
fn two_modules(values: Vec<WireValue>) -> Envelope {
    let mut document = document(Vec::new(), values);
    document.files.push(FileHash {
        path: "src/util.cheby".to_owned(),
        hash: WireHash(normalized_key(UTIL)),
    });
    document
}

/// `name`'s value `text`, from a run that says its initializer reaches
/// the module files `modules` (D-1510).
fn reaching(name: &str, text: &str, modules: &[&str]) -> WireValue {
    WireValue {
        modules: Some(modules.iter().map(|path| (*path).to_owned()).collect()),
        ..value(name, text)
    }
}

/// Whether `name`'s value reads stale in `children`'s package at `root`
/// while the editor holds `editor`.
fn reads_stale(
    children: &Children,
    root: &Path,
    name: &str,
    editor: &BTreeMap<PathBuf, Hash128>,
) -> bool {
    match children
        .constants(root)
        .unwrap()
        .reading(&symbol(name), editor)
    {
        ConstReading::Value { stale, .. } => stale,
        other => panic!("`{name}` has no value: {other:?}"),
    }
}

/// D-1510: a value is current only while each module file its
/// initializer reaches has, in the editor, the text its run read, saved
/// or not. Another module it reaches, edited, makes it stale; a module
/// it does not reach, edited or added, does not. A module it reaches
/// that is gone makes it stale too, and so does its own file. The
/// `stale` of `cheby/status` follows (D-1505).
#[test]
fn a_value_is_current_only_while_the_modules_it_reaches_are() {
    let root = package("reached");
    let fixture = write_fixture(
        &root,
        &[two_modules(vec![
            reaching("limit", "20", &["src/main.cheby", "src/util.cheby"]),
            reaching("boom", "1", &["src/main.cheby"]),
        ])],
    );
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let now = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        now,
    );
    children.schedule(&env(&none), now, &mut Outbox::default());
    let util = root.join("src").join("util.cheby");
    let saved = BTreeMap::from([
        (main_path(&root), normalized_key(MAIN)),
        (util.clone(), normalized_key(UTIL)),
    ]);
    assert!(!reads_stale(&children, &root, "limit", &saved));
    assert!(!reads_stale(&children, &root, "boom", &saved));
    let stale_status = |editor: &BTreeMap<PathBuf, Hash128>| {
        children.status(&root, true, Some(editor), &env(&none), now)["constants"]["stale"].clone()
    };
    assert_eq!(stale_status(&saved), Value::Bool(false));
    // `util` edited and unsaved: `limit` reaches it, `boom` does not.
    let mut edited = saved.clone();
    edited.insert(util.clone(), normalized_key("pub const base: Int = 3\n"));
    assert!(reads_stale(&children, &root, "limit", &edited));
    assert!(!reads_stale(&children, &root, "boom", &edited));
    assert_eq!(stale_status(&edited), Value::Bool(true));
    // `util` gone.
    let mut removed = saved.clone();
    removed.remove(&util);
    assert!(reads_stale(&children, &root, "limit", &removed));
    assert!(!reads_stale(&children, &root, "boom", &removed));
    // A new module, which no value's run read, reaches neither.
    let mut added = saved.clone();
    added.insert(root.join("src").join("extra.cheby"), Hash128([5; 16]));
    assert!(!reads_stale(&children, &root, "limit", &added));
    assert!(!reads_stale(&children, &root, "boom", &added));
    assert_eq!(stale_status(&added), Value::Bool(false));
    // The declaring file edited.
    let mut own = saved.clone();
    own.insert(main_path(&root), Hash128([6; 16]));
    assert!(reads_stale(&children, &root, "boom", &own));
}

/// D-1510: a value that comes with no module list, as an older child
/// writes it, reaches every module of the package: any module edited,
/// gone or added makes it stale.
#[test]
fn a_value_with_no_modules_reaches_every_module() {
    let root = package("reached_all");
    let fixture = write_fixture(&root, &[two_modules(vec![value("boom", "1")])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let now = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        now,
    );
    children.schedule(&env(&none), now, &mut Outbox::default());
    let util = root.join("src").join("util.cheby");
    let saved = BTreeMap::from([
        (main_path(&root), normalized_key(MAIN)),
        (util.clone(), normalized_key(UTIL)),
    ]);
    assert!(!reads_stale(&children, &root, "boom", &saved));
    let mut edited = saved.clone();
    edited.insert(util.clone(), Hash128([3; 16]));
    assert!(reads_stale(&children, &root, "boom", &edited));
    let mut removed = saved.clone();
    removed.remove(&util);
    assert!(reads_stale(&children, &root, "boom", &removed));
    let mut added = saved.clone();
    added.insert(root.join("src").join("extra.cheby"), Hash128([5; 16]));
    assert!(reads_stale(&children, &root, "boom", &added));
}

/// D-1511: the evaluating mark follows the constants state: set while a
/// run is due, from the first clean report and from a save, and cleared
/// when the run ends, when evaluation is turned off, and with the session.
/// Meanwhile a constant with no value reads as being evaluated, and a
/// stale value says so.
#[test]
fn the_evaluating_mark_follows_the_constants_state() {
    let root = package("evaluating");
    let fixture = write_fixture(&root, &[document(Vec::new(), vec![value("limit", "20")])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let t0 = Instant::now();
    let saved = editor(&root, normalized_key(MAIN));
    let evaluating = |children: &Children| children.packages[&root].values.evaluating;
    write_main(&root, MAIN);
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        t0,
    );
    assert!(evaluating(&children), "the first run is due");
    let values = children.constants(&root).unwrap();
    assert_eq!(
        values.reading(&symbol("limit"), &saved),
        ConstReading::Evaluating
    );
    children.schedule(&env(&none), t0, &mut Outbox::default());
    assert!(!evaluating(&children), "the run ended");
    let values = children.constants(&root).unwrap();
    assert_eq!(
        values.reading(&symbol("boom"), &saved),
        ConstReading::NotYet
    );
    // A save of new text: due, and the value read from the old text says
    // that the new save is being evaluated.
    let next = format!("{MAIN}// next\n");
    write_main(&root, &next);
    let mut out = Outbox::default();
    children.on_save(&root, &[main_path(&root)], &env(&none), t0, &mut out);
    assert!(evaluating(&children));
    let values = children.constants(&root).unwrap();
    assert_eq!(
        values.reading(&symbol("limit"), &editor(&root, normalized_key(&next))),
        ConstReading::Value {
            text: "20".to_owned(),
            truncated: false,
            stale: true,
            evaluating: true,
        }
    );
    assert_eq!(
        values.reading(&symbol("boom"), &saved),
        ConstReading::Evaluating
    );
    // Turning evaluation off cancels the due run.
    let off = ChildEnv {
        evaluate: false,
        ..env(&none)
    };
    children.on_settings(&off, t0, &mut out);
    assert!(!evaluating(&children));
    children.on_settings(&env(&none), t0, &mut out);
    assert!(evaluating(&children), "turned on, a run is due again");
    children.end(&root, &mut out);
    assert!(children.constants(&root).is_none(), "the session ended");
}

/// D-1511: the mark comes before the texts of a constant with no value
/// after a failed or skipped run, or a run the server did not start;
/// a failed constant still reads as failed.
#[test]
fn evaluating_comes_before_the_texts_of_no_value() {
    let current = BTreeMap::new();
    for values in [
        ConstValues {
            stale: true,
            evaluating: true,
            ..ConstValues::default()
        },
        ConstValues {
            stale: true,
            front_errors: true,
            evaluating: true,
            ..ConstValues::default()
        },
        ConstValues {
            disk_errors: true,
            evaluating: true,
            ..ConstValues::default()
        },
    ] {
        assert_eq!(
            values.reading(&symbol("limit"), &current),
            ConstReading::Evaluating
        );
    }
    let failed = ConstValues {
        failed: BTreeMap::from([(symbol("boom"), Code::parse("E0075").unwrap())]),
        evaluating: true,
        ..ConstValues::default()
    };
    assert_eq!(
        failed.reading(&symbol("boom"), &current),
        ConstReading::Failed {
            code: Code::parse("E0075").unwrap()
        }
    );
}

/// Real processes: a shell script stands in for `cheby`.
#[cfg(unix)]
mod processes {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// An executable script at `root` with `body` after its first line.
    fn fake_cheby(root: &Path, body: &str) -> PathBuf {
        let path = root.join("fake-cheby");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn bare() -> Children {
        let mut children = Children::new();
        children.fixture = None;
        children
    }

    fn with_exe<'a>(exe: &'a Path, analyzing: &'a BTreeSet<PathBuf>) -> ChildEnv<'a> {
        ChildEnv {
            exe: Some(exe),
            ..env(analyzing)
        }
    }

    fn running_pid(children: &Children, root: &Path) -> u32 {
        match &children.packages[root].constants {
            ChildState::Running(running) => running.child.id(),
            other => panic!("not running: {other:?}"),
        }
    }

    fn alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    fn next_done(children: &Children) -> ChildDone {
        children
            .done()
            .recv_timeout(Duration::from_secs(60))
            .expect("the child's output ended in time")
    }

    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn child_output_is_read_and_kept() {
        let root = package("proc_output");
        let fixture = root.join("document.json");
        std::fs::write(
            &fixture,
            document(Vec::new(), vec![value("limit", "20")])
                .to_json()
                .unwrap(),
        )
        .unwrap();
        let exe = fake_cheby(
            &root,
            &format!(
                "printf '%s\\n' \"$@\" > args.txt\ncat '{}'",
                fixture.display()
            ),
        );
        let mut children = bare();
        let none = BTreeSet::new();
        let now = Instant::now();
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &with_exe(&exe, &none),
            now,
        );
        let mut out = Outbox::default();
        children.schedule(&with_exe(&exe, &none), now, &mut out);
        assert!(matches!(
            children.packages[&root].constants,
            ChildState::Running(_)
        ));
        assert!(
            out.effects.iter().any(|e| matches!(
                e,
                Effect::Request {
                    method: "window/workDoneProgress/create",
                    ..
                }
            )),
            "a run shows as progress (D-1415)"
        );
        let done = next_done(&children);
        let mut out = Outbox::default();
        children.on_done(done, Instant::now(), &mut out);
        assert!(!children.busy(&root));
        assert!(matches!(
            children
                .constants(&root)
                .unwrap()
                .reading(&symbol("limit"), &editor(&root, normalized_key(MAIN))),
            ConstReading::Value { .. }
        ));
        let args = std::fs::read_to_string(root.join("args.txt")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        let jobs = constants_jobs().to_string();
        let root_text = root.display().to_string();
        assert_eq!(
            args,
            [
                "-j",
                jobs.as_str(),
                "__backend",
                "--schema",
                "1",
                "--root",
                root_text.as_str(),
                "--want",
                "constants",
                "--max-text",
                "4096",
            ]
        );
    }

    /// `analysis.target` reaches the child as `--target`, so it checks
    /// what `cheby check --target` checks (D-1489).
    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn target_setting_reaches_the_child() {
        let root = package("proc_target");
        let fixture = root.join("document.json");
        std::fs::write(
            &fixture,
            document(Vec::new(), Vec::new()).to_json().unwrap(),
        )
        .unwrap();
        let exe = fake_cheby(
            &root,
            &format!(
                "printf '%s\\n' \"$@\" > args.txt\ncat '{}'",
                fixture.display()
            ),
        );
        let mut children = bare();
        let none = BTreeSet::new();
        let native = ChildEnv {
            target: Some(Target::Native),
            ..with_exe(&exe, &none)
        };
        let now = Instant::now();
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &native,
            now,
        );
        children.schedule(&native, now, &mut Outbox::default());
        let done = next_done(&children);
        children.on_done(done, Instant::now(), &mut Outbox::default());
        let args = std::fs::read_to_string(root.join("args.txt")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        let at = args.iter().position(|arg| *arg == "--target").unwrap();
        assert_eq!(args.get(at + 1), Some(&"native"), "{args:?}");
    }

    /// A save of unchanged text, such as an editor's echo of its own
    /// save, leaves the running child; one that changes the text on disk
    /// kills it (D-1411, D-1494).
    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn new_save_kills_the_running_child() {
        let root = package("proc_save");
        let exe = fake_cheby(&root, "exec sleep 600");
        let mut children = bare();
        let none = BTreeSet::new();
        let started = Instant::now();
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &with_exe(&exe, &none),
            started,
        );
        children.schedule(&with_exe(&exe, &none), started, &mut Outbox::default());
        let pid = running_pid(&children, &root);
        assert!(alive(pid));
        let mut out = Outbox::default();
        children.on_save(
            &root,
            &[main_path(&root)],
            &with_exe(&exe, &none),
            Instant::now(),
            &mut out,
        );
        assert_eq!(running_pid(&children, &root), pid, "unchanged text");
        assert!(alive(pid));
        assert!(out.effects.is_empty(), "no progress ends");
        write_main(&root, MAIN);
        children.on_save(
            &root,
            &[main_path(&root)],
            &with_exe(&exe, &none),
            Instant::now(),
            &mut out,
        );
        assert!(!alive(pid), "a new save kills the child");
        assert!(
            matches!(
                children.packages[&root].constants,
                ChildState::Waiting { .. }
            ),
            "and runs again after the coalescing wait"
        );
        // The killed run's output is dropped.
        let done = next_done(&children);
        let mut out = Outbox::default();
        children.on_done(done, Instant::now(), &mut out);
        assert!(out.effects.is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "no wait for the limit"
        );
    }

    /// D-1494, D-1502: a run killed by a new save gave no result, so its
    /// text is not the last run's. Saving that text again after a broken
    /// save killed the run makes a run due.
    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn saving_a_killed_runs_text_again_reruns() {
        let root = package("proc_killed_key");
        let exe = fake_cheby(&root, "exec sleep 600");
        let mut children = bare();
        let none = BTreeSet::new();
        let run = with_exe(&exe, &none);
        write_main(&root, MAIN);
        let t0 = Instant::now();
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &run,
            t0,
        );
        children.schedule(&run, t0, &mut Outbox::default());
        let pid = running_pid(&children, &root);
        // A save of a broken file kills the run, and the broken disk drops
        // the run it made due.
        write_main(&root, "pub const limit: Int =\n");
        let broken = ReportFacts {
            disk_unclean: true,
            ..ReportFacts::PACKAGE
        };
        children.on_report(&root, broken, &report(&root, false, 2), &run, t0);
        let mut out = Outbox::default();
        children.on_save(&root, &[main_path(&root)], &run, t0, &mut out);
        assert!(!alive(pid), "the broken save kills the run");
        let t1 = t0.checked_add(COALESCE).unwrap();
        children.schedule(&run, t1, &mut out);
        assert!(!children.busy(&root));
        // The undo, saved: the text of the killed run.
        write_main(&root, MAIN);
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 3),
            &run,
            t1,
        );
        children.on_save(&root, &[main_path(&root)], &run, t1, &mut out);
        assert!(children.busy(&root), "a run is due");
    }

    /// D-1502: a run killed by a change of `analysis.target` does not
    /// count as the last run either. The rerun the change made due waits
    /// for its analysis, a save of a broken file comes first, and the
    /// broken disk starts no run. Saving the killed run's text again makes
    /// a run due.
    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn saving_the_text_of_a_run_killed_by_a_target_change_reruns() {
        let root = package("proc_retarget_key");
        let exe = fake_cheby(&root, "exec sleep 600");
        let mut children = bare();
        let none = BTreeSet::new();
        let run = with_exe(&exe, &none);
        write_main(&root, MAIN);
        let t0 = Instant::now();
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &run,
            t0,
        );
        children.schedule(&run, t0, &mut Outbox::default());
        let pid = running_pid(&children, &root);
        let js = ChildEnv {
            target: Some(Target::Js),
            ..run
        };
        let mut out = Outbox::default();
        children.on_target(&js, t0, &mut out);
        assert!(!alive(pid), "the target change kills the run");
        assert!(children.busy(&root), "and makes a run due");
        // A save of a broken file before the rerun starts.
        write_main(&root, "pub const limit: Int =\n");
        children.on_save(&root, &[main_path(&root)], &js, t0, &mut out);
        let broken = ReportFacts {
            disk_unclean: true,
            ..ReportFacts::PACKAGE
        };
        children.on_report(&root, broken, &report(&root, false, 2), &js, t0);
        let t1 = t0.checked_add(COALESCE).unwrap();
        children.schedule(&js, t1, &mut out);
        assert!(!children.busy(&root), "the broken disk starts no run");
        // The undo, saved: the text of the killed run.
        write_main(&root, MAIN);
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 3),
            &js,
            t1,
        );
        children.on_save(&root, &[main_path(&root)], &js, t1, &mut out);
        assert!(children.busy(&root), "a run is due");
    }

    /// D-1500: a change of `analysis.target` reruns a run that was cut
    /// short, although the text on disk has not changed since, which
    /// keeps a save of that text from rerunning it (D-1493, D-1498).
    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn a_target_change_reruns_a_cut_run_over_unchanged_text() {
        let root = package("proc_retarget_cut");
        let exe = fake_cheby(&root, "exec sleep 600");
        let mut children = bare();
        let none = BTreeSet::new();
        let run = ChildEnv {
            timeout: Duration::from_secs(5),
            ..with_exe(&exe, &none)
        };
        write_main(&root, MAIN);
        let t0 = Instant::now();
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &run,
            t0,
        );
        children.schedule(&run, t0, &mut Outbox::default());
        let pid = running_pid(&children, &root);
        // Past its limit: cut short.
        let t1 = t0.checked_add(Duration::from_secs(6)).unwrap();
        let mut out = Outbox::default();
        children.schedule(&run, t1, &mut out);
        assert!(!alive(pid), "cut short");
        assert!(children.packages[&root].last_cut.is_some());
        // Saving the same text starts nothing (D-1493).
        children.on_save(&root, &[main_path(&root)], &run, t1, &mut out);
        assert!(!children.busy(&root), "an unchanged save reruns nothing");
        let js = ChildEnv {
            target: Some(Target::Js),
            ..run
        };
        children.on_target(&js, t1, &mut out);
        assert!(children.busy(&root), "the target change makes a run due");
        children.schedule(&js, t1, &mut out);
        let rerun = running_pid(&children, &root);
        assert_ne!(rerun, pid, "a new child runs over the unchanged text");
        drop(children);
        assert!(!alive(rerun));
    }

    /// D-1494: an unsaved new buffer is not on disk, so the child does not
    /// read it, and an echo of an unchanged file leaves the running child.
    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn an_unsaved_new_buffer_keeps_the_running_child() {
        let root = package("proc_new_buffer");
        let exe = fake_cheby(&root, "exec sleep 600");
        let mut children = bare();
        let none = BTreeSet::new();
        let run = with_exe(&exe, &none);
        write_main(&root, MAIN);
        let t0 = Instant::now();
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &run,
            t0,
        );
        children.schedule(&run, t0, &mut Outbox::default());
        let pid = running_pid(&children, &root);
        // `didOpen` of `src/fresh.cheby`, which is not on disk.
        let mut opened = report(&root, true, 2);
        let fresh = root.join("src").join("fresh.cheby");
        let main = opened.files.get(&main_path(&root)).cloned().unwrap();
        opened.files.insert(fresh, main);
        children.on_report(&root, ReportFacts::PACKAGE, &opened, &run, t0);
        let mut out = Outbox::default();
        children.on_save(&root, &[main_path(&root)], &run, t0, &mut out);
        assert_eq!(running_pid(&children, &root), pid, "unchanged text on disk");
        assert!(alive(pid));
        drop(children);
        assert!(!alive(pid));
    }

    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn time_limit_cuts_short_and_shows_once() {
        let root = package("proc_timeout");
        let exe = fake_cheby(&root, "exec sleep 600");
        let mut children = bare();
        let none = BTreeSet::new();
        let limit = ChildEnv {
            timeout: Duration::from_secs(5),
            ..with_exe(&exe, &none)
        };
        let t0 = Instant::now();
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &limit,
            t0,
        );
        let mut out = Outbox::default();
        children.schedule(&limit, t0, &mut out);
        let pid = running_pid(&children, &root);
        assert_eq!(children.deadline(), t0.checked_add(Duration::from_secs(5)));
        let late = t0.checked_add(Duration::from_secs(6)).unwrap();
        let mut out = Outbox::default();
        children.schedule(&limit, late, &mut out);
        assert!(!alive(pid));
        assert_eq!(shows(&out), 1);
        assert!(children.packages[&root].values.stale);
        assert_eq!(
            children
                .constants(&root)
                .unwrap()
                .reading(&symbol("limit"), &editor(&root, normalized_key(MAIN))),
            ConstReading::AfterFailure,
            "a run cut short leaves no value (D-1490)"
        );
        // The same key is not run again, so saving unchanged text does not
        // loop (A §11.5, D-1493).
        children.on_save(&root, &[main_path(&root)], &limit, late, &mut out);
        let later = late.checked_add(COALESCE).unwrap();
        children.schedule(&limit, later, &mut out);
        assert!(!children.busy(&root));
        // A new text on disk runs, and its cut gives no second message.
        write_main(&root, MAIN);
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 2),
            &limit,
            later,
        );
        children.on_save(&root, &[main_path(&root)], &limit, later, &mut out);
        let start = later.checked_add(COALESCE).unwrap();
        children.schedule(&limit, start, &mut out);
        let pid = running_pid(&children, &root);
        let mut out = Outbox::default();
        let end = start.checked_add(Duration::from_secs(6)).unwrap();
        children.schedule(&limit, end, &mut out);
        assert!(!alive(pid));
        assert_eq!(shows(&out), 0, "one message per session (D-1415)");
    }

    /// A run cut short is keyed by the text on disk that it read, not by
    /// the editor's buffers: a run that starts after an unsaved edit, and
    /// is cut short, runs again when that edit is saved (D-1412, D-1493).
    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn cut_run_reruns_when_an_unsaved_edit_is_saved() {
        let root = package("proc_cut_key");
        let exe = fake_cheby(&root, "exec sleep 600");
        let mut children = bare();
        let none = BTreeSet::new();
        let limit = ChildEnv {
            timeout: Duration::from_secs(5),
            ..with_exe(&exe, &none)
        };
        let t0 = Instant::now();
        write_main(&root, MAIN);
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &limit,
            t0,
        );
        // An unsaved edit before the run starts: the child reads disk.
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 2),
            &limit,
            t0,
        );
        children.schedule(&limit, t0, &mut Outbox::default());
        let pid = running_pid(&children, &root);
        let late = t0.checked_add(Duration::from_secs(6)).unwrap();
        children.schedule(&limit, late, &mut Outbox::default());
        assert!(!alive(pid));
        assert!(children.packages[&root].last_cut.is_some());
        // Saving the edit leaves the buffers' key as it was, but changes
        // the text on disk, so the run is due and starts.
        write_main(&root, &format!("{MAIN}// fixed\n"));
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 2),
            &limit,
            late,
        );
        let mut out = Outbox::default();
        children.on_save(&root, &[main_path(&root)], &limit, late, &mut out);
        let start = late.checked_add(COALESCE).unwrap();
        children.schedule(&limit, start, &mut out);
        let pid = running_pid(&children, &root);
        children.end(&root, &mut Outbox::default());
        assert!(!alive(pid));
    }

    /// D-1504: a save that fixes the files on disk clears the mark of
    /// D-1501 only when its run starts. Until then a constant with no
    /// value still reads as skipped for errors, and once the run over the
    /// saved files that check has started, while its child runs, it reads
    /// as not yet.
    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn a_fixing_save_clears_the_mark_when_its_run_starts() {
        let root = package("proc_unstarted_fix");
        let exe = fake_cheby(&root, "exec sleep 600");
        let mut children = bare();
        let none = BTreeSet::new();
        let exe_env = with_exe(&exe, &none);
        let fresh = |children: &Children| {
            children
                .constants(&root)
                .unwrap()
                .reading(&symbol("fresh"), &editor(&root, normalized_key(MAIN)))
        };
        let skipped = |children: &Children| {
            children.status(&root, true, None, &exe_env, Instant::now())["constants"]["skippedForErrors"]
                .as_bool()
                .unwrap()
        };
        let mut now = Instant::now();
        write_main(&root, MAIN);
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &exe_env,
            now,
        );
        children.schedule(&exe_env, now, &mut Outbox::default());
        let first = running_pid(&children, &root);
        // A broken save kills that run, and its due run does not start.
        write_main(&root, "pub const fresh: Int =\n");
        let broken = ReportFacts {
            disk_unclean: true,
            saved_errors: BTreeSet::from([main_path(&root)]),
            ..ReportFacts::PACKAGE
        };
        children.on_report(&root, broken, &report(&root, false, 2), &exe_env, now);
        children.on_save(
            &root,
            &[main_path(&root)],
            &exe_env,
            now,
            &mut Outbox::default(),
        );
        assert!(!alive(first));
        now = now.checked_add(COALESCE).unwrap();
        children.schedule(&exe_env, now, &mut Outbox::default());
        assert!(!children.busy(&root));
        assert_eq!(fresh(&children), ConstReading::SkippedForErrors);
        // The fix, saved: its run is due, and the mark stays until it
        // starts.
        write_main(&root, &format!("{MAIN}pub const fresh: Int = 1\n"));
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 3),
            &exe_env,
            now,
        );
        children.on_save(
            &root,
            &[main_path(&root)],
            &exe_env,
            now,
            &mut Outbox::default(),
        );
        assert!(children.busy(&root), "the run waits");
        // The mark stays, but a due run says so first (D-1511).
        assert_eq!(fresh(&children), ConstReading::Evaluating);
        assert!(skipped(&children));
        now = now.checked_add(COALESCE).unwrap();
        children.schedule(&exe_env, now, &mut Outbox::default());
        let pid = running_pid(&children, &root);
        // The mark is gone, and the running child is what hover names
        // (D-1511).
        assert_eq!(fresh(&children), ConstReading::Evaluating);
        assert!(!skipped(&children));
        children.end(&root, &mut Outbox::default());
        assert!(!alive(pid));
    }

    /// D-1504: a run that ends clears the mark of D-1501, whether it is
    /// cut short or gives a result. No path sets the mark while a child
    /// runs: a due run's start sets or clears it, and a save that changes
    /// the text kills the child first. So the test sets it by hand.
    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn a_run_that_ends_clears_the_mark() {
        let root = package("proc_unstarted_end");
        let fixture = root.join("document.json");
        std::fs::write(
            &fixture,
            document(Vec::new(), vec![value("limit", "20")])
                .to_json()
                .unwrap(),
        )
        .unwrap();
        let hold = root.join("hold");
        std::fs::write(&hold, "").unwrap();
        let exe = fake_cheby(
            &root,
            &format!(
                "[ -e '{}' ] && exec sleep 600\ncat '{}'",
                hold.display(),
                fixture.display()
            ),
        );
        let mut children = bare();
        let none = BTreeSet::new();
        let limit = ChildEnv {
            timeout: Duration::from_secs(5),
            ..with_exe(&exe, &none)
        };
        let mark = |children: &mut Children| {
            let files = BTreeMap::from([(main_path(&root), Hash128([9; 16]))]);
            let package = children.packages.get_mut(&root).unwrap();
            package.set_unstarted(Some(files));
            assert!(package.values.disk_errors);
        };
        let marked = |children: &Children| {
            let package = &children.packages[&root];
            package.unstarted.is_some() || package.values.disk_errors
        };
        let t0 = Instant::now();
        write_main(&root, &format!("{MAIN}// first\n"));
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &limit,
            t0,
        );
        children.schedule(&limit, t0, &mut Outbox::default());
        let pid = running_pid(&children, &root);
        mark(&mut children);
        // Cut short.
        let late = t0.checked_add(Duration::from_secs(6)).unwrap();
        children.schedule(&limit, late, &mut Outbox::default());
        assert!(!alive(pid));
        assert!(!marked(&children), "a cut run clears the mark");
        // A run that gives a result, over the text the document read.
        std::fs::remove_file(&hold).unwrap();
        write_main(&root, MAIN);
        children.on_save(
            &root,
            &[main_path(&root)],
            &limit,
            late,
            &mut Outbox::default(),
        );
        let start = late.checked_add(COALESCE).unwrap();
        children.schedule(&limit, start, &mut Outbox::default());
        running_pid(&children, &root);
        mark(&mut children);
        // The end of the cut run's output comes first, and is dropped.
        while children.busy(&root) {
            let done = next_done(&children);
            children.on_done(done, Instant::now(), &mut Outbox::default());
        }
        assert!(!marked(&children), "a run with a result clears the mark");
        assert_eq!(
            children
                .constants(&root)
                .unwrap()
                .reading(&symbol("limit"), &editor(&root, normalized_key(MAIN))),
            ConstReading::Value {
                text: "20".to_owned(),
                truncated: false,
                stale: false,
                evaluating: false,
            }
        );
    }

    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn output_past_the_cap_is_cut_short() {
        let root = package("proc_cap");
        let exe = fake_cheby(&root, "head -c 67108900 /dev/zero; exec sleep 600");
        let mut children = bare();
        let none = BTreeSet::new();
        let now = Instant::now();
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &with_exe(&exe, &none),
            now,
        );
        children.schedule(&with_exe(&exe, &none), now, &mut Outbox::default());
        let done = next_done(&children);
        assert!(matches!(done.outcome, ChildOutcome::TooLarge));
        let mut out = Outbox::default();
        children.on_done(done, Instant::now(), &mut out);
        assert_eq!(shows(&out), 1);
        assert!(!children.busy(&root));
        assert!(
            children.packages[&root]
                .last
                .as_ref()
                .unwrap()
                .outcome
                .contains("64 MiB")
        );
    }

    /// Whether `out` ends a `$/progress`.
    fn ends_progress(out: &Outbox) -> bool {
        out.effects.iter().any(|e| {
            matches!(e, Effect::Notify { method: "$/progress", params }
                if params["value"]["kind"] == "end")
        })
    }

    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn evaluate_off_and_session_end_kill_and_end_progress() {
        let root = package("proc_off");
        let exe = fake_cheby(&root, "exec sleep 600");
        let mut children = bare();
        let none = BTreeSet::new();
        let now = Instant::now();
        let on = with_exe(&exe, &none);
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &on,
            now,
        );
        children.schedule(&on, now, &mut Outbox::default());
        let pid = running_pid(&children, &root);
        // A new limit reaches the running child.
        let short = ChildEnv {
            timeout: Duration::from_secs(1),
            ..on
        };
        children.on_settings(&short, now, &mut Outbox::default());
        assert_eq!(children.deadline(), now.checked_add(Duration::from_secs(1)));
        let off = ChildEnv {
            evaluate: false,
            ..on
        };
        let mut out = Outbox::default();
        children.on_settings(&off, now, &mut out);
        assert!(!alive(pid), "turning evaluation off kills the child");
        assert!(ends_progress(&out));
        assert!(!children.busy(&root));
        children.on_settings(&on, now, &mut Outbox::default());
        children.schedule(&on, now, &mut Outbox::default());
        let pid = running_pid(&children, &root);
        let mut out = Outbox::default();
        children.end(&root, &mut out);
        assert!(!alive(pid), "a session's end kills the child");
        assert!(ends_progress(&out), "and ends its progress");
    }

    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn no_child_outlives_the_server() {
        let root = package("proc_drop");
        let exe = fake_cheby(&root, "exec sleep 600");
        let mut children = bare();
        let none = BTreeSet::new();
        let now = Instant::now();
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &with_exe(&exe, &none),
            now,
        );
        children.schedule(&with_exe(&exe, &none), now, &mut Outbox::default());
        let pid = running_pid(&children, &root);
        drop(children);
        assert!(!alive(pid));
    }

    /// D-1511: the evaluating mark holds while a child runs, through a
    /// save that kills it and makes a new run due, and goes when a run
    /// is cut short by its time limit.
    #[test]
    #[cfg_attr(windows, ignore = "runs processes (D-545)")]
    fn evaluating_holds_while_a_child_runs() {
        let root = package("proc_evaluating");
        let exe = fake_cheby(&root, "exec sleep 600");
        let mut children = bare();
        let none = BTreeSet::new();
        let t0 = Instant::now();
        let evaluating = |children: &Children| children.packages[&root].values.evaluating;
        write_main(&root, MAIN);
        children.on_report(
            &root,
            ReportFacts::PACKAGE,
            &report(&root, true, 1),
            &with_exe(&exe, &none),
            t0,
        );
        children.schedule(&with_exe(&exe, &none), t0, &mut Outbox::default());
        let first = running_pid(&children, &root);
        assert!(evaluating(&children), "running");
        write_main(&root, &format!("{MAIN}// next\n"));
        let mut out = Outbox::default();
        children.on_save(
            &root,
            &[main_path(&root)],
            &with_exe(&exe, &none),
            t0,
            &mut out,
        );
        assert!(!alive(first), "the save killed the child");
        assert!(evaluating(&children), "and a run is due");
        let t1 = t0.checked_add(COALESCE).unwrap();
        children.schedule(&with_exe(&exe, &none), t1, &mut out);
        let second = running_pid(&children, &root);
        assert!(evaluating(&children), "running again");
        let late = t1.checked_add(Duration::from_secs(61)).unwrap();
        children.schedule(&with_exe(&exe, &none), late, &mut out);
        assert!(!alive(second), "cut short");
        assert!(!evaluating(&children), "nothing runs or is due");
        // The killed runs' output is dropped.
        for _ in 0..2 {
            let done = next_done(&children);
            children.on_done(done, Instant::now(), &mut out);
        }
        assert!(!evaluating(&children));
    }
}

#[test]
fn broken_file_stops_a_due_run() {
    let root = package("broken");
    let fixture = write_fixture(&root, &[document(Vec::new(), vec![value("limit", "20")])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let t0 = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        t0,
    );
    children.schedule(&env(&none), t0, &mut Outbox::default());
    // A buffer turns broken, and no report follows (plan §1.2).
    children.on_broken(&root, &[]);
    let mut out = Outbox::default();
    write_main(&root, MAIN);
    children.on_save(&root, &[main_path(&root)], &env(&none), t0, &mut out);
    let t1 = t0.checked_add(COALESCE).unwrap();
    children.schedule(&env(&none), t1, &mut out);
    assert!(children.packages[&root].pending);
    assert_eq!(children.next_run, 1, "no second run");
    // A saved file that is broken waits for the next save.
    children.on_broken(&root, &[main_path(&root)]);
    assert!(!children.packages[&root].pending);
}

/// D-1497: a constant with no value after a run the child skipped
/// because the saved files have front-end errors reads as skipped for
/// errors, not as failed (D-1490). A later run that fails reads as failed
/// again, and one that gives values clears the mark.
#[test]
fn a_run_skipped_for_errors_on_disk_reads_as_not_yet() {
    let root = package("skipped_for_errors");
    let mut skipped = document(Vec::new(), Vec::new());
    skipped.front = Some(Front::Errors);
    skipped.sections.constants = Some(Section::Skipped {
        reason: "front-end errors".to_owned(),
    });
    let refused = Envelope {
        schema: crate::child::wire::SCHEMA,
        status: Status::InternalError {
            message: "boom".to_owned(),
        },
        manifest: None,
        files: Vec::new(),
        front: None,
        sections: Sections::default(),
    };
    let fixture = write_fixture(
        &root,
        &[
            skipped.clone(),
            refused,
            skipped,
            document(Vec::new(), vec![value("limit", "20")]),
        ],
    );
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let mut now = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        now,
    );
    children.schedule(&env(&none), now, &mut Outbox::default());
    let reading = |children: &Children, name: &str| {
        children
            .constants(&root)
            .unwrap()
            .reading(&symbol(name), &editor(&root, normalized_key(MAIN)))
    };
    assert_eq!(reading(&children, "limit"), ConstReading::SkippedForErrors);
    let expected = [
        ConstReading::AfterFailure,
        ConstReading::SkippedForErrors,
        ConstReading::Value {
            text: "20".to_owned(),
            truncated: false,
            stale: false,
            evaluating: false,
        },
    ];
    for (n, want) in expected.into_iter().enumerate() {
        write_main(&root, &format!("{MAIN}// run {n}\n"));
        children.on_save(
            &root,
            &[main_path(&root)],
            &env(&none),
            now,
            &mut Outbox::default(),
        );
        now = now.checked_add(COALESCE).unwrap();
        children.schedule(&env(&none), now, &mut Outbox::default());
        assert_eq!(reading(&children, "limit"), want, "run {n}");
    }
}

/// D-1498: a change of `analysis.target` makes a constants run due at
/// once over a clean report, as the first clean report does, kills a
/// child that checks for the old target, and lifts a cut made under it.
/// Over an unclean report the next clean report starts the run.
#[test]
fn a_target_change_reruns_the_constants() {
    let root = package("retarget");
    let fixture = write_fixture(&root, &[document(Vec::new(), vec![value("limit", "20")])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let t0 = Instant::now();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        t0,
    );
    children.schedule(&env(&none), t0, &mut Outbox::default());
    assert_eq!(children.next_run, 1);
    assert!(!children.busy(&root));
    // A cut under the old target, for the text on disk now.
    let key = disk_key(&children.packages[&root].read);
    children.packages.get_mut(&root).unwrap().last_cut = Some(Cut { key, limit: None });
    let js = ChildEnv {
        target: Some(Target::Js),
        ..env(&none)
    };
    let t1 = t0.checked_add(Duration::from_secs(1)).unwrap();
    let mut out = Outbox::default();
    children.on_settings(&js, t1, &mut out);
    assert!(!children.busy(&root), "other settings rerun nothing");
    children.on_target(&js, t1, &mut out);
    assert!(children.busy(&root), "due at once");
    assert_eq!(children.deadline(), Some(t1));
    assert_eq!(children.packages[&root].last_cut, None);
    children.schedule(&js, t1, &mut out);
    assert_eq!(children.next_run, 2, "the run started");
    // An unclean report: the next clean one starts the run.
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, false, 2),
        &js,
        t1,
    );
    children.on_target(&env(&none), t1, &mut out);
    assert!(!children.busy(&root));
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 3),
        &env(&none),
        t1,
    );
    assert!(children.busy(&root));
    // With `constants.evaluate` off nothing runs.
    let off = ChildEnv {
        evaluate: false,
        ..env(&none)
    };
    children.schedule(&env(&none), t1, &mut out);
    children.on_target(&off, t1, &mut out);
    assert!(!children.busy(&root));
}

/// D-1501: a save that leaves front-end errors on disk starts no run, so
/// a constant with no value reads as skipped for errors, as after a run
/// the child skipped (D-1497), and `cheby/status` says
/// `skippedForErrors`. A failed constant still reads as failed. When the
/// mark is set and cleared is D-1504's: only a save sets it, so a session
/// opened over a broken disk reads as not yet until a save. Saving the
/// last run's text back, with no run waiting, makes no run due and
/// brings back that run's reading (D-1490). A due run over saved files
/// that check ends the mark too, also while an unsaved buffer keeps the
/// report unclean, and so does a run.
#[test]
fn a_save_that_leaves_errors_on_disk_reads_as_skipped() {
    let root = package("unstarted_for_errors");
    let fixture = write_fixture(
        &root,
        &[
            document(vec![panicked("boom")], Vec::new()),
            document(Vec::new(), vec![value("limit", "20")]),
        ],
    );
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let reading = |children: &Children, name: &str| {
        children
            .constants(&root)
            .unwrap()
            .reading(&symbol(name), &editor(&root, normalized_key(MAIN)))
    };
    let skipped = |children: &Children| {
        children.status(&root, true, None, &env(&none), Instant::now())["constants"]["skippedForErrors"]
            .as_bool()
            .unwrap()
    };
    let broken = ReportFacts {
        disk_unclean: true,
        saved_errors: BTreeSet::from([main_path(&root)]),
        ..ReportFacts::PACKAGE
    };
    // Opened over a broken disk, never saved: not yet.
    write_main(&root, "pub const limit: Int =\n");
    let mut now = Instant::now();
    let opened = report(&root, false, 1);
    children.on_report(&root, broken.clone(), &opened, &env(&none), now);
    children.schedule(&env(&none), now, &mut Outbox::default());
    assert_eq!(reading(&children, "limit"), ConstReading::NotYet);
    assert!(!skipped(&children));
    // The fix, saved: the run fails at `boom`, and `limit` has no value.
    write_main(&root, MAIN);
    let save = |children: &mut Children, facts, clean, inputs, now: &mut Instant| {
        children.on_report(
            &root,
            facts,
            &report(&root, clean, inputs),
            &env(&none),
            *now,
        );
        children.on_save(
            &root,
            &[main_path(&root)],
            &env(&none),
            *now,
            &mut Outbox::default(),
        );
        *now = now.checked_add(COALESCE).unwrap();
        children.schedule(&env(&none), *now, &mut Outbox::default());
    };
    save(&mut children, ReportFacts::PACKAGE, true, 2, &mut now);
    assert_eq!(children.next_run, 1);
    assert_eq!(reading(&children, "limit"), ConstReading::AfterFailure);
    // A save of a broken file starts no run, and hover says why.
    write_main(&root, "pub const limit: Int =\n");
    save(&mut children, broken.clone(), false, 3, &mut now);
    assert_eq!(children.next_run, 1, "no run");
    assert!(!children.busy(&root));
    assert_eq!(reading(&children, "limit"), ConstReading::SkippedForErrors);
    assert_eq!(
        reading(&children, "boom"),
        ConstReading::Failed {
            code: Code::parse("E0075").unwrap()
        }
    );
    assert!(skipped(&children));
    // The undo, saved: the failed run's text, so no run is due, and its
    // reading is back.
    write_main(&root, MAIN);
    save(&mut children, ReportFacts::PACKAGE, true, 4, &mut now);
    assert_eq!(children.next_run, 1, "no run");
    assert_eq!(reading(&children, "limit"), ConstReading::AfterFailure);
    assert!(!skipped(&children));
    // Broken again, then a fix saved while another buffer, unsaved, does
    // not check: the saved files check, so the mark goes, and the run
    // waits for that buffer.
    write_main(&root, "pub const limit: Int =\n");
    save(&mut children, broken, false, 5, &mut now);
    assert_eq!(reading(&children, "limit"), ConstReading::SkippedForErrors);
    write_main(&root, &format!("{MAIN}// fixed\n"));
    save(&mut children, ReportFacts::PACKAGE, false, 6, &mut now);
    assert_eq!(children.next_run, 1, "no run");
    assert!(children.packages[&root].pending);
    assert_eq!(reading(&children, "limit"), ConstReading::AfterFailure);
    assert!(!skipped(&children));
    // The buffer fixed: its clean report starts the run, which gives the
    // value.
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 7),
        &env(&none),
        now,
    );
    children.schedule(&env(&none), now, &mut Outbox::default());
    assert_eq!(children.next_run, 2);
    assert_eq!(
        reading(&children, "limit"),
        ConstReading::Value {
            text: "20".to_owned(),
            truncated: false,
            stale: false,
            evaluating: false,
        }
    );
    assert!(!skipped(&children));
}

/// D-1503: while a save has left front-end errors on disk and so started
/// no run (D-1501), every value reads as stale, also one whose declaring
/// file has not changed, as after a run the child skipped (D-1497). A
/// save that fixes the files starts a run, which clears the mark, and
/// the value is current again (D-1504). `cheby/status` says `stale` exactly
/// when hover does (D-1505).
#[test]
fn values_read_as_stale_while_errors_on_disk_start_no_run() {
    let root = package("unstarted_stale");
    let fixture = write_fixture(&root, &[document(Vec::new(), vec![value("limit", "20")])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let limit = |children: &Children| {
        children
            .constants(&root)
            .unwrap()
            .reading(&symbol("limit"), &editor(&root, normalized_key(MAIN)))
    };
    let current = |stale| ConstReading::Value {
        text: "20".to_owned(),
        truncated: false,
        stale,
        evaluating: false,
    };
    let status_stale = |children: &Children| {
        children.status(&root, true, None, &env(&none), Instant::now())["constants"]["stale"]
            .clone()
    };
    let util = root.join("src").join("util.cheby");
    let mut now = Instant::now();
    write_main(&root, MAIN);
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        now,
    );
    children.schedule(&env(&none), now, &mut Outbox::default());
    assert_eq!(limit(&children), current(false));
    // A broken save of another file: `src/main.cheby` is unchanged, but
    // its value may depend on the broken file.
    std::fs::write(&util, "pub const base: Int =\n").unwrap();
    let broken = ReportFacts {
        disk_unclean: true,
        saved_errors: BTreeSet::from([util.clone()]),
        ..ReportFacts::PACKAGE
    };
    children.on_report(&root, broken, &report(&root, false, 2), &env(&none), now);
    children.on_save(
        &root,
        std::slice::from_ref(&util),
        &env(&none),
        now,
        &mut Outbox::default(),
    );
    now = now.checked_add(COALESCE).unwrap();
    children.schedule(&env(&none), now, &mut Outbox::default());
    assert_eq!(children.next_run, 1, "no run");
    assert_eq!(limit(&children), current(true));
    assert_eq!(status_stale(&children), true, "D-1505");
    // The fix, saved: its run gives the value, current again.
    std::fs::write(&util, "pub const base: Int = 1\n").unwrap();
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 3),
        &env(&none),
        now,
    );
    children.on_save(&root, &[util], &env(&none), now, &mut Outbox::default());
    now = now.checked_add(COALESCE).unwrap();
    children.schedule(&env(&none), now, &mut Outbox::default());
    assert_eq!(children.next_run, 2);
    assert_eq!(limit(&children), current(false));
    assert_eq!(status_stale(&children), false, "D-1505");
}

/// A report like [`report`] whose `src/main.cheby` the analysis read as
/// `text`, with no diagnostic.
fn report_reading(root: &Path, clean: bool, inputs: u8, text: &str) -> Report {
    let mut report = report(root, clean, inputs);
    let file = FileReport {
        text: Arc::from(text),
        origin: TextOrigin::Editor,
        version: Some(i32::from(inputs)),
        hash: exact_key(text),
        diagnostics: Vec::new(),
    };
    report.files.insert(main_path(root), Arc::new(file));
    report
}

/// D-1501, D-1504: the server cannot tell from an unsaved buffer whether
/// its saved text checks. After a broken save of `src/main.cheby` and an
/// unsaved edit that leaves it broken, a save of another file finds the
/// report unclean only through that buffer, but its saved text is still
/// the broken one, so the mark stays (D-1504). Once the analysis reads
/// that same saved text without errors, as when another file caused
/// them, the saved files check, so the mark goes at the next due run.
#[test]
fn a_broken_file_with_unsaved_edits_keeps_the_mark() {
    let root = package("unstarted_dirty_buffer");
    let fixture = write_fixture(&root, &[document(Vec::new(), vec![value("limit", "20")])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let fresh = |children: &Children| {
        children
            .constants(&root)
            .unwrap()
            .reading(&symbol("fresh"), &editor(&root, normalized_key(MAIN)))
    };
    let skipped = |children: &Children| {
        children.status(&root, true, None, &env(&none), Instant::now())["constants"]["skippedForErrors"]
            .as_bool()
            .unwrap()
    };
    let util = root.join("src").join("util.cheby");
    let mut now = Instant::now();
    write_main(&root, MAIN);
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        now,
    );
    children.schedule(&env(&none), now, &mut Outbox::default());
    assert_eq!(children.next_run, 1, "the first clean report ran");
    // A broken save: no run, and the mark.
    let broken_text = "pub const limit: Int = 20\npub const fresh: Int = \"no\"\n";
    write_main(&root, broken_text);
    let broken = ReportFacts {
        disk_unclean: true,
        saved_errors: BTreeSet::from([main_path(&root)]),
        ..ReportFacts::PACKAGE
    };
    children.on_report(&root, broken, &report(&root, false, 2), &env(&none), now);
    children.on_save(
        &root,
        &[main_path(&root)],
        &env(&none),
        now,
        &mut Outbox::default(),
    );
    now = now.checked_add(COALESCE).unwrap();
    children.schedule(&env(&none), now, &mut Outbox::default());
    assert_eq!(fresh(&children), ConstReading::SkippedForErrors);
    // Edited again, still broken, not saved: the report is unclean only
    // through that buffer.
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, false, 3),
        &env(&none),
        now,
    );
    assert_eq!(fresh(&children), ConstReading::SkippedForErrors);
    // A save of another file: its run finds the report unclean through
    // the buffer only, but the buffer's saved text is the broken one.
    std::fs::write(&util, "pub const base: Int = 1\n").unwrap();
    children.on_save(
        &root,
        std::slice::from_ref(&util),
        &env(&none),
        now,
        &mut Outbox::default(),
    );
    now = now.checked_add(COALESCE).unwrap();
    children.schedule(&env(&none), now, &mut Outbox::default());
    assert_eq!(children.next_run, 1, "no run");
    assert!(children.packages[&root].pending);
    assert_eq!(fresh(&children), ConstReading::SkippedForErrors);
    assert!(skipped(&children));
    // The analysis reads the saved text of `main` without errors, and
    // another buffer keeps the report unclean: the saved text of `main`
    // checks, so the mark goes at the next due run.
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report_reading(&root, false, 4, broken_text),
        &env(&none),
        now,
    );
    std::fs::write(&util, "pub const base: Int = 2\n").unwrap();
    children.on_save(&root, &[util], &env(&none), now, &mut Outbox::default());
    now = now.checked_add(COALESCE).unwrap();
    children.schedule(&env(&none), now, &mut Outbox::default());
    assert_eq!(children.next_run, 1, "no run");
    assert_eq!(fresh(&children), ConstReading::NotYet);
    assert!(!skipped(&children));
}

/// D-1501, D-1504: a second broken save, and its watched echo, while the
/// run it made due waits, keep the mark of the first: the saved files
/// still do not check, and the waiting run's start decides (D-1504).
#[test]
fn an_echo_of_a_second_broken_save_keeps_the_mark() {
    let root = package("unstarted_echo");
    let fixture = write_fixture(&root, &[document(Vec::new(), vec![value("limit", "20")])]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let fresh = |children: &Children| {
        children
            .constants(&root)
            .unwrap()
            .reading(&symbol("fresh"), &editor(&root, normalized_key(MAIN)))
    };
    let mut now = Instant::now();
    write_main(&root, MAIN);
    children.on_report(
        &root,
        ReportFacts::PACKAGE,
        &report(&root, true, 1),
        &env(&none),
        now,
    );
    children.schedule(&env(&none), now, &mut Outbox::default());
    let broken = ReportFacts {
        disk_unclean: true,
        saved_errors: BTreeSet::from([main_path(&root)]),
        ..ReportFacts::PACKAGE
    };
    let save = |children: &mut Children, text: &str, inputs, now: Instant| {
        write_main(&root, text);
        children.on_report(
            &root,
            broken.clone(),
            &report(&root, false, inputs),
            &env(&none),
            now,
        );
        children.on_save(
            &root,
            &[main_path(&root)],
            &env(&none),
            now,
            &mut Outbox::default(),
        );
    };
    save(&mut children, "pub const fresh: Int =\n", 2, now);
    now = now.checked_add(COALESCE).unwrap();
    children.schedule(&env(&none), now, &mut Outbox::default());
    assert_eq!(fresh(&children), ConstReading::SkippedForErrors);
    // A second broken save, then its echo, before the run it made due.
    save(&mut children, "pub const fresh: Int = \"no\"\n", 3, now);
    let echo = now.checked_add(Duration::from_millis(50)).unwrap();
    children.on_save(
        &root,
        &[main_path(&root)],
        &env(&none),
        echo,
        &mut Outbox::default(),
    );
    assert!(children.busy(&root), "the run waits");
    // A due run says so before the mark does (D-1511).
    assert_eq!(fresh(&children), ConstReading::Evaluating);
    now = now.checked_add(COALESCE).unwrap();
    children.schedule(&env(&none), now, &mut Outbox::default());
    assert_eq!(children.next_run, 1, "no run");
    assert_eq!(fresh(&children), ConstReading::SkippedForErrors);
}

/// D-1501: a package with no constants has no run to miss, so a broken
/// save sets no mark, and `cheby/status` does not say `skippedForErrors`
/// next to "the package has no constants".
#[test]
fn a_package_with_no_constants_gets_no_mark() {
    let root = package("unstarted_no_constants");
    let fixture = write_fixture(&root, &[document(Vec::new(), Vec::new())]);
    let mut children = with_fixture(fixture);
    let none = BTreeSet::new();
    let no_constants = ReportFacts {
        has_constants: false,
        ..ReportFacts::PACKAGE
    };
    let mut now = Instant::now();
    write_main(&root, "fn main() {}\n");
    children.on_report(
        &root,
        no_constants.clone(),
        &report(&root, true, 1),
        &env(&none),
        now,
    );
    children.schedule(&env(&none), now, &mut Outbox::default());
    write_main(&root, "fn main( {}\n");
    let broken = ReportFacts {
        disk_unclean: true,
        saved_errors: BTreeSet::from([main_path(&root)]),
        ..no_constants
    };
    children.on_report(&root, broken, &report(&root, false, 2), &env(&none), now);
    children.on_save(
        &root,
        &[main_path(&root)],
        &env(&none),
        now,
        &mut Outbox::default(),
    );
    now = now.checked_add(COALESCE).unwrap();
    children.schedule(&env(&none), now, &mut Outbox::default());
    let status = children.status(&root, true, None, &env(&none), now);
    assert_eq!(status["notEvaluated"], "the package has no constants");
    assert_eq!(status["constants"]["skippedForErrors"], false);
    assert!(!children.packages[&root].values.disk_errors);
}
