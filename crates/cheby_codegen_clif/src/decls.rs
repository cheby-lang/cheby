//! Declarations that persist across batches of units (E1, E4).

use std::fmt::Write as _;

use cranelift_codegen::ir::{self, Signature};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig, TargetIsa};
use cranelift_module::{DataId, FuncId, FuncOrDataId, Linkage, Module};
use rustc_hash::{FxHashMap, FxHashSet};

use cheby_iface::{DefId, ModuleId};
use cheby_mir::{
    AbiSig, BuiltinDesc, Const, EntryKind, FnKey, Function, LayoutDesc, LayoutRef, MirModule,
    Operand, Rvalue, StaticKind, StaticObject, Stmt, StrId, Terminator, UnitKind,
};
use cheby_runtime::abi;

use crate::layout::builtin_layout_id;
use crate::mangle::{self, Kind};
use crate::runtime::{RuntimeFunc, STATIC_DATA};
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

/// How a foreign symbol was imported ([`ForeignImport`]).
#[derive(Clone, Debug)]
pub(crate) enum ForeignDecl {
    /// A function import with its one C signature.
    Function(FuncId, Signature),
    /// The symbol's address as a data import, called indirectly with each
    /// call site's C signature.
    Data(DataId),
    /// The address of a symbol the module already imports as a function
    /// (a runtime function), called indirectly with each call site's C
    /// signature. The declared signature is only needed to name it.
    FunctionAddress(FuncId, Signature),
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
    strings: FxHashMap<(FnKey, StrId), DataId>,
    runtime: Vec<(FuncId, Signature)>,
    static_data: FxHashMap<&'static str, DataId>,
    foreign: FxHashMap<Box<str>, ForeignDecl>,
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
    pub(crate) pending_strings: Vec<(DataId, Vec<u8>)>,
    pub(crate) pending_slots: Vec<(DataId, FuncId)>,
    /// `(layout, its declaration, value fields)` of layouts to define.
    pub(crate) pending_layouts: Vec<(LayoutRef, LayoutDecl, u32)>,
    pub(crate) pending_consts: Vec<DataId>,
    pub(crate) pending_statics: Vec<PendingStatic>,
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
fn static_print(unit: &MirModule, object: &StaticObject) -> String {
    let mut print = format!("{:?} {:?} [", object.layout, object.strings);
    for field in &object.fields {
        match field {
            Const::Static(id) => match unit.static_object(*id) {
                Some(other) => {
                    let _ = write!(print, "{}, ", Decls::static_symbol(other));
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
            strings: FxHashMap::default(),
            runtime: Vec::new(),
            static_data: FxHashMap::default(),
            foreign: FxHashMap::default(),
            test_ordinals: FxHashMap::default(),
            next_test: FxHashMap::default(),
            layouts: FxHashMap::default(),
            consts: FxHashMap::default(),
            statics: FxHashMap::default(),
            static_prints: FxHashMap::default(),
            builtin_descs: FxHashMap::default(),
            names: FxHashMap::default(),
            pending_strings: Vec::new(),
            pending_slots: Vec::new(),
            pending_layouts: Vec::new(),
            pending_consts: Vec::new(),
            pending_statics: Vec::new(),
        })
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
        let symbol = mangle::item(Kind::FnVal, unit.name(def)?);
        Ok(self.statics.get(&symbol).copied())
    }

    pub(crate) fn string(&self, key: FnKey, id: StrId) -> Result<DataId, CodegenError> {
        self.strings
            .get(&(key, id))
            .copied()
            .ok_or_else(|| CodegenError::ice(format!("string {id:?} of {key:?} was not declared")))
    }

    pub(crate) fn runtime(&self, f: RuntimeFunc) -> Result<(FuncId, &Signature), CodegenError> {
        self.runtime
            .get(f.index())
            .map(|(id, s)| (*id, s))
            .ok_or_else(|| CodegenError::ice("runtime functions were not declared"))
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
        let _ = write!(out, "{}", func.display());
        out
    }

    pub(crate) fn declare_units<M: Module>(
        &mut self,
        module: &mut M,
        mirs: &[MirModule],
        import: ForeignImport,
    ) -> Result<(), CodegenError> {
        self.declare_runtime(module)?;
        for unit in mirs {
            for function in &unit.functions {
                self.declare_function(module, unit, function)?;
            }
        }
        for unit in mirs {
            for desc in &unit.layouts {
                self.declare_layout(module, unit, desc)?;
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
        for f in RuntimeFunc::ALL {
            let (params, ret) = f.ctypes();
            let signature = sig::c_signature(params, ret, self.call_conv);
            let id = module
                .declare_function(f.symbol(), Linkage::Import, &signature)
                .map_err(|e| CodegenError::module(&e))?;
            self.names.insert((0, id.as_u32()), f.symbol().to_owned());
            self.runtime.push((id, signature));
        }
        for symbol in STATIC_DATA {
            let id = self.data(module, symbol, Linkage::Import, false)?;
            self.static_data.insert(symbol, id);
        }
        Ok(())
    }

    fn symbol(&mut self, unit: &MirModule, key: FnKey) -> Result<String, CodegenError> {
        let name = |def| unit.name(def).map_err(CodegenError::from);
        Ok(match key {
            FnKey::Item(def) => mangle::item(Kind::Fn, name(def)?),
            FnKey::ValueShim(def) => mangle::item(Kind::Shim, name(def)?),
            // A test's name is free text that may equal a function's, so
            // a test's closures go by its ordinal, like the test (D-456).
            FnKey::Closure { owner, index } => match self.test_ordinals.get(&owner) {
                Some(&ordinal) => mangle::test_closure(&name(owner)?.module, ordinal, index),
                None => mangle::closure(name(owner)?, index),
            },
            FnKey::ConstInit(def) => mangle::item(Kind::Cinit, name(def)?),
            FnKey::Test(def) => {
                let UnitKind::Tests(module) = unit.unit else {
                    return Err(CodegenError::ice("a test body outside a tests unit"));
                };
                let next = self.next_test.entry(module).or_insert(0);
                let ordinal = *next;
                *next = next.saturating_add(1);
                self.test_ordinals.insert(def, ordinal);
                mangle::test(&name(def)?.module, ordinal)
            }
            FnKey::Entry(EntryKind::Main(def)) => mangle::entry_main(&name(def)?.module),
            FnKey::Entry(EntryKind::Test(def)) => {
                let ordinal = self.test_ordinals.get(&def).copied().ok_or_else(|| {
                    CodegenError::ice("a test entry before its test body was declared")
                })?;
                mangle::entry_test(&name(def)?.module, ordinal)
            }
            FnKey::Entry(EntryKind::Const(def)) => mangle::entry_const(name(def)?),
            FnKey::Entry(EntryKind::Repl(n)) => mangle::entry_repl(n),
            FnKey::Trampoline(arity) => mangle::trampoline(arity),
            FnKey::TupleDictFn { iface_fn, arity } => mangle::tuple_fn(name(iface_fn)?, arity),
            FnKey::DynForward { iface_fn } => mangle::dyn_forward(name(iface_fn)?),
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
            // A REPL redefinition gets a new key generation (D-456);
            // until then a second declaration is a bug.
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
        let id = module
            .declare_function(&symbol, Linkage::Local, &signature)
            .map_err(|e| CodegenError::module(&e))?;
        self.names.insert((0, id.as_u32()), symbol.clone());
        if let FnKey::Item(def) = key {
            let name = unit.name(def)?;
            let slot_symbol = mangle::item(Kind::Slot, name);
            let slot = self.data(module, &slot_symbol, Linkage::Local, true)?;
            self.pending_slots.push((slot, id));
            self.slots.insert(
                SlotKey {
                    path: name.path().into(),
                    sig: abi_sig.clone(),
                },
                SlotDecl {
                    id: slot,
                    symbol: slot_symbol,
                    func: id,
                },
            );
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
        let name = unit.name(def)?;
        let word = self.data(
            module,
            &mangle::item(Kind::Layout, name),
            Linkage::Local,
            true,
        )?;
        let ctor = if desc.fields.is_empty() {
            Some(self.data(
                module,
                &mangle::item(Kind::Ctor, name),
                Linkage::Local,
                false,
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

    /// Declares the slot of every constant the unit initializes or reads
    /// (E6): `cb$const$<module path>$<name>`, a writable word, 0 until the
    /// constant is evaluated.
    fn declare_consts<M: Module>(
        &mut self,
        module: &mut M,
        unit: &MirModule,
    ) -> Result<(), CodegenError> {
        let mut defs = Vec::new();
        for function in &unit.functions {
            if let FnKey::ConstInit(def) = function.key {
                defs.push(def);
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
            let symbol = mangle::item(Kind::Const, unit.name(def)?);
            let id = self.data(module, &symbol, Linkage::Local, true)?;
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
                let id = self.data(module, &builtin_desc_symbol(desc), Linkage::Import, false)?;
                self.builtin_descs.insert(desc, id);
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
        let symbol = mangle::item(Kind::FnVal, unit.name(def)?);
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

    /// The symbol of a static object (D-523): `cb$<kind>$<module>$<name>`,
    /// with the module MIR chose (the unit's, or the type's or interface's
    /// for a descriptor).
    pub(crate) fn static_symbol(object: &StaticObject) -> String {
        let kind = match object.kind {
            StaticKind::Dict => Kind::Dict,
            StaticKind::Desc => Kind::Desc,
            StaticKind::FnValue => Kind::FnVal,
        };
        mangle::static_object(kind, &object.module, &object.name)
    }

    /// Declares the unit's static objects (D-523), with their own strings:
    /// each is defined once per JIT, so a symbol an earlier unit or batch
    /// defined is reused.
    fn declare_statics<M: Module>(
        &mut self,
        module: &mut M,
        unit: &MirModule,
    ) -> Result<(), CodegenError> {
        let mut new = FxHashSet::default();
        let mut ids = Vec::with_capacity(unit.statics.len());
        for object in &unit.statics {
            let symbol = Self::static_symbol(object);
            let print = static_print(unit, object);
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
            let symbol = Self::static_symbol(object);
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
            Const::FnValue(def) => {
                StaticWord::Data(self.static_object(&mangle::item(Kind::FnVal, unit.name(*def)?))?)
            }
            Const::Static(id) => StaticWord::Data(nth(statics, id.as_u32(), "static")?),
            Const::Str(id) => StaticWord::Data(nth(strings, id.as_u32(), "string")?),
            Const::BuiltinDesc(desc) => StaticWord::Data(self.builtin_desc(*desc)?),
            Const::Code(key) => StaticWord::Code(self.func(*key)?.id),
            Const::Int(_) | Const::Float(_) | Const::Bool(_) | Const::Sized { .. } => {
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

    /// Declares the foreign symbols `function` calls. Under
    /// [`ForeignImport::Function`] a symbol has one C signature per module;
    /// under [`ForeignImport::Address`] the import carries no signature, so
    /// call sites of one symbol may use different ones (a REPL
    /// redefinition, or a failed batch whose declarations stay).
    fn declare_foreign<M: Module>(
        &mut self,
        module: &mut M,
        function: &Function,
        import: ForeignImport,
    ) -> Result<(), CodegenError> {
        for block in &function.blocks {
            for stmt in &block.stmts {
                let (Stmt::Assign {
                    value: Rvalue::CallForeign { symbol, sig, .. },
                    ..
                }
                | Stmt::Eval(Rvalue::CallForeign { symbol, sig, .. })) = stmt
                else {
                    continue;
                };
                let signature = sig::foreign_signature(sig, self.call_conv);
                if let Some(existing) = self.foreign.get(symbol) {
                    if let ForeignDecl::Function(_, existing) = existing
                        && *existing != signature
                    {
                        return Err(CodegenError::Module(format!(
                            "the native symbol `{symbol}` is declared with two different signatures"
                        )));
                    }
                    continue;
                }
                let decl = match import {
                    ForeignImport::Function => {
                        let id = module
                            .declare_function(symbol, Linkage::Import, &signature)
                            .map_err(|e| CodegenError::module(&e))?;
                        self.names.insert((0, id.as_u32()), symbol.to_string());
                        ForeignDecl::Function(id, signature)
                    }
                    ForeignImport::Address => {
                        if let Some(FuncOrDataId::Func(id)) = module.get_name(symbol) {
                            // Already a function import (a runtime function
                            // such as `cheby_rt_alloc`): take its address.
                            let declared = module.declarations().get_function_decl(id);
                            ForeignDecl::FunctionAddress(id, declared.signature.clone())
                        } else {
                            ForeignDecl::Data(self.data(module, symbol, Linkage::Import, false)?)
                        }
                    }
                };
                self.foreign.insert(symbol.clone(), decl);
            }
        }
        Ok(())
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
        Terminator::Unreachable => Vec::new(),
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
