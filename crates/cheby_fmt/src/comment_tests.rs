//! Unit tests of comments and blank lines between the parts of a node
//! (wave 3 of the formatter): each position at a narrow and a wide width,
//! never printed verbatim (D-1160, D-1161).

use super::{coverage, format_source};

/// `text` formatted at `width`, checked to be idempotent and to print no
/// node verbatim.
#[track_caller]
fn fmt_at(text: &str, width: usize) -> String {
    let once = format_source(text, width).unwrap();
    let twice = format_source(&once, width).unwrap();
    assert_eq!(once, twice, "not idempotent");
    let c = coverage(text, width).unwrap();
    assert!(
        c.verbatim.is_empty() && c.verbatim_items == 0,
        "verbatim: {:?}, {} item(s)",
        c.verbatim,
        c.verbatim_items
    );
    once
}

/// Checks a whole file formatted at a narrow and a wide width.
#[track_caller]
fn file(src: &str, narrow: (usize, &str), wide: &str) {
    assert_eq!(fmt_at(src, narrow.0), narrow.1, "at {}", narrow.0);
    assert_eq!(fmt_at(src, 100), wide, "at 100");
}

/// Checks `src`, the statements of a function, formatted at a narrow and
/// a wide width; the expected texts are the statements too.
#[track_caller]
fn body(src: &str, narrow: (usize, &str), wide: &str) {
    let wrap = |s: &str| format!("fn f() {{\n{s}}}\n");
    file(&wrap(src), (narrow.0, &wrap(narrow.1)), &wrap(wide));
}

#[test]
fn fmt_comment_between_binary_operands() {
    // After the operator, at the end of its line, or on a line of its own
    // before the right operand: the chain breaks there, one level deeper.
    let src = "  let x = total(alpha, beta) + // why\n    gamma\n  let y = alpha +\n    // own line\n    beta * gamma\n";
    body(
        src,
        (
            20,
            "  let x = total(\n    alpha,\n    beta,\n  ) + // why\n    gamma\n  let y = alpha +\n    // own line\n    beta * gamma\n",
        ),
        src,
    );
    // Every operator of the chain breaks with it.
    body(
        "  let z = a + // one\n    b + c\n",
        (12, "  let z = a + // one\n    b +\n    c\n"),
        "  let z = a + // one\n    b +\n    c\n",
    );
}

#[test]
fn fmt_comment_before_an_operator() {
    // Before `&&` or `||` the comment takes the place of the line break;
    // before another operator, inside brackets, the operator starts the
    // next line.
    let src = "  let ok = valid(order) // first\n    && stocked(order)\n  let n = (alpha // before\n    + beta)\n";
    body(
        src,
        (
            16,
            "  let ok = valid(\n    order,\n  ) // first\n    && stocked(\n      order,\n    )\n  let n = (alpha // before\n    + beta)\n",
        ),
        src,
    );
}

#[test]
fn fmt_comment_around_pipeline_stages() {
    // After the head, between two stages, and after `|>`: the pipeline
    // breaks, one stage per line.
    let src = "  orders // all of them\n  |> list::filter(_, is_paid)\n  // then the prices\n  |> list::map(_, price)\n  |> // and the sum\n    list::sum\n";
    body(
        src,
        (
            24,
            "  orders // all of them\n  |> list::filter(\n    _,\n    is_paid,\n  )\n  // then the prices\n  |> list::map(_, price)\n  |> // and the sum\n    list::sum\n",
        ),
        src,
    );
    // As a value, the head stays on the line of `=` and the `|>` lines go
    // one level deeper (D-1275).
    body(
        "  let t = xs |> // why\n    list::sum\n",
        (12, "  let t = xs\n    |> // why\n      list::sum\n"),
        "  let t = xs\n    |> // why\n      list::sum\n",
    );
}

#[test]
fn fmt_comment_inside_statements() {
    // After `=` of `let` and `let assert`, after `<-` of `use`, after an
    // operator of `assert`: the value starts the next line, one level
    // deeper, and is laid out there.
    let src = "  let x = // value\n    compute(alpha, beta)\n  let assert Ok(v) = // why\n    parse(text) as \"it parses\"\n  use a <- // load\n    result::try(load(path))\n  assert total(xs) == // ten\n    10\n";
    body(
        src,
        (
            22,
            "  let x = // value\n    compute(\n      alpha,\n      beta,\n    )\n  let assert Ok(v) = // why\n    parse(\n      text,\n    ) as \"it parses\"\n  use a <- // load\n    result::try(\n      load(path),\n    )\n  assert total(xs) == // ten\n    10\n",
        ),
        src,
    );
    // A comment on a line of its own before the value.
    let own = "  let x =\n    // value\n    compute(alpha)\n";
    body(
        own,
        (
            12,
            "  let x =\n    // value\n    compute(\n      alpha,\n    )\n",
        ),
        own,
    );
}

#[test]
fn fmt_comment_inside_case_arms() {
    // After `=>`, at the end of the arm's line or on a line of its own;
    // between `case` and its subjects and after them, inside brackets.
    let src = "  case x {\n    Some(value) => // the value\n      value\n    None =>\n      // nothing\n      fallback(xs, ys)\n  }\n  let y = (case // subject\n    x {\n    _ => 1\n  })\n";
    body(
        src,
        (
            20,
            "  case x {\n    Some(value) => // the value\n      value\n    None =>\n      // nothing\n      fallback(\n        xs,\n        ys,\n      )\n  }\n  let y = (case // subject\n    x {\n      _ => 1\n    })\n",
        ),
        "  case x {\n    Some(value) => // the value\n      value\n    None =>\n      // nothing\n      fallback(xs, ys)\n  }\n  let y = (case // subject\n    x {\n      _ => 1\n    })\n",
    );
}

#[test]
fn fmt_comment_between_callee_and_arguments() {
    let src = "  let a = (compute // callee\n    (alpha, beta))\n";
    body(
        src,
        (
            16,
            "  let a = (compute // callee\n    (\n      alpha,\n      beta,\n    ))\n",
        ),
        src,
    );
}

#[test]
fn fmt_comment_around_field_access() {
    let src =
        "  let a = (point // before the dot\n    .x)\n  let b = (point. // after the dot\n    y)\n";
    body(src, (12, src), src);
}

#[test]
fn fmt_comment_in_paths() {
    let src = "  let c = (m:: // after\n    f)\n  let d = (m // before\n    ::f)\n";
    body(src, (12, src), src);
}

#[test]
fn fmt_comment_in_patterns() {
    // Between the alternatives of a pattern, inside brackets.
    let src = "  case v {\n    (Some(x) // left\n      | None) => 1\n  }\n";
    body(src, (20, src), src);
    // Narrower, the arm's body stays on the arm's line
    // (D-1276).
    body(
        src,
        (
            12,
            "  case v {\n    (Some(\n      x,\n    ) // left\n      | None) => 1\n  }\n",
        ),
        src,
    );
}

#[test]
fn fmt_comment_in_types() {
    // After `->` of a function type, and after `=` of an alias.
    let src = "type F = fn(Int, String) -> // the result\n  Result<Int, String>\npub type Shape<T> = // the shape\n  List<T>\n";
    file(
        src,
        (
            20,
            "type F = fn(\n  Int,\n  String,\n) -> // the result\n  Result<\n    Int,\n    String,\n  >\npub type Shape<T> = // the shape\n  List<T>\n",
        ),
        src,
    );
}

#[test]
fn fmt_comment_in_unary_operators() {
    let src = "  let p = (- // negate\n    x)\n  let q = (! // not\n    done)\n";
    body(src, (12, src), src);
}

#[test]
fn fmt_comment_between_element_and_comma() {
    // The comma, the list's own separator, goes before the comment: a
    // comment on the element's line stays at its end, one on a line of its
    // own goes before the next element. The list breaks.
    body(
        "  let xs = [alpha // a\n    , beta]\n  let ys = [\n    alpha\n    // own line\n    , beta\n  ]\n",
        (
            12,
            "  let xs = [\n    alpha, // a\n    beta,\n  ]\n  let ys = [\n    alpha,\n    // own line\n    beta,\n  ]\n",
        ),
        "  let xs = [\n    alpha, // a\n    beta,\n  ]\n  let ys = [\n    alpha,\n    // own line\n    beta,\n  ]\n",
    );
    // In a header.
    file(
        "fn f(\n  a: Int // a\n  , b: Int,\n) {}\n",
        (12, "fn f(\n  a: Int, // a\n  b: Int,\n) {}\n"),
        "fn f(\n  a: Int, // a\n  b: Int,\n) {}\n",
    );
}

#[test]
fn fmt_comment_two_after_one_element() {
    // One before the comma and one after it: the second goes on a line of
    // its own before the next element.
    let out = "  let ys = [\n    alpha, // one\n    // two\n    beta,\n  ]\n";
    body(
        "  let ys = [alpha // one\n    , // two\n    beta]\n",
        (12, out),
        out,
    );
}

#[test]
fn fmt_comment_in_item_headers() {
    // A comment after an attribute, between attributes and the item,
    // after `->` and after `=`: wave 1 kept these items verbatim.
    let src = "@inline // why\n// between\nfn compute(alpha: Int, beta: Int) -> // the sum\n  Int {\n  alpha + beta\n}\n\nconst k: Int = // the answer\n  42\n";
    file(
        src,
        (
            24,
            "@inline // why\n// between\nfn compute(\n  alpha: Int,\n  beta: Int,\n) -> // the sum\n  Int {\n  alpha + beta\n}\n\nconst k: Int = // the answer\n  42\n",
        ),
        src,
    );
    // A line break alone in a header is the layout's to place.
    file(
        "fn f() ->\n  Int {\n  1\n}\n",
        (12, "fn f() -> Int {\n  1\n}\n"),
        "fn f() -> Int {\n  1\n}\n",
    );
}

#[test]
fn fmt_blank_line_between_binary_operands() {
    // D-1161: kept as one blank line, the line
    // broken there.
    let src = "  let x = alpha +\n\n\n    beta + gamma\n";
    let out = "  let x = alpha +\n\n    beta +\n    gamma\n";
    body(src, (12, out), out);
}

#[test]
fn fmt_blank_line_between_pipeline_stages() {
    let src = "  xs\n  |> list::map(_, double)\n\n  |> list::sum\n";
    body(
        src,
        (
            20,
            "  xs\n  |> list::map(\n    _,\n    double,\n  )\n\n  |> list::sum\n",
        ),
        src,
    );
}

#[test]
fn fmt_blank_line_in_statements_and_headers() {
    body(
        "  let x =\n\n    compute(alpha)\n",
        (12, "  let x =\n\n    compute(\n      alpha,\n    )\n"),
        "  let x =\n\n    compute(alpha)\n",
    );
    file(
        "const k: Int =\n\n  42\n",
        (12, "const k: Int =\n\n  42\n"),
        "const k: Int =\n\n  42\n",
    );
    // A blank line around a comment stays where it was.
    body(
        "  let x =\n\n    // value\n\n    compute(alpha)\n",
        (
            12,
            "  let x =\n\n    // value\n\n    compute(\n      alpha,\n    )\n",
        ),
        "  let x =\n\n    // value\n\n    compute(alpha)\n",
    );
}

#[test]
fn fmt_blank_multiline_raw_string_in_a_pattern() {
    // A token that holds a line break is kept as written: the lines inside
    // the string are its value.
    let src = "  case s {\n    r#\"a\n  b \"# => 1\n    _ => 2\n  }\n";
    body(src, (12, src), src);
    body(
        "  case   s {\n      r#\"a\n  b \"#   =>   1\n    _ => 2\n  }\n",
        (12, src),
        src,
    );
}
