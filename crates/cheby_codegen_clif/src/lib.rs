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
//!    ([`compile_function`]), in parallel on rayon's global pool and
//!    collected in declaration order (G1); only when every function of
//!    the batch has compiled are the data objects defined and the
//!    functions installed with `define_function_bytes` in declaration
//!    order, with an explicit alignment of 16 (D-453). A batch that fails
//!    therefore defines nothing, and a caller that restores its [`Decls`]
//!    can retry it.
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
//!
//! REPL session items (E4, D-547) are registered with
//! [`Decls::set_session_name`]: their symbols use the module `<repl>` and
//! carry `$r<generation>`, and a redefinition that takes over an earlier
//! definition repoints that definition's slot once its batch is finalized
//! ([`Decls::take_repoints`]); see `session.rs`.
//!
//! Data objects (D-454, D-523), all named from paths and names:
//!
//! - `cb$layout$<module>$<Ctor>`: a user constructor's header word (count
//!   1, scan count, layout id), filled from the id the JIT registered; code
//!   allocates with it and reads tags against the first constructor's
//!   (D-522), so no code embeds a user layout id. A nullary constructor
//!   also gets its static object `cb$ctor$<module>$<Ctor>` (count 0).
//! - `cb$const$<module>$<name>`: a constant's slot, 0 until evaluated;
//!   `LoadConst` calls `cheby_rt_const_missing` on 0 (E6).
//! - `cb$dict$…`, `cb$desc$…`, `cb$fnval$…`: static objects with count 0,
//!   with their own strings `cb$str$<static>$<n>`; a function value's code
//!   word is its shim's address `| 1`. Each symbol, like each shim, tuple
//!   function, `dyn` forwarder and trampoline, is defined once per JIT
//!   (D-561): a later unit or batch that names it uses the first definition.
//!
//! Scalars (D-521, D-527): `Box`/`Unbox` tag and untag inline and call
//! `cheby_rt_box_*` only outside the tagged range or for floats; checked
//! integer operations branch to cold blocks that call `cheby_rt_panic` with
//! the operation's location; float `<`, `<=`, `>`, `>=` follow `compare`'s
//! total order and `==` §3.3.2; float `%` calls `cheby_rt_fmod_*`. Count
//! operations are inline (tag test, static test, local count update) and
//! call the runtime only on the slow paths, unless
//! [`CodegenOptions::checked_rc`] asks for the checked runtime calls.

mod arith;
mod data;
mod decls;
mod error;
mod func;
pub mod isa;
mod layout;
pub mod mangle;
pub mod runtime;
mod session;
pub mod sig;
mod values;

#[cfg(test)]
mod scale_tests;
#[cfg(test)]
mod tests;

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir;
use cranelift_codegen::isa::TargetIsa;
use cranelift_module::{Module, ModuleReloc};

use cheby_mir::MirModule;

pub use decls::{Decls, FuncDecl, SlotDecl, SlotKey};
pub use error::CodegenError;
pub use func::translate;
pub use layout::{BuiltinLayouts, LayoutIds, builtin_ctor_layout, builtin_layout_id};
pub use session::{SESSION_MODULE, SessionName};

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
    /// Call the runtime's `cheby_rt_dup` and `cheby_rt_drop` for every
    /// count operation, which check the header in the runtime's poisoning
    /// mode (plan §4.3.4), instead of the inline fast paths that skip
    /// tagged integers and static objects and update the count in place.
    pub checked_rc: bool,
}

impl Default for CodegenOptions {
    fn default() -> Self {
        Self {
            call_mode: CallMode::Indirect,
            foreign_import: ForeignImport::Address,
            stack_check: true,
            capture_clif: false,
            checked_rc: false,
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
/// Functions are translated and compiled in parallel on rayon's global
/// pool (G1, D-263); each reads only the batch's MIR and `decls`. The
/// results are collected in declaration order, and the functions are
/// defined in that order, so the module, the CLIF text and the error
/// reported (the first failing function in declaration order) do not
/// depend on the number of threads (D-264).
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
    use rayon::prelude::*;

    // The functions to define, each once, in declaration order.
    let mut todo = Vec::new();
    let mut seen = rustc_hash::FxHashSet::default();
    for unit in mirs {
        for function in &unit.functions {
            let decl = decls.func(function.key)?;
            // A shim, tuple function, `dyn` forwarder or trampoline that an
            // earlier unit or batch defined is not defined again (D-523).
            if decl.defined || !seen.insert(function.key) {
                continue;
            }
            todo.push((unit, function, decl.id));
        }
    }
    let isa = module.isa();
    let shared: &Decls = decls;
    // `collect` keeps the order of `todo` whatever the thread count.
    let results: Vec<Result<(Option<String>, CompiledFunction), CodegenError>> = todo
        .par_iter()
        .map(|&(unit, function, _)| {
            let translated = translate(unit, function, shared, cx, opts)?;
            let text = opts.capture_clif.then(|| shared.describe(&translated));
            Ok((text, compile_function(isa, translated)?))
        })
        .collect();
    let mut clif = Vec::new();
    let mut compiled = Vec::with_capacity(results.len());
    for ((_, function, id), result) in todo.iter().zip(results) {
        let (text, code) = result?;
        clif.extend(text);
        compiled.push((function.key, *id, code));
    }
    // A function defined twice would fail after the data are defined,
    // which the module keeps: check first, so a failed batch defines
    // nothing.
    let mut ids = rustc_hash::FxHashSet::default();
    for &(_, id, _) in &compiled {
        if decls.defined_funcs.contains(&id) || !ids.insert(id) {
            return Err(CodegenError::ice(format!(
                "the function `{}` is defined twice",
                decls.func_name(id)
            )));
        }
    }
    data::define_pending(module, decls, cx)?;
    decls.defined_funcs.extend(ids);
    for (key, id, code) in compiled {
        module
            .define_function_bytes(id, code.align, &code.bytes, &code.relocs)
            .map_err(|e| CodegenError::module(&e))?;
        decls.mark_defined(key)?;
    }
    Ok(clif)
}

/// Defines the data objects declared since the last [`define`] outside any
/// batch: the slots of REPL session values
/// ([`Decls::declare_session_const`]).
///
/// # Errors
///
/// A [`CodegenError`] if a definition fails.
pub fn define_data<M: Module>(
    module: &mut M,
    decls: &mut Decls,
    cx: &CodegenCx<'_>,
) -> Result<(), CodegenError> {
    data::define_pending(module, decls, cx)
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
