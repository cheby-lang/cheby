//! Reading the interfaces a unit may see (D-246): the unit's own module
//! and its dependencies, never another module's bodies.

use cheby_iface::{
    DefId, DefKind, DepView, FnSig, Globals, Ice, IfaceItem, ItemSig, ModuleId, ModuleInterface,
    ModuleTable,
};

use crate::ir::DefName;

/// The interfaces lowering reads.
#[derive(Clone, Copy, Debug)]
pub struct Defs<'a> {
    /// The module being lowered; `None` for entry thunks.
    pub own: Option<&'a ModuleInterface>,
    /// Its dependencies (for entry thunks: the modules they call).
    pub deps: &'a DepView<'a>,
    pub modules: &'a ModuleTable,
    pub globals: &'a Globals,
}

impl<'a> Defs<'a> {
    /// The interface item `def`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `def` is not readable from here.
    pub fn item(&self, def: DefId) -> Result<&'a IfaceItem, Ice> {
        let iface = match self.own {
            Some(own) if own.module == def.module => Some(own),
            _ => self.deps.get(def.module),
        };
        iface
            .and_then(|iface| iface.item(def.index))
            .ok_or_else(|| Ice::new(format!("MIR lowering cannot see {def:?}")))
    }

    /// The signature of the function `def`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `def` is not a function with a valid signature.
    pub fn fn_sig(&self, def: DefId) -> Result<&'a FnSig, Ice> {
        match &self.item(def)?.sig {
            ItemSig::Fn(sig) => Ok(sig),
            other => Err(Ice::new(format!(
                "{def:?} is called as a function but its signature is {other:?}"
            ))),
        }
    }

    /// The path of `def`, split into module and item (as `def_path` writes
    /// it).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `def` or its module is unknown.
    pub fn name(&self, def: DefId) -> Result<DefName, Ice> {
        let item = self.item(def)?;
        let names = &self.globals.names;
        let module = if def.module == ModuleId::BUILTIN {
            String::new()
        } else {
            self.modules.try_get(def.module)?.path.to_string()
        };
        let mut text = String::new();
        if item.kind == DefKind::InterfaceFn {
            let parent = item
                .parent
                .map(|p| DefId::new(def.module, p))
                .ok_or_else(|| Ice::new("interface function without its interface"))?;
            text.push_str(names.get(self.item(parent)?.name));
            text.push_str("::");
        }
        text.push_str(names.get(item.name));
        Ok(DefName {
            def,
            module: module.into(),
            item: text.into(),
        })
    }
}
