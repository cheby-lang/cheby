//! Resolution tests over small multi-module builds, and the import graph
//! and resolution of every example.

use std::path::Path;
use std::sync::Arc;

use cheby_diag::{Diagnostic, FileId, SourceMap};
use cheby_iface::{
    DefId, DefIndex, DefKind, DepView, Globals, IfaceItem, IndexVec, InterfaceSet, ItemSig,
    ModuleId, ModuleInfo, ModuleInterface, ModuleTable, PackageId, TypeDef, TypeDefKind, builtin,
};
use cheby_package::ModulePath;
use cheby_package::graph::{Graph, reachable, topological_order};
use cheby_syntax::SyntaxTree;
use cheby_syntax::ast::{AstNode, Item, PathNode, TypeDecl};
use rustc_hash::FxHashMap;

use crate::*;

/// A small `std/io.cheby`: the slice's four printing functions (D-450),
/// `Error`, `read_line` and `read_file`.
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

/// A std module, so another package: private, package-visible, opaque and
/// exposed items, for E0043.
const STD_SECRET: &str = r"//! Test module.

pub fn open() {
  Nil
}

priv fn hidden() {
  Nil
}

fn internal() {
  Nil
}

pub type Token {
  Token(String),
}

pub exposed type Open {
  Open(String),
}

pub type Pair { left: String, right: String }

interface Internal

pub interface Visible {
  fn visible(Self) -> String
}

pub interface Wide: Visible {
  fn wide(Self) -> String
}
";

/// The home modules of `Option` and of the kernel type `Map` (D-447,
/// D-481): they do not declare the builtin types, but use them.
const STD_OPTION: &str = "//! Options.\n\npub fn is_some<T>(option: Option<T>) -> Bool {\n  case option {\n    Some(_) => True\n    None => False\n  }\n}\n";
const STD_MAP: &str = "//! Maps.\n\n@external(native, \"cheby_map_size\")\n@external(js, \"./map.mjs\", \"size\")\npub fn size<K, V>(map: Map<K, V>) -> Int\n";

/// Standard-library modules the spec plans but the test std does not ship.
fn planned() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/stdlib");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            name.strip_suffix(".md").map(str::to_owned)
        })
        .filter(|n| n != "README" && !SHIPPED.iter().any(|(p, _)| p == n))
        .collect();
    names.push("json::decode".to_owned());
    // `std::fiber`, `std::channel` and `std::duration` ship from M2's first
    // slice (D-729); their pages above still keep them planned here,
    // because this test std does not ship them.
    for tier2 in [
        "selector", "random", "runtime", "file", "net", "http", "time", "process",
    ] {
        names.push(tier2.to_owned());
    }
    names.sort();
    names
}

const SHIPPED: &[(&str, &str)] = &[
    ("io", STD_IO),
    ("secret", STD_SECRET),
    ("option", STD_OPTION),
    ("map", STD_MAP),
    ("std_attrs", STD_ATTRS),
    ("std_ctx", STD_CTX),
];

/// A std module with the std-only attribute `@std_context` (D-482, D-692).
const STD_CTX: &str = r#"//! The std-only attribute `@std_context`.

@std_context
@external(native, "cheby_ctx_kernel")
@external(js, "./ctx.mjs", "kernel")
pub fn kernel(count: Int) -> Int

@std_context
@external(native, "cheby_ctx_with_body")
pub fn with_body(count: Int) -> Int {
  count
}

@external(native, "cheby_ctx_plain")
pub fn plain(count: Int) -> Int

@std_context
@external(js, "./ctx.mjs", "js_only")
pub fn js_only(count: Int) -> Int

@std_context
pub fn no_external(count: Int) -> Int {
  count
}

@std_context(count)
@external(native, "cheby_ctx_arguments")
pub fn arguments(count: Int) -> Int

@std_context
@std_context
@external(native, "cheby_ctx_twice")
pub fn twice(count: Int) -> Int

@std_context
pub type Handle {
  Handle(Int),
}

@std_context
pub const limit: Int = 1
"#;

/// A std module with the std-only attributes (D-482).
const STD_ATTRS: &str = r#"//! Std-only attributes.

@std_clamped(count)
pub fn take(text: String, count: Int) -> String {
  text
}

@std_clamped(count, nope, count)
pub fn drop(text: String, count: Int) -> String {
  text
}

@std_hooks(equal, hash, debug)
pub type Handle {
  Handle(String),
}

@std_hooks(equal, hash)
pub type Broken {
  Broken(String),
}

@std_hooks(equal, missing, debug)
pub type Missing {
  Missing(String),
}

@std_clamped(count)
pub type Misplaced {
  Misplaced(String),
}

fn equal(a: Handle, b: Handle) -> Bool {
  True
}

fn hash(a: Handle) -> Int {
  0
}

fn debug(a: Handle) -> String {
  "handle"
}
"#;

struct Lookup<'a> {
    package: &'a str,
    modules: &'a mut ModuleTable,
    planned: &'a [String],
    /// Std modules loaded on first import, to parse.
    loaded: Vec<(ModuleId, &'static str)>,
}

impl ModuleLookup for Lookup<'_> {
    fn lookup(&mut self, _from: PackageId, path: &[&str]) -> Result<crate::Lookup, Ice> {
        let Some((&first, rest)) = path.split_first() else {
            return Err(Ice::new("empty path"));
        };
        let mut module_path = ModulePath::root(first);
        for segment in rest {
            module_path = module_path.child(segment);
        }
        if first == self.package {
            if rest == ["main"] {
                return Ok(crate::Lookup::RootByMainSegment);
            }
            return Ok(match self.modules.find(&module_path) {
                Some(id) => crate::Lookup::Found(id),
                None => crate::Lookup::NoModule {
                    package: PackageId::LOCAL,
                    closest: None,
                },
            });
        }
        if first != "std" {
            return Ok(crate::Lookup::NoPackage);
        }
        if let Some(id) = self.modules.find(&module_path) {
            return Ok(crate::Lookup::Found(id));
        }
        let rest_text = rest.join("::");
        if let Some(&(_, source)) = SHIPPED.iter().find(|(p, _)| *p == rest_text) {
            let id = self.modules.add(ModuleInfo {
                path: module_path,
                package: PackageId::STD,
                file: None,
            })?;
            self.loaded.push((id, source));
            return Ok(crate::Lookup::Found(id));
        }
        if self.planned.contains(&rest_text) {
            return Ok(crate::Lookup::Planned);
        }
        let closest =
            closest_name(&rest_text, SHIPPED.iter().map(|(p, _)| *p)).map(|p| format!("std::{p}"));
        Ok(crate::Lookup::NoModule {
            package: PackageId::STD,
            closest,
        })
    }
}

struct Build {
    globals: Globals,
    modules: ModuleTable,
    trees: FxHashMap<ModuleId, SyntaxTree>,
    resolved: FxHashMap<ModuleId, ResolvedModule>,
    set: InterfaceSet,
    deps: FxHashMap<ModuleId, Vec<ModuleId>>,
    diags: Vec<Diagnostic>,
}

impl Build {
    fn codes(&self) -> Vec<String> {
        let mut codes: Vec<String> = self.diags.iter().map(|d| d.code.to_string()).collect();
        codes.sort();
        codes
    }

    fn messages(&self) -> Vec<String> {
        let mut messages: Vec<String> = self
            .diags
            .iter()
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect();
        messages.sort();
        messages
    }

    fn module(&self, path: &str) -> ModuleId {
        let mut segments = path.split("::");
        let mut p = ModulePath::root(segments.next().unwrap());
        for s in segments {
            p = p.child(s);
        }
        self.modules.find(&p).unwrap()
    }

    fn resolved(&self, path: &str) -> &ResolvedModule {
        &self.resolved[&self.module(path)]
    }

    /// The resolutions of the paths of item `name`'s body, by path text,
    /// in source order.
    fn body_paths(&self, path: &str, name: &str) -> Vec<(String, Res)> {
        let module = self.module(path);
        let resolved = &self.resolved[&module];
        let tree = &self.trees[&module];
        let (index, _) = resolved
            .items
            .iter_enumerated()
            .find(|(_, item)| self.globals.names.get(item.name) == name)
            .unwrap();
        let body = &resolved.bodies[&index];
        let node = tree.node(resolved.items.get(index).unwrap().node).unwrap();
        node.descendants()
            .filter_map(PathNode::cast)
            .filter_map(|p| {
                let res = body.paths.get(&p.syntax().id())?;
                Some((p.syntax().text().trim().to_owned(), *res))
            })
            .collect()
    }

    fn def(&self, path: &str, name: &str) -> DefId {
        let module = self.module(path);
        let iface = self.set.get(module).unwrap();
        let (index, _) = iface
            .items
            .iter_enumerated()
            .find(|(_, item)| self.globals.names.get(item.name) == name)
            .unwrap();
        DefId::new(module, index)
    }
}

fn parse(id: ModuleId, text: &str) -> (SyntaxTree, Vec<Diagnostic>) {
    let file = FileId::new(id.as_u32());
    let (tree, mut errors) = cheby_syntax::parse(file, Arc::from(text)).into_parts();
    if errors.is_empty() {
        errors.extend(cheby_syntax::validate(&tree));
    }
    (tree, errors)
}

/// Builds package `app` from `(module path under app, text)` pairs, `""`
/// being the root module.
fn build(local: &[(&str, &str)]) -> Build {
    build_package("app", local)
}

#[expect(
    clippy::too_many_lines,
    reason = "the whole front end of a build, in order"
)]
fn build_package(package: &str, local: &[(&str, &str)]) -> Build {
    let globals = Globals::new();
    let mut modules = ModuleTable::new();
    let mut trees = FxHashMap::default();
    let mut diags = Vec::new();
    for (path, text) in local {
        let module_path = if path.is_empty() {
            ModulePath::root(package)
        } else {
            path.split("::")
                .fold(ModulePath::root(package), |p, s| p.child(s))
        };
        let id = modules
            .add(ModuleInfo {
                path: module_path,
                package: PackageId::LOCAL,
                file: None,
            })
            .unwrap();
        let (tree, errors) = parse(id, text);
        assert!(
            errors.is_empty(),
            "syntax errors in test source: {errors:?}"
        );
        trees.insert(id, tree);
    }
    let planned = planned();
    // Imports and their targets, module by module in id order (B3).
    let mut requests: FxHashMap<ModuleId, Vec<ImportRequest>> = FxHashMap::default();
    let mut targets: FxHashMap<ModuleId, Vec<ImportTarget>> = FxHashMap::default();
    let mut next = 1_u32;
    while usize::try_from(next).unwrap() < modules.len() {
        let id = ModuleId::from_u32(next);
        next = next.checked_add(1).unwrap();
        let info = modules.get(id).unwrap().clone();
        let tree = &trees[&id];
        let reqs = collect_imports(tree, &globals.names);
        let mut lookup = Lookup {
            package,
            modules: &mut modules,
            planned: &planned,
            loaded: Vec::new(),
        };
        let (t, d) =
            resolve_import_targets(&reqs, info.package, &mut lookup, &globals.names).unwrap();
        let loaded = std::mem::take(&mut lookup.loaded);
        diags.extend(d);
        for (std_id, source) in loaded {
            let (tree, errors) = parse(std_id, source);
            assert!(errors.is_empty(), "{errors:?}");
            trees.insert(std_id, tree);
        }
        requests.insert(id, reqs);
        targets.insert(id, t);
    }
    // The graph and the order (G1).
    let mut graph = Graph::new(modules.len());
    for (&id, ts) in &targets {
        for t in ts {
            if let Some(to) = t.module() {
                graph.add_edge(id.as_u32(), to.as_u32());
            }
        }
    }
    let mut set = InterfaceSet::new(builtin::interface(&globals));
    let mut resolved = FxHashMap::default();
    let mut deps_of = FxHashMap::default();
    let order = match topological_order(&graph) {
        Ok(order) => order,
        Err(cycles) => {
            for cycle in cycles {
                let nodes = &cycle.nodes;
                let with_spans: Vec<(ModuleId, cheby_diag::Span)> = nodes
                    .iter()
                    .enumerate()
                    .map(|(n, &node)| {
                        let from = ModuleId::from_u32(node);
                        let to = ModuleId::from_u32(
                            nodes[n.checked_add(1).unwrap().checked_rem(nodes.len()).unwrap()],
                        );
                        let k = targets[&from]
                            .iter()
                            .position(|t| t.module() == Some(to))
                            .unwrap();
                        (from, requests[&from][k].span)
                    })
                    .collect();
                diags.push(cycle_diagnostic(&with_spans, &modules).unwrap());
            }
            return Build {
                globals,
                modules,
                trees,
                resolved,
                set,
                deps: deps_of,
                diags,
            };
        }
    };
    let std_modules: Vec<String> = planned
        .iter()
        .cloned()
        .chain(SHIPPED.iter().map(|(p, _)| (*p).to_owned()))
        .collect();
    let std_refs: Vec<&str> = std_modules.iter().map(String::as_str).collect();
    for node in order {
        let id = ModuleId::from_u32(node);
        if id == ModuleId::BUILTIN {
            continue;
        }
        let succ: Vec<u32> = graph.successors(node).to_vec();
        let deps: Vec<ModuleId> = reachable(&graph, &succ)
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
            std: StdCatalog { known: &std_refs },
            package_targets: cheby_package::Targets::ALL,
        };
        let (module, d) = resolve_module(&input).unwrap();
        diags.extend(d);
        let iface = fake_interface(&module, &trees[&id], info.package);
        set.insert(Arc::new(iface)).unwrap();
        resolved.insert(id, module);
        deps_of.insert(id, deps);
    }
    Build {
        globals,
        modules,
        trees,
        resolved,
        set,
        deps: deps_of,
        diags,
    }
}

/// What resolution of dependents needs from an interface: names, kinds,
/// visibility and `exposed`. The real one is built by `cheby_types`.
fn fake_interface(
    module: &ResolvedModule,
    tree: &SyntaxTree,
    package: PackageId,
) -> ModuleInterface {
    let items: Vec<IfaceItem> = module
        .items
        .iter_enumerated()
        .map(|(index, item)| {
            let sig = match item.kind {
                DefKind::Interface => ItemSig::Interface(cheby_iface::InterfaceDef {
                    embeds: embeds(module, tree, item),
                    fns: module
                        .items
                        .iter_enumerated()
                        .filter(|(_, f)| f.parent == Some(index))
                        .map(|(f, _)| f)
                        .collect(),
                }),
                DefKind::Type => {
                    let exposed = tree
                        .node(item.node)
                        .and_then(TypeDecl::cast)
                        .is_some_and(TypeDecl::is_exposed);
                    ItemSig::Type(TypeDef {
                        generics: cheby_iface::Generics::empty(),
                        kind: TypeDefKind::Adt {
                            ctors: Box::new([]),
                        },
                        exposed,
                        home: None,
                        hooks: None,
                    })
                }
                _ => ItemSig::Error,
            };
            IfaceItem {
                name: item.name,
                kind: item.kind,
                vis: item.vis,
                parent: item.parent,
                span: Some(item.name_span),
                targets: item.attrs.targets,
                deprecated: item.attrs.deprecated.clone(),
                sig,
            }
        })
        .collect();
    ModuleInterface::new(module.module, package, IndexVec::from_raw(items).unwrap()).unwrap()
}

/// The interfaces an interface declaration embeds, as resolved.
fn embeds(module: &ResolvedModule, tree: &SyntaxTree, item: &ItemDef) -> Box<[DefId]> {
    let decl = tree
        .node(item.node)
        .and_then(cheby_syntax::ast::InterfaceDecl::cast)
        .unwrap();
    decl.bounds()
        .iter()
        .flat_map(|b| b.interfaces())
        .filter_map(|i| match module.sig_paths.get(&i.path()?.syntax().id()) {
            Some(Res::Def(def)) => Some(*def),
            _ => None,
        })
        .collect()
}

// ---- 001 and imports ------------------------------------------------------

const HELLO: &str = "//! Prints a greeting to standard output.\n\nimport std::io\n\nfn main() {\n  io::println(\"Hello, world!\")\n}\n";

#[test]
fn hello_world_resolves() {
    let b = build(&[("", HELLO)]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    let app = b.resolved("app");
    let main = app.items.get(DefIndex::from_u32(0)).unwrap();
    assert_eq!(b.globals.names.get(main.name), "main");
    assert_eq!(main.vis, cheby_iface::Vis::Package);
    let println = b.def("std::io", "println");
    assert_eq!(
        b.body_paths("app", "main"),
        [("io::println".to_owned(), Res::Def(println))]
    );
    let std_io = b.resolved("std::io");
    let item = std_io.items.get(println.index).unwrap();
    assert_eq!(
        item.attrs.native_external().map(|(s, _)| s),
        Some("cheby_io_println")
    );
    assert_eq!(item.attrs.externals.len(), 2);
    assert!(std_io.bodies.is_empty());
    // `String` in the signature resolves to the prelude type.
    let strings: Vec<_> = std_io
        .sig_paths
        .values()
        .filter(|r| **r == Res::Def(builtin::STRING))
        .collect();
    assert_eq!(strings.len(), 8);
    // The import scope entry.
    let io = b.globals.names.intern("io");
    let entry = app.scope.get(cheby_iface::Namespace::Value, io).unwrap();
    assert_eq!(entry.res, Res::Module(b.module("std::io")));
    assert_eq!(entry.source, ScopeSource::Import(0));
    // std::io's dependencies are only the builtin module.
    assert_eq!(b.deps[&b.module("std::io")], []);
    assert_eq!(b.deps[&b.module("app")], [b.module("std::io")]);
}

#[test]
fn import_errors() {
    let b = build(&[
        (
            "",
            "import app::nope\nimport foo::bar\nimport app::main\nimport std::list\nimport std::iox\nimport std::io as out\n\nfn main() {\n  out::println(\"x\")\n}\n",
        ),
        ("web", "fn f() {\n  Nil\n}\n"),
    ]);
    assert_eq!(
        b.messages(),
        [
            "E0038: the module `std::list` is not supported yet",
            "E0039: the root module is imported as `app`, not `app::main`",
            "E0039: there is no module `app::nope`",
            "E0039: there is no module `std::iox`",
            "E0039: there is no package named `foo`",
        ]
    );
    let d = b
        .diags
        .iter()
        .find(|d| d.message.contains("std::iox"))
        .unwrap();
    assert_eq!(
        d.helps[0].message,
        "a module with a similar name exists: `std::io`"
    );
    // D-142's error points at the import.
    let d = b
        .diags
        .iter()
        .find(|d| d.message.contains("app::main"))
        .unwrap();
    let primary: Vec<(u32, u32)> = d
        .labels
        .iter()
        .filter(|l| l.style == cheby_diag::LabelStyle::Primary)
        .map(|l| (l.span.range.start(), l.span.range.end()))
        .collect();
    assert_eq!(primary, [(40, 49)]);
}

#[test]
fn import_cycles_are_reported_once() {
    let b = build(&[
        ("", "import app::a\n\nfn main() {\n  Nil\n}\n"),
        ("a", "import app::b\n\nfn f() {\n  Nil\n}\n"),
        ("b", "import app::a\n\nfn g() {\n  Nil\n}\n"),
        ("c", "import app::c\n\nfn h() {\n  Nil\n}\n"),
    ]);
    assert_eq!(
        b.messages(),
        [
            "E0040: modules import each other in a cycle",
            "E0040: the module `app::c` imports itself",
        ]
    );
    let cycle = b.diags.iter().find(|d| d.labels.len() == 2).unwrap();
    assert_eq!(cycle.notes, ["the cycle is app::a → app::b → app::a"]);
}

// ---- items, scope, attributes -----------------------------------------------

#[test]
fn duplicates() {
    let b = build(&[(
        "",
        "import std::io\nimport std::secret as io\n\nfn total() {\n  Nil\n}\n\nfn total() {\n  Nil\n}\n\nfn main() {\n  total()\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0041: `io` is imported twice",
            "E0041: `total` is declared twice in this module",
        ]
    );
    let dup = b
        .diags
        .iter()
        .find(|d| d.message.contains("total"))
        .unwrap();
    assert_eq!(dup.labels.len(), 2);
}

#[test]
fn import_colliding_with_item() {
    let b = build(&[(
        "",
        "import std::io\n\nfn io() {\n  Nil\n}\n\nfn main() {\n  io()\n  io::println(\"x\")\n}\n",
    )]);
    // One error, at the declaration, which is the second name (§7.7); the
    // qualified use reports nothing more.
    assert_eq!(
        b.messages(),
        ["E0041: `io` is both imported and declared in this module"]
    );
    let labels: Vec<(cheby_diag::LabelStyle, u32, &str)> = b.diags[0]
        .labels
        .iter()
        .map(|l| (l.style, l.span.range.start(), l.message.as_str()))
        .collect();
    assert_eq!(
        labels,
        [
            (cheby_diag::LabelStyle::Primary, 19, "declared here"),
            (cheby_diag::LabelStyle::Secondary, 12, "imported here"),
        ]
    );
}

#[test]
fn attributes() {
    let b = build(&[(
        "",
        "@inline\nfn a() {\n  Nil\n}\n\n@external(native, \"x\")\n@external(native, \"y\")\nfn b()\n\n@external(native)\nfn c()\n\n@external(native, \"z\")\nconst d: String = \"\"\n\n@target(wasm)\nfn e() {\n  Nil\n}\n\n@deprecated(\"old\")\nfn f() {\n  Nil\n}\n\n@blocking\nfn g() {\n  Nil\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0045: `@blocking` is not allowed on a function",
            "E0045: `@deprecated` is allowed only on `pub` items",
            "E0045: `@external` is not allowed on a constant",
            "E0045: malformed `@external` attribute",
            "E0045: malformed `@target` attribute",
            "E0045: two `@external` attributes for the `native` target",
            "E0045: unknown attribute `@inline`",
            "E0046: the function `c` has no body",
        ]
    );
}

#[test]
fn valid_attributes_are_recorded() {
    let b = build(&[(
        "",
        "@target(native)\nfn a() {\n  Nil\n}\n\n@external(native, \"x\")\n@blocking\npub fn b()\n\n@external(js, \"./m.mjs\", \"c\")\n@async\n@deprecated(\"use b\")\npub fn c()\n\n@external(native, \"d\")\n@external(js, \"./m.mjs\", \"d\")\npub fn d()\n\n@external(native, \"e\")\nfn e() {\n  Nil\n}\n",
    )]);
    // `@target`, `@blocking`, `@async` and `@deprecated` are accepted. A
    // bodiless function that misses a target of the package resolves; the
    // cross-target check (C12) reports its uses.
    assert_eq!(b.messages(), Vec::<String>::new());
    let app = b.resolved("app");
    let status = |n: u32| app.items.get(DefIndex::from_u32(n)).unwrap();
    assert_eq!(
        status(0).attrs.targets,
        cheby_package::Targets::only(cheby_package::Target::Native)
    );
    assert!(status(1).attrs.blocking);
    let c = status(2);
    assert!(c.attrs.is_async);
    assert_eq!(c.attrs.deprecated.as_deref(), Some("use b"));
    assert_eq!(
        c.attrs.targets,
        cheby_package::Targets::only(cheby_package::Target::Js)
    );
    assert_eq!(status(3).attrs.targets, cheby_package::Targets::ALL);
    // A function with a body and an `@external` for one target exists on
    // every target (D-207).
    assert_eq!(status(4).attrs.targets, cheby_package::Targets::ALL);
    assert!(app.bodies.contains_key(&DefIndex::from_u32(4)));
}

#[test]
fn targets_of_every_item_kind() {
    use cheby_package::{Target, Targets};
    let b = build(&[
        (
            "",
            "@target(js)\ntype T {\n  A,\n}\n\n@target(native)\ntype Alias = String\n\n@target(js)\nconst c: String = \"c\"\n\n@target(native)\ninterface I {\n  fn f(Self)\n}\n\n@target(js)\ntest \"t\" {\n  Nil\n}\n\n@target(js)\n@external(native, \"n\")\n@external(js, \"./m.mjs\", \"n\")\nfn n()\n\nfn main() {\n  Nil\n}\n",
        ),
        (
            "web",
            "//! Only on JS.\n\n@!target(js)\n\ntype W {\n  W,\n}\n\n@target(js)\nfn on_js() {\n  Nil\n}\n\n@target(native)\nfn nowhere() {\n  Nil\n}\n\n@external(js, \"./w.mjs\", \"x\")\nfn x()\n",
        ),
    ]);
    // `n` exists only on JS (narrowed by `@target`), which the cross-target
    // check (C12) handles at its uses. `web::nowhere` is native-only in a
    // JS-only module, so it exists on no target.
    assert_eq!(b.messages(), ["E0045: `nowhere` exists on no target"]);
    let targets = |path: &str| -> Vec<(String, Targets)> {
        b.resolved(path)
            .items
            .iter()
            .map(|i| (b.globals.names.get(i.name).to_owned(), i.attrs.targets))
            .collect()
    };
    let js = Targets::only(Target::Js);
    let native = Targets::only(Target::Native);
    assert_eq!(
        targets("app"),
        [
            ("T".to_owned(), js),
            ("A".to_owned(), js),
            ("Alias".to_owned(), native),
            ("c".to_owned(), js),
            ("I".to_owned(), native),
            ("f".to_owned(), native),
            ("t".to_owned(), js),
            ("n".to_owned(), js),
            ("main".to_owned(), Targets::ALL),
        ]
    );
    assert_eq!(b.resolved("app::web").targets, js);
    assert_eq!(
        targets("app::web"),
        [
            ("W".to_owned(), js),
            ("W".to_owned(), js),
            ("on_js".to_owned(), js),
            ("nowhere".to_owned(), Targets::NONE),
            ("x".to_owned(), js),
        ]
    );
}

#[test]
fn std_only_attributes() {
    let b = build(&[(
        "",
        "import std::std_attrs\n\n@std_clamped(count)\nfn take(count: Int) -> Int {\n  count\n}\n\n@std_hooks(a, b, c)\ntype T {\n  T,\n}\n\nfn main() {\n  Nil\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0042: cannot find function `missing` in this module",
            "E0045: `@std_clamped` is not allowed on a type",
            "E0045: `count` is named twice in `@std_clamped`",
            "E0045: malformed `@std_hooks` attribute",
            "E0045: the function has no parameter `nope`",
            // Outside the standard library they are unknown (§4.8).
            "E0045: unknown attribute `@std_clamped`",
            "E0045: unknown attribute `@std_hooks`",
        ]
    );
    let std = b.resolved("std::std_attrs");
    let item = |name: &str| {
        std.items
            .iter()
            .find(|i| b.globals.names.get(i.name) == name)
            .unwrap()
    };
    let names = |list: &[(cheby_iface::Name, cheby_diag::Span)]| -> Vec<&str> {
        list.iter().map(|(n, _)| b.globals.names.get(*n)).collect()
    };
    assert_eq!(names(&item("take").attrs.clamped), ["count"]);
    assert_eq!(names(&item("drop").attrs.clamped), ["count"]);
    assert_eq!(
        names(&item("Handle").attrs.hooks.unwrap()),
        ["equal", "hash", "debug"]
    );
    assert!(item("Broken").attrs.hooks.is_none());
    let app = b.resolved("app");
    assert!(
        app.items
            .iter()
            .all(|i| i.attrs.clamped.is_empty() && i.attrs.hooks.is_none())
    );
}

/// `@std_context` takes no arguments and is allowed only on a function
/// with an `@external`, and only in the standard library; elsewhere it is
/// an unknown attribute (D-482, D-692).
#[test]
fn std_context_attribute() {
    let b = build(&[(
        "",
        "import std::std_ctx\n\n@std_context\n@external(native, \"cheby_app_kernel\")\nfn kernel(count: Int) -> Int\n\nfn main() {\n  Nil\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0045: `@std_context` is not allowed on a constant",
            "E0045: `@std_context` is not allowed on a function",
            "E0045: `@std_context` is not allowed on a type",
            "E0045: `@std_context` takes no arguments",
            "E0045: duplicate `@std_context` attribute",
            // Outside the standard library it is unknown (§4.8).
            "E0045: unknown attribute `@std_context`",
        ]
    );
    let std = b.resolved("std::std_ctx");
    let context = |name: &str| {
        std.items
            .iter()
            .find(|i| b.globals.names.get(i.name) == name)
            .unwrap()
            .attrs
            .std_context
    };
    for name in ["kernel", "with_body", "js_only", "twice"] {
        assert!(context(name), "{name}");
    }
    for name in ["plain", "no_external", "arguments", "Handle", "limit"] {
        assert!(!context(name), "{name}");
    }
    // Neither the type nor its constructor carries it.
    let handles: Vec<bool> = std
        .items
        .iter()
        .filter(|i| b.globals.names.get(i.name) == "Handle")
        .map(|i| i.attrs.std_context)
        .collect();
    assert_eq!(handles, [false, false]);
    let app = b.resolved("app");
    assert!(app.items.iter().all(|i| !i.attrs.std_context));
    // The misplaced attribute says where it belongs.
    let misplaced = b
        .diags
        .iter()
        .find(|d| d.message == "`@std_context` is not allowed on a function")
        .unwrap();
    assert!(
        format!("{misplaced:?}")
            .contains("`@std_context` applies to functions with an `@external` attribute"),
        "{misplaced:?}"
    );
}

#[test]
fn external_functions_take_no_bounds() {
    // One E0045 per bounded type parameter of a function with an
    // `@external`, with or without a body (§12.5, D-207, D-554). Unbounded
    // type parameters (D-186) and bounds on other functions are fine.
    let b = build(&[(
        "",
        "@external(native, \"a\")\npub fn a<T: Show, U, V: Show + Show>(t: T, u: U, v: V) -> Int\n\n@external(native, \"b\")\npub fn b<T: Show>(t: T) -> Int {\n  0\n}\n\n@external(native, \"c\")\npub fn c<T>(t: T) -> T\n\npub fn d<T: Show>(t: T) -> T {\n  t\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0045: the type parameter `T` of `a` has a bound, but `a` has an `@external` attribute",
            "E0045: the type parameter `T` of `b` has a bound, but `b` has an `@external` attribute",
            "E0045: the type parameter `V` of `a` has a bound, but `a` has an `@external` attribute",
        ]
    );
}

#[test]
fn missing_body() {
    let b = build(&[("", "pub fn now() -> Int\n")]);
    assert_eq!(b.messages(), ["E0046: the function `now` has no body"]);
}

#[test]
fn def_index_order_puts_members_after_their_parent() {
    let b = build(&[(
        "",
        "type Shape {\n  Circle(Float),\n  Square(Float),\n}\n\ntype Point { x: Int, y: Int }\n\ninterface Area {\n  fn area(Self) -> Float\n}\n\ntest \"t\" {\n  Nil\n}\n\nfn f() {\n  Nil\n}\n",
    )]);
    let app = b.resolved("app");
    let listed: Vec<(String, DefKind, Option<u32>)> = app
        .items
        .iter()
        .map(|i| {
            (
                b.globals.names.get(i.name).to_owned(),
                i.kind,
                i.parent.map(DefIndex::as_u32),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            ("Shape".to_owned(), DefKind::Type, None),
            ("Circle".to_owned(), DefKind::Ctor, Some(0)),
            ("Square".to_owned(), DefKind::Ctor, Some(0)),
            ("Point".to_owned(), DefKind::Type, None),
            ("Point".to_owned(), DefKind::Ctor, Some(3)),
            ("Area".to_owned(), DefKind::Interface, None),
            ("area".to_owned(), DefKind::InterfaceFn, Some(5)),
            ("t".to_owned(), DefKind::Test, None),
            ("f".to_owned(), DefKind::Fn, None),
        ]
    );
    assert!(b.diags.is_empty(), "{:?}", b.messages());
}

// ---- names, privacy, shadowing ---------------------------------------------

#[test]
fn unresolved_names() {
    let b = build(&[(
        "",
        "import std::io\nimport std::secret\n\nfn main() {\n  let message = \"hi\"\n  io::println(mesage)\n  io(\"x\")\n  list::length(\"x\")\n  secret::nope(\"x\")\n  io::read_all()\n  greet()\n}\n\nfn great() {\n  Nil\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0042: `io` is a module, not a value",
            "E0042: `list` is not imported",
            "E0042: cannot find value `greet` in this scope",
            "E0042: cannot find value `mesage` in this scope",
            "E0042: there is no value `nope` in module `std::secret`",
            "E0042: there is no value `read_all` in module `std::io`",
        ]
    );
    let helps: Vec<&str> = b
        .diags
        .iter()
        .flat_map(|d| d.helps.iter().map(|h| h.message.as_str()))
        .collect();
    assert!(helps.contains(&"add `import std::list` at the top of the file"));
    assert!(helps.contains(&"a binding with a similar name exists: `message`"));
    assert!(helps.contains(&"a value with a similar name exists: `great`"));
}

#[test]
fn privacy() {
    let b = build(&[
        (
            "",
            "import std::secret\nimport app::web\n\nfn main() {\n  secret::open()\n  secret::hidden()\n  secret::internal()\n  web::shared()\n  web::own()\n}\n",
        ),
        (
            "web",
            "fn shared() {\n  Nil\n}\n\npriv fn own() {\n  Nil\n}\n",
        ),
    ]);
    assert_eq!(
        b.messages(),
        [
            "E0043: `hidden` is private to module `std::secret`",
            "E0043: `internal` is visible only inside its package",
            "E0043: `own` is private to module `app::web`",
        ]
    );
}

#[test]
fn shadowing() {
    let b = build(&[(
        "",
        "import std::io\n\nfn main() {\n  let name = \"a\"\n  let name = \"b\"\n  let io = \"c\"\n  let helper = \"d\"\n  let _ = \"e\"\n  let _ = \"f\"\n}\n\nfn helper(main: String) {\n  Nil\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0044: `helper` is the name of a function in this module",
            "E0044: `io` is the name of an imported module",
            "E0044: `main` is the name of a function in this module",
            "E0044: `name` is already bound",
        ]
    );
    let d = b
        .diags
        .iter()
        .find(|d| d.message.contains("`name`"))
        .unwrap();
    assert_eq!(d.labels.len(), 2);
    assert!(d.helps[0].message.contains("name_2"));
}

#[test]
fn lets_and_locals() {
    let b = build(&[(
        "",
        "import std::io\n\nfn main() {\n  let greeting: String = \"hi\"\n  show(greeting)\n  show(greeting)\n}\n\nfn show(text: String) {\n  io::println(text)\n}\n",
    )]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    let app = b.resolved("app");
    let main = &app.bodies[&DefIndex::from_u32(0)];
    assert_eq!(main.locals.len(), 1);
    assert_eq!(main.params, []);
    let show = &app.bodies[&DefIndex::from_u32(1)];
    assert_eq!(show.params.len(), 1);
    let paths = b.body_paths("app", "main");
    let show_def = DefId::new(b.module("app"), DefIndex::from_u32(1));
    assert_eq!(
        paths,
        [
            ("String".to_owned(), Res::Def(builtin::STRING)),
            ("show".to_owned(), Res::Def(show_def)),
            ("greeting".to_owned(), Res::Local(LocalId::from_u32(0))),
            ("show".to_owned(), Res::Def(show_def)),
            ("greeting".to_owned(), Res::Local(LocalId::from_u32(0))),
        ]
    );
}

// ---- spelling of names (E0006, E0007) ---------------------------------------

#[test]
fn names_are_checked_once_where_bound() {
    let b = build(&[(
        "",
        "import std::io as myIo\n\n@async\nfn countAll(maxValue: Int) -> Int {\n  let go = maxValue\n  go + maxValue + countAll(go)\n}\n\ntype Point_2d {\n  Point_2d,\n}\n\nfn main() {\n  let p = Point_2d\n}\n\nfn other(text: String) {\n  return(text)\n}\n",
    )]);
    let naming: Vec<String> = b
        .messages()
        .into_iter()
        .filter(|m| m.starts_with("E0006") || m.starts_with("E0007"))
        .collect();
    assert_eq!(
        naming,
        [
            // Each binding and declaration once, its uses never, and not
            // the attribute `@async` (D-435).
            "E0006: name `countAll` is not in snake_case",
            "E0006: name `maxValue` is not in snake_case",
            "E0006: name `myIo` is not in snake_case",
            "E0006: type name `Point_2d` is not in PascalCase",
            "E0006: type name `Point_2d` is not in PascalCase",
            "E0007: `go` is a reserved word",
            // A reserved word that nothing binds, where it is used.
            "E0007: `return` is a reserved word",
        ]
    );
    let ret = b
        .diags
        .iter()
        .find(|d| d.message.contains("`return`"))
        .unwrap();
    assert_eq!(ret.labels[0].message, "reserved, cannot be used as a name");
    assert_eq!(
        ret.helps[0].message,
        "Cheby has no `return`; a function returns the value of its body"
    );
    assert!(!b.messages().iter().any(|m| m.contains("cannot find")));
}

// ---- what is left of the gate (D-451) -----------------------------------

#[test]
fn every_construct_resolves_without_e0038() {
    let b = build(&[(
        "",
        "import std::io\nimport std::io::{println}\n\ntype Unit {\n  Unit,\n}\n\ntype Pair = (String, String)\n\nconst greeting: String = \"hi\"\n\ninterface Named {\n  fn name(Self) -> String\n}\n\nfn main() {\n  let x = case 1 + 2 {\n    3 => [4]\n    _ => [5]\n  }\n  io::println(\"{x}\")\n  both(_, \"b\")\n  let f = main\n  let (a, b) = (1, 2)\n  let g = Named::name\n  println(greeting)\n}\n\nfn generic<T>(x: T) -> T {\n  x\n}\n\nfn both(a: String, b: String) {\n  Nil\n}\n\ntest \"unit\" {\n  assert Unit == Unit\n}\n",
    )]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    let main = DefId::new(b.module("app"), DefIndex::from_u32(6));
    let paths = b.body_paths("app", "main");
    assert!(paths.contains(&("main".to_owned(), Res::Def(main))));
    let named_fn = DefId::new(b.module("app"), DefIndex::from_u32(5));
    assert!(paths.contains(&("Named::name".to_owned(), Res::Def(named_fn))));
    let println = b.def("std::io", "println");
    assert!(paths.contains(&("println".to_owned(), Res::Def(println))));
}

#[test]
fn e0038_is_left_for_planned_std_modules() {
    let b = build(&[
        (
            "",
            "import std::list\nimport std::io\n\nfn main() {\n  io::println(list::reverse(\"x\"))\n}\n\nfn later() -> String {\n  lib::later()\n}\n",
        ),
        (
            "lib",
            "@external(js, \"./lib.mjs\", \"later\")\npub fn later() -> String\n",
        ),
    ]);
    // Paths through the failed import are silent; nothing else is E0038,
    // and a JS-only function is the cross-target check's (C12).
    assert_eq!(
        b.messages(),
        [
            "E0038: the module `std::list` is not supported yet",
            "E0042: `lib` is not imported",
        ]
    );
    let paths = b.body_paths("app", "main");
    assert!(
        paths.contains(&("list::reverse".to_owned(), Res::Err)),
        "{paths:?}"
    );
}

#[test]
fn functions_on_one_target_resolve() {
    let b = build(&[
        ("", "import app::lib\n\nfn main() {\n  lib::later()\n}\n"),
        ("lib", "@external(native, \"later\")\npub fn later()\n"),
    ]);
    // Whether `main` may call it is the cross-target check's (C12).
    assert_eq!(b.messages(), Vec::<String>::new());
    let later = b.def("app::lib", "later");
    assert_eq!(
        b.body_paths("app", "main"),
        [("lib::later".to_owned(), Res::Def(later))]
    );
}

// ---- prelude (B6) -----------------------------------------------------------

#[test]
fn unknown_types_get_hints() {
    let b = build(&[(
        "",
        "type Shape {\n  Circle { radius: Int },\n}\n\nfn grow(shape: Shape) -> Shape {\n  Circle { ..shape, radius: 2 }\n}\n\nfn largest<T: Compare>(a: T) -> T {\n  a\n}\n",
    )]);
    let helps: Vec<String> = b
        .diags
        .iter()
        .flat_map(|d| d.helps.iter().map(|h| h.message.clone()))
        .collect();
    assert_eq!(
        helps,
        [
            "`Circle` is a constructor of the type `Shape`; name the type here",
            "the operator interfaces are in `std::ops`: add `import std::ops` and write `ops::Compare`",
        ],
        "{:?}",
        b.messages()
    );
}

#[test]
fn kernel_types_are_reachable_only_through_their_home_modules() {
    let b = build(&[(
        "",
        "import std::map\nimport std::map::{Map as Dict}\nimport std::option::{Option, Some}\n\nfn size(m: map::Map<String, Int>) -> Int {\n  map::size(m)\n}\n\nfn same(m: Dict<String, Int>) -> Option<Int> {\n  Some(map::size(m))\n}\n\nfn missing(s: Set<Int>, m: Map<Int, Int>, b: Bytes) {\n  Nil\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0042: cannot find type `Bytes` in this scope",
            "E0042: cannot find type `Map` in this scope",
            "E0042: cannot find type `Set` in this scope",
        ]
    );
    let helps: Vec<&str> = b
        .diags
        .iter()
        .flat_map(|d| d.helps.iter().map(|h| h.message.as_str()))
        .collect();
    assert!(
        helps.contains(&"`Set` is not in the prelude: add `import std::set` and write `set::Set`")
    );
    let app = b.resolved("app");
    let sig: Vec<Res> = {
        let mut v: Vec<_> = app.sig_paths.values().copied().collect();
        v.sort_by_key(|r| format!("{r:?}"));
        v.dedup();
        v
    };
    assert!(sig.contains(&Res::Def(builtin::MAP)));
    assert!(sig.contains(&Res::Def(builtin::OPTION)));
    // The home module itself sees `Map` without declaring it.
    let std_map = b.resolved("std::map");
    assert!(
        std_map
            .sig_paths
            .values()
            .any(|r| *r == Res::Def(builtin::MAP))
    );
    let map = b.globals.names.intern("Map");
    assert_eq!(
        std_map
            .scope
            .get(cheby_iface::Namespace::Type, map)
            .unwrap()
            .source,
        ScopeSource::HomeReexport
    );
    assert!(app.scope.get(cheby_iface::Namespace::Type, map).is_none());
    // An item import of a home re-export binds the builtin item and does
    // not warn.
    let some = b.globals.names.intern("Some");
    let entry = app.scope.get(cheby_iface::Namespace::Value, some).unwrap();
    assert_eq!(entry.res, Res::Def(builtin::SOME));
    assert_eq!(entry.source, ScopeSource::Import(2));
}

#[test]
fn hiding_prelude_names_warns_per_namespace() {
    let b = build(&[
        (
            "",
            "import app::shop::{Order}\n\ntype Option {\n  Some(String),\n  Nothing,\n}\n\ntype Value {\n  String(String),\n  Bool(Bool),\n}\n\ninterface Show\n\nfn main() {\n  let o = Some(\"x\")\n}\n",
        ),
        ("shop", "type Order {\n  Placed,\n}\n"),
    ]);
    assert_eq!(
        b.messages(),
        [
            "W0007: `Option` hides the prelude type `Option`",
            // In `shop`, where it is declared, and in the root, which
            // imports it.
            "W0007: `Order` hides the prelude type `Order`",
            "W0007: `Order` hides the prelude type `Order`",
            "W0007: `Show` hides the prelude interface `Show`",
            "W0007: `Some` hides the prelude constructor `Some`",
        ]
    );
    let some = b
        .diags
        .iter()
        .find(|d| d.message.contains("`Some`"))
        .unwrap();
    assert_eq!(some.labels[0].message, "declared here");
    assert_eq!(
        some.notes,
        [
            "in this module, `Some` no longer means the prelude constructor, which stays reachable as `option::Some` after `import std::option`"
        ]
    );
    assert_eq!(
        some.helps[0].message,
        "if this is not intended, give the constructor another name"
    );
    let labels: Vec<&str> = b
        .diags
        .iter()
        .filter(|d| d.message.contains("`Order`"))
        .map(|d| d.labels[0].message.as_str())
        .collect();
    assert!(labels.contains(&"imported here") && labels.contains(&"declared here"));
    // The local `Some` wins in the module.
    let own_some = DefId::new(b.module("app"), DefIndex::from_u32(1));
    assert!(
        b.body_paths("app", "main")
            .contains(&("Some".to_owned(), Res::Def(own_some)))
    );
}

// ---- visibility (B5) --------------------------------------------------------

#[test]
fn visibility_through_item_imports() {
    let b = build(&[
        (
            "",
            "import app::web::{shared, own, Page}\nimport std::secret::{open, internal, Token, Internal}\n\nfn main() {\n  shared()\n  open()\n}\n",
        ),
        (
            "web",
            "fn shared() {\n  Nil\n}\n\npriv fn own() {\n  Nil\n}\n\npriv type Page {\n  Page,\n}\n",
        ),
    ]);
    // One error per name, also for a type and its constructor.
    assert_eq!(
        b.messages(),
        [
            "E0043: `Internal` is visible only inside its package",
            "E0043: `Page` is private to module `app::web`",
            "E0043: `internal` is visible only inside its package",
            "E0043: `own` is private to module `app::web`",
        ]
    );
}

#[test]
fn constructors_of_opaque_types() {
    let b = build(&[
        (
            "",
            "import app::shapes\nimport std::secret\nimport std::secret::{Token, Open, Pair}\n\nfn make() {\n  let t = secret::Token(\"a\")\n  let u = Token(\"b\")\n  let o = Open(\"c\")\n  let p = Pair { left: \"l\", right: \"r\" }\n  let s = shapes::Secret\n  let c = shapes::Circle\n}\n\nfn read(t: Token, o: secret::Open, p: Pair) {\n  let Token(x) = t\n  let secret::Open(y) = o\n  let q = Pair { ..p, left: \"l\" }\n  let r = secret::Pair { ..p, left: \"l\" }\n}\n",
        ),
        (
            "shapes",
            "priv type Hidden {\n  Secret,\n}\n\ntype Shape {\n  Circle,\n}\n",
        ),
    ]);
    assert_eq!(
        b.messages(),
        [
            "E0043: `Secret` is private to module `app::shapes`",
            "E0043: the constructor `Pair` is not visible outside its package",
            "E0043: the constructor `Token` is not visible outside its package",
            "E0043: the constructor `Token` is not visible outside its package",
            "E0043: the constructor `Token` is not visible outside its package",
            "E0043: the fields of `Pair` are not visible outside its package",
            "E0043: the fields of `Pair` are not visible outside its package",
        ]
    );
    let d = b
        .diags
        .iter()
        .find(|d| d.message.contains("constructor `Token`"))
        .unwrap();
    assert_eq!(
        d.notes,
        ["a `pub` type hides its constructors unless it is `pub exposed`"]
    );
    // The type name itself stays usable in signatures (§4.3.4).
    let token_ty = b.def("std::secret", "Token");
    assert!(
        b.resolved("app")
            .sig_paths
            .values()
            .any(|r| *r == Res::Def(token_ty))
    );
}

#[test]
fn interfaces_in_bounds_and_dyn_follow_item_visibility() {
    let b = build(&[(
        "",
        "import std::secret\n\nfn a<T: secret::Internal>(x: T) {\n  Nil\n}\n\nfn b(x: dyn secret::Internal) {\n  Nil\n}\n\nfn c<T: secret::Visible>(x: T, y: dyn secret::Visible) {\n  Nil\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0043: `Internal` is visible only inside its package",
            "E0043: `Internal` is visible only inside its package",
        ]
    );
}

// ---- items (B4) -------------------------------------------------------------

#[test]
fn duplicates_per_namespace() {
    let b = build(&[(
        "",
        "type Point { x: Int, y: Int }\n\ninterface Point\n\ntype Pair<T, T> = (T, T)\n\nfn total() {\n  Nil\n}\n\nconst total: Int = 1\n\ntype A {\n  Same,\n}\n\ntype B {\n  Same,\n}\n\ntype Pair = Int\n\ninterface Area {\n  fn area(Self)\n  fn area(Self)\n}\n\ninterface Other {\n  fn area(Self)\n}\n\ntest \"t\" {\n  Nil\n}\n\ntest \"t\" {\n  Nil\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0041: `Pair` is declared twice in this module",
            "E0041: `Point` is declared twice in this module",
            "E0041: `Same` is declared twice in this module",
            "E0041: `total` is declared twice in this module",
            "E0041: the interface declares `area` twice",
            "E0041: the type parameter `T` is declared twice",
            "E0041: two tests are named \"t\"",
        ]
    );
    // Every one has a secondary label on the first declaration.
    assert!(b.diags.iter().all(|d| d.labels.len() == 2), "{:?}", b.diags);
}

#[test]
fn interface_function_paths_follow_embedding() {
    let b = build(&[(
        "",
        "import std::secret\n\ninterface Area {\n  fn area(Self) -> Float\n}\n\ninterface Both: Area + Show {\n  fn both(Self)\n}\n\ninterface Far: secret::Wide\n\ntype Point {\n  Point,\n}\n\nfn main() {\n  let a = Both::area\n  let s = Both::show\n  let b = Both::both\n  let v = Far::visible\n  let w = secret::Wide::visible\n  let x = Show::show\n  let n = Both::nope\n  let p = Point::area\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0042: `Both` has no function `nope`",
            "E0042: `Point` is not an interface",
        ]
    );
    let app = b.module("app");
    let visible = b.def("std::secret", "visible");
    assert_eq!(
        b.body_paths("app", "main"),
        [
            (
                "Both::area".to_owned(),
                Res::Def(DefId::new(app, DefIndex::from_u32(1)))
            ),
            ("Both::show".to_owned(), Res::Def(builtin::SHOW_FN)),
            (
                "Both::both".to_owned(),
                Res::Def(DefId::new(app, DefIndex::from_u32(3)))
            ),
            ("Far::visible".to_owned(), Res::Def(visible)),
            ("secret::Wide::visible".to_owned(), Res::Def(visible)),
            ("Show::show".to_owned(), Res::Def(builtin::SHOW_FN)),
            ("Both::nope".to_owned(), Res::Err),
            ("Point::area".to_owned(), Res::Err),
        ]
    );
}

// ---- patterns and locals (B7) -------------------------------------------------

#[test]
fn alternatives_must_bind_the_same_names() {
    let b = build(&[(
        "",
        "fn f(pair: (Int, Int)) -> Int {\n  case pair {\n    (0, n) | (n, 0) => n\n    (a, 1) | (2, b) => 0\n    (c, d) | (d, c) | (c, _) => 0\n    _ => 0\n  }\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0062: this alternative does not bind `a`",
            "E0062: this alternative does not bind `b`",
            "E0062: this alternative does not bind `d`",
        ]
    );
    let d = b.diags.iter().find(|d| d.message.ends_with("`d`")).unwrap();
    assert_eq!(d.labels[0].message, "binds no `d`");
    assert_eq!(d.labels[1].message, "`d` is bound here");
    // Alternatives share their locals.
    let app = b.resolved("app");
    let body = &app.bodies[&DefIndex::from_u32(0)];
    let names: Vec<&str> = body
        .locals
        .iter()
        .map(|l| b.globals.names.get(l.name))
        .collect();
    assert_eq!(names, ["pair", "n", "a", "b", "c", "d"]);
}

#[test]
fn names_bound_twice_in_one_pattern() {
    let b = build(&[(
        "",
        "fn f(pair: (Int, Int), items: List<Int>) {\n  let (x, x) = pair\n  let [y, ..y] = items\n  let (z, _) as z = pair\n  let g = fn(a, a) { a }\n  case pair, pair {\n    (p, _), (p, _) => Nil\n    (q, _) | (_, q), _ => Nil\n  }\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0044: `a` is bound twice in this pattern",
            "E0044: `p` is bound twice in this pattern",
            "E0044: `x` is bound twice in this pattern",
            "E0044: `y` is bound twice in this pattern",
            "E0044: `z` is bound twice in this pattern",
        ]
    );
    let d = b.diags.iter().find(|d| d.message.contains("`x`")).unwrap();
    assert_eq!(d.labels.len(), 2);
    assert!(d.helps[0].message.contains("x_2"));
}

#[test]
fn shadowing_covers_every_binding_form() {
    let b = build(&[(
        "",
        "import std::io\nimport std::io::{println}\n\nfn f(text: String, items: List<String>) {\n  use println <- each(items)\n  let g = fn((text, _)) { Nil }\n  fn h(f) {\n    Nil\n  }\n  case items {\n    [first, ..items] => Nil\n    [_, ..] as io => Nil\n    _ => Nil\n  }\n}\n\nfn each(items: List<String>, callback: fn(String) -> Nil) {\n  Nil\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0044: `f` is the name of a function in this module",
            "E0044: `io` is the name of an imported module",
            "E0044: `items` is already bound",
            "E0044: `println` is imported into this module",
            "E0044: `text` is already bound",
        ]
    );
}

#[test]
fn naming_is_checked_at_every_binding_form() {
    let b = build(&[(
        "",
        "fn f(items: List<String>) {\n  use itemA <- each(items)\n  let g = fn((textB, _)) { Nil }\n  fn localC(argD) {\n    Nil\n  }\n  case items {\n    [firstE, ..restF] as allG => Nil\n    _ => Nil\n  }\n}\n\nfn each(items: List<String>, f: fn(String) -> Nil) {\n  Nil\n}\n",
    )]);
    let naming: Vec<String> = b
        .messages()
        .into_iter()
        .filter(|m| m.starts_with("E0006"))
        .collect();
    assert_eq!(naming.len(), 7, "{naming:?}");
}

#[test]
fn local_functions_see_only_earlier_local_functions() {
    let b = build(&[(
        "",
        "fn f() {\n  fn even(n) {\n    odd(n)\n  }\n  fn odd(n) {\n    even(n)\n  }\n  odd(1)\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        ["E0042: cannot find value `odd` in this scope"]
    );
    let d = &b.diags[0];
    assert_eq!(d.labels[1].message, "declared later");
    assert!(d.notes[0].contains("D-110"));
    let body = &b.resolved("app").bodies[&DefIndex::from_u32(0)];
    let odd = body
        .locals
        .iter_enumerated()
        .find(|(_, l)| b.globals.names.get(l.name) == "odd")
        .map(|(id, _)| id)
        .unwrap();
    let paths = b.body_paths("app", "f");
    assert_eq!(
        paths
            .iter()
            .filter(|(t, r)| t == "odd" && *r == Res::Local(odd))
            .count(),
        1
    );
}

// ---- bodies ------------------------------------------------------------------

#[test]
fn full_resolver_binds_patterns_closures_and_local_fns() {
    let b = build(&[(
        "",
        "import std::list\n\nfn count(numbers: List<Int>) -> Int {\n  fn step(acc, rest) {\n    case rest {\n      [] => acc\n      [first, ..tail] when first > 0 => step(acc + 1, tail)\n      [_, ..tail] => step(acc, tail)\n    }\n  }\n  let add = fn(a: Int, b) { a + b }\n  let Some(x) | Some(x) = Some(1)\n  use y <- list::each(numbers)\n  step(add(x, y), numbers)\n}\n",
    )]);
    // `list::each` is planned in this test std, hence E0038; nothing else.
    assert_eq!(b.codes(), ["E0038"]);
    let app = b.resolved("app");
    let body = &app.bodies[&DefIndex::from_u32(0)];
    let names: Vec<(&str, LocalKind)> = body
        .locals
        .iter()
        .map(|l| (b.globals.names.get(l.name), l.kind))
        .collect();
    assert_eq!(
        names,
        [
            ("numbers", LocalKind::Param),
            ("step", LocalKind::LocalFn),
            ("acc", LocalKind::ClosureParam),
            ("rest", LocalKind::ClosureParam),
            ("first", LocalKind::CaseArm),
            ("tail", LocalKind::CaseArm),
            ("tail", LocalKind::CaseArm),
            ("a", LocalKind::ClosureParam),
            ("b", LocalKind::ClosureParam),
            ("add", LocalKind::Let),
            ("x", LocalKind::Let),
            ("y", LocalKind::UseBinder),
        ]
    );
    let paths = b.body_paths("app", "count");
    let res_of = |text: &str| {
        paths
            .iter()
            .filter(|(t, _)| t == text)
            .map(|(_, r)| *r)
            .collect::<Vec<_>>()
    };
    // `step` inside its own body and after it is the local function.
    assert_eq!(res_of("step"), vec![Res::Local(LocalId::from_u32(1)); 3]);
    // Or-pattern alternatives share their binding.
    let x_bindings = body
        .bindings
        .values()
        .filter(|l| **l == LocalId::from_u32(10))
        .count();
    assert_eq!(x_bindings, 2);
    assert_eq!(res_of("Some"), vec![Res::Def(builtin::SOME); 3]);
    assert_eq!(res_of("Int"), vec![Res::Def(builtin::INT)]);
    assert_eq!(res_of("list::each"), [Res::Err]);
}

#[test]
fn full_resolver_reports_shadowing_in_nested_scopes() {
    let b = build(&[(
        "",
        "fn f(x: Int) -> Int {\n  let g = fn(x) { x }\n  case x {\n    y => y\n  }\n  case x {\n    y => y\n  }\n}\n",
    )]);
    assert_eq!(b.messages(), ["E0044: `x` is already bound"]);
}

// ---- every example ------------------------------------------------------------

/// Every valid example resolves with only E0038 diagnostics (D-451), for
/// std modules this test's small std does not ship, and E0042 for names
/// its small `std::option` and `std::map` lack, never another E0039-E0046,
/// E0062 or W0007 for a valid program and never a panic or an internal
/// error. Examples with phase-A errors (030) stop before resolution. With
/// the real std every example checks cleanly (`cheby_driver`'s
/// `examples_check_cleanly`).
#[test]
fn examples_resolve_with_only_unshipped_modules() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut dirs: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    assert!(dirs.len() >= 32);
    let mut checked = 0;
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
                let segments = f.path.segments();
                (segments[1..].join("::"), text)
            })
            .collect();
        let has_syntax_errors = texts.iter().enumerate().any(|(n, (_, text))| {
            let (_, errors) = parse(ModuleId::from_u32(u32::try_from(n).unwrap()), text);
            errors.iter().any(Diagnostic::is_error)
        });
        if has_syntax_errors || phase_a.iter().any(Diagnostic::is_error) {
            continue;
        }
        let local: Vec<(&str, &str)> = texts
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        let b = build_package(&manifest.name, &local);
        let unexpected: Vec<String> = b
            .diags
            .iter()
            .filter(|d| d.code != cheby_diag::codes::NOT_SUPPORTED_YET)
            .filter(|d| {
                !(d.code == cheby_diag::codes::UNRESOLVED_NAME
                    && ["`std::option`", "`std::map`"]
                        .iter()
                        .any(|m| d.message.ends_with(&format!("in module {m}"))))
            })
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect();
        assert!(unexpected.is_empty(), "{}: {unexpected:#?}", dir.display());
        if dir.ends_with("001_hello_world") {
            assert!(b.diags.is_empty(), "{:?}", b.messages());
        }
        checked += 1;
    }
    assert!(checked >= 30, "only {checked} examples checked");
}

#[test]
fn every_item_kind_has_a_node_view() {
    let b = build(&[("", "type T {\n  A,\n}\n\nfn f() {\n  Nil\n}\n")]);
    let app = b.resolved("app");
    let tree = &b.trees[&b.module("app")];
    for item in &app.items {
        let node = tree.node(item.node).unwrap();
        match item.kind {
            DefKind::Ctor => assert!(cheby_syntax::ast::Variant::cast(node).is_some()),
            _ => assert!(Item::cast(node).is_some()),
        }
    }
}

/// The span of `needle` in `text`, a file of `sources`.
fn span_of(sources: &mut SourceMap, text: &str, needle: &str) -> cheby_diag::Span {
    let file = sources.add_file("src/main.cheby", text).unwrap();
    let start = u32::try_from(text.find(needle).unwrap()).unwrap();
    let end = start
        .checked_add(u32::try_from(needle.len()).unwrap())
        .unwrap();
    cheby_diag::Span::new(file, cheby_diag::TextRange::new(start, end))
}

/// D-944: a name the runtime exports is E0055 outside std, with its own
/// note; the driver passes the runtime's names.
#[test]
fn e0055_runtime_export() {
    let text = "@external(native, \"cheby_io_println\")\nfn say(text: String)\n";
    let mut sources = SourceMap::new();
    let span = span_of(&mut sources, text, "\"cheby_io_println\"");
    let exported = |s: &str| s == "cheby_io_println";
    let diag = runtime_export_reserved("cheby_io_println", span, &exported).unwrap();
    assert_eq!(
        cheby_diag::render_all(&[diag], &sources),
        "\
error[E0055]: the native symbol `cheby_io_println` is reserved for the toolchain
 --> src/main.cheby:1:19
  |
1 | @external(native, \"cheby_io_println\")
  |                   ^^^^^^^^^^^^^^^^^^
  |
  = note: the Cheby runtime exports this symbol, so only the standard library may bind it

"
    );
}

/// A name the runtime does not export stays free (D-455), and a reserved
/// prefix is resolution's own E0055, never reported twice.
#[test]
fn e0055_only_exported_names_without_a_reserved_prefix() {
    let mut sources = SourceMap::new();
    let span = span_of(&mut sources, "\"x\"", "\"x\"");
    let everything = |_: &str| true;
    let nothing = |_: &str| false;
    assert!(runtime_export_reserved("cheby_sqlite_open", span, &nothing).is_none());
    assert!(runtime_export_reserved("cheby_rt_alloc", span, &everything).is_none());
    assert!(runtime_export_reserved("cb$fn$app$main", span, &everything).is_none());
    assert!(runtime_export_reserved("cheby_list_length", span, &everything).is_some());
}
