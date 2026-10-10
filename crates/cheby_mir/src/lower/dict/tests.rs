//! Lowering tests for dictionaries, interface calls and descriptors
//! (W3-A), through `test_support::lower_source`, pinned by goldens under
//! `tests/golden/dict/` (bless with `CHEBY_BLESS=1`, then read them).

use crate::dump::dump;
use crate::golden_tests::check_golden;
use crate::ir::{
    BuiltinDesc, Callee, Const, FnKey, LayoutRef, MirModule, Operand, Rvalue, StaticKind,
    Stmt as MirStmt, Terminator,
};
use crate::test_support::{Lowered, lower_source};

fn lowered(files: &[(&str, &str)]) -> Lowered {
    let lowered = lower_source(files);
    assert_eq!(lowered.unsupported(), Vec::<String>::new());
    lowered
}

fn app(l: &Lowered) -> &MirModule {
    l.unit("app")
}

/// The dumps of the functions of `app` whose keys print as `keys`, then
/// the unit's static objects, for a golden.
fn golden_text(l: &Lowered, keys: &[&str]) -> String {
    let mut parts: Vec<String> = keys.iter().map(|k| l.function("app", k)).collect();
    parts.push(statics(app(l)));
    parts.join("\n")
}

/// The static-object lines of a unit's dump.
fn statics(unit: &MirModule) -> String {
    let text = dump(unit).unwrap();
    let mut out: Vec<&str> = text.lines().filter(|l| l.starts_with("static#")).collect();
    out.push("");
    out.join("\n")
}

/// Every rvalue of the function of `app` with key `key`.
fn rvalues(l: &Lowered, key: FnKey) -> Vec<Rvalue> {
    let f = app(l).function(key).unwrap();
    f.blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter_map(|s| match s {
            MirStmt::Assign { value, .. } | MirStmt::Eval(value) => Some(value.clone()),
            MirStmt::Dup(_) | MirStmt::Drop(_) => None,
        })
        .collect()
}

fn def_named(unit: &MirModule, item: &str) -> cheby_iface::DefId {
    unit.names
        .iter()
        .find(|n| &*n.item == item && &*n.module == "app")
        .unwrap_or_else(|| panic!("no def `{item}`"))
        .def
}

/// `Dict::Param` is the function's own leading dictionary parameter,
/// passed on unchanged (`Callee::Fn` with dictionaries); `Callee::Iface`
/// loads the function's closure from the dictionary and calls it
/// indirectly with uniform arguments, as a tail call in tail position;
/// `main` passes the static dictionary of `Show` for `Int`, whose
/// descriptor is the runtime's `cheby_rt_desc_int`.
#[test]
fn parameters_interface_calls_and_static_dictionaries() {
    let l = lowered(&[(
        "",
        r#"
fn shown<T: Show>(x: T) -> String {
  Show::show(x)
}

fn twice<T: Show>(x: T) -> String {
  let first = shown(x)
  first + shown(x)
}

fn main() {
  let _ = twice(1)
  let _ = twice("a")
}
"#,
    )]);
    let text = golden_text(&l, &["app::shown", "app::twice", "app::main"]);
    check_golden("dict/params_and_calls", &text);
    let unit = app(&l);
    let shown = unit
        .function(FnKey::Item(def_named(unit, "shown")))
        .unwrap();
    // The dictionary, then `x`.
    assert_eq!(shown.params.len(), 2);
    assert!(
        shown.blocks.iter().any(|b| matches!(
            b.term,
            Terminator::TailCall {
                callee: Callee::Indirect(_),
                ..
            }
        )),
        "{text}"
    );
    let ints: Vec<_> = unit
        .statics
        .iter()
        .filter(|s| s.kind == StaticKind::Dict && &*s.name == "Show$Int")
        .collect();
    assert_eq!(ints.len(), 1, "{text}");
    assert_eq!(&*ints[0].module, "app");
    assert_eq!(
        ints[0].layout,
        LayoutRef::Dict {
            fns: 1,
            embedded: 0
        }
    );
    assert_eq!(ints[0].fields[0], Const::BuiltinDesc(BuiltinDesc::Int));
    assert!(matches!(ints[0].fields[1], Const::FnValue(_)));
}

/// A dictionary that holds a dictionary parameter is built at run time:
/// its descriptor `List<T>` from the parameter's descriptor (field 0 of
/// `T`'s first bound, D-499), and `list::show`'s closure holding `T`'s
/// dictionary. A closed one with a conditional function is static all the
/// way down, with a static closure of the shim and a static descriptor.
#[test]
fn dictionaries_built_at_run_time_and_static_ones() {
    let l = lowered(&[(
        "",
        r"
fn shown<T: Show>(x: T) -> String {
  Show::show(x)
}

fn all<T: Show>(xs: List<T>) -> String {
  shown(xs)
}

fn main() {
  let _ = all([1, 2])
  let _ = shown([[True]])
}
",
    )]);
    let text = golden_text(&l, &["app::all", "app::main"]);
    check_golden("dict/run_time_and_static", &text);
    let unit = app(&l);
    let all = rvalues(&l, FnKey::Item(def_named(unit, "all")));
    let allocs: Vec<&LayoutRef> = all
        .iter()
        .filter_map(|r| match r {
            Rvalue::Alloc { layout, .. } => Some(layout),
            _ => None,
        })
        .collect();
    assert!(allocs.contains(&&LayoutRef::Desc { args: 1 }), "{text}");
    assert!(allocs.contains(&&LayoutRef::Closure(1)), "{text}");
    assert!(
        allocs.contains(&&LayoutRef::Dict {
            fns: 1,
            embedded: 0
        }),
        "{text}"
    );
    // The descriptor of `List<List<Bool>>` is static: a `Desc` of the
    // runtime's `list` descriptor and the static `List<Bool>`.
    let nested = unit
        .statics
        .iter()
        .find(|s| s.kind == StaticKind::Desc && &*s.name == "List<List<Bool>>")
        .unwrap_or_else(|| panic!("{text}"));
    assert_eq!(nested.layout, LayoutRef::Desc { args: 1 });
    assert_eq!(nested.fields[0], Const::BuiltinDesc(BuiltinDesc::List));
    let closure = unit
        .statics
        .iter()
        .find(|s| s.kind == StaticKind::FnValue)
        .unwrap_or_else(|| panic!("{text}"));
    assert_eq!(closure.layout, LayoutRef::Closure(1));
    assert!(matches!(
        closure.fields[0],
        Const::Code(FnKey::ValueShim(_))
    ));
}

/// User types: the nominal descriptor `cb$desc$<module path>$<Type>` is
/// a static of the type's module holding its name and module path; a
/// generic user type applied to closed arguments is a static `Desc`.
#[test]
fn user_type_descriptors() {
    let l = lowered(&[
        (
            "",
            r"
import app::boxed
import app::geometry::{Point}

fn shown<T: Show>(x: T) -> String {
  Show::show(x)
}

fn main() {
  let _ = shown(Point { x: 1, y: 2 })
  let _ = shown(boxed::wrap(3))
}
",
        ),
        (
            "geometry",
            r#"
pub type Point {
  Point { x: Int, y: Int }
}

pub fn show(p: Point) -> String {
  "({p.x}, {p.y})"
}
"#,
        ),
        (
            "boxed",
            r#"
pub type Boxed<T> {
  Boxed(T)
}

pub fn wrap<T>(value: T) -> Boxed<T> {
  Boxed(value)
}

pub fn show<T: Show>(b: Boxed<T>) -> String {
  let Boxed(value) = b
  "[{value}]"
}
"#,
        ),
    ]);
    let text = statics(app(&l));
    check_golden("dict/user_descriptors", &text);
    assert!(
        text.contains("desc app::geometry$Point = desc_nominal(str#0, str#1) str#0 = \"Point\" str#1 = \"app::geometry\""),
        "{text}"
    );
}

/// `Dict::Embedded` of a parameter's dictionary loads the embedded
/// dictionary's field, after the descriptor and the functions (D-524);
/// a static dictionary of an interface that embeds another holds the
/// embedded one's dictionary.
#[test]
fn embedded_dictionaries() {
    let l = lowered(&[(
        "",
        r#"
interface Named {
  fn name(Self) -> String
}

interface Greeter: Named {
  fn greeting(Self) -> String
}

type Person {
  Person(String)
}

fn name(p: Person) -> String {
  let Person(n) = p
  n
}

fn greeting(p: Person) -> String {
  "hello, " + name(p)
}

fn just_name<T: Named>(x: T) -> String {
  Named::name(x)
}

fn both<T: Greeter>(x: T) -> String {
  just_name(x) + Greeter::greeting(x)
}

fn main() {
  let _ = both(Person("Ada"))
}
"#,
    )]);
    let text = golden_text(&l, &["app::both"]);
    check_golden("dict/embedded", &text);
    let unit = app(&l);
    let both = rvalues(&l, FnKey::Item(def_named(unit, "both")));
    // fns: name, greeting (embedded first); then the `Named` dictionary.
    assert!(
        both.iter().any(|r| matches!(
            r,
            Rvalue::Field {
                layout: LayoutRef::Dict {
                    fns: 2,
                    embedded: 1
                },
                field: 3,
                ..
            }
        )),
        "{text}"
    );
    let greeter = unit
        .statics
        .iter()
        .find(|s| &*s.name == "app::Greeter$app::Person")
        .unwrap_or_else(|| panic!("{text}"));
    assert_eq!(
        greeter.layout,
        LayoutRef::Dict {
            fns: 2,
            embedded: 1
        }
    );
    let Const::Static(named) = greeter.fields[3] else {
        panic!("{text}");
    };
    assert_eq!(
        &*unit.static_object(named).unwrap().name,
        "app::Named$app::Person"
    );
}

/// Tuples satisfy `Show` and `Compare` by the built-in rule (D-167): the
/// dictionary's functions are closures of `TupleDictFn` code holding the
/// element dictionaries, and the unit defines that code: `show` writes
/// `(a, b)`, `compare` goes left to right and tail-calls the last.
#[test]
fn tuple_dictionaries() {
    let l = lowered(&[(
        "",
        r#"
import std::ops::{Compare}

fn smaller<T: Compare>(a: T, b: T) -> Bool {
  a < b
}

fn shown<T: Show>(x: T) -> String {
  "{x}"
}

fn pairs<T: Compare>(a: T, b: T) -> Bool {
  smaller((a, 1), (b, 2))
}

fn main() {
  let _ = smaller((1, "a"), (1, "b"))
  let _ = shown((1, "a"))
  let _ = pairs(1.5, 2.5)
}
"#,
    )]);
    let unit = app(&l);
    let tuple_fns: Vec<String> = unit
        .functions
        .iter()
        .filter(|f| matches!(f.key, FnKey::TupleDictFn { .. }))
        .map(|f| crate::dump::dump_function(unit, f).unwrap())
        .collect();
    assert_eq!(tuple_fns.len(), 2, "{}", dump(unit).unwrap());
    let mut text = golden_text(&l, &["app::pairs"]);
    text.push('\n');
    text.push_str(&tuple_fns.join("\n"));
    check_golden("dict/tuples", &text);
    let compare = tuple_fns
        .iter()
        .find(|t| t.contains("compare"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(compare.contains("tail_call"), "{compare}");
    assert!(compare.contains("read_tag"), "{compare}");
}

/// A top-level generic function used as a value holds its dictionaries
/// in a closure of its shim; inside a generic function they are its own
/// dictionary parameters.
#[test]
fn function_values_with_dictionaries() {
    let l = lowered(&[(
        "",
        r"
fn shown<T: Show>(x: T) -> String {
  Show::show(x)
}

fn shower<T: Show>(_x: T) -> fn(T) -> String {
  shown
}

fn main() {
  let f = shower(1)
  let _ = f(2)
}
",
    )]);
    let text = golden_text(&l, &["app::shower", "shim app::shown"]);
    check_golden("dict/fn_values", &text);
    let unit = app(&l);
    let shower = rvalues(&l, FnKey::Item(def_named(unit, "shower")));
    assert!(
        shower.iter().any(|r| matches!(
            r,
            Rvalue::Alloc {
                layout: LayoutRef::Closure(1),
                fields
            } if matches!(fields[0], Operand::Const(Const::Code(FnKey::ValueShim(_))))
                && matches!(fields[1], Operand::Local(_))
        )),
        "{text}"
    );
}

/// A closure inside a generic function captures the dictionaries it uses
/// after its captured locals, and reads them from `self`.
#[test]
fn closures_capture_dictionaries() {
    let l = lowered(&[(
        "",
        r#"
import std::list

fn labels<T: Show>(xs: List<T>, prefix: String) -> List<String> {
  list::map(xs, fn(x) { prefix + Show::show(x) })
}

fn main() {
  let _ = labels([1, 2], "n")
}
"#,
    )]);
    let unit = app(&l);
    let labels = def_named(unit, "labels");
    let closure = crate::dump::dump_function(
        unit,
        unit.function(FnKey::Closure {
            owner: labels,
            index: 0,
        })
        .unwrap(),
    )
    .unwrap();
    let text = format!("{}\n{closure}", l.function("app", "app::labels"));
    check_golden("dict/closure_captures", &text);
    assert!(text.contains("alloc closure/2("), "{text}");
    assert!(closure.contains("field _0.2 of closure/2"), "{closure}");
}

/// Interpolation and operators through bounds (D-503): `{x}` calls
/// `show` through the dictionary, `+` through an `Add` bound, `<` through
/// `Compare` and the tag of the `Order`, and `+` through a bound that
/// embeds `Add`.
#[test]
fn interpolation_and_operators_through_dictionaries() {
    let l = lowered(&[(
        "",
        r#"
import std::ops::{Add, Compare, Sub}

interface Num: Add + Sub

fn label<T: Show>(x: T) -> String {
  "<{x}>"
}

fn plus<T: Add>(a: T, b: T) -> T {
  a + b
}

fn less<T: Compare>(a: T, b: T) -> Bool {
  a < b
}

fn diff<T: Num>(a: T, b: T) -> T {
  a - b + b
}

fn main() {
  let _ = label(1)
  let _ = plus(1, 2)
  let _ = less("a", "b")
  let _ = diff(1.5, 0.5)
}
"#,
    )]);
    let text = golden_text(&l, &["app::label", "app::plus", "app::less", "app::diff"]);
    check_golden("dict/operators_and_interpolation", &text);
    let unit = app(&l);
    for f in ["label", "plus", "less", "diff"] {
        let rv = rvalues(&l, FnKey::Item(def_named(unit, f)));
        let terms: Vec<&Terminator> = unit
            .function(FnKey::Item(def_named(unit, f)))
            .unwrap()
            .blocks
            .iter()
            .map(|b| &b.term)
            .collect();
        let indirect = rv.iter().any(|r| {
            matches!(
                r,
                Rvalue::Call {
                    callee: Callee::Indirect(_),
                    ..
                }
            )
        }) || terms.iter().any(|t| {
            matches!(
                t,
                Terminator::TailCall {
                    callee: Callee::Indirect(_),
                    ..
                }
            )
        });
        assert!(indirect, "{f}: {text}");
    }
}

/// A `Result` main whose error type's `show` needs dictionaries calls it
/// through the error type's static dictionary.
#[test]
fn entries_show_errors_through_static_dictionaries() {
    let l = lowered(&[(
        "",
        r#"
fn main() -> Result<Nil, List<String>> {
  Err(["a", "b"])
}
"#,
    )]);
    let entries = l.units.iter().find(|u| u.module().is_none()).unwrap();
    let entry = entries
        .functions
        .iter()
        .find(|f| matches!(f.key, FnKey::Entry(_)))
        .unwrap();
    let entry = crate::dump::dump_function(entries, entry).unwrap();
    let text = format!("{entry}\n{}", statics(entries));
    check_golden("dict/entry_show", &text);
    assert!(text.contains("of dict/1+0"), "{text}");
    assert!(text.contains("dict app$Show$List<String>"), "{text}");
}

/// A test block's unit has its own static dictionaries, with the
/// module's path in their symbols, and a closure nested in a closure of
/// a generic function captures the dictionary through the outer one.
#[test]
fn tests_units_and_nested_closures() {
    let l = crate::test_support::lower_source_with_tests(&[(
        "",
        r#"
import std::list

fn shown<T: Show>(x: T) -> String {
  Show::show(x)
}

fn nested<T: Show>(xs: List<T>) -> List<List<String>> {
  list::map(xs, fn(x) { list::map([x, x], fn(y) { shown(y) }) })
}

fn main() {
  let _ = nested([1])
}

test "shows through a dictionary" {
  assert shown([1]) == "[1]"
}
"#,
    )]);
    assert_eq!(l.unsupported(), Vec::<String>::new());
    let tests = l
        .units
        .iter()
        .find(|u| matches!(u.unit, crate::ir::UnitKind::Tests(_)))
        .unwrap();
    let text = statics(tests);
    assert!(text.contains("dict app$Show$List<Int>"), "{text}");
    let unit = app(&l);
    let owner = def_named(unit, "nested");
    let outer = crate::dump::dump_function(
        unit,
        unit.function(FnKey::Closure { owner, index: 0 }).unwrap(),
    )
    .unwrap();
    let inner = crate::dump::dump_function(
        unit,
        unit.function(FnKey::Closure { owner, index: 1 }).unwrap(),
    )
    .unwrap();
    // The outer closure captures the dictionary and passes it to the
    // inner one; the inner one reads it from its own `self`.
    assert!(outer.contains("field _0.1 of closure/1"), "{outer}");
    assert!(outer.contains("alloc closure/1("), "{outer}");
    assert!(inner.contains("field _0.1 of closure/1"), "{inner}");
}
