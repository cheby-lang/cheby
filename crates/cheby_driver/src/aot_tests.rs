//! `cheby build`'s objects (D-915, D-917, D-924): 001's module objects
//! and program object, made by the crate-private function `build_cached`
//! uses ([`crate::aot::build_objects`]) and read back with the `object`
//! crate; and a warm build that checks no module twice. The module
//! records' tables (D-919) are decoded from the replayed objects and
//! compared with the lowered units' facts and with the records the JIT
//! defines for the same build, which both hosts register through one
//! routine (`cheby_runtime::image::register_records`).

#![expect(
    clippy::arithmetic_side_effects,
    clippy::too_many_lines,
    reason = "test helpers: offsets inside a small record, and one decoder of its layout"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ::object::read::{Object as _, ObjectSection as _, ObjectSymbol as _};
use ::object::{RelocationFlags, SectionKind, SymbolScope, elf, macho};

use cheby_codegen_clif::record::CodeRecord;
use cheby_codegen_clif::{isa, mangle};
use cheby_mir::{LayoutDesc, LayoutRef, MirModule, MirOptions, Repr, UnitKind};
use cheby_runtime::image::{FIELD_KINDS, NO_STRING};
use cheby_runtime::layout::{FieldKind, LayoutInfo};

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
/// `hello_world`, and the program object (D-915, D-1111); `hello_world`
/// defines `main`'s slot hidden and imports `std::io::println`'s, which
/// `std::io` defines hidden; both module records are hidden data, and
/// their tables, read back from the objects, are what the unit defines
/// (D-919): `hello_world` lists no type and no hook, `std::io` lists its
/// types `Error` and `Kind`, and a hook entry for `Error` whose three
/// `cb$fnval$…` values the unit defines; external types, free shapes and
/// constants are empty in both. Only `std::io` defines layout words and
/// constructor statics, and nothing imports one. The program object
/// exports `main` only, defines `cb$image` local and imports every module
/// record and the runtime ID's symbol; text holds only PC-relative
/// relocations.
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
            // only `std::io` defines any, which its record lists.
            if name.starts_with("cb$layout$") || name.starts_with("cb$ctor$") {
                assert!(defined, "{object}: {name}");
                assert_eq!(*object, "std::io", "{name}");
                assert!(name.contains("$std$io$"), "{object}: {name}");
            }
        }
    }
    assert!(program.contains_key("main"));
    // The module records' tables, each count read back from the objects:
    // strings, types, externals, hooks, free shapes, functions, constants.
    let hello = decode_record(&objects[1].1, "cb$mod$hello_world");
    assert!(hello.counts[5] > 0);
    assert_eq!(
        [1, 2, 3, 4, 6].map(|k| hello.counts[k]),
        [0, 0, 0, 0, 0],
        "hello_world"
    );
    let io = decode_record(&objects[0].1, "cb$mod$std$io");
    assert!(io.counts[0] > 0 && io.counts[5] > 0);
    // No external types (std's handle types are runtime kinds, D-982) or
    // constants; one free-shape word per constructor (D-907).
    assert_eq!([2, 6].map(|k| io.counts[k]), [0, 0], "std::io");
    let ctors: usize = io.types.iter().map(|t| t.ctors.len()).sum();
    assert_eq!(io.shapes.len(), ctors, "std::io");
    let types: Vec<&str> = io.types.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(types, ["Error", "Kind"]);
    assert_eq!(io.counts[1], 2);
    assert_eq!(io.counts[3], 1);
    let (error, values) = &io.hooks[0];
    assert_eq!(*error, 0);
    for value in values {
        assert!(value.starts_with("cb$fnval$std$io$"), "{value}");
        assert_eq!(
            table["std::io"].get(value).map(|s| s.0),
            Some(true),
            "{value}"
        );
    }
    for ty in &io.types {
        for ctor in &ty.ctors {
            assert!(ctor.layout_word.0.starts_with("cb$layout$std$io$"));
            assert!(ctor.layout_word.1, "{} is writable", ctor.layout_word.0);
            assert_eq!(ctor.ctor_static.is_some(), ctor.fields.is_empty());
        }
    }
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

/// D-1115: the sizes of 001's two code records
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

/// The front end, the lowered units and the code records of the package
/// at `root`, as `build_objects` makes them, with the default MIR options.
fn lowered_and_compiled(root: &Path) -> (Vec<MirModule>, Vec<(String, CodeRecord)>) {
    let mut out = CheckOutput::default();
    let (front, package) = front_end(root, frontend::Mode::Build, &mut out)
        .unwrap()
        .unwrap_or_else(|| panic!("{}", out.render()));
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
    let compiled =
        compile_records(&front, &lowered.units, main.def, MirOptions::default()).unwrap();
    let records = compiled
        .code
        .iter()
        .zip(compiled.records)
        .map(|(unit, record)| (unit.path.to_string(), record))
        .collect();
    (lowered.units, records)
}

/// A unit that defines no user layout or external type writes the module
/// record it wrote before the records carried types (d13dbbf): 001's
/// `hello_world` code record, encoded, is pinned on macOS arm64 by its
/// size and digest (4352 bytes at version 2 with positions, D-938; 4320
/// at version 1, D-1115), and on every target its module record lists no
/// type, external type, hook or free shape. `std::io`'s record grows by its types' tables; both sizes are
/// printed for the report (`--nocapture`).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn aot_001_hello_world_record_unchanged() {
    let (_, records) = lowered_and_compiled(&example("001_hello_world"));
    let mut sizes = BTreeMap::new();
    for (path, record) in &records {
        let encoded = record.encode();
        eprintln!(
            "record `{path}`: {} bytes encoded, digest {:#018x}",
            encoded.len(),
            digest(&encoded)
        );
        sizes.insert(path.as_str(), (encoded.len(), digest(&encoded)));
    }
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        // The version-2 record with positions (D-938): d13dbbf's and
        // bd16c14's 4320 bytes at version 1 (D-1115), plus the unit's
        // source path and each function's rows.
        assert_eq!(sizes["hello_world"], (4352, 0xf57a_7e8b_b4d8_b194));
    }
    let (_, hello) = &records[1];
    let (bytes, _) = record_object(hello, "cb$mod$hello_world");
    for table in [16, 32, 48, 64, 96] {
        assert_eq!(u64_at(bytes, table), 0, "table at {table}");
    }
}

/// The SipHash-1-3 (zero key) of `bytes`, a stable digest for pins.
fn digest(bytes: &[u8]) -> u64 {
    use std::hash::Hasher as _;
    let mut h = siphasher::sip::SipHasher13::new_with_keys(0, 0);
    h.write(bytes);
    h.finish()
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// The bytes of the data object `name` of a code record, and its
/// relocations as (offset, target, addend).
fn record_object<'r>(record: &'r CodeRecord, name: &str) -> (&'r [u8], Vec<(u32, String, i64)>) {
    let symbol = record
        .symbols
        .iter()
        .position(|s| s.name == name)
        .unwrap_or_else(|| panic!("no {name}"));
    let object = record
        .data
        .iter()
        .find(|d| usize::try_from(d.symbol).unwrap() == symbol)
        .unwrap();
    let cheby_codegen_clif::record::RecordContents::Bytes(bytes) = &object.contents else {
        panic!("{name} is zero-filled");
    };
    let relocs = object
        .relocs
        .iter()
        .map(|r| {
            (
                r.offset,
                record.symbol_at(r.symbol).unwrap().name.clone(),
                r.addend,
            )
        })
        .collect();
    (bytes, relocs)
}

/// A constructor of a module record, decoded from a replayed object.
#[derive(Debug)]
struct DecodedCtor {
    name: String,
    ctor_name: String,
    record: bool,
    /// (kind, name, offset) in declaration order.
    fields: Vec<(FieldKind, Option<String>, u32)>,
    /// The symbol the layout word's relocation names, and whether it sits
    /// in a writable section of the object.
    layout_word: (String, bool),
    ctor_static: Option<(String, bool)>,
}

/// A type of a module record, decoded from a replayed object.
#[derive(Debug)]
struct DecodedType {
    name: String,
    type_key: u64,
    ctors: Vec<DecodedCtor>,
}

/// A module record decoded from a replayed object: the seven tables'
/// counts, the types, the hooks (type index, the three values'
/// symbols), the external types (name, key, whether a drop function is
/// named), the free-shape words and each function's call-site count.
#[derive(Debug)]
struct DecodedRecord {
    counts: [u64; 7],
    types: Vec<DecodedType>,
    hooks: Vec<(u32, [String; 3])>,
    externals: Vec<(String, u64, bool)>,
    shapes: Vec<u64>,
    sites: Vec<u64>,
}

/// Decodes the module record `record` (a symbol name without Mach-O's
/// underscore) of the replayed object `bytes`, as `image.rs`'s module
/// documentation lays it out: words read from the object's section, each
/// pointer resolved through the section's relocations (`object`'s
/// relocation map), and each layout word, constructor static and hook
/// value named by its relocation's target symbol.
fn decode_record(bytes: &[u8], record: &str) -> DecodedRecord {
    use ::object::read::{ObjectSection as _, ObjectSymbol as _, RelocationTarget};
    let file = ::object::File::parse(bytes).unwrap();
    let symbol = file
        .symbols()
        .find(|s| s.name().is_ok_and(|n| plain(n) == record))
        .unwrap_or_else(|| panic!("no {record}"));
    let section = file
        .section_by_index(symbol.section_index().unwrap())
        .unwrap();
    let data = section.data().unwrap();
    let start = symbol.address() - section.address();
    let map = section.relocation_map().unwrap();
    // The target symbol of each relocation of the section, by offset.
    let mut targets: BTreeMap<u64, String> = BTreeMap::new();
    for (offset, reloc) in section.relocations() {
        let name = match reloc.target() {
            RelocationTarget::Symbol(index) => {
                plain(file.symbol_by_index(index).unwrap().name().unwrap()).to_owned()
            }
            other => panic!("{record}: a relocation to {other:?}"),
        };
        targets.insert(offset, name);
    }
    let at = |offset: u64| usize::try_from(start + offset).unwrap();
    let word = |offset: u64| u64_at(data, at(offset));
    let half = |offset: u64| u32_at(data, at(offset));
    // A pointer into the record itself, as an offset from its start.
    let inner = |offset: u64| {
        assert_eq!(
            targets.get(&(start + offset)).map(String::as_str),
            Some(record),
            "{record}: the pointer at {offset}"
        );
        map.relocate(start + offset, word(offset)) - symbol.address()
    };
    // The symbol a pointer names, and whether it is writable data.
    let writable: BTreeMap<String, bool> = file
        .symbols()
        .filter_map(|s| {
            let section = file.section_by_index(s.section_index()?).ok()?;
            Some((
                plain(s.name().ok()?).to_owned(),
                section.kind() == SectionKind::Data,
            ))
        })
        .collect();
    let named = |offset: u64| {
        let name = targets
            .get(&(start + offset))
            .unwrap_or_else(|| panic!("{record}: no relocation at {offset}"))
            .clone();
        let writable = writable.get(&name).copied().unwrap_or(false);
        (name, writable)
    };
    let counts: [u64; 7] = std::array::from_fn(|k| word(16 * u64::try_from(k).unwrap()));
    let strings = if counts[0] == 0 { 0 } else { inner(8) };
    let text = |offset: u32| -> String {
        let string = strings + u64::from(offset);
        let len = usize::try_from(half(string)).unwrap();
        let from = at(string + 4);
        String::from_utf8(data[from..from + len].to_vec()).unwrap()
    };
    let mut types = Vec::new();
    let type_table = if counts[1] == 0 { 0 } else { inner(24) };
    for t in 0..counts[1] {
        let entry = type_table + 32 * t;
        let ctor_count = word(entry + 16);
        let ctor_table = inner(entry + 24);
        let mut ctors = Vec::new();
        for c in 0..ctor_count {
            let ctor = ctor_table + 48 * c;
            let field_count = word(ctor + 16);
            let mut fields = Vec::new();
            if field_count > 0 {
                let field_table = inner(ctor + 24);
                for f in 0..field_count {
                    let field = field_table + 16 * f;
                    let kind = FIELD_KINDS[usize::try_from(half(field)).unwrap()];
                    let name = half(field + 4);
                    let name = (name != NO_STRING).then(|| text(name));
                    fields.push((kind, name, half(field + 8)));
                }
            } else {
                assert_eq!(word(ctor + 24), 0, "{record}: an empty field table");
            }
            let ctor_static = if targets.contains_key(&(start + ctor + 40)) {
                Some(named(ctor + 40))
            } else {
                assert_eq!(word(ctor + 40), 0);
                None
            };
            ctors.push(DecodedCtor {
                name: text(half(ctor)),
                ctor_name: text(half(ctor + 4)),
                record: match half(ctor + 8) {
                    0 => false,
                    1 => true,
                    other => panic!("{record}: record flag {other}"),
                },
                fields,
                layout_word: named(ctor + 32),
                ctor_static,
            });
        }
        types.push(DecodedType {
            name: text(half(entry)),
            type_key: word(entry + 8),
            ctors,
        });
    }
    let mut hooks = Vec::new();
    let hook_table = if counts[3] == 0 { 0 } else { inner(56) };
    for h in 0..counts[3] {
        let entry = hook_table + 32 * h;
        let values = [8, 16, 24].map(|at| named(entry + at).0);
        hooks.push((half(entry), values));
    }
    let mut externals = Vec::new();
    let external_table = if counts[2] == 0 { 0 } else { inner(40) };
    for e in 0..counts[2] {
        let entry = external_table + 24 * e;
        let drop = targets.contains_key(&(start + entry + 16)) || word(entry + 16) != 0;
        externals.push((text(half(entry)), word(entry + 8), drop));
    }
    let shape_table = if counts[4] == 0 { 0 } else { inner(72) };
    let shapes = (0..counts[4]).map(|s| word(shape_table + 8 * s)).collect();
    let function_table = if counts[5] == 0 { 0 } else { inner(88) };
    let sites = (0..counts[5])
        .map(|f| word(function_table + 32 * f + 16))
        .collect();
    DecodedRecord {
        counts,
        types,
        hooks,
        externals,
        shapes,
        sites,
    }
}

/// The field kind a record gives a field of a representation.
fn jit_field_kind(repr: Repr) -> FieldKind {
    match repr {
        Repr::Value => FieldKind::Value,
        Repr::Int => FieldKind::Int,
        Repr::Float => FieldKind::Float,
        Repr::Bool => FieldKind::Bool,
        Repr::I8 => FieldKind::I8,
        Repr::I16 => FieldKind::I16,
        Repr::I32 => FieldKind::I32,
        Repr::I64 => FieldKind::I64,
        Repr::U8 => FieldKind::U8,
        Repr::U16 => FieldKind::U16,
        Repr::U32 => FieldKind::U32,
        Repr::U64 => FieldKind::U64,
        Repr::F32 => FieldKind::F32,
    }
}

/// The facts registration gives the layout of `desc` (D-1163).
fn jit_facts(desc: &LayoutDesc) -> LayoutInfo {
    let fields: Vec<(FieldKind, Option<&str>)> = desc
        .fields
        .iter()
        .map(|f| (jit_field_kind(f.repr), f.name.as_deref()))
        .collect();
    LayoutInfo::ctor(&desc.name, desc.tag, &fields).named(
        &desc.type_name,
        &desc.ctor_name,
        desc.record,
        desc.type_key,
    )
}

/// Replays each code record and checks its module record's tables against
/// the lowered units, as D-1163 orders them: the types in the order of
/// their first constructor in `MirModule::layouts`, each type's
/// constructors in tag order with the facts of their `LayoutDesc`s, each
/// constructor's layout word and (exactly when it has no fields) its
/// constructor static, the unit's own writable data; a hook entry exactly
/// for the types whose three `cb$fnval$…` values the unit defines; and the
/// free-shape word registration derives for each constructor (D-907).
/// Returns each unit's decoded record.
fn check_records(
    units: &[MirModule],
    records: &[(String, CodeRecord)],
) -> Vec<(String, DecodedRecord)> {
    let mut out = Vec::new();
    for (path, record) in records {
        let unit = units
            .iter()
            .find(|u| matches!(u.unit, UnitKind::Code(_)) && &*u.path == path)
            .unwrap();
        let object = cheby_link::object::replay(record, isa::host_isa().unwrap(), path).unwrap();
        let defined = symbols(&object);
        let decoded = decode_record(&object, &mangle::module_record(path));
        // A unit alone lists every constant it initializes, beside its
        // slot (D-979, GAP(const-image-slots)); no external type names a
        // drop function yet (D-1168).
        let consts = unit
            .functions
            .iter()
            .filter(|f| matches!(f.key, cheby_mir::FnKey::ConstInit(_)))
            .count();
        assert_eq!(decoded.counts[6], u64::try_from(consts).unwrap(), "{path}");
        assert!(decoded.externals.iter().all(|e| !e.2), "{path}");
        // One free-shape word per constructor, the one registration
        // derives (D-907, D-1167).
        let mut shapes = Vec::new();
        let mut order = Vec::new();
        for desc in &unit.layouts {
            if !order.contains(&desc.type_def) {
                order.push(desc.type_def);
            }
        }
        assert_eq!(decoded.types.len(), order.len(), "{path}: the types");
        let mut hooks: Vec<(u32, [String; 3])> = Vec::new();
        for (t, (&ty, got)) in order.iter().zip(&decoded.types).enumerate() {
            let mut ctors: Vec<&LayoutDesc> =
                unit.layouts.iter().filter(|d| d.type_def == ty).collect();
            ctors.sort_by_key(|d| d.tag);
            assert_eq!(got.ctors.len(), ctors.len(), "{path}: {}", got.name);
            for (desc, ctor) in ctors.iter().zip(&got.ctors) {
                let expected = jit_facts(desc);
                shapes.push(cheby_runtime::layout::free_shape_word(&expected));
                let fields: Vec<(FieldKind, Option<&str>)> = ctor
                    .fields
                    .iter()
                    .map(|(kind, name, _)| (*kind, name.as_deref()))
                    .collect();
                let registered = LayoutInfo::ctor(&ctor.name, desc.tag, &fields).named(
                    &got.name,
                    &ctor.ctor_name,
                    ctor.record,
                    got.type_key,
                );
                assert_eq!(registered, expected, "{path}: {}", desc.name);
                let offsets: Vec<u32> = ctor.fields.iter().map(|f| f.2).collect();
                let runtime: Vec<u32> = expected.fields.iter().map(|f| f.offset).collect();
                assert_eq!(offsets, runtime, "{path}: {}", desc.name);
                assert_eq!(
                    got.type_key,
                    cheby_runtime::layout::type_key(path, &got.name),
                    "{path}: {}",
                    got.name
                );
                let LayoutRef::Ctor(def) = desc.layout else {
                    panic!("{path}: {:?}", desc.layout);
                };
                let name = unit.name(def).unwrap();
                let word = mangle::item(mangle::Kind::Layout, name);
                assert_eq!(ctor.layout_word, (word.clone(), true), "{path}");
                assert_eq!(defined.get(&word).map(|s| s.0), Some(true), "{word}");
                let ctor_static = desc.fields.is_empty().then(|| {
                    let symbol = mangle::item(mangle::Kind::Ctor, name);
                    assert_eq!(defined.get(&symbol).map(|s| s.0), Some(true), "{symbol}");
                    (symbol, true)
                });
                assert_eq!(ctor.ctor_static, ctor_static, "{path}: {}", desc.name);
            }
            if let Some(defs) = ctors[0].hooks {
                let values =
                    defs.map(|def| mangle::item(mangle::Kind::FnVal, unit.name(def).unwrap()));
                if values.iter().all(|value| record.defines(value)) {
                    hooks.push((u32::try_from(t).unwrap(), values));
                }
            }
        }
        assert_eq!(decoded.hooks, hooks, "{path}: the hooks");
        assert_eq!(decoded.shapes, shapes, "{path}: the free shapes");
        out.push((path.clone(), decoded));
    }
    out
}

/// The facts a module record gives the one routine (D-919), addresses
/// aside: per type its name, key and constructors (path, name, record
/// flag, fields as kind, name and offset, whether it has a constructor
/// static); the hooked types' indices; the external types (name, key,
/// whether a drop function is named); the free-shape words; and the
/// call-site count of every function, sorted.
#[derive(Debug, PartialEq, Eq)]
struct RecordFacts {
    types: Vec<(String, u64, Vec<CtorFacts>)>,
    hooks: Vec<u32>,
    externals: Vec<(String, u64, bool)>,
    shapes: Vec<u64>,
    sites: Vec<u64>,
    constants: u64,
}

/// A constructor's facts in [`RecordFacts`].
type CtorFacts = (
    String,
    String,
    bool,
    Vec<(FieldKind, Option<String>, u32)>,
    bool,
);

impl RecordFacts {
    /// The facts of a record decoded from a replayed object.
    fn of_object(decoded: &DecodedRecord) -> Self {
        let mut sites = decoded.sites.clone();
        sites.retain(|&n| n > 0);
        sites.sort_unstable();
        Self {
            types: decoded
                .types
                .iter()
                .map(|ty| {
                    let ctors = ty
                        .ctors
                        .iter()
                        .map(|c| {
                            (
                                c.name.clone(),
                                c.ctor_name.clone(),
                                c.record,
                                c.fields.clone(),
                                c.ctor_static.is_some(),
                            )
                        })
                        .collect();
                    (ty.name.clone(), ty.type_key, ctors)
                })
                .collect(),
            hooks: decoded.hooks.iter().map(|(t, _)| *t).collect(),
            externals: decoded.externals.clone(),
            shapes: decoded.shapes.clone(),
            sites,
            // A JIT batch's records list no constant, since a JIT host finds
            // slots by def (GAP(const-image-slots)); the object's count is
            // checked against its unit in `check_records`.
            constants: 0,
        }
    }

    /// The facts of the record at `address` in a JIT's memory, read in
    /// the layout of `image.rs`'s module documentation, whose pointers are
    /// the absolute addresses the JIT resolved.
    ///
    /// # Safety
    ///
    /// `address` is a record of a live JIT.
    unsafe fn of_memory(address: usize) -> Self {
        // SAFETY: every address read below lies inside the record or the
        // tables it points to, in the JIT's live, finalized memory.
        let word =
            |at: usize| unsafe { std::ptr::with_exposed_provenance::<u64>(at).read_unaligned() };
        // SAFETY: as above.
        let half =
            |at: usize| unsafe { std::ptr::with_exposed_provenance::<u32>(at).read_unaligned() };
        let pointer = |at: usize| usize::try_from(word(at)).unwrap();
        let count = |table: usize| usize::try_from(word(address + table)).unwrap();
        let strings = pointer(address + 8);
        let text = |offset: u32| -> String {
            let at = strings + usize::try_from(offset).unwrap();
            let len = usize::try_from(half(at)).unwrap();
            // SAFETY: the string's bytes follow its length in the string
            // table, inside the JIT's memory.
            let bytes = unsafe {
                std::slice::from_raw_parts(std::ptr::with_exposed_provenance::<u8>(at + 4), len)
            };
            String::from_utf8(bytes.to_vec()).unwrap()
        };
        let mut types = Vec::new();
        for t in 0..count(16) {
            let entry = pointer(address + 24) + 32 * t;
            let mut ctors = Vec::new();
            for c in 0..usize::try_from(word(entry + 16)).unwrap() {
                let ctor = pointer(entry + 24) + 48 * c;
                let mut fields = Vec::new();
                for f in 0..usize::try_from(word(ctor + 16)).unwrap() {
                    let field = pointer(ctor + 24) + 16 * f;
                    let kind = FIELD_KINDS[usize::try_from(half(field)).unwrap()];
                    let name = half(field + 4);
                    fields.push((
                        kind,
                        (name != NO_STRING).then(|| text(name)),
                        half(field + 8),
                    ));
                }
                assert_ne!(word(ctor + 32), 0, "a layout word");
                ctors.push((
                    text(half(ctor)),
                    text(half(ctor + 4)),
                    half(ctor + 8) == 1,
                    fields,
                    word(ctor + 40) != 0,
                ));
            }
            types.push((text(half(entry)), word(entry + 8), ctors));
        }
        let hooks = (0..count(48))
            .map(|h| half(pointer(address + 56) + 32 * h))
            .collect();
        let externals = (0..count(32))
            .map(|e| {
                let entry = pointer(address + 40) + 24 * e;
                (text(half(entry)), word(entry + 8), word(entry + 16) != 0)
            })
            .collect();
        let shapes = (0..count(64))
            .map(|s| word(pointer(address + 72) + 8 * s))
            .collect();
        let mut sites: Vec<u64> = (0..count(80))
            .map(|f| word(pointer(address + 88) + 32 * f + 16))
            .filter(|&n| n > 0)
            .collect();
        sites.sort_unstable();
        Self {
            types,
            hooks,
            externals,
            shapes,
            sites,
            constants: word(address + 96),
        }
    }
}

/// The package of `aot_records_match_the_jit`: user types in three
/// modules, an external type, `std::io::Error` reached through `main`'s
/// result.
fn facts_package(dir: &Path) -> PathBuf {
    let root = dir.join("facts");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("cheby.toml"),
        "name = \"facts\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.cheby"),
        "//! User types in three modules.\n\nimport std::io\nimport facts::shapes\nimport facts::wide\n\n\
         /// A count.\ntype Tally {\n  Tally(Int),\n  NoTally,\n}\n\n\
         fn main() -> Result<Nil, io::Error> {\n  let circle = shapes::Circle { radius: 2, label: \"c\" }\n  \
         let square = shapes::Square(3, \"s\")\n  \
         let total = shapes::area(circle) + shapes::area(square) + shapes::area(shapes::Dot)\n  \
         let boxed = shapes::unbox(shapes::Boxed(Tally(total)))\n  \
         let _ = wide::make(\"x\")\n  \
         case boxed {\n    Tally(n) => io::println(\"{n}\")\n    NoTally => io::println(\"none\")\n  }\n  \
         Ok(Nil)\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/shapes.cheby"),
        "//! A record constructor, a positional one, a nullary one, a\n//! generic type and an external type.\n\n\
         /// A handle nothing opens.\n@external(native, \"facts_db_close\")\npub type Db\n\n\
         /// A shape.\ntype Shape {\n  Circle { radius: Int, label: String },\n  Square(Int, String),\n  Dot,\n}\n\n\
         /// A box around any value.\ntype Boxed<T> {\n  Boxed(T),\n}\n\n\
         /// The area of `shape`, roughly.\nfn area(shape: Shape) -> Int {\n  case shape {\n    \
         Circle { radius: r, .. } => 3 * r * r\n    Square(side, _) => side * side\n    Dot => 0\n  }\n}\n\n\
         /// The value in `boxed`.\nfn unbox<T>(boxed: Boxed<T>) -> T {\n  case boxed {\n    Boxed(value) => value\n  }\n}\n",
    )
    .unwrap();
    let fields: String = (0..255).map(|_| "    String,\n").collect();
    let values: String = (0..255).map(|_| "    text,\n").collect();
    std::fs::write(
        root.join("src/wide.cheby"),
        format!(
            "//! A constructor with 255 value fields.\n\n/// Many strings.\ntype Wide {{\n  Wide(\n{fields}  ),\n}}\n\n\
             /// A `Wide` of `text` everywhere.\nfn make(text: String) -> Wide {{\n  Wide(\n{values}  )\n}}\n"
        ),
    )
    .unwrap();
    root
}

/// Compiles `units` in a JIT and checks that each code unit's module
/// record, as the JIT defines it (read from its memory after
/// finalization, addresses aside), gives the one routine the facts of the
/// same unit's record decoded from its replayed object (`decoded`):
/// types, constructors, fields, external types, hooks, free shapes and
/// call-site counts (D-919). The entries unit has no record (D-1111).
fn check_jit_records(units: &[MirModule], decoded: &[(String, DecodedRecord)]) {
    let mut jit = cheby_jit::Jit::new().unwrap();
    jit.compile(units).unwrap();
    let records = jit.module_records();
    assert!(
        records.iter().all(|r| !matches!(r.unit, UnitKind::Entries)),
        "{records:?}"
    );
    for (path, object) in decoded {
        let record = records
            .iter()
            .find(|r| matches!(r.unit, UnitKind::Code(_)) && &*r.path == path)
            .unwrap_or_else(|| panic!("no JIT record of {path}"));
        // SAFETY: the record is in `jit`'s memory, which lives.
        let in_memory = unsafe { RecordFacts::of_memory(record.address) };
        assert_eq!(in_memory, RecordFacts::of_object(object), "{path}");
    }
}

/// D-919, D-920 and D-1163: both hosts give the one routine the same
/// facts. Each unit's module record as the JIT defines it equals the one
/// decoded from the replayed object of the same unit, and those list, in
/// order, the layouts `cheby run` registers, with its facts, the external
/// types, the hooks and the free-shape words (plan §2.1). A program with
/// user types in three modules (a record constructor, a positional one, a
/// nullary one, a generic type, a constructor with 255 value fields), an
/// external type and `std::io::Error` reached; and 001.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn aot_records_match_the_jit() {
    let dir = std::env::temp_dir().join(format!("cheby-aot-facts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = facts_package(&dir);
    let (units, records) = lowered_and_compiled(&root);
    let decoded = check_records(&units, &records);
    check_jit_records(&units, &decoded);
    let externals: Vec<&(String, u64, bool)> = decoded
        .iter()
        .flat_map(|(_, record)| &record.externals)
        .collect();
    assert_eq!(
        externals,
        [&(
            "Db".to_owned(),
            cheby_runtime::layout::type_key("facts::shapes", "Db"),
            false
        )]
    );
    let paths: Vec<&str> = decoded.iter().map(|(path, _)| path.as_str()).collect();
    for path in ["std::io", "facts::shapes", "facts::wide", "facts"] {
        assert!(paths.contains(&path), "{path} in {paths:?}");
    }
    let types = |path: &str| -> Vec<&DecodedType> {
        decoded
            .iter()
            .filter(|(p, _)| p == path)
            .flat_map(|(_, record)| &record.types)
            .collect()
    };
    let shapes = types("facts::shapes");
    let names: Vec<&str> = shapes.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["Shape", "Boxed"]);
    let ctors: Vec<(&str, &str, bool, usize, bool)> = shapes[0]
        .ctors
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.ctor_name.as_str(),
                c.record,
                c.fields.len(),
                c.ctor_static.is_some(),
            )
        })
        .collect();
    assert_eq!(
        ctors,
        [
            ("facts::shapes::Circle", "Circle", true, 2, false),
            ("facts::shapes::Square", "Square", false, 2, false),
            ("facts::shapes::Dot", "Dot", false, 0, true),
        ]
    );
    // The record constructor's raw `Int` follows its value field in memory,
    // and the names are written.
    assert_eq!(
        shapes[0].ctors[0].fields,
        [
            (FieldKind::Int, Some("radius".to_owned()), 16),
            (FieldKind::Value, Some("label".to_owned()), 8),
        ]
    );
    let wide = types("facts::wide");
    assert_eq!(wide.len(), 1);
    assert_eq!(wide[0].ctors[0].fields.len(), 255);
    assert!(
        wide[0].ctors[0]
            .fields
            .iter()
            .all(|f| f.0 == FieldKind::Value)
    );
    let tally = types("facts");
    assert_eq!(tally.len(), 1);
    assert_eq!(tally[0].ctors.len(), 2);
    let io: Vec<&str> = types("std::io").iter().map(|t| t.name.as_str()).collect();
    assert!(io.contains(&"Error"), "{io:?}");
    let _ = std::fs::remove_dir_all(&dir);
    // 001's two records, with `std::io`'s hooks exactly as `cheby run`
    // registers them.
    let (units, records) = lowered_and_compiled(&example("001_hello_world"));
    let decoded = check_records(&units, &records);
    check_jit_records(&units, &decoded);
    let (_, io) = &decoded[0];
    let hooked: Vec<&str> = io
        .hooks
        .iter()
        .map(|(t, _)| io.types[usize::try_from(*t).unwrap()].name.as_str())
        .collect();
    eprintln!(
        "001: `std::io` lists {:?}, hooks on {hooked:?}",
        io.types.iter().map(|t| t.name.as_str()).collect::<Vec<_>>()
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
