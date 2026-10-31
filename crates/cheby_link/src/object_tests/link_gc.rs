//! Tests `link_gc_*` (D-1253, `crate::gc`): a replayed module record keeps
//! no function table of its own; each function's entry, its name still an
//! offset into the record's string table, and its call sites go to a part
//! of the object tied to the function, and the Linux link line holds
//! `-z start-stop-gc`.

use std::collections::BTreeMap;
use std::path::Path;

use ::object::read::{Object as _, ObjectSection as _, ObjectSymbol as _};
use ::object::{RelocationTarget, SectionFlags};

use cheby_codegen_clif::mangle;
use cheby_codegen_clif::record::{RecordContents, SymbolKind};

use super::{Format, code_model_unit, record_for, replayed};
use crate::gc;
use crate::link::{LinkHost, LinkJob, START_STOP_GC, command, flags_for};

/// The module record's bytes and relocation offsets in `record`.
fn module_record(
    record: &cheby_codegen_clif::record::CodeRecord,
    path: &str,
) -> (Vec<u8>, Vec<u32>) {
    let name = mangle::module_record(path);
    let data = record
        .data
        .iter()
        .find(|d| record.symbol_at(d.symbol).is_some_and(|s| s.name == name))
        .unwrap();
    let RecordContents::Bytes(bytes) = &data.contents else {
        panic!("a module record of zeros");
    };
    (
        bytes.clone(),
        data.relocs.iter().map(|r| r.offset).collect(),
    )
}

fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// The split takes every entry and its sites out of the record, keeps
/// each entry's length, flags, name offset and site count, empties the
/// record's function table, removes no other relocation's target and
/// leaves the record read-only, its string table (and so every name
/// offset) unchanged.
#[test]
fn link_gc_split_moves_every_entry_and_keeps_names() {
    for &format in Format::ALL {
        let unit = code_model_unit();
        let record = record_for(&unit, format);
        let (before, before_relocs) = module_record(&record, &unit.path);
        let count = usize::try_from(word(&before, 80)).unwrap();
        assert!(count >= 2, "{format:?}: {count}");
        let split = gc::split(&record, &unit.path).unwrap().unwrap();
        assert_eq!(split.chunks.len(), count, "{format:?}");
        assert_eq!(split.key, gc::key(&mangle::module_record(&unit.path)));
        let mut sites = 0;
        for (index, chunk) in split.chunks.iter().enumerate() {
            let at = 112 + index * 40;
            assert_eq!(chunk.entry[8..24], before[at + 8..at + 24], "{format:?}");
            assert_eq!(chunk.entry[24..32], before[at + 24..at + 32], "{format:?}");
            assert_eq!(chunk.entry[..8], [0; 8]);
            assert_eq!(chunk.entry[32..], [0; 8]);
            let n = usize::try_from(word(&chunk.entry, 24)).unwrap();
            assert_eq!(chunk.sites.len(), n * 16, "{format:?}");
            sites += n;
        }
        let (after, after_relocs) = module_record(&split.record, &unit.path);
        let removed = count * 40 + sites * 16;
        assert_eq!(after.len() + removed, before.len(), "{format:?}");
        assert_eq!(word(&after, 80), 0, "{format:?}: an empty function table");
        assert_eq!(word(&after, 88), 0, "{format:?}");
        assert!(!after_relocs.contains(&88), "{format:?}");
        // Every relocation outside the table and its sites stays, shifted.
        let kept = before_relocs
            .iter()
            .filter(|&&o| o != 88 && !(112..112 + u32::try_from(removed).unwrap()).contains(&o))
            .count();
        assert_eq!(after_relocs.len(), kept, "{format:?}");
        // The tail (types, constants, strings) is the same bytes.
        assert_eq!(after[112..], before[112 + removed..], "{format:?}");
        let symbol = split
            .record
            .symbols
            .iter()
            .find(|s| s.name == mangle::module_record(&unit.path))
            .unwrap();
        assert_eq!(symbol.kind, SymbolKind::Data { writable: false });
    }
}

/// In the replayed object, each entry is tied to its function: on Mach-O
/// an atom of the shared `__DATA,cb_fns`, marked `S_ATTR_LIVE_SUPPORT`,
/// whose first relocation names the function and whose `pad` word holds
/// the record's tag; on ELF a `cbf_<key>` section of its own,
/// `SHF_LINK_ORDER`, whose `sh_link` is its function's section, every
/// function in a section of its own.
#[test]
fn link_gc_entries_are_tied_to_their_functions() {
    for &format in Format::ALL {
        let unit = code_model_unit();
        let (record, bytes) = replayed(&unit, format);
        let split = gc::split(&record, &unit.path).unwrap().unwrap();
        let section_name = match format {
            Format::MachO => gc::MACHO_SECTION.to_owned(),
            Format::Elf => gc::section(&split.key),
        };
        let file = ::object::File::parse(&*bytes).unwrap();
        let entries: Vec<_> = file
            .sections()
            .filter(|s| s.name() == Ok(section_name.as_str()))
            .collect();
        let function_names: Vec<String> = split
            .chunks
            .iter()
            .map(|c| format.object_name(&c.function))
            .collect();
        let mut targets = Vec::new();
        match format {
            Format::MachO => {
                assert_eq!(entries.len(), 1);
                let SectionFlags::MachO { flags } = entries[0].flags() else {
                    panic!("Mach-O flags");
                };
                assert_ne!(flags & ::object::macho::S_ATTR_LIVE_SUPPORT, 0);
                assert_eq!(
                    entries[0].size(),
                    40 * u64::try_from(split.chunks.len()).unwrap()
                );
                let relocs: BTreeMap<u64, _> = entries[0].relocations().collect();
                let data = entries[0].data().unwrap();
                for entry in data.chunks(40) {
                    let pad = u32::from_le_bytes(entry[20..24].try_into().unwrap());
                    assert_eq!(pad, split.tag);
                    assert_eq!(pad, gc::tag(&mangle::module_record(&unit.path)));
                }
                for index in 0..split.chunks.len() {
                    let at = 40 * u64::try_from(index).unwrap();
                    let reloc = relocs.get(&at).unwrap();
                    let RelocationTarget::Symbol(symbol) = reloc.target() else {
                        panic!("a relocation without a symbol");
                    };
                    targets.push(
                        file.symbol_by_index(symbol)
                            .unwrap()
                            .name()
                            .unwrap()
                            .to_owned(),
                    );
                }
            }
            Format::Elf => {
                assert_eq!(entries.len(), split.chunks.len());
                let elf =
                    ::object::read::elf::ElfFile64::<::object::Endianness>::parse(&*bytes).unwrap();
                let endian = elf.endian();
                for section in &entries {
                    let SectionFlags::Elf { sh_flags } = section.flags() else {
                        panic!("ELF flags");
                    };
                    assert_ne!(sh_flags & u64::from(::object::elf::SHF_LINK_ORDER), 0);
                    let (_, reloc) = section.relocations().min_by_key(|(o, _)| *o).unwrap();
                    let RelocationTarget::Symbol(symbol) = reloc.target() else {
                        panic!("a relocation without a symbol");
                    };
                    let target = file.symbol_by_index(symbol).unwrap();
                    targets.push(target.name().unwrap().to_owned());
                    let function_section = target.section_index().unwrap();
                    let header = elf.elf_section_table().section(section.index()).unwrap();
                    assert_eq!(
                        usize::try_from(header.sh_link.get(endian)).unwrap(),
                        function_section.0,
                        "{section_name}"
                    );
                    // The function's section holds it alone.
                    let others = file
                        .symbols()
                        .filter(|s| {
                            s.section_index() == Some(function_section)
                                && s.kind() == ::object::SymbolKind::Text
                        })
                        .count();
                    assert_eq!(others, 1, "{:?}", target.name());
                }
            }
        }
        targets.sort();
        let mut wanted = function_names.clone();
        wanted.sort();
        assert_eq!(targets, wanted, "{format:?}");
    }
}

/// D-1253 with D-1112: the Linux link line the driver runs holds
/// `-Wl,-z,start-stop-gc` after `--gc-sections`, checked on the command
/// itself on every host; the macOS line holds `-dead_strip` and not the
/// ELF flag.
#[test]
fn link_gc_linux_link_line_has_start_stop_gc() {
    let objects = vec![("app".to_owned(), Vec::new())];
    let job = LinkJob {
        objects: &objects,
        c_objects: &[],
        libraries: &[],
        archive: Path::new("/archive/libcheby_rt_exe.a"),
        output: Path::new("/out/app"),
    };
    let names = vec!["app.o".to_owned(), "cb$program.o".to_owned()];
    let cmd = command(
        LinkHost::Linux,
        &job,
        Path::new("/objects"),
        Path::new("/out/.stage/app"),
        &names,
    );
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let gc = args.iter().position(|a| a == "-Wl,--gc-sections").unwrap();
    let start_stop = args.iter().position(|a| a == START_STOP_GC).unwrap();
    assert!(gc < start_stop, "{args:?}");
    assert_eq!(START_STOP_GC, "-Wl,-z,start-stop-gc");
    let output = args.iter().position(|a| a == "-o").unwrap();
    assert!(start_stop < output, "{args:?}");
    assert!(
        !flags_for(LinkHost::MacOs)
            .iter()
            .any(|a| a == START_STOP_GC)
    );
    assert!(
        flags_for(LinkHost::MacOs)
            .iter()
            .any(|a| a == "-Wl,-dead_strip")
    );
    if LinkHost::current() == LinkHost::Linux {
        assert!(crate::link::flags().iter().any(|a| a == START_STOP_GC));
    }
}

/// Each record's key is 12 hex digits and its tag is never 0, and distinct
/// paths get distinct keys and tags (the program object refuses two
/// records whose key or tag collide instead of mixing their entries,
/// `crate::program`); every Mach-O object holds one entries' section, so
/// a program of any number of modules stays under Mach-O's 255 sections.
#[test]
fn link_gc_keys_name_one_section_per_record() {
    let paths = [
        "app",
        "app::util",
        "std::io",
        "std::list",
        "std::map",
        "app::a::b",
    ];
    let mut keys: Vec<String> = paths
        .iter()
        .map(|p| gc::key(&mangle::module_record(p)))
        .collect();
    for key in &keys {
        assert_eq!(key.len(), 12);
    }
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), paths.len());
    let mut tags: Vec<u32> = paths
        .iter()
        .map(|p| gc::tag(&mangle::module_record(p)))
        .collect();
    assert!(tags.iter().all(|&t| t != 0));
    tags.sort_unstable();
    tags.dedup();
    assert_eq!(tags.len(), paths.len());
    assert!(gc::MACHO_SECTION.len() <= 16, "Mach-O's 16 bytes");
    let (start, end) = gc::bounds(::object::BinaryFormat::MachO, &keys[0]);
    assert_eq!(
        (start, end),
        gc::bounds(::object::BinaryFormat::MachO, &keys[1])
    );
}
