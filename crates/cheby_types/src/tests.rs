//! Signature and body checking over small multi-module builds, the THIR
//! of 001, the check of every example, and the expression core of
//! `check/` (C3, C7, C11, C13, C15), one section per item of the W2-E1
//! package: inference, order, expected types and conversions, calls,
//! values, operators, interpolation, desugaring, statements, `panic`,
//! items and the end of a body.

use std::path::Path;

use cheby_diag::{Diagnostic, SourceMap};
use cheby_iface::{DefId, ItemSig, Ty, TyKind, builtin};

use crate::test_support::{Build, build, build_package, build_with_std, has_syntax_errors};
use crate::thir::{
    BinOpKind, Body, Callee, Dict, EntryInfo, Expr, ExprKind, InterpPart, Lit, OpImpl, PanicKind,
    ParamOrigin, PatKind, Stmt,
};

fn main_with(body: &str) -> String {
    format!("//! Test.\n\nimport std::io\n\nfn main() {{\n{body}\n}}\n")
}

// ---- fake standard library ------------------------------------------------

const STD_INT: &str = r#"//! Integers.

/// The text of `n`.
@external(native, "cheby_int_show")
@external(js, "./int.mjs", "show")
pub fn show(n: Int) -> String

/// Compares two integers.
@external(native, "cheby_int_compare")
@external(js, "./int.mjs", "compare")
pub fn compare(a: Int, b: Int) -> Order

/// Adds two integers.
pub fn add(a: Int, b: Int) -> Int {
  a + b
}
"#;

const STD_STRING: &str = r#"//! Text.

/// The text itself.
pub fn show(text: String) -> String {
  text
}

/// Joins two strings.
@external(native, "cheby_string_add")
@external(js, "./string.mjs", "add")
pub fn add(a: String, b: String) -> String

/// Compares two strings.
@external(native, "cheby_string_compare")
@external(js, "./string.mjs", "compare")
pub fn compare(a: String, b: String) -> Order
"#;

const STD_LIST: &str = r#"//! Lists.

/// An empty list.
@external(native, "cheby_list_new")
@external(js, "./list.mjs", "new")
pub fn new<T>() -> List<T>

/// The number of elements.
@external(native, "cheby_list_length")
@external(js, "./list.mjs", "length")
pub fn length<T>(items: List<T>) -> Int

/// Applies `f` to every element.
@external(native, "cheby_list_map")
@external(js, "./list.mjs", "map")
pub fn map<A, B>(items: List<A>, f: fn(A) -> B) -> List<B>

/// Folds the elements from the left.
@external(native, "cheby_list_fold")
@external(js, "./list.mjs", "fold")
pub fn fold<A, B>(items: List<A>, initial: B, f: fn(B, A) -> B) -> B

/// Pairs the elements of two lists.
@external(native, "cheby_list_zip")
@external(js, "./list.mjs", "zip")
pub fn zip<A, B>(left: List<A>, right: List<B>) -> List<(A, B)>

/// The elements' texts.
@external(native, "cheby_list_show")
@external(js, "./list.mjs", "show")
pub fn show<T: Show>(items: List<T>) -> String
"#;

const STD_RESULT: &str = r#"//! Results.

/// Calls `next` with the value of `Ok`, or returns the `Err`.
@external(native, "cheby_result_try")
@external(js, "./result.mjs", "try")
pub fn try<T, E, U>(value: Result<T, E>, next: fn(T) -> Result<U, E>) -> Result<U, E>
"#;

fn std_modules() -> [(&'static str, &'static str); 4] {
    [
        ("int", STD_INT),
        ("string", STD_STRING),
        ("list", STD_LIST),
        ("result", STD_RESULT),
    ]
}

/// Package `app` with `items` as its root module, after `//! Test.`,
/// against the fake standard library.
fn check(items: &str) -> Build {
    build_with_std(&std_modules(), &[("", &format!("//! Test.\n\n{items}"))])
}

/// [`check`], which must have no errors.
fn clean(items: &str) -> Build {
    let b = check(items);
    assert!(
        !b.diags.iter().any(Diagnostic::is_error),
        "{items}\n{}",
        b.rendered()
    );
    b
}

/// The error codes of [`check`], sorted.
fn errors(items: &str) -> Vec<String> {
    let b = check(items);
    let mut codes: Vec<String> = b
        .diags
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.code.to_string())
        .collect();
    codes.sort();
    codes
}

/// The checked body of function `name` of the root module.
fn body<'b>(b: &'b Build, name: &str) -> &'b Body {
    b.thir_fn("app", name)
        .and_then(|f| f.body.as_ref())
        .unwrap_or_else(|| panic!("`{name}` has no checked body:\n{}", b.rendered()))
}

/// The expressions of `body` that `pick` accepts.
fn exprs(body: &Body, pick: impl Fn(&ExprKind) -> bool) -> Vec<&Expr> {
    body.exprs.iter().filter(|e| pick(&e.kind)).collect()
}

/// The type `def<args>` as `b` interns it.
fn named(b: &Build, def: DefId, args: &[Ty]) -> Ty {
    b.globals.types.named(def, args)
}

// ---- 001 ---------------------------------------------------------------

const HELLO: &str = "//! Prints a greeting to standard output.\n\nimport std::io\n\nfn main() {\n  io::println(\"Hello, world!\")\n}\n";

#[test]
fn hello_world_checks_into_thir() {
    let b = build(&[("", HELLO)]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    let println = b.def("std::io", "println");
    // The interface of std::io: `println(text: String)` returns `Nil`.
    let ItemSig::Fn(sig) = b.fn_sig("std::io", "println") else {
        panic!("println has no function signature");
    };
    assert_eq!(sig.params.len(), 1);
    assert_eq!(b.globals.names.get(sig.params[0].name), "text");
    assert_eq!(sig.params[0].ty, Ty::STRING);
    assert_eq!(sig.ret, Ty::NIL);
    assert!(sig.generics.is_empty());
    assert!(b.iface("std::io").hash.is_some());
    // Its THIR: no body, a native external.
    let external = b.thir_fn("std::io", "println").unwrap();
    assert!(external.body.is_none());
    assert_eq!(
        external.native_external.as_ref().map(|e| &*e.symbol),
        Some("cheby_io_println")
    );
    assert_eq!(external.entry, None);
    assert_eq!(b.thir[&b.module("std::io")].fns.len(), 4);
    // `main`: an entry returning `Nil`, whose body calls `io::println`.
    let main = b.thir_fn("app", "main").unwrap();
    assert_eq!(main.entry, Some(EntryInfo::Nil));
    assert!(main.native_external.is_none());
    let body = main.body.as_ref().unwrap();
    assert_eq!(body.ret, Ty::NIL);
    assert_eq!(body.params, []);
    let block = body.expr(body.value).unwrap();
    assert_eq!(block.ty, Ty::NIL);
    let ExprKind::Block { stmts, tail } = &block.kind else {
        panic!("the body is not a block: {block:?}");
    };
    assert!(stmts.is_empty());
    let call = body.expr(tail.unwrap()).unwrap();
    assert_eq!(call.ty, Ty::NIL);
    let ExprKind::Call { callee, args } = &call.kind else {
        panic!("not a call: {call:?}");
    };
    assert_eq!(
        callee,
        &Callee::Fn {
            def: println,
            ty_args: Box::new([]),
            dicts: Box::new([]),
        }
    );
    assert_eq!(args.len(), 1);
    let arg = body.expr(args[0]).unwrap();
    assert_eq!(arg.kind, ExprKind::Lit(Lit::Str("Hello, world!".into())));
    assert_eq!(arg.ty, Ty::STRING);
    assert_eq!(body.exprs.len(), 3);
}

#[test]
fn ui_hello_world_checks() {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ui/01-overview/hello_world.cheby"),
    )
    .unwrap();
    let b = build(&[("", &text)]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    assert_eq!(
        b.thir_fn("app", "main").unwrap().entry,
        Some(EntryInfo::Nil)
    );
}

// ---- signatures (C2) ---------------------------------------------------

#[test]
fn signatures_lower_and_omitted_returns_are_nil() {
    let src = "//! Test.\n\nfn shout(text: String, other: String) -> String {\n  text\n}\n\npub fn ignore(text: String) {\n  Nil\n}\n\npriv fn nothing() -> Nil {\n  Nil\n}\n";
    let b = build(&[("", src)]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    let ItemSig::Fn(shout) = b.fn_sig("app", "shout") else {
        panic!()
    };
    assert_eq!(
        shout.params.iter().map(|p| p.ty).collect::<Vec<_>>(),
        [Ty::STRING, Ty::STRING]
    );
    assert_eq!(shout.ret, Ty::STRING);
    let ItemSig::Fn(ignore) = b.fn_sig("app", "ignore") else {
        panic!()
    };
    assert_eq!(ignore.ret, Ty::NIL);
    let ItemSig::Fn(nothing) = b.fn_sig("app", "nothing") else {
        panic!()
    };
    assert_eq!(nothing.ret, Ty::NIL);
    // Not entries.
    assert_eq!(b.thir_fn("app", "shout").unwrap().entry, None);
    let body = b.thir_fn("app", "shout").unwrap().body.as_ref().unwrap();
    assert_eq!(body.params.len(), 2);
    assert_eq!(body.locals.len(), 2);
    assert!(body.locals.iter().all(|l| l.ty == Ty::STRING));
}

#[test]
fn main_must_be_an_entry_point() {
    let with_param = "//! Test.\n\nfn main(name: String) {\n  Nil\n}\n";
    let b = build(&[("", with_param)]);
    assert_eq!(b.codes(), ["E0050"], "{:?}", b.messages());
    assert_eq!(b.fn_sig("app", "main"), &ItemSig::Error);
    assert!(b.thir_fn("app", "main").is_none());
    let returns_string = "//! Test.\n\nfn main() -> String {\n  \"x\"\n}\n";
    let b = build(&[("", returns_string)]);
    assert_eq!(b.codes(), ["E0050"], "{:?}", b.messages());
    // Every module's `main` is checked, of any visibility (D-137, D-163).
    let tool = "//! Test.\n\npriv fn main(x: String) {\n  Nil\n}\n";
    let ok = "//! Test.\n\nimport app::tool\n\npub fn main() {\n  Nil\n}\n";
    let b = build(&[("", ok), ("tool", tool)]);
    assert_eq!(b.codes(), ["E0050"], "{:?}", b.messages());
    assert_eq!(
        b.thir_fn("app", "main").unwrap().entry,
        Some(EntryInfo::Nil)
    );
}

// ---- bodies (C3) -------------------------------------------------------

#[test]
fn lets_locals_and_local_calls() {
    let src = "//! Test.\n\nimport std::io\n\nfn greet(name: String) {\n  io::print(\"hello, \")\n  io::println(name)\n}\n\nfn main() {\n  let who = \"world\"\n  let again: String = who\n  let _ = greet(again)\n  let done: Nil = greet(who)\n}\n";
    let b = build(&[("", src)]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    let greet = b.def("app", "greet");
    let main = b.thir_fn("app", "main").unwrap();
    let body = main.body.as_ref().unwrap();
    let ExprKind::Block { stmts, tail } = &body.expr(body.value).unwrap().kind else {
        panic!()
    };
    // A block ending in `let` has the value `Nil`.
    assert_eq!(*tail, None);
    assert_eq!(stmts.len(), 4);
    let names: Vec<(&str, Ty)> = body
        .locals
        .iter()
        .map(|l| (b.globals.names.get(l.name), l.ty))
        .collect();
    assert_eq!(
        names,
        [
            ("who", Ty::STRING),
            ("again", Ty::STRING),
            ("done", Ty::NIL)
        ]
    );
    let Stmt::Let { pat, init } = &stmts[2] else {
        panic!("{:?}", stmts[2]);
    };
    assert_eq!(pat.kind, PatKind::Wild);
    assert_eq!(pat.ty, Ty::NIL);
    let ExprKind::Call {
        callee: Callee::Fn { def, .. },
        args,
    } = &body.expr(*init).unwrap().kind
    else {
        panic!()
    };
    assert_eq!(*def, greet);
    let arg = body.expr(args[0]).unwrap();
    assert!(matches!(arg.kind, ExprKind::Local(_)));
    assert_eq!(arg.ty, Ty::STRING);
}

#[test]
fn nil_is_a_value() {
    let src = main_with("  let n = Nil\n  let m: Nil = n\n  m");
    let b = build(&[("", &src)]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    let body = b.thir_fn("app", "main").unwrap().body.as_ref().unwrap();
    let nil = body
        .exprs
        .iter()
        .find(|e| matches!(e.kind, ExprKind::Ctor { .. }))
        .unwrap();
    assert_eq!(
        nil.kind,
        ExprKind::Ctor {
            ctor: builtin::NIL,
            ty_args: Box::new([]),
        }
    );
    assert_eq!(nil.ty, Ty::NIL);
}

#[test]
fn mismatches_are_e0047() {
    let cases = [
        main_with("  io::println(Nil)"),
        main_with("  let x: String = Nil"),
        main_with("  let x: Nil = \"text\""),
        "//! Test.\n\nfn name() -> String {\n  Nil\n}\n".to_owned(),
        "//! Test.\n\nfn name() -> String {\n  let x = \"a\"\n}\n".to_owned(),
        main_with("  \"a value\""),
    ];
    for src in &cases {
        let b = build(&[("", src)]);
        assert_eq!(b.codes(), ["E0047"], "{src}: {:?}", b.messages());
        assert_eq!(b.diags[0].message, "mismatched types");
        // A body with an error is not returned.
        assert!(b.thir[&b.module("app")].fns.is_empty(), "{src}");
    }
    let b = build(&[("", &cases[0])]);
    let labels: Vec<&str> = b.diags[0]
        .labels
        .iter()
        .map(|l| l.message.as_str())
        .collect();
    assert_eq!(
        labels,
        [
            "expected `String`, found `Nil`",
            "arguments to this function are incorrect"
        ]
    );
    let b = build(&[("", &cases[5])]);
    let labels: Vec<&str> = b.diags[0]
        .labels
        .iter()
        .map(|l| l.message.as_str())
        .collect();
    assert_eq!(
        labels,
        [
            "expected `Nil`, found `String`",
            "the return type is omitted, so it is `Nil`"
        ]
    );
}

/// A mismatch inside a block is reported once, not again for the block.
#[test]
fn a_mismatch_is_reported_once() {
    let src = "fn f() -> String {\n  let x = {\n    1\n  }\n  {\n    Nil\n  }\n}\n";
    let b = check(src);
    assert_eq!(b.codes(), ["E0047"], "{}", b.rendered());
}

#[test]
fn argument_counts_are_e0048() {
    for (body, expected) in [
        (
            "  io::println(\"a\", \"b\")",
            "this function takes 1 argument but 2 arguments were supplied",
        ),
        (
            "  io::println()",
            "this function takes 1 argument but 0 arguments were supplied",
        ),
    ] {
        let b = build(&[("", &main_with(body))]);
        assert_eq!(b.codes(), ["E0048"], "{:?}", b.messages());
        assert_eq!(b.diags[0].message, expected);
        // The declaration in std::io is the secondary label.
        assert_eq!(b.diags[0].labels.len(), 2);
    }
    let src =
        "//! Test.\n\nfn two(a: String, b: String) {\n  Nil\n}\n\nfn main() {\n  two(\"a\")\n}\n";
    let b = build(&[("", src)]);
    assert_eq!(b.codes(), ["E0048"], "{:?}", b.messages());
    assert_eq!(
        b.diags[0].message,
        "this function takes 2 arguments but 1 argument was supplied"
    );
    // Closures and constructors are counted too.
    assert_eq!(
        errors("fn main() {\n  let f = fn(a: Int) { a }\n  let x = f(1, 2)\n}\n"),
        ["E0048"]
    );
    assert_eq!(errors("fn main() {\n  let x = Some(1, 2)\n}\n"), ["E0048"]);
}

#[test]
fn calling_a_value_that_is_not_a_function_is_e0049() {
    let b = build(&[(
        "",
        &main_with("  let greeting = \"hello\"\n  greeting(\"world\")"),
    )]);
    assert_eq!(b.codes(), ["E0049"], "{:?}", b.messages());
    assert_eq!(b.diags[0].message, "expected a function, found `String`");
    let b = build(&[("", &main_with("  Nil()"))]);
    assert_eq!(b.codes(), ["E0049"], "{:?}", b.messages());
    assert_eq!(b.diags[0].message, "expected a function, found `Nil`");
}

#[test]
fn uses_of_erroneous_items_are_silent() {
    // `count`'s signature has an error: calling it reports nothing more,
    // and `main` is not returned.
    let src = "//! Test.\n\nfn count(text: List) -> Int {\n  1\n}\n\nfn main() {\n  let n = count(\"x\")\n}\n";
    let b = build(&[("", src)]);
    assert_eq!(b.codes(), ["E0051"], "{:?}", b.messages());
    assert!(b.thir_fn("app", "main").is_none());
    // A function whose signature has an error still has its body checked.
    let src = "//! Test.\n\nimport std::io\n\nfn f(x: List) {\n  io::println(Nil)\n}\n";
    let b = build(&[("", src)]);
    assert_eq!(b.codes(), ["E0047", "E0051"], "{:?}", b.messages());
    assert!(b.thir_fn("app", "f").is_none());
}

#[test]
fn cross_module_calls_use_the_dependency_interface() {
    let util = "//! Test.\n\nimport std::io\n\npub fn shout(text: String) -> String {\n  text\n}\n\nfn helper() {\n  io::println(\"x\")\n}\n";
    let main = "//! Test.\n\nimport std::io\nimport app::util\n\nfn main() {\n  io::println(util::shout(\"hi\"))\n  util::helper()\n}\n";
    let b = build(&[("", main), ("util", util)]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    let shout = b.def("app::util", "shout");
    let body = b.thir_fn("app", "main").unwrap().body.as_ref().unwrap();
    assert!(body.exprs.iter().any(|e| matches!(
        &e.kind,
        ExprKind::Call { callee: Callee::Fn { def, .. }, .. } if *def == shout
    ) && e.ty == Ty::STRING));
}

#[test]
fn interface_hash_ignores_bodies_and_private_items() {
    let a = "//! Test.\n\npub fn shout(text: String) -> String {\n  text\n}\n";
    let b_src = "//! Test.\n\npriv fn helper() {\n  Nil\n}\n\npub fn shout(text: String) -> String {\n  let copy = text\n  copy\n}\n";
    let c_src = "//! Test.\n\npub fn shout(text: String) -> Nil {\n  Nil\n}\n";
    // A test before the `pub` item shifts its `DefIndex`.
    let d_src = "//! Test.\n\ntest \"shouting\" {\n  Nil\n}\n\npub fn shout(text: String) -> String {\n  text\n}\n";
    let ha = build(&[("", a)]).iface("app").hash;
    let hb = build(&[("", b_src)]).iface("app").hash;
    let hc = build(&[("", c_src)]).iface("app").hash;
    let hd = build(&[("", d_src)]).iface("app").hash;
    assert!(ha.is_some());
    assert_eq!(ha, hb);
    assert_ne!(ha, hc);
    assert_eq!(ha, hd);
    // Byte-stable across two builds.
    assert_eq!(ha, build(&[("", a)]).iface("app").hash);
}

/// An error in a body is a body matter: the function's interface, its
/// hash and what dependents are told are those of the same function
/// without it (D-246, §13.12.3, B8).
#[test]
fn body_errors_do_not_change_interfaces() {
    let lib = |body: &str| {
        format!(
            "//! Test.\n\nimport std::io\n\npub fn show(text: String) {{\n{body}  io::println(text)\n}}\n"
        )
    };
    let main = "//! Test.\n\nimport app::lib\n\nfn main() {\n  lib::show(Nil)\n}\n";
    let plain = build(&[("", main), ("lib", &lib(""))]);
    let broken = build(&[("", main), ("lib", &lib("  let n: String = 1\n"))]);
    assert_eq!(plain.iface("app::lib").hash, broken.iface("app::lib").hash);
    assert!(matches!(broken.fn_sig("app::lib", "show"), ItemSig::Fn(_)));
    assert_eq!(plain.codes(), ["E0047"], "{:?}", plain.messages());
    assert_eq!(
        broken.codes(),
        ["E0047", "E0047"],
        "{:?}",
        broken.messages()
    );
    assert!(broken.thir_fn("app::lib", "show").is_none());
}

#[test]
fn checking_is_deterministic() {
    let src = "fn greet(name: String) -> String {\n  let parts = [name, \"!\"]\n  let f = fn(x) { x }\n  f(name)\n}\n\nfn main() {\n  let who = greet(\"world\")\n  let n = 1 + 2 * 3\n}\n";
    let one = check(src);
    let two = check(src);
    assert_eq!(one.thir[&one.module("app")], two.thir[&two.module("app")]);
    assert_eq!(one.rendered(), two.rendered());
}

// ---- every example -----------------------------------------------------

/// Every valid example goes through the whole front end without an
/// internal error or a panic, and 001 checks cleanly. Until the standard
/// library ships (W3-L), examples may report E0038 for planned modules
/// and E0063 where a builtin type's home module is missing; any other
/// error is a checker bug. Examples with phase-A errors (030) stop before
/// resolution.
#[test]
fn examples_check_without_internal_errors() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut dirs: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    let mut checked = 0;
    let mut unexpected = Vec::new();
    for dir in dirs {
        let mut sources = SourceMap::new();
        let mut phase_a = Vec::new();
        let Some(manifest) =
            cheby_package::Manifest::load(&dir, &mut sources, &mut phase_a).unwrap()
        else {
            continue;
        };
        let files = cheby_package::discover(&dir, &manifest.name, &mut phase_a).unwrap();
        let texts: Vec<(String, String)> = files
            .iter()
            .map(|f| {
                let text = std::fs::read_to_string(dir.join(&f.file)).unwrap();
                (f.path.segments()[1..].join("::"), text)
            })
            .collect();
        let syntax_errors = texts.iter().any(|(_, text)| has_syntax_errors(text));
        if syntax_errors || phase_a.iter().any(Diagnostic::is_error) {
            continue;
        }
        let local: Vec<(&str, &str)> = texts
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        let b = build_package(&manifest.name, &[], &local);
        for d in b.diags.iter().filter(|d| d.is_error()) {
            let code = d.code.to_string();
            if code != "E0038" && code != "E0063" {
                unexpected.push(format!("{}: {code}: {}", dir.display(), d.message));
            }
        }
        if dir.ends_with("001_hello_world") {
            assert!(b.diags.is_empty(), "{:?}", b.messages());
            let main = b.thir_fn(&manifest.name, "main").unwrap();
            assert_eq!(main.entry, Some(EntryInfo::Nil));
        }
        checked += 1;
    }
    assert!(checked >= 30, "only {checked} examples checked");
    assert!(unexpected.is_empty(), "{unexpected:#?}");
}

// ---- test support ------------------------------------------------------

/// Fake std modules load as std; a loaded home module of a builtin type is
/// an implicit dependency of every local module (D-447), even without an
/// import.
#[test]
fn fake_std_modules_and_home_edges() {
    let int = "//! Integers.\n\n/// Zero, as text.\npub fn zero_text() -> String {\n  \"0\"\n}\n";
    let util = "//! A std module that nothing imports.\n\npub fn unused() {\n  Nil\n}\n";
    let main = "//! Test.\n\nimport std::io\n\nfn main() {\n  io::println(\"x\")\n}\n";
    let b = build_with_std(&[("int", int), ("util", util)], &[("", main)]);
    assert!(b.diags.is_empty(), "{}", b.rendered());
    let int_id = b.module("std::int");
    assert_eq!(
        b.modules.get(int_id).map(|i| i.package),
        Some(cheby_iface::PackageId::STD)
    );
    assert_eq!(b.thir_of("std::int").fns.len(), 1);
    // A std module that is not a home and not imported is never loaded.
    assert!(
        b.modules
            .find(&cheby_package::ModulePath::root("std").child("util"))
            .is_none()
    );
    // A given `io` replaces the slice's and is not partial.
    let io = "//! IO.\n\n@external(native, \"cheby_io_println\")\n@external(js, \"./io.mjs\", \"println\")\npub fn println(text: String)\n";
    let b = build_with_std(&[("io", io)], &[("", main)]);
    assert!(b.diags.is_empty(), "{}", b.rendered());
    assert_eq!(b.thir_of("std::io").fns.len(), 1);
}

#[test]
fn diagnostics_render_with_sources() {
    let b = build(&[("", &main_with("  io::println(Nil)"))]);
    assert_eq!(
        b.rendered(),
        "error[E0047]: mismatched types\n --> src/main.cheby:6:15\n  |\n6 |   io::println(Nil)\n  |   ----------- ^^^ expected `String`, found `Nil`\n  |   |\n  |   arguments to this function are incorrect\n\n"
    );
}

/// Pattern types are filled at the end of the body, and `Body::pats`
/// finds every `let` pattern.
#[test]
fn patterns_get_their_types() {
    let src = main_with("  let who = \"world\"\n  let _ = io::println(who)");
    let b = build(&[("", &src)]);
    assert!(b.diags.is_empty(), "{}", b.rendered());
    let body = b.thir_fn("app", "main").unwrap().body.as_ref().unwrap();
    let pats: Vec<(crate::thir::PatOrigin, Ty, &PatKind)> = body
        .pats()
        .into_iter()
        .map(|(origin, p)| (origin, p.ty, &p.kind))
        .collect();
    assert_eq!(pats.len(), 2);
    assert_eq!(pats[0].1, Ty::STRING);
    assert!(matches!(pats[0].2, PatKind::Bind(_)));
    assert_eq!((pats[1].1, pats[1].2), (Ty::NIL, &PatKind::Wild));
    assert!(
        pats.iter()
            .all(|(origin, _, _)| *origin == crate::thir::PatOrigin::Let)
    );
    assert!(body.locals.iter().all(|l| !l.synthetic));
}

// ---- 1. no E0038 ---------------------------------------------------------

/// Every expression form checks; nothing in `cheby_types` reports E0038.
#[test]
fn every_expression_form_checks() {
    let src = r#"import std::list

type Point {
  x: Int,
  y: Int,
}

fn twice(f: fn(Int) -> Int, x: Int) -> Int {
  f(f(x))
}

fn forms(p: Point, flag: Bool) -> Int {
  let pair = (1, "one")
  let (n, _) = pair
  let xs = [1, 2, ..[3]]
  let total = list::fold(xs, 0, fn(acc, x) { acc + x })
  let add_one = twice(fn(x) { x + 1 }, _)
  let piped = xs |> list::length
  let neg = -n
  let negated = !flag
  let sum = (p.x + p.y) * 2 % 3
  let shifted = 1 << 2 | 4 & 5 ^ 6 >> 1
  let same = n == 1 && flag || n != 2
  let block = {
    let inner = n
    inner
  }
  fn count(k) {
    case k {
      0 => 0
      _ => count(k - 1)
    }
  }
  let label = "{piped}"
  assert same as "same"
  case flag {
    True => add_one(total) + neg + sum + shifted + block + count(3)
    False => todo as "later"
  }
}
"#;
    let b = check(src);
    assert!(!b.codes().contains(&"E0038".to_owned()), "{}", b.rendered());
}

// ---- 2. inference --------------------------------------------------------

#[test]
fn generic_callees_are_instantiated() {
    let src =
        "import std::list\n\nfn count(words: List<String>) -> Int {\n  list::length(words)\n}\n";
    let b = clean(src);
    let length = b.def("std::list", "length");
    let body = body(&b, "count");
    let calls = exprs(body, |k| matches!(k, ExprKind::Call { .. }));
    let ExprKind::Call {
        callee: Callee::Fn { def, ty_args, .. },
        ..
    } = &calls[0].kind
    else {
        panic!("{calls:?}");
    };
    assert_eq!(*def, length);
    assert_eq!(&ty_args[..], [Ty::STRING]);
    assert_eq!(calls[0].ty, Ty::INT);
}

#[test]
fn rigid_type_parameters() {
    let b = clean("fn same<T>(x: T) -> T {\n  let y = x\n  y\n}\n");
    let body = body(&b, "same");
    let ItemSig::Fn(sig) = b.fn_sig("app", "same") else {
        panic!()
    };
    assert_eq!(
        body.local(cheby_resolve::LocalId::from_u32(1)).unwrap().ty,
        sig.ret
    );
    assert!(matches!(b.globals.types.kind(sig.ret), TyKind::Param(_)));
    // A type parameter is not any particular type.
    assert_eq!(errors("fn bad<T>(x: T) -> Int {\n  x\n}\n"), ["E0047"]);
    assert_eq!(
        errors("fn bad<T, U>(x: T, y: U) -> Bool {\n  x == y\n}\n"),
        ["E0047"]
    );
}

#[test]
fn turbofish_gives_type_arguments() {
    let b = clean(
        "import std::list\n\nfn empty() -> Int {\n  let xs = list::new::<String>()\n  list::length(xs)\n}\n",
    );
    let body = body(&b, "empty");
    let new = b.def("std::list", "new");
    let call = exprs(
        body,
        |k| matches!(k, ExprKind::Call { callee: Callee::Fn { def, .. }, .. } if *def == new),
    );
    assert_eq!(call[0].ty, named(&b, builtin::LIST, &[Ty::STRING]));
    // As a value too, and on constructors.
    clean(
        "import std::list\n\nfn f() {\n  let make = list::new::<Int>\n  let none = None::<Int>\n}\n",
    );
    // E0051 for a wrong count.
    let b = check("import std::list\n\nfn f() {\n  let xs = list::new::<Int, String>()\n}\n");
    assert_eq!(
        b.messages(),
        ["E0051: `new` takes 1 type argument but 2 were given [\"\"]"],
        "{}",
        b.rendered()
    );
    let b = check("import std::io\n\nfn f() {\n  io::println::<Int>(\"x\")\n}\n");
    assert_eq!(
        b.messages(),
        ["E0051: `println` takes no type arguments [\"\"]"]
    );
}

#[test]
fn literals_default_after_the_whole_body() {
    let b = clean(
        "fn f() -> U8 {\n  let x = 1\n  let y = 1.5\n  let z: F32 = 2.5\n  let w = 3\n  x\n}\n",
    );
    let body = body(&b, "f");
    let tys: Vec<(&str, Ty)> = body
        .locals
        .iter()
        .map(|l| (b.globals.names.get(l.name), l.ty))
        .collect();
    // `x` is used as a `U8` after its `let` (D-133).
    assert_eq!(
        tys,
        [
            ("x", Ty::U8),
            ("y", Ty::FLOAT),
            ("z", Ty::F32),
            ("w", Ty::INT)
        ]
    );
    // An integer literal is never a float, and the reverse.
    assert_eq!(errors("fn f() -> Float {\n  1\n}\n"), ["E0047"]);
    assert_eq!(errors("fn f() -> Int {\n  1.0\n}\n"), ["E0047"]);
}

#[test]
fn literals_must_fit_their_type() {
    let b = check(
        "fn f() {\n  let b: U8 = 256\n  let c: I8 = -128\n  let d: I8 = -129\n  let e: U32 = -1\n  let g: F32 = 1.0e39\n  let h: Int = 9223372036854775807\n  let i: Int = -9223372036854775808\n  let j: U64 = 18446744073709551615\n}\n",
    );
    assert_eq!(
        b.messages(),
        [
            "E0071: literal out of range for `F32` [\"this is too large for `F32`\"]",
            "E0071: literal out of range for `I8` [\"`-129` does not fit in `I8`\"]",
            "E0071: literal out of range for `U32` [\"`-1` does not fit in `U32`\"]",
            "E0071: literal out of range for `U8` [\"`256` does not fit in `U8`\"]",
        ],
        "{}",
        b.rendered()
    );
    // The type may be fixed after the literal (D-133).
    assert_eq!(errors("fn f() -> U8 {\n  let x = 300\n  x\n}\n"), ["E0071"]);
    assert_eq!(
        errors("fn f() -> Int {\n  9223372036854775808\n}\n"),
        ["E0071"]
    );
}

#[test]
fn unknown_types_need_annotations() {
    // The spec's `unused` (§3.12.4): `a + b` is resolved at the end, and
    // nothing fixes the type of `a`.
    let b = check("fn unused() {\n  let f = fn(a, b) { a + b }\n}\n");
    assert_eq!(
        b.messages(),
        ["E0057: type annotation needed [\"the type of `a` is not known\"]"],
        "{}",
        b.rendered()
    );
    assert!(
        b.rendered()
            .contains("help: give the parameter a type annotation: `a: <type>`")
    );
    // An empty list nothing uses.
    let b = check("fn f() {\n  let xs = []\n}\n");
    assert_eq!(
        b.messages(),
        ["E0057: type annotation needed [\"the type of this is not known\"]"]
    );
    // A type argument nothing fixes.
    let b = check("import std::list\n\nfn f() {\n  let xs = list::new()\n}\n");
    assert_eq!(
        b.messages(),
        ["E0057: type annotation needed [\"the type argument `T` of `new` is not known\"]"]
    );
    // An annotation fixes it.
    clean("fn f() {\n  let xs: List<Int> = []\n  let g = fn(a: Int, b: Int) { a + b }\n}\n");
}

// ---- 3. order of inference ----------------------------------------------

const POINT: &str = "type Point {\n  x: Int,\n  y: Int,\n}\n\n";

#[test]
fn closures_are_checked_after_the_other_arguments() {
    // The closure comes first but is checked after `origin`, so `p` is
    // known to be a `Point` when `p.x` is checked (D-234).
    let src = format!(
        "{POINT}fn apply<A, B>(f: fn(A) -> B, x: A) -> B {{\n  f(x)\n}}\n\nfn main() {{\n  let origin = Point {{ x: 0, y: 0 }}\n  let x = apply(fn(p) {{ p.x }}, origin)\n}}\n"
    );
    let b = clean(&src);
    let body = body(&b, "main");
    let x = body
        .locals
        .iter()
        .find(|l| b.globals.names.get(l.name) == "x")
        .unwrap();
    assert_eq!(x.ty, Ty::INT);
    // The spec's `total` and `sums` (§3.12.4).
    let src = format!(
        "import std::list\n\n{POINT}type Item {{\n  price: Int,\n}}\n\nfn total(items: List<Item>) -> Int {{\n  list::fold(items, 0, fn(acc, item) {{ acc + item.price }})\n}}\n\nfn sums(ps: List<Point>, qs: List<Point>) -> List<Int> {{\n  list::map(list::zip(ps, qs), fn((a, b)) {{ a.x + b.x }})\n}}\n"
    );
    clean(&src);
}

#[test]
fn a_field_needs_a_known_type_at_that_point() {
    // The spec's `get_x` (§3.12.4).
    let src = format!(
        "{POINT}fn main() {{\n  let origin = Point {{ x: 0, y: 0 }}\n  let get_x = fn(p) {{ p.x }}\n  let x = get_x(origin)\n}}\n"
    );
    assert_eq!(errors(&src), ["E0057"]);
    let src = format!(
        "{POINT}fn main() {{\n  let origin = Point {{ x: 0, y: 0 }}\n  let get_x = fn(p: Point) {{ p.x }}\n  let x = get_x(origin)\n}}\n"
    );
    clean(&src);
}

#[test]
fn local_functions_are_checked_at_their_declaration() {
    // The spec's `sum` (§3.12.4): `acc + first` is resolved at the end.
    let src = "fn sum(numbers: List<Int>) -> Int {\n  fn step(acc, rest) {\n    case rest {\n      [] => acc\n      [first, ..tail] => step(acc + first, tail)\n    }\n  }\n  step(0, numbers)\n}\n";
    let b = clean(src);
    let body = body(&b, "sum");
    let local_fn = exprs(body, |k| matches!(k, ExprKind::LocalFn { .. }));
    assert_eq!(local_fn.len(), 1);
    assert_eq!(local_fn[0].ty, Ty::NIL);
    let ExprKind::LocalFn { local, params, .. } = &local_fn[0].kind else {
        panic!()
    };
    assert_eq!(
        body.local(*local).unwrap().ty,
        b.globals
            .types
            .func([Ty::INT, named(&b, builtin::LIST, &[Ty::INT])], Ty::INT)
    );
    assert!(params.iter().all(|p| p.origin == ParamOrigin::LocalFn));
    // The call inside its own body is through `SelfRef` (D-110), the call
    // after it through the local.
    let self_refs = exprs(body, |k| matches!(k, ExprKind::SelfRef { .. }));
    assert_eq!(self_refs.len(), 1);
    let value_calls = exprs(body, |k| {
        matches!(
            k,
            ExprKind::Call {
                callee: Callee::Value(_),
                ..
            }
        )
    });
    assert_eq!(value_calls.len(), 2);
    // Monomorphic (D-123): one type for every call.
    assert_eq!(
        errors("fn f() {\n  fn id(x) {\n    x\n  }\n  let a = id(1)\n  let b = id(\"a\")\n}\n"),
        ["E0047"]
    );
}

#[test]
fn local_bindings_are_monomorphic() {
    // The spec's `wrap` (§3.12.3).
    let b = check(
        "fn main() {\n  let wrap = fn(x) { [x] }\n  let a = wrap(1)\n  let b = wrap(\"hi\")\n}\n",
    );
    assert_eq!(
        b.messages(),
        [
            "E0047: mismatched types [\"expected `{integer}`, found `String`\", \"arguments to this function are incorrect\"]"
        ],
        "{}",
        b.rendered()
    );
}

// ---- 4. expected types and conversions -----------------------------------

const SHAPES: &str = "interface Shape {\n  fn area(Self) -> Float\n}\n\ntype Circle {\n  radius: Float,\n}\n\ntype Square {\n  side: Float,\n}\n\nfn area(shape: Circle) -> Float {\n  shape.radius\n}\n\n";

#[test]
fn never_takes_the_expected_type() {
    // The spec's `case` with `panic` (§3.12.5).
    let b = clean(
        "fn f(x: Option<Int>) -> Int {\n  let n = case x {\n    Some(v) => v\n    None => panic\n  }\n  n\n}\n",
    );
    let body = body(&b, "f");
    let n = body
        .locals
        .iter()
        .find(|l| b.globals.names.get(l.name) == "n")
        .unwrap();
    assert_eq!(n.ty, Ty::INT);
    clean("fn g() -> String {\n  let s: String = todo\n  panic as \"never\"\n}\n");
    // Without an expected type, `Never` stays `Never`.
    let b = clean("fn h() -> Int {\n  let x = panic\n  x\n}\n");
    let body = body_of_fn(&b, "h");
    assert_eq!(
        body.local(cheby_resolve::LocalId::from_u32(0)).unwrap().ty,
        Ty::NEVER
    );
    // It does not convert inside another type (D-235).
    let src = "fn unreachable(what: String) -> Never {\n  panic as \"unreachable: \" + what\n}\n\nfn main() {\n  let f: fn(String) -> Int = unreachable\n  let g: fn(String) -> Int = fn(s) { unreachable(s) }\n}\n";
    let b = check(src);
    assert_eq!(
        b.messages(),
        [
            "E0047: mismatched types [\"expected `fn(String) -> Int`, found `fn(String) -> Never`\", \"expected because of this annotation\"]"
        ],
        "{}",
        b.rendered()
    );
}

#[test]
fn values_convert_to_dyn_where_dyn_is_expected() {
    // The spec's `as_shape` (§3.12.5).
    let b = clean(&format!(
        "{SHAPES}fn as_shape(circle: Circle) -> dyn Shape {{\n  circle\n}}\n"
    ));
    let body = body(&b, "as_shape");
    let coerced = exprs(body, |k| matches!(k, ExprKind::DynCoerce { .. }));
    assert_eq!(coerced.len(), 1);
    let ExprKind::DynCoerce { iface, dict, .. } = &coerced[0].kind else {
        panic!()
    };
    let shape = b.def_in("app", cheby_iface::Namespace::Type, "Shape");
    assert_eq!(*iface, shape);
    assert!(matches!(dict, Dict::Impl { .. }), "{dict:?}");
    assert!(matches!(
        b.globals.types.kind(coerced[0].ty),
        TyKind::Dyn { .. }
    ));
    // List elements, constructor fields and the right of an annotation.
    clean(&format!(
        "{SHAPES}fn f(circle: Circle, circles: List<Circle>) {{\n  let shapes: List<dyn Shape> = [circle, circle]\n  let maybe: Option<dyn Shape> = Some(circle)\n  let empty: List<dyn Shape> = []\n  let one: dyn Shape = circle\n  let again: dyn Shape = one\n}}\n"
    ));
}

/// `dyn I` converts where `dyn J` is expected when `I` embeds `J`, and a
/// whole list converts with `list::map` and a closure (§3.12.5).
#[test]
fn dyn_values_convert_to_embedded_interfaces() {
    let src = format!(
        "import std::list\n\ninterface Named {{\n  fn name(Self) -> String\n}}\n\ninterface Labeled: Named {{\n  fn label(Self) -> String\n}}\n\n{SHAPES}fn widen(x: dyn Labeled) -> dyn Named {{\n  x\n}}\n\nfn convert(circles: List<Circle>) -> List<dyn Shape> {{\n  let converted: List<dyn Shape> = list::map(circles, fn(c) -> dyn Shape {{ c }})\n  converted\n}}\n"
    );
    let b = clean(&src);
    let body = body(&b, "widen");
    let coerced = exprs(body, |k| matches!(k, ExprKind::DynCoerce { .. }));
    assert_eq!(coerced.len(), 1);
    let ExprKind::DynCoerce { dict, .. } = &coerced[0].kind else {
        panic!()
    };
    assert!(matches!(dict, Dict::Dyn { .. }), "{dict:?}");
    let body = body_of_fn(&b, "convert");
    assert_eq!(
        exprs(body, |k| matches!(k, ExprKind::DynCoerce { .. })).len(),
        1
    );
}

#[test]
fn nothing_converts_inside_other_types() {
    let b = check(&format!(
        "{SHAPES}fn f(circles: List<Circle>) {{\n  let all: List<dyn Shape> = circles\n}}\n"
    ));
    assert_eq!(b.codes(), ["E0047"], "{}", b.rendered());
    // Without an expected type nothing converts, and the mismatch suggests
    // a `dyn` element type.
    let b = check(&format!(
        "{SHAPES}fn area(shape: Square) -> Float {{\n  shape.side\n}}\n\nfn f(circle: Circle, square: Square) {{\n  let mixed = [circle, square]\n}}\n"
    ));
    assert!(b.codes().contains(&"E0047".to_owned()), "{}", b.rendered());
    assert!(
        b.rendered().contains("annotate the list as `List<dyn I>`"),
        "{}",
        b.rendered()
    );
}

#[test]
fn a_dyn_conversion_needs_a_known_type_at_the_end() {
    let src = "interface Any\n\nfn f() {\n  let a: dyn Any = []\n}\n";
    assert_eq!(errors(src), ["E0057"]);
    // A numeric literal converts, and its type is fixed later or defaulted
    // (D-237).
    let b = clean(
        "interface Any\n\nfn f() {\n  let x = 1\n  let d: dyn Any = x\n  let e: dyn Any = 2\n}\n",
    );
    let body = body(&b, "f");
    let coerced = exprs(body, |k| matches!(k, ExprKind::DynCoerce { .. }));
    assert_eq!(coerced.len(), 2);
    for c in coerced {
        let ExprKind::DynCoerce { value, .. } = c.kind else {
            panic!()
        };
        assert_eq!(body.expr(value).unwrap().ty, Ty::INT);
    }
}

// ---- 5. calls -------------------------------------------------------------

#[test]
fn constructor_calls() {
    let b = clean("fn f() -> Option<Int> {\n  Some(1)\n}\n");
    let body = body(&b, "f");
    let call = exprs(body, |k| matches!(k, ExprKind::Call { .. }));
    assert_eq!(
        call[0].kind,
        ExprKind::Call {
            callee: Callee::Ctor {
                ctor: builtin::SOME,
                ty_args: Box::new([Ty::INT]),
            },
            args: Box::new([call[0].kind.children()[0]]),
        }
    );
    // A named-field constructor is never called positionally or used as a
    // value (D-138).
    let b = check(&format!(
        "{POINT}fn f() {{\n  let p = Point(1, 2)\n  let q = Point\n}}\n"
    ));
    assert_eq!(b.codes(), ["E0059", "E0059"], "{}", b.rendered());
    assert!(b.messages()[0].contains("`Point` has named fields"));
}

#[test]
fn bounded_calls_pass_dictionaries() {
    let src = "fn describe<T: Show>(x: T) -> String {\n  \"{x}\"\n}\n\nfn main() {\n  let s = describe(\"text\")\n}\n";
    let b = clean(src);
    let body = body(&b, "main");
    let describe = b.def("app", "describe");
    let call = exprs(
        body,
        |k| matches!(k, ExprKind::Call { callee: Callee::Fn { def, .. }, .. } if *def == describe),
    );
    let ExprKind::Call {
        callee: Callee::Fn { ty_args, dicts, .. },
        ..
    } = &call[0].kind
    else {
        panic!()
    };
    assert_eq!(&ty_args[..], [Ty::STRING]);
    assert_eq!(dicts.len(), 1);
    assert!(
        matches!(&dicts[0], Dict::Impl { iface, ty, .. } if *iface == builtin::SHOW && *ty == Ty::STRING),
        "{dicts:?}"
    );
    // Inside, the bound's own dictionary.
    let inner = body_of_fn(&b, "describe");
    let interp = exprs(inner, |k| matches!(k, ExprKind::Interpolate { .. }));
    let ExprKind::Interpolate { parts } = &interp[0].kind else {
        panic!()
    };
    assert!(
        matches!(
            &parts[..],
            [InterpPart::Value {
                show: Some(Callee::Iface { .. }),
                ..
            }]
        ),
        "{parts:?}"
    );
    // A type without `show` does not satisfy the bound.
    let b = check(&format!(
        "{POINT}fn describe<T: Show>(x: T) -> String {{\n  \"{{x}}\"\n}}\n\nfn main() {{\n  let s = describe(Point {{ x: 1, y: 2 }})\n}}\n"
    ));
    assert_eq!(b.codes(), ["E0063"], "{}", b.rendered());
}

fn body_of_fn<'b>(b: &'b Build, name: &str) -> &'b Body {
    body(b, name)
}

#[test]
fn interface_calls_resolve_by_the_self_type() {
    let src = format!(
        "{SHAPES}fn total<T: Shape>(shapes: List<T>, first: T) -> Float {{\n  Shape::area(first)\n}}\n\nfn one(c: Circle) -> Float {{\n  Shape::area(c)\n}}\n\nfn dynamic(s: dyn Shape) -> Float {{\n  Shape::area(s)\n}}\n"
    );
    let b = clean(&src);
    let area = b.def("app", "area");
    let callee = |name: &str| {
        let body = body(&b, name);
        let call = exprs(body, |k| matches!(k, ExprKind::Call { .. }));
        let ExprKind::Call { callee, .. } = &call[0].kind else {
            panic!()
        };
        callee.clone()
    };
    assert!(matches!(callee("one"), Callee::Fn { def, .. } if def == area));
    assert!(matches!(callee("total"), Callee::Iface { .. }));
    assert!(matches!(callee("dynamic"), Callee::Dyn { .. }));
    // The result type comes from the interface signature before the end.
    let b = check(&format!(
        "{SHAPES}fn bad(c: Circle) -> String {{\n  Shape::area(c)\n}}\n"
    ));
    assert_eq!(b.codes(), ["E0047"], "{}", b.rendered());
}

// ---- 6. values ------------------------------------------------------------

#[test]
fn items_as_values() {
    let src = "import std::list\n\nfn double(n: Int) -> Int {\n  n * 2\n}\n\nfn show_all<T: Show>(xs: List<T>) -> String {\n  \"\"\n}\n\nfn main() {\n  let f = double\n  let g = show_all\n  let h = g([1])\n  let w: fn(Int) -> Option<Int> = Some\n  let n: Option<Int> = None\n  let s = Show::show\n  let t = s(\"x\")\n}\n";
    let b = clean(src);
    let body = body(&b, "main");
    let fn_refs = exprs(body, |k| matches!(k, ExprKind::FnRef { .. }));
    assert_eq!(fn_refs.len(), 2);
    assert_eq!(fn_refs[0].ty, b.globals.types.func([Ty::INT], Ty::INT));
    let ExprKind::FnRef { ty_args, dicts, .. } = &fn_refs[1].kind else {
        panic!()
    };
    assert_eq!(&ty_args[..], [Ty::INT]);
    assert!(matches!(&dicts[..], [Dict::Impl { .. }]), "{dicts:?}");
    // `Some` and `Show::show` are closures (GAP(interface-fn-value)).
    let closures = exprs(body, |k| matches!(k, ExprKind::Closure { .. }));
    assert_eq!(closures.len(), 2);
    let units = exprs(body, |k| matches!(k, ExprKind::Ctor { .. }));
    assert_eq!(units[0].ty, named(&b, builtin::OPTION, &[Ty::INT]));
}

#[test]
fn constants_are_read_through_const_refs() {
    let src = "const base: Int = 40\n\nconst answer: Int = base + extra\n\nconst extra: Int = 2\n\nfn main() -> Nil {\n  let a = answer\n}\n";
    let b = clean(src);
    let module = b.thir_of("app");
    assert_eq!(module.consts.len(), 3);
    let answer = b.def("app", "answer");
    let base = b.def("app", "base");
    let extra = b.def("app", "extra");
    let answer_def = module.consts.iter().find(|c| c.def == answer).unwrap();
    // Sorted by (module path, name).
    assert_eq!(&answer_def.deps[..], [base, extra]);
    assert_eq!(answer_def.body.ret, Ty::INT);
    let body = body(&b, "main");
    assert!(
        body.exprs
            .iter()
            .any(|e| e.kind == ExprKind::ConstRef { def: answer } && e.ty == Ty::INT)
    );
    // The initializer is checked against the declared type.
    assert_eq!(errors("const name: String = 1\n"), ["E0047"]);
}

// ---- 7. operators ----------------------------------------------------------

#[test]
fn numeric_operators_are_built_in() {
    let b = clean(
        "fn f(a: Int, b: Float, c: U8) -> Bool {\n  let x = a + a * a - a / a % a\n  let y = -b\n  let z = c & c | c ^ c << c >> c\n  a < a && b >= b\n}\n",
    );
    let body = body(&b, "f");
    for e in exprs(body, |k| {
        matches!(k, ExprKind::Binary { .. } | ExprKind::Neg { .. })
    }) {
        match &e.kind {
            ExprKind::Binary { imp, .. } | ExprKind::Neg { imp, .. } => {
                assert_eq!(*imp, OpImpl::Builtin, "{e:?}");
            }
            _ => panic!(),
        }
    }
    // Comparisons give `Bool`, arithmetic the operand type (D-118).
    let cmp = exprs(body, |k| {
        matches!(
            k,
            ExprKind::Binary {
                op: BinOpKind::Lt,
                ..
            }
        )
    });
    assert_eq!(cmp[0].ty, Ty::BOOL);
    let neg = exprs(body, |k| matches!(k, ExprKind::Neg { .. }));
    assert_eq!(neg[0].ty, Ty::FLOAT);
}

#[test]
fn other_operands_go_through_their_module() {
    let b = clean("fn f(a: String, b: String) -> Bool {\n  let c = a + b\n  c < a\n}\n");
    let body = body(&b, "f");
    let add = b.def("std::string", "add");
    let compare = b.def("std::string", "compare");
    let imps: Vec<&OpImpl> = exprs(body, |k| matches!(k, ExprKind::Binary { .. }))
        .into_iter()
        .map(|e| match &e.kind {
            ExprKind::Binary { imp, .. } => imp,
            _ => panic!(),
        })
        .collect();
    assert!(
        matches!(imps[0], OpImpl::Call(Callee::Fn { def, .. }) if *def == add),
        "{imps:?}"
    );
    assert!(
        matches!(imps[1], OpImpl::Call(Callee::Fn { def, .. }) if *def == compare),
        "{imps:?}"
    );
    // A type without `add` is E0063.
    assert_eq!(errors("fn f(a: Bool) -> Bool {\n  a + a\n}\n"), ["E0063"]);
}

#[test]
fn logical_operators_take_bool() {
    assert_eq!(
        errors("fn f(a: Int) -> Bool {\n  a && True\n}\n"),
        ["E0047"]
    );
    assert_eq!(errors("fn f(a: Int) -> Bool {\n  !a\n}\n"), ["E0047"]);
    // `==` needs one type.
    assert_eq!(
        errors("fn f(a: Int, b: String) -> Bool {\n  a == b\n}\n"),
        ["E0047"]
    );
    let b = clean("fn f(a: String, b: String) -> Bool {\n  a == b\n}\n");
    let body = body(&b, "f");
    let eq = exprs(body, |k| matches!(k, ExprKind::Binary { .. }));
    assert!(matches!(
        eq[0].kind,
        ExprKind::Binary {
            imp: OpImpl::Builtin,
            ..
        }
    ));
}

#[test]
fn non_overloadable_operators_need_their_types() {
    let b = check(
        "fn f(a: Bool, s: String, x: Float) {\n  let p = a & a\n  let q = s % s\n  let r = x << x\n  let t = x % x\n}\n",
    );
    assert_eq!(
        b.messages(),
        [
            "E0072: `%` cannot be applied to `String` [\"`%` on `String`\"]",
            "E0072: `&` cannot be applied to `Bool` [\"`&` on `Bool`\"]",
            "E0072: `<<` cannot be applied to `Float` [\"`<<` on `Float`\"]",
        ],
        "{}",
        b.rendered()
    );
    assert!(b.rendered().contains("help: for logical and, write `&&`"));
}

// ---- 8. interpolation ------------------------------------------------------

#[test]
fn interpolation_shows_values() {
    let src = format!(
        "{POINT}fn f(name: String, p: Point, n: Int) -> String {{\n  \"hello {{name}}, at {{p.x}} \\u{{41}} {{p:?}}\"\n}}\n"
    );
    let b = clean(&src);
    let body = body(&b, "f");
    let interp = exprs(body, |k| matches!(k, ExprKind::Interpolate { .. }));
    let ExprKind::Interpolate { parts } = &interp[0].kind else {
        panic!()
    };
    let string_show = b.def("std::string", "show");
    let int_show = b.def("std::int", "show");
    assert!(matches!(&parts[0], InterpPart::Text(t) if &**t == "hello "));
    assert!(
        matches!(&parts[1], InterpPart::Value { debug: false, show: Some(Callee::Fn { def, .. }), .. } if *def == string_show)
    );
    assert!(matches!(&parts[2], InterpPart::Text(t) if &**t == ", at "));
    assert!(
        matches!(&parts[3], InterpPart::Value { debug: false, show: Some(Callee::Fn { def, .. }), expr } if *def == int_show && matches!(body.expr(*expr).unwrap().kind, ExprKind::Field { .. }))
    );
    assert!(matches!(&parts[4], InterpPart::Text(t) if &**t == " A "));
    assert!(matches!(
        &parts[5],
        InterpPart::Value {
            debug: true,
            show: None,
            ..
        }
    ));
    // A type without `show` needs `{x:?}`.
    let b = check(&format!(
        "{POINT}fn f(p: Point) -> String {{\n  \"{{p}}\"\n}}\n"
    ));
    assert_eq!(b.codes(), ["E0063"], "{}", b.rendered());
    assert!(b.rendered().contains("{p:?}"), "{}", b.rendered());
}

// ---- 9. desugaring --------------------------------------------------------

#[test]
fn use_becomes_a_call_with_a_callback() {
    let src = "import std::result\n\nfn parse(text: String) -> Result<Int, String> {\n  Ok(1)\n}\n\nfn both(a: String, b: String) -> Result<Int, String> {\n  use x <- result::try(parse(a))\n  use y <- result::try(parse(b))\n  Ok(x + y)\n}\n\nfn run(f: fn() -> Int) -> Int {\n  f()\n}\n\nfn zero() -> Int {\n  use <- run\n  0\n}\n";
    let b = clean(src);
    let body = body(&b, "both");
    let try_ = b.def("std::result", "try");
    let calls = exprs(
        body,
        |k| matches!(k, ExprKind::Call { callee: Callee::Fn { def, .. }, .. } if *def == try_),
    );
    assert_eq!(calls.len(), 2);
    for call in calls {
        let ExprKind::Call { args, .. } = &call.kind else {
            panic!()
        };
        let ExprKind::Closure { params, .. } = &body.expr(args[1]).unwrap().kind else {
            panic!("{:?}", body.expr(args[1]).unwrap());
        };
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].origin, ParamOrigin::Use);
        assert_eq!(params[0].pat.ty, Ty::INT);
    }
    // No binders: a zero-parameter callback.
    let body = body_of_fn(&b, "zero");
    let closure = exprs(body, |k| matches!(k, ExprKind::Closure { .. }));
    let ExprKind::Closure { params, .. } = &closure[0].kind else {
        panic!()
    };
    assert!(params.is_empty());
    assert_eq!(closure[0].ty, b.globals.types.func([], Ty::INT));
}

/// A `use` binds as many values as the callback takes (§5.10).
#[test]
fn use_binders_match_the_callback() {
    let src = "fn each(x: Int, f: fn(Int) -> Int) -> Int {\n  f(x)\n}\n\nfn g() -> Int {\n  use a, b <- each(1)\n  a + b\n}\n";
    let b = check(src);
    assert_eq!(
        b.messages(),
        [
            "E0047: mismatched types [\"this `use` has 2 binders, but the callback takes 1 parameter\", \"the function called by this `use`\"]"
        ],
        "{}",
        b.rendered()
    );
}

#[test]
fn captures_evaluate_their_arguments_once() {
    let src = "import std::list\nimport std::int\n\nfn next_id() -> Int {\n  1\n}\n\nfn f(numbers: List<Int>) -> List<Int> {\n  let add_next = int::add(next_id(), _)\n  list::map(numbers, int::add(1, _))\n}\n";
    let b = clean(src);
    let body = body(&b, "f");
    // `next_id()` is bound to a synthetic local before the closure.
    let blocks = exprs(
        body,
        |k| matches!(k, ExprKind::Block { tail: Some(_), stmts } if !stmts.is_empty() && matches!(stmts[0], Stmt::Let { .. })),
    );
    assert_ne!(blocks.len(), 0);
    let synthetic: Vec<&str> = body
        .locals
        .iter()
        .filter(|l| l.synthetic)
        .map(|l| b.globals.names.get(l.name))
        .collect();
    assert_eq!(synthetic, ["%arg", "%hole", "%hole"]);
    let params: Vec<ParamOrigin> = exprs(body, |k| matches!(k, ExprKind::Closure { .. }))
        .iter()
        .flat_map(|e| match &e.kind {
            ExprKind::Closure { params, .. } => params.iter().map(|p| p.origin).collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(params, [ParamOrigin::Capture, ParamOrigin::Capture]);
    // A literal argument needs no local.
    let closures = exprs(body, |k| matches!(k, ExprKind::Closure { .. }));
    assert_eq!(closures[1].ty, b.globals.types.func([Ty::INT], Ty::INT));
}

#[test]
fn pipes_are_calls() {
    let src = "import std::list\n\nfn make(n: Int) -> fn(List<Int>) -> Int {\n  list::length\n}\n\nfn f(xs: List<Int>) -> Int {\n  let a = xs |> list::length\n  let b = xs |> list::map(_, fn(x) { x + 1 }) |> list::length\n  let c = xs |> list::fold(_, 0, fn(acc, x) { acc + x })\n  let d = [1] |> list::fold(xs, _, fn(acc, x) { acc })\n  let e = xs |> (make(1))\n  a + b + c + e + list::length(d)\n}\n";
    let b = clean(src);
    let body = body(&b, "f");
    let synthetic: Vec<&str> = body
        .locals
        .iter()
        .filter(|l| l.synthetic)
        .map(|l| b.globals.names.get(l.name))
        .collect();
    // `[1]` comes after `xs` in `fold(xs, _, …)`, so it is bound first;
    // `xs` before `make(1)` is a local and needs no binding.
    assert_eq!(synthetic, ["%piped"]);
    let fold = b.def("std::list", "fold");
    let folds = exprs(
        body,
        |k| matches!(k, ExprKind::Call { callee: Callee::Fn { def, .. }, .. } if *def == fold),
    );
    assert_eq!(folds.len(), 2);
    assert!(
        folds
            .iter()
            .all(|f| f.ty == Ty::INT || f.ty == named(&b, builtin::LIST, &[Ty::INT]))
    );
}

#[test]
fn parentheses_leave_no_node() {
    let b = clean("fn f() -> Int {\n  (1 + (2))\n}\n");
    let body = body(&b, "f");
    assert_eq!(body.exprs.len(), 4);
}

// ---- 10. statements --------------------------------------------------------

#[test]
fn statements_check() {
    let src = "fn f(x: Option<Int>) {\n  let assert Some(n) = x as \"must be there\"\n  assert n > 0 as \"positive\"\n  let (a, b): (Int, String) = (n, \"b\")\n  fn g((p, q)) {\n    p + q\n  }\n  let s = g((a, n))\n}\n";
    let b = clean(src);
    let body = body(&b, "f");
    let ExprKind::Block { stmts, tail } = &body.expr(body.value).unwrap().kind else {
        panic!()
    };
    assert!(tail.is_none());
    assert!(matches!(
        stmts[0],
        Stmt::LetAssert {
            message: Some(_),
            ..
        }
    ));
    assert!(matches!(
        stmts[1],
        Stmt::Assert {
            message: Some(_),
            ..
        }
    ));
    assert!(matches!(stmts[2], Stmt::Let { .. }));
    assert!(matches!(stmts[3], Stmt::Expr(_)));
    // The condition is a `Bool`, the messages `String`s.
    assert_eq!(errors("fn f() {\n  assert 1\n}\n"), ["E0047"]);
    assert_eq!(errors("fn f() {\n  assert True as 1\n}\n"), ["E0047"]);
    assert_eq!(
        errors("fn f(x: Option<Int>) {\n  let assert Some(n) = x as Nil\n}\n"),
        ["E0047"]
    );
}

#[test]
fn block_values() {
    let b = clean(
        "fn f() -> Int {\n  let x = {\n    let y = 1\n    y + 1\n  }\n  let z = {\n    let w = 1\n  }\n  x\n}\n",
    );
    let body = body(&b, "f");
    let z = body
        .locals
        .iter()
        .find(|l| b.globals.names.get(l.name) == "z")
        .unwrap();
    assert_eq!(z.ty, Ty::NIL);
}

// ---- 11. panic and todo -------------------------------------------------

#[test]
fn panic_and_todo_are_never() {
    let b = clean("fn f() -> Int {\n  todo\n}\n\nfn g() -> String {\n  panic as \"g\"\n}\n");
    let body = body(&b, "g");
    let panic = exprs(body, |k| matches!(k, ExprKind::Panic { .. }));
    assert_eq!(panic[0].ty, Ty::NEVER);
    assert!(matches!(
        panic[0].kind,
        ExprKind::Panic {
            kind: PanicKind::Panic,
            message: Some(_)
        }
    ));
    assert_eq!(errors("fn f() -> Int {\n  panic as 1\n}\n"), ["E0047"]);
}

// ---- 12. items ------------------------------------------------------------

#[test]
fn main_may_return_a_result() {
    let b = clean("fn main() -> Result<Nil, String> {\n  Ok(Nil)\n}\n");
    let main = b.thir_fn("app", "main").unwrap();
    let Some(EntryInfo::Result { err_ty, show }) = &main.entry else {
        panic!("{:?}", main.entry);
    };
    assert_eq!(*err_ty, Ty::STRING);
    assert!(show.is_some());
    // An error type without `Show` is printed with debug printing (D-182).
    let b = clean(&format!(
        "{POINT}fn main() -> Result<Nil, Point> {{\n  Ok(Nil)\n}}\n"
    ));
    let main = b.thir_fn("app", "main").unwrap();
    assert!(matches!(
        main.entry,
        Some(EntryInfo::Result { show: None, .. })
    ));
}

#[test]
fn tests_return_nil_or_a_result() {
    let src = "test \"nil\" {\n  assert 1 == 1\n}\n\ntest \"result\" {\n  let x: Result<Int, String> = Ok(1)\n  use n <- result_try(x)\n  Ok(Nil)\n}\n\nfn result_try(r: Result<Int, String>, f: fn(Int) -> Result<Nil, String>) -> Result<Nil, String> {\n  case r {\n    Ok(v) => f(v)\n    Err(e) => Err(e)\n  }\n}\n";
    let b = clean(src);
    let tests = &b.thir_of("app").tests;
    assert_eq!(tests.len(), 2);
    assert_eq!(&*tests[0].name, "nil");
    assert_eq!(tests[0].entry, EntryInfo::Nil);
    assert_eq!(tests[0].body.ret, Ty::NIL);
    assert!(matches!(
        &tests[1].entry,
        EntryInfo::Result { err_ty, .. } if *err_ty == Ty::STRING
    ));
    // Anything else is E0073.
    let b = check(
        "fn double(n: Int) -> Int {\n  n * 2\n}\n\ntest \"double doubles\" {\n  double(2) == 4\n}\n",
    );
    assert_eq!(b.codes(), ["E0073"], "{}", b.rendered());
    assert!(b.rendered().contains("write `assert` before it"));
}

// ---- 13. the end of a body ------------------------------------------------

#[test]
fn exhaustiveness_runs_on_bodies_without_errors() {
    assert_eq!(
        errors("fn f(x: Option<Int>) -> Int {\n  case x {\n    Some(n) => n\n  }\n}\n"),
        ["E0061"]
    );
    // Not after another error.
    assert_eq!(
        errors(
            "fn f(x: Option<Int>) -> Int {\n  let y: String = 1\n  case x {\n    Some(n) => n\n  }\n}\n"
        ),
        ["E0047"]
    );
}

#[test]
fn every_type_is_closed_in_thir() {
    let src = "import std::list\n\nfn f(xs: List<Int>) -> List<String> {\n  let ys = list::map(xs, fn(x) { \"{x}\" })\n  let pairs = list::zip(xs, ys)\n  list::map(pairs, fn((n, s)) { s })\n}\n";
    let b = clean(src);
    let body = body(&b, "f");
    let types = &b.globals.types;
    for e in &body.exprs {
        assert!(!crate::infer::contains_error(types, e.ty), "{e:?}");
    }
    for l in &body.locals {
        assert!(!crate::infer::contains_error(types, l.ty), "{l:?}");
    }
    for (_, p) in body.pats() {
        p.walk(&mut |p| assert!(!crate::infer::contains_error(types, p.ty), "{p:?}"));
    }
}
