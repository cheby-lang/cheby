//! Tests of the type hints: each row of X §7.1 and its skip rules, on
//! clean text and on a body with a type error. Each test runs only the
//! front end, so it runs on Windows too (D-1516).

use cheby_driver::analysis::TextRange;

use super::{Switches, hints};
use crate::features::CancelToken;
use crate::features::ide::hover::tests::{Temp, analyze};

/// `text` as `src/main.cheby` of a package, with each hint written into it
/// as `«label»`, and `{edit}` after it when it has one. Only the hints in
/// `within` when it is given.
fn render(text: &str, switches: Switches, within: Option<TextRange>) -> String {
    let temp = Temp::package("hints", &[("main.cheby", text)]);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path.clone(), text)]);
    let snapshot = &*analyzed.snapshot;
    let view = snapshot
        .module_at(snapshot.file_of(&path).expect("the file"))
        .expect("the module");
    let found = hints(snapshot, view, within, switches, &CancelToken::new()).expect("hints");
    let mut out = String::new();
    let mut at = 0;
    for hint in found {
        let offset = usize::try_from(hint.offset).unwrap();
        out.push_str(text.get(at..offset).unwrap());
        out.push('«');
        out.push_str(&hint.label);
        out.push('»');
        if let Some(edit) = hint.edit {
            out.push('{');
            out.push_str(&edit);
            out.push('}');
        }
        at = offset;
    }
    out.push_str(text.get(at..).unwrap());
    out
}

/// [`render`] with D-1376's defaults and no range.
fn all(text: &str) -> String {
    render(text, Switches::DEFAULTS, None)
}

/// Asserts that `text` renders as `expected` with D-1376's defaults.
#[track_caller]
fn check(text: &str, expected: &str) {
    assert_eq!(all(text), expected);
}

#[test]
fn let_types_and_their_skips() {
    let text = r#"import std::base64

type Circle { radius: Float }

type Shape {
  Dot,
  Square(Float),
}

type Pair<T> { Pair(T, T) }

fn takes(b: U8) -> U8 {
  b
}

fn main() {
  let xs = [1, 2]
  let n: Int = 2
  let _ignored = xs
  let i = 1
  let f = 1.5
  let s = "text {i}"
  let r = r"raw"
  let neg = -3
  let byte = 7
  let c = Circle { radius: f }
  let sq = Square(f)
  let dot = Dot
  let p = Pair(n, neg)
  let decoded = base64::decode(s)
  let hidden = fn(x: Int) -> Int { x }
  assert takes(byte) == 7 && c.radius == f && sq != dot && p == Pair(1, 2)
  assert decoded != Err(Nil) && r != s && hidden(1) == 1 && xs != []
}
"#;
    check(
        text,
        r#"import std::base64

type Circle { radius: Float }

type Shape {
  Dot,
  Square(Float),
}

type Pair<T> { Pair(T, T) }

fn takes(b: U8) -> U8 {
  b
}

fn main() {
  let xs«: List<Int>»{: List<Int>} = [1, 2]
  let n: Int = 2
  let _ignored = xs
  let i = 1
  let f = 1.5
  let s = "text {i}"
  let r = r"raw"
  let neg = -3
  let byte«: U8»{: U8} = 7
  let c = Circle { radius: f }
  let sq«: Shape»{: Shape} = Square(f)
  let dot«: Shape»{: Shape} = Dot
  let p«: Pair<Int>»{: Pair<Int>} = Pair(n, neg)
  let decoded«: Result<bytes::Bytes, Nil>» = base64::decode(s)
  let hidden«: fn(Int) -> Int»{: fn(Int) -> Int} = fn(x: Int) -> Int { x }
  assert takes(byte) == 7 && c.radius == f && sq != dot && p == Pair(1, 2)
  assert decoded != Err(Nil) && r != s && hidden(1) == 1 && xs != []
}
"#,
    );
}

#[test]
fn pattern_binders_show_after_each_name_with_no_edit() {
    let text = r#"type Point { x: Int, y: Float }

fn main() {
  let (a, b) = (1, "two")
  let Point { x, y: why } = Point { x: a, y: 1.5 }
  let [first, ..rest] = [x, 2]
  let (_skip, kept) as both = (b, why)
  let guard Ok(value) = Ok(first) else {
    Nil
  }
  assert rest == [] && kept == 1.5 && both == ("two", 1.5) && value == 1
}
"#;
    check(
        text,
        r#"type Point { x: Int, y: Float }

fn main() {
  let (a«: Int», b«: String») = (1, "two")
  let Point { x«: Int», y: why«: Float» } = Point { x: a, y: 1.5 }
  let [first«: Int», ..rest«: List<Int>»] = [x, 2]
  let (_skip, kept«: Float») as both«: (String, Float)» = (b, why)
  let guard Ok(value«: Int») = Ok(first) else {
    Nil
  }
  assert rest == [] && kept == 1.5 && both == ("two", 1.5) && value == 1
}
"#,
    );
}

#[test]
fn use_binders() {
    let text = r#"fn twice(f: fn(Int, String) -> Int) -> Int {
  f(1, "a") * 2
}

fn main() {
  use n, _label <- twice
  use m: Int, s <- twice
  n + m + string_length(s)
}

fn string_length(s: String) -> Int {
  case s {
    "" => 0
    _ => 1
  }
}
"#;
    check(
        text,
        r#"fn twice(f: fn(Int, String) -> Int) -> Int {
  f(1, "a") * 2
}

fn main() {
  use n«: Int», _label <- twice
  use m: Int, s«: String» <- twice
  n + m + string_length(s)
}

fn string_length(s: String) -> Int {
  case s {
    "" => 0
    _ => 1
  }
}
"#,
    );
}

#[test]
fn closures_and_local_functions() {
    let text = r#"import std::list

fn main() {
  let doubled = list::map([1, 2], fn(x) { x * 2 })
  let typed = list::map(doubled, fn(x: Int) -> String { "{x}" })
  let pairs = list::map(typed, fn(s, ) { (s, 1) })
  fn step(acc, (k, _v)) {
    acc + k
  }
  fn keep(n: Int) -> Int {
    n
  }
  fn ignore(_n) {
    Nil
  }
  assert step("", ("a", 1)) == "a" && keep(1) == 1 && pairs != [] && ignore(1) == Nil
}
"#;
    check(
        text,
        r#"import std::list

fn main() {
  let doubled«: List<Int>»{: List<Int>} = list::map([1, 2], fn(x«: Int»)« -> Int» { x * 2 })
  let typed«: List<String>»{: List<String>} = list::map(doubled, fn(x: Int) -> String { "{x}" })
  let pairs«: List<(String, Int)>»{: List<(String, Int)>} = list::map(typed, fn(s«: String», )« -> (String, Int)» { (s, 1) })
  fn step(acc«: String», (k«: String», _v))« -> String» {
    acc + k
  }
  fn keep(n: Int) -> Int {
    n
  }
  fn ignore(_n)« -> Nil» {
    Nil
  }
  assert step("", ("a", 1)) == "a" && keep(1) == 1 && pairs != [] && ignore(1) == Nil
}
"#,
    );
}

#[test]
fn pipe_stages_on_multi_line_pipelines() {
    let text = r#"import std::list

fn main() {
  let total = [1, 2]
    |> list::map(_, fn(x: Int) -> Int { x * 2 })
    |> list::filter(_, fn(x: Int) -> Bool { x > 1 })
    |> list::length
  let count: Int = [1]
    |> list::map(_, fn(x: Int) -> String { "{x}" })
    |> list::length
  let names = [1] |> list::map(_, fn(x: Int) -> String { "{x}" }) |> list::length
  let _shown = [1, 2]
    |> list::map(_, fn(x: Int) -> Float { 1.5 })
    |> list::length
  let mixed = [3]
    |> list::reverse |> list::length
  assert total + count + names + mixed == 0
}
"#;
    check(
        text,
        r#"import std::list

fn main() {
  let total«: Int»{: Int} = [1, 2]
    |> list::map(_, fn(x: Int) -> Int { x * 2 })«: List<Int>»
    |> list::filter(_, fn(x: Int) -> Bool { x > 1 })«: List<Int>»
    |> list::length
  let count: Int = [1]
    |> list::map(_, fn(x: Int) -> String { "{x}" })«: List<String>»
    |> list::length
  let names«: Int»{: Int} = [1] |> list::map(_, fn(x: Int) -> String { "{x}" }) |> list::length
  let _shown = [1, 2]
    |> list::map(_, fn(x: Int) -> Float { 1.5 })«: List<Float>»
    |> list::length«: Int»
  let mixed«: Int»{: Int} = [3]
    |> list::reverse |> list::length
  assert total + count + names + mixed == 0
}
"#,
    );
}

#[test]
fn implicit_dyn_after_the_value() {
    let text = r"interface Shape {
  fn area(Self) -> Float
}

type Circle { radius: Float }

fn area(c: Circle) -> Float {
  c.radius * c.radius * 3.0
}

fn shapes() -> List<dyn Shape> {
  [Circle { radius: 1.0 }, Circle { radius: 2.0 }]
}

fn main() {
  let one: dyn Shape = Circle { radius: 1.0 }
  assert [one] != shapes()
}
";
    check(
        text,
        r"interface Shape {
  fn area(Self) -> Float
}

type Circle { radius: Float }

fn area(c: Circle) -> Float {
  c.radius * c.radius * 3.0
}

fn shapes() -> List<dyn Shape> {
  [Circle { radius: 1.0 }« as dyn Shape», Circle { radius: 2.0 }« as dyn Shape»]
}

fn main() {
  let one: dyn Shape = Circle { radius: 1.0 }« as dyn Shape»
  assert [one] != shapes()
}
",
    );
}

#[test]
fn a_type_error_keeps_types_and_skips_unknown_parts() {
    let text = r#"import std::list

fn main() {
  let n = 1
  let wrong: String = n
  let unknown = n + "x"
  let partly = list::map([n], fn(x) { x + "s" })
  let ys = list::map([n], fn(x) { x + 1 })
  assert wrong == "" && unknown == partly && ys == []
}
"#;
    check(
        text,
        r#"import std::list

fn main() {
  let n = 1
  let wrong: String = n
  let unknown = n + "x"
  let partly = list::map([n], fn(x«: Int») { x + "s" })
  let ys«: List<Int>»{: List<Int>} = list::map([n], fn(x«: Int»)« -> Int» { x + 1 })
  assert wrong == "" && unknown == partly && ys == []
}
"#,
    );
}

#[test]
fn switches_turn_rows_off() {
    let text = r"import std::list

fn twice(f: fn(Int) -> Int) -> Int {
  f(1)
}

fn main() {
  let xs = list::map([1], fn(x) { x })
    |> list::length
  use n <- twice
  xs + n
}
";
    let off = Switches {
        let_types: false,
        pattern_binders: false,
        use_binders: false,
        closure_types: false,
        local_fn_types: false,
        pipe_stages: false,
        dyn_: false,
        parameter_names: false,
    };
    assert_eq!(render(text, off, None), text);
    let pipes_only = Switches {
        pipe_stages: true,
        ..off
    };
    // With the `let` hint off, nothing shows the last stage's type.
    assert_eq!(
        render(text, pipes_only, None),
        text.replace("|> list::length\n", "|> list::length«: Int»\n")
    );
}

#[test]
fn a_range_limits_the_hints_but_not_the_rows() {
    let text = "fn main() {\n  let a = [1]\n  let b = [a]\n  let c = [b]\n  assert c != []\n}\n";
    let start = u32::try_from(text.find("let b").unwrap()).unwrap();
    let end = u32::try_from(text.find("let c").unwrap()).unwrap();
    let within = Some(TextRange::new(start, end));
    assert_eq!(
        render(text, Switches::DEFAULTS, within),
        text.replace("let b", "let b«: List<List<Int>>»{: List<List<Int>>}")
    );
    // A range's ends count.
    let at = u32::try_from(text.find(" = [a]").unwrap()).unwrap();
    assert_eq!(
        render(text, Switches::DEFAULTS, Some(TextRange::empty(at))),
        text.replace("let b", "let b«: List<List<Int>>»{: List<List<Int>>}")
    );
}

/// D-1483: only a record literal of a non-generic type skips its `let`
/// hint. A constructor call, a unit constructor and a variant with named
/// fields do not write their type's name, so they keep theirs, and a
/// generic record keeps its hint.
#[test]
fn only_a_non_generic_record_literal_skips_its_let_hint() {
    let text = r"type Point { x: Int }

type Shape {
  Circle(Float),
  Rect { w: Int },
  Dot,
}

type Box<T> { value: T }

fn main() {
  let p = Point { x: 1 }
  let c = Circle(1.0)
  let d = Dot
  let r = Rect { w: 1 }
  let b = Box { value: 1 }
  assert p.x == 1 && c != d && r != d && b.value == 1
}
";
    assert_eq!(
        all(text),
        text.replace("let c =", "let c«: Shape»{: Shape} =")
            .replace("let d =", "let d«: Shape»{: Shape} =")
            .replace("let r =", "let r«: Shape»{: Shape} =")
            .replace("let b =", "let b«: Box<Int>»{: Box<Int>} =")
    );
}

/// D-1480: each row has its own switch. `localFnTypes` covers a local
/// function's parameters and result and `closureTypes` only closures;
/// `patternBinders` covers a destructuring `let` and `letTypes` only a
/// `let` whose pattern is one name.
#[test]
fn local_functions_and_pattern_binders_have_their_own_switches() {
    let text = r"import std::list

fn main() {
  let ys = list::map([1], fn(x) { x })
  let (a, b) = (1, 2.5)
  fn step(n) {
    n + a
  }
  assert step(1) == 2 && ys != [] && b == 2.5
}
";
    let let_hint = ("let ys =", "let ys«: List<Int>»{: List<Int>} =");
    let closure = ("fn(x) {", "fn(x«: Int»)« -> Int» {");
    let binders = ("let (a, b)", "let (a«: Int», b«: Float»)");
    let local_fn = ("fn step(n) {", "fn step(n«: Int»)« -> Int» {");
    let with = |rows: &[(&str, &str)]| {
        rows.iter()
            .fold(text.to_owned(), |out, (from, to)| out.replace(from, to))
    };
    assert_eq!(all(text), with(&[let_hint, closure, binders, local_fn]));
    let no_local_fn = Switches {
        local_fn_types: false,
        ..Switches::DEFAULTS
    };
    assert_eq!(
        render(text, no_local_fn, None),
        with(&[let_hint, closure, binders])
    );
    let no_closures = Switches {
        closure_types: false,
        ..Switches::DEFAULTS
    };
    assert_eq!(
        render(text, no_closures, None),
        with(&[let_hint, binders, local_fn])
    );
    let no_binders = Switches {
        pattern_binders: false,
        ..Switches::DEFAULTS
    };
    assert_eq!(
        render(text, no_binders, None),
        with(&[let_hint, closure, local_fn])
    );
    let no_lets = Switches {
        let_types: false,
        ..Switches::DEFAULTS
    };
    assert_eq!(
        render(text, no_lets, None),
        with(&[closure, binders, local_fn])
    );
}

/// [`render`] with D-1376's defaults and `inlayHints.parameterNames` on.
fn with_names(text: &str) -> String {
    let switches = Switches {
        parameter_names: true,
        ..Switches::DEFAULTS
    };
    render(text, switches, None)
}

/// The type rows off, so that only the parameter names show.
const NAMES_ONLY: Switches = Switches {
    let_types: false,
    pattern_binders: false,
    use_binders: false,
    closure_types: false,
    local_fn_types: false,
    pipe_stages: false,
    dyn_: false,
    parameter_names: true,
};

/// The text of [`names_and_their_skips`].
const CALLS: &str = r#"import std::list

interface Shape {
  fn scale(Self, Float) -> Self
}

type Figure {
  Circle(Float),
  Dot,
}

fn scale(f: Figure, factor: Float) -> Figure {
  case f {
    Circle(r) => Circle(r * factor)
    Dot => Dot
  }
}

fn label(text: String, width: Int, bold: Bool, shape: Figure) -> String {
  case bold, shape {
    True, Dot => "{text} {width}"
    _, _ => text
  }
}

fn sum((a, b): (Int, Int), c: Int) -> Int {
  a + b + c
}

fn add(a: Int, b: Int) -> Int {
  a + b
}

fn twice(start: Int, next: fn(Int) -> Int) -> Int {
  next(start * 2)
}

fn main() {
  let width = 3
  let show = label
  let a = label("hi", -2, True, Dot)
  let b = label(r"raw", width, False, Circle(1.0))
  let c = label("w {width}", width + 1, !True, Dot)
  let d = show("x", 1, True, Dot)
  let e = Shape::scale(Dot, 2.0)
  let g = scale(Circle(1.0), 2.0)
  let h = sum((1, 2), 3)
  let plus = add(1, _)
  fn local(n: Int) -> Int {
    n
  }
  let xs = [1, 2]
    |> list::take(_, 1)
  use n <- twice(4)
  assert a != b && c != d && e != g && h + plus(local(5)) + n == 0 && xs != []
}
"#;

/// D-1515: `name: ` before a number, a negative number, a string, raw or
/// interpolated, and a unit constructor such as `True` and `Dot`, of a call
/// to a function with named parameters, including a capture, a pipe stage
/// with a hole and the call of a `use`. Skipped: an argument that is not a
/// literal, a call through a function value, a local function and an
/// interface function, a constructor, a parameter with no name, and the
/// `_` hole.
#[test]
fn names_and_their_skips() {
    let expected = CALLS
        .replace(
            r#"label("hi", -2, True, Dot)"#,
            r#"label(«text: »"hi", «width: »-2, «bold: »True, «shape: »Dot)"#,
        )
        .replace(
            r#"label(r"raw", width, False, Circle(1.0))"#,
            r#"label(«text: »r"raw", width, «bold: »False, Circle(1.0))"#,
        )
        .replace(
            r#"label("w {width}", width + 1, !True, Dot)"#,
            r#"label(«text: »"w {width}", width + 1, !True, «shape: »Dot)"#,
        )
        .replace(
            "scale(Circle(1.0), 2.0)",
            "scale(Circle(1.0), «factor: »2.0)",
        )
        .replace("sum((1, 2), 3)", "sum((1, 2), «c: »3)")
        .replace("add(1, _)", "add(«a: »1, _)")
        .replace("list::take(_, 1)", "list::take(_, «count: »1)")
        .replace("twice(4)", "twice(«start: »4)");
    assert_eq!(render(CALLS, NAMES_ONLY, None), expected);
}

/// D-1515, D-1376: the row is off by default, and its switch adds the
/// names to the type rows without changing them.
#[test]
fn names_are_off_by_default() {
    let text = "fn add(a: Int, b: Int) -> Int {\n  a + b\n}\n\nfn main() {\n  let n = add(1, 2)\n  assert n == 3\n}\n";
    assert_eq!(all(text), text.replace("let n =", "let n«: Int»{: Int} ="));
    assert_eq!(
        with_names(text),
        text.replace("let n =", "let n«: Int»{: Int} =")
            .replace("add(1, 2)", "add(«a: »1, «b: »2)")
    );
}

/// D-1515, D-244: a pipe stage with no hole is an error, and its
/// arguments get no names, since none of them is known to fill the
/// parameter at its position; a body with that error keeps the names of
/// its other calls.
#[test]
fn a_pipe_stage_with_no_hole_gets_no_names() {
    let text = r"import std::list

fn main() {
  let xs = [1, 2]
    |> list::take(1)
  let ys = list::take([3], 1)
  assert xs == ys
}
";
    assert_eq!(
        render(text, NAMES_ONLY, None),
        text.replace("list::take([3], 1)", "list::take([3], «count: »1)")
    );
}

/// D-1515: where a comma is missing, two arguments share a position, so
/// the names stop before the second: neither it nor a later argument is
/// known to fill the parameter its commas count to.
#[test]
fn a_missing_comma_ends_the_names() {
    let text = "fn pair(left: String, right: String, count: Int) -> Int {\n  count\n}\n\nfn \
                main() {\n  assert pair(\"x\" \"y\", 3) == 3\n}\n";
    assert_eq!(
        render(text, NAMES_ONLY, None),
        text.replace(r#"pair("x" "y", 3)"#, r#"pair(«left: »"x" "y", 3)"#)
    );
}

/// D-1515: an argument whose text is the parameter's name gets no hint.
/// Only a literal gets one, and no literal is written as a parameter's
/// name, so a name-like argument is skipped as not a literal too: this
/// pins that a variable named after its parameter shows nothing.
#[test]
fn an_argument_named_as_its_parameter_gets_none() {
    let text = "fn add(a: Int, b: Int) -> Int {\n  a + b\n}\n\nfn main() {\n  let a = 1\n  assert add(a, 2) == 3\n}\n";
    assert_eq!(
        render(text, NAMES_ONLY, None),
        text.replace("add(a, 2)", "add(a, «b: »2)")
    );
}
