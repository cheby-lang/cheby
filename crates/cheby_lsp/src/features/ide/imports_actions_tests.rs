//! Tests of the import actions (X §6.3, D-1691, D-1694): add, remove,
//! organize, E0018's move and E0042's std row, each over a real run and
//! its report, through the provider and the resolver the frame calls.
//! They run only the front end, so they run on Windows too (D-1516).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use cheby_driver::analysis::{
    FileReport, Globals, Overlay, OverlayFile, PackageKind, PlainDiagnostic, PlainEdit, Session,
    SessionConfig, StdBase, StdIndex, TextRange, TextSize, codes,
};

use super::{MOVE_IMPORT, MOVE_IMPORTS, ORGANIZE, REMOVE_IMPORT, actions, removal, resolve};
use crate::features::ide::actions::{Action, ActionId, ActionKind, FixCx};
use crate::features::ide::checkout_tests::Checkout;
use crate::features::ide::hover::tests::Temp;
use crate::features::stdlib::StdService;
use crate::features::{Analyzed, CancelToken, DocText};
use crate::text::uri::normalize;

/// The binary's std index, built once per test process.
fn std_index() -> &'static StdIndex {
    static BASE: OnceLock<Arc<StdBase>> = OnceLock::new();
    BASE.get_or_init(|| StdBase::build(Arc::new(Globals::new())).expect("std checks"))
        .index()
}

/// One run of a session with `main` open, and the report of `main`.
struct Run {
    analyzed: Analyzed,
    path: PathBuf,
    text: String,
    report: Option<Arc<FileReport>>,
}

impl Run {
    /// A run of the package `app` whose `src/main.cheby` is `main`, beside
    /// `files`.
    fn package(name: &str, main: &str, files: &[(&str, &str)]) -> Self {
        let mut all = files.to_vec();
        all.push(("main.cheby", main));
        let temp = Temp::package(name, &all);
        let root = normalize(&temp.0);
        let path = root.join("src").join("main.cheby");
        let run = Self::of(root, PackageKind::Package, path, main);
        drop(temp);
        run
    }

    /// A run of a std checkout with the probe at `display` open as `text`.
    fn checkout(checkout: &Checkout, display: &str, text: &str) -> Self {
        let path = normalize(&checkout.at(display));
        let root = path
            .ancestors()
            .find(|dir| dir.join("std").is_dir() && dir.join("crates").is_dir())
            .expect("the checkout's root")
            .to_path_buf();
        let kind = PackageKind::StdCheckout {
            checkout: root.clone(),
        };
        Self::of(root, kind, path, text)
    }

    fn of(root: PathBuf, kind: PackageKind, path: PathBuf, text: &str) -> Self {
        let mut session = Session::new(SessionConfig {
            root,
            kind,
            target: None,
            globals: Arc::new(Globals::new()),
        });
        let open = OverlayFile {
            text: Arc::from(text),
            version: 1,
            stamp: 1,
        };
        let overlay = Overlay {
            files: [(path.clone(), open)].into_iter().collect(),
        };
        let live = session.analyze_live(&overlay).expect("a run");
        // A file with phase-A errors is published from `phase_a`.
        let report = live
            .report
            .as_ref()
            .and_then(|report| report.files.get(&path))
            .or_else(|| live.phase_a.get(&path))
            .cloned();
        Self {
            analyzed: Analyzed::new(live.snapshot.expect("a snapshot")),
            path,
            text: text.to_owned(),
            report,
        }
    }

    /// The report's diagnostics, with their codes.
    fn diagnostics(&self) -> &[PlainDiagnostic] {
        self.report
            .as_deref()
            .map_or(&[], |r| r.diagnostics.as_slice())
    }

    /// The actions at the whole of `needle`, with `std` as std's index
    /// and the report `substituted` or not.
    fn listed(&self, needle: &str, std: Option<&StdIndex>, substituted: bool) -> Vec<Action> {
        let at = self
            .text
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` in the text"));
        let range = TextRange::new(size(at), size(at.checked_add(needle.len()).unwrap()));
        let overlapping: Vec<(usize, &PlainDiagnostic)> = self
            .diagnostics()
            .iter()
            .enumerate()
            .filter(|(_, d)| {
                d.range
                    .is_some_and(|r| r.start() <= range.end() && range.start() <= r.end())
            })
            .collect();
        self.with_cx(substituted, |cx| actions(cx, std, &overlapping))
    }

    /// Runs `f` with the request's context.
    fn with_cx<T>(&self, substituted: bool, f: impl FnOnce(&FixCx<'_>) -> T) -> T {
        let snapshot = &self.analyzed.snapshot;
        let view = snapshot
            .file_of(&self.path)
            .and_then(|file| snapshot.module_at(file))
            .expect("the module");
        let std = StdService::none();
        let cancel = CancelToken::new();
        let cx = FixCx {
            analyzed: &self.analyzed,
            view,
            report: self.report.as_deref(),
            substituted,
            context: &[],
            text: &self.text,
            std: &std,
            resolve_edits: true,
            disabled_support: false,
            cancel: &cancel,
        };
        f(&cx)
    }

    /// The text after the edits of `action`, resolved first if it has data.
    fn applied(&self, action: &Action) -> String {
        let edits = match (&action.edit, &action.data) {
            (Some(edits), _) => edits.clone(),
            (None, Some(id)) => self
                .with_cx(false, |cx| resolve(cx, TextRange::empty(0), id))
                .expect("resolves"),
            (None, None) => panic!("`{}` has no edit", action.title),
        };
        apply(&self.text, &edits)
    }
}

fn size(n: usize) -> TextSize {
    TextSize::try_from(n).unwrap()
}

/// `text` with `edits`, which must not overlap, applied.
fn apply(text: &str, edits: &[PlainEdit]) -> String {
    let mut sorted: Vec<&PlainEdit> = edits.iter().collect();
    sorted.sort_by_key(|edit| edit.range.start());
    let mut out = String::new();
    let mut at = 0;
    for edit in sorted {
        let start = usize::try_from(edit.range.start()).unwrap();
        assert!(start >= at, "edits overlap: {edits:?}");
        out.push_str(text.get(at..start).unwrap());
        out.push_str(&edit.replacement);
        at = usize::try_from(edit.range.end()).unwrap();
    }
    out.push_str(text.get(at..).unwrap());
    out
}

/// The titles of `actions`, in order.
fn titles(actions: &[Action]) -> Vec<&str> {
    actions.iter().map(|a| a.title.as_str()).collect()
}

/// The one action titled `title`.
fn titled<'a>(actions: &'a [Action], title: &str) -> &'a Action {
    let found: Vec<&Action> = actions.iter().filter(|a| a.title == title).collect();
    assert_eq!(found.len(), 1, "one `{title}` in {:?}", titles(actions));
    found[0]
}

/// The text of `text` after removing what `removal` cuts for the
/// significant text `what`.
fn removed(text: &str, what: &str) -> Option<String> {
    let doc = DocText::new(PathBuf::from("main.cheby"), None, Arc::from(text)).unwrap();
    let at = text.find(what).unwrap();
    let range = TextRange::new(size(at), size(at.checked_add(what.len()).unwrap()));
    let cut = removal(text, &doc.tree, range)?;
    let edit = PlainEdit {
        path: PathBuf::from("main.cheby"),
        range: cut,
        replacement: String::new(),
    };
    Some(apply(text, &[edit]))
}

/// W0003's removal: the whole import with its line, or one name of a list
/// with its comma, the last one's before it (X §6.3).
#[test]
fn a_removal_takes_the_import_or_one_name() {
    let text = "import std::io\nimport std::list::{length, map, reverse}\n\nfn main() {}\n";
    assert_eq!(
        removed(text, "import std::io").as_deref(),
        Some("import std::list::{length, map, reverse}\n\nfn main() {}\n")
    );
    assert_eq!(
        removed(text, "length").as_deref(),
        Some("import std::io\nimport std::list::{map, reverse}\n\nfn main() {}\n")
    );
    assert_eq!(
        removed(text, "map").as_deref(),
        Some("import std::io\nimport std::list::{length, reverse}\n\nfn main() {}\n")
    );
    assert_eq!(
        removed(text, "reverse").as_deref(),
        Some("import std::io\nimport std::list::{length, map}\n\nfn main() {}\n")
    );
    // A name with its alias.
    let aliased = "import std::list::{length as len, map}\n";
    assert_eq!(
        removed(aliased, "length as len").as_deref(),
        Some("import std::list::{map}\n")
    );
    // The last line, with no line break after it.
    assert_eq!(
        removed("import std::io\nimport std::list", "import std::list").as_deref(),
        Some("import std::io\n")
    );
    // Text that is no import or name of one is not cut.
    assert_eq!(removed(text, "std::io"), None);
}

const UNUSED: &str = "\
//! Root.

import std::io
import std::list

fn main() {
  assert list::length([1]) == 1
}
";

/// W0003 offers its removal, marked so that the frame drops it while the
/// report is substituted (D-1360).
#[test]
fn an_unused_import_is_removed() {
    let run = Run::package("unused", UNUSED, &[]);
    assert!(
        run.diagnostics()
            .iter()
            .any(|d| d.code == codes::UNUSED_IMPORT)
    );
    let listed = run.listed("import std::io", None, false);
    let remove = titled(&listed, REMOVE_IMPORT);
    assert!(remove.removes && remove.preferred);
    assert_eq!(remove.kind, ActionKind::QuickFix);
    assert_eq!(run.applied(remove), UNUSED.replace("import std::io\n", ""));
    // Not at the import that is used.
    let used = run.listed("import std::list", None, false);
    assert!(!titles(&used).contains(&REMOVE_IMPORT));
}

/// Organize imports sorts the imports and removes W0003's (X §6.3); with
/// nothing to do it has an empty edit, which the frame keeps only under an
/// `only` that asks for it (D-1691), and its edit is computed on resolve.
#[test]
fn organize_sorts_and_removes() {
    let text = "\
//! Root.

import std::list
import std::io
import std::int

fn main() {
  io::println(int::show(list::length([1])))
}
";
    let run = Run::package("organize", text, &[]);
    let listed = run.listed("fn main", None, false);
    let organize = titled(&listed, ORGANIZE);
    assert_eq!(organize.kind, ActionKind::SourceOrganizeImports);
    assert_eq!(organize.edit, None);
    assert_eq!(organize.data, Some(ActionId::OrganizeImports));
    assert!(!organize.removes);
    assert_eq!(
        run.applied(organize),
        text.replace(
            "import std::list\nimport std::io\nimport std::int\n",
            "import std::int\nimport std::io\nimport std::list\n"
        )
    );
    // In order: nothing to do.
    let sorted = Run::package("organized", &run.applied(organize), &[]);
    let listed = sorted.listed("fn main", None, false);
    assert_eq!(titled(&listed, ORGANIZE).edit, Some(Vec::new()));
    // The unused import goes, unless the report is substituted (D-1360).
    let run = Run::package("organize_unused", UNUSED, &[]);
    let listed = run.listed("fn main", None, false);
    let organize = titled(&listed, ORGANIZE);
    assert!(organize.removes);
    assert_eq!(
        run.applied(organize),
        UNUSED.replace("import std::io\n", "")
    );
    let listed = run.listed("fn main", None, true);
    let organize = titled(&listed, ORGANIZE);
    assert!(!organize.removes);
    assert_eq!(organize.edit, Some(Vec::new()));
}

/// Two names of one list, the last among them, are cut as one, and every
/// block of imports is sorted on its own (D-1148).
#[test]
fn organize_cuts_names_and_keeps_blocks() {
    let text = "\
import std::list::{length, map, reverse}
import std::int

import std::io

fn main() {
  io::println(int::show(length([1])))
}
";
    let run = Run::package("organize_names", text, &[]);
    let unused = run
        .diagnostics()
        .iter()
        .filter(|d| d.code == codes::UNUSED_IMPORT)
        .count();
    assert_eq!(unused, 2, "{:?}", run.diagnostics());
    let listed = run.listed("fn main", None, false);
    let organize = titled(&listed, ORGANIZE);
    assert_eq!(
        run.applied(organize),
        text.replace(
            "import std::list::{length, map, reverse}\nimport std::int\n",
            "import std::int\nimport std::list::{length}\n"
        )
    );
}

/// A file with a grammar error gets no organize imports (X §6.3).
#[test]
fn organize_refuses_a_grammar_error() {
    let text = "import std::list\nimport std::io\n\nfn main() {\n  let = 1\n}\n";
    let run = Run::package("organize_broken", text, &[]);
    let listed = run.listed("fn main", None, false);
    assert!(
        !titles(&listed).contains(&ORGANIZE),
        "{:?}",
        titles(&listed)
    );
    let refused = run.with_cx(false, |cx| {
        resolve(cx, TextRange::empty(0), &ActionId::OrganizeImports)
    });
    assert!(refused.is_err());
}

/// E0042 on a qualifier no import binds: the session's own module first,
/// then std's, each imported where `cheby fmt` keeps imports.
#[test]
fn a_missing_module_is_imported() {
    let text = "\
//! Root.

import std::io

fn main() {
  io::println(list::reverse(\"ab\"))
}
";
    let list = "//! A list of its own.\n\npub fn reverse(s: String) -> String {\n  s\n}\n";
    let run = Run::package("add", text, &[("list.cheby", list)]);
    let listed = run.listed("list::reverse", Some(std_index()), false);
    let imports: Vec<&str> = titles(&listed)
        .into_iter()
        .filter(|t| t.starts_with("Import"))
        .collect();
    assert_eq!(imports, ["Import `app::list`", "Import `std::list`"]);
    assert_eq!(
        run.applied(titled(&listed, "Import `app::list`")),
        text.replace("import std::io\n", "import app::list\nimport std::io\n")
    );
    assert_eq!(
        run.applied(titled(&listed, "Import `std::list`")),
        text.replace("import std::io\n", "import std::io\nimport std::list\n")
    );
    // Without std's index, only the own module.
    let listed = run.listed("list::reverse", None, false);
    assert!(!titles(&listed).contains(&"Import `std::list`"));
}

/// An import whose name a local takes gets an alias, and the diagnosed
/// qualifier is written with it (D-1395).
#[test]
fn an_import_with_a_clash_is_aliased() {
    let text = "\
fn other() -> Int {
  let list = 1
  list
}

fn main() {
  assert list::length([1]) == other()
}
";
    let run = Run::package("alias", text, &[]);
    let listed = run.listed("list::length", Some(std_index()), false);
    let import = titled(&listed, "Import `std::list as std_list`");
    assert_eq!(
        run.applied(import),
        format!(
            "import std::list as std_list\n\n{}",
            text.replace("list::length", "std_list::length")
        )
    );
}

/// Nothing is offered for a qualifier the scope binds to something that is
/// not a module ("`x` is not a module").
#[test]
fn a_bound_qualifier_imports_nothing() {
    let text = "fn list() -> Int {\n  1\n}\n\nfn main() {\n  assert list::length([1]) == 1\n}\n";
    let run = Run::package("bound", text, &[]);
    assert!(
        run.diagnostics()
            .iter()
            .any(|d| d.code == codes::UNRESOLVED_NAME)
    );
    let listed = run.listed("list::length", Some(std_index()), false);
    assert!(
        !listed.iter().any(|a| a.title.starts_with("Import")),
        "{:?}",
        titles(&listed)
    );
}

/// E0042 on `m::x` of a std module: one fix per item the help names,
/// with the import when the file lacks it (D-1694): an alias of D-1658's
/// table, a name in several modules, and a builtin homed in std.
#[test]
fn an_item_elsewhere_in_std_is_used() {
    let text = "\
import std::file
import std::io

fn main() {
  let read = file::read_text(\"a.txt\")
  assert read == Ok(\"\")
}
";
    let run = Run::package("elsewhere", text, &[]);
    let listed = run.listed("read_text", Some(std_index()), false);
    let fix = titled(&listed, "Use `io::read_file`");
    assert_eq!(fix.kind, ActionKind::QuickFix);
    assert_eq!(
        run.applied(fix),
        text.replace("file::read_text", "io::read_file")
    );
    // Not imported: the import comes with it.
    let alone = text.replace("import std::io\n", "");
    let run = Run::package("elsewhere_import", &alone, &[]);
    let listed = run.listed("read_text", Some(std_index()), false);
    let fix = titled(&listed, "Use `io::read_file` and import `std::io`");
    assert_eq!(
        run.applied(fix),
        alone
            .replace("file::read_text", "io::read_file")
            .replace("import std::file\n", "import std::file\nimport std::io\n")
    );
    // A builtin homed in std, in a type.
    let homed = "\
import std::list

fn first(xs: List<Int>) -> list::Option<Int> {
  list::first(xs)
}

fn main() {
  assert first([1]) == Some(1)
}
";
    let run = Run::package("elsewhere_home", homed, &[]);
    let listed = run.listed("Option", Some(std_index()), false);
    let fix = titled(&listed, "Use `option::Option` and import `std::option`");
    assert!(run.applied(fix).contains("-> option::Option<Int>"));
}

/// An item in several std modules gives one fix each, in the help's order
/// and number (D-1658), and a close name in the module gives none, since
/// the help is then its "did you mean".
#[test]
fn every_module_the_help_names_gets_a_fix() {
    let text = "\
import std::int

fn main() {
  assert int::length(\"ab\") == 2
}
";
    let run = Run::package("elsewhere_several", text, &[]);
    let help = run
        .diagnostics()
        .iter()
        .find(|d| d.code == codes::UNRESOLVED_NAME)
        .and_then(|d| d.helps.first())
        .map(|h| h.message.clone())
        .expect("a help");
    let listed = run.listed("length", Some(std_index()), false);
    let used: Vec<&str> = titles(&listed)
        .into_iter()
        .filter(|t| t.starts_with("Use"))
        .collect();
    assert_eq!(used.len(), 3, "{used:?}");
    for title in &used {
        let module = title
            .split("import `std::")
            .nth(1)
            .and_then(|rest| rest.strip_suffix('`'))
            .expect("an import");
        assert!(
            help.contains(&format!("`std::{module}`")),
            "{help}: {title}"
        );
    }
    let close = "import std::io\n\nfn main() {\n  io::println_err(\"a\")\n}\n";
    let run = Run::package("elsewhere_close", close, &[]);
    let listed = run.listed("println_err", Some(std_index()), false);
    assert!(
        !listed.iter().any(|a| a.title.starts_with("Use")),
        "{:?}",
        titles(&listed)
    );
}

/// D-1743: a qualifier that names a module imported under another name is
/// rewritten to that name, when the module has the item, and nothing is
/// imported twice.
#[test]
fn a_module_imported_under_another_name_is_requalified() {
    let text = "\
import std::list as l

fn main() {
  assert list::length([1]) == 1
  assert list::nothing([1]) == 1
}
";
    let run = Run::package("requalify", text, &[]);
    let listed = run.listed("list::length", Some(std_index()), false);
    let used: Vec<&str> = titles(&listed)
        .into_iter()
        .filter(|t| t.starts_with("Use") || t.starts_with("Import"))
        .collect();
    assert_eq!(used, ["Use `l::length`"], "{:?}", run.diagnostics());
    assert_eq!(
        run.applied(titled(&listed, "Use `l::length`")),
        text.replace("list::length", "l::length")
    );
    let listed = run.listed("list::nothing", Some(std_index()), false);
    assert!(
        !titles(&listed).iter().any(|t| t.starts_with("Use")),
        "{:?}",
        titles(&listed)
    );
    // A `priv` item gets no such fix, which would trade E0042 for E0043
    // (W11's review, A6).
    let shapes =
        "//! Shapes.\n\npub fn area() -> Int {\n  1\n}\n\npriv fn hidden() -> Int {\n  2\n}\n";
    let main = "\
import app::shapes as sh

fn main() {
  assert shapes::area() == 1
  assert shapes::hidden() == 2
}
";
    let run = Run::package("requalify_priv", main, &[("shapes.cheby", shapes)]);
    let requalified = |needle: &str| -> Vec<String> {
        titles(&run.listed(needle, None, false))
            .into_iter()
            .filter(|t| t.starts_with("Use"))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(
        requalified("shapes::area"),
        ["Use `sh::area`"],
        "{:?}",
        run.diagnostics()
    );
    assert_eq!(requalified("shapes::hidden"), Vec::<String>::new());
}

/// E0018: the imports after an item move to the import block, all of
/// them, from either one's fix (D-1744), and one alone keeps D-1736's
/// title. With no import block left they open one at the file's start.
#[test]
fn an_import_after_an_item_moves() {
    let text = "\
import std::list

fn main() {
  io::println(int::show(list::length([1])))
}

import std::io
import std::int
";
    let run = Run::package("move", text, &[]);
    let want = "\
import std::int
import std::io
import std::list

fn main() {
  io::println(int::show(list::length([1])))
}

";
    for needle in ["import std::io", "import std::int"] {
        let listed = run.listed(needle, None, false);
        let moved = titled(&listed, MOVE_IMPORTS);
        assert_eq!(run.applied(moved), want, "{needle}");
    }
    let one = text
        .replace("import std::int\n", "")
        .replace("int::show(list::length([1]))", "\"x\"");
    let run = Run::package("move_one", &one, &[]);
    let listed = run.listed("import std::io", None, false);
    assert!(
        run.applied(titled(&listed, MOVE_IMPORT))
            .starts_with("import std::io\nimport std::list\n")
    );
    let none = "\
fn main() {
  io::println(int::show(1))
}

import std::io
import std::int
";
    let run = Run::package("move_all", none, &[]);
    let listed = run.listed("import std::int", None, false);
    assert_eq!(
        run.applied(titled(&listed, MOVE_IMPORTS)),
        "\
import std::int
import std::io

fn main() {
  io::println(int::show(1))
}

"
    );
}

/// In a std checkout the std row reads the checkout's items, so an item
/// only the checkout has is found, and no index of the binary's std is read
/// (D-1700, D-1706, D-1726).
#[test]
fn a_checkout_finds_its_own_items() {
    let checkout = Checkout::new("imports_elsewhere");
    let only = "//! A probe.\n\npub fn only_in_the_checkout() -> Int {\n  1\n}\n";
    std::fs::write(checkout.at("std/probe_only.cheby"), only).unwrap();
    let probe = "\
//! A probe.

import std::list

/// One.
pub fn one() -> Int {
  list::only_in_the_checkout()
}
";
    std::fs::write(checkout.at("std/probe_use.cheby"), probe).unwrap();
    let run = Run::checkout(&checkout, "std/probe_use.cheby", probe);
    let listed = run.listed("only_in_the_checkout", None, false);
    let fix = titled(
        &listed,
        "Use `probe_only::only_in_the_checkout` and import `std::probe_only`",
    );
    assert!(
        run.applied(fix)
            .contains("import std::list\nimport std::probe_only\n")
    );
}
