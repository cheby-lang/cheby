//! Tests of the fact log: recording changes nothing, and each table holds
//! what the language server reads from it (D-1380).
//!
//! [`crate::test_support`] rejects a source with a syntax error, and a
//! recording run is for such sources too, so these tests run one module
//! through the front end themselves. Modules here import nothing.

use std::sync::Arc;

use cheby_diag::{Diagnostic, SourceMap, TextRange};
use cheby_iface::{
    DefIndex, DepView, Globals, Ice, InterfaceSet, ModuleId, ModuleInfo, ModuleInterface,
    ModuleTable, PackageId, Ty, builtin, display,
};
use cheby_package::ModulePath;
use cheby_resolve::{
    Lookup, ModuleLookup, ResolveInput, StdCatalog, collect_imports, resolve_import_targets,
    resolve_module,
};
use cheby_syntax::{NodeId, SyntaxKind};

use crate::facts::{BodyFacts, FieldSite, ModuleFacts};
use crate::thir::{self, Callee};
use crate::{Tests, TypesInput, build_interface, check_bodies_recording, check_bodies_with};

/// No module exists: the sources here import nothing.
struct NoModules;

impl ModuleLookup for NoModules {
    fn lookup(&mut self, _from: PackageId, _path: &[&str]) -> Result<Lookup, Ice> {
        Ok(Lookup::NoPackage)
    }
}

/// Runs `check` on a one-module package, after resolution and the
/// interface. Syntax errors are kept, as in a tolerant run.
fn with_module<R>(text: &str, check: impl FnOnce(&TypesInput<'_>, &ModuleInterface) -> R) -> R {
    let globals = Globals::new();
    let mut modules = ModuleTable::new();
    let mut sources = SourceMap::new();
    let file = sources.add_file("src/main.cheby".to_owned(), text).unwrap();
    let module = modules
        .add(ModuleInfo {
            path: ModulePath::root("app"),
            package: PackageId::LOCAL,
            file: Some(file),
        })
        .unwrap();
    let (tree, _) = cheby_syntax::parse(file, Arc::from(text)).into_parts();
    let imports = collect_imports(&tree, &globals.names);
    let (targets, _) =
        resolve_import_targets(&imports, PackageId::LOCAL, &mut NoModules, &globals.names).unwrap();
    let set = InterfaceSet::new(builtin::interface(&globals));
    let deps: [ModuleId; 0] = [];
    let (resolved, _) = resolve_module(&ResolveInput {
        module,
        package: PackageId::LOCAL,
        tree: &tree,
        imports: &imports,
        targets: &targets,
        deps: DepView::new(&set, &deps),
        modules: &modules,
        globals: &globals,
        std: StdCatalog { known: &[] },
        package_targets: cheby_package::Targets::ALL,
    })
    .unwrap();
    let input = TypesInput {
        module,
        package: PackageId::LOCAL,
        tree: &tree,
        resolved: &resolved,
        deps: DepView::new(&set, &deps),
        modules: &modules,
        globals: &globals,
    };
    let (own, _) = build_interface(&input).unwrap();
    check(&input, &own)
}

/// What the tests read of a recording run.
struct Recorded<'a> {
    input: &'a TypesInput<'a>,
    own: &'a ModuleInterface,
    module: thir::Module,
    diags: Vec<Diagnostic>,
    facts: ModuleFacts,
}

impl Recorded<'_> {
    fn index(&self, name: &str) -> DefIndex {
        let items = self.input.resolved.items.iter_enumerated();
        items
            .filter(|(_, item)| self.input.globals.names.get(item.name) == name)
            .map(|(index, _)| index)
            .next()
            .unwrap_or_else(|| panic!("no item `{name}`"))
    }

    /// The facts of the body of `name`.
    fn body(&self, name: &str) -> &BodyFacts {
        self.facts
            .body(self.index(name))
            .unwrap_or_else(|| panic!("no facts for `{name}`"))
    }

    fn has_fn(&self, name: &str) -> bool {
        let index = self.index(name);
        self.module.fns.iter().any(|f| f.def.index == index)
    }

    /// Whether a diagnostic has the code `code`.
    fn has_code(&self, code: &str) -> bool {
        self.diags.iter().any(|d| d.code.to_string() == code)
    }

    /// The range of the first `needle` in the source.
    fn range(&self, needle: &str) -> TextRange {
        let start = self.input.tree.text().find(needle).unwrap();
        let start = u32::try_from(start).unwrap();
        TextRange::at(start, u32::try_from(needle.len()).unwrap())
    }

    /// The node of kind `kind` that starts at the first `needle`, the
    /// outermost one if there are several.
    fn node(&self, kind: SyntaxKind, needle: &str) -> NodeId {
        let start = self.range(needle).start();
        self.input
            .tree
            .root()
            .descendants()
            .find(|n| n.kind() == kind && n.text_range().start() == start)
            .unwrap_or_else(|| panic!("no {kind:?} at `{needle}`"))
            .id()
    }

    fn show(&self, ty: Ty) -> String {
        display(ty, &self.input.display_cx(self.own, None))
    }

    fn show_all(&self, tys: &[Ty]) -> Vec<String> {
        tys.iter().map(|&ty| self.show(ty)).collect()
    }
}

/// Runs `check` on the recording run of `items`, which follow a module doc.
fn recorded<R>(items: &str, check: impl FnOnce(&Recorded<'_>) -> R) -> R {
    with_module(&format!("//! Test.\n\n{items}"), |input, own| {
        let (module, diags, facts) = check_bodies_recording(input, own, Tests::All).unwrap();
        check(&Recorded {
            input,
            own,
            module,
            diags,
            facts,
        })
    })
}

const POINT: &str = "type Point {
  Point { x: Int, y: Int },
}

";

/// One module with every construct that has a hook, and no error.
const CLEAN: &str = r#"//! Test.

type Point {
  Point { x: Int, y: Int },
}

type Shape {
  Circle { radius: Int },
  Dot,
}

interface Area {
  fn area(Self) -> Int
}

fn area(p: Point) -> Int {
  p.x * p.y
}

fn show(p: Point) -> String {
  "point {p.x:?}"
}

fn add(a: Point, b: Point) -> Point {
  Point { x: a.x + b.x, y: a.y + b.y }
}

fn twice<T>(x: T, f: fn(T) -> T) -> T {
  f(f(x))
}

fn bump(n: Int) -> Int {
  n + 1
}

fn try_it(n: Int, f: fn(Int) -> Int) -> Int {
  f(n)
}

fn size(s: Shape) -> Int {
  case s {
    Circle { radius } => radius
    Dot => 0
  }
}

fn all(p: Point) -> String {
  let q = Point { ..p, x: 3 }
  let Point { x, y: z } = q
  let r = p + q
  let d: dyn Area = r
  let n = x |> bump |> twice(_, bump)
  let m = (z + n)
  let g = twice(_, bump)
  let a = Area::area(d)
  let h = g(a)
  "{r} {h:?} {m:?}"
}

fn chain(n: Int) -> Int {
  use k <- try_it(n)
  k + 1
}

const start: Int = 40 + 2

test "bump" {
  assert bump(1) == 2
}

fn main() {
  let s = all(Point { x: 1, y: start })
  let c = size(Dot)
  let t = "{s:?}{c:?}"
  panic t
}
"#;

/// Sources with an error, and the codes they must give.
const ERRORED: &[(&str, &str)] = &[
    ("E0047", "fn f() -> Int {\n  \"s\"\n}\n"),
    (
        "E0048",
        "fn g(a: Int, b: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(1)\n}\n",
    ),
    (
        "E0048",
        "fn g(a: Int, b: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  1 |> g(_, 2, 3)\n}\n",
    ),
    ("E0057", "fn f() -> Int {\n  let xs = []\n  1\n}\n"),
    (
        "E0058",
        "type P {\n  P { x: Int },\n}\n\nfn f(p: P) -> Int {\n  p.y\n}\n",
    ),
    (
        "E0061",
        "fn f(o: Option<Int>) -> Int {\n  case o {\n    Some(x) => x\n  }\n}\n",
    ),
    (
        "E0063",
        "type P {\n  P { x: Int },\n}\n\nfn f(p: P) -> String {\n  \"{p}\"\n}\n",
    ),
];

/// Sources that do not parse.
const BROKEN: &[&str] = &[
    "fn f(xs: Int) -> Int {\n  let y = xs |> \n  y\n}\n",
    "fn f(xs: Int) -> Int {\n  xs.\n}\n",
    "fn f(xs: Int) -> Int {\n  let z = \n  let y = xs\n  y\n}\n",
    "fn f(xs: Int) -> Int {\n  case xs {\n  let y = xs\n  y\n}\n",
    "fn g(a: Int, b: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(1, \n}\n",
    "fn f() -> Int {\n  use <- \n  1 + \n}\n",
];

/// The output of a run with recording off and on must be the same.
fn assert_same_output(text: &str) {
    with_module(text, |input, own| {
        let (off, off_diags) = check_bodies_with(input, own, Tests::All).unwrap();
        let (on, on_diags, facts) = check_bodies_recording(input, own, Tests::All).unwrap();
        assert_eq!(on_diags, off_diags, "diagnostics of {text}");
        assert_eq!(format!("{on:?}"), format!("{off:?}"), "THIR of {text}");
        assert_eq!(facts.lost.len(), 0, "{text}");
        assert_eq!(facts.lost_passes.len(), 0, "{text}");
    });
}

#[test]
fn recording_changes_nothing_on_clean_code() {
    assert_same_output(CLEAN);
    with_module(CLEAN, |input, own| {
        let (module, diags, facts) = check_bodies_recording(input, own, Tests::All).unwrap();
        let errors: Vec<_> = diags.iter().filter(|d| d.is_error()).collect();
        assert!(errors.is_empty(), "{errors:?}");
        // Every body gave facts, and every body stayed in THIR.
        let kept = module.fns.len() + module.consts.len() + module.tests.len();
        assert_eq!(facts.bodies.len(), kept);
        assert!(facts.bodies.iter().all(|b| !b.errored));
    });
}

#[test]
fn recording_changes_nothing_on_errors() {
    for (code, items) in ERRORED {
        let text = format!("//! Test.\n\n{items}");
        assert_same_output(&text);
        recorded(items, |r| {
            assert!(r.has_code(code), "{items}: {:?}", r.diags);
        });
    }
}

#[test]
fn recording_changes_nothing_on_broken_trees() {
    for items in BROKEN {
        assert_same_output(&format!("//! Test.\n\n{items}"));
    }
}

#[test]
fn facts_are_the_same_on_every_run() {
    let run = |r: &Recorded<'_>| format!("{:?}", r.facts);
    assert_eq!(recorded(CLEAN, run), recorded(CLEAN, run));
}

#[test]
fn an_ice_fails_a_run_without_recording() {
    with_module(
        "//! Test.\n\nfn ice_by_test() -> Int {\n  1\n}\n",
        |input, own| {
            assert!(check_bodies_with(input, own, Tests::All).is_err());
        },
    );
}

#[test]
fn an_ice_loses_one_body() {
    let items = "fn ice_by_test() -> Int {\n  1\n}\n\nfn f() -> Int {\n  \"s\"\n}\n\n\
                 fn g() -> Int {\n  2\n}\n";
    recorded(items, |r| {
        let lost: Vec<DefIndex> = r.facts.lost.iter().map(|(index, _)| *index).collect();
        assert_eq!(lost, [r.index("ice_by_test")]);
        // The bodies around it keep their facts and their diagnostics.
        assert!(r.body("f").errored);
        assert!(!r.body("g").errored);
        assert!(r.has_fn("g"));
        assert!(r.has_code("E0047"));
    });
}

#[test]
fn an_ice_loses_one_pass() {
    let items = "fn pass_ice_by_test() -> Int {\n  1\n}\n\n\
                 fn unused() -> Int {\n  let a = 1\n  2\n}\n";
    with_module(&format!("//! Test.\n\n{items}"), |input, own| {
        assert!(check_bodies_with(input, own, Tests::All).is_err());
    });
    recorded(items, |r| {
        assert_eq!(r.facts.lost_passes.len(), 1);
        assert_eq!(r.facts.lost.len(), 0);
        assert!(!r.body("unused").errored);
        assert!(r.has_fn("unused"));
        // The passes after it still ran: the warning pass is the last one.
        assert!(r.has_code("W0002"), "{:?}", r.diags);
    });
}

#[test]
fn expr_keeps_its_own_type_and_the_expected_one() {
    recorded("fn f() -> Int {\n  \"s\"\n}\n", |r| {
        let facts = r.body("f");
        let lit = r.node(SyntaxKind::STRING_LIT, "\"s\"");
        let fact = facts.expr(lit).unwrap();
        assert_eq!(r.show(fact.ty), "String");
        assert_eq!(fact.expected.map(|t| r.show(t)).as_deref(), Some("Int"));
        let mismatch = facts.mismatch_at(r.range("\"s\"")).unwrap();
        assert_eq!(r.show(mismatch.expected), "Int");
        assert_eq!(r.show(mismatch.found), "String");
        assert!(facts.mismatch_at(r.range("Int")).is_none());
        assert!(facts.errored);
    });
}

#[test]
fn a_pattern_of_another_type_is_a_mismatch() {
    let items = "fn f(n: Int) -> Int {\n  case n {\n    \"a\" => 1\n    _ => 2\n  }\n}\n";
    recorded(items, |r| {
        let mismatch = r.body("f").mismatch_at(r.range("\"a\"")).unwrap();
        assert_eq!(r.show(mismatch.expected), "Int");
        assert_eq!(r.show(mismatch.found), "String");
    });
}

#[test]
fn alternatives_of_two_types_are_a_mismatch() {
    let items = "type Two {\n  A(Int),\n  B(String),\n}\n\n\
                 fn f(t: Two) -> Int {\n  case t {\n    A(v) | B(v) => 1\n  }\n}\n";
    recorded(items, |r| {
        let name = TextRange::at(r.range("B(v)").start() + 2, 1);
        let mismatch = r.body("f").mismatch_at(name).unwrap();
        assert_eq!(r.show(mismatch.expected), "Int");
        assert_eq!(r.show(mismatch.found), "String");
    });
}

#[test]
fn an_update_of_another_type_is_a_mismatch() {
    let items = format!(
        "{POINT}type Other {{\n  Other {{ a: Int }},\n}}\n\n\
         fn f(o: Other) -> Point {{\n  Point {{ ..o, x: 1 }}\n}}\n"
    );
    recorded(&items, |r| {
        let base = TextRange::at(r.range("..o").start() + 2, 1);
        let mismatch = r.body("f").mismatch_at(base).unwrap();
        assert_eq!(r.show(mismatch.expected), "Point");
        assert_eq!(r.show(mismatch.found), "Other");
    });
}

#[test]
fn a_use_with_the_wrong_binders_is_a_mismatch() {
    let items = "fn run(n: Int, f: fn(Int) -> Int) -> Int {\n  f(n)\n}\n\n\
                 fn f() -> Int {\n  use a, b <- run(1)\n  1\n}\n";
    recorded(items, |r| {
        assert!(r.has_code("E0047"));
        let mismatch = r
            .body("f")
            .mismatch_at(r.range("use a, b <- run(1)"))
            .unwrap();
        assert_eq!(r.show(mismatch.expected), "fn(Int) -> Int");
        assert_eq!(r.show(mismatch.found), "fn(_, _) -> _");
    });
}

#[test]
fn todo_answers_with_the_expected_type() {
    recorded("fn f() -> Int {\n  todo\n}\n", |r| {
        let fact = r.body("f").expr(r.node(SyntaxKind::TODO_EXPR, "todo"));
        let fact = fact.unwrap();
        assert_eq!(fact.ty, Ty::NEVER);
        assert_eq!(fact.expected, Some(Ty::INT));
    });
}

#[test]
fn a_body_ended_by_e0061_is_errored() {
    let items = "fn f(o: Option<Int>) -> Int {\n  case o {\n    Some(x) => x\n  }\n}\n";
    recorded(items, |r| {
        assert!(r.has_code("E0061"));
        assert!(!r.has_fn("f"));
        assert!(r.body("f").errored);
    });
}

#[test]
fn a_literal_gets_its_final_type() {
    let items = "fn g(n: U8) -> U8 {\n  n\n}\n\nfn f() -> U8 {\n  let x = 1\n  g(x)\n}\n";
    recorded(items, |r| {
        let pat = r.node(SyntaxKind::IDENT_PAT, "x = 1");
        assert_eq!(r.body("f").pat(pat), Some(Ty::U8));
        let lit = r.node(SyntaxKind::LITERAL, "1\n");
        assert_eq!(r.body("f").expr(lit).unwrap().ty, Ty::U8);
    });
}

#[test]
fn a_parenthesis_has_the_inner_facts() {
    let items = format!(
        "{POINT}fn add(a: Point, b: Point) -> Point {{\n  a\n}}\n\n\
         fn f(p: Point) -> Point {{\n  (p + p)\n}}\n"
    );
    recorded(&items, |r| {
        let facts = r.body("f");
        let paren = r.node(SyntaxKind::PAREN_EXPR, "(p + p)");
        let bin = r.node(SyntaxKind::BIN_EXPR, "p + p");
        assert_eq!(r.show(facts.expr(paren).unwrap().ty), "Point");
        assert_eq!(r.show(facts.expr(bin).unwrap().ty), "Point");
        // The operator is the `BIN_EXPR`'s alone.
        assert!(matches!(
            facts.op(bin).unwrap().callee,
            Some(Callee::Fn { .. })
        ));
        assert!(facts.op(paren).is_none());
    });
}

#[test]
fn a_builtin_operator_has_no_callee() {
    recorded("fn f(a: Int) -> Int {\n  a + 1\n}\n", |r| {
        let bin = r.node(SyntaxKind::BIN_EXPR, "a + 1");
        assert!(r.body("f").op(bin).unwrap().callee.is_none());
    });
}

#[test]
fn a_call_has_its_instantiated_signature() {
    let items = "fn id<T>(x: T) -> T {\n  x\n}\n\nfn f() -> String {\n  id(\"s\")\n}\n";
    recorded(items, |r| {
        let call = r
            .body("f")
            .call(r.node(SyntaxKind::PATH_EXPR, "id("))
            .unwrap();
        assert_eq!(call.def.map(|d| d.index), Some(r.index("id")));
        assert_eq!(r.show_all(&call.ty_args), ["String"]);
        assert_eq!(r.show_all(&call.params), ["String"]);
        assert_eq!(r.show(call.ret), "String");
        assert!(matches!(call.resolved, Some(Callee::Fn { .. })));
    });
}

#[test]
fn a_call_with_too_few_arguments_pairs_what_it_has() {
    let items = "fn pair<A, B>(a: A, b: B) -> A {\n  a\n}\n\nfn f(n: Int) -> Int {\n  pair(n)\n}\n";
    recorded(items, |r| {
        let call = r
            .body("f")
            .call(r.node(SyntaxKind::PATH_EXPR, "pair("))
            .unwrap();
        assert_eq!(r.show_all(&call.params), ["Int", "_"]);
        assert_eq!(r.show(call.ret), "Int");
        assert!(call.resolved.is_none());
    });
}

#[test]
fn a_failed_call_reaches_an_earlier_call_of_the_same_variable() {
    let items = "fn take<T>(f: fn(T) -> T) -> Int {\n  1\n}\n\n\
                 fn f(n: Int) -> Int {\n  let g = fn(a) { a }\n  let b = take(g)\n  \
                 g(n, b)\n}\n";
    recorded(items, |r| {
        assert!(r.has_code("E0048"));
        let call = r
            .body("f")
            .call(r.node(SyntaxKind::PATH_EXPR, "take("))
            .unwrap();
        assert_eq!(r.show_all(&call.ty_args), ["Int"]);
    });
}

#[test]
fn a_capture_with_too_many_arguments_skips_its_hole() {
    let items = "fn pair<A, B>(a: A, b: B) -> A {\n  a\n}\n\n\
                 fn f() -> Int {\n  let g = pair(_, \"s\", 1)\n  2\n}\n";
    recorded(items, |r| {
        assert!(r.has_code("E0048"));
        let call = r
            .body("f")
            .call(r.node(SyntaxKind::PATH_EXPR, "pair(_"))
            .unwrap();
        // `"s"` is written second, so it meets `B`, not `A`.
        assert_eq!(r.show_all(&call.params), ["_", "String"]);
    });
}

#[test]
fn a_pipe_stage_has_its_signature() {
    let items = "fn add(a: Int, b: Int) -> Int {\n  a + b\n}\n\n\
                 fn make() -> fn(Int) -> Int {\n  add(_, 1)\n}\n\n\
                 fn f(x: Int) -> Int {\n  x |> add(_, 1) |> (make())\n}\n";
    recorded(items, |r| {
        let facts = r.body("f");
        let stage = facts
            .call(r.node(SyntaxKind::PATH_EXPR, "add(_, 1) |>"))
            .unwrap();
        assert_eq!(r.show_all(&stage.params), ["Int", "Int"]);
        // A stage that is a value: its callee is the right side itself.
        let value = facts
            .call(r.node(SyntaxKind::PAREN_EXPR, "(make())"))
            .unwrap();
        assert_eq!(value.def, None);
        assert_eq!(r.show_all(&value.params), ["Int"]);
        assert!(matches!(value.resolved, Some(Callee::Value(_))));
        // The input of the first stage.
        let input = r.node(SyntaxKind::PATH_EXPR, "x |>");
        assert_eq!(facts.expr(input).unwrap().ty, Ty::INT);
    });
}

#[test]
fn a_pipe_with_no_right_side_keeps_its_left_side() {
    recorded("fn f(xs: Int) -> Int {\n  let y = xs |> \n  y\n}\n", |r| {
        let left = r.node(SyntaxKind::PATH_EXPR, "xs |>");
        assert_eq!(r.body("f").expr(left).unwrap().ty, Ty::INT);
        assert!(r.body("f").errored);
    });
}

#[test]
fn a_use_callback_is_on_the_statement() {
    let items = "fn run(n: Int, f: fn(Int) -> Int) -> Int {\n  f(n)\n}\n\n\
                 fn f() -> Int {\n  use k <- run(1)\n  k\n}\n";
    recorded(items, |r| {
        let stmt = r.node(SyntaxKind::USE_STMT, "use k");
        assert_eq!(r.show(r.body("f").expr(stmt).unwrap().ty), "fn(Int) -> Int");
    });
}

#[test]
fn a_failed_use_pairs_its_callback_with_the_last_parameter() {
    let items = "fn r2<T>(n: T, f: fn(Int) -> Int) -> T {\n  n\n}\n\n\
                 fn f() -> Int {\n  use a <- r2()\n  a\n}\n\n\
                 fn g(n: Int) -> Int {\n  use a <- r2(n, n)\n  a\n}\n";
    recorded(items, |r| {
        let call = r.body("f").call(r.node(SyntaxKind::PATH_EXPR, "r2()"));
        let call = call.unwrap();
        assert_eq!(r.show_all(&call.ty_args), ["_"]);
        assert_eq!(r.show_all(&call.params), ["_", "fn(Int) -> Int"]);
        assert_eq!(r.show(call.ret), "_");
        // What is written pairs in order, and an extra argument with nothing.
        let call = r.body("g").call(r.node(SyntaxKind::PATH_EXPR, "r2(n"));
        assert_eq!(r.show_all(&call.unwrap().params), ["Int", "fn(Int) -> Int"]);
    });
}

#[test]
fn field_names_have_their_site() {
    let items = format!(
        "{POINT}fn f(p: Point) -> Int {{\n  let q = Point {{ x: 1, y: p.y }}\n  \
         let r = Point {{ ..q, x: 2 }}\n  let Point {{ x: a, y: b }} = r\n  a + b\n}}\n"
    );
    recorded(&items, |r| {
        let facts = r.body("f");
        let at = |needle: &str| facts.field_at(r.range(needle).start()).unwrap();
        let read = facts.field_at(r.range("p.y").start() + 2).unwrap();
        assert_eq!(read.site, FieldSite::Read);
        assert_eq!(read.base.map(|t| r.show(t)).as_deref(), Some("Point"));
        assert_eq!(read.ty, Some(Ty::INT));
        let init = at("x: 1");
        assert_eq!(init.site, FieldSite::Init);
        assert!(init.ctor.is_some());
        let update = at("x: 2");
        assert_eq!(update.site, FieldSite::Update);
        assert_eq!(update.ctor, None);
        let pattern = at("x: a");
        assert_eq!(pattern.site, FieldSite::Pattern);
        assert_eq!(pattern.ty, Some(Ty::INT));
        for fact in [read, init, update, pattern] {
            assert_eq!(r.input.globals.names.get(fact.name).len(), 1);
            assert!(fact.owner.is_some());
            let owner = r.input.resolved.item(fact.owner.unwrap().index).unwrap();
            assert_eq!(r.input.globals.names.get(owner.name), "Point");
        }
    });
}

#[test]
fn an_unknown_field_has_no_type() {
    let items = format!(
        "{POINT}fn f(p: Point) -> Point {{\n  let n = p.z\n  let q = Point {{ ..p, v: 4 }}\n  \
         let Point {{ u: a, .. }} = p\n  Point {{ x: 1, y: 2, w: 3 }}\n}}\n"
    );
    recorded(&items, |r| {
        let facts = r.body("f");
        let read = facts.field_at(r.range("p.z").start() + 2).unwrap();
        assert!(read.owner.is_some());
        assert_eq!(read.ty, None);
        let init = facts.field_at(r.range("w: 3").start()).unwrap();
        assert!(init.owner.is_some());
        assert_eq!(init.ty, None);
        let update = facts.field_at(r.range("v: 4").start()).unwrap();
        assert_eq!(update.site, FieldSite::Update);
        assert!(update.owner.is_some());
        assert_eq!(update.ty, None);
        let pattern = facts.field_at(r.range("u: a").start()).unwrap();
        assert_eq!(pattern.site, FieldSite::Pattern);
        assert!(pattern.owner.is_some());
        assert_eq!(pattern.ty, None);
    });
}

#[test]
fn a_punned_field_has_its_own_type_and_the_field_type() {
    let items = format!("{POINT}fn f(x: String) -> Point {{\n  Point {{ x, y: 2 }}\n}}\n");
    recorded(&items, |r| {
        let init = r.node(SyntaxKind::FIELD_INIT, "x, y");
        let fact = r.body("f").expr(init).unwrap();
        assert_eq!(r.show(fact.ty), "String");
        assert_eq!(fact.expected.map(|t| r.show(t)).as_deref(), Some("Int"));
    });
}

#[test]
fn a_read_nothing_can_place_has_no_owner() {
    let items = "fn f() -> Int {\n  let g = fn(v) { v.size }\n  1\n}\n";
    recorded(items, |r| {
        let read = r.body("f").field_at(r.range("v.size").start() + 2).unwrap();
        assert_eq!(read.owner, None);
    });
}

#[test]
fn interpolation_has_values_and_shows() {
    let items = format!(
        "{POINT}fn show(p: Point) -> String {{\n  \"p\"\n}}\n\n\
         fn f(p: Point) -> String {{\n  \"a {{p}} b {{p.x:?}} c {{p}}\"\n}}\n"
    );
    recorded(&items, |r| {
        let facts = r.body("f");
        let string = r.node(SyntaxKind::STRING_LIT, "\"a {p}");
        let parts: Vec<u32> = facts
            .shows
            .iter()
            .filter(|s| s.node == string)
            .map(|s| s.part)
            .collect();
        // `{p:?}`-style parts have no `show`.
        assert_eq!(parts, [1, 5]);
        let value = r.node(SyntaxKind::INTERPOLATION, "{p.x:?}");
        assert_eq!(facts.expr(value).unwrap().ty, Ty::INT);
        let field = facts.field_at(r.range("{p.x").start() + 3).unwrap();
        assert_eq!(field.site, FieldSite::Read);
    });
}

#[test]
fn a_dyn_conversion_keeps_the_value_type() {
    let items = format!(
        "{POINT}interface Area {{\n  fn area(Self) -> Int\n}}\n\n\
         fn area(p: Point) -> Int {{\n  p.x\n}}\n\n\
         fn f(p: Point) -> dyn Area {{\n  (p)\n}}\n"
    );
    recorded(&items, |r| {
        let facts = r.body("f");
        let inner = r.node(SyntaxKind::PATH_EXPR, "p)");
        let fact = facts.expr(inner).unwrap();
        assert_eq!(r.show(fact.ty), "Point");
        assert_eq!(
            fact.expected.map(|t| r.show(t)).as_deref(),
            Some("dyn Area")
        );
        let converted = facts.dyn_at(inner).unwrap();
        assert_eq!(converted.iface.index, r.index("Area"));
        // A parenthesis around it is the same conversion, not a second.
        assert_eq!(facts.dyns.len(), 1);
        let paren = r.node(SyntaxKind::PAREN_EXPR, "(p)");
        assert_eq!(r.show(facts.expr(paren).unwrap().ty), "Point");
    });
}

#[test]
fn an_unsatisfied_bound_leaves_no_callee() {
    let items = format!(
        "{POINT}interface Area {{\n  fn area(Self) -> Int\n}}\n\n\
         fn g<T: Area>(x: T) -> T {{\n  x\n}}\n\n\
         fn f(p: Point) -> Point {{\n  g(p)\n}}\n"
    );
    recorded(&items, |r| {
        assert!(r.has_code("E0063"));
        let facts = r.body("f");
        let call = facts.call(r.node(SyntaxKind::PATH_EXPR, "g(p)")).unwrap();
        assert!(call.resolved.is_none());
        let [fact] = &facts.unsatisfied[..] else {
            panic!("{:?}", facts.unsatisfied);
        };
        assert_eq!(r.show(fact.err.ty), "Point");
        assert!(facts.unsatisfied_at(fact.span.range).is_some());
        assert!(facts.unsatisfied_at(r.range("Point")).is_none());
    });
}

#[test]
fn an_unsatisfied_interface_call_leaves_no_callee() {
    let items = format!(
        "{POINT}interface Area {{\n  fn area(Self) -> Int\n}}\n\n\
         fn f(p: Point) -> Int {{\n  Area::area(p)\n}}\n"
    );
    recorded(&items, |r| {
        assert!(r.has_code("E0063"));
        let path = r.node(SyntaxKind::PATH_EXPR, "Area::area(p)");
        assert!(r.body("f").call(path).unwrap().resolved.is_none());
    });
}

#[test]
fn an_unsatisfied_call_on_dyn_leaves_no_callee() {
    let items = "interface Area {\n  fn area(Self) -> Int\n}\n\n\
                 interface B {\n  fn b(Self) -> Int\n}\n\n\
                 fn f(d: dyn Area) -> Int {\n  B::b(d)\n}\n";
    recorded(items, |r| {
        assert!(r.has_code("E0063"));
        let path = r.node(SyntaxKind::PATH_EXPR, "B::b(d)");
        assert_eq!(r.body("f").call(path).unwrap().resolved, None);
    });
}

#[test]
fn an_interface_call_on_dyn_keeps_its_callee() {
    let items = "interface Area {\n  fn area(Self) -> Int\n}\n\n\
                 fn f(d: dyn Area) -> Int {\n  Area::area(d)\n}\n";
    recorded(items, |r| {
        let path = r.node(SyntaxKind::PATH_EXPR, "Area::area(d)");
        let call = r.body("f").call(path).unwrap();
        assert!(matches!(call.resolved, Some(Callee::Dyn { .. })));
    });
}

#[test]
fn a_case_has_its_fact_beside_another_error() {
    let items = "fn f(o: Option<Int>) -> Int {\n  let s: Int = \"s\"\n  case o {\n    Some(x) => x\n  }\n}\n";
    recorded(items, |r| {
        let node = r.node(SyntaxKind::CASE_EXPR, "case o");
        assert_eq!(r.body("f").case(node).unwrap().missing.len(), 1);
    });
}

#[test]
fn constants_and_tests_have_facts() {
    let items = "const size: Int = 40 + 2\n\ntest \"t\" {\n  let n = size\n  assert n == 42\n}\n";
    recorded(items, |r| {
        let bin = r.node(SyntaxKind::BIN_EXPR, "40 + 2");
        assert_eq!(r.body("size").expr(bin).unwrap().ty, Ty::INT);
        let pat = r.node(SyntaxKind::IDENT_PAT, "n = size");
        assert_eq!(r.body("t").pat(pat), Some(Ty::INT));
    });
}

#[test]
fn facts_are_sorted_for_their_lookups() {
    recorded(CLEAN, |r| {
        for body in &r.facts.bodies {
            assert!(body.exprs.is_sorted_by_key(|e| e.node));
            assert!(body.pats.is_sorted_by_key(|p| p.0));
            assert!(body.fields.is_sorted_by_key(|f| f.name_span.range.start()));
            assert!(body.calls.is_sorted_by_key(|c| c.callee));
            assert!(body.ops.is_sorted_by_key(|o| o.node));
            assert!(body.shows.is_sorted_by_key(|s| (s.node, s.part)));
            assert!(body.dyns.is_sorted_by_key(|d| d.node));
        }
        let items: Vec<DefIndex> = r.facts.bodies.iter().map(|b| b.item).collect();
        assert!(items.is_sorted());
    });
}
