//! Target flags (E1, D-259): the JIT and, from M3, AOT build their
//! ISA from these.

use cranelift_codegen::settings::{self, Configurable};

use crate::CodegenError;

/// The shared Cranelift flags: no optimization (D-259), frame pointers kept
/// for stack walking, no PIC and no colocated libcalls (the JIT resolves
/// symbols to absolute addresses), and the verifier in debug builds of the
/// compiler.
///
/// # Errors
///
/// A [`CodegenError`] if Cranelift does not know a flag.
pub fn flags() -> Result<settings::Flags, CodegenError> {
    let mut builder = settings::builder();
    let verifier = if cfg!(debug_assertions) {
        "true"
    } else {
        "false"
    };
    for (name, value) in [
        ("opt_level", "none"),
        ("preserve_frame_pointers", "true"),
        ("is_pic", "false"),
        ("use_colocated_libcalls", "false"),
        ("enable_verifier", verifier),
    ] {
        builder
            .set(name, value)
            .map_err(|e| CodegenError::ice(format!("Cranelift flag `{name}`: {e}")))?;
    }
    Ok(settings::Flags::new(builder))
}
