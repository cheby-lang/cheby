//! D-917's units compiled alone on a real program: 001's code units and
//! its entries unit, lowered as `cheby run` lowers them, each compiled
//! alone into a recording module (D-914), define only their own unit's
//! symbols and import what other units define.

use std::path::{Path, PathBuf};

use cheby_codegen_clif::record::{CodeRecord, RecordingModule, SymbolLinkage};
use cheby_codegen_clif::{CodegenOptions, UnitCx, compile_unit, isa, mangle};
use cheby_iface::{DefId, Vis};
use cheby_mir::{FnKey, MirModule, MirOptions, UnitKind};

use super::*;

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/examples")
        .join(name)
}

/// 001's units, lowered from the front end as `cheby run` lowers them.
fn units_of_001() -> (frontend::Frontend, Vec<MirModule>) {
    let mut out = CheckOutput::default();
    let (front, package) = front_end(&example("001_hello_world"), frontend::Mode::Build, &mut out)
        .unwrap()
        .unwrap();
    assert!(!out.has_errors(), "{}", out.render());
    let module = front.local_module(&ModulePath::root(&package)).unwrap();
    let main = front.main_of(module).unwrap();
    let lowered = backend::lower(
        &front,
        &out.sources,
        &backend::Roots::Main(main),
        MirOptions::default(),
    )
    .unwrap();
    (front, lowered.units)
}

/// `unit` compiled alone into N2's recording module (D-914).
fn record(unit: &MirModule, ucx: &UnitCx<'_>) -> CodeRecord {
    let mut module = RecordingModule::new(isa::host_isa().unwrap());
    compile_unit(&mut module, unit, ucx, &CodegenOptions::default()).unwrap();
    module.finish().unwrap()
}

fn linkage(record: &CodeRecord, symbol: &str) -> SymbolLinkage {
    record
        .symbol(symbol)
        .unwrap_or_else(|| panic!("`{symbol}` is not in the record"))
        .linkage
}

/// The symbols of `unit`'s own items, functions and slots, which only
/// its own record may define.
fn own_items(unit: &MirModule) -> Vec<String> {
    let mut own = Vec::new();
    for function in &unit.functions {
        if let FnKey::Item(def) = function.key {
            let name = unit.name(def).unwrap();
            own.push(mangle::item(mangle::Kind::Fn, name));
            own.push(mangle::slot(name, function.borrow));
        }
    }
    own
}

/// Each unit of 001 compiled alone defines its own items and imports the
/// items of other units: `hello_world` imports `std::io::println`'s slot,
/// which `std::io` defines hidden; the entries unit defines its thunk
/// local and imports `main`'s slot, which `hello_world` defines hidden;
/// every import word is local in the unit that uses it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn alone_001_units() {
    let (front, units) = units_of_001();
    // What the front end says (D-1110): an item other
    // units may name is not `priv`; `main`, tests and constant
    // initializers are package-visible or named by the entries unit.
    let named_outside = |def: DefId| {
        front
            .set
            .get(def.module)
            .and_then(|iface| iface.item(def.index))
            .is_none_or(|item| item.vis != Vis::Priv)
    };
    let ucx = UnitCx {
        named_outside: &named_outside,
    };
    let code = |path: &str| {
        units
            .iter()
            .find(|u| matches!(u.unit, UnitKind::Code(_)) && &*u.path == path)
            .unwrap_or_else(|| panic!("no code unit `{path}`"))
    };
    let entries = units.iter().find(|u| u.unit == UnitKind::Entries).unwrap();
    let hello = record(code("hello_world"), &ucx);
    let io = record(code("std::io"), &ucx);
    let thunks = record(entries, &ucx);

    assert_eq!(
        linkage(&hello, "cb$slot$std$io$println"),
        SymbolLinkage::Import
    );
    assert!(!hello.defines("cb$slot$std$io$println"));
    assert!(io.defines("cb$slot$std$io$println"));
    assert_eq!(
        linkage(&io, "cb$slot$std$io$println"),
        SymbolLinkage::Hidden
    );
    assert!(thunks.defines("cb$entry$main$hello_world"));
    assert_eq!(
        linkage(&thunks, "cb$entry$main$hello_world"),
        SymbolLinkage::Local
    );
    assert_eq!(
        linkage(&thunks, "cb$slot$hello_world$main"),
        SymbolLinkage::Import
    );
    assert!(hello.defines("cb$slot$hello_world$main"));
    assert_eq!(
        linkage(&hello, "cb$slot$hello_world$main"),
        SymbolLinkage::Hidden
    );
    assert_eq!(
        linkage(&hello, "cb$fn$hello_world$main"),
        SymbolLinkage::Hidden
    );

    check_every_unit(&units, &ucx);

    // D-264: the same unit compiled again gives the same record.
    assert_eq!(record(code("hello_world"), &ucx).encode(), hello.encode());
}

/// Every unit, std's others included: its own items' functions and slots
/// are defined by it alone, every other unit's are imported and defined
/// hidden by their owner, import words are local, and nothing is
/// exported.
fn check_every_unit(units: &[MirModule], ucx: &UnitCx<'_>) {
    let mut records = Vec::new();
    for unit in units {
        records.push((unit, record(unit, ucx)));
    }
    let mut owner = std::collections::BTreeMap::new();
    for (unit, record) in &records {
        for symbol in own_items(unit) {
            assert!(
                record.defines(&symbol),
                "`{}` defines `{symbol}`",
                unit.path
            );
            assert!(
                owner.insert(symbol.clone(), unit.path.clone()).is_none(),
                "`{symbol}` has two owners"
            );
        }
    }
    for (unit, record) in &records {
        for symbol in &record.symbols {
            let name = symbol.name.as_str();
            if name.starts_with("cb$imp$") {
                assert_eq!(symbol.linkage, SymbolLinkage::Local, "{name}");
            }
            if !name.starts_with(mangle::PREFIX) {
                assert_eq!(symbol.linkage, SymbolLinkage::Import, "{name}");
            }
            assert_ne!(
                symbol.linkage,
                SymbolLinkage::Export,
                "`{}` exports `{name}`",
                unit.path
            );
            let item = name.starts_with("cb$fn$") || name.starts_with("cb$slot$");
            if !item {
                continue;
            }
            match owner.get(name) {
                Some(path) if *path == unit.path => assert!(record.defines(name), "{name}"),
                Some(path) => {
                    assert_eq!(
                        symbol.linkage,
                        SymbolLinkage::Import,
                        "`{}` names `{name}` of `{path}`",
                        unit.path
                    );
                    // Its owner defines it hidden, since another unit
                    // names it.
                    let (_, theirs) = records.iter().find(|(u, _)| u.path == *path).unwrap();
                    assert_eq!(linkage(theirs, name), SymbolLinkage::Hidden, "{name}");
                }
                None => panic!("`{}` names `{name}`, which no unit defines", unit.path),
            }
        }
    }
}
