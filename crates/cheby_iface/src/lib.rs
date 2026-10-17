//! The shared vocabulary of every phase after syntax: ids, names, types,
//! module interfaces and the builtin module (§13.12.1, D-249).
//!
//! - [`IndexVec`] and [`newtype_index!`]: dense ids with no panicking
//!   indexing; a missing id is an [`Ice`] (D-423).
//! - [`Name`], [`Interner`] and [`sym`]: interned identifiers.
//! - [`ModuleId`], [`DefId`], [`ModuleTable`]: what the build contains.
//! - [`Ty`], [`TyKind`], [`TyInterner`]: closed, interned types.
//! - [`ModuleInterface`]: what dependents may know about a module (B8),
//!   with its canonical encoding and hash in [`hash`].
//! - [`builtin`]: the prelude's declarations (H1).
//! - [`InterfaceSet`] and [`DepView`]: the interfaces a module may read,
//!   which are only those of its dependencies (D-246).

use std::fmt;

pub mod builtin;
mod def;
pub mod hash;
mod ids;
mod iface;
mod index;
mod leak;
mod name;
mod set;
mod ty;

pub use def::{DefKind, Namespace, Vis};
pub use hash::{InterfaceHash, encode, interface_hash};
pub use ids::{
    BUILTIN_MODULE_NAME, DefId, DefIndex, ModuleId, ModuleInfo, ModuleTable, PackageId, ParamIdx,
};
pub use iface::{
    AliasDef, BuiltinType, ConstSig, CtorFields, CtorSig, FnSig, IfaceItem, InterfaceDef,
    InterfaceFnSig, ItemSig, ModuleInterface, ParamSig, TypeDef, TypeDefKind, TypeHooks,
    interface_fns, structure_visible,
};
pub use index::{Idx, IndexVec};
pub use name::{Interner, Name, sym};
pub use set::{DepView, InterfaceSet, def_path};
pub use ty::{DisplayCx, GenericParam, Generics, Ty, TyInterner, TyKind, display};

/// The process-wide tables every phase shares, as `Arc<Globals>`.
#[derive(Debug, Default)]
pub struct Globals {
    pub names: Interner,
    pub types: TyInterner,
}

impl Globals {
    #[must_use]
    pub fn new() -> Self {
        Self {
            names: Interner::new(),
            types: TyInterner::new(),
        }
    }
}

/// An internal compiler error: a broken invariant, never a user error.
/// The driver prints it as `error: internal compiler error: <message>`
/// (D-464).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ice {
    message: String,
}

impl Ice {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Ice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Ice {}
