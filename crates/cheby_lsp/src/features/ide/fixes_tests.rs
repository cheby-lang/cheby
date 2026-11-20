//! Tests of the server's fix rows: each row's edit applied to its text,
//! and each condition under which a row offers nothing. Each runs only the
//! front end, so it runs on Windows too (D-1516).

use std::path::PathBuf;
use std::sync::Arc;

use cheby_driver::analysis::{
    Code, FileReport, Globals, Overlay, OverlayFile, PackageKind, PlainDiagnostic, Session,
    SessionConfig, TextRange, codes,
};

use super::{closest_name, provide, resolve};
use crate::features::ide::actions::{Action, ActionId, FixCx};
use crate::features::ide::hover::tests::Temp;
use crate::features::stdlib::StdService;
use crate::features::{Analyzed, CancelToken};

/// One run of a package whose `main.cheby` is the text.
struct Run {
    _temp: Temp,
    analyzed: Analyzed,
    report: Arc<FileReport>,
    path: PathBuf,
    text: String,
}

/// `text` as `src/main.cheby` of a package, run as the server runs it,
/// with the report the server would publish for it.
fn run(text: &str) -> Run {
    let temp = Temp::package("fixes", &[("main.cheby", text)]);
    let path = temp.src("main.cheby");
    let mut session = Session::new(SessionConfig {
        root: temp.0.clone(),
        kind: PackageKind::Package,
        target: None,
        globals: Arc::new(Globals::new()),
    });
    let overlay = Overlay {
        files: [(
            path.clone(),
            OverlayFile {
                text: Arc::from(text),
                version: 1,
                stamp: 1,
            },
        )]
        .into_iter()
        .collect(),
    };
    let live = session.analyze_live(&overlay).expect("a run");
    let report = live
        .report
        .as_ref()
        .and_then(|report| report.files.get(&path).cloned())
        .or_else(|| live.phase_a.get(&path).cloned())
        .expect("a report of the file");
    Run {
        analyzed: Analyzed::new(live.snapshot.expect("a snapshot")),
        report,
        path,
        text: text.to_owned(),
        _temp: temp,
    }
}

impl Run {
    /// `f` of the request's context, with the whole file as the range and
    /// every diagnostic of the report.
    fn with<T>(
        &self,
        f: impl FnOnce(&FixCx<'_>, TextRange, &[(usize, &PlainDiagnostic)]) -> T,
    ) -> T {
        let snapshot = &self.analyzed.snapshot;
        let file = snapshot.file_of(&self.path).expect("the file");
        let view = snapshot.module_at(file).expect("its module");
        let std = StdService::none();
        let cancel = CancelToken::new();
        let cx = FixCx {
            analyzed: &self.analyzed,
            view,
            report: Some(&self.report),
            substituted: false,
            context: &[],
            text: &self.text,
            std: &std,
            resolve_edits: true,
            disabled_support: true,
            cancel: &cancel,
        };
        let diagnostics: Vec<(usize, &PlainDiagnostic)> =
            self.report.diagnostics.iter().enumerate().collect();
        let end = u32::try_from(self.text.len()).expect("a small text");
        f(&cx, TextRange::new(0, end), &diagnostics)
    }

    /// The actions the rows offer.
    fn actions(&self) -> Vec<Action> {
        self.with(provide)
    }

    /// The codes of the report.
    fn codes(&self) -> Vec<Code> {
        self.report.diagnostics.iter().map(|d| d.code).collect()
    }
}

/// `text` with `action`'s edits applied.
fn applied(text: &str, action: &Action) -> String {
    let mut edits = action.edit.clone().expect("an edit at listing");
    edits.sort_by_key(|edit| std::cmp::Reverse((edit.range.start(), edit.range.end())));
    let mut out = text.to_owned();
    for edit in edits {
        let start = usize::try_from(edit.range.start()).unwrap();
        let end = usize::try_from(edit.range.end()).unwrap();
        out.replace_range(start..end, &edit.replacement);
    }
    out
}

/// Each fix offered for `text` as its title and the text it leaves, after
/// checking that the text has a diagnostic of `code`.
fn fixes(text: &str, code: Code) -> Vec<(String, String)> {
    let run = run(text);
    assert!(run.codes().contains(&code), "{code} in {:?}", run.codes());
    run.actions()
        .iter()
        .map(|action| (action.title.clone(), applied(text, action)))
        .collect()
}

/// The one fix offered for `text`, with its title.
fn one(text: &str, code: Code) -> (String, String) {
    let mut all = fixes(text, code);
    assert_eq!(all.len(), 1, "{all:?}");
    all.remove(0)
}

/// The text the fix titled `title` leaves.
fn by_title(text: &str, code: Code, title: &str) -> String {
    fixes(text, code)
        .into_iter()
        .find(|(t, _)| t == title)
        .unwrap_or_else(|| panic!("no fix `{title}` for {text}"))
        .1
}

/// `body` as the body of `main`, after a module doc.
fn in_main(body: &str) -> String {
    format!("//! P.\n\nfn main() {{\n{body}\n}}\n")
}

/// E0010 closes the string at the end of its line, only when that line
/// holds no other quote and nothing that would start an interpolation.
#[test]
fn e0010_closes_a_string_on_its_line() {
    let text = in_main("  let s = \"hello\n  assert s == s");
    let (title, fixed) = one(&text, codes::UNTERMINATED_STRING);
    assert_eq!(title, "Close the string");
    assert_eq!(fixed, in_main("  let s = \"hello\"\n  assert s == s"));
    for line in [
        "  let s = \"a\" + \"b",
        "  let s = \"hi {x",
        "  let s = \"hi\\",
    ] {
        let text = in_main(&format!("{line}\n  assert s == s"));
        assert_eq!(fixes(&text, codes::UNTERMINATED_STRING), [], "{line}");
    }
}

/// D-1755: no fix when `//` follows the opening quote on its line, where the
/// quote could land in a comment; an unclosed URL gets none either.
#[test]
fn e0010_offers_nothing_before_a_comment() {
    let say = "//! P.\n\nfn say(s: String) -> List<String> {\n  [s]\n}\n\n";
    for line in [
        "  let _ = say(\"hello) // greet",
        "  let _ = say(\"https://example.com)",
        "  let s = \"a // b",
    ] {
        let text = format!("{say}fn main() {{\n{line}\n}}\n");
        assert_eq!(fixes(&text, codes::UNTERMINATED_STRING), [], "{line}");
    }
}

/// D-1748: E0010 closes the string before the closers that end its line
/// when they balance openers before the string, innermost first, and at
/// the end of the line when they do not.
#[test]
fn e0010_closes_before_balancing_closers() {
    let say = "//! P.\n\nfn say(s: String) -> List<String> {\n  [s]\n}\n\nfn pair(a: String, b: String) -> List<String> {\n  [a, b]\n}\n\n";
    for (line, want) in [
        ("  let _ = say(\"hello)", "  let _ = say(\"hello\")"),
        ("  let _ = say(say(\"x))", "  let _ = say(say(\"x\"))"),
        ("  let _ = [say(\"a)]", "  let _ = [say(\"a\")]"),
        ("  let _ = say(\"a))", "  let _ = say(\"a)\")"),
        ("  let _ = \"a)", "  let _ = \"a)\""),
        ("  let _ = [say(\"a])", "  let _ = [say(\"a]\")"),
        // A closer that balances an opener in the string stays in it
        // (W11's review, A4).
        ("  let _ = say(\"(done)", "  let _ = say(\"(done)\""),
        ("  let _ = say(\"a (b) c)", "  let _ = say(\"a (b) c\")"),
        ("  let _ = say(\"a (b))", "  let _ = say(\"a (b)\")"),
        // An opener on an earlier line of the statement counts.
        (
            "  let _ = say(\n    \"hello)",
            "  let _ = say(\n    \"hello\")",
        ),
        // An opener that a later line closes does not (D-1748).
        (
            "  let _ = say(\n    \"hello)\n  )",
            "  let _ = say(\n    \"hello)\"\n  )",
        ),
        ("  let _ = [\n    \"a]\n  ]", "  let _ = [\n    \"a]\"\n  ]"),
        (
            "  let _ = pair(\"a\",\n    \"there)\n  )",
            "  let _ = pair(\"a\",\n    \"there)\"\n  )",
        ),
        // A later closer closes the outermost opener, so the call's own
        // `(` stays open (W11's review, G2).
        (
            "  case 1 {\n    1 => say(\"hello)\n    _ => []\n  }",
            "  case 1 {\n    1 => say(\"hello\")\n    _ => []\n  }",
        ),
        (
            "  let _ = say(\n    [say(\"a)]\n  )",
            "  let _ = say(\n    [say(\"a\")]\n  )",
        ),
    ] {
        let text = format!("{say}fn main() {{\n{line}\n}}\n");
        let fixed = by_title(&text, codes::UNTERMINATED_STRING, "Close the string");
        assert_eq!(fixed, format!("{say}fn main() {{\n{want}\n}}\n"), "{line}");
    }
}

/// E0015 inserts the closer after the last token before the place the
/// parser expected it, only when one opener is named.
#[test]
fn e0015_inserts_the_one_closer() {
    let add = "//! P.\n\nfn add(a: Int, b: Int) -> Int {\n  a + b\n}\n\n";
    let text = format!("{add}fn main() {{\n  let x = add(1, 2\n  assert x == 3\n}}\n");
    let (title, fixed) = one(&text, codes::UNCLOSED_DELIMITER);
    assert_eq!(title, "Insert `)`");
    assert_eq!(
        fixed,
        format!("{add}fn main() {{\n  let x = add(1, 2)\n  assert x == 3\n}}\n")
    );
    // A brace that may be the one missing its `}` is a second place.
    let text = "//! P.\n\nfn sign(n: Int) -> Int {\n  case n {\n    0 => 0\n    _ => 1\n}\n\nfn main() {\n  assert sign(1) == 1\n}\n";
    assert_eq!(fixes(text, codes::UNCLOSED_DELIMITER), []);
}

/// E0045 removes a whole attribute with its line, and nothing for an
/// error inside one.
#[test]
fn e0045_removes_the_attribute() {
    let item = "pub fn fast() -> Int {\n  1\n}\n";
    let text = format!("//! P.\n\n@inline\n{item}");
    let (title, fixed) = one(&text, codes::INVALID_ATTRIBUTE);
    assert_eq!(title, "Remove the attribute");
    assert_eq!(fixed, format!("//! P.\n\n{item}"));
    let text = format!("//! P.\n\n@target(native)\n@target(native)\n{item}");
    assert_eq!(
        by_title(&text, codes::INVALID_ATTRIBUTE, "Remove the attribute"),
        format!("//! P.\n\n@target(native)\n{item}")
    );
}

/// E0046 adds a body after the signature.
#[test]
fn e0046_adds_a_body() {
    let main = "\n\nfn main() {\n  assert answer() == 42\n}\n";
    let text = format!("//! P.\n\nfn answer() -> Int{main}");
    let (title, fixed) = one(&text, codes::MISSING_BODY);
    assert_eq!(title, "Add the body `{ todo }`");
    assert_eq!(
        fixed,
        format!("//! P.\n\nfn answer() -> Int {{\n  todo\n}}{main}")
    );
}

/// E0047 wraps in the constructor whose payload is the found type, and
/// offers nothing when no payload is.
#[test]
fn e0047_wraps_the_value() {
    let text = "//! P.\n\nfn first(n: Int) -> Option<Int> {\n  n\n}\n";
    let (title, fixed) = one(text, codes::TYPE_MISMATCH);
    assert_eq!(title, "Wrap in `Some(…)`");
    assert_eq!(
        fixed,
        "//! P.\n\nfn first(n: Int) -> Option<Int> {\n  Some(n)\n}\n"
    );
    let text = "//! P.\n\nfn parse(s: String) -> Result<Int, String> {\n  s\n}\n";
    assert_eq!(
        one(text, codes::TYPE_MISMATCH),
        (
            "Wrap in `Err(…)`".to_owned(),
            "//! P.\n\nfn parse(s: String) -> Result<Int, String> {\n  Err(s)\n}\n".to_owned()
        )
    );
    let text = "//! P.\n\nfn same(n: Int) -> Result<Int, Int> {\n  n\n}\n";
    let titles: Vec<String> = fixes(text, codes::TYPE_MISMATCH)
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    assert_eq!(titles, ["Wrap in `Ok(…)`", "Wrap in `Err(…)`"]);
    let text = "//! P.\n\nfn first(s: String) -> Option<Int> {\n  s\n}\n";
    assert_eq!(fixes(text, codes::TYPE_MISMATCH), []);
    // A module whose own `Some` hides the prelude's gets no wrap (D-1678).
    let text =
        "//! P.\n\ntype Maybe {\n  Some(Int),\n}\n\nfn first(n: Int) -> Option<Int> {\n  n\n}\n";
    assert_eq!(fixes(text, codes::TYPE_MISMATCH), []);
}

/// E0048 adds `todo` for the missing arguments or removes the extra ones.
#[test]
fn e0048_fits_the_arguments() {
    let add = "//! P.\n\nfn add(a: Int, b: Int) -> Int {\n  a + b\n}\n\n";
    let call = |args: &str| format!("{add}fn main() {{\n  assert add({args}) == 3\n}}\n");
    assert_eq!(
        one(&call("1"), codes::WRONG_ARGUMENT_COUNT),
        (
            "Add the missing arguments as `todo`".to_owned(),
            call("1, todo")
        )
    );
    assert_eq!(
        one(&call(""), codes::WRONG_ARGUMENT_COUNT),
        (
            "Add the missing arguments as `todo`".to_owned(),
            call("todo, todo")
        )
    );
    assert_eq!(
        one(&call("1, 2, 3"), codes::WRONG_ARGUMENT_COUNT),
        ("Remove the extra arguments".to_owned(), call("1, 2"))
    );
    // A pattern with the wrong count is no call.
    let text = "//! P.\n\ntype Pair {\n  Pair(Int, Int),\n}\n\nfn first(pair: Pair) -> Int {\n  let Pair(a) = pair\n  a\n}\n";
    assert_eq!(fixes(text, codes::WRONG_ARGUMENT_COUNT), []);
}

/// E0052 writes `dyn` before an interface used as a type, and nothing
/// after `dyn`.
#[test]
fn e0052_writes_dyn() {
    let iface = "//! P.\n\npub interface Describe {\n  fn describe(Self) -> String\n}\n\n";
    let text = format!(
        "{iface}pub fn label(item: Describe) -> String {{\n  Describe::describe(item)\n}}\n"
    );
    let (title, fixed) = one(&text, codes::NOT_A_TYPE);
    assert_eq!(title, "Write `dyn Describe`");
    assert_eq!(fixed, text.replace("item: Describe", "item: dyn Describe"));
    let text = "//! P.\n\npub type Point {\n  x: Int,\n  y: Int,\n}\n\npub fn boxed(point: Point) -> dyn Point {\n  point\n}\n";
    assert_eq!(fixes(text, codes::NOT_A_TYPE), []);
}

/// E0058 writes the closest field, at a step of a field path too
/// (D-1694), and nothing when no field is close.
#[test]
fn e0058_uses_the_closest_field() {
    let types = "//! P.\n\ntype Point {\n  x: Int,\n  y: Int,\n}\n\ntype Shape {\n  origin: Point,\n  size: Int,\n}\n\n";
    let read = |field: &str| format!("{types}fn depth(s: Shape) -> Int {{\n  s.{field}\n}}\n");
    assert_eq!(
        one(&read("sizz"), codes::UNKNOWN_FIELD),
        ("Use the field `size`".to_owned(), read("size"))
    );
    assert_eq!(fixes(&read("weight"), codes::UNKNOWN_FIELD), []);
    let update = |path: &str| {
        format!("{types}fn moved(s: Shape) -> Shape {{\n  Shape {{ ..s, {path}: 1 }}\n}}\n")
    };
    assert_eq!(
        one(&update("origni.x"), codes::UNKNOWN_FIELD),
        ("Use the field `origin`".to_owned(), update("origin.x"))
    );
    // A field that one variant has and another lacks is no typo.
    let text = "//! P.\n\ntype Shape {\n  Circle { radius: Int },\n  Square { side: Int },\n}\n\nfn size(s: Shape) -> Int {\n  s.side\n}\n";
    assert_eq!(fixes(text, codes::UNKNOWN_FIELD), []);
}

/// E0059 adds the missing fields as `todo`, or removes a field written
/// twice, and nothing for a field path.
#[test]
fn e0059_fits_the_fields() {
    let point = "//! P.\n\ntype Point {\n  x: Int,\n  y: Int,\n}\n\n";
    let make =
        |fields: &str| format!("{point}fn origin() -> Point {{\n  Point {{ {fields} }}\n}}\n");
    assert_eq!(
        one(&make("x: 0"), codes::INVALID_RECORD),
        (
            "Add the missing fields as `todo`".to_owned(),
            make("x: 0, y: todo")
        )
    );
    assert_eq!(
        one(&make("x: 0, y: 1, x: 2"), codes::INVALID_RECORD),
        ("Remove the field `x`".to_owned(), make("x: 0, y: 1"))
    );
    let shape = "type Shape {\n  origin: Point,\n  size: Int,\n}\n\n";
    let text = format!(
        "{point}{shape}fn make() -> Shape {{\n  Shape {{ origin.x: 1, origin.y: 2, size: 3 }}\n}}\n"
    );
    assert_eq!(fixes(&text, codes::INVALID_RECORD), []);
}

/// E0060 offers `let assert` and `let guard … else { todo }` on a `let`.
#[test]
fn e0060_makes_the_let_refutable() {
    let head = "//! P.\n\nfn get(o: Option<Int>) -> Int {\n";
    let text = format!("{head}  let Some(x) = o\n  x\n}}\n");
    let all = fixes(&text, codes::REFUTABLE_PATTERN);
    assert_eq!(
        all,
        [
            (
                "Use `let assert`".to_owned(),
                format!("{head}  let assert Some(x) = o\n  x\n}}\n")
            ),
            (
                "Use `let guard … else { todo }`".to_owned(),
                format!("{head}  let guard Some(x) = o else {{\n    todo\n  }}\n  x\n}}\n")
            ),
        ]
    );
}

/// E0065 bounds the type parameter by the interface its leaf requires.
#[test]
fn e0065_adds_the_bound() {
    let text = "//! P.\n\ninterface Any\n\nfn wrap<T>(xs: List<T>) -> dyn Any {\n  xs\n}\n";
    let (title, fixed) = one(text, codes::DYN_NEEDS_BOUND);
    assert_eq!(title, "Add bound `T: Any`");
    assert_eq!(fixed, text.replace("wrap<T>", "wrap<T: Any>"));
}

/// E0067 restricts the using item, or the module, to the one target the
/// used item exists on, and offers no second `@target`.
#[test]
fn e0067_restricts_the_target() {
    let js = "//! P.\n\n@target(js)\n@external(js, \"./m.mjs\", \"now\")\nfn js_now() -> Int\n\n";
    let text = format!("{js}fn main() {{\n  let _ = js_now()\n}}\n");
    assert_eq!(
        fixes(&text, codes::TARGET_MISMATCH),
        [
            (
                "Add `@target(js)`".to_owned(),
                format!("{js}@target(js)\nfn main() {{\n  let _ = js_now()\n}}\n")
            ),
            (
                "Add `@!target(js)`".to_owned(),
                text.replacen("//! P.\n", "//! P.\n@!target(js)\n", 1)
            ),
        ]
    );
}

/// E0067 offers no second `@target` on an item that has one.
#[test]
fn e0067_keeps_one_target_attribute() {
    let js = "//! P.\n\n@target(js)\n@external(js, \"./m.mjs\", \"now\")\nfn js_now() -> Int\n\n";
    let text = format!("{js}@target(native)\nfn main() {{\n  let _ = js_now()\n}}\n");
    let titles: Vec<String> = fixes(&text, codes::TARGET_MISMATCH)
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    assert_eq!(titles, ["Add `@!target(js)`"]);
}

/// W0010 removes an arm with the comment after it, and no arm that shares
/// its line with other code.
#[test]
fn w0010_removes_only_whole_lines() {
    let head = "//! P.\n\nfn sign(n: Int) -> Int {\n  case n {\n    _ => 0\n";
    let main = "\n\nfn main() {\n  assert sign(1) == 0\n}\n";
    let text = format!("{head}    1 => 1 // one\n  }}\n}}{main}");
    assert_eq!(
        by_title(&text, codes::UNREACHABLE_ARM, "Remove the arm"),
        format!("{head}  }}\n}}{main}")
    );
    let text = format!("{head}    1 => 1 }}\n}}{main}");
    assert_eq!(fixes(&text, codes::UNREACHABLE_ARM), []);
}

/// E0045 on a malformed attribute removes it whole, as its span is.
#[test]
fn e0045_removes_a_malformed_attribute() {
    let item = "pub fn fast() -> Int {\n  1\n}\n";
    let text = format!("//! P.\n\n@target(wasm)\n{item}");
    assert_eq!(
        one(&text, codes::INVALID_ATTRIBUTE),
        (
            "Remove the attribute".to_owned(),
            format!("//! P.\n\n{item}")
        )
    );
}

/// W0004 and W0005 remove the item with its doc comment and a blank line,
/// as a removal (D-1360).
#[test]
fn unused_items_are_removed() {
    let main = "//! P.\n\nfn main() {\n  assert 1 == 1\n}\n";
    let text = format!("{main}\n/// Doc.\npriv fn helper() -> Int {{\n  1\n}}\n");
    let run = run(&text);
    let actions = run.actions();
    let [action] = actions.as_slice() else {
        panic!("{actions:?}");
    };
    assert_eq!(action.title, "Remove the unused `helper`");
    assert!(action.removes);
    assert_eq!(applied(&text, action), main);
    let text = format!(
        "//! P.\n\nfn helper() -> Int {{\n  1\n}}\n\n{}",
        main.replace("//! P.\n\n", "")
    );
    let (title, fixed) = one(&text, codes::UNUSED_PACKAGE_ITEM);
    assert_eq!(title, "Remove the unused `helper`");
    assert_eq!(fixed, main);
}

/// W0006 discards the value with `let _ =`, not at a block's end.
#[test]
fn w0006_discards_the_value() {
    let parse = "//! P.\n\nfn parse(_s: String) -> Result<Int, String> {\n  Ok(1)\n}\n\n";
    let text = format!("{parse}fn main() {{\n  parse(\"1\")\n  assert 1 == 1\n}}\n");
    assert_eq!(
        one(&text, codes::DISCARDED_RESULT),
        (
            "Discard the value with `let _ =`".to_owned(),
            text.replace("  parse(", "  let _ = parse(")
        )
    );
}

/// W0008 writes `let p` for a `let assert` and drops the `else` of a
/// `let guard`.
#[test]
fn w0008_writes_a_plain_let() {
    let text = in_main("  let assert (a, b) = (1, 2)\n  assert a < b");
    assert_eq!(
        one(&text, codes::IRREFUTABLE_LET_ASSERT),
        (
            "Write `let (a, b)`".to_owned(),
            in_main("  let (a, b) = (1, 2)\n  assert a < b")
        )
    );
    let text = in_main("  let guard (a, b) = (1, 2) else {\n    panic\n  }\n  assert a < b");
    assert_eq!(
        one(&text, codes::IRREFUTABLE_LET_ASSERT),
        (
            "Drop the `else`".to_owned(),
            in_main("  let (a, b) = (1, 2)\n  assert a < b")
        )
    );
}

/// W0010 removes the arm whose pattern is the span (D-1687), with its line.
#[test]
fn w0010_removes_the_arm() {
    let arms = |rest: &str| {
        format!(
            "//! P.\n\nfn sign(n: Int) -> Int {{\n  case n {{\n    _ => 0\n{rest}  }}\n}}\n\nfn main() {{\n  assert sign(1) == 0\n}}\n"
        )
    };
    assert_eq!(
        one(&arms("    1 => 1\n"), codes::UNREACHABLE_ARM),
        ("Remove the arm".to_owned(), arms(""))
    );
}

/// A resolve of a row's id gives the edits the listing gave.
#[test]
fn a_resolve_gives_the_listed_edits() {
    let text = "//! P.\n\nfn first(n: Int) -> Option<Int> {\n  n\n}\n";
    let run = run(text);
    let listed = run.actions();
    let [action] = listed.as_slice() else {
        panic!("{listed:?}");
    };
    let id = ActionId::Fix {
        code: codes::TYPE_MISMATCH,
        index: 0,
    };
    let resolved = run
        .with(|cx, range, _| resolve(cx, range, &id))
        .expect("the edits");
    assert_eq!(Some(resolved), action.edit);
    let gone = ActionId::Fix {
        code: codes::TYPE_MISMATCH,
        index: 1,
    };
    assert!(run.with(|cx, range, _| resolve(cx, range, &gone)).is_err());
    let other = ActionId::FillArms;
    assert!(run.with(|cx, range, _| resolve(cx, range, &other)).is_err());
}

/// The did-you-mean of field names: within a third of the length, ties
/// to the first.
#[test]
fn closest_names() {
    assert_eq!(closest_name("sizz", ["size", "origin"]), Some("size"));
    assert_eq!(closest_name("weight", ["size", "origin"]), None);
    assert_eq!(closest_name("ab", ["ac", "ad"]), Some("ac"));
    assert_eq!(closest_name("x", ["x"]), None);
}
