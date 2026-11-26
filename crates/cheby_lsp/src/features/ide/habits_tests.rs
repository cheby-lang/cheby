//! Tests of the habit rewrites (X §6.5, plan §1.11): each row on the
//! tested inputs, whose output parses with no grammar error, and the
//! negatives. Each runs only the front end, so it runs on Windows too
//! (D-1516).

use std::path::PathBuf;
use std::sync::Arc;

use cheby_driver::analysis::codes::Code;
use cheby_driver::analysis::{
    FileReport, Globals, Overlay, OverlayFile, PackageKind, PlainDiagnostic, PlainEdit, Session,
    SessionConfig, TextRange, TextSize,
};

use super::{DROPS_A_COMMENT, check_rewrite, provide, resolve};
use crate::features::ide::actions::{Action, ActionId, DiagRef, FixCx};
use crate::features::ide::hover::tests::Temp;
use crate::features::ide::rename::RenameError;
use crate::features::stdlib::StdService;
use crate::features::{Analyzed, CancelToken};

/// One package's run, with the report of `src/main.cheby`: the check's, or
/// phase A's when the file has a grammar error.
pub(crate) struct Case {
    _temp: Temp,
    analyzed: Analyzed,
    path: PathBuf,
    report: Arc<FileReport>,
}

/// The package `app` with these files under `src/`, all open.
pub(crate) fn case(name: &str, files: &[(&str, &str)]) -> Case {
    let temp = Temp::package(name, files);
    let mut session = Session::new(SessionConfig {
        root: temp.0.clone(),
        kind: PackageKind::Package,
        target: None,
        globals: Arc::new(Globals::new()),
    });
    let overlay = Overlay {
        files: files
            .iter()
            .map(|(file, text)| {
                let open = OverlayFile {
                    text: Arc::from(*text),
                    version: 1,
                    stamp: 1,
                };
                (temp.src(file), open)
            })
            .collect(),
    };
    let live = session.analyze_live(&overlay).unwrap();
    let path = temp.src("main.cheby");
    let report = live
        .report
        .as_ref()
        .and_then(|report| report.files.get(&path))
        .or_else(|| live.phase_a.get(&path))
        .map(Arc::clone)
        .expect("a report of main.cheby");
    Case {
        _temp: temp,
        analyzed: Analyzed::new(live.snapshot.unwrap()),
        path,
        report,
    }
}

impl Case {
    /// `f` with the `FixCx` of `main.cheby`, whose request carried
    /// `context`.
    pub(crate) fn with<R>(
        &self,
        context: &[(Code, TextRange)],
        f: impl FnOnce(&FixCx<'_>) -> R,
    ) -> R {
        self.with_client(context, true, f)
    }

    /// [`Case::with`] for a client that resolves edits, or not.
    fn with_client<R>(
        &self,
        context: &[(Code, TextRange)],
        resolve_edits: bool,
        f: impl FnOnce(&FixCx<'_>) -> R,
    ) -> R {
        let snapshot = &self.analyzed.snapshot;
        let file = snapshot.file_of(&self.path).unwrap();
        let std = StdService::none();
        let cancel = CancelToken::new();
        let cx = FixCx {
            analyzed: &self.analyzed,
            view: snapshot.module_at(file).unwrap(),
            report: Some(&self.report),
            substituted: false,
            context,
            text: snapshot.source(file).unwrap().text(),
            std: &std,
            resolve_edits,
            disabled_support: true,
            cancel: &cancel,
        };
        f(&cx)
    }

    /// The report's diagnostics that touch `range`, with their indexes.
    fn touching(&self, range: TextRange) -> Vec<(usize, &PlainDiagnostic)> {
        self.report
            .diagnostics
            .iter()
            .enumerate()
            .filter(|(_, d)| {
                d.range
                    .is_some_and(|at| at.start() <= range.end() && range.start() <= at.end())
            })
            .collect()
    }

    /// The habit rewrites at the start of `needle` in `main.cheby`.
    fn actions(&self, needle: &str) -> Vec<Action> {
        let range = TextRange::empty(offset(&self.report.text, needle));
        let touching = self.touching(range);
        self.with(&[], |cx| provide(cx, range, &touching))
    }

    /// `action`'s edits: those of the listing, or a resolve's at its
    /// diagnostic for one that carries `data` (D-1753).
    fn edits(&self, action: &Action) -> Vec<PlainEdit> {
        if let Some(edits) = &action.edit {
            return edits.clone();
        }
        let id = action.data.as_ref().expect("edits at listing or data");
        let Some(DiagRef::Report(index)) = action.diagnostic else {
            panic!("a rewrite of a reported diagnostic");
        };
        let at = self.report.diagnostics[index]
            .range
            .expect("a placed diagnostic");
        self.with(&[], |cx| resolve(cx, TextRange::empty(at.start()), id))
            .expect("a resolve")
    }

    /// The text of `main.cheby` with `action`'s edits applied.
    fn applied(&self, action: &Action) -> String {
        let mut edits = self.edits(action);
        assert!(edits.iter().all(|edit| edit.path == self.path));
        edits.sort_by_key(|edit| edit.range.start());
        let text = &self.report.text;
        let mut out = String::new();
        let mut at = 0_usize;
        for edit in edits {
            let start = usize::try_from(edit.range.start()).unwrap();
            out.push_str(text.get(at..start).unwrap());
            out.push_str(&edit.replacement);
            at = usize::try_from(edit.range.end()).unwrap();
        }
        out.push_str(text.get(at..).unwrap());
        out
    }
}

/// The offset of `needle`, which occurs once in `text`.
fn offset(text: &str, needle: &str) -> TextSize {
    assert_eq!(text.matches(needle).count(), 1, "`{needle}` occurs once");
    TextSize::try_from(text.find(needle).unwrap()).unwrap()
}

/// `f` with the `FixCx` of a clean one-file package, whose request
/// carried `context`.
pub(crate) fn fix_cx_for<R>(
    name: &str,
    context: &[(Code, TextRange)],
    f: impl FnOnce(&FixCx<'_>) -> R,
) -> R {
    case(
        name,
        &[("main.cheby", "//! Root.\n\nfn main() {\n  Nil\n}\n")],
    )
    .with(context, f)
}

impl Case {
    /// The grammar errors of `text`, parsed as `main.cheby`.
    fn errors(&self, text: &str) -> usize {
        let file = self.analyzed.snapshot.file_of(&self.path).unwrap();
        cheby_syntax::parse(file, Arc::from(text)).errors().len()
    }
}

/// The one rewrite at `needle`, which must have `title`, and the text it
/// gives, which must have one grammar error fewer than the case's for a
/// rewrite of one, and none more for any other.
fn only(case: &Case, needle: &str, title: &str) -> String {
    let actions = case.actions(needle);
    let titles: Vec<&str> = actions.iter().map(|a| a.title.as_str()).collect();
    assert_eq!(titles, [title]);
    let action = &actions[0];
    assert!(action.disabled.is_none());
    assert!(matches!(action.diagnostic, Some(DiagRef::Report(_))));
    let text = case.applied(action);
    let before = case.errors(&case.report.text);
    let grammar = case
        .touching(TextRange::empty(offset(&case.report.text, needle)))
        .iter()
        .any(|(_, d)| d.code == cheby_driver::analysis::codes::SYNTAX_ERROR);
    let wanted = if grammar {
        before.checked_sub(1)
    } else {
        Some(before)
    };
    assert_eq!(Some(case.errors(&text)), wanted, "{text}");
    text
}

const IF_FORM: &str = "//! Ifs.

fn pick(flag: Bool, a: Int) -> Int {
  let n = if flag { 1 } else if a > 2 { a + 1 } else { 3 }
  n
}

fn side(flag: Bool) -> Nil {
  if flag { Nil }
}
";

/// `if` chains become a subjectless `case`, one arm per branch on its
/// own line at the anchor line's indentation, and an `if` with no `else`
/// gets `_ => Nil`.
#[test]
fn an_if_chain_becomes_case() {
    let case = case("if_chain", &[("main.cheby", IF_FORM)]);
    assert!(!case.actions("flag { 1 }")[0].preferred, "D-1737");
    let text = only(&case, "flag { 1 }", "Rewrite as `case`");
    assert!(
        text.contains(
            "  let n = case {\n    flag => 1\n    a > 2 => a + 1\n    _ => 3\n  }\n  n\n"
        )
    );
    let text = only(&case, "flag { Nil }", "Rewrite as `case`");
    assert!(text.contains("  case {\n    flag => Nil\n    _ => Nil\n  }\n"));
}

/// A block of several statements, or over several lines, is kept as a
/// block, its lines moved two spaces deeper.
#[test]
fn a_block_of_statements_stays_a_block() {
    let text = "//! Ifs.

fn pick(flag: Bool, a: Int) -> Int {
  let n = if flag {
    let b = a + 1
    b
  } else { 2 }
  n
}
";
    let case = case("if_block", &[("main.cheby", text)]);
    let out = only(&case, "flag {", "Rewrite as `case`");
    assert!(out.contains(
        "  let n = case {\n    flag => {\n      let b = a + 1\n      b\n    }\n    _ => 2\n  }\n"
    ), "{out}");
}

const RETURNS: &str = "//! Returns.

fn early(a: Int) -> Int {
  if a > 0 { return 1 }
  a
}

fn last(a: Int) -> Int {
  return a + 1
}

fn nested(a: Int) -> Int {
  case a {
    0 => {
      if a > 0 { return 1 }
      a
    }
    _ => a
  }
}

fn middle(a: Int) -> Int {
  return a
  a
}

fn closure(a: Int) -> Int {
  let f = fn(x: Int) -> Int {
    return x
  }
  f(a)
}
";

/// `if c { return x }` in a function's body block becomes a `guard` with
/// the condition negated as `!( )`.
#[test]
fn an_early_return_becomes_guard() {
    let case = case("guard", &[("main.cheby", RETURNS)]);
    assert!(
        !case.actions("a > 0 { return 1 }\n  a\n}\n\nfn last")[0].preferred,
        "D-1737"
    );
    let text = only(
        &case,
        "a > 0 { return 1 }\n  a\n}\n\nfn last",
        "Rewrite as `guard`",
    );
    assert!(
        text.contains("fn early(a: Int) -> Int {\n  guard !(a > 0) else {\n    1\n  }\n  a\n}")
    );
}

/// `guard_block`: `if … return` in a block that is not a body block is
/// left alone, and so is a `return` that is not last (plan §1.11).
#[test]
fn guard_block() {
    let case = case("guard_block", &[("main.cheby", RETURNS)]);
    assert_eq!(case.actions("a > 0 { return 1 }\n      a"), Vec::new());
    assert_eq!(case.actions("a\n  a\n}\n\nfn closure"), Vec::new());
}

/// A last `return` of a function's or a closure's body block is removed.
#[test]
fn a_last_return_is_removed() {
    let case = case("return_last", &[("main.cheby", RETURNS)]);
    assert!(!case.actions("a + 1\n}")[0].preferred, "D-1737");
    let text = only(&case, "a + 1\n}", "Remove the `return`");
    assert!(text.contains("fn last(a: Int) -> Int {\n  a + 1\n}"));
    let text = only(&case, "x\n  }", "Remove the `return`");
    assert!(text.contains("  let f = fn(x: Int) -> Int {\n    x\n  }"));
}

const LOCAL_RETURNS: &str = "//! Local returns.

fn outer(a: Int) -> Int {
  fn early(b: Int) -> Int {
    if b > 0 { return 1 }
    b
  }
  fn last(b: Int) -> Int {
    return b + 2
  }
  early(a) + last(a)
}
";

/// D-1746: a local function's body block is a function's body block,
/// so `if … return` there becomes a `guard` and its last `return` goes.
#[test]
fn a_local_function_s_returns_are_rewritten() {
    let case = case("local_returns", &[("main.cheby", LOCAL_RETURNS)]);
    let text = only(&case, "b > 0 { return 1 }", "Rewrite as `guard`");
    assert!(
        text.contains(
            "  fn early(b: Int) -> Int {\n    guard !(b > 0) else {\n      1\n    }\n    b\n  }"
        ),
        "{text}"
    );
    let text = only(&case, "b + 2", "Remove the `return`");
    assert!(
        text.contains("  fn last(b: Int) -> Int {\n    b + 2\n  }"),
        "{text}"
    );
}

const FOR_FORM: &str = "//! Loops.

import app::other

fn show(xs: List<Int>) -> Nil {
  for x in xs { other::put(x) }
}
";

const OTHER: &str = "//! Uses lists.

import std::list

pub fn put(x: Int) -> Nil {
  let _ = list::length([x])
  Nil
}
";

/// `for` becomes `list::each` with a closure, and the import of
/// `std::list` is added where the file has none.
#[test]
fn a_for_loop_becomes_each() {
    let case = case(
        "for_each",
        &[("main.cheby", FOR_FORM), ("other.cheby", OTHER)],
    );
    assert!(!case.actions("x in xs")[0].preferred, "D-1737");
    let text = only(&case, "x in xs", "Rewrite as `list::each`");
    assert!(
        text.contains("  list::each(xs, fn(x) { other::put(x) })\n"),
        "{text}"
    );
    assert!(
        text.contains("import app::other\nimport std::list\n"),
        "{text}"
    );
}

/// D-1753: the `for` rewrite comes on resolve, checked through `verify`. A
/// body whose value is `Nil` resolves, even beside a type error elsewhere
/// that the rewrite reveals; one whose value is not, which `list::each`
/// refuses, is refused as a fix is (D-1731); and a client that cannot
/// resolve edits is not offered it.
#[test]
fn the_for_rewrite_is_checked_on_resolve() {
    const NOT_NIL: &str =
        "//! Loops.\n\nfn count(xs: List<Int>) -> Nil {\n  for x in xs { x + 1 }\n}\n";
    let text = format!("{FOR_FORM}\nfn answer() -> Int {{\n  \"forty-two\"\n}}\n");
    let nil = case("for_nil", &[("main.cheby", &text), ("other.cheby", OTHER)]);
    let actions = nil.actions("x in xs");
    assert_eq!(titles(&actions), ["Rewrite as `list::each`"]);
    assert_eq!(actions[0].edit, None);
    assert_eq!(actions[0].data, Some(ActionId::Habit { index: 0 }));
    let edits = nil.edits(&actions[0]);
    if let Err(error) = check_rewrite(&nil.analyzed, &edits) {
        panic!("a `Nil` body is refused: {error:?}");
    }
    let range = TextRange::empty(offset(&nil.report.text, "x in xs"));
    let touching = nil.touching(range);
    let offered = nil.with_client(&[], false, |cx| provide(cx, range, &touching));
    assert_eq!(offered, Vec::new());
    let not_nil = case("for_not_nil", &[("main.cheby", NOT_NIL)]);
    let actions = not_nil.actions("x in xs");
    assert_eq!(titles(&actions), ["Rewrite as `list::each`"]);
    let edits = not_nil.edits(&actions[0]);
    let Err(RenameError::Refused(refusal)) = check_rewrite(&not_nil.analyzed, &edits) else {
        panic!("a body that is not `Nil` resolves");
    };
    assert!(
        refusal
            .0
            .starts_with("Applying the fix would cause: error[E0047]"),
        "{}",
        refusal.0
    );
    // An old error of the same message below the loop, in a file whose
    // added import moves every line, does not stand in for the closure's
    // (W11's review, G1).
    let below = format!("{NOT_NIL}\nfn other() -> Int {{\n  \"s\"\n}}\n");
    let below = case("for_not_nil_below", &[("main.cheby", &below)]);
    let actions = below.actions("x in xs");
    assert_eq!(titles(&actions), ["Rewrite as `list::each`"]);
    let edits = below.edits(&actions[0]);
    let Err(RenameError::Refused(refusal)) = check_rewrite(&below.analyzed, &edits) else {
        panic!("an old error below the loop lets a body that is not `Nil` resolve");
    };
    assert!(
        refusal
            .0
            .starts_with("Applying the fix would cause: error[E0047]"),
        "{}",
        refusal.0
    );
}

/// `let mut`, `else =>` and the lower-case type names.
#[test]
fn small_habits() {
    let text = "//! Small.

fn f(a: int) -> string {
  let mut y = a
  case y {
    1 => \"one\"
    else => \"many\"
  }
}
";
    let case = case("small", &[("main.cheby", text)]);
    let out = case.applied(&case.actions("y = a")[0]);
    assert!(out.contains("  let y = a\n"));
    assert_eq!(case.actions("y = a")[0].title, "Remove `mut`");
    let out = case.applied(&case.actions("else =>")[0]);
    assert!(out.contains("    _ => \"many\"\n"));
    assert_eq!(case.actions("else =>")[0].title, "Write `_`");
    // A type word is its help's fix, not a row here (D-1931).
    assert_eq!(titles(&case.actions("int)")), Vec::<&str>::new());
    assert_eq!(titles(&case.actions("string {")), Vec::<&str>::new());
    // D-1737: no row of E0013 is preferred.
    for needle in ["y = a", "else =>"] {
        assert!(!case.actions(needle)[0].preferred, "{needle}");
    }
}

/// `null`, `nil` and `none` (P-4). `true` and `false` are their help's fix,
/// and have no row here (D-1931).
#[test]
fn literal_words_of_other_languages() {
    let text = "//! Words.

fn a() -> Bool {
  true
}

fn b() -> Bool {
  false
}

fn c() -> Option<Int> {
  null
}

fn d() -> Nil {
  nil
}

fn e() -> Option<Int> {
  none
}
";
    let case = case("words", &[("main.cheby", text)]);
    for (needle, title, written) in [
        ("null", "Write `None`", "fn c() -> Option<Int> {\n  None\n"),
        ("nil", "Write `Nil`", "fn d() -> Nil {\n  Nil\n"),
        ("none", "Write `None`", "fn e() -> Option<Int> {\n  None\n"),
    ] {
        let actions = case.actions(&format!("{needle}\n"));
        assert_eq!(actions.len(), 1, "{needle}");
        assert_eq!(actions[0].title, title);
        assert!(actions[0].preferred);
        assert!(case.applied(&actions[0]).contains(written), "{needle}");
    }
    for needle in ["true", "false"] {
        assert_eq!(
            titles(&case.actions(&format!("{needle}\n"))),
            Vec::<&str>::new(),
            "{needle}"
        );
    }
}

/// E0044: the second binding and its later uses are renamed to a free
/// name, `y_2` unless it is taken, and nothing else (D-1668).
#[test]
fn a_shadowed_binding_is_renamed() {
    let text = "//! Shadows.

fn f(a: Int) -> Int {
  let y = a
  let y = y + 1
  y * 2
}

fn g(a: Int) -> Int {
  let y_2 = a
  let y = y_2
  let y = y + 1
  y
}
";
    let case = case("shadowed", &[("main.cheby", text)]);
    let actions = case.actions("y = y + 1\n  y * 2");
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].title, "Rename `y` to `y_2`");
    assert!(!actions[0].preferred);
    let out = case.applied(&actions[0]);
    assert!(
        out.contains("  let y = a\n  let y_2 = y + 1\n  y_2 * 2\n"),
        "{out}"
    );
    let actions = case.actions("y = y + 1\n  y\n");
    assert_eq!(actions[0].title, "Rename `y` to `y_2_value`");
}

const METHODS: &str = "//! Methods.

import std::list

fn f(xs: List<Int>) -> List<Int> {
  xs.map(fn(x) { x + 1 })
}

fn g(xs: List<Int>) -> Int {
  xs.length() + 1
}

fn h(xs: List<Int>) -> Int {
  xs.length(1)
}

fn k(xs: List<Int>) -> Int {
  xs.nothing()
}
";

/// E0058: a call of a field that is a function of the receiver's home
/// module becomes a pipe (X §6.5), and the call form as an operand of `+`
/// (D-1749); a wrong argument count or an unknown name gives nothing.
#[test]
fn a_method_call_becomes_a_pipe() {
    let case = case("methods", &[("main.cheby", METHODS)]);
    let text = only(
        &case,
        "map(fn",
        "Write `xs |> list::map(_, fn(x) { x + 1 })`",
    );
    assert!(text.contains("  xs |> list::map(_, fn(x) { x + 1 })\n"));
    let text = only(&case, "length() + 1", "Write `list::length(xs)`");
    assert!(text.contains("  list::length(xs) + 1\n"), "{text}");
    assert_eq!(case.actions("length(1)"), Vec::new());
    assert_eq!(case.actions("nothing()"), Vec::new());
    // Outside an operand in a file that does not import the home module:
    // the pipe, and the import (W11's review, A8).
    let bare = "//! Methods.\n\nfn g(xs: List<Int>) -> Int {\n  let n = xs.length()\n  n\n}\n";
    let bare_case = super::tests::case("methods_bare", &[("main.cheby", bare)]);
    let text = only(&bare_case, "length()", "Write `xs |> list::length`");
    assert!(text.contains("import std::list\n"), "{text}");
    assert!(text.contains("  let n = xs |> list::length\n"), "{text}");
}

/// A comment the rewrite would drop disables it (plan §1.11); one inside
/// a block keeps the block, and the rewrite is offered.
#[test]
fn a_comment_in_every_gap() {
    let text = "//! Comments.

fn pick(flag: Bool) -> Int {
  let n = if flag {
    // one
    1
  } // between
  else { 2 }
  n
}

fn kept(flag: Bool) -> Int {
  let n = if flag { 1 // one
  } else { 2 }
  n
}
";
    let case = case("comments", &[("main.cheby", text)]);
    let actions = case.actions("flag {\n    // one");
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].disabled.as_deref(), Some(DROPS_A_COMMENT));
    assert!(!actions[0].preferred);
    let out = only(&case, "flag { 1 // one", "Rewrite as `case`");
    assert!(out.contains("    flag => { 1 // one\n    }\n"), "{out}");
}

/// Every gap that a rewrite drops: after `else` before `if` or `{`, and
/// between a guard's `{` and its `return`.
#[test]
fn every_dropped_gap_disables() {
    let text = "//! Gaps.

fn a(flag: Bool, b: Bool) -> Int {
  let n = if flag { 1 } else // one
  if b { 2 } else { 3 }
  n
}

fn c(flag: Bool) -> Int {
  let n = if flag { 1 } else // two
  { 3 }
  n
}

fn d(x: Int) -> Int {
  if x > 0 { // three
    return 1 }
  x
}
";
    let case = case("gaps", &[("main.cheby", text)]);
    for needle in ["flag { 1 } else // one", "flag { 1 } else // two", "x > 0"] {
        let actions = case.actions(needle);
        assert_eq!(actions.len(), 1, "{needle}");
        assert_eq!(
            actions[0].disabled.as_deref(),
            Some(DROPS_A_COMMENT),
            "{needle}"
        );
    }
}

/// One comment in each other gap of the multi-token rewrites: after `if`,
/// between the condition and `{`, inside the `else` block, and each gap
/// of `for … in … {`, of a last `return x` and of `xs.map(f)`. Each
/// rewrite is offered `disabled`, or not at all, or keeps every comment
/// and leaves no grammar error (T-1). A comment between the condition and
/// `{` once landed inside the arm's condition, so the arm's `=> 1` joined
/// the comment.
#[test]
fn a_comment_in_each_other_gap() {
    let cases = [
        (
            "after_if",
            "  let n = if // c\n    flag { 1 } else { 2 }\n  n\n",
            "c\n    flag",
        ),
        (
            "before_brace",
            "  let n = if flag // c\n  { 1 } else { 2 }\n  n\n",
            "flag // c",
        ),
        (
            "else_block",
            "  let n = if flag { 1 } else {\n    // c\n    2\n  }\n  n\n",
            "flag { 1 }",
        ),
        (
            "for_pattern",
            "  for x // c\n  in [1] { x }\n  0\n",
            "x // c",
        ),
        ("for_list", "  for x in [1] // c\n  { x }\n  0\n", "x in"),
        (
            "for_block",
            "  for x in [1] {\n    // c\n    x\n  }\n  0\n",
            "x in",
        ),
        ("return_value", "  return // c\n    1\n", "// c"),
        (
            "method",
            "  let xs = [1]\n  xs // c\n    .map(fn(x) { x })\n  0\n",
            "map",
        ),
    ];
    for (name, body, needle) in cases {
        let text = format!("//! Gap.\n\nfn pick(flag: Bool) -> Int {{\n{body}}}\n");
        let case = case(name, &[("main.cheby", &text)]);
        let before = cheby_syntax::parse(
            case.analyzed.snapshot.file_of(&case.path).unwrap(),
            Arc::from(text.as_str()),
        );
        for action in case.actions(needle) {
            if action.disabled.is_some() {
                assert!(!action.preferred, "{name}");
                continue;
            }
            let out = case.applied(&action);
            assert_eq!(out.matches("// c").count(), 1, "{name}: {out}");
            let after = cheby_syntax::parse(
                case.analyzed.snapshot.file_of(&case.path).unwrap(),
                Arc::from(out.as_str()),
            );
            assert!(
                after.errors().len() < before.errors().len()
                    || before.errors().is_empty() && after.errors().is_empty(),
                "{name}: {} left {:?}\n{out}",
                action.title,
                after.errors()
            );
        }
    }
    // The gap of the reported case: the rewrite is offered, disabled.
    let text = "//! Gap.\n\nfn pick(flag: Bool) -> Int {\n  let n = if flag // one\n  { 1 } else { 2 }\n  n\n}\n";
    let case = case("reported", &[("main.cheby", text)]);
    let actions = case.actions("flag // one");
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].disabled.as_deref(), Some(DROPS_A_COMMENT));
}

/// A module that hides the prelude's `Nil` gets no rewrite that writes it
/// (D-1678): no `case` with `_ => Nil` for an `if` with no `else`; a module
/// that does not hide it gets one. `True` and `Int` are written by their
/// helps' fixes, which `actions::offers_fix` checks (D-1931).
#[test]
fn no_rewrite_writes_a_hidden_prelude_name() {
    let cases = [(
        "nil",
        "fn f(flag: Bool) -> Unit {\n  if flag { Other }\n}\n",
        "type Unit {\n  Nil,\n  Other,\n}\n",
        "flag { Other }",
        "Rewrite as `case`",
    )];
    for (name, body, hiding, needle, title) in cases {
        let plain = format!("//! Plain.\n\ntype Unit {{\n  Other,\n}}\n\n{body}");
        let case_plain = case(&format!("plain_{name}"), &[("main.cheby", &plain)]);
        assert_eq!(titles(&case_plain.actions(needle)), [title], "{name}");
        let hidden = format!("//! Hidden.\n\n{hiding}\n{body}");
        let case_hidden = case(&format!("hidden_{name}"), &[("main.cheby", &hidden)]);
        assert_eq!(
            titles(&case_hidden.actions(needle)),
            Vec::<&str>::new(),
            "{name}"
        );
    }
}

/// E0044 on a name that E0006 refuses: the new name is in snake case, and
/// the search for it ends (W11's review, S1).
#[test]
fn a_shadowed_camel_case_binding_gets_a_snake_case_name() {
    let text =
        "//! Shadows.\n\nfn main() {\n  let myVal = 1\n  let myVal = 2\n  assert myVal == 2\n}\n";
    let case = case("shadowed_camel", &[("main.cheby", text)]);
    let actions = case.actions("myVal = 2");
    assert_eq!(titles(&actions), ["Rename `myVal` to `my_val_2`"]);
    let out = case.applied(&actions[0]);
    assert!(
        out.contains("  let my_val_2 = 2\n  assert my_val_2 == 2\n"),
        "{out}"
    );
}

/// E0044's fresh name skips a name the body uses unresolved, which the
/// renamed binding would capture (C-5).
#[test]
fn a_fresh_name_skips_an_unresolved_use() {
    let text = "//! Shadow.\n\nfn f(a: Int) -> Int {\n  let sum = a\n  let sum = sum + 1\n  let r = sum_2 + 1\n  sum * 2 + r\n}\n";
    let case = case("shadow", &[("main.cheby", text)]);
    let actions = case.actions("sum = sum + 1");
    let titles = titles(&actions);
    assert_eq!(titles.len(), 1, "{titles:?}");
    assert_ne!(titles[0], "Rename `sum` to `sum_2`");
}

/// The titles of `actions`.
fn titles(actions: &[Action]) -> Vec<&str> {
    actions.iter().map(|a| a.title.as_str()).collect()
}

/// The reparse check: an `if` whose branch keeps a `return` would leave
/// as many grammar errors, so nothing is offered (X §6.5).
#[test]
fn a_rewrite_that_leaves_the_error_is_not_offered() {
    let text = "//! Returns.

fn f(a: Int) -> Int {
  let n = if a > 0 { return 1 } else { 2 }
  n
}
";
    let case = case("reparse", &[("main.cheby", text)]);
    assert_eq!(case.actions("a > 0"), Vec::new());
}

/// A resolve computes the rewrite of its index again, and refuses an id
/// of another provider.
#[test]
fn a_resolve_computes_the_same_rewrite() {
    let case = case("resolve", &[("main.cheby", IF_FORM)]);
    let range = TextRange::empty(offset(&case.report.text, "flag { 1 }"));
    let listed = case.actions("flag { 1 }");
    case.with(&[], |cx| {
        let edits = resolve(cx, range, &ActionId::Habit { index: 0 }).unwrap();
        assert_eq!(Some(edits), listed[0].edit.clone());
        assert!(resolve(cx, range, &ActionId::Habit { index: 1 }).is_err());
        assert!(resolve(cx, range, &ActionId::FillArms).is_err());
    });
    assert_eq!(listed.len(), 1);
}
