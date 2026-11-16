//! Tests of the key rules: the rewrite of the call that is a constant's
//! whole initializer, the path and type texts, E0080 at every other
//! mention, the exemption of a key's type argument from E0068, and that
//! the key functions never satisfy an interface
//! (D-1565, D-1572, D-1578, D-1580, D-1581, D-1587, D-1589, D-1591, D-1593).
#![expect(
    clippy::wildcard_enum_match_arm,
    reason = "a test matches only the variants it checks"
)]

use cheby_iface::DefId;

use super::key_path_text;
use crate::test_support::quiet::{build_package, build_with_std};
use crate::test_support::{Build, STD_CONTEXT, build_recording};
use crate::thir::{Body, Callee, ExprKind, Lit};

/// A build of package `app` from `local`, with the test `std::context`.
fn with_context(local: &[(&str, &str)]) -> Build {
    build_with_std(&[("context", STD_CONTEXT)], local)
}

/// The codes of `b`'s errors, sorted.
fn errors(b: &Build) -> Vec<String> {
    b.codes()
        .into_iter()
        .filter(|c| c.starts_with('E'))
        .collect()
}

/// How many diagnostics of `b` have the code `code`.
fn count(b: &Build, code: &str) -> usize {
    b.diags
        .iter()
        .filter(|d| d.code.to_string() == code)
        .count()
}

/// The initializer of the constant `name` of the module at `path`.
fn const_body<'b>(b: &'b Build, path: &str, name: &str) -> &'b Body {
    let def = b.def(path, name);
    &b.thir_of(path)
        .consts
        .iter()
        .find(|c| c.def == def)
        .unwrap_or_else(|| panic!("no THIR for `{path}::{name}`"))
        .body
}

/// What a rewritten key initializer calls: the callee, the type
/// arguments' kinds and the texts of the string arguments, in order.
fn key_call(b: &Build, path: &str, name: &str) -> (DefId, usize, Vec<String>) {
    let body = const_body(b, path, name);
    let root = body.expr(body.value).unwrap();
    let ExprKind::Call {
        callee: Callee::Fn { def, ty_args, .. },
        args,
    } = &root.kind
    else {
        panic!("`{name}` is not a call: {:?}", root.kind);
    };
    let texts = args
        .iter()
        .filter_map(|&arg| match &body.expr(arg).unwrap().kind {
            ExprKind::Lit(Lit::Str(text)) => Some(text.to_string()),
            _ => None,
        })
        .collect();
    (*def, ty_args.len(), texts)
}

const KEYS: &str = r#"//! Keys.

import std::context

pub type Circle {
  Circle(Int),
}

fn show_it(n: Int) -> Option<String> {
  None
}

type PortKey = context::Key<Int>

type Pair = (Int, String)

pub const k: context::Key<Int> = context::key(1)

pub const maybe: context::Key<Option<Int>> = context::key_without_default()

pub const circle: context::Key<Circle> = context::key(Circle(1))

pub const mixed: context::Key<(List<Circle>, fn(Int) -> Option<String>)> =
  context::key(([], show_it))

pub const port: PortKey = context::key(8080)

pub const pair: context::Key<Pair> = context::key((1, "a"))

pub const paren: context::Key<Int> = (context::key(2))

pub const turbo: context::Key<Int> = context::key::<Int>(3)

pub const alias: context::Key<Int> = k
"#;

#[test]
fn a_whole_initializer_calls_the_constructor_with_both_texts() {
    let b = with_context(&[("", KEYS)]);
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    let key_at = b.def("std::context", "key_at");
    let without_at = b.def("std::context", "key_without_default_at");
    let texts = |name: &str| {
        let (def, ty_args, texts) = key_call(&b, "app", name);
        assert_eq!(def, key_at, "`{name}` calls `key_at`");
        assert_eq!(ty_args, 1);
        texts
    };
    // `1` is the first argument, then the path text and the type text.
    let body = const_body(&b, "app", "k");
    let root = body.expr(body.value).unwrap();
    let ExprKind::Call { args, .. } = &root.kind else {
        panic!("not a call");
    };
    assert_eq!(args.len(), 3);
    assert_eq!(body.expr(args[0]).unwrap().kind, ExprKind::Lit(Lit::Int(1)));
    assert_eq!(texts("k"), ["app::k", "Int"]);
    let (def, ty_args, maybe) = key_call(&b, "app", "maybe");
    assert_eq!((def, ty_args), (without_at, 1));
    assert_eq!(maybe, ["app::maybe", "Option<Int>"]);
    assert_eq!(texts("circle"), ["app::circle", "app::Circle"]);
    assert_eq!(
        texts("mixed"),
        [
            "app::mixed",
            "(List<app::Circle>, fn(Int) -> Option<String>)"
        ]
    );
    // Aliases are expanded, in the declared type and inside it.
    assert_eq!(texts("port"), ["app::port", "Int"]);
    assert_eq!(texts("pair"), ["app::pair", "(Int, String)"]);
    // Parentheses pass, and type arguments are a direct call (D-1581).
    assert_eq!(texts("paren"), ["app::paren", "Int"]);
    assert_eq!(texts("turbo"), ["app::turbo", "Int"]);
    // A constant initialized from another key constant reads it.
    let alias = const_body(&b, "app", "alias");
    assert!(matches!(
        alias.expr(alias.value).unwrap().kind,
        ExprKind::ConstRef { .. }
    ));
}

#[test]
fn std_context_itself_uses_a_bare_key() {
    let b = with_context(&[("", "//! Nothing.\n\nimport std::context\n")]);
    let (def, _, texts) = key_call(&b, "std::context", "own_key");
    assert_eq!(def, b.def("std::context", "key_at"));
    assert_eq!(texts, ["std::context::own_key", "Int"]);
    // The declarations of `key` and `key_without_default` and their
    // constructors' bodies mention nothing that is E0080.
    assert_eq!(count(&b, "E0080"), 0, "{}", b.rendered());
}

#[test]
fn an_imported_key_is_not_a_mention() {
    let b = with_context(&[(
        "",
        "//! Imported.\n\nimport std::context::{Key, key, key_without_default}\n\npub const k: Key<Int> = key(1)\n\npub const none: Key<Option<String>> = key_without_default()\n",
    )]);
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    let (def, _, texts) = key_call(&b, "app", "k");
    assert_eq!(def, b.def("std::context", "key_at"));
    assert_eq!(texts, ["app::k", "Int"]);
    let (def, _, texts) = key_call(&b, "app", "none");
    assert_eq!(def, b.def("std::context", "key_without_default_at"));
    assert_eq!(texts, ["app::none", "Option<String>"]);
}

/// A REPL entry's constant has its bare name as its path text, and a
/// reloaded module's has the unmarked path, while the type text keeps the
/// internal module of a session type and of a reloaded type (D-1572,
/// D-1580).
#[test]
fn path_texts_are_mapped_and_type_texts_are_not() {
    let entry = "//! Entry.\n\nimport std::context\n\npub type T {\n  T(Int),\n}\n\npub const k: context::Key<T> = context::key(T(1))\n";
    let std = [("context", STD_CONTEXT)];
    for module in ["<repl:3>", "<repl:3:1>"] {
        let b = build_package(module, &std, &[("", entry)]);
        assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
        let (_, _, texts) = key_call(&b, module, "k");
        assert_eq!(texts, ["k".to_owned(), format!("{module}::T")]);
    }
    let prices = "//! Prices.\n\nimport std::context\n\npub type Item {\n  Item(Int),\n}\n\npub const pool: context::Key<Item> = context::key(Item(1))\n\npub const count: context::Key<Int> = context::key(0)\n";
    let b = with_context(&[("", "//! Root.\n"), ("prices<r1>", prices)]);
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    let (_, _, texts) = key_call(&b, "app::prices<r1>", "pool");
    assert_eq!(texts, ["app::prices::pool", "app::prices<r1>::Item"]);
    let (_, _, texts) = key_call(&b, "app::prices<r1>", "count");
    assert_eq!(texts, ["app::prices::count", "Int"]);
}

#[test]
fn path_text_forms() {
    let path = |segments: &[&str], name: &str| {
        let segments: Vec<String> = segments.iter().map(|&s| s.to_owned()).collect();
        key_path_text(&segments, name)
    };
    assert_eq!(path(&["app"], "k"), "app::k");
    assert_eq!(path(&["app", "db"], "pool"), "app::db::pool");
    assert_eq!(path(&["std", "time"], "clock"), "std::time::clock");
    assert_eq!(path(&["<repl:3>"], "k"), "k");
    assert_eq!(path(&["<repl:12:4>"], "k"), "k");
    assert_eq!(path(&["app", "db<r1>"], "pool"), "app::db::pool");
    assert_eq!(path(&["app<r12>"], "pool"), "app::pool");
    // Only a well-formed mark is left out.
    assert_eq!(path(&["app", "db<r>"], "pool"), "app::db<r>::pool");
    assert_eq!(path(&["app", "db<rx>"], "pool"), "app::db<rx>::pool");
}

const MENTIONS: &str = "//! Mentions.

import std::context

fn in_body() -> context::Key<Int> {
  context::key(1)
}

pub const nested: Option<context::Key<Int>> = Some(context::key(2))

fn as_value() -> fn(Int) -> context::Key<Int> {
  context::key
}

pub const piped: context::Key<Int> = 3
  |> context::key

pub const blocked: context::Key<Int> = { context::key(5) }

fn used() -> context::Key<fn() -> Int> {
  use <- context::key
  6
}

fn without_default() -> context::Key<Option<Int>> {
  context::key_without_default()
}
";

const INTERPOLATED: &str = r#"//! An interpolation root.

import std::context::{key}

fn text() -> String {
  "{key}"
}
"#;

#[test]
fn every_other_mention_is_e0080() {
    let b = with_context(&[("", MENTIONS), ("interp", INTERPOLATED)]);
    let rendered = b.rendered();
    // A call in a body, a nested call, a function value, a pipe into the
    // bare path, a block, a `use`, a call of `key_without_default`, and an
    // interpolation root: one each. `key(_)`, in a pipe or not, is E0027
    // before resolution (D-245), so it never reaches the checker.
    assert_eq!(count(&b, "E0080"), 8, "{rendered}");
    let messages = b.messages();
    let e0080: Vec<&String> = messages.iter().filter(|m| m.starts_with("E0080")).collect();
    assert!(e0080.iter().any(|m| m.contains(
        "`context::key_without_default` may only be called as the whole initializer of a constant"
    )));
    assert!(e0080.iter().any(|m| m.contains("this uses it as a value")));
    assert!(
        rendered.contains(
            "help: make the call the whole initializer of a `const`, as in `const name: context::Key<Int> = context::key(0)`"
        ),
        "{rendered}"
    );
    // Each body with the error is dropped, so no constant is rewritten.
    for name in ["nested", "piped", "blocked"] {
        let def = b.def("app", name);
        assert!(!b.thir_of("app").consts.iter().any(|c| c.def == def));
    }
}

#[test]
fn a_keys_type_argument_is_not_looked_at_for_e0068() {
    let b = with_context(&[(
        "",
        "//! Keys of functions.\n\nimport std::context\n\nfn make() -> Int {\n  1\n}\n\npub const f: context::Key<fn() -> Int> = context::key(make)\n\npub const fs: List<context::Key<fn() -> Int>> = [f]\n\npub const pair: (context::Key<fn() -> Int>, Int) = (f, 1)\n\npub const g: fn() -> Int = make\n",
    )]);
    assert_eq!(
        b.messages()
            .into_iter()
            .filter(|m| m.starts_with('E'))
            .collect::<Vec<_>>(),
        ["E0068: the constant `g` has a type that contains a function [\"\"]"],
    );
}

/// The facts keep the function that is written, for hover and
/// go-to-definition, while THIR calls the constructor.
#[test]
fn the_facts_name_the_written_function() {
    let b = build_recording(
        &[("context", STD_CONTEXT)],
        &[(
            "",
            "//! Facts.\n\nimport std::context\n\npub const k: context::Key<Int> = context::key(1)\n",
        )],
    );
    let key = b.def("std::context", "key");
    let k = b.def("app", "k");
    let facts = b.facts[&k.module].body(k.index).unwrap();
    let call = facts
        .calls
        .iter()
        .find(|c| c.def == Some(key))
        .expect("a call fact for `key`");
    assert!(matches!(
        call.resolved,
        Some(Callee::Fn { def, .. }) if def == key
    ));
    let (def, _, _) = key_call(&b, "app", "k");
    assert_eq!(def, b.def("std::context", "key_at"));
}

/// A conversion to `dyn` around a key's whole initializer is E0068 by the
/// constant's type, and the call inside it is still rewritten, not an
/// internal error.
#[test]
fn a_key_converted_to_dyn_is_rewritten_inside_the_conversion() {
    let b = with_context(&[(
        "",
        "//! A key as `dyn`.\n\nimport std::context\n\npub interface Marker {}\n\npub const d: dyn Marker = context::key(1)\n",
    )]);
    assert_eq!(errors(&b), ["E0068"], "{}", b.rendered());
    let body = const_body(&b, "app", "d");
    let ExprKind::DynCoerce { value, .. } = body.expr(body.value).unwrap().kind else {
        panic!("not a conversion");
    };
    let ExprKind::Call {
        callee: Callee::Fn { def, .. },
        args,
    } = &body.expr(value).unwrap().kind
    else {
        panic!("not a call");
    };
    assert_eq!(*def, b.def("std::context", "key_at"));
    let texts: Vec<String> = args
        .iter()
        .filter_map(|&arg| match &body.expr(arg).unwrap().kind {
            ExprKind::Lit(Lit::Str(text)) => Some(text.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["app::d", "Int"]);
}

/// Reaching `key` through an interface: an interface call in a body, a
/// bound of a generic function, an interface function as a value, a
/// conversion to `dyn` in a body and in a constant, and `Context`, another
/// type of `std::context` (D-1593).
const WRAPPED: &str = "//! Keys through an interface.

import std::context

pub interface Wrap {
  fn key(Self) -> context::Key<Self>
}

pub interface Boxed {
  fn key(Self) -> context::Key<context::Key<Int>>
}

fn direct(base: context::Key<Int>) -> context::Key<context::Key<Int>> {
  Wrap::key(base)
}

fn generic<T: Wrap>(x: T) -> context::Key<T> {
  Wrap::key(x)
}

fn bounded(base: context::Key<Int>) -> context::Key<context::Key<Int>> {
  generic(base)
}

fn as_value() -> fn(context::Key<Int>) -> context::Key<context::Key<Int>> {
  Wrap::key
}

fn boxed(base: context::Key<Int>) -> dyn Boxed {
  base
}

pub const one: context::Key<Int> = context::key(1)

pub const d: dyn Boxed = one

fn of_context(c: context::Context) -> context::Key<context::Context> {
  Wrap::key(c)
}
";

/// An interface function named `key` that is E0066 at its declaration (no
/// `Self` in a parameter): its uses report nothing more.
const NO_SELF: &str = "//! An interface function without `Self` in a parameter.

import std::context

pub interface NoSelf {
  fn key(Int) -> context::Key<Int>
}

fn no_self<T: NoSelf>(x: T) -> Int {
  1
}

fn use_no_self(base: context::Key<Int>) -> Int {
  no_self(base)
}
";

/// The same interfaces over a user's own `Key` and `key`, which count.
const OWN_KEYS: &str = "//! A user's own keys, which satisfy the interface.

import app::mykeys

pub interface Wrap {
  fn key(Self) -> mykeys::Key<Self>
}

pub interface Boxed {
  fn key(Self) -> mykeys::Key<mykeys::Key<Int>>
}

fn direct(base: mykeys::Key<Int>) -> mykeys::Key<mykeys::Key<Int>> {
  Wrap::key(base)
}

fn generic<T: Wrap>(x: T) -> mykeys::Key<T> {
  Wrap::key(x)
}

fn bounded(base: mykeys::Key<Int>) -> mykeys::Key<mykeys::Key<Int>> {
  generic(base)
}

fn as_value() -> fn(mykeys::Key<Int>) -> mykeys::Key<mykeys::Key<Int>> {
  Wrap::key
}

fn boxed(base: mykeys::Key<Int>) -> dyn Boxed {
  base
}
";

const MY_KEYS: &str = "//! A user's own `Key` and `key`.

pub type Key<T> {
  Key(T),
}

pub fn key<T>(default: T) -> Key<T> {
  Key(default)
}
";

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

#[test]
fn the_key_functions_never_satisfy_an_interface() {
    let b = with_context(&[("", WRAPPED)]);
    let rendered = b.rendered();
    let note = "`context::key` exists but does not count, because a key is made only by the call \
                that is a constant's whole initializer (§10.11, D-1593)";
    let unsatisfied: Vec<_> = b
        .diags
        .iter()
        .filter(|d| d.code.to_string() == "E0063")
        .collect();
    // `direct`, `bounded`, `as_value`, `boxed`, `d` and `of_context`.
    assert_eq!(unsatisfied.len(), 6, "{rendered}");
    for diag in &unsatisfied {
        assert!(diag.notes.iter().any(|n| n == note), "{rendered}");
    }
    let messages = b.messages();
    for want in [
        "E0063: `context::Key<Int>` does not satisfy `Wrap`",
        "E0063: `context::Key<Int>` does not satisfy `Boxed`",
        "E0063: `context::Context` does not satisfy `Wrap`",
    ] {
        assert!(
            messages.iter().any(|m| m.starts_with(want)),
            "{want}\n{rendered}"
        );
    }
    // Not a mention, so not E0080. `d`'s type is E0068 by itself.
    assert_eq!(count(&b, "E0080"), 0, "{rendered}");
    assert_eq!(count(&b, "E0068"), 1, "{rendered}");
    assert_eq!(errors(&b).len(), 7, "{rendered}");
    for name in ["direct", "bounded", "as_value", "boxed", "of_context"] {
        assert!(b.thir_fn("app", name).is_none(), "`{name}` was accepted");
    }
    // `generic` itself is fine: its bound is a dictionary parameter.
    assert!(b.thir_fn("app", "generic").is_some());
    assert!(!thir_names(&b, b.def("std::context", "key")));
    assert!(!thir_names(
        &b,
        b.def("std::context", "key_without_default")
    ));
}

/// The rule is about `std::context` only: a module's own `Key` and `key`
/// satisfy the same interfaces, and the dictionaries hold its `key`.
#[test]
fn an_interface_function_without_self_reports_nothing_more() {
    let b = with_context(&[("", NO_SELF)]);
    let rendered = b.rendered();
    assert_eq!(count(&b, "E0066"), 1, "{rendered}");
    assert_eq!(count(&b, "E0063"), 0, "{rendered}");
    assert_eq!(errors(&b).len(), 1, "{rendered}");
}

#[test]
fn a_users_own_key_function_still_satisfies() {
    let b = with_context(&[("", OWN_KEYS), ("mykeys", MY_KEYS)]);
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    for name in ["direct", "bounded", "as_value", "boxed"] {
        assert!(b.thir_fn("app", name).is_some(), "`{name}` was rejected");
    }
    assert!(thir_names(&b, b.def("app::mykeys", "key")));
}

/// A std module whose function carries `@std_initializer` under another
/// name than the key functions' (D-1599).
const STD_TAGS: &str = r#"//! Tags, for the checker's tests.

/// A tag: its constant's path and type as text, and a value.
pub type Tag<T> {
  Tag { path: String, type_text: String, value: T },
}

/// A tag of `value`.
@std_initializer(make_here)
pub fn make<T>(value: T) -> Tag<T> {
  make_here(value, "", "")
}

fn make_here<T>(value: T, path: String, type_text: String) -> Tag<T> {
  Tag { path: path, type_text: type_text, value: value }
}
"#;

const TAGGED: &str = "//! Tags.

import std::tags

pub const t: tags::Tag<Int> = tags::make(1)

pub fn body() -> tags::Tag<Int> {
  tags::make(2)
}

interface Wrap {
  fn make(Self) -> tags::Tag<Self>
}

fn direct(base: tags::Tag<Int>) -> tags::Tag<tags::Tag<Int>> {
  Wrap::make(base)
}
";

/// The rules follow the attribute, not a name: a function of another std
/// module that carries it is rewritten as a constant's whole initializer,
/// is E0080 elsewhere, with a message and an example built from its own
/// path, and never satisfies an interface (D-1565, D-1593, D-1599).
#[test]
fn any_function_with_the_attribute_follows_the_key_rules() {
    let b = build_with_std(&[("tags", STD_TAGS)], &[("", TAGGED)]);
    let rendered = b.rendered();
    assert_eq!(errors(&b), ["E0063", "E0080"], "{rendered}");
    let (def, ty_args, texts) = key_call(&b, "app", "t");
    assert_eq!(def, b.def("std::tags", "make_here"));
    assert_eq!(ty_args, 1);
    assert_eq!(texts, ["app::t", "Int"]);
    let messages = b.messages();
    assert!(
        messages.iter().any(|m| m.starts_with(
            "E0080: `tags::make` may only be called as the whole initializer of a constant"
        )),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "help: make the call the whole initializer of a `const`, as in `const name: tags::Tag<Int> = tags::make(0)`"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "`tags::make` exists but does not count, because a key is made only by the call that is a constant's whole initializer"
        ),
        "{rendered}"
    );
    for name in ["body", "direct"] {
        assert!(b.thir_fn("app", name).is_none(), "`{name}` was accepted");
    }
    assert!(!thir_names(&b, b.def("std::tags", "make")));
}

const PLAIN_KEYS: &str = "//! Keys without the attribute.

import std::context

pub const k: context::Key<Int> = context::key(1)

pub fn body() -> context::Key<Int> {
  context::key(2)
}

pub fn value() -> fn(Int) -> context::Key<Int> {
  context::key
}

interface Wrap {
  fn key(Self) -> context::Key<Self>
}

fn direct(base: context::Key<Int>) -> context::Key<context::Key<Int>> {
  Wrap::key(base)
}
";

/// A function named `key` in `std::context` without `@std_initializer` is
/// an ordinary function: no rewrite, no E0080, and it satisfies an
/// interface (D-1599).
#[test]
fn a_key_function_without_the_attribute_is_ordinary() {
    let plain = STD_CONTEXT
        .replace("@std_initializer(key_at)\n", "")
        .replace("@std_initializer(key_without_default_at)\n", "");
    assert!(!plain.contains("@std_initializer"));
    let b = build_with_std(&[("context", &plain)], &[("", PLAIN_KEYS)]);
    assert_eq!(errors(&b), Vec::<String>::new(), "{}", b.rendered());
    let (def, _, texts) = key_call(&b, "app", "k");
    assert_eq!(def, b.def("std::context", "key"));
    assert_eq!(texts, Vec::<String>::new());
    for name in ["body", "value", "direct"] {
        assert!(b.thir_fn("app", name).is_some(), "`{name}` was rejected");
    }
}

/// A std module whose constructors are unusable: `broken_at`'s signature
/// names an unknown type, and `short_at` lacks the type text's parameter.
const STD_BROKEN: &str = "//! Tags with faulty constructors, for the checker's tests.

/// A tag.
pub type Tag<T> {
  Tag { value: T },
}

/// A tag whose constructor has an error in its signature.
@std_initializer(broken_at)
pub fn broken<T>(value: T) -> Tag<T> {
  Tag { value: value }
}

fn broken_at<T>(value: T, path: Strng, type_text: String) -> Tag<T> {
  Tag { value: value }
}

/// A tag whose constructor has the wrong shape.
@std_initializer(short_at)
pub fn short<T>(value: T) -> Tag<T> {
  Tag { value: value }
}

fn short_at<T>(value: T, path: String) -> Tag<T> {
  Tag { value: value }
}

/// A tag whose constructor is `priv`.
@std_initializer(hidden_at)
pub fn hidden<T>(value: T) -> Tag<T> {
  Tag { value: value }
}

priv fn hidden_at<T>(value: T, path: String, type_text: String) -> Tag<T> {
  Tag { value: value }
}
";

const USES_BROKEN: &str = "//! Uses of faulty constructors.

import std::tags

pub const a: tags::Tag<Int> = tags::broken(1)

pub const b: tags::Tag<Int> = tags::short(2)

pub const c: tags::Tag<Int> = tags::hidden(3)
";

/// A constructor that its own module reports, for an error in its
/// signature or as E0045 for its shape or a `priv` visibility, makes a whole initializer that
/// uses it silent and bodiless, never an internal error or an ill-typed
/// call (D-1599).
#[test]
fn a_faulty_constructor_is_reported_once_in_its_module() {
    let b = build_with_std(&[("tags", STD_BROKEN)], &[("", USES_BROKEN)]);
    let rendered = b.rendered();
    assert_eq!(count(&b, "E0045"), 2, "{rendered}");
    assert!(
        b.messages()
            .iter()
            .any(|m| m
                .starts_with("E0045: `short_at` cannot be the initializer constructor of `short`")),
        "{rendered}"
    );
    assert!(
        b.messages()
            .iter()
            .any(|m| m.starts_with("E0045: the initializer constructor `hidden_at` of `hidden`")),
        "{rendered}"
    );
    assert!(rendered.contains("Strng"), "{rendered}");
    assert!(!rendered.contains("E0080"), "{rendered}");
    let consts = &b.thir_of("app").consts;
    for name in ["a", "b", "c"] {
        let def = b.def("app", name);
        assert!(consts.iter().all(|c| c.def != def), "`{name}` has a body");
    }
}

const STD_RESULTS: &str = "//! Initializer functions whose results have not one type argument.

/// A tag without a type argument.
pub type Tag {
  Tag,
}

/// A pair with two.
pub type Pair<A, B> {
  Pair(A, B),
}

@std_initializer(int_at)
pub fn int(value: Int) -> Int {
  value
}

fn int_at(value: Int, path: String, type_text: String) -> Int {
  value
}

@std_initializer(tag_at)
pub fn tag() -> Tag {
  Tag
}

fn tag_at(path: String, type_text: String) -> Tag {
  Tag
}

@std_initializer(pair_at)
pub fn pair<A, B>(a: A, b: B) -> Pair<A, B> {
  Pair(a, b)
}

fn pair_at<A, B>(a: A, b: B, path: String, type_text: String) -> Pair<A, B> {
  Pair(a, b)
}
";

const USES_RESULTS: &str = "//! Uses of initializer functions with the wrong results.

import std::results

pub const a: Int = results::int(1)

pub const b: results::Tag = results::tag()

pub const c: results::Pair<Int, Bool> = results::pair(1, True)
";

/// A function whose result has not one type argument is reported once, as
/// E0045 at its attribute (D-1601), and a whole initializer that calls it
/// in another module is silent and bodiless, never an internal error.
#[test]
fn a_result_without_one_type_argument_is_reported_once_in_its_module() {
    let b = build_with_std(&[("results", STD_RESULTS)], &[("", USES_RESULTS)]);
    let rendered = b.rendered();
    assert_eq!(count(&b, "E0045"), 3, "{rendered}");
    assert_eq!(b.messages().len(), 3, "{rendered}");
    let consts = &b.thir_of("app").consts;
    for name in ["a", "b", "c"] {
        let def = b.def("app", name);
        assert!(consts.iter().all(|c| c.def != def), "`{name}` has a body");
    }
}
