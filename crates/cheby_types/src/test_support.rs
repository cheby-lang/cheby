//! Multi-module builds for unit tests (plan §4.6): the whole front end as
//! the driver runs it (G1), over sources given as strings.
//!
//! - [`build`] checks package `app` against a small `std::io`.
//! - [`build_with_std`] also loads fake standard-library modules, given as
//!   `(path under std, source)` pairs such as `("int", "…")` or
//!   `("json::decode", "…")`, as [`PackageId::STD`] modules. A given `io`
//!   replaces the small one. Every local module depends on each loaded home
//!   module of a builtin type, such as `std::int`, as in the driver
//!   (D-447).
//!
//! [`Build`] gives the diagnostics (also rendered, as the CLI prints
//! them), each module's interface and each module's THIR.
//!
//! Std modules' `test` blocks are checked, as when testing std itself
//! ([`Tests::All`]); a user build skips them.
//!
//! Phase-A errors in a test source are a bug in the test and panic.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use cheby_diag::{Diagnostic, FileId, SourceMap};
use cheby_iface::{
    DefId, DepView, Globals, Ice, InterfaceSet, ItemSig, ModuleId, ModuleInfo, ModuleInterface,
    ModuleTable, Namespace, PackageId, builtin,
};
use cheby_package::ModulePath;
use cheby_package::graph::{Graph, reachable, topological_order};
use cheby_resolve::{
    ImportRequest, ImportTarget, ModuleLookup, ResolveInput, StdCatalog, collect_imports,
    resolve_import_targets, resolve_module,
};
use cheby_syntax::SyntaxTree;

use crate::thir;
use crate::{Tests, TypesInput, build_interface, check_bodies_with};

/// A small `std::io` with the four printing functions, `Error`, `read_line`
/// and `read_file`, so builds that need nothing else from std stay small
/// (the real one imports `std::bytes`).
pub(crate) const STD_IO: &str = r#"//! Standard input, standard output, standard error, and reading and
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

/// An input or output error. The real one is a record with a kind and a
/// message; tests only need the type.
pub type Error {
  Error(String),
}

/// Reads the next line from standard input, without its line ending, or
/// `None` at the end of input.
@external(native, "cheby_io_read_line")
@external(js, "./io.mjs", "read_line")
pub fn read_line() -> Result<Option<String>, Error>

/// Reads the whole file at `path` as UTF-8 text.
@external(native, "cheby_io_read_file")
@external(js, "./io.mjs", "read_file")
pub fn read_file(path: String) -> Result<String, Error>
"#;

/// Standard-library modules the spec plans, by path under `std`.
fn planned() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/stdlib");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            name.strip_suffix(".md").map(str::to_owned)
        })
        .filter(|n| n != "README")
        .collect();
    names.extend(["json::decode", "json::tree"].map(str::to_owned));
    for tier2 in [
        "fiber", "channel", "selector", "duration", "random", "runtime", "file", "net", "http",
        "time", "process",
    ] {
        names.push(tier2.to_owned());
    }
    names.sort();
    names.dedup();
    names
}

/// Maps written import paths to modules, loading the test's std modules
/// on first use.
struct Lookup<'a> {
    package: &'a str,
    modules: &'a mut ModuleTable,
    sources: &'a mut SourceMap,
    shipped: &'a [(String, String)],
    planned: &'a [String],
    loaded: Vec<(ModuleId, FileId, String)>,
}

impl ModuleLookup for Lookup<'_> {
    fn lookup(&mut self, _from: PackageId, path: &[&str]) -> Result<cheby_resolve::Lookup, Ice> {
        let Some((&first, rest)) = path.split_first() else {
            return Err(Ice::new("empty path"));
        };
        let module_path = rest.iter().fold(ModulePath::root(first), |p, s| p.child(s));
        if first == self.package {
            if rest == ["main"] {
                return Ok(cheby_resolve::Lookup::RootByMainSegment);
            }
            return Ok(match self.modules.find(&module_path) {
                Some(id) => cheby_resolve::Lookup::Found(id),
                None => cheby_resolve::Lookup::NoModule {
                    package: PackageId::LOCAL,
                    closest: None,
                },
            });
        }
        if first != "std" {
            return Ok(cheby_resolve::Lookup::NoPackage);
        }
        if let Some(id) = self.modules.find(&module_path) {
            return Ok(cheby_resolve::Lookup::Found(id));
        }
        let rest_text = rest.join("::");
        if let Some((_, source)) = self.shipped.iter().find(|(p, _)| *p == rest_text) {
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
            return Ok(cheby_resolve::Lookup::Found(id));
        }
        if self.planned.contains(&rest_text) {
            return Ok(cheby_resolve::Lookup::Planned);
        }
        Ok(cheby_resolve::Lookup::NoModule {
            package: PackageId::STD,
            closest: None,
        })
    }
}

/// The result of a test build.
pub(crate) struct Build {
    pub globals: Globals,
    pub modules: ModuleTable,
    pub set: InterfaceSet,
    pub sources: SourceMap,
    /// Each checked module's THIR.
    pub thir: BTreeMap<ModuleId, thir::Module>,
    /// Every diagnostic, in the order the phases produced them.
    pub diags: Vec<Diagnostic>,
}

impl Build {
    /// The codes of every diagnostic, sorted.
    pub fn codes(&self) -> Vec<String> {
        let mut codes: Vec<String> = self.diags.iter().map(|d| d.code.to_string()).collect();
        codes.sort();
        codes
    }

    /// `CODE: message [labels]` for every diagnostic, sorted.
    pub fn messages(&self) -> Vec<String> {
        let mut messages: Vec<String> = self
            .diags
            .iter()
            .map(|d| {
                let labels: Vec<&str> = d.labels.iter().map(|l| l.message.as_str()).collect();
                format!("{}: {} {labels:?}", d.code, d.message)
            })
            .collect();
        messages.sort();
        messages
    }

    /// Every diagnostic rendered as plain text, in output order (D-418,
    /// D-264), as `cheby check` prints them.
    pub fn rendered(&self) -> String {
        let mut diags = self.diags.clone();
        self.sources.sort(&mut diags);
        cheby_diag::render_all(&diags, &self.sources)
    }

    /// The module at `path`, such as `app`, `app::util` or `std::io`.
    pub fn module(&self, path: &str) -> ModuleId {
        let mut segments = path.split("::");
        let first = ModulePath::root(segments.next().unwrap());
        self.modules
            .find(&segments.fold(first, |p, s| p.child(s)))
            .unwrap_or_else(|| panic!("no module `{path}`"))
    }

    /// The interface of the module at `path`.
    pub fn iface(&self, path: &str) -> &ModuleInterface {
        self.set.get(self.module(path)).unwrap()
    }

    /// The item `name` of namespace `ns` in the module at `path`.
    pub fn def_in(&self, path: &str, ns: Namespace, name: &str) -> DefId {
        let module = self.module(path);
        let name_id = self.globals.names.intern(name);
        let index = self
            .set
            .get(module)
            .unwrap()
            .lookup(ns, name_id)
            .unwrap_or_else(|| panic!("no `{name}` in `{path}`"));
        DefId::new(module, index)
    }

    /// The value item `name` in the module at `path`.
    pub fn def(&self, path: &str, name: &str) -> DefId {
        self.def_in(path, Namespace::Value, name)
    }

    /// The signature of the value item `name` in the module at `path`.
    pub fn fn_sig(&self, path: &str, name: &str) -> &ItemSig {
        let def = self.def(path, name);
        &self
            .set
            .get(def.module)
            .unwrap()
            .item(def.index)
            .unwrap()
            .sig
    }

    /// The THIR of the module at `path`.
    pub fn thir_of(&self, path: &str) -> &thir::Module {
        &self.thir[&self.module(path)]
    }

    /// The THIR of function `name` in the module at `path`, if it checked.
    pub fn thir_fn(&self, path: &str, name: &str) -> Option<&thir::FnDef> {
        let def = self.def(path, name);
        self.thir[&def.module].fns.iter().find(|f| f.def == def)
    }
}

fn parse(file: FileId, text: &str) -> (SyntaxTree, Vec<Diagnostic>) {
    let (tree, mut errors) = cheby_syntax::parse(file, Arc::from(text)).into_parts();
    if errors.is_empty() {
        errors.extend(cheby_syntax::validate(&tree));
    }
    (tree, errors)
}

/// Whether `text` has phase-A errors.
pub(crate) fn has_syntax_errors(text: &str) -> bool {
    let (_, errors) = parse(FileId::new(0), text);
    errors.iter().any(Diagnostic::is_error)
}

/// Builds package `app` from `(module path under app, text)` pairs, `""`
/// being the root module, with the small `std::io`.
pub(crate) fn build(local: &[(&str, &str)]) -> Build {
    build_package("app", &[], local)
}

/// [`build`] with fake standard-library modules: `(path under std,
/// text)` pairs.
pub(crate) fn build_with_std(std: &[(&str, &str)], local: &[(&str, &str)]) -> Build {
    build_package("app", std, local)
}

/// The whole front end of a build of package `package`, as the driver
/// runs it (G1): imports, the graph with the implicit home-module edges
/// (D-447), then per module in order resolution, `build_interface` and
/// `check_bodies`.
#[expect(
    clippy::too_many_lines,
    reason = "the whole front end of a build, in order"
)]
pub(crate) fn build_package(package: &str, std: &[(&str, &str)], local: &[(&str, &str)]) -> Build {
    let globals = Globals::new();
    let mut modules = ModuleTable::new();
    let mut sources = SourceMap::new();
    let mut trees: BTreeMap<ModuleId, SyntaxTree> = BTreeMap::new();
    let mut shipped: Vec<(String, String)> = std
        .iter()
        .map(|&(p, t)| (p.to_owned(), t.to_owned()))
        .collect();
    if !shipped.iter().any(|(p, _)| p == "io") {
        shipped.push(("io".to_owned(), STD_IO.to_owned()));
    }
    let planned: Vec<String> = planned()
        .into_iter()
        .filter(|p| !shipped.iter().any(|(s, _)| s == p))
        .collect();
    for (path, text) in local {
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
        let (tree, errors) = parse(file, text);
        assert!(
            errors.is_empty(),
            "syntax errors in test source: {errors:?}"
        );
        trees.insert(id, tree);
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
        let mut lookup = Lookup {
            package,
            modules: &mut modules,
            sources: &mut sources,
            shipped: &shipped,
            planned: &planned,
            loaded: Vec::new(),
        };
        let (t, d) =
            resolve_import_targets(&reqs, info.package, &mut lookup, &globals.names).unwrap();
        // A package module depends on every loaded home module of a
        // builtin type (D-447).
        let mut homes = Vec::new();
        if info.package == PackageId::LOCAL {
            for (_, segment) in builtin::HOMES {
                if shipped.iter().any(|(p, _)| p == segment)
                    && let cheby_resolve::Lookup::Found(home) =
                        lookup.lookup(PackageId::LOCAL, &["std", segment]).unwrap()
                    && !homes.contains(&home)
                {
                    homes.push(home);
                }
            }
        }
        let loaded = std::mem::take(&mut lookup.loaded);
        diags.extend(d);
        for (std_id, file, source) in loaded {
            let (tree, errors) = parse(file, &source);
            assert!(errors.is_empty(), "std module {std_id:?}: {errors:?}");
            trees.insert(std_id, tree);
        }
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
    let order = topological_order(&graph).expect("no import cycles in type tests");
    let known: Vec<String> = planned
        .iter()
        .cloned()
        .chain(shipped.iter().map(|(p, _)| p.clone()))
        .collect();
    let known_refs: Vec<&str> = known.iter().map(String::as_str).collect();
    let mut set = InterfaceSet::new(builtin::interface(&globals));
    let mut thir = BTreeMap::new();
    for node in order {
        let id = ModuleId::from_u32(node);
        if id == ModuleId::BUILTIN {
            continue;
        }
        let deps: Vec<ModuleId> = reachable(&graph, graph.successors(node))
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
        let (module, d) = check_bodies_with(&types, &iface, Tests::All).unwrap();
        diags.extend(d);
        thir.insert(id, module);
        set.insert(Arc::new(iface)).unwrap();
    }
    Build {
        globals,
        modules,
        set,
        sources,
        thir,
        diags,
    }
}

/// The same builds without the lint warnings of §13.8 that the module and
/// package warning passes add (unused code, discarded results, `todo`,
/// function equality, deprecation, negative counts), for tests about
/// other rules whose sources trigger them in passing. Resolver and
/// exhaustiveness warnings (W0001, W0007, W0008, W0010) stay.
pub(crate) mod quiet {
    use super::Build;

    const LINTS: [&str; 9] = [
        "W0002", "W0003", "W0004", "W0005", "W0006", "W0009", "W0011", "W0012", "W0013",
    ];

    fn strip(mut b: Build) -> Build {
        b.diags
            .retain(|d| !LINTS.contains(&d.code.to_string().as_str()));
        b
    }

    /// [`super::build`] without lint warnings.
    pub(crate) fn build(local: &[(&str, &str)]) -> Build {
        strip(super::build(local))
    }

    /// [`super::build_with_std`] without lint warnings.
    pub(crate) fn build_with_std(std: &[(&str, &str)], local: &[(&str, &str)]) -> Build {
        strip(super::build_with_std(std, local))
    }

    /// [`super::build_package`] without lint warnings.
    pub(crate) fn build_package(
        package: &str,
        std: &[(&str, &str)],
        local: &[(&str, &str)],
    ) -> Build {
        strip(super::build_package(package, std, local))
    }
}
