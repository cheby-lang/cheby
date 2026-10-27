//! Units compiled alone for a JIT host (K4c, D-914, D-923): every JIT host
//! compiles each unit it needs alone into a code record and loads the
//! records into its `JITModule` (`cheby_jit`), so fresh and cached code take
//! one load path (D-1217).
//!
//! [`record_for_jit`] compiles one unit, a code, tests or entries unit, into
//! its [`CodeRecord`] and a [`UnitIndex`]: where the loader finds the
//! unit's functions, slots, constant slots and layouts among the record's
//! symbols, which a JIT host looks up by `FnKey` and `DefId`
//! (`Jit::entry`, `Jit::const_slot`, the REPL's slots). A record that comes
//! from the store has no index; nothing of a cached unit is looked up by
//! key but its constants, which the driver names by symbol.
//!
//! A unit for a JIT that holds earlier batches (the REPL) gets a [`Seed`]:
//! its session names (D-547), the slots, constant slots and layouts the
//! earlier batches defined, the external types they registered, and test
//! ordinals (D-1179). The unit imports what the seed names instead of
//! defining it again, and a redefinition that takes over a slot imports
//! that slot and lists the repoint in its index (D-043, D-712;
//! D-1225). Without a seed, the record is the one the cache
//! stores: [`crate::record::record_unit`]'s for a code or tests unit.

use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_module::{DataId, FuncId};
use rustc_hash::{FxHashMap, FxHashSet};

use cheby_iface::DefId;
use cheby_mir::{FnKey, LayoutRef, MirModule};

use crate::decls::IndexParts;
use crate::record::{CodeRecord, RecordError, RecordingModule};
use crate::{CodegenOptions, DefineStats, SessionName, SlotKey, UnitCx};

/// What a JIT that holds earlier batches tells a unit compiled alone for
/// it (module docs). Every map is looked up only, never iterated for
/// output.
#[derive(Clone, Debug, Default)]
pub struct Seed {
    /// The session names of REPL items (D-547, E4).
    pub session: FxHashMap<DefId, SessionName>,
    /// The symbol of each slot an earlier batch defined, by slot key
    /// (D-456): a redefinition that takes one over imports it.
    pub slots: FxHashMap<SlotKey, String>,
    /// The symbol of each constant slot an earlier batch defined, or that a
    /// session value lives in (D-547).
    pub consts: FxHashMap<DefId, String>,
    /// The symbols of each user constructor layout an earlier batch
    /// declared: its `cb$layout$` word and, for a nullary constructor, its
    /// `cb$ctor$` static.
    pub layouts: FxHashMap<LayoutRef, (String, Option<String>)>,
    /// The external types an earlier batch registered (D-1169).
    pub externals: FxHashSet<DefId>,
    /// The ordinal of each test among all of its module's tests on native,
    /// in source order (D-1179), for a unit that names tests another unit
    /// defines.
    pub test_ordinals: FxHashMap<DefId, u32>,
}

/// Where a JIT's loader finds a unit's functions and data among its
/// record's symbols (indices into [`CodeRecord::symbols`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnitIndex {
    /// Every function the unit declares or imports, by key.
    pub funcs: Vec<(FnKey, u32)>,
    /// The slot each top-level function of the unit was put in, with the
    /// slot's key (D-456).
    pub slots: Vec<(DefId, SlotKey, u32)>,
    /// Every constant slot the unit declares or imports.
    pub consts: Vec<(DefId, u32)>,
    /// Every user constructor layout the unit declares or imports: its
    /// word and, for a nullary constructor, its static.
    pub layouts: Vec<(LayoutRef, u32, Option<u32>)>,
    /// The external types the unit's module record registers.
    pub externals: Vec<DefId>,
    /// The slots the unit's REPL redefinitions take over, with their new
    /// code and the redefinition (D-043): the JIT points each slot at its
    /// code once the batch is loaded.
    pub repoints: Vec<(u32, u32, DefId)>,
}

/// One unit compiled alone for a JIT host.
#[derive(Debug)]
pub struct JitUnit {
    /// The unit's code record.
    pub record: CodeRecord,
    /// Where its functions and data are among the record's symbols.
    pub index: UnitIndex,
    /// The CLIF text of each function, under
    /// [`CodegenOptions::capture_clif`].
    pub clif: Vec<String>,
    /// The machine code compiled.
    pub stats: DefineStats,
}

/// `unit`, a code, tests or entries unit, compiled alone into its code
/// record for a JIT host (module docs), with `seed` for a JIT that holds
/// earlier batches. A code or tests unit's record holds its module record
/// (`cb$mod$…`, D-1107); an entries unit's holds none, and it is never
/// stored (D-964, D-1218). Without a seed, a code
/// or tests unit's record is [`crate::record::record_unit`]'s.
///
/// # Errors
///
/// [`RecordError::Codegen`] if code generation fails, and
/// [`RecordError::Undefined`] or [`RecordError::Unsupported`] if the record
/// cannot be finished.
pub fn record_for_jit(
    isa: OwnedTargetIsa,
    unit: &MirModule,
    ucx: &UnitCx<'_>,
    seed: Option<&Seed>,
    opts: &CodegenOptions,
) -> Result<JitUnit, RecordError> {
    record_with(isa, unit, ucx, seed, opts)
}

/// [`record_for_jit`] for each of `units`, each with its seed, in parallel
/// on rayon's global pool (D-923, G1), collected in the order given (D-264).
#[must_use]
pub fn records_for_jit(
    isa: &OwnedTargetIsa,
    units: &[(&MirModule, Option<&Seed>)],
    ucx: &UnitCx<'_>,
    opts: &CodegenOptions,
) -> Vec<Result<JitUnit, RecordError>> {
    use rayon::prelude::*;
    units
        .par_iter()
        .map(|&(unit, seed)| record_with(isa.clone(), unit, ucx, seed, opts))
        .collect()
}

/// [`record_for_jit`] for every kind of unit.
pub(crate) fn record_with(
    isa: OwnedTargetIsa,
    unit: &MirModule,
    ucx: &UnitCx<'_>,
    seed: Option<&Seed>,
    opts: &CodegenOptions,
) -> Result<JitUnit, RecordError> {
    let module = RecordingModule::new(isa);
    let (record, rest) = crate::record::record_into(module, unit, ucx, seed, opts)?;
    Ok(JitUnit {
        record,
        index: rest.index,
        clif: rest.clif,
        stats: rest.stats,
    })
}

/// The index of a unit's `parts` in `module`'s symbols.
///
/// # Errors
///
/// [`RecordError::Malformed`] if a part names an id `module` never
/// declared.
pub(crate) fn index_of(
    module: &RecordingModule,
    parts: &IndexParts,
) -> Result<UnitIndex, RecordError> {
    let func = |id: FuncId| {
        module
            .func_symbol(id)
            .ok_or(RecordError::Malformed("an index of an undeclared function"))
    };
    let data = |id: DataId| {
        module.data_symbol(id).ok_or(RecordError::Malformed(
            "an index of an undeclared data object",
        ))
    };
    let mut index = UnitIndex::default();
    for &(key, id) in &parts.funcs {
        index.funcs.push((key, func(id)?));
    }
    for (def, key, id) in &parts.slots {
        index.slots.push((*def, key.clone(), data(*id)?));
    }
    for &(def, id) in &parts.consts {
        index.consts.push((def, data(id)?));
    }
    for &(layout, word, ctor) in &parts.layouts {
        let ctor = match ctor {
            Some(ctor) => Some(data(ctor)?),
            None => None,
        };
        index.layouts.push((layout, data(word)?, ctor));
    }
    for &(slot, code, def) in &parts.repoints {
        index.repoints.push((data(slot)?, func(code)?, def));
    }
    Ok(index)
}
