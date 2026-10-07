//! Data objects (D-454): static strings and indirection slots.

use cranelift_module::{DataDescription, Module};

use crate::{CodegenError, Decls};

/// The alignment of every object, so a pointer's bit 0 is clear (D-278).
const OBJECT_ALIGN: u64 = 8;

/// Defines the strings and slots declared since the last call.
///
/// - A string is a read-only static `String` object (D-454): count 0,
///   so count operations never write it, `cap == len`, little-endian.
/// - A slot is a writable 8-byte word holding its function's address
///   (D-043); the REPL repoints it later.
pub(crate) fn define_pending<M: Module>(
    module: &mut M,
    decls: &mut Decls,
) -> Result<(), CodegenError> {
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
    Ok(())
}
