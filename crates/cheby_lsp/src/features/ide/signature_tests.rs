//! Tests of signature help: the call at a cursor in the tree alone, then
//! each row of X §4.7's table on clean text, a type error and a grammar
//! error. Tests that analyze run only the front end, so they run on
//! Windows too (D-1516).

use std::path::Path;

use cheby_driver::analysis::TextSize;

use super::{SignatureText, call_at, file_at, param_ranges, signature_help};
use crate::features::Analyzed;
use crate::features::handlers::symbols::tests::doc;
use crate::features::ide::hover::StdDocs;
use crate::features::ide::hover::tests::{Temp, analyze};

/// The text at the last `|`, with the marker removed, and its offset: a
/// `|>` may come before it.
fn marked(text: &str) -> (String, TextSize) {
    let at = text.rfind('|').expect("a marker");
    let mut plain = text.to_owned();
    plain.remove(at);
    (plain, TextSize::try_from(at).unwrap())
}

/// The callee's text and the argument index at the `|` of `text`.
fn call(text: &str) -> Option<(String, u32, bool)> {
    let (text, offset) = marked(text);
    let doc = doc(&text);
    let at = call_at(doc.tree.root(), offset)?;
    let callee = at.callee.text_range();
    let callee = callee.slice(&text).unwrap().to_owned();
    Some((callee, at.index, at.in_use))
}

#[test]
fn call_at_counts_the_lists_own_commas() {
    let cases: &[(&str, Option<(&str, u32)>)] = &[
        ("fn f() {\n  foo(|1, 2)\n}\n", Some(("foo", 0))),
        ("fn f() {\n  foo(1|, 2)\n}\n", Some(("foo", 0))),
        ("fn f() {\n  foo(1,| 2)\n}\n", Some(("foo", 1))),
        ("fn f() {\n  foo(1, 2|)\n}\n", Some(("foo", 1))),
        ("fn f() {\n  foo(1, 2,|)\n}\n", Some(("foo", 2))),
        ("fn f() {\n  foo|(1, 2)\n}\n", None),
        ("fn f() {\n  foo(1, 2)|\n}\n", None),
        // Nested: the inner list while inside it, the outer one after it.
        ("fn f() {\n  foo(bar(1, |2), 3)\n}\n", Some(("bar", 1))),
        ("fn f() {\n  foo(bar(1, 2)|, 3)\n}\n", Some(("foo", 0))),
        ("fn f() {\n  foo(bar(1, 2), |3)\n}\n", Some(("foo", 1))),
        ("fn f() {\n  foo((1, 2), |3)\n}\n", Some(("foo", 1))),
        // A pipe stage: the hole is a whole argument (D-244).
        (
            "fn f() {\n  xs |> list::map(_, |)\n}\n",
            Some(("list::map", 1)),
        ),
        ("fn f() {\n  x |> g(|)\n}\n", Some(("g", 0))),
        // A capture.
        ("fn f() {\n  let h = g(a, _, |)\n}\n", Some(("g", 2))),
        // Any expression as the callee.
        ("fn f() {\n  make(1)(|)\n}\n", Some(("make(1)", 0))),
        // A record literal and a parameter list have none.
        ("fn f() {\n  P { x: |1 }\n}\n", None),
        ("fn f(a: Int, |b: Int) {}\n", None),
    ];
    for (text, want) in cases {
        let got = call(text).map(|(callee, index, _)| (callee, index));
        let want = want.map(|(callee, index)| (callee.to_owned(), index));
        assert_eq!(got, want, "{text:?}");
    }
}

#[test]
fn call_at_answers_an_open_list() {
    let cases: &[(&str, Option<(&str, u32)>)] = &[
        // Unterminated at the end of the file.
        ("fn f() {\n  foo(1, |", Some(("foo", 1))),
        ("fn f() {\n  foo(|", Some(("foo", 0))),
        // Before a `let` on the next line: the known recovery gap.
        ("fn f() {\n  foo(1, |\n  let y = 2\n}\n", Some(("foo", 1))),
        (
            "fn f() {\n  foo(1,\n  |\n  let y = 2\n}\n",
            Some(("foo", 1)),
        ),
        // Past the next statement, the list is behind.
        ("fn f() {\n  foo(1, \n  let y = 2|\n}\n", None),
        // The inner of two open lists.
        ("fn f() {\n  foo(1, bar(|", Some(("bar", 0))),
        (
            "fn f() {\n  let a = foo(bar(1, 2), |\n}\n",
            Some(("foo", 1)),
        ),
        // A comment between the comma and the cursor.
        ("fn f() {\n  foo(1, // c\n  |\n}\n", Some(("foo", 1))),
    ];
    for (text, want) in cases {
        let got = call(text).map(|(callee, index, _)| (callee, index));
        let want = want.map(|(callee, index)| (callee.to_owned(), index));
        assert_eq!(got, want, "{text:?}");
    }
}

#[test]
fn call_at_sees_a_use() {
    let got = call("fn f() {\n  use x <- g(a, |)\n  x\n}\n");
    assert_eq!(got, Some(("g".to_owned(), 1, true)));
    let got = call("fn f() {\n  let x = g(a, |)\n  x\n}\n");
    assert_eq!(got, Some(("g".to_owned(), 1, false)));
}

#[test]
fn param_ranges_split_at_top_level_commas() {
    let texts = |label: &str| -> Option<Vec<String>> {
        let ranges = param_ranges(label)?;
        Some(
            ranges
                .iter()
                .map(|r| r.slice(label).unwrap().to_owned())
                .collect(),
        )
    };
    let cases: &[(&str, Option<&[&str]>)] = &[
        (
            "fn map<T, U>(items: List<T>, f: fn(T) -> U) -> List<U>",
            Some(&["items: List<T>", "f: fn(T) -> U"]),
        ),
        (
            "fn fold<K: Compare + Show, V>(m: Map<K, V>, f: fn((K, V)) -> Nil)",
            Some(&["m: Map<K, V>", "f: fn((K, V)) -> Nil"]),
        ),
        ("fn scale(Self, Float) -> Self", Some(&["Self", "Float"])),
        ("Circle(Float)", Some(&["Float"])),
        ("fn main()", Some(&[])),
        ("fn broken(a: Int", None),
    ];
    for (label, want) in cases {
        let want = want.map(|w| w.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert_eq!(texts(label), want, "{label:?}");
    }
}

/// The signature help at the `|` of `needle` in `path`, whose text is
/// `text` with the `|` removed.
fn at(analyzed: &Analyzed, path: &Path, text: &str, needle: &str) -> Option<SignatureText> {
    let bar = needle.find('|').expect("a cursor");
    let plain = needle.replacen('|', "", 1);
    let start = text.find(&plain).expect("the needle in the text");
    assert_eq!(text.matches(&plain).count(), 1, "`{plain}` occurs once");
    let offset = TextSize::try_from(start.checked_add(bar).unwrap()).unwrap();
    let file = file_at(&analyzed.snapshot, path).expect("the file");
    signature_help(analyzed, file, offset, StdDocs::None)
}

/// The label, the parameters' texts and the active parameter.
fn shape(found: &SignatureText) -> (String, Vec<String>, Option<u32>) {
    let params = (0..found.params.len())
        .map(|n| found.param_text(n).unwrap().to_owned())
        .collect();
    (found.label.clone(), params, found.active())
}

fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| (*s).to_owned()).collect()
}

const CLEAN: &str = r"import std::list

/// Something with an area.
interface Shape {
  /// Grows by `factor`.
  fn scale(Self, Float) -> Self
}

type Figure {
  /// A circle by its radius.
  Circle(Float),
  Square { side: Float },
}

fn scale(f: Figure, factor: Float) -> Figure {
  case f {
    Circle(r) => Circle(r * factor)
    Square { side } => Square { side: side * factor }
  }
}

/// Adds two numbers.
///
/// ```
/// assert add(1, 2) == 3
/// ```
fn add(a: Int, b: Int) -> Int {
  a + b
}

/// The first of `xs`, or `fallback`.
fn first_or<T>(xs: List<T>, fallback: T) -> T {
  case xs {
    [head, ..] => head
    [] => fallback
  }
}

fn twice(n: Int, f: fn(Int) -> Int) -> Int {
  f(f(n))
}

fn grow<S: Shape>(s: S) -> S {
  Shape::scale(s, 2.0)
}

fn main() {
  let total = add(1, add(2, 3))
  let shapes = [Circle(1.0)]
  let first = first_or(shapes, Circle(0.5))
  let doubled = [1, 2] |> list::map(_, fn(n) { n * 2 })
  let inc = fn(n: Int) -> Int { n + 1 }
  fn dec(n: Int) -> Int {
    n - 1
  }
  let more = twice(inc(1), dec)
  assert total + more + list::length(doubled) > 0
  assert scale(first, 2.0) != first
}
";

#[test]
fn clean_text_shows_each_kind_of_callee() {
    let temp = Temp::package("sig_clean", &[("main.cheby", CLEAN)]);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path.clone(), CLEAN)]);
    let get = |needle: &str| at(&analyzed, &path, CLEAN, needle).expect(needle);

    // A function, with its docs, fences marked as Cheby.
    let plain = get("add(1, |add(2, 3))");
    assert_eq!(
        shape(&plain),
        (
            "fn add(a: Int, b: Int) -> Int".to_owned(),
            strings(&["a: Int", "b: Int"]),
            Some(1),
        )
    );
    assert_eq!(plain.code, None);
    assert_eq!(
        plain.docs.as_deref(),
        Some("Adds two numbers.\n\n```cheby\nassert add(1, 2) == 3\n```")
    );
    // Nested: the inner call.
    assert_eq!(shape(&get("add(2|, 3)")).2, Some(0));

    // A positional constructor, with the variant's docs.
    let ctor = get("[Circle(|1.0)]");
    assert_eq!(
        shape(&ctor),
        ("Circle(Float)".to_owned(), strings(&["Float"]), Some(0))
    );
    assert_eq!(ctor.docs.as_deref(), Some("A circle by its radius."));

    // A generic function: declared, then instantiated at this call.
    let generic = get("first_or(shapes, |Circle(0.5))");
    assert_eq!(
        shape(&generic),
        (
            "fn first_or<T>(xs: List<T>, fallback: T) -> T".to_owned(),
            strings(&["xs: List<T>", "fallback: T"]),
            Some(1),
        )
    );
    assert_eq!(
        generic.code.as_deref(),
        Some("fn(List<Figure>, Figure) -> Figure")
    );

    // A pipe stage: the hole counts as the first argument.
    let piped = get("list::map(_, |fn(n)");
    assert_eq!(
        piped.label,
        "fn map<T, U>(xs: List<T>, f: fn(T) -> U) -> List<U>"
    );
    assert_eq!(piped.active(), Some(1));
    assert_eq!(
        piped.code.as_deref(),
        Some("fn(List<Int>, fn(Int) -> Int) -> List<Int>")
    );

    // An interface function: types as labels.
    let iface = get("Shape::scale(s, |2.0)");
    assert_eq!(
        shape(&iface),
        (
            "fn scale(Self, Float) -> Self".to_owned(),
            strings(&["Self", "Float"]),
            Some(1),
        )
    );
    assert_eq!(iface.code.as_deref(), Some("fn(S, Float) -> S"));
    assert_eq!(iface.docs.as_deref(), Some("Grows by `factor`."));

    // Function values: a parameter, a closure, a local function.
    assert_eq!(
        shape(&get("f(|f(n))")),
        ("fn(Int) -> Int".to_owned(), strings(&["Int"]), Some(0))
    );
    assert_eq!(shape(&get("inc(|1)")).0, "fn(Int) -> Int");
    assert_eq!(
        shape(&get("twice(inc(1), |dec)")).1,
        strings(&["n: Int", "f: fn(Int) -> Int"])
    );

    // A record literal and a place outside every call have none.
    assert_eq!(at(&analyzed, &path, CLEAN, "Square { side: |side"), None);
    assert_eq!(at(&analyzed, &path, CLEAN, "let |total"), None);
}

const USE: &str = r#"import std::result

fn parse(s: String) -> Result<Int, String> {
  case s {
    "1" => Ok(1)
    _ => Err("not one")
  }
}

fn main() {
  let r = {
    use n <- result::try(parse("1"))
    Ok(n + 1)
  }
  assert r == Ok(2)
}
"#;

#[test]
fn use_keeps_the_callback_in_the_label_and_notes_it() {
    let temp = Temp::package("sig_use", &[("main.cheby", USE)]);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path.clone(), USE)]);
    let found = at(&analyzed, &path, USE, "result::try(|parse").expect("an answer");
    assert_eq!(found.params.len(), 2);
    assert_eq!(found.active(), Some(0));
    let last = found.param_text(1).unwrap();
    let name = last.split_once(": ").unwrap().0;
    assert_eq!(
        found.notes,
        vec![format!("The rest of the block is passed as `{name}`.")]
    );
}

const TYPE_ERROR: &str = r#"fn add(a: Int, b: Int) -> Int {
  a + b
}

fn first_or<T>(xs: List<T>, fallback: T) -> T {
  case xs {
    [head, ..] => head
    [] => fallback
  }
}

fn main() {
  let wrong = add("one", 2)
  let extra = add(1, 2, 3)
  let nums: List<Int> = [1, 2]
  let partial = first_or(nums, )
  let blind = first_or(nowhere, )
  assert wrong + extra + partial + blind > 0
}
"#;

#[test]
fn a_type_error_keeps_the_signature() {
    let temp = Temp::package("sig_type_error", &[("main.cheby", TYPE_ERROR)]);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path.clone(), TYPE_ERROR)]);
    let get = |needle: &str| at(&analyzed, &path, TYPE_ERROR, needle).expect(needle);
    assert_eq!(
        shape(&get("add(\"one\", |2)")),
        (
            "fn add(a: Int, b: Int) -> Int".to_owned(),
            strings(&["a: Int", "b: Int"]),
            Some(1),
        )
    );
    // More arguments than parameters: none is active.
    let extra = get("add(1, 2, |3)");
    assert_eq!((extra.index, extra.active()), (2, None));
    // A missing argument (E0048): the arguments still instantiate it
    // (F §1.6).
    let partial = get("first_or(nums, |)");
    assert_eq!(partial.active(), Some(1));
    assert_eq!(partial.code.as_deref(), Some("fn(List<Int>, Int) -> Int"));
    // No type argument known: the instantiated form would say no more
    // than the label, `fn(List<_>, _) -> _` (D-1482).
    let blind = get("first_or(nowhere, |)");
    assert_eq!(blind.active(), Some(1));
    assert_eq!(blind.code, None);
}

/// Two open calls, one followed by a `let`, one at the end of the file.
/// Each `(1, ` keeps its space, which a raw string would not show.
const GRAMMAR_ERROR: &str = concat!(
    "fn add(a: Int, b: Int) -> Int {\n",
    "  a + b\n",
    "}\n",
    "\n",
    "fn twice(n: Int, f: fn(Int) -> Int) -> Int {\n",
    "  f(f(n))\n",
    "}\n",
    "\n",
    "fn main() {\n",
    "  let inc = fn(n: Int) -> Int { n + 1 }\n",
    "  let total = add(1, \n",
    "  let more = twice(2, inc)\n",
    "  assert total + more > 0\n",
    "}\n",
    "\n",
    "fn later() {\n",
    "  let k = add(1, add(2, \n",
);

#[test]
fn a_grammar_error_still_answers() {
    let temp = Temp::package("sig_grammar_error", &[("main.cheby", GRAMMAR_ERROR)]);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path.clone(), GRAMMAR_ERROR)]);
    let get = |needle: &str| at(&analyzed, &path, GRAMMAR_ERROR, needle).expect(needle);
    // `foo(1, ` followed by a `let`.
    assert_eq!(
        shape(&get("add(1, |\n  let more")),
        (
            "fn add(a: Int, b: Int) -> Int".to_owned(),
            strings(&["a: Int", "b: Int"]),
            Some(1),
        )
    );
    // A complete call after the broken one.
    assert_eq!(shape(&get("twice(2, |inc)")).2, Some(1));
    // Unterminated at the end of the file, inside an open call.
    let inner = get("add(2, |\n");
    assert_eq!(
        (inner.label.as_str(), inner.active()),
        ("fn add(a: Int, b: Int) -> Int", Some(1))
    );
}
