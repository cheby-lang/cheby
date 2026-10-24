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
//! | after the sites  | one `TypeEntry` (32 bytes) per user type, in [`RecordType`]   |
//! |                  | order                                                         |
//! | after the types  | every type's `CtorEntry`s (48 bytes), types in order, each    |
//! |                  | type's constructors in tag order                              |
//! | after the ctors  | every constructor's `FieldEntry`s (16 bytes), constructors in |
//! |                  | order, each constructor's fields in declaration order         |
//! | after the fields | one `HookEntry` (32 bytes) per hooked type, in type order     |
//! | after the hooks  | the string table: each string once, `u32` length then UTF-8:  |
//! |                  | the call sites' paths first, then the types' strings          |
//!
//! Its table pointers are relocations to the record itself with the
//! table's offset as addend; each function's code pointer is a relocation
//! to the function; a constructor's `layout_word` and `ctor_static` and a
//! hook's three function values are data relocations to the unit's own
//! `cb$layout$…` word, `cb$ctor$…` static and `cb$fnval$…` values. An
//! empty table is count 0 and pointer 0, and a constructor with fields
//! has `ctor_static` 0. A unit without user layouts writes no type, so its
//! record is the call-site table and the string table alone.
//!
//! The call-site table (D-856, D-867), the types (D-919, D-522) and the
//! hooks (D-482) are filled. The external types and free shapes are
//! N5b's, and the constants K5a's: they stay empty, and `image::install`
//! refuses what it cannot register yet. A record is a pure function of its
//! unit (D-915): it holds no layout id, which an executable's start
//! assigns (D-920).

use std::collections::BTreeMap;
use std::mem::{offset_of, size_of};

use cranelift_codegen::ir::GlobalValue;
use cranelift_module::{DataDescription, DataId, Linkage, Module};

use cheby_iface::DefId;
use cheby_mir::{LayoutDesc, MirModule, Repr};
use cheby_runtime::image::{
    CallSiteEntry, CtorEntry, FIELD_KINDS, FN_CHEBY_ABI, FieldEntry, FunctionEntry, HookEntry,
    ModuleRecord, NO_STRING, TypeEntry,
};
use cheby_runtime::layout::FieldKind;

use crate::record::RecordError;
use crate::{CodegenError, Decls, DefinedCode};

/// The offsets of `ModuleRecord`'s tables (its fields are private; the
/// module documentation gives them).
const STRINGS: u32 = 0;
const TYPES: u32 = 16;
const HOOKS: u32 = 48;
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
};

/// One user type of the unit, as its record lists it (D-919, D-522).
#[derive(Clone, Debug)]
pub(crate) struct RecordType {
    /// The type's name as debug printing shows it (D-463).
    pub name: String,
    /// The type's key (D-559): `cheby_runtime::layout::type_key(<module
    /// path>, <type name>)`, as `LayoutDesc::type_key` carries it.
    pub type_key: u64,
    /// The constructors in tag order.
    pub ctors: Vec<RecordCtor>,
    /// The static function values of `equal`, `hash` and `debug` (D-482):
    /// `Some` only when the type has `@std_hooks` and the unit defines all
    /// three `cb$fnval$…` values (`Decls::fn_value` is `Some` for each), as
    /// `cheby_jit`'s `register_hooks` decides; never a partial or null set.
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

/// The user types of `unit`, compiled alone with `decls`, in the order
/// `cheby_jit`'s `register_layouts` registers them
/// (GAP(aot-layout-registration-order)): types in the order of their first
/// constructor in `MirModule::layouts` (`DefIndex` order), each type's
/// constructors in tag order, with the facts its `layout_info` reads from
/// each `LayoutDesc`, and hooks exactly where its `register_hooks` sets
/// them (all three function values present).
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
            name: first.type_name.to_string(),
            type_key: first.type_key,
            ctors,
            hooks,
        });
    }
    Ok(types)
}

/// The record's facts of one constructor (`cheby_jit`'s `layout_info`).
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

/// Declares the read-only data object `name` with `Linkage::Hidden`
/// (D-917) in `module` and defines it as the module record of a unit whose
/// functions are `defined`, in definition order (`UnitCode::defined`), and
/// whose user types are `types` ([`record_types`]).
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
    types: &[RecordType],
) -> Result<DataId, RecordError> {
    let module_error = |e: cranelift_module::ModuleError| CodegenError::module(&e);
    let id = module
        .declare_data(name, Linkage::Hidden, false, false)
        .map_err(module_error)?;
    let layout = Layout::new(defined, types)?;
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
    /// Each type's strings, in the order of the types.
    type_strings: Vec<TypeStrings>,
    /// The string table's bytes.
    strings: Vec<u8>,
    /// The string table's offset.
    strings_at: u32,
    /// The record's size.
    size: u32,
}

impl Layout {
    fn new(defined: &[DefinedCode], types: &[RecordType]) -> Result<Self, RecordError> {
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
        let functions = u32::try_from(defined.len()).map_err(|_| too_large())?;
        let type_count = u32::try_from(types.len()).map_err(|_| too_large())?;
        let sites_at = add(HEADER, mul(functions, FUNCTION_ENTRY)?)?;
        let types_at = add(sites_at, mul(site_count, SITE_ENTRY)?)?;
        let ctors_at = add(types_at, mul(type_count, TYPE_ENTRY)?)?;
        let fields_at = add(ctors_at, mul(ctor_count, CTOR_ENTRY)?)?;
        let hooks_at = add(fields_at, mul(field_count, FIELD_ENTRY)?)?;
        let strings_at = add(hooks_at, mul(hook_count, HOOK_ENTRY)?)?;
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
            type_strings,
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
