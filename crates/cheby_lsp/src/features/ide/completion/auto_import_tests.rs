//! Auto-import (X §4.4, D-1395): the two-character rule, the sources
//! (package modules, loaded std, std's index), the import as an
//! additional edit with an alias on a clash, the type namespace, and the
//! cap. The items of imported modules (D-1507), the qualified label
//! (D-1508), and no such item before `::` (D-1521). Each test runs only
//! the front end, so it runs on Windows too (D-1516).

use std::sync::{Arc, OnceLock};

use cheby_driver::analysis::{Globals, StdBase, TextRange};

use super::super::items::{EntryKind, rank};
use super::super::{Completions, Options, StdAccess};
use super::CAP;
use crate::features::ide::completion::dot::tests::{applied, entry, labels, parses, run};

/// Every option on.
const WITH: Options = Options {
    snippets: true,
    type_driven: true,
    auto_import: true,
    zed: false,
};

/// Std's base, built once for these tests.
fn base() -> &'static StdBase {
    static BASE: OnceLock<Arc<StdBase>> = OnceLock::new();
    BASE.get_or_init(|| StdBase::build(Arc::new(Globals::new())).expect("std checks"))
}

/// The completions at the `‸` of `main`, with std's index.
fn at(main: &str, files: &[(&str, &str)]) -> (Completions, String) {
    run(main, files, WITH, StdAccess::Index(base().index()))
}

/// The items that need an import, by label and description.
fn imported(found: &Completions) -> Vec<(String, String)> {
    found
        .items
        .iter()
        .filter(|e| e.rank == rank::NOT_IMPORTED)
        .map(|e| (e.label.clone(), e.description.clone().unwrap_or_default()))
        .collect()
}

#[test]
fn one_character_imports_nothing() {
    let (found, _) = at("fn main() {\n  let n = r‸\n}\n", &[]);
    assert_eq!(imported(&found), Vec::<(String, String)>::new());
    // The imports start at the second character, so the client must ask
    // again (D-1487). So must it with no partial word.
    assert!(found.incomplete);
    let (found, _) = at("fn main() {\n  let n = ‸\n}\n", &[]);
    assert!(found.incomplete);
}

#[test]
fn a_loaded_module_is_imported_by_an_additional_edit() {
    let main = "//! Main.\n\nfn main() {\n  let xs = [1]\n  let n = xs_rev‸\n}\n";
    let (found, _) = at(main, &[]);
    assert!(imported(&found).is_empty(), "no name has `xs_rev`");
    let main = "//! Main.\n\nfn main() {\n  let xs = [1]\n  let n = revers‸\n}\n";
    let (found, text) = at(main, &[]);
    let reverse = found
        .items
        .iter()
        .find(|e| e.label == "list::reverse" && e.description.as_deref() == Some("std::list"))
        .unwrap_or_else(|| panic!("{:?}", imported(&found)));
    assert_eq!(reverse.insert.text, "list::reverse(${1:xs})$0");
    assert_eq!(slice(&text, reverse.insert.range), "revers");
    assert_eq!(
        reverse.additional,
        vec![(TextRange::empty(11), "import std::list\n\n".to_owned())]
    );
    let done = applied(&text, reverse);
    assert!(
        done.starts_with("//! Main.\n\nimport std::list\n\nfn main() {"),
        "{done}"
    );
    assert!(parses(&done));
}

/// The text `range` of `text` covers.
fn slice(text: &str, range: TextRange) -> &str {
    range.slice(text).expect("a range of the text")
}

#[test]
fn a_taken_name_imports_with_an_alias() {
    let main = "fn main() {\n  let list = [1]\n  let n = revers‸\n}\n";
    let (found, _) = at(main, &[]);
    let reverse = found
        .items
        .iter()
        .find(|e| e.label == "std_list::reverse" && e.description.as_deref() == Some("std::list"))
        .unwrap_or_else(|| panic!("{:?}", imported(&found)));
    assert_eq!(reverse.insert.text, "std_list::reverse(${1:xs})$0");
    assert_eq!(
        reverse.additional.first().map(|(_, t)| t.as_str()),
        Some("import std::list as std_list\n\n")
    );
}

#[test]
fn modules_the_run_did_not_load_come_from_the_index() {
    let main = "fn main() {\n  let n = url_enc‸\n}\n";
    let (found, text) = at(main, &[]);
    let encode = entry(&found, "base64::url_encode");
    assert_eq!(encode.description.as_deref(), Some("std::base64"));
    assert_eq!(encode.label_detail.as_deref(), Some("(data)"));
    assert_eq!(encode.insert.text, "base64::url_encode(${1:data})$0");
    assert_eq!(
        encode.additional.first().map(|(_, t)| t.as_str()),
        Some("import std::base64\n\n")
    );
    assert!(parses(&applied(&text, encode)));
}

#[test]
fn package_modules_are_offered_and_imported_modules_are_not() {
    let main = "fn main() {\n  let n = orig‸\n}\n";
    let (found, _) = at(main, &[]);
    let origin = entry(&found, "geo::origin");
    assert_eq!(origin.description.as_deref(), Some("app::geo"));
    assert_eq!(origin.insert.text, "geo::origin");
    // A package item of another module is visible; a `priv` one is not.
    let main = "fn main() {\n  let n = hidd‸\n}\n";
    let (found, _) = at(main, &[]);
    assert!(found.items.iter().any(|e| e.label == "geo::hidden"));
    let main = "fn main() {\n  let n = secr‸\n}\n";
    let (found, _) = at(main, &[]);
    assert!(!found.items.iter().any(|e| e.label == "geo::secret"));
    // `geo` is imported: its items need no import edit (D-1507).
    let main = "import app::geo\n\nfn main() {\n  let n = orig‸\n}\n";
    let (found, _) = at(main, &[]);
    assert!(imported(&found).iter().all(|(l, _)| l != "geo::origin"));
    let origin = entry(&found, "geo::origin");
    assert_eq!(origin.rank, rank::IMPORTED);
    assert_eq!(origin.additional, Vec::new());
    // A module that imports this one would close a cycle (E0040).
    let back = (
        "back.cheby",
        "import app\n\npub fn backward() -> Int {\n  1\n}\n",
    );
    let main = "fn main() {\n  let n = backw‸\n}\n";
    let (found, _) = at(main, &[back]);
    assert!(
        !found.items.iter().any(|e| e.label.ends_with("::backward")),
        "{:?}",
        labels(&found)
    );
}

#[test]
fn a_type_imports_its_module() {
    let main = "fn wait(d: Durat‸) {\n}\n";
    let (found, text) = at(main, &[]);
    let duration = found
        .items
        .iter()
        .find(|e| e.label == "duration::Duration")
        .unwrap_or_else(|| panic!("{:?}", labels(&found)));
    assert_eq!(duration.insert.text, "duration::Duration");
    assert_eq!(duration.description.as_deref(), Some("std::duration"));
    assert!(parses(&applied(&text, duration)));
    // No function in a type.
    assert!(
        imported(&found)
            .iter()
            .all(|(l, _)| l != "duration::seconds")
    );
}

#[test]
fn the_list_is_capped_and_incomplete() {
    let (found, _) = at("fn main() {\n  let n = in‸\n}\n", &[]);
    let names = imported(&found);
    // `Items::finish` keeps one item per label and kind, so items of one
    // name from several modules count once there.
    assert!(!names.is_empty() && names.len() <= CAP, "{}", names.len());
    assert!(found.incomplete);
    // Sorted by name, then module path, before the cap.
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
}

#[test]
fn a_function_value_is_written_bare() {
    // The parameter takes a function, so the item names it: no call, as
    // for the module's own items (X §4.1).
    let main = "import std::list\n\nfn main() {\n  let names = [\"a\"]\n  let n = list::map(names, lengt‸)\n}\n";
    let (found, text) = at(main, &[]);
    let length = found
        .items
        .iter()
        .find(|e| e.label == "string::length" && e.description.as_deref() == Some("std::string"))
        .unwrap_or_else(|| panic!("{:?}", imported(&found)));
    assert_eq!(length.insert.text, "string::length");
    assert!(!length.insert.snippet);
    assert_eq!(length.additional.len(), 1);
    assert!(parses(&applied(&text, length)));
    // Where a call is wanted, the call is written.
    let main = main.replace("list::map(names, lengt‸)", "lengt‸");
    let (found, _) = at(&main, &[]);
    let length = found
        .items
        .iter()
        .find(|e| e.label == "string::length" && e.description.as_deref() == Some("std::string"))
        .unwrap_or_else(|| panic!("{:?}", imported(&found)));
    assert_eq!(length.insert.text, "string::length(${1:text})$0");
}

/// A package module with a `scale`, as `std::duration` has one.
const SHAPE: (&str, &str) = (
    "shape.cheby",
    "//! Shapes.\n\npub type Shape {\n  Square(Float),\n}\n\npub fn scale(s: Shape, k: Float) -> Shape {\n  s\n}\n",
);

#[test]
fn an_imported_modules_items_rank_above_items_to_import() {
    // D-1507: `sca` with `shape` imported offers `shape::scale` with no
    // import edit at rank 30, ahead of `duration::scale` with one at 60.
    let main = "import app::shape\n\nfn main() {\n  let n = sca‸\n}\n";
    let (found, text) = at(main, &[SHAPE]);
    let ours = entry(&found, "shape::scale");
    assert_eq!(ours.rank, rank::IMPORTED);
    assert_eq!(ours.additional, Vec::new());
    assert_eq!(ours.insert.text, "shape::scale(${1:s}, ${2:k})$0");
    assert_eq!(ours.description.as_deref(), Some("app::shape"));
    let theirs = entry(&found, "duration::scale");
    assert_eq!(theirs.rank, rank::NOT_IMPORTED);
    assert_eq!(theirs.additional.len(), 1);
    let place = |label: &str| found.items.iter().position(|e| e.label == label);
    assert!(place("shape::scale") < place("duration::scale"));
    assert!(parses(&applied(&text, ours)));
    // So in a type.
    let main = "import app::shape\n\nfn f(s: Sha‸) {\n}\n";
    let (found, text) = at(main, &[SHAPE]);
    let shape = entry(&found, "shape::Shape");
    assert_eq!(shape.rank, rank::IMPORTED);
    assert_eq!(shape.additional, Vec::new());
    assert!(parses(&applied(&text, shape)));
}

#[test]
fn an_aliased_import_writes_the_alias() {
    // D-1507: the item is written through the import's binding.
    let main = "import app::shape as sh\n\nfn main() {\n  let n = sca‸\n}\n";
    let (found, text) = at(main, &[SHAPE]);
    let scale = entry(&found, "sh::scale");
    assert_eq!(scale.insert.text, "sh::scale(${1:s}, ${2:k})$0");
    assert_eq!(scale.additional, Vec::new());
    assert!(!labels(&found).contains(&"shape::scale"));
    assert!(parses(&applied(&text, scale)));
}

#[test]
fn imported_modules_items_need_no_auto_import() {
    // D-1507: with `completion.autoImport` off, the imported module's items
    // are still offered, and nothing to import.
    let off = Options {
        auto_import: false,
        ..WITH
    };
    let main = "import app::shape\n\nfn main() {\n  let n = sca‸\n}\n";
    let (found, _) = run(main, &[SHAPE], off, StdAccess::Index(base().index()));
    assert_eq!(entry(&found, "shape::scale").rank, rank::IMPORTED);
    assert!(imported(&found).is_empty(), "{:?}", imported(&found));
    // Below two characters they wait, so the list is incomplete when the
    // file imports a module (D-1487, D-1521), and complete when it imports
    // none.
    let short = main.replace("sca‸", "s‸");
    let (found, _) = run(&short, &[SHAPE], off, StdAccess::Index(base().index()));
    assert!(!labels(&found).contains(&"shape::scale"));
    assert!(found.incomplete);
    let none = "fn main() {\n  let n = s‸\n}\n";
    let (found, _) = run(none, &[SHAPE], off, StdAccess::Index(base().index()));
    assert!(!found.incomplete);
}

#[test]
fn an_item_of_another_module_is_labelled_by_its_path() {
    // D-1508: the label is the qualified name as written, `filterText` the
    // bare name, the description the module's whole path.
    let main = "import app::shape\n\nfn main() {\n  let n = sca‸\n}\n";
    let (found, _) = at(main, &[SHAPE]);
    for (label, name, module) in [
        ("shape::scale", "scale", "app::shape"),
        ("duration::scale", "scale", "std::duration"),
    ] {
        let item = entry(&found, label);
        assert_eq!(item.filter_text.as_deref(), Some(name), "{label}");
        assert_eq!(item.description.as_deref(), Some(module), "{label}");
    }
    // Through an alias that a clash forces (D-1395) too.
    let main = "fn main() {\n  let list = [1]\n  let n = revers‸\n}\n";
    let (found, _) = at(main, &[]);
    let reverse = entry(&found, "std_list::reverse");
    assert_eq!(reverse.filter_text.as_deref(), Some("reverse"));
}

/// Whether `found` holds an item written through a module path: an
/// imported module's item or one to import (D-1507, X §4.4).
fn has_path_items(found: &Completions) -> bool {
    found.items.iter().any(|e| e.label.contains("::"))
}

#[test]
fn before_a_written_path_no_item_writes_a_path() {
    // D-1521: accepting `string::starts_with` at `st‸::length` would write
    // `string::starts_with::length`, so where `::` follows the word no
    // item of an imported module and none to import is offered.
    let main = "import std::string\n\nfn main() {\n  let n = st‸::length(\"a\")\n  n\n}\n";
    let (found, text) = at(main, &[]);
    assert!(!has_path_items(&found), "{:?}", labels(&found));
    // The module's own name is still offered, written bare.
    let string = entry(&found, "string");
    assert_eq!(string.insert.text, "string");
    assert!(parses(&applied(&text, string)));
    // The same word with no `::` after it offers both kinds.
    let main = "import std::string\n\nfn main() {\n  let n = st‸\n  n\n}\n";
    let (found, _) = at(main, &[]);
    assert_eq!(entry(&found, "string::starts_with").rank, rank::IMPORTED);
    assert!(!imported(&found).is_empty(), "{:?}", labels(&found));
    // `::` after white space is not part of the word's path, so the items
    // stay.
    let main = "import std::string\n\nfn main() {\n  let n = st‸ ::length\n  n\n}\n";
    let (found, _) = at(main, &[]);
    assert!(has_path_items(&found), "{:?}", labels(&found));
    // A short partial does not mark the list incomplete there, since
    // nothing more comes at the second character (D-1525).
    let main = "import std::string\n\nfn main() {\n  let n = s‸::length(\"a\")\n  n\n}\n";
    let (found, _) = at(main, &[]);
    assert!(!has_path_items(&found), "{:?}", labels(&found));
    assert!(!found.incomplete);
    // With no `::` after it, it does (D-1487).
    let main = "import std::string\n\nfn main() {\n  let n = s‸\n  n\n}\n";
    let (found, _) = at(main, &[]);
    assert!(found.incomplete);
}

#[test]
fn before_a_written_path_in_a_type_no_item_writes_a_path() {
    // D-1521, in a type: `sh‸::Shape` offers neither `shape::Shape` nor
    // an item to import.
    let main = "import app::shape\n\nfn f(s: sh‸::Shape) -> Int {\n  1\n}\n";
    let (found, _) = at(main, &[SHAPE]);
    assert!(!has_path_items(&found), "{:?}", labels(&found));
    assert_eq!(entry(&found, "shape").insert.text, "shape");
    let main = "import app::shape\n\nfn f(s: sh‸) -> Int {\n  1\n}\n";
    let (found, _) = at(main, &[SHAPE]);
    assert_eq!(entry(&found, "shape::Shape").rank, rank::IMPORTED);
    // A short partial does not mark the list incomplete before `::`
    // (D-1525), and does with nothing after it (D-1487).
    let main = "import app::shape\n\nfn f(s: s‸::Shape) -> Int {\n  1\n}\n";
    let (found, _) = at(main, &[SHAPE]);
    assert!(!found.incomplete);
    let main = "import app::shape\n\nfn f(s: s‸) -> Int {\n  1\n}\n";
    let (found, _) = at(main, &[SHAPE]);
    assert!(found.incomplete);
}

#[test]
fn before_a_written_path_auto_import_off_offers_no_path() {
    // D-1521 with `completion.autoImport` off: the imported module's items
    // are left out before `::` too.
    let off = Options {
        auto_import: false,
        ..WITH
    };
    let main = "import std::string\n\nfn main() {\n  let n = st‸::length(\"a\")\n  n\n}\n";
    let (found, _) = run(main, &[], off, StdAccess::Index(base().index()));
    assert!(!has_path_items(&found), "{:?}", labels(&found));
    assert!(labels(&found).contains(&"string"));
}

#[test]
fn before_a_turbofish_items_are_offered_and_module_names_are_not() {
    // D-1525: `list::reverse::<Int>(…)` is a valid path, so before `::<`
    // the items written through a module path are offered as with no
    // `::`, written as names since the turbofish's call follows.
    let main = "import std::list\n\nfn main() {\n  let n = re‸::<Int>([1, 2])\n  n\n}\n";
    let (found, text) = at(main, &[]);
    let reverse = entry(&found, "list::reverse");
    assert_eq!(reverse.rank, rank::IMPORTED);
    assert_eq!(reverse.insert.text, "list::reverse");
    assert!(parses(&applied(&text, reverse)));
    // `list::<Int>` is no path (E0042), so no module name is offered.
    let modules: Vec<&str> = found
        .items
        .iter()
        .filter(|e| e.kind == EntryKind::Module)
        .map(|e| e.label.as_str())
        .collect();
    assert_eq!(modules, Vec::<&str>::new());
    // An item to import is offered with its import.
    let main = "fn main() {\n  let n = revers‸::<Int>([1, 2])\n  n\n}\n";
    let (found, text) = at(main, &[]);
    let reverse = found
        .items
        .iter()
        .find(|e| e.label == "list::reverse" && e.description.as_deref() == Some("std::list"))
        .unwrap_or_else(|| panic!("{:?}", labels(&found)));
    assert_eq!(reverse.rank, rank::NOT_IMPORTED);
    assert_eq!(reverse.insert.text, "list::reverse");
    assert_eq!(reverse.additional.len(), 1);
    assert!(parses(&applied(&text, reverse)));
    // A short partial waits, so the list is incomplete, as with no `::`
    // (D-1487).
    let main = "import std::list\n\nfn main() {\n  let n = r‸::<Int>([1, 2])\n  n\n}\n";
    let (found, _) = at(main, &[]);
    assert!(!has_path_items(&found), "{:?}", labels(&found));
    assert!(found.incomplete);
    // White space between `::` and `<` is still a turbofish, as the parser
    // reads it.
    let main = "import std::list\n\nfn main() {\n  let n = re‸:: <Int>([1, 2])\n  n\n}\n";
    let (found, _) = at(main, &[]);
    assert!(labels(&found).contains(&"list::reverse"));
    assert!(!labels(&found).contains(&"list"));
}

#[test]
fn a_turbofish_is_found_past_what_the_parser_skips() {
    // D-1529: the `<` is found as the parser reads a turbofish. Inside
    // `(` or `[`, it skips line breaks and comments (§2.8), so the `::` is
    // a turbofish's, and `list` would write `list::\n<Int>(…)` (E0042).
    let turbofish = [
        "import std::list\n\nfn main() {\n  let n = list::length(\n    re‸:: // c\n      <Int>([1]),\n  )\n  n\n}\n",
        "import std::list\n\nfn main() {\n  let m = [\n    re‸::\n      <Int>([1]),\n  ]\n  m\n}\n",
        // A block comment is an error token the parser skips (E0004).
        "import std::list\n\nfn main() {\n  let n = re‸::/* c */<Int>([1])\n  n\n}\n",
    ];
    for main in turbofish {
        let (found, _) = at(main, &[]);
        assert!(labels(&found).contains(&"list::reverse"), "{main}");
        assert!(!labels(&found).contains(&"list"), "{main}");
    }
    // Outside brackets, and in a block inside them, a line break after
    // `::` ends the path (E0013), so the `::` is a path's (D-1529), and
    // only what a path starts with is offered (D-1530).
    let path = [
        "import std::list\n\nfn main() {\n  let n = re‸::\n    <Int>([1])\n  n\n}\n",
        "import std::list\n\nfn main() {\n  let m = [\n    {\n      re‸::\n        <Int>([1])\n    },\n  ]\n  m\n}\n",
    ];
    for main in path {
        let (found, _) = at(main, &[]);
        assert!(!has_path_items(&found), "{main}: {:?}", labels(&found));
        assert!(labels(&found).contains(&"list"), "{main}");
    }
}

#[test]
fn before_a_turbofish_in_a_type_items_are_offered_and_module_names_are_not() {
    // D-1525 in a type, where `::<` is no type syntax: the rule is the
    // expression's, so no module name, which would write `shape::<Int>`.
    let main = "import app::shape\n\nfn f(s: sh‸::<Int>) -> Int {\n  1\n}\n";
    let (found, _) = at(main, &[SHAPE]);
    assert_eq!(entry(&found, "shape::Shape").rank, rank::IMPORTED);
    assert!(!labels(&found).contains(&"shape"), "{:?}", labels(&found));
}
