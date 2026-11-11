//! Tests of the base of std (A §7, D-1372, D-1369): the index against the
//! interfaces that checked, names and ranges, type heads, and the
//! materialized sources.
//!
//! Nothing here runs generated code, so every platform runs them.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use cheby_iface::{
    DefId, DefIndex, DefKind, Globals, ModuleId, PackageId, Ty, builtin, structure_visible,
};

use super::std_base::materialize_std_at;
use crate::analysis::{StdBase, StdIndex, Symbol, SymbolNs, SymbolTarget, TypeHead};
use crate::std_lib;

/// One base for every test of this file: building it checks all of std.
fn base() -> &'static Arc<StdBase> {
    static BASE: OnceLock<Arc<StdBase>> = OnceLock::new();
    BASE.get_or_init(|| StdBase::build(Arc::new(Globals::new())).unwrap())
}

fn index() -> &'static StdIndex {
    base().index()
}

/// Whether a package may name the constructor or interface function `at`
/// of `iface`, a public one, as a member of something it may see into: a
/// constructor of a type whose structure it sees, a function of an
/// interface it may name.
fn member_seen(iface: &cheby_iface::ModuleInterface, module: ModuleId, at: DefIndex) -> bool {
    let Some(item) = iface.item(at) else {
        return false;
    };
    let parent = item.parent.and_then(|p| Some((p, iface.item(p)?)));
    let seen = parent.is_some_and(|(p, parent)| match item.kind {
        DefKind::Ctor => structure_visible(
            parent,
            module,
            PackageId::STD,
            ModuleId::BUILTIN,
            PackageId::LOCAL,
        ),
        _ => iface.visible_from(p, PackageId::LOCAL),
    });
    seen && !iface.is_hidden(at) && iface.is_current(at)
}

/// The std module at `path` under `std`, such as `list`.
fn std_path(path: &str) -> cheby_package::ModulePath {
    path.split("::")
        .fold(cheby_package::ModulePath::root("std"), |p, s| p.child(s))
}

#[test]
fn index_holds_every_pub_item() {
    let snapshot = base().snapshot();
    let names = &snapshot.globals().names;
    let mut expected = 0_usize;
    let (mut ctors, mut iface_fns) = (0_usize, 0_usize);
    for module in snapshot.all_modules() {
        let info = snapshot.table().get(module.id()).unwrap();
        if info.package != PackageId::STD || module.id() == ModuleId::BUILTIN {
            continue;
        }
        let iface = module.interface().unwrap();
        let path = info.path.to_string();
        for (at, item) in iface.items.iter_enumerated() {
            let def = DefId::new(module.id(), at);
            let key = snapshot.key_of(def);
            let found = key.as_ref().and_then(|key| index().get(key));
            let public = item.kind != DefKind::Test && iface.visible_from(at, PackageId::LOCAL);
            match item.kind {
                // Items with a namespace and no parent: exactly the `pub` ones.
                DefKind::Fn
                | DefKind::Const
                | DefKind::Type
                | DefKind::Alias
                | DefKind::Interface => {
                    let name = names.get(item.name);
                    assert_eq!(found.is_some(), public, "{path}::{name}");
                    if let Some(found) = found {
                        expected += 1;
                        assert_eq!(found.module, path, "{path}::{name}");
                        assert_eq!(found.kind, item.kind, "{path}::{name}");
                        assert_eq!(
                            found.file,
                            std_lib::display_path(path.strip_prefix("std::").unwrap())
                        );
                        assert_eq!(snapshot.find(&found.key), Some(SymbolTarget::Item(def)));
                    }
                }
                DefKind::Test => assert!(found.is_none(), "a test of {path}"),
                DefKind::Ctor | DefKind::InterfaceFn => {
                    let name = names.get(item.name);
                    if !public {
                        assert!(found.is_none(), "{path}::{name}");
                    }
                    if public && member_seen(iface, module.id(), at) {
                        assert!(found.is_some(), "{path}::{name} is missing");
                        if item.kind == DefKind::Ctor {
                            ctors += 1;
                        } else {
                            iface_fns += 1;
                        }
                    }
                    if found.is_some() {
                        expected += 1;
                    }
                }
            }
        }
        for homed in builtin::homed_at(&info.path) {
            let key = snapshot.key_of(homed).unwrap();
            let found = index()
                .get(&key)
                .unwrap_or_else(|| panic!("{key:?} at {path}"));
            assert_eq!(found.module, path);
            assert!(found.name_range.is_none());
        }
    }
    let homed = index()
        .items
        .iter()
        .filter(|item| item.key.module == builtin_path())
        .count();
    assert_eq!(index().items.len(), expected + homed);
    assert_ne!(ctors, 0, "no public constructor of std is checked");
    assert_ne!(iface_fns, 0, "no interface function of std is checked");
    // A few that features depend on.
    let map = index()
        .get(&Symbol::item(std_path("list"), SymbolNs::Value, "map"))
        .unwrap();
    assert_eq!(map.kind, DefKind::Fn);
    assert!(
        map.signature.starts_with("pub fn map<"),
        "{}",
        map.signature
    );
    assert!(map.doc.is_some());
    let some = index()
        .get(&Symbol::item(builtin_path(), SymbolNs::Value, "Some"))
        .unwrap();
    assert_eq!(some.module, "std::option");
    let list = index()
        .get(&Symbol::item(builtin_path(), SymbolNs::Type, "List"))
        .unwrap();
    assert_eq!(list.module, "std::list");
    assert!(list.doc.is_some(), "a builtin has its home module's docs");
    assert!(index().named("println").any(|i| i.module == "std::io"));
    assert!(
        index()
            .prefix("printl")
            .all(|i| i.key.text().starts_with("printl"))
    );
    assert!(index().prefix("printl").count() >= 1);
}

/// The builtin module's path, which builtin symbols carry.
fn builtin_path() -> cheby_package::ModulePath {
    base()
        .snapshot()
        .table()
        .get(ModuleId::BUILTIN)
        .unwrap()
        .path
        .clone()
}

#[test]
fn index_ranges_are_names() {
    let mut ranged = 0;
    for item in &index().items {
        let Some(range) = item.name_range else {
            assert_eq!(item.key.module, builtin_path(), "{:?}", item.key);
            continue;
        };
        let module = std_lib::STD_MODULES
            .iter()
            .find(|m| std_lib::display_path(m.path) == item.file)
            .unwrap_or_else(|| panic!("{} is not shipped", item.file));
        assert_eq!(
            range.slice(module.source),
            Some(item.key.text()),
            "{:?} in {}",
            item.key,
            item.file
        );
        ranged += 1;
    }
    assert!(ranged > 100, "{ranged}");
    // Std constants carry their initializer, exposed types their fields.
    let constants: Vec<_> = index()
        .items
        .iter()
        .filter(|i| i.kind == DefKind::Const)
        .collect();
    assert_ne!(constants.len(), 0);
    for constant in constants {
        let text = constant.initializer.as_deref().unwrap();
        assert!(!text.trim().is_empty() && text.trim() == text, "{text:?}");
    }
    let request = index()
        .get(&Symbol::item(std_path("http"), SymbolNs::Type, "Request"))
        .unwrap();
    let fields: Vec<&str> = request.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        fields,
        ["method", "path", "segments", "query", "headers", "body"]
    );
    // No std field has a doc comment yet; any that gains one shows here.
    for field in index().items.iter().flat_map(|i| &i.fields) {
        assert!(field.doc.as_deref().is_none_or(|d| !d.trim().is_empty()));
    }
    for module in &index().modules {
        assert_eq!(
            module.path.rsplit("::").next(),
            Some(module.binding.as_str())
        );
        assert!(module.doc.is_some(), "{} has a module doc", module.path);
    }
}

#[test]
fn head_of_matches_index() {
    let snapshot = base().snapshot();
    let types = &snapshot.globals().types;
    let list_of_int = types.named(builtin::LIST, [Ty::INT]);
    let head = snapshot.head_of(list_of_int);
    assert_eq!(head, TypeHead::Builtin("List".to_owned()));
    let map = Symbol::item(std_path("list"), SymbolNs::Value, "map");
    assert!(
        index()
            .accepting(&head)
            .any(|(item, at)| item.key == map && at == 0)
    );
    // Every function's heads are what `head_of` gives for its signature.
    for module in snapshot.all_modules() {
        let Some(iface) = module.interface() else {
            continue;
        };
        for (at, item) in iface.items.iter_enumerated() {
            let cheby_iface::ItemSig::Fn(sig) = &item.sig else {
                continue;
            };
            let Some(key) = snapshot.key_of(DefId::new(module.id(), at)) else {
                continue;
            };
            let Some(found) = index().get(&key) else {
                continue;
            };
            let heads: Vec<_> = sig.params.iter().map(|p| snapshot.head_of(p.ty)).collect();
            assert_eq!(found.params, heads, "{key:?}");
            assert_eq!(found.ret, snapshot.head_of(sig.ret), "{key:?}");
            for (position, head) in heads.iter().enumerate() {
                let position = u8::try_from(position).unwrap();
                assert!(
                    index()
                        .accepting(head)
                        .any(|(item, p)| item.key == key && p == position)
                        || heads.iter().take(usize::from(position)).any(|h| h == head),
                    "{key:?} at {position}"
                );
            }
        }
    }
    let tuple = types.intern(cheby_iface::TyKind::Tuple(Box::new([Ty::INT, Ty::INT])));
    assert_eq!(snapshot.head_of(tuple), TypeHead::Tuple(2));
    assert_eq!(snapshot.head_of(Ty::ERROR), TypeHead::Unknown);
}

#[test]
fn no_unshipped_module() {
    let shipped: Vec<String> = std_lib::STD_MODULES
        .iter()
        .map(|m| format!("std::{}", m.path))
        .collect();
    let indexed: Vec<&str> = index().modules.iter().map(|m| m.path.as_str()).collect();
    let mut sorted = shipped.clone();
    sorted.sort();
    assert_eq!(
        indexed, sorted,
        "every shipped module, sorted, and no other"
    );
    let unshipped = std_lib::STAGED_STD_MODULES
        .iter()
        .map(|m| m.path)
        .chain(std_lib::PLANNED_STD_MODULES.iter().copied())
        .chain(std_lib::PROBE_PLANNED_STD_MODULES.iter().copied())
        .map(|p| format!("std::{p}"));
    for path in unshipped {
        assert!(index().module(&path).is_none(), "{path}");
        assert!(index().items.iter().all(|i| i.module != path), "{path}");
    }
    for item in &index().items {
        assert!(shipped.contains(&item.module), "{}", item.module);
    }
}

#[test]
fn base_keeps_trees_and_facts_of_every_module() {
    let snapshot = base().snapshot();
    for module in snapshot
        .all_modules()
        .filter(|m| m.id() != ModuleId::BUILTIN)
    {
        assert!(module.tree().is_some(), "{module:?}");
        assert!(module.resolved().is_some(), "{module:?}");
        let facts = module.facts().unwrap();
        assert!(facts.lost.is_empty() && facts.lost_passes.is_empty());
    }
    assert!(snapshot.lost().is_empty(), "{:?}", snapshot.lost());
    assert!(!snapshot.silent_drop());
    // Two bases have distinct generations.
    let other = StdBase::build(Arc::new(Globals::new())).unwrap();
    assert_ne!(other.snapshot().id(), snapshot.id());
    assert_eq!(other.index().items, index().items);
}

/// `Retain::STD` has no containment: an internal error in the run fails
/// the build, as it fails a batch run.
#[test]
fn base_fails_on_an_internal_error() {
    let extra = "\nfn ice_by_test() -> Int {\n  1\n}\n";
    let error = StdBase::build_with(Arc::new(Globals::new()), extra).unwrap_err();
    assert!(
        error.message().contains("a body ended by a test"),
        "{}",
        error.message()
    );
    assert!(StdBase::build_with(Arc::new(Globals::new()), "").is_ok());
}

/// A constant of a module whose tree a package run dropped shows its
/// initializer as written, read again from its text: how hover shows a
/// dependency's constant, which no child evaluates (D-1491). Std stands
/// in for a dependency, the one other package a run has before M5.
#[test]
#[cfg_attr(windows, ignore = "runs the front end (D-545, D-641)")]
fn const_initializer_reads_dropped_trees() {
    let dir = std::env::temp_dir().join(format!("cheby_std_base_init_{}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let root = std::path::absolute(&dir).unwrap();
    std::fs::write(
        root.join("cheby.toml"),
        "name = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.cheby"),
        "import std::int\n\npub const local: Int = 1 +\n  2\n\nfn main() {\n  assert \
         int::max_safe > local\n}\n",
    )
    .unwrap();
    let mut session = crate::analysis::Session::new(crate::analysis::SessionConfig {
        root: root.clone(),
        kind: crate::analysis::PackageKind::Package,
        target: None,
        globals: Arc::new(Globals::new()),
    });
    let live = session
        .analyze_live(&crate::analysis::Overlay::default())
        .unwrap();
    let snapshot = live.snapshot.unwrap();
    let item = |module: cheby_package::ModulePath, name: &str| match snapshot.find(&Symbol::item(
        module,
        SymbolNs::Value,
        name,
    )) {
        Some(SymbolTarget::Item(def)) => def,
        other => panic!("{name}: {other:?}"),
    };
    let max_safe = item(std_path("int"), "max_safe");
    assert!(snapshot.module(max_safe.module).unwrap().tree().is_none());
    assert_eq!(
        snapshot.const_initializer(max_safe).as_deref(),
        Some("9_007_199_254_740_991")
    );
    let app = cheby_package::ModulePath::root("app");
    assert_eq!(
        snapshot
            .const_initializer(item(app.clone(), "local"))
            .as_deref(),
        Some("1 +\n  2")
    );
    assert_eq!(snapshot.const_initializer(item(app, "main")), None);
    drop(std::fs::remove_dir_all(&dir));
}

#[test]
fn materialized_std_is_read_only_and_kept() {
    struct Removed(PathBuf);
    impl Drop for Removed {
        fn drop(&mut self) {
            let _ = make_writable(&self.0);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = Removed(std::env::temp_dir().join(format!("cheby_std_base_{}", std::process::id())));
    materialize_std_at(&dir.0).unwrap();
    for module in std_lib::STD_MODULES {
        let path = dir.0.join(std_lib::display_path(module.path));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), module.source);
        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
    }
    // A file changed behind the server's back is written again; the rest
    // stay as they are.
    let first = std_lib::STD_MODULES.first().unwrap();
    let changed = dir.0.join(std_lib::display_path(first.path));
    let mut permissions = std::fs::metadata(&changed).unwrap().permissions();
    #[expect(
        clippy::permissions_set_readonly_false,
        reason = "the test edits the file"
    )]
    permissions.set_readonly(false);
    std::fs::set_permissions(&changed, permissions).unwrap();
    std::fs::write(&changed, "changed").unwrap();
    materialize_std_at(&dir.0).unwrap();
    assert_eq!(std::fs::read_to_string(&changed).unwrap(), first.source);
    let leftovers = walk(&dir.0)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "tmp"))
        .count();
    assert_eq!(leftovers, 0);
}

/// Every file under `dir`.
fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// Clears the read-only flag of every file under `dir`, so that it can be
/// removed on Windows.
fn make_writable(dir: &std::path::Path) -> std::io::Result<()> {
    for path in walk(dir) {
        let mut permissions = std::fs::metadata(&path)?.permissions();
        #[expect(
            clippy::permissions_set_readonly_false,
            reason = "the test removes the file"
        )]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions)?;
    }
    Ok(())
}
