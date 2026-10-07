//! Signature and body checking over small multi-module builds, the THIR
//! of 001, and the check of every example.

use std::path::Path;
use std::sync::Arc;

use cheby_diag::{Diagnostic, FileId, SourceMap, codes};
use cheby_iface::{
    DefId, DepView, Globals, Ice, InterfaceSet, ItemSig, ModuleId, ModuleInfo, ModuleInterface,
    ModuleTable, Namespace, PackageId, Ty, builtin,
};
use cheby_package::ModulePath;
use cheby_package::graph::{Graph, reachable, topological_order};
use cheby_resolve::{
    ImportRequest, ImportTarget, ModuleLookup, ResolveInput, StdCatalog, collect_imports,
    resolve_import_targets, resolve_module,
};
use cheby_syntax::SyntaxTree;
use rustc_hash::FxHashMap;

use crate::thir::{self, Callee, EntryInfo, ExprKind, Lit, Pat, Stmt};
use crate::{TypesInput, build_interface, check_bodies};

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

const SHIPPED: &[(&str, &str)] = &[("io", STD_IO)];

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

struct Lookup<'a> {
    package: &'a str,
    modules: &'a mut ModuleTable,
    planned: &'a [String],
    loaded: Vec<(ModuleId, &'static str)>,
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
        if let Some(&(_, source)) = SHIPPED.iter().find(|(p, _)| *p == rest_text) {
            let id = self.modules.add(ModuleInfo {
                path: module_path,
                package: PackageId::STD,
                file: None,
            })?;
            self.loaded.push((id, source));
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

struct Build {
    globals: Globals,
    modules: ModuleTable,
    set: InterfaceSet,
    thir: FxHashMap<ModuleId, thir::Module>,
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
            .map(|d| {
                let labels: Vec<&str> = d.labels.iter().map(|l| l.message.as_str()).collect();
                format!("{}: {} {labels:?}", d.code, d.message)
            })
            .collect();
        messages.sort();
        messages
    }

    fn module(&self, path: &str) -> ModuleId {
        let mut segments = path.split("::");
        let first = ModulePath::root(segments.next().unwrap());
        self.modules
            .find(&segments.fold(first, |p, s| p.child(s)))
            .unwrap()
    }

    fn iface(&self, path: &str) -> &ModuleInterface {
        self.set.get(self.module(path)).unwrap()
    }

    fn def(&self, path: &str, name: &str) -> DefId {
        let module = self.module(path);
        let name = self.globals.names.intern(name);
        let index = self
            .set
            .get(module)
            .unwrap()
            .lookup(Namespace::Value, name)
            .unwrap();
        DefId::new(module, index)
    }

    fn fn_sig(&self, path: &str, name: &str) -> &ItemSig {
        let def = self.def(path, name);
        &self
            .set
            .get(def.module)
            .unwrap()
            .item(def.index)
            .unwrap()
            .sig
    }

    fn thir_fn(&self, path: &str, name: &str) -> Option<&thir::FnDef> {
        let def = self.def(path, name);
        self.thir[&def.module].fns.iter().find(|f| f.def == def)
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

/// The whole front end of a build, as the driver runs it (G1):
/// imports, graph, then per module in order resolve, `build_interface`
/// and `check_bodies`.
#[expect(
    clippy::too_many_lines,
    reason = "the whole front end of a build, in order"
)]
fn build_package(package: &str, local: &[(&str, &str)]) -> Build {
    let globals = Globals::new();
    let mut modules = ModuleTable::new();
    let mut trees = FxHashMap::default();
    for (path, text) in local {
        let module_path = path
            .split("::")
            .filter(|s| !s.is_empty())
            .fold(ModulePath::root(package), |p, s| p.child(s));
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
    let mut diags = Vec::new();
    let mut requests: FxHashMap<ModuleId, Vec<ImportRequest>> = FxHashMap::default();
    let mut targets: FxHashMap<ModuleId, Vec<ImportTarget>> = FxHashMap::default();
    let mut next = 1_u32;
    while usize::try_from(next).unwrap() < modules.len() {
        let id = ModuleId::from_u32(next);
        next = next.checked_add(1).unwrap();
        let info = modules.get(id).unwrap().clone();
        let reqs = collect_imports(&trees[&id], &globals.names);
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
    let mut graph = Graph::new(modules.len());
    for (&id, ts) in &targets {
        for t in ts {
            if let Some(to) = t.module() {
                graph.add_edge(id.as_u32(), to.as_u32());
            }
        }
    }
    let order = topological_order(&graph).expect("no import cycles in type tests");
    let std_modules: Vec<String> = planned
        .iter()
        .cloned()
        .chain(SHIPPED.iter().map(|(p, _)| (*p).to_owned()))
        .collect();
    let std_refs: Vec<&str> = std_modules.iter().map(String::as_str).collect();
    let mut set = InterfaceSet::new(builtin::interface(&globals));
    let mut thir = FxHashMap::default();
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
        let (module, d) = check_bodies(&types, &iface).unwrap();
        diags.extend(d);
        thir.insert(id, module);
        set.insert(Arc::new(iface)).unwrap();
    }
    Build {
        globals,
        modules,
        set,
        thir,
        diags,
    }
}

fn main_with(body: &str) -> String {
    format!("//! Test.\n\nimport std::io\n\nfn main() {{\n{body}\n}}\n")
}

// ---- 001 ---------------------------------------------------------------

const HELLO: &str = "//! Prints a greeting to standard output.\n\nimport std::io\n\nfn main() {\n  io::println(\"Hello, world!\")\n}\n";

#[test]
fn hello_world_checks_into_thir() {
    let b = build(&[("", HELLO)]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    let println = b.def("std::io", "println");
    // The interface of std::io: `println(text: String)` returns `Nil`.
    let ItemSig::Fn(sig) = b.fn_sig("std::io", "println") else {
        panic!("println has no function signature");
    };
    assert_eq!(sig.params.len(), 1);
    assert_eq!(b.globals.names.get(sig.params[0].name), "text");
    assert_eq!(sig.params[0].ty, Ty::STRING);
    assert_eq!(sig.ret, Ty::NIL);
    assert!(sig.generics.is_empty());
    assert!(b.iface("std::io").hash.is_some());
    // Its THIR: no body, a native external.
    let external = b.thir_fn("std::io", "println").unwrap();
    assert!(external.body.is_none());
    assert_eq!(
        external.native_external.as_ref().map(|e| &*e.symbol),
        Some("cheby_io_println")
    );
    assert_eq!(external.entry, None);
    assert_eq!(b.thir[&b.module("std::io")].fns.len(), 4);
    // `main`: an entry returning `Nil`, whose body calls `io::println`.
    let main = b.thir_fn("app", "main").unwrap();
    assert_eq!(main.entry, Some(EntryInfo::Nil));
    assert!(main.native_external.is_none());
    let body = main.body.as_ref().unwrap();
    assert_eq!(body.ret, Ty::NIL);
    assert_eq!(body.params, []);
    let block = body.expr(body.value).unwrap();
    assert_eq!(block.ty, Ty::NIL);
    let ExprKind::Block { stmts, tail } = &block.kind else {
        panic!("the body is not a block: {block:?}");
    };
    assert!(stmts.is_empty());
    let call = body.expr(tail.unwrap()).unwrap();
    assert_eq!(call.ty, Ty::NIL);
    let ExprKind::Call { callee, args } = &call.kind else {
        panic!("not a call: {call:?}");
    };
    assert_eq!(
        callee,
        &Callee::Fn {
            def: println,
            ty_args: Box::new([]),
            dicts: Box::new([]),
        }
    );
    assert_eq!(args.len(), 1);
    let arg = body.expr(args[0]).unwrap();
    assert_eq!(arg.kind, ExprKind::Lit(Lit::Str("Hello, world!".into())));
    assert_eq!(arg.ty, Ty::STRING);
    assert_eq!(body.exprs.len(), 3);
}

#[test]
fn ui_hello_world_checks() {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ui/01-overview/hello_world.cheby"),
    )
    .unwrap();
    let b = build(&[("", &text)]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    assert_eq!(
        b.thir_fn("app", "main").unwrap().entry,
        Some(EntryInfo::Nil)
    );
}

// ---- signatures (C2) ---------------------------------------------------

#[test]
fn signatures_lower_and_omitted_returns_are_nil() {
    let src = "//! Test.\n\nfn shout(text: String, other: String) -> String {\n  text\n}\n\npub fn ignore(text: String) {\n  Nil\n}\n\npriv fn nothing() -> Nil {\n  Nil\n}\n";
    let b = build(&[("", src)]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    let ItemSig::Fn(shout) = b.fn_sig("app", "shout") else {
        panic!()
    };
    assert_eq!(
        shout.params.iter().map(|p| p.ty).collect::<Vec<_>>(),
        [Ty::STRING, Ty::STRING]
    );
    assert_eq!(shout.ret, Ty::STRING);
    let ItemSig::Fn(ignore) = b.fn_sig("app", "ignore") else {
        panic!()
    };
    assert_eq!(ignore.ret, Ty::NIL);
    let ItemSig::Fn(nothing) = b.fn_sig("app", "nothing") else {
        panic!()
    };
    assert_eq!(nothing.ret, Ty::NIL);
    // Not entries.
    assert_eq!(b.thir_fn("app", "shout").unwrap().entry, None);
    let body = b.thir_fn("app", "shout").unwrap().body.as_ref().unwrap();
    assert_eq!(body.params.len(), 2);
    assert_eq!(body.locals.len(), 2);
    assert!(body.locals.iter().all(|l| l.ty == Ty::STRING));
}

#[test]
fn the_slice_gates_other_types_in_signatures() {
    let src = "//! Test.\n\nfn count(text: String) -> Int {\n  todo\n}\n\nfn wrap(items: List<String>, flag: Bool) {\n  Nil\n}\n\nfn main() {\n  Nil\n}\n";
    let b = build(&[("", src)]);
    // `todo` makes `count` unsupported in the resolver already; the
    // signature gate reports each other written type once.
    let messages = b.messages();
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("E0038: the type `List<String>` is not supported yet")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("E0038: the type `Bool` is not supported yet")),
        "{messages:?}"
    );
    assert!(b.codes().iter().all(|c| c == "E0038"), "{messages:?}");
    assert_eq!(b.fn_sig("app", "wrap"), &ItemSig::Unsupported);
    assert!(b.thir_fn("app", "wrap").is_none());
    assert!(b.thir_fn("app", "main").is_some());
}

#[test]
fn written_types_are_checked() {
    let src = "//! Test.\n\nfn a(x: List) {\n  Nil\n}\n\nfn b(x: String<Int>) {\n  Nil\n}\n\nfn c(x: Show) {\n  Nil\n}\n\nfn d(x: dyn String) {\n  Nil\n}\n";
    let b = build(&[("", src)]);
    let messages = b.messages();
    assert_eq!(
        b.codes(),
        ["E0051", "E0051", "E0052", "E0052"],
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("`List` takes 1 type argument but 0 were given"))
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("`String` takes no type arguments"))
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("`Show` is an interface, not a type"))
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("`String` is not an interface"))
    );
    // `dyn Show<Int>` is E0023 in the syntax layer.
    for name in ["a", "b", "c", "d"] {
        assert_eq!(b.fn_sig("app", name), &ItemSig::Error, "{name}");
        assert!(b.thir_fn("app", name).is_none());
    }
}

#[test]
fn main_must_be_an_entry_point() {
    let with_param = "//! Test.\n\nfn main(name: String) {\n  Nil\n}\n";
    let b = build(&[("", with_param)]);
    assert_eq!(b.codes(), ["E0050"], "{:?}", b.messages());
    assert_eq!(b.fn_sig("app", "main"), &ItemSig::Error);
    assert!(b.thir_fn("app", "main").is_none());
    // An unsupported return type is still the wrong signature for `main`.
    let returns_int = "//! Test.\n\nfn main() -> Int {\n  todo\n}\n";
    let b = build(&[("", returns_int)]);
    assert!(b.codes().contains(&"E0038".to_owned()));
    let returns_string = "//! Test.\n\nfn main() -> String {\n  \"x\"\n}\n";
    let b = build(&[("", returns_string)]);
    assert_eq!(b.codes(), ["E0050"], "{:?}", b.messages());
    // Every module's `main` is checked, of any visibility (D-137, D-163).
    let tool = "//! Test.\n\npriv fn main(x: String) {\n  Nil\n}\n";
    let ok = "//! Test.\n\nimport app::tool\n\npub fn main() {\n  Nil\n}\n";
    let b = build(&[("", ok), ("tool", tool)]);
    assert_eq!(b.codes(), ["E0050"], "{:?}", b.messages());
    assert_eq!(
        b.thir_fn("app", "main").unwrap().entry,
        Some(EntryInfo::Nil)
    );
    // `main` returning `Result` is an entry point, but not in the slice.
    let result = "//! Test.\n\nfn main() -> Result<Nil, String> {\n  todo\n}\n";
    let b = build(&[("", result)]);
    assert!(
        !b.codes().contains(&"E0050".to_owned()),
        "{:?}",
        b.messages()
    );
}

// ---- bodies (C3) -------------------------------------------------------

#[test]
fn lets_locals_and_local_calls() {
    let src = "//! Test.\n\nimport std::io\n\nfn greet(name: String) {\n  io::print(\"hello, \")\n  io::println(name)\n}\n\nfn main() {\n  let who = \"world\"\n  let again: String = who\n  let _ = greet(again)\n  let done: Nil = greet(who)\n}\n";
    let b = build(&[("", src)]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    let greet = b.def("app", "greet");
    let main = b.thir_fn("app", "main").unwrap();
    let body = main.body.as_ref().unwrap();
    let ExprKind::Block { stmts, tail } = &body.expr(body.value).unwrap().kind else {
        panic!()
    };
    // A block ending in `let` has the value `Nil`.
    assert_eq!(*tail, None);
    assert_eq!(stmts.len(), 4);
    let names: Vec<(&str, Ty)> = body
        .locals
        .iter()
        .map(|l| (b.globals.names.get(l.name), l.ty))
        .collect();
    assert_eq!(
        names,
        [
            ("who", Ty::STRING),
            ("again", Ty::STRING),
            ("done", Ty::NIL)
        ]
    );
    let Stmt::Let {
        pat: Pat::Wild,
        init,
    } = &stmts[2]
    else {
        panic!("{:?}", stmts[2]);
    };
    let ExprKind::Call {
        callee: Callee::Fn { def, .. },
        args,
    } = &body.expr(*init).unwrap().kind
    else {
        panic!()
    };
    assert_eq!(*def, greet);
    let arg = body.expr(args[0]).unwrap();
    assert!(matches!(arg.kind, ExprKind::Local(_)));
    assert_eq!(arg.ty, Ty::STRING);
    let greet_body = b.thir_fn("app", "greet").unwrap().body.as_ref().unwrap();
    let ExprKind::Block { stmts, tail } = &greet_body.expr(greet_body.value).unwrap().kind else {
        panic!()
    };
    assert!(matches!(stmts[..], [Stmt::Expr(_)]));
    assert!(tail.is_some());
}

#[test]
fn nil_is_a_value() {
    let src = main_with("  let n = Nil\n  let m: Nil = n\n  m");
    let b = build(&[("", &src)]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    let body = b.thir_fn("app", "main").unwrap().body.as_ref().unwrap();
    let nil = body
        .exprs
        .iter()
        .find(|e| matches!(e.kind, ExprKind::Ctor { .. }))
        .unwrap();
    assert_eq!(
        nil.kind,
        ExprKind::Ctor {
            ctor: builtin::NIL,
            ty_args: Box::new([]),
        }
    );
    assert_eq!(nil.ty, Ty::NIL);
}

#[test]
fn mismatches_are_e0047() {
    let cases = [
        main_with("  io::println(Nil)"),
        main_with("  let x: String = Nil"),
        main_with("  let x: Nil = \"text\""),
        "//! Test.\n\nfn name() -> String {\n  Nil\n}\n".to_owned(),
        "//! Test.\n\nfn name() -> String {\n  let x = \"a\"\n}\n".to_owned(),
        main_with("  \"a value\""),
    ];
    for src in &cases {
        let b = build(&[("", src)]);
        assert_eq!(b.codes(), ["E0047"], "{src}: {:?}", b.messages());
        assert_eq!(b.diags[0].message, "mismatched types");
        // A body with an error is not returned.
        assert!(b.thir[&b.module("app")].fns.is_empty(), "{src}");
    }
    let b = build(&[("", &cases[0])]);
    let labels: Vec<&str> = b.diags[0]
        .labels
        .iter()
        .map(|l| l.message.as_str())
        .collect();
    assert_eq!(
        labels,
        [
            "expected `String`, found `Nil`",
            "arguments to this function are incorrect"
        ]
    );
    let b = build(&[("", &cases[5])]);
    let labels: Vec<&str> = b.diags[0]
        .labels
        .iter()
        .map(|l| l.message.as_str())
        .collect();
    assert_eq!(
        labels,
        [
            "expected `Nil`, found `String`",
            "the return type is omitted, so it is `Nil`"
        ]
    );
}

#[test]
fn argument_counts_are_e0048() {
    for (body, expected) in [
        (
            "  io::println(\"a\", \"b\")",
            "this function takes 1 argument but 2 arguments were supplied",
        ),
        (
            "  io::println()",
            "this function takes 1 argument but 0 arguments were supplied",
        ),
    ] {
        let b = build(&[("", &main_with(body))]);
        assert_eq!(b.codes(), ["E0048"], "{:?}", b.messages());
        assert_eq!(b.diags[0].message, expected);
        // The declaration in std::io is the secondary label.
        assert_eq!(b.diags[0].labels.len(), 2);
    }
    let src =
        "//! Test.\n\nfn two(a: String, b: String) {\n  Nil\n}\n\nfn main() {\n  two(\"a\")\n}\n";
    let b = build(&[("", src)]);
    assert_eq!(b.codes(), ["E0048"], "{:?}", b.messages());
    assert_eq!(
        b.diags[0].message,
        "this function takes 2 arguments but 1 argument was supplied"
    );
}

#[test]
fn calling_a_value_that_is_not_a_function_is_e0049() {
    let b = build(&[(
        "",
        &main_with("  let greeting = \"hello\"\n  greeting(\"world\")"),
    )]);
    assert_eq!(b.codes(), ["E0049"], "{:?}", b.messages());
    assert_eq!(b.diags[0].message, "expected a function, found `String`");
    let b = build(&[("", &main_with("  Nil()"))]);
    assert_eq!(b.codes(), ["E0049"], "{:?}", b.messages());
    assert_eq!(b.diags[0].message, "expected a function, found `Nil`");
}

#[test]
fn the_slice_gates_other_values() {
    for (body, message) in [
        (
            "  io::println(True)",
            "values of type `Bool` are not supported yet",
        ),
        (
            "  let x = None",
            "values of type `Option<_>` are not supported yet",
        ),
        (
            "  let x = Some(\"a\")",
            "values of type `Option<_>` are not supported yet",
        ),
        (
            "  let x = Some(True)",
            "values of type `Option<_>` are not supported yet",
        ),
        (
            "  let x: Order = Less",
            "the type `Order` is not supported yet",
        ),
    ] {
        let b = build(&[("", &main_with(body))]);
        assert_eq!(b.codes()[0], "E0038", "{body}: {:?}", b.messages());
        assert!(
            b.codes().iter().all(|c| c == "E0038"),
            "{body}: {:?}",
            b.messages()
        );
        assert_eq!(b.diags[0].message, message, "{body}");
        assert!(b.thir_fn("app", "main").is_none());
    }
    // The annotation and the value are separate constructs.
    let b = build(&[("", &main_with("  let x: Bool = True"))]);
    assert_eq!(b.codes(), ["E0038", "E0038"], "{:?}", b.messages());
}

#[test]
fn uses_of_rejected_items_are_silent() {
    // `count` is rejected by the signature gate (its body's `todo` is a
    // separate E0038); calling it reports nothing more, and `main` is not
    // returned.
    let src = "//! Test.\n\nfn count(text: String) -> Bool {\n  todo\n}\n\nfn main() {\n  let n = count(\"x\")\n}\n";
    let b = build(&[("", src)]);
    assert_eq!(b.codes(), ["E0038", "E0038"], "{:?}", b.messages());
    assert!(b.thir_fn("app", "main").is_none());
    // A function whose signature has an error still has its body checked.
    let src = "//! Test.\n\nimport std::io\n\nfn f(x: List) {\n  io::println(Nil)\n}\n";
    let b = build(&[("", src)]);
    assert_eq!(b.codes(), ["E0047", "E0051"], "{:?}", b.messages());
    assert!(b.thir_fn("app", "f").is_none());
}

#[test]
fn cross_module_calls_use_the_dependency_interface() {
    let util = "//! Test.\n\nimport std::io\n\npub fn shout(text: String) -> String {\n  text\n}\n\nfn helper() {\n  io::println(\"x\")\n}\n";
    let main = "//! Test.\n\nimport std::io\nimport app::util\n\nfn main() {\n  io::println(util::shout(\"hi\"))\n  util::helper()\n}\n";
    let b = build(&[("", main), ("util", util)]);
    assert!(b.diags.is_empty(), "{:?}", b.messages());
    let shout = b.def("app::util", "shout");
    let body = b.thir_fn("app", "main").unwrap().body.as_ref().unwrap();
    assert!(body.exprs.iter().any(|e| matches!(
        &e.kind,
        ExprKind::Call { callee: Callee::Fn { def, .. }, .. } if *def == shout
    ) && e.ty == Ty::STRING));
}

#[test]
fn interface_hash_ignores_bodies_and_private_items() {
    let a = "//! Test.\n\npub fn shout(text: String) -> String {\n  text\n}\n";
    let b_src = "//! Test.\n\npriv fn helper() {\n  Nil\n}\n\npub fn shout(text: String) -> String {\n  let copy = text\n  copy\n}\n";
    let c_src = "//! Test.\n\npub fn shout(text: String) -> Nil {\n  Nil\n}\n";
    let ha = build(&[("", a)]).iface("app").hash;
    let hb = build(&[("", b_src)]).iface("app").hash;
    let hc = build(&[("", c_src)]).iface("app").hash;
    assert!(ha.is_some());
    assert_eq!(ha, hb);
    assert_ne!(ha, hc);
}

/// An unsupported construct in a body is a body matter: the function's
/// interface, its hash and what dependents are told are those of the same
/// function without it (D-246, §13.12.3, B8).
#[test]
fn unsupported_bodies_do_not_change_interfaces() {
    let lib = |body: &str| {
        format!(
            "//! Test.\n\nimport std::io\n\npub fn show(text: String) {{\n{body}  io::println(text)\n}}\n"
        )
    };
    let main = "//! Test.\n\nimport app::lib\n\nfn main() {\n  lib::show(Nil)\n}\n";
    let plain = build(&[("", main), ("lib", &lib(""))]);
    let gated = build(&[("", main), ("lib", &lib("  let n = 1\n"))]);
    assert_eq!(plain.iface("app::lib").hash, gated.iface("app::lib").hash);
    assert!(matches!(gated.fn_sig("app::lib", "show"), ItemSig::Fn(_)));
    let dependent = |b: &Build| -> Vec<String> {
        b.messages()
            .into_iter()
            .filter(|m| !m.starts_with("E0038"))
            .collect()
    };
    assert_eq!(plain.codes(), ["E0047"], "{:?}", plain.messages());
    assert_eq!(gated.codes(), ["E0038", "E0047"], "{:?}", gated.messages());
    assert_eq!(dependent(&plain), dependent(&gated));
    // The body itself is not checked or returned.
    assert!(gated.thir_fn("app::lib", "show").is_none());
}

#[test]
fn checking_is_deterministic() {
    let src = "//! Test.\n\nimport std::io\n\nfn greet(name: String) {\n  io::println(name)\n}\n\nfn main() {\n  let who = \"world\"\n  greet(who)\n  io::println(who)\n}\n";
    let one = build(&[("", src)]);
    let two = build(&[("", src)]);
    assert_eq!(one.thir[&one.module("app")], two.thir[&two.module("app")]);
}

// ---- every example -----------------------------------------------------

/// Every valid example checks with only E0038 (D-451): never a
/// false E0039-E0052, never a panic or an internal error; 001 checks
/// cleanly. Examples with phase-A errors (030) stop before resolution.
#[test]
fn examples_check_with_only_unsupported_constructs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut dirs: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
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
                (f.path.segments()[1..].join("::"), text)
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
            .filter(|d| d.code != codes::NOT_SUPPORTED_YET)
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect();
        assert!(unexpected.is_empty(), "{}: {unexpected:#?}", dir.display());
        if dir.ends_with("001_hello_world") {
            assert!(b.diags.is_empty(), "{:?}", b.messages());
            let main = b.thir_fn(&manifest.name, "main").unwrap();
            assert_eq!(main.entry, Some(EntryInfo::Nil));
        }
        checked += 1;
    }
    assert!(checked >= 30, "only {checked} examples checked");
}
