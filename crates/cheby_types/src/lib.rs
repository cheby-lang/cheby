//! Type checking (C1-C15): signatures into the module's interface, bodies
//! into typed THIR.
//!
//! - [`build_interface`] (C2) lowers every item's signature and returns
//!   the module's [`ModuleInterface`], which is all that dependents see.
//! - [`check_bodies`] and [`check_item`] (C3, C15) check bodies against
//!   the module's own interface and its dependencies' interfaces, and
//!   return [`thir`]. A body with an error is not returned, so MIR never
//!   sees errors.
//! - [`infer`] (C1) holds the body-local inference types and unification.
//! - [`package_warnings`] (C14) is the whole-package warning pass, which
//!   the driver runs after every local module is checked, for
//!   `cheby check` only (D-266).
//! - [`repl::ReplSession`] checks REPL entries against their session and
//!   keeps the session's names, redefinitions and stale types (§13.5).
//!
//! Internal passes: `decl` and `ifaces` check declarations at the end of
//! [`build_interface`]; `exhaust` checks patterns of each body
//! without errors; `targets`, `consts` and `warnings` run over the
//! module's THIR at the end of [`check_bodies`].
//!
//! Locality (D-246): the checker of module M reads M's tree, M's
//! resolution and the [`ModuleInterface`]s of M's dependencies through a
//! [`DepView`], never another module's tree, resolution or bodies.
//!
//! The front end checks every construct; what the back end cannot lower
//! yet is reported by MIR lowering (E0038, D-480, D-510).

mod check;
mod consts;
mod decl;
mod exhaust;
mod ifaces;
pub mod infer;
mod lower_ty;
pub mod repl;
mod sig;
mod targets;
pub mod thir;
mod warnings;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use cheby_diag::Diagnostic;
use cheby_iface::{
    DefId, DefIndex, DepView, DisplayCx, Generics, Globals, Ice, IfaceItem, ModuleId,
    ModuleInterface, ModuleTable, PackageId,
};
use cheby_resolve::ResolvedModule;
use cheby_syntax::SyntaxTree;

pub use check::{Tests, check_bodies, check_bodies_with, check_item};
pub use sig::build_interface;
pub use warnings::{
    ModuleRefs, PackageCandidate, PackageModule, module_refs, package_warnings, unused_package_item,
};

/// What checking module M needs (D-246).
#[derive(Clone, Copy, Debug)]
pub struct TypesInput<'a> {
    pub module: ModuleId,
    pub package: PackageId,
    pub tree: &'a SyntaxTree,
    pub resolved: &'a ResolvedModule,
    /// The interfaces of M's dependencies and the builtin module.
    pub deps: DepView<'a>,
    pub modules: &'a ModuleTable,
    pub globals: &'a Globals,
}

impl<'a> TypesInput<'a> {
    /// The interface item `def`: from `own` for this module's items, else
    /// from a dependency.
    pub(crate) fn item(&self, own: &'a ModuleInterface, def: DefId) -> Result<&'a IfaceItem, Ice> {
        let item = if def.module == self.module {
            own.item(def.index)
        } else {
            self.deps.item(def)
        };
        item.ok_or_else(|| Ice::new(format!("no interface item for {def:?}")))
    }

    /// The `DefId` of this module's item `index`.
    pub(crate) const fn def_id(&self, index: DefIndex) -> DefId {
        DefId::new(self.module, index)
    }

    /// How to show types in this module's diagnostics.
    pub(crate) const fn display_cx<'s>(
        &'s self,
        own: &'s ModuleInterface,
        generics: Option<&'s Generics>,
    ) -> DisplayCx<'s> {
        DisplayCx {
            globals: self.globals,
            modules: self.modules,
            own: Some(own),
            deps: &self.deps,
            generics,
        }
    }

    /// The diagnostics' name of `def`: `println`, `Option`.
    pub(crate) fn name_of(&self, own: &'a ModuleInterface, def: DefId) -> &'static str {
        self.item(own, def)
            .map_or("?", |item| self.globals.names.get(item.name))
    }
}

/// Whether any of `diags` is an error.
fn has_error(diags: &[Diagnostic]) -> bool {
    diags.iter().any(Diagnostic::is_error)
}
