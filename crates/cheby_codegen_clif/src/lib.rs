//! MIR to Cranelift IR, shared by the JIT, AOT and the constant evaluator
//! (E1-E5).
//!
//! Code generation runs in two phases over a batch of MIR units, so units
//! can reference each other's functions:
//!
//! 1. [`declare`]: every function, indirection slot (D-043) and string of
//!    every unit gets a symbol in the Cranelift module, in unit order and
//!    then function order. Declaration order fixes `FuncId`s, which appear
//!    in CLIF text, so it is deterministic (D-264). [`Decls`] persists
//!    across batches: the REPL, constant evaluation (E6) and M3's lazy
//!    compilation add code that must find the existing slots.
//! 2. [`define`]: each function is translated ([`translate`], which needs
//!    no `&mut Module`, so it is the parallel unit, D-263) and compiled
//!    ([`compile_function`]); only when every function of the batch has
//!    compiled are the data objects defined and the functions installed
//!    with `define_function_bytes` in declaration order, with an explicit
//!    alignment of 16 (D-453). A batch that fails therefore defines
//!    nothing, and a caller that restores its [`Decls`] can retry it.
//!
//! The ABI comes from two single sources: [`cheby_mir::abi`] for every
//! signature, and `cheby_runtime::abi` for object headers, string images,
//! context offsets and symbol names. This crate never calls the runtime.
//!
//! Lowering rules (D-453): every Cheby-ABI function uses Cranelift's
//! `tail` convention with the hidden `ctx` parameter first, and a tail call
//! is `return_call` (D-015). Every call of a top-level function loads the
//! callee's slot (D-043, [`CallMode::Indirect`]). Entry thunks use the
//! platform C convention and are the only way the host enters generated
//! code. Every Cheby-ABI function starts with the stack-limit check of
//! D-124, whose slow path is `cheby_rt_stack_slow(ctx, location)`.

mod data;
mod decls;
mod error;
mod func;
pub mod isa;
mod layout;
pub mod mangle;
pub mod runtime;
pub mod sig;

#[cfg(test)]
mod tests;

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir;
use cranelift_codegen::isa::TargetIsa;
use cranelift_module::{Module, ModuleReloc};

use cheby_mir::MirModule;

pub use decls::{Decls, FuncDecl, SlotKey};
pub use error::CodegenError;
pub use func::translate;
pub use layout::{BuiltinLayouts, LayoutIds, builtin_layout_id};

/// The alignment every function is defined with. Bit 0 of a code address
/// must be clear so a closure's code word `addr | 1` reads as a tagged
/// integer (D-453), and Cranelift's minimum function alignment on
/// x86-64 is 1.
pub const FUNCTION_ALIGN: u64 = 16;

/// How a call of a top-level function reaches its callee.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum CallMode {
    /// Load the callee's slot and call the address in it, so the REPL and
    /// hot reloading can repoint the slot (D-043). The JIT's mode.
    #[default]
    Indirect,
    /// Call the callee's symbol directly (AOT, M3).
    Direct,
}

/// How generated code imports and calls an `@external(native, …)` symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum ForeignImport {
    /// Import the symbol's address, with no signature, and call it with
    /// `call_indirect` and the C signature of each call site. One module
    /// can then hold several C signatures for one symbol, which the REPL
    /// needs: a redefinition may change a binding's C signature, and a
    /// failed batch leaves its declarations behind. The JIT's mode.
    #[default]
    Address,
    /// Import the symbol as a function with its C signature. A module
    /// holds one signature per symbol, which D-455 guarantees within a
    /// program (AOT, M3).
    Function,
}

/// What code generation emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CodegenOptions {
    pub call_mode: CallMode,
    pub foreign_import: ForeignImport,
    /// Emit the function-entry stack check (D-124).
    pub stack_check: bool,
    /// Keep the CLIF text of every translated function, for dumps and
    /// tests.
    pub capture_clif: bool,
}

impl Default for CodegenOptions {
    fn default() -> Self {
        Self {
            call_mode: CallMode::Indirect,
            foreign_import: ForeignImport::Address,
            stack_check: true,
            capture_clif: false,
        }
    }
}

/// What code generation reads besides the MIR. MIR units carry the paths
/// and signatures of every def they mention, so only layout ids come from
/// outside.
#[derive(Clone, Copy)]
pub struct CodegenCx<'a> {
    /// Runtime layout ids. Every layout a unit uses is registered before
    /// its functions are translated (D-454).
    pub layouts: &'a dyn LayoutIds,
}

impl std::fmt::Debug for CodegenCx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodegenCx").finish_non_exhaustive()
    }
}

/// Declares every function, slot, string and import of `mirs` in `module`
/// and records them in `decls`. Call [`define`] with the same units next.
///
/// # Errors
///
/// A [`CodegenError`] if a symbol is declared twice with different
/// signatures (for foreign symbols, only under
/// [`ForeignImport::Function`]), a unit is inconsistent, or a function key
/// was already declared by an earlier batch.
pub fn declare<M: Module>(
    module: &mut M,
    decls: &mut Decls,
    mirs: &[MirModule],
    _cx: &CodegenCx<'_>,
    opts: &CodegenOptions,
) -> Result<(), CodegenError> {
    decls.declare_units(module, mirs, opts.foreign_import)
}

/// Defines the data objects declared by [`declare`] and every function of
/// `mirs`, in declaration order. Returns the CLIF text of each function
/// when [`CodegenOptions::capture_clif`] is set.
///
/// Every function is translated and compiled before anything is defined,
/// so a translation or compilation error leaves `module` without any
/// definition from this batch.
///
/// # Errors
///
/// A [`CodegenError`] if translation or compilation fails.
pub fn define<M: Module>(
    module: &mut M,
    mirs: &[MirModule],
    decls: &mut Decls,
    cx: &CodegenCx<'_>,
    opts: &CodegenOptions,
) -> Result<Vec<String>, CodegenError> {
    let mut clif = Vec::new();
    let mut compiled = Vec::new();
    for unit in mirs {
        for function in &unit.functions {
            let id = decls.func(function.key)?.id;
            let translated = translate(unit, function, decls, cx, opts)?;
            if opts.capture_clif {
                clif.push(decls.describe(&translated));
            }
            compiled.push((
                function.key,
                id,
                compile_function(module.isa(), translated)?,
            ));
        }
    }
    data::define_pending(module, decls)?;
    for (key, id, code) in compiled {
        module
            .define_function_bytes(id, code.align, &code.bytes, &code.relocs)
            .map_err(|e| CodegenError::module(&e))?;
        decls.mark_defined(key)?;
    }
    Ok(clif)
}

/// The machine code of one function, ready for `define_function_bytes`.
pub struct CompiledFunction {
    pub bytes: Vec<u8>,
    pub align: u64,
    pub relocs: Vec<ModuleReloc>,
}

impl std::fmt::Debug for CompiledFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledFunction")
            .field("len", &self.bytes.len())
            .field("align", &self.align)
            .field("relocs", &self.relocs.len())
            .finish()
    }
}

/// Compiles one translated function for `isa`. Needs no `&mut Module`, so
/// functions can be compiled in parallel and installed in declaration
/// order afterwards (D-263, D-264).
///
/// # Errors
///
/// A [`CodegenError`] with the verifier's or compiler's message.
pub fn compile_function(
    isa: &dyn TargetIsa,
    func: ir::Function,
) -> Result<CompiledFunction, CodegenError> {
    let ir::UserFuncName::User(name) = func.name.clone() else {
        return Err(CodegenError::ice("a translated function has no user name"));
    };
    let func_id = cranelift_module::FuncId::from_u32(name.index);
    let mut ctx = cranelift_codegen::Context::for_function(func);
    if let Err(e) = ctx.compile(isa, &mut ControlPlane::default()) {
        let message = cranelift_codegen::print_errors::pretty_error(e.func, e.inner);
        return Err(CodegenError::Compile(message));
    }
    let compiled = ctx
        .compiled_code()
        .ok_or_else(|| CodegenError::ice("Cranelift returned no code"))?;
    let align = u64::from(compiled.buffer.alignment).max(FUNCTION_ALIGN);
    let bytes = compiled.code_buffer().to_vec();
    let relocs = compiled
        .buffer
        .relocs()
        .iter()
        .map(|r| ModuleReloc::from_mach_reloc(r, &ctx.func, func_id))
        .collect();
    Ok(CompiledFunction {
        bytes,
        align,
        relocs,
    })
}
