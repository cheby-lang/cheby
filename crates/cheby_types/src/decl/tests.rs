//! Signatures of every item kind (C2), written types and aliases, and the
//! declaration checks of this directory, through the whole front end.

use cheby_iface::{
    CtorFields, DefId, IfaceItem, ItemSig, Namespace, ParamIdx, Ty, TyKind, TypeDefKind, Vis,
    builtin,
};
use cheby_package::{Target, Targets};

use crate::test_support::{Build, build, build_with_std};

/// A root module with a `//!` doc line and `body`.
fn module(body: &str) -> String {
    format!("//! Test.\n\n{body}")
}

/// Builds package `app` with one root module.
fn one(body: &str) -> Build {
    build(&[("", &module(body))])
}

/// The diagnostics' codes, without warnings.
fn errors(b: &Build) -> Vec<String> {
    b.codes()
        .into_iter()
        .filter(|c| c.starts_with('E'))
        .collect()
}

/// The messages with code `code`.
fn messages(b: &Build, code: &str) -> Vec<String> {
    b.messages()
        .into_iter()
        .filter(|m| m.starts_with(code))
        .collect()
}

fn item<'b>(b: &'b Build, ns: Namespace, name: &str) -> (DefId, &'b IfaceItem) {
    let def = b.def_in("app", ns, name);
    (def, b.iface("app").item(def.index).unwrap())
}

fn ty_item<'b>(b: &'b Build, name: &str) -> (DefId, &'b IfaceItem) {
    item(b, Namespace::Type, name)
}

fn value_item<'b>(b: &'b Build, name: &str) -> (DefId, &'b IfaceItem) {
    item(b, Namespace::Value, name)
}

fn tuple(b: &Build, elems: &[Ty]) -> Ty {
    b.globals.types.intern(TyKind::Tuple(elems.into()))
}

fn named(b: &Build, def: DefId, args: &[Ty]) -> Ty {
    b.globals.types.named(def, args.to_vec())
}

fn param(b: &Build, n: u32) -> Ty {
    b.globals.types.param(ParamIdx::from_u32(n))
}

#[test]
fn functions_with_generics_bounds_and_omitted_results() {
    let b = one(
        "pub interface Describe {\n  fn describe(Self) -> String\n}\n\n\
         pub fn pick<T: Describe, U>(first: T, second: U) -> T {\n  first\n}\n\n\
         pub fn nothing(text: String) {\n  Nil\n}\n",
    );
    assert_eq!(errors(&b), Vec::<String>::new(), "{:?}", b.messages());
    let (describe, _) = ty_item(&b, "Describe");
    let ItemSig::Fn(pick) = &value_item(&b, "pick").1.sig else {
        panic!("pick has no function signature");
    };
    let generics: Vec<(&str, Vec<DefId>)> = pick
        .generics
        .params
        .iter()
        .map(|p| (b.globals.names.get(p.name), p.bounds.to_vec()))
        .collect();
    assert_eq!(generics, [("T", vec![describe]), ("U", vec![])]);
    let params: Vec<Ty> = pick.params.iter().map(|p| p.ty).collect();
    assert_eq!(params, [param(&b, 0), param(&b, 1)]);
    assert_eq!(pick.ret, param(&b, 0));
    let ItemSig::Fn(nothing) = &value_item(&b, "nothing").1.sig else {
        panic!("nothing has no function signature");
    };
    assert_eq!(nothing.ret, Ty::NIL);
    assert!(nothing.clamped.is_empty());
}

#[test]
fn types_constructors_and_the_record_shorthand() {
    let b = one(
        "pub exposed type Shape {\n  Circle { radius: Float },\n  Rect(Float, Float),\n  Dot,\n}\n\n\
         pub type Tree<T> {\n  Leaf,\n  Node(Tree<T>, T, Tree<T>),\n}\n\n\
         type Point { x: Float, y: Float }\n",
    );
    assert_eq!(errors(&b), Vec::<String>::new(), "{:?}", b.messages());
    let (shape, shape_item) = ty_item(&b, "Shape");
    let ItemSig::Type(def) = &shape_item.sig else {
        panic!("Shape is not a type");
    };
    assert!(def.exposed);
    assert!(def.generics.params.is_empty());
    assert!(def.hooks.is_none());
    assert_eq!(def.home, None);
    let TypeDefKind::Adt { ctors } = &def.kind else {
        panic!("Shape is not an ADT");
    };
    let names: Vec<&str> = ctors
        .iter()
        .map(|&c| b.globals.names.get(b.iface("app").item(c).unwrap().name))
        .collect();
    assert_eq!(names, ["Circle", "Rect", "Dot"]);
    let radius = b.globals.names.intern("radius");
    let expected = [
        CtorFields::Named(Box::new([(radius, Ty::FLOAT)])),
        CtorFields::Positional(Box::new([Ty::FLOAT, Ty::FLOAT])),
        CtorFields::Unit,
    ];
    for (tag, (&ctor, fields)) in ctors.iter().zip(expected).enumerate() {
        let ItemSig::Ctor(sig) = &b.iface("app").item(ctor).unwrap().sig else {
            panic!("not a constructor");
        };
        assert_eq!(sig.ty, shape.index);
        assert_eq!(usize::try_from(sig.tag).unwrap(), tag);
        assert_eq!(sig.fields, fields);
    }
    // Field types of a generic type use its parameters.
    let (tree, _) = ty_item(&b, "Tree");
    let ItemSig::Ctor(node) = &value_item(&b, "Node").1.sig else {
        panic!("Node is not a constructor");
    };
    let tree_t = named(&b, tree, &[param(&b, 0)]);
    assert_eq!(
        node.fields,
        CtorFields::Positional(Box::new([tree_t, param(&b, 0), tree_t]))
    );
    // The shorthand declares one constructor named like the type (D-082).
    let (point, point_item) = ty_item(&b, "Point");
    let ItemSig::Type(point_def) = &point_item.sig else {
        panic!("Point is not a type");
    };
    assert!(!point_def.exposed);
    let (ctor, ctor_item) = value_item(&b, "Point");
    assert_eq!(
        point_def.kind,
        TypeDefKind::Adt {
            ctors: Box::new([ctor.index])
        }
    );
    let ItemSig::Ctor(sig) = &ctor_item.sig else {
        panic!("Point has no constructor");
    };
    assert_eq!(sig.ty, point.index);
    let (x, y) = (b.globals.names.intern("x"), b.globals.names.intern("y"));
    assert_eq!(
        sig.fields,
        CtorFields::Named(Box::new([(x, Ty::FLOAT), (y, Ty::FLOAT)]))
    );
}

#[test]
fn external_types_have_no_constructors() {
    let b = one(
        "@external(native, \"cheby_db_close\")\n@external(js, \"./db.mjs\", \"close\")\npub type Db\n\n\
         pub type Handle<T>\n\n\
         @external(native, \"cheby_db_open\")\n@external(js, \"./db.mjs\", \"open\")\npub fn open(path: String) -> Db\n",
    );
    assert_eq!(errors(&b), Vec::<String>::new(), "{:?}", b.messages());
    for (name, arity) in [("Db", 0), ("Handle", 1)] {
        let ItemSig::Type(def) = &ty_item(&b, name).1.sig else {
            panic!("{name} is not a type");
        };
        assert_eq!(def.kind, TypeDefKind::External);
        assert_eq!(def.generics.len(), arity);
    }
    let (db, _) = ty_item(&b, "Db");
    let ItemSig::Fn(open) = &value_item(&b, "open").1.sig else {
        panic!("open has no signature");
    };
    assert_eq!(open.ret, named(&b, db, &[]));
}

#[test]
fn constants_and_tests() {
    let b = one(
        "pub const limit: Int = 100\n\nconst names: List<String> = [\"a\"]\n\n\
         test \"limit is positive\" {\n  assert limit > 0\n}\n",
    );
    assert_eq!(errors(&b), Vec::<String>::new(), "{:?}", b.messages());
    let ItemSig::Const(limit) = &value_item(&b, "limit").1.sig else {
        panic!("limit is not a constant");
    };
    assert_eq!(limit.ty, Ty::INT);
    let ItemSig::Const(names) = &value_item(&b, "names").1.sig else {
        panic!("names is not a constant");
    };
    assert_eq!(names.ty, named(&b, builtin::LIST, &[Ty::STRING]));
    assert!(
        b.iface("app")
            .items
            .iter()
            .any(|i| matches!(i.sig, ItemSig::Test) && i.vis == Vis::Priv)
    );
}

#[test]
fn interfaces_embeds_and_self() {
    let b = one("pub interface Named {\n  fn name(Self) -> String\n}\n\n\
         pub interface Shape: Named {\n  fn area(Self) -> Float\n  fn scale(Self, Float) -> Self\n  fn touch(Self)\n  fn pair(Self, List<Self>) -> fn(Self) -> Bool\n}\n\n\
         pub interface Any\n");
    assert_eq!(errors(&b), Vec::<String>::new(), "{:?}", b.messages());
    let (named_iface, _) = ty_item(&b, "Named");
    let ItemSig::Interface(shape) = &ty_item(&b, "Shape").1.sig else {
        panic!("Shape is not an interface");
    };
    assert_eq!(&*shape.embeds, [named_iface]);
    let this = param(&b, 0);
    let sigs: Vec<(Vec<Ty>, Ty)> = shape
        .fns
        .iter()
        .map(|&f| match &b.iface("app").item(f).unwrap().sig {
            ItemSig::InterfaceFn(sig) => (sig.params.to_vec(), sig.ret),
            other => panic!("not an interface function: {other:?}"),
        })
        .collect();
    let list_self = named(&b, builtin::LIST, &[this]);
    let pred = b.globals.types.func(vec![this], Ty::BOOL);
    assert_eq!(
        sigs,
        [
            (vec![this], Ty::FLOAT),
            (vec![this, Ty::FLOAT], this),
            (vec![this], Ty::NIL),
            (vec![this, list_self], pred),
        ]
    );
    let ItemSig::Interface(any) = &ty_item(&b, "Any").1.sig else {
        panic!("Any is not an interface");
    };
    assert!(any.embeds.is_empty() && any.fns.is_empty());
}

#[test]
fn aliases_expand_with_their_arguments_in_any_order() {
    let b = build(&[
        (
            "",
            &module(
                "import app::lib\n\n\
                 pub fn swap(pair: Pair<Int, String>) -> Pair<String, Int> {\n  todo\n}\n\n\
                 pub fn table(t: lib::Table<Bool>) -> Ids {\n  todo\n}\n\n\
                 pub type Pair<A, B> = (A, B)\n\n\
                 pub type Ids = List<UserId>\n\n\
                 pub type UserId = Int\n",
            ),
        ),
        ("lib", &module("pub type Table<V> = List<(String, V)>\n")),
    ]);
    assert_eq!(errors(&b), Vec::<String>::new(), "{:?}", b.messages());
    let ItemSig::Fn(swap) = &value_item(&b, "swap").1.sig else {
        panic!("swap has no signature");
    };
    assert_eq!(swap.params[0].ty, tuple(&b, &[Ty::INT, Ty::STRING]));
    assert_eq!(swap.ret, tuple(&b, &[Ty::STRING, Ty::INT]));
    let ItemSig::Fn(table) = &value_item(&b, "table").1.sig else {
        panic!("table has no signature");
    };
    let entry = tuple(&b, &[Ty::STRING, Ty::BOOL]);
    assert_eq!(table.params[0].ty, named(&b, builtin::LIST, &[entry]));
    assert_eq!(table.ret, named(&b, builtin::LIST, &[Ty::INT]));
    // The alias itself is in the interface, expanded.
    let ItemSig::Alias(pair) = &ty_item(&b, "Pair").1.sig else {
        panic!("Pair is not an alias");
    };
    assert_eq!(pair.generics.len(), 2);
    assert_eq!(pair.target, tuple(&b, &[param(&b, 0), param(&b, 1)]));
}

#[test]
fn aliases_that_refer_to_themselves() {
    let b = one("type Direct = List<Direct>\n\n\
         type Ping = Option<Pong>\n\n\
         type Pong = (Ping, Int)\n\n\
         type User = Ping\n\n\
         pub fn use_it(x: User) -> Direct {\n  todo\n}\n");
    let found = messages(&b, "E0070");
    assert_eq!(
        found.len(),
        3,
        "one E0070 per alias on a cycle: {:?}",
        b.messages()
    );
    assert!(
        found
            .iter()
            .any(|m| m.contains("`Direct` refers to itself"))
    );
    assert!(found.iter().any(|m| m.contains("`Ping` refers to itself")));
    assert!(found.iter().any(|m| m.contains("`Pong` refers to itself")));
    // `User` only depends on a cycle, and the function's uses are silent.
    assert_eq!(errors(&b), ["E0070", "E0070", "E0070"]);
    assert!(matches!(ty_item(&b, "User").1.sig, ItemSig::Error));
    assert!(matches!(value_item(&b, "use_it").1.sig, ItemSig::Error));
    let rendered = b.rendered();
    assert!(
        rendered.contains("the aliases form a cycle: `Ping` → `Pong` → `Ping`"),
        "{rendered}"
    );
}

#[test]
fn alias_parameters_must_be_used() {
    let b = one(
        "type Pair<A, B> = (A, A)\n\ntype Wrap<T> = Pair<T, Int>\n\n\
         pub fn f(p: Pair<Int, Int>) -> Wrap<Int> {\n  todo\n}\n",
    );
    let found = messages(&b, "E0070");
    assert_eq!(found.len(), 1, "{:?}", b.messages());
    assert!(found[0].contains("the type parameter `B` of the alias `Pair` is not used"));
    // `Wrap` uses `T`, but expands through the bad `Pair`: silent.
    assert_eq!(errors(&b), ["E0070"]);
}

#[test]
fn written_types_need_the_right_arguments_and_kinds() {
    let b = one("pub interface Shape {\n  fn area(Self) -> Float\n}\n\n\
         pub type Box<T> { Box(T) }\n\n\
         pub fn a(x: List) -> Box<Int, Int> {\n  todo\n}\n\n\
         pub fn b<T>(x: T<Int>) -> Int {\n  todo\n}\n\n\
         pub fn c(x: Shape) -> Int {\n  todo\n}\n\n\
         pub fn d<T: Box>(x: T) -> dyn Box {\n  todo\n}\n");
    let e0051 = messages(&b, "E0051");
    assert_eq!(e0051.len(), 3, "{:?}", b.messages());
    assert!(
        e0051
            .iter()
            .any(|m| m.contains("`List` takes 1 type argument but 0 were given"))
    );
    assert!(
        e0051
            .iter()
            .any(|m| m.contains("`Box` takes 1 type argument but 2 were given"))
    );
    assert!(
        e0051
            .iter()
            .any(|m| m.contains("`T` takes no type arguments"))
    );
    let e0052 = messages(&b, "E0052");
    assert_eq!(e0052.len(), 3, "{:?}", b.messages());
    assert!(
        e0052
            .iter()
            .any(|m| m.contains("`Shape` is an interface, not a type"))
    );
    assert_eq!(
        e0052
            .iter()
            .filter(|m| m.contains("`Box` is not an interface"))
            .count(),
        2
    );
    let rendered = b.rendered();
    assert!(rendered.contains("write `dyn Shape`"), "{rendered}");
    for f in ["a", "b", "c", "d"] {
        assert!(matches!(value_item(&b, f).1.sig, ItemSig::Error), "{f}");
    }
}

#[test]
fn never_and_function_types() {
    let b = one("pub fn fail(why: String) -> Never {\n  panic as why\n}\n\n\
         pub fn apply(f: fn(Int) -> String, g: fn()) -> fn(String) {\n  todo\n}\n");
    assert_eq!(errors(&b), Vec::<String>::new(), "{:?}", b.messages());
    let ItemSig::Fn(fail) = &value_item(&b, "fail").1.sig else {
        panic!("fail has no signature");
    };
    assert_eq!(fail.ret, Ty::NEVER);
    let ItemSig::Fn(apply) = &value_item(&b, "apply").1.sig else {
        panic!("apply has no signature");
    };
    let types = &b.globals.types;
    assert_eq!(apply.params[0].ty, types.func(vec![Ty::INT], Ty::STRING));
    assert_eq!(apply.params[1].ty, types.func(vec![], Ty::NIL));
    assert_eq!(apply.ret, types.func(vec![Ty::STRING], Ty::NIL));
}

#[test]
fn every_main_is_an_entry_point() {
    let ok = [
        "fn main() {\n  Nil\n}\n",
        "priv fn main() -> Nil {\n  Nil\n}\n",
        "pub fn main() -> Result<Nil, String> {\n  Ok(Nil)\n}\n",
    ];
    for src in ok {
        let b = one(src);
        assert!(
            !b.codes().contains(&"E0050".to_owned()),
            "{src}: {:?}",
            b.messages()
        );
        assert!(matches!(value_item(&b, "main").1.sig, ItemSig::Fn(_)));
    }
    let bad = [
        (
            "fn main(args: List<String>) {\n  Nil\n}\n",
            "takes no parameters",
        ),
        ("fn main() -> Int {\n  0\n}\n", "must return"),
        (
            "fn main() -> Result<Int, String> {\n  Ok(0)\n}\n",
            "must return",
        ),
        ("fn main<T>() {\n  Nil\n}\n", "takes no type parameters"),
    ];
    for (src, label) in bad {
        let b = one(src);
        let found = messages(&b, "E0050");
        assert_eq!(found.len(), 1, "{src}: {:?}", b.messages());
        assert!(found[0].contains(label), "{src}: {found:?}");
        assert!(matches!(value_item(&b, "main").1.sig, ItemSig::Error));
    }
    // A `main` in any module is checked (D-137).
    let b = build(&[
        ("", &module("fn main() {\n  Nil\n}\n")),
        ("tool", &module("fn main() -> String {\n  \"x\"\n}\n")),
    ]);
    assert_eq!(messages(&b, "E0050").len(), 1, "{:?}", b.messages());
}

#[test]
fn targets_and_deprecation_come_from_attributes() {
    let b = one("@target(native)\npub type Native {\n  Native(Int),\n}\n\n\
         @deprecated(\"use `new_name`\")\npub fn old_name() {\n  Nil\n}\n\n\
         pub fn new_name() {\n  Nil\n}\n\n\
         @target(js)\npub const js_only: Int = 1\n");
    assert_eq!(errors(&b), Vec::<String>::new(), "{:?}", b.messages());
    let native = Targets::only(Target::Native);
    assert_eq!(ty_item(&b, "Native").1.targets, native);
    assert_eq!(value_item(&b, "Native").1.targets, native);
    assert_eq!(
        value_item(&b, "js_only").1.targets,
        Targets::only(Target::Js)
    );
    assert_eq!(value_item(&b, "new_name").1.targets, Targets::ALL);
    assert_eq!(
        value_item(&b, "old_name").1.deprecated.as_deref(),
        Some("use `new_name`")
    );
    assert_eq!(value_item(&b, "new_name").1.deprecated, None);
}

#[test]
fn std_attributes_reach_the_interface() {
    let lib = "//! A fake std module.\n\n\
               @std_hooks(equal, hash, debug)\npub type Bag {\n  Bag(List<Int>),\n}\n\n\
               pub fn equal(a: Bag, b: Bag) -> Bool {\n  todo\n}\n\n\
               pub fn hash(bag: Bag) -> Int {\n  todo\n}\n\n\
               pub fn debug(bag: Bag) -> String {\n  todo\n}\n\n\
               @std_clamped(count, start)\npub fn take(start: Int, items: List<Int>, count: Int) -> List<Int> {\n  todo\n}\n";
    let b = build_with_std(
        &[("bag", lib)],
        &[(
            "",
            &module("import std::bag\n\npub fn f(b: bag::Bag) -> bag::Bag {\n  b\n}\n"),
        )],
    );
    assert_eq!(errors(&b), Vec::<String>::new(), "{:?}", b.messages());
    let bag = b.def_in("std::bag", Namespace::Type, "Bag");
    let ItemSig::Type(def) = &b.iface("std::bag").item(bag.index).unwrap().sig else {
        panic!("Bag is not a type");
    };
    let hooks = def.hooks.unwrap();
    let names = &b.globals.names;
    assert_eq!(
        [hooks.equal, hooks.hash, hooks.debug].map(|n| names.get(n)),
        ["equal", "hash", "debug"]
    );
    let ItemSig::Fn(take) = b.fn_sig("std::bag", "take") else {
        panic!("take has no signature");
    };
    assert_eq!(&*take.clamped, [0, 2]);
}

/// The functions of `@std_hooks` must have the hooks' shapes (D-342):
/// E0045 for each one that does not.
#[test]
fn std_hooks_need_their_shapes() {
    let lib = "//! A fake std module.\n\n\
               @std_hooks(equal, hash, debug)\npub type Bag<T> {\n  Bag(List<T>),\n}\n\n\
               pub fn equal<T>(a: Bag<T>, b: List<T>) -> Bool {\n  todo\n}\n\n\
               pub fn hash<T: Show>(bag: Bag<T>) -> List<T> {\n  todo\n}\n\n\
               pub fn debug<T>(bag: Bag<T>) -> Bool {\n  todo\n}\n\n\
               @std_hooks(same, key, text)\npub type Good<T> {\n  Good(T),\n}\n\n\
               pub fn same<T>(a: Good<T>, b: Good<T>) -> Bool {\n  todo\n}\n\n\
               pub fn key<T>(good: Good<T>) -> (Int, List<T>) {\n  todo\n}\n\n\
               pub fn text<T>(good: Good<T>) -> String {\n  todo\n}\n";
    let b = build_with_std(
        &[("bag", lib)],
        &[(
            "",
            &module("import std::bag\n\npub fn f(b: bag::Bag<Int>) -> bag::Bag<Int> {\n  b\n}\n"),
        )],
    );
    assert_eq!(
        messages(&b, "E0045"),
        [
            "E0045: `debug` cannot be the debug hook of `Bag` [\"named here\"]",
            "E0045: `equal` cannot be the equality hook of `Bag` [\"named here\"]",
            "E0045: `hash` cannot be the hash hook of `Bag` [\"named here\"]",
        ],
        "{}",
        b.rendered()
    );
}

/// Editing bodies, initializers and tests never changes the interface or
/// its hash (B8, D-460); editing a signature does.
#[test]
fn interface_hashes_ignore_bodies() {
    let src = |body: &str, init: &str, check: &str, ret: &str| {
        module(&format!(
            "pub type Shape {{\n  Circle {{ radius: Float }},\n  Dot,\n}}\n\n\
             pub type Area = Float\n\n\
             pub interface Measure {{\n  fn measure(Self) -> Float\n}}\n\n\
             pub const zero: Float = {init}\n\n\
             pub fn area(shape: Shape) -> {ret} {{\n{body}\n}}\n\n\
             test \"zero\" {{\n{check}\n}}\n"
        ))
    };
    let base = one_src(&src("  todo", "0.0", "  assert zero == 0.0", "Area"));
    let edited = one_src(&src(
        "  case shape {\n    Circle { radius } => radius * radius\n    Dot => 0.0\n  }",
        "1.0 - 1.0",
        "  assert zero < 1.0",
        "Area",
    ));
    let changed = one_src(&src("  todo", "0.0", "  assert zero == 0.0", "Int"));
    let hash = |b: &Build| b.iface("app").hash;
    assert!(hash(&base).is_some());
    assert_eq!(hash(&base), hash(&edited));
    assert_ne!(hash(&base), hash(&changed));
    assert_eq!(
        hash(&base),
        hash(&one_src(&src(
            "  todo",
            "0.0",
            "  assert zero == 0.0",
            "Area"
        )))
    );
    // The interfaces are equal item by item, not only by hash.
    let sigs = |b: &Build| -> Vec<ItemSig> {
        b.iface("app").items.iter().map(|i| i.sig.clone()).collect()
    };
    assert_eq!(sigs(&base), sigs(&edited));
}

fn one_src(text: &str) -> Build {
    build(&[("", text)])
}

#[test]
fn field_names_are_distinct_within_a_variant() {
    let b = one(
        "pub type Shape {\n  Rect { width: Float, width: Float },\n  Circle { width: Float },\n}\n\n\
         pub type Point { x: Float, y: Float, x: Int }\n",
    );
    let found = messages(&b, "E0041");
    assert_eq!(found.len(), 2, "{:?}", b.messages());
    assert!(
        found
            .iter()
            .any(|m| m.contains("the field `width` is declared twice in `Rect`"))
    );
    assert!(
        found
            .iter()
            .any(|m| m.contains("the field `x` is declared twice in `Point`"))
    );
}

#[test]
fn signatures_mention_only_visible_enough_types() {
    // The spec's example (§7.4).
    let b = one(
        "priv type Secret { Secret(String) }\n\ntype Token { Token(String) }\n\n\
         fn reveal() -> Secret {\n  todo\n}\n\n\
         pub fn issue() -> Token {\n  todo\n}\n",
    );
    let found = messages(&b, "E0074");
    assert_eq!(found.len(), 2, "{:?}", b.messages());
    assert!(found.iter().any(|m| {
        m.contains("the package-visible function `reveal` mentions the `priv` type `Secret`")
    }));
    assert!(found.iter().any(|m| {
        m.contains("the `pub` function `issue` mentions the package-visible type `Token`")
    }));
    // Fields: hidden outside the package unless `exposed` (D-198).
    let b = one(
        "type Inner { Inner(Int) }\n\npriv type Hidden { Hidden(Int) }\n\n\
         pub type Opaque { Opaque(Inner) }\n\n\
         pub exposed type Open { Open(Inner) }\n\n\
         type Leaky { Leaky(Hidden) }\n\n\
         priv type Free { Free(Hidden, Inner) }\n",
    );
    let found = messages(&b, "E0074");
    assert_eq!(found.len(), 2, "{:?}", b.messages());
    assert!(found.iter().any(|m| m.contains(
        "the fields of the `pub exposed` type `Open` mention the package-visible type `Inner`"
    )));
    assert!(found.iter().any(|m| m.contains(
        "the fields of the package-visible type `Leaky` mention the `priv` type `Hidden`"
    )));
    // Aliases are transparent; interfaces in bounds, `dyn` and embeddings
    // count; constants, aliases and interface functions too.
    let b = one(
        "type Id = Int\n\ntype Token { Token(String) }\n\ntype Wrapped = List<Token>\n\n\
         interface Local {\n  fn local(Self) -> Int\n}\n\n\
         pub fn by_id(id: Id) -> Id {\n  id\n}\n\n\
         pub fn wrapped(w: Wrapped) -> Int {\n  todo\n}\n\n\
         pub fn bounded<T: Local>(x: T) -> T {\n  x\n}\n\n\
         pub const tokens: List<Token> = []\n\n\
         pub type Tokens = List<Token>\n\n\
         pub interface Public: Local {\n  fn token(Self) -> Token\n}\n",
    );
    let found = messages(&b, "E0074");
    assert_eq!(found.len(), 6, "{:?}", b.messages());
    assert!(found.iter().any(|m| {
        m.contains("function `wrapped` mentions the package-visible type `Token`")
            && m.contains("`Wrapped` stands for a type that mentions `Token`")
    }));
    assert!(
        found.iter().any(
            |m| m.contains("function `bounded` mentions the package-visible interface `Local`")
        )
    );
    assert!(found.iter().any(|m| m.contains("constant `tokens`")));
    assert!(found.iter().any(|m| m.contains("type alias `Tokens`")));
    // `Public` mentions `Local` and `Token`: each is reported once.
    assert_eq!(
        found
            .iter()
            .filter(|m| m.contains("interface `Public`"))
            .count(),
        2,
        "{found:?}"
    );
}

#[test]
fn interface_function_errors_keep_the_shape() {
    let b = one(
        "pub interface Shape {\n  fn area(Self) -> Missing\n  fn scale(Self, Float) -> Self\n}\n",
    );
    assert_eq!(errors(&b), ["E0042"], "{:?}", b.messages());
    let ItemSig::Interface(shape) = &ty_item(&b, "Shape").1.sig else {
        panic!("Shape is not an interface");
    };
    let ItemSig::InterfaceFn(area) = &b.iface("app").item(shape.fns[0]).unwrap().sig else {
        panic!("area is not an interface function");
    };
    assert_eq!(area.ret, Ty::ERROR);
}

#[test]
fn dyn_types_in_signatures_are_checked_once() {
    let b = one(
        "pub interface Shape {\n  fn area(Self) -> Float\n  fn scale(Self, Float) -> Self\n}\n\n\
         pub interface Show2 {\n  fn show2(Self) -> String\n}\n\n\
         pub type Shapes = List<dyn Shape>\n\n\
         pub fn total(shapes: Shapes, one: dyn Shape) -> Float {\n  todo\n}\n\n\
         pub fn fine(x: dyn Show2) -> dyn Show2 {\n  x\n}\n",
    );
    let found = messages(&b, "E0064");
    // Once in the alias, once in `total`; uses of the alias are silent.
    assert_eq!(found.len(), 2, "{:?}", b.messages());
    assert!(matches!(value_item(&b, "fine").1.sig, ItemSig::Fn(_)));
}

/// An own alias written in a body expands from its declaration, whose
/// paths belong to the signatures.
#[test]
fn aliases_expand_in_bodies() {
    let b = one(
        "type Entry = (Int, String)\n\ntype Entries = List<Entry>\n\n\
         pub fn first() -> Int {\n  let entries: Entries = [(1, \"one\")]\n  let _ = entries\n  1\n}\n",
    );
    assert_eq!(errors(&b), Vec::<String>::new(), "{:?}", b.messages());
}

/// A bound that fails to lower makes the function's signature an error,
/// and its parameter is the error type in the body: neither its uses nor
/// the function's callers report a follow-up E0063.
#[test]
fn a_failed_bound_silences_its_parameter() {
    let b = one("fn square<T: Nope>(x: T) -> T {\n  x * x\n}\n\n\
         pub fn main() {\n  let _ = square(\"a\")\n}\n");
    let codes = errors(&b);
    assert!(!codes.is_empty(), "{}", b.rendered());
    assert!(!codes.iter().any(|c| c == "E0063"), "{}", b.rendered());
    let (_, square) = item(&b, Namespace::Value, "square");
    assert!(matches!(square.sig, ItemSig::Error), "{:?}", square.sig);
}

// E0079: external values only as direct results (§12.5.1, D-948) ----------

/// An external type `Db` and `rest`.
fn with_db(rest: &str) -> Build {
    one(&format!(
        "@external(native, \"db_close\")\npub type Db\n\n{rest}"
    ))
}

/// D-948: an external type inside a `Result`, an `Option`, a tuple, a
/// list or a type of the package is E0079, with the result type and the
/// external type named.
#[test]
fn ext_result_below_the_top_level_is_an_error() {
    for (ret, outer) in [
        ("Result<Db, String>", "`Result`"),
        ("Option<Db>", "`Option`"),
        ("(Db, Int)", "a tuple"),
        ("List<Db>", "`List`"),
        ("Holder", "`Holder`"),
        ("Option<Holder>", "`Option`"),
        ("Wrapped", "`Option`"),
    ] {
        let b = with_db(&format!(
            "pub type Holder {{\n  Holder(Int, Db),\n}}\n\npub type Wrapped = Option<Db>\n\n\
             @external(native, \"db_open\")\npub fn open(path: String) -> {ret}\n"
        ));
        let shown = if ret == "Wrapped" { "Option<Db>" } else { ret };
        assert_eq!(
            messages(&b, "E0079"),
            [format!(
                "E0079: the native foreign result type `{shown}` holds the external type `Db` \
                 below its top level [\"native foreign code cannot build this result\", \
                 \"`Db` is inside {outer}\"]"
            )],
            "{ret}: {:?}",
            b.messages()
        );
        assert_eq!(errors(&b), ["E0079"], "{ret}");
    }
}

/// D-948: a direct result, parameters that hold external types at any
/// depth, a type parameter, and a function without `@external(native, …)`
/// are not checked.
#[test]
fn ext_result_direct_results_parameters_and_type_parameters_are_allowed() {
    let b = with_db(
        "@external(native, \"db_open\")\npub fn open(path: String) -> Db\n\n\
         @external(native, \"db_first\")\npub fn first(dbs: List<Option<Db>>, pair: (Db, Int)) -> Int\n\n\
         @external(native, \"db_any\")\npub fn any<T>(n: Int) -> Option<T>\n\n\
         @external(native, \"db_maker\")\npub fn maker() -> fn(Int) -> Db\n\n\
         pub fn wrap(db: Db) -> Option<Db> {\n  Some(db)\n}\n",
    );
    assert_eq!(errors(&b), Vec::<String>::new(), "{:?}", b.messages());
}

/// D-948: the type's own recursion is walked once, and a type of the
/// package that holds no external type is fine.
#[test]
fn ext_result_recursive_types_end() {
    let b = with_db(
        "pub type Tree {\n  Leaf,\n  Node(Tree, Tree),\n}\n\n\
         pub type Chain {\n  End(Db),\n  Link(Chain),\n}\n\n\
         @external(native, \"db_tree\")\npub fn tree() -> Tree\n\n\
         @external(native, \"db_chain\")\npub fn chain() -> Chain\n",
    );
    assert_eq!(errors(&b), ["E0079"], "{:?}", b.messages());
    assert_eq!(messages(&b, "E0079").len(), 1);
    assert!(messages(&b, "E0079")[0].contains("`Chain`"));
}

/// D-948: the standard library is exempt, since its kernels are Rust.
#[test]
fn ext_result_std_is_exempt() {
    let b = build_with_std(
        &[(
            "store",
            "//! A store.\n\npub type Handle\n\n\
             @external(native, \"cheby_store_open\")\npub fn open() -> Option<Handle>\n",
        )],
        &[(
            "",
            &module("import std::store\n\npub fn main() {\n  let _ = store::open()\n}\n"),
        )],
    );
    // (`main` runs on `js` too, which `open` lacks: E0067, not this rule.)
    assert_eq!(
        messages(&b, "E0079"),
        Vec::<String>::new(),
        "{:?}",
        b.messages()
    );
    // The std module was checked: its function has its signature.
    assert!(
        matches!(b.fn_sig("std::store", "open"), ItemSig::Fn(_)),
        "std::store was not built"
    );
}

/// D-948, plan §7 Q4: the rendered error, its primary span on the
/// `@external(native, …)` attribute and its label on the result type.
#[test]
fn ext_result_renders_at_the_attribute() {
    let b = with_db(
        "@external(native, \"db_open\")\npub fn open(path: String) -> Result<Db, String>\n",
    );
    let rendered = b.rendered();
    let expected = "error[E0079]: the native foreign result type `Result<Db, String>` holds the \
                    external type `Db` below its top level\n";
    assert!(rendered.starts_with(expected), "{rendered}");
    assert!(
        rendered.contains(
            "6 | @external(native, \"db_open\")\n  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^ native foreign code cannot build this result\n"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("------------------ `Db` is inside `Result`"),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "= help: return `Db` from the foreign function and build the `Result` in Cheby \
             with a wrapper (§12.5)"
        ),
        "{rendered}"
    );
}
