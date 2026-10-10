//! The interfaces of a build and what each module may read of them.

use std::sync::{Arc, OnceLock};

use crate::{
    DefId, DefKind, Ice, IfaceItem, IndexVec, Interner, ModuleId, ModuleInterface, ModuleTable,
};

/// Every published interface of the build, by module.
///
/// Each module's slot is written once. [`InterfaceSet::publish`] writes
/// through a shared reference, so modules checked in parallel publish
/// their interfaces while others read theirs (D-263). A reader sees only
/// the modules its [`DepView`] allows, and those are published before it
/// starts, so what it reads never depends on the schedule.
#[derive(Clone, Debug)]
pub struct InterfaceSet {
    ifaces: IndexVec<ModuleId, OnceLock<Arc<ModuleInterface>>>,
}

impl InterfaceSet {
    /// A set holding only the builtin module's interface.
    #[must_use]
    pub fn new(builtin: ModuleInterface) -> Self {
        let mut ifaces = IndexVec::new();
        // An empty vector always has room for one element.
        let _ = ifaces.push(OnceLock::from(Arc::new(builtin)));
        Self { ifaces }
    }

    /// Makes room for the modules with ids below `count`, so that
    /// [`InterfaceSet::publish`] can fill their slots.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `count` is beyond the id space.
    pub fn reserve_modules(&mut self, count: usize) -> Result<(), Ice> {
        while self.ifaces.len() < count {
            self.ifaces.push(OnceLock::new())?;
        }
        Ok(())
    }

    /// Publishes a module's interface.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the module already has one.
    pub fn insert(&mut self, iface: Arc<ModuleInterface>) -> Result<(), Ice> {
        self.reserve_modules(crate::Idx::index(iface.module).saturating_add(1))?;
        self.publish(iface)
    }

    /// Publishes a module's interface through a shared reference. The
    /// module's slot must have been made with
    /// [`InterfaceSet::reserve_modules`].
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the module has no slot or already has an interface.
    pub fn publish(&self, iface: Arc<ModuleInterface>) -> Result<(), Ice> {
        let module = iface.module;
        let slot = self
            .ifaces
            .get(module)
            .ok_or_else(|| Ice::new(format!("no interface slot for {module:?}")))?;
        slot.set(iface)
            .map_err(|_| Ice::new(format!("interface of {module:?} published twice")))
    }

    #[must_use]
    pub fn get(&self, module: ModuleId) -> Option<&ModuleInterface> {
        self.get_arc(module).map(Arc::as_ref)
    }

    /// The shared handle of a module's interface.
    #[must_use]
    pub fn get_arc(&self, module: ModuleId) -> Option<&Arc<ModuleInterface>> {
        self.ifaces.get(module).and_then(OnceLock::get)
    }
}

/// What the phases of module M may read: the builtin module and the
/// interfaces of M's transitive dependencies (D-246, D-260). Lookups
/// outside the set return `None`, so results never depend on which
/// unrelated modules a parallel schedule happened to finish first.
#[derive(Clone, Copy, Debug)]
pub struct DepView<'a> {
    set: &'a InterfaceSet,
    /// Sorted, without duplicates.
    allowed: &'a [ModuleId],
}

impl<'a> DepView<'a> {
    /// A view of `set` restricted to `allowed`, which must be sorted. The
    /// builtin module is always readable.
    #[must_use]
    pub const fn new(set: &'a InterfaceSet, allowed: &'a [ModuleId]) -> Self {
        Self { set, allowed }
    }

    /// Whether this view may read `module`.
    #[must_use]
    pub fn allows(&self, module: ModuleId) -> bool {
        module == ModuleId::BUILTIN || self.allowed.binary_search(&module).is_ok()
    }

    /// The interface of `module`, if it is readable and published.
    #[must_use]
    pub fn get(&self, module: ModuleId) -> Option<&'a ModuleInterface> {
        if self.allows(module) {
            self.set.get(module)
        } else {
            None
        }
    }

    /// The item `def`, if its module is readable.
    #[must_use]
    pub fn item(&self, def: DefId) -> Option<&'a IfaceItem> {
        self.get(def.module).and_then(|iface| iface.item(def.index))
    }

    /// The readable modules besides the builtin one, sorted.
    #[must_use]
    pub const fn allowed(&self) -> &'a [ModuleId] {
        self.allowed
    }
}

/// The path text of a definition for dumps, symbols and diagnostics:
/// `std::io::println`, `hello_world::main`, `shape::Shape::area` for an
/// interface function; builtin items by bare name (`Option`, `Some`).
///
/// # Errors
///
/// An [`Ice`] if the definition or its module is unknown.
pub fn def_path(
    def: DefId,
    own: Option<&ModuleInterface>,
    deps: &DepView<'_>,
    modules: &ModuleTable,
    names: &Interner,
) -> Result<String, Ice> {
    let iface = match own {
        Some(own) if own.module == def.module => Some(own),
        _ => deps.get(def.module),
    }
    .ok_or_else(|| Ice::new(format!("no interface for {def:?}")))?;
    let item = iface
        .item(def.index)
        .ok_or_else(|| Ice::new(format!("no item {def:?}")))?;
    let mut out = String::new();
    if def.module != ModuleId::BUILTIN {
        out.push_str(&modules.try_get(def.module)?.path.to_string());
        out.push_str("::");
    } else if !crate::builtin::is_prelude(def)
        && let Some(&(_, segment)) = crate::builtin::HOMES.iter().find(|(d, _)| *d == def)
    {
        // `std::map::Map`: builtin, but named through its home module.
        out.push_str(&crate::builtin::home_path(segment).to_string());
        out.push_str("::");
    }
    if item.kind == DefKind::InterfaceFn {
        let parent = item
            .parent
            .and_then(|p| iface.item(p))
            .ok_or_else(|| Ice::new("interface function without its interface"))?;
        out.push_str(names.get(parent.name));
        out.push_str("::");
    }
    out.push_str(names.get(item.name));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use cheby_package::ModulePath;

    use super::*;
    use crate::{Globals, ModuleInfo, PackageId, builtin};

    #[test]
    fn views_restrict_reads() {
        let globals = Globals::new();
        let mut modules = ModuleTable::new();
        let a = modules
            .add(ModuleInfo {
                path: ModulePath::root("app"),
                package: PackageId::LOCAL,
                file: None,
            })
            .unwrap();
        let b = modules
            .add(ModuleInfo {
                path: ModulePath::root("app").child("b"),
                package: PackageId::LOCAL,
                file: None,
            })
            .unwrap();
        let mut set = InterfaceSet::new(builtin::interface(&globals));
        set.insert(Arc::new(ModuleInterface::empty(b, PackageId::LOCAL)))
            .unwrap();
        assert!(
            set.insert(Arc::new(ModuleInterface::empty(b, PackageId::LOCAL)))
                .is_err()
        );
        let none: [ModuleId; 0] = [];
        let view = DepView::new(&set, &none);
        assert!(view.get(b).is_none());
        assert!(view.get(ModuleId::BUILTIN).is_some());
        let allowed = [b];
        let view = DepView::new(&set, &allowed);
        assert!(view.get(b).is_some());
        assert!(view.get(a).is_none());
        assert_eq!(
            def_path(builtin::SOME, None, &view, &modules, &globals.names).unwrap(),
            "Some"
        );
        assert_eq!(
            def_path(builtin::SHOW_FN, None, &view, &modules, &globals.names).unwrap(),
            "Show::show"
        );
        assert_eq!(
            def_path(builtin::MAP, None, &view, &modules, &globals.names).unwrap(),
            "std::map::Map"
        );
    }
}
