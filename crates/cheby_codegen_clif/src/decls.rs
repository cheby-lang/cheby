//! Declarations that persist across batches of units (E1, E4).

use std::fmt::Write as _;

use cranelift_codegen::ir::{self, Signature};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig, TargetIsa};
use cranelift_module::{DataId, FuncId, FuncOrDataId, Linkage, Module};
use rustc_hash::{FxHashMap, FxHashSet};

use cheby_iface::{DefId, ModuleId};
use cheby_mir::{
    AbiSig, BuiltinDesc, Const, DefName, EntryKind, FnKey, ForeignSig, Function, LayoutDesc,
    LayoutRef, MirModule, Operand, Rvalue, StaticKind, StaticObject, Stmt, StrId, Terminator,
    UnitKind,
};
use cheby_runtime::abi;

use crate::layout::builtin_layout_id;
use crate::mangle::{self, Kind};
use crate::runtime::{RuntimeFunc, STATIC_DATA};
use crate::session::{SessionName, SessionNames};
use crate::sig;
use crate::{CodegenError, ForeignImport};

/// A declared function of generated code.
#[derive(Clone, Debug)]
pub struct FuncDecl {
    pub id: FuncId,
    pub symbol: String,
    /// The machine signature, `ctx` included.
    pub sig: Signature,
    /// The MIR signature it was declared with.
    pub abi: AbiSig,
    pub defined: bool,
}

/// What identifies an indirection slot: the item's path and its ABI
/// signature (D-456). A REPL redefinition with the same ABI repoints the
/// slot; one with another ABI gets a new slot, so a slot is never pointed
/// at code of another signature.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SlotKey {
    pub path: Box<str>,
    pub sig: AbiSig,
}

/// How a foreign symbol was imported ([`ForeignImport`]): through its
/// import word, which every call site loads and calls with its own C
/// signature (D-913, D-475).
#[derive(Clone, Debug)]
pub(crate) struct ForeignDecl {
    /// The import word `cb$imp$<symbol>` ([`Decls::import_word`]).
    pub(crate) word: DataId,
    /// The one C signature of the symbol under
    /// [`ForeignImport::Function`]; `None` under [`ForeignImport::Address`],
    /// whose import carries no signature.
    pub(crate) signature: Option<Signature>,
}

/// How [`Decls::import_word`] declares the symbol an import word holds,
/// with `Linkage::Import`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ImportTarget<'s> {
    /// A function with its C signature: a runtime function, or a foreign
    /// symbol under [`ForeignImport::Function`].
    Function(&'s Signature),
    /// A symbol with no signature: a runtime static, or a foreign symbol
    /// under [`ForeignImport::Address`]. A symbol the module already
    /// declares (a runtime function named by an `@external`) keeps its
    /// declaration.
    Data,
}

/// The declaration an import word's data relocation names.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ImportRef {
    Func(FuncId),
    Data(DataId),
}

/// A declared indirection slot (D-043).
#[derive(Clone, Debug)]
pub struct SlotDecl {
    pub id: DataId,
    pub symbol: String,
    /// The function the slot initially points to.
    pub func: FuncId,
}

/// The data objects of a user constructor's layout (D-454): the
/// `cb$layout$` word holding the header that allocation copies (count 1),
/// and, for a nullary constructor, its `cb$ctor$` static object (count 0).
/// Both are filled from the runtime layout id when they are defined, so no
/// code embeds the id.
#[derive(Clone, Debug)]
pub(crate) struct LayoutDecl {
    pub(crate) word: DataId,
    pub(crate) ctor: Option<DataId>,
}

/// One word of a static object: the address of a data object, a code word
/// `address | 1` (D-453), or a plain word such as the tagged field count of
/// an escaped object.
#[derive(Clone, Copy, Debug)]
pub(crate) enum StaticWord {
    Data(DataId),
    Code(FuncId),
    Word(u64),
}

/// A static object waiting to be defined: its header (count 0) and fields.
#[derive(Clone, Debug)]
pub(crate) struct PendingStatic {
    pub(crate) id: DataId,
    pub(crate) header: u64,
    pub(crate) fields: Vec<StaticWord>,
}

/// Every symbol declared so far, persistent across [`crate::declare`]
/// calls: the REPL, constant evaluation and lazy compilation add units
/// whose code must find existing functions and slots. Lookups use hash
/// maps; nothing is iterated from them for output.
///
/// It is `Clone` so a caller can snapshot it before a batch and restore
/// the snapshot if the batch fails: re-declaring the same symbols with the
/// same signatures in the Cranelift module gives back the same ids.
#[derive(Clone)]
pub struct Decls {
    call_conv: CallConv,
    frontend: TargetFrontendConfig,
    funcs: FxHashMap<FnKey, FuncDecl>,
    slots: FxHashMap<SlotKey, SlotDecl>,
    /// In a unit alone, the slots of other units' functions that the
    /// unit's code loads, declared with `Linkage::Import` (D-917).
    imported_slots: FxHashMap<SlotKey, DataId>,
    strings: FxHashMap<(FnKey, StrId), DataId>,
    /// By [`RuntimeFunc::index`]: the function's import word and C
    /// signature; `None` for a function declared on demand
    /// ([`RuntimeFunc::on_demand`]) that no unit has used yet.
    runtime: Vec<Option<(DataId, Signature)>>,
    /// The runtime's static objects, by symbol: their `Linkage::Import`
    /// declarations, which static objects' fields name (code loads their
    /// import words instead).
    static_data: FxHashMap<&'static str, DataId>,
    foreign: FxHashMap<Box<str>, ForeignDecl>,
    /// The import word `cb$imp$<symbol>` of every runtime function,
    /// runtime static and foreign symbol declared so far, by symbol
    /// (D-913). In the JIT the words of a session serve all its batches.
    imports: FxHashMap<Box<str>, DataId>,
    test_ordinals: FxHashMap<DefId, u32>,
    next_test: FxHashMap<ModuleId, u32>,
    /// User constructor layouts (D-454).
    layouts: FxHashMap<LayoutRef, LayoutDecl>,
    /// Constant slots `cb$const$…` (E6), by constant.
    consts: FxHashMap<DefId, DataId>,
    /// Static objects (D-523) by symbol: dictionaries, descriptors and
    /// function values. Defined once; a later unit naming the same symbol
    /// uses the existing object.
    statics: FxHashMap<String, DataId>,
    /// What each static object of a unit was defined with, by symbol
    /// ([`static_print`]), so a later unit that names the symbol with other
    /// contents is an internal error rather than a silent reuse.
    static_prints: FxHashMap<String, String>,
    /// The runtime's builtin nominal descriptors `cheby_rt_desc_<name>`
    /// imported so far (D-523), by symbol.
    builtin_descs: FxHashMap<BuiltinDesc, DataId>,
    /// Symbol of every declared function (namespace 0) and data object
    /// (namespace 1), for CLIF legends.
    names: FxHashMap<(u32, u32), String>,
    /// The names of REPL session items (D-547, E4).
    session: SessionNames,
    /// The slot each top-level function's code was last put in.
    def_slots: FxHashMap<DefId, SlotKey>,
    /// The ABI signature of every slot, including slots a newer one of the
    /// same path replaced in `slots`.
    slot_sigs: FxHashMap<DataId, AbiSig>,
    /// Import words to define, with the declaration each holds.
    pub(crate) pending_imports: Vec<(DataId, ImportRef)>,
    pub(crate) pending_strings: Vec<(DataId, Vec<u8>)>,
    pub(crate) pending_slots: Vec<(DataId, FuncId)>,
    /// Existing slots to point at new code once the batch is finalized: a
    /// REPL redefinition with the same signature (D-043), with the
    /// redefinition's item.
    pub(crate) pending_repoints: Vec<(DataId, FuncId, DefId)>,
    /// `(layout, its declaration, value fields)` of layouts to define.
    pub(crate) pending_layouts: Vec<(LayoutRef, LayoutDecl, u32)>,
    pub(crate) pending_consts: Vec<DataId>,
    pub(crate) pending_statics: Vec<PendingStatic>,
    /// Every data object and function defined in the module so far, so a
    /// batch that would define one again fails before it defines anything
    /// (the JIT's all-or-nothing batches).
    pub(crate) defined_data: FxHashSet<DataId>,
    pub(crate) defined_funcs: FxHashSet<FuncId>,
    /// The shims of `@blocking` foreign calls (D-683, D-818),
    /// by symbol and C signature, each defined once per JIT like a
    /// trampoline (`blocking.rs`).
    blocking_shims: FxHashMap<(Box<str>, ForeignSig), FuncId>,
    /// How many shims each blocking symbol has, which numbers the next
    /// one ([`mangle::blocking_shim`]).
    blocking_variants: FxHashMap<Box<str>, u32>,
    /// The shims declared and not defined yet, in declaration order:
    /// [`crate::define`] defines them with its batch.
    pub(crate) pending_shims: Vec<(FuncId, Box<str>, ForeignSig)>,
    /// `Some` in the unit-alone mode of [`crate::compile_unit`] (D-917),
    /// which the JIT never uses: the unit's own items that another unit
    /// may name ([`crate::UnitCx::named_outside`]), whose functions and
    /// slots are global hidden. `None` in the JIT's mode, where every
    /// symbol is local.
    alone: Option<FxHashSet<DefId>>,
}

impl std::fmt::Debug for Decls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decls")
            .field("call_conv", &self.call_conv)
            .field("funcs", &self.funcs.len())
            .field("slots", &self.slots.len())
            .field("strings", &self.strings.len())
            .field("layouts", &self.layouts.len())
            .field("consts", &self.consts.len())
            .field("statics", &self.statics.len())
            .finish_non_exhaustive()
    }
}

/// The contents of a static object in a form that does not depend on its
/// unit: its layout, strings and fields, with a reference to another static
/// of the unit written as that static's symbol. Two units that define one
/// symbol must give it the same print (D-523).
fn static_print(decls: &Decls, unit: &MirModule, object: &StaticObject) -> String {
    let mut print = format!("{:?} {:?} [", object.layout, object.strings);
    for field in &object.fields {
        match field {
            Const::Static(id) => match unit.static_object(*id) {
                Some(other) => {
                    let _ = write!(print, "{}, ", decls.static_symbol(other));
                }
                None => {
                    let _ = write!(print, "{field:?}, ");
                }
            },
            other => {
                let _ = write!(print, "{other:?}, ");
            }
        }
    }
    print.push(']');
    print
}

/// The item whose redefinition replaces the function `key`: its own code,
/// its value shim, its closures and continuations, its constant
/// initializer and their entry.
const fn owner_of(key: FnKey) -> Option<DefId> {
    match key {
        FnKey::Item(def)
        | FnKey::ValueShim(def)
        | FnKey::ConstInit(def)
        | FnKey::Closure { owner: def, .. }
        | FnKey::Continuation { owner: def, .. }
        | FnKey::Entry(EntryKind::Const(def)) => Some(def),
        _ => None,
    }
}

/// Whether a second unit may declare `key` again: the functions that
/// depend on nothing but their key (shims, tuple functions, `dyn`
/// forwarders, trampolines) are defined once per JIT, and a later unit
/// that names one uses the existing code (D-523's rule for statics).
const fn shared_key(key: FnKey) -> bool {
    matches!(
        key,
        FnKey::ValueShim(_)
            | FnKey::TupleDictFn { .. }
            | FnKey::DynForward { .. }
            | FnKey::Trampoline(_)
    )
}

/// The header of an object, with the scan count escaped above
/// [`abi::SCAN_MAX`].
pub(crate) fn header(rc: i32, value_fields: u64, layout: u32) -> Result<u64, CodegenError> {
    let scan = u8::try_from(value_fields)
        .ok()
        .filter(|&s| s <= abi::SCAN_MAX)
        .ok_or_else(|| {
            CodegenError::ice(format!(
                "an object with {value_fields} value fields needs an escaped scan count"
            ))
        })?;
    Ok(abi::header(rc, scan, layout))
}

/// The header of a constructor object, with the escape value of D-279 as
/// its scan count above [`abi::SCAN_MAX`] value fields. The fields stay
/// where the layout puts them: freeing, equality, hashing and debug text
/// read a constructor's fields from the layout table, never from the scan
/// count (D-555).
pub(crate) fn ctor_header(rc: i32, value_fields: u64, layout: u32) -> u64 {
    let scan = u8::try_from(value_fields)
        .ok()
        .filter(|&s| s <= abi::SCAN_MAX)
        .unwrap_or(abi::SCAN_ESCAPE);
    abi::header(rc, scan, layout)
}

/// Where an object of `layout` with `count` uniform fields keeps its field
/// count when that is above [`abi::SCAN_MAX`] (D-279), as an index among
/// the words after the header; `None` when the scan count holds `count`.
///
/// The escaped object holds, at that index, the number of the fields that
/// follow as a tagged integer, which freeing skips like any tagged word:
/// a tuple at index 0 (offset 8), and a closure, dictionary or descriptor
/// at index 1 (offset 16), after its code word, type descriptor or
/// nominal descriptor, which stays at offset 8 so indirect calls and
/// readers of the first field need no check (D-555).
pub(crate) fn escaped_count_index(layout: LayoutRef, count: u64) -> Option<usize> {
    if count <= u64::from(abi::SCAN_MAX) {
        return None;
    }
    match layout {
        LayoutRef::Tuple(_) => Some(0),
        LayoutRef::Closure(_) | LayoutRef::Dict { .. } | LayoutRef::Desc { .. } => Some(1),
        LayoutRef::Ctor(_) | LayoutRef::Dyn | LayoutRef::DescNominal | LayoutRef::Builtin(_) => {
            None
        }
    }
}

impl Decls {
    /// An empty table for code running on `isa`.
    ///
    /// # Errors
    ///
    /// [`CodegenError::UnsupportedHost`] unless the target is 64-bit and
    /// little-endian.
    pub fn new(isa: &dyn TargetIsa) -> Result<Self, CodegenError> {
        if isa.endianness() != ir::Endianness::Little {
            return Err(CodegenError::UnsupportedHost(
                "only little-endian targets are supported".to_owned(),
            ));
        }
        if isa.pointer_type() != sig::WORD {
            return Err(CodegenError::UnsupportedHost(
                "only 64-bit targets are supported".to_owned(),
            ));
        }
        Ok(Self {
            call_conv: isa.default_call_conv(),
            frontend: isa.frontend_config(),
            funcs: FxHashMap::default(),
            slots: FxHashMap::default(),
            imported_slots: FxHashMap::default(),
            strings: FxHashMap::default(),
            runtime: Vec::new(),
            static_data: FxHashMap::default(),
            foreign: FxHashMap::default(),
            imports: FxHashMap::default(),
            test_ordinals: FxHashMap::default(),
            next_test: FxHashMap::default(),
            layouts: FxHashMap::default(),
            consts: FxHashMap::default(),
            statics: FxHashMap::default(),
            static_prints: FxHashMap::default(),
            builtin_descs: FxHashMap::default(),
            names: FxHashMap::default(),
            session: SessionNames::default(),
            def_slots: FxHashMap::default(),
            slot_sigs: FxHashMap::default(),
            pending_imports: Vec::new(),
            pending_strings: Vec::new(),
            pending_slots: Vec::new(),
            pending_repoints: Vec::new(),
            pending_layouts: Vec::new(),
            pending_consts: Vec::new(),
            pending_statics: Vec::new(),
            defined_data: FxHashSet::default(),
            defined_funcs: FxHashSet::default(),
            blocking_shims: FxHashMap::default(),
            blocking_variants: FxHashMap::default(),
            pending_shims: Vec::new(),
            alone: None,
        })
    }

    /// An empty table for one unit compiled alone ([`crate::compile_unit`],
    /// D-917): `named_outside` holds the unit's own items that another unit
    /// may name.
    ///
    /// # Errors
    ///
    /// As [`Decls::new`].
    pub(crate) fn new_alone(
        isa: &dyn TargetIsa,
        named_outside: FxHashSet<DefId>,
    ) -> Result<Self, CodegenError> {
        let mut decls = Self::new(isa)?;
        decls.alone = Some(named_outside);
        Ok(decls)
    }

    /// Whether this table compiles one unit alone (D-917).
    pub(crate) const fn is_alone(&self) -> bool {
        self.alone.is_some()
    }

    /// The linkage of the function and slot of the unit's own item `def`
    /// (D-917): global hidden in a unit alone when another unit may name
    /// it (GAP(unit-alone-visibility)), local otherwise and always in the
    /// JIT.
    fn item_linkage(&self, def: DefId) -> Linkage {
        match &self.alone {
            Some(named) if named.contains(&def) => Linkage::Hidden,
            Some(_) | None => Linkage::Local,
        }
    }

    /// The linkage of the unit's own writable objects that other objects
    /// patch or read (D-917, D-918): layout words, `cb$ctor$` statics and
    /// constant slots, global hidden in a unit alone and local in the JIT.
    const fn owned_data_linkage(&self) -> Linkage {
        if self.alone.is_some() {
            Linkage::Hidden
        } else {
            Linkage::Local
        }
    }

    /// The platform's C calling convention.
    #[must_use]
    pub const fn c_call_conv(&self) -> CallConv {
        self.call_conv
    }

    pub(crate) const fn frontend_config(&self) -> TargetFrontendConfig {
        self.frontend
    }

    /// The declaration of the function `key`.
    ///
    /// # Errors
    ///
    /// An ICE if `key` was never declared.
    pub fn func(&self, key: FnKey) -> Result<&FuncDecl, CodegenError> {
        self.funcs
            .get(&key)
            .ok_or_else(|| CodegenError::ice(format!("function {key:?} was not declared")))
    }

    /// The slot of the item at `path` with signature `sig`.
    ///
    /// # Errors
    ///
    /// An ICE if no function with that path and signature was declared.
    pub fn slot(&self, path: &str, sig: &AbiSig) -> Result<&SlotDecl, CodegenError> {
        let key = SlotKey {
            path: path.into(),
            sig: sig.clone(),
        };
        self.slots.get(&key).ok_or_else(|| {
            CodegenError::ice(format!(
                "no slot for `{path}` with signature {sig:?}; was its module compiled?"
            ))
        })
    }

    /// The data object of the slot that calls of the item at `path` with
    /// signature `sig` load: a slot of this table's units, or, in a unit
    /// alone, another unit's slot imported (D-917).
    ///
    /// # Errors
    ///
    /// An ICE if no such slot was declared.
    pub(crate) fn slot_data(&self, path: &str, sig: &AbiSig) -> Result<DataId, CodegenError> {
        let key = SlotKey {
            path: path.into(),
            sig: sig.clone(),
        };
        if let Some(&id) = self.imported_slots.get(&key) {
            return Ok(id);
        }
        self.slot(path, sig).map(|slot| slot.id)
    }

    /// The slot `cb$const$…` of the constant `def`, if a compiled unit
    /// declared it (its initializer, or code that reads it).
    #[must_use]
    pub fn const_slot(&self, def: DefId) -> Option<DataId> {
        self.consts.get(&def).copied()
    }

    /// The static function value `cb$fnval$…` of `def` (D-523), if a
    /// compiled unit declared it; `unit` names `def`.
    ///
    /// # Errors
    ///
    /// An ICE if `unit` does not name `def`.
    pub fn fn_value(&self, unit: &MirModule, def: DefId) -> Result<Option<DataId>, CodegenError> {
        let symbol = mangle::item(Kind::FnVal, &self.def_name(unit, def)?);
        Ok(self.statics.get(&symbol).copied())
    }

    /// Registers how a REPL session names `def` (D-547, E4): its symbols
    /// use the module `<repl>` and carry its generation, and
    /// [`SessionName::reuse`] makes it take over an earlier definition's
    /// slot. Registering a new generation for a def that is already
    /// declared (a module compiled again, `:reload`) forgets the def's old
    /// declarations, so its functions, layouts and constant slot are
    /// declared anew; slots stay, keyed by path and signature.
    pub fn set_session_name(&mut self, def: DefId, name: SessionName) {
        let generation = name.generation;
        let keeps_const = name.reuse == Some(def);
        let previous = self.session.insert(def, name);
        let changed = previous.map_or(generation > 0, |p| p.generation != generation);
        if changed {
            self.funcs.retain(|key, _| owner_of(*key) != Some(def));
            self.layouts.remove(&LayoutRef::Ctor(def));
            if !keeps_const {
                self.consts.remove(&def);
            }
        }
    }

    /// Every symbol declared so far, sorted: for tests and dumps.
    #[must_use]
    pub fn symbols(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.names.values().map(String::as_str).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Whether the data objects of the user constructor layout `layout`
    /// are declared.
    #[must_use]
    pub fn has_layout(&self, layout: LayoutRef) -> bool {
        self.layouts.contains_key(&layout)
    }

    /// The session name of `def`, if it is a REPL session item.
    #[must_use]
    pub fn session_name(&self, def: DefId) -> Option<&SessionName> {
        self.session.get(def)
    }

    /// The name symbols of `def` are built from: its session name, or the
    /// path `unit` records.
    pub(crate) fn def_name(&self, unit: &MirModule, def: DefId) -> Result<DefName, CodegenError> {
        match self.session.get(def) {
            Some(name) => Ok(name.def_name(def)),
            None => Ok(unit.name(def)?.clone()),
        }
    }

    /// The path that identifies `def`'s slot (D-456): its session path
    /// without generation, or its own path.
    pub(crate) fn slot_path(&self, unit: &MirModule, def: DefId) -> Result<String, CodegenError> {
        match self.session.get(def) {
            Some(name) => Ok(name.slot_path()),
            None => Ok(unit.name(def)?.path()),
        }
    }

    /// The slots to point at new code now that their batch is finalized,
    /// with that code and the redefinition it is: REPL redefinitions that
    /// take over a slot (D-043).
    pub fn take_repoints(&mut self) -> Vec<(DataId, FuncId, DefId)> {
        std::mem::take(&mut self.pending_repoints)
    }

    /// The ABI signature of the code the slot `slot` holds.
    #[must_use]
    pub fn slot_sig(&self, slot: DataId) -> Option<&AbiSig> {
        self.slot_sigs.get(&slot)
    }

    /// The slot the top-level function `def` was last compiled into.
    #[must_use]
    pub fn slot_of(&self, def: DefId) -> Option<&SlotDecl> {
        self.slots.get(self.def_slots.get(&def)?)
    }

    /// Declares the slot `cb$const$…` of the session value `def` if no
    /// compiled unit did (a value made by a prompt-level `let`, D-547): the
    /// slot of the definition it takes over, or a new one, defined by
    /// the next [`crate::define_data`].
    ///
    /// # Errors
    ///
    /// An ICE if `def` has no session name.
    pub fn declare_session_const<M: Module>(
        &mut self,
        module: &mut M,
        def: DefId,
    ) -> Result<DataId, CodegenError> {
        if let Some(id) = self.const_slot(def) {
            return Ok(id);
        }
        let name = self
            .session
            .get(def)
            .ok_or_else(|| CodegenError::ice(format!("the session value {def:?} has no name")))?
            .clone();
        if let Some(old) = name.reuse
            && let Some(id) = self.const_slot(old)
        {
            self.consts.insert(def, id);
            return Ok(id);
        }
        let symbol = mangle::item(Kind::Const, &name.def_name(def));
        let id = self.data(module, &symbol, Linkage::Local, true)?;
        self.pending_consts.push(id);
        self.consts.insert(def, id);
        Ok(id)
    }

    pub(crate) fn string(&self, key: FnKey, id: StrId) -> Result<DataId, CodegenError> {
        self.strings
            .get(&(key, id))
            .copied()
            .ok_or_else(|| CodegenError::ice(format!("string {id:?} of {key:?} was not declared")))
    }

    /// The import word of the runtime function `f` and its C signature.
    pub(crate) fn runtime(&self, f: RuntimeFunc) -> Result<(DataId, &Signature), CodegenError> {
        self.runtime
            .get(f.index())
            .and_then(Option::as_ref)
            .map(|(id, s)| (*id, s))
            .ok_or_else(|| {
                CodegenError::ice(format!(
                    "the runtime function `{}` was not declared",
                    f.symbol()
                ))
            })
    }

    /// The import word of the runtime or foreign symbol `symbol` (D-913).
    pub(crate) fn import(&self, symbol: &str) -> Result<DataId, CodegenError> {
        self.imports
            .get(symbol)
            .copied()
            .ok_or_else(|| CodegenError::ice(format!("`{symbol}` has no import word")))
    }

    /// The symbol of every import word declared so far, sorted, each with
    /// whether its word is defined in the module: for tests, which check
    /// that a failed batch leaves no word declared and undefined.
    #[must_use]
    pub fn import_words(&self) -> Vec<(&str, bool)> {
        let mut out: Vec<(&str, bool)> = self
            .imports
            .iter()
            .map(|(symbol, id)| (symbol.as_ref(), self.defined_data.contains(id)))
            .collect();
        out.sort_unstable();
        out
    }

    /// The import word `cb$imp$<symbol>` of the runtime or foreign symbol
    /// `symbol` (D-913, D-917's `imp` kind): a read-only 8-byte data object,
    /// `Linkage::Local`, defined by the next [`crate::define`] with one
    /// `Abs8` data relocation to `symbol`, which is declared here with
    /// `Linkage::Import` as `target` says. Code loads the word and calls
    /// the address or uses it as the static's, so text never names a
    /// runtime or foreign symbol. Every import word is declared here, once
    /// per [`Decls`]: a JIT session's later batches use the words of its
    /// earlier ones.
    ///
    /// GAP(jit-import-words-per-session): one JIT module holds every batch
    /// of a session, so its words are declared once and shared by later
    /// batches; objects get per-unit copies from wave 2 (D-917, D-918).
    ///
    /// Under [`ImportTarget::Function`] the symbol is declared as a
    /// function each time, so a second signature for it is the module's
    /// error, as before import words.
    ///
    /// # Errors
    ///
    /// The module's error if a declaration conflicts with an earlier one.
    pub(crate) fn import_word<M: Module>(
        &mut self,
        module: &mut M,
        symbol: &str,
        target: ImportTarget<'_>,
    ) -> Result<DataId, CodegenError> {
        let declared = match target {
            ImportTarget::Function(signature) => {
                let id = module
                    .declare_function(symbol, Linkage::Import, signature)
                    .map_err(|e| CodegenError::module(&e))?;
                self.names.insert((0, id.as_u32()), symbol.to_owned());
                Some(ImportRef::Func(id))
            }
            ImportTarget::Data => None,
        };
        if let Some(&word) = self.imports.get(symbol) {
            return Ok(word);
        }
        let declared = match declared {
            Some(declared) => declared,
            // A symbol the module already declares: a runtime function an
            // `@external` names, or a declaration a failed batch left in
            // the module after its tables were restored.
            None => match module.get_name(symbol) {
                Some(FuncOrDataId::Func(id)) => {
                    self.names.insert((0, id.as_u32()), symbol.to_owned());
                    ImportRef::Func(id)
                }
                Some(FuncOrDataId::Data(id)) => {
                    self.names.insert((1, id.as_u32()), symbol.to_owned());
                    ImportRef::Data(id)
                }
                None => ImportRef::Data(self.data(module, symbol, Linkage::Import, false)?),
            },
        };
        let word = self.data(module, &mangle::import_word(symbol), Linkage::Local, false)?;
        self.pending_imports.push((word, declared));
        self.imports.insert(symbol.into(), word);
        Ok(word)
    }

    /// Whether `symbol`, declared with `Linkage::Import`, is a generated
    /// symbol of another unit that text may reach PC-relative (D-917):
    /// only in a unit alone, a `cb$` name that is not the target of an
    /// import word (a runtime or foreign symbol, which code reaches only
    /// through its word, D-913) and not a libcall.
    pub(crate) fn is_generated_import(&self, symbol: &str) -> bool {
        self.is_alone()
            && symbol.starts_with(mangle::PREFIX)
            && !symbol.starts_with("cb$libcall$")
            && !self.imports.contains_key(symbol)
    }

    pub(crate) fn static_data(&self, symbol: &str) -> Result<DataId, CodegenError> {
        self.static_data
            .get(symbol)
            .copied()
            .ok_or_else(|| CodegenError::ice(format!("`{symbol}` was not declared")))
    }

    pub(crate) fn foreign(&self, symbol: &str) -> Result<&ForeignDecl, CodegenError> {
        self.foreign
            .get(symbol)
            .ok_or_else(|| CodegenError::ice(format!("foreign `{symbol}` was not declared")))
    }

    /// The shim of the blocking foreign function `symbol` called with the C
    /// signature `sig` (`blocking.rs`).
    pub(crate) fn blocking_shim(
        &self,
        symbol: &str,
        sig: &ForeignSig,
    ) -> Result<FuncId, CodegenError> {
        self.blocking_shims
            .get(&(Box::from(symbol), sig.clone()))
            .copied()
            .ok_or_else(|| {
                CodegenError::ice(format!("the blocking shim of `{symbol}` was not declared"))
            })
    }

    pub(crate) fn layout(&self, layout: LayoutRef) -> Result<&LayoutDecl, CodegenError> {
        self.layouts.get(&layout).ok_or_else(|| {
            CodegenError::ice(format!(
                "the layout {layout:?} was not declared; was its module compiled?"
            ))
        })
    }

    pub(crate) fn const_data(&self, def: DefId) -> Result<DataId, CodegenError> {
        self.const_slot(def)
            .ok_or_else(|| CodegenError::ice(format!("the constant {def:?} has no slot")))
    }

    pub(crate) fn static_object(&self, symbol: &str) -> Result<DataId, CodegenError> {
        self.statics
            .get(symbol)
            .copied()
            .ok_or_else(|| CodegenError::ice(format!("the static `{symbol}` was not declared")))
    }

    /// Every foreign symbol declared so far, sorted.
    #[must_use]
    pub fn foreign_symbols(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.foreign.keys().map(AsRef::as_ref).collect();
        out.sort_unstable();
        out
    }

    /// The runtime data symbols declared on demand so far (the builtin
    /// nominal descriptors, D-523), sorted. Unlike the fixed runtime
    /// imports, later packages define them, so the JIT checks that they
    /// resolve before finalizing.
    #[must_use]
    pub fn runtime_data_symbols(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .builtin_descs
            .keys()
            .map(|b| builtin_desc_symbol(*b))
            .collect();
        out.sort_unstable();
        out
    }

    pub(crate) fn builtin_desc(&self, desc: BuiltinDesc) -> Result<DataId, CodegenError> {
        self.builtin_descs.get(&desc).copied().ok_or_else(|| {
            CodegenError::ice(format!("`{}` was not declared", builtin_desc_symbol(desc)))
        })
    }

    pub(crate) fn mark_defined(&mut self, key: FnKey) -> Result<(), CodegenError> {
        let decl = self
            .funcs
            .get_mut(&key)
            .ok_or_else(|| CodegenError::ice(format!("function {key:?} was not declared")))?;
        decl.defined = true;
        Ok(())
    }

    /// The symbol of the data object `id`, for messages.
    pub(crate) fn data_name(&self, id: DataId) -> &str {
        self.names
            .get(&(1, id.as_u32()))
            .map_or("?", String::as_str)
    }

    /// The symbol of the function `id`, for messages.
    pub(crate) fn func_name(&self, id: FuncId) -> &str {
        self.names
            .get(&(0, id.as_u32()))
            .map_or("?", String::as_str)
    }

    /// The CLIF text of `func`, preceded by its symbol and a legend that
    /// names each external function and data object it references.
    #[must_use]
    pub fn describe(&self, func: &ir::Function) -> String {
        let mut out = String::new();
        if let ir::UserFuncName::User(name) = &func.name
            && let Some(symbol) = self.names.get(&(name.namespace, name.index))
        {
            let _ = writeln!(out, "; {symbol}");
        }
        for (reference, name) in func.params.user_named_funcs() {
            let symbol = self
                .names
                .get(&(name.namespace, name.index))
                .map_or("?", String::as_str);
            let _ = writeln!(out, "; {reference} = {symbol}");
        }
        // D-869: the text leaves out the calls' location tags
        // (D-856), which are metadata, so a dump is the same with and
        // without them (D-551).
        if func.debug_tags.is_empty() {
            let _ = write!(out, "{}", func.display());
        } else {
            let mut untagged = func.clone();
            untagged.debug_tags.clear();
            let _ = write!(out, "{}", untagged.display());
        }
        out
    }

    pub(crate) fn declare_units<M: Module>(
        &mut self,
        module: &mut M,
        mirs: &[MirModule],
        import: ForeignImport,
    ) -> Result<(), CodegenError> {
        self.declare_runtime(module)?;
        if self.is_alone() {
            for unit in mirs {
                self.number_imported_tests(unit);
            }
        }
        for unit in mirs {
            for function in &unit.functions {
                self.declare_function(module, unit, function)?;
            }
        }
        if self.is_alone() {
            for unit in mirs {
                self.declare_code_imports(module, unit)?;
            }
        }
        for unit in mirs {
            self.declare_unit_runtime(module, unit)?;
        }
        for unit in mirs {
            for desc in &unit.layouts {
                self.declare_layout(module, unit, desc)?;
            }
        }
        if self.is_alone() {
            for unit in mirs {
                self.declare_layout_imports(module, unit)?;
            }
        }
        for unit in mirs {
            self.declare_consts(module, unit)?;
            self.declare_fn_values(module, unit)?;
        }
        for unit in mirs {
            self.declare_statics(module, unit)?;
        }
        for unit in mirs {
            for function in &unit.functions {
                self.declare_foreign(module, function, import)?;
            }
        }
        Ok(())
    }

    fn data<M: Module>(
        &mut self,
        module: &mut M,
        symbol: &str,
        linkage: Linkage,
        writable: bool,
    ) -> Result<DataId, CodegenError> {
        let id = module
            .declare_data(symbol, linkage, writable, false)
            .map_err(|e| CodegenError::module(&e))?;
        self.names.insert((1, id.as_u32()), symbol.to_owned());
        Ok(id)
    }

    fn declare_runtime<M: Module>(&mut self, module: &mut M) -> Result<(), CodegenError> {
        if !self.runtime.is_empty() {
            return Ok(());
        }
        self.runtime = vec![None; RuntimeFunc::ALL.len()];
        for f in RuntimeFunc::ALL {
            if !f.on_demand() {
                self.declare_runtime_func(module, f)?;
            }
        }
        for symbol in STATIC_DATA {
            self.declare_runtime_static(module, symbol)?;
        }
        Ok(())
    }

    /// Declares the runtime static `symbol` (`Linkage::Import`), which
    /// static objects' fields name, and its import word, which code loads
    /// (D-913).
    fn declare_runtime_static<M: Module>(
        &mut self,
        module: &mut M,
        symbol: &'static str,
    ) -> Result<(), CodegenError> {
        let id = self.data(module, symbol, Linkage::Import, false)?;
        self.static_data.insert(symbol, id);
        self.import_word(module, symbol, ImportTarget::Data)?;
        Ok(())
    }

    /// Declares the runtime function `f` if it is not declared yet: the
    /// eager ones once per session, the on-demand ones when a unit first
    /// uses them ([`RuntimeFunc::on_demand`]).
    pub(crate) fn declare_runtime_func<M: Module>(
        &mut self,
        module: &mut M,
        f: RuntimeFunc,
    ) -> Result<(), CodegenError> {
        let slot = self
            .runtime
            .get(f.index())
            .ok_or_else(|| CodegenError::ice("runtime functions were not declared"))?;
        if slot.is_some() {
            return Ok(());
        }
        let (params, ret) = f.ctypes();
        let signature = sig::c_signature(params, ret, self.call_conv);
        // The function is declared first, in the order it always was, so
        // the ids of generated functions do not move (D-264).
        let id = self.import_word(module, f.symbol(), ImportTarget::Function(&signature))?;
        let slot = self
            .runtime
            .get_mut(f.index())
            .ok_or_else(|| CodegenError::ice("runtime functions were not declared"))?;
        *slot = Some((id, signature));
        Ok(())
    }

    /// Declares `cb$big$…`, the unchecked inner body of the function
    /// `function`, whose frame turned out to be large (D-679), with its
    /// signature, and the runtime function its outer function's slow path
    /// calls: the releasing one when its entry pad allows it
    /// (D-775, [`crate::func::entry_release`]).
    /// [`crate::define`] calls it after compiling a batch, for each large
    /// function in declaration order, so the ids are the same in every
    /// build (D-264). Declaring the same body again (a batch retried after
    /// its [`Decls`] were restored) gives back the same id.
    ///
    /// # Errors
    ///
    /// An ICE if `key` was never declared, or the module's error.
    pub(crate) fn declare_large_frame_body<M: Module>(
        &mut self,
        module: &mut M,
        function: &Function,
    ) -> Result<FuncId, CodegenError> {
        let slow = if crate::func::entry_release(function).is_some() {
            RuntimeFunc::StackSlowFrameUnwind
        } else {
            RuntimeFunc::StackSlowFrame
        };
        self.declare_runtime_func(module, slow)?;
        let decl = self.func(function.key)?;
        let symbol = mangle::large_frame_body(&decl.symbol);
        let signature = decl.sig.clone();
        let id = module
            .declare_function(&symbol, Linkage::Local, &signature)
            .map_err(|e| CodegenError::module(&e))?;
        self.names.insert((0, id.as_u32()), symbol);
        Ok(id)
    }

    /// Declares what reuse's code imports (D-716), once: its runtime
    /// functions, checked and inline, and the counting flag
    /// ([`crate::runtime::ON_DEMAND_DATA`]), in a fixed order, so the ids
    /// are the same in every build (D-264).
    fn declare_reuse<M: Module>(&mut self, module: &mut M) -> Result<(), CodegenError> {
        for f in [
            RuntimeFunc::ReleaseFields,
            RuntimeFunc::ReuseHit,
            RuntimeFunc::AllocMiss,
            RuntimeFunc::DropReuseChecked,
            RuntimeFunc::AllocReuseChecked,
        ] {
            self.declare_runtime_func(module, f)?;
        }
        for symbol in crate::runtime::ON_DEMAND_DATA {
            if !self.static_data.contains_key(symbol) {
                self.declare_runtime_static(module, symbol)?;
            }
        }
        Ok(())
    }

    /// Declares the on-demand runtime functions the helpers of `unit`'s
    /// code call (D-693), and the entry check's releasing slow path when a
    /// function's entry pad allows it (D-775).
    fn declare_unit_runtime<M: Module>(
        &mut self,
        module: &mut M,
        unit: &MirModule,
    ) -> Result<(), CodegenError> {
        for function in &unit.functions {
            if function.abi == cheby_mir::Abi::Cheby
                && crate::func::entry_release(function).is_some()
            {
                self.declare_runtime_func(module, RuntimeFunc::StackSlowUnwind)?;
            }
            for block in &function.blocks {
                for stmt in &block.stmts {
                    if let Stmt::Assign {
                        value: Rvalue::CallRuntime { func, .. },
                        ..
                    }
                    | Stmt::Eval(Rvalue::CallRuntime { func, .. }) = stmt
                    {
                        let (f, _) = RuntimeFunc::of(*func);
                        if f.on_demand() {
                            self.declare_runtime_func(module, f)?;
                        }
                    }
                    if let Stmt::Assign {
                        value: Rvalue::DropReuse { .. } | Rvalue::AllocReuse { .. },
                        ..
                    } = stmt
                    {
                        self.declare_reuse(module)?;
                    }
                    // A specialized drop's hit path (D-850).
                    if let Stmt::DropKeep { .. } = stmt {
                        self.declare_runtime_func(module, RuntimeFunc::ReleaseFields)?;
                        self.declare_runtime_func(module, RuntimeFunc::FreeUnique)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn symbol(&mut self, unit: &MirModule, key: FnKey) -> Result<String, CodegenError> {
        Ok(match key {
            FnKey::Item(def) => mangle::item(Kind::Fn, &self.def_name(unit, def)?),
            FnKey::ValueShim(def) => mangle::item(Kind::Shim, &self.def_name(unit, def)?),
            // A test's name is free text that may equal a function's, so
            // a test's closures go by its ordinal, like the test (D-456).
            FnKey::Closure { owner, index } => {
                let name = self.def_name(unit, owner)?;
                match self.test_ordinals.get(&owner) {
                    Some(&ordinal) => mangle::test_closure(&name.module, ordinal, index),
                    None => mangle::closure(&name, index),
                }
            }
            // Named as the owner's closures are (D-780).
            FnKey::Continuation { owner, index } => {
                let name = self.def_name(unit, owner)?;
                match self.test_ordinals.get(&owner) {
                    Some(&ordinal) => mangle::test_continuation(&name.module, ordinal, index),
                    None => mangle::continuation(&name, index),
                }
            }
            FnKey::ConstInit(def) => mangle::item(Kind::Cinit, &self.def_name(unit, def)?),
            FnKey::Test(def) => {
                let UnitKind::Tests(module) = unit.unit else {
                    // In a unit alone, the entries unit imports the test
                    // bodies it calls, numbered before
                    // ([`Decls::number_imported_tests`]).
                    return match self.test_ordinals.get(&def) {
                        Some(&ordinal) if self.is_alone() => {
                            Ok(mangle::test(&self.def_name(unit, def)?.module, ordinal))
                        }
                        _ => Err(CodegenError::ice("a test body outside a tests unit")),
                    };
                };
                let name = self.def_name(unit, def)?;
                let next = self.next_test.entry(module).or_insert(0);
                let ordinal = *next;
                *next = next.saturating_add(1);
                self.test_ordinals.insert(def, ordinal);
                mangle::test(&name.module, ordinal)
            }
            FnKey::Entry(EntryKind::Main(def)) => {
                mangle::entry_main(&self.def_name(unit, def)?.module)
            }
            FnKey::Entry(EntryKind::Test(def)) => {
                let ordinal = self.test_ordinals.get(&def).copied().ok_or_else(|| {
                    CodegenError::ice("a test entry before its test body was declared")
                })?;
                mangle::entry_test(&self.def_name(unit, def)?.module, ordinal)
            }
            FnKey::Entry(EntryKind::Const(def)) => mangle::entry_const(&self.def_name(unit, def)?),
            FnKey::Entry(EntryKind::Repl(n)) => mangle::entry_repl(n),
            FnKey::Trampoline(arity) => mangle::trampoline(arity),
            FnKey::TupleDictFn { iface_fn, arity } => {
                mangle::tuple_fn(&self.def_name(unit, iface_fn)?, arity)
            }
            FnKey::DynForward {
                iface_fn,
                dyn_iface,
            } => mangle::dyn_forward(
                &self.def_name(unit, iface_fn)?,
                &self.def_name(unit, dyn_iface)?,
            ),
        })
    }

    fn declare_function<M: Module>(
        &mut self,
        module: &mut M,
        unit: &MirModule,
        function: &Function,
    ) -> Result<(), CodegenError> {
        let key = function.key;
        let abi_sig = function.sig()?;
        if let Some(existing) = self.funcs.get(&key) {
            if shared_key(key) && existing.abi == abi_sig {
                // Defined once per JIT: this unit uses the first copy.
                return Ok(());
            }
            // A REPL redefinition of the same item first registers its new
            // generation ([`Decls::set_session_name`]), which forgets this
            // declaration; anything else is a bug.
            return Err(CodegenError::ice(format!(
                "function {key:?} is declared twice"
            )));
        }
        if let FnKey::Item(def) = key
            && let Some(callee) = unit.callee(def)
            && callee.sig != abi_sig
        {
            return Err(CodegenError::ice(format!(
                "the body of {def:?} does not have its declared signature"
            )));
        }
        let symbol = self.symbol(unit, key)?;
        let signature = sig::signature(&abi_sig, self.call_conv);
        // D-917: in a unit alone, an item, test body or constant initializer
        // that another unit may name is global hidden (the entries unit
        // calls the last two directly); everything else, value shims and
        // the other copies of D-918 included, is local.
        let linkage = match key {
            FnKey::Item(def) | FnKey::Test(def) | FnKey::ConstInit(def) => self.item_linkage(def),
            _ => Linkage::Local,
        };
        let id = module
            .declare_function(&symbol, linkage, &signature)
            .map_err(|e| CodegenError::module(&e))?;
        self.names.insert((0, id.as_u32()), symbol.clone());
        if let FnKey::Item(def) = key {
            self.declare_slot(module, unit, def, &abi_sig, id, linkage)?;
        }
        for (sid, text) in function.strings.iter_enumerated() {
            let string_symbol = mangle::string(&symbol, sid);
            let data = self.data(module, &string_symbol, Linkage::Local, false)?;
            self.strings.insert((key, sid), data);
            self.pending_strings
                .push((data, abi::string::static_image(text)));
        }
        self.funcs.insert(
            key,
            FuncDecl {
                id,
                symbol,
                sig: signature,
                abi: abi_sig,
                defined: false,
            },
        );
        Ok(())
    }

    /// Puts the code `id` of the top-level function `def` in its slot
    /// (D-043, D-456): the slot identified by its path and ABI signature
    /// when the session says it takes over an earlier definition, which is
    /// then repointed once the batch is finalized; otherwise a new slot,
    /// `cb$slot$<path>` with the generation of a session item, which
    /// becomes the one later callers of that path and signature load. The
    /// slot's name carries the borrow mask when it is not all-owned
    /// ([`mangle::slot`], D-917), and its linkage is its function's.
    fn declare_slot<M: Module>(
        &mut self,
        module: &mut M,
        unit: &MirModule,
        def: DefId,
        abi_sig: &AbiSig,
        id: FuncId,
        linkage: Linkage,
    ) -> Result<(), CodegenError> {
        let key = SlotKey {
            path: self.slot_path(unit, def)?.into(),
            sig: abi_sig.clone(),
        };
        let takes_over = self.session.get(def).is_some_and(|n| n.reuse.is_some());
        if takes_over && let Some(slot) = self.slots.get_mut(&key) {
            slot.func = id;
            self.pending_repoints.push((slot.id, id, def));
            self.def_slots.insert(def, key);
            return Ok(());
        }
        let slot_symbol = mangle::slot(&self.def_name(unit, def)?, abi_sig.borrow);
        let slot = self.data(module, &slot_symbol, linkage, true)?;
        self.pending_slots.push((slot, id));
        self.slot_sigs.insert(slot, abi_sig.clone());
        self.slots.insert(
            key.clone(),
            SlotDecl {
                id: slot,
                symbol: slot_symbol,
                func: id,
            },
        );
        self.def_slots.insert(def, key);
        Ok(())
    }

    /// Declares the `cb$layout$` word of a user constructor and, for a
    /// nullary one, its `cb$ctor$` static (D-454). A layout registered by
    /// an earlier batch keeps its objects.
    fn declare_layout<M: Module>(
        &mut self,
        module: &mut M,
        unit: &MirModule,
        desc: &LayoutDesc,
    ) -> Result<(), CodegenError> {
        let LayoutRef::Ctor(def) = desc.layout else {
            return Err(CodegenError::ice(format!(
                "the unit lists {:?} among its constructor layouts",
                desc.layout
            )));
        };
        if self.layouts.contains_key(&desc.layout) || builtin_layout_id(&desc.layout).is_some() {
            return Ok(());
        }
        let name = self.def_name(unit, def)?;
        let linkage = self.owned_data_linkage();
        let word = self.data(module, &mangle::item(Kind::Layout, &name), linkage, true)?;
        // D-920: an executable's start patches the `cb$ctor$` header, so
        // in a unit alone the static is writable; the JIT fills it before
        // any code runs and keeps it read-only.
        let ctor = if desc.fields.is_empty() {
            Some(self.data(
                module,
                &mangle::item(Kind::Ctor, &name),
                linkage,
                self.is_alone(),
            )?)
        } else {
            None
        };
        let decl = LayoutDecl { word, ctor };
        self.pending_layouts
            .push((desc.layout, decl.clone(), desc.value_fields));
        self.layouts.insert(desc.layout, decl);
        Ok(())
    }

    /// In a unit alone (D-917), numbers the test bodies another unit
    /// defines that `unit` names (the entries unit's test thunks), before
    /// any function is declared: a test's symbol carries its ordinal in
    /// its tests unit, which lowers its tests sorted by `DefId`, so the
    /// ordinal is the test's rank among the tests of its module that this
    /// unit names, sorted the same way. The entries unit and the tests
    /// units of one build hold the same selected tests.
    fn number_imported_tests(&mut self, unit: &MirModule) {
        if matches!(unit.unit, UnitKind::Tests(_)) {
            return;
        }
        let mut tests = std::collections::BTreeSet::new();
        for function in &unit.functions {
            if let FnKey::Entry(EntryKind::Test(def)) = function.key {
                tests.insert(def);
            }
            for_each_code_key(function, |key| {
                if let FnKey::Test(def) = key {
                    tests.insert(def);
                }
            });
        }
        for def in tests {
            if self.test_ordinals.contains_key(&def) {
                continue;
            }
            let next = self.next_test.entry(def.module).or_insert(0);
            self.test_ordinals.insert(def, *next);
            *next = next.saturating_add(1);
        }
    }

    /// In a unit alone (D-917), declares with `Linkage::Import` what
    /// another unit defines and `unit`'s code reaches: the slot of every
    /// direct callee whose function is not the unit's own, and the code of
    /// every function the unit calls or names directly without defining it
    /// (the entries unit's test bodies and constant initializers, the only
    /// direct calls across units, D-916). Text reaches both PC-relative,
    /// since every generated symbol is colocated (D-913).
    ///
    /// # Errors
    ///
    /// An ICE if the unit names code of another unit that has no fixed
    /// signature, or the module's error.
    fn declare_code_imports<M: Module>(
        &mut self,
        module: &mut M,
        unit: &MirModule,
    ) -> Result<(), CodegenError> {
        for callee in &unit.callees {
            let key = SlotKey {
                path: self.slot_path(unit, callee.def)?.into(),
                sig: callee.sig.clone(),
            };
            if self.slots.contains_key(&key) || self.imported_slots.contains_key(&key) {
                continue;
            }
            let symbol = mangle::slot(&self.def_name(unit, callee.def)?, callee.sig.borrow);
            let id = self.data(module, &symbol, Linkage::Import, true)?;
            self.slot_sigs.insert(id, callee.sig.clone());
            self.imported_slots.insert(key, id);
        }
        let mut keys = Vec::new();
        for function in &unit.functions {
            for_each_code_key(function, |key| keys.push(key));
        }
        for object in &unit.statics {
            for field in &object.fields {
                if let Const::Code(key) = field {
                    keys.push(*key);
                }
            }
        }
        for key in keys {
            if self.funcs.contains_key(&key) {
                continue;
            }
            let abi_sig = cheby_mir::fixed_sig(key).ok_or_else(|| {
                CodegenError::ice(format!(
                    "the unit calls {key:?}, which another unit defines, directly"
                ))
            })?;
            let symbol = self.symbol(unit, key)?;
            let signature = sig::signature(&abi_sig, self.call_conv);
            let id = module
                .declare_function(&symbol, Linkage::Import, &signature)
                .map_err(|e| CodegenError::module(&e))?;
            self.names.insert((0, id.as_u32()), symbol.clone());
            self.funcs.insert(
                key,
                FuncDecl {
                    id,
                    symbol,
                    sig: signature,
                    abi: abi_sig,
                    // Another unit defines it: never defined here.
                    defined: true,
                },
            );
        }
        Ok(())
    }

    /// In a unit alone (D-917), declares with `Linkage::Import` the
    /// `cb$layout$` word and, for a nullary constructor, the `cb$ctor$`
    /// static of every user constructor another unit declares that
    /// `unit`'s code or statics use ([`MirModule::foreign_layouts`]): its
    /// own unit defines them, global hidden, and an executable's start
    /// patches them (D-920).
    ///
    /// # Errors
    ///
    /// The module's error, or an ICE if a def has no name in the unit.
    fn declare_layout_imports<M: Module>(
        &mut self,
        module: &mut M,
        unit: &MirModule,
    ) -> Result<(), CodegenError> {
        for desc in &unit.foreign_layouts {
            let LayoutRef::Ctor(def) = desc.layout else {
                continue;
            };
            if self.layouts.contains_key(&desc.layout) || builtin_layout_id(&desc.layout).is_some()
            {
                continue;
            }
            let name = self.def_name(unit, def)?;
            let word = self.data(
                module,
                &mangle::item(Kind::Layout, &name),
                Linkage::Import,
                true,
            )?;
            let ctor = if desc.fields.is_empty() {
                Some(self.data(
                    module,
                    &mangle::item(Kind::Ctor, &name),
                    Linkage::Import,
                    true,
                )?)
            } else {
                None
            };
            self.layouts.insert(desc.layout, LayoutDecl { word, ctor });
        }
        Ok(())
    }

    /// Declares the slot of every constant the unit initializes or reads
    /// (E6): `cb$const$<module path>$<name>`, a writable word, 0 until the
    /// constant is evaluated.
    fn declare_consts<M: Module>(
        &mut self,
        module: &mut M,
        unit: &MirModule,
    ) -> Result<(), CodegenError> {
        let mut defs = Vec::new();
        // The constants this unit initializes, whose slots it defines.
        let mut own = FxHashSet::default();
        for function in &unit.functions {
            if let FnKey::ConstInit(def) = function.key {
                defs.push(def);
                own.insert(def);
            }
            for block in &function.blocks {
                for stmt in &block.stmts {
                    if let Stmt::Assign {
                        value: Rvalue::LoadConst(def),
                        ..
                    } = stmt
                    {
                        defs.push(*def);
                    }
                }
            }
        }
        for def in defs {
            if self.consts.contains_key(&def) {
                continue;
            }
            // A REPL constant with the type of the one it replaces shares
            // its slot, so readers see the new value (D-547).
            if let Some(old) = self.session.get(def).and_then(|n| n.reuse)
                && let Some(&id) = self.consts.get(&old)
            {
                self.consts.insert(def, id);
                continue;
            }
            let symbol = mangle::item(Kind::Const, &self.def_name(unit, def)?);
            // D-917, D-918: in a unit alone, the unit that initializes a
            // constant defines its slot, global hidden, and every other unit
            // that reads it imports the slot.
            if self.is_alone() && !own.contains(&def) {
                let id = self.data(module, &symbol, Linkage::Import, true)?;
                self.consts.insert(def, id);
                continue;
            }
            let id = self.data(module, &symbol, self.owned_data_linkage(), true)?;
            self.pending_consts.push(id);
            self.consts.insert(def, id);
        }
        Ok(())
    }

    /// Declares the static closure `cb$fnval$…` of every function value the
    /// unit's code or statics use (D-453, D-523), and imports every builtin
    /// nominal descriptor they use.
    fn declare_fn_values<M: Module>(
        &mut self,
        module: &mut M,
        unit: &MirModule,
    ) -> Result<(), CodegenError> {
        let mut defs = Vec::new();
        let mut descs = Vec::new();
        let mut add = |c: &Const| match c {
            Const::FnValue(def) => defs.push(*def),
            Const::BuiltinDesc(desc) => descs.push(*desc),
            _ => {}
        };
        for function in &unit.functions {
            for block in &function.blocks {
                for stmt in &block.stmts {
                    if let Stmt::Assign { value, .. } | Stmt::Eval(value) = stmt {
                        for op in value.operands() {
                            if let Operand::Const(c) = &op {
                                add(c);
                            }
                        }
                    }
                }
                for op in terminator_consts(&block.term) {
                    add(op);
                }
            }
        }
        for object in &unit.statics {
            for field in &object.fields {
                add(field);
            }
        }
        for def in defs {
            self.declare_fn_value(module, unit, def)?;
        }
        // The `@std_hooks` functions of the unit's own types, which the
        // JIT registers with the runtime's layout table (D-482). Their
        // value shims come from lowering; a hook without one is skipped,
        // and its type then compares and prints structurally.
        for desc in &unit.layouts {
            for def in desc.hooks.into_iter().flatten() {
                if self.funcs.contains_key(&FnKey::ValueShim(def)) {
                    self.declare_fn_value(module, unit, def)?;
                }
            }
        }
        for desc in descs {
            if !self.builtin_descs.contains_key(&desc) {
                let symbol = builtin_desc_symbol(desc);
                let id = self.data(module, &symbol, Linkage::Import, false)?;
                self.builtin_descs.insert(desc, id);
                self.import_word(module, &symbol, ImportTarget::Data)?;
            }
        }
        Ok(())
    }

    fn declare_fn_value<M: Module>(
        &mut self,
        module: &mut M,
        unit: &MirModule,
        def: DefId,
    ) -> Result<DataId, CodegenError> {
        let symbol = mangle::item(Kind::FnVal, &self.def_name(unit, def)?);
        if let Some(&id) = self.statics.get(&symbol) {
            return Ok(id);
        }
        let shim = self.func(FnKey::ValueShim(def))?.id;
        let id = self.data(module, &symbol, Linkage::Local, false)?;
        self.pending_statics.push(PendingStatic {
            id,
            header: header(abi::RC_STATIC, 1, abi::layout::CLOSURE)?,
            fields: vec![StaticWord::Code(shim)],
        });
        self.statics.insert(symbol, id);
        Ok(id)
    }

    /// The symbol of a static object (D-523, D-579): `cb$<kind>$<module>$$<name>`,
    /// with the module MIR chose (the unit's, or the type's or interface's
    /// for a descriptor), and the paths of REPL session items written with
    /// their generations (D-547, `crate::session`).
    pub(crate) fn static_symbol(&self, object: &StaticObject) -> String {
        let kind = match object.kind {
            StaticKind::Dict => Kind::Dict,
            StaticKind::Desc => Kind::Desc,
            StaticKind::FnValue => Kind::FnVal,
        };
        match self
            .session
            .static_parts(kind == Kind::Desc, &object.module, &object.name)
        {
            Some((module, name)) => mangle::static_object(kind, &module, &name),
            None => mangle::static_object(kind, &object.module, &object.name),
        }
    }

    /// Declares the unit's static objects (D-523), with their own strings:
    /// each is defined once per JIT, so a symbol an earlier unit or batch
    /// defined is reused when its contents are the same, and is an
    /// internal error otherwise (D-579).
    fn declare_statics<M: Module>(
        &mut self,
        module: &mut M,
        unit: &MirModule,
    ) -> Result<(), CodegenError> {
        let mut new = FxHashSet::default();
        let mut ids = Vec::with_capacity(unit.statics.len());
        for object in &unit.statics {
            let symbol = self.static_symbol(object);
            let print = static_print(self, unit, object);
            match self.static_prints.get(&symbol) {
                Some(earlier) if *earlier != print => {
                    return Err(CodegenError::ice(format!(
                        "the static `{symbol}` is defined twice with different contents: \
                         {earlier} and {print}"
                    )));
                }
                Some(_) => {}
                None => {
                    self.static_prints.insert(symbol.clone(), print);
                }
            }
            let id = if let Some(&id) = self.statics.get(&symbol) {
                id
            } else {
                let id = self.data(module, &symbol, Linkage::Local, false)?;
                self.statics.insert(symbol, id);
                new.insert(id);
                id
            };
            ids.push(id);
        }
        for (object, &id) in unit.statics.iter().zip(&ids) {
            if !new.contains(&id) {
                continue;
            }
            let symbol = self.static_symbol(object);
            let mut strings = Vec::with_capacity(object.strings.len());
            for (sid, text) in object.strings.iter_enumerated() {
                let data =
                    self.data(module, &mangle::string(&symbol, sid), Linkage::Local, false)?;
                self.pending_strings
                    .push((data, abi::string::static_image(text)));
                strings.push(data);
            }
            let mut fields = object
                .fields
                .iter()
                .map(|field| self.static_word(unit, &ids, &strings, field))
                .collect::<Result<Vec<_>, _>>()?;
            let layout = static_layout_id(object.layout)?;
            let count = u64::try_from(fields.len()).unwrap_or(u64::MAX);
            // A static object has the shape allocation gives the same
            // layout, so field reads need not know which one they read.
            let header = if let Some(index) = escaped_count_index(object.layout, count) {
                let after = fields.len().checked_sub(index).ok_or_else(|| {
                    CodegenError::ice("an escaped static object without its first field")
                })?;
                let tagged = i64::try_from(after)
                    .ok()
                    .and_then(abi::tag_int)
                    .ok_or_else(|| CodegenError::ice("a static object too large to count"))?;
                fields.insert(index, StaticWord::Word(tagged));
                abi::header(abi::RC_STATIC, abi::SCAN_ESCAPE, layout)
            } else {
                header(abi::RC_STATIC, count, layout)?
            };
            self.pending_statics
                .push(PendingStatic { id, header, fields });
        }
        Ok(())
    }

    /// The word a constant field of a static object holds; a string is one
    /// of the static's own (`strings`).
    ///
    /// GAP(static-fields-runtime-symbols): a field that holds a runtime
    /// static (`Nil`, the empty list, a builtin constructor or descriptor)
    /// names it with the static object's own `Abs8` data relocation, as an
    /// import word does, since the field must hold the static's address
    /// itself; only text goes through import words (D-913, D-917).
    fn static_word(
        &self,
        unit: &MirModule,
        statics: &[DataId],
        strings: &[DataId],
        field: &Const,
    ) -> Result<StaticWord, CodegenError> {
        let nth = |ids: &[DataId], n: u32, what: &str| {
            usize::try_from(n)
                .ok()
                .and_then(|n| ids.get(n))
                .copied()
                .ok_or_else(|| CodegenError::ice(format!("unknown {what} {n} in a static")))
        };
        Ok(match field {
            Const::Nil => StaticWord::Data(self.static_data(abi::sym::STATIC_NIL)?),
            Const::EmptyList => StaticWord::Data(self.static_data(abi::sym::STATIC_EMPTY_LIST)?),
            Const::StaticCtor(def) => StaticWord::Data(self.ctor_object(*def)?),
            Const::FnValue(def) => StaticWord::Data(
                self.static_object(&mangle::item(Kind::FnVal, &self.def_name(unit, *def)?))?,
            ),
            Const::Static(id) => StaticWord::Data(nth(statics, id.as_u32(), "static")?),
            Const::Str(id) => StaticWord::Data(nth(strings, id.as_u32(), "string")?),
            Const::BuiltinDesc(desc) => StaticWord::Data(self.builtin_desc(*desc)?),
            Const::ErrNil => StaticWord::Data(self.static_data(abi::sym::STATIC_ERR_NIL)?),
            Const::Code(key) => StaticWord::Code(self.func(*key)?.id),
            Const::Int(_) | Const::Float(_) | Const::Bool(_) | Const::Sized { .. } | Const::Ctx => {
                return Err(CodegenError::ice(format!(
                    "a static object holds the raw constant {field:?}"
                )));
            }
        })
    }

    /// The static object of the nullary constructor `def`: the runtime's for
    /// a builtin one, `cb$ctor$…` for a user one.
    pub(crate) fn ctor_object(&self, def: DefId) -> Result<DataId, CodegenError> {
        if let Some(id) = builtin_layout_id(&LayoutRef::Ctor(def)) {
            let symbol = abi::sym::static_ctor(id).ok_or_else(|| {
                CodegenError::ice(format!("the builtin constructor {def:?} is not nullary"))
            })?;
            return self.static_data(symbol);
        }
        self.layout(LayoutRef::Ctor(def))?
            .ctor
            .ok_or_else(|| CodegenError::ice(format!("the constructor {def:?} is not nullary")))
    }

    /// Declares the foreign symbols `function` calls, each with its import
    /// word (D-913). Under [`ForeignImport::Function`] a symbol has one C
    /// signature per module; under [`ForeignImport::Address`] the import
    /// carries no signature, so call sites of one symbol may use different
    /// ones (a REPL redefinition, or a failed batch whose declarations
    /// stay).
    fn declare_foreign<M: Module>(
        &mut self,
        module: &mut M,
        function: &Function,
        import: ForeignImport,
    ) -> Result<(), CodegenError> {
        for block in &function.blocks {
            for stmt in &block.stmts {
                let (Stmt::Assign {
                    value:
                        Rvalue::CallForeign {
                            symbol,
                            sig,
                            blocking,
                            ..
                        },
                    ..
                }
                | Stmt::Eval(Rvalue::CallForeign {
                    symbol,
                    sig,
                    blocking,
                    ..
                })) = stmt
                else {
                    continue;
                };
                if *blocking {
                    self.declare_blocking_shim(module, symbol, sig)?;
                }
                let signature = sig::foreign_signature(sig, self.call_conv);
                if let Some(existing) = self.foreign.get(symbol) {
                    if let Some(existing) = &existing.signature
                        && *existing != signature
                    {
                        return Err(CodegenError::Module(format!(
                            "the native symbol `{symbol}` is declared with two different signatures"
                        )));
                    }
                    continue;
                }
                // D-913: through its import word in both modes; the mode
                // decides only how the symbol itself is declared.
                let decl = match import {
                    ForeignImport::Function => ForeignDecl {
                        word: self.import_word(
                            module,
                            symbol,
                            ImportTarget::Function(&signature),
                        )?,
                        signature: Some(signature),
                    },
                    ForeignImport::Address => ForeignDecl {
                        word: self.import_word(module, symbol, ImportTarget::Data)?,
                        signature: None,
                    },
                };
                self.foreign.insert(symbol.clone(), decl);
            }
        }
        Ok(())
    }

    /// Declares the shim of the blocking foreign function `symbol` called
    /// with the C signature `sig`, and `cheby_rt_blocking_call`, unless an
    /// earlier unit or batch did (`blocking.rs`, D-818). The
    /// first signature of a symbol names its shim `cb$blk$<symbol>`; a
    /// later one, which only signature-free imports allow, gets the next
    /// number ([`mangle::blocking_shim`]).
    fn declare_blocking_shim<M: Module>(
        &mut self,
        module: &mut M,
        symbol: &str,
        sig: &ForeignSig,
    ) -> Result<(), CodegenError> {
        let key = (Box::<str>::from(symbol), sig.clone());
        if self.blocking_shims.contains_key(&key) {
            return Ok(());
        }
        self.declare_runtime_func(module, RuntimeFunc::BlockingCall)?;
        let variant = self.blocking_variants.entry(key.0.clone()).or_insert(0);
        let name = mangle::blocking_shim(symbol, *variant);
        *variant = variant.saturating_add(1);
        let signature = crate::blocking::shim_signature(self.call_conv);
        let id = module
            .declare_function(&name, Linkage::Local, &signature)
            .map_err(|e| CodegenError::module(&e))?;
        self.names.insert((0, id.as_u32()), name);
        self.blocking_shims.insert(key.clone(), id);
        self.pending_shims.push((id, key.0, key.1));
        Ok(())
    }
}

/// Calls `f` with the key of every function `function` calls directly
/// ([`cheby_mir::Callee::Code`]) or names as a code word
/// ([`Const::Code`]), in statement order.
fn for_each_code_key(function: &Function, mut f: impl FnMut(FnKey)) {
    use cheby_mir::Callee;
    for block in &function.blocks {
        for stmt in &block.stmts {
            if let Stmt::Assign { value, .. } | Stmt::Eval(value) = stmt {
                if let Rvalue::Call {
                    callee: Callee::Code(key),
                    ..
                } = value
                {
                    f(*key);
                }
                for op in value.operands() {
                    if let Operand::Const(Const::Code(key)) = op {
                        f(key);
                    }
                }
            }
        }
        if let Terminator::TailCall {
            callee: Callee::Code(key),
            ..
        } = &block.term
        {
            f(*key);
        }
        for c in terminator_consts(&block.term) {
            if let Const::Code(key) = c {
                f(*key);
            }
        }
    }
}

/// The constants a terminator reads.
fn terminator_consts(term: &Terminator) -> Vec<&Const> {
    let ops: Vec<&Operand> = match term {
        Terminator::Return(op) => vec![op],
        Terminator::TailCall { args, .. } | Terminator::Goto { args, .. } => args.iter().collect(),
        Terminator::Branch { cond, .. } => vec![cond],
        Terminator::Switch { on, .. } => vec![on],
        Terminator::Panic { message, .. } => message.iter().collect(),
        Terminator::Unreachable | Terminator::Resume => Vec::new(),
    };
    ops.into_iter()
        .filter_map(|op| match op {
            Operand::Const(c) => Some(c),
            Operand::Local(_) => None,
        })
        .collect()
}

/// `cheby_rt_desc_<name>` (D-523).
pub(crate) fn builtin_desc_symbol(desc: BuiltinDesc) -> String {
    format!("{}{}", abi::sym::DESC_PREFIX, desc.name())
}

/// The fixed layout id of a static object's layout (D-523, D-524).
fn static_layout_id(layout: LayoutRef) -> Result<u32, CodegenError> {
    match layout {
        LayoutRef::Dict { .. } => Ok(abi::layout::DICT),
        LayoutRef::Desc { .. } => Ok(abi::layout::DESC),
        LayoutRef::DescNominal => Ok(abi::layout::DESC_NOMINAL),
        LayoutRef::Closure(_) => Ok(abi::layout::CLOSURE),
        LayoutRef::Dyn | LayoutRef::Tuple(_) | LayoutRef::Ctor(_) | LayoutRef::Builtin(_) => Err(
            CodegenError::ice(format!("a static object with layout {layout:?}")),
        ),
    }
}
