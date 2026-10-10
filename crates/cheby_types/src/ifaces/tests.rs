//! Unit tests of satisfaction, dictionaries, operators, `dyn` and
//! interface declarations.
//!
//! Interfaces come from source through the whole front end
//! ([`test_support`]), with fake standard-library modules. The API is then
//! called directly from a *probe* module: one more module, resolved and
//! given its interface like any other, whose dependencies the test
//! chooses.

use std::sync::Arc;

use cheby_diag::{Diagnostic, Span, TextRange};
use cheby_iface::{
    DefId, DepView, GenericParam, Generics, Ice, IndexVec, ModuleId, ModuleInfo, ModuleInterface,
    Namespace, PackageId, ParamIdx, Ty, TyKind, builtin,
};
use cheby_package::ModulePath;
use cheby_resolve::{
    Lookup, ModuleLookup, ResolveInput, ResolvedModule, StdCatalog, collect_imports,
    resolve_import_targets, resolve_module,
};
use cheby_syntax::SyntaxTree;

use super::*;
use crate::test_support::Build;
use crate::test_support::quiet::build_with_std;
use crate::thir::{BinOpKind, Callee, Dict, DictFn, OpImpl};
use crate::{TypesInput, build_interface};

const OPS: &str = r"//! Fake `std::ops`.

pub interface Add {
  fn add(Self, Self) -> Self
}

pub interface Sub {
  fn sub(Self, Self) -> Self
}

pub interface Mul {
  fn mul(Self, Self) -> Self
}

pub interface Div {
  fn div(Self, Self) -> Self
}

pub interface Neg {
  fn neg(Self) -> Self
}

pub interface Compare {
  fn compare(Self, Self) -> Order
}
";

const INT: &str = r#"//! Fake `std::int`.

pub fn show(n: Int) -> String {
  "int"
}

pub fn compare(a: Int, b: Int) -> Order {
  Equal
}

pub fn add(a: Int, b: Int) -> Int {
  a
}
"#;

const STRING: &str = r"//! Fake `std::string`.

pub fn show(text: String) -> String {
  text
}

pub fn compare(a: String, b: String) -> Order {
  Equal
}

pub fn add(a: String, b: String) -> String {
  a
}
";

const BOOL: &str = r#"//! Fake `std::bool`.

pub fn show(b: Bool) -> String {
  "bool"
}

pub fn compare(a: Bool, b: Bool) -> Order {
  Equal
}
"#;

const FLOAT: &str = r#"//! Fake `std::float`.

pub fn show(x: Float) -> String {
  "float"
}
"#;

const LIST: &str = r#"//! Fake `std::list`.

import std::ops::{Compare}

pub fn show<T: Show>(xs: List<T>) -> String {
  "list"
}

pub fn compare<T: Compare>(a: List<T>, b: List<T>) -> Order {
  Equal
}
"#;

const OPTION: &str = r#"//! Fake `std::option`.

pub fn show<T: Show>(o: Option<T>) -> String {
  "option"
}
"#;

fn std_modules() -> Vec<(&'static str, &'static str)> {
    vec![
        ("ops", OPS),
        ("int", INT),
        ("string", STRING),
        ("bool", BOOL),
        ("float", FLOAT),
        ("list", LIST),
        ("option", OPTION),
    ]
}

const MAIN: &str = r"//! Root.

fn main() {}
";

const POINT: &str = r#"//! A point that satisfies `Show`, `Add`, `Neg` and `Compare` (§8.2.1).

pub type Point {
  x: Int,
  y: Int,
}

pub fn show(p: Point) -> String {
  "point"
}

pub fn add(a: Point, b: Point) -> Point {
  a
}

pub fn neg(p: Point) -> Point {
  p
}

pub fn compare(a: Point, b: Point) -> Order {
  Equal
}
"#;

const ANGLE: &str = r#"//! §8.2: a `pub` type whose `show` is only package-visible.

pub type Angle {
  radians: Float,
}

fn show(a: Angle) -> String {
  "angle"
}
"#;

const SHAPES: &str = r"//! Interfaces.

import std::ops::{Add, Sub}

pub interface Shape {
  fn area(Self) -> Float
  fn scale(Self, Float) -> Self
}

pub interface Area {
  fn area(Self) -> Float
}

pub interface Named: Show {
  fn name(Self) -> String
}

pub interface Sized: Shape

pub interface Any

pub interface Num: Add + Sub
";

const CIRCLE: &str = r#"//! A type that satisfies `Shape`, `Area` and `Named`.

pub type Circle {
  radius: Float,
}

pub fn area(c: Circle) -> Float {
  1.0
}

pub fn scale(c: Circle, k: Float) -> Circle {
  c
}

pub fn show(c: Circle) -> String {
  "circle"
}

pub fn name(c: Circle) -> String {
  "circle"
}
"#;

const BAD: &str = r#"//! A `show` with the wrong signature.

pub type Bad {
  Bad,
}

pub fn show(b: Bad, n: Int) -> String {
  "bad"
}
"#;

const BOXED: &str = r#"//! A generic type: `show` with a bound, `compare` without.

pub type Box<T> {
  Box(T),
}

pub fn show<T: Show>(b: Box<T>) -> String {
  "box"
}

pub fn compare<T>(a: Box<T>, b: Box<T>) -> Order {
  Equal
}
"#;

const PAIR: &str = r#"//! Two types, one `show` (D-045).

pub type Left {
  Left,
}

pub type Right {
  Right,
}

pub fn show(l: Left) -> String {
  "left"
}
"#;

fn local_modules() -> Vec<(&'static str, &'static str)> {
    vec![
        ("", MAIN),
        ("geometry::point", POINT),
        ("geometry::angle", ANGLE),
        ("shapes", SHAPES),
        ("circle", CIRCLE),
        ("bad", BAD),
        ("boxed", BOXED),
        ("pair", PAIR),
    ]
}

/// The full path of a module: `std::…` as written, else under `app`.
fn full(path: &str) -> String {
    if path.starts_with("std::") {
        path.to_owned()
    } else {
        format!("app::{path}")
    }
}

/// Finds the build's modules by path.
struct Finder<'m> {
    modules: &'m cheby_iface::ModuleTable,
}

impl ModuleLookup for Finder<'_> {
    fn lookup(&mut self, _from: PackageId, path: &[&str]) -> Result<Lookup, Ice> {
        let (first, rest) = path.split_first().ok_or_else(|| Ice::new("empty path"))?;
        let path = rest.iter().fold(ModulePath::root(first), |p, s| p.child(s));
        Ok(match self.modules.find(&path) {
            Some(id) => Lookup::Found(id),
            None => Lookup::NoPackage,
        })
    }
}

/// A build plus one more module, `probe`, from which the tests call the
/// API.
struct Probe {
    b: Build,
    module: ModuleId,
    package: PackageId,
    tree: SyntaxTree,
    resolved: ResolvedModule,
    own: ModuleInterface,
    deps: Vec<ModuleId>,
    /// The probe's own resolution and declaration diagnostics.
    diags: Vec<Diagnostic>,
}

/// The default build with a local probe module that reads every module.
fn probe(source: &str) -> Probe {
    probe_with(PackageId::LOCAL, None, source)
}

/// A probe of package `package` reading only `deps` (paths), or every
/// module of the build.
fn probe_with(package: PackageId, deps: Option<&[&str]>, source: &str) -> Probe {
    let mut b = build_with_std(&std_modules(), &local_modules());
    let file = b.sources.add_file("src/probe.cheby", source).unwrap();
    let path = match package {
        PackageId::STD => ModulePath::root("std").child("probe"),
        _ => ModulePath::root("app").child("probe"),
    };
    let module = b
        .modules
        .add(ModuleInfo {
            path,
            package,
            file: Some(file),
        })
        .unwrap();
    let (tree, errors) = cheby_syntax::parse(file, Arc::from(source)).into_parts();
    assert!(errors.is_empty(), "{errors:?}");
    let deps: Vec<ModuleId> = match deps {
        Some(paths) => {
            let mut ids: Vec<ModuleId> = paths.iter().map(|p| b.module(&full(p))).collect();
            ids.sort();
            ids
        }
        None => (1..module.as_u32()).map(ModuleId::from_u32).collect(),
    };
    let requests = collect_imports(&tree, &b.globals.names);
    let mut finder = Finder {
        modules: &b.modules,
    };
    let (targets, mut diags) =
        resolve_import_targets(&requests, package, &mut finder, &b.globals.names).unwrap();
    let input = ResolveInput {
        module,
        package,
        tree: &tree,
        imports: &requests,
        targets: &targets,
        deps: DepView::new(&b.set, &deps),
        modules: &b.modules,
        globals: &b.globals,
        std: StdCatalog::default(),
        package_targets: cheby_package::Targets::ALL,
    };
    let (resolved, d) = resolve_module(&input).unwrap();
    diags.extend(d);
    let types_input = TypesInput {
        module,
        package,
        tree: &tree,
        resolved: &resolved,
        deps: DepView::new(&b.set, &deps),
        modules: &b.modules,
        globals: &b.globals,
    };
    let (own, d) = build_interface(&types_input).unwrap();
    diags.extend(d);
    Probe {
        b,
        module,
        package,
        tree,
        resolved,
        own,
        deps,
        diags,
    }
}

impl Probe {
    fn input(&self) -> TypesInput<'_> {
        TypesInput {
            module: self.module,
            package: self.package,
            tree: &self.tree,
            resolved: &self.resolved,
            deps: DepView::new(&self.b.set, &self.deps),
            modules: &self.b.modules,
            globals: &self.b.globals,
        }
    }

    /// Runs `f` with an [`IfaceCx`] whose enclosing item has `generics`.
    fn with<R>(&self, generics: &Generics, f: impl FnOnce(&IfaceCx<'_>) -> R) -> R {
        let input = self.input();
        let cx = IfaceCx {
            input: &input,
            own: &self.own,
            generics,
        };
        f(&cx)
    }

    fn types(&self) -> &cheby_iface::TyInterner {
        &self.b.globals.types
    }

    /// The type item `name` of the module at `path` (or the probe's).
    fn type_def(&self, path: &str, name: &str) -> DefId {
        if path == "probe" {
            let name = self.b.globals.names.intern(name);
            return DefId::new(self.module, self.own.lookup(Namespace::Type, name).unwrap());
        }
        self.b.def_in(&full(path), Namespace::Type, name)
    }

    /// The nominal type `name` of the module at `path`, with `args`.
    fn named(&self, path: &str, name: &str, args: &[Ty]) -> Ty {
        self.types().named(self.type_def(path, name), args)
    }

    fn list(&self, elem: Ty) -> Ty {
        self.types().named(builtin::LIST, [elem])
    }

    fn tuple(&self, elems: &[Ty]) -> Ty {
        self.types().intern(TyKind::Tuple(elems.into()))
    }

    fn dyn_of(&self, iface: DefId) -> Ty {
        self.types().intern(TyKind::Dyn {
            iface,
            args: Box::new([]),
        })
    }

    fn param(&self, n: u32) -> Ty {
        self.types().param(ParamIdx::from_u32(n))
    }

    fn func(&self, path: &str, name: &str) -> DefId {
        self.b.def(&full(path), name)
    }

    /// The interface function `f` of interface `iface`.
    fn iface_fn(&self, iface: DefId, f: &str) -> DefId {
        let name = self.b.globals.names.intern(f);
        let module = self.b.set.get(iface.module).unwrap();
        let (index, _) = module
            .items
            .iter_enumerated()
            .find(|(_, item)| item.name == name && item.parent == Some(iface.index))
            .unwrap();
        DefId::new(iface.module, index)
    }

    /// Type parameters named `T`, `U`, … with these bounds.
    fn generics(&self, bounds: &[&[DefId]]) -> Generics {
        let names = ["T", "U", "V"];
        let params = bounds
            .iter()
            .zip(names)
            .map(|(b, n)| GenericParam {
                name: self.b.globals.names.intern(n),
                bounds: (*b).into(),
            })
            .collect();
        Generics {
            params: IndexVec::from_raw(params).unwrap(),
        }
    }

    /// A span in the probe's source.
    fn span(&self) -> Span {
        Span::new(self.tree.file(), TextRange::at(0, 2))
    }

    fn render(&self, diag: Diagnostic) -> String {
        cheby_diag::render_all(&[diag], &self.b.sources)
    }

    fn int_show(&self) -> Dict {
        show_impl(
            Ty::INT,
            vec![impl_fn(self.func("std::int", "show"), &[], vec![])],
        )
    }
}

fn impl_fn(def: DefId, ty_args: &[Ty], dicts: Vec<Dict>) -> DictFn {
    DictFn {
        def,
        ty_args: ty_args.into(),
        dicts: dicts.into(),
    }
}

fn show_impl(ty: Ty, fns: Vec<DictFn>) -> Dict {
    Dict::Impl {
        iface: builtin::SHOW,
        ty,
        fns: fns.into(),
    }
}

const PROBE: &str = r"//! The checking module.

fn probe() {}
";

fn no_generics() -> Generics {
    Generics::empty()
}

#[test]
fn the_build_has_no_errors() {
    let p = probe(PROBE);
    let errors: Vec<String> =
        p.b.diags
            .iter()
            .chain(&p.diags)
            .filter(|d| d.is_error())
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect();
    assert_eq!(errors, Vec::<String>::new());
}

#[test]
fn builtin_types_satisfy_through_their_home_module() {
    let p = probe(PROBE);
    let g = no_generics();
    p.with(&g, |cx| {
        assert_eq!(
            satisfy(cx, Ty::INT, builtin::SHOW).unwrap(),
            Ok(p.int_show())
        );
        // `String`'s `show` is `std::string::show`.
        let callee = show(cx, Ty::STRING).unwrap().unwrap();
        assert_eq!(
            callee,
            Callee::Fn {
                def: p.func("std::string", "show"),
                ty_args: Box::new([]),
                dicts: Box::new([]),
            }
        );
    });
}

#[test]
fn conditional_conformance_passes_the_bounds_dictionaries() {
    let p = probe(PROBE);
    let g = no_generics();
    let list_int = p.list(Ty::INT);
    p.with(&g, |cx| {
        let dict = satisfy(cx, list_int, builtin::SHOW).unwrap().unwrap();
        let expected = show_impl(
            list_int,
            vec![impl_fn(
                p.func("std::list", "show"),
                &[Ty::INT],
                vec![p.int_show()],
            )],
        );
        assert_eq!(dict, expected);
        // Nested: `Option<List<Point>>`.
        let point = p.named("geometry::point", "Point", &[]);
        let opt = p.types().named(builtin::OPTION, [p.list(point)]);
        let Dict::Impl { fns, .. } = satisfy(cx, opt, builtin::SHOW).unwrap().unwrap() else {
            panic!("not an Impl");
        };
        let [option_show] = &*fns else {
            panic!("{fns:?}")
        };
        assert_eq!(option_show.def, p.func("std::option", "show"));
        let [Dict::Impl { fns: inner, .. }] = &*option_show.dicts else {
            panic!("{option_show:?}")
        };
        assert_eq!(inner[0].def, p.func("std::list", "show"));
        assert_eq!(
            inner[0].dicts[0],
            show_impl(
                point,
                vec![impl_fn(p.func("geometry::point", "show"), &[], vec![])]
            )
        );
    });
}

#[test]
fn a_function_type_element_does_not_satisfy_a_bound() {
    let p = probe(PROBE);
    let g = no_generics();
    let f = p.types().func([], Ty::NIL);
    let list_f = p.list(f);
    p.with(&g, |cx| {
        let err = satisfy(cx, list_f, builtin::SHOW).unwrap().unwrap_err();
        let Reason::Bound { found, inner, .. } = &err.reason else {
            panic!("{err:?}")
        };
        assert_eq!(*found, p.func("std::list", "show"));
        assert_eq!(inner.ty, f);
        assert_eq!(inner.reason, Reason::NoModule);
        let text = p.render(unsatisfied_diagnostic(
            cx,
            &err,
            p.span(),
            Need::Interpolation,
        ));
        assert!(
            text.contains("error[E0063]: `List<fn() -> Nil>` does not satisfy `Show`"),
            "{text}"
        );
        assert!(
            text.contains("`list::show` requires `T: Show`, and `fn() -> Nil` does not satisfy it"),
            "{text}"
        );
        assert!(
            text.contains("function types satisfy only empty interfaces"),
            "{text}"
        );
        assert!(text.contains(":?}` to use debug printing"), "{text}");
    });
}

/// §8.2: a function less visible than its type does not count, and the
/// error points at it.
#[test]
fn a_less_visible_function_does_not_count() {
    let p = probe(PROBE);
    let g = no_generics();
    let angle = p.named("geometry::angle", "Angle", &[]);
    p.with(&g, |cx| {
        let err = satisfy(cx, angle, builtin::SHOW).unwrap().unwrap_err();
        assert!(
            matches!(
                err.reason,
                Reason::LessVisible {
                    type_vis: Vis::Pub,
                    ..
                }
            ),
            "{err:?}"
        );
        let text = p.render(unsatisfied_diagnostic(
            cx,
            &err,
            p.span(),
            Need::Interpolation,
        ));
        assert!(
            text.contains("`angle::Angle` does not satisfy `Show`"),
            "{text}"
        );
        assert!(text.contains("src/geometry/angle.cheby"), "{text}");
        assert!(
            text.contains("`angle::show` is package-visible, less visible than `angle::Angle`"),
            "{text}"
        );
        assert!(
            text.contains("help: make `show` `pub`, or make `angle::Angle` less visible"),
            "{text}"
        );
    });
}

#[test]
fn a_wrong_signature_and_one_type_per_module() {
    let p = probe(PROBE);
    let g = no_generics();
    let bad = p.named("bad", "Bad", &[]);
    let right = p.named("pair", "Right", &[]);
    let left = p.named("pair", "Left", &[]);
    p.with(&g, |cx| {
        let err = satisfy(cx, bad, builtin::SHOW).unwrap().unwrap_err();
        assert!(matches!(err.reason, Reason::Signature { .. }), "{err:?}");
        let text = p.render(unsatisfied_diagnostic(
            cx,
            &err,
            p.span(),
            Need::Interpolation,
        ));
        assert!(
            text.contains("`bad::show` has the signature `show(bad::Bad, Int) -> String`"),
            "{text}"
        );
        assert!(
            text.contains("`Show` needs `show(bad::Bad) -> String`"),
            "{text}"
        );
        // `pair` has one `show`, for `Left` (D-045).
        assert!(satisfy(cx, left, builtin::SHOW).unwrap().is_ok());
        let err = satisfy(cx, right, builtin::SHOW).unwrap().unwrap_err();
        assert!(matches!(err.reason, Reason::Signature { .. }), "{err:?}");
    });
}

#[test]
fn a_missing_function_names_the_module() {
    let p = probe(PROBE);
    let g = no_generics();
    let area = p.type_def("shapes", "Area");
    let angle = p.named("geometry::angle", "Angle", &[]);
    p.with(&g, |cx| {
        let err = satisfy(cx, angle, area).unwrap().unwrap_err();
        assert!(matches!(err.reason, Reason::Missing { .. }), "{err:?}");
        let text = p.render(unsatisfied_diagnostic(cx, &err, p.span(), Need::Dyn));
        assert!(
            text.contains("`angle::Angle` does not satisfy `shapes::Area`"),
            "{text}"
        );
        assert!(
            text.contains("`app::geometry::angle` has no function `area(angle::Angle) -> Float`"),
            "{text}"
        );
        assert!(
            text.contains("converted to `dyn shapes::Area` here"),
            "{text}"
        );
    });
}

#[test]
fn own_module_types_and_private_functions() {
    let source = r#"//! A `priv` type counts `priv` functions.

priv type Secret {
  Secret,
}

priv fn show(s: Secret) -> String {
  "secret"
}
"#;
    let p = probe(source);
    let g = no_generics();
    let secret = p.named("probe", "Secret", &[]);
    p.with(&g, |cx| {
        let dict = satisfy(cx, secret, builtin::SHOW).unwrap().unwrap();
        let show_fn = DefId::new(
            p.module,
            p.own
                .lookup(Namespace::Value, p.b.globals.names.intern("show"))
                .unwrap(),
        );
        assert_eq!(dict, show_impl(secret, vec![impl_fn(show_fn, &[], vec![])]));
    });
}

#[test]
fn embedding_orders_the_dictionary() {
    let p = probe(PROBE);
    let g = no_generics();
    let named = p.type_def("shapes", "Named");
    let circle = p.named("circle", "Circle", &[]);
    p.with(&g, |cx| {
        let Dict::Impl { fns, .. } = satisfy(cx, circle, named).unwrap().unwrap() else {
            panic!()
        };
        let defs: Vec<DefId> = fns.iter().map(|f| f.def).collect();
        assert_eq!(defs, [p.func("circle", "show"), p.func("circle", "name")]);
        // `Angle` fails on the embedded `show`, which the note says.
        let angle = p.named("geometry::angle", "Angle", &[]);
        let err = satisfy(cx, angle, named).unwrap().unwrap_err();
        let text = p.render(unsatisfied_diagnostic(cx, &err, p.span(), Need::Dyn));
        assert!(
            text.contains("`show` is required by `Show`, which `shapes::Named` embeds"),
            "{text}"
        );
    });
}

#[test]
fn tuples_satisfy_show_and_compare_by_the_builtin_rule() {
    let p = probe(PROBE);
    let g = no_generics();
    let compare = p.type_def("std::ops", "Compare");
    let add = p.type_def("std::ops", "Add");
    let any = p.type_def("shapes", "Any");
    let point = p.named("geometry::point", "Point", &[]);
    let pair = p.tuple(&[Ty::INT, Ty::STRING]);
    p.with(&g, |cx| {
        let Dict::Tuple { iface, elems, .. } = satisfy(cx, pair, builtin::SHOW).unwrap().unwrap()
        else {
            panic!()
        };
        assert_eq!(iface, builtin::SHOW);
        assert_eq!(elems[0], p.int_show());
        let with_point = p.tuple(&[Ty::INT, point]);
        assert!(matches!(
            satisfy(cx, with_point, compare).unwrap(),
            Ok(Dict::Tuple { .. })
        ));
        // Only `Show`, `Compare` and empty interfaces.
        let err = satisfy(cx, pair, add).unwrap().unwrap_err();
        assert_eq!(err.reason, Reason::NoModule);
        assert!(matches!(
            satisfy(cx, pair, any).unwrap(),
            Ok(Dict::Impl { .. })
        ));
        // An element that fails.
        let angle = p.named("geometry::angle", "Angle", &[]);
        let with_angle = p.tuple(&[Ty::INT, angle]);
        let err = satisfy(cx, with_angle, builtin::SHOW).unwrap().unwrap_err();
        let Reason::Element { index: 1, .. } = err.reason else {
            panic!("{err:?}")
        };
        let text = p.render(unsatisfied_diagnostic(
            cx,
            &err,
            p.span(),
            Need::Interpolation,
        ));
        assert!(
            text.contains("element 2 of `(Int, angle::Angle)` is `angle::Angle`"),
            "{text}"
        );
        assert!(text.contains("`angle::show` is package-visible"), "{text}");
    });
}

#[test]
fn function_types_and_never_satisfy_only_empty_interfaces() {
    let p = probe(PROBE);
    let g = no_generics();
    let any = p.type_def("shapes", "Any");
    let f = p.types().func([Ty::INT], Ty::STRING);
    p.with(&g, |cx| {
        assert_eq!(
            satisfy(cx, f, any).unwrap(),
            Ok(Dict::Impl {
                iface: any,
                ty: f,
                fns: Box::new([])
            })
        );
        assert_eq!(
            satisfy(cx, f, builtin::SHOW).unwrap().unwrap_err().reason,
            Reason::NoModule
        );
        assert!(satisfy(cx, Ty::NEVER, any).unwrap().is_ok());
        let err = satisfy(cx, Ty::NEVER, builtin::SHOW).unwrap().unwrap_err();
        let text = p.render(unsatisfied_diagnostic(
            cx,
            &err,
            p.span(),
            Need::Interpolation,
        ));
        assert!(text.contains("`Never` is declared in no module"), "{text}");
    });
}

#[test]
fn errors_are_silent() {
    let p = probe(PROBE);
    let g = no_generics();
    let list_err = p.list(Ty::ERROR);
    p.with(&g, |cx| {
        assert!(satisfy(cx, list_err, builtin::SHOW).unwrap().is_ok());
        assert!(show(cx, Ty::ERROR).unwrap().is_ok());
        assert_eq!(
            operator(cx, BinOpKind::Add, Ty::ERROR).unwrap(),
            Ok(OpImpl::Builtin)
        );
        assert!(dyn_coerce(cx, Ty::ERROR, builtin::SHOW).unwrap().is_ok());
    });
}

#[test]
fn type_parameters_satisfy_through_their_bounds() {
    let p = probe(PROBE);
    let named = p.type_def("shapes", "Named");
    let shape = p.type_def("shapes", "Shape");
    let area = p.type_def("shapes", "Area");
    let any = p.type_def("shapes", "Any");
    let g = p.generics(&[&[shape, named], &[]]);
    let t = p.param(0);
    let u = p.param(1);
    p.with(&g, |cx| {
        assert_eq!(
            satisfy(cx, t, named).unwrap(),
            Ok(Dict::Param {
                param: ParamIdx::from_u32(0),
                bound: 1
            })
        );
        assert_eq!(
            satisfy(cx, t, builtin::SHOW).unwrap(),
            Ok(Dict::Embedded {
                of: Box::new(Dict::Param {
                    param: ParamIdx::from_u32(0),
                    bound: 1
                }),
                iface: builtin::SHOW
            })
        );
        let err = satisfy(cx, t, area).unwrap().unwrap_err();
        assert_eq!(
            err.reason,
            Reason::Param {
                param: ParamIdx::from_u32(0)
            }
        );
        let text = p.render(unsatisfied_diagnostic(
            cx,
            &err,
            p.span(),
            Need::Bound {
                callee: p.func("circle", "area"),
            },
        ));
        assert!(
            text.contains("help: add the bound `T: shapes::Area`"),
            "{text}"
        );
        assert!(text.contains("required by a bound of `area`"), "{text}");
        // An empty interface: any type, but a parameter needs a bound to
        // supply its descriptor (D-236).
        assert!(matches!(
            satisfy(cx, t, any).unwrap(),
            Ok(Dict::Impl { .. })
        ));
        let err = satisfy(cx, u, any).unwrap().unwrap_err();
        assert_eq!(
            err.reason,
            Reason::Descriptor {
                param: ParamIdx::from_u32(1)
            }
        );
    });
}

/// §8.6.1: `wrap`, `label` and `wrap_bad`.
#[test]
fn dyn_conversions_need_a_descriptor() {
    let p = probe(PROBE);
    let any = p.type_def("shapes", "Any");
    let g = p.generics(&[&[any], &[builtin::SHOW], &[]]);
    let (bounded, shown, free) = (p.param(0), p.param(1), p.param(2));
    p.with(&g, |cx| {
        assert!(matches!(
            dyn_coerce(cx, p.list(bounded), any).unwrap(),
            Ok(Dict::Impl { .. })
        ));
        let Dict::Impl { fns, .. } = dyn_coerce(cx, p.list(shown), builtin::SHOW)
            .unwrap()
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            fns[0].dicts[0],
            Dict::Param {
                param: ParamIdx::from_u32(1),
                bound: 0
            }
        );
        let err = dyn_coerce(cx, p.list(free), any).unwrap().unwrap_err();
        let diag = unsatisfied_diagnostic(cx, &err, p.span(), Need::Dyn);
        assert_eq!(diag.code, cheby_diag::codes::DYN_NEEDS_BOUND);
        let text = p.render(diag);
        assert!(
            text.contains(
                "cannot convert `List<V>` to `dyn shapes::Any`: the type parameter `V` has no bound"
            ),
            "{text}"
        );
        assert!(
            text.contains("help: add a bound such as `V: Any`"),
            "{text}"
        );
    });
}

/// GAP(bound-needs-descriptor): a dictionary passed for a bound carries a
/// descriptor too.
#[test]
fn a_bound_dictionary_needs_a_descriptor() {
    let p = probe(PROBE);
    let compare = p.type_def("std::ops", "Compare");
    let g = p.generics(&[&[]]);
    let boxed = p.named("boxed", "Box", &[p.param(0)]);
    p.with(&g, |cx| {
        // As the operand of `<`, `boxed::compare` is called directly.
        assert_eq!(
            operator(cx, BinOpKind::Lt, boxed).unwrap(),
            Ok(OpImpl::Call(Callee::Fn {
                def: p.func("boxed", "compare"),
                ty_args: Box::new([p.param(0)]),
                dicts: Box::new([]),
            }))
        );
        // As a dictionary, it needs `T`'s descriptor.
        let err = satisfy(cx, boxed, compare).unwrap().unwrap_err();
        let diag = unsatisfied_diagnostic(cx, &err, p.span(), Need::Bound { callee: p.func("std::list", "compare") });
        assert_eq!(diag.code, cheby_diag::codes::DYN_NEEDS_BOUND);
        // Nested inside a bound of the satisfying function.
        let list_boxed = p.list(boxed);
        let err = satisfy(cx, list_boxed, compare).unwrap().unwrap_err();
        let diag = unsatisfied_diagnostic(cx, &err, p.span(), Need::Bound { callee: p.func("std::list", "compare") });
        assert_eq!(diag.code, cheby_diag::codes::DYN_NEEDS_BOUND);
        let text = p.render(diag);
        assert!(text.contains("`list::compare` requires `T: ops::Compare`, so it receives a dictionary for `boxed::Box<T>`"), "{text}");
    });
}

#[test]
fn dyn_values_satisfy_their_interface_and_embedded_ones() {
    let p = probe(PROBE);
    let named = p.type_def("shapes", "Named");
    let area = p.type_def("shapes", "Area");
    let any = p.type_def("shapes", "Any");
    let g = no_generics();
    let dyn_named = p.dyn_of(named);
    let dyn_area = p.dyn_of(area);
    let circle = p.named("circle", "Circle", &[]);
    p.with(&g, |cx| {
        assert_eq!(
            dyn_coerce(cx, dyn_named, builtin::SHOW).unwrap(),
            Ok(Dict::Dyn {
                iface: builtin::SHOW,
                ty: dyn_named
            })
        );
        assert_eq!(
            dyn_coerce(cx, dyn_named, named).unwrap(),
            Ok(Dict::Dyn {
                iface: named,
                ty: dyn_named
            })
        );
        assert!(matches!(
            dyn_coerce(cx, dyn_named, any).unwrap(),
            Ok(Dict::Impl { .. })
        ));
        let err = dyn_coerce(cx, dyn_area, builtin::SHOW)
            .unwrap()
            .unwrap_err();
        assert_eq!(err.reason, Reason::Dyn { has: area });
        let text = p.render(unsatisfied_diagnostic(cx, &err, p.span(), Need::Dyn));
        assert!(
            text.contains(
                "`dyn shapes::Area` satisfies only `shapes::Area` and the interfaces it embeds"
            ),
            "{text}"
        );
        // A concrete value packaged with its functions.
        let Dict::Impl { fns, .. } = dyn_coerce(cx, circle, area).unwrap().unwrap() else {
            panic!()
        };
        assert_eq!(fns[0].def, p.func("circle", "area"));
    });
}

#[test]
fn interface_calls_resolve_by_the_self_type() {
    let p = probe(PROBE);
    let named = p.type_def("shapes", "Named");
    let area = p.type_def("shapes", "Area");
    let g = p.generics(&[&[named]]);
    let point = p.named("geometry::point", "Point", &[]);
    let angle = p.named("geometry::angle", "Angle", &[]);
    let area_fn = p.iface_fn(area, "area");
    p.with(&g, |cx| {
        assert_eq!(
            iface_call(cx, builtin::SHOW_FN, point).unwrap(),
            Ok(Callee::Fn {
                def: p.func("geometry::point", "show"),
                ty_args: Box::new([]),
                dicts: Box::new([]),
            })
        );
        assert!(matches!(
            iface_call(cx, builtin::SHOW_FN, p.param(0)).unwrap(),
            Ok(Callee::Iface {
                dict: Dict::Embedded { .. },
                ..
            })
        ));
        assert_eq!(
            iface_call(cx, builtin::SHOW_FN, p.dyn_of(named)).unwrap(),
            Ok(Callee::Dyn {
                iface_fn: builtin::SHOW_FN
            })
        );
        assert!(matches!(
            iface_call(cx, builtin::SHOW_FN, p.tuple(&[Ty::INT, point])).unwrap(),
            Ok(Callee::Iface {
                dict: Dict::Tuple { .. },
                ..
            })
        ));
        let err = iface_call(cx, area_fn, angle).unwrap().unwrap_err();
        let text = p.render(unsatisfied_diagnostic(
            cx,
            &err,
            p.span(),
            Need::IfaceCall { iface_fn: area_fn },
        ));
        assert!(
            text.contains("`shapes::Area::area` is called with `Self` = `angle::Angle`"),
            "{text}"
        );
    });
}

#[test]
fn operators_use_builtins_or_the_operand_types_functions() {
    let p = probe(PROBE);
    let g = no_generics();
    let point = p.named("geometry::point", "Point", &[]);
    let call = |def: DefId, ty_args: &[Ty], dicts: Vec<Dict>| {
        Ok(OpImpl::Call(Callee::Fn {
            def,
            ty_args: ty_args.into(),
            dicts: dicts.into(),
        }))
    };
    p.with(&g, |cx| {
        for op in [BinOpKind::Add, BinOpKind::Lt, BinOpKind::Rem, BinOpKind::Eq] {
            assert_eq!(operator(cx, op, Ty::INT).unwrap(), Ok(OpImpl::Builtin));
            assert_eq!(operator(cx, op, Ty::U8).unwrap(), Ok(OpImpl::Builtin));
        }
        assert_eq!(negation(cx, Ty::FLOAT).unwrap(), Ok(OpImpl::Builtin));
        assert_eq!(operator(cx, BinOpKind::Add, point).unwrap(), call(p.func("geometry::point", "add"), &[], vec![]));
        assert_eq!(operator(cx, BinOpKind::Ge, point).unwrap(), call(p.func("geometry::point", "compare"), &[], vec![]));
        assert_eq!(negation(cx, point).unwrap(), call(p.func("geometry::point", "neg"), &[], vec![]));
        assert_eq!(operator(cx, BinOpKind::Add, Ty::STRING).unwrap(), call(p.func("std::string", "add"), &[], vec![]));
        assert_eq!(operator(cx, BinOpKind::Lt, Ty::STRING).unwrap(), call(p.func("std::string", "compare"), &[], vec![]));
        assert_eq!(operator(cx, BinOpKind::Lt, Ty::BOOL).unwrap(), call(p.func("std::bool", "compare"), &[], vec![]));
        let int_compare = Dict::Impl {
            iface: p.type_def("std::ops", "Compare"),
            ty: Ty::INT,
            fns: Box::new([impl_fn(p.func("std::int", "compare"), &[], vec![])]),
        };
        assert_eq!(
            operator(cx, BinOpKind::Le, p.list(Ty::INT)).unwrap(),
            call(p.func("std::list", "compare"), &[Ty::INT], vec![int_compare])
        );
        let pair = p.tuple(&[Ty::INT, Ty::STRING]);
        assert!(matches!(
            operator(cx, BinOpKind::Gt, pair).unwrap(),
            Ok(OpImpl::Call(Callee::Iface { dict: Dict::Tuple { .. }, .. }))
        ));
        // Not defined.
        let err = operator(cx, BinOpKind::Add, Ty::BOOL).unwrap().unwrap_err();
        let text = p.render(unsatisfied_diagnostic(cx, &err, p.span(), Need::Operator(BinOpKind::Add)));
        assert!(text.contains("error[E0063]: `Bool` does not satisfy `Add`"), "{text}");
        assert!(text.contains("`+` calls `add` of the operand type"), "{text}");
        assert!(text.contains("`Bool` gets its functions from `std::bool`, which has no function `add(Bool, Bool) -> Bool`"), "{text}");
        assert_eq!(operator(cx, BinOpKind::Mul, pair).unwrap().unwrap_err().reason, Reason::NoModule);
        let angle = p.named("geometry::angle", "Angle", &[]);
        assert!(matches!(negation(cx, angle).unwrap().unwrap_err().reason, Reason::Missing { .. }));
    });
}

/// The `largest` example of ops.md: `>` on a type parameter bounded by
/// `Compare`, and `+` through an embedding interface.
#[test]
fn operators_on_type_parameters_need_a_bound() {
    let p = probe(PROBE);
    let compare = p.type_def("std::ops", "Compare");
    let add = p.type_def("std::ops", "Add");
    let num = p.type_def("shapes", "Num");
    let g = p.generics(&[&[compare], &[num], &[builtin::SHOW]]);
    p.with(&g, |cx| {
        assert_eq!(
            operator(cx, BinOpKind::Gt, p.param(0)).unwrap(),
            Ok(OpImpl::Call(Callee::Iface {
                iface_fn: p.iface_fn(compare, "compare"),
                dict: Dict::Param {
                    param: ParamIdx::from_u32(0),
                    bound: 0
                },
            }))
        );
        assert_eq!(
            operator(cx, BinOpKind::Add, p.param(1)).unwrap(),
            Ok(OpImpl::Call(Callee::Iface {
                iface_fn: p.iface_fn(add, "add"),
                dict: Dict::Embedded {
                    of: Box::new(Dict::Param {
                        param: ParamIdx::from_u32(1),
                        bound: 0
                    }),
                    iface: add
                },
            }))
        );
        let err = operator(cx, BinOpKind::Lt, p.param(2))
            .unwrap()
            .unwrap_err();
        let text = p.render(unsatisfied_diagnostic(
            cx,
            &err,
            p.span(),
            Need::Operator(BinOpKind::Lt),
        ));
        assert!(text.contains("`V` does not satisfy `Compare`"), "{text}");
        assert!(
            text.contains("help: add the bound `V: Compare` (`import std::ops::{Compare}`)"),
            "{text}"
        );
    });
}

/// GAP(home-not-in-deps): a std module that does not import `std::int`
/// cannot use `Int`'s functions.
#[test]
fn a_home_module_must_be_readable() {
    let p = probe_with(PackageId::STD, Some(&["std::ops"]), PROBE);
    let g = no_generics();
    p.with(&g, |cx| {
        let err = satisfy(cx, Ty::INT, builtin::SHOW).unwrap().unwrap_err();
        assert_eq!(err.reason, Reason::HomeNotInDeps { home: "int" });
        let text = p.render(unsatisfied_diagnostic(
            cx,
            &err,
            p.span(),
            Need::Interpolation,
        ));
        assert!(
            text.contains(
                "`Int` gets its functions from `std::int`, which this module does not import"
            ),
            "{text}"
        );
        assert!(text.contains("help: add `import std::int`"), "{text}");
        // The tuple rule reaches `String`'s `compare` through `std::string`.
        let err = operator(cx, BinOpKind::Lt, p.tuple(&[Ty::STRING]))
            .unwrap()
            .unwrap_err();
        let Reason::Element { index: 0, inner } = err.reason else {
            panic!("{err:?}")
        };
        assert_eq!(inner.reason, Reason::HomeNotInDeps { home: "string" });
    });
}

#[test]
fn deep_requirements_fail_cleanly() {
    let p = probe(PROBE);
    let g = no_generics();
    let mut ty = Ty::INT;
    for _ in 0..(super::solve::DEPTH_LIMIT + 5) {
        ty = p.list(ty);
    }
    p.with(&g, |cx| {
        let mut err = satisfy(cx, ty, builtin::SHOW).unwrap().unwrap_err();
        while let Reason::Bound { inner, .. } = err.reason {
            err = *inner;
        }
        assert_eq!(err.reason, Reason::TooDeep);
    });
}

#[test]
fn object_safety_names_the_offending_function() {
    let p = probe(PROBE);
    let shape = p.type_def("shapes", "Shape");
    let sized = p.type_def("shapes", "Sized");
    let area = p.type_def("shapes", "Area");
    let named = p.type_def("shapes", "Named");
    let compare = p.type_def("std::ops", "Compare");
    let input = p.input();
    assert!(
        check_object_safe(&input, &p.own, area, p.span())
            .unwrap()
            .is_none()
    );
    assert!(
        check_object_safe(&input, &p.own, named, p.span())
            .unwrap()
            .is_none()
    );
    let diag = check_object_safe(&input, &p.own, shape, p.span())
        .unwrap()
        .unwrap();
    let text = p.render(diag);
    assert!(text.contains("error[E0064]: `dyn shapes::Shape` is not allowed: `scale` uses `Self` other than as its first parameter"), "{text}");
    assert!(text.contains("`fn scale(Self, Float) -> Self`"), "{text}");
    let text = p.render(
        check_object_safe(&input, &p.own, sized, p.span())
            .unwrap()
            .unwrap(),
    );
    assert!(
        text.contains("`scale` comes from `shapes::Shape`, which `shapes::Sized` embeds"),
        "{text}"
    );
    assert!(
        check_object_safe(&input, &p.own, compare, p.span())
            .unwrap()
            .is_some()
    );
}

fn decl_messages(source: &str) -> Vec<String> {
    let p = probe(source);
    let mut out: Vec<String> = p
        .diags
        .iter()
        .map(|d| format!("{}: {}", d.code, d.message))
        .collect();
    out.sort();
    out
}

#[test]
fn interface_declaration_errors() {
    // §8.1, D-209.
    assert_eq!(
        decl_messages(
            r"//! A function without `Self` in a parameter.

interface Default {
  fn default() -> Self
}
"
        ),
        ["E0066: interface function `default` must take `Self` in a parameter"]
    );
    // §8.3: two embedded functions with different signatures.
    assert_eq!(
        decl_messages(
            r"//! E0066's example.

interface Named {
  fn name(Self) -> String
}

interface Numbered {
  fn name(Self) -> Int
}

interface Tagged: Named + Numbered
"
        ),
        ["E0066: `Tagged` inherits two different functions named `name`"]
    );
    // Identical signatures count once; a diamond is fine.
    assert_eq!(
        decl_messages(
            r"//! Identical requirements.

interface Base {
  fn id(Self) -> Int
}

interface Left: Base {
  fn name(Self) -> String
}

interface Right: Base {
  fn name(Self) -> String
}

interface Both: Left + Right
"
        ),
        Vec::<String>::new()
    );
    // An own function with an embedded function's name (§8.1).
    assert_eq!(
        decl_messages(
            r"//! Own and embedded.

interface Again: Show {
  fn show(Self) -> String
}
"
        ),
        ["E0066: `show` is already required by the embedded interface `Show`"]
    );
    // Cycles are reported once.
    assert_eq!(
        decl_messages(
            r"//! A cycle.

interface A: B

interface B: C

interface C: A
"
        ),
        ["E0066: interface `A` embeds itself"]
    );
}

#[test]
fn interface_declaration_diagnostics_render() {
    let p = probe(
        r"//! E0066's example.

interface Named {
  fn name(Self) -> String
}

interface Numbered {
  fn name(Self) -> Int
}

interface Tagged: Named + Numbered
",
    );
    let text = p.render(p.diags[0].clone());
    assert!(
        text.contains("`Named::name` is `fn name(Self) -> String`"),
        "{text}"
    );
    assert!(
        text.contains("`Numbered::name` is `fn name(Self) -> Int`"),
        "{text}"
    );
}

/// The whole checker: interpolating a `List` of functions, as the
/// hand-written golden of `tests/ui/08-interfaces/satisfaction/
/// list_conditional` expects once `std::list` ships.
#[test]
fn interpolating_a_list_of_functions_end_to_end() {
    let source = r#"// @mode: check-fail
// @decisions: D-064, D-166
//! `List<T>` satisfies `Show` when `T` does, through `std::list`'s
//! `show<T: Show>`, and function types do not (§8.2.2).

import std::io

fn main() {
  let actions = [fn() { Nil }]
  io::println("{actions}")
}
"#;
    let b = build_with_std(&std_modules(), &[("", source)]);
    let expected = r#"error[E0063]: `List<fn() -> Nil>` does not satisfy `Show`
  --> src/main.cheby:10:17
   |
10 |   io::println("{actions}")
   |                 ^^^^^^^ interpolated here
   |
   = note: `list::show` requires `T: Show`, and `fn() -> Nil` does not satisfy it (§8.2.2, D-064)
   = note: function types satisfy only empty interfaces (§8.2, D-166)
   = help: write `{actions:?}` to use debug printing instead (D-086)

"#;
    assert_eq!(b.rendered(), expected);
}

/// The whole checker: operators, interpolation and an interface call
/// resolve to the functions of the operand's module (§8.2.1, §8.5).
#[test]
fn a_user_point_end_to_end() {
    let source = r#"//! Uses `Point`.

import app::geometry::point

fn total(a: point::Point, b: point::Point) -> point::Point {
  a + b
}

fn label(p: point::Point) -> String {
  "{p}"
}

fn smaller(a: point::Point, b: point::Point) -> Bool {
  a < b
}

fn shown(p: point::Point) -> String {
  Show::show(p)
}
"#;
    let b = build_with_std(&std_modules(), &[("", source), ("geometry::point", POINT)]);
    assert_eq!(b.messages(), Vec::<String>::new());
    let point_fn = |name: &str| b.def("app::geometry::point", name);
    let callees: Vec<DefId> = ["total", "label", "smaller", "shown"]
        .iter()
        .flat_map(|f| {
            let body = b.thir_fn("app", f).unwrap().body.as_ref().unwrap();
            body.exprs
                .iter()
                .filter_map(|e| match &e.kind {
                    crate::thir::ExprKind::Binary {
                        imp: OpImpl::Call(Callee::Fn { def, .. }),
                        ..
                    }
                    | crate::thir::ExprKind::Call {
                        callee: Callee::Fn { def, .. },
                        ..
                    } => Some(*def),
                    crate::thir::ExprKind::Interpolate { parts } => {
                        parts.iter().find_map(|p| match p {
                            crate::thir::InterpPart::Value {
                                show: Some(Callee::Fn { def, .. }),
                                ..
                            } => Some(*def),
                            _ => None,
                        })
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        callees,
        [
            point_fn("add"),
            point_fn("show"),
            point_fn("compare"),
            point_fn("show")
        ]
    );
}

/// The whole checker on the example of ops.md: `>` on a type parameter
/// bounded by `Compare`, and callers passing `Int`'s and `String`'s
/// dictionaries.
#[test]
fn a_compare_bound_end_to_end() {
    let source = r#"//! `largest` of ops.md, without `list::fold`.

import std::ops::{Compare}

fn larger<T: Compare>(a: T, b: T) -> T {
  case {
    a > b => a
    _ => b
  }
}

fn main() {
  assert larger(3, 7) == 7
  assert larger("pear", "fig") == "pear"
}
"#;
    let b = build_with_std(&std_modules(), &[("", source)]);
    assert_eq!(b.messages(), Vec::<String>::new());
    let compare = b.def_in("std::ops", Namespace::Type, "Compare");
    let larger = b.thir_fn("app", "larger").unwrap().body.as_ref().unwrap();
    let imps: Vec<&OpImpl> = larger
        .exprs
        .iter()
        .filter_map(|e| match &e.kind {
            crate::thir::ExprKind::Binary { imp, .. } => Some(imp),
            _ => None,
        })
        .collect();
    let [OpImpl::Call(Callee::Iface { iface_fn, dict })] = imps.as_slice() else {
        panic!("{imps:?}")
    };
    assert_eq!(
        b.globals
            .names
            .get(b.iface("std::ops").item(iface_fn.index).unwrap().name),
        "compare"
    );
    assert_eq!(
        *dict,
        Dict::Param {
            param: ParamIdx::from_u32(0),
            bound: 0
        }
    );
    let main = b.thir_fn("app", "main").unwrap().body.as_ref().unwrap();
    let dicts: Vec<&Dict> = main
        .exprs
        .iter()
        .filter_map(|e| match &e.kind {
            crate::thir::ExprKind::Call {
                callee: Callee::Fn { dicts, .. },
                ..
            } => dicts.first(),
            _ => None,
        })
        .collect();
    let compare_of = |path: &str, ty: Ty| Dict::Impl {
        iface: compare,
        ty,
        fns: Box::new([impl_fn(b.def(path, "compare"), &[], vec![])]),
    };
    assert_eq!(
        dicts,
        [
            &compare_of("std::int", Ty::INT),
            &compare_of("std::string", Ty::STRING)
        ]
    );
}
