//! Tests of the server's features in a std checkout's session (D-1700,
//! D-1703): the repository's `std/` copied to a temporary checkout
//! beside the marker file of D-1699, with probe modules of its own, and
//! checked as package `std` as the server runs it. Each answer must
//! treat the checkout's std modules as the session's own: their files, their
//! text, never the binary's std.
//!
//! Each test runs only the front end, so it runs on Windows too (D-1516).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cheby_driver::analysis::{
    FileLocation, Globals, ModulePath, ModuleView, Overlay, OverlayFile, PackageKind, Session,
    SessionConfig, Snapshot, StdBase, TextSize,
};
use serde_json::{Value, json};

use crate::features::handlers::completion::{CompletionCaps, resolve};
use crate::features::handlers::nav::{LinkCaps, NavEnv, definition};
use crate::features::handlers::workspace_symbol::{StdSymbols, workspace_symbol};
use crate::features::ide::completion::{Options, StdAccess, complete};
use crate::features::ide::hints::{Switches, hints};
use crate::features::ide::hover::{StdDocs, hover, source};
use crate::features::ide::imports::import_plan;
use crate::features::ide::nav::references;
use crate::features::ide::symbols::{OutlineKind, outline};
use crate::features::ide::tokens::tokens;
use crate::features::{Analyzed, CancelToken, RequestCx};
use crate::server::{ClientCaps, Settings};
use crate::text::lines::PositionEncoding;
use crate::text::uri::path_to_uri;

/// A probe module with a package-visible helper and a `priv` one.
const PROBE_A: &str = r#"//! A probe of the checkout tests: helpers another module calls.

import std::list

/// Doubles each element.
fn helper(xs: List<Int>) -> List<Int> {
  list::map(xs, secret)
}

priv fn secret(x: Int) -> Int {
  x * 2
}

test "helper doubles" {
  assert helper([1]) == [secret(1)]
}
"#;

/// A probe module that calls `PROBE_A`, `std::list` and `std::int`.
const PROBE_B: &str = "//! A probe of the checkout tests: calls into the other probe and std.

import std::int
import std::list
import std::probe_a

/// The doubled list's length, past the largest safe integer.
pub fn total(xs: List<Int>) -> Int {
  let doubled = probe_a::helper(xs)
  let mapped = list::map(doubled, fn(x) { x + 1 })
  list::length(mapped) + int::max_safe
}
";

/// The repository's `std/`.
fn repo_std() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("std")
}

/// A copy of the repository's `std/` in the temporary directory, with the
/// marker file of D-1699 and the two probes on disk, removed on drop.
pub(crate) struct Checkout(PathBuf);

impl Drop for Checkout {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Checkout {
    pub(crate) fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cheby_lsp_checkout_{}_{name}_{n}",
            std::process::id()
        ));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        copy_dir(&repo_std(), &dir.join("std"));
        let marker = ["crates", "cheby_driver", "src", "std_lib.rs"]
            .iter()
            .fold(dir.clone(), |path, part| path.join(part));
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, "").unwrap();
        let checkout = Self(std::path::absolute(dir).unwrap());
        checkout.write("std/probe_a.cheby", PROBE_A);
        checkout.write("std/probe_b.cheby", PROBE_B);
        checkout
    }

    /// The file at `display`, such as `std/json/tree.cheby`.
    pub(crate) fn at(&self, display: &str) -> PathBuf {
        display
            .split('/')
            .fold(self.0.clone(), |path, part| path.join(part))
    }

    pub(crate) fn read(&self, display: &str) -> String {
        std::fs::read_to_string(self.at(display)).unwrap()
    }

    fn write(&self, display: &str, text: &str) {
        std::fs::write(self.at(display), text).unwrap();
    }

    /// One Q run of the checkout with `open` as open documents, by display
    /// path and text, as the server runs it.
    pub(crate) fn analyze(&self, open: &[(&str, &str)]) -> Analyzed {
        let mut session = Session::new(SessionConfig {
            root: self.0.clone(),
            kind: PackageKind::StdCheckout {
                checkout: self.0.clone(),
            },
            target: None,
            globals: Arc::new(Globals::new()),
        });
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
                    (self.at(display), file)
                })
                .collect(),
        };
        let live = session.analyze_live(&overlay).expect("a run");
        let analyzed = Analyzed::new(live.snapshot.expect("a snapshot"));
        assert!(analyzed.snapshot.is_std_checkout());
        analyzed
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

/// The module of the checkout's file at `display`.
fn view<'s>(checkout: &Checkout, snapshot: &'s Snapshot, display: &str) -> ModuleView<'s> {
    let file = snapshot
        .file_of(&checkout.at(display))
        .expect("a file of the session");
    snapshot.module_at(file).expect("a module")
}

/// The offset of the `|` in `needle`, which must occur once in `text`
/// with the `|` removed.
fn offset_of(text: &str, needle: &str) -> TextSize {
    let bar = needle.find('|').expect("a cursor");
    let plain = needle.replace('|', "");
    assert_eq!(text.matches(&plain).count(), 1, "`{plain}` occurs once");
    let at = text.find(&plain).unwrap();
    TextSize::from(u32::try_from(at.checked_add(bar).unwrap()).unwrap())
}

/// `size` as an index into a text.
fn at_usize(size: TextSize) -> usize {
    usize::try_from(size).unwrap()
}

/// The path of `file` in `snapshot`, which must be a file of the session.
fn path_of(snapshot: &Snapshot, file: cheby_driver::analysis::FileId) -> PathBuf {
    match snapshot.location(file) {
        Some(FileLocation::Package(path)) => path.clone(),
        other => panic!("a std file of the checkout is the session's, not {other:?}"),
    }
}

/// Runs `f` with a request context as the server makes one.
fn with_cx<T>(f: impl FnOnce(&RequestCx<'_>) -> T) -> T {
    let settings = Settings::default();
    let caps = ClientCaps::default();
    let cancel = CancelToken::new();
    let cx = RequestCx {
        settings: &settings,
        caps: &caps,
        encoding: PositionEncoding::Utf16,
        cancel: &cancel,
        std: &crate::features::stdlib::StdService::none(),
        uris: &crate::text::uri::UriMap::new(),
        constants: None,
        reports: None,
    };
    f(&cx)
}

/// The binary's std index, built by the std base.
fn std_base() -> Arc<StdBase> {
    StdBase::build(Arc::new(Globals::new())).expect("std checks")
}

/// A directory the binary's std sources would be materialized in. Nothing
/// is read from it.
fn std_dir() -> PathBuf {
    std::env::temp_dir().join("cheby_lsp_checkout_std_src")
}

#[test]
fn checkout_outline_of_list() {
    let checkout = Checkout::new("outline");
    let text = checkout.read("std/list.cheby");
    let analyzed = checkout.analyze(&[("std/list.cheby", &text)]);
    let snapshot = &analyzed.snapshot;
    let list = view(&checkout, snapshot, "std/list.cheby");
    assert!(list.is_own(), "std::list is a module of the session");
    assert!(
        snapshot
            .own_modules()
            .any(|m| m.path().is_some_and(|p| p.to_string() == "std::list")),
        "own_modules holds std::list"
    );
    let tree = list.tree().expect("a checkout module keeps its tree");
    let entries = outline(tree.root());
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    for name in ["new", "range", "map", "fold", "length"] {
        assert!(names.contains(&name), "`{name}` in the outline: {names:?}");
    }
    let map = entries.iter().find(|e| e.name == "map").unwrap();
    assert_eq!(map.kind, OutlineKind::Function);
    let at = text.find("pub fn map<").unwrap() + "pub fn ".len();
    assert_eq!(at_usize(map.selection.start()), at);
    assert!(entries.iter().any(|e| e.kind == OutlineKind::Test));
}

#[test]
fn checkout_references_across_two_modules() {
    let checkout = Checkout::new("references");
    let analyzed = checkout.analyze(&[("std/probe_b.cheby", PROBE_B)]);
    let snapshot = &analyzed.snapshot;
    let index = analyzed.index();
    let cancel = CancelToken::new();
    let found = |display: &str, text: &str, needle: &str| -> Vec<(String, usize)> {
        let at = view(&checkout, snapshot, display);
        let answer = references(index, at, offset_of(text, needle), true, &cancel)
            .expect("no cancel")
            .expect("a symbol");
        let mut hits: Vec<(String, usize)> = answer
            .hits
            .iter()
            .map(|&(file, range)| {
                let path = path_of(snapshot, file);
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                (name, at_usize(range.start()))
            })
            .collect();
        hits.sort();
        hits
    };
    let a = |needle: &str| {
        let at = at_usize(offset_of(PROBE_A, needle));
        (String::from("probe_a.cheby"), at)
    };
    let b = |needle: &str| {
        let at = at_usize(offset_of(PROBE_B, needle));
        (String::from("probe_b.cheby"), at)
    };
    // A package-visible helper: its declaration and its uses in both modules.
    let helper = found("std/probe_b.cheby", PROBE_B, "probe_a::|helper(xs)");
    let mut expected = vec![
        a("fn |helper(xs"),
        a("assert |helper("),
        b("probe_a::|helper(xs)"),
    ];
    expected.sort();
    assert_eq!(helper, expected);
    // A `priv` helper: only in its own module.
    let secret = found("std/probe_a.cheby", PROBE_A, "priv fn |secret");
    let mut expected = vec![
        a("priv fn |secret"),
        a("list::map(xs, |secret)"),
        a("[|secret(1)]"),
    ];
    expected.sort();
    assert_eq!(secret, expected);
}

#[test]
fn checkout_hover_reads_the_checkout() {
    let checkout = Checkout::new("hover");
    // The checkout's text differs from the binary's: `list::map` lost its
    // doc comment and `int::max_safe` has another initializer.
    let list = checkout.read("std/list.cheby");
    let doc = "/// Returns the results of calling `f` on each element, in order.\n";
    assert_eq!(list.matches(doc).count(), 1, "std/list.cheby documents map");
    checkout.write("std/list.cheby", &list.replacen(doc, "", 1));
    let int = checkout.read("std/int.cheby");
    let old = "pub const max_safe: Int = 9_007_199_254_740_991\n";
    assert_eq!(
        int.matches(old).count(),
        1,
        "std/int.cheby declares max_safe"
    );
    checkout.write(
        "std/int.cheby",
        &int.replacen(old, "pub const max_safe: Int = 9_007_199_254_740_990\n", 1),
    );
    let analyzed = checkout.analyze(&[("std/probe_b.cheby", PROBE_B)]);
    let base = std_base();
    let path = checkout.at("std/probe_b.cheby");
    let doc = source(&analyzed.snapshot, &path).expect("the document");
    assert_eq!(doc.text, PROBE_B);
    let at = |needle: &str| {
        let shown = hover(
            &analyzed,
            &doc,
            offset_of(PROBE_B, needle),
            StdDocs::Index(base.index()),
            None,
        )
        .expect("a hover");
        shown.render(true)
    };
    let map = at("list::m|ap(doubled");
    assert!(map.contains("Defined in `std::list`."), "{map}");
    assert!(
        !map.contains("Returns the results of calling"),
        "the binary's docs of a checkout module: {map}"
    );
    let constant = at("int::max_sa|fe");
    assert!(
        constant.contains("max_safe: Int = 9_007_199_254_740_990"),
        "the checkout's initializer: {constant}"
    );
    assert!(
        !constant.contains("740_991"),
        "the binary's initializer: {constant}"
    );
    let helper = at("probe_a::help|er");
    assert!(helper.contains("Doubles each element."), "{helper}");
    assert!(helper.contains("Defined in `std::probe_a`."), "{helper}");
    // `completionItem/resolve` agrees: a checkout module's docs are its
    // own tree's, never the binary's (D-1700).
    let origin = path_to_uri(&path).unwrap().as_str().to_owned();
    let resolved = |name: &str, module: &str| {
        let item = json!({
            "label": name,
            "data": { "module": module, "ns": "value", "name": name, "origin": origin },
        });
        let caps = CompletionCaps {
            markdown: true,
            ..CompletionCaps::default()
        };
        with_cx(|cx| resolve(cx, &[&analyzed], item, caps, StdAccess::Index(base.index())))
            .expect("an answer")
            .value
    };
    let map = resolved("map", "std::list");
    assert!(
        map.get("documentation")
            .is_none_or(|docs| !docs.to_string().contains("Returns the results of calling")),
        "the binary's docs of a checkout module: {map}"
    );
    let length = resolved("length", "std::list");
    assert!(
        length["documentation"].is_object(),
        "a checkout module's own docs: {length}"
    );
}

#[test]
fn checkout_tokens_and_hints_are_present() {
    let checkout = Checkout::new("tokens");
    let analyzed = checkout.analyze(&[("std/probe_b.cheby", PROBE_B)]);
    let snapshot = &analyzed.snapshot;
    let probe = view(&checkout, snapshot, "std/probe_b.cheby");
    let cancel = CancelToken::new();
    let module = analyzed.index().module(probe.id()).expect("an index");
    let found = tokens(snapshot, module, None, &cancel).expect("tokens");
    assert!(!found.is_empty(), "a checkout module has tokens");
    let at = |needle: &str| {
        let offset = offset_of(PROBE_B, needle);
        found
            .iter()
            .find(|t| t.range.start() == offset)
            .unwrap_or_else(|| panic!("a token at `{needle}`"))
    };
    // The checkout's std items are the session's own, not a library's.
    for needle in ["list::|map(doubled", "probe_a::|helper", "int::|max_safe"] {
        let token = at(needle);
        assert!(
            !token.mods.names().contains(&"defaultLibrary"),
            "`{needle}`: {:?}",
            token.mods.names()
        );
    }
    assert!(at("pub fn |total").mods.names().contains(&"declaration"));
    let hinted = hints(snapshot, probe, None, Switches::DEFAULTS, None, &cancel).expect("hints");
    let doubled = offset_of(PROBE_B, "let doubled| =");
    let hint = hinted
        .iter()
        .find(|h| h.offset == doubled)
        .unwrap_or_else(|| panic!("a hint after `doubled`: {hinted:?}"));
    assert_eq!(hint.label, ": List<Int>");
    assert_eq!(
        hint.edit.as_deref(),
        Some(": List<Int>"),
        "a checkout file is editable"
    );
}

#[test]
fn checkout_workspace_symbol_lists_map_twice() {
    let checkout = Checkout::new("workspace_symbol");
    let analyzed = checkout.analyze(&[]);
    let base = std_base();
    let dir = std_dir();
    let std = StdSymbols {
        index: base.index(),
        dir: &dir,
    };
    let got =
        with_cx(|cx| workspace_symbol(cx, &[&analyzed], json!({ "query": "map" }), Some(std)))
            .expect("an answer")
            .value;
    let maps: Vec<&Value> = got
        .as_array()
        .expect("a list")
        .iter()
        .filter(|s| s["name"] == "map" && s["containerName"] == "std::list")
        .collect();
    let uris: Vec<&str> = maps
        .iter()
        .map(|s| s["location"]["uri"].as_str().unwrap())
        .collect();
    assert_eq!(
        uris.len(),
        2,
        "list::map from the checkout and from std: {uris:?}"
    );
    let own = path_to_uri(&checkout.at("std/list.cheby")).unwrap();
    assert_eq!(
        uris.first().copied(),
        Some(own.as_str()),
        "the checkout's first"
    );
    assert_ne!(uris.first(), uris.get(1), "two paths tell the two apart");
    assert!(
        uris.get(1)
            .is_some_and(|uri| uri.ends_with("/std/list.cheby")),
        "{uris:?}"
    );
    // The synthetic root is no module of the session.
    let roots = got
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| {
            s["containerName"]
                .as_str()
                .is_some_and(|c| c.starts_with("std_checkout"))
        })
        .count();
    assert_eq!(roots, 0);
}

#[test]
fn checkout_definition_stays_in_the_checkout() {
    let checkout = Checkout::new("definition");
    let analyzed = checkout.analyze(&[("std/probe_b.cheby", PROBE_B)]);
    let uri = path_to_uri(&checkout.at("std/probe_b.cheby")).unwrap();
    let dir = std_dir();
    let env = NavEnv {
        links: LinkCaps::default(),
        std_dir: Some(&dir),
    };
    let jump = |needle: &str| -> Value {
        let offset = at_usize(offset_of(PROBE_B, needle));
        let before = PROBE_B.get(..offset).unwrap();
        let line = before.matches('\n').count();
        let col = before.rsplit('\n').next().unwrap().len();
        let params = json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": line, "character": col },
        });
        let value = with_cx(|cx| definition(cx, &analyzed, params, &env))
            .expect("an answer")
            .value;
        let Value::Array(mut items) = value else {
            return value;
        };
        assert_eq!(items.len(), 1, "one target: {items:?}");
        items.remove(0)
    };
    let map = jump("list::m|ap(doubled");
    let list = checkout.read("std/list.cheby");
    let line = list
        .get(..list.find("pub fn map<").unwrap())
        .unwrap()
        .matches('\n')
        .count();
    assert_eq!(
        map["uri"].as_str(),
        Some(
            path_to_uri(&checkout.at("std/list.cheby"))
                .unwrap()
                .as_str()
        ),
        "{map}"
    );
    assert_eq!(
        map["range"]["start"],
        json!({ "line": line, "character": 7 })
    );
    let helper = jump("probe_a::he|lper");
    assert_eq!(
        helper["uri"].as_str(),
        Some(
            path_to_uri(&checkout.at("std/probe_a.cheby"))
                .unwrap()
                .as_str()
        ),
        "{helper}"
    );
}

/// A builtin type whose home module the checkout lacks has no target,
/// never the binary's materialized copy (D-1703).
#[test]
fn checkout_definition_of_a_homeless_builtin() {
    let checkout = Checkout::new("homeless");
    std::fs::remove_file(checkout.at("std/i8.cheby")).unwrap();
    let probe = "//! Probe.\n\npub fn small(x: I8) -> I8 {\n  x\n}\n";
    let analyzed = checkout.analyze(&[("std/probe_c.cheby", probe)]);
    let uri = path_to_uri(&checkout.at("std/probe_c.cheby")).unwrap();
    let dir = std_dir();
    let env = NavEnv {
        links: LinkCaps::default(),
        std_dir: Some(&dir),
    };
    let col = probe.find("I8").unwrap() - probe.rfind("\npub").unwrap();
    let params = json!({
        "textDocument": { "uri": uri.as_str() },
        "position": { "line": 2, "character": col },
    });
    let value = with_cx(|cx| definition(cx, &analyzed, params, &env))
        .expect("an answer")
        .value;
    assert!(
        value.is_null() || value.as_array().is_some_and(Vec::is_empty),
        "no target: {value}"
    );
}

#[test]
fn checkout_import_plans_follow_the_checkout() {
    let checkout = Checkout::new("imports");
    std::fs::remove_file(checkout.at("std/hex.cheby")).unwrap();
    let analyzed = checkout.analyze(&[]);
    let snapshot = &analyzed.snapshot;
    let base = std_base();
    let index = Some(base.index());
    let probe = view(&checkout, snapshot, "std/probe_a.cheby");
    let path = |text: &str| {
        text.split("::")
            .skip(1)
            .fold(ModulePath::root("std"), |path, s| path.child(s))
    };
    // `std::probe_b` imports `std::probe_a`: the import would close a cycle.
    assert_eq!(
        import_plan(snapshot, probe, index, &path("std::probe_b")),
        None
    );
    // A module the checkout lacks, though the binary ships it.
    assert!(base.index().module("std::hex").is_some());
    assert_eq!(import_plan(snapshot, probe, index, &path("std::hex")), None);
    // Another std module of the checkout.
    let plan = import_plan(snapshot, probe, index, &path("std::option")).expect("a plan");
    assert_eq!(plan.name, "option");
    assert!(
        plan.edit
            .is_some_and(|(_, line)| line == "import std::option\n")
    );
}

#[test]
fn checkout_import_paths_are_the_checkout_modules() {
    let checkout = Checkout::new("import_paths");
    std::fs::remove_file(checkout.at("std/hex.cheby")).unwrap();
    let base = std_base();
    let options = Options {
        snippets: true,
        type_driven: true,
        auto_import: false,
        zed: false,
    };
    let labels = |text: &str| -> Vec<String> {
        let offset = text.find('|').expect("a cursor");
        let text = text.replacen('|', "", 1);
        let analyzed = checkout.analyze(&[("std/probe_c.cheby", &text)]);
        let snapshot = &analyzed.snapshot;
        let probe = view(&checkout, snapshot, "std/probe_c.cheby");
        let tree = probe.tree().expect("a tree");
        let offset = TextSize::from(u32::try_from(offset).unwrap());
        let found = complete(
            snapshot,
            Some(probe),
            tree,
            &text,
            offset,
            options,
            StdAccess::Index(base.index()),
        );
        found.items.iter().map(|e| e.label.clone()).collect()
    };
    let under_std = labels("//! A probe.\n\nimport std::|\n");
    for module in ["list", "probe_a", "json"] {
        assert!(
            under_std.iter().any(|l| l == module),
            "`{module}`: {under_std:?}"
        );
    }
    assert!(!under_std.iter().any(|l| l == "hex"), "{under_std:?}");
    let roots = labels("//! A probe.\n\nimport |\n");
    assert!(roots.iter().any(|l| l == "std"), "{roots:?}");
    assert!(!roots.iter().any(|l| l == "std_checkout"), "{roots:?}");
}
