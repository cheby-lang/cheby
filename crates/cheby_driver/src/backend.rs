//! MIR lowering, code generation and running (G3).
//!
//! A run lowers only what its entry needs (D-510): the items reachable
//! from the `main` being run, across modules, with the constructor
//! layouts of every module that reachable code uses. Each such module is lowered to its own MIR unit, in build
//! order with items in `DefIndex` order, and the entry thunk is a separate
//! unit (D-453). The JIT compiles all units in one batch.
//!
//! A reachable construct that the back end does not implement yet is
//! E0038 at the construct; every reachable item is lowered first, so one
//! run reports all of them, and nothing is compiled.

use cheby_diag::{Diagnostic, SourceMap};
use cheby_iface::{DepView, Ice, ModuleId};
use cheby_jit::Jit;
use cheby_mir::{
    Defs, EntryKind, EntryRequest, LowerCx, MirModule, Program, Root, Unsupported, count_and_check,
    lower_entries, lower_items, reachable,
};
use cheby_types::thir;

use crate::frontend::Frontend;

/// The MIR units that running `main` needs, after reference counting and
/// validation: one per needed module in build order, then the entry unit.
/// `Err` holds the E0038 diagnostics of the constructs the back end
/// cannot lower yet, unsorted.
pub(crate) fn lower_program(
    front: &Frontend,
    sources: &SourceMap,
    main: &thir::FnDef,
) -> Result<Result<Vec<MirModule>, Vec<Diagnostic>>, Ice> {
    let mut program = Program::new();
    for &module in &front.order {
        program.insert(module, &checked(front, module)?.thir);
    }
    let reach = reachable(&program, &[Root::Fn(main)])?;
    let mut units = Vec::new();
    let mut unsupported = Unsupported::default();
    for &module in &front.order {
        if !reach.needs_module(module) {
            continue;
        }
        let checked = checked(front, module)?;
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
        let lowered = lower_items(&checked.thir, &|def| reach.contains(def), &cx);
        if let Some(mut unit) = unsupported.take(lowered)? {
            count_and_check(&mut unit)?;
            units.push(unit);
        }
    }
    if let Some(entry) = unsupported.take(lower_main_entry(front, sources, main))? {
        units.push(entry);
    }
    if unsupported.is_empty() {
        Ok(Ok(units))
    } else {
        Ok(Err(unsupported.into_diagnostics()))
    }
}

fn checked(front: &Frontend, module: ModuleId) -> Result<&crate::frontend::Checked, Ice> {
    front
        .checked(module)
        .ok_or_else(|| Ice::new(format!("module {module:?} was not checked")))
}

/// The entry unit with the thunk of `main` (D-137).
fn lower_main_entry(
    front: &Frontend,
    sources: &SourceMap,
    main: &thir::FnDef,
) -> Result<MirModule, cheby_mir::LowerError> {
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
