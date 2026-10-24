//! The JIT (E1-E5): a Cranelift `JITModule` that compiles
//! batches of MIR units, the runtime's symbol registry, the indirection
//! table of top-level functions (D-043), and the way into generated code.
//!
//! - [`Jit::compile`] is incremental: each batch declares its functions
//!   (extending the persistent declaration table), defines them and a
//!   module record for each of its code and tests units, finalizes, and
//!   registers the records with `cheby_runtime::image::register_records`,
//!   the one routine an executable's start calls too (D-919): layouts,
//!   external types, hooks and call-site ranges. The JIT registers
//!   nothing itself. Later batches call earlier code through the existing
//!   slots, and use the static objects, layout words and constant slots
//!   earlier batches defined (D-523). `cheby run` compiles one batch;
//!   constant evaluation (E6) and the REPL (G5) compile one per step.
//! - Every call of a top-level function loads the callee's slot, a
//!   writable data object holding its address (D-043). The REPL (E4)
//!   registers its items' session names first ([`Jit::set_session_names`]):
//!   their symbols carry `$r<generation>` (D-547), and a redefinition that
//!   takes over an earlier definition repoints that slot once its batch is
//!   finalized, so existing callers reach the new code
//!   ([`Jit::revert_repoints`] undoes it for a rejected entry;
//!   [`Jit::compile_unpublished`] leaves it to the REPL, which publishes
//!   it once the constants the new code reads are filled); one with
//!   another signature gets a new slot (D-456). [`Jit::slot`] and
//!   [`Jit::repoint`] do the same by hand, and [`Jit::bind_values`] stores
//!   the session values of prompt-level `let`s in constant slots. Slots
//!   are written with release stores, since a REPL session's detached
//!   fibers may run the JIT's code while a later entry compiles (D-712):
//!   they call the old code or the new, and code is never freed while the
//!   `Jit` lives.
//! - The host never calls `tail`-convention code: it calls C-ABI entry
//!   thunks ([`Jit::entry`]), which call the Cheby functions (D-453). An
//!   [`Entry`] borrows its [`Jit`], so safe code cannot run an entry after
//!   its code is freed; [`Entry::run_main`] runs `main`'s thunk with the
//!   exit-status policy of §10.10, and [`Entry::run_root`] runs an entry
//!   as a root fiber of a [`Runtime`] (D-690). Every run gets the JIT's
//!   trampolines ([`Jit::callbacks`], D-526), its accounting domain
//!   ([`Jit::domain`]) and whether its code has unwind edges
//!   ([`Jit::unwind_edges`], D-742). An entry that panics unwinds by the
//!   return flag (D-733): its frames release what they hold, and its
//!   thread ends.
//! - Code and data live in one reservation of 1 GiB of address space per
//!   `Jit`, committed segment by segment as batches need it (see
//!   `memory.rs`). Dropping a [`Jit`] hands the memory to its
//!   [`CodeToken`]s, and the last of them frees it ([`Jit::code_token`],
//!   D-708): a runtime that runs the JIT's code as fibers holds one until
//!   its fibers have stopped.
//! - Runtime symbols resolve through `cheby_runtime::symbols()`; other
//!   native symbols of `@external(native, …)` through the symbols defined
//!   with [`Jit::define_symbol`] and then the process's dynamic symbol
//!   table, checked before finalizing so a missing symbol is an error,
//!   never a panic (D-532). Foreign symbols are imported by address, with
//!   no signature, and called indirectly with each call site's C
//!   signature, so batches may bind one symbol with different signatures.
//! - The code model is the one the cache and executables use (D-913,
//!   D-921, ADR-0064): the target's fixed ISA
//!   ([`cheby_codegen_clif::isa::host_isa`]), never the host's detected
//!   features, non-PIC, with every generated symbol colocated. Runtime and
//!   foreign symbols are resolved into the session's import words
//!   `cb$imp$<symbol>`, data the code loads, so no text holds a process
//!   address. No libcall resolves ([`cheby_codegen_clif::isa::libcall_names`]).

use std::fmt;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::Module;
use rustc_hash::FxHashMap;

use cheby_codegen_clif::{
    BatchNew, BatchRecord, BuiltinLayouts, CodegenCx, CodegenError, CodegenOptions, Decls,
    DefineStats, SessionName,
};
use cheby_iface::{DefId, Ice};
use cheby_mir::{AbiSig, EntryKind, FnKey, MirModule, UnitKind};
use cheby_runtime::abi::sym;
use cheby_runtime::alloc::Domain;
use cheby_runtime::entry::{Tramp0, Tramp1, Tramp2, Tramp3, Tramp4};
use cheby_runtime::image::{self, ModuleRecord};
use cheby_runtime::{
    Callbacks, CodeToken, DetachedGroup, EntryConfig, EntryFn, EntryOutcome, RootDone, RootHandle,
    RunConfig, Runtime, RuntimeError,
};

mod memory;
#[cfg(test)]
mod start_root_tests;
// The tests compile and run machine code. Windows support is M6 (D-545),
// so they are not built there. Two attributes, not `all(test, …)`, so
// Clippy still sees a test module (`allow-unwrap-in-tests`).
#[cfg(test)]
#[cfg(not(windows))]
mod tests;

/// Why the JIT failed. None of these is a diagnostic about the program
/// except [`JitError::UnresolvedSymbols`].
#[derive(Debug)]
pub enum JitError {
    Codegen(CodegenError),
    /// The host cannot run generated code.
    Host(String),
    /// Every `@external(native, …)` symbol of the batch that neither the
    /// runtime, [`Jit::define_symbol`] nor the process defines, sorted
    /// (D-532). The driver reports one E0053 each.
    UnresolvedSymbols(Vec<String>),
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
            Self::UnresolvedSymbols(symbols) => {
                let list: Vec<String> = symbols.iter().map(|s| format!("`{s}`")).collect();
                if let [one] = list.as_slice() {
                    write!(f, "the native symbol {one} is not defined")
                } else {
                    write!(f, "the native symbols {} are not defined", list.join(", "))
                }
            }
            Self::Runtime(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for JitError {}

/// What a [`Jit`] does beyond code generation's options (plan §4.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitOptions {
    /// Every count operation calls the runtime's `cheby_rt_dup` and
    /// `cheby_rt_drop`, which check headers in poisoning mode, instead of
    /// the inline fast paths ([`CodegenOptions::checked_rc`]). On when
    /// `CHEBY_RT_POISON=1` ([`JitOptions::from_env`]).
    pub checked_rc: bool,
    /// Binds every unresolved native symbol to `cheby_rt_unresolved`,
    /// which panics `ForbiddenEffect`, instead of failing the batch: the
    /// batches of `cheby check`, which compile code only to evaluate
    /// constants (D-532).
    pub stub_unresolved: bool,
}

impl JitOptions {
    /// The options of a JIT that runs programs: `checked_rc` when the
    /// runtime's poisoning mode is on (`CHEBY_RT_POISON=1` in a debug
    /// build), no stubs.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            checked_rc: cheby_runtime::alloc::poison_enabled(),
            stub_unresolved: false,
        }
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

/// Symbols the JIT resolves itself, before the process: those defined
/// with [`Jit::define_symbol`] and, with [`JitOptions::stub_unresolved`],
/// the stubbed ones. Addresses are kept as integers so the table is
/// `Send` for Cranelift's lookup function.
type SymbolTable = Arc<Mutex<FxHashMap<Box<str>, usize>>>;

/// The code and data memory of a [`Jit`] after the `Jit` is dropped
/// (D-708): [`Jit`]'s `Drop` moves its module here, and the last
/// [`CodeToken`] (or the `Jit` itself, if it handed none out) frees it.
struct CodeKeepAlive {
    /// The module, once the `Jit` is dropped.
    module: Mutex<Option<JITModule>>,
    /// The key of the JIT's functions in the process-wide code map
    /// (D-856, D-867): registered batch by batch, removed
    /// with the memory.
    code_owner: u64,
}

impl Drop for CodeKeepAlive {
    fn drop(&mut self) {
        // The map forgets the ranges before their memory goes, so a later
        // JIT's code at the same addresses is never taken for this one's.
        cheby_runtime::locate::unregister_code(self.code_owner);
        let module = self
            .module
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(module) = module {
            // SAFETY: no code of the JIT can run any more: the `Jit` is
            // dropped, so no `Entry` borrows it and no isolated entry runs
            // its code (running one returns only after its thread ended:
            // its thunk returned, or panicked and unwound out of the
            // generated frames, D-733); and this is the last token, so no
            // `Runtime` that ran its code as fibers holds it, and a runtime
            // releases its tokens only once none of its fibers can run
            // again (D-708, I-6): `shutdown` and `Drop` first tear down
            // every group of detached fibers, cancelling and unwinding each
            // member until the group is empty (D-710, D-723), then stop
            // and join the workers; a halt instead freezes every fiber at a
            // safepoint, where it never runs again, and joins every thread,
            // the poll holder included (D-073, D-709). Either way the
            // tokens go last. A worker whose fiber is blocked inline in a
            // system call (D-786) is never joined, and its runtime then
            // keeps (leaks) its tokens, so this never runs while such a
            // worker can return into the code. `run_main`'s runtime holds a
            // token too (`Entry::run_main`). Without this, cranelift-jit
            // keeps finalized memory forever.
            unsafe { module.free_memory() };
        }
    }
}

/// A JIT session: the module, the declarations of every batch so far, and
/// the module records it registered. Dropping it frees its code and data,
/// or hands them to its outstanding [`CodeToken`]s (D-708).
pub struct Jit {
    /// Taken only by `Drop`, which moves it into `keep_alive`.
    module: ManuallyDrop<JITModule>,
    /// What frees the module's memory once the `Jit` and every token are
    /// gone.
    keep_alive: Arc<CodeKeepAlive>,
    decls: Decls,
    codegen: CodegenOptions,
    options: JitOptions,
    clif: Vec<String>,
    /// How much of the code arena is reserved and committed.
    memory: Arc<memory::Usage>,
    symbols: SymbolTable,
    /// The accounting domain every entry of this JIT charges (plan
    /// §4.3.4): one build, one domain.
    domain: Arc<Domain>,
    /// The trampolines compiled so far (D-526).
    callbacks: Callbacks,
    /// The slots the last batch repointed, with the words they held
    /// before ([`Jit::revert_repoints`]).
    repointed: Vec<(cranelift_module::DataId, u64)>,
    /// The repoints of the last batch not published yet: the slot, the
    /// word of the new code and the redefinition
    /// ([`Jit::compile_unpublished`]).
    pending: Vec<(cranelift_module::DataId, u64, DefId)>,
    /// The code every batch that compiled defined ([`Jit::code_stats`]).
    code_stats: DefineStats,
    /// The module records of every batch that compiled, in order
    /// ([`Jit::module_records`]).
    records: Vec<JitRecord>,
    /// The external types this JIT's records registered, so a later
    /// batch registers only the others (D-1169).
    externals: rustc_hash::FxHashSet<DefId>,
}

/// A module record one batch of a [`Jit`] defined in its memory (D-919):
/// the record of a code or tests unit, in the layout of
/// [`cheby_runtime::image::ModuleRecord`], listing that unit's functions,
/// the user types and external types the batch registered, their hooks
/// and free-shape words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitRecord {
    /// The unit's kind.
    pub unit: UnitKind,
    /// The unit's module path.
    pub path: Box<str>,
    /// The record's address in the JIT's memory, valid while the [`Jit`]
    /// (or a [`CodeToken`] of it) lives.
    pub address: usize,
}

/// An indirection slot of a [`Jit`] (D-043): a writable word holding the
/// address of a top-level function's code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotHandle {
    data: cranelift_module::DataId,
}

impl fmt::Debug for Jit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Jit")
            .field("decls", &self.decls)
            .field("records", &self.records)
            .field("codegen", &self.codegen)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl Jit {
    /// A JIT for the host with the default code generation options (calls
    /// through slots, stack checks on) and [`JitOptions::from_env`].
    ///
    /// # Errors
    ///
    /// [`JitError::Host`] if the host is not supported.
    pub fn new() -> Result<Self, JitError> {
        Self::configured(CodegenOptions::default(), JitOptions::from_env())
    }

    /// A JIT for the host with `options` and [`JitOptions::from_env`].
    /// Batches that bind one foreign symbol with different C signatures (a
    /// REPL redefinition, or a retry after a failed batch) need the default
    /// [`ForeignImport::Address`](cheby_codegen_clif::ForeignImport::Address).
    ///
    /// # Errors
    ///
    /// [`JitError::Host`] if the host is not supported or the code arena
    /// cannot be reserved.
    pub fn with_options(options: CodegenOptions) -> Result<Self, JitError> {
        Self::configured(options, JitOptions::from_env())
    }

    /// A JIT for the host with code generation `options` and JIT options
    /// `jit`; `jit.checked_rc` overrides `options.checked_rc`.
    ///
    /// # Errors
    ///
    /// [`JitError::Host`] if the host is not supported or the code arena
    /// cannot be reserved.
    pub fn configured(options: CodegenOptions, jit: JitOptions) -> Result<Self, JitError> {
        let options = CodegenOptions {
            checked_rc: jit.checked_rc,
            ..options
        };
        // D-921: the target's fixed feature set, never the host's detected
        // features, so the JIT runs the code the cache and executables
        // hold. D-913: no libcall resolves.
        let isa = cheby_codegen_clif::isa::host_isa().map_err(|e| match e {
            CodegenError::UnsupportedHost(message) => JitError::Host(message),
            other => JitError::Codegen(other),
        })?;
        let mut builder = JITBuilder::with_isa(isa, cheby_codegen_clif::isa::libcall_names());
        builder.symbols(cheby_runtime::symbols());
        let symbols: SymbolTable = Arc::default();
        let table = Arc::clone(&symbols);
        // Tried before the process's symbols (Cranelift asks the last
        // lookup function first); the runtime's registry comes before both.
        builder.symbol_lookup_fn(Box::new(move |name| {
            let table = table.lock().unwrap_or_else(PoisonError::into_inner);
            table
                .get(name)
                .map(|&address| std::ptr::with_exposed_provenance::<u8>(address))
        }));
        let arena = memory::ReservedArena::new(memory::ARENA_SIZE)
            .map_err(|e| JitError::Host(format!("cannot reserve memory for code: {e}")))?;
        let usage = arena.usage();
        builder.memory_provider(Box::new(arena));
        let module = ManuallyDrop::new(JITModule::new(builder));
        let decls = Decls::new(module.isa())?;
        Ok(Self {
            module,
            keep_alive: Arc::new(CodeKeepAlive {
                module: Mutex::new(None),
                code_owner: cheby_runtime::locate::new_code_owner(),
            }),
            decls,
            codegen: options,
            options: jit,
            clif: Vec::new(),
            memory: usage,
            symbols,
            domain: Domain::new(),
            callbacks: Callbacks::default(),
            repointed: Vec::new(),
            pending: Vec::new(),
            code_stats: DefineStats::default(),
            records: Vec::new(),
            externals: rustc_hash::FxHashSet::default(),
        })
    }

    /// The module records of every batch compiled so far, in order: one
    /// per code and tests unit, none for an entries unit (D-919, D-1107,
    /// D-1111). Each lists what its batch registered (tests and tools read
    /// them; they live as long as the `Jit`'s memory).
    #[must_use]
    pub fn module_records(&self) -> &[JitRecord] {
        &self.records
    }

    /// The machine code defined so far: its bytes and its functions, summed
    /// over every batch that compiled (D-718, `cargo xtask bench-compile`).
    #[must_use]
    pub const fn code_stats(&self) -> DefineStats {
        self.code_stats
    }

    /// A token that keeps this JIT's code and data memory alive after the
    /// `Jit` is dropped (D-708): the memory is freed by the last of the
    /// `Jit` and its tokens. A [`Runtime`] whose fibers run this JIT's code
    /// holds one ([`Runtime::attach`]) until its fibers have stopped.
    #[must_use]
    pub fn code_token(&self) -> CodeToken {
        let keep_alive: Arc<CodeKeepAlive> = Arc::clone(&self.keep_alive);
        let keep_alive: Arc<dyn Send + Sync> = keep_alive;
        CodeToken::new(keep_alive)
    }

    /// Whether the code this JIT compiles has unwind edges
    /// ([`CodegenOptions::unwind_edges`], D-742), which every entry it runs
    /// records in its control block: a raise in a fiber whose code has
    /// none ends the process with its report (D-770).
    /// Constant evaluation and the REPL copy it into the configurations
    /// of the entries they build, as [`Entry::run`] does.
    #[must_use]
    pub const fn unwind_edges(&self) -> bool {
        self.codegen.unwind_edges
    }

    /// Makes the native symbol `name` resolve to `address` in later
    /// batches, before the process's own symbols (tests, and kernels the
    /// host provides).
    ///
    /// # Safety
    ///
    /// `address` must stay valid for as long as this JIT's code can run,
    /// and be a function with the C signature of every `@external` that
    /// names `name` (or a data object, for a data import).
    pub unsafe fn define_symbol(&mut self, name: &str, address: *const u8) {
        let mut table = self.symbols.lock().unwrap_or_else(PoisonError::into_inner);
        table.insert(name.into(), address.expose_provenance());
    }

    /// Compiles a batch of units, in order: declares and defines their
    /// functions and data and a module record for each code and tests
    /// unit, finalizes, and registers the records with the runtime's one
    /// routine, `cheby_runtime::image::register_records` (D-919): the
    /// layouts, external types, hooks and call-site ranges, before any
    /// code of the batch runs. Units may call functions of earlier
    /// batches.
    ///
    /// A batch is all or nothing: if it fails, the JIT is as it was before
    /// it, so a corrected batch with the same items can be compiled next
    /// (the REPL after a bad entry).
    ///
    /// # Errors
    ///
    /// A [`JitError`] if code generation fails or native symbols are
    /// missing ([`JitError::UnresolvedSymbols`], all of them).
    pub fn compile(&mut self, mirs: &[MirModule]) -> Result<(), JitError> {
        self.compile_unpublished(mirs)?;
        self.publish_all_repoints()
    }

    /// [`Jit::compile`], except that the slots its REPL redefinitions take
    /// over keep their old code until [`Jit::publish_repoints`] (or
    /// [`Jit::publish_all_repoints`]) points them at the new code. A REPL
    /// session's detached fibers may call through those slots while the
    /// batch's constants are evaluated, and the new code may read constants
    /// that are still empty then (D-712, gate 3's CONC-W3-1), so the
    /// session publishes a repoint only once every constant the new code
    /// reads is filled ([`Jit::pending_repoints`]).
    ///
    /// # Errors
    ///
    /// As [`Jit::compile`]; a failed batch leaves nothing pending.
    pub fn compile_unpublished(&mut self, mirs: &[MirModule]) -> Result<(), JitError> {
        let decls = self.decls.clone();
        let result = self.compile_batch(mirs, &decls);
        if result.is_err() {
            // Nothing of the batch was defined in the module (`define`
            // compiles everything first), so restoring the table undoes
            // it; its declarations stay in the module, unused.
            self.decls = decls;
            self.pending.clear();
        }
        result
    }

    /// Compiles the batch `mirs`; `before` is the declaration table as it
    /// was before the batch, which tells the layouts this batch declares.
    fn compile_batch(&mut self, mirs: &[MirModule], before: &Decls) -> Result<(), JitError> {
        // No layout id is known before registration: layout words and
        // constructor statics hold 0 until the records are registered
        // (D-454, D-920).
        let cx = CodegenCx {
            layouts: &BuiltinLayouts,
        };
        cheby_codegen_clif::declare(&mut *self.module, &mut self.decls, mirs, &cx, &self.codegen)?;
        self.check_symbols()?;
        let (clif, stats, defined) = cheby_codegen_clif::define_with_stats(
            &mut *self.module,
            mirs,
            &mut self.decls,
            &cx,
            &self.codegen,
        )?;
        // The batch's module records (D-919), defined last, so no id of
        // the code before them moves (D-264).
        let known_externals = &self.externals;
        let records = cheby_codegen_clif::define_batch_records(
            &mut *self.module,
            &self.decls,
            mirs,
            &defined,
            &BatchNew {
                layout: &|layout| !before.has_layout(layout),
                external: &|def| !known_externals.contains(&def),
            },
        )
        .map_err(|e| JitError::ice(format!("cannot define the batch's module records: {e}")))?;
        self.clif.extend(clif);
        self.module
            .finalize_definitions()
            .map_err(|e| JitError::Codegen(CodegenError::Module(e.to_string())))?;
        self.register_records(mirs, &records)?;
        self.take_repoints()?;
        self.callbacks = self.find_callbacks();
        self.code_stats = self.code_stats.add(stats);
        Ok(())
    }

    /// Registers the finalized batch's module records `records` with the
    /// runtime's one routine (D-919, D-1169): its
    /// user layouts, writing their layout words and constructor statics,
    /// then its external types, hooks and the call-site ranges of its
    /// functions under this JIT's code owner (D-867), all before any code
    /// of the batch runs, as an executable's start registers its records.
    /// Ranges of earlier batches stay, REPL redefinitions included, since
    /// their code stays while the JIT's memory lives (D-712). The entries
    /// unit has no record, so its thunks and trampolines are in no range
    /// (D-1111). A refused record registers nothing.
    fn register_records(
        &mut self,
        mirs: &[MirModule],
        records: &[BatchRecord],
    ) -> Result<(), JitError> {
        let mut batch = Vec::with_capacity(records.len());
        let mut pointers: Vec<&ModuleRecord> = Vec::with_capacity(records.len());
        for record in records {
            let (address, size) = self.module.get_finalized_data(record.data);
            let unit = mirs
                .get(record.unit)
                .ok_or_else(|| JitError::ice("a module record of no unit"))?;
            #[expect(
                clippy::cast_ptr_alignment,
                reason = "code generation defines every data object with alignment 8; checked below"
            )]
            let pointer = address.cast::<ModuleRecord>();
            if pointer.is_null()
                || !pointer.is_aligned()
                || size < std::mem::size_of::<ModuleRecord>()
            {
                return Err(JitError::ice("a module record without its bytes"));
            }
            // SAFETY: the record is a finalized, read-only, 8-aligned data
            // object of this JIT's module, at least as large as a
            // `ModuleRecord`, laid out as `cheby_runtime::image` documents
            // (`define_batch_records`); it stays mapped while the JIT's
            // memory lives, and nothing writes it.
            pointers.push(unsafe { &*pointer });
            batch.push(JitRecord {
                unit: unit.unit,
                path: unit.path.clone(),
                address: address.expose_provenance(),
            });
        }
        // SAFETY: every record and every table, word, static, function
        // value and function it points to lives in this JIT's memory,
        // which stays mapped for as long as anything registered may be
        // used: the code owner's ranges are unregistered before the memory
        // is freed (`CodeKeepAlive`), and the layouts' hooks are static
        // function values in this memory, called only on values of these
        // layouts, which only this JIT's code makes and which a runtime
        // keeps alive with its code token (D-708), as the JIT's own
        // registration of hooks did. The layout words and constructor statics are
        // writable data of this batch, which no code has run yet and
        // nothing else writes.
        unsafe { image::register_records(&pointers, self.keep_alive.code_owner) }.map_err(|e| {
            JitError::ice(format!("cannot register the batch's module records: {e}"))
        })?;
        for unit in mirs {
            self.externals
                .extend(unit.externals.iter().map(|external| external.def));
        }
        self.records.extend(batch);
        Ok(())
    }

    /// Records the slots that the batch's REPL redefinitions take over,
    /// with the words of their new code, as pending (D-043); the last
    /// batch's repoints can no longer be reverted.
    fn take_repoints(&mut self) -> Result<(), JitError> {
        self.repointed.clear();
        self.pending.clear();
        for (slot, func, def) in self.decls.take_repoints() {
            let code = self.module.get_finalized_function(func);
            let word = u64::try_from(code.expose_provenance())
                .map_err(|_| JitError::ice("an address does not fit in a word"))?;
            self.pending.push((slot, word, def));
        }
        Ok(())
    }

    /// The REPL redefinitions of the last batch whose slots still hold the
    /// old code ([`Jit::compile_unpublished`]), with those slots.
    #[must_use]
    pub fn pending_repoints(&self) -> Vec<(DefId, SlotHandle)> {
        self.pending
            .iter()
            .map(|&(slot, _, def)| (def, SlotHandle { data: slot }))
            .collect()
    }

    /// Points the pending slots among `slots` at their new code, with
    /// release stores (D-712), remembering what they held
    /// ([`Jit::revert_repoints`]). Slots that are not pending are ignored.
    ///
    /// # Errors
    ///
    /// An ICE if a slot's word cannot be written.
    pub fn publish_repoints(&mut self, slots: &[SlotHandle]) -> Result<(), JitError> {
        let (now, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|&(slot, _, _)| slots.contains(&SlotHandle { data: slot }));
        self.pending = rest;
        for (slot, word, _) in now {
            let old = self.write_slot(slot, word)?;
            self.repointed.push((slot, old));
        }
        Ok(())
    }

    /// Points every pending slot at its new code ([`Jit::publish_repoints`]).
    ///
    /// # Errors
    ///
    /// An ICE if a slot's word cannot be written.
    pub fn publish_all_repoints(&mut self) -> Result<(), JitError> {
        let slots: Vec<SlotHandle> = self
            .pending
            .iter()
            .map(|&(slot, _, _)| SlotHandle { data: slot })
            .collect();
        self.publish_repoints(&slots)
    }

    /// The slot the top-level function `def`'s code was last put in, whatever
    /// its signature: the slot its callers load (D-043, D-456).
    #[must_use]
    pub fn slot_of(&self, def: DefId) -> Option<SlotHandle> {
        self.decls
            .slot_of(def)
            .map(|decl| SlotHandle { data: decl.id })
    }

    /// Writes `word` into the slot `slot` with a release store and returns
    /// what it held (D-712).
    ///
    /// Fibers of a REPL session's runtime may run this JIT's code while it
    /// compiles (detached fibers, D-194), and their calls load the slot
    /// with a plain aligned 8-byte load. Such a load is single-copy atomic
    /// on x86-64 and aarch64, so a caller sees the old word or the new one,
    /// never a mix, and calls the old code or the new; both stay valid,
    /// since code is never freed while the `Jit` lives (D-625, D-626), and
    /// the REPL publishes new code only once the constants it reads are
    /// filled ([`Jit::compile_unpublished`]). The
    /// release orders the new code's bytes, finalized and made visible to
    /// every thread's instruction stream before this store (`finalize`'s
    /// pipeline flush, `memory.rs`), and the data it reads before the word
    /// that leads to it.
    fn write_slot(&mut self, slot: cranelift_module::DataId, word: u64) -> Result<u64, JitError> {
        let (address, size) = self.module.get_finalized_data(slot);
        if address.is_null() || size < 8 {
            return Err(JitError::ice("a slot without its word"));
        }
        #[expect(
            clippy::cast_ptr_alignment,
            reason = "code generation defines every data object with alignment 8"
        )]
        let pointer = address.cast_mut().cast::<u64>();
        // SAFETY: the slot is an 8-aligned, writable 8-byte data object of
        // this JIT's module (declared writable and finalized; a finalized
        // writable segment stays mapped read-write until the JIT's memory
        // is freed), valid for the reference's short life here. Rust code
        // touches it only through atomics of this JIT (`write_slot` under
        // `&mut self`, `slot_target`); generated code reads it with plain
        // aligned loads, which are single-copy atomic on the supported
        // targets (see above).
        let atomic = unsafe { AtomicU64::from_ptr(pointer) };
        // Only this JIT writes its slots, under `&mut self`: the word read
        // here is the last one stored.
        let old = atomic.load(Ordering::Relaxed);
        atomic.store(word, Ordering::Release);
        Ok(old)
    }

    /// Registers how a REPL session names items (D-547, E4): their symbols
    /// use `<repl>` and carry their generations, and a name whose
    /// [`SessionName::reuse`] is set takes over that definition's slot when
    /// its batch compiles. A new generation of an item that is already
    /// compiled forgets the old declarations of its functions, constructor
    /// layouts and constant slot, which the next batch declares anew
    /// ([`Decls::set_session_name`]).
    pub fn set_session_names(&mut self, names: impl IntoIterator<Item = (DefId, SessionName)>) {
        for (def, name) in names {
            // A new generation of an external type is registered anew,
            // as `Decls::set_session_name` forgets a constructor's layout
            // (D-1169).
            let previous = self.decls.session_name(def).map(|old| old.generation);
            let changed = previous.map_or(name.generation > 0, |old| old != name.generation);
            if changed {
                self.externals.remove(&def);
            }
            self.decls.set_session_name(def, name);
        }
    }

    /// Undoes the repointing of the last compiled batch: the REPL calls it
    /// when an entry whose code compiled is rejected afterwards, so that
    /// existing callers keep the old code (all or nothing). Repoints not
    /// published yet are dropped.
    ///
    /// # Errors
    ///
    /// An ICE if a slot's word cannot be written.
    pub fn revert_repoints(&mut self) -> Result<(), JitError> {
        self.pending.clear();
        for (slot, old) in std::mem::take(&mut self.repointed).into_iter().rev() {
            self.write_slot(slot, old)?;
        }
        Ok(())
    }

    /// The slot the top-level function `def` was last compiled into, if
    /// that slot has the ABI signature `sig` (D-456).
    #[must_use]
    pub fn slot(&self, def: DefId, sig: &AbiSig) -> Option<SlotHandle> {
        let decl = self.decls.slot_of(def)?;
        (self.decls.slot_sig(decl.id) == Some(sig)).then_some(SlotHandle { data: decl.id })
    }

    /// The address a slot holds now.
    #[must_use]
    pub fn slot_target(&self, slot: SlotHandle) -> Option<*const u8> {
        let (address, size) = self.module.get_finalized_data(slot.data);
        if address.is_null() || size < 8 {
            return None;
        }
        #[expect(
            clippy::cast_ptr_alignment,
            reason = "code generation defines every data object with alignment 8"
        )]
        let pointer = address.cast_mut().cast::<u64>();
        // SAFETY: a finalized, 8-aligned, writable data object of at least
        // 8 bytes of this JIT's module, valid for the reference's short
        // life here; Rust code writes it only with `write_slot`'s atomic
        // store (D-712).
        let word = unsafe { AtomicU64::from_ptr(pointer) }.load(Ordering::Acquire);
        Some(std::ptr::with_exposed_provenance(
            usize::try_from(word).ok()?,
        ))
    }

    /// Points `slot` at the compiled code of the top-level function `code`
    /// (D-043): every caller that loads the slot reaches it from now on.
    ///
    /// # Errors
    ///
    /// An ICE if `code` was not compiled or its ABI signature is not the
    /// slot's.
    pub fn repoint(&mut self, slot: SlotHandle, code: DefId) -> Result<(), JitError> {
        let decl = self.decls.func(FnKey::Item(code))?;
        if !decl.defined {
            return Err(JitError::ice(format!("{code:?} was not compiled")));
        }
        if self.decls.slot_sig(slot.data) != Some(&decl.abi) {
            return Err(JitError::ice(
                "a slot cannot point at code of another signature",
            ));
        }
        let address = self.module.get_finalized_function(decl.id);
        let word = u64::try_from(address.expose_provenance())
            .map_err(|_| JitError::ice("an address does not fit in a word"))?;
        self.write_slot(slot.data, word)?;
        Ok(())
    }

    /// Stores the values of REPL session values (a prompt-level `let`,
    /// D-547) in their constant slots `cb$const$…`, declaring and defining
    /// the slots no compiled unit declared. A value whose
    /// [`SessionName::reuse`] names an earlier session value goes in that
    /// value's slot, so code that reads it sees the new value. Each word is
    /// a reference the slot holds for as long as code can read it.
    ///
    /// # Errors
    ///
    /// An ICE if a value has no session name.
    pub fn bind_values(&mut self, values: &[(DefId, u64)]) -> Result<(), JitError> {
        for &(def, _) in values {
            self.decls.declare_session_const(&mut *self.module, def)?;
        }
        let cx = CodegenCx {
            layouts: &BuiltinLayouts,
        };
        cheby_codegen_clif::define_data(&mut *self.module, &mut self.decls, &cx)?;
        self.module
            .finalize_definitions()
            .map_err(|e| JitError::Codegen(CodegenError::Module(e.to_string())))?;
        for &(def, word) in values {
            self.set_const(def, word)?;
        }
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

    /// The accounting domain every entry of this JIT charges: one build,
    /// one domain (plan §4.3.4). Its live objects after a run are what the
    /// run leaked.
    #[must_use]
    pub fn domain(&self) -> Arc<Domain> {
        Arc::clone(&self.domain)
    }

    /// The trampolines `cb$tramp$0` to `cb$tramp$4` compiled so far
    /// (D-526), which every [`Entry`] passes to the runtime so kernels can
    /// call closures.
    #[must_use]
    pub const fn callbacks(&self) -> Callbacks {
        self.callbacks
    }

    fn trampoline(&self, arity: u32) -> Option<*const u8> {
        let decl = self.decls.func(FnKey::Trampoline(arity)).ok()?;
        if !decl.defined {
            return None;
        }
        let address = self.module.get_finalized_function(decl.id);
        (!address.is_null()).then_some(address)
    }

    fn find_callbacks(&self) -> Callbacks {
        let [t0, t1, t2, t3, t4] = [0, 1, 2, 3, 4].map(|n| self.trampoline(n));
        // SAFETY: a `FnKey::Trampoline(n)` function is declared with
        // `cheby_mir::trampoline_sig(n)`: the platform C convention, `ctx`,
        // the closure and `n` words, returning one word, which is exactly
        // `TrampN`. It is finalized, so executable, and the `Callbacks` are
        // used only by entries that borrow this JIT, so the code outlives
        // every call.
        unsafe {
            Callbacks {
                t0: t0.map(|a| std::mem::transmute::<*const u8, Tramp0>(a)),
                t1: t1.map(|a| std::mem::transmute::<*const u8, Tramp1>(a)),
                t2: t2.map(|a| std::mem::transmute::<*const u8, Tramp2>(a)),
                t3: t3.map(|a| std::mem::transmute::<*const u8, Tramp3>(a)),
                t4: t4.map(|a| std::mem::transmute::<*const u8, Tramp4>(a)),
            }
        }
    }

    /// The address of the slot `cb$const$…` of the constant `def` (E6), if
    /// a compiled batch declared it: 0 until the constant is evaluated.
    #[must_use]
    pub fn const_slot(&self, def: DefId) -> Option<*mut u64> {
        let id = self.decls.const_slot(def)?;
        let (address, size) = self.module.get_finalized_data(id);
        #[expect(
            clippy::cast_ptr_alignment,
            reason = "code generation defines every data object with alignment 8"
        )]
        let slot = address.cast_mut().cast::<u64>();
        (!address.is_null() && size >= 8).then_some(slot)
    }

    /// Stores `word` in the slot of the constant `def` with a release store:
    /// the constant's evaluated value, or a REPL session value (D-547),
    /// which code reads borrowed (E6). The slot holds the reference; a
    /// value stored there must be static (frozen, D-539) or stay alive for
    /// as long as code can read it, and a session value is marked shared
    /// first (D-705, D-712). Fibers of a REPL session may read a session
    /// value's slot while a later entry rebinds it (D-194): their plain
    /// aligned loads see the old word or the new one, and the release
    /// orders the new value's object before the word that leads to it.
    ///
    /// # Errors
    ///
    /// An ICE if no compiled batch declared the slot.
    pub fn set_const(&mut self, def: DefId, word: u64) -> Result<(), JitError> {
        let slot = self
            .const_slot(def)
            .ok_or_else(|| JitError::ice(format!("the constant {def:?} has no slot")))?;
        // SAFETY: the slot is an 8-aligned, writable 8-byte data object of
        // this JIT's module (declared writable and finalized; it stays
        // mapped read-write until the JIT's memory is freed), valid for the
        // reference's short life here. Generated code reads it with plain
        // aligned loads, single-copy atomic on x86-64 and aarch64. The
        // only other Rust writes to a slot are the constant evaluator's
        // lazy forcing (`cheby_runtime::consteval`), on the entry thread
        // of an evaluation that `&mut self` here does not overlap.
        unsafe { AtomicU64::from_ptr(slot) }.store(word, Ordering::Release);
        Ok(())
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
        Ok(Entry { thunk, jit: self })
    }

    /// Checks that every native symbol resolves, because Cranelift's JIT
    /// panics on a missing one when finalizing. `cheby check` cannot know
    /// which symbols the process will have, so the driver reports these
    /// only when running, as E0053 at the `@external` that names each
    /// symbol (D-455, D-532); with [`JitOptions::stub_unresolved`] they
    /// are bound to `cheby_rt_unresolved` instead.
    fn check_symbols(&self) -> Result<(), JitError> {
        let runtime = cheby_runtime::symbols();
        // Runtime objects later packages define (the builtin nominal
        // descriptors, W3-B): a missing one is a bug, not a program error.
        for symbol in self.decls.runtime_data_symbols() {
            if !runtime.iter().any(|(name, _)| *name == symbol) {
                return Err(JitError::ice(format!(
                    "the runtime does not define `{symbol}`"
                )));
            }
        }
        let mut table = self.symbols.lock().unwrap_or_else(PoisonError::into_inner);
        let missing: Vec<String> = self
            .decls
            .foreign_symbols()
            .into_iter()
            .filter(|symbol| {
                !runtime.iter().any(|(name, _)| name == symbol)
                    && !table.contains_key(*symbol)
                    && !process_defines(symbol)
            })
            .map(str::to_owned)
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        if !self.options.stub_unresolved {
            return Err(JitError::UnresolvedSymbols(missing));
        }
        let stub = runtime
            .iter()
            .find(|(name, _)| *name == sym::UNRESOLVED)
            .map(|&(_, address)| address)
            .ok_or_else(|| JitError::ice("the runtime does not define `cheby_rt_unresolved`"))?;
        for symbol in missing {
            table.insert(symbol.into(), stub.expose_provenance());
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
        // again by the `Jit`.
        let module = unsafe { ManuallyDrop::take(&mut self.module) };
        // The memory is freed when the last holder of the keep-alive goes:
        // this `Jit`, right below, or the last `CodeToken` a runtime holds
        // until its fibers have stopped (D-708, I-6).
        *self
            .keep_alive
            .module
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(module);
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
    jit: &'jit Jit,
}

impl fmt::Debug for Entry<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry").finish_non_exhaustive()
    }
}

impl Entry<'_> {
    /// The entry's thunk, for a host that calls it on an entry it already
    /// runs: constant evaluation registers each constant's thunk for lazy
    /// forcing (D-535). It is valid only while the [`Jit`] lives.
    #[must_use]
    pub fn thunk(&self) -> EntryFn {
        self.thunk
    }

    /// Runs the entry on its own stack and reports how it ended, with the
    /// JIT's trampolines, accounting domain and [`Jit::unwind_edges`] in
    /// place of `config`'s. A
    /// returned word may point into the `Jit`'s data (a static string or
    /// constructor) and is valid only while the `Jit` lives.
    ///
    /// # Errors
    ///
    /// [`JitError::Runtime`] if the entry cannot run.
    pub fn run(self, config: &EntryConfig) -> Result<EntryOutcome, JitError> {
        let config = self.config(config);
        // SAFETY: `thunk` is an entry thunk with the `EntryFn` ABI built by
        // the JIT (see `Jit::entry`), and the borrow of the `Jit` keeps its
        // code and data alive until this returns.
        unsafe { cheby_runtime::run_entry(self.thunk, &config) }.map_err(JitError::Runtime)
    }

    /// Runs the entry as a root fiber of `runtime` (D-690) and reports how
    /// it ended, with the JIT's trampolines, accounting domain and
    /// [`Jit::unwind_edges`] in place of `config`'s. The runtime keeps the
    /// JIT's code alive until it stops ([`Jit::code_token`]). A returned
    /// word is valid as for [`Entry::run`].
    ///
    /// # Errors
    ///
    /// [`JitError::Runtime`] if the entry cannot run.
    pub fn run_root(
        self,
        runtime: &Runtime,
        config: &EntryConfig,
    ) -> Result<EntryOutcome, JitError> {
        let config = self.config(config);
        runtime.attach(self.jit.code_token());
        // SAFETY: `thunk` is an entry thunk with the `EntryFn` ABI built by
        // the JIT (see `Jit::entry`), and `runtime` holds the JIT's code
        // token, so its code and data stay alive while the runtime's
        // fibers may run it.
        unsafe { runtime.run_root(self.thunk, &config) }.map_err(JitError::Runtime)
    }

    /// [`Entry::run_root`] in `group`: the detached fibers that the entry
    /// or its fibers spawn join `group`, which the host tears down when it
    /// is done with them ([`Runtime::teardown`]), as `cheby test` does
    /// after each test (D-723, D-782). The runtime keeps the JIT's code
    /// alive until it stops, as for [`Entry::run_root`], so a detached
    /// fiber that outlives the entry may still run it after the `Jit` is
    /// dropped.
    ///
    /// # Errors
    ///
    /// [`JitError::Runtime`] if the entry cannot run.
    pub fn run_root_in(
        self,
        runtime: &Runtime,
        config: &EntryConfig,
        group: &Arc<DetachedGroup>,
    ) -> Result<EntryOutcome, JitError> {
        let config = self.config(config);
        runtime.attach(self.jit.code_token());
        // SAFETY: as in `run_root`: an entry thunk with the `EntryFn` ABI
        // built by the JIT, whose code and data the runtime keeps alive
        // through the token attached above while its fibers, detached ones
        // in `group` included, may run them.
        unsafe { runtime.run_root_in(self.thunk, &config, group) }.map_err(JitError::Runtime)
    }

    /// [`Entry::run_root_in`] without waiting ([`Runtime::start_root_in`]),
    /// keeping `config.domain`: the root and every fiber it starts charge
    /// the caller's domain (a per-test domain, D-691, D-723), while the
    /// JIT's trampolines and [`Jit::unwind_edges`] replace `config`'s as in
    /// [`Entry::run_root_in`]. The runtime keeps the JIT's code alive until
    /// it stops, so the root may outlive this call and the borrow of the
    /// `Jit`; a returned word is valid only while the `Jit` lives.
    ///
    /// # Errors
    ///
    /// [`JitError::Runtime`] if the root cannot start.
    pub fn start_root_in(
        self,
        runtime: &Runtime,
        config: &EntryConfig,
        group: &Arc<DetachedGroup>,
    ) -> Result<RootHandle, JitError> {
        let config = self.config_keeping_domain(config);
        runtime.attach(self.jit.code_token());
        // SAFETY: as in `run_root_in`: an entry thunk with the `EntryFn`
        // ABI built by the JIT, whose code and data the runtime keeps alive
        // through the token attached above for as long as the root and its
        // fibers may run them, after this returns included.
        unsafe { runtime.start_root_in(self.thunk, &config, group) }.map_err(JitError::Runtime)
    }

    /// [`Entry::start_root_in`] reporting on `done` with `tag`
    /// ([`Runtime::start_root_notify`]): one host thread waits for any of
    /// several roots on one receiver.
    ///
    /// # Errors
    ///
    /// [`JitError::Runtime`] if the root cannot start; nothing is sent on
    /// `done` then.
    pub fn start_root_notify(
        self,
        runtime: &Runtime,
        config: &EntryConfig,
        group: &Arc<DetachedGroup>,
        tag: usize,
        done: RootDone,
    ) -> Result<(), JitError> {
        let config = self.config_keeping_domain(config);
        runtime.attach(self.jit.code_token());
        // SAFETY: as in `start_root_in`.
        unsafe { runtime.start_root_notify(self.thunk, &config, group, tag, done) }
            .map_err(JitError::Runtime)
    }

    /// `config` with the JIT's trampolines, domain and `unwind_edges`.
    fn config(&self, config: &EntryConfig) -> EntryConfig {
        EntryConfig {
            domain: self.jit.domain(),
            ..self.config_keeping_domain(config)
        }
    }

    /// `config` with the JIT's trampolines and `unwind_edges`, and its own
    /// domain.
    fn config_keeping_domain(&self, config: &EntryConfig) -> EntryConfig {
        EntryConfig {
            callbacks: self.jit.callbacks(),
            unwind_edges: self.jit.unwind_edges(),
            ..config.clone()
        }
    }

    /// Runs `main`'s entry thunk under `cheby run`'s policy (§10.10,
    /// D-182), with the JIT's trampolines, accounting domain and
    /// [`Jit::unwind_edges`] in place of `config`'s, and returns the exit
    /// status: 0 or 1
    /// from the program, 101 after a panic, 102 in debug builds when
    /// objects of the domain leak (D-458). `main` runs as the root fiber of
    /// a runtime made for the run (D-690).
    ///
    /// # Errors
    ///
    /// [`JitError::Runtime`] if the runtime cannot start (an invalid
    /// `CHEBY_THREADS`, D-658) or the entry cannot run.
    pub fn run_main(self, config: RunConfig) -> Result<i32, JitError> {
        let config = RunConfig {
            callbacks: self.jit.callbacks(),
            domain: self.jit.domain(),
            unwind_edges: self.jit.unwind_edges(),
            code: Some(self.jit.code_token()),
            ..config
        };
        // SAFETY: as in `run`; and the runtime `run_main` makes for the run
        // holds the JIT's code token, so the `Jit` may be dropped once it
        // returns: it tears down `main`'s detached fibers (cancelled and
        // unwound until their group is empty, D-710), or under
        // `AtExit::Freeze` freezes them at a safepoint, and after a
        // termination completes the halt (D-073, D-708, D-709), and it
        // joins every thread of the runtime, releasing the token after the
        // joins. The one exception is a worker whose fiber is blocked
        // inline in a system call (D-786): it is never joined, and the
        // runtime then keeps (leaks) the token, so the code that worker
        // returns into stays mapped.
        unsafe { cheby_runtime::run_main(self.thunk, config) }.map_err(JitError::Runtime)
    }
}
