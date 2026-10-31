//! The program object (D-915, D-919, D-925, D-930): the one object of a
//! link that is specific to the program, built anew for every link and
//! never cached (D-964).
//!
//! It holds:
//!
//! - the entries unit (`main`'s entry thunk and the trampolines),
//!   compiled alone into a code record and replayed here, every one of its
//!   definitions local (D-1111): the entries
//!   unit belongs to no module, so no module object holds it, and its code
//!   is never cached or shared;
//! - `cb$image`, local and read-only, in the exact layout of
//!   `cheby_runtime::image::ProgramImage` (that module's documentation is
//!   the contract), with the table of module pointers right after its 120
//!   bytes, the table's pointer a relocation to `cb$image` itself with
//!   that offset as addend, as module records point at their own tables,
//!   then the table of function bounds (D-1253, `crate::gc`): for each
//!   module record, relocations to the start and the end of the section
//!   that holds its kept entries, which the linker defines (the record's
//!   own weak `__start_cbf_<key>` and `__stop_cbf_<key>` on ELF, tag 0; the
//!   shared `section$start$__DATA$cb_fns` and `section$end$…` on Mach-O,
//!   with the record's tag), so the records stay read-only; then the
//!   constants table's bytes ([`ProgramSpec::constants`], D-979), pointed
//!   at the same way;
//! - the C `int main(int argc, char **argv)`, the program's only exported
//!   symbol (D-917), which returns `cheby_rt_main(argc, argv, &cb$image)`
//!   (D-925). It reaches `cheby_rt_main` through a local import word
//!   `cb$imp$cheby_rt_main` and the image PC-relative, so its text holds
//!   only PC-relative relocations (D-913);
//! - an import of the runtime ID's symbol `cheby_rt_id$<hex>` through the
//!   import word `cb$imp$cheby_rt_id$<hex>`, which `main` loads, so that a
//!   linker that drops unreferenced sections still needs the symbol and an
//!   archive of other runtime sources cannot link (D-930);
//! - D-943's stubs, one per native symbol that only unreached functions of
//!   whole modules (D-966) name and the link cannot resolve
//!   ([`ProgramSpec::stubs`]): a function of that name, global with hidden
//!   visibility (`Linkage::Hidden`: `STV_HIDDEN` on ELF, private extern on
//!   Mach-O), so the modules' import words resolve to it while the
//!   executable does not export it. It calls `cheby_rt_unresolved`
//!   through the local import word `cb$imp$cheby_rt_unresolved` and
//!   returns its result, as if the import word named `cheby_rt_unresolved`
//!   itself: like it, the stub takes no `ctx` and ignores its arguments
//!   (D-532). A program without stubs gets none of this, not even the
//!   import word.
//!
//! The module records `cb$mod$<path>` are imports: each module's object
//! defines its own hidden (D-917). Nothing in the object depends on a path
//! or on the output's name; its file symbol is [`PROGRAM_OBJECT_NAME`].

use cranelift_codegen::ir::{AbiParam, InstBuilder as _, MemFlagsData, Signature, types};
use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{DataDescription, DataId, FuncOrDataId, Linkage, Module, ModuleRelocTarget};
use cranelift_object::ObjectModule;

use cheby_codegen_clif::mangle;
use cheby_codegen_clif::record::{CodeRecord, SymbolKind, SymbolLinkage};
use cheby_runtime::abi;
use cheby_runtime::image::ProgramImage;

use crate::LinkError;
use crate::object::{check_record, emit, new_module, object_error, replay_into};

/// The program object's file symbol, which no module path can spell (`$`
/// never occurs in one).
pub const PROGRAM_OBJECT_NAME: &str = "cb$program";

/// The runtime's entry of an executable (D-925).
pub const RT_MAIN: &str = "cheby_rt_main";

/// The number of trampolines the image holds, `cb$tramp$0` to
/// `cb$tramp$4` (D-526).
const TRAMPOLINES: u32 = 5;

/// `ProgramImage`'s size and its fields' offsets, as `image.rs`
/// documents them.
const IMAGE_SIZE: u32 = 120;
const VERSION: usize = 0;
const UNWIND_EDGES: usize = 4;
const MODULES_COUNT: usize = 8;
const MODULES_PTR: u32 = 16;
const ENTRY: u32 = 24;
const TRAMP0: u32 = 32;
const MAIN_STACK: usize = 72;
const FIBER_STACK: usize = 80;
/// The constants table (D-979): its count at 88, its pointer at 96, a
/// relocation to `cb$image` itself whose addend is where the table's bytes
/// start, right after the table of module records. Count 0 and pointer 0
/// for a program without constants.
const CONSTANTS_COUNT: usize = 88;
const CONSTANTS_PTR: u32 = 96;
/// The function tables (D-1253): one `FunctionBounds` (24 bytes) per
/// module record, right after the table of module records, its count at
/// 104 and its pointer at 112 (a relocation to `cb$image` itself), each
/// bound's `start` and `end` relocations to the linker's bounds of the
/// record's entries' section and its `tag` at 16 (`crate::gc`).
const FUNCTIONS_COUNT: usize = 104;
const FUNCTIONS_PTR: u32 = 112;
/// A `FunctionBounds`, and its `tag`'s offset.
const BOUNDS_SIZE: u32 = 24;
const BOUNDS_TAG: u32 = 16;

const _: () = assert!(std::mem::size_of::<ProgramImage>() == 120);
const _: () = assert!(std::mem::size_of::<cheby_runtime::image::FunctionBounds>() == 24);
const _: () = assert!(std::mem::offset_of!(cheby_runtime::image::FunctionBounds, tag) == 16);
const _: () = assert!(BOUNDS_TAG == 16);

/// What the program object holds besides the entries unit.
#[derive(Clone, Copy, Debug)]
pub struct ProgramSpec<'a> {
    /// Every code unit's module record, `cb$mod$<path>`, in build order.
    pub module_records: &'a [String],
    /// The entry thunk of `main`, `cb$entry$main$<path>`, which the
    /// entries unit defines.
    pub entry: &'a str,
    /// The root package's `[runtime]` stacks in bytes, as `cheby run`
    /// passes them to the runtime, which rounds them up to its page size
    /// (D-660, D-796, D-927).
    pub main_stack: u64,
    pub fiber_stack: u64,
    /// Whether the code was compiled with unwind edges (D-742).
    pub unwind_edges: bool,
    /// D-943: the symbols only unreached functions of whole modules
    /// (D-966) name and the link cannot resolve; each becomes a hidden
    /// function of the program object (a global symbol with hidden
    /// visibility, `Linkage::Hidden`: `STV_HIDDEN` on ELF, private extern on
    /// Mach-O) that calls `cheby_rt_unresolved` through an import word, so
    /// the linker resolves the modules' undefined references to it and the
    /// executable does not export it. Never `Linkage::Local`, since a
    /// local symbol cannot satisfy another object's reference. Sorted,
    /// without repeats; empty for a program without one, whose object is
    /// then the same bytes as before stubs existed.
    pub stubs: &'a [String],
    /// D-979: the encoded constant images (`encode_table`), empty for a
    /// program without constants, whose object is then the same bytes as
    /// before. They follow the table of module records inside `cb$image`.
    pub constants: &'a [u8],
}

/// The program object for `isa` (D-915): the entries unit's record
/// `entries`, replayed with its definitions local, `cb$image`, the C
/// `main` and the runtime ID's import (module documentation).
///
/// The bytes depend only on `isa`, `spec` and `entries`.
///
/// # Errors
///
/// [`LinkError::Record`] if `entries` is malformed or does not fit the
/// target, [`LinkError::Object`] if it does not define `spec.entry`, if it
/// defines a symbol that is not local, if the stubs are not sorted and
/// distinct or name a generated or runtime symbol, or if
/// `cranelift-object` refuses the object.
pub fn program_object(
    isa: OwnedTargetIsa,
    spec: &ProgramSpec<'_>,
    entries: &CodeRecord,
) -> Result<Vec<u8>, LinkError> {
    check_record(entries, &isa)?;
    check_entries(entries, spec)?;
    check_stubs(spec.stubs)?;
    let format = if isa.triple().binary_format.to_string() == "macho" {
        object::BinaryFormat::MachO
    } else {
        object::BinaryFormat::Elf
    };
    let mut module = new_module(isa, PROGRAM_OBJECT_NAME)?;
    replay_into(&mut module, entries)?;
    let image = module
        .declare_data(mangle::PROGRAM_IMAGE, Linkage::Local, false, false)
        .map_err(object_error)?;
    let mut records = Vec::with_capacity(spec.module_records.len());
    for name in spec.module_records {
        records.push(
            module
                .declare_data(name, Linkage::Import, false, false)
                .map_err(object_error)?,
        );
    }
    // D-1253: the bounds of each record's entries' section, which the
    // linker defines; their names are set raw once the object is finished.
    // Two records may not share a key (ELF: their section) or a tag
    // (Mach-O: their entries' mark in the shared section).
    let mut keys = std::collections::BTreeSet::new();
    let mut tags = std::collections::BTreeSet::new();
    let mut symbols: std::collections::BTreeMap<String, DataId> = std::collections::BTreeMap::new();
    let mut bounds: Vec<(DataId, DataId, u32)> = Vec::with_capacity(records.len());
    for name in spec.module_records {
        let key = crate::gc::key(name);
        let tag = crate::gc::tag(name);
        if !keys.insert(key.clone()) || !tags.insert(tag) {
            return Err(LinkError::Object(format!(
                "two module records share the entries' key `{key}` or tag {tag:#010x}"
            )));
        }
        let (start, end) = crate::gc::bounds(format, &key);
        let mut ids = [None, None];
        for (slot, symbol) in ids.iter_mut().zip([start, end]) {
            let id = if let Some(&id) = symbols.get(&symbol) {
                id
            } else {
                let id = module
                    .declare_data(&symbol, Linkage::Import, false, false)
                    .map_err(object_error)?;
                symbols.insert(symbol, id);
                id
            };
            *slot = Some(id);
        }
        let [Some(start), Some(end)] = ids else {
            return Err(LinkError::Object("a record's bounds".into()));
        };
        bounds.push((start, end, crate::gc::image_tag(format, name)));
    }
    define_image(&mut module, image, spec, &records, &bounds)?;
    let rt_main = import_word(&mut module, RT_MAIN, true)?;
    let runtime_id = import_word(&mut module, abi::sym::RUNTIME_ID_SYMBOL, false)?;
    define_main(&mut module, image, rt_main, runtime_id)?;
    // After everything else, so a program without stubs keeps its bytes.
    if !spec.stubs.is_empty() {
        let unresolved = import_word(&mut module, abi::sym::UNRESOLVED, true)?;
        for name in spec.stubs {
            define_stub(&mut module, name, unresolved)?;
        }
    }
    if symbols.is_empty() {
        return emit(module);
    }
    let mut product = module.finish();
    for (name, id) in symbols {
        let symbol = product.data_symbol(id);
        let symbol = product.object.symbol_mut(symbol);
        // The linker's own names, without Mach-O's `_` prefix; weak on ELF,
        // so the bounds of a section the linker dropped whole are 0 and
        // the table is empty.
        symbol.name = name.into_bytes();
        if format == object::BinaryFormat::Elf {
            symbol.weak = true;
        }
    }
    product.emit().map_err(|e| LinkError::Object(e.to_string()))
}

/// D-943's stubs are sorted and distinct, and none names a generated
/// symbol (`cb$…`), the C `main` or a runtime internal (`cheby_rt_…`,
/// which E0055 keeps out of packages): such a name would clash with a
/// definition or import of the object or of the runtime.
fn check_stubs(stubs: &[String]) -> Result<(), LinkError> {
    if let Some(pair) = stubs.windows(2).find(|w| w.first() >= w.get(1)) {
        return Err(LinkError::Object(format!(
            "the stubs are not sorted and distinct: {pair:?}"
        )));
    }
    if let Some(name) = stubs.iter().find(|name| {
        name.is_empty()
            || name.starts_with(mangle::PREFIX)
            || name.starts_with("cheby_rt_")
            || *name == "main"
    }) {
        return Err(LinkError::Object(format!(
            "`{name}` cannot be a stub of an unresolved symbol"
        )));
    }
    Ok(())
}

/// Defines the stub `name` (module documentation): global with hidden
/// visibility, it loads `cheby_rt_unresolved` from the import word
/// `unresolved`, calls it without arguments and returns its result.
fn define_stub(module: &mut ObjectModule, name: &str, unresolved: DataId) -> Result<(), LinkError> {
    let pointer = module.target_config().pointer_type();
    let mut signature = Signature::new(module.isa().default_call_conv());
    signature.returns.push(AbiParam::new(types::I64));
    let stub = module
        .declare_function(name, Linkage::Hidden, &signature)
        .map_err(object_error)?;
    let mut ctx = module.make_context();
    ctx.func.signature = signature.clone();
    let mut fctx = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let block = b.create_block();
        b.switch_to_block(block);
        b.seal_block(block);
        let word = module.declare_data_in_func(unresolved, b.func);
        let word = b.ins().symbol_value(pointer, word);
        let callee = b
            .ins()
            .load(pointer, MemFlagsData::trusted().with_readonly(), word, 0);
        let signature = b.import_signature(signature);
        let call = b.ins().call_indirect(signature, callee, &[]);
        let result = b
            .inst_results(call)
            .first()
            .copied()
            .ok_or_else(|| LinkError::Object("`cheby_rt_unresolved` returns a word".into()))?;
        b.ins().return_(&[result]);
        b.finalize(module.target_config());
    }
    module.define_function(stub, &mut ctx).map_err(object_error)
}

/// The entries record defines `spec.entry`, and every definition of it
/// is local: nothing of the entries unit is shared (D-917).
fn check_entries(entries: &CodeRecord, spec: &ProgramSpec<'_>) -> Result<(), LinkError> {
    let entry = entries
        .symbol(spec.entry)
        .filter(|s| s.kind == SymbolKind::Function && s.linkage == SymbolLinkage::Local);
    if entry.is_none() {
        return Err(LinkError::Object(format!(
            "the entries unit does not define the entry thunk `{}`",
            spec.entry
        )));
    }
    if let Some(symbol) = entries
        .symbols
        .iter()
        .find(|s| matches!(s.linkage, SymbolLinkage::Hidden | SymbolLinkage::Export))
    {
        return Err(LinkError::Object(format!(
            "the entries unit defines `{}` globally",
            symbol.name
        )));
    }
    Ok(())
}

/// The function `name` the replayed entries unit defines, if it does.
fn defined_function(module: &ObjectModule, name: &str) -> Option<FuncOrDataId> {
    match module.get_name(name) {
        Some(id @ FuncOrDataId::Func(_)) => Some(id),
        _ => None,
    }
}

/// Defines `cb$image` (module documentation): its header, the table of
/// module records, the table of function bounds (`bounds`: each record's
/// start and end symbols and its tag, D-1253), then the constants table.
fn define_image(
    module: &mut ObjectModule,
    image: DataId,
    spec: &ProgramSpec<'_>,
    records: &[DataId],
    bounds: &[(DataId, DataId, u32)],
) -> Result<(), LinkError> {
    let count = u32::try_from(records.len())
        .map_err(|_| LinkError::Object("too many module records".into()))?;
    let functions_at = count
        .checked_mul(8)
        .and_then(|table| table.checked_add(IMAGE_SIZE))
        .ok_or_else(|| LinkError::Object("too many module records".into()))?;
    let functions = u32::try_from(bounds.len())
        .map_err(|_| LinkError::Object("too many module records".into()))?;
    let constants_at = functions
        .checked_mul(BOUNDS_SIZE)
        .and_then(|table| table.checked_add(functions_at))
        .ok_or_else(|| LinkError::Object("too many module records".into()))?;
    let constants_len = u32::try_from(spec.constants.len()).map_err(|_| too_large())?;
    let size = constants_at
        .checked_add(constants_len)
        .ok_or_else(too_large)?;
    let mut bytes = vec![0_u8; usize::try_from(size).map_err(|_| too_large())?];
    let constants_start = usize::try_from(constants_at).map_err(|_| too_large())?;
    put(&mut bytes, constants_start, spec.constants)?;
    put(
        &mut bytes,
        CONSTANTS_COUNT,
        &u64::from(constants_len).to_le_bytes(),
    )?;
    put(&mut bytes, VERSION, &abi::IMAGE_VERSION.to_le_bytes())?;
    put(
        &mut bytes,
        UNWIND_EDGES,
        &u32::from(spec.unwind_edges).to_le_bytes(),
    )?;
    put(&mut bytes, MODULES_COUNT, &u64::from(count).to_le_bytes())?;
    put(&mut bytes, MAIN_STACK, &spec.main_stack.to_le_bytes())?;
    put(&mut bytes, FIBER_STACK, &spec.fiber_stack.to_le_bytes())?;
    put(
        &mut bytes,
        FUNCTIONS_COUNT,
        &u64::from(functions).to_le_bytes(),
    )?;
    for (index, &(_, _, tag)) in (0_u32..).zip(bounds) {
        let at = bound_at(functions_at, index)?
            .checked_add(BOUNDS_TAG)
            .ok_or_else(too_large)?;
        put(
            &mut bytes,
            usize::try_from(at).map_err(|_| too_large())?,
            &tag.to_le_bytes(),
        )?;
    }
    let mut desc = DataDescription::new();
    desc.define(bytes.into_boxed_slice());
    desc.set_align(8);
    if count > 0 || constants_len > 0 {
        let this = desc.import_global_value(ModuleRelocTarget::from(FuncOrDataId::Data(image)));
        if count > 0 {
            desc.write_data_addr(MODULES_PTR, this, i64::from(IMAGE_SIZE));
        }
        if functions > 0 {
            desc.write_data_addr(FUNCTIONS_PTR, this, i64::from(functions_at));
        }
        if constants_len > 0 {
            desc.write_data_addr(CONSTANTS_PTR, this, i64::from(constants_at));
        }
    }
    for (index, &(start, end, _)) in (0_u32..).zip(bounds) {
        let at = bound_at(functions_at, index)?;
        for (word, symbol) in [(0, start), (8, end)] {
            let target =
                desc.import_global_value(ModuleRelocTarget::from(FuncOrDataId::Data(symbol)));
            desc.write_data_addr(at.checked_add(word).ok_or_else(too_large)?, target, 0);
        }
    }
    let entry = defined_function(module, spec.entry).ok_or_else(|| {
        LinkError::Object(format!("the entry thunk `{}` is not defined", spec.entry))
    })?;
    let entry = desc.import_global_value(ModuleRelocTarget::from(entry));
    desc.write_data_addr(ENTRY, entry, 0);
    for arity in 0..TRAMPOLINES {
        // A trampoline the program needs is in the entries unit; one it
        // does not need stays 0 (`image.rs`).
        if let Some(tramp) = defined_function(module, &mangle::trampoline(arity)) {
            let tramp = desc.import_global_value(ModuleRelocTarget::from(tramp));
            desc.write_data_addr(word_at(TRAMP0, arity)?, tramp, 0);
        }
    }
    for (index, &record) in (0_u32..).zip(records) {
        let target = desc.import_global_value(ModuleRelocTarget::from(FuncOrDataId::Data(record)));
        desc.write_data_addr(word_at(IMAGE_SIZE, index)?, target, 0);
    }
    module.define_data(image, &desc).map_err(object_error)
}

/// The offset of the `FunctionBounds` `index` of the table at `base`.
fn bound_at(base: u32, index: u32) -> Result<u32, LinkError> {
    index
        .checked_mul(BOUNDS_SIZE)
        .and_then(|offset| offset.checked_add(base))
        .ok_or_else(too_large)
}

/// The offset of the 8-byte word `index` of a table at `base`.
fn word_at(base: u32, index: u32) -> Result<u32, LinkError> {
    index
        .checked_mul(8)
        .and_then(|offset| offset.checked_add(base))
        .ok_or_else(too_large)
}

fn too_large() -> LinkError {
    LinkError::Object("the program image is too large".into())
}

/// Writes `value` at `offset` of `bytes`.
fn put(bytes: &mut [u8], offset: usize, value: &[u8]) -> Result<(), LinkError> {
    let end = offset.checked_add(value.len()).ok_or_else(too_large)?;
    bytes
        .get_mut(offset..end)
        .ok_or_else(too_large)?
        .copy_from_slice(value);
    Ok(())
}

/// Declares the import of `symbol`, a function or data, and defines its
/// local import word `cb$imp$<symbol>`: an 8-byte read-only word that one
/// absolute data relocation fills (D-913).
fn import_word(
    module: &mut ObjectModule,
    symbol: &str,
    function: bool,
) -> Result<DataId, LinkError> {
    let target = if function {
        let signature = module.make_signature();
        FuncOrDataId::Func(
            module
                .declare_function(symbol, Linkage::Import, &signature)
                .map_err(object_error)?,
        )
    } else {
        FuncOrDataId::Data(
            module
                .declare_data(symbol, Linkage::Import, false, false)
                .map_err(object_error)?,
        )
    };
    let word = module
        .declare_data(&mangle::import_word(symbol), Linkage::Local, false, false)
        .map_err(object_error)?;
    let mut desc = DataDescription::new();
    desc.define(vec![0_u8; 8].into_boxed_slice());
    desc.set_align(8);
    let target = desc.import_global_value(ModuleRelocTarget::from(target));
    desc.write_data_addr(0, target, 0);
    module.define_data(word, &desc).map_err(object_error)?;
    Ok(word)
}

/// Defines `int main(int argc, char **argv)`, exported: it loads the
/// runtime ID's import word (so the import stays referenced), then calls
/// `cheby_rt_main(argc, argv, &cb$image)` through its import word and
/// returns its status (D-925).
fn define_main(
    module: &mut ObjectModule,
    image: DataId,
    rt_main: DataId,
    runtime_id: DataId,
) -> Result<(), LinkError> {
    let pointer = module.target_config().pointer_type();
    let mut signature = Signature::new(module.isa().default_call_conv());
    signature.params.push(AbiParam::new(types::I32));
    signature.params.push(AbiParam::new(pointer));
    signature.returns.push(AbiParam::new(types::I32));
    let mut rt_signature = signature.clone();
    rt_signature.params.push(AbiParam::new(pointer));
    let main = module
        .declare_function("main", Linkage::Export, &signature)
        .map_err(object_error)?;
    let mut ctx = module.make_context();
    ctx.func.signature = signature;
    let mut fctx = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let block = b.create_block();
        b.append_block_params_for_function_params(block);
        b.switch_to_block(block);
        b.seal_block(block);
        let &[count, args] = b.block_params(block) else {
            return Err(LinkError::Object("`main` has two parameters".into()));
        };
        let image = module.declare_data_in_func(image, b.func);
        let rt_main = module.declare_data_in_func(rt_main, b.func);
        let runtime_id = module.declare_data_in_func(runtime_id, b.func);
        // A load that may trap is never removed, so the word stays
        // referenced from `main`.
        let id_word = b.ins().symbol_value(pointer, runtime_id);
        b.ins().load(pointer, MemFlagsData::new(), id_word, 0);
        let rt_word = b.ins().symbol_value(pointer, rt_main);
        let callee = b
            .ins()
            .load(pointer, MemFlagsData::trusted().with_readonly(), rt_word, 0);
        let image = b.ins().symbol_value(pointer, image);
        let rt_signature = b.import_signature(rt_signature);
        let call = b
            .ins()
            .call_indirect(rt_signature, callee, &[count, args, image]);
        let status = b
            .inst_results(call)
            .first()
            .copied()
            .ok_or_else(|| LinkError::Object("`cheby_rt_main` returns a status".into()))?;
        b.ins().return_(&[status]);
        b.finalize(module.target_config());
    }
    module
        .define_function(main, &mut ctx)
        .map_err(object_error)?;
    Ok(())
}
