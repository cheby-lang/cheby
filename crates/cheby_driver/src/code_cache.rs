//! Code records in the store (K4c: D-914, D-922, D-923, D-1193, D-964,
//! D-965, D-1138, D-1139; `cache.md` §7).
//!
//! Every command's JIT (`cheby run`'s and `cheby test`'s program, `cheby
//! check`'s and every evaluation module of D-1187) compiles its units
//! through code records ([`cheby_jit::Jit::compile_units`]): each unit it
//! needs is compiled alone into a record and loaded, or its record comes
//! from the store and is loaded the same way, so fresh and cached code take
//! one load path (D-1217). The lookups come
//! before lowering (`backend.rs`'s `lower_through_cache`): a unit whose
//! record hits is not lowered, and reaches [`compile`] as its record in
//! [`Lowered::order`].
//!
//! - A package module's code unit's record lies under its code key
//!   (`KeyKind::Code`) and its tests unit's under its tests key
//!   (`KeyKind::Tests`), for the kinds `run`, `test` and `plain` (D-1193),
//!   as `CodeRecord::encode`'s bytes (D-1221).
//! - Each lookup is logged under `CHEBY_CACHE_LOG` as `code.<kind> <module>
//!   hit|miss` or `tests.<kind> <module>#tests hit|miss`, beside K4b's
//!   `key` lines (D-973, D-1224).
//! - A miss is compiled eagerly and whole, its functions in parallel
//!   (D-923), and its record written behind the command (D-1138), at most
//!   once per process (D-1139). Std's units are not stored here: a JIT host
//!   with a cache that reads loads them from std's own code records, keyed
//!   by the build ID and settings with no mask, callee or identity, which
//!   std's first use writes (D-1264, D-1266; `std_cache.rs`). Nor is the
//!   entries unit stored (D-964), nor anything of a module with errors,
//!   which is never lowered.
//! - `CHEBY_DUMP_DIR` bypasses reads (D-965) and still writes.
//!   `--reuse-report` reads a record only with the reuse lines cached
//!   beside it, which it replays (K4d, D-965, refining D-1223;
//!   `reuse_replay.rs`).
//!
//! A record from the store has no index of its functions by key, so the
//! command names its constants' slots by symbol ([`name_consts`]).

use std::sync::Arc;

use cheby_codegen_clif::CodegenOptions;
use cheby_codegen_clif::mangle::{self, Kind};
use cheby_consteval::ConstInfo;
use cheby_iface::{DefId, Ice, ModuleId};
use cheby_jit::{BatchCx, BatchUnit, Jit, JitError, JitOptions, NativeLibrary};
use cheby_mir::{DefName, UnitKind};

use crate::backend::{Lowered, Placed};
use crate::cache::code_keys::UnitPart;
use crate::frontend::Frontend;

/// A JIT with `codegen` and `options` and the package libraries
/// `libraries` (D-941, build order), with `lowered`'s units loaded in one
/// batch, in build order (module docs): each package unit that the
/// lowering took from the store as its record ([`Placed::Cached`]), every
/// other unit compiled alone into its record, and each fresh package
/// unit's record stored under its key.
///
/// # Errors
///
/// As [`Jit::compile_units`].
pub(crate) fn compile(
    front: &Frontend,
    lowered: &Lowered,
    codegen: CodegenOptions,
    options: JitOptions,
    libraries: &[Arc<NativeLibrary>],
) -> Result<Jit, JitError> {
    let mut jit = Jit::configured(codegen, options)?;
    for library in libraries {
        jit.add_library(Arc::clone(library))?;
    }
    let mut batch = Vec::with_capacity(lowered.order.len());
    let mut ordinals = Vec::new();
    for placed in &lowered.order {
        match placed {
            Placed::Fresh(i) => {
                let unit = lowered
                    .units
                    .get(*i)
                    .ok_or_else(|| JitError::from(Ice::new("a lowered unit went missing")))?;
                batch.push(BatchUnit::Mir(unit));
            }
            Placed::Cached { unit, path, record } => {
                if let UnitKind::Tests(module) = *unit {
                    test_ordinals(front, module, &mut ordinals).map_err(JitError::from)?;
                }
                batch.push(BatchUnit::Record {
                    record,
                    unit: *unit,
                    path,
                });
            }
        }
    }
    let fresh = jit.compile_units(
        &batch,
        &BatchCx {
            test_ordinals: &ordinals,
        },
    )?;
    if let Some(sink) = crate::borrowing::sink(front) {
        for (unit, record) in batch.iter().zip(&fresh) {
            if let (BatchUnit::Mir(unit), Some(record)) = (unit, record)
                && let Some((_, key)) = unit_key(lowered, unit)
            {
                sink.code_store(key, record);
            }
        }
    }
    Ok(jit)
}

/// The part and code key of `unit`, if it is a package unit with one.
fn unit_key<'a>(
    lowered: &'a Lowered,
    unit: &cheby_mir::MirModule,
) -> Option<(UnitPart, &'a crate::cache::Key)> {
    let (part, module) = match unit.unit {
        UnitKind::Code(module) => (UnitPart::Code, module),
        UnitKind::Tests(module) => (UnitPart::Tests, module),
        UnitKind::Entries => return None,
    };
    lowered
        .keys
        .keys
        .get(&(part, module))
        .map(|key| (part, key))
}

/// Appends the ordinal of each test of `module` among all of its tests on
/// native, in source order (D-1179): how its whole tests unit numbers
/// them, so the entries unit names a cached tests unit's tests as a fresh
/// one's.
fn test_ordinals(
    front: &Frontend,
    module: ModuleId,
    out: &mut Vec<(DefId, u32)>,
) -> Result<(), Ice> {
    let tests = front
        .tests_of(module)
        .ok_or_else(|| Ice::new(format!("module {module:?} was not checked")))?;
    let mut next = 0_u32;
    for test in tests {
        if crate::test::on_native(front, test) {
            out.push((test.def, next));
            next = next.saturating_add(1);
        }
    }
    Ok(())
}

/// Names the slot of each constant of `infos` by its symbol
/// (`cb$const$<module>$<name>`, D-456), so the JIT finds the slots of
/// constants whose module's record came from the store (module docs).
///
/// # Errors
///
/// An [`Ice`] if a constant's module is unknown.
pub(crate) fn name_consts(front: &Frontend, jit: &mut Jit, infos: &[ConstInfo]) -> Result<(), Ice> {
    let mut names = Vec::with_capacity(infos.len());
    for info in infos {
        let module = front.modules.try_get(info.def.module)?.path.to_string();
        let name = DefName {
            def: info.def,
            module: module.into(),
            item: info.name.clone(),
        };
        names.push((info.def, mangle::item(Kind::Const, &name)));
    }
    jit.name_consts(names);
    Ok(())
}
