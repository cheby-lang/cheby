//! Tests of the type hints: each row of X §7.1 and its skip rules, on
//! clean text and on a body with a type error. Each test runs only the
//! front end, so it runs on Windows too (D-1516).

use std::collections::BTreeMap;
use std::path::Path;

use cheby_driver::analysis::{
    Code, Hash128, ModulePath, Symbol, SymbolNs, TextRange, normalized_key,
};

use super::{HintKind, Switches, hints};
use crate::features::CancelToken;
use crate::features::ide::hover::tests::{Temp, analyze};
use crate::server::ConstValues;

/// `text` as `src/main.cheby` of a package, with each hint written into it
/// as `«label»`, and `{edit}` after it when it has one. Only the hints in
/// `within` when it is given.
fn render(text: &str, switches: Switches, within: Option<TextRange>) -> String {
    render_with(text, switches, within, |_| None)
}

/// [`render`] with the constant values that `values` makes from the
/// absolute path of `src/main.cheby`, which a value's modules name
/// (D-1510).
fn render_with(
    text: &str,
    switches: Switches,
    within: Option<TextRange>,
    values: impl FnOnce(&Path) -> Option<ConstValues>,
) -> String {
    let temp = Temp::package("hints", &[("main.cheby", text)]);
    let path = temp.src("main.cheby");
    let analyzed = analyze(&temp.0, &[(path.clone(), text)]);
    let snapshot = &*analyzed.snapshot;
    let view = snapshot
        .module_at(snapshot.file_of(&path).expect("the file"))
        .expect("the module");
    let values = values(&path);
    let found = hints(
        snapshot,
        view,
        within,
        switches,
        values.as_ref(),
        &CancelToken::new(),
    )
    .expect("hints");
    if let Some(hint) = found
        .iter()
        .find(|hint| (hint.kind == HintKind::Value) != hint.label.starts_with(" = "))
    {
        panic!("a constant's value has kind `Value`, and nothing else: {hint:?}");
    }
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
    let text = "interface Shape {
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
        "interface Shape {
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
    let text = "import std::list

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
        constants: false,
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
    let text = "type Point { x: Int }

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
    let text = "import std::list

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
    constants: false,
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
    let text = "import std::list

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

/// What a test of the constant row makes the values from: the path of
/// `src/main.cheby`.
type MakeValues<'a> = &'a dyn Fn(&Path) -> ConstValues;

/// Only the constant row: every type row off.
const CONSTANTS_ONLY: Switches = Switches {
    let_types: false,
    pattern_binders: false,
    use_binders: false,
    closure_types: false,
    local_fn_types: false,
    pipe_stages: false,
    dyn_: false,
    parameter_names: false,
    constants: true,
};

/// Constants of every kind of initializer: literals, which get no value,
/// and expressions, which do.
const CONSTANTS: &str = r#"type Shape {
  Dot,
  Square(Int),
}

const limit: Int = 20

const negative: Int = -3

const ratio: Float = 1.5

const name: String = "cheby"

const raw: String = r"raw"

const flag: Bool = True

const shape: Shape = Dot

const twice: Int = limit * 2

const greeting: String = "hi {name}"

const square: Shape = Square(limit)

const pair: (Int, Int) = (limit,
  twice)

fn main() {
  assert twice == 40
}
"#;

/// The package's symbol of the root module's value `name`.
fn app_value(name: &str) -> Symbol {
    Symbol::item(ModulePath::root("app"), SymbolNs::Value, name)
}

/// Values for each of `names` with text `text` from a run that read
/// `main` with the hash `read` (D-1510).
fn values_of(main: &Path, read: Hash128, values: &[(&str, &str)]) -> ConstValues {
    let mut out = ConstValues::default();
    for &(name, text) in values {
        out.insert_value(
            app_value(name),
            text,
            false,
            BTreeMap::from([(main.to_path_buf(), read)]),
        );
    }
    out
}

/// A value for every constant of [`CONSTANTS`], read from its text.
fn every_value(main: &Path, read: Hash128) -> ConstValues {
    values_of(
        main,
        read,
        &[
            ("limit", "20"),
            ("negative", "-3"),
            ("ratio", "1.5"),
            ("name", "\"cheby\""),
            ("raw", "\"raw\""),
            ("flag", "True"),
            ("shape", "Dot"),
            ("twice", "40"),
            ("greeting", "\"hi cheby\""),
            ("square", "Square(20)"),
            ("pair", "(20, 40)"),
        ],
    )
}

/// [`CONSTANTS`] with the values of its constants that are not literals.
fn constants_shown() -> String {
    CONSTANTS
        .replace("= limit * 2\n", "= limit * 2« = 40»\n")
        .replace("= \"hi {name}\"\n", "= \"hi {name}\"« = \"hi cheby\"»\n")
        .replace("= Square(limit)\n", "= Square(limit)« = Square(20)»\n")
        .replace("  twice)\n", "  twice)« = (20, 40)»\n")
}

/// D-1679: with the row on, each constant whose value reads current shows
/// ` = value` after its initializer, at the end of its last line; one
/// whose initializer is a number, with its `-`, a string with no
/// interpolation, raw or not, a `Bool` or a unit constructor shows none.
#[test]
fn current_values_show_after_initializers_but_literals() {
    let current = normalized_key(CONSTANTS);
    let got = render_with(CONSTANTS, CONSTANTS_ONLY, None, |main| {
        Some(every_value(main, current))
    });
    assert_eq!(got, constants_shown());
}

/// D-1679, D-1376: the row is off by default, and a package that never
/// evaluates (no values) shows nothing with it on.
#[test]
fn the_constant_row_is_off_by_default() {
    let current = normalized_key(CONSTANTS);
    let types_off = Switches {
        constants: false,
        ..CONSTANTS_ONLY
    };
    let got = render_with(CONSTANTS, types_off, None, |main| {
        Some(every_value(main, current))
    });
    assert_eq!(got, CONSTANTS);
    const { assert!(!Switches::DEFAULTS.constants) };
    assert_eq!(
        render_with(CONSTANTS, CONSTANTS_ONLY, None, |_| None),
        CONSTANTS
    );
}

/// D-1679, D-1510, D-1503, D-1511: a value shows only while it reads
/// current. Stale because a module it reaches has other text than its
/// run read, because the last run failed, or while errors on disk start
/// no run, failed itself, or no value at all, whether a run is due, the
/// last one failed, or none ran: nothing shows. A value that reads
/// current while a run is due still shows.
#[test]
fn only_a_current_value_shows() {
    let current = normalized_key(CONSTANTS);
    let twice_only = CONSTANTS.replace("= limit * 2\n", "= limit * 2« = 40»\n");
    let shows = |make: MakeValues<'_>| {
        render_with(CONSTANTS, CONSTANTS_ONLY, None, |main| Some(make(main)))
    };
    assert_eq!(
        shows(&|main| values_of(main, current, &[("twice", "40")])),
        twice_only
    );
    assert_eq!(
        shows(&|main| ConstValues {
            evaluating: true,
            ..values_of(main, current, &[("twice", "40")])
        }),
        twice_only,
        "current while a run is due"
    );
    let nothing: [(&str, MakeValues<'_>); 7] = [
        ("an edited module", &|main| {
            values_of(main, Hash128([9; 16]), &[("twice", "40")])
        }),
        ("after a failed run", &|main| ConstValues {
            stale: true,
            ..values_of(main, current, &[("twice", "40")])
        }),
        ("errors on disk", &|main| ConstValues {
            disk_errors: true,
            ..values_of(main, current, &[("twice", "40")])
        }),
        ("failed", &|main| ConstValues {
            failed: BTreeMap::from([(app_value("twice"), Code::parse("E0075").unwrap())]),
            ..values_of(main, current, &[("twice", "40")])
        }),
        ("evaluating", &|_| ConstValues {
            evaluating: true,
            ..ConstValues::default()
        }),
        ("skipped for errors", &|_| ConstValues {
            front_errors: true,
            stale: true,
            ..ConstValues::default()
        }),
        ("not yet", &|_| ConstValues::default()),
    ];
    for (state, make) in nothing {
        assert_eq!(shows(make), CONSTANTS, "{state}");
    }
}

/// D-1679: the value is one line, cut at 40 characters with `…`: a longer
/// text, a text of more than one line, and a text the child cut at 4,096
/// characters all end in `…`; one of exactly 40 characters does not.
#[test]
fn a_value_is_one_line_cut_at_40() {
    let text = "import std::list\n\nconst xs: List<Int> = list::reverse([1, 2])\n";
    let current = normalized_key(text);
    let forty = "[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 123]";
    assert_eq!(forty.chars().count(), 40);
    let long = format!("{forty}, 5]");
    let cases = [
        (long, false, format!("{forty}…")),
        (forty.to_owned(), false, forty.to_owned()),
        ("[1,\n2]".to_owned(), false, "[1,…".to_owned()),
        ("[2, 1".to_owned(), true, "[2, 1…".to_owned()),
        (
            format!("[\"é{}\"]", "é".repeat(50)),
            false,
            format!("[\"{}…", "é".repeat(38)),
        ),
    ];
    for (value, truncated, shown) in cases {
        let got = render_with(text, CONSTANTS_ONLY, None, |main| {
            let mut values = ConstValues::default();
            values.insert_value(
                app_value("xs"),
                &value,
                truncated,
                BTreeMap::from([(main.to_path_buf(), current)]),
            );
            Some(values)
        });
        let expected = text.replace("[1, 2])\n", &format!("[1, 2])« = {shown}»\n"));
        assert_eq!(got, expected, "{value:?}");
    }
}

/// D-1679: the range limits the constant row as it limits the others.
#[test]
fn the_range_limits_the_constant_row() {
    let current = normalized_key(CONSTANTS);
    let start = CONSTANTS.find("const greeting").unwrap();
    let end = CONSTANTS.find("const square").unwrap();
    let within = TextRange::new(u32::try_from(start).unwrap(), u32::try_from(end).unwrap());
    let got = render_with(CONSTANTS, CONSTANTS_ONLY, Some(within), |main| {
        Some(every_value(main, current))
    });
    assert_eq!(
        got,
        CONSTANTS.replace("= \"hi {name}\"\n", "= \"hi {name}\"« = \"hi cheby\"»\n")
    );
}

/// D-1678, D-146: in a module whose own type is named `Option`, a `let`
/// of the prelude's `Option<Int>` still shows its type, but offers no edit,
/// since `Option` would name the module's own type; a `let` of the
/// module's `Option` offers one. Hiding the constructor `Some` alone
/// leaves the type writable.
#[test]
fn no_edit_writes_a_prelude_name_the_module_hides() {
    let text = "type Option {\n  Mine,\n}\n\nfn main() {\n  let found = Some(1)\n  let mine = Mine\n  assert found != None && mine == Mine\n}\n";
    check(
        text,
        &text
            .replace("let found =", "let found«: Option<Int>» =")
            .replace("let mine =", "let mine«: Option»{: Option} ="),
    );
    let text = "type Maybe {\n  Some(Int),\n  Nothing,\n}\n\nfn main() {\n  let found = option::Some(1)\n  let maybe = Some(2)\n  assert found != None && maybe != Nothing\n}\n";
    let text = &format!("import std::option\n\n{text}");
    check(
        text,
        &text
            .replace("let found =", "let found«: Option<Int>»{: Option<Int>} =")
            .replace("let maybe =", "let maybe«: Maybe»{: Maybe} ="),
    );
    // At any depth: in a type argument, a tuple and a function type.
    let text = "type Option {\n  Mine,\n}\n\nfn main() {\n  let xs = [Some(1)]\n  let pair = (Some(1), 2)\n  let make = fn() { Some(1) }\n  assert xs != [] && pair != (None, 0) && make() != None\n}\n";
    check(
        text,
        &text
            .replace("let xs =", "let xs«: List<Option<Int>>» =")
            .replace("let pair =", "let pair«: (Option<Int>, Int)» =")
            .replace("let make =", "let make«: fn() -> Option<Int>» =")
            .replace("fn() { Some(1) }", "fn()« -> Option<Int>» { Some(1) }"),
    );
}

/// D-1719: a type parameter of the enclosing item hides a prelude type
/// as a module's own type does, so a `let` of the prelude's `Option<Int>`
/// inside `fn keep<Option>` offers no edit, and one outside it does.
#[test]
fn no_edit_writes_a_prelude_name_a_type_parameter_hides() {
    let text = "fn keep<Option>(x: Option) -> Option {\n  let found = Some(1)\n  assert found != None\n  x\n}\n\nfn main() {\n  let other = Some(2)\n  assert keep(1) == 1 && other != None\n}\n";
    check(
        text,
        &text
            .replace("let found =", "let found«: Option<Int>» =")
            .replace("let other =", "let other«: Option<Int>»{: Option<Int>} ="),
    );
}
