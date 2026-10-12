//! The JIT (E1-E5): a Cranelift `JITModule` that compiles
//! batches of MIR units, the runtime's symbol registry, the indirection
//! table of top-level functions (D-043), and the way into generated code.
//!
//! - [`Jit::compile`] is incremental: each batch registers its layouts,
//!   declares its functions (extending the persistent declaration table),
//!   defines them and finalizes. Later batches call earlier code through
//!   the existing slots, and use the static objects, layout words and
//!   constant slots earlier batches defined (D-523). `cheby run` compiles
//!   one batch; constant evaluation (E6) and the REPL (G5) compile one per
//!   step.
//! - Every call of a top-level function loads the callee's slot, a
//!   writable data object holding its address (D-043). The REPL (E4)
//!   registers its items' session names first ([`Jit::set_session_names`]):
//!   their symbols carry `$r<generation>` (D-547), and a redefinition that
//!   takes over an earlier definition repoints that slot once its batch is
//!   finalized, so existing callers reach the new code
//!   ([`Jit::revert_repoints`] undoes it for a rejected entry); one with
//!   another signature gets a new slot (D-456). [`Jit::slot`] and
//!   [`Jit::repoint`] do the same by hand, and [`Jit::bind_values`] stores
//!   the session values of prompt-level `let`s in constant slots.
//! - The host never calls `tail`-convention code: it calls C-ABI entry
//!   thunks ([`Jit::entry`]), which call the Cheby functions (D-453). An
//!   [`Entry`] borrows its [`Jit`], so safe code cannot run an entry after
//!   its code is freed; [`Entry::run_main`] runs `main`'s thunk with the
//!   exit-status policy of §10.10. Every run gets the JIT's trampolines
//!   ([`Jit::callbacks`], D-526) and its accounting domain ([`Jit::domain`]).
//! - Code and data live in one reservation of 1 GiB of address space per
//!   `Jit`, committed segment by segment as batches need it (see
//!   `memory.rs`). Dropping a [`Jit`] frees it.
//! - Runtime symbols resolve through `cheby_runtime::symbols()`; other
//!   native symbols of `@external(native, …)` through the symbols defined
//!   with [`Jit::define_symbol`] and then the process's dynamic symbol
//!   table, checked before finalizing so a missing symbol is an error,
//!   never a panic (D-532). Foreign symbols are imported by address, with
//!   no signature, and called indirectly with each call site's C
//!   signature, so batches may bind one symbol with different signatures.

use std::fmt;
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex, PoisonError};

use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::Module;
use rustc_hash::FxHashMap;

use cheby_codegen_clif::{
    CodegenCx, CodegenError, CodegenOptions, Decls, DefineStats, LayoutIds, SessionName,
    builtin_layout_id,
};
use cheby_iface::{DefId, Ice};
use cheby_mir::{AbiSig, EntryKind, FnKey, LayoutDesc, LayoutRef, MirModule, Repr};
use cheby_runtime::abi::sym;
use cheby_runtime::alloc::Domain;
use cheby_runtime::entry::{Tramp0, Tramp1, Tramp2, Tramp3, Tramp4};
use cheby_runtime::layout::{self as rt_layout, FieldKind, LayoutInfo};
use cheby_runtime::{Callbacks, EntryConfig, EntryFn, EntryOutcome, RunConfig, RuntimeError};

mod memory;
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
    let info = LayoutInfo::ctor(&desc.name, desc.tag, &fields).named(
        &desc.type_name,
        &desc.ctor_name,
        desc.record,
        desc.type_key,
    );
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

/// Symbols the JIT resolves itself, before the process: those defined
/// with [`Jit::define_symbol`] and, with [`JitOptions::stub_unresolved`],
/// the stubbed ones. Addresses are kept as integers so the table is
/// `Send` for Cranelift's lookup function.
type SymbolTable = Arc<Mutex<FxHashMap<Box<str>, usize>>>;

/// A JIT session: the module, the declarations of every batch so far, and
/// the registered layouts. Dropping it frees its code and data.
pub struct Jit {
    /// Taken only by `Drop`, which frees its memory.
    module: ManuallyDrop<JITModule>,
    decls: Decls,
    layouts: JitLayouts,
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
    /// The code every batch that compiled defined ([`Jit::code_stats`]).
    code_stats: DefineStats,
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
            .field("layouts", &self.layouts)
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
        let flags = cheby_codegen_clif::isa::flags()?;
        let isa = cranelift_native::builder()
            .map_err(|message| JitError::Host(message.to_owned()))?
            .finish(flags)
            .map_err(|e| JitError::Host(e.to_string()))?;
        let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
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
            decls,
            layouts: JitLayouts::default(),
            codegen: options,
            options: jit,
            clif: Vec::new(),
            memory: usage,
            symbols,
            domain: Domain::new(),
            callbacks: Callbacks::default(),
            repointed: Vec::new(),
            code_stats: DefineStats::default(),
        })
    }

    /// The machine code defined so far: its bytes and its functions, summed
    /// over every batch that compiled (D-718, `cargo xtask bench-compile`).
    #[must_use]
    pub const fn code_stats(&self) -> DefineStats {
        self.code_stats
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

    /// Compiles a batch of units, in order: registers their layouts,
    /// declares and defines their functions and data, and finalizes. Units
    /// may call functions of earlier batches.
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
        cheby_codegen_clif::declare(&mut *self.module, &mut self.decls, mirs, &cx, &self.codegen)?;
        self.check_symbols()?;
        let (clif, stats) = cheby_codegen_clif::define_with_stats(
            &mut *self.module,
            mirs,
            &mut self.decls,
            &cx,
            &self.codegen,
        )?;
        self.clif.extend(clif);
        self.module
            .finalize_definitions()
            .map_err(|e| JitError::Codegen(CodegenError::Module(e.to_string())))?;
        self.apply_repoints()?;
        self.callbacks = self.find_callbacks();
        self.register_hooks(mirs)?;
        self.code_stats = self.code_stats.add(stats);
        Ok(())
    }

    /// Points the slots that the batch's REPL redefinitions take over at
    /// their new code (D-043), remembering what they held.
    fn apply_repoints(&mut self) -> Result<(), JitError> {
        self.repointed.clear();
        for (slot, func) in self.decls.take_repoints() {
            let code = self.module.get_finalized_function(func);
            let word = u64::try_from(code.expose_provenance())
                .map_err(|_| JitError::ice("an address does not fit in a word"))?;
            let old = self.write_slot(slot, word)?;
            self.repointed.push((slot, old));
        }
        Ok(())
    }

    /// Writes `word` into the slot `slot` and returns what it held.
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
        // this JIT's module (declared writable and finalized), and `&mut
        // self` keeps any entry of this JIT from running meanwhile.
        let old = unsafe { pointer.replace(word) };
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
            self.decls.set_session_name(def, name);
            let layout = LayoutRef::Ctor(def);
            if !self.decls.has_layout(layout) {
                self.layouts.ids.remove(&layout);
            }
        }
    }

    /// Undoes the repointing of the last compiled batch: the REPL calls it
    /// when an entry whose code compiled is rejected afterwards, so that
    /// existing callers keep the old code (all or nothing).
    ///
    /// # Errors
    ///
    /// An ICE if a slot's word cannot be written.
    pub fn revert_repoints(&mut self) -> Result<(), JitError> {
        for (slot, old) in std::mem::take(&mut self.repointed) {
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
        let pointer = address.cast::<u64>();
        // SAFETY: a finalized, 8-aligned data object of at least 8 bytes.
        let word = unsafe { pointer.read() };
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
            layouts: &self.layouts,
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

    /// Registers the `@std_hooks(equal, hash, debug)` functions of the
    /// batch's types with the runtime's layout table, for every
    /// constructor of each such type (D-482): the words are the hooks'
    /// static function values `cb$fnval$…`, which the runtime calls through
    /// the trampolines. A type whose hooks have no function value (no value
    /// shim was lowered) keeps structural equality and printing.
    fn register_hooks(&self, mirs: &[MirModule]) -> Result<(), JitError> {
        for unit in mirs {
            for desc in &unit.layouts {
                let Some(hooks) = desc.hooks else {
                    continue;
                };
                let mut words = [0_u64; 3];
                let mut complete = true;
                for (word, def) in words.iter_mut().zip(hooks) {
                    let Some(id) = self.decls.fn_value(unit, def)? else {
                        complete = false;
                        break;
                    };
                    let (address, _) = self.module.get_finalized_data(id);
                    *word = u64::try_from(address.expose_provenance())
                        .map_err(|_| JitError::ice("an address does not fit in a word"))?;
                }
                if !complete {
                    continue;
                }
                let id = self.layouts.id(&desc.layout)?;
                rt_layout::set_hooks(id, words).map_err(|e| {
                    JitError::ice(format!("cannot register the hooks of `{}`: {e}", desc.name))
                })?;
            }
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

    /// Stores `word` in the slot of the constant `def`: the constant's
    /// evaluated value, which code reads borrowed (E6). The slot holds the
    /// reference; a value stored there must be static (frozen, D-539) or
    /// stay alive for as long as code can read it.
    ///
    /// # Errors
    ///
    /// An ICE if no compiled batch declared the slot.
    pub fn set_const(&mut self, def: DefId, word: u64) -> Result<(), JitError> {
        let slot = self
            .const_slot(def)
            .ok_or_else(|| JitError::ice(format!("the constant {def:?} has no slot")))?;
        // SAFETY: the slot is an 8-aligned, writable 8-byte data object of
        // this JIT's module (declared writable and finalized), and `&mut
        // self` keeps any entry of this JIT from running meanwhile.
        unsafe { slot.write(word) };
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

    /// Registers the constructor layouts of `mirs`, units in order and
    /// types in `DefIndex` order of their first constructor, so ids do not
    /// depend on scheduling. The constructors of one type get consecutive
    /// ids in tag order (D-522), so a tag is a layout id minus the first
    /// constructor's.
    ///
    /// The ids come from the runtime's process-wide table, so they also
    /// depend on what earlier `Jit`s in the process registered. Generated
    /// code therefore never embeds one (D-264): it loads the header from
    /// the layout's `cb$layout$…` data symbol, which code generation fills
    /// from these ids (D-454).
    fn register_layouts(&mut self, mirs: &[MirModule]) -> Result<(), JitError> {
        for unit in mirs {
            let mut types: Vec<DefId> = Vec::new();
            for desc in &unit.layouts {
                if !types.contains(&desc.type_def) {
                    types.push(desc.type_def);
                }
            }
            for ty in types {
                let mut ctors: Vec<&LayoutDesc> =
                    unit.layouts.iter().filter(|d| d.type_def == ty).collect();
                ctors.sort_by_key(|d| d.tag);
                let known = ctors
                    .iter()
                    .filter(|d| self.layouts.ids.contains_key(&d.layout))
                    .count();
                if known == ctors.len() {
                    continue;
                }
                if known != 0 {
                    return Err(JitError::ice(format!(
                        "some constructors of the type {ty:?} are registered and some are not"
                    )));
                }
                let in_order = ctors
                    .iter()
                    .enumerate()
                    .all(|(n, d)| usize::try_from(d.tag).is_ok_and(|tag| tag == n));
                if !in_order {
                    return Err(JitError::ice(format!(
                        "the constructors of the type {ty:?} do not have the tags 0, 1, …"
                    )));
                }
                let infos = ctors
                    .iter()
                    .map(|d| layout_info(d))
                    .collect::<Result<Vec<_>, _>>()?;
                let first = rt_layout::register_consecutive(infos)
                    .map_err(|e| JitError::ice(format!("cannot register a layout: {e}")))?;
                for (desc, id) in ctors.iter().zip(first..) {
                    self.layouts.ids.insert(desc.layout, id);
                }
            }
        }
        Ok(())
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
    /// JIT's trampolines and accounting domain in place of `config`'s. A
    /// returned word may point into the `Jit`'s data (a static string or
    /// constructor) and is valid only while the `Jit` lives.
    ///
    /// # Errors
    ///
    /// [`JitError::Runtime`] if the entry cannot run.
    pub fn run(self, config: &EntryConfig) -> Result<EntryOutcome, JitError> {
        let config = EntryConfig {
            callbacks: self.jit.callbacks(),
            domain: self.jit.domain(),
            ..config.clone()
        };
        // SAFETY: `thunk` is an entry thunk with the `EntryFn` ABI built by
        // the JIT (see `Jit::entry`), and the borrow of the `Jit` keeps its
        // code and data alive until this returns.
        unsafe { cheby_runtime::run_entry(self.thunk, &config) }.map_err(JitError::Runtime)
    }

    /// Runs `main`'s entry thunk under `cheby run`'s policy (§10.10,
    /// D-182), with the JIT's trampolines and accounting domain in place
    /// of `config`'s, and returns the exit status: 0 or 1 from the program,
    /// 101 after a panic, 102 in debug builds when objects of the domain
    /// leak (D-458).
    ///
    /// # Errors
    ///
    /// [`JitError::Runtime`] if the entry cannot run.
    pub fn run_main(self, config: RunConfig) -> Result<i32, JitError> {
        let config = RunConfig {
            callbacks: self.jit.callbacks(),
            domain: self.jit.domain(),
            ..config
        };
        // SAFETY: as in `run`.
        unsafe { cheby_runtime::run_main(self.thunk, config) }.map_err(JitError::Runtime)
    }
}
