//! Completion items on whole and broken text: each context's sources, the
//! ranks of X §4.1, keywords and snippets by context (X §4.6), and the L0
//! failure rule. Each test runs only the front end, so it runs on Windows
//! too (D-1516).

use std::sync::Arc;

use cheby_driver::analysis::{Globals, ModuleView, StdBase};

use super::items::{EntryKind, rank};
use super::{Completions, Entry, Options, StdAccess, complete};
use crate::features::DocText;
use crate::features::ide::hover::source;
use crate::features::ide::hover::tests::{Temp, analyze};

/// A module the main module imports, with public and package items.
pub(crate) const GEO: &str = r#"//! Shapes on a plane.

/// A point.
pub exposed type Point {
  x: Int,
  y: Int,
}

/// A shape.
pub exposed type Shape {
  Circle { radius: Float },
  Square(Float),
  Dot,
}

/// Adds two points.
pub fn add(a: Point, b: Point) -> Point {
  Point { x: a.x + b.x, y: a.y + b.y }
}

/// The old way.
@deprecated("use add")
pub fn plus(a: Point, b: Point) -> Point {
  add(a, b)
}

fn hidden() -> Int {
  secret()
}

priv fn secret() -> Int {
  1
}

/// The origin.
pub const origin: Point = Point { x: 0, y: 0 }
"#;

/// Every option on, as the standard test client and the default settings
/// have them.
pub(crate) const ALL: Options = Options {
    snippets: true,
    type_driven: true,
    auto_import: false,
    zed: false,
};

/// The completions at the `|` of `main`, the text of `src/main.cheby` in
/// a package that also holds `geo.cheby`.
pub(crate) fn at(main: &str, options: Options) -> Completions {
    at_with(main, options, StdAccess::None)
}

/// [`at`], with std's index from `std`.
pub(crate) fn at_with(main: &str, options: Options, std: StdAccess<'_>) -> Completions {
    let offset = main.find('|').expect("a cursor");
    let text = main.replacen('|', "", 1);
    let temp = Temp::package("completion", &[("geo.cheby", GEO), ("main.cheby", &text)]);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path.clone(), &text)]);
    let offset = u32::try_from(offset).unwrap();
    let snapshot = &analyzed.snapshot;
    let doc = source(snapshot, &path).expect("the document");
    let view = snapshot.file_of(&path).and_then(|f| snapshot.module_at(f));
    let tree = view.and_then(ModuleView::tree).expect("a tree");
    complete(snapshot, view, tree, doc.text, offset, options, std)
}

/// The labels, in order.
fn labels(found: &Completions) -> Vec<&str> {
    found.items.iter().map(|e| e.label.as_str()).collect()
}

/// The entry labelled `label`.
fn entry<'a>(found: &'a Completions, label: &str) -> &'a Entry {
    found
        .items
        .iter()
        .find(|e| e.label == label)
        .unwrap_or_else(|| panic!("no `{label}` in {:?}", labels(found)))
}

/// Whether some entry is labelled `label`.
fn has(found: &Completions, label: &str) -> bool {
    found.items.iter().any(|e| e.label == label)
}

const BODY: &str = r#"import app::geo
import std::list

const limit: Int = 3

fn need(n: Int) -> Int {
  n
}

fn main() {
  let count = 1
  let words = ["a"]
  let total = count + limit
  @@
}
"#;

/// `BODY` with `@@` replaced by `line`.
fn body(line: &str) -> String {
    BODY.replace("@@", line)
}

#[test]
fn an_expression_offers_locals_items_modules_the_prelude_and_keywords() {
    let found = at(&body("let y = |"), ALL);
    // Locals, nearest binding first.
    let locals: Vec<&str> = found
        .items
        .iter()
        .filter(|e| e.kind == EntryKind::Variable)
        .map(|e| e.label.as_str())
        .collect();
    assert_eq!(locals, ["total", "words", "count"]);
    assert_eq!(entry(&found, "total").rank, rank::LOCAL);
    assert_eq!(entry(&found, "total").detail.as_deref(), Some("Int"));
    // The module's items, a call with placeholders for a function.
    let need = entry(&found, "need");
    assert_eq!(need.rank, rank::MODULE);
    assert_eq!(need.insert.text, "need(${1:n})$0");
    assert!(need.insert.snippet);
    assert_eq!(need.detail.as_deref(), Some("fn need(n: Int) -> Int"));
    assert_eq!(need.label_detail.as_deref(), Some("(n)"));
    assert_eq!(entry(&found, "limit").kind, EntryKind::Constant);
    // Imported modules, written with `::`.
    let geo = entry(&found, "geo");
    assert_eq!((geo.kind, geo.rank), (EntryKind::Module, rank::IMPORTED));
    assert_eq!(geo.insert.text, "geo::");
    // The prelude's constructors.
    assert_eq!(entry(&found, "Some").rank, rank::PRELUDE);
    assert_eq!(entry(&found, "Some").insert.text, "Some($1)$0");
    assert_eq!(entry(&found, "None").insert.text, "None");
    // Expression keywords and snippets, no statement keyword.
    assert_eq!(entry(&found, "case").kind, EntryKind::Snippet);
    assert!(has(&found, "case {") && has(&found, "todo") && has(&found, "panic"));
    assert!(!has(&found, "let") && !has(&found, "guard") && !has(&found, "use"));
    // The binding's own statement holds the cursor, so `y` is not offered.
    assert!(!has(&found, "y"));
    // Sorted by rank.
    let ranks: Vec<u8> = found.items.iter().map(|e| e.rank).collect();
    assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "{ranks:?}");
}

#[test]
fn a_statement_offers_statement_keywords_and_snippets() {
    let found = at(&body("|"), ALL);
    for snippet in ["case", "case {", "guard", "let guard", "use", "fn"] {
        assert_eq!(entry(&found, snippet).kind, EntryKind::Snippet, "{snippet}");
    }
    for keyword in ["let", "assert", "panic", "todo"] {
        assert_eq!(entry(&found, keyword).kind, EntryKind::Keyword, "{keyword}");
    }
    assert_eq!(
        entry(&found, "guard").insert.text,
        "guard ${1:condition} else {\n  $0\n}"
    );
    // Without snippets the keywords stand, and a call is only its name.
    let plain = at(
        &body("|"),
        Options {
            snippets: false,
            ..ALL
        },
    );
    assert!(plain.items.iter().all(|e| e.kind != EntryKind::Snippet));
    for keyword in [
        "let", "use", "assert", "guard", "case", "panic", "todo", "fn",
    ] {
        assert_eq!(entry(&plain, keyword).kind, EntryKind::Keyword, "{keyword}");
    }
    assert_eq!(entry(&plain, "need").insert.text, "need");
    assert!(!entry(&plain, "need").insert.snippet);
}

#[test]
fn the_expected_type_ranks_first() {
    // A call's parameter.
    let found = at(&body("need(|"), ALL);
    assert_eq!(entry(&found, "count").rank, rank::EXPECTED);
    assert_eq!(entry(&found, "total").rank, rank::EXPECTED);
    assert_eq!(entry(&found, "words").rank, rank::LOCAL);
    assert_eq!(entry(&found, "need").rank, rank::EXPECTED);
    assert_eq!(found.items.first().map(|e| e.label.as_str()), Some("total"));
    // A `let` annotation.
    let found = at(&body("let o: Option<Int> = |"), ALL);
    assert_eq!(entry(&found, "Some").rank, rank::EXPECTED);
    assert_eq!(entry(&found, "None").rank, rank::EXPECTED);
    assert_eq!(entry(&found, "Ok").rank, rank::PRELUDE);
    // Off with `completion.typeDriven`.
    let off = at(
        &body("need(|"),
        Options {
            type_driven: false,
            ..ALL
        },
    );
    assert_eq!(entry(&off, "count").rank, rank::LOCAL);
}

#[test]
fn a_function_argument_is_a_value_and_a_written_paren_is_kept() {
    let found = at(&body("let f: fn(Int) -> Int = |"), ALL);
    assert_eq!(entry(&found, "need").insert.text, "need");
    let found = at(&body("let n = ne|ed(1)"), ALL);
    let need = entry(&found, "need");
    assert_eq!(need.insert.text, "need");
    let range = need.insert.range;
    assert_eq!(range.end() - range.start(), 4);
}

#[test]
fn qualified_names_of_a_module() {
    for line in ["geo::|", "let a = geo::|", "need(geo::|)"] {
        let found = at(&body(line), ALL);
        assert_eq!(
            labels(&found),
            [
                "Circle", "Dot", "Point", "Square", "add", "hidden", "origin", "plus"
            ],
            "{line}"
        );
        let add = entry(&found, "add");
        assert_eq!(add.insert.text, "add(${1:a}, ${2:b})$0");
        assert_eq!(add.description, Some("geo".to_owned()));
        assert!(entry(&found, "plus").deprecated);
        assert!(!entry(&found, "add").deprecated);
    }
    // A partial name, in a type, in a pattern.
    let found = at(&body("let p = geo::Po|"), ALL);
    assert!(has(&found, "Point") && has(&found, "add"));
    let found = at(&body("let p: geo::|"), ALL);
    assert_eq!(labels(&found), ["Point", "Shape"]);
    let found = at(
        &body("case geo::Dot {\n    geo::Ci| => 1\n    _ => 2\n  }"),
        ALL,
    );
    assert_eq!(labels(&found), ["Circle", "Dot", "Point", "Square"]);
    assert_eq!(entry(&found, "Circle").insert.text, "Circle { radius }");
    assert_eq!(entry(&found, "Point").insert.text, "Point { x, y }");
}

#[test]
fn qualified_names_of_a_std_module_and_of_an_interface() {
    let found = at(&body("list::|"), ALL);
    let map = entry(&found, "map");
    assert_eq!(map.insert.text, "map(${1:xs}, ${2:f})$0");
    assert_eq!(map.kind, EntryKind::Function);
    // `List` is homed in `std::list`, and is a type, not a value.
    assert!(!has(&found, "List"));
    let found = at(&body("Show::|"), ALL);
    assert_eq!(labels(&found), ["show"]);
    assert_eq!(entry(&found, "show").kind, EntryKind::Method);
}

#[test]
fn a_type_offers_types_parameters_modules_and_dyn() {
    let text = "import app::geo\n\nfn f<T>(x: |) {}\n";
    let found = at(text, ALL);
    assert_eq!(entry(&found, "T").kind, EntryKind::TypeParameter);
    assert_eq!(entry(&found, "T").rank, rank::LOCAL);
    assert_eq!(entry(&found, "Int").rank, rank::PRELUDE);
    assert_eq!(entry(&found, "Option").kind, EntryKind::Enum);
    assert_eq!(entry(&found, "geo").insert.text, "geo::");
    assert_eq!(entry(&found, "dyn").kind, EntryKind::Keyword);
    assert!(!has(&found, "Some") && !has(&found, "f"));
    let found = at("interface Twice {\n  fn twice(Self) -> |\n}\n", ALL);
    assert_eq!(entry(&found, "Self").kind, EntryKind::TypeParameter);
}

#[test]
fn a_case_arm_offers_the_subjects_constructors_first() {
    let text =
        "import app::geo\n\nfn f(s: geo::Shape, radius: Int) -> Int {\n  case s {\n    |\n  }\n}\n";
    let found = at(text, ALL);
    assert_eq!(
        labels(&found),
        ["geo::Circle", "geo::Square", "geo::Dot", "_"]
    );
    // `radius` is taken by a parameter, so the field is not punned.
    assert_eq!(
        entry(&found, "geo::Circle").insert.text,
        "geo::Circle { radius: _ }"
    );
    assert_eq!(
        entry(&found, "geo::Square").insert.text,
        "geo::Square(${1:_})"
    );
    assert_eq!(entry(&found, "geo::Dot").insert.text, "geo::Dot");
    let plain = at(
        text,
        Options {
            snippets: false,
            ..ALL
        },
    );
    assert_eq!(entry(&plain, "geo::Square").insert.text, "geo::Square(_)");
    // An unknown subject: the constructors in scope.
    let found = at("fn f() {\n  let |\n}\n", ALL);
    assert!(has(&found, "Some") && has(&found, "_") && has(&found, "True"));
    assert!(!has(&found, "f"));
}

#[test]
fn a_record_offers_the_fields_not_written() {
    let found = at(&body("let p = geo::Point { x: 1, |"), ALL);
    assert_eq!(labels(&found), ["y"]);
    assert_eq!(entry(&found, "y").insert.text, "y: ");
    assert_eq!(entry(&found, "y").detail.as_deref(), Some("Int"));
    let found = at(&body("let p = geo::Point { |}"), ALL);
    assert_eq!(labels(&found), ["x", "y", ".."]);
}

#[test]
fn an_interpolation_offers_locals_and_constants_only() {
    let found = at(&body("let s = \"{|}\""), ALL);
    assert_eq!(labels(&found), ["total", "words", "count", "limit"]);
}

#[test]
fn import_paths_and_lists() {
    let found = at("import |\n\nfn main() {}\n", ALL);
    assert_eq!(labels(&found), ["app", "std"]);
    let found = at("import app::|\n\nfn main() {}\n", ALL);
    assert_eq!(labels(&found), ["geo"]);
    assert_eq!(entry(&found, "geo").detail.as_deref(), Some("app::geo"));
    let found = at("import std::|\n\nfn main() {}\n", ALL);
    // Only the loaded std modules here: std's index adds the others.
    assert!(
        has(&found, "list") && has(&found, "option") && !has(&found, "io"),
        "{:?}",
        labels(&found)
    );
    let found = at("import app::geo::{add, |\n\nfn main() {}\n", ALL);
    assert_eq!(
        labels(&found),
        ["Point", "Shape", "hidden", "origin", "plus"]
    );
}

#[test]
fn attributes_and_items() {
    let found = at("@|\nfn main() {}\n", ALL);
    assert_eq!(
        labels(&found),
        ["async", "blocking", "deprecated", "external", "target"]
    );
    assert_eq!(
        entry(&found, "deprecated").insert.text,
        "deprecated(\"$1\")$0"
    );
    let found = at("@!|\n\nfn main() {}\n", ALL);
    assert_eq!(labels(&found), ["target"]);
    let found = at("fn main() {}\n\n|", ALL);
    assert_eq!(
        labels(&found),
        ["const", "fn", "interface", "priv", "pub", "test", "type"]
    );
    assert_eq!(entry(&found, "fn").kind, EntryKind::Snippet);
    let found = at("|\n\nfn main() {}\n", ALL);
    assert_eq!(entry(&found, "import").kind, EntryKind::Keyword);
    let found = at("fn main() {}\n\npub |", ALL);
    assert_eq!(
        labels(&found),
        ["const", "exposed", "fn", "interface", "type"]
    );
}

#[test]
fn nothing_in_comments_strings_and_mid_line() {
    assert_eq!(at(&body("// a no|te"), ALL), Completions::default());
    assert_eq!(at(&body("let s = \"te|xt\""), ALL), Completions::default());
    assert_eq!(at(&body("count |"), ALL), Completions::default());
}

#[test]
fn grammar_and_type_errors_keep_the_items() {
    // A type error elsewhere.
    let found = at(&body("let bad: Int = \"a\"\n  let y = |"), ALL);
    assert!(has(&found, "total") && has(&found, "bad") && has(&found, "need"));
    // `foo(1, ` followed by a `let`: the call's argument, then the `let`
    // is kept (D-1381).
    let found = at(&body("need(1, |\n  let after = 2"), ALL);
    assert!(has(&found, "total") && has(&found, "need"));
    // An open `case` and a missing arm body.
    let found = at(&body("case count {\n    1 =>|\n  }"), ALL);
    assert_eq!(entry(&found, "count").rank, rank::LOCAL);
    assert!(has(&found, "case"));
}

#[test]
fn item_keywords_follow_import_placement_and_visibility() {
    // `import` only before the first item (D-143), and `exposed` only
    // after `pub` (D-150), before `type` alone (D-121).
    let first = at("import std::list\n\n|\n\nfn main() {}\n", ALL);
    assert!(has(&first, "import") && has(&first, "pub"));
    assert!(!has(&first, "exposed"));
    let later = at("fn main() {}\n\n|\n", ALL);
    assert!(!has(&later, "import") && !has(&later, "exposed"));
    assert!(has(&later, "fn") && has(&later, "test"));
    let public = at("fn main() {}\n\npub |\n", ALL);
    assert!(has(&public, "exposed") && has(&public, "const"));
    let private = at("fn main() {}\n\npriv |\n", ALL);
    assert!(!has(&private, "exposed") && has(&private, "fn"));
    let exposed = at("fn main() {}\n\npub exposed |\n", ALL);
    assert_eq!(labels(&exposed), ["type"]);
}

#[test]
fn a_file_with_no_module_gets_keywords_and_snippets() {
    // X §4.1's failure rule: with no resolution, the context's keywords
    // and snippets, and no names.
    let text = "fn main() {\n  let x = 1\n  \n}\n";
    let temp = Temp::package("l0", &[("main.cheby", "fn main() {}\n")]);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path, "fn main() {}\n")]);
    let doc = DocText::new(temp.0.join("other.cheby"), None, text.into()).unwrap();
    let offset = u32::try_from(text.find("  \n").unwrap() + 2).unwrap();
    let found = complete(
        &analyzed.snapshot,
        None,
        &doc.tree,
        text,
        offset,
        ALL,
        StdAccess::None,
    );
    assert!(has(&found, "let") && has(&found, "guard"));
    assert!(!has(&found, "x") && !has(&found, "Some"));
    assert!(!found.incomplete);
}

#[test]
fn std_modules_the_run_did_not_load_come_from_the_index() {
    let base = StdBase::build(Arc::new(Globals::new())).expect("std checks");
    let std = StdAccess::Index(base.index());
    let found = at_with("import std::|\n\nfn main() {}\n", ALL, std);
    assert!(has(&found, "io") && has(&found, "list") && has(&found, "json"));
    let found = at_with("import std::json::|\n\nfn main() {}\n", ALL, std);
    assert!(has(&found, "decode"), "{:?}", labels(&found));
    let found = at_with("import std::io::{|\n\nfn main() {}\n", ALL, std);
    let println = entry(&found, "println");
    assert_eq!(println.kind, EntryKind::Function);
    assert!(
        println
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("fn println("))
    );
    assert!(println.key.is_some());
}

#[test]
fn what_follows_the_word_is_not_written_again() {
    use super::dot::tests::{applied, at as at_caret, parses};
    let cases = [
        // A module before `::`, also before a turbofish's.
        (
            "import std::list\n\nfn main() {\n  let xs = [1]\n  let n = li‸::length(xs)\n  n\n}\n",
            "list",
            "list",
        ),
        (
            "fn make<T>(x: Int) -> List<T> {\n  []\n}\n\nfn main() {\n  let a = mak‸::<Int>(0)\n  a\n}\n",
            "make",
            "make",
        ),
        // A field name before `:`.
        (
            "type Grid {\n  cells: Int,\n  live: Int,\n}\n\nfn main() {\n  let live = 1\n  let g = Grid { ce‸: live, live }\n  g\n}\n",
            "cells",
            "cells",
        ),
        // A constructor before its fields.
        (
            "type Sh {\n  Ci { r: Int },\n  Sq(Int),\n}\n\nfn f(s: Sh) -> Int {\n  case s {\n    Sq‸(n) => n\n    Ci { r } => r\n  }\n}\n",
            "Sq",
            "Sq",
        ),
        (
            "type Sh {\n  Ci { r: Int },\n  Sq(Int),\n}\n\nfn f(s: Sh) -> Int {\n  case s {\n    Sq(n) => n\n    C‸ { r } => r\n  }\n}\n",
            "Ci",
            "Ci",
        ),
    ];
    for (main, label, written) in cases {
        let (found, text) = at_caret(main, ALL);
        let item = entry(&found, label);
        assert_eq!(item.insert.text, written, "{main}");
        assert!(parses(&applied(&text, item)), "{main}");
    }
}

#[test]
fn an_arm_of_a_case_with_no_subject_is_an_expression() {
    let found = at(
        "fn main() {\n  let total = 3\n  let y = case {\n    tot| > 2 => 1\n    _ => 0\n  }\n  y\n}\n",
        ALL,
    );
    assert_eq!(entry(&found, "total").kind, EntryKind::Variable);
    assert!(!has(&found, "_"));
}

#[test]
fn a_qualified_function_where_a_function_is_expected_is_its_name() {
    let found = at(
        "import std::list\n\nfn main() {\n  let words = [\"a\"]\n  let n = list::map(words, list::len|)\n  n\n}\n",
        ALL,
    );
    assert_eq!(entry(&found, "length").insert.text, "length");
    let found = at(
        "import std::list\n\nfn main() {\n  let words = [\"a\"]\n  let n = list::map(words, list::|)\n  n\n}\n",
        ALL,
    );
    assert_eq!(entry(&found, "length").insert.text, "length");
}

#[test]
fn a_parameter_type_at_the_end_of_the_file_offers_type_parameters() {
    // X §4.1's `fn f(x: ` with nothing after it: the token at the cursor is
    // the file's trailing white space, outside the function.
    for text in ["fn first<T>(x: |", "fn first<T>(x: |\n"] {
        let found = at(text, ALL);
        assert_eq!(entry(&found, "T").rank, rank::LOCAL, "{text:?}");
    }
}
