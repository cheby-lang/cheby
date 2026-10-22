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
//! The code model is D-913's, one for the JIT, the cache and executables
//! (ADR-0064): non-PIC code ([`isa::flags`]) for D-921's fixed ISA
//! ([`isa::host_isa`]). Every generated symbol is referenced colocated, so
//! text reaches it PC-relative. Runtime functions, runtime statics and
//! foreign symbols are reached only through import words
//! `cb$imp$<symbol>` (`Decls::import_word`'s read-only words with an
//! `Abs8` data relocation), which code loads and then calls with
//! `call_indirect` and the C signature, or uses as the static's address.
//! No Cranelift libcall appears ([`isa::libcall_names`]). [`define`] checks
//! every compiled function's relocations: text has only PC-relative ones,
//! to generated symbols.
//!
//! [`compile_unit`] compiles one unit alone, for code records and objects
//! (D-914, D-917, D-918, D-920): what another unit may name is global
//! hidden, what other units define is imported, code and statics fixed by
//! their key are local copies, and layout words hold no layout id. In the
//! JIT every symbol stays local. A slot whose signature borrows a
//! parameter carries the mask in its name ([`mangle::slot`]) in both.
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
//! An `@blocking` foreign call (D-683) runs on the blocking pool through
//! `cheby_rt_blocking_call` and a generated C-ABI shim `cb$blk$<symbol>`
//! per symbol and C signature, defined once per JIT with the batch that
//! first uses it (`blocking.rs`, D-818).
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

#[cfg(test)]
mod alone_tests;
mod arith;
mod blocking;
#[cfg(test)]
mod code_model_tests;
mod data;
mod decls;
mod error;
mod func;
pub mod isa;
mod layout;
#[cfg(test)]
mod locs_tests;
pub mod mangle;
mod modrec;
pub mod record;
pub mod runtime;
mod session;
pub mod sig;
mod unit;
mod unwind;
mod values;

#[cfg(test)]
mod record_tests;
#[cfg(test)]
mod scale_tests;
#[cfg(test)]
mod tests;

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir;
use cranelift_codegen::isa::TargetIsa;
use cranelift_module::{Module, ModuleReloc};

use cheby_mir::{MirModule, StrId};
use cheby_runtime::locate::CallSiteDesc;

pub use decls::{Decls, FuncDecl, SlotDecl, SlotKey};
pub use error::CodegenError;
pub use func::translate;
pub use layout::{BuiltinLayouts, LayoutIds, builtin_ctor_layout, builtin_layout_id};
pub use session::{SESSION_MODULE, SessionName};
pub use unit::{UnitCode, UnitCx, compile_unit};

/// The alignment every function is defined with. Bit 0 of a code address
/// must be clear so a closure's code word `addr | 1` reads as a tagged
/// integer (D-453), and Cranelift's minimum function alignment on
/// x86-64 is 1.
pub const FUNCTION_ALIGN: u64 = 16;

/// How a call of a top-level function reaches its callee.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum CallMode {
    /// Load the callee's slot and call the address in it, so the REPL and
    /// hot reloading can repoint the slot (D-043). The only mode used
    /// between Cheby functions, in the JIT and in executables alike
    /// (D-916).
    #[default]
    Indirect,
    /// Call the callee's symbol directly. No build uses it (D-916: direct
    /// calls are reconsidered with M6's release profile).
    Direct,
}

/// How generated code declares an `@external(native, …)` symbol. In both
/// modes code reaches the symbol only through its import word
/// `cb$imp$<symbol>` and calls the loaded address with `call_indirect`
/// and the call site's C signature (D-913, D-475).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum ForeignImport {
    /// Declare the symbol with no signature. One module can then hold
    /// several C signatures for one symbol, which the REPL needs: a
    /// redefinition may change a binding's C signature, and a failed batch
    /// leaves its declarations behind. The JIT's mode.
    #[default]
    Address,
    /// Declare the symbol as a function with its C signature. A module
    /// holds one signature per symbol, which D-455 guarantees within a
    /// program.
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

/// One function that [`define_with_stats`] defined, for the JIT's map
/// from code addresses to call locations (D-856, D-867).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefinedCode {
    /// The function, whose address the module gives once finalized.
    pub id: cranelift_module::FuncId,
    /// Its machine code's length in bytes.
    pub len: u32,
    /// Whether it has the Cheby ABI ([`CompiledFunction::cheby_abi`]).
    pub cheby_abi: bool,
    /// Its calls with a location, by return offset, sorted
    /// ([`CompiledFunction::call_sites`]).
    pub sites: Vec<CallSiteDesc>,
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
    define_with_stats(module, mirs, decls, cx, opts).map(|(clif, _, _)| clif)
}

/// [`define`], returning also what it defined: the bytes of machine code
/// and the number of functions (D-718), and each function defined, in
/// definition order, with its length, ABI and call locations (D-856).
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
) -> Result<(Vec<String>, DefineStats, Vec<DefinedCode>), CodegenError> {
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
    // The shims of the batch's `@blocking` calls (D-818), in
    // declaration order; each is defined once per JIT, with the batch that
    // declared it. They are a few instructions each, so they are compiled
    // here rather than on the pool.
    let mut shims = Vec::with_capacity(decls.pending_shims.len());
    for (id, symbol, sig) in &decls.pending_shims {
        let translated = blocking::translate_shim(decls, *id, symbol, sig)?;
        let text = opts.capture_clif.then(|| decls.describe(&translated));
        shims.push((*id, text, compile_function(module.isa(), translated)?));
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
    // `(key, MIR function, id, code)`; the key is `None` for an inner body
    // and the function `None` for a shim.
    let mut compiled = Vec::with_capacity(todo.len().saturating_add(large.len()));
    for (index, (&(_, function, id), (text, code))) in todo.iter().zip(first).enumerate() {
        if let Some((inner, split)) = split_by_index.remove(&index) {
            clif.extend(split.outer_text);
            clif.extend(split.inner_text);
            compiled.push((Some(function.key), Some(function), id, split.outer));
            compiled.push((None, Some(function), inner, split.inner));
        } else {
            clif.extend(text);
            compiled.push((Some(function.key), Some(function), id, code));
        }
    }
    for (id, text, code) in shims {
        clif.extend(text);
        compiled.push((None, None, id, code));
    }
    // What the JIT's code map needs (D-856), built before anything is
    // defined, so a missing location string or D-913's code model, checked
    // on every compile, fails the batch whole.
    let mut defined = Vec::with_capacity(compiled.len());
    for &(_, function, id, ref code) in &compiled {
        defined.push(defined_code(&*module, decls, function, id, code)?);
    }
    // A function defined twice would fail after the data are defined,
    // which the module keeps: check first, so a failed batch defines
    // nothing.
    let mut ids = rustc_hash::FxHashSet::default();
    for &(_, _, id, _) in &compiled {
        if decls.defined_funcs.contains(&id) || !ids.insert(id) {
            return Err(CodegenError::ice(format!(
                "the function `{}` is defined twice",
                decls.func_name(id)
            )));
        }
    }
    data::define_pending(module, decls, cx)?;
    decls.defined_funcs.extend(ids);
    decls.pending_shims.clear();
    let mut stats = DefineStats::default();
    for (key, _, id, code) in compiled {
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
    Ok((clif, stats, defined))
}

/// Whether a relocation of kind `kind` in text is PC-relative, the only
/// kinds D-913's code model leaves there: on x86-64 `X86CallPCRel4`, for
/// calls and for `lea` of colocated data alike (X0's caveat 3), and
/// `X86PCRel4`; on aarch64 `Arm64Call` (`bl`) and the
/// `adrp`+`add` pair `Aarch64AdrPrelPgHi21` and `Aarch64AddAbsLo12Nc`
/// (a page offset, despite its name). Absolute kinds (`Abs4`, `Abs8`),
/// GOT, PLT and TLS kinds are never PC-relative text in this model.
const fn pc_relative(kind: cranelift_codegen::binemit::Reloc) -> bool {
    use cranelift_codegen::binemit::Reloc;
    matches!(
        kind,
        Reloc::X86PCRel4
            | Reloc::X86CallPCRel4
            | Reloc::Arm64Call
            | Reloc::Aarch64AdrPrelPgHi21
            | Reloc::Aarch64AddAbsLo12Nc
    )
}

/// D-913's check of one compiled function's text, before anything is
/// defined: every relocation is PC-relative ([`pc_relative`]) and names a
/// generated symbol of `module`, never a libcall, a symbol the linker
/// knows, or a runtime or foreign symbol (`Linkage::Import`), which only
/// import words' data relocations name. A generated `cb$` symbol another
/// unit defines, which a unit alone imports (D-917), is allowed. One
/// record then serves the JIT, the cache and executables (ADR-0064): text
/// needs no absolute relocation, which PIE executables and arm64 Mach-O
/// refuse.
///
/// # Errors
///
/// An ICE naming the function and the relocation.
fn check_text_relocs<M: Module>(
    module: &M,
    decls: &Decls,
    id: cranelift_module::FuncId,
    code: &CompiledFunction,
) -> Result<(), CodegenError> {
    use cranelift_module::{Linkage, ModuleRelocTarget};
    for reloc in &code.relocs {
        let fail = |what: &str| {
            CodegenError::ice(format!(
                "the function `{}` has {what} at offset {} of its text (D-913)",
                decls.func_name(id),
                reloc.offset
            ))
        };
        let target = match reloc.name {
            ModuleRelocTarget::User {
                namespace: 0,
                index,
            } => {
                let target = cranelift_module::FuncId::from_u32(index);
                let linkage = module.declarations().get_function_decl(target).linkage;
                (decls.func_name(target).to_owned(), linkage)
            }
            ModuleRelocTarget::User {
                namespace: 1,
                index,
            } => {
                let target = cranelift_module::DataId::from_u32(index);
                let linkage = module.declarations().get_data_decl(target).linkage;
                (decls.data_name(target).to_owned(), linkage)
            }
            ModuleRelocTarget::LibCall(libcall) => {
                return Err(fail(&format!("a relocation to the libcall `{libcall}`")));
            }
            ModuleRelocTarget::User { namespace, .. } => {
                return Err(fail(&format!(
                    "a relocation to a name of namespace {namespace}"
                )));
            }
            ModuleRelocTarget::KnownSymbol(_) | ModuleRelocTarget::FunctionOffset(..) => {
                return Err(fail(&format!("a relocation to `{}`", reloc.name)));
            }
        };
        let (name, linkage) = target;
        if !pc_relative(reloc.kind) {
            return Err(fail(&format!(
                "an absolute or unexpected `{:?}` relocation to `{name}`",
                reloc.kind
            )));
        }
        // D-917: in a unit alone, text reaches another unit's generated
        // symbols (slots, layout words, `cb$ctor$` statics, constant slots,
        // test bodies and constant initializers) as imports, PC-relative;
        // runtime and foreign symbols stay behind import words.
        if linkage == Linkage::Import && !decls.is_generated_import(&name) {
            return Err(fail(&format!(
                "a relocation to `{name}`, which only an import word may name"
            )));
        }
    }
    Ok(())
}

/// The code map's entry of `code`, the function `id` compiled from
/// `function` (`None` for a shim, which has no calls with a location),
/// once its text passed D-913's check ([`check_text_relocs`]).
///
/// # Errors
///
/// An ICE if the text has a relocation the code model forbids, the code
/// is too long or a call's location is not in `function`'s table.
fn defined_code<M: Module>(
    module: &M,
    decls: &Decls,
    function: Option<&cheby_mir::Function>,
    id: cranelift_module::FuncId,
    code: &CompiledFunction,
) -> Result<DefinedCode, CodegenError> {
    check_text_relocs(module, decls, id, code)?;
    let len = u32::try_from(code.bytes.len())
        .map_err(|_| CodegenError::ice("a function's code is longer than 4 GiB"))?;
    let mut sites = Vec::with_capacity(code.call_sites.len());
    for &(ret_offset, loc) in &code.call_sites {
        let location = function
            .and_then(|f| f.call_loc_strings.get(loc))
            .ok_or_else(|| {
                CodegenError::ice(format!("a call's location str#{} is unknown", loc.as_u32()))
            })?;
        sites.push(CallSiteDesc {
            ret_offset,
            location: location.to_string(),
        });
    }
    Ok(DefinedCode {
        id,
        len,
        cheby_abi: code.cheby_abi,
        sites,
    })
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
    /// The return offset of each call with a location (D-856), with the
    /// location's id in [`cheby_mir::Function::call_loc_strings`], sorted
    /// by offset: each call's debug tag paired with its return address.
    pub call_sites: Vec<(u32, StrId)>,
    /// Whether the function has the Cheby ABI (Cranelift's `tail`
    /// convention): a deadlock report's walk passes through such a frame,
    /// and stops at a C-ABI one (entry thunks, trampolines, `@blocking`
    /// shims).
    pub cheby_abi: bool,
}

impl std::fmt::Debug for CompiledFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledFunction")
            .field("len", &self.bytes.len())
            .field("align", &self.align)
            .field("relocs", &self.relocs.len())
            .field("frame_size", &self.frame_size)
            .field("call_sites", &self.call_sites.len())
            .field("cheby_abi", &self.cheby_abi)
            .finish()
    }
}

/// Pairs each call's `Post` debug tag at its offset in `tags` with the
/// call's return offset, the greatest of `returns` (sorted, as Cranelift
/// records them) at or below it: the instructions that reclaim stack
/// arguments or load stack results may sit between the return address and
/// the tag (survey-locations §1.7). `Pre` tags, which Cranelift copies to
/// a call's argument moves, are ignored. The result is sorted by return
/// offset, each call once.
///
/// # Errors
///
/// An ICE if a tag has no call at or before it.
fn pair_call_sites(
    returns: &[u32],
    tags: impl Iterator<Item = (u32, bool, u32)>,
) -> Result<Vec<(u32, StrId)>, CodegenError> {
    let mut sites = Vec::new();
    for (offset, post, tag) in tags {
        if !post {
            continue;
        }
        let at = returns.partition_point(|&r| r <= offset);
        let ret = at
            .checked_sub(1)
            .and_then(|i| returns.get(i))
            .ok_or_else(|| {
                CodegenError::ice(format!("a call's location tag at {offset} follows no call"))
            })?;
        sites.push((*ret, StrId::from_u32(tag)));
    }
    sites.sort_by_key(|&(ret, _)| ret);
    sites.dedup();
    Ok(sites)
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
    let cheby_abi = func.signature.call_conv == cranelift_codegen::isa::CallConv::Tail;
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
    let call_sites = if ctx.func.debug_tags.is_empty() {
        Vec::new()
    } else {
        let returns: Vec<u32> = compiled.buffer.call_sites().map(|s| s.ret_addr).collect();
        let tags = compiled.buffer.debug_tags().flat_map(|list| {
            let post = list.pos == cranelift_codegen::MachDebugTagPos::Post;
            list.tags.iter().filter_map(move |tag| match tag {
                ir::DebugTag::User(id) => Some((list.offset, post, *id)),
                ir::DebugTag::StackSlot(_) => None,
            })
        });
        pair_call_sites(&returns, tags)?
    };
    Ok(CompiledFunction {
        bytes,
        align,
        relocs,
        frame_size,
        call_sites,
        cheby_abi,
    })
}
