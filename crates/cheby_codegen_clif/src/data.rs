//! Data objects (D-454, D-523): static strings, indirection slots, layout
//! words, constructor statics, constant slots and static objects.

use cranelift_module::{DataDescription, Module, ModuleRelocTarget};

use cheby_mir::LayoutRef;
use cheby_runtime::abi;

use crate::decls::{ImportRef, StaticWord, ctor_header, header};
use crate::{CodegenCx, CodegenError, Decls};

/// The alignment of every object, so a pointer's bit 0 is clear (D-278).
const OBJECT_ALIGN: u64 = 8;

/// The size of a word, as a data offset.
const WORD_BYTES: u32 = 8;

/// Defines the data objects declared since the last call.
///
/// - An import word `cb$imp$<symbol>` (D-913) is a read-only 8-byte word
///   with one `Abs8` data relocation to the runtime or foreign symbol;
///   code loads it and calls or uses the address.
/// - A string is a read-only static `String` object (D-454): count 0,
///   so count operations never write it, `cap == len`, little-endian.
/// - A slot is a writable 8-byte word holding its function's address
///   (D-043); the REPL repoints it later.
/// - A layout word `cb$layout$…` holds the header allocation copies:
///   count 1, the scan count (escaped above 254 value fields, D-279), and
///   the runtime layout id registered for the constructor (D-454). A nullary constructor's static `cb$ctor$…` is a
///   header with count 0. Only these objects hold user layout ids; code
///   loads them.
/// - A constant slot `cb$const$…` is a writable word, 0 until the
///   constant is evaluated (E6).
/// - A static object (D-523) is a header with count 0, then its fields:
///   addresses of other data objects, or code words `address | 1`, and
///   the tagged field count of an escaped object (`escaped_count_index`).
pub(crate) fn define_pending<M: Module>(
    module: &mut M,
    decls: &mut Decls,
    cx: &CodegenCx<'_>,
) -> Result<(), CodegenError> {
    check_pending(decls)?;
    for (id, target) in std::mem::take(&mut decls.pending_imports) {
        let mut desc = DataDescription::new();
        desc.define_zeroinit(8);
        desc.set_align(OBJECT_ALIGN);
        match target {
            ImportRef::Func(func) => {
                let func_ref = module.declare_func_in_data(func, &mut desc);
                desc.write_function_addr(0, func_ref);
            }
            ImportRef::Data(data) => {
                let gv = module.declare_data_in_data(data, &mut desc);
                desc.write_data_addr(0, gv, 0);
            }
        }
        module
            .define_data(id, &desc)
            .map_err(|e| CodegenError::module(&e))?;
    }
    for (id, image) in std::mem::take(&mut decls.pending_strings) {
        let mut desc = DataDescription::new();
        desc.define(image.into_boxed_slice());
        desc.set_align(OBJECT_ALIGN);
        module
            .define_data(id, &desc)
            .map_err(|e| CodegenError::module(&e))?;
    }
    for (id, func) in std::mem::take(&mut decls.pending_slots) {
        let mut desc = DataDescription::new();
        desc.define_zeroinit(8);
        desc.set_align(OBJECT_ALIGN);
        let func_ref = module.declare_func_in_data(func, &mut desc);
        desc.write_function_addr(0, func_ref);
        module
            .define_data(id, &desc)
            .map_err(|e| CodegenError::module(&e))?;
    }
    for (layout, decl, value_fields) in std::mem::take(&mut decls.pending_layouts) {
        let id = cx.layouts.id(&layout)?;
        let word = ctor_header(abi::RC_UNIQUE, u64::from(value_fields), id);
        define_words(module, decl.word, &[word])?;
        if let Some(ctor) = decl.ctor {
            define_words(module, ctor, &[header(abi::RC_STATIC, 0, id)?])?;
        }
    }
    for id in std::mem::take(&mut decls.pending_consts) {
        define_words(module, id, &[0])?;
    }
    for object in std::mem::take(&mut decls.pending_statics) {
        let count = object.fields.len().saturating_add(1);
        let mut desc = DataDescription::new();
        let mut bytes = vec![0_u8; count.saturating_mul(8)];
        let words =
            std::iter::once(object.header).chain(object.fields.iter().map(|field| match *field {
                StaticWord::Word(word) => word,
                StaticWord::Data(_) | StaticWord::Code(_) => 0,
            }));
        for (slot, word) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(words) {
            slot.copy_from_slice(&word.to_le_bytes());
        }
        desc.define(bytes.into_boxed_slice());
        desc.set_align(OBJECT_ALIGN);
        let mut offset = WORD_BYTES;
        for field in &object.fields {
            match *field {
                // Written into the image above.
                StaticWord::Word(_) => {}
                StaticWord::Data(data) => {
                    let gv = module.declare_data_in_data(data, &mut desc);
                    desc.write_data_addr(offset, gv, 0);
                }
                StaticWord::Code(func) => {
                    // The code word `address | 1` (D-453). Data relocations
                    // carry an addend, function relocations do not, so the
                    // function is named as a data relocation target.
                    let gv = desc.import_global_value(ModuleRelocTarget::from(func));
                    desc.write_data_addr(offset, gv, 1);
                }
            }
            offset = offset
                .checked_add(WORD_BYTES)
                .ok_or_else(|| CodegenError::ice("a static object is too large"))?;
        }
        module
            .define_data(object.id, &desc)
            .map_err(|e| CodegenError::module(&e))?;
    }
    Ok(())
}

/// Fails, before anything is defined, if a pending data object was defined
/// already or is pending twice: a module keeps what a failed batch defined,
/// and the next finalize would resolve its relocations against symbols that
/// were never defined. Records the pending objects as defined.
fn check_pending(decls: &mut Decls) -> Result<(), CodegenError> {
    let ids = decls
        .pending_imports
        .iter()
        .map(|&(id, _)| id)
        .chain(decls.pending_strings.iter().map(|&(id, _)| id))
        .chain(decls.pending_slots.iter().map(|&(id, _)| id))
        .chain(
            decls
                .pending_layouts
                .iter()
                .flat_map(|(_, decl, _)| std::iter::once(decl.word).chain(decl.ctor)),
        )
        .chain(decls.pending_consts.iter().copied())
        .chain(decls.pending_statics.iter().map(|object| object.id));
    let mut seen = rustc_hash::FxHashSet::default();
    for id in ids {
        if decls.defined_data.contains(&id) || !seen.insert(id) {
            return Err(CodegenError::ice(format!(
                "the data object `{}` is defined twice",
                decls.data_name(id)
            )));
        }
    }
    decls.defined_data.extend(seen);
    Ok(())
}

fn define_words<M: Module>(
    module: &mut M,
    id: cranelift_module::DataId,
    words: &[u64],
) -> Result<(), CodegenError> {
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut desc = DataDescription::new();
    desc.define(bytes.into_boxed_slice());
    desc.set_align(OBJECT_ALIGN);
    module
        .define_data(id, &desc)
        .map_err(|e| CodegenError::module(&e))
}

/// Whether `layout` is a user constructor, whose header comes from its
/// `cb$layout$` word rather than from a fixed id.
pub(crate) fn is_user_ctor(layout: &LayoutRef) -> bool {
    matches!(layout, LayoutRef::Ctor(_)) && crate::layout::builtin_layout_id(layout).is_none()
}
