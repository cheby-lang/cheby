//! The near-miss scan of "Satisfy interface…" (D-1696, X §6.6, F §7):
//! the interfaces a type misses by one function.
//!
//! The scan goes over every interface of the run that the type's module
//! could satisfy: those of the session's own modules, std's, and the
//! prelude's, each visible from the type's module. Satisfaction is
//! structural, so the module need not import the interface. A type misses
//! an interface by one function when the solver's answer is that a
//! function is missing (`Reason::Missing`) and exactly one function of the
//! interface has no function of its name in the type's module, while at
//! least one other has: otherwise a type with nothing in common would
//! match every interface (F §7). An empty interface is never a near miss
//! (D-166).
//!
//! It is kept apart from `stubs`, so W12 A can reuse or replace it.

use cheby_driver::analysis::{
    DefId, DefIndex, DefKind, ItemSig, ModuleId, ModuleView, Namespace, PackageId, Reason,
    Snapshot, Verdict, Vis, interface_fns, satisfies,
};

/// One interface that a type misses by one function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NearMiss {
    /// The interface.
    pub interface: DefId,
    /// The one function of the interface that the type's module lacks.
    pub function: DefId,
}

/// The near misses of the type `ty` of `view`, a module of `snapshot`, in
/// module and item order. Empty for a module that is not one of the
/// session's own (D-1700), such as the binary's std seen from a package, and
/// for an item that is no type.
pub(crate) fn near_misses(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    ty: DefIndex,
) -> Vec<NearMiss> {
    if !view.is_own() {
        return Vec::new();
    }
    let Some(own) = view.interface() else {
        return Vec::new();
    };
    let Some(ItemSig::Type(def)) = own
        .item(ty)
        .filter(|item| item.kind == DefKind::Type)
        .map(|item| &item.sig)
    else {
        return Vec::new();
    };
    let Some(input) = snapshot.query_input(view.id(), view) else {
        return Vec::new();
    };
    let types = &snapshot.globals().types;
    let generics = &def.generics;
    let args: Box<[_]> = generics
        .params
        .iter_enumerated()
        .map(|(param, _)| types.param(param))
        .collect();
    let self_ty = types.named(DefId::new(view.id(), ty), args);
    let interfaces = snapshot.interfaces();
    let item_of = |d: DefId| interfaces.get(d.module).and_then(|i| i.item(d.index));
    let mut out = Vec::new();
    for &module in snapshot.all_ids() {
        if !scanned(snapshot, view, module) {
            continue;
        }
        let Some(iface) = interfaces.get(module) else {
            continue;
        };
        for (index, item) in iface.items.iter_enumerated() {
            let visible = match item.vis {
                Vis::Pub => true,
                Vis::Package => snapshot.module(module).is_some_and(ModuleView::is_own),
                Vis::Priv => module == view.id(),
            };
            if item.kind != DefKind::Interface || !visible || iface.is_hidden(index) {
                continue;
            }
            let interface = DefId::new(module, index);
            let Ok(fns) = interface_fns(interface, &item_of) else {
                continue;
            };
            if fns.len() < 2 {
                continue;
            }
            let Ok(Verdict::No(err)) =
                satisfies(&input.input, input.own, generics, self_ty, interface)
            else {
                continue;
            };
            if err.ty != self_ty || !matches!(err.reason, Reason::Missing { .. }) {
                continue;
            }
            let missing: Vec<DefId> = fns
                .iter()
                .copied()
                .filter(|&f| {
                    item_of(f).is_none_or(|f| {
                        own.lookup(Namespace::Value, f.name)
                            .and_then(|found| own.item(found))
                            .is_none_or(|found| found.kind != DefKind::Fn)
                    })
                })
                .collect();
            if let [function] = missing.as_slice() {
                out.push(NearMiss {
                    interface,
                    function: *function,
                });
            }
        }
    }
    out
}

/// Whether the scan reads the interfaces of `module`: one of the session's
/// own modules, a std module or the builtin module. Another package's
/// interfaces come with dependencies (M5).
fn scanned(snapshot: &Snapshot, view: ModuleView<'_>, module: ModuleId) -> bool {
    if module == view.id() || module == ModuleId::BUILTIN {
        return true;
    }
    snapshot.module(module).is_some_and(|m| {
        m.is_own()
            || snapshot
                .table()
                .get(module)
                .is_some_and(|info| info.package == PackageId::STD)
    })
}

#[cfg(test)]
#[path = "near_miss_tests.rs"]
mod tests;
