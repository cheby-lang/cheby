//! Tests of single-file mode (D-1371, A §6.4): a file outside every
//! package gets all of a run's diagnostics but W0005, and a file inside a
//! package that is none of its modules gets its phase-A list only.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::SourceMap;
use cheby_iface::Globals;
use cheby_package::MANIFEST_FILE;

use super::{FileReport, Overlay, OverlayFile, PackageKind, Report, Session, SessionConfig, codes};
use crate::parse_file;

const CLEAN: &str = "//! A script.\n\nimport std::io\n\nfn main() {\n  io::println(\"hi\")\n}\n";
/// A type error in a body.
const TYPED: &str = "//! A script.\n\npub fn f() -> Int {\n  \"x\"\n}\n";
/// An unclosed body: a grammar error.
const BROKEN: &str = "//! A script.\n\nfn main() {\n  let _ = 1\n";
/// `helper` is used by nothing, which W0005 reports in a package.
const UNUSED: &str =
    "//! A script.\n\nfn helper() -> Int {\n  2\n}\n\nfn main() {\n  let _ = 1\n}\n";

/// A directory in the temporary directory, removed on drop.
struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Temp {
    /// A fresh directory with these files.
    fn new(name: &str, files: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!("cheby_single_{}_{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        for (path, text) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        Self(std::path::absolute(dir).unwrap())
    }

    fn at(&self, file: &str) -> PathBuf {
        self.0.join(file)
    }
}

/// A single-file session over `file`.
fn single(file: &Path, strict: bool) -> Session {
    Session::new(SessionConfig {
        root: file.parent().unwrap().to_path_buf(),
        kind: PackageKind::SingleFile {
            file: file.to_path_buf(),
            strict,
        },
        target: None,
        globals: Arc::new(Globals::new()),
    })
}

/// `file` open with `text` at `version`.
fn open(file: &Path, text: &str, version: i32) -> Overlay {
    let open = OverlayFile {
        text: Arc::from(text),
        version,
        stamp: 1,
    };
    Overlay {
        files: [(file.to_path_buf(), open)].into(),
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

/// The one file of `report`, which must be `file`.
fn only<'a>(report: &'a Report, file: &Path) -> &'a FileReport {
    let paths: Vec<&PathBuf> = report.files.keys().collect();
    assert_eq!(paths, [file]);
    report.files.get(file).unwrap()
}

/// `text`'s own phase-A diagnostics, as code and message.
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

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn loose_clean_file_reports_an_empty_list() {
    let dir = Temp::new("clean", &[]);
    let file = dir.at("script.cheby");
    assert_eq!(crate::find_package_root(&dir.0), None, "a package above");
    let mut s = single(&file, true);
    let live = s.analyze_live(&open(&file, CLEAN, 3)).unwrap();
    assert_eq!(live.files, BTreeSet::from([file.clone()]));
    assert!(live.phase_a.is_empty());
    assert!(live.plan.is_none());
    let report = live.report.as_ref().expect("the Q run is the check");
    let shown = only(report, &file);
    assert_eq!(codes_of(shown), Vec::<String>::new());
    assert_eq!(shown.version, Some(3));
    // `cheby check` refuses a loose file, and there is no child (A §6.4).
    assert!(!report.clean);
    // The snapshot serves queries on the file, and names no manifest.
    let snapshot = live.snapshot.unwrap();
    assert_eq!(snapshot.version_of(&file), Some(3));
    assert_eq!(snapshot.version_of(&dir.at(MANIFEST_FILE)), None);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn loose_type_error_is_reported() {
    let dir = Temp::new("typed", &[("script.cheby", TYPED)]);
    let file = dir.at("script.cheby");
    let mut s = single(&file, true);
    // Closed: the text on disk.
    let live = s.analyze_live(&Overlay::default()).unwrap();
    let report = live.report.as_ref().unwrap();
    let shown = only(report, &file);
    assert_eq!(codes_of(shown), [codes::TYPE_MISMATCH.to_string()]);
    assert_eq!(shown.version, None);
    assert!(!report.clean);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn loose_file_drops_w0005() {
    // The same text as a package's root module gets W0005.
    let pkg = Temp::new(
        "w0005_pkg",
        &[
            (MANIFEST_FILE, "name = \"app\"\nversion = \"0.1.0\"\n"),
            ("src/main.cheby", UNUSED),
        ],
    );
    let mut package = Session::new(SessionConfig {
        root: pkg.0.clone(),
        kind: PackageKind::Package,
        target: None,
        globals: Arc::new(Globals::new()),
    });
    let live = package.analyze_live(&Overlay::default()).unwrap();
    let main = live.report.unwrap().files.remove(&pkg.at("src/main.cheby"));
    let w0005 = codes::UNUSED_PACKAGE_ITEM.to_string();
    assert_eq!(codes_of(&main.unwrap()), [w0005]);
    let dir = Temp::new("w0005", &[]);
    let file = dir.at("helper.cheby");
    let mut s = single(&file, true);
    let live = s.analyze_live(&open(&file, UNUSED, 1)).unwrap();
    let report = live.report.unwrap();
    assert_eq!(codes_of(only(&report, &file)), Vec::<String>::new());
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn loose_broken_file_shows_phase_a_and_plans_nothing() {
    let dir = Temp::new("broken", &[]);
    let file = dir.at("script.cheby");
    let mut s = single(&file, true);
    let live = s.analyze_live(&open(&file, BROKEN, 1)).unwrap();
    assert!(live.report.is_none());
    // A D run would report no broken file, and there is no other.
    assert!(live.plan.is_none());
    assert_eq!(live.broken, BTreeSet::from([file.clone()]));
    let shown = live.phase_a.get(&file).unwrap();
    assert_eq!(pairs(shown), phase_a_of("script.cheby", BROKEN));
    assert_eq!(shown.version, Some(1));
    assert!(live.snapshot.is_some(), "the Q run still serves queries");
    // Fixed: the Q run is the check again, and the list empties.
    let live = s.analyze_live(&open(&file, CLEAN, 2)).unwrap();
    assert!(live.phase_a.is_empty());
    assert!(live.broken.is_empty());
    let report = live.report.unwrap();
    assert_eq!(codes_of(only(&report, &file)), Vec::<String>::new());
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn not_a_module_reports_phase_a_only() {
    let pkg = Temp::new(
        "not_module",
        &[
            (MANIFEST_FILE, "name = \"app\"\nversion = \"0.1.0\"\n"),
            (
                "src/main.cheby",
                "//! Root.\n\nfn main() {\n  let _ = 1\n}\n",
            ),
        ],
    );
    let file = pkg.at("notes.cheby");
    let mut s = single(&file, false);
    // A type error and an import of a module the file cannot reach: the
    // run sees both, and neither is reported (D-1371).
    let typed = "//! Notes.\n\nimport app::geo\n\nfn main() {\n  let x: Int = \"no\"\n}\n";
    let live = s.analyze_live(&open(&file, typed, 4)).unwrap();
    assert!(live.plan.is_none());
    assert!(live.phase_a.is_empty());
    let report = live.report.unwrap();
    let shown = only(&report, &file);
    assert_eq!(codes_of(shown), Vec::<String>::new());
    assert_eq!(shown.version, Some(4));
    assert!(!report.clean);
    // A grammar error is reported.
    let live = s.analyze_live(&open(&file, BROKEN, 5)).unwrap();
    assert!(live.plan.is_none());
    let report = live.report.unwrap();
    let shown = only(&report, &file);
    assert_eq!(pairs(shown), phase_a_of("notes.cheby", BROKEN));
    assert_ne!(pairs(shown), Vec::new());
    assert!(!report.clean);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn contained_ice_plans_a_d_run_on_the_live_text() {
    let text =
        "//! A script.\n\nfn ice_by_test() -> Int {\n  1\n}\n\nfn main() {\n  let _ = 1\n}\n";
    let dir = Temp::new("ice", &[]);
    let file = dir.at("script.cheby");
    let mut s = single(&file, true);
    let live = s.analyze_live(&open(&file, text, 1)).unwrap();
    assert!(live.snapshot.is_some(), "the Q run contains the Ice");
    assert!(live.report.is_none());
    assert!(live.phase_a.is_empty());
    // The D run has no containment, so it fails as a package's does, and
    // the next run plans it again.
    let plan = live.plan.expect("a D run is planned");
    assert!(!plan.substituted());
    assert!(s.analyze_strict(plan).is_err());
    let again = s.analyze_live(&open(&file, text, 2)).unwrap();
    assert!(again.plan.is_some());
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn loose_d_run_names_no_manifest() {
    // The Q run cuts the self-import, so the D run is the check. `helper`
    // is used by nothing, but E0040 stops W0005 there as in a package.
    let text = "//! A script.\n\nimport script\n\nfn helper() -> Int {\n  2\n}\n\nfn main() {\n  let _ = 1\n}\n";
    let dir = Temp::new("d_run", &[]);
    let file = dir.at("script.cheby");
    let mut s = single(&file, true);
    let live = s.analyze_live(&open(&file, text, 1)).unwrap();
    assert!(live.report.is_none());
    let plan = live.plan.expect("a D run is planned");
    let report = s.analyze_strict(plan).unwrap();
    // The synthetic manifest is no file to publish (A §6.4).
    let codes = codes_of(only(&report, &file));
    assert!(
        codes.contains(&codes::IMPORT_CYCLE.to_string()),
        "{codes:?}"
    );
    assert!(
        !codes.contains(&codes::UNUSED_PACKAGE_ITEM.to_string()),
        "{codes:?}"
    );
    assert!(!report.clean);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "builds JIT code paths; Windows waits for M6 (D-641)"
)]
fn loose_constants_are_not_evaluated() {
    // `cheby check` gives E0075 for this constant in a package (D-1371).
    let text = "//! A script.\n\npub const boom: Int = 1 / 0\n\nfn main() {\n  let _ = boom\n}\n";
    let dir = Temp::new("consts", &[]);
    let file = dir.at("script.cheby");
    let mut s = single(&file, true);
    let live = s.analyze_live(&open(&file, text, 1)).unwrap();
    let report = live.report.expect("the Q run is the check");
    assert_eq!(codes_of(only(&report, &file)), Vec::<String>::new());
}
