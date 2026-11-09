//! Tests of the analysis API: `verify` (plan §1.6, D-1389, D-1393).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::{TextRange, TextSize, codes};
use cheby_iface::Globals;
use cheby_package::MANIFEST_FILE;

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
    let dir = std::env::temp_dir().join(format!("cheby_verify_{}_{name}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    for (path, text) in std::iter::once(&(MANIFEST_FILE, MANIFEST)).chain(files) {
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
        renamed: None,
    }
}

/// The codes and messages of an outcome's new diagnostics.
fn introduced(outcome: &VerifyOutcome) -> Vec<(String, String)> {
    match outcome {
        VerifyOutcome::Introduces(diags) => diags
            .iter()
            .map(|d| (d.code.to_string(), d.message.clone()))
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
        renamed: Some(("Foo", "Some")),
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
        .find(|d| d.code == codes::HIDDEN_PRELUDE_NAME)
        .unwrap();
    // The range lies in the edited text: the new name.
    let edited = CAPTURE.replacen("Foo", "Some", 1);
    assert_eq!(w0007.range.unwrap().slice(&edited), Some("Some"));
    // A name that hides nothing passes.
    let edit = [replace(&main, CAPTURE, "Foo", 0, "Bar")];
    let request = VerifyRequest {
        renamed: Some(("Foo", "Bar")),
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
        diags
            .iter()
            .any(|d| d.range.and_then(|r| r.slice(&edited)) == Some("\"x\"")),
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
        renamed: Some(("total", "amount")),
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
        renamed: Some(("total", "amount")),
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
        renamed: Some(("total", "amount")),
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
        super::verify::rename_in("`count` and `counter` and count", "count", "n"),
        "`n` and `counter` and n"
    );
    assert_eq!(super::verify::rename_in("aa", "a", "b"), "aa");
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
