//! MIR lowering, code generation and running (G3).
//!
//! Every checked module is lowered to its own MIR unit, the same whichever
//! module is run; the entry thunk for the chosen `main` is a separate unit
//! (D-453). The JIT compiles all units in one batch, in build order.

use cheby_diag::SourceMap;
use cheby_iface::{DepView, Ice, ModuleId};
use cheby_jit::Jit;
use cheby_mir::{
    Defs, EntryKind, EntryRequest, LowerCx, MirModule, count_and_check, lower_entries, lower_module,
};
use cheby_types::thir;

use crate::frontend::Frontend;

/// Every checked module's MIR after reference counting and validation, in
/// build order.
pub(crate) fn lower_modules(front: &Frontend, sources: &SourceMap) -> Result<Vec<MirModule>, Ice> {
    let mut units = Vec::with_capacity(front.order.len());
    for &module in &front.order {
        let checked = front
            .checked(module)
            .ok_or_else(|| Ice::new(format!("module {module:?} was not checked")))?;
        let own = front
            .set
            .get(module)
            .ok_or_else(|| Ice::new(format!("module {module:?} has no interface")))?;
        let deps = DepView::new(&front.set, &checked.deps);
        let cx = LowerCx {
            defs: Defs {
                own: Some(own),
                deps: &deps,
                modules: &front.modules,
                globals: &front.globals,
            },
            sources,
        };
        let mut unit = lower_module(&checked.thir, &cx)?;
        count_and_check(&mut unit)?;
        units.push(unit);
    }
    Ok(units)
}

/// The entry unit with the thunk of `main` (D-137).
pub(crate) fn lower_main_entry(
    front: &Frontend,
    sources: &SourceMap,
    main: &thir::FnDef,
) -> Result<MirModule, Ice> {
    let mut all: Vec<ModuleId> = front.order.clone();
    all.sort_unstable();
    let deps = DepView::new(&front.set, &all);
    let cx = LowerCx {
        defs: Defs {
            own: None,
            deps: &deps,
            modules: &front.modules,
            globals: &front.globals,
        },
        sources,
    };
    let mut unit = lower_entries(
        &[EntryRequest {
            kind: EntryKind::Main(main.def),
            info: main.entry.as_ref(),
        }],
        &cx,
    )?;
    count_and_check(&mut unit)?;
    Ok(unit)
}

/// Compiles `units` in one batch.
pub(crate) fn compile(units: &[MirModule], capture_clif: bool) -> Result<Jit, cheby_jit::JitError> {
    let mut jit = Jit::with_options(cheby_codegen_clif::CodegenOptions {
        capture_clif,
        ..cheby_codegen_clif::CodegenOptions::default()
    })?;
    jit.compile(units)?;
    Ok(jit)
}
