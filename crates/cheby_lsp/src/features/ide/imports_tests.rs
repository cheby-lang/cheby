//! Import plans: where a new import goes in the text (D-1030, D-1953,
//! E0018), and the plan over a run: an existing binding, an alias on a
//! clash (D-1395), and refusals. Tests that analyze run only the front
//! end, so they run on Windows too (D-1516).

use std::sync::Arc;

use cheby_driver::analysis::{ModuleView, TextRange};
use cheby_syntax::imports::{Misplaced, import_section};

use super::{ImportPlan, import_plan, parse_path, placement};
use crate::features::DocText;
use crate::features::ide::hover::tests::{Temp, analyze};

/// `text`, a file of the package `app`, with the import of `path`,
/// written as `line`, inserted where [`placement`] puts it.
fn placed(text: &str, path: &str, line: &str) -> String {
    let doc = DocText::new("placed.cheby".into(), None, Arc::from(text)).unwrap();
    let (range, insert): (TextRange, String) =
        placement(doc.tree.root(), text, Some("app"), path, line);
    assert!(range.is_empty(), "an insertion");
    let at = usize::try_from(range.start()).unwrap();
    format!(
        "{}{insert}{}",
        text.get(..at).unwrap(),
        text.get(at..).unwrap()
    )
}

#[test]
fn an_import_goes_into_the_block_of_its_class_in_byte_order() {
    let text = "import std::io\n\nimport json\n\nimport app::geo\n\nfn main() {}\n";
    assert_eq!(
        placed(text, "std::list", "import std::list"),
        "import std::io\nimport std::list\n\nimport json\n\nimport app::geo\n\nfn main() {}\n"
    );
    assert_eq!(
        placed(text, "std::dict", "import std::dict"),
        "import std::dict\nimport std::io\n\nimport json\n\nimport app::geo\n\nfn main() {}\n"
    );
    assert_eq!(
        placed(text, "app", "import app"),
        "import std::io\n\nimport json\n\nimport app\nimport app::geo\n\nfn main() {}\n"
    );
    assert_eq!(
        placed(text, "yaml", "import yaml"),
        "import std::io\n\nimport json\nimport yaml\n\nimport app::geo\n\nfn main() {}\n"
    );
}

#[test]
fn equal_paths_sort_by_their_whole_text() {
    // `import std :: list :: { map }` sorts before `import std :: list as
    // std_list`, since `:` is before `a` (D-1953).
    let text = "import std::list::{map}\n\nfn main() {}\n";
    assert_eq!(
        placed(text, "std::list", "import std::list as std_list"),
        "import std::list::{map}\nimport std::list as std_list\n\nfn main() {}\n"
    );
    assert_eq!(
        placed(text, "std::list", "import std::list"),
        "import std::list\nimport std::list::{map}\n\nfn main() {}\n"
    );
    // A list's tokens are spaced as the section's keys space them.
    let text = "import std::list::{map}\n\nfn main() {}\n";
    assert_eq!(
        placed(text, "std::list", "import std::list::{length, map}"),
        "import std::list::{length, map}\nimport std::list::{map}\n\nfn main() {}\n"
    );
}

/// D-1953: a file with one block gets the others, each at its place with
/// a blank line between two blocks, and stays in `cheby fmt`'s order.
#[test]
fn a_file_with_one_block_gets_three() {
    let text = "//! Doc.\n\nimport json\n\nfn main() {}\n";
    let with_std = placed(text, "std::io", "import std::io");
    assert_eq!(
        with_std,
        "//! Doc.\n\nimport std::io\n\nimport json\n\nfn main() {}\n"
    );
    let all = placed(&with_std, "app::geo", "import app::geo");
    assert_eq!(
        all,
        "//! Doc.\n\nimport std::io\n\nimport json\n\nimport app::geo\n\nfn main() {}\n"
    );
    assert_eq!(misplaced(&all), None);
    // The local block alone gets std's before it, and the other packages'
    // between the two.
    let local = placed("import app::geo\n", "std::io", "import std::io");
    assert_eq!(local, "import std::io\n\nimport app::geo\n");
    assert_eq!(
        placed(&local, "json", "import json"),
        "import std::io\n\nimport json\n\nimport app::geo\n"
    );
    // At the end of a file with no line break.
    assert_eq!(
        placed("import std::io", "app::geo", "import app::geo"),
        "import std::io\n\nimport app::geo\n"
    );
}

/// An insertion before an import goes before the comments that travel
/// with it (D-1953), and a header that a blank line keeps apart stays.
#[test]
fn an_import_goes_before_the_comments_of_the_next() {
    let text = "import std::io\n// the rest\nimport std::list // last\n";
    assert_eq!(
        placed(text, "std::json", "import std::json"),
        "import std::io\nimport std::json\n// the rest\nimport std::list // last\n"
    );
    assert_eq!(
        placed(text, "std::map", "import std::map"),
        "import std::io\n// the rest\nimport std::list // last\nimport std::map\n"
    );
    let text = "// header\n\n// about geo\nimport app::geo\n";
    assert_eq!(
        placed(text, "std::io", "import std::io"),
        "// header\n\nimport std::io\n\n// about geo\nimport app::geo\n"
    );
    // A file of no package has no local block: `app` is another package.
    let doc = DocText::new("placed.cheby".into(), None, Arc::from("import json\n")).unwrap();
    let (at, insert) = placement(doc.tree.root(), "import json\n", None, "app", "import app");
    assert_eq!((at, insert.as_str()), (TextRange::empty(0), "import app\n"));
}

/// The section of `text` in the package `app`: what `cheby fmt` would
/// change first.
fn misplaced(text: &str) -> Option<Misplaced> {
    let doc = DocText::new("placed.cheby".into(), None, Arc::from(text)).unwrap();
    import_section(doc.tree.root(), Some("app"))?.misplaced()
}

/// D-1953's property: every import placed into a section in `cheby fmt`'s
/// order leaves it in order, whatever blocks the section has and wherever
/// comments travel.
#[test]
fn a_section_in_order_stays_in_order() {
    let pool = [
        "import std::io",
        "import std::list",
        "import std::list::{map}",
        "import json",
        "import yaml::parse",
        "import app",
        "import app::geo",
    ];
    let path = |line: &str| {
        line.trim_start_matches("import ")
            .split("::{")
            .next()
            .unwrap()
            .to_owned()
    };
    let class = |line: &str| {
        if line.starts_with("import std") {
            0
        } else if line.starts_with("import app") {
            2
        } else {
            1
        }
    };
    // Every subset of the pool as an ordered section, with a comment above
    // its second import, then every import of the pool it lacks.
    for mask in 0_u32..(1 << pool.len()) {
        let kept: Vec<&str> = pool
            .iter()
            .enumerate()
            .filter(|(n, _)| mask & (1 << n) != 0)
            .map(|(_, line)| *line)
            .collect();
        let mut section = String::new();
        for (n, line) in kept.iter().enumerate() {
            if n > 0 {
                section.push('\n');
                if class(kept[n - 1]) != class(line) {
                    section.push('\n');
                }
            }
            if n == 1 {
                section.push_str("// note\n");
            }
            section.push_str(line);
        }
        let text = format!("{section}\n\nfn main() {{}}\n");
        assert_eq!(misplaced(&text), None, "{text}");
        for line in pool.iter().filter(|line| !kept.contains(line)) {
            let after = placed(&text, &path(line), line);
            assert_eq!(
                misplaced(&after),
                None,
                "{line} into:\n{text}\ngave:\n{after}"
            );
            assert!(after.ends_with("\n\nfn main() {}\n"), "{after}");
        }
    }
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
