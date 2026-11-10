//! Completion after `|>` (X §4.3): the hole at the accepting parameter,
//! a one-parameter function bare (D-244, D-245), the ranks of each
//! source, `xs |> m::`, and an unknown left side. Each test runs the
//! front end, so each carries the Windows ignore (D-545, D-641).

use std::sync::Arc;

use cheby_driver::analysis::{Globals, StdBase};

use super::super::StdAccess;
use super::super::items::{EntryKind, rank};
use super::super::tests::ALL;
use super::call_text;
use crate::features::ide::completion::dot::tests::{
    IMPORTING, ON, applied, at, entry, labels, parses, run, slice,
};

#[test]
fn calls_put_the_hole_at_the_accepting_parameter() {
    assert_eq!(
        call_text("list::map", &["xs", "f"], 0, None),
        (
            Some("list::map(_, ${1:f})$0".to_owned()),
            "list::map(_, f)".to_owned()
        )
    );
    assert_eq!(
        call_text("put", &["n", "xs"], 1, None),
        (Some("put(${1:n}, _)$0".to_owned()), "put(n, _)".to_owned())
    );
    // One parameter piped: bare, never `f(_)` (D-245).
    assert_eq!(
        call_text("list::length", &["xs"], 0, None),
        (None, "list::length".to_owned())
    );
    // The call form fills the hole with the receiver.
    assert_eq!(
        call_text("list::length", &["xs"], 0, Some("ys")),
        (None, "list::length(ys)".to_owned())
    );
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn after_a_pipe_functions_take_the_hole() {
    let main = "import std::list\n\nfn twice(f: fn(Int) -> Int) -> fn(Int) -> Int {\n  fn(x) { f(f(x)) }\n}\n\nfn main() {\n  let inc = fn(x: Int) { x + 1 }\n  let pair = fn(a: Int, b: Int) { a + b }\n  let n = 1 |> ‸\n}\n";
    let (found, _) = at(main, ON);
    // `std::int` is the head's module, but the file does not import it, so
    // its functions wait for two characters (D-1479).
    assert!(!found.items.iter().any(|e| e.label == "show"));
    // A local of a one-parameter function type is bare, rank 15; one of
    // two parameters is not offered.
    let inc = entry(&found, "inc");
    assert_eq!(
        (inc.rank, inc.insert.text.as_str()),
        (rank::LOCAL_FN, "inc")
    );
    assert!(!found.items.iter().any(|e| e.label == "pair"));
    // A function whose parameter does not accept `Int` is left out.
    assert!(!found.items.iter().any(|e| e.label == "twice"));
    // Imported modules' names, so a qualified function can follow.
    let list = entry(&found, "list");
    assert_eq!(
        (list.kind, list.insert.text.as_str()),
        (EntryKind::Module, "list::")
    );
    // With two characters, the head's module: rank 10, with its import.
    let main = main.replace("let n = 1 |> ‸", "let s = \"a\"\n  let n = s |> le‸");
    let (found, text) = at(&main, IMPORTING);
    let length = found
        .items
        .iter()
        .find(|e| e.label == "length" && !e.additional.is_empty())
        .unwrap_or_else(|| panic!("{:?}", labels(&found)));
    assert_eq!(length.rank, rank::LOCAL);
    assert_eq!(length.insert.text, "string::length");
    assert_eq!(slice(&text, length.insert.range), "le");
    let done = applied(&text, length);
    assert!(done.contains("let n = s |> string::length\n"), "{done}");
    assert!(parses(&done));
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_partial_name_is_replaced() {
    let main = "fn main() {\n  let xs = [1, 2]\n  let n = xs |> le‸\n}\n";
    let (found, text) = at(main, IMPORTING);
    let length = entry(&found, "length");
    assert_eq!(slice(&text, length.insert.range), "le");
    assert_eq!(length.insert.text, "list::length");
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn other_loaded_modules_rank_forty_and_need_auto_import() {
    let main = "fn main() {\n  let xs = [1, 2]\n  let n = xs |> fr‸\n}\n";
    let with = super::super::Options {
        auto_import: true,
        ..ON
    };
    let (found, _) = at(main, with);
    // `set::from_list` takes a `List<T>`; `std::set` is loaded but not
    // imported.
    let from_list = found
        .items
        .iter()
        .find(|e| e.label == "from_list" && e.description.as_deref() == Some("set"))
        .unwrap_or_else(|| panic!("{:?}", labels(&found)));
    assert_eq!(from_list.rank, rank::PRELUDE);
    assert_eq!(from_list.insert.text, "set::from_list");
    assert_eq!(from_list.additional.len(), 1);
    // A bare type parameter accepts anything, so it does not count there.
    let (found, _) = at(&main.replace("fr‸", "de‸"), with);
    assert!(
        found
            .items
            .iter()
            .filter(|e| e.rank == rank::PRELUDE)
            .all(|e| e.label != "debug"),
        "{:?}",
        labels(&found)
    );
    // Without auto-import, nothing that needs an import, the head's module
    // included (D-1486).
    let (found, _) = at(main, ON);
    assert!(!found.items.iter().any(|e| e.label == "from_list"));
    assert!(
        found.items.iter().all(|e| e.additional.is_empty()),
        "{:?}",
        labels(&found)
    );
    assert!(!found.items.iter().any(|e| e.label == "length"));
    assert!(!found.incomplete);
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn imports_wait_for_two_characters_of_the_name() {
    let with = super::super::Options {
        auto_import: true,
        ..ON
    };
    // No partial word: only the head's module, which the file imports,
    // and module names (D-1479).
    let main = "import std::list\n\nfn main() {\n  let xs = [1, 2]\n  let n = xs |> ‸\n}\n";
    let (found, _) = at(main, with);
    assert!(
        found.items.iter().all(|e| e.additional.is_empty()),
        "{:?}",
        labels(&found)
    );
    assert!(found.items.iter().any(|e| e.label == "map"));
    // The imports start at the second character, so the list is
    // incomplete (D-1487).
    assert!(found.incomplete);
    // One character is too few.
    let (found, _) = at(&main.replace("|> ‸", "|> f‸"), with);
    assert!(found.items.iter().all(|e| e.additional.is_empty()));
    assert!(found.incomplete);
    // Without auto-import, no import waits, so the list is complete.
    let (found, _) = at(&main.replace("|> ‸", "|> f‸"), ON);
    assert!(!found.incomplete);
    // Two characters: a function that needs an import is offered only when
    // its name holds them as a subsequence; the imported module's are all
    // offered.
    let (found, _) = at(&main.replace("|> ‸", "|> fl‸"), with);
    let imported: Vec<&str> = found
        .items
        .iter()
        .filter(|e| !e.additional.is_empty())
        .map(|e| e.label.as_str())
        .collect();
    assert!(imported.contains(&"from_list"), "{imported:?}");
    assert!(
        imported
            .iter()
            .all(|name| super::super::auto_import::admits("fl", name)),
        "{imported:?}"
    );
    assert!(found.items.iter().any(|e| e.label == "map"));
}

#[test]
fn the_cap_keeps_the_first_by_name_then_path() {
    use super::super::Items;
    use super::super::auto_import::{CAP, push_capped};
    use super::super::items::{Entry, Insert};
    use cheby_driver::analysis::TextRange;
    let entry = |label: &str, path: &str| {
        let insert = Insert {
            range: TextRange::empty(0),
            text: label.to_owned(),
            snippet: false,
        };
        let mut entry = Entry::new(label, EntryKind::Function, rank::PRELUDE, insert);
        entry.description = Some(path.to_owned());
        entry
    };
    let found = |n: usize| -> Vec<(String, String, Entry)> {
        (0..n)
            .rev()
            .map(|k| {
                let name = format!("f{k:03}");
                (name.clone(), "std::b".to_owned(), entry(&name, "std::b"))
            })
            .chain(std::iter::once((
                "f000".to_owned(),
                "std::a".to_owned(),
                entry("f000", "std::a"),
            )))
            .collect()
    };
    let mut out = Items::default();
    push_capped(found(CAP), &mut out, |_, _, e| Some(e));
    let done = out.finish();
    assert!(done.incomplete, "one more than the cap");
    assert_eq!(done.items.len(), CAP);
    // `f000` of `std::a` sorts before `f000` of `std::b`, and the last
    // name is the one cut.
    assert_eq!(
        done.items.first().and_then(|e| e.description.as_deref()),
        Some("std::a")
    );
    assert!(
        !done
            .items
            .iter()
            .any(|e| e.label == format!("f{:03}", CAP - 1))
    );
    let mut out = Items::default();
    push_capped(found(CAP - 1), &mut out, |_, _, e| Some(e));
    let done = out.finish();
    assert!(!done.incomplete, "exactly the cap");
    assert_eq!(done.items.len(), CAP);
    // Two items of one name at the cut: the path decides which stays.
    let last = format!("f{:03}", CAP - 1);
    let tied: Vec<(String, String, Entry)> = (0..CAP - 1)
        .map(|k| format!("f{k:03}"))
        .map(|name| (name.clone(), "std::b".to_owned(), entry(&name, "std::b")))
        .chain(["std::b", "std::a"].map(|path| (last.clone(), path.to_owned(), entry(&last, path))))
        .collect();
    let mut out = Items::default();
    push_capped(tied, &mut out, |_, _, e| Some(e));
    let done = out.finish();
    assert!(done.incomplete);
    let kept: Vec<_> = done.items.iter().filter(|e| e.label == last).collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].description.as_deref(), Some("std::a"));
    // An item `build` leaves out does not count.
    let mut out = Items::default();
    push_capped(found(CAP), &mut out, |_, path, e| {
        (path != "std::a").then_some(e)
    });
    let done = out.finish();
    assert!(!done.incomplete);
    assert_eq!(done.items.len(), CAP);
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn std_modules_the_run_did_not_load_rank_sixty() {
    let base = StdBase::build(Arc::new(Globals::new())).expect("std checks");
    let with = super::super::Options {
        auto_import: true,
        ..ON
    };
    let main = "fn main() {\n  let s = \"a\"\n  let n = s |> ‸\n}\n";
    let (found, _) = run(main, &[], with, StdAccess::Index(base.index()));
    assert!(
        found.items.iter().all(|e| e.rank < rank::NOT_IMPORTED),
        "no partial word, no import (D-1479): {:?}",
        labels(&found)
    );
    let mut seen = false;
    for partial in ["en", "de", "pa", "to", "re"] {
        let main = main.replace("|> ‸", &format!("|> {partial}‸"));
        let (found, text) = run(&main, &[], with, StdAccess::Index(base.index()));
        let unloaded: Vec<_> = found
            .items
            .iter()
            .filter(|e| e.rank == rank::NOT_IMPORTED || e.rank == rank::NOT_IMPORTED + 5)
            .collect();
        seen |= !unloaded.is_empty();
        for item in unloaded {
            assert!(super::super::auto_import::admits(partial, &item.label));
            assert_eq!(item.additional.len(), 1, "{}", item.label);
            assert!(parses(&applied(&text, item)), "{}", item.label);
        }
    }
    assert!(seen, "some unloaded std function takes a `String`");
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_qualified_pipe_writes_the_function_part() {
    let main = "import std::list\n\nfn main() {\n  let xs = [1, 2]\n  let n = xs |> list::‸\n}\n";
    let (found, text) = at(main, ALL);
    let map = entry(&found, "map");
    assert_eq!(
        (map.rank, map.insert.text.as_str()),
        (rank::MODULE, "map(_, ${1:f})$0")
    );
    assert_eq!(entry(&found, "length").insert.text, "length");
    // A function that does not accept the left side stays, at rank 70,
    // with the hole first; one with no parameter is left out.
    let range = entry(&found, "range");
    assert_eq!(
        (range.rank, range.insert.text.as_str()),
        (rank::NOT_ACCEPTING, "range(_, ${1:end})$0")
    );
    assert!(!found.items.iter().any(|e| e.label == "new"));
    assert!(parses(&applied(&text, map)));
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn an_unknown_left_side_offers_no_function() {
    let main =
        "import std::list\n\nfn main() {\n  let f = fn(x: Int) { x }\n  let n = nowhere |> ‸\n}\n";
    let (found, _) = at(main, ON);
    assert_eq!(labels(&found), ["f", "list"]);
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn before_a_written_path_only_module_names_are_offered_bare() {
    let main = "import std::list\nimport std::string\n\nfn main() {\n  let xs = [\"a\"]\n  let n = xs |> st‸::length\n  n\n}\n";
    let (found, text) = at(main, ON);
    assert_eq!(labels(&found), ["list", "string"]);
    let string = entry(&found, "string");
    assert_eq!(string.insert.text, "string");
    assert!(parses(&applied(&text, string)));
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_parameter_whose_bound_fails_does_not_accept() {
    // `list::sum`'s `zero: T` needs `T: Add`, which `Option<Int>` lacks
    // (E0063), while `List<Int>` gives `T = Int`, which has it.
    let main = "import std::list\n\nfn main() {\n  let o = Some(1)\n  let n = o.‸\n  n\n}\n";
    let (found, _) = at(main, ON);
    assert!(!labels(&found).contains(&"sum"), "{:?}", labels(&found));
    let main = "import std::list\n\nfn main() {\n  let xs = [1, 2]\n  let n = xs.‸\n  n\n}\n";
    let (found, _) = at(main, ON);
    assert_eq!(
        entry(&found, "sum").insert.text,
        " |> list::sum(_, ${1:zero})$0"
    );
}
