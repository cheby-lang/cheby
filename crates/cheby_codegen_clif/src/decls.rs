//! Declarations that persist across batches of units (E1, E4).

use std::fmt::Write as _;

use cranelift_codegen::ir::{self, Signature};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig, TargetIsa};
use cranelift_module::{DataId, FuncId, Linkage, Module};
use rustc_hash::FxHashMap;

use cheby_iface::{DefId, ModuleId};
use cheby_mir::{AbiSig, EntryKind, FnKey, Function, MirModule, Rvalue, Stmt, StrId, UnitKind};
use cheby_runtime::abi;

use crate::CodegenError;
use crate::mangle::{self, Kind};
use crate::runtime::{RuntimeFunc, STATIC_DATA};
use crate::sig;

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

/// A declared indirection slot (D-043).
#[derive(Clone, Debug)]
pub struct SlotDecl {
    pub id: DataId,
    pub symbol: String,
    /// The function the slot initially points to.
    pub func: FuncId,
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
    foreign: FxHashMap<Box<str>, (FuncId, Signature)>,
    test_ordinals: FxHashMap<DefId, u32>,
    next_test: FxHashMap<ModuleId, u32>,
    /// Symbol of every declared function (namespace 0) and data object
    /// (namespace 1), for CLIF legends.
    names: FxHashMap<(u32, u32), String>,
    pub(crate) pending_strings: Vec<(DataId, Vec<u8>)>,
    pub(crate) pending_slots: Vec<(DataId, FuncId)>,
}

impl std::fmt::Debug for Decls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decls")
            .field("call_conv", &self.call_conv)
            .field("funcs", &self.funcs.len())
            .field("slots", &self.slots.len())
            .field("strings", &self.strings.len())
            .finish_non_exhaustive()
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
            names: FxHashMap::default(),
            pending_strings: Vec::new(),
            pending_slots: Vec::new(),
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

    pub(crate) fn foreign(&self, symbol: &str) -> Result<(FuncId, &Signature), CodegenError> {
        self.foreign
            .get(symbol)
            .map(|(id, s)| (*id, s))
            .ok_or_else(|| CodegenError::ice(format!("foreign `{symbol}` was not declared")))
    }

    /// Every foreign symbol declared so far, sorted.
    #[must_use]
    pub fn foreign_symbols(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.foreign.keys().map(AsRef::as_ref).collect();
        out.sort_unstable();
        out
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
    ) -> Result<(), CodegenError> {
        self.declare_runtime(module)?;
        for unit in mirs {
            for function in &unit.functions {
                self.declare_function(module, unit, function)?;
            }
        }
        for unit in mirs {
            for function in &unit.functions {
                self.declare_foreign(module, function)?;
            }
        }
        Ok(())
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
            let id = module
                .declare_data(symbol, Linkage::Import, false, false)
                .map_err(|e| CodegenError::module(&e))?;
            self.names.insert((1, id.as_u32()), symbol.to_owned());
            self.static_data.insert(symbol, id);
        }
        Ok(())
    }

    fn symbol(&mut self, unit: &MirModule, key: FnKey) -> Result<String, CodegenError> {
        let name = |def| unit.name(def).map_err(CodegenError::from);
        Ok(match key {
            FnKey::Item(def) => mangle::item(Kind::Fn, name(def)?),
            FnKey::ValueShim(def) => mangle::item(Kind::Shim, name(def)?),
            FnKey::Closure { owner, index } => mangle::closure(name(owner)?, index),
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
        })
    }

    fn declare_function<M: Module>(
        &mut self,
        module: &mut M,
        unit: &MirModule,
        function: &Function,
    ) -> Result<(), CodegenError> {
        let key = function.key;
        if self.funcs.contains_key(&key) {
            // A REPL redefinition gets a new key generation (D-456);
            // until then a second declaration is a bug.
            return Err(CodegenError::ice(format!(
                "function {key:?} is declared twice"
            )));
        }
        let abi_sig = function.sig()?;
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
            let slot = module
                .declare_data(&slot_symbol, Linkage::Local, true, false)
                .map_err(|e| CodegenError::module(&e))?;
            self.names.insert((1, slot.as_u32()), slot_symbol.clone());
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
            let data = module
                .declare_data(&string_symbol, Linkage::Local, false, false)
                .map_err(|e| CodegenError::module(&e))?;
            self.names.insert((1, data.as_u32()), string_symbol);
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

    fn declare_foreign<M: Module>(
        &mut self,
        module: &mut M,
        function: &Function,
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
                if let Some((_, existing)) = self.foreign.get(symbol) {
                    if *existing != signature {
                        return Err(CodegenError::Module(format!(
                            "the native symbol `{symbol}` is declared with two different signatures"
                        )));
                    }
                    continue;
                }
                let id = module
                    .declare_function(symbol, Linkage::Import, &signature)
                    .map_err(|e| CodegenError::module(&e))?;
                self.names.insert((0, id.as_u32()), symbol.to_string());
                self.foreign.insert(symbol.clone(), (id, signature));
            }
        }
        Ok(())
    }
}
