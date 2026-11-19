//! Tests of the analysis API: `verify` (plan §1.6, D-1389, D-1393).
#![expect(
    clippy::wildcard_enum_match_arm,
    reason = "a test matches only the variants it checks"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::{Severity, TextRange, TextSize, codes};
use cheby_iface::Globals;
use cheby_package::{MANIFEST_FILE, ModulePath};

use super::{
    Overlay, OverlayFile, PackageKind, PlainEdit, Session, SessionConfig, Snapshot, VerifyOutcome,
    VerifyRequest, verify,
};

const MANIFEST: &str = "name = \"app\"\nversion = \"0.1.0\"\n";

/// A package in a fresh temporary directory, removed on drop.
struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A package `name` with these files, `cheby.toml` included.
fn package(name: &str, files: &[(&str, &str)]) -> Temp {
    let manifest = [(MANIFEST_FILE, MANIFEST)];
    directory(name, &[&manifest, files].concat())
}

/// A fresh directory `name` with these files and nothing else.
fn directory(name: &str, files: &[(&str, &str)]) -> Temp {
    let dir = std::env::temp_dir().join(format!("cheby_verify_{}_{name}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    for (path, text) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    Temp(std::path::absolute(dir).unwrap())
}

/// The snapshot of a live run over `root` with these open documents.
fn snapshot(root: &Path, open: &[(&str, &str)]) -> Arc<Snapshot> {
    let overlay = Overlay {
        files: open
            .iter()
            .zip(1_u64..)
            .map(|(&(path, text), stamp)| {
                let file = OverlayFile {
                    text: Arc::from(text),
                    version: 1,
                    stamp,
                };
                (root.join(path), file)
            })
            .collect(),
    };
    let mut session = Session::new(SessionConfig {
        root: root.to_path_buf(),
        kind: PackageKind::Package,
        target: None,
        globals: Arc::new(Globals::new()),
    });
    let live = session.analyze_live(&overlay).unwrap();
    assert_eq!(live.failed, None);
    live.snapshot.unwrap()
}

/// The snapshot of a single-file session over `file` (D-1371), open with
/// `text`. `strict` when no package is above the file.
fn single_snapshot(file: &Path, strict: bool, text: &str) -> Arc<Snapshot> {
    let open = OverlayFile {
        text: Arc::from(text),
        version: 1,
        stamp: 1,
    };
    let overlay = Overlay {
        files: [(file.to_path_buf(), open)].into(),
    };
    let mut session = Session::new(SessionConfig {
        root: file.parent().unwrap().to_path_buf(),
        kind: PackageKind::SingleFile {
            file: file.to_path_buf(),
            strict,
        },
        target: None,
        globals: Arc::new(Globals::new()),
    });
    let live = session.analyze_live(&overlay).unwrap();
    assert_eq!(live.failed, None);
    let snapshot = live.snapshot.unwrap();
    assert!(snapshot.is_single_file());
    snapshot
}

/// The edit that replaces the `n`th `old` (from 0) of `text`, the file at
/// `path`, with `new`.
fn replace(path: &Path, text: &str, old: &str, n: usize, new: &str) -> PlainEdit {
    let (at, _) = text.match_indices(old).nth(n).unwrap();
    let start = TextSize::try_from(at).unwrap();
    let end = TextSize::try_from(at.checked_add(old.len()).unwrap()).unwrap();
    PlainEdit {
        path: path.to_path_buf(),
        range: TextRange::new(start, end),
        replacement: new.to_owned(),
    }
}

/// The edits that replace every `old` of `text` with `new`.
fn replace_all(path: &Path, text: &str, old: &str, new: &str) -> Vec<PlainEdit> {
    (0..text.matches(old).count())
        .map(|n| replace(path, text, old, n, new))
        .collect()
}

/// A request of these edits alone.
const fn edits(edits: &[PlainEdit]) -> VerifyRequest<'_> {
    VerifyRequest {
        edits,
        moves: &[],
        renamed: &[],
        hides: false,
    }
}

/// A rename's request: these edits, `old` read as `new`, and the check
/// that no error goes away (D-1668).
const fn rename<'a>(edits: &'a [PlainEdit], pair: &'a [(&'a str, &'a str)]) -> VerifyRequest<'a> {
    VerifyRequest {
        edits,
        moves: &[],
        renamed: pair,
        hides: true,
    }
}

/// The codes and messages of an outcome's hidden errors.
fn hidden(outcome: &VerifyOutcome) -> Vec<(String, Option<PathBuf>, String)> {
    match outcome {
        VerifyOutcome::Hides(entries) => entries
            .iter()
            .map(|(_, (code, path, message))| (code.to_string(), path.clone(), message.clone()))
            .collect(),
        other => panic!("expected hidden errors, got {other:?}"),
    }
}

/// The places of an outcome's hidden errors.
fn hidden_places(outcome: &VerifyOutcome) -> Vec<Option<String>> {
    match outcome {
        VerifyOutcome::Hides(entries) => entries.iter().map(|(at, _)| at.clone()).collect(),
        other => panic!("expected hidden errors, got {other:?}"),
    }
}

/// The codes and messages of an outcome's new diagnostics.
fn introduced(outcome: &VerifyOutcome) -> Vec<(String, String)> {
    match outcome {
        VerifyOutcome::Introduces(diags) => diags
            .iter()
            .map(|i| (i.diagnostic.code.to_string(), i.diagnostic.message.clone()))
            .collect(),
        other => panic!("expected new diagnostics, got {other:?}"),
    }
}

/// S-1's package: renaming the constructor `Foo` to `Some` captures the
/// prelude `Some` of `let x = Some(1)` with only a W0007, and changes what
/// the program prints. `verify` refuses on that warning.
const CAPTURE: &str = "//! Root.\n\nimport std::io\n\npub type Mine {\n  Foo(Int),\n}\n\n\
                       pub fn show(m: Mine) -> String {\n  \"custom\"\n}\n\n\
                       fn main() {\n  let x = Some(1)\n  io::println(\"{x}\")\n}\n";

/// S-1, D-1389: a rename that hides a prelude name is refused, though it
/// adds no error.
#[test]
fn verify_refuses_prelude_capture() {
    let temp = package("capture", &[("src/main.cheby", CAPTURE)]);
    let snapshot = snapshot(&temp.0, &[]);
    let main = temp.0.join("src/main.cheby");
    let edit = [replace(&main, CAPTURE, "Foo", 0, "Some")];
    let request = VerifyRequest {
        renamed: &[("Foo", "Some")],
        ..edits(&edit)
    };
    let outcome = verify(&snapshot, &request).unwrap();
    let found = introduced(&outcome);
    assert!(found.iter().any(|(code, _)| code == "W0007"), "{found:?}");
    let VerifyOutcome::Introduces(diags) = &outcome else {
        panic!("{outcome:?}")
    };
    let w0007 = diags
        .iter()
        .find(|i| i.diagnostic.code == codes::HIDDEN_PRELUDE_NAME)
        .unwrap();
    // The range lies in the edited text: the new name, which `at` places
    // as `cheby check` does, in the file `path` names (D-1671, D-1709).
    let edited = CAPTURE.replacen("Foo", "Some", 1);
    let range = w0007.diagnostic.range.unwrap();
    assert_eq!(range.slice(&edited), Some("Some"));
    assert_eq!(w0007.at, "src/main.cheby:6:3");
    assert_eq!(w0007.path, main);
    // A name that hides nothing passes.
    let edit = [replace(&main, CAPTURE, "Foo", 0, "Bar")];
    let request = VerifyRequest {
        renamed: &[("Foo", "Bar")],
        ..edits(&edit)
    };
    assert_eq!(verify(&snapshot, &request).unwrap(), VerifyOutcome::Clean);
}

const MAIN_UTIL: &str = "//! Root.\n\nimport app::util\n\nfn main() {\n  let _ = util::ok()\n}\n";
const UTIL: &str = "//! Util.\n\npub fn ok() -> Int {\n  1\n}\n";
const BROKEN: &str = "//! Broken.\n\npub fn broken( -> Int {\n  1\n}\n";

/// S-6: a grammar error in another module does not make the check
/// vacuous, since `verify` has no phase-A gate. A new type error in a
/// clean module is seen, and an edit that adds none passes. That a loss in
/// a broken module does not stop the run is `Retain::VERIFY`'s tolerance,
/// which the front end's `an_ice_in_one_module_loses_it_and_its_importers`
/// tests.
#[test]
fn verify_sees_past_a_broken_module() {
    let temp = package(
        "broken",
        &[
            ("src/main.cheby", MAIN_UTIL),
            ("src/util.cheby", UTIL),
            ("src/broken.cheby", BROKEN),
        ],
    );
    let snapshot = snapshot(&temp.0, &[]);
    assert!(!snapshot.status().broken.is_empty());
    let util = temp.0.join("src/util.cheby");
    let edit = [replace(&util, UTIL, "1", 0, "\"x\"")];
    let outcome = verify(&snapshot, &edits(&edit)).unwrap();
    let found = introduced(&outcome);
    assert!(
        found.iter().all(|(code, _)| code.starts_with('E')) && !found.is_empty(),
        "{found:?}"
    );
    let VerifyOutcome::Introduces(diags) = &outcome else {
        panic!("{outcome:?}")
    };
    let edited = UTIL.replacen('1', "\"x\"", 1);
    assert!(
        diags.iter().any(|i| i.path == util
            && i.diagnostic.range.and_then(|r| r.slice(&edited)) == Some("\"x\"")),
        "{diags:?}"
    );
    let edit = [replace(&util, UTIL, "1", 0, "2")];
    assert_eq!(
        verify(&snapshot, &edits(&edit)).unwrap(),
        VerifyOutcome::Clean
    );
    // No edit at all is clean too: before and after compare like with like.
    assert_eq!(
        verify(&snapshot, &edits(&[])).unwrap(),
        VerifyOutcome::Clean
    );
}

const MAIN_A: &str = "//! Root.\n\nimport app::a\n\nfn main() {\n  let _ = a::f()\n}\n";
const A: &str = "//! A.\n\nimport app::b\n\npub fn f() -> Int {\n  1\n}\n\n\
                 fn uses_b() -> Int {\n  b::g()\n}\n";
const B: &str = "//! B.\n\nimport app::a\n\npub fn g() -> Int {\n  2\n}\n\n\
                 fn uses_a() -> Int {\n  a::f()\n}\n";

/// D-1389: a clean package with an import cycle. The Q run cut an import,
/// so "before" cannot be trusted, and any edit is blocked.
#[test]
fn verify_blocked_by_cycle() {
    let temp = package(
        "cycle",
        &[
            ("src/main.cheby", MAIN_A),
            ("src/a.cheby", A),
            ("src/b.cheby", B),
        ],
    );
    let snapshot = snapshot(&temp.0, &[]);
    assert_ne!(snapshot.status().cut_imports.len(), 0);
    let a = temp.0.join("src/a.cheby");
    let edit = [replace(&a, A, "1", 0, "3")];
    let outcome = verify(&snapshot, &edits(&edit)).unwrap();
    let VerifyOutcome::Blocked(reason) = outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert!(reason.contains("cycle"), "{reason}");
}

/// A package with one E0044 whose message names `total`.
const SHADOW: &str = "//! Root.\n\nfn main() {\n  let _ = sum_up()\n}\n\n\
                      fn sum_up() -> Int {\n  let total = 1\n  let total = 2\n  total\n}\n";

/// Plan §1.6 step 5: a rename reads the old name as the new one in the
/// messages of "before", so an existing error that names the symbol is not
/// new. Without that reading it would be.
#[test]
fn verify_rename_keeps_old_messages() {
    let temp = package("rename", &[("src/main.cheby", SHADOW)]);
    let snapshot = snapshot(&temp.0, &[]);
    assert!(
        snapshot
            .baseline()
            .iter()
            .any(|(code, _, message)| *code == codes::SHADOWED_NAME && message.contains("`total`")),
        "{:?}",
        snapshot.baseline()
    );
    let main = temp.0.join("src/main.cheby");
    let all = replace_all(&main, SHADOW, "total", "amount");
    let request = VerifyRequest {
        renamed: &[("total", "amount")],
        ..edits(&all)
    };
    assert_eq!(verify(&snapshot, &request).unwrap(), VerifyOutcome::Clean);
    let found = introduced(&verify(&snapshot, &edits(&all)).unwrap());
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found.iter().all(|(_, m)| m.contains("`amount`")),
        "{found:?}"
    );
}

/// Plan §1.6 step 5: the comparison counts. A second error with the same
/// code, file and message is new, which a set would hide.
#[test]
fn verify_counts_duplicates() {
    let temp = package("duplicates", &[("src/main.cheby", SHADOW)]);
    let snapshot = snapshot(&temp.0, &[]);
    let main = temp.0.join("src/main.cheby");
    let at = SHADOW.find("  total\n}").unwrap();
    let start = TextSize::try_from(at).unwrap();
    let edit = [PlainEdit {
        path: main,
        range: TextRange::empty(start),
        replacement: "  let total = 3\n".to_owned(),
    }];
    let found = introduced(&verify(&snapshot, &edits(&edit)).unwrap());
    assert_eq!(
        found,
        [("E0044".to_owned(), "`total` is already bound".to_owned())]
    );
}

/// S-7, D-1389, D-1393: a closed file that changed on disk since the
/// snapshot read it makes any edit stale, touched or not. An open file is
/// the editor's truth, so a change on disk under it is not.
#[test]
fn verify_stale_closed_file() {
    let temp = package(
        "stale",
        &[("src/main.cheby", MAIN_UTIL), ("src/util.cheby", UTIL)],
    );
    let snapshot = snapshot(&temp.0, &[("src/main.cheby", MAIN_UTIL)]);
    let util = temp.0.join("src/util.cheby");
    let main = temp.0.join("src/main.cheby");
    let edit = [replace(&util, UTIL, "1", 0, "2")];
    let open = [replace(&main, MAIN_UTIL, "Root", 0, "Main")];
    assert_eq!(
        verify(&snapshot, &edits(&edit)).unwrap(),
        VerifyOutcome::Clean
    );
    std::fs::write(&main, "//! Changed.\n").unwrap();
    assert_eq!(
        verify(&snapshot, &edits(&open)).unwrap(),
        VerifyOutcome::Clean
    );
    std::fs::write(&util, UTIL.replace('1', "5")).unwrap();
    assert_eq!(
        verify(&snapshot, &edits(&edit)).unwrap(),
        VerifyOutcome::Stale(util.clone())
    );
    assert_eq!(
        verify(&snapshot, &edits(&open)).unwrap(),
        VerifyOutcome::Stale(util.clone())
    );
    // A move of a stale file is stale too, and a file that went is.
    let moves = [(util.clone(), temp.0.join("src/tools.cheby"))];
    let request = VerifyRequest {
        moves: &moves,
        ..edits(&[])
    };
    assert_eq!(
        verify(&snapshot, &request).unwrap(),
        VerifyOutcome::Stale(util.clone())
    );
    std::fs::remove_file(&util).unwrap();
    assert_eq!(
        verify(&snapshot, &edits(&edit)).unwrap(),
        VerifyOutcome::Stale(util)
    );
}

/// D-1389: a closed file that no edit touches but that changed on disk
/// makes the plan stale, since the edits may break it. So does a move onto
/// a file that appeared on disk after the snapshot.
#[test]
fn verify_stale_untouched_file() {
    let extra = "//! Extra.\n\npub fn e() -> Int {\n  0\n}\n";
    let temp = package(
        "stale_untouched",
        &[
            ("src/main.cheby", MAIN_UTIL),
            ("src/util.cheby", UTIL),
            ("src/extra.cheby", extra),
        ],
    );
    let snapshot = snapshot(&temp.0, &[]);
    let main = temp.0.join("src/main.cheby");
    let util = temp.0.join("src/util.cheby");
    let tools = temp.0.join("src/tools.cheby");
    let fix = [
        replace(&main, MAIN_UTIL, "app::util", 0, "app::tools"),
        replace(&main, MAIN_UTIL, "util::ok", 0, "tools::ok"),
    ];
    let moves = [(util.clone(), tools.clone())];
    let request = VerifyRequest {
        moves: &moves,
        ..edits(&fix)
    };
    assert_eq!(verify(&snapshot, &request).unwrap(), VerifyOutcome::Clean);
    std::fs::write(&tools, UTIL).unwrap();
    assert_eq!(
        verify(&snapshot, &request).unwrap(),
        VerifyOutcome::Stale(tools.clone())
    );
    std::fs::remove_file(&tools).unwrap();
    let rename = [
        replace(&util, UTIL, "ok", 0, "fine"),
        replace(&main, MAIN_UTIL, "ok", 0, "fine"),
    ];
    assert_eq!(
        verify(&snapshot, &edits(&rename)).unwrap(),
        VerifyOutcome::Clean
    );
    let extra_path = temp.0.join("src/extra.cheby");
    let calls_ok = "//! Extra.\n\nimport app::util\n\npub fn e() -> Int {\n  util::ok()\n}\n";
    std::fs::write(&extra_path, calls_ok).unwrap();
    assert_eq!(
        verify(&snapshot, &edits(&rename)).unwrap(),
        VerifyOutcome::Stale(extra_path)
    );
}

/// A module that is not UTF-8 enters the run on the edited text as it
/// entered the Q run, so it adds nothing, and it is not stale while its
/// bytes stay. An edit of it is refused, since the snapshot does not hold
/// its bytes.
#[test]
fn verify_ignores_a_non_utf8_module() {
    let temp = package(
        "not_utf8",
        &[("src/main.cheby", MAIN_UTIL), ("src/util.cheby", UTIL)],
    );
    let bad = temp.0.join("src/bad.cheby");
    std::fs::write(&bad, b"//! Bad.\n\xff\npub fn b() -> Int {\n  1\n}\n").unwrap();
    let snapshot = snapshot(&temp.0, &[]);
    assert!(
        snapshot
            .baseline()
            .iter()
            .any(|(code, path, _)| *code == codes::INVALID_UTF8
                && path.as_deref() == Some(bad.as_path())),
        "{:?}",
        snapshot.baseline()
    );
    assert_eq!(
        verify(&snapshot, &edits(&[])).unwrap(),
        VerifyOutcome::Clean
    );
    let util = temp.0.join("src/util.cheby");
    let edit = [replace(&util, UTIL, "1", 0, "2")];
    assert_eq!(
        verify(&snapshot, &edits(&edit)).unwrap(),
        VerifyOutcome::Clean
    );
    let lossy = snapshot
        .source(snapshot.file_of(&bad).unwrap())
        .unwrap()
        .text()
        .to_owned();
    let edit = [replace(&bad, &lossy, "1", 0, "2")];
    let outcome = verify(&snapshot, &edits(&edit)).unwrap();
    assert!(matches!(outcome, VerifyOutcome::Blocked(_)), "{outcome:?}");
    std::fs::write(&bad, b"//! Bad.\n\xfe\npub fn b() -> Int {\n  1\n}\n").unwrap();
    assert_eq!(
        verify(&snapshot, &edits(&[])).unwrap(),
        VerifyOutcome::Stale(bad)
    );
}

/// The run on the edited text loses a module to an internal error, so
/// "after" cannot be trusted: blocked.
#[test]
fn verify_blocked_by_a_loss_after() {
    let temp = package(
        "loss_after",
        &[("src/main.cheby", MAIN_UTIL), ("src/util.cheby", UTIL)],
    );
    let snapshot = snapshot(&temp.0, &[]);
    let util = temp.0.join("src/util.cheby");
    let at = TextSize::try_from(UTIL.len()).unwrap();
    let edit = [PlainEdit {
        path: util,
        range: TextRange::empty(at),
        replacement: "\npub fn module_ice_by_test() -> Int {\n  2\n}\n".to_owned(),
    }];
    let outcome = verify(&snapshot, &edits(&edit)).unwrap();
    let VerifyOutcome::Blocked(reason) = outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert!(reason.contains("edited package"), "{reason}");
}

/// A package where `f` has an E0044 that names `total`, and `g` binds its
/// own `total`.
const TWO_TOTALS: &str = "//! Root.\n\nfn main() {\n  let _ = f() + g()\n}\n\n\
                          fn f() -> Int {\n  let total = 1\n  let total = 2\n  total\n}\n\n\
                          fn g() -> Int {\n  let total = 3\n  total\n}\n";

/// A rename of `g`'s `total` leaves `f`'s E0044, whose message still names
/// `total`: that error is not new.
#[test]
fn verify_rename_keeps_another_binding_s_message() {
    let temp = package("rename_other", &[("src/main.cheby", TWO_TOTALS)]);
    let snapshot = snapshot(&temp.0, &[]);
    let main = temp.0.join("src/main.cheby");
    let edit = [
        replace(&main, TWO_TOTALS, "total", 3, "amount"),
        replace(&main, TWO_TOTALS, "total", 4, "amount"),
    ];
    let request = VerifyRequest {
        renamed: &[("total", "amount")],
        ..edits(&edit)
    };
    assert_eq!(verify(&snapshot, &request).unwrap(), VerifyOutcome::Clean);
    // Each entry of "before" still accounts for one diagnostic only.
    let at = TWO_TOTALS.find("  total\n}\n\nfn g").unwrap();
    let mut both = edit.to_vec();
    both.push(PlainEdit {
        path: main,
        range: TextRange::empty(TextSize::try_from(at).unwrap()),
        replacement: "  let total = 4\n".to_owned(),
    });
    let request = VerifyRequest {
        renamed: &[("total", "amount")],
        ..edits(&both)
    };
    let found = introduced(&verify(&snapshot, &request).unwrap());
    assert_eq!(
        found,
        [("E0044".to_owned(), "`total` is already bound".to_owned())]
    );
}

/// `willRenameFiles`: a module moved with its importer's edit is clean,
/// and a move without the edit gives the importer's new error.
#[test]
fn verify_moves_a_module() {
    let temp = package(
        "moves",
        &[("src/main.cheby", MAIN_UTIL), ("src/util.cheby", UTIL)],
    );
    let snapshot = snapshot(&temp.0, &[]);
    let main = temp.0.join("src/main.cheby");
    let moves = [(
        temp.0.join("src/util.cheby"),
        temp.0.join("src/tools.cheby"),
    )];
    let fix = [
        replace(&main, MAIN_UTIL, "app::util", 0, "app::tools"),
        replace(&main, MAIN_UTIL, "util::ok", 0, "tools::ok"),
    ];
    let request = VerifyRequest {
        moves: &moves,
        ..edits(&fix)
    };
    assert_eq!(verify(&snapshot, &request).unwrap(), VerifyOutcome::Clean);
    let request = VerifyRequest {
        moves: &moves,
        ..edits(&[])
    };
    let found = introduced(&verify(&snapshot, &request).unwrap());
    assert_ne!(found.len(), 0);
}

/// A request that does not fit the snapshot is an error, not an outcome.
#[test]
fn verify_rejects_a_bad_request() {
    let temp = package(
        "bad",
        &[("src/main.cheby", MAIN_UTIL), ("src/util.cheby", UTIL)],
    );
    let snapshot = snapshot(&temp.0, &[]);
    let util = temp.0.join("src/util.cheby");
    let outside = [replace(&temp.0.join("src/none.cheby"), UTIL, "1", 0, "2")];
    assert!(verify(&snapshot, &edits(&outside)).is_err());
    let overlapping = [
        replace(&util, UTIL, "ok", 0, "a"),
        replace(&util, UTIL, "k()", 0, "b"),
    ];
    assert!(verify(&snapshot, &edits(&overlapping)).is_err());
    let onto = [(util, temp.0.join("src/main.cheby"))];
    let request = VerifyRequest {
        moves: &onto,
        ..edits(&[])
    };
    assert!(verify(&snapshot, &request).is_err());
}

#[test]
fn rename_reads_whole_names_only() {
    assert_eq!(
        verify::rename_all("`count` and `counter` and count", &[("count", "n")]),
        "`n` and `counter` and n"
    );
    assert_eq!(verify::rename_all("aa", &[("a", "b")]), "aa");
    assert_eq!(verify::rename_all("aa", &[("", "b")]), "aa");
}

/// D-1674: every pair is read at once, so a chain of moves in either order
/// reads each path once, and the longer of two paths that start at one
/// place is read.
#[test]
fn rename_reads_pairs_at_once() {
    let message = "`app::a` and `app::b`";
    let chain = [("app::a", "app::b"), ("app::b", "app::c")];
    assert_eq!(verify::rename_all(message, &chain), "`app::b` and `app::c`");
    let reversed = [("app::b", "app::c"), ("app::a", "app::b")];
    assert_eq!(
        verify::rename_all(message, &reversed),
        "`app::b` and `app::c`"
    );
    let folder = [("app::geo", "app::shapes"), ("app::geo::x", "app::y")];
    assert_eq!(
        verify::rename_all("`app::geo::x`, `app::geo`, `app::geo::z`", &folder),
        "`app::y`, `app::shapes`, `app::shapes::z`"
    );
}

/// The diagnostics of a Q-run report, by path.
fn report_lists(live: &super::Live) -> Vec<(PathBuf, Vec<super::PlainDiagnostic>)> {
    let report = live
        .report
        .as_ref()
        .expect("a clean manifest and no broken file");
    report
        .files
        .iter()
        .map(|(path, file)| (path.clone(), file.diagnostics.clone()))
        .collect()
}

/// Plan §1.6: a verify and a Q run in parallel on one `Globals`, 200
/// rounds. Each round interns names no earlier round did, and each run
/// gives what it gives alone on fresh interners (D-264).
#[test]
fn concurrent_runs_share_globals() {
    let temp = package(
        "concurrent",
        &[("src/main.cheby", MAIN_UTIL), ("src/util.cheby", UTIL)],
    );
    let main = temp.0.join("src/main.cheby");
    let util = temp.0.join("src/util.cheby");
    let globals = Arc::new(Globals::new());
    let config = |globals: &Arc<Globals>| SessionConfig {
        root: temp.0.clone(),
        kind: PackageKind::Package,
        target: None,
        globals: Arc::clone(globals),
    };
    let mut shared = Session::new(config(&globals));
    let snapshot = shared
        .analyze_live(&Overlay::default())
        .unwrap()
        .snapshot
        .unwrap();
    let alone = Session::new(config(&Arc::new(Globals::new())))
        .analyze_live(&Overlay::default())
        .unwrap()
        .snapshot
        .unwrap();
    for round in 0..200_u64 {
        // The verify renames `ok` in `util` alone, so `main`'s call breaks.
        let name = format!("ok_{round}");
        let edit = [replace(&util, UTIL, "ok", 0, &name)];
        let text = MAIN_UTIL.replace("util::ok()", &format!("util::ok() + missing_{round}()"));
        let overlay = Overlay {
            files: [(
                main.clone(),
                OverlayFile {
                    text: Arc::from(text.as_str()),
                    version: 1,
                    stamp: round.saturating_add(1),
                },
            )]
            .into(),
        };
        let (verified, live) = std::thread::scope(|scope| {
            let verified = scope.spawn(|| verify(&snapshot, &edits(&edit)));
            let live = shared.analyze_live(&overlay);
            (verified.join().unwrap(), live)
        });
        let verified = verified.unwrap();
        let live = live.unwrap();
        assert!(
            matches!(verified, VerifyOutcome::Introduces(_)),
            "{verified:?}"
        );
        assert_eq!(
            verified,
            verify(&alone, &edits(&edit)).unwrap(),
            "round {round}"
        );
        let mut fresh = Session::new(config(&Arc::new(Globals::new())));
        let expected = fresh.analyze_live(&overlay).unwrap();
        assert_eq!(
            report_lists(&live),
            report_lists(&expected),
            "round {round}"
        );
    }
}

/// A loose script: `total` is a local of `main`.
const SCRIPT: &str = "//! A script.\n\nimport std::io\n\nfn main() {\n  let total = 1\n  io::println(\"{total}\")\n}\n";

/// D-1667: a loose file is checked as the one-module package `script`,
/// as its Q run is. A rename passes, an edit that adds an error is seen,
/// and a `src/` beside the file is no package's, so it is not listed
/// (D-1669). The same holds for a file in a package that is none of its
/// modules.
#[test]
fn verify_single_file_loose() {
    let temp = directory(
        "single_loose",
        &[("script.cheby", SCRIPT), ("src/main.cheby", MAIN_UTIL)],
    );
    assert_eq!(crate::find_package_root(&temp.0), None, "a package above");
    let file = temp.0.join("script.cheby");
    let snapshot = single_snapshot(&file, true, SCRIPT);
    let modules: Vec<_> = snapshot
        .local_modules()
        .filter_map(|m| m.path().cloned())
        .collect();
    assert_eq!(modules, [ModulePath::root(verify::SINGLE_PACKAGE)]);
    let all = replace_all(&file, SCRIPT, "total", "amount");
    let pair = [("total", "amount")];
    assert_eq!(
        verify(&snapshot, &rename(&all, &pair)).unwrap(),
        VerifyOutcome::Clean
    );
    let edit = [replace(&file, SCRIPT, "1", 0, "missing")];
    let found = introduced(&verify(&snapshot, &edits(&edit)).unwrap());
    assert!(
        found
            .iter()
            .any(|(code, m)| code.starts_with('E') && m.contains("missing")),
        "{found:?}"
    );
    // A file in a package that is not one of its modules.
    let package = package(
        "single_inside",
        &[
            ("src/main.cheby", MAIN_UTIL),
            ("tools/script.cheby", SCRIPT),
        ],
    );
    let file = package.0.join("tools/script.cheby");
    let snapshot = single_snapshot(&file, false, SCRIPT);
    let all = replace_all(&file, SCRIPT, "total", "amount");
    assert_eq!(
        verify(&snapshot, &rename(&all, &pair)).unwrap(),
        VerifyOutcome::Clean
    );
}

/// A copy of this repository's `std/` in a fresh temporary directory,
/// beside the marker file of D-1699, so a session over it is a std
/// checkout's. Paths are built from components (ws-plan §9).
fn std_checkout(name: &str) -> Temp {
    let temp = directory(name, &[]);
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("std"),
        &temp.0.join("std"),
    );
    let marker = at(&temp.0, "crates/cheby_driver/src/std_lib.rs");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, "").unwrap();
    temp
}

/// Copies `from` into a new directory `to`, path by path.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

/// The file at `display` under `root`, such as `std/json/tree.cheby`.
fn at(root: &Path, display: &str) -> PathBuf {
    display
        .split('/')
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

/// The snapshot of a std checkout's live run over `root` (D-1700), with
/// these open documents by display path.
fn checkout_snapshot(root: &Path, open: &[(&str, &str)]) -> Arc<Snapshot> {
    let overlay = Overlay {
        files: open
            .iter()
            .zip(1_u64..)
            .map(|(&(display, text), stamp)| {
                let file = OverlayFile {
                    text: Arc::from(text),
                    version: 1,
                    stamp,
                };
                (at(root, display), file)
            })
            .collect(),
    };
    let mut session = Session::new(SessionConfig {
        root: root.to_path_buf(),
        kind: PackageKind::StdCheckout {
            checkout: root.to_path_buf(),
        },
        target: None,
        globals: Arc::new(Globals::new()),
    });
    let live = session.analyze_live(&overlay).unwrap();
    assert_eq!(live.failed, None);
    let snapshot = live.snapshot.unwrap();
    assert!(snapshot.is_std_checkout());
    snapshot
}

/// The errors of a snapshot's baseline, by code and display path.
fn baseline_errors(snapshot: &Snapshot) -> Vec<(String, String)> {
    snapshot
        .baseline()
        .iter()
        .filter(|(code, _, _)| code.severity() == Severity::Error)
        .map(|(code, path, _)| {
            let path = path.as_deref().map_or_else(String::new, |p| {
                cheby_diag::display_path(p, snapshot.root())
            });
            (code.to_string(), path)
        })
        .collect()
}

/// `time::civil_from_days`, a package-visible function of `std/time.cheby`
/// that `std/http.cheby` calls, and the name it is renamed to.
const HELPER: (&str, &str) = ("civil_from_days", "days_to_civil");

/// A std checkout's `time` and `http`, each with its path and text.
struct TimeHttp {
    time: PathBuf,
    time_text: String,
    http: PathBuf,
    http_text: String,
}

impl TimeHttp {
    fn read(root: &Path) -> Self {
        let time = at(root, "std/time.cheby");
        let http = at(root, "std/http.cheby");
        Self {
            time_text: std::fs::read_to_string(&time).unwrap(),
            http_text: std::fs::read_to_string(&http).unwrap(),
            time,
            http,
        }
    }

    /// The rename of [`HELPER`] in `time`, with `time`'s text `time_text`.
    fn in_time(&self, time_text: &str) -> Vec<PlainEdit> {
        let (old, new) = HELPER;
        let edits = replace_all(&self.time, time_text, old, new);
        assert!(edits.len() >= 2, "a declaration and a use: {edits:?}");
        edits
    }

    /// The rename of [`HELPER`] in `http`.
    fn in_http(&self) -> Vec<PlainEdit> {
        let (old, new) = HELPER;
        let edits = replace_all(&self.http, &self.http_text, old, new);
        assert_eq!(edits.len(), 1, "one call");
        edits
    }
}

/// D-1700, D-1706: in a std checkout, `verify` builds package `std` from
/// the checkout's edited texts, as the Q run does, which replaces
/// D-1667's single-file std. A package-visible helper renamed in its
/// module and in another std module that calls it passes; the std is the
/// checkout's, so no error of the single-file run is in "before".
#[test]
fn verify_std_checkout() {
    let temp = std_checkout("std_checkout");
    let files = TimeHttp::read(&temp.0);
    let snapshot = checkout_snapshot(&temp.0, &[("std/time.cheby", &files.time_text)]);
    assert_eq!(baseline_errors(&snapshot), [], "{:?}", snapshot.baseline());
    let all = [files.in_time(&files.time_text), files.in_http()].concat();
    let pair = [HELPER];
    assert_eq!(
        verify(&snapshot, &rename(&all, &pair)).unwrap(),
        VerifyOutcome::Clean
    );
    // An edit that adds an error is seen, in the checkout's file.
    let edit = [replace(
        &files.time,
        &files.time_text,
        "days_to_epoch",
        0,
        "missing",
    )];
    let outcome = verify(&snapshot, &edits(&edit)).unwrap();
    let VerifyOutcome::Introduces(found) = &outcome else {
        panic!("{outcome:?}")
    };
    assert!(
        found
            .iter()
            .any(|i| i.path == files.time && i.at.starts_with("std/time.cheby:")),
        "{found:?}"
    );
}

/// A test that compares `time::civil_from_days`'s tuple with an `Int`: a
/// type error in a test body.
const OPEN_TEST_ERROR: &str = "\ntest \"ws open error\" {\n  assert civil_from_days(0) == 1\n}\n";

/// A test with a type error, in a module that stays closed.
const CLOSED_TEST_ERROR: &str = "\ntest \"ws closed error\" {\n  assert 1 == \"x\"\n}\n";

/// D-1701, D-1668: `verify` checks the test bodies the Q run checked,
/// those of the open modules. An error in an open module's test body is in
/// "before" and "after", so a rename in that module is not refused as
/// hiding it, and the error stays until an edit removes it. A closed
/// module's test body is checked in neither run, so its error is no new
/// one.
#[test]
fn verify_std_checkout_open_test_error() {
    let temp = std_checkout("std_checkout_test_error");
    let files = TimeHttp::read(&temp.0);
    let order = at(&temp.0, "std/order.cheby");
    let closed = std::fs::read_to_string(&order).unwrap() + CLOSED_TEST_ERROR;
    std::fs::write(&order, closed).unwrap();
    let open = files.time_text.clone() + OPEN_TEST_ERROR;
    let snapshot = checkout_snapshot(&temp.0, &[("std/time.cheby", &open)]);
    let errors = baseline_errors(&snapshot);
    assert_eq!(errors.len(), 1, "{:?}", snapshot.baseline());
    assert!(
        errors.iter().all(|(_, path)| path == "std/time.cheby"),
        "{errors:?}"
    );
    let all = [files.in_time(&open), files.in_http()].concat();
    let pair = [HELPER];
    assert_eq!(
        verify(&snapshot, &rename(&all, &pair)).unwrap(),
        VerifyOutcome::Clean
    );
    // The error is in the run on the edited text: an edit that removes the
    // test makes it go.
    let at_test = open.find(OPEN_TEST_ERROR).unwrap();
    let start = TextSize::try_from(at_test).unwrap();
    let end = TextSize::try_from(open.len()).unwrap();
    let remove = [PlainEdit {
        path: files.time.clone(),
        range: TextRange::new(start, end),
        replacement: "\n".to_owned(),
    }];
    let gone = hidden(&verify(&snapshot, &rename(&remove, &[])).unwrap());
    assert_eq!(gone.len(), 1, "{gone:?}");
    assert!(
        gone.iter()
            .all(|(_, path, _)| path.as_ref() == Some(&files.time)),
        "{gone:?}"
    );
}

/// D-1700, D-1389: renaming the helper in its module alone breaks its
/// caller in another std module, which `verify` sees there.
#[test]
fn verify_std_checkout_breaks_a_dependent() {
    let temp = std_checkout("std_checkout_dependent");
    let files = TimeHttp::read(&temp.0);
    let snapshot = checkout_snapshot(&temp.0, &[("std/time.cheby", &files.time_text)]);
    let only_time = files.in_time(&files.time_text);
    let pair = [HELPER];
    let outcome = verify(&snapshot, &rename(&only_time, &pair)).unwrap();
    let VerifyOutcome::Introduces(found) = &outcome else {
        panic!("{outcome:?}")
    };
    let call = files.http_text.find("time::civil_from_days").unwrap();
    let line = files.http_text.get(..call).unwrap().matches('\n').count() + 1;
    let place = format!("std/http.cheby:{line}:");
    assert!(
        found.iter().all(|i| i.path == files.http
            && i.at.starts_with(&place)
            && i.diagnostic.severity == Severity::Error),
        "{found:?}"
    );
    assert!(
        found
            .iter()
            .any(|i| i.diagnostic.message.contains(HELPER.0)),
        "{found:?}"
    );
}

/// A function whose parameter list never closes: a grammar error.
const GRAMMAR_ERROR: &str = "\n/// Broken.\npub fn ws_broken( -> Int {\n  1\n}\n";

/// S-6, D-1347, D-1700: a std module with a grammar error is preloaded with
/// its tree, as a package module is, so `verify` gives what it gives past
/// a broken package module (`verify_sees_past_a_broken_module`): a new type
/// error in a clean module is seen, an edit that adds none passes, and the
/// grammar error is in "before" and "after", so a rename that leaves it is
/// not refused as hiding it, while an edit that fixes it hides it.
#[test]
fn verify_std_checkout_grammar_error() {
    let temp = std_checkout("std_checkout_grammar");
    let files = TimeHttp::read(&temp.0);
    let order = at(&temp.0, "std/order.cheby");
    let broken = std::fs::read_to_string(&order).unwrap() + GRAMMAR_ERROR;
    let snapshot = checkout_snapshot(
        &temp.0,
        &[
            ("std/order.cheby", &broken),
            ("std/time.cheby", &files.time_text),
        ],
    );
    assert!(!snapshot.status().broken.is_empty());
    let errors = baseline_errors(&snapshot);
    assert!(
        !errors.is_empty() && errors.iter().all(|(_, path)| path == "std/order.cheby"),
        "{errors:?}"
    );
    // A new type error in a clean module.
    let edit = [replace(
        &files.http,
        &files.http_text,
        "civil_from_days(days)",
        0,
        "civil_from_days(\"x\")",
    )];
    let found = introduced(&verify(&snapshot, &edits(&edit)).unwrap());
    assert!(
        !found.is_empty() && found.iter().all(|(code, _)| code.starts_with('E')),
        "{found:?}"
    );
    // A rename that adds none, checked for hidden errors too.
    let all = [files.in_time(&files.time_text), files.in_http()].concat();
    let pair = [HELPER];
    assert_eq!(
        verify(&snapshot, &rename(&all, &pair)).unwrap(),
        VerifyOutcome::Clean
    );
    assert_eq!(
        verify(&snapshot, &rename(&[], &[])).unwrap(),
        VerifyOutcome::Clean
    );
    // Closing the parameter list fixes the grammar error, which goes.
    let fix = [replace(&order, &broken, "ws_broken(", 0, "ws_broken()")];
    let gone = hidden(&verify(&snapshot, &rename(&fix, &[])).unwrap());
    assert!(
        !gone.is_empty()
            && gone
                .iter()
                .all(|(_, path, _)| path.as_ref() == Some(&order)),
        "{gone:?}"
    );
}

/// D-1389, D-1669 in a std checkout: a closed std file that changed on
/// disk makes any edit stale, touched or not, and so does a new module
/// file under `std/`, which the run would read. An open file's change on
/// disk does not, nor does a file under `std/` that names no module, which
/// the Q run gives phase-A diagnostics only (D-1700); an edit of one is
/// refused as an edit of no file of the package.
#[test]
fn verify_std_checkout_stale() {
    let temp = std_checkout("std_checkout_stale");
    let files = TimeHttp::read(&temp.0);
    let loose = at(&temp.0, "std/main.cheby");
    std::fs::write(&loose, "//! Names no module.\n").unwrap();
    let snapshot = checkout_snapshot(&temp.0, &[("std/time.cheby", &files.time_text)]);
    assert!(snapshot.file_of(&loose).is_some(), "the loose file is read");
    let all = [files.in_time(&files.time_text), files.in_http()].concat();
    let pair = [HELPER];
    let request = rename(&all, &pair);
    assert_eq!(verify(&snapshot, &request).unwrap(), VerifyOutcome::Clean);
    // The open file and the loose one.
    std::fs::write(&files.time, "//! Changed.\n").unwrap();
    std::fs::write(&loose, "//! Changed.\n").unwrap();
    assert_eq!(verify(&snapshot, &request).unwrap(), VerifyOutcome::Clean);
    let edit = [PlainEdit {
        path: loose,
        range: TextRange::empty(0),
        replacement: "//! Edited.\n".to_owned(),
    }];
    assert!(verify(&snapshot, &edits(&edit)).is_err());
    // A closed file that no edit touches.
    let order = at(&temp.0, "std/order.cheby");
    let order_text = std::fs::read_to_string(&order).unwrap();
    std::fs::write(&order, order_text.clone() + "\n").unwrap();
    assert_eq!(
        verify(&snapshot, &request).unwrap(),
        VerifyOutcome::Stale(order.clone())
    );
    std::fs::write(&order, order_text).unwrap();
    // A closed file that an edit touches.
    std::fs::write(&files.http, files.http_text.clone() + "\n").unwrap();
    assert_eq!(
        verify(&snapshot, &request).unwrap(),
        VerifyOutcome::Stale(files.http.clone())
    );
    std::fs::write(&files.http, &files.http_text).unwrap();
    assert_eq!(verify(&snapshot, &request).unwrap(), VerifyOutcome::Clean);
    // A new module under `std/`, which may call the helper.
    let new = at(&temp.0, "std/json/ws_new.cheby");
    std::fs::write(&new, "//! New.\n").unwrap();
    assert_eq!(
        verify(&snapshot, &request).unwrap(),
        VerifyOutcome::Stale(new)
    );
}

/// `main` calls `util::fine`, which `util` does not have.
const MAIN_FINE: &str = "//! Root.\n\nimport app::util\n\nfn main() {\n  let _ = util::ok()\n  let _ = util::fine()\n}\n";

/// D-1668: renaming `ok` to `fine` binds the unresolved `util::fine`, so
/// its error goes away and the rename is refused, by the error as it read.
#[test]
fn verify_hides_an_unresolved_path() {
    let temp = package(
        "hides_path",
        &[("src/main.cheby", MAIN_FINE), ("src/util.cheby", UTIL)],
    );
    let snapshot = snapshot(&temp.0, &[]);
    let main = temp.0.join("src/main.cheby");
    let util = temp.0.join("src/util.cheby");
    let edit = [
        replace(&util, UTIL, "ok", 0, "fine"),
        replace(&main, MAIN_FINE, "ok", 0, "fine"),
    ];
    let pair = [("ok", "fine")];
    let found = hidden(&verify(&snapshot, &rename(&edit, &pair)).unwrap());
    assert_eq!(found.len(), 1, "{found:?}");
    let (code, path, message) = &found[0];
    assert!(code.starts_with('E'), "{found:?}");
    assert_eq!(path.as_deref(), Some(main.as_path()));
    assert!(message.contains("`fine`"), "{found:?}");
    // A name that binds nothing unresolved passes.
    let edit = [
        replace(&util, UTIL, "ok", 0, "good"),
        replace(&main, MAIN_FINE, "ok", 0, "good"),
    ];
    assert_eq!(
        verify(&snapshot, &rename(&edit, &[("ok", "good")])).unwrap(),
        VerifyOutcome::Clean
    );
}

/// A record with a field `x` and an access to a field `y` it lacks.
const FIELD: &str = "//! Root.\n\ntype Point {\n  x: Int,\n}\n\nfn get(p: Point) -> Int {\n  p.y\n}\n\n\
                     fn main() {\n  let _ = get\n}\n";

/// D-1668: renaming the field `x` to `y` makes the access `p.y` legal,
/// so its error goes away.
#[test]
fn verify_hides_a_field_error() {
    let temp = package("hides_field", &[("src/main.cheby", FIELD)]);
    let snapshot = snapshot(&temp.0, &[]);
    let errors: Vec<_> = snapshot
        .baseline()
        .iter()
        .filter(|(code, _, _)| code.severity() == Severity::Error)
        .collect();
    assert_eq!(errors.len(), 1, "{errors:?}");
    let main = temp.0.join("src/main.cheby");
    let edit = [replace(&main, FIELD, "x: Int", 0, "y: Int")];
    let found = hidden(&verify(&snapshot, &rename(&edit, &[("x", "y")])).unwrap());
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, errors[0].0.to_string());
}

/// A body that binds `total` twice (E0044 at the second).
const TWICE: &str =
    "//! Root.\n\nfn main() {\n  let total = 1\n  let total = 2\n  assert total == 2\n}\n";

/// D-1668, D-1698: renaming the second `total` to `sum` makes its E0044
/// go away. The hidden error is the one the user sees, by its message as
/// it was, which names `total`, never `sum`, and at its place.
#[test]
fn verify_hides_an_error_as_it_was() {
    let temp = package("hides_as_was", &[("src/main.cheby", TWICE)]);
    let snapshot = snapshot(&temp.0, &[]);
    let main = temp.0.join("src/main.cheby");
    let edit = [
        replace(&main, TWICE, "total", 1, "sum"),
        replace(&main, TWICE, "total", 2, "sum"),
    ];
    let outcome = verify(&snapshot, &rename(&edit, &[("total", "sum")])).unwrap();
    assert_eq!(
        hidden(&outcome),
        [(
            "E0044".to_owned(),
            Some(main),
            "`total` is already bound".to_owned()
        )]
    );
    assert_eq!(
        hidden_places(&outcome),
        [Some("src/main.cheby:5:7".to_owned())]
    );
}

/// A root module, and a directory under `src/` whose name is no module
/// name: the Q run's walk reports it as E0037.
const ROOT_WITH_LOCAL: &str = "//! Root.\n\nfn main() {\n  let total = 1\n  assert total == 1\n}\n";

/// D-1668: E0037, which only the Q run's walk of `src/` reports, is
/// compared on neither side (`counts`), since the edited run walks nothing,
/// so the E0037 of a directory with an invalid name never reads as an
/// error that a rename makes go away.
#[test]
fn verify_does_not_hide_an_e0037() {
    let temp = package(
        "keeps_e0037",
        &[
            ("src/main.cheby", ROOT_WITH_LOCAL),
            ("src/test-data/sample.txt", "data\n"),
        ],
    );
    let snapshot = snapshot(&temp.0, &[]);
    assert!(
        snapshot
            .baseline()
            .iter()
            .any(|(code, _, _)| *code == codes::INVALID_MODULE_NAME),
        "{:?}",
        snapshot.baseline()
    );
    let main = temp.0.join("src/main.cheby");
    let edit = replace_all(&main, ROOT_WITH_LOCAL, "total", "sum");
    assert_eq!(
        verify(&snapshot, &rename(&edit, &[("total", "sum")])).unwrap(),
        VerifyOutcome::Clean
    );
}

/// A body that reads a `b` it never binds, beside a local `a`.
const LOCAL: &str =
    "//! Root.\n\nfn main() {\n  let _ = f()\n}\n\nfn f() -> Int {\n  let a = 1\n  b\n}\n";

/// D-1668: renaming the local `a` to `b` binds the unresolved `b`.
#[test]
fn verify_hides_a_local() {
    let temp = package("hides_local", &[("src/main.cheby", LOCAL)]);
    let snapshot = snapshot(&temp.0, &[]);
    let main = temp.0.join("src/main.cheby");
    let at = TextSize::try_from(LOCAL.find("let a").unwrap().checked_add(4).unwrap()).unwrap();
    let edit = [PlainEdit {
        path: main.clone(),
        range: TextRange::at(at, TextSize::from(1_u32)),
        replacement: "b".to_owned(),
    }];
    let found = hidden(&verify(&snapshot, &rename(&edit, &[("a", "b")])).unwrap());
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].1.as_deref(), Some(main.as_path()));
    assert!(found[0].2.contains("`b`"), "{found:?}");
    // `c` binds nothing.
    let edit = [PlainEdit {
        replacement: "c".to_owned(),
        ..edit[0].clone()
    }];
    assert_eq!(
        verify(&snapshot, &rename(&edit, &[("a", "c")])).unwrap(),
        VerifyOutcome::Clean
    );
}

/// `main` calls `util::secret`, which is private to `util`: an E0043 in
/// `main`.
const MAIN_SECRET: &str =
    "//! Root.\n\nimport app::util\n\nfn main() {\n  let _ = util::secret()\n}\n";
const UTIL_SECRET: &str = "//! Util.\n\npriv fn secret() -> Int {\n  1\n}\n";

/// D-1668: making `secret` visible, as "Make `pub`" does, removes an
/// error in another file on purpose, so without `hides` the edit is clean.
/// With it, the same edit hides the E0043.
#[test]
fn verify_does_not_hide_without_the_flag() {
    let temp = package(
        "no_hides",
        &[
            ("src/main.cheby", MAIN_SECRET),
            ("src/util.cheby", UTIL_SECRET),
        ],
    );
    let snapshot = snapshot(&temp.0, &[]);
    let util = temp.0.join("src/util.cheby");
    let edit = [replace(&util, UTIL_SECRET, "priv ", 0, "")];
    assert_eq!(
        verify(&snapshot, &edits(&edit)).unwrap(),
        VerifyOutcome::Clean
    );
    let asked = VerifyRequest {
        hides: true,
        ..edits(&edit)
    };
    let found = hidden(&verify(&snapshot, &asked).unwrap());
    assert_eq!(
        found,
        [(
            "E0043".to_owned(),
            Some(temp.0.join("src/main.cheby")),
            "`secret` is private to module `app::util`".to_owned()
        )]
    );
}

/// D-1669: a module file that appeared on disk after the snapshot, which a
/// watched-file job has not added yet, makes any edit stale, at any depth.
/// A file that is no module is not listed.
#[test]
fn verify_stale_new_module() {
    let temp = package(
        "stale_new",
        &[("src/main.cheby", MAIN_UTIL), ("src/util.cheby", UTIL)],
    );
    let snapshot = snapshot(&temp.0, &[]);
    for other in [
        "src/notes.txt",
        "src/.hidden.cheby",
        "src/Bad-Name.cheby",
        "extra.cheby",
    ] {
        std::fs::write(temp.0.join(other), UTIL).unwrap();
    }
    assert_eq!(
        verify(&snapshot, &edits(&[])).unwrap(),
        VerifyOutcome::Clean
    );
    let extra = temp.0.join("src/extra.cheby");
    std::fs::write(&extra, UTIL).unwrap();
    assert_eq!(
        verify(&snapshot, &edits(&[])).unwrap(),
        VerifyOutcome::Stale(extra.clone())
    );
    std::fs::remove_file(&extra).unwrap();
    let inner = temp.0.join("src/deep/inner.cheby");
    std::fs::create_dir_all(inner.parent().unwrap()).unwrap();
    std::fs::write(&inner, UTIL).unwrap();
    assert_eq!(
        verify(&snapshot, &edits(&[])).unwrap(),
        VerifyOutcome::Stale(inner)
    );
}

/// `main` calls the private `hidden` of `b`: an E0043 that names
/// `app::b`.
const MAIN_B: &str = "//! Root.\n\nimport app::b\n\nfn main() {\n  let _ = b::hidden()\n}\n";
const PRIVATE: &str = "//! A module.\n\npriv fn hidden() -> Int {\n  1\n}\n";

/// D-1674: a move into a folder prints the module's path with one more
/// segment, so "before" is read with the pair `app::b` to `app::sub::b`
/// and the move with its importer's edit is clean. Without the pair the
/// E0043 reads as new.
#[test]
fn verify_moves_into_a_folder() {
    let temp = package(
        "move_folder",
        &[("src/main.cheby", MAIN_B), ("src/b.cheby", PRIVATE)],
    );
    let snapshot = snapshot(&temp.0, &[]);
    let main = temp.0.join("src/main.cheby");
    let moves = [(temp.0.join("src/b.cheby"), temp.0.join("src/sub/b.cheby"))];
    let fix = [replace(&main, MAIN_B, "app::b", 0, "app::sub::b")];
    let request = VerifyRequest {
        moves: &moves,
        renamed: &[("app::b", "app::sub::b")],
        ..edits(&fix)
    };
    assert_eq!(verify(&snapshot, &request).unwrap(), VerifyOutcome::Clean);
    let request = VerifyRequest {
        moves: &moves,
        ..edits(&fix)
    };
    let found = introduced(&verify(&snapshot, &request).unwrap());
    assert_eq!(
        found,
        [(
            "E0043".to_owned(),
            "`hidden` is private to module `app::sub::b`".to_owned()
        )]
    );
}

/// `main` calls the private `hidden` of `a` and of `b`.
const MAIN_AB: &str = "//! Root.\n\nimport app::a\nimport app::b\n\n\
                       fn main() {\n  let _ = a::hidden()\n  let _ = b::hidden()\n}\n";

/// D-1674: two moves in one request, `b` to `c` and then `a` to `b`,
/// each with its own pair. Every pair is read at once, so `app::a` reads
/// as `app::b` and not on as `app::c`, whatever the pairs' order. One pair
/// alone leaves the other move's error new.
#[test]
fn verify_two_moves() {
    let temp = package(
        "two_moves",
        &[
            ("src/main.cheby", MAIN_AB),
            ("src/a.cheby", PRIVATE),
            ("src/b.cheby", PRIVATE),
        ],
    );
    let snapshot = snapshot(&temp.0, &[]);
    let main = temp.0.join("src/main.cheby");
    let src = temp.0.join("src");
    let moves = [
        (src.join("b.cheby"), src.join("c.cheby")),
        (src.join("a.cheby"), src.join("b.cheby")),
    ];
    let fix = [
        replace(&main, MAIN_AB, "app::a", 0, "app::b"),
        replace(&main, MAIN_AB, "app::b", 0, "app::c"),
        replace(&main, MAIN_AB, "a::hidden", 0, "b::hidden"),
        replace(&main, MAIN_AB, "b::hidden", 0, "c::hidden"),
    ];
    for renamed in [
        [("app::a", "app::b"), ("app::b", "app::c")],
        [("app::b", "app::c"), ("app::a", "app::b")],
    ] {
        let request = VerifyRequest {
            moves: &moves,
            renamed: &renamed,
            ..edits(&fix)
        };
        assert_eq!(
            verify(&snapshot, &request).unwrap(),
            VerifyOutcome::Clean,
            "{renamed:?}"
        );
    }
    let request = VerifyRequest {
        moves: &moves,
        renamed: &[("app::b", "app::c")],
        ..edits(&fix)
    };
    let found = introduced(&verify(&snapshot, &request).unwrap());
    assert_eq!(
        found,
        [(
            "E0043".to_owned(),
            "`hidden` is private to module `app::b`".to_owned()
        )]
    );
}

/// Two errors that tie on code, file and message, one in each function.
const TWO_BARS: &str = "//! Root.\n\nfn a() -> Int {\n  bar\n}\n\nfn b() -> Int {\n  let foo = 1\n  bar + foo\n}\n\nfn main() {\n  assert a() + b() == 0\n}\n";

/// D-1698, D-1709: of two errors that tie on code, file and message, the
/// one a rename makes go away is quoted at its own place, not at the place
/// of the one that stays: an error of "after" takes the entry on its line.
#[test]
fn verify_hides_the_tied_error_that_went() {
    let temp = package("two_bars", &[("src/main.cheby", TWO_BARS)]);
    let snapshot = snapshot(&temp.0, &[]);
    let main = temp.0.join("src/main.cheby");
    let tied = snapshot
        .baseline()
        .iter()
        .filter(|(code, _, message)| *code == codes::UNRESOLVED_NAME && message.contains("`bar`"))
        .count();
    assert_eq!(tied, 2, "{:?}", snapshot.baseline());
    let at = TWO_BARS.find("  let foo").unwrap();
    let edits: Vec<PlainEdit> = TWO_BARS
        .match_indices("foo")
        .filter(|(start, _)| *start > at)
        .map(|(start, _)| PlainEdit {
            path: main.clone(),
            range: TextRange::new(
                TextSize::try_from(start).unwrap(),
                TextSize::try_from(start + 3).unwrap(),
            ),
            replacement: "bar".to_owned(),
        })
        .collect();
    assert_eq!(edits.len(), 2);
    let outcome = verify(&snapshot, &rename(&edits, &[("foo", "bar")])).unwrap();
    assert_eq!(
        hidden_places(&outcome),
        [Some("src/main.cheby:9:3".to_owned())]
    );
}

const MAIN_SHAPES: &str = "//! Root.\n\nimport app::util\n\ninterface Wrap {\n  fn wrap(Self) -> Int\n}\n\nfn wrapped<T: Wrap>(x: T) -> Int {\n  Wrap::wrap(x)\n}\n\nfn main() {\n  assert wrapped(util::make()) == 1\n}\n";
const SHAPES: &str = "//! Util.\n\npub type Shape {\n  side: Int,\n}\n\npub fn make() -> Shape {\n  Shape { side: 1 }\n}\n";

/// D-1715: a move that changes a module's last segment reads `util::` as
/// `shapes::` in the messages of "before", after the whole paths, so an
/// error that names `util::Shape` before the move and `shapes::Shape`
/// after it is not new. Without the pair it would be.
#[test]
fn verify_reads_a_moved_modules_last_segment() {
    let temp = package(
        "last_segment",
        &[("src/main.cheby", MAIN_SHAPES), ("src/util.cheby", SHAPES)],
    );
    let snapshot = snapshot(&temp.0, &[]);
    assert!(
        snapshot
            .baseline()
            .iter()
            .any(|(_, _, message)| message.contains("`util::Shape`")),
        "{:?}",
        snapshot.baseline()
    );
    let main = temp.0.join("src/main.cheby");
    let edits = [
        replace(&main, MAIN_SHAPES, "app::util", 0, "app::shapes"),
        replace(&main, MAIN_SHAPES, "util::make", 0, "shapes::make"),
    ];
    let moves = [(
        temp.0.join("src/util.cheby"),
        temp.0.join("src/shapes.cheby"),
    )];
    let request = |renamed| VerifyRequest {
        edits: &edits,
        moves: &moves,
        renamed,
        hides: false,
    };
    let both = [("app::util", "app::shapes"), ("util::", "shapes::")];
    assert_eq!(
        verify(&snapshot, &request(&both)).unwrap(),
        VerifyOutcome::Clean
    );
    let found = introduced(&verify(&snapshot, &request(&both[..1])).unwrap());
    assert!(
        found.iter().any(|(_, m)| m.contains("`shapes::Shape`")),
        "{found:?}"
    );
}

/// Two functions: `a` binds `x` twice (E0044 at 5:7), and `b` binds `x`
/// and `z`.
const TWO_XS: &str = "//! Root.\n\nfn a() -> Int {\n  let x = 1\n  let x = 2\n  x\n}\n\nfn b() -> Int {\n  let x = 1\n  let z = 5\n  x + z\n}\n\nfn main() {\n  assert a() + b() == 8\n}\n";

/// D-1709: renaming `b`'s `x` to `z` gives a new E0044, "`z` is already
/// bound" at 11:7, which is also the renamed reading of `a`'s E0044 at 5:7.
/// That old error still stands on its own line and keeps its entry, so the
/// new one is quoted at its own place, not the old one at 5:7.
#[test]
fn verify_quotes_the_new_error_not_an_old_one_it_reads_like() {
    let temp = package("two_xs", &[("src/main.cheby", TWO_XS)]);
    let snapshot = snapshot(&temp.0, &[]);
    let main = temp.0.join("src/main.cheby");
    assert_eq!(
        snapshot
            .baseline()
            .iter()
            .filter(|(code, _, _)| code.severity() == Severity::Error)
            .count(),
        1,
        "{:?}",
        snapshot.baseline()
    );
    let edits = [
        replace(&main, TWO_XS, "x", 3, "z"),
        replace(&main, TWO_XS, "x", 4, "z"),
    ];
    let outcome = verify(&snapshot, &rename(&edits, &[("x", "z")])).unwrap();
    match &outcome {
        VerifyOutcome::Introduces(diags) => {
            let found: Vec<(String, String)> = diags
                .iter()
                .map(|i| (i.diagnostic.message.clone(), i.at.clone()))
                .collect();
            assert_eq!(
                found,
                [(
                    "`z` is already bound".to_owned(),
                    "src/main.cheby:11:7".to_owned()
                )]
            );
        }
        other => panic!("expected new diagnostics, got {other:?}"),
    }
}

/// A closure's parameter `foo` beside two reads of an unbound `bar`, all
/// on one line.
const ONE_LINE_BARS: &str = "//! Root.\n\nfn apply(f: fn(Int) -> Int) -> Int {\n  f(1)\n}\n\nfn main() {\n  let r = apply(fn(foo) { foo + bar }) + bar\n  assert r == 1\n}\n";

/// D-1709: renaming `foo` to `bar` binds the `bar` inside the closure.
/// One of the two tied E0042 on line 8 goes and one stays, and the line
/// cannot tell which went, so the hidden error is quoted by its file only.
#[test]
fn verify_quotes_the_file_of_a_tie_on_one_line() {
    let temp = package("one_line_bars", &[("src/main.cheby", ONE_LINE_BARS)]);
    let snapshot = snapshot(&temp.0, &[]);
    let main = temp.0.join("src/main.cheby");
    let edits = replace_all(&main, ONE_LINE_BARS, "foo", "bar");
    assert_eq!(edits.len(), 2);
    let outcome = verify(&snapshot, &rename(&edits, &[("foo", "bar")])).unwrap();
    assert_eq!(
        hidden(&outcome),
        [(
            "E0042".to_owned(),
            Some(main),
            "cannot find value `bar` in this scope".to_owned()
        )]
    );
    assert_eq!(hidden_places(&outcome), [Some("src/main.cheby".to_owned())]);
}

/// Three reads of an unbound `missing`, two on line 5 and one on line 10.
const THREE_MISSING: &str = "//! Root.\n\nfn one() -> Int {\n  let a = 1\n  a + missing() + missing()\n}\n\nfn two() -> Int {\n  let b = 2\n  b + missing()\n}\n\nfn main() {\n  assert one() + two() == 3\n}\n";

/// D-1698: errors that tie on code, file and message keep the file's
/// order in the baseline, so of the errors that go away the first quoted
/// is the first in the file, line 5 before line 10.
#[test]
fn verify_hides_tied_errors_in_file_order() {
    let temp = package("three_missing", &[("src/main.cheby", THREE_MISSING)]);
    let snapshot = snapshot(&temp.0, &[]);
    let main = temp.0.join("src/main.cheby");
    let edits = replace_all(&main, THREE_MISSING, "one", "missing");
    let outcome = verify(&snapshot, &rename(&edits, &[("one", "missing")])).unwrap();
    assert_eq!(
        hidden_places(&outcome),
        [
            Some("src/main.cheby:5:7".to_owned()),
            Some("src/main.cheby:5:19".to_owned()),
            Some("src/main.cheby:10:7".to_owned()),
        ]
    );
}

/// D-1715: two moves that give two modules of one last segment two new
/// ones read `util::` both ways, one reading per choice; with one move
/// there is one reading.
#[test]
fn rename_reads_a_shared_old_each_way() {
    let message = "`util::Shape` does not satisfy `Area`";
    assert_eq!(
        verify::readings(message, &[("util::", "tools::")]),
        ["`tools::Shape` does not satisfy `Area`"]
    );
    assert_eq!(
        verify::readings(
            message,
            &[
                ("app::a::util", "app::a::tools"),
                ("app::b::util", "app::b::helpers"),
                ("util::", "tools::"),
                ("util::", "helpers::"),
            ]
        ),
        [
            "`tools::Shape` does not satisfy `Area`",
            "`helpers::Shape` does not satisfy `Area`",
        ]
    );
}
