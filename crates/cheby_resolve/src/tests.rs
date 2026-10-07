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

use crate::body::Mode;
use crate::*;

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

/// A std module with a private and a package-visible item, for E0043.
const STD_SECRET: &str = "//! Test module.\n\npub fn open() {\n  Nil\n}\n\npriv fn hidden() {\n  Nil\n}\n\nfn internal() {\n  Nil\n}\n";

/// Standard-library modules the spec plans but the test std does not ship.
fn planned() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/stdlib");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            name.strip_suffix(".md").map(str::to_owned)
        })
        .filter(|n| n != "README" && n != "io")
        .collect();
    names.push("json::decode".to_owned());
    for tier2 in [
        "fiber", "channel", "selector", "duration", "random", "runtime", "file", "net", "http",
        "time", "process",
    ] {
        names.push(tier2.to_owned());
    }
    names.sort();
    names
}

const SHIPPED: &[(&str, &str)] = &[("io", STD_IO), ("secret", STD_SECRET)];

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
    build_package("app", local, Mode::Slice)
}

fn build_full(local: &[(&str, &str)]) -> Build {
    build_package("app", local, Mode::Full)
}

#[expect(
    clippy::too_many_lines,
    reason = "the whole front end of a build, in order"
)]
fn build_package(package: &str, local: &[(&str, &str)], mode: Mode) -> Build {
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
            std: StdCatalog {
                known: &std_refs,
                partial: &["io"],
            },
            package_targets: cheby_package::Targets::ALL,
        };
        let (module, d) = resolve_module_in(&input, mode).unwrap();
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
/// visibility, `exposed`, and which items are unsupported. The real one is
/// built by `cheby_types`.
fn fake_interface(
    module: &ResolvedModule,
    tree: &SyntaxTree,
    package: PackageId,
) -> ModuleInterface {
    let items: Vec<IfaceItem> = module
        .items
        .iter_enumerated()
        .map(|(index, item)| {
            let sig = match (item.status, item.kind) {
                (ItemStatus::Unsupported, _) => ItemSig::Unsupported,
                (_, DefKind::Interface) => ItemSig::Interface(cheby_iface::InterfaceDef {
                    embeds: Box::new([]),
                    fns: module
                        .items
                        .iter_enumerated()
                        .filter(|(_, f)| f.parent == Some(index))
                        .map(|(f, _)| f)
                        .collect(),
                }),
                (_, DefKind::Type) => {
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
                sig,
            }
        })
        .collect();
    ModuleInterface::new(module.module, package, IndexVec::from_raw(items).unwrap()).unwrap()
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
    assert_eq!(main.status, ItemStatus::Ok);
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
    assert_eq!(item.status, ItemStatus::Ok);
    assert!(std_io.bodies.is_empty());
    // `String` in the signature resolves to the prelude type.
    let strings: Vec<_> = std_io
        .sig_paths
        .values()
        .filter(|r| **r == Res::Def(builtin::STRING))
        .collect();
    assert_eq!(strings.len(), 4);
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
            "E0038: constants are not supported yet",
            // `b` exists only on native, in a package built for both.
            "E0038: functions that exist only on some of the package's targets are not supported yet",
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
fn valid_attributes_are_gated_in_the_slice() {
    let b = build(&[(
        "",
        "@target(native)\nfn a() {\n  Nil\n}\n\n@external(native, \"x\")\n@blocking\npub fn b()\n\n@external(js, \"./m.mjs\", \"c\")\n@async\n@deprecated(\"use b\")\npub fn c()\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0038: `@async` attributes are not supported yet",
            "E0038: `@blocking` attributes are not supported yet",
            "E0038: `@deprecated` attributes are not supported yet",
            "E0038: `@target` attributes are not supported yet",
            "E0038: functions without an `@external(native, …)` implementation are not supported yet",
        ]
    );
    let app = b.resolved("app");
    assert!(
        app.items
            .iter()
            .all(|i| i.status == ItemStatus::Unsupported)
    );
    let c = app.items.get(DefIndex::from_u32(2)).unwrap();
    assert!(c.attrs.is_async);
    assert_eq!(c.attrs.deprecated.as_deref(), Some("use b"));
    assert_eq!(
        c.attrs.targets,
        cheby_package::Targets::only(cheby_package::Target::Js)
    );
}

#[test]
fn missing_body() {
    let b = build(&[("", "pub fn now() -> Int\n")]);
    assert_eq!(b.messages(), ["E0046: the function `now` has no body"]);
}

#[test]
fn def_index_order_puts_members_after_their_parent() {
    let b = build_full(&[(
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
    assert!(app.items.iter().all(|i| i.status == ItemStatus::Ok));
    // In the slice, the type, interface and test are gated with their
    // members.
    let b = build(&[(
        "",
        "type Shape {\n  Circle,\n}\n\ninterface Area {\n  fn area(Self) -> Float\n}\n\nfn f() {\n  Nil\n}\n",
    )]);
    let app = b.resolved("app");
    let statuses: Vec<ItemStatus> = app.items.iter().map(|i| i.status).collect();
    assert_eq!(
        statuses,
        [
            ItemStatus::Unsupported,
            ItemStatus::Unsupported,
            ItemStatus::Unsupported,
            ItemStatus::Unsupported,
            ItemStatus::Ok
        ]
    );
}

// ---- names, privacy, shadowing ---------------------------------------------

#[test]
fn unresolved_names() {
    let b = build(&[(
        "",
        "import std::io\nimport std::secret\n\nfn main() {\n  let message = \"hi\"\n  io::println(mesage)\n  io(\"x\")\n  list::length(\"x\")\n  secret::nope(\"x\")\n  io::read_line()\n  greet()\n}\n\nfn great() {\n  Nil\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            // `std::io` is partly shipped, so a missing name is E0038.
            "E0038: `std::io::read_line` is not supported yet",
            "E0042: `io` is a module, not a value",
            "E0042: `list` is not imported",
            "E0042: cannot find value `greet` in this scope",
            "E0042: cannot find value `mesage` in this scope",
            "E0042: there is no value `nope` in module `std::secret`",
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

// ---- the gate ---------------------------------------------------------------

#[test]
fn gate_reports_outermost_constructs_once() {
    let b = build(&[(
        "",
        "import std::io\nimport std::io::{println}\n\nfn main() {\n  let x = case 1 + 2 {\n    3 => [4]\n    _ => [5]\n  }\n  io::println(\"{x}\")\n  both(_, \"b\")\n  let f = main\n  let (a, b) = (1, 2)\n}\n\nfn ok() {\n  io::println(\"fine\")\n}\n\nfn generic<T>(x: T) -> T {\n  x\n}\n\ntype Unit {\n  Unit,\n}\n\nfn both(a: String, b: String) {\n  Nil\n}\n",
    )]);
    assert_eq!(
        b.messages(),
        [
            "E0038: `case` expressions are not supported yet",
            "E0038: function captures are not supported yet",
            "E0038: interpolated strings are not supported yet",
            "E0038: item imports are not supported yet",
            "E0038: patterns other than a name or `_` in `let` are not supported yet",
            "E0038: tuples are not supported yet",
            "E0038: type declarations are not supported yet",
            "E0038: type parameters are not supported yet",
        ]
    );
    let app = b.resolved("app");
    let status = |name: &str| {
        app.items
            .iter()
            .find(|i| b.globals.names.get(i.name) == name)
            .unwrap()
            .status
    };
    // Hits in the body leave the signature in force (D-246); hits in the
    // declaration do not.
    assert_eq!(status("main"), ItemStatus::BodyUnsupported);
    assert_eq!(status("ok"), ItemStatus::Ok);
    assert_eq!(status("generic"), ItemStatus::Unsupported);
    assert!(app.bodies.contains_key(&DefIndex::from_u32(1)));
    assert!(!app.bodies.contains_key(&DefIndex::from_u32(0)));
}

#[test]
fn gate_function_values_and_uses_of_unsupported_items() {
    let b = build(&[
        (
            "",
            "import app::lib\n\nfn main() {\n  let f = helper\n}\n\nfn helper() {\n  Nil\n}\n\nfn other() {\n  lib::later()\n  later()\n  lib::generic()\n}\n\nfn later() {\n  let x = 1\n}\n",
        ),
        (
            "lib",
            "pub fn later() {\n  let x = 1\n}\n\npub fn generic<T>() {\n  Nil\n}\n",
        ),
    ]);
    assert_eq!(
        b.messages(),
        [
            "E0038: functions used as values are not supported yet",
            "E0038: numeric literals are not supported yet",
            "E0038: numeric literals are not supported yet",
            "E0038: type parameters are not supported yet",
        ]
    );
    // A function whose body alone is unsupported keeps its signature, so
    // calls of it resolve (D-246); uses of an item whose declaration is
    // unsupported are silent errors.
    let later = |module: &str| {
        DefId::new(
            b.module(module),
            DefIndex::from_u32(match module {
                "app" => 3,
                _ => 0,
            }),
        )
    };
    assert_eq!(
        b.body_paths("app", "other"),
        [
            ("lib::later".to_owned(), Res::Def(later("app::lib"))),
            ("later".to_owned(), Res::Def(later("app"))),
            ("lib::generic".to_owned(), Res::Err),
        ]
    );
    let main_status = b
        .resolved("app")
        .items
        .iter()
        .find(|i| b.globals.names.get(i.name) == "main")
        .map(|i| i.status);
    assert_eq!(main_status, Some(ItemStatus::BodyUnsupported));
}

// ---- the full resolver (gate off) ---------------------------------------------

#[test]
fn full_resolver_binds_patterns_closures_and_local_fns() {
    let b = build_full(&[(
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
    let b = build_full(&[(
        "",
        "fn f(x: Int) -> Int {\n  let g = fn(x) { x }\n  case x {\n    y => y\n  }\n  case x {\n    y => y\n  }\n}\n",
    )]);
    assert_eq!(b.messages(), ["E0044: `x` is already bound"]);
}

// ---- every example ------------------------------------------------------------

/// Every valid example resolves with only E0038 diagnostics
/// (D-451), never an E0039-E0046 for a valid program and never a panic or
/// an internal error. Examples with phase-A errors (030) stop before
/// resolution.
#[test]
fn examples_resolve_with_only_unsupported_constructs() {
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
        let b = build_package(&manifest.name, &local, Mode::Slice);
        let unexpected: Vec<String> = b
            .diags
            .iter()
            .filter(|d| d.code != cheby_diag::codes::NOT_SUPPORTED_YET)
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect();
        assert!(unexpected.is_empty(), "{}: {unexpected:#?}", dir.display());
        if dir.ends_with("001_hello_world") {
            assert!(b.diags.is_empty(), "{:?}", b.messages());
        } else {
            assert!(
                !b.resolved.is_empty() || !b.diags.is_empty(),
                "{}",
                dir.display()
            );
        }
        checked += 1;
    }
    assert!(checked >= 30, "only {checked} examples checked");
}

/// The same examples with the gates off: no false errors from the full
/// resolver (B7), apart from E0038 for planned std modules.
#[test]
fn examples_resolve_cleanly_with_the_full_resolver() {
    let mut checked = 0;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut dirs: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
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
                (f.path.segments()[1..].join("::"), text)
            })
            .collect();
        let broken = texts.iter().any(|(_, text)| {
            parse(ModuleId::from_u32(0), text)
                .1
                .iter()
                .any(Diagnostic::is_error)
        });
        if broken || phase_a.iter().any(Diagnostic::is_error) {
            continue;
        }
        let local: Vec<(&str, &str)> = texts
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        let b = build_package(&manifest.name, &local, Mode::Full);
        checked += 1;
        let unexpected: Vec<String> = b
            .diags
            .iter()
            .filter(|d| d.code != cheby_diag::codes::NOT_SUPPORTED_YET)
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect();
        assert!(unexpected.is_empty(), "{}: {unexpected:#?}", dir.display());
    }
    assert!(checked >= 30, "only {checked} examples checked");
}

#[test]
fn every_item_kind_has_a_node_view() {
    let b = build_full(&[("", "type T {\n  A,\n}\n\nfn f() {\n  Nil\n}\n")]);
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
