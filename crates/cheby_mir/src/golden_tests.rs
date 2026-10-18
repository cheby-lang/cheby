//! MIR goldens: whole programs through the front end, lowering, reference
//! counting and validation, compared with `crates/cheby_mir/tests/golden/<name>.mir`.
//! `CHEBY_BLESS=1` rewrites the goldens.
//!
//! The harness mirrors the driver's flow (G1) with an in-test
//! `std/io.cheby`, like `cheby_types`' tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::borrow::{BuildFacts, IfaceEligibility, infer, infer_build};
use crate::{
    Defs, EntryKind, EntryRequest, LowerCx, MirModule, MirOptions, check_lowered,
    count_lowered_with, dump, lower_entries, lower_module,
};
use cheby_diag::{Diagnostic, FileId, SourceMap};
use cheby_iface::{
    DepView, Globals, Ice, InterfaceSet, ModuleId, ModuleInfo, ModuleTable, PackageId, builtin,
};
use cheby_package::ModulePath;
use cheby_package::graph::{Graph, reachable, topological_order};
use cheby_resolve::{
    ImportRequest, ImportTarget, Lookup, ModuleLookup, ResolveInput, StdCatalog, collect_imports,
    resolve_import_targets, resolve_module,
};
use cheby_syntax::SyntaxTree;
use cheby_types::thir;
use cheby_types::{TypesInput, build_interface, check_bodies};

/// The slice's `std/io.cheby` (D-450).
const STD_IO: &str = r#"//! Standard input, standard output, standard error, and reading and
//! writing whole files.

/// Writes `text` to standard output, with no line ending.
@external(native, "cheby_io_print")
@external(js, "./io.mjs", "print")
pub fn print(text: String)

/// Writes `text` and a line ending to standard output.
@external(native, "cheby_io_println")
@external(js, "./io.mjs", "println")
pub fn println(text: String)

/// Writes `text` to standard error, with no line ending.
@external(native, "cheby_io_eprint")
@external(js, "./io.mjs", "eprint")
pub fn eprint(text: String)

/// Writes `text` and a line ending to standard error.
@external(native, "cheby_io_eprintln")
@external(js, "./io.mjs", "eprintln")
pub fn eprintln(text: String)
"#;

struct Modules<'a> {
    package: &'a str,
    table: ModuleTable,
    /// Per module id: display path and text.
    files: Vec<(String, String)>,
}

impl ModuleLookup for Modules<'_> {
    fn lookup(&mut self, _from: PackageId, path: &[&str]) -> Result<Lookup, Ice> {
        let Some((&first, rest)) = path.split_first() else {
            return Err(Ice::new("empty path"));
        };
        let module_path = rest.iter().fold(ModulePath::root(first), |p, s| p.child(s));
        if let Some(id) = self.table.find(&module_path) {
            return Ok(Lookup::Found(id));
        }
        if first == "std" && rest == ["io"] {
            let id = self.table.add(ModuleInfo {
                path: module_path,
                package: PackageId::STD,
                file: None,
            })?;
            self.files
                .push(("std/io.cheby".to_owned(), STD_IO.to_owned()));
            return Ok(Lookup::Found(id));
        }
        if first == self.package {
            return Ok(Lookup::NoModule {
                package: PackageId::LOCAL,
                closest: None,
            });
        }
        Ok(Lookup::NoPackage)
    }
}

fn parse(id: ModuleId, text: &str) -> SyntaxTree {
    let (tree, mut errors) =
        cheby_syntax::parse(FileId::new(id.as_u32()), Arc::from(text)).into_parts();
    if errors.is_empty() {
        errors.extend(cheby_syntax::validate(&tree));
    }
    assert!(errors.is_empty(), "syntax errors: {errors:?}");
    tree
}

/// Builds a one-module package and returns the dump of every unit: module
/// units in build order, then the entry unit for the root's `main`.
#[expect(
    clippy::too_many_lines,
    reason = "the whole front end of a build, in order"
)]
fn build(package: &str, main: &str) -> String {
    let globals = Globals::new();
    let mut modules = Modules {
        package,
        table: ModuleTable::new(),
        files: vec![("<builtin>".to_owned(), String::new())],
    };
    let root = modules
        .table
        .add(ModuleInfo {
            path: ModulePath::root(package),
            package: PackageId::LOCAL,
            file: None,
        })
        .unwrap();
    modules
        .files
        .push(("src/main.cheby".to_owned(), main.to_owned()));
    let mut trees: Vec<Option<SyntaxTree>> = vec![None, Some(parse(root, main))];
    let mut requests: Vec<Vec<ImportRequest>> = vec![Vec::new(), Vec::new()];
    let mut targets: Vec<Vec<ImportTarget>> = vec![Vec::new(), Vec::new()];
    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut next = 1;
    while next < modules.table.len() {
        let id = ModuleId::from_u32(u32::try_from(next).unwrap());
        let info = modules.table.get(id).unwrap().clone();
        let reqs = collect_imports(trees[next].as_ref().unwrap(), &globals.names);
        let (t, d) =
            resolve_import_targets(&reqs, info.package, &mut modules, &globals.names).unwrap();
        diags.extend(d);
        requests[next] = reqs;
        targets[next] = t;
        while trees.len() < modules.table.len() {
            let new = ModuleId::from_u32(u32::try_from(trees.len()).unwrap());
            trees.push(Some(parse(new, &modules.files[trees.len()].1)));
            requests.push(Vec::new());
            targets.push(Vec::new());
        }
        next = next.checked_add(1).unwrap();
    }
    let mut graph = Graph::new(modules.table.len());
    for (id, ts) in targets.iter().enumerate() {
        for t in ts {
            if let Some(to) = t.module() {
                graph.add_edge(u32::try_from(id).unwrap(), to.as_u32());
            }
        }
    }
    let order = topological_order(&graph).unwrap();
    let mut sources = SourceMap::new();
    for (path, text) in &modules.files {
        sources.add_file(path.clone(), text.clone()).unwrap();
    }
    let mut set = InterfaceSet::new(builtin::interface(&globals));
    let mut units: Vec<MirModule> = Vec::new();
    // Each module's THIR and dependencies, for the borrowing inference.
    let mut checked: BTreeMap<ModuleId, (thir::Module, Vec<ModuleId>)> = BTreeMap::new();
    let mut main_entry = None;
    for node in order {
        let id = ModuleId::from_u32(node);
        if id == ModuleId::BUILTIN {
            continue;
        }
        let n = usize::try_from(node).unwrap();
        let succ = graph.successors(node).to_vec();
        let mut deps: Vec<ModuleId> = reachable(&graph, &succ)
            .into_iter()
            .map(ModuleId::from_u32)
            .collect();
        deps.sort();
        let info = modules.table.get(id).unwrap().clone();
        let tree = trees[n].as_ref().unwrap();
        let input = ResolveInput {
            module: id,
            package: info.package,
            tree,
            imports: &requests[n],
            targets: &targets[n],
            deps: DepView::new(&set, &deps),
            modules: &modules.table,
            globals: &globals,
            std: StdCatalog { known: &["io"] },
            package_targets: cheby_package::Targets::ALL,
        };
        let (resolved, d) = resolve_module(&input).unwrap();
        diags.extend(d);
        let types = TypesInput {
            module: id,
            package: info.package,
            tree,
            resolved: &resolved,
            deps: DepView::new(&set, &deps),
            modules: &modules.table,
            globals: &globals,
        };
        let (iface, d) = build_interface(&types).unwrap();
        diags.extend(d);
        let (thir, d) = check_bodies(&types, &iface).unwrap();
        diags.extend(d);
        // Warnings such as an unused `let` are fine: MIR sees no errors.
        assert!(
            !diags.iter().any(cheby_diag::Diagnostic::is_error),
            "diagnostics: {diags:#?}"
        );
        let view = DepView::new(&set, &deps);
        let cx = LowerCx {
            defs: Defs {
                own: Some(&iface),
                deps: &view,
                modules: &modules.table,
                globals: &globals,
            },
            sources: &sources,
        };
        let unit = lower_module(&thir, &cx).unwrap();
        check_lowered(&unit).unwrap();
        units.push(unit);
        if id == root {
            main_entry = thir.fns.iter().find(|f| f.entry.is_some()).cloned();
        }
        set.insert(Arc::new(iface)).unwrap();
        checked.insert(id, (thir, deps));
    }
    let main = main_entry.expect("the root module has a `main`");
    let all: Vec<ModuleId> = (1..modules.table.len())
        .map(|n| ModuleId::from_u32(u32::try_from(n).unwrap()))
        .collect();
    let view = DepView::new(&set, &all);
    let cx = LowerCx {
        defs: Defs {
            own: None,
            deps: &view,
            modules: &modules.table,
            globals: &globals,
        },
        sources: &sources,
    };
    let entries = lower_entries(
        &[EntryRequest {
            kind: EntryKind::Main(main.def),
            info: main.entry.as_ref(),
        }],
        &cx,
    )
    .unwrap();
    check_lowered(&entries).unwrap();
    units.push(entries);
    // As a build does it (D-779): the masks inferred over every unit and
    // written into each, then counting.
    let per_unit = |unit: &MirModule, facts: &BuildFacts| {
        let crate::UnitKind::Code(id) = unit.unit else {
            return Ok(BTreeMap::new());
        };
        let (thir, dep_ids) = &checked[&id];
        let view = DepView::new(&set, dep_ids);
        let readable = Defs {
            own: set.get(id),
            deps: &view,
            modules: &modules.table,
            globals: &globals,
        };
        Ok::<_, Ice>(infer(unit, facts, &IfaceEligibility::new(readable, thir)))
    };
    infer_build(&mut units, &per_unit).unwrap();
    for unit in &mut units {
        count_lowered_with(unit, &MirOptions::default()).unwrap();
    }
    units
        .iter()
        .map(|u| dump(u).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Compares `actual` with `tests/golden/<name>.mir`; `name` may hold a
/// subdirectory, such as `pat/or_patterns`. `CHEBY_BLESS=1` rewrites it.
pub(crate) fn check_golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.mir"));
    if std::env::var_os("CHEBY_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (bless with CHEBY_BLESS=1)", path.display()));
    assert_eq!(
        actual,
        expected,
        "MIR of {name} differs from {}",
        path.display()
    );
}

#[test]
fn hello_world_example() {
    let src = std::fs::read_to_string(repo().join("docs/examples/001_hello_world/src/main.cheby"))
        .unwrap();
    let mir = build("hello_world", &src);
    check_golden("001_hello_world", &mir);
    // Determinism (D-264): a second build gives the same text.
    assert_eq!(build("hello_world", &src), mir);
}

#[test]
fn hello_world_ui_test() {
    let src =
        std::fs::read_to_string(repo().join("tests/ui/01-overview/hello_world.cheby")).unwrap();
    check_golden("ui_hello_world", &build("hello_world", &src));
}

/// `let`, a value used twice (one dup), an unused definition (a drop),
/// discarded call results, and tail calls within the module and across.
#[test]
fn lets_dups_and_tail_calls() {
    let src = r#"//! Prints a greeting twice.

import std::io

fn main() {
  let greeting = "hi"
  io::println(greeting)
  shout(greeting)
}

fn shout(text: String) -> Nil {
  let _ = io::print(text)
  let unused = "never printed"
  let again = text
  io::println("!")
  io::eprintln(again)
}
"#;
    check_golden("lets_dups_and_tail_calls", &build("app", src));
}

/// Constructor layouts (value fields first, then raw ones), a user unit
/// constructor as a static object, and boxing at a generic boundary
/// (D-454, D6).
#[test]
fn layouts_static_constructors_and_boxing() {
    let src = r#"//! Shapes.

import std::io

type Shape {
  Dot,
  Circle(Float),
  Rect { w: Int, h: Int, label: String },
}

fn id<T>(x: T) -> T {
  x
}

fn dot() -> Shape {
  Dot
}

fn same(flag: Bool) -> Bool {
  id(flag)
}

fn main() {
  let _ = dot()
  let _ = same(True)
  io::println("shapes")
}
"#;
    check_golden("layouts_static_constructors_and_boxing", &build("app", src));
}

/// The `.cheby` files of the example package in `dir` as `(module path
/// under the package, text)`, and the package name from its manifest.
pub(crate) fn example_sources(dir: &Path) -> (String, Vec<(String, String)>) {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            let module = match (prefix, stem.as_str()) {
                ("", "main") => String::new(),
                ("", _) => stem.clone(),
                _ => format!("{prefix}::{stem}"),
            };
            if path.is_dir() {
                walk(&path, &module, out);
            } else if path.extension().is_some_and(|e| e == "cheby") {
                out.push((module, std::fs::read_to_string(&path).unwrap()));
            }
        }
    }
    let manifest = std::fs::read_to_string(dir.join("cheby.toml")).unwrap();
    let name = manifest
        .lines()
        .find_map(|l| l.strip_prefix("name = \""))
        .and_then(|l| l.strip_suffix('"'))
        .unwrap()
        .to_owned();
    let mut files = Vec::new();
    walk(&dir.join("src"), "", &mut files);
    (name, files)
}

/// The MIR that `cheby run` compiles for examples 003, 024 and 025 (D2,
/// reviewed by W5-C): the package's own units and the entry unit, after
/// reference counting and validation. The units of `std` modules are left
/// out, so a change to `std` does not move these goldens.
#[test]
fn example_goldens() {
    for example in ["003_fizzbuzz", "024_two_sum", "025_linked_list"] {
        let (name, files) = example_sources(&repo().join("docs/examples").join(example));
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        let lowered = crate::test_support::lower_package(&name, &refs)
            .unwrap_or_else(|errors| panic!("{example}: {errors}"));
        let text = lowered
            .units
            .iter()
            .filter(|u| &*u.path != "std" && !u.path.starts_with("std::"))
            .map(|u| dump(u).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        check_golden(&format!("examples/{example}"), &text);
    }
}
