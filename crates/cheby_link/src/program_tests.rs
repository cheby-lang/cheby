//! Program-object tests `link_program_*` (D-915, D-917, D-925, D-930):
//! the program object of a hand-built entries record, read back with the
//! `object` crate, and linked by `cc` against a stub archive.
//!
//! Mach-O is written on aarch64 hosts only: macOS is aarch64 only
//! (D-892), so on an x86-64 host these tests cover ELF alone.

#![expect(
    clippy::arithmetic_side_effects,
    reason = "test offsets on small, known values"
)]

use std::collections::BTreeMap;
use std::path::Path;

use ::object::read::{Object as _, ObjectSection as _, ObjectSymbol as _};
use ::object::{RelocationFlags, RelocationTarget, SectionKind, SymbolScope, elf, macho};

use cheby_codegen_clif::isa;
use cheby_codegen_clif::record::{
    CodeRecord, RecordFunction, RecordSymbol, SymbolKind, SymbolLinkage,
};
use cheby_runtime::abi;

use crate::LinkError;
use crate::program::{ProgramSpec, program_object};

/// The object formats written for the host architecture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Elf,
    MachO,
}

impl Format {
    /// ELF, and Mach-O on aarch64 hosts only (D-892).
    const ALL: &[Self] = if cfg!(target_arch = "aarch64") {
        &[Self::Elf, Self::MachO]
    } else {
        &[Self::Elf]
    };

    /// The host's own format, the one `cc` links.
    fn host() -> Self {
        if cfg!(target_os = "macos") {
            Self::MachO
        } else {
            Self::Elf
        }
    }

    fn isa(self) -> cranelift_codegen::isa::OwnedTargetIsa {
        let arch = std::env::consts::ARCH;
        let triple = match self {
            Self::Elf => format!("{arch}-unknown-linux-gnu"),
            Self::MachO => format!("{arch}-apple-darwin"),
        };
        isa::isa_for(&triple).unwrap()
    }

    /// A symbol's name as the object spells it.
    fn strip(self, name: &str) -> &str {
        match self {
            Self::Elf => name,
            Self::MachO => name.strip_prefix('_').unwrap_or(name),
        }
    }
}

const ENTRY: &str = "cb$entry$main$app";
const RECORDS: [&str; 2] = ["cb$mod$app", "cb$mod$std$io"];

/// A function that only returns, for the host architecture.
fn ret() -> Vec<u8> {
    if cfg!(target_arch = "aarch64") {
        vec![0xc0, 0x03, 0x5f, 0xd6]
    } else {
        vec![0xc3]
    }
}

/// An entries record by hand: `main`'s entry thunk and the trampolines of
/// `arities`, each a function that returns, all local.
fn entries(arities: &[u32]) -> CodeRecord {
    let mut names = vec![ENTRY.to_owned()];
    names.extend(arities.iter().map(|n| format!("cb$tramp${n}")));
    let symbols = names
        .iter()
        .map(|name| RecordSymbol {
            name: name.clone(),
            kind: SymbolKind::Function,
            linkage: SymbolLinkage::Local,
        })
        .collect();
    let functions = (0_u32..)
        .zip(&names)
        .map(|(symbol, _)| RecordFunction {
            symbol,
            align: 16,
            bytes: ret(),
            relocs: Vec::new(),
            positions: Vec::new(),
        })
        .collect();
    CodeRecord {
        symbols,
        functions,
        data: Vec::new(),
        source: None,
    }
}

fn records() -> Vec<String> {
    RECORDS.map(str::to_owned).to_vec()
}

fn spec(records: &[String]) -> ProgramSpec<'_> {
    ProgramSpec {
        module_records: records,
        entry: ENTRY,
        main_stack: 8 << 20,
        fiber_stack: 1 << 20,
        unwind_edges: true,
        stubs: &[],
        constants: &[],
    }
}

fn program(format: Format) -> Vec<u8> {
    let records = records();
    program_object(format.isa(), &spec(&records), &entries(&[0, 2])).unwrap()
}

/// Every symbol of the object, by its name without Mach-O's underscore:
/// whether it is defined, and its scope.
fn symbols(format: Format, bytes: &[u8]) -> BTreeMap<String, (bool, SymbolScope)> {
    let file = ::object::File::parse(bytes).unwrap();
    file.symbols()
        .filter(|s| !s.name().unwrap_or("").is_empty() && s.kind() != ::object::SymbolKind::File)
        .filter(|s| s.kind() != ::object::SymbolKind::Section)
        .map(|s| {
            (
                format.strip(s.name().unwrap()).to_owned(),
                (s.is_definition(), s.scope()),
            )
        })
        .collect()
}

/// `main` is the only exported symbol; `cb$image`, the entry thunk, the
/// trampolines and the import words are local definitions; the module
/// records, `cheby_rt_main` and the runtime ID's symbol are imports.
#[test]
fn link_program_symbols_and_linkage() {
    for &format in Format::ALL {
        let bytes = program(format);
        let symbols = symbols(format, &bytes);
        assert_eq!(
            symbols.get("main"),
            Some(&(true, SymbolScope::Dynamic)),
            "{format:?}: {symbols:?}"
        );
        for (name, (defined, scope)) in &symbols {
            if *defined && name != "main" {
                assert_eq!(*scope, SymbolScope::Compilation, "{format:?}: {name}");
            }
        }
        let rt_id_word = format!("cb$imp${}", abi::sym::RUNTIME_ID_SYMBOL);
        for local in [
            "cb$image",
            ENTRY,
            "cb$tramp$0",
            "cb$tramp$2",
            "cb$imp$cheby_rt_main",
            rt_id_word.as_str(),
        ] {
            assert_eq!(
                symbols.get(local).map(|s| s.0),
                Some(true),
                "{format:?}: {local}"
            );
        }
        for import in RECORDS
            .iter()
            .copied()
            .chain(["cheby_rt_main", abi::sym::RUNTIME_ID_SYMBOL])
        {
            assert_eq!(
                symbols.get(import).map(|s| s.0),
                Some(false),
                "{format:?}: {import}"
            );
        }
        assert!(!symbols.contains_key("cb$tramp$1"), "{format:?}");
    }
}

/// The relocations of the section that holds `name`, at offsets from
/// `name`'s start, with their targets' names (a section for a target the
/// format names by section), and the symbol's bytes.
fn relocs_of(format: Format, bytes: &[u8], name: &str) -> (BTreeMap<u64, Option<String>>, Vec<u8>) {
    let file = ::object::File::parse(bytes).unwrap();
    let symbol = file
        .symbols()
        .find(|s| s.name().is_ok_and(|n| format.strip(n) == name))
        .unwrap();
    let section = file
        .section_by_index(symbol.section_index().unwrap())
        .unwrap();
    let start = symbol.address() - section.address();
    // The image's 120 bytes, its table of two module records and their
    // two function bounds (D-1253).
    let size = IMAGE_WITH_TABLES;
    let data = section.data().unwrap();
    let mut relocs = BTreeMap::new();
    for (offset, reloc) in section.relocations() {
        if offset < start || offset >= start + size {
            continue;
        }
        let target = match reloc.target() {
            RelocationTarget::Symbol(index) => file
                .symbol_by_index(index)
                .ok()
                .and_then(|s| s.name().ok().map(|n| format.strip(n).to_owned()))
                .filter(|n| !n.is_empty()),
            _ => None,
        };
        relocs.insert(offset - start, target);
    }
    let end = usize::try_from(start + IMAGE_WITH_TABLES).unwrap();
    let image = data[usize::try_from(start).unwrap()..end.min(data.len())].to_vec();
    (relocs, image)
}

/// `cb$image`'s 120 bytes, its table of [`RECORDS`]' two pointers and
/// their two function bounds of 24 bytes each (D-1253).
const IMAGE_WITH_TABLES: u64 = 120 + 16 + 48;

fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// `cb$image` holds the version, `unwind_edges`, two module records in
/// build order, the stacks, an empty constants table and the two records'
/// function bounds (D-1253), with relocations to its own module and
/// bounds tables, the entry thunk, the trampolines the program has (0 and
/// 2), the module records and the linker's bounds of each record's
/// entries' section (`crate::gc`), and none where a trampoline is missing.
#[test]
fn link_program_image_words() {
    for &format in Format::ALL {
        let bytes = program(format);
        let (relocs, image) = relocs_of(format, &bytes, "cb$image");
        assert_eq!(
            u32::from_le_bytes(image[0..4].try_into().unwrap()),
            abi::IMAGE_VERSION
        );
        assert_eq!(u32::from_le_bytes(image[4..8].try_into().unwrap()), 1);
        assert_eq!(word(&image, 8), 2, "{format:?}: two module records");
        assert_eq!(word(&image, 72), 8 << 20);
        assert_eq!(word(&image, 80), 1 << 20);
        assert_eq!(word(&image, 88), 0, "{format:?}: no constants");
        assert_eq!(word(&image, 96), 0, "{format:?}: no constants");
        assert_eq!(word(&image, 104), 2, "{format:?}: two function bounds");
        let offsets: Vec<u64> = relocs.keys().copied().collect();
        assert_eq!(
            offsets,
            [16, 24, 32, 48, 112, 120, 128, 136, 144, 160, 168],
            "{format:?}: {relocs:?}"
        );
        for (offset, wanted) in [(120, RECORDS[0]), (128, RECORDS[1])] {
            assert_eq!(
                relocs[&offset].as_deref(),
                Some(wanted),
                "{format:?}: {relocs:?}"
            );
        }
        let object_format = match format {
            Format::Elf => ::object::BinaryFormat::Elf,
            Format::MachO => ::object::BinaryFormat::MachO,
        };
        for (at, record) in [(136, RECORDS[0]), (160, RECORDS[1])] {
            let (start, end) = crate::gc::bounds(object_format, &crate::gc::key(record));
            assert_eq!(relocs[&at].as_deref(), Some(start.as_str()), "{format:?}");
            assert_eq!(
                relocs[&(at + 8)].as_deref(),
                Some(end.as_str()),
                "{format:?}"
            );
            // The tag: the record's on Mach-O, whose entries share one
            // section; 0 on ELF, whose entries' section is the record's own.
            let at = usize::try_from(at).unwrap();
            let tag = u32::from_le_bytes(image[at + 16..at + 20].try_into().unwrap());
            assert_eq!(
                tag,
                crate::gc::image_tag(object_format, record),
                "{format:?}"
            );
            assert_eq!(&image[at + 20..at + 24], &[0; 4], "{format:?}");
        }
        if let Some(Some(entry)) = relocs.get(&24) {
            assert_eq!(entry, ENTRY, "{format:?}");
        }
    }
}

/// Whether a relocation in text is PC-relative (D-913).
/// D-979: the constants table follows the tables of module records and
/// function bounds in `cb$image`, its count at 88 and a relocation to it
/// at 96; the rest of the image is unchanged.
#[test]
fn link_program_constants_follow_the_module_table() {
    for &format in Format::ALL {
        let records = records();
        let table = b"CHBT\x00tail";
        let spec = ProgramSpec {
            constants: table,
            ..spec(&records)
        };
        let bytes = program_object(format.isa(), &spec, &entries(&[0, 2])).unwrap();
        let (relocs, image) = relocs_of(format, &bytes, "cb$image");
        assert_eq!(word(&image, 88), 9, "{format:?}");
        let file = ::object::File::parse(&*bytes).unwrap();
        let symbol = file
            .symbols()
            .find(|s| s.name().is_ok_and(|n| format.strip(n) == "cb$image"))
            .unwrap();
        let section = file
            .section_by_index(symbol.section_index().unwrap())
            .unwrap();
        let start = usize::try_from(symbol.address() - section.address()).unwrap();
        let data = section.data().unwrap();
        assert_eq!(&data[start + 184..start + 193], table, "{format:?}");
        let offsets: Vec<u64> = relocs.keys().copied().collect();
        assert_eq!(
            offsets,
            [16, 24, 32, 48, 96, 112, 120, 128, 136, 144, 160, 168],
            "{format:?}: {relocs:?}"
        );
    }
}

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

/// `main`'s text holds only PC-relative relocations (D-913): the image,
/// `cheby_rt_main`'s import word and the runtime ID's import word.
#[test]
fn link_program_text_is_pc_relative() {
    for &format in Format::ALL {
        let bytes = program(format);
        let file = ::object::File::parse(&*bytes).unwrap();
        let mut text_relocs = 0;
        for section in file.sections() {
            if section.kind() != SectionKind::Text {
                continue;
            }
            for (offset, reloc) in section.relocations() {
                assert!(
                    pc_relative(reloc.flags()),
                    "{format:?}: {:?} at {offset}",
                    reloc.flags()
                );
                text_relocs += 1;
            }
        }
        // Three addresses, each one or two relocations.
        assert!(text_relocs >= 3, "{format:?}: {text_relocs}");
    }
}

/// The same inputs give the same bytes, and the object names no path of
/// the build (D-264, D-934).
#[test]
fn link_program_is_byte_stable() {
    for &format in Format::ALL {
        let first = program(format);
        assert_eq!(first, program(format), "{format:?}");
        let cwd = std::env::current_dir().unwrap();
        let cwd = cwd.to_string_lossy();
        assert!(
            !first.windows(cwd.len()).any(|w| w == cwd.as_bytes()),
            "{format:?} holds {cwd}"
        );
    }
}

/// An entries record without the entry thunk, or with a global
/// definition, is refused without panicking.
#[test]
fn link_program_refuses_bad_entries() {
    let format = Format::host();
    let records = records();
    let missing = ProgramSpec {
        entry: "cb$entry$main$other",
        ..spec(&records)
    };
    let err = program_object(format.isa(), &missing, &entries(&[])).unwrap_err();
    assert!(err.to_string().contains("cb$entry$main$other"), "{err}");
    let mut hidden = entries(&[0]);
    hidden.symbols[1].linkage = SymbolLinkage::Hidden;
    let err = program_object(format.isa(), &spec(&records), &hidden).unwrap_err();
    assert!(matches!(err, LinkError::Object(_)), "{err:?}");
}

/// D-918's check: equal local copies pass, differing ones are named.
#[test]
fn link_program_copies_are_checked() {
    let a = entries(&[0]);
    let b = entries(&[0]);
    crate::link::check_copies(&[("a", &a), ("b", &b)]).unwrap();
    let mut c = entries(&[0]);
    c.functions[1].bytes.push(0);
    let err = crate::link::check_copies(&[("a", &a), ("c", &c)]).unwrap_err();
    assert!(
        err.to_string().contains("`cb$tramp$0` in `a` and `c`"),
        "{err}"
    );
}

/// Runs `cmd`, failing the test with its output if it fails.
fn run(cmd: &mut std::process::Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A stub runtime archive in `dir`: `cheby_rt_main` returns 7 when it gets
/// an image, and the runtime ID's symbol only when `with_id`.
fn stub_archive(dir: &Path, with_id: bool) -> std::path::PathBuf {
    let mut c = String::from(
        "int cheby_rt_main(int argc, char **argv, const void *image) {\n\
         (void)argc; (void)argv; return image ? 7 : 1; }\n",
    );
    if with_id {
        let symbol = abi::sym::RUNTIME_ID_SYMBOL;
        c.push_str("const unsigned char ");
        c.push_str(symbol);
        c.push_str("[16] = {0};\n");
    }
    std::fs::write(dir.join("stub.c"), c).unwrap();
    run(std::process::Command::new("cc")
        .current_dir(dir)
        .args(["-c", "-fPIC", "-o", "stub.o", "stub.c"]));
    let archive = dir.join("libstub.a");
    let _ = std::fs::remove_file(&archive);
    run(std::process::Command::new("ar")
        .current_dir(dir)
        .args(["crs", "libstub.a", "stub.o"]));
    archive
}

/// The program object imports the runtime ID's symbol, so a link against
/// an archive without it fails even when `cheby build`'s check of the
/// archive is bypassed (D-930); with the symbol, the same objects link
/// into an executable whose `main` returns `cheby_rt_main`'s status with
/// the image (D-925).
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn link_program_imports_runtime_id() {
    let dir = std::env::temp_dir().join(format!("cheby-link-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let format = Format::host();
    let no_records: Vec<String> = Vec::new();
    let object = program_object(format.isa(), &spec(&no_records), &entries(&[0])).unwrap();
    let objects = vec![("cb$program".to_owned(), object)];
    let output = dir.join("prog");

    let stale = stub_archive(&dir, false);
    let err = crate::link::link(&crate::link::LinkJob {
        objects: &objects,
        c_objects: &[],
        libraries: &[],
        archive: &stale,
        output: &output,
    })
    .unwrap_err();
    let LinkError::LinkFailed { output: text, .. } = &err else {
        panic!("not a failed link: {err:?}");
    };
    assert!(text.contains("cheby_rt_id$"), "{text}");
    assert!(!output.exists());

    let good = stub_archive(&dir, true);
    crate::link::link(&crate::link::LinkJob {
        objects: &objects,
        c_objects: &[],
        libraries: &[],
        archive: &good,
        output: &output,
    })
    .unwrap();
    let status = std::process::Command::new(&output).status().unwrap();
    assert_eq!(status.code(), Some(7));
    // Only the executable is left in the output directory.
    let left: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with(".cheby-link"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// D-943's stubs of the tests below, sorted.
const STUBS: [&str; 2] = ["missing_alpha", "missing_beta"];

/// The program object of [`program`] with [`STUBS`].
fn program_with_stubs(format: Format) -> Vec<u8> {
    let records = records();
    let stubs: Vec<String> = STUBS.map(str::to_owned).to_vec();
    let spec = ProgramSpec {
        stubs: &stubs,
        ..spec(&records)
    };
    program_object(format.isa(), &spec, &entries(&[0, 2])).unwrap()
}

/// FNV-1a over `bytes` with the runtime ID's symbol (D-930), which every
/// edit of the runtime's sources changes, read as zeros. Over an object's
/// raw bytes this is not enough for a pin: see [`layout_free`].
fn fnv(bytes: &[u8]) -> u64 {
    let id = abi::sym::RUNTIME_ID_SYMBOL.as_bytes();
    let mut normal = bytes.to_vec();
    let mut at = 0;
    while let Some(found) = normal[at..].windows(id.len()).position(|w| w == id) {
        let start = at + found + "cheby_rt_id$".len();
        normal[start..at + found + id.len()].fill(b'0');
        at += found + id.len();
    }
    normal.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The object as a reader sees it, without its string tables' layout:
/// the header's facts, every section (index, name, kind, alignment,
/// address, size, its bytes unless it is a symbol, string or relocation
/// table, and its relocations with their targets by name) and every
/// symbol in table order (name, kind, section, scope, address, size,
/// weak, flags).
///
/// `object` writes a string table sorted from each string's last byte
/// (for suffix sharing), so the runtime ID's digits decide where the
/// other names land, and every name's offset with them: an object's raw
/// bytes change with an edit of the runtime's sources even when [`fnv`]
/// reads the ID as zeros. Everything else the object holds is here.
fn layout_free(format: Format, bytes: &[u8]) -> Vec<u8> {
    use std::fmt::Write as _;
    let file = ::object::File::parse(bytes).unwrap();
    let name_of = |index| {
        file.symbol_by_index(index)
            .ok()
            .and_then(|s| s.name().ok().map(|n| format.strip(n).to_owned()))
            .unwrap_or_default()
    };
    let mut out = String::new();
    writeln!(
        out,
        "{:?} {:?} {:?} {}",
        file.format(),
        file.architecture(),
        file.flags(),
        file.is_little_endian()
    )
    .unwrap();
    for section in file.sections() {
        writeln!(
            out,
            "section {} {} {:?} align {} address {} size {}",
            section.index().0,
            section.name().unwrap(),
            section.kind(),
            section.align(),
            section.address(),
            section.size()
        )
        .unwrap();
        if section.kind() != SectionKind::Metadata {
            writeln!(out, "  data {:02x?}", section.data().unwrap()).unwrap();
        }
        for (offset, reloc) in section.relocations() {
            let target = match reloc.target() {
                RelocationTarget::Symbol(index) => format!("symbol {}", name_of(index)),
                RelocationTarget::Section(index) => format!("section {}", index.0),
                other => format!("{other:?}"),
            };
            writeln!(
                out,
                "  reloc {offset} {:?} {target} addend {} implicit {}",
                reloc.flags(),
                reloc.addend(),
                reloc.has_implicit_addend()
            )
            .unwrap();
        }
    }
    for symbol in file.symbols() {
        writeln!(
            out,
            "symbol {} {} {:?} {:?} {:?} address {} size {} weak {} {:?}",
            symbol.index().0,
            name_of(symbol.index()),
            symbol.kind(),
            symbol.section(),
            symbol.scope(),
            symbol.address(),
            symbol.size(),
            symbol.is_weak(),
            symbol.flags()
        )
        .unwrap();
    }
    out.into_bytes()
}

/// D-943: each stub is a defined function, global with hidden visibility
/// (scope `Linkage`: `STV_HIDDEN` on ELF, private extern on Mach-O), never
/// local and never exported; `main` stays the only exported symbol;
/// `cheby_rt_unresolved` is an import and its import word a local
/// definition.
#[test]
fn program_stubs_are_global_and_hidden() {
    for &format in Format::ALL {
        let bytes = program_with_stubs(format);
        let file = ::object::File::parse(&*bytes).unwrap();
        for stub in STUBS {
            let symbol = file
                .symbols()
                .find(|s| s.name().is_ok_and(|n| format.strip(n) == stub))
                .unwrap_or_else(|| panic!("{format:?}: no `{stub}`"));
            assert!(symbol.is_definition(), "{format:?}: {stub}");
            assert!(symbol.is_global(), "{format:?}: {stub}");
            assert_eq!(symbol.kind(), ::object::SymbolKind::Text, "{format:?}");
            assert_eq!(symbol.scope(), SymbolScope::Linkage, "{format:?}: {stub}");
            match symbol.flags() {
                ::object::SymbolFlags::Elf { st_other, .. } => {
                    assert_eq!(st_other & 0x3, elf::STV_HIDDEN, "{format:?}: {stub}");
                }
                ::object::SymbolFlags::MachO { .. } => {}
                other => panic!("{format:?}: {other:?}"),
            }
        }
        let symbols = symbols(format, &bytes);
        let exported: Vec<&String> = symbols
            .iter()
            .filter(|(_, (defined, scope))| *defined && *scope == SymbolScope::Dynamic)
            .map(|(name, _)| name)
            .collect();
        assert_eq!(exported, ["main"], "{format:?}");
        assert_eq!(
            symbols.get(abi::sym::UNRESOLVED).map(|s| s.0),
            Some(false),
            "{format:?}"
        );
        assert_eq!(
            symbols.get("cb$imp$cheby_rt_unresolved"),
            Some(&(true, SymbolScope::Compilation)),
            "{format:?}"
        );
    }
}

/// The relocations of `section` within `[start, end)` of its addresses,
/// with their flags and their targets' names (`None` for a section).
fn relocs_in(
    format: Format,
    file: &::object::File<'_>,
    section: &::object::Section<'_, '_>,
    start: u64,
    end: u64,
) -> Vec<(RelocationFlags, Option<String>)> {
    let base = section.address();
    section
        .relocations()
        .filter(|(offset, _)| (start..end).contains(&(base + offset)))
        .map(|(_, reloc)| {
            let target = match reloc.target() {
                RelocationTarget::Symbol(index) => file
                    .symbol_by_index(index)
                    .ok()
                    .and_then(|s| s.name().ok().map(|n| format.strip(n).to_owned()))
                    .filter(|n| !n.is_empty()),
                _ => None,
            };
            (reloc.flags(), target)
        })
        .collect()
}

/// D-943: a stub's text reaches its import word PC-relative, and only it
/// (D-913); the import word holds one absolute data relocation, to
/// `cheby_rt_unresolved`.
#[test]
fn program_stubs_call_unresolved_through_an_import_word() {
    for &format in Format::ALL {
        let bytes = program_with_stubs(format);
        let file = ::object::File::parse(&*bytes).unwrap();
        let find = |name: &str| {
            file.symbols()
                .find(|s| s.name().is_ok_and(|n| format.strip(n) == name))
                .unwrap_or_else(|| panic!("{format:?}: no `{name}`"))
        };
        for stub in STUBS {
            let symbol = find(stub);
            let section = file
                .section_by_index(symbol.section_index().unwrap())
                .unwrap();
            // Up to the next symbol of the section, or the section's end.
            let end = file
                .symbols()
                .filter(|s| s.section_index() == symbol.section_index())
                .map(|s| s.address())
                .filter(|&a| a > symbol.address())
                .min()
                .unwrap_or(section.address() + section.size());
            let relocs = relocs_in(format, &file, &section, symbol.address(), end);
            assert!(!relocs.is_empty(), "{format:?}: {stub}");
            for (flags, target) in &relocs {
                assert!(pc_relative(*flags), "{format:?}: {stub}: {flags:?}");
                if let Some(target) = target {
                    assert_eq!(target, "cb$imp$cheby_rt_unresolved", "{format:?}");
                }
            }
        }
        let word = find("cb$imp$cheby_rt_unresolved");
        let section = file
            .section_by_index(word.section_index().unwrap())
            .unwrap();
        let relocs = relocs_in(format, &file, &section, word.address(), word.address() + 8);
        assert_eq!(relocs.len(), 1, "{format:?}: {relocs:?}");
        assert_eq!(
            relocs[0].1.as_deref(),
            Some(abi::sym::UNRESOLVED),
            "{format:?}"
        );
        assert!(!pc_relative(relocs[0].0), "{format:?}");
    }
}

/// A program without stubs is the object it was without them: its length
/// and its content apart from the string tables' layout ([`layout_free`])
/// pinned (FNV-1a, per format; taken again in wave 10 with D-1253's
/// function bounds), and no import of `cheby_rt_unresolved`.
#[test]
fn program_stubs_none_keeps_the_object() {
    for &format in Format::ALL {
        let bytes = program(format);
        if cfg!(target_arch = "aarch64") {
            let pinned = match format {
                Format::Elf => PINNED_ELF_AARCH64,
                Format::MachO => PINNED_MACHO_AARCH64,
            };
            // The pins hold version 1 of the image; K5a raised it to 2 for
            // the records' constants tables (D-1189), the
            // only word of the object that changed.
            let content = fnv(&layout_free(format, &with_version_one(format, &bytes)));
            assert_eq!((bytes.len(), content), pinned, "{format:?}");
        }
        let symbols = symbols(format, &bytes);
        assert!(!symbols.contains_key(abi::sym::UNRESOLVED), "{format:?}");
        assert!(
            !symbols.contains_key("cb$imp$cheby_rt_unresolved"),
            "{format:?}"
        );
    }
}

/// `bytes` with `cb$image`'s version word set to 1, the version the pins
/// below were taken with.
fn with_version_one(format: Format, bytes: &[u8]) -> Vec<u8> {
    let file = ::object::File::parse(bytes).unwrap();
    let image = file
        .symbols()
        .find(|s| s.name().is_ok_and(|n| format.strip(n) == "cb$image"))
        .unwrap();
    let section = file
        .section_by_index(image.section_index().unwrap())
        .unwrap();
    let (start, _) = section.file_range().unwrap();
    let at = usize::try_from(start + image.address() - section.address()).unwrap();
    let mut out = bytes.to_vec();
    assert_eq!(
        u32::from_le_bytes(out[at..at + 4].try_into().unwrap()),
        abi::IMAGE_VERSION
    );
    out[at..at + 4].copy_from_slice(&1_u32.to_le_bytes());
    out
}

/// [`program`]'s object, as `(length, FNV-1a of` [`layout_free`]`)`, on an
/// aarch64 host. Wave 10 (N9, D-1253) took these pins again when the
/// image gained its function bounds: 16 more bytes of header, two bounds
/// of 24 bytes each (start, end, tag) with their relocations, and the
/// imported bounds symbols, four on ELF and the shared section's two on
/// Mach-O (`crate::gc`); `bd16c14`'s pins were (1744,
/// 6805927779267993231) for ELF and (1128, 11813137239158497229) for
/// Mach-O. At `bd16c14` the raw bytes were first
/// pinned by this file's [`program`] on a copy of `bd16c14`'s tree with
/// the stubs added (FNV-1a 11913639352118050896 for ELF and
/// 18436484272339440522 for Mach-O, with the runtime ID read as zeros).
/// These pins were taken where those raw pins still held, and are the
/// same under another runtime ID whose digits move the string tables.
/// Cranelift's x86-64 backend is built only on x86-64 hosts, so no pin
/// was taken there.
const PINNED_ELF_AARCH64: (usize, u64) = (2128, 5_914_861_851_712_781_503);
const PINNED_MACHO_AARCH64: (usize, u64) = (1312, 10_231_809_143_618_702_932);

/// Stubs that are not sorted and distinct, or that name a generated
/// symbol, `main` or a runtime internal, are refused without panicking.
#[test]
fn program_stubs_refuses_bad_names() {
    let format = Format::host();
    let records = records();
    for stubs in [
        &["b", "a"][..],
        &["a", "a"],
        &["cb$fn$app$f"],
        &["main"],
        &["cheby_rt_main"],
        &[""],
    ] {
        let stubs: Vec<String> = stubs.iter().map(|&s| s.to_owned()).collect();
        let spec = ProgramSpec {
            stubs: &stubs,
            ..spec(&records)
        };
        let err = program_object(format.isa(), &spec, &entries(&[0])).unwrap_err();
        assert!(matches!(err, LinkError::Object(_)), "{stubs:?}: {err:?}");
    }
}

/// D-943 end to end: an archive whose `cheby_rt_main` calls `missing_beta`,
/// which nothing but the program object's stub defines, links; the stub
/// calls the archive's `cheby_rt_unresolved`, whose result `main`
/// returns; and the executable exports neither stub.
#[test]
#[cfg_attr(windows, ignore = "executables on Windows are M6 (D-935)")]
fn program_stubs_link_and_reach_unresolved() {
    let dir = std::env::temp_dir().join(format!("cheby-link-stubs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let format = Format::host();
    let no_records: Vec<String> = Vec::new();
    let stubs: Vec<String> = STUBS.map(str::to_owned).to_vec();
    let spec = ProgramSpec {
        stubs: &stubs,
        ..spec(&no_records)
    };
    let object = program_object(format.isa(), &spec, &entries(&[0])).unwrap();
    let objects = vec![("cb$program".to_owned(), object)];
    let symbol = abi::sym::RUNTIME_ID_SYMBOL;
    let c = format!(
        "long missing_beta(void);\n\
         long cheby_rt_unresolved(void) {{ return 9; }}\n\
         int cheby_rt_main(int argc, char **argv, const void *image) {{\n\
         (void)argc; (void)argv; return image ? (int)missing_beta() : 1; }}\n\
         const unsigned char {symbol}[16] = {{0}};\n"
    );
    std::fs::write(dir.join("stub.c"), c).unwrap();
    run(std::process::Command::new("cc")
        .current_dir(&dir)
        .args(["-c", "-fPIC", "-o", "stub.o", "stub.c"]));
    let _ = std::fs::remove_file(dir.join("libstub.a"));
    run(std::process::Command::new("ar")
        .current_dir(&dir)
        .args(["crs", "libstub.a", "stub.o"]));
    let output = dir.join("prog");
    crate::link::link(&crate::link::LinkJob {
        objects: &objects,
        c_objects: &[],
        libraries: &[],
        archive: &dir.join("libstub.a"),
        output: &output,
    })
    .unwrap();
    let status = std::process::Command::new(&output).status().unwrap();
    assert_eq!(status.code(), Some(9));
    let exe = std::fs::read(&output).unwrap();
    let file = ::object::File::parse(&*exe).unwrap();
    let exports: Vec<String> = file
        .exports()
        .unwrap()
        .iter()
        .map(|e| String::from_utf8_lossy(e.name()).into_owned())
        .collect();
    let dynamic: Vec<String> = file
        .dynamic_symbols()
        .filter_map(|s| s.name().ok().map(str::to_owned))
        .collect();
    for stub in STUBS {
        assert!(
            !exports
                .iter()
                .chain(&dynamic)
                .any(|n| format.strip(n) == stub),
            "{stub}: {exports:?} {dynamic:?}"
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
