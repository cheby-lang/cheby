//! Unit tests of the layout of expressions, statements, patterns and
//! types (wave 2 of the formatter): each rule at a narrow and a wide
//! width.

use super::format_source;

/// `text` formatted at `width`, checked to be idempotent.
#[track_caller]
fn fmt_at(text: &str, width: usize) -> String {
    let once = format_source(text, width).unwrap();
    let twice = format_source(&once, width).unwrap();
    assert_eq!(once, twice, "not idempotent");
    once
}

/// Checks `body`, the statements of a function, formatted at a narrow and
/// a wide width; the expected texts are the statements too.
#[track_caller]
fn body(src: &str, narrow: (usize, &str), wide: &str) {
    let wrap = |s: &str| format!("fn f() {{\n{s}}}\n");
    assert_eq!(
        fmt_at(&wrap(src), narrow.0),
        wrap(narrow.1),
        "at {}",
        narrow.0
    );
    assert_eq!(fmt_at(&wrap(src), 100), wrap(wide), "at 100");
}

#[test]
fn fmt_expr_call_arguments_break_one_per_line() {
    body(
        "  let x = compute( alpha,beta , gamma )\n",
        (
            30,
            "  let x = compute(\n    alpha,\n    beta,\n    gamma,\n  )\n",
        ),
        "  let x = compute(alpha, beta, gamma)\n",
    );
    // A trailing comma goes when the list fits.
    body("  g(a, b,)\n", (30, "  g(a, b)\n"), "  g(a, b)\n");
}

#[test]
fn fmt_expr_last_closure_hugs_the_call() {
    body(
        "  list::each(items, fn(item) { io::println(item) })\n",
        (
            30,
            "  list::each(items, fn(item) {\n    io::println(item)\n  })\n",
        ),
        "  list::each(items, fn(item) { io::println(item) })\n",
    );
    // A closure of several statements always breaks, and still hugs.
    let src =
        "  list::each(items, fn(item) {\n    io::println(item)\n    io::println(item)\n  })\n";
    body(src, (30, src), src);
}

#[test]
fn fmt_expr_last_list_hugs_the_call() {
    body(
        "  report(title, [first_entry, second_entry])\n",
        (
            30,
            "  report(title, [\n    first_entry,\n    second_entry,\n  ])\n",
        ),
        "  report(title, [first_entry, second_entry])\n",
    );
}

#[test]
fn fmt_expr_call_hugs_a_last_call_that_hugs() {
    let src = "  [head, ..tail] => Some(fold(tail, head, fn(best, item) { pick(best, item) }))\n";
    let wrap = |arm: &str| format!("  case xs {{\n  {arm}  }}\n");
    body(
        &wrap(src),
        (
            64,
            &wrap(
                "  [head, ..tail] => Some(fold(tail, head, fn(best, item) {\n      pick(best, item)\n    }))\n",
            ),
        ),
        &wrap(src),
    );
    // Narrower, the call breaks on the arm's line: an arm's body never
    // moves to the next line (D-1276).
    body(
        &wrap(src),
        (
            50,
            &wrap(
                "  [head, ..tail] => Some(\n      fold(tail, head, fn(best, item) {\n        pick(best, item)\n      }),\n    )\n",
            ),
        ),
        &wrap(src),
    );
}

#[test]
fn fmt_expr_value_after_equals_hugs_before_moving() {
    // After `=`, a list that does not fit hugs the `let` line.
    body(
        "  let values = Array([Float(nan), Float(infinity), Int(1)])\n",
        (
            40,
            "  let values = Array([\n    Float(nan),\n    Float(infinity),\n    Int(1),\n  ])\n",
        ),
        "  let values = Array([Float(nan), Float(infinity), Int(1)])\n",
    );
}

#[test]
fn fmt_expr_arm_body_breaks_on_the_arm_line() {
    // D-1276: after `=>`, a body that does not fit flat on
    // the arm's line breaks there; it never moves to the next line.
    let wrap = |arm: &str| format!("  case x {{\n{arm}  }}\n");
    body(
        &wrap("    Missing(what) => error_json(404, \"no such {what}\", [])\n"),
        (
            50,
            &wrap(
                "    Missing(what) => error_json(\n      404,\n      \"no such {what}\",\n      [],\n    )\n",
            ),
        ),
        &wrap("    Missing(what) => error_json(404, \"no such {what}\", [])\n"),
    );
    // The next-line body of the old layout comes back to the arm's line,
    // hugging its last argument when that first line fits.
    body(
        &wrap("    Some(name) =>\n      step(names, [..seated, sit(name)])\n"),
        (
            44,
            &wrap("    Some(name) => step(names, [\n      ..seated,\n      sit(name),\n    ])\n"),
        ),
        &wrap("    Some(name) => step(names, [..seated, sit(name)])\n"),
    );
    // A body that cannot break stays on the arm's line past the width.
    body(
        &wrap("    Missing(what) =>\n      \"no such thing as {what} anywhere\"\n"),
        (
            30,
            &wrap("    Missing(what) => \"no such thing as {what} anywhere\"\n"),
        ),
        &wrap("    Missing(what) => \"no such thing as {what} anywhere\"\n"),
    );
}

#[test]
fn fmt_expr_arm_block_body_always_breaks() {
    // D-1276: an arm's block body is broken even with one
    // statement that fits.
    let wrap = |arms: &str| format!("  case x {{\n{arms}  }}\n");
    let out = wrap(
        "    Some(0x20) | Some(0x0A) => {\n      read_value(builder, i + 1)\n    }\n    None => done\n    _ => fail\n",
    );
    body(
        &wrap(
            "    Some(0x20) | Some(0x0A) => { read_value(builder, i + 1) }\n    None => done\n    _ => fail\n",
        ),
        (60, &out),
        &out,
    );
    // A closure's or a `let`'s one-statement block keeps D-1155's form.
    body(
        "  let g = fn(y) { y + 1 }\n",
        (40, "  let g = fn(y) { y + 1 }\n"),
        "  let g = fn(y) { y + 1 }\n",
    );
}

#[test]
fn fmt_expr_arms_have_one_space_before_the_arrow() {
    // D-1276: every arm has one space between its patterns
    // and guard and `=>`, whatever its neighbours; source padded to line
    // up the arrows formats to one space (and stays there: idempotence).
    let wrap = |arms: &str| format!("  case op {{\n{arms}  }}\n");
    let out = wrap(
        "    Add => a + b\n    Subtract => a - b\n\n    Divide when b == 0 => Err(\"zero\")\n    Divide => Ok(a / b)\n",
    );
    body(
        &wrap(
            "    Add      => a + b\n    Subtract => a - b\n\n    Divide when b == 0 => Err(\"zero\")\n    Divide             => Ok(a / b)\n",
        ),
        (60, &out),
        &out,
    );
    // A subjectless `case`'s arms too.
    let out = "  case {\n    x < 0 => \"negative\"\n    _ => \"other\"\n  }\n";
    body(
        "  case {\n    x < 0 => \"negative\"\n    _     => \"other\"\n  }\n",
        (60, out),
        out,
    );
}

#[test]
fn fmt_expr_binary_chain_breaks_after_each_operator() {
    body(
        "  let total = alpha+beta * gamma +   delta\n",
        (30, "  let total = alpha +\n    beta * gamma +\n    delta\n"),
        "  let total = alpha + beta * gamma + delta\n",
    );
}

#[test]
fn fmt_expr_conditions_break_before_and_and_or() {
    body(
        "  let ok = is_valid(order) && has_stock(order) || is_late(order)\n",
        (
            30,
            "  let ok = is_valid(order)\n    && has_stock(order)\n    || is_late(order)\n",
        ),
        "  let ok = is_valid(order) && has_stock(order) || is_late(order)\n",
    );
}

#[test]
fn fmt_expr_one_operator_lays_out_its_right_operand_as_a_value() {
    // The right operand stays on the line, or moves to the next one, or
    // opens a tuple there that breaks inside.
    body(
        "  assert compute(alpha, beta) == (expected_alpha, expected_beta)\n",
        (
            60,
            "  assert compute(alpha, beta) ==\n    (expected_alpha, expected_beta)\n",
        ),
        "  assert compute(alpha, beta) == (expected_alpha, expected_beta)\n",
    );
    body(
        "  assert compute(alpha, beta) == (expected_alpha, expected_beta)\n",
        (
            30,
            "  assert compute(\n    alpha,\n    beta,\n  ) == (\n    expected_alpha,\n    expected_beta,\n  )\n",
        ),
        "  assert compute(alpha, beta) == (expected_alpha, expected_beta)\n",
    );
}

#[test]
fn fmt_expr_pipeline_one_stage_per_line() {
    // D-1275: one stage per line even when it fits; as
    // a statement, the `|>` lines stay at the head's indentation.
    let out = "  orders\n  |> list::filter(_, is_paid)\n  |> list::map(_, price)\n";
    body(
        "  orders |> list::filter(_, is_paid)\n    |> list::map(_, price)\n",
        (30, out),
        out,
    );
    // A single `|>` breaks too.
    body(
        "  data |> bytes::to_list\n",
        (30, "  data\n  |> bytes::to_list\n"),
        "  data\n  |> bytes::to_list\n",
    );
}

#[test]
fn fmt_expr_pipeline_value_stays_on_the_operator_line() {
    // After `=`, `<-` and `=>`, the head stays on the operator's line and
    // the `|>` lines go one level deeper (D-1275).
    let out = "  let total = orders\n    |> list::filter(_, is_paid)\n    |> list::map(_, price)\n";
    body(
        "  let total = orders |> list::filter(_, is_paid) |> list::map(_, price)\n",
        (32, out),
        out,
    );
    let out = "  use x <- xs\n    |> result::try\n  x\n";
    body("  use x <- xs |> result::try\n  x\n", (30, out), out);
    let out = "  case path {\n    \"/old\" => text(301, \"\")\n      |> with_header(_, \"location\", \"/new\")\n    _ => text(404, \"\")\n  }\n";
    body(
        "  case path {\n    \"/old\" => text(301, \"\") |> with_header(_, \"location\", \"/new\")\n    _ => text(404, \"\")\n  }\n",
        (60, out),
        out,
    );
}

#[test]
fn fmt_expr_pipeline_nested_in_other_constructs() {
    // A pipe in a call argument, in a one-statement closure and as an
    // operand breaks; the construct around it breaks as a multi-line
    // element requires (D-1275).
    body(
        "  f(xs |> list::sum)\n",
        (40, "  f(\n    xs\n    |> list::sum,\n  )\n"),
        "  f(\n    xs\n    |> list::sum,\n  )\n",
    );
    body(
        "  let g = fn(x) { x |> double }\n",
        (40, "  let g = fn(x) {\n    x\n    |> double\n  }\n"),
        "  let g = fn(x) {\n    x\n    |> double\n  }\n",
    );
    body(
        "  assert [\"x\", \"y\"] |> join(_, \"\") == \"xy\"\n",
        (
            60,
            "  assert [\"x\", \"y\"]\n    |> join(_, \"\") == \"xy\"\n",
        ),
        "  assert [\"x\", \"y\"]\n    |> join(_, \"\") == \"xy\"\n",
    );
}

#[test]
fn fmt_expr_pipeline_last_stage_breaks_by_itself() {
    // The last stage hugs its closure by its own layout, on its own line.
    let out = "  xs\n  |> list::each(_, fn(x) {\n    io::println(x)\n    io::println(x)\n  })\n";
    let src = "  xs |> list::each(_, fn(x) {\n    io::println(x)\n    io::println(x)\n  })\n";
    body(src, (30, out), out);
    // When the last stage cannot hug on its line, its arguments break.
    body(
        src,
        (
            20,
            "  xs\n  |> list::each(\n    _,\n    fn(x) {\n      io::println(x)\n      io::println(x)\n    },\n  )\n",
        ),
        out,
    );
    body(
        src,
        (
            16,
            "  xs\n  |> list::each(\n    _,\n    fn(x) {\n      io::println(\n        x,\n      )\n      io::println(\n        x,\n      )\n    },\n  )\n",
        ),
        out,
    );
}

#[test]
fn fmt_expr_record_literals() {
    body(
        "  let p = Point{x: 1,y: 2}\n  let q = Point { ..p, label: name }\n",
        (
            24,
            "  let p = Point {\n    x: 1,\n    y: 2,\n  }\n  let q = Point {\n    ..p,\n    label: name,\n  }\n",
        ),
        "  let p = Point { x: 1, y: 2 }\n  let q = Point { ..p, label: name }\n",
    );
}

#[test]
fn fmt_expr_lists_and_tuples() {
    body(
        "  let xs = [first, second, ..rest]\n  let pair = (a, b,)\n",
        (
            20,
            "  let xs = [\n    first,\n    second,\n    ..rest,\n  ]\n  let pair = (a, b)\n",
        ),
        "  let xs = [first, second, ..rest]\n  let pair = (a, b)\n",
    );
}

#[test]
fn fmt_expr_one_element_tuple_keeps_its_comma() {
    // `(x,)` is a tuple and `(x)` is not: the comma stays at every width.
    let src = "  let one = ( x , )\n  let paren = ( x )\n";
    let out = "  let one = (x,)\n  let paren = (x)\n";
    body(src, (18, out), out);
    let long = "a_long_name_for_the_only_element";
    body(
        &format!("  let one = ({long},)\n"),
        (20, &format!("  let one = (\n    {long},\n  )\n")),
        &format!("  let one = ({long},)\n"),
    );
}

#[test]
fn fmt_expr_unary_field_access_and_paths_are_tight() {
    let src = "  let a = - x\n  let b = ! done\n  let c = -1\n  let d = point . x . y\n  let e = m :: f :: < Int > ( x )\n";
    let out = "  let a = -x\n  let b = !done\n  let c = -1\n  let d = point.x.y\n  let e = m::f::<Int>(x)\n";
    body(src, (24, out), out);
}

#[test]
fn fmt_expr_negative_literal_gap_is_kept() {
    // `- 1` is an error (D-470); the formatter does not close the gap.
    body(
        "  let c = - 1\n",
        (20, "  let c = - 1\n"),
        "  let c = - 1\n",
    );
}

#[test]
fn fmt_expr_case_arms() {
    body(
        "  case shape {\n    Circle(radius) when radius > 0 => area(radius)\n    Square(side) => { side * side }\n    _ => 0\n  }\n",
        (
            48,
            "  case shape {\n    Circle(radius) when radius > 0 => area(\n      radius,\n    )\n    Square(side) => {\n      side * side\n    }\n    _ => 0\n  }\n",
        ),
        "  case shape {\n    Circle(radius) when radius > 0 => area(radius)\n    Square(side) => {\n      side * side\n    }\n    _ => 0\n  }\n",
    );
}

#[test]
fn fmt_expr_case_always_breaks() {
    let out = "  case {\n    x < 0 => \"negative\"\n    _ => \"other\"\n  }\n";
    body(
        "  case { x < 0 => \"negative\"\n    _ => \"other\" }\n",
        (30, out),
        out,
    );
}

#[test]
fn fmt_expr_blocks_and_closures() {
    body(
        "  let x = {   compute(alpha)   }\n  let g = fn(y) { y + 1 }\n",
        (
            20,
            "  let x = {\n    compute(alpha)\n  }\n  let g = fn(y) {\n    y + 1\n  }\n",
        ),
        "  let x = { compute(alpha) }\n  let g = fn(y) { y + 1 }\n",
    );
}

#[test]
fn fmt_expr_strings_are_kept_as_written() {
    let src = "  let s = \"a   {name}  b\"\n  let r = r#\"raw  \"text\"\"#\n  let m = \"one\n   two  \"\n";
    body(src, (30, src), src);
    // A string that does not fit moves to the next line, whole.
    body(
        src,
        (
            20,
            "  let s =\n    \"a   {name}  b\"\n  let r =\n    r#\"raw  \"text\"\"#\n  let m = \"one\n   two  \"\n",
        ),
        src,
    );
}

#[test]
fn fmt_expr_comment_inside_an_expression_is_laid_out() {
    // A comment between operands keeps its place (wave 3; wave 2 kept the
    // expression's lines verbatim).
    let src = "  let x = alpha + // why\n    beta\n";
    body(src, (20, src), src);
    body("  let x = alpha   + // why\n beta\n", (20, src), src);
    // Comments between arguments have their place.
    body(
        "  g(\n    a, // first\n    b,\n  )\n",
        (20, "  g(\n    a, // first\n    b,\n  )\n"),
        "  g(\n    a, // first\n    b,\n  )\n",
    );
}

#[test]
fn fmt_stmt_let_use_and_assert() {
    body(
        "  let x : Int = compute(alpha)\n  let assert Ok(v) = parse(text) as \"it parses\"\n  use a , b <- result::try(load(path))\n  assert total(xs) == 10 as \"the total\"\n",
        (
            24,
            "  let x: Int =\n    compute(alpha)\n  let assert Ok(v) = parse(\n    text,\n  ) as \"it parses\"\n  use a, b <- result::try(\n    load(path),\n  )\n  assert total(xs) ==\n    10 as \"the total\"\n",
        ),
        "  let x: Int = compute(alpha)\n  let assert Ok(v) = parse(text) as \"it parses\"\n  use a, b <- result::try(load(path))\n  assert total(xs) == 10 as \"the total\"\n",
    );
}

#[test]
fn fmt_stmt_local_function_body_breaks() {
    body(
        "  fn helper(n: Int) -> Int { n + 1 }\n  helper(x)\n",
        (
            20,
            "  fn helper(\n    n: Int,\n  ) -> Int {\n    n + 1\n  }\n  helper(x)\n",
        ),
        "  fn helper(n: Int) -> Int {\n    n + 1\n  }\n  helper(x)\n",
    );
}

#[test]
fn fmt_stmt_comments_and_blank_lines() {
    // Blank lines at the start and the end of a block go, a single one
    // between statements stays, and comments keep their places.
    let src = "\n  // lead\n  let a = 1 // why\n\n\n  let b = 2\n  // tail\n\n";
    let out = "  // lead\n  let a = 1 // why\n\n  let b = 2\n  // tail\n";
    body(src, (12, out), out);
    assert_eq!(
        fmt_at("fn f() { // note\n  x\n}\n", 100),
        "fn f() { // note\n  x\n}\n"
    );
}

#[test]
fn fmt_pattern_constructors_lists_and_tuples() {
    body(
        "  let Point { x, y: _, .. } = point\n  let [head, ..tail] = items\n  let (only,) = single\n",
        (
            24,
            "  let Point {\n    x,\n    y: _,\n    ..,\n  } = point\n  let [head, ..tail] =\n    items\n  let (only,) = single\n",
        ),
        "  let Point { x, y: _, .. } = point\n  let [head, ..tail] = items\n  let (only,) = single\n",
    );
}

#[test]
fn fmt_pattern_lone_rest_gets_no_trailing_comma() {
    // Appendix A has no comma after a lone `..` in a field pattern.
    let name = "VeryLongConstructorNameForTesting";
    body(
        &format!("  let {name} {{ .. }} = value\n"),
        (30, &format!("  let {name} {{\n    ..\n  }} = value\n")),
        &format!("  let {name} {{ .. }} = value\n"),
    );
}

#[test]
fn fmt_pattern_alternatives_and_bindings() {
    let src = "  case v {\n    Some(1)|Some(-2) as hit => hit\n    Wrapper( (a, b) ) => a\n    _ => 0\n  }\n";
    body(
        src,
        (
            26,
            "  case v {\n    Some(1) | Some(\n      -2,\n    ) as hit => hit\n    Wrapper((a, b)) => a\n    _ => 0\n  }\n",
        ),
        "  case v {\n    Some(1) | Some(-2) as hit => hit\n    Wrapper((a, b)) => a\n    _ => 0\n  }\n",
    );
}

#[test]
fn fmt_type_arguments_functions_and_tuples() {
    let src = "fn f(m: map::Map< String , List<Int> >, g: fn( Int, String )->Bool, t: (Int, String), one: (Int,), d: dyn  Show) {}\n";
    assert_eq!(
        fmt_at(src, 24),
        "fn f(\n  m: map::Map<\n    String,\n    List<Int>,\n  >,\n  g: fn(\n    Int,\n    String,\n  ) -> Bool,\n  t: (Int, String),\n  one: (Int,),\n  d: dyn Show,\n) {}\n"
    );
    assert_eq!(
        fmt_at(src, 120),
        "fn f(m: map::Map<String, List<Int>>, g: fn(Int, String) -> Bool, t: (Int, String), one: (Int,), d: dyn Show) {}\n"
    );
}

#[test]
fn fmt_type_alias_value() {
    let src = "type Handler = fn(Request, map::Map<String, String>) -> Result<Response, Error>\n";
    assert_eq!(fmt_at(src, 100), src);
    // The parameters break before the return type does.
    assert_eq!(
        fmt_at(src, 60),
        "type Handler = fn(\n  Request,\n  map::Map<String, String>,\n) -> Result<Response, Error>\n"
    );
}

#[test]
fn fmt_type_return_type_stays_on_the_line_of_the_parameters() {
    let src = "fn observe(names: List<String>, meals: Int) -> Result<map::Map<String, Int>, Panic> {\n  todo\n}\n";
    assert_eq!(
        fmt_at(src, 60),
        "fn observe(\n  names: List<String>,\n  meals: Int,\n) -> Result<map::Map<String, Int>, Panic> {\n  todo\n}\n"
    );
    assert_eq!(fmt_at(src, 100), src);
}
