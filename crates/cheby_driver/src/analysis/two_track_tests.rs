//! Tests of the two-track algorithm (D-1347, D-1355 to D-1360, A §2.8):
//! what a session publishes from the Q run and from the D run while files
//! are broken, stubbed, cut or lost, with small packages and scripted
//! overlays.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::SourceMap;
use cheby_iface::Globals;
use cheby_package::MANIFEST_FILE;

use super::{
    FileReport, Live, Overlay, OverlayFile, PackageKind, Report, Session, SessionConfig,
    TextOrigin, codes,
};
use crate::parse_file;

const MANIFEST: &str = "name = \"app\"\nversion = \"0.1.0\"\n";
/// Not TOML: the manifest gives nothing.
const MANIFEST_UNUSABLE: &str = "name = \n";
/// A manifest with an error that still gives a manifest (example 030's).
const MANIFEST_USABLE: &str = "name = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\n\
                               sql = { git = \"https://example.com/sql\", tag = \"v1.0.0\" }\n";

const MAIN: &str = "//! Root.\n\nimport app::b\n\nfn main() {\n  let _ = b::g()\n}\n";
const B: &str = "//! B.\n\npub fn g() -> Int {\n  2\n}\n";
const B_BROKEN: &str = "//! B.\n\npub fn g() -> Int {\n  2\n";
const B_BROKEN_2: &str = "//! B.\n\npub fn g() -> Int {\n  2 +\n";
const B_BROKEN_3: &str = "//! B.\n\npub fn g() -> Int {\n  2 + 3\n";
/// F has a real type error.
const F: &str = "//! F.\n\npub fn f() -> Int {\n  \"x\"\n}\n";
const MAIN_BF: &str =
    "//! Root.\n\nimport app::b\nimport app::f\n\nfn main() {\n  let _ = b::g() + f::f()\n}\n";

/// A package in a fresh temporary directory, removed on drop.
struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Temp {
    /// The absolute path of `file` in the package.
    fn at(&self, file: &str) -> PathBuf {
        self.0.join(file)
    }
}

/// A package `name` with these files, under the temporary directory.
fn package(name: &str, files: &[(&str, &str)]) -> Temp {
    let dir = std::env::temp_dir().join(format!("cheby_two_track_{}_{name}", std::process::id()));
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

/// A session over `root` with fresh `Globals`.
fn session(root: &Temp) -> Session {
    Session::new(SessionConfig {
        root: root.0.clone(),
        kind: PackageKind::Package,
        target: None,
        globals: Arc::new(Globals::new()),
    })
}

/// An overlay of these open documents, each with its version, stamped in
/// order.
fn open(root: &Temp, files: &[(&str, &str, i32)]) -> Overlay {
    Overlay {
        files: files
            .iter()
            .zip(1_u64..)
            .map(|(&(path, text, version), stamp)| {
                (
                    root.at(path),
                    OverlayFile {
                        text: Arc::from(text),
                        version,
                        stamp,
                    },
                )
            })
            .collect(),
    }
}

/// The codes of a file's diagnostics, in order.
fn codes_of(report: &FileReport) -> Vec<String> {
    report
        .diagnostics
        .iter()
        .map(|d| d.code.to_string())
        .collect()
}

/// Each file's codes, by path relative to `root`.
fn all_codes(
    root: &Temp,
    files: &BTreeMap<PathBuf, Arc<FileReport>>,
) -> BTreeMap<String, Vec<String>> {
    files
        .iter()
        .map(|(path, report)| {
            (
                path.strip_prefix(&root.0).unwrap().display().to_string(),
                codes_of(report),
            )
        })
        .collect()
}

/// The paths of `paths` relative to `root`.
fn relative<'a>(root: &Temp, paths: impl IntoIterator<Item = &'a PathBuf>) -> BTreeSet<String> {
    paths
        .into_iter()
        .map(|p| p.strip_prefix(&root.0).unwrap().display().to_string())
        .collect()
}

/// `text`'s own phase-A diagnostics, as code and message: a parse of the
/// file alone.
fn phase_a_of(display: &str, text: &str) -> Vec<(String, String)> {
    let mut sources = SourceMap::new();
    let mut diagnostics = Vec::new();
    parse_file(
        &mut sources,
        display,
        text.as_bytes().to_vec(),
        &mut diagnostics,
    )
    .unwrap();
    sources.sort(&mut diagnostics);
    diagnostics
        .iter()
        .map(|d| (d.code.to_string(), d.message.clone()))
        .collect()
}

/// A report's diagnostics as code and message.
fn pairs(report: &FileReport) -> Vec<(String, String)> {
    report
        .diagnostics
        .iter()
        .map(|d| (d.code.to_string(), d.message.clone()))
        .collect()
}

/// The D run a live run planned.
fn strict(session: &mut Session, live: Live) -> Report {
    let plan = live.plan.expect("a D run is planned");
    session.analyze_strict(plan).unwrap()
}

#[test]
fn typing_in_broken_file_skips_the_strict_run() {
    let pkg = package(
        "typing",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN),
            ("src/b.cheby", B),
        ],
    );
    let b = pkg.at("src/b.cheby");
    let mut s = session(&pkg);
    // The first broken version: B's last good text is its clean disk text,
    // and no D run has happened yet.
    let first = s
        .analyze_live(&open(&pkg, &[("src/b.cheby", B_BROKEN, 1)]))
        .unwrap();
    assert!(first.report.is_none());
    assert_eq!(first.phase_a.get(&b).unwrap().version, Some(1));
    let report = strict(&mut s, first);
    assert!(!report.files.contains_key(&b));
    for (version, text) in [(2, B_BROKEN_2), (3, B_BROKEN_3)] {
        let live = s
            .analyze_live(&open(&pkg, &[("src/b.cheby", text, version)]))
            .unwrap();
        assert!(live.plan.is_none(), "version {version} plans a D run");
        assert!(live.report.is_none());
        let shown = live.phase_a.get(&b).unwrap();
        assert_eq!(shown.version, Some(version));
        assert_eq!(&*shown.text, text);
        assert_ne!(shown.diagnostics.len(), 0);
    }
    // B turns broken from clean: the last run was the check, on the text
    // that is now B's last good text, so no D run either.
    let mut s = session(&pkg);
    assert!(
        s.analyze_live(&Overlay::default())
            .unwrap()
            .report
            .is_some()
    );
    let live = s
        .analyze_live(&open(&pkg, &[("src/b.cheby", B_BROKEN, 1)]))
        .unwrap();
    assert!(live.plan.is_none());
    // Another file edited: the strict inputs moved.
    let main = MAIN.replace("b::g()", "b::g() + 1");
    let live = s
        .analyze_live(&open(
            &pkg,
            &[("src/b.cheby", B_BROKEN, 1), ("src/main.cheby", &main, 1)],
        ))
        .unwrap();
    assert!(live.plan.is_some());
}

#[test]
fn broken_file_shows_only_phase_a() {
    let pkg = package(
        "only_phase_a",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN),
            ("src/b.cheby", B),
        ],
    );
    let b = pkg.at("src/b.cheby");
    let mut s = session(&pkg);
    let live = s
        .analyze_live(&open(&pkg, &[("src/b.cheby", B_BROKEN, 4)]))
        .unwrap();
    assert!(live.report.is_none());
    assert_eq!(
        relative(&pkg, live.phase_a.keys()),
        BTreeSet::from([MANIFEST_FILE.to_owned(), "src/b.cheby".to_owned()])
    );
    let shown = live.phase_a.get(&b).unwrap();
    assert_eq!(pairs(shown), phase_a_of("src/b.cheby", B_BROKEN));
    assert_eq!(shown.origin, TextOrigin::Editor);
    assert!(live.snapshot.as_ref().unwrap().status().broken.contains(&b));
    let report = strict(&mut s, live);
    assert_eq!(
        relative(&pkg, report.files.keys()),
        BTreeSet::from([MANIFEST_FILE.to_owned(), "src/main.cheby".to_owned()])
    );
    assert_eq!(report.held.len(), 0);
    assert!(report.substituted);
}

#[test]
fn other_file_stays_real_while_b_is_broken() {
    let pkg = package(
        "stays_real",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN_BF),
            ("src/b.cheby", B),
            ("src/f.cheby", F),
        ],
    );
    let f = pkg.at("src/f.cheby");
    let clean = session(&pkg).analyze_live(&Overlay::default()).unwrap();
    let clean = clean.report.unwrap();
    let expected = pairs(clean.files.get(&f).unwrap());
    assert_ne!(expected.len(), 0, "F has a type error");
    let mut s = session(&pkg);
    let live = s
        .analyze_live(&open(&pkg, &[("src/b.cheby", B_BROKEN, 1)]))
        .unwrap();
    let report = strict(&mut s, live);
    assert_eq!(pairs(report.files.get(&f).unwrap()), expected);
    assert_eq!(
        all_codes(&pkg, &report.files).get("src/main.cheby"),
        Some(&Vec::new())
    );
}

#[test]
fn no_false_errors_from_a_broken_module() {
    // The mutant loses `h` and does not parse: a run on it reports a false
    // E0042 in `main`, a run on B's last good text does not.
    let b = "//! B.\n\npub fn g() -> Int {\n  2\n}\n\npub fn h() -> Int {\n  3\n}\n";
    let mutant = "//! B.\n\npub fn g() -> Int {\n  2\n}\n\n)\n";
    let main = "//! Root.\n\nimport app::b\nimport app::f\n\n\
                fn main() {\n  let _ = b::g() + b::h() + f::f()\n}\n";
    let pkg = package(
        "false_errors",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", main),
            ("src/b.cheby", b),
            ("src/f.cheby", F),
        ],
    );
    let main_path = pkg.at("src/main.cheby");
    let clean = session(&pkg).analyze_live(&Overlay::default()).unwrap();
    let clean = clean.report.unwrap();
    let mut s = session(&pkg);
    let live = s
        .analyze_live(&open(&pkg, &[("src/b.cheby", mutant, 1)]))
        .unwrap();
    let snapshot = live.snapshot.clone().unwrap();
    let false_error = snapshot.baseline().iter().any(|(code, path, _)| {
        *code == codes::UNRESOLVED_NAME && path.as_deref() == Some(main_path.as_path())
    });
    assert!(false_error, "the Q run reports the false E0042");
    let report = strict(&mut s, live);
    for path in [pkg.at("src/main.cheby"), pkg.at("src/f.cheby")] {
        assert_eq!(
            pairs(report.files.get(&path).unwrap()),
            pairs(clean.files.get(&path).unwrap()),
            "{}",
            path.display()
        );
    }
}

#[test]
fn stub_holds_dependents_only() {
    // B never parsed: F imports it and `main` imports F, so both are held.
    // G does not depend on B and keeps its real type error.
    let f = "//! F.\n\nimport app::b\n\npub fn f() -> Int {\n  b::g()\n}\n";
    let g = "//! G.\n\npub fn g() -> Int {\n  \"x\"\n}\n";
    let main = "//! Root.\n\nimport app::f\nimport app::g\n\n\
                fn main() {\n  let _ = f::f() + g::g()\n}\n";
    let pkg = package(
        "stub_holds",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", main),
            ("src/b.cheby", B_BROKEN),
            ("src/f.cheby", f),
            ("src/g.cheby", g),
        ],
    );
    let mut s = session(&pkg);
    let live = s.analyze_live(&Overlay::default()).unwrap();
    assert!(live.phase_a.contains_key(&pkg.at("src/b.cheby")));
    let report = strict(&mut s, live);
    assert_eq!(
        relative(&pkg, &report.held),
        BTreeSet::from(["src/f.cheby".to_owned(), "src/main.cheby".to_owned()])
    );
    let codes = all_codes(&pkg, &report.files);
    assert_eq!(
        codes.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([MANIFEST_FILE.to_owned(), "src/g.cheby".to_owned()])
    );
    assert_ne!(codes.get("src/g.cheby").unwrap().len(), 0);
    assert!(report.substituted);
    assert!(!report.clean);
}

#[test]
fn stub_drops_w0005() {
    // `helper` is used only by B. With B gone, W0005 flags it; with B a
    // stub, the D run drops W0005.
    let g = "//! G.\n\npub fn used() -> Int {\n  1\n}\n\nfn helper() -> Int {\n  2\n}\n";
    let b = "//! B.\n\nimport app::g\n\npub fn b() -> Int {\n  g::helper(\n";
    let main = "//! Root.\n\nimport app::g\n\nfn main() {\n  let _ = g::used()\n}\n";
    let without_b = package(
        "w0005_control",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", main),
            ("src/g.cheby", g),
        ],
    );
    let control = session(&without_b)
        .analyze_live(&Overlay::default())
        .unwrap()
        .report
        .unwrap();
    assert_eq!(
        all_codes(&without_b, &control.files).get("src/g.cheby"),
        Some(&vec!["W0005".to_owned()])
    );
    let pkg = package(
        "w0005_stub",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", main),
            ("src/g.cheby", g),
            ("src/b.cheby", b),
        ],
    );
    let mut s = session(&pkg);
    let live = s.analyze_live(&Overlay::default()).unwrap();
    let report = strict(&mut s, live);
    assert_eq!(report.held.len(), 0);
    assert_eq!(
        all_codes(&pkg, &report.files).get("src/g.cheby"),
        Some(&Vec::new())
    );
    assert!(report.substituted);
}

#[test]
fn cycle_keeps_e0040_in_report_and_types_in_snapshot() {
    let a = "//! A.\n\nimport app::b\n\npub fn f() -> Int {\n  1\n}\n\n\
             fn uses_b() -> Int {\n  b::g()\n}\n";
    let b = "//! B.\n\nimport app::a\n\npub fn g() -> Int {\n  2\n}\n\n\
             fn uses_a() -> Int {\n  a::f()\n}\n";
    let main = "//! Root.\n\nimport app::a\n\nfn main() {\n  let _ = a::f()\n}\n";
    let pkg = package(
        "cycle",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", main),
            ("src/a.cheby", a),
            ("src/b.cheby", b),
        ],
    );
    let mut s = session(&pkg);
    let live = s.analyze_live(&Overlay::default()).unwrap();
    assert!(live.report.is_none());
    let snapshot = live.snapshot.clone().unwrap();
    assert_ne!(snapshot.status().cut_imports.len(), 0);
    for module in snapshot.local_modules() {
        assert!(module.thir().is_some(), "{:?}", module.path());
    }
    let report = strict(&mut s, live);
    let codes: Vec<String> = all_codes(&pkg, &report.files)
        .into_values()
        .flatten()
        .collect();
    assert_ne!(codes.len(), 0);
    assert!(codes.iter().all(|c| c == "E0040"), "{codes:?}");
    assert!(!report.substituted);
    assert!(!report.clean);
}

#[test]
fn validate_error_counts_as_broken() {
    let b = "//! B.\n\npub fn g() -> Int {\n  2\n}\n\n\
             pub fn sum((a, b): (Int, Int)) -> Int {\n  a + b\n}\n";
    let pkg = package(
        "validate",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN),
            ("src/b.cheby", B),
        ],
    );
    let b_path = pkg.at("src/b.cheby");
    for disk in [false, true] {
        let mut s = session(&pkg);
        let overlay = if disk {
            // Never parsed: a stub.
            std::fs::write(&b_path, b).unwrap();
            Overlay::default()
        } else {
            // Its last good text is the disk's.
            open(&pkg, &[("src/b.cheby", b, 1)])
        };
        let live = s.analyze_live(&overlay).unwrap();
        assert!(live.report.is_none());
        assert_eq!(
            codes_of(live.phase_a.get(&b_path).unwrap()),
            vec!["E0021".to_owned()]
        );
        let report = strict(&mut s, live);
        assert!(!report.files.contains_key(&b_path));
        let codes: Vec<String> = all_codes(&pkg, &report.files)
            .into_values()
            .flatten()
            .collect();
        assert!(!codes.contains(&"E0021".to_owned()), "{codes:?}");
    }
}

#[test]
fn manifest_broken_keeps_last_good() {
    let pkg = package(
        "manifest_broken",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN),
            ("src/b.cheby", B),
        ],
    );
    let manifest = pkg.at(MANIFEST_FILE);
    let mut s = session(&pkg);
    let first = s.analyze_live(&Overlay::default()).unwrap();
    assert!(first.report.is_some());
    assert!(!first.snapshot.unwrap().status().manifest_is_last_good);
    for (text, version) in [(MANIFEST_UNUSABLE, 2), (MANIFEST_USABLE, 3)] {
        // `main` changes each time, so the strict inputs move.
        let main = MAIN.replace("b::g()", &format!("b::g() + {version}"));
        let live = s
            .analyze_live(&open(
                &pkg,
                &[
                    (MANIFEST_FILE, text, version),
                    ("src/main.cheby", &main, version),
                ],
            ))
            .unwrap();
        assert!(live.report.is_none());
        let snapshot = live
            .snapshot
            .as_ref()
            .expect("the Q run runs on a manifest");
        // Only the unusable manifest is replaced by the last good one.
        assert_eq!(snapshot.status().manifest_is_last_good, version == 2);
        let shown = live.phase_a.get(&manifest).unwrap();
        assert_eq!(&*shown.text, text);
        assert_eq!(shown.version, Some(version));
        assert_ne!(shown.diagnostics.len(), 0);
        assert!(codes_of(shown).iter().all(|c| c == "E0036"));
        let report = strict(&mut s, live);
        assert_eq!(
            relative(&pkg, report.files.keys()),
            BTreeSet::from(["src/b.cheby".to_owned(), "src/main.cheby".to_owned()])
        );
        assert!(report.substituted);
        assert!(!report.clean);
    }
}

#[test]
fn manifest_never_good_is_syntax_only() {
    let pkg = package(
        "manifest_never_good",
        &[
            (MANIFEST_FILE, MANIFEST_UNUSABLE),
            ("src/main.cheby", MAIN),
            ("src/b.cheby", B),
        ],
    );
    let mut s = session(&pkg);
    let live = s
        .analyze_live(&open(
            &pkg,
            &[("src/b.cheby", B_BROKEN, 1), ("src/main.cheby", MAIN, 1)],
        ))
        .unwrap();
    assert!(live.snapshot.is_none());
    assert!(live.report.is_none() && live.plan.is_none() && live.failed.is_none());
    let codes = all_codes(&pkg, &live.phase_a);
    assert_eq!(
        codes.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            MANIFEST_FILE.to_owned(),
            "src/b.cheby".to_owned(),
            "src/main.cheby".to_owned(),
        ])
    );
    assert_eq!(codes.get(MANIFEST_FILE), Some(&vec!["E0036".to_owned()]));
    assert_eq!(codes.get("src/main.cheby"), Some(&Vec::new()));
    assert_eq!(
        pairs(live.phase_a.get(&pkg.at("src/b.cheby")).unwrap()),
        phase_a_of("src/b.cheby", B_BROKEN)
    );
}

#[test]
fn example_030_reports_e0036_only() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/030_shop_service");
    let mut s = Session::new(SessionConfig {
        root: std::path::absolute(&root).unwrap(),
        kind: PackageKind::Package,
        target: None,
        globals: Arc::new(Globals::new()),
    });
    let live = s.analyze_live(&Overlay::default()).unwrap();
    assert!(live.snapshot.is_some());
    assert!(live.report.is_none() && live.plan.is_none() && live.failed.is_none());
    let codes: Vec<(PathBuf, Vec<String>)> = live
        .phase_a
        .iter()
        .map(|(path, report)| (path.clone(), codes_of(report)))
        .collect();
    assert_eq!(codes.len(), 1, "{codes:?}");
    let (path, list) = codes.first().unwrap();
    assert!(path.ends_with(MANIFEST_FILE));
    assert!(
        !list.is_empty() && list.iter().all(|c| c == "E0036"),
        "{list:?}"
    );
}

#[test]
fn deleted_file_leaves_report() {
    let lone = "//! Lone.\n\npub fn lone() -> Int {\n  3\n}\n";
    let pkg = package(
        "deleted",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN),
            ("src/b.cheby", B),
            ("src/lone.cheby", lone),
        ],
    );
    let lone_path = pkg.at("src/lone.cheby");
    let mut s = session(&pkg);
    let live = s.analyze_live(&Overlay::default()).unwrap();
    assert!(live.files.contains(&lone_path));
    assert!(live.report.unwrap().files.contains_key(&lone_path));
    std::fs::remove_file(&lone_path).unwrap();
    s.invalidate(std::slice::from_ref(&lone_path));
    let live = s.analyze_live(&Overlay::default()).unwrap();
    assert!(!live.files.contains(&lone_path));
    assert!(!live.report.unwrap().files.contains_key(&lone_path));
}

#[test]
fn overlay_only_file_is_a_module() {
    let pkg = package(
        "overlay_only",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN),
            ("src/b.cheby", B),
        ],
    );
    let fresh = "//! Fresh.\n\npub fn fresh() -> Int {\n  \"x\"\n}\n";
    let fresh_path = pkg.at("src/fresh.cheby");
    let mut s = session(&pkg);
    let live = s
        .analyze_live(&open(&pkg, &[("src/fresh.cheby", fresh, 7)]))
        .unwrap();
    assert!(live.files.contains(&fresh_path));
    let snapshot = live.snapshot.clone().unwrap();
    let file = snapshot.file_of(&fresh_path).unwrap();
    assert!(
        snapshot
            .module_at(file)
            .is_some_and(super::ModuleView::is_local)
    );
    let report = live.report.unwrap();
    let shown = report.files.get(&fresh_path).unwrap();
    assert_eq!(shown.version, Some(7));
    assert_ne!(shown.diagnostics.len(), 0, "the type error is reported");
}

#[test]
fn closed_file_reads_disk() {
    let pkg = package(
        "closed",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN),
            ("src/b.cheby", B),
        ],
    );
    let b = pkg.at("src/b.cheby");
    let mut s = session(&pkg);
    let typed = "//! B.\n\npub fn g() -> Int {\n  \"x\"\n}\n";
    let live = s
        .analyze_live(&open(&pkg, &[("src/b.cheby", typed, 2)]))
        .unwrap();
    let report = live.report.unwrap();
    let shown = report.files.get(&b).unwrap();
    assert_eq!((shown.origin, shown.version), (TextOrigin::Editor, Some(2)));
    assert_ne!(shown.diagnostics.len(), 0);
    let live = s.analyze_live(&Overlay::default()).unwrap();
    let report = live.report.unwrap();
    let shown = report.files.get(&b).unwrap();
    assert_eq!((shown.origin, shown.version), (TextOrigin::Disk, None));
    assert_eq!(&*shown.text, B);
    assert_eq!(shown.diagnostics.len(), 0);
}

#[test]
fn ice_forces_a_d_run() {
    let util =
        "//! Util.\n\npub fn ok() -> Int {\n  1\n}\n\npub fn ice_by_test() -> Int {\n  2\n}\n";
    let main = "//! Root.\n\nimport app::util\n\nfn main() {\n  let _ = util::ok()\n}\n";
    let pkg = package(
        "ice_d_run",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", main),
            ("src/util.cheby", util),
        ],
    );
    let mut s = session(&pkg);
    let live = s.analyze_live(&Overlay::default()).unwrap();
    assert!(live.snapshot.is_some(), "the Q run contains the Ice");
    assert!(live.report.is_none());
    assert!(live.plan.is_some());
    // The D run has no containment: it fails as `cheby check` does.
    let plan = live.plan.unwrap();
    assert!(s.analyze_strict(plan).is_err());
    // A failed D run is not recorded, so the next run plans it again.
    let again = s.analyze_live(&Overlay::default()).unwrap();
    assert!(again.plan.is_some());
}

#[test]
fn failed_q_run_still_plans() {
    let lone =
        "//! Lone.\n\npub fn lone() -> Int {\n  3\n}\n\nfn panic_by_test() -> Int {\n  0\n}\n";
    let pkg = package(
        "failed_q",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN),
            ("src/b.cheby", B),
            ("src/lone.cheby", lone),
        ],
    );
    let b = pkg.at("src/b.cheby");
    let mut s = session(&pkg);
    let live = s
        .analyze_live(&open(&pkg, &[("src/b.cheby", B_BROKEN, 1)]))
        .unwrap();
    assert!(live.failed.is_some());
    assert!(live.snapshot.is_none());
    assert!(live.report.is_none());
    assert_eq!(
        pairs(live.phase_a.get(&b).unwrap()),
        phase_a_of("src/b.cheby", B_BROKEN)
    );
    assert!(live.plan.is_some());
}

#[test]
fn phase_a_holds_no_tolerant_diagnostic() {
    // B has a type error in `g`, which parses, and a grammar error in `h`.
    let broken = "//! B.\n\npub fn g() -> Int {\n  \"x\"\n}\n\npub fn h() -> Int {\n  (\n}\n";
    let pkg = package(
        "no_tolerant",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN),
            ("src/b.cheby", B),
        ],
    );
    let b = pkg.at("src/b.cheby");
    let mut s = session(&pkg);
    let live = s
        .analyze_live(&open(&pkg, &[("src/b.cheby", broken, 1)]))
        .unwrap();
    let own = phase_a_of("src/b.cheby", broken);
    assert_ne!(own.len(), 0);
    assert_eq!(pairs(live.phase_a.get(&b).unwrap()), own);
    let tolerant: Vec<_> = live
        .snapshot
        .as_ref()
        .unwrap()
        .baseline()
        .iter()
        .filter(|(_, path, _)| path.as_deref() == Some(b.as_path()))
        .filter(|(code, _, message)| {
            !own.iter()
                .any(|(c, m)| *c == code.to_string() && m == message)
        })
        .collect();
    assert_ne!(tolerant.len(), 0, "the Q run reports more for B");
}

#[test]
fn clean_is_false_while_broken() {
    let pkg = package(
        "clean_false",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN),
            ("src/b.cheby", B),
        ],
    );
    let mut s = session(&pkg);
    let live = s.analyze_live(&Overlay::default()).unwrap();
    assert!(live.report.unwrap().clean);
    let mut s = session(&pkg);
    let live = s
        .analyze_live(&open(&pkg, &[("src/b.cheby", B_BROKEN, 1)]))
        .unwrap();
    let report = strict(&mut s, live);
    assert!(
        report.files.values().all(|f| f
            .diagnostics
            .iter()
            .all(|d| d.severity != super::Severity::Error)),
        "the D run on B's last good text has no error"
    );
    assert!(!report.clean);
    // B turns broken right after a clean run: no D run and no report
    // follow, so the latest report stays clean, and `Live::broken` is what
    // says the package is not.
    let mut s = session(&pkg);
    assert!(
        s.analyze_live(&Overlay::default())
            .unwrap()
            .report
            .unwrap()
            .clean
    );
    let live = s
        .analyze_live(&open(&pkg, &[("src/b.cheby", B_BROKEN, 1)]))
        .unwrap();
    assert!(live.report.is_none() && live.plan.is_none());
    assert_eq!(
        relative(&pkg, &live.broken),
        BTreeSet::from(["src/b.cheby".to_owned()])
    );
}

/// A main that imports B and C, with a clean and a broken C.
const MAIN_BC: &str =
    "//! Root.\n\nimport app::b\nimport app::c\n\nfn main() {\n  let _ = b::g() + c::h()\n}\n";
const C: &str = "//! C.\n\npub fn h() -> Int {\n  3\n}\n";
const C_BROKEN: &str = "//! C.\n\npub fn h() -> Int {\n  3 +\n";

/// A §2.3 "B becomes clean": B goes back to its last good text while C
/// stays broken. The strict key does not move, since B's strict text is
/// the same, but B's phase-A list must not stay: a D run reports it.
#[test]
fn reverted_file_gets_a_fresh_list() {
    let pkg = package(
        "reverted",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN_BC),
            ("src/b.cheby", B),
            ("src/c.cheby", C),
        ],
    );
    let b = pkg.at("src/b.cheby");
    let c = pkg.at("src/c.cheby");
    // With and without a D run before B goes back.
    for d_run in [false, true] {
        let mut s = session(&pkg);
        assert!(
            s.analyze_live(&Overlay::default())
                .unwrap()
                .report
                .is_some()
        );
        if d_run {
            s.forget_strict_key();
        }
        let live = s
            .analyze_live(&open(&pkg, &[("src/b.cheby", B_BROKEN, 1)]))
            .unwrap();
        assert_eq!(live.plan.is_some(), d_run);
        if let Some(plan) = live.plan {
            let _ = s.analyze_strict(plan).unwrap();
        }
        let both = [("src/b.cheby", B_BROKEN, 1), ("src/c.cheby", C_BROKEN, 1)];
        let live = s.analyze_live(&open(&pkg, &both)).unwrap();
        if let Some(plan) = live.plan {
            let _ = s.analyze_strict(plan).unwrap();
        }
        let back = [("src/b.cheby", B, 2), ("src/c.cheby", C_BROKEN, 1)];
        let live = s.analyze_live(&open(&pkg, &back)).unwrap();
        assert!(!live.phase_a.contains_key(&b), "d_run {d_run}");
        assert_eq!(
            relative(&pkg, &live.broken),
            BTreeSet::from(["src/c.cheby".to_owned()])
        );
        let report = strict(&mut s, live);
        let shown = report.files.get(&b).expect("B is reported");
        assert_eq!((shown.version, shown.diagnostics.len()), (Some(2), 0));
        assert!(!report.files.contains_key(&c));
        // Typing on in C costs no D run again.
        let typing = [("src/b.cheby", B, 2), ("src/c.cheby", C_BROKEN_2, 2)];
        let live = s.analyze_live(&open(&pkg, &typing)).unwrap();
        assert!(live.plan.is_none(), "d_run {d_run}");
    }
}

const C_BROKEN_2: &str = "//! C.\n\npub fn h() -> Int {\n  3 + 4\n";

/// A §2.4 with A §2.3: F imports a stub, so it is held. F breaks, which
/// publishes its phase-A list, and is fixed. Its list must not stay, so
/// the D run gives it an empty one.
#[test]
fn held_file_fixed_gets_an_empty_list() {
    let f = "//! F.\n\nimport app::b\n\npub fn f() -> Int {\n  b::g()\n}\n";
    let f_broken = "//! F.\n\nimport app::b\n\npub fn f() -> Int {\n  b::g(\n";
    let main = "//! Root.\n\nimport app::f\n\nfn main() {\n  let _ = f::f()\n}\n";
    let pkg = package(
        "held_fixed",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", main),
            ("src/b.cheby", B_BROKEN),
            ("src/f.cheby", f),
        ],
    );
    let fp = pkg.at("src/f.cheby");
    let mut s = session(&pkg);
    let live = s.analyze_live(&Overlay::default()).unwrap();
    let report = strict(&mut s, live);
    assert_eq!(
        relative(&pkg, &report.held),
        BTreeSet::from(["src/f.cheby".to_owned(), "src/main.cheby".to_owned()])
    );
    let live = s
        .analyze_live(&open(&pkg, &[("src/f.cheby", f_broken, 1)]))
        .unwrap();
    assert_ne!(live.phase_a.get(&fp).unwrap().diagnostics.len(), 0);
    if let Some(plan) = live.plan {
        let _ = s.analyze_strict(plan).unwrap();
    }
    let live = s
        .analyze_live(&open(&pkg, &[("src/f.cheby", f, 2)]))
        .unwrap();
    let report = strict(&mut s, live);
    assert_eq!(
        relative(&pkg, &report.held),
        BTreeSet::from(["src/main.cheby".to_owned()])
    );
    let shown = report.files.get(&fp).expect("F gets a list");
    assert_eq!((shown.version, shown.diagnostics.len()), (Some(2), 0));
}

/// B was a stub, and the user empties it while C is broken. B's strict
/// text is the empty text both times, but the run with a stub held its
/// importers and dropped W0005, so a D run is due. The strict key's stub
/// mark and B leaving the broken set each plan it.
#[test]
fn emptied_stub_plans_a_d_run() {
    let main = "//! Root.\n\nimport app::c\n\nfn main() {\n  let _ = c::h()\n}\n";
    let pkg = package(
        "stub_mark",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", main),
            ("src/b.cheby", B_BROKEN),
            ("src/c.cheby", C),
        ],
    );
    let mut s = session(&pkg);
    let live = s.analyze_live(&Overlay::default()).unwrap();
    let _ = strict(&mut s, live);
    let live = s
        .analyze_live(&open(
            &pkg,
            &[("src/b.cheby", "", 1), ("src/c.cheby", C_BROKEN, 1)],
        ))
        .unwrap();
    assert!(live.plan.is_some());
}

/// D-1356: the last good text is the newest clean live text, not the disk
/// text. B gains `k` in the editor, then breaks, and main starts to call
/// `k`: the D run sees `k`.
#[test]
fn last_good_is_the_newest_live_text() {
    let b2 = "//! B.\n\npub fn g() -> Int {\n  2\n}\n\npub fn k() -> Int {\n  4\n}\n";
    let b2_broken = "//! B.\n\npub fn g() -> Int {\n  2\n}\n\npub fn k() -> Int {\n  4\n";
    let main = "//! Root.\n\nimport app::b\n\nfn main() {\n  let _ = b::g() + b::k()\n}\n";
    let pkg = package(
        "newest_live",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", MAIN),
            ("src/b.cheby", B),
        ],
    );
    let mut s = session(&pkg);
    let live = s
        .analyze_live(&open(&pkg, &[("src/b.cheby", b2, 1)]))
        .unwrap();
    assert!(live.report.is_some());
    let live = s
        .analyze_live(&open(
            &pkg,
            &[("src/b.cheby", b2_broken, 2), ("src/main.cheby", main, 1)],
        ))
        .unwrap();
    let report = strict(&mut s, live);
    let shown = report.files.get(&pkg.at("src/main.cheby")).unwrap();
    assert_eq!(codes_of(shown), Vec::<String>::new());
}

/// A §2.3: a label or an edit of a D run that points into a file on its
/// last good text is dropped, since the client cannot place it. The same
/// diagnostic on a clean run has one.
#[test]
fn label_into_last_good_is_dropped() {
    let b = "//! B.\n\npub fn g(x: Int) -> Int {\n  x\n}\n";
    let b_broken = "//! B.\n\npub fn g(x: Int) -> Int {\n  x\n";
    let main = "//! Root.\n\nimport app::b\n\nfn main() {\n  let _ = b::g(\"s\", 2)\n}\n";
    let pkg = package(
        "label_last_good",
        &[
            (MANIFEST_FILE, MANIFEST),
            ("src/main.cheby", main),
            ("src/b.cheby", b),
        ],
    );
    let bp = pkg.at("src/b.cheby");
    let into_b = |report: &Report| {
        let main = report.files.get(&pkg.at("src/main.cheby")).unwrap();
        assert_ne!(main.diagnostics.len(), 0);
        main.diagnostics
            .iter()
            .flat_map(|d| {
                d.related
                    .iter()
                    .map(|l| &l.path)
                    .chain(d.helps.iter().flat_map(|h| h.edits.iter().map(|e| &e.path)))
            })
            .filter(|path| **path == bp)
            .count()
    };
    let clean = session(&pkg)
        .analyze_live(&Overlay::default())
        .unwrap()
        .report
        .unwrap();
    assert_ne!(into_b(&clean), 0, "the clean run points into B");
    let mut s = session(&pkg);
    let live = s
        .analyze_live(&open(&pkg, &[("src/b.cheby", b_broken, 1)]))
        .unwrap();
    let report = strict(&mut s, live);
    assert_eq!(into_b(&report), 0);
}
