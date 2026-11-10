//! Import plans: where a new import goes in the text (D-1030, D-1148,
//! E0018), and the plan over a run: an existing binding, an alias on a
//! clash (D-1395), and refusals. Tests that analyze carry the Windows
//! ignore (D-545, D-641).

use std::sync::Arc;

use cheby_driver::analysis::{ModuleView, TextRange};

use super::{ImportPlan, import_plan, parse_path, placement};
use crate::features::DocText;
use crate::features::ide::hover::tests::{Temp, analyze};

/// `text` with the import of `path`, written as `line`, inserted where
/// [`placement`] puts it.
fn placed(text: &str, path: &str, line: &str) -> String {
    let doc = DocText::new("placed.cheby".into(), None, Arc::from(text)).unwrap();
    let (range, insert): (TextRange, String) = placement(doc.tree.root(), text, path, line);
    assert!(range.is_empty(), "an insertion");
    let at = usize::try_from(range.start()).unwrap();
    format!(
        "{}{insert}{}",
        text.get(..at).unwrap(),
        text.get(at..).unwrap()
    )
}

#[test]
fn an_import_goes_into_the_first_block_in_byte_order() {
    let text = "import app::geo\nimport std::io\n\nfn main() {}\n";
    assert_eq!(
        placed(text, "std::list", "import std::list"),
        "import app::geo\nimport std::io\nimport std::list\n\nfn main() {}\n"
    );
    assert_eq!(
        placed(text, "std::dict", "import std::dict"),
        "import app::geo\nimport std::dict\nimport std::io\n\nfn main() {}\n"
    );
    assert_eq!(
        placed(text, "app", "import app"),
        "import app\nimport app::geo\nimport std::io\n\nfn main() {}\n"
    );
}

#[test]
fn equal_paths_sort_by_their_whole_text() {
    // `import std :: list :: { map }` sorts before `import std :: list as
    // std_list`, since `:` is before `a` (D-1148).
    let text = "import std::list::{map}\n\nfn main() {}\n";
    assert_eq!(
        placed(text, "std::list", "import std::list as std_list"),
        "import std::list::{map}\nimport std::list as std_list\n\nfn main() {}\n"
    );
    assert_eq!(
        placed(text, "std::list", "import std::list"),
        "import std::list\nimport std::list::{map}\n\nfn main() {}\n"
    );
}

#[test]
fn only_the_first_block_is_searched() {
    // A blank line ends a block: the import goes after its last.
    let text = "import std::io\n\nimport std::array\n\nfn main() {}\n";
    assert_eq!(
        placed(text, "std::list", "import std::list"),
        "import std::io\nimport std::list\n\nimport std::array\n\nfn main() {}\n"
    );
    // So does a comment.
    let text = "import std::io\n// the rest\nimport std::array\n";
    assert_eq!(
        placed(text, "std::list", "import std::list"),
        "import std::io\nimport std::list\n// the rest\nimport std::array\n"
    );
}

#[test]
fn a_last_import_with_no_line_break_gets_one() {
    assert_eq!(
        placed("import std::io", "std::list", "import std::list"),
        "import std::io\nimport std::list\n"
    );
}

#[test]
fn with_no_import_it_follows_the_header() {
    // After the `//!` block and a blank line, with a blank line before the
    // item.
    assert_eq!(
        placed(
            "//! Doc.\n\nfn main() {}\n",
            "std::list",
            "import std::list"
        ),
        "//! Doc.\n\nimport std::list\n\nfn main() {}\n"
    );
    assert_eq!(
        placed("//! Doc.\nfn main() {}\n", "std::list", "import std::list"),
        "//! Doc.\n\nimport std::list\n\nfn main() {}\n"
    );
    // After the inner attributes, which follow the doc (E0018).
    assert_eq!(
        placed(
            "//! Doc.\n@!target(native)\n\nfn main() {}\n",
            "std::list",
            "import std::list"
        ),
        "//! Doc.\n@!target(native)\n\nimport std::list\n\nfn main() {}\n"
    );
    // A header that ends the file.
    assert_eq!(
        placed("//! Doc.", "std::list", "import std::list"),
        "//! Doc.\n\nimport std::list\n"
    );
    // No header: the file's start.
    assert_eq!(
        placed("fn main() {}\n", "std::list", "import std::list"),
        "import std::list\n\nfn main() {}\n"
    );
    assert_eq!(
        placed("", "std::list", "import std::list"),
        "import std::list\n"
    );
}

#[test]
fn paths_read_from_text() {
    let path = parse_path("std::json::decode").unwrap();
    assert_eq!(path.segments(), ["std", "json", "decode"]);
    assert!(parse_path("").is_none());
}

/// The plan for importing `target` into `main` of a package that also
/// holds `files`.
fn plan(main: &str, files: &[(&str, &str)], target: &str) -> Option<ImportPlan> {
    let mut all: Vec<(&str, &str)> = files.to_vec();
    all.push(("main.cheby", main));
    let temp = Temp::package("imports", &all);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path.clone(), main)]);
    let snapshot = &analyzed.snapshot;
    let view: ModuleView<'_> = snapshot
        .file_of(&path)
        .and_then(|f| snapshot.module_at(f))
        .expect("the module");
    import_plan(snapshot, view, None, &parse_path(target).unwrap())
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn an_imported_module_is_named_by_its_binding() {
    let main = "import std::list as l\n\nfn main() {\n  assert l::length([1]) == 1\n}\n";
    assert_eq!(
        plan(main, &[], "std::list"),
        Some(ImportPlan {
            name: "l".to_owned(),
            edit: None,
        })
    );
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn a_name_taken_anywhere_in_the_file_gets_an_alias() {
    // `let list` in another function still clashes: E0044 (D-1395).
    let main = "fn f() -> Int {\n  let list = 1\n  list\n}\n\nfn main() {\n  assert f() == 1\n}\n";
    let found = plan(main, &[], "std::list").expect("a plan");
    assert_eq!(found.name, "std_list");
    assert_eq!(
        found.edit.map(|(_, text)| text),
        Some("import std::list as std_list\n\n".to_owned())
    );
    // A function of the module takes the name too (E0041).
    let main = "fn list() -> Int {\n  1\n}\n\nfn main() {\n  assert list() == 1\n}\n";
    assert_eq!(
        plan(main, &[], "std::list").expect("a plan").name,
        "std_list"
    );
    // A package module's alias is its parent and its name.
    let main = "fn util() -> Int {\n  1\n}\n\nfn main() {\n  assert util() == 1\n}\n";
    let util = ("util.cheby", "pub fn one() -> Int {\n  1\n}\n");
    assert_eq!(
        plan(main, &[util], "app::util").expect("a plan").name,
        "app_util"
    );
}

#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn imports_that_would_be_errors_are_refused() {
    let main = "import app::util\n\nfn main() {\n  assert util::one() == 1\n}\n";
    // `util` imports `main`: importing it back closes a cycle (E0040).
    let util = (
        "util.cheby",
        "import app::other\n\npub fn one() -> Int {\n  other::two() - 1\n}\n",
    );
    let other = (
        "other.cheby",
        "import app\n\npub fn two() -> Int {\n  2\n}\n",
    );
    let free = "fn main() {\n  assert 1 == 1\n}\n";
    assert_eq!(plan(free, &[util, other], "app::other"), None);
    // The module itself.
    assert_eq!(plan(main, &[util, other], "app"), None);
    // A std module that is not loaded, with no index to say it ships.
    assert_eq!(plan(free, &[], "std::nowhere"), None);
}
