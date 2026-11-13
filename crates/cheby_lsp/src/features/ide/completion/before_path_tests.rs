//! Completion before a path's `::` (D-1530): only what a path starts
//! with, by context, written with no `::` added, and interfaces only when
//! one segment follows (D-1539). A path's later segment, where a module's
//! interfaces come too (D-1538). Before a turbofish, only names that take
//! type arguments (D-1536), and nothing after `xs.` (D-1537). Then the
//! subject of a `let`, `let guard` and `let assert` pattern, with and
//! without a type annotation (D-1531). Each test runs only the front end,
//! so it runs on Windows too (D-1516).

use std::sync::{Arc, OnceLock};

use cheby_driver::analysis::{Globals, StdBase};

use super::dot::tests::{applied, entry, labels, parses, run};
use super::items::{EntryKind, rank};
use super::{Completions, Options, StdAccess};

/// Every option on, auto-import included.
const WITH: Options = Options {
    snippets: true,
    type_driven: true,
    auto_import: true,
    zed: false,
};

/// A module with an interface, a type that satisfies it (§8.2), a
/// function and a constant.
const SHAPE: (&str, &str) = (
    "shape.cheby",
    "//! Shapes.\n\n/// Has an area.\npub interface Shape {\n  fn area(Self) -> Float\n}\n\n/// A square.\npub type Square {\n  Square(Float),\n}\n\n/// The area of a square.\npub fn area(s: Square) -> Float {\n  let Square(side) = s\n  side * side\n}\n\n/// How many sides a square has.\npub const sides: Int = 4\n",
);

/// Std's base, built once for these tests.
fn base() -> &'static StdBase {
    static BASE: OnceLock<Arc<StdBase>> = OnceLock::new();
    BASE.get_or_init(|| StdBase::build(Arc::new(Globals::new())).expect("std checks"))
}

/// The completions at the `‸` of `main`, with `shape.cheby`, std's index
/// and every option on.
fn at(main: &str) -> (Completions, String) {
    run(main, &[SHAPE], WITH, StdAccess::Index(base().index()))
}

/// The labels of `found`'s items of `kind`.
fn of_kind(found: &Completions, kind: EntryKind) -> Vec<&str> {
    found
        .items
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.label.as_str())
        .collect()
}

/// Whether accepting each item of `found` gives text that parses.
fn every_item_parses(found: &Completions, text: &str) -> bool {
    found.items.iter().all(|e| parses(&applied(text, e)))
}

/// The imports the D-1530 tests share: a module, an alias, a module with
/// interfaces, and one imported with an item by name.
const IMPORTS: &str =
    "import app::shape\nimport std::list as l\nimport std::ops\nimport std::string::{length}\n\n";

/// `IMPORTS`, then a function `f` whose body holds `line`.
fn body(line: &str) -> String {
    format!(
        "{IMPORTS}const limit: Int = 3\n\nfn f(xs: List<Int>, s: shape::Square) -> Int {{\n  let words = [\"a\"]\n  {line}\n  limit\n}}\n"
    )
}

/// The modules `IMPORTS` binds, by label.
const MODULES: [&str; 3] = ["l", "ops", "shape"];

/// The interfaces of `std::ops` through its module.
const OPS: [&str; 6] = [
    "ops::Add",
    "ops::Compare",
    "ops::Div",
    "ops::Mul",
    "ops::Neg",
    "ops::Sub",
];

#[test]
fn before_a_path_an_expression_offers_modules_and_interfaces() {
    // D-1530: in an expression only modules and interfaces start a path,
    // an interface bare or through its module (D-173). The file's own
    // function and constant, the local, the name imported by name, the
    // keywords, the snippets and the prelude's constructors are left out.
    let (found, text) = at(&body("let n = st‸::length(\"a\")"));
    assert_eq!(of_kind(&found, EntryKind::Module), MODULES);
    let mut interfaces = vec!["Show", "shape::Shape"];
    interfaces.extend(OPS);
    interfaces.sort_unstable();
    let mut offered = of_kind(&found, EntryKind::Interface);
    offered.sort_unstable();
    assert_eq!(offered, interfaces);
    assert_eq!(
        found.items.len(),
        MODULES.len() + interfaces.len(),
        "{:?}",
        labels(&found)
    );
    // Nothing more comes at the second character, so the list is complete
    // (D-1525).
    assert!(!found.incomplete);
    // The modules rank 30, a bare interface by where it comes from, one
    // through its module 30 (D-1507).
    assert_eq!(entry(&found, "shape").rank, rank::IMPORTED);
    assert_eq!(entry(&found, "Show").rank, rank::PRELUDE);
    let shape = entry(&found, "shape::Shape");
    assert_eq!(shape.rank, rank::IMPORTED);
    assert_eq!(shape.filter_text.as_deref(), Some("Shape"));
    assert_eq!(shape.description.as_deref(), Some("app::shape"));
    assert_eq!(shape.additional, Vec::new());
    // Each is written as the file binds it, with no `::` added, and the
    // text parses.
    for item in &found.items {
        assert_eq!(item.insert.text, item.label, "{}", item.label);
    }
    assert!(every_item_parses(&found, &text));
    // An alias is the module's name in the file.
    let alias = entry(&found, "l");
    assert_eq!(alias.description.as_deref(), Some("std::list"));
    assert!(applied(&text, alias).contains("let n = l::length(\"a\")"));
    // A short partial gives the same complete list.
    let (short, _) = at(&body("let n = s‸::length(\"a\")"));
    assert_eq!(labels(&short), labels(&found));
    assert!(!short.incomplete);
    // A statement's start is an expression too.
    let (statement, _) = at(&body("st‸::length(\"a\")"));
    assert_eq!(labels(&statement), labels(&found));
}

#[test]
fn before_a_path_an_interface_names_its_function() {
    // D-1530 with D-173: `Compare::compare(a, b)` calls an interface
    // function, so before its `::` the interface is offered, bare when
    // imported by name, through its module when the module is imported.
    let main = "import std::ops::{Compare}\n\nfn f(a: Int, b: Int) -> Order {\n  Comp‸::compare(a, b)\n}\n";
    let (found, text) = run(main, &[], WITH, StdAccess::Index(base().index()));
    let compare = entry(&found, "Compare");
    assert_eq!(compare.kind, EntryKind::Interface);
    assert_eq!(compare.rank, rank::IMPORTED);
    assert_eq!(compare.insert.text, "Compare");
    assert!(applied(&text, compare).contains("  Compare::compare(a, b)\n"));
    assert_eq!(labels(&found), ["Compare", "Show"]);
    let main = "import std::ops\n\nfn f(a: Int, b: Int) -> Order {\n  Comp‸::compare(a, b)\n}\n";
    let (found, text) = run(main, &[], WITH, StdAccess::Index(base().index()));
    let compare = entry(&found, "ops::Compare");
    assert_eq!(compare.insert.text, "ops::Compare");
    assert_eq!(compare.filter_text.as_deref(), Some("Compare"));
    let done = applied(&text, compare);
    assert!(done.contains("  ops::Compare::compare(a, b)\n"), "{done}");
    assert!(parses(&done));
}

#[test]
fn before_a_path_through_a_module_only_modules_are_offered() {
    // D-1539: at `sha‸::Shape::area(x)` two segments follow the `::`, so
    // only a module starts the path. An interface, bare or through its
    // module, would write `Show::Shape::area(x)` or
    // `shape::Shape::Shape::area(x)`.
    let (found, text) = at(&body("let a = sha‸::Shape::area(s)"));
    assert_eq!(labels(&found), MODULES);
    assert!(!found.incomplete);
    let shape = entry(&found, "shape");
    assert_eq!(shape.insert.text, "shape");
    let done = applied(&text, shape);
    assert!(done.contains("let a = shape::Shape::area(s)"), "{done}");
    assert!(every_item_parses(&found, &text));
    // So after `|>`.
    let (found, _) = at(&body("let a = s |> sha‸::Shape::area"));
    assert_eq!(labels(&found), MODULES);
    // A turbofish after the segment counts too: only a module's function
    // or constructor takes type arguments there (E0023, D-1544).
    let (found, _) = at(&body("let a = sha‸::Square::<Int>(1.0)"));
    assert_eq!(labels(&found), MODULES);
    // Typed as the interface's name, one segment follows, so the
    // interface through its module writes the whole path.
    let (found, text) = at(&body("let a = Sha‸::area(s)"));
    let done = applied(&text, entry(&found, "shape::Shape"));
    assert!(done.contains("let a = shape::Shape::area(s)"), "{done}");
}

#[test]
fn the_segments_after_the_colons_are_read_as_the_parser_reads_them() {
    // D-1539 with D-1529: inside `[`, the parser reads past a line break
    // and a comment after the `::`, so two segments follow and only
    // modules are offered.
    let (found, _) = at(&body(
        "let a = [\n    sha‸:: // the interface\n      Shape::area(s),\n  ]",
    ));
    assert_eq!(labels(&found), MODULES);
    // Outside brackets a line break ends the path, so one segment
    // follows, and the interfaces are offered.
    let (found, _) = at(&body("let a = Sha‸::area\n  ::x"));
    assert!(
        labels(&found).contains(&"shape::Shape"),
        "{:?}",
        labels(&found)
    );
    // A space before the second `::` is skipped, as the parser skips it.
    let (found, _) = at(&body("let a = sha‸::Shape ::area(s)"));
    assert_eq!(labels(&found), MODULES);
}

#[test]
fn before_a_path_a_type_offers_modules_only() {
    // D-1530: in a type only a module starts a path. The type parameter,
    // the types in scope, `dyn` and the types to import are left out.
    let main = format!("{IMPORTS}fn g<T>(x: T, s: sh‸::Square) -> T {{\n  x\n}}\n");
    let (found, text) = at(&main);
    assert_eq!(labels(&found), MODULES);
    assert!(!found.incomplete);
    assert!(every_item_parses(&found, &text));
    // Also after `dyn`, where the interface's path follows.
    let main =
        format!("{IMPORTS}fn g(s: dyn sh‸::Shape) -> Float {{\n  shape::Shape::area(s)\n}}\n");
    let (found, _) = at(&main);
    assert_eq!(labels(&found), MODULES);
}

#[test]
fn before_a_path_a_pattern_offers_modules_only() {
    // D-1530: in a pattern only a module starts a path. Patterns offered
    // no module before; now they do before `::`. The subject's
    // constructors, those in scope and `_` are left out.
    let main = "import app::geo as g\nimport std::list\n\nfn f(s: g::Shape) -> Int {\n  case s {\n    ge‸::Square(_) => 1\n    _ => 0\n  }\n}\n";
    let (found, text) = run(main, &[], WITH, StdAccess::None);
    assert_eq!(labels(&found), ["g", "list"]);
    assert!(!found.incomplete);
    let alias = entry(&found, "g");
    assert_eq!(alias.insert.text, "g");
    assert!(parses(&applied(&text, alias)));
    // So in a `let` pattern, where the subject is known (D-1531).
    let main = "import app::geo\n\nfn f(p: geo::Point) -> Int {\n  let ge‸::Point { x, y } = p\n  x + y\n}\n";
    let (found, _) = run(main, &[], WITH, StdAccess::None);
    assert_eq!(labels(&found), ["geo"]);
}

#[test]
fn before_a_path_after_a_pipe_offers_modules_and_interfaces() {
    // D-1530 after `|>`: the stage's path starts with a module or an
    // interface, as `xs |> Show::show`. No function is offered.
    let (found, text) = at(&body("let t = xs |> st‸::length"));
    assert_eq!(of_kind(&found, EntryKind::Module), MODULES);
    assert!(labels(&found).contains(&"Show"));
    assert!(labels(&found).contains(&"shape::Shape"));
    assert_eq!(of_kind(&found, EntryKind::Function), Vec::<&str>::new());
    assert!(!found.incomplete);
    assert!(every_item_parses(&found, &text));
}

#[test]
fn before_a_path_after_a_dot_nothing_is_offered() {
    // D-1530: after `xs.` no path can start, so a field or a function in
    // the pipe form would write `xs |> l::length::length`.
    let (found, _) = at(&body("let k = xs.le‸::length"));
    assert_eq!(labels(&found), Vec::<&str>::new());
    assert!(!found.incomplete);
    // With no `::` after the word, the functions are offered.
    let (found, _) = at(&body("let k = xs.le‸"));
    assert!(labels(&found).contains(&"length"), "{:?}", labels(&found));
}

#[test]
fn before_a_turbofish_the_rule_of_a_path_does_not_apply() {
    // D-1530 covers a path's `::`, not a turbofish's (D-1525, D-1529):
    // before `::<` the names are offered as with no `::`, but no module.
    let (found, _) = at(&body("let r = re‸::<Int>(xs)"));
    assert!(
        labels(&found).contains(&"l::reverse"),
        "{:?}",
        labels(&found)
    );
    assert_eq!(of_kind(&found, EntryKind::Module), Vec::<&str>::new());
}

/// The kinds of `found`'s items, each once.
fn kinds(found: &Completions) -> Vec<EntryKind> {
    let mut kinds: Vec<EntryKind> = found.items.iter().map(|e| e.kind).collect();
    kinds.sort_unstable();
    kinds.dedup();
    kinds
}

#[test]
fn before_a_turbofish_an_expression_offers_only_names() {
    // D-1536: only a name stands before type arguments: functions and
    // constructors, bare or through a module path, and locals. The
    // keywords, the snippets, the constants, the modules and the
    // interfaces are left out, since `case::<Int>` or `limit::<Int>`
    // cannot be written.
    let (found, text) = at(&body("let r = re‸::<Int>(xs)"));
    assert_eq!(
        kinds(&found),
        [
            EntryKind::Function,
            EntryKind::Variable,
            EntryKind::Struct,
            EntryKind::EnumMember,
        ],
        "{:?}",
        labels(&found)
    );
    for name in [
        "f",
        "xs",
        "words",
        "Some",
        "None",
        "l::reverse",
        "shape::area",
    ] {
        assert!(
            labels(&found).contains(&name),
            "{name}: {:?}",
            labels(&found)
        );
    }
    for name in ["limit", "shape::sides", "case", "fn", "todo", "l", "Show"] {
        assert!(
            !labels(&found).contains(&name),
            "{name}: {:?}",
            labels(&found)
        );
    }
    // A name's text is the name alone, so the turbofish follows it.
    assert_eq!(entry(&found, "l::reverse").insert.text, "l::reverse");
    assert!(every_item_parses(&found, &text));
    // A statement's start offers no statement keyword or snippet either.
    let (statement, _) = at(&body("re‸::<Int>(xs)"));
    for name in ["let", "use", "guard", "let guard", "case {"] {
        assert!(!labels(&statement).contains(&name), "{name}");
    }
    assert!(labels(&statement).contains(&"l::reverse"));
}

#[test]
fn before_a_turbofish_after_a_pipe_offers_only_functions() {
    // D-1536 after `|>`: functions and local functions, no module.
    let (found, text) = at(&body("let r = xs |> re‸::<Int>"));
    assert!(
        labels(&found).contains(&"l::reverse"),
        "{:?}",
        labels(&found)
    );
    assert_eq!(kinds(&found), [EntryKind::Function]);
    assert!(every_item_parses(&found, &text));
}

#[test]
fn before_a_turbofish_a_qualified_path_offers_functions_and_constructors() {
    // D-1536 after `m::`: the module's functions and constructors, not
    // its constant or interface (D-1538), which take no type arguments.
    let (found, text) = at(&body("let r = shape::a‸::<Int>(s)"));
    assert_eq!(labels(&found), ["Square", "area"]);
    assert!(every_item_parses(&found, &text));
    // An interface's function takes no type arguments either (E0023).
    let (found, _) = at(&body("let r = shape::Shape::ar‸::<Int>(s)"));
    assert_eq!(labels(&found), Vec::<&str>::new());
    let (found, _) = at(&body("let r = Show::sh‸::<Int>(1)"));
    assert_eq!(labels(&found), Vec::<&str>::new());
}

#[test]
fn before_a_turbofish_a_type_and_a_pattern_offer_no_keyword() {
    // D-1536 and D-1544 in a type, where `::<` is no type syntax: the
    // names stay as D-1525 has them, but no `dyn` and no module.
    let main = format!("{IMPORTS}fn g(x: Li‸::<Int>) -> Int {{\n  1\n}}\n");
    let (found, _) = at(&main);
    assert!(labels(&found).contains(&"List"), "{:?}", labels(&found));
    assert!(!labels(&found).contains(&"dyn"));
    assert_eq!(of_kind(&found, EntryKind::Module), Vec::<&str>::new());
    // In a pattern, the constructors stay, but no `_`.
    let main = "import app::geo\n\nfn f(s: geo::Shape) -> Int {\n  case s {\n    Ci‸::<Int>(_) => 1\n    _ => 0\n  }\n}\n";
    let (found, _) = run(main, &[], WITH, StdAccess::None);
    assert!(
        labels(&found).contains(&"geo::Circle"),
        "{:?}",
        labels(&found)
    );
    assert!(!labels(&found).contains(&"_"));
}

#[test]
fn before_a_turbofish_after_a_dot_nothing_is_offered() {
    // D-1537: the pipe form puts its argument list before the turbofish,
    // so `map` would write `xs |> l::map(_, f)::<Int>`.
    let (found, _) = at(&body("let k = xs.le‸::<Int>"));
    assert_eq!(labels(&found), Vec::<&str>::new());
    assert!(!found.incomplete);
}

#[test]
fn after_a_module_its_interfaces_are_offered() {
    // D-1538: after `shape::` in an expression, the module's interface
    // comes with its function, constant and constructor, written with its
    // `::`, as a module is (D-1548), so `shape::Shape::area(s)` completes
    // a segment at a time.
    let (found, text) = at(&body("let a = shape::Sh‸"));
    let shape = entry(&found, "Shape");
    assert_eq!(shape.kind, EntryKind::Interface);
    assert_eq!(shape.rank, rank::MODULE);
    assert_eq!(shape.insert.text, "Shape::");
    assert_eq!(shape.additional, Vec::new());
    for name in ["Square", "area", "sides"] {
        assert!(
            labels(&found).contains(&name),
            "{name}: {:?}",
            labels(&found)
        );
    }
    assert!(applied(&text, shape).contains("let a = shape::Shape::\n"));
    // Then the interface's functions follow it, as before.
    let (found, text) = at(&body("let a = shape::Shape::ar‸"));
    assert_eq!(labels(&found), ["area"]);
    let done = applied(&text, entry(&found, "area"));
    assert!(done.contains("let a = shape::Shape::area()"), "{done}");
    // A type offered the interface already.
    let main = format!("{IMPORTS}fn g(x: dyn shape::Sh‸) -> Int {{\n  1\n}}\n");
    let (found, _) = at(&main);
    assert!(labels(&found).contains(&"Shape"));
}

#[test]
fn before_a_later_segments_colons_only_the_modules_interfaces_are_offered() {
    // D-1538: at `shape::Sh‸::area(s)` a path's `::` follows the segment,
    // so only the module's interfaces fit: `Square` would write E0013 and
    // `area` E0042.
    let (found, text) = at(&body("let a = shape::Sh‸::area(s)"));
    assert_eq!(labels(&found), ["Shape"]);
    assert!(!found.incomplete);
    let done = applied(&text, entry(&found, "Shape"));
    assert!(done.contains("let a = shape::Shape::area(s)"), "{done}");
    assert!(parses(&done));
    // So after `|>`, where the module's functions come without it.
    let (found, _) = at(&body("let a = s |> shape::Sh‸::area"));
    assert_eq!(labels(&found), ["Shape"]);
    let (found, _) = at(&body("let a = s |> shape::‸"));
    assert!(labels(&found).contains(&"Shape"), "{:?}", labels(&found));
    assert!(labels(&found).contains(&"area"));
}

/// The labels of the constructors ranked first, the subject's (X §4.1).
fn subject_ctors(found: &Completions) -> Vec<&str> {
    found
        .items
        .iter()
        .filter(|e| e.rank == rank::EXPECTED)
        .map(|e| e.label.as_str())
        .collect()
}

#[test]
fn let_guard_and_let_assert_read_the_value_as_the_subject() {
    // D-1531: the pattern of `let`, `let guard` and `let assert` is
    // completed with the value after `=` as its subject, also with a type
    // annotation, where it was the annotation before.
    let forms = [
        "let ge‸ = s",
        "let ge‸: geo::Shape = s",
        "let guard ge‸ = s else {\n    0\n  }",
        "let guard ge‸: geo::Shape = s else {\n    0\n  }",
        "let assert ge‸ = s",
        "let assert ge‸: geo::Shape = s",
    ];
    for form in forms {
        let main = format!("import app::geo\n\nfn f(s: geo::Shape) -> Int {{\n  {form}\n  1\n}}\n");
        let (found, _) = run(&main, &[], WITH, StdAccess::None);
        assert_eq!(
            subject_ctors(&found),
            ["geo::Circle", "geo::Square", "geo::Dot"],
            "{form}"
        );
        assert_eq!(labels(&found).last(), Some(&"_"), "{form}");
        assert!(
            of_kind(&found, EntryKind::Function).is_empty()
                && of_kind(&found, EntryKind::Snippet).is_empty(),
            "{form}: {:?}",
            labels(&found)
        );
    }
}
