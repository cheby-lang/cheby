//! Linker GC of record entries (D-1253, wave 6's spike): in an executable,
//! the linker drops the functions of a whole module (D-966) that nothing
//! reaches, and each dropped function's `FunctionEntry` and call sites with
//! it.
//!
//! A module record (`cb$mod$<path>`, D-919) lists every function of its
//! unit in its function table, and each entry's code pointer is a
//! relocation to its function, so a record in the record's own data keeps
//! every function alive (`-dead_strip` and `--gc-sections` see the
//! references). [`replay`](crate::object::replay) therefore moves the
//! table out of the record ([`split`]):
//!
//! - each function's 40-byte `FunctionEntry` (D-1234), its `name` still an
//!   offset into the record's string table, goes into a part of the object
//!   tied to the function: on Mach-O one atom per entry (an `l` symbol) in
//!   the one section `__DATA,cb_fns` that every object shares
//!   ([`MACHO_SECTION`]: a Mach-O file holds at most 255 sections, so not
//!   one per module), marked `S_ATTR_LIVE_SUPPORT`, so `-dead_strip` keeps
//!   it only while its function is live, the entry's `pad` word holding its
//!   record's 32-bit [`tag`]; on ELF one section per entry, named
//!   `cbf_<key>` after its record (`<key>`: 12 hex digits of the FNV-1a hash
//!   of the record's symbol, [`key`]), `SHF_LINK_ORDER` with `sh_link`
//!   its function's section, so `--gc-sections` drops it with the function,
//!   where every function and every data object gets a section of its own
//!   (or one live `.data` would keep every `cb$fnval$…`, and so every
//!   function) and the link line carries `-z start-stop-gc`
//!   (`crate::link`), so that the program object's `__start_`/`__stop_`
//!   references keep no section alive;
//! - each function's call sites go into one atom (Mach-O, `__DATA,cb_sites`)
//!   or one section (ELF, `.rodata.cb_sites`) that only its entry
//!   references;
//! - the record keeps everything else, shifted down over the removed
//!   entries and sites, with an empty function table; it stays read-only
//!   (D-919);
//! - the program object (`crate::program`) passes each record's table to
//!   the runtime through `ProgramImage`'s `functions` bounds, which the
//!   linker fills (the shared section's `section$start$…`/`section$end$…`
//!   with the record's tag on Mach-O, the record's own weak
//!   `__start_…`/`__stop_…` with tag 0 on ELF), and the runtime takes the
//!   entries in the bounds whose `pad` equals the tag, so it registers only
//!   the kept functions and no entry can dangle.
//!
//! Code records, the JIT and `RECORD_VERSION` do not change (D-1253): the
//! split is a step of replay. D-1288: the section names,
//! the key, `-z start-stop-gc` and the image's bounds table.

use std::mem::{offset_of, size_of};

use object::write::{Object, Relocation, SectionId, Symbol, SymbolId, SymbolSection};
use object::{
    BinaryFormat, RelocationEncoding, RelocationFlags, RelocationKind, SectionFlags, SectionKind,
    SymbolFlags, SymbolKind as ObjSymbolKind, SymbolScope,
};

use cheby_codegen_clif::mangle;
use cheby_codegen_clif::record::{
    CodeRecord, RecordContents, RecordDataReloc, RecordError, SymbolKind,
};
use cheby_runtime::image::{CallSiteEntry, FunctionEntry, ModuleRecord};

use crate::LinkError;

/// The offset of `ModuleRecord`'s `functions` table (`image.rs`'s
/// documentation): its count, then its pointer at 8.
const FUNCTIONS: usize = 80;
/// The record's header: its seven tables.
const HEADER: usize = size_of::<ModuleRecord>();
/// A `FunctionEntry`.
const ENTRY: usize = size_of::<FunctionEntry>();
/// A `CallSiteEntry`.
const SITE: usize = size_of::<CallSiteEntry>();
/// The entry's fields that the split moves.
const ENTRY_LEN: usize = offset_of!(FunctionEntry, len);
const ENTRY_NAME: usize = offset_of!(FunctionEntry, name);
const ENTRY_SITES: usize = offset_of!(FunctionEntry, sites);

const _: () = {
    assert!(HEADER == 112);
    assert!(ENTRY == 40);
    assert!(SITE == 16);
    assert!(offset_of!(FunctionEntry, code) == 0);
    assert!(ENTRY_LEN == 8);
    assert!(offset_of!(FunctionEntry, flags) == 12);
    assert!(ENTRY_NAME == 16);
    assert!(ENTRY_SITES == 24);
};

/// The prefix of the ELF entries' sections.
pub const SECTION_PREFIX: &str = "cbf_";

/// The one entries' section of every Mach-O object, `__DATA,cb_fns`: a
/// Mach-O file holds at most 255 sections (`n_sect` is a byte, and ld64
/// stops with "custom section index overflow" past it), so a section per
/// module cannot hold a program of hundreds of modules.
pub const MACHO_SECTION: &str = "cb_fns";

/// The FNV-1a hash of the record `record_name` (`cb$mod$<path>`).
fn fnv(record_name: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in record_name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The 12-hex-digit key of the record `record_name` (`cb$mod$<path>`):
/// the top 48 bits of its FNV-1a hash, so the ELF section name
/// `cbf_<key>` is the record's own.
#[must_use]
pub fn key(record_name: &str) -> String {
    format!("{:012x}", fnv(record_name) >> 16)
}

/// The record's tag on Mach-O: the low 32 bits of its FNV-1a hash, never
/// 0, which every entry moved into [`MACHO_SECTION`] carries in its `pad`
/// word, so the runtime finds a record's entries in the shared section.
#[must_use]
pub fn tag(record_name: &str) -> u32 {
    let low = u32::try_from(fnv(record_name) & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    low.max(1)
}

/// The ELF entries' section of the record whose key is `key`.
#[must_use]
pub fn section(key: &str) -> String {
    format!("{SECTION_PREFIX}{key}")
}

/// The symbols the linker defines at the start and the end of the
/// entries' section of the record whose key is `key` (on Mach-O the one
/// shared section's, whatever the key), as the object must spell them (no
/// Mach-O `_` prefix).
#[must_use]
pub fn bounds(format: BinaryFormat, key: &str) -> (String, String) {
    if format == BinaryFormat::MachO {
        (
            format!("section$start$__DATA${MACHO_SECTION}"),
            format!("section$end$__DATA${MACHO_SECTION}"),
        )
    } else {
        let section = section(key);
        (format!("__start_{section}"), format!("__stop_{section}"))
    }
}

/// The tag the program image gives the record `record_name` in `format`
/// (`FunctionBounds::tag`): [`tag`] on Mach-O, 0 on ELF, whose entries'
/// section is the record's own and whose entries keep `pad` 0.
#[must_use]
pub fn image_tag(format: BinaryFormat, record_name: &str) -> u32 {
    if format == BinaryFormat::MachO {
        tag(record_name)
    } else {
        0
    }
}

/// One function's entry, moved out of its record.
pub(crate) struct Chunk {
    /// The function's symbol name in the record.
    pub function: String,
    /// The `FunctionEntry`, its code and sites pointers 0.
    pub entry: [u8; ENTRY],
    /// Its `CallSiteEntry`s.
    pub sites: Vec<u8>,
}

/// What [`split`] gives: the record without its function table, and the
/// entries to add once its object is finished.
pub(crate) struct Split {
    pub record: CodeRecord,
    pub key: String,
    /// The record's Mach-O [`tag`].
    pub tag: u32,
    pub chunks: Vec<Chunk>,
}

fn malformed(what: &'static str) -> LinkError {
    LinkError::Record(RecordError::Malformed(what))
}

fn bytes_at<const N: usize>(bytes: &[u8], at: usize) -> Result<[u8; N], LinkError> {
    at.checked_add(N)
        .and_then(|end| bytes.get(at..end))
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| malformed("a word outside the module record"))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, LinkError> {
    bytes_at::<8>(bytes, at).map(u64::from_le_bytes)
}

fn add(a: usize, b: usize) -> Result<usize, LinkError> {
    a.checked_add(b)
        .ok_or_else(|| malformed("a module record too large"))
}

fn mul(a: usize, b: usize) -> Result<usize, LinkError> {
    a.checked_mul(b)
        .ok_or_else(|| malformed("a module record too large"))
}

/// Moves the function table out of the module record of `record`, the
/// code record of the module `module` (module documentation); `None` for
/// a record without that module record.
///
/// # Errors
///
/// [`LinkError::Record`] if the module record is not laid out as
/// `cheby_codegen_clif`'s `modrec.rs` writes it: the entries right after
/// the header, each one's sites right after the entries in their order, no
/// other pointer into them.
#[expect(
    clippy::too_many_lines,
    reason = "one pass over the record's entries, then its relocations"
)]
pub(crate) fn split(record: &CodeRecord, module: &str) -> Result<Option<Split>, LinkError> {
    let name = mangle::module_record(module);
    let Some(data_index) = record.data.iter().position(|data| {
        record
            .symbol_at(data.symbol)
            .is_some_and(|s| s.name == name)
    }) else {
        return Ok(None);
    };
    let data = record
        .data
        .get(data_index)
        .ok_or_else(|| malformed("the module record"))?;
    let RecordContents::Bytes(bytes) = &data.contents else {
        return Err(malformed("a module record without bytes"));
    };
    let this = data.symbol;
    let record_name = record
        .symbol_at(this)
        .ok_or_else(|| malformed("the module record's symbol"))?
        .name
        .clone();
    let count =
        usize::try_from(read_u64(bytes, FUNCTIONS)?).map_err(|_| malformed("a function count"))?;
    let reloc_at = |offset: usize| -> Option<&RecordDataReloc> {
        data.relocs
            .iter()
            .find(|r| usize::try_from(r.offset).is_ok_and(|o| o == offset))
    };
    let sites_at = add(HEADER, mul(count, ENTRY)?)?;
    if sites_at > bytes.len() {
        return Err(malformed("a function table outside the module record"));
    }
    let mut next_site = sites_at;
    let mut chunks = Vec::with_capacity(count);
    for index in 0..count {
        let entry_at = add(HEADER, mul(index, ENTRY)?)?;
        let code = reloc_at(entry_at).ok_or_else(|| malformed("an entry without code"))?;
        let function = record
            .symbol_at(code.symbol)
            .ok_or_else(|| malformed("an entry's function"))?;
        if function.kind != SymbolKind::Function || code.addend != 0 {
            return Err(malformed("an entry whose code is not a function"));
        }
        let mut entry: [u8; ENTRY] = bytes_at(bytes, entry_at)?;
        // The code pointer and the sites' pointer are relocations, added
        // again in the entry's own section.
        entry
            .get_mut(..ENTRY_LEN)
            .ok_or_else(|| malformed("an entry"))?
            .fill(0);
        let site_count = read_u64(bytes, add(entry_at, ENTRY_SITES)?)?;
        let ptr_at = add(ENTRY_SITES, 8)?;
        entry
            .get_mut(ptr_at..ENTRY)
            .ok_or_else(|| malformed("an entry"))?
            .fill(0);
        let sites_len = usize::try_from(site_count)
            .ok()
            .and_then(|n| n.checked_mul(SITE))
            .ok_or_else(|| malformed("a site count"))?;
        let sites = if sites_len == 0 {
            Vec::new()
        } else {
            let ptr = reloc_at(add(entry_at, ptr_at)?)
                .ok_or_else(|| malformed("sites without a pointer"))?;
            if ptr.symbol != this || usize::try_from(ptr.addend).ok() != Some(next_site) {
                return Err(malformed("call sites out of order"));
            }
            let end = add(next_site, sites_len)?;
            let sites = bytes
                .get(next_site..end)
                .ok_or_else(|| malformed("call sites outside the module record"))?
                .to_vec();
            next_site = end;
            sites
        };
        chunks.push(Chunk {
            function: function.name.clone(),
            entry,
            sites,
        });
    }
    // The removed region: the entries and their sites, [HEADER, next_site).
    let delta = next_site
        .checked_sub(HEADER)
        .ok_or_else(|| malformed("a module record"))?;
    let delta_i = i64::try_from(delta).map_err(|_| malformed("a module record too large"))?;
    let delta_u = u32::try_from(delta).map_err(|_| malformed("a module record too large"))?;
    let mut new_bytes = Vec::with_capacity(bytes.len().saturating_sub(delta));
    new_bytes.extend_from_slice(bytes.get(..HEADER).ok_or_else(|| malformed("a header"))?);
    new_bytes.extend_from_slice(
        bytes
            .get(next_site..)
            .ok_or_else(|| malformed("a record"))?,
    );
    // An empty function table: count 0, pointer 0.
    new_bytes
        .get_mut(FUNCTIONS..add(FUNCTIONS, 16)?)
        .ok_or_else(|| malformed("a header"))?
        .fill(0);
    let mut relocs = Vec::with_capacity(data.relocs.len());
    for reloc in &data.relocs {
        let offset = usize::try_from(reloc.offset).map_err(|_| malformed("an offset"))?;
        if offset == add(FUNCTIONS, 8)? || (HEADER..next_site).contains(&offset) {
            continue;
        }
        let mut reloc = *reloc;
        if offset >= next_site {
            reloc.offset = reloc
                .offset
                .checked_sub(delta_u)
                .ok_or_else(|| malformed("an offset"))?;
        }
        if reloc.symbol == this {
            let addend = usize::try_from(reloc.addend).map_err(|_| malformed("an addend"))?;
            if (HEADER..next_site).contains(&addend) {
                return Err(malformed("a pointer into the function table"));
            }
            if addend >= next_site {
                reloc.addend = reloc
                    .addend
                    .checked_sub(delta_i)
                    .ok_or_else(|| malformed("an addend"))?;
            }
        }
        relocs.push(reloc);
    }
    let mut out = record.clone();
    let moved = out
        .data
        .get_mut(data_index)
        .ok_or_else(|| malformed("the module record"))?;
    moved.contents = RecordContents::Bytes(new_bytes);
    moved.relocs = relocs;
    Ok(Some(Split {
        record: out,
        key: key(&record_name),
        tag: tag(&record_name),
        chunks,
    }))
}

/// A 64-bit absolute data relocation at `offset` to `symbol`.
const fn abs64(offset: u64, symbol: SymbolId) -> Relocation {
    Relocation {
        offset,
        symbol,
        addend: 0,
        flags: RelocationFlags::Generic {
            kind: RelocationKind::Absolute,
            encoding: RelocationEncoding::Generic,
            size: 64,
        },
    }
}

/// A local symbol `name` at the start of an atom in `section`, without
/// Mach-O's `_` prefix: an `l` symbol starts an atom and stays out of the
/// executable's symbol table.
fn atom_symbol(object: &mut Object<'_>, name: &str, section: SectionId) -> SymbolId {
    let id = object.add_symbol(Symbol {
        name: name.as_bytes().to_vec(),
        value: 0,
        size: 0,
        kind: ObjSymbolKind::Data,
        scope: SymbolScope::Compilation,
        weak: false,
        section: SymbolSection::Section(section),
        flags: SymbolFlags::None,
    });
    object.symbol_mut(id).name = name.as_bytes().to_vec();
    id
}

/// Adds the entries of `split` to `object` (module documentation);
/// `function(name)` gives each function's symbol.
///
/// # Errors
///
/// [`LinkError::Record`] for an entry whose function the object does not
/// define, [`LinkError::Object`] if `object` refuses a relocation.
pub(crate) fn add_chunks(
    object: &mut Object<'_>,
    split: &Split,
    function: &dyn Fn(&str) -> Option<SymbolId>,
) -> Result<(), LinkError> {
    let section_name = section(&split.key).into_bytes();
    let err = |e: object::write::Error| LinkError::Object(e.to_string());
    let sites_ptr = u64::try_from(ENTRY_SITES)
        .ok()
        .and_then(|at| at.checked_add(8))
        .ok_or_else(|| malformed("an entry"))?;
    if object.format() == BinaryFormat::MachO {
        let entries = object.add_section(
            b"__DATA".to_vec(),
            MACHO_SECTION.as_bytes().to_vec(),
            SectionKind::Data,
        );
        object.section_mut(entries).flags = SectionFlags::MachO {
            flags: object::macho::S_REGULAR | object::macho::S_ATTR_LIVE_SUPPORT,
        };
        let sites = object.add_section(b"__DATA".to_vec(), b"cb_sites".to_vec(), SectionKind::Data);
        let tag = split.tag.to_le_bytes();
        let pad = add(ENTRY_NAME, 4)?;
        for (index, chunk) in split.chunks.iter().enumerate() {
            let func = function(&chunk.function)
                .ok_or_else(|| malformed("an entry's function is not in the object"))?;
            let entry = atom_symbol(object, &format!("lcb$fe${}${index}", split.key), entries);
            // The shared section's entries carry their record's tag in
            // `pad`, which the runtime matches against the image's.
            let mut bytes = chunk.entry;
            bytes
                .get_mut(pad..add(pad, 4)?)
                .ok_or_else(|| malformed("an entry"))?
                .copy_from_slice(&tag);
            let at = object.add_symbol_data(entry, entries, &bytes, 8);
            object
                .add_relocation(entries, abs64(at, func))
                .map_err(err)?;
            if !chunk.sites.is_empty() {
                let site = atom_symbol(object, &format!("lcb$fs${}${index}", split.key), sites);
                object.add_symbol_data(site, sites, &chunk.sites, 4);
                let ptr = at
                    .checked_add(sites_ptr)
                    .ok_or_else(|| malformed("an entry"))?;
                object
                    .add_relocation(entries, abs64(ptr, site))
                    .map_err(err)?;
            }
        }
    } else {
        for chunk in &split.chunks {
            let func = function(&chunk.function)
                .ok_or_else(|| malformed("an entry's function is not in the object"))?;
            let entries = object.add_section(Vec::new(), section_name.clone(), SectionKind::Data);
            object.section_mut(entries).flags = SectionFlags::Elf {
                sh_flags: u64::from(
                    object::elf::SHF_ALLOC | object::elf::SHF_WRITE | object::elf::SHF_LINK_ORDER,
                ),
            };
            let at = object.append_section_data(entries, &chunk.entry, 8);
            object
                .add_relocation(entries, abs64(at, func))
                .map_err(err)?;
            if !chunk.sites.is_empty() {
                let sites = object.add_section(
                    Vec::new(),
                    b".rodata.cb_sites".to_vec(),
                    SectionKind::ReadOnlyData,
                );
                object.append_section_data(sites, &chunk.sites, 4);
                let symbol = object.section_symbol(sites);
                let ptr = at
                    .checked_add(sites_ptr)
                    .ok_or_else(|| malformed("an entry"))?;
                object
                    .add_relocation(entries, abs64(ptr, symbol))
                    .map_err(err)?;
            }
        }
    }
    Ok(())
}

/// ELF: sets `sh_link` of every entries' section (`cbf_…`) of `bytes` to
/// the section of the function its first relocation (the code pointer)
/// names; `object::write` writes `sh_link` 0 for them.
///
/// # Errors
///
/// [`LinkError::Object`] if `bytes` is not a 64-bit ELF object, and
/// [`LinkError::Record`] for an entries' section without its relocation.
pub(crate) fn patch_link_order(bytes: &mut [u8]) -> Result<(), LinkError> {
    use object::read::{Object as _, ObjectSection as _, ObjectSymbol as _};
    let mut patches = Vec::new();
    let shoff;
    let shentsize;
    {
        let file = object::read::elf::ElfFile64::<object::Endianness>::parse(&*bytes)
            .map_err(|e| LinkError::Object(e.to_string()))?;
        let header = file.elf_header();
        let endian = file.endian();
        shoff = header.e_shoff.get(endian);
        shentsize = u64::from(header.e_shentsize.get(endian));
        for section in file.sections() {
            if !section.name().is_ok_and(|n| n.starts_with(SECTION_PREFIX)) {
                continue;
            }
            let Some((_, reloc)) = section.relocations().min_by_key(|(offset, _)| *offset) else {
                return Err(malformed("an entry's section without its relocation"));
            };
            let object::RelocationTarget::Symbol(symbol) = reloc.target() else {
                return Err(malformed("an entry's relocation without a symbol"));
            };
            let target = file
                .symbol_by_index(symbol)
                .map_err(|e| LinkError::Object(e.to_string()))?
                .section_index()
                .ok_or_else(|| malformed("an entry's function without a section"))?;
            patches.push((section.index().0, target.0));
        }
    }
    for (section, target) in patches {
        // `sh_link` is at 40 in `Elf64_Shdr`.
        let at = u64::try_from(section)
            .ok()
            .and_then(|s| shentsize.checked_mul(s))
            .and_then(|s| s.checked_add(shoff))
            .and_then(|a| a.checked_add(40))
            .and_then(|a| usize::try_from(a).ok())
            .ok_or_else(|| LinkError::Object("a section header out of range".into()))?;
        let link = u32::try_from(target)
            .map_err(|_| LinkError::Object("a section index out of range".into()))?;
        bytes
            .get_mut(at..at.saturating_add(4))
            .ok_or_else(|| LinkError::Object("a section header out of range".into()))?
            .copy_from_slice(&link.to_le_bytes());
    }
    Ok(())
}
