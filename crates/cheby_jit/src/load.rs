//! Loading code records into a [`Jit`] (K4c: D-914, D-922, D-1169;
//! `cache.md` §7.3; D-1219).
//!
//! Every JIT host compiles each unit it needs alone into a code record
//! (`cheby_codegen_clif::jit_unit`) or takes one from the store, and loads
//! the batch's records here, in build order:
//!
//! 1. Before anything is declared, the batch is checked: no global symbol
//!    is defined twice, in the batch or by an earlier batch; every
//!    generated symbol (`cb$…`) a record imports is defined by the batch or
//!    an earlier one, or the batch is an internal error; and every runtime
//!    and foreign symbol resolves in the JIT's own symbols, the runtime's,
//!    the package libraries of the name's generation, then the process
//!    (D-455, D-941, D-1197). A missing one fails the batch with
//!    [`JitError::UnresolvedSymbols`], or with
//!    [`crate::JitOptions::stub_unresolved`] binds to `cheby_rt_unresolved`
//!    (D-943). A batch that fails here changes nothing.
//! 2. Every global symbol of a record (linkage `Hidden` or `Export`) and
//!    every import is declared by name, and each record's local symbols
//!    anonymously, so copies of one shim or static in two records never
//!    clash (D-918). Each record's import words are local, so each record
//!    has its own (D-1105 becomes one set per record), and each record's
//!    module record `cb$mod$…` is declared anonymously too, as D-1169's
//!    batch records were, so a REPL batch that loads a second record of one
//!    module path never declares a name twice. Functions are declared with
//!    one signature: nothing in a JIT calls through a declaration's
//!    signature, and every declaration of a name must agree.
//! 3. Functions are defined with `define_function_bytes` and data with
//!    `define_data`, then the module is finalized.
//! 4. The records' module records are registered with
//!    `image::register_records` under the JIT's code owner, in build order,
//!    before any code of the batch runs (D-1169).
//!
//! The JIT then finds what its hosts look up by key (`Jit::entry`,
//! `Jit::const_slot`, the REPL's slots and repoints) through each fresh
//! unit's index; a record from the store has none, and a host names its
//! constants by symbol ([`Jit::name_consts`]). A loaded record's constants
//! table (D-1189) is never read here.

use cranelift_codegen::ir;
use cranelift_module::{
    DataDescription, DataId, FuncId, FuncOrDataId, Linkage, Module, ModuleReloc, ModuleRelocTarget,
};
use rustc_hash::{FxHashMap, FxHashSet};

use cheby_codegen_clif::jit_unit::{Seed, UnitIndex};
use cheby_codegen_clif::record::{CodeRecord, RecordContents, SymbolKind, SymbolLinkage};
use cheby_iface::DefId;
use cheby_mir::{AbiSig, FnKey, UnitKind};
use cheby_runtime::abi::sym;
use cheby_runtime::image::{self, ModuleRecord};

use crate::{Jit, JitError, JitRecord, resolve_generation};

/// The prefix of every generated symbol (D-456).
const GENERATED: &str = cheby_codegen_clif::mangle::PREFIX;

/// The prefix of a module record's symbol (D-1107).
const MODULE_RECORD: &str = "cb$mod$";

/// The prefix of a constant slot's symbol (E6).
const CONST_SLOT: &str = "cb$const$";

/// One record of a batch to load.
pub(crate) struct Item<'a> {
    /// The record.
    pub(crate) record: &'a CodeRecord,
    /// Where its functions and data are, for a unit compiled fresh.
    pub(crate) index: Option<&'a UnitIndex>,
    /// The unit's kind.
    pub(crate) unit: UnitKind,
    /// The unit's module path.
    pub(crate) path: &'a str,
}

/// What a [`Jit`]'s loader keeps across its batches.
#[derive(Default)]
pub(crate) struct Loaded {
    /// The functions of fresh units, by key: entry thunks, items, tests.
    pub(crate) funcs: FxHashMap<FnKey, FuncId>,
    /// The constant slots of fresh units, by constant.
    pub(crate) consts: FxHashMap<DefId, DataId>,
    /// The symbols of constant slots a host named ([`Jit::name_consts`]).
    pub(crate) const_names: FxHashMap<DefId, String>,
    /// The slot each top-level function of a fresh unit was put in.
    pub(crate) slots: FxHashMap<DefId, DataId>,
    /// The ABI signature of each such slot (D-456).
    pub(crate) slot_sigs: FxHashMap<DataId, AbiSig>,
    /// Every global symbol defined so far, by name.
    pub(crate) defined: FxHashSet<String>,
    /// What a later batch's units are told (`jit_unit::Seed`).
    pub(crate) seed: Seed,
    /// The trampolines `cb$tramp$0` to `cb$tramp$4` (D-526), the latest
    /// batch's that defined each.
    pub(crate) trampolines: [Option<FuncId>; 5],
}

impl Loaded {
    /// Forgets what a new generation of the REPL item `def` replaces, as
    /// `Decls::set_session_name` does (D-547): its functions, its layout
    /// and, unless it keeps it (`keeps_const`, D-547), its constant slot;
    /// its external type is registered anew (D-1169). Slots stay, keyed
    /// by path and signature.
    pub(crate) fn forget(&mut self, def: DefId, keeps_const: bool) {
        self.funcs.retain(|key, _| owner_of(*key) != Some(def));
        self.seed.layouts.remove(&cheby_mir::LayoutRef::Ctor(def));
        self.seed.externals.remove(&def);
        if !keeps_const {
            self.consts.remove(&def);
            self.seed.consts.remove(&def);
        }
    }
}

/// The item whose redefinition replaces the function `key`, as
/// `cheby_codegen_clif`'s declarations have it.
const fn owner_of(key: FnKey) -> Option<DefId> {
    match key {
        FnKey::Item(def)
        | FnKey::ValueShim(def)
        | FnKey::ConstInit(def)
        | FnKey::Closure { owner: def, .. }
        | FnKey::Continuation { owner: def, .. }
        | FnKey::Entry(cheby_mir::EntryKind::Const(def)) => Some(def),
        _ => None,
    }
}

/// Whether `symbol` is a definition by name: global, and not a module
/// record, which is declared anonymously.
fn global_definition(name: &str, linkage: SymbolLinkage) -> bool {
    matches!(linkage, SymbolLinkage::Hidden | SymbolLinkage::Export)
        && !name.starts_with(MODULE_RECORD)
}

/// The relocation target of a declared symbol.
fn target(id: FuncOrDataId) -> ModuleRelocTarget {
    match id {
        FuncOrDataId::Func(func) => ModuleRelocTarget::from(func),
        FuncOrDataId::Data(data) => ModuleRelocTarget::from(data),
    }
}

/// The declaration of the symbol `index` among `ids`.
fn id_at(ids: &[FuncOrDataId], index: u32) -> Result<FuncOrDataId, JitError> {
    usize::try_from(index)
        .ok()
        .and_then(|i| ids.get(i))
        .copied()
        .ok_or_else(|| JitError::ice("a code record names a symbol it does not declare"))
}

impl Jit {
    /// Loads `items` as one batch (module docs).
    ///
    /// # Errors
    ///
    /// [`JitError::UnresolvedSymbols`] for runtime or foreign symbols
    /// nothing defines (without stubs), an internal error for a generated
    /// symbol nothing defines, a global defined twice, or a record the
    /// module refuses.
    pub(crate) fn load_items(&mut self, items: &[Item<'_>]) -> Result<(), JitError> {
        let slots = self.check_items(items)?;
        self.define_const_slots(&slots)?;
        let signature = ir::Signature::new(self.module.isa().default_call_conv());
        let mut ids = Vec::with_capacity(items.len());
        for item in items {
            ids.push(self.declare_record(item.record, &signature)?);
        }
        for (item, ids) in items.iter().zip(&ids) {
            self.define_record(item.record, ids)?;
        }
        self.module
            .finalize_definitions()
            .map_err(|e| JitError::ice(format!("cannot finalize the batch: {e}")))?;
        self.register_loaded(items, &ids)?;
        self.note_loaded(items, &ids)
    }

    /// Step 1 of the module docs: duplicates, missing generated symbols,
    /// and the runtime and foreign symbols nothing defines. Returns the
    /// constant slots `cb$const$…` the batch reads that no loaded record
    /// defines, which the JIT defines itself, 0 (E6), as one JIT's units
    /// did before records (D-1219).
    fn check_items(&self, items: &[Item<'_>]) -> Result<Vec<String>, JitError> {
        let mut batch: FxHashSet<&str> = FxHashSet::default();
        for item in items {
            for symbol in &item.record.symbols {
                if global_definition(&symbol.name, symbol.linkage)
                    && (self.loaded.defined.contains(&symbol.name) || !batch.insert(&symbol.name))
                {
                    return Err(JitError::ice(format!(
                        "the symbol `{}` is defined twice in one JIT",
                        symbol.name
                    )));
                }
            }
        }
        let mut foreign: Vec<&str> = Vec::new();
        let mut slots: Vec<String> = Vec::new();
        for item in items {
            for symbol in &item.record.symbols {
                if symbol.linkage != SymbolLinkage::Import {
                    continue;
                }
                let name = symbol.name.as_str();
                if !name.starts_with(GENERATED) {
                    foreign.push(name);
                    continue;
                }
                if batch.contains(name) || self.loaded.defined.contains(name) {
                    continue;
                }
                if name.starts_with(CONST_SLOT)
                    && symbol.kind == (SymbolKind::Data { writable: true })
                {
                    slots.push(name.to_owned());
                    continue;
                }
                return Err(JitError::ice(format!(
                    "the generated symbol `{name}` is defined by no loaded record"
                )));
            }
        }
        self.resolve_foreign(foreign)?;
        slots.sort_unstable();
        slots.dedup();
        Ok(slots)
    }

    /// Checks that every runtime and foreign symbol among `names`
    /// resolves: in the JIT's own symbols, then the runtime's, the package
    /// libraries of the name's generation and the process (D-455, D-941,
    /// D-1197). The rest fail with [`JitError::UnresolvedSymbols`], all of
    /// them, sorted, or with stubs bind to `cheby_rt_unresolved` (D-532,
    /// D-943). A runtime object later packages define (a builtin nominal
    /// descriptor, W3-B) that the runtime lacks is a bug.
    ///
    /// # Errors
    ///
    /// As described.
    pub(crate) fn resolve_foreign<'n>(
        &self,
        names: impl IntoIterator<Item = &'n str>,
    ) -> Result<(), JitError> {
        let runtime = cheby_runtime::symbols();
        let mut missing: Vec<String> = Vec::new();
        {
            let table = self
                .symbols
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for name in names {
                if name.starts_with(sym::DESC_PREFIX) && !runtime.iter().any(|(n, _)| *n == name) {
                    return Err(JitError::ice(format!(
                        "the runtime does not define `{name}`"
                    )));
                }
                if !table.contains_key(name) && resolve_generation(name, &self.libraries).is_none()
                {
                    missing.push(name.to_owned());
                }
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        missing.sort_unstable();
        missing.dedup();
        if !self.options.stub_unresolved {
            return Err(JitError::UnresolvedSymbols(missing));
        }
        let stub = runtime
            .iter()
            .find(|(name, _)| *name == sym::UNRESOLVED)
            .map(|&(_, address)| address)
            .ok_or_else(|| JitError::ice("the runtime does not define `cheby_rt_unresolved`"))?;
        let mut table = self
            .symbols
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for symbol in missing {
            table.insert(symbol.into(), stub.expose_provenance());
        }
        Ok(())
    }

    /// Declares and defines each of `slots`, a constant slot no record
    /// defines, by name: one writable word, 0 until the constant is
    /// evaluated or bound (E6).
    fn define_const_slots(&mut self, slots: &[String]) -> Result<(), JitError> {
        for name in slots {
            let id = self
                .module
                .declare_data(name, SESSION_DATA, true, false)
                .map_err(|e| JitError::ice(format!("cannot declare `{name}`: {e}")))?;
            let mut desc = DataDescription::new();
            desc.define_zeroinit(8);
            desc.set_align(8);
            self.module
                .define_data(id, &desc)
                .map_err(|e| JitError::ice(format!("cannot define `{name}`: {e}")))?;
            self.loaded.defined.insert(name.clone());
        }
        Ok(())
    }

    /// Step 2: every symbol of `record` declared, by symbol index.
    fn declare_record(
        &mut self,
        record: &CodeRecord,
        signature: &ir::Signature,
    ) -> Result<Vec<FuncOrDataId>, JitError> {
        let module_error = |e: cranelift_module::ModuleError| {
            JitError::ice(format!("cannot declare a code record's symbol: {e}"))
        };
        let mut ids = Vec::with_capacity(record.symbols.len());
        for symbol in &record.symbols {
            let name = symbol.name.as_str();
            let local = symbol.linkage == SymbolLinkage::Local || name.starts_with(MODULE_RECORD);
            let id = match (symbol.kind, local, symbol.linkage) {
                (SymbolKind::Function, true, _) => FuncOrDataId::Func(
                    self.module
                        .declare_anonymous_function(signature)
                        .map_err(module_error)?,
                ),
                (SymbolKind::Data { writable }, true, _) => FuncOrDataId::Data(
                    self.module
                        .declare_anonymous_data(writable, false)
                        .map_err(module_error)?,
                ),
                // A runtime or foreign symbol: only its address matters,
                // which every import word's data relocation takes (D-913),
                // so a name an earlier record declared keeps its
                // declaration whatever its kind.
                (kind, false, SymbolLinkage::Import)
                    if !name.starts_with(GENERATED) && self.module.get_name(name).is_some() =>
                {
                    let _ = kind;
                    self.module
                        .get_name(name)
                        .ok_or_else(|| JitError::ice("a declared name went missing"))?
                }
                (SymbolKind::Function, false, linkage) => FuncOrDataId::Func(
                    self.module
                        .declare_function(name, linkage.linkage(), signature)
                        .map_err(module_error)?,
                ),
                (SymbolKind::Data { writable }, false, linkage) => FuncOrDataId::Data(
                    self.module
                        .declare_data(name, linkage.linkage(), writable, false)
                        .map_err(module_error)?,
                ),
            };
            ids.push(id);
        }
        Ok(ids)
    }

    /// Step 3: `record`'s functions and data defined, with the
    /// declarations `ids`.
    fn define_record(&mut self, record: &CodeRecord, ids: &[FuncOrDataId]) -> Result<(), JitError> {
        let module_error = |e: cranelift_module::ModuleError| {
            JitError::ice(format!("cannot load a code record: {e}"))
        };
        for function in &record.functions {
            let FuncOrDataId::Func(id) = id_at(ids, function.symbol)? else {
                return Err(JitError::ice("a code record defines data as a function"));
            };
            let mut relocs = Vec::with_capacity(function.relocs.len());
            for reloc in &function.relocs {
                relocs.push(ModuleReloc {
                    offset: reloc.offset,
                    kind: reloc.kind,
                    name: target(id_at(ids, reloc.symbol)?),
                    addend: reloc.addend,
                });
            }
            self.module
                .define_function_bytes(id, function.align, &function.bytes, &relocs)
                .map_err(module_error)?;
        }
        for object in &record.data {
            let FuncOrDataId::Data(id) = id_at(ids, object.symbol)? else {
                return Err(JitError::ice("a code record defines a function as data"));
            };
            let mut desc = DataDescription::new();
            match &object.contents {
                RecordContents::Bytes(bytes) => desc.define(bytes.clone().into_boxed_slice()),
                RecordContents::Zeros(size) => desc.define_zeroinit(
                    usize::try_from(*size)
                        .map_err(|_| JitError::ice("a data object too large for the host"))?,
                ),
            }
            if let Some(align) = object.align {
                if !align.is_power_of_two() {
                    return Err(JitError::ice(
                        "a data object's alignment is not a power of two",
                    ));
                }
                desc.set_align(align);
            }
            for reloc in &object.relocs {
                let gv = desc.import_global_value(target(id_at(ids, reloc.symbol)?));
                desc.write_data_addr(reloc.offset, gv, reloc.addend);
            }
            self.module.define_data(id, &desc).map_err(module_error)?;
        }
        Ok(())
    }

    /// Step 4: the module records of the batch's code and tests units
    /// registered with the one routine (D-919, D-1169), in build order.
    fn register_loaded(
        &mut self,
        items: &[Item<'_>],
        ids: &[Vec<FuncOrDataId>],
    ) -> Result<(), JitError> {
        let mut pointers: Vec<&ModuleRecord> = Vec::new();
        let mut batch = Vec::new();
        for (item, ids) in items.iter().zip(ids) {
            if item.unit == UnitKind::Entries {
                continue;
            }
            let at = item
                .record
                .symbols
                .iter()
                .position(|s| s.name.starts_with(MODULE_RECORD))
                .and_then(|at| u32::try_from(at).ok())
                .ok_or_else(|| JitError::ice("a code record without its module record"))?;
            let FuncOrDataId::Data(data) = id_at(ids, at)? else {
                return Err(JitError::ice("a module record that is not data"));
            };
            let (address, size) = self.module.get_finalized_data(data);
            #[expect(
                clippy::cast_ptr_alignment,
                reason = "code generation defines every data object with alignment 8; checked below"
            )]
            let pointer = address.cast::<ModuleRecord>();
            if pointer.is_null() || !pointer.is_aligned() || size < size_of::<ModuleRecord>() {
                return Err(JitError::ice("a module record without its bytes"));
            }
            // SAFETY: the record is a finalized, read-only, 8-aligned data
            // object of this JIT's module, at least as large as a
            // `ModuleRecord`, laid out as `cheby_runtime::image` documents
            // (`modrec::define_unit`, checked again by `register_records`);
            // it stays mapped while the JIT's memory lives, and nothing
            // writes it.
            pointers.push(unsafe { &*pointer });
            batch.push(JitRecord {
                unit: item.unit,
                path: item.path.into(),
                address: address.expose_provenance(),
            });
        }
        // SAFETY: every record and every table, word, static, function
        // value and function it points to lives in this JIT's memory, which
        // stays mapped for as long as anything registered may be used: the
        // code owner's ranges are unregistered before the memory is freed
        // (`CodeKeepAlive`), and the layouts' hooks are static function
        // values in this memory, called only on values of these layouts,
        // which only this JIT's code makes and which a runtime keeps alive
        // with its code token (D-708). The layout words and constructor
        // statics are writable data of this batch, which no code has run
        // yet and nothing else writes.
        unsafe { image::register_records(&pointers, self.keep_alive.code_owner) }.map_err(|e| {
            JitError::ice(format!("cannot register the batch's module records: {e}"))
        })?;
        self.records.extend(batch);
        Ok(())
    }

    /// What the JIT keeps of a loaded batch: its global names, and for
    /// each fresh unit its functions, slots, constants and repoints by key,
    /// and what later batches' units are told ([`Loaded::seed`]).
    fn note_loaded(
        &mut self,
        items: &[Item<'_>],
        ids: &[Vec<FuncOrDataId>],
    ) -> Result<(), JitError> {
        self.repointed.clear();
        self.pending.clear();
        for (item, ids) in items.iter().zip(ids) {
            let record = item.record;
            let defines = |index: u32| {
                record
                    .symbol_at(index)
                    .filter(|s| global_definition(&s.name, s.linkage))
                    .map(|s| s.name.clone())
            };
            for symbol in &record.symbols {
                if global_definition(&symbol.name, symbol.linkage) {
                    self.loaded.defined.insert(symbol.name.clone());
                }
            }
            let Some(index) = item.index else {
                continue;
            };
            for &(key, at) in &index.funcs {
                if let FuncOrDataId::Func(id) = id_at(ids, at)? {
                    self.loaded.funcs.insert(key, id);
                    if let FnKey::Trampoline(arity) = key
                        && record.symbol_at(at).is_some_and(|s| s.linkage.is_defined())
                        && let Some(slot) = usize::try_from(arity)
                            .ok()
                            .and_then(|a| self.loaded.trampolines.get_mut(a))
                    {
                        *slot = Some(id);
                    }
                }
            }
            for (def, key, at) in &index.slots {
                if let FuncOrDataId::Data(id) = id_at(ids, *at)? {
                    self.loaded.slots.insert(*def, id);
                    self.loaded.slot_sigs.insert(id, key.sig.clone());
                    if let Some(name) = defines(*at) {
                        self.loaded.seed.slots.insert(key.clone(), name);
                    }
                }
            }
            for &(def, at) in &index.consts {
                if let FuncOrDataId::Data(id) = id_at(ids, at)? {
                    self.loaded.consts.insert(def, id);
                    if let Some(name) = defines(at) {
                        self.loaded.seed.consts.insert(def, name);
                    }
                }
            }
            for &(layout, word, ctor) in &index.layouts {
                if let Some(word) = defines(word) {
                    let ctor = ctor.and_then(defines);
                    self.loaded.seed.layouts.insert(layout, (word, ctor));
                }
            }
            self.loaded
                .seed
                .externals
                .extend(index.externals.iter().copied());
            for &(slot, code, def) in &index.repoints {
                let (FuncOrDataId::Data(slot), FuncOrDataId::Func(code)) =
                    (id_at(ids, slot)?, id_at(ids, code)?)
                else {
                    return Err(JitError::ice("a repoint of a slot that is not data"));
                };
                let address = self.module.get_finalized_function(code);
                let word = u64::try_from(address.expose_provenance())
                    .map_err(|_| JitError::ice("an address does not fit in a word"))?;
                self.pending.push((slot, word, def));
            }
        }
        Ok(())
    }
}

/// The native symbols the foreign calls of `units` name, and the drop
/// functions of their external types (D-949), so a batch whose symbols do
/// not resolve fails before anything is compiled, as it always did (D-532).
pub(crate) fn foreign_symbols<'u>(units: &[&'u cheby_mir::MirModule]) -> Vec<&'u str> {
    use cheby_mir::{Rvalue, Stmt};
    let mut out = Vec::new();
    for unit in units {
        out.extend(unit.externals.iter().filter_map(|e| e.drop.as_deref()));
        for function in &unit.functions {
            for block in &function.blocks {
                for stmt in &block.stmts {
                    if let Stmt::Assign {
                        value: Rvalue::CallForeign { symbol, .. },
                        ..
                    }
                    | Stmt::Eval(Rvalue::CallForeign { symbol, .. }) = stmt
                    {
                        out.push(&**symbol);
                    }
                }
            }
        }
    }
    out
}

/// The ordinal of each test of every tests unit among `units`, by its
/// place among its unit's test bodies (D-1179: a tests unit is whole, in
/// source order).
pub(crate) fn test_ordinals(units: &[&cheby_mir::MirModule]) -> FxHashMap<DefId, u32> {
    let mut out = FxHashMap::default();
    for unit in units {
        if !matches!(unit.unit, UnitKind::Tests(_)) {
            continue;
        }
        let mut next = 0_u32;
        for function in &unit.functions {
            if let FnKey::Test(def) = function.key {
                out.insert(def, next);
                next = next.saturating_add(1);
            }
        }
    }
    out
}

/// The linkage of a JIT's own data declared by name: session values'
/// slots (`Jit::bind_values`).
pub(crate) const SESSION_DATA: Linkage = Linkage::Hidden;
