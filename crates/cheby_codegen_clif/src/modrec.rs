//! The module record `cb$mod$<path>` of a unit (D-919), in the layout of
//! `cheby_runtime::image::ModuleRecord` (that module's documentation is
//! the contract: 8-byte words that one absolute data relocation fills,
//! little-endian counts, length-prefixed strings). A unit compiled alone
//! gets one in its code record ([`define_unit`]); a JIT batch defines one
//! for each of its code and tests units ([`define_batch_records`]), which
//! the JIT registers through the same runtime routine as an executable's
//! start, `cheby_runtime::image::register_records`.
//!
//! The record is one read-only data object, 8-byte aligned, laid out as:
//!
//! | Offset              | Contents                                                      |
//! | ------------------- | ------------------------------------------------------------- |
//! | 0                   | the seven tables of `ModuleRecord`, 16 bytes each             |
//! | 112                 | one `FunctionEntry` (32 bytes) per function, definition order |
//! | after them          | each function's `CallSiteEntry`s (16 bytes), in its order     |
//! | after the sites     | one `TypeEntry` (32 bytes) per user type, in [`RecordType`]   |
//! |                     | order                                                         |
//! | after the types     | every type's `CtorEntry`s (48 bytes), types in order, each    |
//! |                     | type's constructors in tag order                              |
//! | after the ctors     | every constructor's `FieldEntry`s (16 bytes), constructors in |
//! |                     | order, each constructor's fields in declaration order         |
//! | after the fields    | one `HookEntry` (32 bytes) per hooked type, in type order     |
//! | after the hooks     | one `ExternalEntry` (24 bytes) per external type, `DefIndex`  |
//! |                     | order                                                         |
//! | after the externals | one free-shape word (8 bytes) per constructor, in the order   |
//! |                     | of the `CtorEntry`s                                           |
//! | after the shapes    | one `ConstEntry` (16 bytes) per constant the unit initializes |
//! |                     | (a unit alone only), in function (`DefIndex`) order           |
//! | after the constants | the string table: each string once, `u32` length then UTF-8:  |
//! |                     | the call sites' paths first, then the types' strings, then    |
//! |                     | the external types' names, then the constants' paths          |
//!
//! Its table pointers are relocations to the record itself with the
//! table's offset as addend; each function's code pointer is a relocation
//! to the function; a constructor's `layout_word` and `ctor_static` and a
//! hook's three function values are data relocations to the unit's own
//! `cb$layout$…` word, `cb$ctor$…` static and `cb$fnval$…` values. An
//! empty table is count 0 and pointer 0, a constructor with fields has
//! `ctor_static` 0, and an external type's `drop` is 0 (user drop
//! functions come later, D-101, D-703, D-1168). A
//! unit without user layouts or external types writes no type, external
//! or shape, so its record is the call-site table and the string table
//! alone.
//!
//! The call-site table (D-856, D-867), the types (D-919, D-522), the
//! hooks (D-482), the external types (std's handle types aside, which are
//! runtime kinds, D-647, D-982) and the free-shape words (D-907, computed
//! with `cheby_runtime::layout::free_shape_word`, which registration
//! checks them against, D-1167) are filled. A unit alone's record lists
//! its constants, each path beside a data relocation to its slot, which
//! an executable's start reads to install the program's constant images
//! (D-979, D-1189); a JIT batch's records list none, since
//! a JIT host finds slots through `Jit::const_slot`, and a unit without
//! constants keeps the record it had. A record of a unit alone is a pure function of its
//! unit (D-915): it holds no layout id, which registration assigns
//! (D-920).

use std::collections::BTreeMap;
use std::mem::{offset_of, size_of};

use cranelift_codegen::ir::GlobalValue;
use cranelift_module::{DataDescription, DataId, Linkage, Module};

use cheby_iface::DefId;
use cheby_mir::layout::ExternalDesc;
use cheby_mir::{LayoutDesc, LayoutRef, MirModule, Repr, UnitKind};
use cheby_runtime::image::{
    CallSiteEntry, ConstEntry, CtorEntry, ExternalEntry, FIELD_KINDS, FN_CHEBY_ABI, FieldEntry,
    FunctionEntry, HookEntry, ModuleRecord, NO_STRING, TypeEntry,
};
use cheby_runtime::layout::{FieldKind, LayoutInfo, free_shape_word};

use crate::record::RecordError;
use crate::unit::UnitCode;
use crate::{CodegenError, Decls, DefinedCode};

/// The offsets of `ModuleRecord`'s tables (its fields are private; the
/// module documentation gives them).
const STRINGS: u32 = 0;
const TYPES: u32 = 16;
const EXTERNALS: u32 = 32;
const HOOKS: u32 = 48;
const FREE_SHAPES: u32 = 64;
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
const TYPE_ENTRY: u32 = 32;
const TYPE_NAME: u32 = 0;
const TYPE_KEY: u32 = 8;
const TYPE_CTORS: u32 = 16;
const CTOR_ENTRY: u32 = 48;
const CTOR_NAME: u32 = 0;
const CTOR_CTOR_NAME: u32 = 4;
const CTOR_RECORD: u32 = 8;
const CTOR_FIELDS: u32 = 16;
const CTOR_LAYOUT_WORD: u32 = 32;
const CTOR_STATIC: u32 = 40;
const FIELD_ENTRY: u32 = 16;
const FIELD_KIND: u32 = 0;
const FIELD_NAME: u32 = 4;
const FIELD_OFFSET: u32 = 8;
const HOOK_ENTRY: u32 = 32;
const HOOK_TYPE: u32 = 0;
/// The `equal`, `hash` and `debug` values, in that order.
const HOOK_VALUES: [u32; 3] = [8, 16, 24];
const EXTERNAL_ENTRY: u32 = 24;
const EXTERNAL_NAME: u32 = 0;
const EXTERNAL_KEY: u32 = 8;
const EXTERNAL_DROP: u32 = 16;
const SHAPE_WORD: u32 = 8;
const CONSTANTS: u32 = 96;
const CONST_ENTRY: u32 = 16;
const CONST_PATH: u32 = 0;
const CONST_SLOT: u32 = 8;

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
    assert!(TYPES == TABLES[1] && HOOKS == TABLES[3]);
    assert!(TYPE_ENTRY as usize == size_of::<TypeEntry>());
    assert!(TYPE_NAME as usize == offset_of!(TypeEntry, name));
    assert!(TYPE_KEY as usize == offset_of!(TypeEntry, type_key));
    assert!(TYPE_CTORS as usize == offset_of!(TypeEntry, ctors));
    assert!(CTOR_ENTRY as usize == size_of::<CtorEntry>());
    assert!(CTOR_NAME as usize == offset_of!(CtorEntry, name));
    assert!(CTOR_CTOR_NAME as usize == offset_of!(CtorEntry, ctor_name));
    assert!(CTOR_RECORD as usize == offset_of!(CtorEntry, record));
    assert!(CTOR_FIELDS as usize == offset_of!(CtorEntry, fields));
    assert!(CTOR_LAYOUT_WORD as usize == offset_of!(CtorEntry, layout_word));
    assert!(CTOR_STATIC as usize == offset_of!(CtorEntry, ctor_static));
    assert!(FIELD_ENTRY as usize == size_of::<FieldEntry>());
    assert!(FIELD_KIND as usize == offset_of!(FieldEntry, kind));
    assert!(FIELD_NAME as usize == offset_of!(FieldEntry, name));
    assert!(FIELD_OFFSET as usize == offset_of!(FieldEntry, offset));
    assert!(HOOK_ENTRY as usize == size_of::<HookEntry>());
    assert!(HOOK_TYPE as usize == offset_of!(HookEntry, type_index));
    assert!(HOOK_VALUES[0] as usize == offset_of!(HookEntry, equal));
    assert!(HOOK_VALUES[1] as usize == offset_of!(HookEntry, hash));
    assert!(HOOK_VALUES[2] as usize == offset_of!(HookEntry, debug));
    assert!(EXTERNALS == TABLES[2] && FREE_SHAPES == TABLES[4]);
    assert!(EXTERNAL_ENTRY as usize == size_of::<ExternalEntry>());
    assert!(EXTERNAL_NAME as usize == offset_of!(ExternalEntry, type_name));
    assert!(EXTERNAL_KEY as usize == offset_of!(ExternalEntry, type_key));
    assert!(EXTERNAL_DROP as usize == offset_of!(ExternalEntry, drop));
    assert!(SHAPE_WORD as usize == size_of::<u64>());
    assert!(CONSTANTS == TABLES[6]);
    assert!(CONST_ENTRY as usize == size_of::<ConstEntry>());
    assert!(CONST_PATH as usize == offset_of!(ConstEntry, path));
    assert!(CONST_SLOT as usize == offset_of!(ConstEntry, slot));
};

/// One user type of the unit, as its record lists it (D-919, D-522).
#[derive(Clone, Debug)]
pub(crate) struct RecordType {
    /// The type (not written into the record).
    pub def: DefId,
    /// The type's name as debug printing shows it (D-463).
    pub name: String,
    /// The type's key (D-559): `cheby_runtime::layout::type_key(<module
    /// path>, <type name>)`, as `LayoutDesc::type_key` carries it.
    pub type_key: u64,
    /// The constructors in tag order.
    pub ctors: Vec<RecordCtor>,
    /// The static function values of `equal`, `hash` and `debug` (D-482):
    /// `Some` only when the type has `@std_hooks` and the unit defines all
    /// three `cb$fnval$…` values (`Decls::fn_value` is `Some` for each)
    /// (D-1163); never a partial or null set.
    pub hooks: Option<[DataId; 3]>,
}

/// One constructor of a [`RecordType`].
#[derive(Clone, Debug)]
pub(crate) struct RecordCtor {
    /// `LayoutDesc::name`: the constructor's path, `module::Ctor`.
    pub name: String,
    /// The constructor's name as debug printing shows it.
    pub ctor_name: String,
    /// Whether its fields are named.
    pub record: bool,
    /// Its fields in declaration order: the index of the field's kind in
    /// `cheby_runtime::image::FIELD_KINDS`, its name, its byte offset.
    pub fields: Vec<(u32, Option<String>, u32)>,
    /// Its `cb$layout$…` word, writable and `Hidden` (D-917, D-920).
    pub layout_word: DataId,
    /// The `cb$ctor$…` static of a constructor without fields.
    pub ctor_static: Option<DataId>,
}

/// The user types of `unit`, compiled with `decls`, in D-1163's order:
/// types in the order of their first constructor in `MirModule::layouts`
/// (`DefIndex` order), each type's constructors in tag order, with the
/// facts of each `LayoutDesc`, and hooks only where all three function
/// values are present.
///
/// # Errors
///
/// An internal error if a type's constructors do not have the tags 0, 1,
/// …, disagree on their type's name or key, or were not declared.
pub(crate) fn record_types(
    unit: &MirModule,
    decls: &Decls,
) -> Result<Vec<RecordType>, CodegenError> {
    let mut order: Vec<DefId> = Vec::new();
    for desc in &unit.layouts {
        if !order.contains(&desc.type_def) {
            order.push(desc.type_def);
        }
    }
    let mut types = Vec::with_capacity(order.len());
    for ty in order {
        let mut ctors: Vec<&LayoutDesc> =
            unit.layouts.iter().filter(|d| d.type_def == ty).collect();
        ctors.sort_by_key(|d| d.tag);
        let in_order = ctors
            .iter()
            .enumerate()
            .all(|(n, d)| usize::try_from(d.tag).is_ok_and(|tag| tag == n));
        let Some(first) = ctors.first() else {
            continue;
        };
        let same_type = ctors
            .iter()
            .all(|d| d.type_name == first.type_name && d.type_key == first.type_key);
        if !in_order || !same_type {
            return Err(CodegenError::ice(format!(
                "the constructors of the type of `{}` do not have the tags 0, 1, … or one type",
                first.name
            )));
        }
        let ctors = ctors
            .iter()
            .map(|desc| record_ctor(desc, decls))
            .collect::<Result<Vec<_>, _>>()?;
        let hooks = match first.hooks {
            Some(defs) => {
                let [equal, hash, debug] = defs;
                match (
                    decls.fn_value(unit, equal)?,
                    decls.fn_value(unit, hash)?,
                    decls.fn_value(unit, debug)?,
                ) {
                    (Some(equal), Some(hash), Some(debug)) => Some([equal, hash, debug]),
                    _ => None,
                }
            }
            None => None,
        };
        types.push(RecordType {
            def: ty,
            name: first.type_name.to_string(),
            type_key: first.type_key,
            ctors,
            hooks,
        });
    }
    Ok(types)
}

/// The record's facts of one constructor, from its `LayoutDesc` (D-1163).
fn record_ctor(desc: &LayoutDesc, decls: &Decls) -> Result<RecordCtor, CodegenError> {
    let decl = decls.layout(desc.layout)?;
    let fields = desc
        .fields
        .iter()
        .map(|field| {
            let offset = u32::try_from(field.offset).map_err(|_| {
                CodegenError::ice(format!("a field of `{}` has a negative offset", desc.name))
            })?;
            Ok((
                field_kind_index(field.repr),
                field.name.as_deref().map(str::to_owned),
                offset,
            ))
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    if decl.ctor.is_some() != desc.fields.is_empty() {
        return Err(CodegenError::ice(format!(
            "`{}` has a constructor static exactly when it has fields",
            desc.name
        )));
    }
    Ok(RecordCtor {
        name: desc.name.to_string(),
        ctor_name: desc.ctor_name.to_string(),
        record: desc.record,
        fields,
        layout_word: decl.word,
        ctor_static: decl.ctor,
    })
}

/// The index in `FIELD_KINDS` of the kind `cheby_jit` gives a field of
/// representation `repr`.
fn field_kind_index(repr: Repr) -> u32 {
    let kind = match repr {
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
    };
    FIELD_KINDS
        .iter()
        .position(|&k| k == kind)
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or(u32::MAX)
}

/// One module record of a JIT batch ([`define_batch_records`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchRecord {
    /// The unit's index in the batch.
    pub unit: usize,
    /// The record, a read-only anonymous data object of the batch's
    /// module, whose address the module gives once finalized.
    pub data: DataId,
}

/// What a JIT batch registers that no earlier batch registered
/// ([`define_batch_records`]).
pub struct BatchNew<'a> {
    /// Whether the user constructor layout was declared by this batch,
    /// not by an earlier one.
    pub layout: &'a dyn Fn(LayoutRef) -> bool,
    /// Whether no earlier batch registered the external type.
    pub external: &'a dyn Fn(DefId) -> bool,
}

impl std::fmt::Debug for BatchNew<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchNew").finish_non_exhaustive()
    }
}

/// Defines in `module` the module record of each code and tests unit of
/// a JIT batch, `mirs`, whose functions [`crate::define_with_stats`]
/// defined as `defined` with `decls` (D-919, D-1107,
/// D-1169): the JIT registers them with
/// `cheby_runtime::image::register_records` once the batch is finalized,
/// as an executable's start registers its records. An entries unit gets no
/// record (D-1107, D-1111), so its entry thunks and trampolines are in no
/// registered range, as in an executable.
///
/// Each record lists the unit's functions of `defined`, in definition
/// order: a function belongs to the first unit of the batch that lists
/// it, as [`crate::define_with_stats`] defines it once; a large frame's
/// inner body `cb$big$…` to its outer function's unit (it follows it);
/// a `@blocking` shim `cb$blk$…`, which the batch defines once, to the
/// batch's first code or tests unit. Its types are the unit's user types
/// ([`record_types`]) whose layouts this batch declared (`new.layout`),
/// and its external types those no earlier batch registered
/// (`new.external`), so a REPL batch registers only what earlier batches
/// did not. Records are anonymous: a module's units may be compiled in
/// several batches of one JIT, and nothing links to a JIT record by name.
///
/// # Errors
///
/// [`RecordError::Codegen`] if a type has constructors this batch
/// declared and constructors an earlier batch declared, a function was
/// not declared, or the module refuses a definition, and
/// [`RecordError::Unsupported`] if a record would not fit 4 GiB.
pub fn define_batch_records<M: Module>(
    module: &mut M,
    decls: &Decls,
    mirs: &[MirModule],
    defined: &[DefinedCode],
    new: &BatchNew<'_>,
) -> Result<Vec<BatchRecord>, RecordError> {
    let units = batch_function_units(decls, mirs, defined)?;
    let mut out = Vec::new();
    for (index, unit) in mirs.iter().enumerate() {
        if matches!(unit.unit, UnitKind::Entries) {
            continue;
        }
        let functions: Vec<DefinedCode> = defined
            .iter()
            .zip(&units)
            .filter(|&(_, &owner)| owner == Some(index))
            .map(|(code, _)| code.clone())
            .collect();
        let mut types = Vec::new();
        for ty in record_types(unit, decls)? {
            let ctors = unit.layouts.iter().filter(|desc| desc.type_def == ty.def);
            let fresh = ctors
                .clone()
                .filter(|desc| (new.layout)(desc.layout))
                .count();
            if fresh == 0 {
                continue;
            }
            if fresh != ctors.count() {
                return Err(CodegenError::ice(format!(
                    "some constructors of the type `{}` are registered and some are not",
                    ty.name
                ))
                .into());
            }
            types.push(ty);
        }
        let externals: Vec<ExternalDesc> = unit
            .externals
            .iter()
            .filter(|external| (new.external)(external.def))
            .cloned()
            .collect();
        // A JIT host finds a constant's slot by its def (`Jit::const_slot`),
        // so a batch's records list no constant (D-1189).
        let data = define_record(module, None, &functions, &types, &externals, &[])?;
        out.push(BatchRecord { unit: index, data });
    }
    Ok(out)
}

/// The index of the unit each function of `defined` belongs to
/// ([`define_batch_records`]), `None` for none.
fn batch_function_units(
    decls: &Decls,
    mirs: &[MirModule],
    defined: &[DefinedCode],
) -> Result<Vec<Option<usize>>, RecordError> {
    let mut owners: rustc_hash::FxHashMap<cranelift_module::FuncId, usize> =
        rustc_hash::FxHashMap::default();
    for (index, unit) in mirs.iter().enumerate() {
        for function in &unit.functions {
            let id = decls.func(function.key)?.id;
            owners.entry(id).or_insert(index);
        }
    }
    let first_module_unit = mirs
        .iter()
        .position(|unit| !matches!(unit.unit, UnitKind::Entries));
    let shim_prefix = crate::mangle::blocking_shim("", 0);
    let mut out = Vec::with_capacity(defined.len());
    let mut previous = None;
    for code in defined {
        let owner = match owners.get(&code.id) {
            Some(&index) => Some(index),
            None if decls.func_name(code.id).starts_with(&shim_prefix) => first_module_unit,
            // A large frame's inner body, right after its outer function.
            None => previous,
        };
        out.push(owner);
        previous = owner;
    }
    Ok(out)
}

/// Declares the read-only data object `name` with `Linkage::Hidden`
/// (D-917) in `module` and defines it as the module record of `code`, a
/// unit compiled alone ([`crate::compile_unit`]): its functions in
/// definition order, its user types ([`record_types`]), its external
/// types, the free-shape words of its user layouts (D-919) and the
/// constants it initializes with their slots (D-979,
/// D-1189).
///
/// # Errors
///
/// [`RecordError::Codegen`] if the module refuses the declaration or the
/// definition, and [`RecordError::Unsupported`] if the record would not
/// fit 4 GiB.
pub(crate) fn define_unit<M: Module>(
    module: &mut M,
    name: &str,
    code: &UnitCode,
) -> Result<DataId, RecordError> {
    define_record(
        module,
        Some(name),
        &code.defined,
        &code.types,
        &code.externals,
        &code.consts,
    )
}

/// Declares a read-only data object in `module`, `name` with
/// `Linkage::Hidden` (D-917) or an anonymous one for `None` (a JIT
/// batch's record, which nothing names), and defines it as the module
/// record of a unit whose functions are `defined`, in definition order,
/// whose user types are `types` ([`record_types`]), whose external
/// types are `externals` and whose constants are `consts` (path and
/// slot).
///
/// # Errors
///
/// As [`define_unit`].
fn define_record<M: Module>(
    module: &mut M,
    name: Option<&str>,
    defined: &[DefinedCode],
    types: &[RecordType],
    externals: &[ExternalDesc],
    consts: &[(String, DataId)],
) -> Result<DataId, RecordError> {
    let module_error = |e: cranelift_module::ModuleError| CodegenError::module(&e);
    let id = match name {
        Some(name) => module.declare_data(name, Linkage::Hidden, false, false),
        None => module.declare_anonymous_data(false, false),
    }
    .map_err(module_error)?;
    let layout = Layout::new(defined, types, externals, consts)?;
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
    write_types(module, &mut desc, this, &mut bytes, &layout, types)?;
    write_externals(&mut desc, this, &mut bytes, &layout, externals)?;
    write_consts(module, &mut desc, this, &mut bytes, &layout, consts)?;
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

/// Writes the `types` and `hooks` tables of the record `this` (module
/// documentation) at the offsets `layout` gives: the entries into `bytes`,
/// their pointers as relocations into `desc`.
fn write_types<M: Module>(
    module: &mut M,
    desc: &mut DataDescription,
    this: GlobalValue,
    bytes: &mut Bytes,
    layout: &Layout,
    types: &[RecordType],
) -> Result<(), RecordError> {
    // One global value per data object the record points to.
    let mut targets: BTreeMap<u32, GlobalValue> = BTreeMap::new();
    let mut target = |desc: &mut DataDescription, id: DataId| {
        *targets
            .entry(id.as_u32())
            .or_insert_with(|| module.declare_data_in_data(id, desc))
    };
    let count = |n: usize| u64::try_from(n).map_err(|_| too_large());
    bytes.u64(TYPES, count(types.len())?)?;
    if !types.is_empty() {
        desc.write_data_addr(add(TYPES, TABLE_PTR)?, this, i64::from(layout.types_at));
    }
    let mut type_at = layout.types_at;
    let mut ctor_at = layout.ctors_at;
    let mut field_at = layout.fields_at;
    let mut hook_at = layout.hooks_at;
    let mut hooks: u64 = 0;
    for (index, (ty, names)) in types.iter().zip(&layout.type_strings).enumerate() {
        bytes.u32(add(type_at, TYPE_NAME)?, names.name)?;
        bytes.u64(add(type_at, TYPE_KEY)?, ty.type_key)?;
        bytes.u64(add(type_at, TYPE_CTORS)?, count(ty.ctors.len())?)?;
        if !ty.ctors.is_empty() {
            desc.write_data_addr(
                add(add(type_at, TYPE_CTORS)?, TABLE_PTR)?,
                this,
                i64::from(ctor_at),
            );
        }
        for (ctor, ctor_names) in ty.ctors.iter().zip(&names.ctors) {
            bytes.u32(add(ctor_at, CTOR_NAME)?, ctor_names.name)?;
            bytes.u32(add(ctor_at, CTOR_CTOR_NAME)?, ctor_names.ctor_name)?;
            bytes.u32(add(ctor_at, CTOR_RECORD)?, u32::from(ctor.record))?;
            bytes.u64(add(ctor_at, CTOR_FIELDS)?, count(ctor.fields.len())?)?;
            if !ctor.fields.is_empty() {
                desc.write_data_addr(
                    add(add(ctor_at, CTOR_FIELDS)?, TABLE_PTR)?,
                    this,
                    i64::from(field_at),
                );
            }
            let word = target(desc, ctor.layout_word);
            desc.write_data_addr(add(ctor_at, CTOR_LAYOUT_WORD)?, word, 0);
            if let Some(ctor_static) = ctor.ctor_static {
                let ctor_static = target(desc, ctor_static);
                desc.write_data_addr(add(ctor_at, CTOR_STATIC)?, ctor_static, 0);
            }
            for (&(kind, _, offset), &name) in ctor.fields.iter().zip(&ctor_names.fields) {
                bytes.u32(add(field_at, FIELD_KIND)?, kind)?;
                bytes.u32(add(field_at, FIELD_NAME)?, name)?;
                bytes.u32(add(field_at, FIELD_OFFSET)?, offset)?;
                field_at = add(field_at, FIELD_ENTRY)?;
            }
            ctor_at = add(ctor_at, CTOR_ENTRY)?;
        }
        if let Some(values) = ty.hooks {
            let type_index = u32::try_from(index).map_err(|_| too_large())?;
            bytes.u32(add(hook_at, HOOK_TYPE)?, type_index)?;
            for (value, at) in values.into_iter().zip(HOOK_VALUES) {
                let value = target(desc, value);
                desc.write_data_addr(add(hook_at, at)?, value, 0);
            }
            hook_at = add(hook_at, HOOK_ENTRY)?;
            hooks = hooks.saturating_add(1);
        }
        type_at = add(type_at, TYPE_ENTRY)?;
    }
    bytes.u64(HOOKS, hooks)?;
    if hooks > 0 {
        desc.write_data_addr(add(HOOKS, TABLE_PTR)?, this, i64::from(layout.hooks_at));
    }
    Ok(())
}

/// Writes the `externals` and `free_shapes` tables of the record `this`
/// (module documentation) at the offsets `layout` gives: one entry per
/// external type, its `drop` 0 (D-1168), and one
/// free-shape word per constructor (D-907, D-1167).
fn write_externals(
    desc: &mut DataDescription,
    this: GlobalValue,
    bytes: &mut Bytes,
    layout: &Layout,
    externals: &[ExternalDesc],
) -> Result<(), RecordError> {
    let count = |n: usize| u64::try_from(n).map_err(|_| too_large());
    bytes.u64(EXTERNALS, count(externals.len())?)?;
    if !externals.is_empty() {
        desc.write_data_addr(
            add(EXTERNALS, TABLE_PTR)?,
            this,
            i64::from(layout.externals_at),
        );
    }
    let mut at = layout.externals_at;
    for (external, &name) in externals.iter().zip(&layout.external_names) {
        bytes.u32(add(at, EXTERNAL_NAME)?, name)?;
        bytes.u64(add(at, EXTERNAL_KEY)?, external.type_key)?;
        bytes.u64(add(at, EXTERNAL_DROP)?, 0)?;
        at = add(at, EXTERNAL_ENTRY)?;
    }
    bytes.u64(FREE_SHAPES, count(layout.shapes.len())?)?;
    if !layout.shapes.is_empty() {
        desc.write_data_addr(
            add(FREE_SHAPES, TABLE_PTR)?,
            this,
            i64::from(layout.shapes_at),
        );
    }
    let mut at = layout.shapes_at;
    for &word in &layout.shapes {
        bytes.u64(at, word)?;
        at = add(at, SHAPE_WORD)?;
    }
    Ok(())
}

/// Writes the `constants` table of the record `this` at the offset
/// `layout` gives: each constant's path string and a data relocation to
/// its slot (D-1189).
fn write_consts<M: Module>(
    module: &mut M,
    desc: &mut DataDescription,
    this: GlobalValue,
    bytes: &mut Bytes,
    layout: &Layout,
    consts: &[(String, DataId)],
) -> Result<(), RecordError> {
    let count = u64::try_from(consts.len()).map_err(|_| too_large())?;
    bytes.u64(CONSTANTS, count)?;
    if count > 0 {
        desc.write_data_addr(
            add(CONSTANTS, TABLE_PTR)?,
            this,
            i64::from(layout.consts_at),
        );
    }
    let mut at = layout.consts_at;
    for ((_, slot), &path) in consts.iter().zip(&layout.const_paths) {
        bytes.u32(add(at, CONST_PATH)?, path)?;
        let slot = module.declare_data_in_data(*slot, desc);
        desc.write_data_addr(add(at, CONST_SLOT)?, slot, 0);
        at = add(at, CONST_ENTRY)?;
    }
    Ok(())
}

/// The free-shape word of each constructor of `types`, types in order and
/// each type's constructors in tag order: what registration derives for
/// the layout it makes from the record's facts (D-907, D-1163), computed
/// with the runtime's own function, so `register_records` finds the words
/// it derives (D-1167).
fn free_shapes(types: &[RecordType]) -> Result<Vec<u64>, RecordError> {
    let mut words = Vec::new();
    for ty in types {
        for (tag, ctor) in ty.ctors.iter().enumerate() {
            let tag = u32::try_from(tag).map_err(|_| too_large())?;
            let fields = ctor
                .fields
                .iter()
                .map(|(kind, name, _)| {
                    let kind = usize::try_from(*kind)
                        .ok()
                        .and_then(|kind| FIELD_KINDS.get(kind))
                        .ok_or(RecordError::Malformed("a field kind of a module record"))?;
                    Ok((*kind, name.as_deref()))
                })
                .collect::<Result<Vec<(FieldKind, Option<&str>)>, RecordError>>()?;
            let info = LayoutInfo::ctor(&ctor.name, tag, &fields).named(
                &ty.name,
                &ctor.ctor_name,
                ctor.record,
                ty.type_key,
            );
            words.push(free_shape_word(&info));
        }
    }
    Ok(words)
}

/// The string offsets of one [`RecordType`].
struct TypeStrings {
    name: u32,
    ctors: Vec<CtorStrings>,
}

/// The string offsets of one [`RecordCtor`]: [`NO_STRING`] for a field
/// without a name.
struct CtorStrings {
    name: u32,
    ctor_name: u32,
    fields: Vec<u32>,
}

/// A record's string table under construction: each string once, in the
/// order first interned.
#[derive(Default)]
struct Strings<'a> {
    bytes: Vec<u8>,
    interned: BTreeMap<&'a str, u32>,
}

impl<'a> Strings<'a> {
    fn intern(&mut self, text: &'a str) -> Result<u32, RecordError> {
        if let Some(&offset) = self.interned.get(text) {
            return Ok(offset);
        }
        let offset = u32::try_from(self.bytes.len()).map_err(|_| too_large())?;
        let len = u32::try_from(text.len()).map_err(|_| too_large())?;
        self.bytes.extend_from_slice(&len.to_le_bytes());
        self.bytes.extend_from_slice(text.as_bytes());
        self.interned.insert(text, offset);
        Ok(offset)
    }
}

/// Where everything of a record goes.
struct Layout {
    /// Each function's call sites, their paths interned.
    sites: Vec<Vec<CallSiteEntry>>,
    /// The offset of the first call site.
    sites_at: u32,
    /// The offsets of the type, constructor, field and hook entries.
    types_at: u32,
    ctors_at: u32,
    fields_at: u32,
    hooks_at: u32,
    /// The offsets of the external entries, of the free-shape words and
    /// of the constant entries.
    externals_at: u32,
    shapes_at: u32,
    consts_at: u32,
    /// Each type's strings, in the order of the types.
    type_strings: Vec<TypeStrings>,
    /// Each external type's name.
    external_names: Vec<u32>,
    /// Each constant's path.
    const_paths: Vec<u32>,
    /// The free-shape words ([`free_shapes`]).
    shapes: Vec<u64>,
    /// The string table's bytes.
    strings: Vec<u8>,
    /// The string table's offset.
    strings_at: u32,
    /// The record's size.
    size: u32,
}

impl Layout {
    fn new(
        defined: &[DefinedCode],
        types: &[RecordType],
        externals: &[ExternalDesc],
        consts: &[(String, DataId)],
    ) -> Result<Self, RecordError> {
        let mut table = Strings::default();
        let mut sites = Vec::with_capacity(defined.len());
        let mut site_count: u32 = 0;
        for code in defined {
            let mut entries = Vec::with_capacity(code.sites.len());
            for site in &code.sites {
                // As `cheby_runtime::locate` reads a location: split from
                // the right, so a path may hold colons (`<repl:3>:1:16`);
                // one without that form has no printable location.
                let (path, line, column) = match split_location(&site.location) {
                    Some((path, line, column)) => (table.intern(path)?, line, column),
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
        // The types' strings after the paths, so a unit without types
        // keeps the string table it had before records carried types.
        let mut type_strings = Vec::with_capacity(types.len());
        let (mut ctor_count, mut field_count, mut hook_count) = (0_u32, 0_u32, 0_u32);
        for ty in types {
            let name = table.intern(&ty.name)?;
            let mut ctors = Vec::with_capacity(ty.ctors.len());
            for ctor in &ty.ctors {
                let names = CtorStrings {
                    name: table.intern(&ctor.name)?,
                    ctor_name: table.intern(&ctor.ctor_name)?,
                    fields: ctor
                        .fields
                        .iter()
                        .map(|(_, name, _)| {
                            name.as_deref().map_or(Ok(NO_STRING), |n| table.intern(n))
                        })
                        .collect::<Result<_, _>>()?,
                };
                ctors.push(names);
                ctor_count = add(ctor_count, 1)?;
                field_count = add(
                    field_count,
                    u32::try_from(ctor.fields.len()).map_err(|_| too_large())?,
                )?;
            }
            if ty.hooks.is_some() {
                hook_count = add(hook_count, 1)?;
            }
            type_strings.push(TypeStrings { name, ctors });
        }
        // The external types' names last, so a unit without them keeps
        // the string table it had before records listed them.
        let external_names = externals
            .iter()
            .map(|external| table.intern(&external.name))
            .collect::<Result<Vec<_>, _>>()?;
        // The constants' paths after them, so a unit without constants
        // keeps the string table it had before records listed them.
        let const_paths = consts
            .iter()
            .map(|(path, _)| table.intern(path))
            .collect::<Result<Vec<_>, _>>()?;
        let shapes = free_shapes(types)?;
        let functions = u32::try_from(defined.len()).map_err(|_| too_large())?;
        let type_count = u32::try_from(types.len()).map_err(|_| too_large())?;
        let external_count = u32::try_from(externals.len()).map_err(|_| too_large())?;
        let shape_count = u32::try_from(shapes.len()).map_err(|_| too_large())?;
        let const_count = u32::try_from(consts.len()).map_err(|_| too_large())?;
        let sites_at = add(HEADER, mul(functions, FUNCTION_ENTRY)?)?;
        let types_at = add(sites_at, mul(site_count, SITE_ENTRY)?)?;
        let ctors_at = add(types_at, mul(type_count, TYPE_ENTRY)?)?;
        let fields_at = add(ctors_at, mul(ctor_count, CTOR_ENTRY)?)?;
        let hooks_at = add(fields_at, mul(field_count, FIELD_ENTRY)?)?;
        let externals_at = add(hooks_at, mul(hook_count, HOOK_ENTRY)?)?;
        let shapes_at = add(externals_at, mul(external_count, EXTERNAL_ENTRY)?)?;
        let consts_at = add(shapes_at, mul(shape_count, SHAPE_WORD)?)?;
        let strings_at = add(consts_at, mul(const_count, CONST_ENTRY)?)?;
        let strings = table.bytes;
        let size = add(
            strings_at,
            u32::try_from(strings.len()).map_err(|_| too_large())?,
        )?;
        Ok(Self {
            sites,
            sites_at,
            types_at,
            ctors_at,
            fields_at,
            hooks_at,
            externals_at,
            shapes_at,
            consts_at,
            type_strings,
            external_names,
            const_paths,
            shapes,
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
