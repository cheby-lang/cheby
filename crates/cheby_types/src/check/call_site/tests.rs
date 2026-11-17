//! Tests of the call-site rules: the replacement of every call of a
//! function with `@std_call_site` by a call of its constructor with the
//! three parts of the call site, which is the start of the path that names
//! the function, the closure of a mention as a value, the facts that keep
//! the written function, and that such a function never satisfies an
//! interface (D-1628, D-1629, D-1630, D-1631).
#![expect(
    clippy::wildcard_enum_match_arm,
    reason = "a test matches only the variants it checks"
)]

use cheby_iface::DefId;

use crate::test_support::quiet::build_with_std;
use crate::test_support::{Build, STD_SITE, build_recording};
use crate::thir::{Body, CallSitePart, Callee, ExprId, ExprKind};

/// A build of package `app` from the root module `main`, with the test
/// `std::site`.
fn with_site(main: &str) -> Build {
    build_with_std(&[("site", STD_SITE)], &[("", main)])
}

/// The codes of `b`'s errors, sorted.
fn errors(b: &Build) -> Vec<String> {
    b.codes()
        .into_iter()
        .filter(|c| c.starts_with('E'))
        .collect()
}

/// The body of function `name` of `app`.
fn body<'b>(b: &'b Build, name: &str) -> &'b Body {
    b.thir_fn("app", name)
        .and_then(|f| f.body.as_ref())
        .unwrap_or_else(|| panic!("no body for `{name}`:\n{}", b.rendered()))
}

/// One call of a constructor found in a body: its callee, its number of
/// type arguments, the number of its arguments and where its call site
/// is, as `path:line:col`.
#[derive(Debug, PartialEq, Eq)]
struct Site {
    ctor: DefId,
    ty_args: usize,
    args: usize,
    at: String,
}

/// Every call in `body` whose last three arguments are the parts of a call
/// site, in expression order.
fn sites(b: &Build, body: &Body) -> Vec<Site> {
    body.exprs
        .iter()
        .filter_map(|expr| {
            let ExprKind::Call {
                callee: Callee::Fn { def, ty_args, .. },
                args,
            } = &expr.kind
            else {
                return None;
            };
            let [.., path, line, column] = **args else {
                return None;
            };
            let part = |id: ExprId| match body.expr(id).map(|e| (&e.kind, e.span)) {
                Some((ExprKind::CallSite(part), span)) => Some((*part, span)),
                _ => None,
            };
            let (
                Some((CallSitePart::Path, at)),
                Some((CallSitePart::Line, l)),
                Some((CallSitePart::Column, c)),
            ) = (part(path), part(line), part(column))
            else {
                return None;
            };
            assert!(at == l && l == c, "the three parts share one span");
            Some(Site {
                ctor: *def,
                ty_args: ty_args.len(),
                args: args.len(),
                at: b.sources.location(at).unwrap().to_string(),
            })
        })
        .collect()
}

/// Whether any THIR of `b` calls `def`, refers to it as a value or holds
/// it in a dictionary. A function's own definition does not count.
fn thir_names(b: &Build, def: DefId) -> bool {
    let def = format!("{def:?}");
    b.thir.values().any(|module| {
        let text = format!("{module:?}");
        ["Fn { def: ", "FnRef { def: ", "DictFn { def: "]
            .iter()
            .any(|form| text.contains(&format!("{form}{def}")))
    })
}

const CALLS: &str = r#"//! Calls of call-site functions in every form.

import std::site

fn plain() {
  site::log("plain")
}

fn piped(message: String) {
  message
  |> string_of
  |> site::log
}

fn piped_hole(note: site::Note) -> Int {
  note
  |> site::fold(_, fn(text) { 1 })
}

fn used() -> Int {
  use text <- site::fold(site::Note("u"))
  2
}

fn captured() -> fn(fn(String) -> Int) -> Int {
  site::fold(site::Note("c"), _)
}

fn string_of(text: String) -> String {
  text
}

fn turbofish() -> Int {
  site::fold::<site::Note>(site::Note("t"), fn(text) { 5 })
}
"#;

/// A call, a pipe stage with and without a hole, a `use` and the call
/// inside a capture all call the constructor with the type arguments and
/// arguments of the call, then the place of the path that names the
/// function, not the start of the chain (D-1628, D-1629).
#[test]
fn every_call_form_calls_the_constructor_at_the_path() {
    let b = with_site(CALLS);
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    let log_at = b.def("std::site", "log_at");
    let fold_at = b.def("std::site", "fold_at");
    let one = |name: &str| {
        let found = sites(&b, body(&b, name));
        let [site] =
            <[Site; 1]>::try_from(found).unwrap_or_else(|found| panic!("{name}: {found:?}"));
        site
    };
    let site = |ctor, ty_args, args, at: &str| Site {
        ctor,
        ty_args,
        args,
        at: at.to_owned(),
    };
    assert_eq!(one("plain"), site(log_at, 0, 4, "src/main.cheby:6:3"));
    // The stage's path, two lines after the chain's start.
    assert_eq!(one("piped"), site(log_at, 0, 4, "src/main.cheby:12:6"));
    assert_eq!(
        one("piped_hole"),
        site(fold_at, 1, 5, "src/main.cheby:17:6")
    );
    // The path after the arrow, not the `use` keyword.
    assert_eq!(one("used"), site(fold_at, 1, 5, "src/main.cheby:21:15"));
    assert_eq!(one("captured"), site(fold_at, 1, 5, "src/main.cheby:26:3"));
    assert_eq!(one("turbofish"), site(fold_at, 1, 5, "src/main.cheby:34:3"));
    // No call of the written functions is left.
    assert!(!thir_names(&b, b.def("std::site", "log")));
    assert!(!thir_names(&b, b.def("std::site", "fold")));
}

const VALUES: &str = r#"//! Call-site functions as values.

import std::site

fn each(lines: List<String>, f: fn(String) -> Nil) {
  case lines {
    [] => Nil
    [first, ..rest] => {
      f(first)
      each(rest, f)
    }
  }
}

fn passed(lines: List<String>) {
  each(lines, site::log)
}

fn bound() {
  let f = site::log
  f("a")
  f("b")
}

fn generic() -> fn(site::Note, fn(String) -> Int) -> Int {
  site::fold
}
"#;

/// A mention as a value, passed to a higher-order function, bound to a
/// name and called through it, or a generic function, is a closure whose
/// body calls the constructor with its parameters and the place of the
/// mention (D-1629). Every call through the value reports the mention.
#[test]
fn a_value_is_a_closure_that_passes_the_mention() {
    let b = with_site(VALUES);
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    let log_at = b.def("std::site", "log_at");
    let fold_at = b.def("std::site", "fold_at");
    let closures = |name: &str| {
        let body = body(&b, name);
        let closures: Vec<_> = body
            .exprs
            .iter()
            .filter_map(|e| match &e.kind {
                ExprKind::Closure { params, body } => Some((params.len(), *body)),
                _ => None,
            })
            .collect();
        (closures, sites(&b, body))
    };
    let (found, calls) = closures("passed");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        calls,
        [Site {
            ctor: log_at,
            ty_args: 0,
            args: 4,
            at: "src/main.cheby:16:15".to_owned(),
        }]
    );
    // The closure's body is that call, of its one parameter.
    let passed = body(&b, "passed");
    let (params, call) = found[0];
    assert_eq!(params, 1);
    let ExprKind::Call { args, .. } = &passed.expr(call).unwrap().kind else {
        panic!("the closure's body is not a call");
    };
    assert!(matches!(
        passed.expr(args[0]).unwrap().kind,
        ExprKind::Local(_)
    ));
    // Two calls through `f` are two calls of the closure, which reports
    // the `let`'s line.
    let (found, calls) = closures("bound");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        calls,
        [Site {
            ctor: log_at,
            ty_args: 0,
            args: 4,
            at: "src/main.cheby:20:11".to_owned(),
        }]
    );
    let (found, calls) = closures("generic");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, 2);
    assert_eq!(
        calls,
        [Site {
            ctor: fold_at,
            ty_args: 1,
            args: 5,
            at: "src/main.cheby:26:3".to_owned(),
        }]
    );
    assert!(!thir_names(&b, b.def("std::site", "log")));
    assert!(!thir_names(&b, b.def("std::site", "fold")));
}

/// A generic caller passes its own type parameter and dictionary on: the
/// constructor has the function's generics (D-1630).
#[test]
fn a_generic_caller_passes_its_dictionary() {
    let b = with_site(
        "//! A generic caller.\n\nimport std::site\n\nfn twice<T: site::Tagged>(value: T) -> Int {\n  \
         site::fold(value, fn(text) { 1 })\n}\n\nfn main() {\n  let _ = twice(site::Note(\"a\"))\n  Nil\n}\n",
    );
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    let body = body(&b, "twice");
    assert_eq!(
        sites(&b, body),
        [Site {
            ctor: b.def("std::site", "fold_at"),
            ty_args: 1,
            args: 5,
            at: "src/main.cheby:6:3".to_owned(),
        }]
    );
    let text = format!("{body:?}");
    assert!(text.contains("Param { param: "), "{text}");
}

/// The facts keep the function that is written, for hover, navigation,
/// references, rename and signature help, while THIR calls the
/// constructor (D-1631).
#[test]
fn the_facts_name_the_written_function() {
    let b = build_recording(
        &[("site", STD_SITE)],
        &[(
            "",
            "//! Facts.\n\nimport std::site\n\nfn main() {\n  site::log(\"a\")\n}\n",
        )],
    );
    let log = b.def("std::site", "log");
    let main = b.def("app", "main");
    let facts = b.facts[&main.module].body(main.index).unwrap();
    let call = facts
        .calls
        .iter()
        .find(|c| c.def == Some(log))
        .expect("a call fact for `log`");
    assert!(matches!(
        call.resolved,
        Some(Callee::Fn { def, .. }) if def == log
    ));
    assert_eq!(call.params.len(), 1);
    assert_eq!(
        sites(&b, body(&b, "main"))
            .into_iter()
            .map(|s| s.ctor)
            .collect::<Vec<_>>(),
        [b.def("std::site", "log_at")]
    );
}

/// A call-site function never counts toward satisfying an interface,
/// whatever its signature, with a note of its own (D-1630).
#[test]
fn a_call_site_function_never_satisfies_an_interface() {
    let b = with_site(
        "//! An interface that `site::text` would fit.\n\nimport std::site\n\n\
         pub interface Texty {\n  fn text(Self) -> String\n}\n\n\
         fn of<T: Texty>(x: T) -> String {\n  Texty::text(x)\n}\n\n\
         fn direct(note: site::Note) -> String {\n  Texty::text(note)\n}\n\n\
         fn bounded(note: site::Note) -> String {\n  of(note)\n}\n",
    );
    let rendered = b.rendered();
    let note = "`site::text` exists but does not count, because the compiler passes it the place \
                of its call, which a call through an interface does not have (§8.2, D-1630)";
    let unsatisfied: Vec<_> = b
        .diags
        .iter()
        .filter(|d| d.code.to_string() == "E0063")
        .collect();
    assert_eq!(unsatisfied.len(), 2, "{rendered}");
    for diag in &unsatisfied {
        assert_eq!(
            diag.message, "`site::Note` does not satisfy `Texty`",
            "{rendered}"
        );
        assert!(diag.notes.iter().any(|n| n == note), "{rendered}");
    }
    assert_eq!(errors(&b).len(), 2, "{rendered}");
    assert!(b.thir_fn("app", "direct").is_none());
    assert!(b.thir_fn("app", "bounded").is_none());
    assert!(b.thir_fn("app", "of").is_some());
    assert!(!thir_names(&b, b.def("std::site", "text")));
    assert!(!thir_names(&b, b.def("std::site", "text_at")));
}

/// The constructors are named only by the attribute in their module, and
/// no unused-item warning names them, in std's own module or in a user
/// build (W0004 is for `priv` items, W0005 for the local package's).
#[test]
fn the_constructors_are_never_reported_unused() {
    let b = crate::test_support::build_with_std(
        &[("site", STD_SITE)],
        &[(
            "",
            "//! Nothing.\n\nimport std::site\n\nfn main() {\n  site::log(\"a\")\n}\n",
        )],
    );
    let unused: Vec<String> = b
        .messages()
        .into_iter()
        .filter(|m| m.starts_with("W0004") || m.starts_with("W0005"))
        .collect();
    assert!(unused.is_empty(), "{unused:?}");
}

const STD_BROKEN: &str = "//! Call-site functions with faulty constructors.

/// A constructor without the column.
@std_call_site(short_at)
pub fn short(message: String) {
  let _ = message
  panic as \"not replaced\"
}

fn short_at(message: String, path: String, line: Int) {
  let _ = (message, path, line)
  Nil
}

/// A `priv` constructor.
@std_call_site(hidden_at)
pub fn hidden(message: String) {
  let _ = message
  panic as \"not replaced\"
}

priv fn hidden_at(message: String, path: String, line: Int, column: Int) {
  let _ = (message, path, line, column)
  Nil
}
";

/// A constructor that its own module reports as E0045 makes a call of the
/// function in another module silent and bodiless, never an internal
/// error or an ill-typed call (D-1630).
#[test]
fn a_faulty_constructor_is_reported_once_in_its_module() {
    let b = build_with_std(
        &[("broken", STD_BROKEN)],
        &[(
            "",
            "//! Uses.\n\nimport std::broken\n\nfn a() {\n  broken::short(\"a\")\n}\n\n\
             fn b() {\n  broken::hidden(\"b\")\n}\n\nfn c() -> fn(String) -> Nil {\n  broken::short\n}\n",
        )],
    );
    let rendered = b.rendered();
    assert_eq!(errors(&b), ["E0045", "E0045"], "{rendered}");
    for name in ["a", "b", "c"] {
        assert!(b.thir_fn("app", name).is_none(), "`{name}` has a body");
    }
}

/// Std's own module replaces its calls too: a call of a call-site function
/// in the module that declares it calls the constructor (D-1631).
#[test]
fn std_replaces_its_own_calls() {
    let lib = format!(
        "{STD_SITE}\n/// Logs twice.\npub fn twice(message: String) {{\n  log(message)\n  log(message)\n}}\n"
    );
    let b = build_with_std(
        &[("site", &lib)],
        &[("", "//! Nothing.\n\nimport std::site\n")],
    );
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    let twice = b.def("std::site", "twice");
    let body = b.thir[&twice.module]
        .fns
        .iter()
        .find(|f| f.def == twice)
        .and_then(|f| f.body.as_ref())
        .unwrap();
    let found: Vec<String> = sites(&b, body).into_iter().map(|s| s.at).collect();
    let line = STD_SITE.lines().count().saturating_add(4);
    assert_eq!(
        found,
        [
            format!("std/site.cheby:{line}:3"),
            format!("std/site.cheby:{}:3", line.saturating_add(1)),
        ]
    );
}
