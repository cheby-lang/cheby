//! The module record `cb$mod$<path>` of a unit compiled alone (D-919), in
//! the layout of `cheby_runtime::image::ModuleRecord` (that module's
//! documentation is the contract: 8-byte words that one absolute data
//! relocation fills, little-endian counts, length-prefixed strings).
//!
//! The record is one read-only data object, 8-byte aligned, laid out as:
//!
//! | Offset           | Contents                                                      |
//! | ---------------- | ------------------------------------------------------------- |
//! | 0                | the seven tables of `ModuleRecord`, 16 bytes each             |
//! | 112              | one `FunctionEntry` (32 bytes) per function, definition order |
//! | after them       | each function's `CallSiteEntry`s (16 bytes), in its order     |
//! | after the sites  | the string table: each path once, `u32` length then UTF-8     |
//!
//! Its table pointers are relocations to the record itself with the
//! table's offset as addend; each function's code pointer is a relocation
//! to the function. An empty table is count 0 and pointer 0.
//!
//! This wave fills only the call-site table (D-856, D-867). The types,
//! external types, hooks, free shapes and constants tables are empty: user
//! layouts, `[native]` types and constants reach records with N5 and K5a,
//! and `image::install` refuses what it cannot register yet.

use std::collections::BTreeMap;
use std::mem::{offset_of, size_of};

use cranelift_module::{DataDescription, DataId, Linkage, Module};

use cheby_runtime::image::{CallSiteEntry, FN_CHEBY_ABI, FunctionEntry, ModuleRecord, NO_STRING};

use crate::record::RecordError;
use crate::{CodegenError, DefinedCode};

/// The offsets of `ModuleRecord`'s tables (its fields are private; the
/// module documentation gives them).
const STRINGS: u32 = 0;
const FUNCTIONS: u32 = 80;
/// Every table of the header: strings, types, externals, hooks, free
/// shapes, functions, constants.
const TABLES: [u32; 7] = [0, 16, 32, 48, 64, 80, 96];
/// The header's size.
const HEADER: u32 = 112;
/// A table's count, then its pointer.
const TABLE_PTR: u32 = 8;
/// The alignment of the record and of every entry in it.
const ALIGN: u64 = 8;

const FUNCTION_ENTRY: u32 = 32;
const SITE_ENTRY: u32 = 16;
const FN_CODE: u32 = 0;
const FN_LEN: u32 = 8;
const FN_FLAGS: u32 = 12;
const FN_SITES: u32 = 16;
const SITE_RET: u32 = 0;
const SITE_PATH: u32 = 4;
const SITE_LINE: u32 = 8;
const SITE_COLUMN: u32 = 12;

// The layout written here is `image.rs`'s, checked against its structs.
#[expect(
    clippy::as_conversions,
    reason = "widening `u32` offsets in a const context, where `From` is not available"
)]
const _: () = {
    assert!(size_of::<ModuleRecord>() == 112);
    assert!(HEADER as usize == size_of::<ModuleRecord>());
    assert!(STRINGS == TABLES[0] && FUNCTIONS == TABLES[5]);
    assert!(FUNCTION_ENTRY as usize == size_of::<FunctionEntry>());
    assert!(FN_CODE as usize == offset_of!(FunctionEntry, code));
    assert!(FN_LEN as usize == offset_of!(FunctionEntry, len));
    assert!(FN_FLAGS as usize == offset_of!(FunctionEntry, flags));
    assert!(FN_SITES as usize == offset_of!(FunctionEntry, sites));
    assert!(SITE_ENTRY as usize == size_of::<CallSiteEntry>());
    assert!(SITE_RET as usize == offset_of!(CallSiteEntry, ret_offset));
    assert!(SITE_PATH as usize == offset_of!(CallSiteEntry, path));
    assert!(SITE_LINE as usize == offset_of!(CallSiteEntry, line));
    assert!(SITE_COLUMN as usize == offset_of!(CallSiteEntry, column));
};

/// Declares the read-only data object `name` with `Linkage::Hidden`
/// (D-917) in `module` and defines it as the module record of a unit whose
/// functions are `defined`, in definition order (`UnitCode::defined`).
///
/// # Errors
///
/// [`RecordError::Codegen`] if the module refuses the declaration or the
/// definition, and [`RecordError::Unsupported`] if the record would not
/// fit 4 GiB.
pub(crate) fn define<M: Module>(
    module: &mut M,
    name: &str,
    defined: &[DefinedCode],
) -> Result<DataId, RecordError> {
    let module_error = |e: cranelift_module::ModuleError| CodegenError::module(&e);
    let id = module
        .declare_data(name, Linkage::Hidden, false, false)
        .map_err(module_error)?;
    let layout = Layout::new(defined)?;
    let mut bytes = Bytes(vec![
        0;
        usize::try_from(layout.size).map_err(|_| too_large())?
    ]);
    let mut desc = DataDescription::new();
    let this = module.declare_data_in_data(id, &mut desc);
    // The functions table, then the string table.
    let count = u64::try_from(defined.len()).map_err(|_| too_large())?;
    bytes.u64(FUNCTIONS, count)?;
    if count > 0 {
        desc.write_data_addr(add(FUNCTIONS, TABLE_PTR)?, this, i64::from(HEADER));
    }
    bytes.u64(
        STRINGS,
        u64::try_from(layout.strings.len()).map_err(|_| too_large())?,
    )?;
    if !layout.strings.is_empty() {
        desc.write_data_addr(add(STRINGS, TABLE_PTR)?, this, i64::from(layout.strings_at));
    }
    let mut entry = HEADER;
    let mut site_at = layout.sites_at;
    for (code, sites) in defined.iter().zip(&layout.sites) {
        let func = module.declare_func_in_data(code.id, &mut desc);
        desc.write_function_addr(add(entry, FN_CODE)?, func);
        bytes.u32(add(entry, FN_LEN)?, code.len)?;
        let flags = if code.cheby_abi { FN_CHEBY_ABI } else { 0 };
        bytes.u32(add(entry, FN_FLAGS)?, flags)?;
        let site_count = u64::try_from(sites.len()).map_err(|_| too_large())?;
        bytes.u64(add(entry, FN_SITES)?, site_count)?;
        if site_count > 0 {
            desc.write_data_addr(
                add(add(entry, FN_SITES)?, TABLE_PTR)?,
                this,
                i64::from(site_at),
            );
        }
        for site in sites {
            bytes.u32(add(site_at, SITE_RET)?, site.ret_offset)?;
            bytes.u32(add(site_at, SITE_PATH)?, site.path)?;
            bytes.u32(add(site_at, SITE_LINE)?, site.line)?;
            bytes.u32(add(site_at, SITE_COLUMN)?, site.column)?;
            site_at = add(site_at, SITE_ENTRY)?;
        }
        entry = add(entry, FUNCTION_ENTRY)?;
    }
    let strings_at = usize::try_from(layout.strings_at).map_err(|_| too_large())?;
    bytes
        .0
        .get_mut(strings_at..)
        .ok_or_else(too_large)?
        .copy_from_slice(&layout.strings);
    desc.define(bytes.0.into_boxed_slice());
    desc.set_align(ALIGN);
    module.define_data(id, &desc).map_err(module_error)?;
    Ok(id)
}

/// Where everything of a record goes.
struct Layout {
    /// Each function's call sites, their paths interned.
    sites: Vec<Vec<CallSiteEntry>>,
    /// The offset of the first call site.
    sites_at: u32,
    /// The string table's bytes.
    strings: Vec<u8>,
    /// The string table's offset.
    strings_at: u32,
    /// The record's size.
    size: u32,
}

impl Layout {
    fn new(defined: &[DefinedCode]) -> Result<Self, RecordError> {
        let mut strings = Vec::new();
        let mut interned: BTreeMap<&str, u32> = BTreeMap::new();
        let mut sites = Vec::with_capacity(defined.len());
        let mut site_count: u32 = 0;
        for code in defined {
            let mut entries = Vec::with_capacity(code.sites.len());
            for site in &code.sites {
                // As `cheby_runtime::locate` reads a location: split from
                // the right, so a path may hold colons (`<repl:3>:1:16`);
                // one without that form has no printable location.
                let (path, line, column) = match split_location(&site.location) {
                    Some((path, line, column)) => {
                        let offset = if let Some(&offset) = interned.get(path) {
                            offset
                        } else {
                            let offset = u32::try_from(strings.len()).map_err(|_| too_large())?;
                            let len = u32::try_from(path.len()).map_err(|_| too_large())?;
                            strings.extend_from_slice(&len.to_le_bytes());
                            strings.extend_from_slice(path.as_bytes());
                            interned.insert(path, offset);
                            offset
                        };
                        (offset, line, column)
                    }
                    None => (NO_STRING, 0, 0),
                };
                entries.push(CallSiteEntry {
                    ret_offset: site.ret_offset,
                    path,
                    line,
                    column,
                });
                site_count = site_count.checked_add(1).ok_or_else(too_large)?;
            }
            sites.push(entries);
        }
        let functions = u32::try_from(defined.len()).map_err(|_| too_large())?;
        let sites_at = add(HEADER, mul(functions, FUNCTION_ENTRY)?)?;
        let strings_at = add(sites_at, mul(site_count, SITE_ENTRY)?)?;
        let size = add(
            strings_at,
            u32::try_from(strings.len()).map_err(|_| too_large())?,
        )?;
        Ok(Self {
            sites,
            sites_at,
            strings,
            strings_at,
            size,
        })
    }
}

/// Splits `path:line:column` from the right, as `cheby_runtime::locate`
/// does, so a path may hold colons; `None` without that form.
fn split_location(location: &str) -> Option<(&str, u32, u32)> {
    let mut parts = location.rsplitn(3, ':');
    let column = parts.next()?.parse().ok()?;
    let line = parts.next()?.parse().ok()?;
    let path = parts.next().filter(|path| !path.is_empty())?;
    Some((path, line, column))
}

/// The record's bytes, written at offsets that [`Layout`] keeps inside.
struct Bytes(Vec<u8>);

impl Bytes {
    fn put(&mut self, at: u32, value: &[u8]) -> Result<(), RecordError> {
        let start = usize::try_from(at).map_err(|_| too_large())?;
        let end = start.checked_add(value.len()).ok_or_else(too_large)?;
        self.0
            .get_mut(start..end)
            .ok_or(RecordError::Malformed("a module record field outside it"))?
            .copy_from_slice(value);
        Ok(())
    }

    fn u32(&mut self, at: u32, value: u32) -> Result<(), RecordError> {
        self.put(at, &value.to_le_bytes())
    }

    fn u64(&mut self, at: u32, value: u64) -> Result<(), RecordError> {
        self.put(at, &value.to_le_bytes())
    }
}

fn too_large() -> RecordError {
    RecordError::Unsupported("a module record larger than 4 GiB".into())
}

fn add(a: u32, b: u32) -> Result<u32, RecordError> {
    a.checked_add(b).ok_or_else(too_large)
}

fn mul(a: u32, b: u32) -> Result<u32, RecordError> {
    a.checked_mul(b).ok_or_else(too_large)
}
