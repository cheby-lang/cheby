//! Auto-import (X §4.4, D-1395): the two-character rule, the sources
//! (package modules, loaded std, std's index), the import as an
//! additional edit with an alias on a clash, the type namespace, and the
//! cap. Each test runs the front end, so each carries the Windows ignore
//! (D-545, D-641).

use std::sync::{Arc, OnceLock};

use cheby_driver::analysis::{Globals, StdBase, TextRange};

use super::super::items::rank;
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
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
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
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_loaded_module_is_imported_by_an_additional_edit() {
    let main = "//! Main.\n\nfn main() {\n  let xs = [1]\n  let n = xs_rev‸\n}\n";
    let (found, _) = at(main, &[]);
    assert!(imported(&found).is_empty(), "no name has `xs_rev`");
    let main = "//! Main.\n\nfn main() {\n  let xs = [1]\n  let n = revers‸\n}\n";
    let (found, text) = at(main, &[]);
    let reverse = found
        .items
        .iter()
        .find(|e| e.label == "reverse" && e.description.as_deref() == Some("std::list"))
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
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_taken_name_imports_with_an_alias() {
    let main = "fn main() {\n  let list = [1]\n  let n = revers‸\n}\n";
    let (found, _) = at(main, &[]);
    let reverse = found
        .items
        .iter()
        .find(|e| e.label == "reverse" && e.description.as_deref() == Some("std::list"))
        .unwrap_or_else(|| panic!("{:?}", imported(&found)));
    assert_eq!(reverse.insert.text, "std_list::reverse(${1:xs})$0");
    assert_eq!(
        reverse.additional.first().map(|(_, t)| t.as_str()),
        Some("import std::list as std_list\n\n")
    );
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn modules_the_run_did_not_load_come_from_the_index() {
    let main = "fn main() {\n  let n = url_enc‸\n}\n";
    let (found, text) = at(main, &[]);
    let encode = entry(&found, "url_encode");
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
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn package_modules_are_offered_and_imported_modules_are_not() {
    let main = "fn main() {\n  let n = orig‸\n}\n";
    let (found, _) = at(main, &[]);
    let origin = entry(&found, "origin");
    assert_eq!(origin.description.as_deref(), Some("app::geo"));
    assert_eq!(origin.insert.text, "geo::origin");
    // A package item of another module is visible; a `priv` one is not.
    let main = "fn main() {\n  let n = hidd‸\n}\n";
    let (found, _) = at(main, &[]);
    assert!(found.items.iter().any(|e| e.label == "hidden"));
    let main = "fn main() {\n  let n = secr‸\n}\n";
    let (found, _) = at(main, &[]);
    assert!(!found.items.iter().any(|e| e.label == "secret"));
    // `geo` is imported: its items are not offered again.
    let main = "import app::geo\n\nfn main() {\n  let n = orig‸\n}\n";
    let (found, _) = at(main, &[]);
    assert!(imported(&found).iter().all(|(l, _)| l != "origin"));
    // A module that imports this one would close a cycle (E0040).
    let back = (
        "back.cheby",
        "import app\n\npub fn backward() -> Int {\n  1\n}\n",
    );
    let main = "fn main() {\n  let n = backw‸\n}\n";
    let (found, _) = at(main, &[back]);
    assert!(
        !found.items.iter().any(|e| e.label == "backward"),
        "{:?}",
        labels(&found)
    );
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_type_imports_its_module() {
    let main = "fn wait(d: Durat‸) {\n}\n";
    let (found, text) = at(main, &[]);
    let duration = found
        .items
        .iter()
        .find(|e| e.label == "Duration")
        .unwrap_or_else(|| panic!("{:?}", labels(&found)));
    assert_eq!(duration.insert.text, "duration::Duration");
    assert_eq!(duration.description.as_deref(), Some("std::duration"));
    assert!(parses(&applied(&text, duration)));
    // No function in a type.
    assert!(imported(&found).iter().all(|(l, _)| l != "seconds"));
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
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
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_function_value_is_written_bare() {
    // The parameter takes a function, so the item names it: no call, as
    // for the module's own items (X §4.1).
    let main = "import std::list\n\nfn main() {\n  let names = [\"a\"]\n  let n = list::map(names, lengt‸)\n}\n";
    let (found, text) = at(main, &[]);
    let length = found
        .items
        .iter()
        .find(|e| e.label == "length" && e.description.as_deref() == Some("std::string"))
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
        .find(|e| e.label == "length" && e.description.as_deref() == Some("std::string"))
        .unwrap_or_else(|| panic!("{:?}", imported(&found)));
    assert_eq!(length.insert.text, "string::length(${1:text})$0");
}
