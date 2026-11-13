//! Completion of the paths that call an interface function (D-173): its
//! functions in the pipe form after `xs |> I::` and `xs |> m::I::`
//! (D-1545), the interfaces at a path's first segment, written with the
//! `::` of the path (D-1546), nothing at a later segment before `::` in a
//! type or a pattern (D-1547), an interface at a later segment
//! written `Shape::` (D-1548), and an interface's function as a value
//! (X §4.1, D-500). Each test runs only the front end, so it
//! runs on Windows too (D-1516).

use std::sync::{Arc, OnceLock};

use cheby_driver::analysis::{Globals, StdBase};

use super::dot::tests::{IMPORTING, ON, applied, entry, expand, labels, parses, run};
use super::items::{EntryKind, rank};
use super::{Completions, Options, StdAccess};

/// No snippets, as a plain-text client has it.
const PLAIN: Options = Options {
    snippets: false,
    ..ON
};

/// A module with an interface, a type that satisfies it (§8.2) and a
/// function.
const SHAPE: (&str, &str) = (
    "shape.cheby",
    "//! Shapes.\n\n/// Has an area.\npub interface Shape {\n  fn area(Self) -> Int\n}\n\n/// A square.\npub type Square {\n  Square(Int),\n}\n\n/// The area of a square.\npub fn area(s: Square) -> Int {\n  let Square(side) = s\n  side * side\n}\n",
);

/// Std's base, built once for these tests.
fn base() -> &'static StdBase {
    static BASE: OnceLock<Arc<StdBase>> = OnceLock::new();
    BASE.get_or_init(|| StdBase::build(Arc::new(Globals::new())).expect("std checks"))
}

/// The completions at the `‸` of `main`, with `shape.cheby` and std's
/// index.
fn at(main: &str, options: Options) -> (Completions, String) {
    run(main, &[SHAPE], options, StdAccess::Index(base().index()))
}

/// `main` with `line` in a function whose parameters are `s`, a square,
/// and `a` and `b`, two numbers, after `imports`.
fn body(imports: &str, line: &str) -> String {
    format!(
        "{imports}\nfn f(s: shape::Square, a: Int, b: Int) -> Nil {{\n  {line}\n  assert a == b\n}}\n"
    )
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

#[test]
fn after_a_pipe_an_interfaces_functions_take_the_hole() {
    // D-1545: after `xs |> m::I::` the interface's functions come in the
    // pipe form, a one-parameter function bare (D-245).
    let (found, text) = at(
        &body("import app::shape\n", "let n = s |> shape::Shape::‸"),
        ON,
    );
    assert_eq!(labels(&found), ["area"]);
    let area = entry(&found, "area");
    assert_eq!(
        (area.kind, area.rank, area.insert.text.as_str()),
        (EntryKind::Method, rank::MODULE, "area")
    );
    let done = applied(&text, area);
    assert!(done.contains("let n = s |> shape::Shape::area\n"), "{done}");
    assert!(parses(&done));
    // After `I::`, the prelude's `Show` among them.
    let (found, text) = at(&body("", "let t = a |> Show::sh‸"), ON);
    let show = entry(&found, "show");
    assert_eq!(show.insert.text, "show");
    assert!(parses(&applied(&text, show)));
    // A function of two parameters puts the hole at the first `Self`,
    // through a module or an alias imported by name.
    for (imports, path) in [
        ("import std::ops\n", "ops::Compare"),
        ("import std::ops::{Compare as Cmp}\n", "Cmp"),
    ] {
        let (found, text) = at(&body(imports, &format!("let o = a |> {path}::c‸")), ON);
        let compare = entry(&found, "compare");
        assert_eq!(compare.insert.text, "compare(_, $1)$0", "{path}");
        assert!(compare.insert.snippet);
        let done = applied(&text, compare);
        assert!(
            done.contains(&format!("let o = a |> {path}::compare(_, )\n")),
            "{done}"
        );
        // A client with no snippets gets the name, as in an expression.
        let (found, _) = at(&body(imports, &format!("let o = a |> {path}::c‸")), PLAIN);
        assert_eq!(entry(&found, "compare").insert.text, "compare");
    }
}

#[test]
fn after_a_pipe_a_later_self_ranks_later_and_no_self_parameter_is_left_out() {
    // D-1545: the hole is the first parameter of type `Self`. A later one
    // ranks five later, as a module's function does (X §4.3). A function
    // whose `Self` is only inside a parameter's type has no such
    // parameter, so it is not offered.
    let main = "interface Scale {\n  fn by(Int, Self) -> Self\n  fn twice(Self, Self) -> Self\n  fn all(List<Self>) -> Bool\n}\n\nfn f(a: Int) -> Nil {\n  let n = a |> Scale::‸\n}\n";
    let (found, _) = at(main, ON);
    assert_eq!(labels(&found), ["twice", "by"]);
    let by = entry(&found, "by");
    assert_eq!(
        (by.rank, by.insert.text.as_str()),
        (rank::MODULE + 5, "by($1, _)$0")
    );
    assert_eq!(entry(&found, "twice").insert.text, "twice(_, $1)$0");
    // In an expression every function is still offered, with no hole.
    let main = main.replace("a |> Scale::‸", "Scale::‸");
    let (found, _) = at(&main, ON);
    assert_eq!(labels(&found), ["all", "by", "twice"]);
    assert_eq!(entry(&found, "by").insert.text, "by($1, $2)$0");
}

#[test]
fn after_a_pipe_no_interface_function_before_colons() {
    // D-1545 with D-1536: an interface's function takes no type arguments
    // (E0023), and no path follows it.
    for line in [
        "let t = a |> Show::sh‸::<Int>",
        "let t = a |> Show::sh‸::x",
        "let t = a |> Show::‸::x",
    ] {
        let (found, _) = at(&body("", line), ON);
        assert_eq!(labels(&found), Vec::<&str>::new(), "{line}");
    }
}

#[test]
fn an_expression_offers_the_interfaces_in_scope_with_their_colons() {
    // D-1546: the module's own interface, one imported by name and the
    // prelude's, written with the path's `::`, as a module is, ranked by
    // where they come from.
    let main = "import std::ops::{Compare}\n\ninterface Sized {\n  fn size(Self) -> Int\n}\n\nfn f(a: Int) -> Nil {\n  let n = ‸\n}\n";
    let (found, text) = at(main, ON);
    for (label, rank) in [
        ("Sized", rank::MODULE),
        ("Compare", rank::IMPORTED),
        ("Show", rank::PRELUDE),
    ] {
        let item = entry(&found, label);
        assert_eq!(
            (item.kind, item.rank, item.insert.text.clone()),
            (EntryKind::Interface, rank, format!("{label}::")),
            "{label}"
        );
        assert_eq!(item.additional, Vec::new());
        assert_eq!(item.filter_text, None);
        assert!(applied(&text, item).contains(&format!("let n = {label}::\n")));
    }
    assert_eq!(of_kind(&found, EntryKind::Interface).len(), 3);
    // A statement's start is an expression too, and so is a position that
    // takes a function's value, as `list::map(xs, Show::show)` (§8.5).
    let (found, _) = at(&main.replace("let n = ‸", "Sh‸"), ON);
    assert_eq!(entry(&found, "Show").insert.text, "Show::");
    let main =
        "import std::list\n\nfn f(xs: List<Int>) -> Nil {\n  let n = list::map(xs, Sh‸)\n}\n";
    let (found, _) = at(main, ON);
    assert_eq!(entry(&found, "Show").insert.text, "Show::");
    // Before a turbofish no interface fits (D-1536).
    let (found, _) = at(&body("", "let n = Sh‸::<Int>(a)"), ON);
    assert_eq!(of_kind(&found, EntryKind::Interface), Vec::<&str>::new());
}

#[test]
fn an_expression_offers_the_interfaces_of_imported_modules_from_two_characters() {
    // D-1546 with D-1507: through the import's binding, labelled by the
    // path and filtered by the name, at rank 30 with no import edit.
    let main = body("import std::ops\n", "let o = Comp‸");
    let (found, text) = at(&main, ON);
    let compare = entry(&found, "ops::Compare");
    assert_eq!(
        (compare.kind, compare.rank, compare.insert.text.as_str()),
        (EntryKind::Interface, rank::IMPORTED, "ops::Compare::")
    );
    assert_eq!(compare.filter_text.as_deref(), Some("Compare"));
    assert_eq!(compare.description.as_deref(), Some("std::ops"));
    assert_eq!(compare.additional, Vec::new());
    assert!(applied(&text, compare).contains("let o = ops::Compare::\n"));
    // A one-character partial waits, and the list says so (D-1521).
    let (found, _) = at(&body("import std::ops\n", "let o = C‸"), ON);
    assert!(!labels(&found).contains(&"ops::Compare"));
    assert!(found.incomplete);
    // Imported both by name and through its module, it is offered both
    // ways (D-1521).
    let (found, _) = at(
        &body(
            "import std::ops\nimport std::ops::{Compare}\n",
            "let o = Comp‸",
        ),
        ON,
    );
    assert_eq!(entry(&found, "Compare").insert.text, "Compare::");
    assert_eq!(entry(&found, "ops::Compare").insert.text, "ops::Compare::");
}

#[test]
fn auto_import_offers_interfaces_with_their_import() {
    // D-1546 with D-1479: with `completion.autoImport` on, an interface of
    // a module the file does not import comes with its import edit, at
    // rank 60, as a function to import does.
    let main = body("import app::shape\n", "let o = Comp‸");
    let (found, text) = at(&main, IMPORTING);
    let compare = entry(&found, "ops::Compare");
    assert_eq!(
        (compare.rank, compare.insert.text.as_str()),
        (rank::NOT_IMPORTED, "ops::Compare::")
    );
    assert_eq!(compare.filter_text.as_deref(), Some("Compare"));
    let done = applied(&text, compare);
    assert!(done.contains("import std::ops\n"), "{done}");
    assert!(done.contains("let o = ops::Compare::\n"), "{done}");
    // Off, it is not offered.
    let (found, _) = at(&main, ON);
    assert!(!labels(&found).contains(&"ops::Compare"));
    // In a type, an interface stays a type, with no `::`.
    let main = "import app::shape\n\nfn g(x: dyn Comp‸) -> Int {\n  1\n}\n";
    let (found, _) = at(main, IMPORTING);
    assert_eq!(entry(&found, "ops::Compare").insert.text, "ops::Compare");
}

#[test]
fn after_a_pipe_the_interfaces_start_a_path() {
    // D-1546 after `|>`: those in scope at any partial, written `Show::`,
    // also with the left side's type unknown, as the modules are.
    let (found, _) = at(&body("import app::shape\n", "let t = s |> ‸"), ON);
    let show = entry(&found, "Show");
    assert_eq!(
        (show.kind, show.rank, show.insert.text.as_str()),
        (EntryKind::Interface, rank::PRELUDE, "Show::")
    );
    let (found, _) = at(&body("", "let t = nowhere |> Sh‸"), ON);
    assert_eq!(entry(&found, "Show").insert.text, "Show::");
    // An imported module's from two characters, at rank 30.
    let (found, text) = at(&body("import app::shape\n", "let t = s |> Sh‸"), ON);
    let shape = entry(&found, "shape::Shape");
    assert_eq!(
        (shape.rank, shape.insert.text.as_str()),
        (rank::IMPORTED, "shape::Shape::")
    );
    assert!(applied(&text, shape).contains("let t = s |> shape::Shape::\n"));
    // Below two characters the list is incomplete, since that interface
    // comes at the second (D-1521), but not when no imported module has
    // an interface.
    let (found, _) = at(&body("import app::shape\n", "let t = s |> S‸"), ON);
    assert!(!labels(&found).contains(&"shape::Shape"));
    assert!(found.incomplete);
    let main = "import std::list\n\nfn f(xs: List<Int>) -> Nil {\n  let t = xs |> S‸\n}\n";
    let (found, _) = at(main, ON);
    assert!(!found.incomplete, "{:?}", labels(&found));
    // With `completion.autoImport` on, one to import comes with its edit,
    // ranked as a function of its source after `|>` (X §4.3).
    let (found, text) = at(
        &body("import app::shape\n", "let o = a |> Comp‸"),
        IMPORTING,
    );
    let compare = entry(&found, "ops::Compare");
    assert!(
        [rank::PRELUDE, rank::NOT_IMPORTED].contains(&compare.rank),
        "{}",
        compare.rank
    );
    assert_eq!(compare.insert.text, "ops::Compare::");
    assert!(applied(&text, compare).contains("import std::ops\n"));
    // Before a turbofish none fits (D-1536).
    let (found, _) = at(&body("import app::shape\n", "let t = s |> Sh‸::<Int>"), ON);
    assert_eq!(of_kind(&found, EntryKind::Interface), Vec::<&str>::new());
}

#[test]
fn after_a_pipe_an_interface_imported_by_name_comes_once() {
    // D-1550: after `|>`, an interface the file imports by name is
    // written by that name only, and not offered again through its
    // module, whether the module is imported too or would be imported by
    // an edit, which would leave the first import unused (W0003), as a
    // pipe's function imported by name comes once.
    for (imports, options) in [
        ("import std::ops::{Compare}\n", IMPORTING),
        ("import std::ops\nimport std::ops::{Compare}\n", ON),
        ("import std::ops\nimport std::ops::{Compare}\n", IMPORTING),
    ] {
        let main = body(imports, "let o = a |> Comp‸");
        let (found, _) = at(&main, options);
        let compare = entry(&found, "Compare");
        assert_eq!(
            (compare.insert.text.as_str(), compare.additional.len()),
            ("Compare::", 0),
            "{imports}"
        );
        assert_eq!(
            found
                .items
                .iter()
                .filter(|e| e.insert.text.ends_with("Compare::"))
                .count(),
            1,
            "{imports}: {:?}",
            labels(&found)
        );
    }
    // The module's other interfaces still come through it.
    let main = body(
        "import std::ops\nimport std::ops::{Compare}\n",
        "let o = a |> Ad‸",
    );
    let (found, _) = at(&main, ON);
    assert_eq!(entry(&found, "ops::Add").insert.text, "ops::Add::");
    // The function it names is not offered twice either.
    let main = body("import std::int::{compare}\n", "let o = a |> comp‸");
    let (found, _) = at(&main, IMPORTING);
    assert!(!labels(&found).contains(&"int::compare"));
    // In an expression both ways stay (D-1521), for a function too.
    let main = body("import std::ops::{Compare}\n", "let o = Comp‸");
    let (found, _) = at(&main, IMPORTING);
    assert_eq!(entry(&found, "Compare").insert.text, "Compare::");
    assert_eq!(entry(&found, "ops::Compare").insert.text, "ops::Compare::");
    let main = body(
        "import std::ops\nimport std::ops::{Compare}\n",
        "let o = Comp‸",
    );
    let (found, _) = at(&main, ON);
    assert_eq!(entry(&found, "Compare").insert.text, "Compare::");
    let through = entry(&found, "ops::Compare");
    assert_eq!(
        (through.insert.text.as_str(), through.additional.len()),
        ("ops::Compare::", 0)
    );
}

#[test]
fn an_interface_function_as_a_value_is_written_bare() {
    // X §4.1 with D-500: where a function value is expected, as in
    // `list::map(xs, Show::show)`, an interface's function is written
    // bare after `I::` and `m::I::`, as a module's function is, not as a
    // call that E0048 rejects.
    for (line, path) in [
        ("let n = list::map([a], Show::‸)", "Show::show"),
        ("let n = list::map([a], Show::sh‸)", "Show::show"),
        ("let n = [a] |> list::map(_, Show::‸)", "Show::show"),
        (
            "let n = list::map([s], shape::Shape::‸)",
            "shape::Shape::area",
        ),
    ] {
        let (found, text) = at(&body("import app::shape\nimport std::list\n", line), ON);
        let name = path.rsplit("::").next().expect("a name");
        let item = entry(&found, name);
        assert_eq!(item.insert.text, name, "{line}");
        assert!(!item.insert.snippet, "{line}");
        let done = applied(&text, item);
        assert!(done.contains(&format!("{path})\n")), "{done}");
        assert!(parses(&done));
    }
    // Where no function is expected, it is still a call.
    let (found, _) = at(&body("", "let n = Show::‸"), ON);
    assert_eq!(entry(&found, "show").insert.text, "show($1)$0");
}

#[test]
fn in_a_type_or_a_pattern_a_later_segment_before_colons_offers_nothing() {
    // D-1547: a type's or a constructor's path has one qualifier, so
    // every item there wrote E0013.
    let main = "import app::shape\n\nfn f(s: shape::Square) -> Int {\n  let q: shape::Sq‸::X = s\n  case s {\n    shape::Square(n) => n\n  }\n}\n";
    let (found, _) = at(main, ON);
    assert_eq!(labels(&found), Vec::<&str>::new());
    assert!(!found.incomplete);
    let main = "import app::shape\n\nfn f(s: shape::Square) -> Int {\n  case s {\n    shape::Sq‸::X(n) => n\n  }\n}\n";
    let (found, _) = at(main, ON);
    assert_eq!(labels(&found), Vec::<&str>::new());
    // With no `::` after the segment, the items stay.
    let main =
        "import app::shape\n\nfn f(s: shape::Square) -> Int {\n  let q: shape::Sq‸ = s\n  1\n}\n";
    let (found, _) = at(main, ON);
    assert!(labels(&found).contains(&"Square"), "{:?}", labels(&found));
    // A turbofish's `::` is no path's (D-1525).
    let main = "import app::shape\n\nfn f(s: shape::Square) -> Int {\n  let q: shape::Sq‸::<Int> = s\n  1\n}\n";
    let (found, _) = at(main, ON);
    assert!(labels(&found).contains(&"Square"), "{:?}", labels(&found));
}

#[test]
fn an_interface_at_a_later_segment_writes_its_colons() {
    // D-1548: after `module::` an interface is written `Shape::`, so the
    // list of its functions opens next, after `|>` too; where a path's
    // `::` follows, it stays `Shape` (D-1538).
    for (line, text) in [
        ("let n = shape::Sh‸", "Shape::"),
        ("let n = shape::Sh‸::area(s)", "Shape"),
        ("let n = s |> shape::Sh‸", "Shape::"),
        ("let n = s |> shape::Sh‸::area", "Shape"),
        ("let n = list::map([s], shape::Sh‸)", "Shape::"),
    ] {
        let (found, _) = at(&body("import app::shape\nimport std::list\n", line), ON);
        let shape = entry(&found, "Shape");
        assert_eq!(shape.kind, EntryKind::Interface, "{line}");
        assert_eq!(shape.insert.text, text, "{line}");
    }
    // In a type, an interface is a type, written bare.
    let main = "import app::shape\n\nfn g(x: dyn shape::Sh‸) -> Int {\n  1\n}\n";
    let (found, _) = at(main, ON);
    assert_eq!(entry(&found, "Shape").insert.text, "Shape");
}

/// The text of `text` before byte `end`, a character boundary.
fn upto(text: &str, end: usize) -> &str {
    text.get(..end).expect("a character boundary")
}

/// Whether some item of `found` writes the segment that `rest`, the
/// target from the word's start, begins with: its text up to a `(` is a
/// prefix of `rest` at least as long as the segment.
fn writes_segment(found: &Completions, rest: &str) -> bool {
    let segment = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map_or(rest, |end| upto(rest, end));
    found.items.iter().any(|item| {
        let text = if item.insert.snippet {
            expand(&item.insert.text)
        } else {
            item.insert.text.clone()
        };
        let head = text.find('(').map_or(text.as_str(), |at| upto(&text, at));
        head.len() >= segment.len() && rest.starts_with(head)
    })
}

#[test]
fn the_forms_of_d173_complete_one_character_at_a_time() {
    // D-173's forms, typed from their first character: at each character
    // of a name, an item writes that name as the form has it, with the
    // `::` that follows a module or an interface (D-1546, D-1548), and
    // the last item writes the call (D-1538, D-1545).
    let forms = [
        ("", "assert Show::show(a) == \"1\"", "Show::show(a)"),
        (
            "import std::ops\n",
            "let o = ops::Compare::compare(a, b)",
            "ops::Compare::compare(a, b)",
        ),
        (
            "import std::ops::{Compare}\n",
            "let o = Compare::compare(a, b)",
            "Compare::compare(a, b)",
        ),
        (
            "import app::shape\n",
            "let n = shape::Shape::area(s)",
            "shape::Shape::area(s)",
        ),
        (
            "import app::shape\n",
            "let n = s |> shape::Shape::area",
            "shape::Shape::area",
        ),
        ("", "let t = a |> Show::show", "Show::show"),
        (
            "import std::ops\n",
            "let o = a |> ops::Compare::compare(_, b)",
            "ops::Compare::compare(_, b)",
        ),
    ];
    for (imports, line, form) in forms {
        let start = line.find(form).expect("the form in its line");
        let ident = |c: char| c.is_alphanumeric() || c == '_';
        for end in 1..=form.len() {
            let typed = upto(form, end);
            if !typed.ends_with(ident) || typed.contains('(') {
                continue;
            }
            let word = typed.rfind(|c: char| !ident(c)).map_or(0, |at| at + 1);
            let partial = format!("{}{typed}‸", upto(line, start));
            let (found, _) = at(&body(imports, &partial), ON);
            assert!(
                writes_segment(&found, form.get(word..).expect("a boundary")),
                "{typed}: {:?}",
                labels(&found)
            );
        }
        // The last name's item writes the rest of the form: the hole where
        // the form has it, a tab stop for each other argument, which the
        // user fills. Piped bare, it writes the whole line, which parses.
        let last = form.find('(').unwrap_or(form.len());
        let partial = format!("{}{}‸", upto(line, start), upto(form, last));
        let (found, text) = at(&body(imports, &partial), ON);
        let name = upto(form, last).rsplit("::").next().expect("a name");
        let done = applied(&text, entry(&found, name));
        let call = form
            .get(last + 1..form.len().saturating_sub(1))
            .map_or(String::new(), |args| {
                let holes: Vec<&str> = args
                    .split(", ")
                    .map(|arg| if arg == "_" { "_" } else { "" })
                    .collect();
                format!("({})", holes.join(", "))
            });
        let written = format!("{}{}{call}\n", upto(line, start), upto(form, last));
        assert!(done.contains(&written), "{written:?} in {done}");
        if last == form.len() {
            assert!(parses(&done), "{done}");
        }
    }
}
