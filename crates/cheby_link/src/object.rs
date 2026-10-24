//! Replay of a code record into a relocatable object (D-915).
//!
//! [`replay`] declares every symbol of the record, in record order, with
//! its recorded name and linkage in a `cranelift_object::ObjectModule`,
//! defines each function with `define_function_bytes` and each data
//! object with `define_data`, and emits the object. Linkage maps as
//! `cranelift-object` maps it (D-917): `Local` is a local symbol, `Hidden`
//! a global hidden one (ELF `STV_HIDDEN`, Mach-O private extern), `Import`
//! an undefined one and `Export` a global default one, which only N4's
//! program object has (`main`).
//!
//! D-1108: every symbol keeps its recorded name in
//! the object's symbol table, local ones included; only the JIT declares a
//! record's local symbols anonymously (D-914), so copies in two records
//! never clash there. In objects local names cannot clash, and they keep
//! code readable in a debugger and in `nm`.
//!
//! Replay never compiles and never panics on a record: the record is
//! checked first ([`CodeRecord::check`]) and against the target, since
//! `cranelift-object` panics on a relocation kind the format or the
//! architecture does not have, and on a definition it cannot place.
//!
//! A record with positions (D-938) also gets its DWARF line tables,
//! `.debug_line` and a minimal `.debug_info` with their sections
//! (`crate::dwarf`); a record without them, as with
//! `CHEBY_POSITIONS=off`, and the program object get none.

use cranelift_codegen::binemit::Reloc;
use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_module::{DataDescription, FuncOrDataId, Module, ModuleReloc, ModuleRelocTarget};
use cranelift_object::{ObjectBuilder, ObjectModule};

use cheby_codegen_clif::isa;
use cheby_codegen_clif::record::{CodeRecord, RecordContents, RecordError, SymbolKind};

use crate::LinkError;

/// Replays `record` into one relocatable object for `isa`'s format (ELF
/// on Linux targets, Mach-O on macOS), every symbol with its recorded name
/// and linkage (D-915). `name` is the module path, never a file path: it
/// becomes the object's file symbol, so it must not depend on where the
/// build runs (D-264).
///
/// The bytes depend only on the record, `isa` and `name`: two replays of
/// one record are identical. A record with positions gets its DWARF line
/// tables (D-938, `crate::dwarf`).
///
/// # Errors
///
/// [`LinkError::Record`] if the record is malformed or holds a relocation
/// `isa`'s architecture does not have, and [`LinkError::Object`] if the
/// target's format is neither ELF nor Mach-O or `cranelift-object`
/// refuses a declaration, a definition or the emission.
pub fn replay(record: &CodeRecord, isa: OwnedTargetIsa, name: &str) -> Result<Vec<u8>, LinkError> {
    check_record(record, &isa)?;
    let mut module = new_module(isa, name)?;
    replay_into(&mut module, record)?;
    // D-938: a record with positions gets its line tables.
    if record.source.is_none() {
        return emit(module);
    }
    let mut funcs = Vec::with_capacity(record.functions.len());
    for function in &record.functions {
        let symbol = record
            .symbol_at(function.symbol)
            .ok_or(RecordError::Malformed("a symbol index out of range"))?;
        match module.get_name(&symbol.name) {
            Some(FuncOrDataId::Func(id)) => funcs.push(id),
            _ => return Err(RecordError::Malformed("a definition of the wrong kind").into()),
        }
    }
    let mut product = module.finish();
    crate::dwarf::write(&mut product, record, &funcs)?;
    product.emit().map_err(|e| LinkError::Object(e.to_string()))
}

/// Checks `record` and that `isa` is a target replay writes, with the
/// record's text relocations among its PC-relative kinds (D-913).
pub(crate) fn check_record(record: &CodeRecord, isa: &OwnedTargetIsa) -> Result<(), LinkError> {
    record.check()?;
    check_target(record, isa)
}

/// An empty object module for `isa` whose file symbol is `name`, without
/// unwind info (D-913).
pub(crate) fn new_module(isa: OwnedTargetIsa, name: &str) -> Result<ObjectModule, LinkError> {
    let mut builder = ObjectBuilder::new(isa, name, isa::libcall_names()).map_err(object_error)?;
    // D-913: no unwind info; Cheby frames unwind by return flags (D-733).
    builder.unwind_info(false);
    Ok(ObjectModule::new(builder))
}

/// Writes the object's bytes.
pub(crate) fn emit(module: ObjectModule) -> Result<Vec<u8>, LinkError> {
    module
        .finish()
        .emit()
        .map_err(|e| LinkError::Object(e.to_string()))
}

/// Declares and defines every symbol of `record`, checked with
/// [`check_record`], in `module`, in record order.
pub(crate) fn replay_into(module: &mut ObjectModule, record: &CodeRecord) -> Result<(), LinkError> {
    // A declaration's signature reaches no object; one with the target's
    // default convention serves every function.
    let signature = module.make_signature();
    let mut ids = Vec::new();
    for symbol in &record.symbols {
        let linkage = symbol.linkage.linkage();
        let id = match symbol.kind {
            SymbolKind::Function => FuncOrDataId::Func(
                module
                    .declare_function(&symbol.name, linkage, &signature)
                    .map_err(object_error)?,
            ),
            SymbolKind::Data { writable } => FuncOrDataId::Data(
                module
                    .declare_data(&symbol.name, linkage, writable, false)
                    .map_err(object_error)?,
            ),
        };
        ids.push(id);
    }
    let id_of = |symbol: u32| -> Result<FuncOrDataId, LinkError> {
        usize::try_from(symbol)
            .ok()
            .and_then(|index| ids.get(index))
            .copied()
            .ok_or(LinkError::Record(RecordError::Malformed(
                "a symbol index out of range",
            )))
    };
    for function in &record.functions {
        let FuncOrDataId::Func(id) = id_of(function.symbol)? else {
            return Err(RecordError::Malformed("a definition of the wrong kind").into());
        };
        let mut relocs = Vec::with_capacity(function.relocs.len());
        for reloc in &function.relocs {
            relocs.push(ModuleReloc {
                offset: reloc.offset,
                kind: reloc.kind,
                name: ModuleRelocTarget::from(id_of(reloc.symbol)?),
                addend: reloc.addend,
            });
        }
        module
            .define_function_bytes(id, function.align, &function.bytes, &relocs)
            .map_err(object_error)?;
    }
    for object in &record.data {
        let FuncOrDataId::Data(id) = id_of(object.symbol)? else {
            return Err(RecordError::Malformed("a definition of the wrong kind").into());
        };
        let mut desc = DataDescription::new();
        match &object.contents {
            RecordContents::Bytes(bytes) => desc.define(bytes.clone().into_boxed_slice()),
            RecordContents::Zeros(size) => desc.define_zeroinit(
                usize::try_from(*size)
                    .map_err(|_| RecordError::Malformed("a data object too large"))?,
            ),
        }
        if let Some(align) = object.align {
            // A power of two: `CodeRecord::check` holds.
            desc.set_align(align);
        }
        // Every relocation as a data relocation, which carries an addend
        // and may name a function, in the record's order, which is
        // `DataDescription::all_relocs`'s.
        for reloc in &object.relocs {
            let target = desc.import_global_value(ModuleRelocTarget::from(id_of(reloc.symbol)?));
            desc.write_data_addr(reloc.offset, target, reloc.addend);
        }
        module.define_data(id, &desc).map_err(object_error)?;
    }
    Ok(())
}

/// The relocation kinds `isa`'s architecture has in a record's text: the
/// PC-relative kinds D-913 leaves there (data relocations are 64-bit
/// absolute words, by construction). `None` for a target replay does not
/// write: an object format other than ELF or Mach-O (COFF is M6's, D-935),
/// or an architecture other than x86-64 and aarch64.
fn text_relocs(isa: &OwnedTargetIsa) -> Option<&'static [Reloc]> {
    // `target-lexicon` is not a dependency: its names print as the
    // triple spells them.
    let triple = isa.triple();
    let format = triple.binary_format.to_string();
    if !matches!(format.as_str(), "elf" | "macho") || isa.pointer_bytes() != 8 {
        return None;
    }
    match triple.architecture.to_string().as_str() {
        "x86_64" => Some(&[Reloc::X86PCRel4, Reloc::X86CallPCRel4]),
        "aarch64" => Some(&[
            Reloc::Arm64Call,
            Reloc::Aarch64AdrPrelPgHi21,
            Reloc::Aarch64AddAbsLo12Nc,
        ]),
        _ => None,
    }
}

/// Fails before anything is written if `record`'s text holds a relocation
/// that is not one of `isa`'s PC-relative kinds (D-913: text has nothing
/// else, which PIE executables and arm64 Mach-O need), or `isa` is not a
/// target replay writes.
fn check_target(record: &CodeRecord, isa: &OwnedTargetIsa) -> Result<(), LinkError> {
    let Some(kinds) = text_relocs(isa) else {
        return Err(LinkError::Object(format!(
            "objects for `{}` are not written in M3 (D-935, D-936)",
            isa.triple()
        )));
    };
    for function in &record.functions {
        for reloc in &function.relocs {
            if !kinds.contains(&reloc.kind) {
                return Err(RecordError::Malformed(
                    "a text relocation that is absolute or of another architecture",
                )
                .into());
            }
        }
    }
    Ok(())
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "passed to `map_err`, which hands over the error"
)]
pub(crate) fn object_error(error: cranelift_module::ModuleError) -> LinkError {
    LinkError::Object(error.to_string())
}
