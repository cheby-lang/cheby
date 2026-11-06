//! The functions' display names in module records (D-1041, N7b): the rule
//! per kind of code, the names a unit compiled alone gives its functions,
//! a large frame's inner body, and the names in the record's bytes.

use cheby_iface::{DefId, DefIndex, ModuleId};
use cheby_mir::{DefName, EntryKind, FnKey, MirModule, UnitKind};

use crate::modrec::{DisplayName, display_name};
use crate::record_tests::{code, code_model_unit, located_unit, named};
use crate::{CodegenOptions, UnitCx, compile_unit, isa, record::RecordingModule};

fn def(index: u32) -> DefId {
    DefId::new(ModuleId::from_u32(1), DefIndex::from_u32(index))
}

/// `unit` with its defs named in `module` as `items`, in def order.
fn renamed(mut unit: MirModule, module: &str, items: &[&str]) -> MirModule {
    unit.names = items
        .iter()
        .enumerate()
        .map(|(n, item)| DefName {
            def: def(u32::try_from(n).unwrap()),
            module: module.into(),
            item: (*item).into(),
        })
        .collect();
    unit
}

fn name(unit: &MirModule, key: FnKey) -> Option<String> {
    display_name(unit, key).unwrap()
}

/// What `unit` defines, compiled alone with `opts`.
fn defined(unit: &MirModule, opts: &CodegenOptions) -> Vec<crate::DefinedCode> {
    let mut module = RecordingModule::new(isa::host_isa().unwrap());
    compile_unit(
        &mut module,
        unit,
        &UnitCx {
            named_outside: &named,
        },
        opts,
    )
    .unwrap()
    .defined
}

/// D-1041's names in a code unit: a function and a constant by module
/// path and name, a closure under its owner with `{fn#<n>}`, a
/// continuation under its owner, built-in tuple code as `<builtin>`, and
/// no name for value shims, `dyn` forwarders, trampolines and entry
/// thunks.
#[test]
fn modrec_names_code_unit_kinds() {
    let unit = renamed(located_unit(code()), "demo", &["check", "double_all"]);
    assert_eq!(
        name(&unit, FnKey::Item(def(0))).as_deref(),
        Some("demo::check")
    );
    assert_eq!(
        name(&unit, FnKey::ConstInit(def(1))).as_deref(),
        Some("demo::double_all")
    );
    assert_eq!(
        name(
            &unit,
            FnKey::Closure {
                owner: def(1),
                index: 0
            }
        )
        .as_deref(),
        Some("demo::double_all::{fn#0}")
    );
    assert_eq!(
        name(
            &unit,
            FnKey::Closure {
                owner: def(1),
                index: 3
            }
        )
        .as_deref(),
        Some("demo::double_all::{fn#3}")
    );
    assert_eq!(
        name(
            &unit,
            FnKey::Continuation {
                owner: def(0),
                index: 1
            }
        )
        .as_deref(),
        Some("demo::check")
    );
    assert_eq!(
        name(
            &unit,
            FnKey::TupleDictFn {
                iface_fn: def(0),
                arity: 2
            }
        )
        .as_deref(),
        Some("<builtin>")
    );
    for unnamed in [
        FnKey::ValueShim(def(0)),
        FnKey::DynForward {
            iface_fn: def(0),
            dyn_iface: def(1),
        },
        FnKey::Trampoline(1),
        FnKey::Entry(EntryKind::Main(def(0))),
        FnKey::Entry(EntryKind::Test(def(0))),
        FnKey::Entry(EntryKind::Repl(2)),
    ] {
        assert_eq!(name(&unit, unnamed), None, "{unnamed:?}");
    }
}

/// A std function keeps its std path, also where a user unit instantiates
/// it (its `DefName` is std's).
#[test]
fn modrec_names_std_path() {
    let mut unit = renamed(located_unit(code()), "demo", &["main", "fold"]);
    if let Some(fold) = unit.names.get_mut(1) {
        fold.module = "std::list".into();
    }
    assert_eq!(
        name(&unit, FnKey::Item(def(1))).as_deref(),
        Some("std::list::fold")
    );
    assert_eq!(
        name(
            &unit,
            FnKey::Closure {
                owner: def(1),
                index: 0
            }
        )
        .as_deref(),
        Some("std::list::fold::{fn#0}")
    );
}

/// A test is named as `cheby test` prints it, `demo "adds"`, and its
/// closures under that name; a REPL entry's body is `<repl:N>`, its
/// closures `<repl:N>::{fn#n}`, and a definition made in the REPL is its
/// item path in its entry's module.
#[test]
fn modrec_names_tests_and_repl() {
    let tests = renamed(
        located_unit(UnitKind::Tests(ModuleId::from_u32(1))),
        "demo",
        &["adds", "two words"],
    );
    assert_eq!(
        name(&tests, FnKey::Test(def(0))).as_deref(),
        Some("demo \"adds\"")
    );
    assert_eq!(
        name(&tests, FnKey::Test(def(1))).as_deref(),
        Some("demo \"two words\"")
    );
    assert_eq!(
        name(
            &tests,
            FnKey::Closure {
                owner: def(0),
                index: 0
            }
        )
        .as_deref(),
        Some("demo \"adds\"::{fn#0}")
    );
    let entry = renamed(
        located_unit(UnitKind::Tests(ModuleId::from_u32(1))),
        "<repl:3>",
        &["<repl>"],
    );
    assert_eq!(
        name(&entry, FnKey::Test(def(0))).as_deref(),
        Some("<repl:3>")
    );
    assert_eq!(
        name(
            &entry,
            FnKey::Closure {
                owner: def(0),
                index: 1
            }
        )
        .as_deref(),
        Some("<repl:3>::{fn#1}")
    );
    let item = renamed(located_unit(code()), "<repl:3>", &["f"]);
    assert_eq!(
        name(&item, FnKey::Item(def(0))).as_deref(),
        Some("<repl:3>::f")
    );
}

/// A module that a REPL `:reload` loaded again (`fib<r1>`, `app::util<r12>`)
/// and a REPL entry checked again (`<repl:2:1>`) keep the names source code
/// gives them: the review's R-1 (D-1041).
#[test]
fn modrec_names_after_reload() {
    let reloaded = renamed(located_unit(code()), "fib<r1>", &["boom"]);
    assert_eq!(
        name(&reloaded, FnKey::Item(def(0))).as_deref(),
        Some("fib::boom")
    );
    assert_eq!(
        name(
            &reloaded,
            FnKey::Closure {
                owner: def(0),
                index: 2
            }
        )
        .as_deref(),
        Some("fib::boom::{fn#2}")
    );
    let nested = renamed(located_unit(code()), "app::util<r12>", &["go"]);
    assert_eq!(
        name(&nested, FnKey::Item(def(0))).as_deref(),
        Some("app::util::go")
    );
    let again = renamed(located_unit(code()), "<repl:2:1>", &["f"]);
    assert_eq!(
        name(&again, FnKey::Item(def(0))).as_deref(),
        Some("<repl:2>::f")
    );
    let entry = renamed(
        located_unit(UnitKind::Tests(ModuleId::from_u32(1))),
        "<repl:4:3>",
        &["<repl>"],
    );
    assert_eq!(
        name(&entry, FnKey::Test(def(0))).as_deref(),
        Some("<repl:4>")
    );
    let tests = renamed(
        located_unit(UnitKind::Tests(ModuleId::from_u32(1))),
        "fib<r3>",
        &["adds"],
    );
    assert_eq!(
        name(&tests, FnKey::Test(def(0))).as_deref(),
        Some("fib \"adds\"")
    );
}

/// A unit compiled alone names every function it defines by its MIR key,
/// in definition order, and a `@blocking` shim not at all.
#[test]
fn modrec_names_compiled_unit() {
    let names: Vec<Option<String>> = defined(&located_unit(code()), &CodegenOptions::default())
        .into_iter()
        .map(|code| code.name.map(|n| n.text))
        .collect();
    assert_eq!(
        names,
        [
            Some("app::sub::f0".to_owned()),
            Some("app::sub::f1".to_owned())
        ]
    );
    let with_shim = defined(&code_model_unit(), &CodegenOptions::default());
    let unnamed = with_shim.iter().filter(|code| code.name.is_none()).count();
    assert_eq!(unnamed, 1, "{with_shim:#?}");
    for code in &with_shim {
        if let Some(DisplayName { text, large_body }) = &code.name {
            assert!(text == "app::f" || text == "app::g", "{text}");
            assert!(!large_body);
        }
    }
}

/// A large frame's inner body `cb$big$…` follows its outer function and
/// is named as it, marked as an inner body (D-679); the outer is not.
#[test]
fn modrec_names_large_frame_body() {
    let opts = CodegenOptions {
        large_frame_threshold: 0,
        ..CodegenOptions::default()
    };
    let codes = defined(&located_unit(code()), &opts);
    let mut inner = 0;
    for pair in codes.windows(2) {
        if let [outer, body] = pair
            && body.name.as_ref().is_some_and(|n| n.large_body)
        {
            let outer = outer.name.as_ref().unwrap();
            assert!(!outer.large_body);
            assert_eq!(body.name.as_ref().unwrap().text, outer.text);
            inner += 1;
        }
    }
    assert!(inner > 0, "{codes:#?}");
    assert!(
        codes
            .first()
            .and_then(|c| c.name.as_ref())
            .is_some_and(|n| !n.large_body)
    );
}
