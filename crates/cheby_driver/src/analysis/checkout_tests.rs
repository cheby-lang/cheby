//! Tests of a std checkout's run (D-1700, D-1701, D-1703): the
//! repository's `std/` copied to a temporary checkout, beside an empty
//! `crates/cheby_driver/src/std_lib.rs` (D-1699), and checked as package
//! `std` through a session, as the server runs it.
//!
//! Nothing here runs generated code, so every platform runs them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_iface::{Globals, PackageId};
use cheby_package::ModulePath;

use super::{
    FileLocation, Live, Overlay, OverlayFile, PackageKind, Session, SessionConfig, Snapshot,
    StdBase,
};
use crate::cache::Hash128;
use crate::std_lib;

/// The repository's `std/`.
fn repo_std() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("std")
}

/// A copy of the repository's `std/` in the temporary directory, with the
/// marker file of D-1699, removed on drop.
struct Checkout(PathBuf);

impl Drop for Checkout {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Checkout {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("cheby_checkout_{}_{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        copy_dir(&repo_std(), &dir.join("std"));
        let marker = ["crates", "cheby_driver", "src", "std_lib.rs"]
            .iter()
            .fold(dir.clone(), |path, part| path.join(part));
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, "").unwrap();
        Self(std::path::absolute(dir).unwrap())
    }

    /// The file at `display`, such as `std/json/tree.cheby`.
    fn at(&self, display: &str) -> PathBuf {
        display
            .split('/')
            .fold(self.0.clone(), |path, part| path.join(part))
    }

    fn read(&self, display: &str) -> String {
        std::fs::read_to_string(self.at(display)).unwrap()
    }

    fn write(&self, display: &str, text: &str) {
        let path = self.at(display);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn session(&self, globals: &Arc<Globals>) -> Session {
        Session::new(SessionConfig {
            root: self.0.clone(),
            kind: PackageKind::StdCheckout {
                checkout: self.0.clone(),
            },
            target: None,
            globals: Arc::clone(globals),
        })
    }

    /// An overlay with each of `files` open, by display path and text.
    fn open(&self, files: &[(&str, &str)]) -> Overlay {
        Overlay {
            files: files
                .iter()
                .zip(1_u64..)
                .map(|(&(display, text), stamp)| {
                    let file = OverlayFile {
                        text: Arc::from(text),
                        version: 1,
                        stamp,
                    };
                    (self.at(display), file)
                })
                .collect(),
        }
    }

    /// An overlay with each of `files` open with its disk text.
    fn open_as_is(&self, files: &[&str]) -> Overlay {
        let texts: Vec<(&str, String)> = files.iter().map(|&f| (f, self.read(f))).collect();
        let pairs: Vec<(&str, &str)> = texts.iter().map(|(f, t)| (*f, t.as_str())).collect();
        self.open(&pairs)
    }
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

/// Every published file of `live` by display path, with its codes in
/// order: the phase-A lists, then the report's.
fn published(checkout: &Checkout, live: &Live) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let report = live.report.iter().flat_map(|r| &r.files);
    for (path, file) in live.phase_a.iter().chain(report) {
        let display = cheby_diag::display_path(path, &checkout.0);
        let codes = file.diagnostics.iter().map(|d| d.code.to_string());
        out.entry(display).or_insert_with(Vec::new).extend(codes);
    }
    out
}

/// The files of `published` with any diagnostic.
fn noisy(published: &BTreeMap<String, Vec<String>>) -> BTreeMap<&str, &[String]> {
    published
        .iter()
        .filter(|(_, codes)| !codes.is_empty())
        .map(|(path, codes)| (path.as_str(), codes.as_slice()))
        .collect()
}

/// Whether any code of `codes` is an error's.
fn has_error(codes: &[String]) -> bool {
    codes.iter().any(|code| code.starts_with('E'))
}

/// The display path of every shipped std module.
fn shipped() -> Vec<String> {
    std_lib::STD_MODULES
        .iter()
        .map(|m| std_lib::display_path(m.path))
        .collect()
}

/// The std module at `path` under `std`.
fn std_path(path: &str) -> ModulePath {
    path.split("::")
        .fold(ModulePath::root("std"), |p, s| p.child(s))
}

fn snapshot(live: &Live) -> &Snapshot {
    live.snapshot.as_deref().expect("a snapshot")
}

/// A module with a test body that has a type error, and a test body that
/// a user package's W0011 would mark (D-479).
const TESTED: &str = "//! A module whose tests the checkout checks when it is open.\n\n\
                      /// One.\npub fn one() -> Int {\n  1\n}\n\n\
                      test \"ws typed\" {\n  assert one() == \"x\"\n}\n\n\
                      test \"ws equality\" {\n  let f = fn(x: Int) { x }\n  assert f == f\n}\n";

/// [`TESTED`] with no type error.
const TESTED_CLEAN: &str = "//! A module whose tests the checkout checks when it is open.\n\n\
                            /// One.\npub fn one() -> Int {\n  1\n}\n\n\
                            test \"ws equality\" {\n  let f = fn(x: Int) { x }\n  assert f == f\n}\n";

#[test]
fn checkout_is_clean() {
    let checkout = Checkout::new("clean");
    let globals = Arc::new(Globals::new());
    let mut session = checkout.session(&globals);
    let live = session.analyze_live(&Overlay::default()).unwrap();
    assert!(live.failed.is_none(), "{:?}", live.failed);
    assert!(live.report.is_some(), "the Q run is the check");
    let shown = published(&checkout, &live);
    assert_eq!(noisy(&shown), BTreeMap::new(), "warnings included");
    let modules: Vec<String> = shown.keys().cloned().collect();
    assert_eq!(
        modules,
        shipped(),
        "every module of the checkout is published"
    );
    // With two modules open their test bodies are checked, and are clean.
    let open = checkout.open_as_is(&["std/list.cheby", "std/json/tree.cheby"]);
    let live = session.analyze_live(&open).unwrap();
    assert_eq!(noisy(&published(&checkout, &live)), BTreeMap::new());
}

#[test]
fn checkout_reports_a_type_error() {
    let checkout = Checkout::new("typed");
    let mut session = checkout.session(&Arc::new(Globals::new()));
    let text = checkout.read("std/list.cheby") + "\nfn ws_probe() -> Int {\n  \"x\"\n}\n";
    let live = session
        .analyze_live(&checkout.open(&[("std/list.cheby", &text)]))
        .unwrap();
    let shown = published(&checkout, &live);
    let errors: Vec<&str> = shown
        .iter()
        .filter(|(_, codes)| has_error(codes))
        .map(|(path, _)| path.as_str())
        .collect();
    assert_eq!(errors, ["std/list.cheby"], "{shown:?}");
}

#[test]
fn checkout_reports_a_broken_dependent() {
    let checkout = Checkout::new("dependent");
    let mut session = checkout.session(&Arc::new(Globals::new()));
    // `std/json.cheby` calls `lists::map` with two arguments; it stays closed.
    let text = checkout.read("std/list.cheby").replace(
        "pub fn map<T, U>(xs: List<T>, f: fn(T) -> U) -> List<U> {",
        "pub fn map<T, U>(xs: List<T>, f: fn(T) -> U, ws_extra: Int) -> List<U> {",
    );
    let live = session
        .analyze_live(&checkout.open(&[("std/list.cheby", &text)]))
        .unwrap();
    let shown = published(&checkout, &live);
    assert!(has_error(&shown["std/json.cheby"]), "{shown:?}");
}

#[test]
fn checkout_grammar_error_keeps_last_good() {
    let checkout = Checkout::new("grammar");
    let mut session = checkout.session(&Arc::new(Globals::new()));
    let clean = session.analyze_live(&Overlay::default()).unwrap();
    assert!(clean.report.is_some());
    let list = checkout.at("std/list.cheby");
    let text = checkout.read("std/list.cheby") + "\nfn ws_broken( {\n";
    let live = session
        .analyze_live(&checkout.open(&[("std/list.cheby", &text)]))
        .unwrap();
    assert!(
        live.failed.is_none(),
        "no internal error: {:?}",
        live.failed
    );
    assert!(live.report.is_none(), "a broken module: not the check");
    assert_eq!(live.broken, BTreeSet::from([list.clone()]));
    let phase = &live.phase_a[&list];
    assert!(
        phase
            .diagnostics
            .iter()
            .any(|d| d.code.to_string().starts_with('E'))
    );
    // The Q run kept the broken module's tree and facts.
    let snap = snapshot(&live);
    let module = snap.module_at(snap.file_of(&list).unwrap()).unwrap();
    assert!(module.tree().is_some() && module.facts().is_some());
    // The D run reads the last good text: no dependent breaks.
    let plan = live.plan.expect("a D run on the last good text");
    assert!(plan.substituted());
    let report = session.analyze_strict(plan).unwrap();
    assert!(
        !report.files.contains_key(&list),
        "a broken file is phase A's"
    );
    let errors: Vec<&PathBuf> = report
        .files
        .iter()
        .filter(|(_, f)| {
            f.diagnostics
                .iter()
                .any(|d| d.code.to_string().starts_with('E'))
        })
        .map(|(path, _)| path)
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn checkout_new_module() {
    let checkout = Checkout::new("new");
    checkout.write(
        "std/extra.cheby",
        "//! Extra.\n\n/// One.\npub fn ws_extra() -> Int {\n  1\n}\n",
    );
    checkout.write(
        "std/extra_user.cheby",
        "//! Uses extra.\n\nimport std::extra\n\n/// Two.\npub fn ws_two() -> Int {\n  extra::ws_extra() + 1\n}\n",
    );
    let mut session = checkout.session(&Arc::new(Globals::new()));
    let live = session.analyze_live(&Overlay::default()).unwrap();
    let shown = published(&checkout, &live);
    assert_eq!(shown["std/extra.cheby"], Vec::<String>::new());
    assert_eq!(shown["std/extra_user.cheby"], Vec::<String>::new());
    let snap = snapshot(&live);
    let own: Vec<String> = snap
        .own_modules()
        .filter_map(|m| m.path().map(ToString::to_string))
        .collect();
    assert!(own.contains(&"std::extra".to_owned()), "{own:?}");
}

#[test]
fn checkout_deleted_module() {
    let checkout = Checkout::new("deleted");
    // Only `std/http.cheby` imports `std::url`.
    std::fs::remove_file(checkout.at("std/url.cheby")).unwrap();
    let mut session = checkout.session(&Arc::new(Globals::new()));
    let live = session.analyze_live(&Overlay::default()).unwrap();
    let shown = published(&checkout, &live);
    assert!(!shown.contains_key("std/url.cheby"));
    assert!(
        shown["std/http.cheby"].contains(&"E0039".to_owned()),
        "{shown:?}"
    );
}

#[test]
fn checkout_planned_module_stays_e0038() {
    let checkout = Checkout::new("planned");
    // `unshipped_probe` is planned in this crate's test builds (D-1296).
    assert!(std_lib::is_planned("unshipped_probe"));
    checkout.write(
        "std/unshipped_probe.cheby",
        "//! Planned.\n\n/// One.\npub fn one() -> Int {\n  \"x\"\n}\n",
    );
    checkout.write(
        "std/ws_user.cheby",
        "//! Uses it.\n\nimport std::unshipped_probe\n\n/// One.\npub fn ws_one() -> Int {\n  1\n}\n",
    );
    let mut session = checkout.session(&Arc::new(Globals::new()));
    let live = session.analyze_live(&Overlay::default()).unwrap();
    let shown = published(&checkout, &live);
    assert!(
        shown["std/ws_user.cheby"].contains(&"E0038".to_owned()),
        "{shown:?}"
    );
    // The planned file is no module: phase A only, so its type error is
    // not reported, and it is in the phase-A lists.
    let planned = checkout.at("std/unshipped_probe.cheby");
    assert_eq!(shown["std/unshipped_probe.cheby"], Vec::<String>::new());
    assert!(live.phase_a.contains_key(&planned));
    let snap = snapshot(&live);
    let file = snap.file_of(&planned).unwrap();
    assert!(snap.module_at(file).is_none());
}

#[test]
fn checkout_file_naming_no_module() {
    let checkout = Checkout::new("loose");
    // A name that is not a module name, a file that would name `std` itself,
    // and one with a grammar error.
    checkout.write("std/Bad.cheby", "//! Bad.\n\nfn f() -> Int {\n  \"x\"\n}\n");
    checkout.write(
        "std/main.cheby",
        "//! Main.\n\nfn f() -> Int {\n  \"x\"\n}\n",
    );
    checkout.write("std/Broken.cheby", "//! Broken.\n\nfn f( {\n");
    let mut session = checkout.session(&Arc::new(Globals::new()));
    let live = session.analyze_live(&Overlay::default()).unwrap();
    assert!(live.report.is_some(), "a loose file breaks no run");
    let shown = published(&checkout, &live);
    assert_eq!(shown["std/Bad.cheby"], Vec::<String>::new());
    assert_eq!(shown["std/main.cheby"], Vec::<String>::new());
    assert!(has_error(&shown["std/Broken.cheby"]), "{shown:?}");
    for loose in ["std/Bad.cheby", "std/main.cheby", "std/Broken.cheby"] {
        let path = checkout.at(loose);
        assert!(live.files.contains(&path), "{loose}");
        assert!(live.phase_a.contains_key(&path), "{loose}");
        assert!(!live.broken.contains(&path), "{loose}");
    }
    assert_eq!(noisy(&shown).len(), 1, "{shown:?}");
}

#[test]
fn checkout_tests_of_open_modules() {
    let checkout = Checkout::new("tests");
    checkout.write("std/ws_tested.cheby", TESTED);
    checkout.write("std/ws_closed.cheby", TESTED);
    let mut session = checkout.session(&Arc::new(Globals::new()));
    let live = session
        .analyze_live(&checkout.open_as_is(&["std/ws_tested.cheby"]))
        .unwrap();
    let shown = published(&checkout, &live);
    // The open module's test body is checked: its error is reported, and
    // no body warning of D-479 is, W0011 included.
    assert!(has_error(&shown["std/ws_tested.cheby"]), "{shown:?}");
    assert!(!shown["std/ws_tested.cheby"].contains(&"W0011".to_owned()));
    assert_eq!(shown["std/ws_closed.cheby"], Vec::<String>::new());
    // The same test body in a user package has W0011.
    let package = Checkout::new("tests_package");
    package.write("cheby.toml", "name = \"app\"\nversion = \"0.1.0\"\n");
    package.write("src/main.cheby", "//! Root.\n\nfn main() {\n  Nil\n}\n");
    package.write("src/tested.cheby", TESTED_CLEAN);
    let mut user = Session::new(SessionConfig {
        root: package.0.clone(),
        kind: PackageKind::Package,
        target: None,
        globals: Arc::new(Globals::new()),
    });
    let live = user.analyze_live(&Overlay::default()).unwrap();
    let shown = published(&package, &live);
    assert!(
        shown["src/tested.cheby"].contains(&"W0011".to_owned()),
        "{shown:?}"
    );
}

#[test]
fn checkout_test_bodies_follow_open() {
    let checkout = Checkout::new("follow");
    checkout.write("std/ws_tested.cheby", TESTED);
    let mut session = checkout.session(&Arc::new(Globals::new()));
    let tested = std_path("ws_tested");
    let open = session
        .analyze_live(&checkout.open_as_is(&["std/ws_tested.cheby"]))
        .unwrap();
    assert!(has_error(
        &published(&checkout, &open)["std/ws_tested.cheby"]
    ));
    assert_eq!(
        snapshot(&open).tested_std(),
        Some(&BTreeSet::from([tested]))
    );
    // Closed, with the same text: its test bodies are skipped.
    let closed = session.analyze_live(&Overlay::default()).unwrap();
    assert_eq!(
        published(&checkout, &closed)["std/ws_tested.cheby"],
        Vec::<String>::new()
    );
    assert_eq!(snapshot(&closed).tested_std(), Some(&BTreeSet::new()));
}

#[test]
fn checkout_facts_and_locations() {
    let checkout = Checkout::new("facts");
    let mut session = checkout.session(&Arc::new(Globals::new()));
    let live = session
        .analyze_live(&checkout.open_as_is(&["std/list.cheby"]))
        .unwrap();
    let snap = snapshot(&live);
    assert!(snap.is_std_checkout());
    let tree = checkout.at("std/json/tree.cheby");
    let file = snap.file_of(&tree).unwrap();
    assert_eq!(snap.location(file), Some(&FileLocation::Package(tree)));
    let module = snap.module_at(file).unwrap();
    assert_eq!(module.path(), Some(&std_path("json::tree")));
    assert!(module.is_own() && module.tree().is_some() && module.facts().is_some());
    let own: Vec<String> = snap
        .own_modules()
        .map(|m| {
            let file = m.file().unwrap();
            let Some(FileLocation::Package(path)) = snap.location(file) else {
                panic!("{m:?} is not a file of the checkout");
            };
            cheby_diag::display_path(path, &checkout.0)
        })
        .collect();
    let mut sorted = own;
    sorted.sort();
    assert_eq!(sorted, shipped());
    // In a package session the own modules are its local modules, and std's
    // are not.
    let package = Checkout::new("facts_package");
    package.write("cheby.toml", "name = \"app\"\nversion = \"0.1.0\"\n");
    package.write(
        "src/main.cheby",
        "//! Root.\n\nimport std::list\n\nfn main() {\n  let _ = list::length([1])\n}\n",
    );
    let mut user = Session::new(SessionConfig {
        root: package.0.clone(),
        kind: PackageKind::Package,
        target: None,
        globals: Arc::new(Globals::new()),
    });
    let live = user.analyze_live(&Overlay::default()).unwrap();
    let snap = snapshot(&live);
    assert!(!snap.is_std_checkout());
    assert_eq!(snap.tested_std(), None);
    let own: Vec<_> = snap.own_modules().map(super::ModuleView::id).collect();
    let local: Vec<_> = snap.local_modules().map(super::ModuleView::id).collect();
    assert_eq!(own, local);
    let list = snap.table().find(&std_path("list")).unwrap();
    assert!(!snap.module(list).unwrap().is_own());
    let std_file = snap.module(list).unwrap().file().unwrap();
    assert!(matches!(
        snap.location(std_file),
        Some(FileLocation::Std(_))
    ));
}

#[test]
fn checkout_does_not_reach_std_base() {
    let checkout = Checkout::new("isolated");
    let text = checkout.read("std/list.cheby")
        + "\n/// Only in the checkout.\npub fn ws_only_in_checkout() -> Int {\n  1\n}\n";
    checkout.write("std/list.cheby", &text);
    let globals = Arc::new(Globals::new());
    let mut session = checkout.session(&globals);
    let live = session.analyze_live(&Overlay::default()).unwrap();
    assert_eq!(noisy(&published(&checkout, &live)), BTreeMap::new());
    // The checkout has the function; the base and a package run do not
    // (D-1372, D-1348).
    let snap = snapshot(&live);
    let list = snap
        .module(snap.table().find(&std_path("list")).unwrap())
        .unwrap();
    let name = snap.globals().names.lookup("ws_only_in_checkout").unwrap();
    assert!(
        list.interface()
            .unwrap()
            .items
            .iter()
            .any(|i| i.name == name)
    );
    let base = StdBase::build(Arc::clone(&globals)).unwrap();
    assert_eq!(base.index().named("ws_only_in_checkout").count(), 0);
    let package = Checkout::new("isolated_package");
    package.write("cheby.toml", "name = \"app\"\nversion = \"0.1.0\"\n");
    package.write(
        "src/main.cheby",
        "//! Root.\n\nimport std::list\n\nfn main() {\n  let _ = list::ws_only_in_checkout()\n}\n",
    );
    let mut user = Session::new(SessionConfig {
        root: package.0.clone(),
        kind: PackageKind::Package,
        target: None,
        globals,
    });
    let live = user.analyze_live(&Overlay::default()).unwrap();
    assert!(has_error(&published(&package, &live)["src/main.cheby"]));
}

#[test]
fn checkout_source_hashes() {
    let checkout = Checkout::new("hashes");
    let text = checkout.read("std/list.cheby") + "\n// A change the binary does not have.\n";
    checkout.write("std/list.cheby", &text);
    let mut session = checkout.session(&Arc::new(Globals::new()));
    let live = session.analyze_live(&Overlay::default()).unwrap();
    let snap = snapshot(&live);
    let file = snap.file_of(&checkout.at("std/list.cheby")).unwrap();
    let hash = snap.bytes_hash(file).unwrap();
    assert_eq!(hash, Hash128::of(text.as_bytes()));
    assert_ne!(hash, std_lib::source_hashes("list").unwrap().sip128);
    // An unchanged module's hash is the binary's.
    let bool_file = snap.file_of(&checkout.at("std/bool.cheby")).unwrap();
    assert_eq!(
        snap.bytes_hash(bool_file),
        std_lib::source_hashes("bool").map(|h| h.sip128)
    );
}

#[test]
fn checkout_unsaved_module() {
    let checkout = Checkout::new("unsaved");
    let probe = "//! Probe.\n\nimport std::list\n\n/// A type error.\npub fn ws_probe() -> Int {\n  list::length([1]) + \"x\"\n}\n";
    let mut session = checkout.session(&Arc::new(Globals::new()));
    let live = session
        .analyze_live(&checkout.open(&[("std/ws_probe.cheby", probe)]))
        .unwrap();
    let path = checkout.at("std/ws_probe.cheby");
    assert!(!path.exists());
    let shown = published(&checkout, &live);
    assert!(has_error(&shown["std/ws_probe.cheby"]), "{shown:?}");
    let snap = snapshot(&live);
    let module = snap.module_at(snap.file_of(&path).unwrap()).unwrap();
    assert_eq!(module.path(), Some(&std_path("ws_probe")));
    assert!(module.is_own());
}

#[test]
fn checkout_synthetic_root_hidden() {
    let checkout = Checkout::new("root");
    let mut session = checkout.session(&Arc::new(Globals::new()));
    let live = session.analyze_live(&Overlay::default()).unwrap();
    let snap = snapshot(&live);
    let root = snap
        .table()
        .find(&ModulePath::root("std_checkout"))
        .expect("the synthetic root");
    let module = snap.module(root).unwrap();
    assert!(!module.is_own());
    assert_eq!(snap.table().get(root).unwrap().package, PackageId::LOCAL);
    assert!(snap.own_modules().all(|m| m.id() != root));
    let file = module.file().unwrap();
    assert_eq!(snap.location(file), Some(&FileLocation::Synthetic));
    // Its unused imports, one per module, reach no publish and no baseline.
    let shown = published(&checkout, &live);
    assert!(
        shown.values().flatten().all(|code| code != "W0003"),
        "{shown:?}"
    );
    assert_eq!(snap.baseline(), []);
    // With every module open, every test body is checked, still with no
    // diagnostic of the root.
    let every: Vec<String> = shipped();
    let names: Vec<&str> = every.iter().map(String::as_str).collect();
    let live = session.analyze_live(&checkout.open_as_is(&names)).unwrap();
    assert_eq!(snapshot(&live).baseline(), []);
}

#[test]
fn checkout_walk_follows_std() {
    let checkout = Checkout::new("walk");
    let mut session = checkout.session(&Arc::new(Globals::new()));
    let first = session.analyze_live(&Overlay::default()).unwrap();
    assert!(!published(&checkout, &first).contains_key("std/ws_new.cheby"));
    // A file that comes under `std/` with no event: a sweep sees it.
    assert!(!session.sweep(), "nothing changed yet");
    checkout.write(
        "std/ws_new.cheby",
        "//! New.\n\n/// One.\npub fn one() -> Int {\n  \"x\"\n}\n",
    );
    assert!(session.sweep());
    let swept = session.analyze_live(&Overlay::default()).unwrap();
    assert!(has_error(&published(&checkout, &swept)["std/ws_new.cheby"]));
    // A file that goes, told by an event on its path.
    std::fs::remove_file(checkout.at("std/ws_new.cheby")).unwrap();
    session.invalidate(&[checkout.at("std/ws_new.cheby")]);
    let gone = session.analyze_live(&Overlay::default()).unwrap();
    assert!(!gone.files.contains(&checkout.at("std/ws_new.cheby")));
}

/// The codes package `std` keeps out, each with its entry, in the files
/// that would show them outside package `std` (std-checkout.md §1).
const HAZARDS: &[(&str, &str, &str)] = &[
    ("std/map.cheby", "E0042", "D-447"),
    ("std/json.cheby", "E0043", "D-483"),
    ("std/heap.cheby", "E0045", "D-482"),
    ("std/fiber.cheby", "E0055", "D-455"),
];

#[test]
fn checkout_keeps_std_hazards() {
    let checkout = Checkout::new("hazards");
    let every: Vec<String> = shipped();
    let names: Vec<&str> = every.iter().map(String::as_str).collect();
    let mut session = checkout.session(&Arc::new(Globals::new()));
    // Every module open: every test body is checked (D-1701).
    let live = session.analyze_live(&checkout.open_as_is(&names)).unwrap();
    let shown = published(&checkout, &live);
    for (file, code, entry) in HAZARDS {
        assert!(
            !shown[*file].iter().any(|c| c == code),
            "{code} in {file}, which {entry} rules out: {shown:?}"
        );
    }
    // D-479: no body warning in std's test bodies, nor any other.
    assert_eq!(noisy(&shown), BTreeMap::new());
    assert_eq!(
        snapshot(&live).tested_std().map(BTreeSet::len),
        Some(every.len())
    );
}

/// The helps of every E0042 published for `path`, in order.
fn e0042_helps(live: &Live, path: &Path) -> Vec<String> {
    let report = live.report.iter().flat_map(|r| &r.files);
    live.phase_a
        .iter()
        .chain(report)
        .filter(|(file, _)| file.as_path() == path)
        .flat_map(|(_, file)| &file.diagnostics)
        .filter(|d| d.code.to_string() == "E0042")
        .flat_map(|d| d.helps.iter().map(|help| help.message.clone()))
        .collect()
}

/// Calls an item that only the checkout has and one that only the binary
/// has, each through a module that has neither.
const HELP_PROBE: &str = "//! Probe.\n\nimport std::int\n\n\
                          /// Two calls E0042 rejects.\npub fn ws_probe() -> Int {\n  \
                          int::ws_only_in_checkout() + int::frequencies([1])\n}\n";

#[test]
fn checkout_help_lists_its_own_items() {
    let checkout = Checkout::new("help");
    let bool_text = checkout.read("std/bool.cheby")
        + "\n/// Only in the checkout.\npub fn ws_only_in_checkout() -> Int {\n  1\n}\n";
    checkout.write("std/bool.cheby", &bool_text);
    // `map::frequencies` is gone from the checkout; only `std/map.cheby`'s
    // own test bodies use it, whose E0042s are no concern here.
    let map_text = checkout.read("std/map.cheby").replace(
        "pub fn frequencies<K>(",
        "pub fn ws_frequencies_renamed<K>(",
    );
    assert_ne!(map_text, checkout.read("std/map.cheby"));
    checkout.write("std/map.cheby", &map_text);
    checkout.write("std/ws_probe.cheby", HELP_PROBE);
    let mut session = checkout.session(&Arc::new(Globals::new()));
    let live = session.analyze_live(&Overlay::default()).unwrap();
    assert_eq!(
        e0042_helps(&live, &checkout.at("std/ws_probe.cheby")),
        ["`std::bool` has `ws_only_in_checkout`"],
        "{:?}",
        published(&checkout, &live)
    );
    // A package run reads the binary's std: no help for the checkout's
    // item, and one for the item the checkout removed.
    let package = Checkout::new("help_package");
    package.write("cheby.toml", "name = \"app\"\nversion = \"0.1.0\"\n");
    package.write(
        "src/main.cheby",
        &(HELP_PROBE.replace("pub fn ws_probe() -> Int", "fn main() -> Int")),
    );
    let mut user = Session::new(SessionConfig {
        root: package.0.clone(),
        kind: PackageKind::Package,
        target: None,
        globals: Arc::new(Globals::new()),
    });
    let live = user.analyze_live(&Overlay::default()).unwrap();
    assert_eq!(
        e0042_helps(&live, &package.at("src/main.cheby")),
        ["`std::map` has `frequencies`"],
        "{:?}",
        published(&package, &live)
    );
}
