//! DWARF line tables in replayed objects (D-938).
//!
//! When [`crate::object::replay`] turns a code record with positions into
//! an object, [`write`] adds, through `gimli::write`, a minimal DWARF 4
//! description of the record's code (GAP(dwarf-minimal-info)):
//!
//! - `.debug_line`: one line program whose only file is the record's
//!   source, D-421's diagnostic path split into a relative directory and a
//!   name (`src` and `main.cheby`); one sequence per function with rows,
//!   one row per source row in address order, ending at the function's
//!   end;
//! - `.debug_info` and `.debug_abbrev`: one compile unit with
//!   `DW_AT_name` the diagnostic path, `DW_AT_producer`, `DW_AT_language`
//!   (`DW_LANG_lo_user`: DWARF has no code for Cheby), `DW_AT_stmt_list`
//!   and the code's ranges (`DW_AT_ranges`, in `.debug_ranges`), and one
//!   `DW_TAG_subprogram` per function, named by its symbol, with its
//!   range; no `DW_AT_comp_dir` and no absolute path anywhere, so the
//!   bytes do not depend on where the build ran (D-264, D-934);
//! - `.debug_str`: the names.
//!
//! Addresses are relocations against the functions' symbols (through the
//! section symbol of their section and their offset in it, as LLVM
//! writes them). In ELF objects the offsets into other DWARF sections are
//! relocations too, since the linker concatenates the sections; Mach-O
//! objects keep them as plain offsets, as Apple's tools expect, and macOS
//! executables carry no debug map until M6 anyway (D-938, D-1112). The
//! sections are `.debug_*` in ELF and `__DWARF,__debug_*` in Mach-O.
//!
//! A record without positions, and the program object (`program.rs`),
//! get no DWARF.

use cranelift_module::FuncId;
use cranelift_object::ObjectProduct;
use gimli::write::{
    Address, AttributeValue, DwarfUnit, EndianVec, LineProgram, LineString, Range, RangeList,
    Sections, Writer,
};
use gimli::{Encoding, Format, LineEncoding, RunTimeEndian, SectionId};
use object::write::{Object, Relocation, StandardSegment, SymbolId};
use object::{BinaryFormat, RelocationEncoding, RelocationFlags, RelocationKind, SectionKind};

use cheby_codegen_clif::record::CodeRecord;

use crate::LinkError;

/// What `DW_AT_producer` says.
const PRODUCER: &str = concat!("cheby ", env!("CARGO_PKG_VERSION"));

/// Where one function of the record lies in the object: the symbol to
/// relocate against and the offset of the code from it.
struct Placed {
    symbol: SymbolId,
    offset: u64,
}

/// Adds the DWARF sections of `record` to `product`, whose functions are
/// the record's, `funcs[k]` the id of `record.functions[k]` (D-938). Does
/// nothing for a record without a source or without rows.
///
/// # Errors
///
/// [`LinkError::Object`] if a function is missing from the object or
/// `gimli` or `object` refuse a section or a relocation.
pub(crate) fn write(
    product: &mut ObjectProduct,
    record: &CodeRecord,
    funcs: &[FuncId],
) -> Result<(), LinkError> {
    let Some(source) = record.source.as_deref() else {
        return Ok(());
    };
    if record.functions.iter().all(|f| f.positions.is_empty()) {
        return Ok(());
    }
    if funcs.len() != record.functions.len() {
        return Err(dwarf_error("functions missing from the object"));
    }
    let mut placed = Vec::with_capacity(record.functions.len());
    for &id in funcs {
        let symbol = product
            .functions
            .get(id)
            .and_then(|f| f.as_ref())
            .map(|&(symbol, _)| symbol)
            .ok_or_else(|| dwarf_error("a function missing from the object"))?;
        let (symbol, offset) = product
            .object
            .symbol_section_and_offset(symbol)
            .ok_or_else(|| dwarf_error("a function without a section"))?;
        placed.push(Placed { symbol, offset });
    }
    let object = &mut product.object;
    // Replay writes x86-64 and aarch64 objects only, both little-endian
    // (`object::check_target`, D-936).
    let endian = RunTimeEndian::Little;
    let mut dwarf = unit(record, source);
    let relocate_offsets = object.format() == BinaryFormat::Elf;
    let mut sections = Sections::new(RelocWriter::new(endian, relocate_offsets));
    dwarf.write(&mut sections).map_err(gimli_error)?;
    add_sections(object, &mut sections, &placed)
}

/// The compile unit of `record`, its line program and its subprograms.
/// Address symbols are indices into `record.functions`.
fn unit(record: &CodeRecord, source: &str) -> DwarfUnit {
    let encoding = Encoding {
        format: Format::Dwarf32,
        version: 4,
        address_size: 8,
    };
    let mut dwarf = DwarfUnit::new(encoding);
    let (dir, name) = match source.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, source),
    };
    // Version 4 leaves directory 0, the compilation directory, out of the
    // header, and the unit has no `DW_AT_comp_dir`: "." is never written.
    let mut program = LineProgram::new(
        encoding,
        LineEncoding::default(),
        LineString::String(b".".to_vec()),
        None,
        LineString::String(name.as_bytes().to_vec()),
        None,
    );
    let directory = match dir {
        Some(dir) => program.add_directory(LineString::String(dir.as_bytes().to_vec())),
        None => program.default_directory(),
    };
    let file = program.add_file(
        LineString::String(name.as_bytes().to_vec()),
        directory,
        None,
    );
    let mut ranges = Vec::with_capacity(record.functions.len());
    for (k, function) in record.functions.iter().enumerate() {
        let len = u64::try_from(function.bytes.len()).unwrap_or(u64::MAX);
        let start = Address::Symbol {
            symbol: k,
            addend: 0,
        };
        ranges.push(Range::StartLength {
            begin: start,
            length: len,
        });
        if function.positions.is_empty() {
            continue;
        }
        program.begin_sequence(Some(start));
        for row in &function.positions {
            let at = program.row();
            at.address_offset = u64::from(row.offset);
            at.file = file;
            at.line = u64::from(row.line);
            at.column = u64::from(row.column);
            program.generate_row();
        }
        program.end_sequence(len);
    }
    dwarf.unit.line_program = program;
    let range_list = dwarf.unit.ranges.add(RangeList(ranges));
    let names: Vec<(gimli::write::StringId, u64)> = record
        .functions
        .iter()
        .map(|function| {
            let name = record
                .symbol_at(function.symbol)
                .map_or_else(Vec::new, |s| s.name.as_bytes().to_vec());
            let len = u64::try_from(function.bytes.len()).unwrap_or(u64::MAX);
            (dwarf.strings.add(name), len)
        })
        .collect();
    let source_name = dwarf.strings.add(source.as_bytes().to_vec());
    let producer = dwarf.strings.add(PRODUCER.as_bytes().to_vec());
    let root = dwarf.unit.root();
    let cu = dwarf.unit.get_mut(root);
    cu.set(gimli::DW_AT_name, AttributeValue::StringRef(source_name));
    cu.set(gimli::DW_AT_producer, AttributeValue::StringRef(producer));
    cu.set(
        gimli::DW_AT_language,
        AttributeValue::Language(gimli::DW_LANG_lo_user),
    );
    cu.set(gimli::DW_AT_stmt_list, AttributeValue::LineProgramRef);
    cu.set(
        gimli::DW_AT_low_pc,
        AttributeValue::Address(Address::Constant(0)),
    );
    cu.set(
        gimli::DW_AT_ranges,
        AttributeValue::RangeListRef(range_list),
    );
    for (k, (name, len)) in names.into_iter().enumerate() {
        let id = dwarf.unit.add(root, gimli::DW_TAG_subprogram);
        let entry = dwarf.unit.get_mut(id);
        entry.set(gimli::DW_AT_name, AttributeValue::StringRef(name));
        entry.set(
            gimli::DW_AT_low_pc,
            AttributeValue::Address(Address::Symbol {
                symbol: k,
                addend: 0,
            }),
        );
        entry.set(gimli::DW_AT_high_pc, AttributeValue::Udata(len));
    }
    dwarf
}

/// Adds the non-empty sections of `sections` to `object` with their
/// relocations: an address against its function's placement, an offset
/// (ELF only) against the target section's symbol.
fn add_sections(
    object: &mut Object<'static>,
    sections: &mut Sections<RelocWriter>,
    placed: &[Placed],
) -> Result<(), LinkError> {
    let segment = object.segment_name(StandardSegment::Debug).to_vec();
    let macho = object.format() == BinaryFormat::MachO;
    // First every section, so an offset can name any of them.
    let mut ids: Vec<(SectionId, object::write::SectionId)> = Vec::new();
    sections.for_each(|id, w| {
        if !w.out.slice().is_empty() {
            let name = if macho {
                id.name().replacen('.', "__", 1)
            } else {
                id.name().to_owned()
            };
            let section =
                object.add_section(segment.clone(), name.into_bytes(), SectionKind::Debug);
            object.set_section_data(section, w.out.slice().to_vec(), 1);
            ids.push((id, section));
        }
        Ok::<(), LinkError>(())
    })?;
    let section_of = |id: SectionId| ids.iter().find(|(g, _)| *g == id).map(|&(_, s)| s);
    let mut relocs = Vec::new();
    sections.for_each(|id, w| {
        if let Some(section) = section_of(id) {
            relocs.extend(w.relocs.iter().map(|r| (section, r.clone())));
        }
        Ok::<(), LinkError>(())
    })?;
    for (section, reloc) in relocs {
        let (symbol, addend) = match reloc.target {
            Target::Function(k) => {
                let place = placed
                    .get(k)
                    .ok_or_else(|| dwarf_error("an address of an unknown function"))?;
                let base = i64::try_from(place.offset)
                    .map_err(|_| dwarf_error("a function offset out of range"))?;
                (place.symbol, base.saturating_add(reloc.addend))
            }
            Target::Section(target) => {
                let target = section_of(target)
                    .ok_or_else(|| dwarf_error("an offset into an empty section"))?;
                (object.section_symbol(target), reloc.addend)
            }
        };
        object
            .add_relocation(
                section,
                Relocation {
                    offset: reloc.offset,
                    symbol,
                    addend,
                    flags: RelocationFlags::Generic {
                        kind: RelocationKind::Absolute,
                        encoding: RelocationEncoding::Generic,
                        size: reloc.size.saturating_mul(8),
                    },
                },
            )
            .map_err(|e| dwarf_error(&e.to_string()))?;
    }
    Ok(())
}

/// What a relocation in a DWARF section points at.
#[derive(Clone, Copy, Debug)]
enum Target {
    /// The start of `record.functions[k]`.
    Function(usize),
    /// The start of a DWARF section.
    Section(SectionId),
}

/// A relocation a DWARF section needs.
#[derive(Clone, Debug)]
struct DebugReloc {
    offset: u64,
    size: u8,
    target: Target,
    addend: i64,
}

/// A `gimli` writer that records the relocations of addresses and, when
/// `relocate_offsets`, of offsets into other sections.
#[derive(Clone, Debug)]
struct RelocWriter {
    out: EndianVec<RunTimeEndian>,
    relocs: Vec<DebugReloc>,
    relocate_offsets: bool,
}

impl RelocWriter {
    fn new(endian: RunTimeEndian, relocate_offsets: bool) -> Self {
        Self {
            out: EndianVec::new(endian),
            relocs: Vec::new(),
            relocate_offsets,
        }
    }

    fn here(&self) -> u64 {
        u64::try_from(self.out.len()).unwrap_or(u64::MAX)
    }
}

impl Writer for RelocWriter {
    type Endian = RunTimeEndian;

    fn endian(&self) -> Self::Endian {
        self.out.endian()
    }

    fn len(&self) -> usize {
        self.out.len()
    }

    fn write(&mut self, bytes: &[u8]) -> gimli::write::Result<()> {
        self.out.write(bytes)
    }

    fn write_at(&mut self, offset: usize, bytes: &[u8]) -> gimli::write::Result<()> {
        self.out.write_at(offset, bytes)
    }

    fn write_address(&mut self, address: Address, size: u8) -> gimli::write::Result<()> {
        match address {
            Address::Constant(value) => self.write_udata(value, size),
            Address::Symbol { symbol, addend } => {
                self.relocs.push(DebugReloc {
                    offset: self.here(),
                    size,
                    target: Target::Function(symbol),
                    addend,
                });
                self.write_udata(0, size)
            }
        }
    }

    fn write_offset(
        &mut self,
        val: usize,
        section: SectionId,
        size: u8,
    ) -> gimli::write::Result<()> {
        let value = u64::try_from(val).map_err(|_| gimli::write::Error::OffsetOutOfBounds)?;
        if !self.relocate_offsets {
            return self.write_udata(value, size);
        }
        self.relocs.push(DebugReloc {
            offset: self.here(),
            size,
            target: Target::Section(section),
            addend: i64::try_from(value).map_err(|_| gimli::write::Error::OffsetOutOfBounds)?,
        });
        self.write_udata(0, size)
    }

    fn write_offset_at(
        &mut self,
        offset: usize,
        val: usize,
        section: SectionId,
        size: u8,
    ) -> gimli::write::Result<()> {
        let value = u64::try_from(val).map_err(|_| gimli::write::Error::OffsetOutOfBounds)?;
        if !self.relocate_offsets {
            return self.write_udata_at(offset, value, size);
        }
        self.relocs.push(DebugReloc {
            offset: u64::try_from(offset).map_err(|_| gimli::write::Error::OffsetOutOfBounds)?,
            size,
            target: Target::Section(section),
            addend: i64::try_from(value).map_err(|_| gimli::write::Error::OffsetOutOfBounds)?,
        });
        self.write_udata_at(offset, 0, size)
    }
}

fn dwarf_error(what: &str) -> LinkError {
    LinkError::Object(format!("DWARF line tables (D-938): {what}"))
}

fn gimli_error(error: gimli::write::Error) -> LinkError {
    dwarf_error(&error.to_string())
}
