//! Tests of `record.rs`: construction, record update and field access
//! (§5.11), with opacity (§4.3.4), D-071 and D-213.

use crate::test_support::{Build, build, build_with_std};
use crate::thir::ExprKind;

fn check(items: &str) -> Build {
    build(&[("", &format!("//! Test.\n\n{items}"))])
}

fn messages(items: &str) -> Vec<String> {
    check(items).messages()
}

const POINT: &str = "type Point {\n  x: Int,\n  y: Int,\n}\n\n";

const SHAPES: &str = "type Shape {\n  Circle { name: String, radius: Int },\n  Rect { name: String, width: Int, height: Int },\n}\n\n";

/// The first expression of function `name` that `pick` accepts.
fn find<T>(b: &Build, name: &str, pick: impl Fn(&ExprKind) -> Option<T>) -> T {
    let body = b
        .thir_fn("app", name)
        .and_then(|f| f.body.as_ref())
        .unwrap_or_else(|| panic!("`{name}` does not check:\n{}", b.rendered()));
    body.exprs
        .iter()
        .find_map(|e| pick(&e.kind))
        .expect("no such expression")
}

#[test]
fn construction_initializes_every_field_once_in_any_order() {
    let src = format!("{POINT}fn make(y: Int) -> Point {{\n  Point {{ y, x: 1 }}\n}}\n");
    let b = check(&src);
    assert_eq!(b.messages(), Vec::<String>::new(), "{}", b.rendered());
    // Written order, with declaration indexes.
    let fields = find(&b, "make", |k| match k {
        ExprKind::Record { fields, .. } => Some(fields.iter().map(|(i, _)| *i).collect::<Vec<_>>()),
        _ => None,
    });
    assert_eq!(fields, [1, 0]);
}

#[test]
fn construction_errors() {
    let src = format!(
        "{POINT}fn a() -> Point {{\n  Point {{ x: 1 }}\n}}\n\n\
         fn b() -> Point {{\n  Point {{ x: 1, y: 2, x: 3 }}\n}}\n\n\
         fn c() -> Point {{\n  Point {{ x: 1, y: 2, z: 3 }}\n}}\n\n\
         fn d() -> Option<Int> {{\n  Some {{ value: 1 }}\n}}\n\n\
         fn e() -> Point {{\n  Point {{ x: \"one\", y: 2 }}\n}}\n"
    );
    let got = messages(&src);
    let expected = [
        "E0047: mismatched types [\"expected `Int`, found `String`\"]",
        "E0058: `Point` has no field `z` [\"unknown field\"]",
        "E0059: `Some` has positional fields, not named ones [\"\"]",
        "E0059: field `x` is initialized twice [\"initialized again here\", \"first initialized here\"]",
        "E0059: missing field `y` in `Point` [\"missing `y`\", \"`Point` is declared here\"]",
    ];
    assert_eq!(got, expected);
}

#[test]
fn generic_records_take_their_type_arguments() {
    let src = "type Boxed<T> {\n  value: T,\n  label: String,\n}\n\n\
               fn wrap(n: Int) -> Boxed<Int> {\n  Boxed { value: n, label: \"n\" }\n}\n\n\
               fn bad(n: Int) -> Boxed<String> {\n  Boxed { value: n, label: \"n\" }\n}\n";
    let got = messages(src);
    assert_eq!(
        got,
        ["E0047: mismatched types [\"expected `String`, found `Int`\"]"]
    );
}

#[test]
fn update_keeps_the_variant_and_needs_common_fields() {
    let src =
        format!("{SHAPES}fn rename(s: Shape) -> Shape {{\n  Shape {{ ..s, name: \"new\" }}\n}}\n");
    let b = check(&src);
    assert_eq!(b.messages(), Vec::<String>::new(), "{}", b.rendered());
    let per_ctor = find(&b, "rename", |k| match k {
        ExprKind::RecordUpdate { fields, .. } => Some(fields[0].index_per_ctor.clone()),
        _ => None,
    });
    assert_eq!(per_ctor.iter().map(|(_, i)| *i).collect::<Vec<_>>(), [0, 0]);

    let src = format!(
        "{SHAPES}fn a(s: Shape) -> Shape {{\n  Shape {{ ..s, radius: 2 }}\n}}\n\n\
         fn b(s: Shape) -> Shape {{\n  Shape {{ ..s, depth: 2 }}\n}}\n\n\
         fn c(s: Shape) -> Shape {{\n  Shape {{ ..s, name: \"a\", name: \"b\" }}\n}}\n\n\
         fn d(s: Shape) -> Shape {{\n  Shape {{ ..s, name: 1 }}\n}}\n"
    );
    let got = messages(&src);
    let expected = [
        "E0047: mismatched types [\"expected `String`, found `{integer}`\"]",
        "E0058: `Shape` has no field `depth` [\"unknown field\"]",
        "E0058: not every variant of `Shape` has the field `radius` [\"`Rect` has no field `radius`\"]",
        "E0059: field `name` is listed twice [\"listed again here\", \"first listed here\"]",
    ];
    assert_eq!(got, expected);
}

#[test]
fn a_field_must_have_one_type_in_every_variant() {
    let src = "type Value {\n  Small { n: Int },\n  Big { n: String },\n}\n\n\
               fn a(v: Value) -> Int {\n  v.n\n}\n\n\
               fn b(v: Value) -> Value {\n  Value { ..v, n: 1 }\n}\n";
    let got = messages(src);
    let expected = [
        "E0058: the field `n` does not have one type in every variant of `Value` [\"`n` is `Int` in `Small` and `String` in `Big`\"]",
        "E0058: the field `n` does not have one type in every variant of `Value` [\"`n` is `Int` in `Small` and `String` in `Big`\"]",
    ];
    assert_eq!(got, expected);
}

#[test]
fn update_needs_the_base_type_known_and_matching() {
    let src = format!(
        "{POINT}fn a(p: Point) -> Point {{\n  let f = fn(q) {{ Point {{ ..q, x: 1 }} }}\n  f(p)\n}}\n\n\
         fn b(n: Int) -> Point {{\n  Point {{ ..n, x: 1 }}\n}}\n"
    );
    let got = messages(&src);
    let expected = [
        "E0047: mismatched types [\"expected `Point`, found `Int`\", \"expected because of this type name\"]",
        "E0057: type annotation needed [\"the type of `q` must be known here to update it\", \"`q` is bound here\"]",
    ];
    assert_eq!(got, expected);
}

#[test]
fn field_access_reads_common_fields() {
    let src = format!(
        "{SHAPES}{POINT}fn name(s: Shape) -> String {{\n  s.name\n}}\n\n\
         fn sum(p: Point) -> Int {{\n  p.x + p.y\n}}\n"
    );
    let b = check(&src);
    assert_eq!(b.messages(), Vec::<String>::new(), "{}", b.rendered());
    let per_ctor = find(&b, "name", |k| match k {
        ExprKind::Field { index_per_ctor, .. } => Some(index_per_ctor.clone()),
        _ => None,
    });
    assert_eq!(per_ctor.len(), 2);
}

#[test]
fn field_access_errors() {
    let src = format!(
        "{SHAPES}{POINT}fn a(s: Shape) -> Int {{\n  s.radius\n}}\n\n\
         fn b(p: Point) -> Int {{\n  p.z\n}}\n\n\
         fn c(t: (Int, Int)) -> Int {{\n  t.first\n}}\n\n\
         fn d(n: Int) -> Int {{\n  n.value\n}}\n\n\
         fn e(o: Option<Int>) -> Int {{\n  o.value\n}}\n"
    );
    let got = messages(&src);
    let expected = [
        "E0058: `(Int, Int)` has no field `first` [\"unknown field\", \"this has type `(Int, Int)`\"]",
        "E0058: `Int` has no field `value` [\"unknown field\", \"this has type `Int`\"]",
        "E0058: `Option` has no field `value` [\"unknown field\"]",
        "E0058: `Point` has no field `z` [\"unknown field\"]",
        "E0058: not every variant of `Shape` has the field `radius` [\"`Rect` has no field `radius`\"]",
    ];
    assert_eq!(got, expected);
}

#[test]
fn field_access_needs_a_known_type() {
    // §3.12.4's example: `p`'s type is not known when `p.x` is checked.
    let src = format!(
        "{POINT}fn main() {{\n  let get_x = fn(p) {{ p.x }}\n  let _ = get_x(Point {{ x: 1, y: 2 }})\n}}\n"
    );
    let b = check(&src);
    assert_eq!(
        b.messages(),
        [
            "E0057: type annotation needed [\"the type of `p` must be known here to read its field `x`\", \"`p` is bound here\"]"
        ],
    );
    assert!(
        b.rendered()
            .contains("annotate the parameter: `fn(p: Point)`"),
        "{}",
        b.rendered()
    );
    // An annotation, or a use checked earlier, makes it known.
    let src = format!(
        "{POINT}fn main() {{\n  let get_x = fn(p: Point) {{ p.x }}\n  let _ = get_x(Point {{ x: 1, y: 2 }})\n}}\n"
    );
    assert_eq!(messages(&src), Vec::<String>::new());
}

const GEO: &str = "//! Geometry.\n\npub type Point {\n  x: Int,\n  y: Int,\n}\n\npub exposed type Size {\n  width: Int,\n}\n\npub fn origin() -> Point {\n  Point { x: 0, y: 0 }\n}\n\npub fn unit() -> Size {\n  Size { width: 1 }\n}\n";

#[test]
fn fields_of_an_opaque_type_are_not_visible_in_another_package() {
    let local = "//! Test.\n\nimport std::geo\n\nfn a() -> Int {\n  geo::origin().x\n}\n\nfn b() -> geo::Point {\n  geo::Point { ..geo::origin(), y: 1 }\n}\n\nfn c() -> Int {\n  geo::unit().width\n}\n";
    let b = build_with_std(&[("geo", GEO)], &[("", local)]);
    let got = b.messages();
    let expected = [
        "E0043: the fields of `Point` are not visible outside its package [\"\"]",
        "E0043: the fields of `Point` are not visible outside its package [\"\"]",
    ];
    assert_eq!(got, expected, "{}", b.rendered());
}

#[test]
fn punned_fields_read_locals_and_items() {
    // `y` is a constant and `f` a function; both are punned (D-113).
    let src = "type Holder {\n  y: Int,\n  f: fn(Int) -> Int,\n}\n\n\
               const y: Int = 2\n\n\
               fn f(n: Int) -> Int {\n  n\n}\n\n\
               fn make() -> Holder {\n  Holder { y, f }\n}\n";
    let b = check(src);
    assert_eq!(b.messages(), Vec::<String>::new(), "{}", b.rendered());
    let kinds = find(&b, "make", |k| match k {
        ExprKind::Record { fields, .. } => Some(fields.clone()),
        _ => None,
    });
    let body = b.thir_fn("app", "make").unwrap().body.as_ref().unwrap();
    let value_of = |n: usize| &body.exprs.get(kinds[n].1).unwrap().kind;
    assert!(matches!(value_of(0), ExprKind::ConstRef { .. }));
    assert!(matches!(value_of(1), ExprKind::FnRef { .. }));
}

#[test]
fn the_update_help_names_the_type() {
    let src = format!(
        "{POINT}fn main() {{\n  let f = fn(q) {{ Point {{ ..q, x: 1 }} }}\n  let _ = f(Point {{ x: 0, y: 0 }})\n}}\n"
    );
    let b = check(&src);
    assert_eq!(b.codes(), ["E0057"]);
    assert!(
        b.rendered()
            .contains("annotate the parameter: `fn(q: Point)`"),
        "{}",
        b.rendered()
    );
}

#[test]
fn update_works_through_an_alias() {
    // An alias is interchangeable with its definition (§4.4, D-081).
    let src = format!(
        "{POINT}type Spot = Point\n\ntype Pair = (Int, Int)\n\n\
         fn moved(p: Spot) -> Point {{\n  Spot {{ ..p, x: 1 }}\n}}\n\n\
         fn bad(p: Pair) -> Pair {{\n  Pair {{ ..p, x: 1 }}\n}}\n"
    );
    let got = messages(&src);
    assert_eq!(
        got,
        [
            "E0058: `Pair` stands for `(Int, Int)`, which has no fields to update [\"\", \"`Pair` is declared here\"]"
        ]
    );
}
