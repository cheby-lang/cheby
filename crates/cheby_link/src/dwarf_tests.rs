//! Tests of the DWARF line tables in replayed objects (D-938,
//! GAP(dwarf-minimal-info)): a record with positions replayed into an ELF
//! and a Mach-O object, its sections relocated and read back with
//! `gimli`'s reader, each row found at its function's address, an offset
//! mapped to its line, no absolute path and no `DW_AT_comp_dir`, two
//! replays identical, and no DWARF for a record without positions.
//! Mach-O is replayed on aarch64 hosts only (macOS is aarch64 only,
//! D-892), as in `object_tests.rs`.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "test arithmetic on small, known values"
)]

use std::borrow::Cow;
use std::collections::BTreeMap;

use ::object::read::{Object as _, ObjectSection as _, ObjectSymbol as _};
use ::object::{RelocationTarget, SymbolKind};

use cheby_codegen_clif::SourceRow;
use cheby_codegen_clif::isa;
use cheby_codegen_clif::record::{
    CodeRecord, RecordFunction, RecordSymbol, SymbolKind as RecordKind, SymbolLinkage,
};

use crate::object::replay;

/// The formats replay writes on this host (as `object_tests.rs`'s).
fn formats() -> Vec<&'static str> {
    if cfg!(target_arch = "aarch64") {
        vec!["unknown-linux-gnu", "apple-darwin"]
    } else {
        vec!["unknown-linux-gnu"]
    }
}

fn triple(os: &str) -> String {
    format!("{}-{os}", std::env::consts::ARCH)
}

fn row(offset: u32, line: u32, column: u32) -> SourceRow {
    SourceRow {
        offset,
        line,
        column,
    }
}

/// A record by hand: `f` (64 bytes, local) and `g` (32 bytes, hidden)
/// with rows, `h` (16 bytes) without, in `src/main.cheby`. Replay copies
/// code bytes, so zeros serve.
fn record() -> CodeRecord {
    let symbol = |name: &str, linkage| RecordSymbol {
        name: name.to_owned(),
        kind: RecordKind::Function,
        linkage,
    };
    let function = |symbol, len, positions| RecordFunction {
        symbol,
        align: 16,
        bytes: vec![0; len],
        relocs: Vec::new(),
        positions,
    };
    CodeRecord {
        symbols: vec![
            symbol("cb$fn$app$f", SymbolLinkage::Local),
            symbol("cb$fn$app$g", SymbolLinkage::Hidden),
            symbol("cb$fn$app$h", SymbolLinkage::Local),
        ],
        functions: vec![
            function(0, 64, vec![row(0, 3, 4), row(8, 4, 3), row(20, 5, 7)]),
            function(1, 32, vec![row(0, 9, 4), row(16, 7, 11)]),
            function(2, 16, Vec::new()),
        ],
        data: Vec::new(),
        source: Some("src/main.cheby".to_owned()),
    }
}

/// The DWARF sections of the object `bytes`, by their ELF names
/// (`.debug_line`), with every relocation applied: an address is its
/// symbol's address in the object plus the addend (explicit, or implicit
/// in Mach-O), as a linker would place them.
fn relocated(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let file = ::object::File::parse(bytes).unwrap();
    let mut out = BTreeMap::new();
    for section in file.sections() {
        let name = section.name().unwrap();
        let Some(rest) = name
            .strip_prefix(".debug_")
            .or_else(|| name.strip_prefix("__debug_"))
        else {
            continue;
        };
        let mut data = section.data().unwrap().to_vec();
        for (offset, reloc) in section.relocations() {
            let at = offset as usize;
            let size = usize::from(reloc.size() / 8);
            let target = match reloc.target() {
                RelocationTarget::Symbol(index) => file.symbol_by_index(index).unwrap().address(),
                RelocationTarget::Section(index) => file.section_by_index(index).unwrap().address(),
                other => panic!("{other:?}"),
            };
            let mut implicit = 0_i64;
            if reloc.has_implicit_addend() {
                implicit = match size {
                    4 => i64::from(i32::from_le_bytes(data[at..at + 4].try_into().unwrap())),
                    8 => i64::from_le_bytes(data[at..at + 8].try_into().unwrap()),
                    _ => panic!("size {size}"),
                };
            }
            let value = (target.cast_signed() + implicit + reloc.addend()).cast_unsigned();
            data[at..at + size].copy_from_slice(&value.to_le_bytes()[..size]);
        }
        out.insert(format!(".debug_{rest}"), data);
    }
    out
}

/// The address of the function `name` in the object `bytes`.
fn address(bytes: &[u8], name: &str) -> u64 {
    let file = ::object::File::parse(bytes).unwrap();
    file.symbols()
        .find(|s| {
            s.kind() == SymbolKind::Text
                && s.name()
                    .is_ok_and(|n| n == name || n.strip_prefix('_') == Some(name))
        })
        .unwrap_or_else(|| panic!("no {name}"))
        .address()
}

/// What the DWARF of an object says, read with `gimli`.
#[derive(Debug, Default)]
struct Read {
    /// The compile units' `DW_AT_name`s.
    units: Vec<String>,
    /// Whether any unit has a `DW_AT_comp_dir`.
    comp_dir: bool,
    producer: String,
    language: u16,
    /// The subprograms: name, low pc, length.
    subprograms: Vec<(String, u64, u64)>,
    /// The line table's include directories and file names.
    dirs: Vec<String>,
    files: Vec<String>,
    /// The rows: address, line, column, end of a sequence.
    rows: Vec<(u64, u64, u64, bool)>,
}

fn read(sections: &BTreeMap<String, Vec<u8>>) -> Read {
    let load = |id: gimli::SectionId| -> Result<Cow<'_, [u8]>, gimli::Error> {
        Ok(sections
            .get(id.name())
            .map_or(Cow::Borrowed(&[][..]), |d| Cow::Borrowed(d.as_slice())))
    };
    let owned = gimli::DwarfSections::load(load).unwrap();
    let dwarf = owned.borrow(|s| gimli::EndianSlice::new(s, gimli::LittleEndian));
    let mut out = Read::default();
    let mut iter = dwarf.units();
    while let Some(header) = iter.next().unwrap() {
        let unit = dwarf.unit(header).unwrap();
        let text = |value| {
            dwarf
                .attr_string(&unit, value)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        let mut entries = unit.entries();
        while let Some(entry) = entries.next_dfs().unwrap() {
            if entry.tag() == gimli::DW_TAG_compile_unit {
                out.units
                    .push(text(entry.attr_value(gimli::DW_AT_name).unwrap()));
                out.comp_dir |= entry.attr(gimli::DW_AT_comp_dir).is_some();
                out.producer = text(entry.attr_value(gimli::DW_AT_producer).unwrap());
                if let Some(gimli::AttributeValue::Language(lang)) =
                    entry.attr_value(gimli::DW_AT_language)
                {
                    out.language = lang.0;
                }
            } else if entry.tag() == gimli::DW_TAG_subprogram {
                let name = text(entry.attr_value(gimli::DW_AT_name).unwrap());
                let Some(gimli::AttributeValue::Addr(low)) = entry.attr_value(gimli::DW_AT_low_pc)
                else {
                    panic!("no low pc");
                };
                let Some(gimli::AttributeValue::Udata(len)) =
                    entry.attr_value(gimli::DW_AT_high_pc)
                else {
                    panic!("no high pc");
                };
                out.subprograms.push((name, low, len));
            }
        }
        let program = unit.line_program.clone().unwrap();
        let header = program.header();
        for dir in header.include_directories() {
            out.dirs.push(text(*dir));
        }
        for file in header.file_names() {
            out.files.push(text(file.path_name()));
        }
        let mut rows = program.rows();
        while let Some((_, r)) = rows.next_row().unwrap() {
            let column = match r.column() {
                gimli::ColumnType::LeftEdge => 0,
                gimli::ColumnType::Column(c) => c.get(),
            };
            out.rows.push((
                r.address(),
                r.line().map_or(0, std::num::NonZero::get),
                column,
                r.end_sequence(),
            ));
        }
    }
    out
}

/// The line of the code at `address`: the last row at or below it in its
/// sequence.
fn line_at(rows: &[(u64, u64, u64, bool)], address: u64) -> Option<u64> {
    let mut best = None;
    for window in rows.windows(2) {
        let (start, line, _, end) = window[0];
        let (next, ..) = window[1];
        if !end && start <= address && address < next {
            best = Some(line);
        }
    }
    best
}

/// ELF and Mach-O: one compile unit named by the diagnostic path, without
/// `DW_AT_comp_dir`; the producer and the user language; a subprogram per
/// function at its symbol's address; the file `main.cheby` in the relative
/// directory `src`; every row at its function's address plus its offset,
/// each sequence ending at the function's end; an offset mapped to its
/// line.
#[test]
fn dwarf_read_back_in_elf_and_macho() {
    let record = record();
    for os in formats() {
        let bytes = replay(&record, isa::isa_for(&triple(os)).unwrap(), "app").unwrap();
        let sections = relocated(&bytes);
        for name in [".debug_info", ".debug_abbrev", ".debug_line", ".debug_str"] {
            assert!(sections.contains_key(name), "{os}: no {name}");
        }
        let read = read(&sections);
        assert_eq!(read.units, vec!["src/main.cheby"], "{os}");
        assert!(!read.comp_dir, "{os}");
        assert!(read.producer.starts_with("cheby "), "{os}");
        assert_eq!(read.language, 0x8000, "{os}");
        assert_eq!(read.dirs, vec!["src"], "{os}");
        assert_eq!(read.files, vec!["main.cheby"], "{os}");
        let (f, g, h) = (
            address(&bytes, "cb$fn$app$f"),
            address(&bytes, "cb$fn$app$g"),
            address(&bytes, "cb$fn$app$h"),
        );
        assert_eq!(
            read.subprograms,
            vec![
                ("cb$fn$app$f".to_owned(), f, 64),
                ("cb$fn$app$g".to_owned(), g, 32),
                ("cb$fn$app$h".to_owned(), h, 16),
            ],
            "{os}"
        );
        assert_eq!(
            read.rows,
            vec![
                (f, 3, 4, false),
                (f + 8, 4, 3, false),
                (f + 20, 5, 7, false),
                (f + 64, 5, 7, true),
                (g, 9, 4, false),
                (g + 16, 7, 11, false),
                (g + 32, 7, 11, true),
            ],
            "{os}"
        );
        assert_eq!(line_at(&read.rows, f + 12), Some(4), "{os}");
        assert_eq!(line_at(&read.rows, g + 31), Some(7), "{os}");
        assert_eq!(line_at(&read.rows, h), None, "{os}");
    }
}

/// D-938's macOS objects: the line table lives in the `__DWARF` segment's
/// `__debug_line`, which `-Wl,-S` then keeps out of the executable.
#[test]
#[cfg(target_arch = "aarch64")]
fn dwarf_macho_holds_debug_line() {
    let bytes = replay(
        &record(),
        isa::isa_for(&triple("apple-darwin")).unwrap(),
        "app",
    )
    .unwrap();
    let file = ::object::File::parse(&*bytes).unwrap();
    let line = file.section_by_name("__debug_line").unwrap();
    assert_eq!(line.segment_name().unwrap(), Some("__DWARF"));
    assert_ne!(line.data().unwrap().len(), 0);
}

/// Two replays of one record give the same bytes (D-264), and no
/// absolute path reaches the object (D-421, D-934).
#[test]
fn dwarf_two_replays_identical() {
    for os in formats() {
        let isa = || isa::isa_for(&triple(os)).unwrap();
        let a = replay(&record(), isa(), "app").unwrap();
        let b = replay(&record(), isa(), "app").unwrap();
        assert_eq!(a, b, "{os}");
        let read = read(&relocated(&a));
        assert!(
            read.units
                .iter()
                .chain(&read.dirs)
                .chain(&read.files)
                .all(|p| !p.starts_with('/')),
            "{read:?}"
        );
    }
}

/// A record without positions, as with `CHEBY_POSITIONS=off`, gets no
/// DWARF section, and its object is what replay wrote before D-938.
#[test]
fn dwarf_none_without_positions() {
    let mut plain = record();
    plain.source = None;
    for f in &mut plain.functions {
        f.positions.clear();
    }
    for os in formats() {
        let bytes = replay(&plain, isa::isa_for(&triple(os)).unwrap(), "app").unwrap();
        assert!(relocated(&bytes).is_empty(), "{os}");
    }
}
