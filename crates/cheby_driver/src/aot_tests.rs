//! `cheby build`'s objects (D-915, D-917, D-924): 001's module objects
//! and program object, made by the crate-private function `build_cached`
//! uses ([`crate::aot::build_objects`]) and read back with the `object`
//! crate; and a warm build that checks no module twice.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ::object::read::{Object as _, ObjectSection as _, ObjectSymbol as _};
use ::object::{RelocationFlags, SectionKind, SymbolScope, elf, macho};

use cheby_mir::MirOptions;

use super::*;
use crate::aot::{BuildOptions, Objects, build_objects, compile_records};
use crate::cache::{BuildId, CacheLog, Store};

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/examples")
        .join(name)
}

/// The executable's path: never written, since the tests stop before the
/// link.
fn options() -> BuildOptions {
    BuildOptions {
        output: Some(std::env::temp_dir().join("cheby-aot-tests-never-written")),
        ..BuildOptions::default()
    }
}

/// The objects of 001, without a cache.
fn objects_of_001() -> Vec<(String, Vec<u8>)> {
    match build_objects(&example("001_hello_world"), &options(), None).unwrap() {
        Objects::Ready { output, built } => {
            assert!(!output.has_errors(), "{}", output.render());
            built.objects
        }
        Objects::CompileFailed(output) => panic!("001 failed: {}", output.render()),
    }
}

/// A symbol's name without Mach-O's leading underscore.
fn plain(name: &str) -> &str {
    if cfg!(target_os = "macos") {
        name.strip_prefix('_').unwrap_or(name)
    } else {
        name
    }
}

/// Every named symbol of an object: (defined, scope, is data).
fn symbols(bytes: &[u8]) -> BTreeMap<String, (bool, SymbolScope, bool)> {
    let file = ::object::File::parse(bytes).unwrap();
    file.symbols()
        .filter(|s| {
            !matches!(
                s.kind(),
                ::object::SymbolKind::File | ::object::SymbolKind::Section
            ) && s.name().is_ok_and(|n| !n.is_empty())
        })
        .map(|s| {
            let data = s
                .section_index()
                .and_then(|i| file.section_by_index(i).ok())
                .is_some_and(|section| section.kind() != SectionKind::Text);
            (
                plain(s.name().unwrap()).to_owned(),
                (s.is_definition(), s.scope(), data),
            )
        })
        .collect()
}

/// Whether a text relocation is PC-relative (D-913).
fn pc_relative(flags: RelocationFlags) -> bool {
    match flags {
        RelocationFlags::Elf { r_type } => matches!(
            r_type,
            elf::R_AARCH64_ADR_PREL_PG_HI21
                | elf::R_AARCH64_ADD_ABS_LO12_NC
                | elf::R_AARCH64_CALL26
                | elf::R_X86_64_PC32
                | elf::R_X86_64_PLT32
        ),
        RelocationFlags::MachO { r_type, .. } => matches!(
            r_type,
            macho::ARM64_RELOC_PAGE21 | macho::ARM64_RELOC_PAGEOFF12 | macho::ARM64_RELOC_BRANCH26
        ),
        _ => false,
    }
}

/// 001 builds from exactly two module objects, `std::io` and
/// `hello_world`, and the program object; its units meet no "not
/// available yet" criterion (no code that can run uses a layout word or a
/// `cb$ctor$` static; `std::io::Error`'s hooks are compiled but
/// unreachable, GAP(aot-layouts-not-yet)); `hello_world` defines `main`'s slot hidden and imports
/// `std::io::println`'s, which `std::io` defines hidden; both module
/// records are hidden data; the program object exports `main` only,
/// defines `cb$image` local and imports every module record and the
/// runtime ID's symbol; text holds only PC-relative relocations.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn aot_001_objects() {
    let objects = objects_of_001();
    let names: Vec<&str> = objects.iter().map(|(name, _)| name.as_str()).collect();
    // Build order: `std::io` before `hello_world`, which imports it (D-915).
    assert_eq!(names, ["std::io", "hello_world", "cb$program"]);
    let table: BTreeMap<&str, BTreeMap<String, (bool, SymbolScope, bool)>> = objects
        .iter()
        .map(|(name, bytes)| (name.as_str(), symbols(bytes)))
        .collect();
    let hidden = |object: &str, symbol: &str| {
        let s = table[object].get(symbol).copied();
        assert_eq!(
            s.map(|(defined, scope, _)| (defined, scope)),
            Some((true, SymbolScope::Linkage)),
            "{object}: {symbol}"
        );
    };
    let imported = |object: &str, symbol: &str| {
        let s = table[object].get(symbol).copied();
        assert_eq!(s.map(|s| s.0), Some(false), "{object}: {symbol}");
    };
    hidden("hello_world", "cb$slot$hello_world$main");
    imported("hello_world", "cb$slot$std$io$println");
    hidden("std::io", "cb$slot$std$io$println");
    for (object, record) in [
        ("hello_world", "cb$mod$hello_world"),
        ("std::io", "cb$mod$std$io"),
    ] {
        hidden(object, record);
        assert!(table[object][record].2, "{record} is data");
        imported("cb$program", record);
    }
    imported("cb$program", cheby_runtime::abi::sym::RUNTIME_ID_SYMBOL);
    let program = &table["cb$program"];
    assert_eq!(
        program.get("cb$image").map(|s| (s.0, s.1, s.2)),
        Some((true, SymbolScope::Compilation, true))
    );
    for (object, symbols) in &table {
        for (name, &(defined, scope, _)) in symbols {
            // The program's only exported symbol is `main` (D-917).
            if scope == SymbolScope::Dynamic {
                assert_eq!((*object, name.as_str()), ("cb$program", "main"));
                assert!(defined);
            }
            // No object imports a layout word or a constructor static, and
            // only `std::io` defines any: those of `Error`'s `Kind`, which
            // only `Error`'s `@std_hooks` use, compiled but unreachable
            // (GAP(aot-layouts-not-yet)).
            if name.starts_with("cb$layout$") || name.starts_with("cb$ctor$") {
                assert!(defined, "{object}: {name}");
                assert_eq!(*object, "std::io", "{name}");
                assert!(name.contains("$std$io$"), "{object}: {name}");
            }
        }
    }
    assert!(program.contains_key("main"));
    for (name, bytes) in &objects {
        let file = ::object::File::parse(&**bytes).unwrap();
        for section in file.sections().filter(|s| s.kind() == SectionKind::Text) {
            for (offset, reloc) in section.relocations() {
                assert!(
                    pc_relative(reloc.flags()),
                    "{name}: {:?} at {offset}",
                    reloc.flags()
                );
            }
        }
    }
}

/// GAP(aot-import-words-per-unit): the sizes of 001's two code records
/// and the entries record, printed for the report (`--nocapture`); each
/// unit alone keeps every runtime import word, as the JIT's first batch
/// declares them.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn aot_001_record_sizes() {
    let mut out = CheckOutput::default();
    let (front, package) = front_end(&example("001_hello_world"), frontend::Mode::Build, &mut out)
        .unwrap()
        .unwrap();
    let module = front.local_module(&ModulePath::root(&package)).unwrap();
    let main = front.main_of(module).unwrap();
    let lowered = backend::lower(
        &front,
        &out.sources,
        &backend::Roots::Main(main),
        MirOptions::default(),
    )
    .unwrap();
    let compiled =
        compile_records(&front, &lowered.units, main.def, MirOptions::default()).unwrap();
    for (unit, record) in compiled.code.iter().zip(&compiled.records) {
        let words = record
            .symbols
            .iter()
            .filter(|s| s.name.starts_with("cb$imp$"))
            .count();
        eprintln!(
            "record `{}`: {} bytes encoded, {} symbols, {words} import words",
            unit.path,
            record.encode().len(),
            record.symbols.len()
        );
        assert!(words > 0);
    }
    eprintln!(
        "entries record: {} bytes encoded",
        compiled.entries.encode().len()
    );
}

/// A warm build (D-934 with D-955): 001's objects built twice on one
/// cache are the same, and the warm front end checks no module twice
/// (K3b's count, `Cache::checked_modules`).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn aot_warm_build_checks_each_module_once() {
    let dir = std::env::temp_dir().join(format!("cheby-aot-warm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = || Store::open(&dir.join("cache"), BuildId([9; 16])).unwrap();
    let build =
        |cache: &Cache| match build_objects(&example("001_hello_world"), &options(), Some(cache))
            .unwrap()
        {
            Objects::Ready { built, .. } => built.objects,
            Objects::CompileFailed(output) => panic!("001 failed: {}", output.render()),
        };
    let cold_cache = Cache::with_store(store(), CacheLog::new(None));
    let cold = build(&cold_cache);
    cold_cache.finish(false).unwrap();
    let warm_cache = Cache::with_store(store(), CacheLog::new(None));
    let warm = build(&warm_cache);
    let checked = warm_cache.checked_modules();
    let mut once = checked.clone();
    once.dedup();
    assert_eq!(checked, once, "a module was checked twice");
    warm_cache.finish(false).unwrap();
    assert!(cold == warm, "the warm build's objects differ");
    let _ = std::fs::remove_dir_all(&dir);
}
