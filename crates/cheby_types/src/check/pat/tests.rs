//! Tests of `pat.rs`: every pattern form and its errors (chapter 6).

use crate::test_support::Build;
use crate::test_support::quiet::build;
use crate::thir::{ExprKind, Lit, Pat, PatKind, Stmt};

/// Builds a one-module package from `items`, which follow a module doc.
fn check(items: &str) -> Build {
    build(&[("", &format!("//! Test.\n\n{items}"))])
}

/// The diagnostics as `CODE: message [labels]`, sorted.
fn messages(items: &str) -> Vec<String> {
    check(items).messages()
}

const SHAPES: &str =
    "type Shape {\n  Circle { radius: Int },\n  Rect { width: Int, height: Int },\n  Dot,\n}\n\n";

/// The root patterns of the first `case` in function `name`, per arm.
fn arm_pats(b: &Build, name: &str) -> Vec<Vec<Pat>> {
    let f = b.thir_fn("app", name).expect("the function checks");
    let body = f.body.as_ref().expect("a body");
    body.exprs
        .iter()
        .find_map(|e| match &e.kind {
            ExprKind::Case { arms, .. } => {
                Some(arms.iter().map(|a| a.pats.to_vec()).collect::<Vec<_>>())
            }
            _ => None,
        })
        .expect("a case")
}

#[test]
fn every_pattern_form_checks() {
    let src = format!(
        "{SHAPES}fn area(shape: Shape) -> Int {{\n  case shape {{\n    Circle {{ radius }} => radius\n    Rect {{ width: w, .. }} => w\n    Dot => 0\n  }}\n}}\n\n\
         fn pick(pair: (Int, String)) -> String {{\n  let (_, text) = pair\n  text\n}}\n\n\
         fn code(n: Int) -> String {{\n  case n {{\n    0 => \"zero\"\n    -1 => \"minus one\"\n    _ => \"other\"\n  }}\n}}\n\n\
         fn word(s: String) -> Int {{\n  case s {{\n    \"a\" => 1\n    r\"b\" => 2\n    _ => 3\n  }}\n}}\n\n\
         fn ends(xs: List<Int>) -> List<Int> {{\n  case xs {{\n    [first, ..middle, last] => [first, ..middle, last]\n    [only] | [only, _] as two => [only, ..two]\n    [] => []\n  }}\n}}\n"
    );
    let b = check(&src);
    assert_eq!(b.messages(), Vec::<String>::new(), "{}", b.rendered());

    let area = arm_pats(&b, "area");
    let PatKind::Ctor { fields, .. } = &area[0][0].kind else {
        panic!("a constructor pattern: {:?}", area[0][0]);
    };
    assert_eq!(fields.len(), 1);
    assert!(matches!(
        fields[0],
        (
            0,
            Pat {
                kind: PatKind::Bind(_),
                ..
            }
        )
    ));
    let PatKind::Ctor { fields, .. } = &area[1][0].kind else {
        panic!("a constructor pattern");
    };
    // Named fields by declaration index, listed ones only.
    assert_eq!(fields.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0]);
    assert!(matches!(&area[2][0].kind, PatKind::Ctor { fields, .. } if fields.is_empty()));
    // Every pattern has a closed type.
    assert_eq!(area[0][0].ty, area[2][0].ty);

    let code = arm_pats(&b, "code");
    assert!(matches!(code[0][0].kind, PatKind::Lit(Lit::Int(0))));
    assert!(matches!(code[1][0].kind, PatKind::Lit(Lit::Int(-1))));
    assert_eq!(code[1][0].ty, cheby_iface::Ty::INT);

    let word = arm_pats(&b, "word");
    assert!(matches!(&word[1][0].kind, PatKind::Lit(Lit::Str(s)) if &**s == "b"));

    let ends = arm_pats(&b, "ends");
    let PatKind::List {
        prefix,
        rest,
        suffix,
    } = &ends[0][0].kind
    else {
        panic!("a list pattern");
    };
    assert_eq!((prefix.len(), suffix.len()), (1, 1));
    let rest = rest.as_ref().expect("a spread");
    assert!(matches!(rest.kind, PatKind::Bind(_)));
    // The spread has the list's type, its elements the element type.
    assert_eq!(rest.ty, ends[0][0].ty);
    assert_eq!(prefix[0].ty, cheby_iface::Ty::INT);
    assert!(
        matches!(&ends[1][0].kind, PatKind::As { pat, .. } if matches!(pat.kind, PatKind::Or(_)))
    );

    let pick = b.thir_fn("app", "pick").unwrap().body.as_ref().unwrap();
    let let_pat = pick
        .exprs
        .iter()
        .find_map(|e| match &e.kind {
            ExprKind::Block { stmts, .. } => stmts.iter().find_map(|s| match s {
                Stmt::Let { pat, .. } => Some(pat.clone()),
                _ => None,
            }),
            _ => None,
        })
        .unwrap();
    assert!(matches!(&let_pat.kind, PatKind::Tuple(elems) if elems.len() == 2));
}

#[test]
fn bindings_take_the_matched_types() {
    let src = "type Pair<A, B> {\n  Pair(A, B),\n}\n\nfn second(p: Pair<Int, String>) -> String {\n  let Pair(_, s) = p\n  s\n}\n\nfn first(p: Pair<Int, String>) -> String {\n  let Pair(n, _) = p\n  n\n}\n";
    let got = messages(src);
    assert_eq!(
        got,
        [
            "E0047: mismatched types [\"expected `String`, found `Int`\", \"expected because of this return type\"]"
        ],
    );
}

#[test]
fn constructor_patterns_check_their_shape() {
    let src = format!(
        "{SHAPES}type Pair {{\n  Pair(Int, Int),\n}}\n\n\
         fn a(p: Pair) -> Int {{\n  case p {{\n    Pair(x) => x\n  }}\n}}\n\n\
         fn b(s: Shape) -> Int {{\n  case s {{\n    Circle {{ diameter, .. }} => 0\n    _ => 1\n  }}\n}}\n\n\
         fn c(s: Shape) -> Int {{\n  case s {{\n    Rect {{ width }} => width\n    _ => 1\n  }}\n}}\n\n\
         fn d(o: Option<Int>) -> Int {{\n  case o {{\n    Some => 1\n    None(x) => 2\n  }}\n}}\n\n\
         fn e(s: Shape) -> Int {{\n  case s {{\n    Circle(r) => r\n    Rect {{ width: w, width: v, .. }} => w\n    _ => 0\n  }}\n}}\n"
    );
    let got = messages(&src);
    let expected = [
        "E0048: the constructor `None` has no fields [\"expected no fields\"]",
        "E0048: the constructor `Pair` has 2 fields, but the pattern has 1 [\"expected 2 fields\", \"`Pair` is declared here\"]",
        "E0048: the constructor `Some` has 1 field, but the pattern has none [\"expected 1 field\"]",
        "E0058: `Circle` has no field `diameter` [\"unknown field\"]",
        "E0059: `Circle` has named fields, not positional ones [\"matched by position here\", \"`Circle` is declared here\"]",
        "E0059: field `width` is listed twice in this pattern [\"listed again here\", \"first listed here\"]",
        "E0059: the pattern does not list the field `height` of `Rect` [\"missing `height`\"]",
    ];
    assert_eq!(got, expected);
}

#[test]
fn literal_patterns_fit_their_type() {
    let src = "fn a(n: U8) -> Int {\n  case n {\n    256 => 1\n    _ => 0\n  }\n}\n\n\
               fn b(n: U32) -> Int {\n  case n {\n    -1 => 1\n    _ => 0\n  }\n}\n\n\
               fn c(n: I8) -> Int {\n  case n {\n    -128 => 1\n    127 => 2\n    _ => 0\n  }\n}\n\n\
               fn d(s: String) -> Int {\n  case s {\n    1 => 1\n    _ => 0\n  }\n}\n\n\
               fn e(n: Int) -> Int {\n  case n {\n    \"one\" => 1\n    _ => 0\n  }\n}\n";
    let got = messages(src);
    let expected = [
        "E0047: mismatched types [\"expected `Int`, found `String`\", \"this subject has type `Int`\"]",
        "E0047: mismatched types [\"expected `String`, found `{integer}`\", \"this subject has type `String`\"]",
        "E0071: literal out of range for `U32` [\"`-1` does not fit in `U32`\"]",
        "E0071: literal out of range for `U8` [\"`256` does not fit in `U8`\"]",
    ];
    assert_eq!(got, expected);
}

#[test]
fn a_literal_pattern_of_a_type_fixed_later_is_checked_at_the_end() {
    let src = "fn a(n: U8) -> Int {\n  let f = fn(x) {\n    case x {\n      300 => 1\n      _ => 0\n    }\n  }\n  f(n)\n}\n";
    let got = messages(src);
    assert_eq!(
        got,
        ["E0071: literal out of range for `U8` [\"`300` does not fit in `U8`\"]"]
    );
}

#[test]
fn tuple_and_list_patterns_need_matching_types() {
    let src = "fn a(n: Int) -> Int {\n  let (x, y) = n\n  x\n}\n\n\
               fn b(p: (Int, Int)) -> Int {\n  let (x, y, z) = p\n  x\n}\n\n\
               fn c(s: String) -> Int {\n  case s {\n    [x] => 1\n    _ => 0\n  }\n}\n";
    let got = messages(src);
    let expected = [
        "E0047: mismatched types [\"expected `(Int, Int)`, found `(_, _, _)`\"]",
        "E0047: mismatched types [\"expected `Int`, found `(_, _)`\"]",
        "E0047: mismatched types [\"expected `String`, found `List<_>`\", \"this subject has type `String`\"]",
    ];
    assert_eq!(got, expected);
}

#[test]
fn alternatives_bind_one_type() {
    let src = "fn first(p: (Int, String)) -> Int {\n  case p {\n    (0, n) | (n, \"\") => 1\n    _ => 0\n  }\n}\n\n\
               fn second(p: (Int, Int)) -> Int {\n  case p {\n    (0, n) | (n, 0) => n\n    (a, _) => a\n  }\n}\n";
    let got = messages(src);
    assert_eq!(
        got,
        [
            "E0047: mismatched types [\"`n` is `Int` here\", \"`n` is `String` in an earlier alternative\"]"
        ]
    );
}

#[test]
fn errors_inside_a_wrong_constructor_still_bind_names() {
    // `Nope` is unresolved: its fields are checked against the error type,
    // so `x` is bound and the body reports nothing else.
    let src = "fn a(o: Option<Int>) -> Int {\n  case o {\n    Nope(x) => x\n    _ => 0\n  }\n}\n";
    let b = check(src);
    assert_eq!(b.codes(), ["E0042"], "{}", b.rendered());
}

/// A pattern checked against the error type gives its sub-patterns the
/// error type, so uses of the names it binds are silent (no E0057).
#[test]
fn patterns_against_an_error_bind_errors() {
    let src = "type Point {\n  Point { x: Int },\n}\n\n\
               pub fn tuple(p: List) -> Int {\n  let (a, b) = p\n  a.x + b.x\n}\n\n\
               pub fn ctor(p: List) -> Int {\n  case p {\n    Some(a) => a.x\n    _ => 0\n  }\n}\n\n\
               pub fn list(p: List) -> Int {\n  case p {\n    [a, ..] => a.x\n    _ => 0\n  }\n}\n";
    let b = check(src);
    assert_eq!(b.codes(), ["E0051", "E0051", "E0051"], "{}", b.rendered());
}
