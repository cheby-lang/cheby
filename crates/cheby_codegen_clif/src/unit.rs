//! Units compiled alone (D-917, D-918, D-920): one MIR unit into a module
//! that holds nothing else, so its code record stands alone and any
//! program that needs the module reuses it (D-914, ADR-0047).
//!
//! The JIT's batches ([`crate::declare`], [`crate::define`]) never take
//! this path; they keep their one-module mode.

use cranelift_module::Module;
use rustc_hash::FxHashSet;

use cheby_iface::DefId;
use cheby_mir::{FnKey, MirModule};

use crate::{CodegenError, CodegenOptions, DefineStats, DefinedCode};

/// What compiling a unit alone needs besides its MIR (D-917). MIR carries
/// no visibility, so the caller answers it from the front end.
#[derive(Clone, Copy)]
pub struct UnitCx<'a> {
    /// Whether another unit of a program may name `def`, an item of this
    /// unit: package-visible and `pub` items, `main`, tests and constant
    /// initializers (the entries unit calls the last three), and every
    /// item the module's own tests unit names. Their functions and slots
    /// are global hidden (`Linkage::Hidden`); every other own function is
    /// local.
    pub named_outside: &'a (dyn Fn(DefId) -> bool + Sync),
}

impl std::fmt::Debug for UnitCx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnitCx").finish_non_exhaustive()
    }
}

/// One unit compiled alone.
#[derive(Debug)]
pub struct UnitCode {
    /// Every function defined, in definition order, with its length, ABI
    /// and call sites: what the module record lists (D-919).
    pub defined: Vec<DefinedCode>,
    /// The machine code defined.
    pub stats: DefineStats,
    /// The CLIF text of each function under
    /// [`CodegenOptions::capture_clif`].
    pub clif: Vec<String>,
    /// The unit's user types with their layout words, constructor statics
    /// and hooks, as the module record lists them (D-919).
    pub(crate) types: Vec<crate::modrec::RecordType>,
    /// The unit's external types, as the module record lists them (D-919):
    /// `MirModule::externals`.
    pub(crate) externals: Vec<cheby_mir::layout::ExternalDesc>,
    /// The constants the unit initializes, in function (`DefIndex`)
    /// order: each one's path `module::name` and the slot the unit
    /// defines, which the module record lists (D-979,
    /// D-1189).
    pub(crate) consts: Vec<(String, cranelift_module::DataId)>,
}

/// Compiles `unit` alone into `module`, which holds nothing else (D-917,
/// D-918): own definitions local or global hidden, every other unit's
/// slots, layout words, `cb$ctor$` statics, constant slots and directly
/// called code imported, code and statics fixed by their key copied
/// locally, import words local, layout words and `cb$ctor$` statics with 0
/// for the layout id (D-920). The result depends on no other unit and on
/// no layout id.
///
/// # Errors
///
/// A [`CodegenError`] if the unit is inconsistent, or translation,
/// compilation or a definition fails.
pub fn compile_unit<M: Module>(
    module: &mut M,
    unit: &MirModule,
    ucx: &UnitCx<'_>,
    opts: &CodegenOptions,
) -> Result<UnitCode, CodegenError> {
    // The unit's own items that another unit may name: those whose
    // function is global hidden (D-1110). Asked once
    // per item, in function order; the set is only looked up.
    let mut named = FxHashSet::default();
    for function in &unit.functions {
        if let FnKey::Item(def) | FnKey::Test(def) | FnKey::ConstInit(def) = function.key
            && (ucx.named_outside)(def)
        {
            named.insert(def);
        }
    }
    let mut decls = crate::Decls::new_alone(module.isa(), named)?;
    let cx = crate::CodegenCx {
        layouts: &NoLayoutIds,
    };
    let units = std::slice::from_ref(unit);
    crate::declare(module, &mut decls, units, &cx, opts)?;
    let (clif, stats, defined) = crate::define_with_stats(module, units, &mut decls, &cx, opts)?;
    let types = crate::modrec::record_types(unit, &decls)?;
    let mut consts = Vec::new();
    for function in &unit.functions {
        if let FnKey::ConstInit(def) = function.key {
            let slot = decls.const_slot(def).ok_or_else(|| {
                CodegenError::ice(format!("the constant {def:?} has no slot in its own unit"))
            })?;
            consts.push((unit.name(def)?.path(), slot));
        }
    }
    Ok(UnitCode {
        defined,
        stats,
        clif,
        types,
        externals: unit.externals.clone(),
        consts,
    })
}

/// The layout ids of a unit alone: none. Its layout words and `cb$ctor$`
/// statics hold 0 where the id goes (D-920), so code generation never
/// asks; a question would be an internal error.
struct NoLayoutIds;

impl crate::LayoutIds for NoLayoutIds {
    fn id(&self, layout: &cheby_mir::LayoutRef) -> Result<u32, cheby_iface::Ice> {
        Err(cheby_iface::Ice::new(format!(
            "a unit compiled alone asked for the layout id of {layout:?} (D-920)"
        )))
    }
}
