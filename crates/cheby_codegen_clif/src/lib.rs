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
//! D-124, whose slow path is `cheby_rt_stack_slow(ctx, location)`, or
//! `cheby_rt_stack_slow_unwind`, which also releases up to four counted
//! parameters when it starts an unwinding (D-775). The
//! check runs after the prologue has allocated the frame, so a function
//! whose frame exceeds [`CodegenOptions::large_frame_threshold`] (64 KiB)
//! is compiled as two (D-679): an outer function with its symbol and a
//! small frame, whose check accounts for the whole frame with the slow
//! path `cheby_rt_stack_slow_frame(ctx, location, frame_size)`, and an
//! unchecked inner body `cb$big$…` that the outer one `return_call`s.
//! [`define`] learns the frame size from the first compilation.
//!
//! Unwinding is return-flag propagation (D-733, ADR-0061): every unwind
//! edge of the MIR (D-698) is lowered to a test of the call's result or of
//! the flag at `ctx+12`, with a branch to the edge's pad, a cold block
//! (`unwind`).
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
mod unwind;
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent switches, each set by its own caller"
)]
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
    /// Whether the MIR was compiled with unwind edges, that is without
    /// `CHEBY_UNWIND_EDGES=off` (D-742); `true` by default. Code generation
    /// does not read it: it lowers whatever edges the MIR has (D-733). The
    /// JIT copies it into the runtime's entry and run configurations, so a
    /// raise in a fiber compiled without edges ends the process with its
    /// report instead of returning into code that tests no flag
    /// (D-770).
    pub unwind_edges: bool,
    /// The frame size in bytes above which a function is split into an
    /// outer function, which checks the stack for its whole frame, and an
    /// unchecked inner body `cb$big$…` (D-679). Only consulted when
    /// [`CodegenOptions::stack_check`] is set. Tests lower it to split
    /// small functions.
    pub large_frame_threshold: u32,
}

/// The default [`CodegenOptions::large_frame_threshold`]: 64 KiB (D-679).
/// The ordinary entry check runs after the prologue, so a frame up to this
/// size must fit in the stack headroom together with the slow path.
pub const LARGE_FRAME_THRESHOLD: u32 = 64 << 10;

impl Default for CodegenOptions {
    fn default() -> Self {
        Self {
            call_mode: CallMode::Indirect,
            foreign_import: ForeignImport::Address,
            stack_check: true,
            capture_clif: false,
            checked_rc: false,
            unwind_edges: true,
            large_frame_threshold: LARGE_FRAME_THRESHOLD,
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

/// What one call of [`define_with_stats`] defined: the size of the machine
/// code it installed, for `cargo xtask bench-compile` (D-718).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DefineStats {
    /// The bytes of machine code of the functions defined, without
    /// alignment padding or data objects.
    pub code_bytes: u64,
    /// How many functions were defined.
    pub functions: u64,
}

impl DefineStats {
    /// The sum of `self` and `other`, saturating.
    #[must_use]
    pub const fn add(self, other: Self) -> Self {
        Self {
            code_bytes: self.code_bytes.saturating_add(other.code_bytes),
            functions: self.functions.saturating_add(other.functions),
        }
    }
}

/// Defines the data objects declared by [`declare`] and every function of
/// `mirs`, in declaration order. Returns the CLIF text of each function
/// when [`CodegenOptions::capture_clif`] is set.
///
/// [`define_with_stats`] also returns the size of the code defined.
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
    define_with_stats(module, mirs, decls, cx, opts).map(|(clif, _)| clif)
}

/// [`define`], returning also what it defined: the bytes of machine code
/// and the number of functions (D-718).
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
/// A function whose frame exceeds [`CodegenOptions::large_frame_threshold`]
/// is then compiled again as two (D-679): its outer function, defined with
/// its id, and its inner body `cb$big$…`, declared here in declaration
/// order and defined right after it. Both count in [`DefineStats`], and
/// with [`CodegenOptions::capture_clif`] both texts replace the first one.
///
/// # Errors
///
/// A [`CodegenError`] if translation or compilation fails.
pub fn define_with_stats<M: Module>(
    module: &mut M,
    mirs: &[MirModule],
    decls: &mut Decls,
    cx: &CodegenCx<'_>,
    opts: &CodegenOptions,
) -> Result<(Vec<String>, DefineStats), CodegenError> {
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
    let shared: &Decls = decls;
    let isa = module.isa();
    // `collect` keeps the order of `todo` whatever the thread count.
    let results: Vec<Result<(Option<String>, CompiledFunction), CodegenError>> = todo
        .par_iter()
        .map(|&(unit, function, _)| {
            let translated = translate(unit, function, shared, cx, opts)?;
            let text = opts.capture_clif.then(|| shared.describe(&translated));
            Ok((text, compile_function(isa, translated)?))
        })
        .collect();
    let mut first = Vec::with_capacity(results.len());
    for result in results {
        first.push(result?);
    }
    // D-679: a function whose frame is large becomes an outer function
    // with its symbol and an unchecked inner body. The bodies are declared
    // here, after every function compiled, in declaration order, so their
    // ids are the same in every build (D-264); then the large functions
    // are translated and compiled again, in parallel.
    let mut large = Vec::new();
    for (index, (&(_, function, _), (_, code))) in todo.iter().zip(&first).enumerate() {
        if is_large_frame(function, code, opts) {
            large.push((index, decls.declare_large_frame_body(module, function)?));
        }
    }
    let shared: &Decls = decls;
    let isa = module.isa();
    let split: Vec<Result<LargeFrame, CodegenError>> = large
        .par_iter()
        .map(|&(index, inner)| {
            let &(unit, function, _) = todo
                .get(index)
                .ok_or_else(|| CodegenError::ice("a large frame out of the batch"))?;
            compile_large_frame(isa, unit, function, shared, opts, inner)
        })
        .collect();
    let mut split_by_index = rustc_hash::FxHashMap::default();
    for (&(index, inner), result) in large.iter().zip(split) {
        split_by_index.insert(index, (inner, result?));
    }
    let mut clif = Vec::new();
    // `(key, id, code)`; the key is `None` for an inner body.
    let mut compiled = Vec::with_capacity(todo.len().saturating_add(large.len()));
    for (index, (&(_, function, id), (text, code))) in todo.iter().zip(first).enumerate() {
        if let Some((inner, split)) = split_by_index.remove(&index) {
            clif.extend(split.outer_text);
            clif.extend(split.inner_text);
            compiled.push((Some(function.key), id, split.outer));
            compiled.push((None, inner, split.inner));
        } else {
            clif.extend(text);
            compiled.push((Some(function.key), id, code));
        }
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
    let mut stats = DefineStats::default();
    for (key, id, code) in compiled {
        module
            .define_function_bytes(id, code.align, &code.bytes, &code.relocs)
            .map_err(|e| CodegenError::module(&e))?;
        if let Some(key) = key {
            decls.mark_defined(key)?;
        }
        stats = stats.add(DefineStats {
            code_bytes: u64::try_from(code.bytes.len()).unwrap_or(u64::MAX),
            functions: 1,
        });
    }
    Ok((clif, stats))
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

/// Whether `function`, compiled to `code`, has a large frame and is split
/// into an outer function and an inner body (D-679): a Cheby function with
/// the entry check whose frame exceeds
/// [`CodegenOptions::large_frame_threshold`].
fn is_large_frame(
    function: &cheby_mir::Function,
    code: &CompiledFunction,
    opts: &CodegenOptions,
) -> bool {
    function.abi == cheby_mir::Abi::Cheby
        && opts.stack_check
        && code.frame_size > opts.large_frame_threshold
}

/// A function with a large frame, compiled as two (D-679).
struct LargeFrame {
    outer: CompiledFunction,
    inner: CompiledFunction,
    outer_text: Option<String>,
    inner_text: Option<String>,
}

/// Translates and compiles `function` as its inner body `inner`, then its
/// outer function with the inner body's frame size.
fn compile_large_frame(
    isa: &dyn TargetIsa,
    unit: &MirModule,
    function: &cheby_mir::Function,
    decls: &Decls,
    opts: &CodegenOptions,
    inner: cranelift_module::FuncId,
) -> Result<LargeFrame, CodegenError> {
    let body = func::translate_large_frame_body(unit, function, decls, opts, inner)?;
    let inner_text = opts.capture_clif.then(|| decls.describe(&body));
    let inner_code = compile_function(isa, body)?;
    let outer = func::translate_large_frame_outer(
        unit,
        function,
        decls,
        opts,
        inner,
        inner_code.frame_size,
    )?;
    let outer_text = opts.capture_clif.then(|| decls.describe(&outer));
    Ok(LargeFrame {
        outer: compile_function(isa, outer)?,
        inner: inner_code,
        outer_text,
        inner_text,
    })
}

/// The machine code of one function, ready for `define_function_bytes`.
pub struct CompiledFunction {
    pub bytes: Vec<u8>,
    pub align: u64,
    pub relocs: Vec<ModuleReloc>,
    /// The bytes the prologue allocates below the frame pointer: spill
    /// slots, stack slots, saved registers and outgoing arguments
    /// (Cranelift's `frame_to_fp_offset`). It decides whether the function
    /// has a large frame (D-679).
    pub frame_size: u32,
}

impl std::fmt::Debug for CompiledFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledFunction")
            .field("len", &self.bytes.len())
            .field("align", &self.align)
            .field("relocs", &self.relocs.len())
            .field("frame_size", &self.frame_size)
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
    let frame_size = compiled
        .buffer
        .frame_layout()
        .ok_or_else(|| CodegenError::ice("Cranelift returned no frame layout"))?
        .frame_to_fp_offset;
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
        frame_size,
    })
}
