//! Lowering tests for `dyn` values (W4-A, §8.6, D-502, D-524), through
//! `test_support::lower_source`, pinned by goldens under
//! `tests/golden/dict/` (bless with `CHEBY_BLESS=1`, then read them).

use cheby_iface::DefId;

use crate::dump::dump;
use crate::golden_tests::check_golden;
use crate::ir::{
    Callee, Const, FnKey, LayoutRef, MirModule, Rvalue, StaticKind, StaticObject, Stmt as MirStmt,
    Terminator,
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

/// The static-object lines of a unit's dump.
fn statics(unit: &MirModule) -> String {
    let text = dump(unit).unwrap();
    let mut out: Vec<&str> = text.lines().filter(|l| l.starts_with("static#")).collect();
    out.push("");
    out.join("\n")
}

/// Every rvalue of the function of `unit` with key `key`.
fn rvalues(unit: &MirModule, key: FnKey) -> Vec<Rvalue> {
    let f = unit
        .function(key)
        .unwrap_or_else(|| panic!("no function {key:?}"));
    f.blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter_map(|s| match s {
            MirStmt::Assign { value, .. } | MirStmt::Eval(value) => Some(value.clone()),
            MirStmt::Dup(_) | MirStmt::Drop(_) => None,
        })
        .collect()
}

/// The def of `module::item` among the unit's names.
fn def_named(unit: &MirModule, module: &str, item: &str) -> DefId {
    unit.names
        .iter()
        .find(|n| &*n.item == item && &*n.module == module)
        .unwrap_or_else(|| panic!("no def `{module}::{item}`"))
        .def
}

/// The static of `kind` named `name`.
fn static_named<'u>(unit: &'u MirModule, kind: StaticKind, name: &str) -> &'u StaticObject {
    unit.statics
        .iter()
        .find(|s| s.kind == kind && &*s.name == name)
        .unwrap_or_else(|| panic!("no static `{name}` in:\n{}", statics(unit)))
}

/// The static that the constant `c` names.
fn target<'u>(unit: &'u MirModule, c: &Const) -> &'u StaticObject {
    let Const::Static(id) = c else {
        panic!("{c:?} is not a static");
    };
    unit.static_object(*id).unwrap()
}

/// Three modules: the interface `Shape`, which embeds `Show`, and two
/// types that satisfy it.
const SHAPES: [(&str, &str); 3] = [
    (
        "shape",
        r"
/// Something with an area.
interface Shape: Show {
  fn area(Self) -> Float
}
",
    ),
    (
        "circle",
        r#"
type Circle {
  Circle(Float)
}

fn new(r: Float) -> Circle {
  Circle(r)
}

fn area(c: Circle) -> Float {
  let Circle(r) = c
  3.0 * r * r
}

fn show(c: Circle) -> String {
  let Circle(r) = c
  "circle {r}"
}
"#,
    ),
    (
        "square",
        r#"
type Square {
  Square(Float)
}

fn new(side: Float) -> Square {
  Square(side)
}

fn area(s: Square) -> Float {
  let Square(side) = s
  side * side
}

fn show(s: Square) -> String {
  let Square(side) = s
  "square {side}"
}
"#,
    ),
];

const MAIN: &str = r#"
import std::list
import app::circle
import app::shape
import app::square

interface Any

fn total(shapes: List<dyn shape::Shape>) -> Float {
  list::fold(shapes, 0.0, fn(acc, s) { acc + shape::Shape::area(s) })
}

fn label(s: dyn shape::Shape) -> String {
  "{s}"
}

fn measure<T: shape::Shape>(x: T) -> Float {
  shape::Shape::area(x)
}

fn boxed<T: Any>(x: T) -> dyn Any {
  x
}

fn main() {
  let shapes: List<dyn shape::Shape> = [circle::new(1.0), square::new(2.0)]
  let _ = total(shapes)
  let first: dyn shape::Shape = circle::new(3.0)
  let _ = label(first)
  let _ = measure(first)
  let any: dyn Any = first
  let shown: dyn Show = first
  let _ = any == boxed(first)
  let _ = "{shown}"
}
"#;

fn shapes_program() -> Lowered {
    let mut files = vec![("", MAIN)];
    files.extend(SHAPES);
    lowered(&files)
}

/// A conversion allocates a `dyn` value of the type's static dictionary
/// and the value; a call on a `dyn` value loads the closure from the
/// value's dictionary and passes the value inside; `dyn` to `dyn` of an
/// embedded interface takes the embedded dictionary, and to `dyn` of an
/// empty interface builds a dictionary holding the inner descriptor
/// (D-502).
#[test]
fn conversions_and_calls_through_dyn_values() {
    let l = shapes_program();
    let text = [
        l.function("app", "app::main"),
        l.function("app", "app::label"),
        statics(app(&l)),
    ]
    .join("\n");
    check_golden("dict/dyn_values", &text);
    let unit = app(&l);
    let main = rvalues(unit, FnKey::Item(def_named(unit, "app", "main")));
    let allocs = main
        .iter()
        .filter(|r| {
            matches!(
                r,
                Rvalue::Alloc {
                    layout: LayoutRef::Dyn,
                    ..
                }
            )
        })
        .count();
    // Two list elements, `first`, `any` and `shown`.
    assert_eq!(allocs, 5, "{text}");
    // `any` keeps the descriptor: field 0 of the `Shape` dictionary
    // inside `first`, in a new dictionary of `Any`.
    assert!(
        main.iter().any(|r| matches!(
            r,
            Rvalue::Field {
                layout: LayoutRef::Dict {
                    fns: 2,
                    embedded: 1
                },
                field: 0,
                ..
            }
        )),
        "{text}"
    );
    assert!(
        main.iter().any(|r| matches!(
            r,
            Rvalue::Alloc {
                layout: LayoutRef::Dict {
                    fns: 0,
                    embedded: 0
                },
                ..
            }
        )),
        "{text}"
    );
    // `shown` takes the embedded `Show` dictionary, field 3.
    assert!(
        main.iter().any(|r| matches!(
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
    // `label`: `{s}` is `Show::show` through the value's dictionary,
    // field 1 of `Shape`'s, with the value inside as `Self`.
    let label = rvalues(unit, FnKey::Item(def_named(unit, "app", "label")));
    assert!(
        label.iter().any(|r| matches!(
            r,
            Rvalue::Field {
                layout: LayoutRef::Dyn,
                field: 1,
                ..
            }
        )),
        "{text}"
    );
    assert!(
        label.iter().any(|r| matches!(
            r,
            Rvalue::Call {
                callee: Callee::Indirect(_),
                ..
            }
        )),
        "{text}"
    );
    // The static dictionaries of the converted types.
    let circle = static_named(
        unit,
        StaticKind::Dict,
        "app::shape::Shape$app::circle::Circle",
    );
    assert_eq!(
        circle.layout,
        LayoutRef::Dict {
            fns: 2,
            embedded: 1
        }
    );
}

/// A `dyn J` value satisfies `J` as a bound through a static forwarding
/// dictionary: the descriptor of the type `dyn J`
/// (`cb$desc$<module of J>$$dyn$<J>`), one static closure per function
/// whose code is the forwarder, then the forwarding dictionaries of the
/// embedded interfaces. The unit defines the forwarders.
#[test]
fn dyn_values_satisfy_bounds_through_forwarding_dictionaries() {
    let l = shapes_program();
    let unit = app(&l);
    let shape = def_named(unit, "app::shape", "Shape::area");
    let show = cheby_iface::builtin::SHOW_FN;
    let iface = def_named(unit, "app::shape", "Shape");
    let forwarders: Vec<String> = [show, shape]
        .into_iter()
        .map(|iface_fn| {
            let key = FnKey::DynForward {
                iface_fn,
                dyn_iface: iface,
            };
            let f = unit
                .function(key)
                .unwrap_or_else(|| panic!("no forwarder {key:?}"));
            crate::dump::dump_function(unit, f).unwrap()
        })
        .collect();
    let text = format!("{}\n{}", forwarders.join("\n"), statics(unit));
    check_golden("dict/dyn_forwarding", &text);
    // Each forwarder unwraps its receiver and tail-calls the closure of
    // the value's dictionary.
    for f in [show, shape] {
        let key = FnKey::DynForward {
            iface_fn: f,
            dyn_iface: iface,
        };
        let func = unit.function(key).unwrap();
        assert_eq!(func.params.len(), 2, "{text}");
        assert!(
            func.blocks.iter().any(|b| matches!(
                b.term,
                Terminator::TailCall {
                    callee: Callee::Indirect(_),
                    ..
                }
            )),
            "{text}"
        );
    }
    let dict = static_named(
        unit,
        StaticKind::Dict,
        "app::shape::Shape$dyn app::shape::Shape",
    );
    assert_eq!(
        dict.layout,
        LayoutRef::Dict {
            fns: 2,
            embedded: 1
        }
    );
    let desc = target(unit, &dict.fields[0]);
    assert_eq!(desc.kind, StaticKind::Desc);
    assert_eq!(&*desc.module, "app::shape");
    assert_eq!(&*desc.name, "dyn$Shape");
    assert_eq!(desc.layout, LayoutRef::DescNominal);
    let strings: Vec<&str> = desc.strings.iter().map(|s| &**s).collect();
    assert_eq!(strings, ["dyn Shape", "app::shape"]);
    let area = target(unit, &dict.fields[2]);
    assert_eq!(area.kind, StaticKind::FnValue);
    assert_eq!(area.layout, LayoutRef::Closure(0));
    assert_eq!(
        &*area.fields,
        [Const::Code(FnKey::DynForward {
            iface_fn: shape,
            dyn_iface: iface,
        })]
    );
    let embedded = target(unit, &dict.fields[3]);
    assert_eq!(&*embedded.name, "Show$dyn app::shape::Shape");
    // The embedded forwarding dictionary has the same descriptor.
    assert_eq!(embedded.fields[0], dict.fields[0]);
}

/// Where an interface function sits in a dictionary depends on the
/// interface (D-524): with `J: A + B`, `B`'s function is field 2 of a
/// `J` dictionary, so the forwarder for `dyn J` through a `B` bound
/// reads field 2, and `dyn J` converted to `dyn B` takes the second
/// embedded dictionary. The builtin `Show`'s `dyn` descriptor has the
/// empty module path.
#[test]
fn forwarders_follow_the_dyn_interface() {
    let l = lowered(&[(
        "",
        r"
interface A {
  fn a(Self) -> Int
}

interface B {
  fn b(Self) -> Int
}

interface J: A + B {
  fn j(Self) -> Int
}

type T {
  T
}

fn a(_t: T) -> Int {
  1
}

fn b(_t: T) -> Int {
  2
}

fn j(_t: T) -> Int {
  3
}

fn via_b<X: B>(x: X) -> Int {
  B::b(x)
}

fn shown<X: Show>(x: X) -> String {
  Show::show(x)
}

fn main() {
  let v: dyn J = T
  let _ = via_b(v)
  let w: dyn B = v
  let _ = B::b(w)
  let s: dyn Show = 1
  let _ = shown(s)
}
",
    )]);
    let unit = app(&l);
    let b = def_named(unit, "app", "B::b");
    let j = def_named(unit, "app", "J");
    let forward = rvalues(
        unit,
        FnKey::DynForward {
            iface_fn: b,
            dyn_iface: j,
        },
    );
    assert!(
        forward.iter().any(|r| matches!(
            r,
            Rvalue::Field {
                layout: LayoutRef::Dict {
                    fns: 3,
                    embedded: 2
                },
                field: 2,
                ..
            }
        )),
        "{forward:?}"
    );
    let main = rvalues(unit, FnKey::Item(def_named(unit, "app", "main")));
    // `w`: the second embedded dictionary of `J`'s, field 5.
    assert!(
        main.iter().any(|r| matches!(
            r,
            Rvalue::Field {
                layout: LayoutRef::Dict {
                    fns: 3,
                    embedded: 2
                },
                field: 5,
                ..
            }
        )),
        "{main:?}"
    );
    let dyn_show = static_named(unit, StaticKind::Desc, "dyn$Show");
    assert_eq!(&*dyn_show.module, "");
    let strings: Vec<&str> = dyn_show.strings.iter().map(|s| &**s).collect();
    assert_eq!(strings, ["dyn Show", ""]);
    static_named(unit, StaticKind::Dict, "Show$dyn Show");
    static_named(unit, StaticKind::Dict, "app::B$dyn app::J");
}

/// Descriptors of types that mention `dyn` are statics too, and a
/// dictionary for such a type holds the forwarding dictionary of its
/// elements (`Show` for `List<dyn Show>`).
#[test]
fn types_mentioning_dyn() {
    let l = lowered(&[(
        "",
        r#"
fn shown<X: Show>(x: X) -> String {
  Show::show(x)
}

fn main() {
  let xs: List<dyn Show> = [1, "a"]
  let _ = shown(xs)
  let _ = "{xs}"
}
"#,
    )]);
    let unit = app(&l);
    let list = static_named(unit, StaticKind::Desc, "List<dyn Show>");
    assert_eq!(list.layout, LayoutRef::Desc { args: 1 });
    assert_eq!(&*target(unit, &list.fields[1]).name, "dyn$Show");
    static_named(unit, StaticKind::Dict, "Show$List<dyn Show>");
}
