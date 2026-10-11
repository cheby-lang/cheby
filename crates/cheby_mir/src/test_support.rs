//! Lowering tests from source (plan §4.2): [`lower_source`] checks a
//! small package `app` with the whole front end, as the driver runs it
//! (G1), against the real standard library in `std/`, then lowers what
//! its `main` reaches (D-510), with reference counts and validation, as
//! `cheby run` does.
//!
//! Every MIR package writes its lowering tests with it inside its own
//! files and pins dumps with string comparisons or goldens under
//! `crates/cheby_mir/tests/golden/<package-subdir>/` with
//! `crate::golden_tests::check_golden("pat/or_patterns", &text)`, where
//! `text` is [`Lowered::dump`] or [`Lowered::function`] (`CHEBY_BLESS=1`
//! rewrites goldens).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_diag::{Diagnostic, FileId, SourceMap};
use cheby_iface::{
    DepView, Globals, Ice, InterfaceSet, ModuleId, ModuleInfo, ModuleTable, PackageId, builtin,
};
use cheby_package::ModulePath;
use cheby_package::graph::{Graph, reachable as graph_reachable, topological_order};
use cheby_resolve::{
    ImportRequest, ImportTarget, Lookup, ModuleLookup, ResolveInput, StdCatalog, collect_imports,
    resolve_import_targets, resolve_module,
};
use cheby_syntax::SyntaxTree;
use cheby_types::{Tests, TypesInput, build_interface, check_bodies_with, thir};

use crate::{
    Defs, EntryKind, EntryRequest, LowerCx, MirModule, Program, Root, count_and_check, dump,
    dump_function, lower_entries, lower_items, lower_tests, reachable,
};

/// What [`lower_source`] produced.
pub(crate) struct Lowered {
    /// The units, after reference counting and validation: one per needed
    /// module in build order, then the entries unit. Front-end errors
    /// stop before lowering, which reports no diagnostics (D-554).
    pub units: Vec<MirModule>,
}

impl Lowered {
    /// The dump of every unit, separated by blank lines.
    pub fn dump(&self) -> String {
        self.units
            .iter()
            .map(|u| dump(u).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The unit of the module at `path` (`app`, `std::io`).
    pub fn unit(&self, path: &str) -> &MirModule {
        self.units
            .iter()
            .find(|u| &*u.path == path && u.module().is_some())
            .unwrap_or_else(|| panic!("no unit `{path}`"))
    }

    /// The dump of the function whose key prints as `key` (`app::main`,
    /// `shim app::f`) in the unit of `path`.
    pub fn function(&self, path: &str, key: &str) -> String {
        let unit = self.unit(path);
        unit.functions
            .iter()
            .map(|f| dump_function(unit, f).unwrap())
            .find(|text| {
                text.strip_prefix("fn ")
                    .is_some_and(|rest| rest.starts_with(&format!("{key}(")))
            })
            .unwrap_or_else(|| panic!("no function `{key}` in:\n{}", dump(unit).unwrap()))
    }

    /// The dump of the function whose key prints as `key` (`entry
    /// main(app::main)`, `test app::…`) in any unit.
    pub fn any_function(&self, key: &str) -> String {
        self.units
            .iter()
            .flat_map(|u| {
                u.functions
                    .iter()
                    .map(move |f| dump_function(u, f).unwrap())
            })
            .find(|text| {
                text.strip_prefix("fn ")
                    .is_some_and(|rest| rest.starts_with(&format!("{key}(")))
            })
            .unwrap_or_else(|| panic!("no function `{key}` in:\n{}", self.dump()))
    }
}

/// The repository root.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The shipped standard-library modules: `(path under std, source)`.
fn shipped() -> Vec<(String, String)> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            let full = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}::{name}")
            };
            if path.is_dir() {
                walk(&path, &full, out);
            } else if path.extension().is_some_and(|e| e == "cheby") {
                out.push((full, std::fs::read_to_string(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(&repo().join("std"), "", &mut out);
    out
}

/// Standard-library modules the spec plans, by path under `std`.
fn planned() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(repo().join("docs/stdlib"))
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            name.strip_suffix(".md").map(str::to_owned)
        })
        .filter(|n| n != "README")
        .collect();
    names.sort();
    names
}

/// Maps written import paths to modules, loading std modules on first
/// use, as the driver does.
struct TestLookup<'a> {
    package: &'a str,
    modules: &'a mut ModuleTable,
    sources: &'a mut SourceMap,
    shipped: &'a [(String, String)],
    planned: &'a [String],
    loaded: Vec<(ModuleId, FileId, String)>,
}

impl ModuleLookup for TestLookup<'_> {
    fn lookup(&mut self, _from: PackageId, path: &[&str]) -> Result<Lookup, Ice> {
        let Some((&first, rest)) = path.split_first() else {
            return Err(Ice::new("empty path"));
        };
        let module_path = rest.iter().fold(ModulePath::root(first), |p, s| p.child(s));
        if first == self.package {
            if rest == ["main"] {
                return Ok(Lookup::RootByMainSegment);
            }
            return Ok(match self.modules.find(&module_path) {
                Some(id) => Lookup::Found(id),
                None => Lookup::NoModule {
                    package: PackageId::LOCAL,
                    closest: None,
                },
            });
        }
        if first != "std" {
            return Ok(Lookup::NoPackage);
        }
        if let Some(id) = self.modules.find(&module_path) {
            return Ok(Lookup::Found(id));
        }
        let under = rest.join("::");
        if let Some((_, source)) = self.shipped.iter().find(|(p, _)| *p == under) {
            let shown = format!("std/{}.cheby", rest.join("/"));
            let file = self
                .sources
                .add_file(shown, source.as_str())
                .map_err(|e| Ice::new(e.to_string()))?;
            let id = self.modules.add(ModuleInfo {
                path: module_path,
                package: PackageId::STD,
                file: Some(file),
            })?;
            self.loaded.push((id, file, source.clone()));
            return Ok(Lookup::Found(id));
        }
        if self.planned.contains(&under) {
            return Ok(Lookup::Planned);
        }
        Ok(Lookup::NoModule {
            package: PackageId::STD,
            closest: None,
        })
    }
}

fn parse(file: FileId, text: &str) -> SyntaxTree {
    let (tree, mut errors) = cheby_syntax::parse(file, Arc::from(text)).into_parts();
    if errors.is_empty() {
        errors.extend(cheby_syntax::validate(&tree));
    }
    assert!(errors.is_empty(), "syntax errors: {errors:?}");
    tree
}

/// Checks package `app` from `(module path under app, text)` pairs, `""`
/// being the root module, which must have a `main`, and lowers what
/// `main` reaches. Front-end errors are a bug in the test and panic, as
/// does an internal compiler error.
pub(crate) fn lower_source(files: &[(&str, &str)]) -> Lowered {
    lower_package("app", files).unwrap_or_else(|errors| panic!("front-end errors: {errors}"))
}

/// [`lower_source`] for a package named `package`; the front end's errors
/// are `Err`, rendered as the CLI prints them.
pub(crate) fn lower_package(package: &str, files: &[(&str, &str)]) -> Result<Lowered, String> {
    lower_package_with(package, files, false, None)
}

/// [`lower_source`] whose back end (lowering, reference counting and
/// validation) runs on a thread of its own with a stack of `stack` bytes,
/// after running `setup` there; the front end runs on the caller's
/// thread. For tests that the back end needs no deep stack.
pub(crate) fn lower_source_on_stack(files: &[(&str, &str)], stack: usize, setup: fn()) -> Lowered {
    lower_package_with("app", files, false, Some((stack, setup)))
        .unwrap_or_else(|errors| panic!("front-end errors: {errors}"))
}

/// [`lower_source`] that also lowers the `test` blocks of package `app`
/// (D-265): what they reach, a tests unit after each module's code unit,
/// and a test thunk per test after `main`'s in the entries unit, as
/// `cheby test` builds them (G4).
pub(crate) fn lower_source_with_tests(files: &[(&str, &str)]) -> Lowered {
    lower_package_with("app", files, true, None)
        .unwrap_or_else(|errors| panic!("front-end errors: {errors}"))
}

#[expect(
    clippy::too_many_lines,
    reason = "the whole front end of a build, in order"
)]
fn lower_package_with(
    package: &str,
    files: &[(&str, &str)],
    with_tests: bool,
    back_end_thread: Option<(usize, fn())>,
) -> Result<Lowered, String> {
    let globals = Globals::new();
    let mut modules = ModuleTable::new();
    let mut sources = SourceMap::new();
    let mut trees: BTreeMap<ModuleId, SyntaxTree> = BTreeMap::new();
    let shipped = shipped();
    let planned: Vec<String> = planned()
        .into_iter()
        .filter(|p| !shipped.iter().any(|(s, _)| s == p))
        .collect();
    for (path, text) in files {
        let module_path = path
            .split("::")
            .filter(|s| !s.is_empty())
            .fold(ModulePath::root(package), |p, s| p.child(s));
        let shown = match path.replace("::", "/") {
            p if p.is_empty() => "src/main.cheby".to_owned(),
            p => format!("src/{p}.cheby"),
        };
        let file = sources.add_file(shown, *text).unwrap();
        let id = modules
            .add(ModuleInfo {
                path: module_path,
                package: PackageId::LOCAL,
                file: Some(file),
            })
            .unwrap();
        trees.insert(id, parse(file, text));
    }
    let mut diags = Vec::new();
    let mut requests: BTreeMap<ModuleId, Vec<ImportRequest>> = BTreeMap::new();
    let mut targets: BTreeMap<ModuleId, Vec<ImportTarget>> = BTreeMap::new();
    let mut implicit: BTreeMap<ModuleId, Vec<ModuleId>> = BTreeMap::new();
    let mut next = 1_u32;
    while usize::try_from(next).unwrap() < modules.len() {
        let id = ModuleId::from_u32(next);
        next = next.checked_add(1).unwrap();
        let info = modules.get(id).unwrap().clone();
        let reqs = collect_imports(&trees[&id], &globals.names);
        let mut lookup = TestLookup {
            package,
            modules: &mut modules,
            sources: &mut sources,
            shipped: &shipped,
            planned: &planned,
            loaded: Vec::new(),
        };
        let (t, d) =
            resolve_import_targets(&reqs, info.package, &mut lookup, &globals.names).unwrap();
        // A package module depends on every shipped home module of a
        // prelude type (D-447).
        let mut homes = Vec::new();
        if info.package == PackageId::LOCAL {
            for (_, segment) in builtin::HOMES {
                if let Lookup::Found(home) =
                    lookup.lookup(PackageId::LOCAL, &["std", segment]).unwrap()
                    && !homes.contains(&home)
                {
                    homes.push(home);
                }
            }
        }
        for (std_id, file, source) in std::mem::take(&mut lookup.loaded) {
            trees.insert(std_id, parse(file, &source));
        }
        diags.extend(d);
        requests.insert(id, reqs);
        targets.insert(id, t);
        implicit.insert(id, homes);
    }
    let mut graph = Graph::new(modules.len());
    for (&id, ts) in &targets {
        for t in ts {
            if let Some(to) = t.module() {
                graph.add_edge(id.as_u32(), to.as_u32());
            }
        }
    }
    for (&id, homes) in &implicit {
        for home in homes {
            graph.add_edge(id.as_u32(), home.as_u32());
        }
    }
    let order: Vec<ModuleId> = topological_order(&graph)
        .expect("no import cycles in lowering tests")
        .into_iter()
        .map(ModuleId::from_u32)
        .filter(|&m| m != ModuleId::BUILTIN)
        .collect();
    let known: Vec<String> = planned
        .iter()
        .cloned()
        .chain(shipped.iter().map(|(p, _)| p.clone()))
        .collect();
    let known_refs: Vec<&str> = known.iter().map(String::as_str).collect();
    let mut set = InterfaceSet::new(builtin::interface(&globals));
    let mut checked: BTreeMap<ModuleId, (thir::Module, Vec<ModuleId>)> = BTreeMap::new();
    for &id in &order {
        let deps: Vec<ModuleId> = graph_reachable(&graph, graph.successors(id.as_u32()))
            .into_iter()
            .map(ModuleId::from_u32)
            .collect();
        let info = modules.get(id).unwrap().clone();
        let input = ResolveInput {
            module: id,
            package: info.package,
            tree: &trees[&id],
            imports: &requests[&id],
            targets: &targets[&id],
            deps: DepView::new(&set, &deps),
            modules: &modules,
            globals: &globals,
            std: StdCatalog { known: &known_refs },
            package_targets: cheby_package::Targets::ALL,
        };
        let (resolved, d) = resolve_module(&input).unwrap();
        diags.extend(d);
        let types = TypesInput {
            module: id,
            package: info.package,
            tree: &trees[&id],
            resolved: &resolved,
            deps: DepView::new(&set, &deps),
            modules: &modules,
            globals: &globals,
        };
        let (iface, d) = build_interface(&types).unwrap();
        diags.extend(d);
        let (module, d) = check_bodies_with(&types, &iface, Tests::SkipStd).unwrap();
        diags.extend(d);
        checked.insert(id, (module, deps));
        set.insert(Arc::new(iface)).unwrap();
    }
    if diags.iter().any(Diagnostic::is_error) {
        let mut sorted = diags.clone();
        sources.sort(&mut sorted);
        return Err(cheby_diag::render_all(&sorted, &sources));
    }

    // The back end, as `cheby run` does it (D-510).
    let back_end = || {
        back_end(
            package, &modules, &checked, &order, &set, &globals, &sources, with_tests,
        )
    };
    let units = match back_end_thread {
        None => back_end(),
        Some((stack, setup)) => std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("back end".to_owned())
                .stack_size(stack)
                .spawn_scoped(scope, || {
                    setup();
                    back_end()
                })
                .unwrap()
                .join()
                .unwrap()
        }),
    };
    Ok(Lowered { units })
}

/// The back end of [`lower_package_with`]: the units, each counted and
/// checked. An internal error panics with the unit's dump.
#[expect(
    clippy::too_many_arguments,
    reason = "what the front end of a test build produced"
)]
fn back_end(
    package: &str,
    modules: &ModuleTable,
    checked: &BTreeMap<ModuleId, (thir::Module, Vec<ModuleId>)>,
    order: &[ModuleId],
    set: &InterfaceSet,
    globals: &Globals,
    sources: &SourceMap,
    with_tests: bool,
) -> Vec<MirModule> {
    let root = modules.find(&ModulePath::root(package)).unwrap();
    let main = checked[&root]
        .0
        .fns
        .iter()
        .find(|f| f.entry.is_some())
        .expect("the root module has a `main`")
        .clone();
    let program: Program<'_> = checked.iter().map(|(&m, (t, _))| (m, t)).collect();
    // The local package's tests, by module path and then in source order.
    let mut tests: Vec<(ModuleId, &thir::TestDef)> = Vec::new();
    if with_tests {
        let mut local: Vec<ModuleId> = order
            .iter()
            .copied()
            .filter(|&m| {
                modules
                    .get(m)
                    .is_some_and(|i| i.package == PackageId::LOCAL)
            })
            .collect();
        local.sort_by_key(|&m| modules.get(m).map(|i| i.path.to_string()));
        for m in local {
            let mut ts: Vec<&thir::TestDef> = checked[&m].0.tests.iter().collect();
            ts.sort_by_key(|t| t.def);
            tests.extend(ts.into_iter().map(|t| (m, t)));
        }
    }
    let mut roots = vec![Root::Fn(&main)];
    roots.extend(tests.iter().map(|&(_, t)| Root::Test(t)));
    let reach = reachable(&program, &roots).unwrap();
    let mut units = Vec::new();
    let mut push = |mut unit: MirModule| {
        if let Err(ice) = count_and_check(&mut unit) {
            panic!("{ice}\n{}", dump(&unit).unwrap_or_default());
        }
        units.push(unit);
    };
    for &id in order {
        if !reach.needs_module(id) {
            continue;
        }
        let (thir, deps) = &checked[&id];
        let view = DepView::new(set, deps);
        let cx = LowerCx {
            defs: Defs {
                own: set.get(id),
                deps: &view,
                modules,
                globals,
            },
            sources,
        };
        push(lower_items(thir, &|def| reach.contains(def), &cx).unwrap());
        if tests.iter().any(|&(m, _)| m == id) {
            push(lower_tests(thir, &cx).unwrap());
        }
    }
    let mut all: Vec<ModuleId> = order.to_vec();
    all.sort_unstable();
    let view = DepView::new(set, &all);
    let cx = LowerCx {
        defs: Defs {
            own: None,
            deps: &view,
            modules,
            globals,
        },
        sources,
    };
    let mut requests = vec![EntryRequest {
        kind: EntryKind::Main(main.def),
        info: main.entry.as_ref(),
    }];
    requests.extend(tests.iter().map(|&(_, t)| EntryRequest {
        kind: EntryKind::Test(t.def),
        info: Some(&t.entry),
    }));
    push(lower_entries(&requests, &cx).unwrap());
    units
}
