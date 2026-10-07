//! The JIT (E1-E5): a Cranelift `JITModule` that compiles
//! batches of MIR units, the runtime's symbol registry, the indirection
//! table of top-level functions (D-043), and the way into generated code.
//!
//! - [`Jit::compile`] is incremental: each batch registers its layouts,
//!   declares its functions (extending the persistent declaration table),
//!   defines them and finalizes. Later batches call earlier code through
//!   the existing slots. `cheby run` compiles one batch; constant
//!   evaluation (E6) and the REPL (G5) compile one per step.
//! - Every call of a top-level function loads the callee's slot, a
//!   writable data object holding its address (D-043).
//! - The host never calls `tail`-convention code: it calls C-ABI entry
//!   thunks ([`Jit::entry`]), which call the Cheby functions (D-453). An
//!   [`Entry`] borrows its [`Jit`], so safe code cannot run an entry after
//!   its code is freed; [`Entry::run_main`] runs `main`'s thunk with the
//!   exit-status policy of §10.10.
//! - Code and data live in one reservation of 1 GiB of address space per
//!   `Jit`, committed segment by segment as batches need it (see
//!   `memory.rs`). Dropping a [`Jit`] frees it.
//! - Runtime symbols resolve through `cheby_runtime::symbols()`; other
//!   native symbols of `@external(native, …)` through the process's
//!   dynamic symbol table, checked before finalizing so a missing symbol
//!   is an error, never a panic. Foreign symbols are imported by address,
//!   with no signature, and called indirectly with each call site's C
//!   signature, so batches may bind one symbol with different signatures.

use std::fmt;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::sync::Arc;

use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::Module;
use rustc_hash::FxHashMap;

use cheby_codegen_clif::{
    CodegenCx, CodegenError, CodegenOptions, Decls, LayoutIds, builtin_layout_id,
};
use cheby_iface::Ice;
use cheby_mir::{EntryKind, FnKey, LayoutDesc, LayoutRef, MirModule, Repr};
use cheby_runtime::layout::{self as rt_layout, FieldKind, LayoutInfo};
use cheby_runtime::{EntryConfig, EntryFn, EntryOutcome, RunConfig, RuntimeError};

mod memory;
#[cfg(test)]
mod tests;

/// Why the JIT failed. None of these is a diagnostic about the program
/// except [`JitError::UnresolvedSymbol`].
#[derive(Debug)]
pub enum JitError {
    Codegen(CodegenError),
    /// The host cannot run generated code.
    Host(String),
    /// An `@external(native, …)` symbol that neither the runtime nor the
    /// process defines.
    UnresolvedSymbol(String),
    /// Running the entry failed in the runtime.
    Runtime(RuntimeError),
}

impl JitError {
    fn ice(message: impl Into<String>) -> Self {
        Self::Codegen(CodegenError::Ice(Ice::new(message)))
    }
}

impl From<CodegenError> for JitError {
    fn from(e: CodegenError) -> Self {
        Self::Codegen(e)
    }
}

impl From<Ice> for JitError {
    fn from(e: Ice) -> Self {
        Self::Codegen(CodegenError::Ice(e))
    }
}

impl fmt::Display for JitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codegen(e) => write!(f, "{e}"),
            Self::Host(message) => write!(f, "cannot run generated code on this host: {message}"),
            Self::UnresolvedSymbol(symbol) => {
                write!(f, "the native symbol `{symbol}` is not defined")
            }
            Self::Runtime(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for JitError {}

/// Runtime layout ids of the layouts registered so far: the fixed builtin
/// ids, plus user constructors registered per batch before any of its
/// code is defined (D-454).
#[derive(Debug, Default)]
pub struct JitLayouts {
    ids: FxHashMap<LayoutRef, u32>,
}

impl LayoutIds for JitLayouts {
    fn id(&self, layout: &LayoutRef) -> Result<u32, Ice> {
        builtin_layout_id(layout)
            .or_else(|| self.ids.get(layout).copied())
            .ok_or_else(|| Ice::new(format!("layout {layout:?} was not registered")))
    }
}

/// How a constructor field of `repr` is stored in the runtime's layout
/// table.
const fn field_kind(repr: Repr) -> FieldKind {
    match repr {
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
    }
}

/// The runtime's entry for a MIR layout. MIR computes offsets from
/// interfaces and the runtime from field kinds; they must agree, or
/// generated code and freeing would disagree on where fields are.
fn layout_info(desc: &LayoutDesc) -> Result<LayoutInfo, Ice> {
    let fields: Vec<(FieldKind, Option<&str>)> = desc
        .fields
        .iter()
        .map(|f| (field_kind(f.repr), f.name.as_deref()))
        .collect();
    let info = LayoutInfo::ctor(&desc.name, desc.tag, &fields);
    let agree = info.value_fields == desc.value_fields
        && info.raw_fields == desc.raw_fields
        && info.fields.len() == desc.fields.len()
        && info
            .fields
            .iter()
            .zip(&desc.fields)
            .all(|(rt, mir)| i64::from(rt.offset) == i64::from(mir.offset));
    if agree {
        Ok(info)
    } else {
        Err(Ice::new(format!(
            "MIR and the runtime disagree on the layout of `{}`",
            desc.name
        )))
    }
}

/// The memory a [`Jit`] holds for code and data, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeMemory {
    /// Address space reserved, none of it usable until committed.
    pub reserved: usize,
    /// Pages committed for the segments of compiled batches.
    pub committed: usize,
}

/// A JIT session: the module, the declarations of every batch so far, and
/// the registered layouts. Dropping it frees its code and data.
pub struct Jit {
    /// Taken only by `Drop`, which frees its memory.
    module: ManuallyDrop<JITModule>,
    decls: Decls,
    layouts: JitLayouts,
    options: CodegenOptions,
    clif: Vec<String>,
    /// How much of the code arena is reserved and committed.
    memory: Arc<memory::Usage>,
}

impl fmt::Debug for Jit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Jit")
            .field("decls", &self.decls)
            .field("layouts", &self.layouts)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl Jit {
    /// A JIT for the host with the default options: calls through slots,
    /// stack checks on.
    ///
    /// # Errors
    ///
    /// [`JitError::Host`] if the host is not supported.
    pub fn new() -> Result<Self, JitError> {
        Self::with_options(CodegenOptions::default())
    }

    /// A JIT for the host with `options`. Batches that bind one foreign
    /// symbol with different C signatures (a REPL redefinition, or a retry
    /// after a failed batch) need the default
    /// [`ForeignImport::Address`](cheby_codegen_clif::ForeignImport::Address).
    ///
    /// # Errors
    ///
    /// [`JitError::Host`] if the host is not supported or the code arena
    /// cannot be reserved.
    pub fn with_options(options: CodegenOptions) -> Result<Self, JitError> {
        let flags = cheby_codegen_clif::isa::flags()?;
        let isa = cranelift_native::builder()
            .map_err(|message| JitError::Host(message.to_owned()))?
            .finish(flags)
            .map_err(|e| JitError::Host(e.to_string()))?;
        let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        builder.symbols(cheby_runtime::symbols());
        let arena = memory::ReservedArena::new(memory::ARENA_SIZE)
            .map_err(|e| JitError::Host(format!("cannot reserve memory for code: {e}")))?;
        let usage = arena.usage();
        builder.memory_provider(Box::new(arena));
        let module = ManuallyDrop::new(JITModule::new(builder));
        let decls = Decls::new(module.isa())?;
        Ok(Self {
            module,
            decls,
            layouts: JitLayouts::default(),
            options,
            clif: Vec::new(),
            memory: usage,
        })
    }

    /// Compiles a batch of units, in order: registers their layouts,
    /// declares and defines their functions, and finalizes. Units may call
    /// functions of earlier batches.
    ///
    /// A batch is all or nothing: if it fails, the JIT is as it was before
    /// it, so a corrected batch with the same items can be compiled next
    /// (the REPL after a bad entry).
    ///
    /// # Errors
    ///
    /// A [`JitError`] if code generation fails or a native symbol is
    /// missing.
    pub fn compile(&mut self, mirs: &[MirModule]) -> Result<(), JitError> {
        let decls = self.decls.clone();
        let layouts = self.layouts.ids.clone();
        let result = self.compile_batch(mirs);
        if result.is_err() {
            // Nothing of the batch was defined in the module (`define`
            // compiles everything first), so restoring the tables undoes
            // it; its declarations stay in the module, unused.
            self.decls = decls;
            self.layouts.ids = layouts;
        }
        result
    }

    fn compile_batch(&mut self, mirs: &[MirModule]) -> Result<(), JitError> {
        self.register_layouts(mirs)?;
        let cx = CodegenCx {
            layouts: &self.layouts,
        };
        cheby_codegen_clif::declare(&mut *self.module, &mut self.decls, mirs, &cx, &self.options)?;
        self.check_symbols()?;
        let clif = cheby_codegen_clif::define(
            &mut *self.module,
            mirs,
            &mut self.decls,
            &cx,
            &self.options,
        )?;
        self.clif.extend(clif);
        self.module
            .finalize_definitions()
            .map_err(|e| JitError::Codegen(CodegenError::Module(e.to_string())))?;
        Ok(())
    }

    /// How much memory this JIT holds for code and data.
    #[must_use]
    pub fn code_memory(&self) -> CodeMemory {
        CodeMemory {
            reserved: self.memory.reserved(),
            committed: self.memory.committed(),
        }
    }

    /// The CLIF text of every function compiled since the last call, when
    /// the options capture it.
    pub fn take_clif(&mut self) -> Vec<String> {
        std::mem::take(&mut self.clif)
    }

    /// The compiled entry thunk of `kind`, which borrows this JIT so that
    /// its code outlives every run of it.
    ///
    /// # Errors
    ///
    /// An ICE if no compiled batch defined it.
    pub fn entry(&self, kind: EntryKind) -> Result<Entry<'_>, JitError> {
        let decl = self.decls.func(FnKey::Entry(kind))?;
        if !decl.defined {
            return Err(JitError::ice(format!("entry {kind:?} was not compiled")));
        }
        let address = self.module.get_finalized_function(decl.id);
        if address.is_null() {
            return Err(JitError::ice("a compiled entry has no address"));
        }
        // SAFETY: the thunk was generated with the platform C calling
        // convention and exactly the `EntryFn` signature (`ctx` in, one
        // 64-bit word out, D-453), and it was finalized, so its
        // memory is executable. The address stays valid while
        // `self.module` lives, which the returned `Entry`'s borrow of
        // `self` guarantees for every use of it.
        let thunk = unsafe { std::mem::transmute::<*const u8, EntryFn>(address) };
        Ok(Entry {
            thunk,
            _jit: PhantomData,
        })
    }

    /// Registers the constructor layouts of `mirs`, units in order and
    /// layouts in `DefIndex` order, so ids do not depend on scheduling.
    ///
    /// The ids come from the runtime's process-wide table, so they also
    /// depend on what earlier `Jit`s in the process registered. Generated
    /// code must therefore never embed one (D-264): it loads the header
    /// from the layout's `cb$layout$…` data symbol, which is filled in here
    /// at registration (D-454). No generated code needs a user layout id
    /// yet, because `Alloc` and static user constructors are rejected
    /// before codegen.
    fn register_layouts(&mut self, mirs: &[MirModule]) -> Result<(), JitError> {
        for unit in mirs {
            for desc in &unit.layouts {
                if self.layouts.ids.contains_key(&desc.layout) {
                    continue;
                }
                let info = layout_info(desc)?;
                let id = rt_layout::register(info)
                    .map_err(|e| JitError::ice(format!("cannot register a layout: {e}")))?;
                self.layouts.ids.insert(desc.layout, id);
            }
        }
        Ok(())
    }

    /// Checks that every native symbol resolves, because Cranelift's JIT
    /// panics on a missing one when finalizing. `cheby check` cannot know
    /// which symbols the process will have, so the driver reports this
    /// only when running, as E0053 at the `@external` that names the
    /// symbol (D-455).
    fn check_symbols(&self) -> Result<(), JitError> {
        let runtime: Vec<&str> = cheby_runtime::symbols()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        for symbol in self.decls.foreign_symbols() {
            if !runtime.contains(&symbol) && !process_defines(symbol) {
                return Err(JitError::UnresolvedSymbol(symbol.to_owned()));
            }
        }
        Ok(())
    }
}

/// Whether the process defines `symbol`, the JIT's fallback lookup.
#[cfg(unix)]
fn process_defines(symbol: &str) -> bool {
    let Ok(name) = std::ffi::CString::new(symbol) else {
        return false;
    };
    // SAFETY: `name` is a valid NUL-terminated string that outlives the
    // call, and `RTLD_DEFAULT` is a valid handle; `dlsym` only reads.
    let address = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    !address.is_null()
}

/// Whether the process defines `symbol`. Only the runtime's symbols are
/// known off Unix.
#[cfg(not(unix))]
fn process_defines(_symbol: &str) -> bool {
    false
}

impl Drop for Jit {
    fn drop(&mut self) {
        // SAFETY: `module` is taken exactly once, here, and never used
        // again.
        let module = unsafe { ManuallyDrop::take(&mut self.module) };
        // SAFETY: no code of this JIT can run after this: every `Entry`
        // borrows the `Jit`, so none outlives it, and running one returns
        // only after its thunk returned or panicked. A panicked entry's
        // thread is parked forever inside the runtime and never resumes
        // into this code. Without this, cranelift-jit keeps finalized
        // memory forever.
        unsafe { module.free_memory() };
    }
}

/// A compiled entry thunk of a [`Jit`] (D-453). It borrows the
/// `Jit`, so its code stays valid for as long as it can be run:
///
/// ```compile_fail,E0505
/// let jit = cheby_jit::Jit::new().unwrap();
/// let entry = jit.entry(cheby_mir::EntryKind::Repl(0)).unwrap();
/// drop(jit);
/// let _ = entry.run_main(cheby_runtime::RunConfig::default());
/// ```
#[derive(Clone, Copy)]
pub struct Entry<'jit> {
    thunk: EntryFn,
    _jit: PhantomData<&'jit Jit>,
}

impl fmt::Debug for Entry<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry").finish_non_exhaustive()
    }
}

impl Entry<'_> {
    /// Runs the entry on its own stack and reports how it ended. A
    /// returned word may point into the `Jit`'s data (a static string or
    /// constructor) and is valid only while the `Jit` lives.
    ///
    /// # Errors
    ///
    /// [`JitError::Runtime`] if the entry cannot run.
    pub fn run(self, config: &EntryConfig) -> Result<EntryOutcome, JitError> {
        // SAFETY: `thunk` is an entry thunk with the `EntryFn` ABI built by
        // the JIT (see `Jit::entry`), and the borrow of the `Jit` keeps its
        // code and data alive until this returns.
        unsafe { cheby_runtime::run_entry(self.thunk, config) }.map_err(JitError::Runtime)
    }

    /// Runs `main`'s entry thunk under `cheby run`'s policy (§10.10,
    /// D-182) and returns the exit status: 0 or 1 from the program, 101
    /// after a panic, 102 in debug builds when objects leak (D-458).
    ///
    /// # Errors
    ///
    /// [`JitError::Runtime`] if the entry cannot run.
    pub fn run_main(self, config: RunConfig) -> Result<i32, JitError> {
        // SAFETY: as in `run`.
        unsafe { cheby_runtime::run_main(self.thunk, config) }.map_err(JitError::Runtime)
    }
}
