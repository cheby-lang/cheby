//! Ids of packages, modules and definitions, and the module table.
//!
//! `ModuleId`s are assigned by the driver in a deterministic order, and
//! `DefId`s are meaningful only within one build. Anything persisted,
//! hashed or named (interface encoding, symbols, dumps) uses module paths
//! and item names, never raw ids.

use cheby_diag::FileId;
use cheby_package::ModulePath;
use rustc_hash::FxHashMap;

use crate::{Ice, IndexVec};

crate::newtype_index! {
    /// A package of the build: [`PackageId::STD`], [`PackageId::LOCAL`],
    /// then dependencies (M5).
    pub struct PackageId;
}

crate::newtype_index! {
    /// A module of the build. [`ModuleId::BUILTIN`] is the builtin module
    /// that holds the prelude declarations (H1).
    pub struct ModuleId;
}

crate::newtype_index! {
    /// The index of an item within its module.
    ///
    /// Items are numbered in source order. A type's constructors come
    /// right after the type and an interface's functions right after the
    /// interface, and `test` blocks are numbered too. Because private and
    /// test items shift later indexes, a `DefIndex` never appears in
    /// anything hashed, cached or named.
    pub struct DefIndex;
}

crate::newtype_index! {
    /// A type parameter of an item's generics.
    pub struct ParamIdx;
}

impl PackageId {
    /// The standard library, which also owns the builtin module.
    pub const STD: Self = Self::from_u32(0);
    /// The package being built.
    pub const LOCAL: Self = Self::from_u32(1);
}

impl ModuleId {
    /// The builtin module (H1). It has no source and cannot be imported.
    pub const BUILTIN: Self = Self::from_u32(0);
}

/// A definition: an item of a module.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DefId {
    pub module: ModuleId,
    pub index: DefIndex,
}

impl DefId {
    #[must_use]
    pub const fn new(module: ModuleId, index: DefIndex) -> Self {
        Self { module, index }
    }
}

/// The display path of the builtin module's single segment. It is not a
/// valid module name, so no import can reach it.
pub const BUILTIN_MODULE_NAME: &str = "<builtin>";

/// What the build knows about one module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleInfo {
    /// `hello_world`, `std::io`; `<builtin>` for the builtin module.
    pub path: ModulePath,
    pub package: PackageId,
    /// The source file; `None` for the builtin module.
    pub file: Option<FileId>,
}

/// Every module of the build, by id and by path.
#[derive(Clone, Debug)]
pub struct ModuleTable {
    modules: IndexVec<ModuleId, ModuleInfo>,
    by_path: FxHashMap<ModulePath, ModuleId>,
}

impl ModuleTable {
    /// A table holding only [`ModuleId::BUILTIN`].
    #[must_use]
    pub fn new() -> Self {
        let mut modules = IndexVec::new();
        let builtin = ModuleInfo {
            path: ModulePath::root(BUILTIN_MODULE_NAME),
            package: PackageId::STD,
            file: None,
        };
        // An empty vector always has room for one element.
        let _ = modules.push(builtin);
        Self {
            modules,
            by_path: FxHashMap::default(),
        }
    }

    /// Adds a module and returns its id.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a module with this path is already in the table.
    pub fn add(&mut self, info: ModuleInfo) -> Result<ModuleId, Ice> {
        if self.by_path.contains_key(&info.path) {
            return Err(Ice::new(format!("module `{}` added twice", info.path)));
        }
        let path = info.path.clone();
        let id = self.modules.push(info)?;
        self.by_path.insert(path, id);
        Ok(id)
    }

    #[must_use]
    pub fn get(&self, id: ModuleId) -> Option<&ModuleInfo> {
        self.modules.get(id)
    }

    /// The module with `id`, or an [`Ice`].
    ///
    /// # Errors
    ///
    /// An [`Ice`] if there is no such module.
    pub fn try_get(&self, id: ModuleId) -> Result<&ModuleInfo, Ice> {
        self.modules.try_get(id, "module")
    }

    /// The module with this path. The builtin module is never found.
    #[must_use]
    pub fn find(&self, path: &ModulePath) -> Option<ModuleId> {
        self.by_path.get(path).copied()
    }

    /// The number of modules, the builtin module included.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.modules.len()
    }

    /// Always false: the builtin module is always present.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// Every module with its id, in id order.
    pub fn iter(&self) -> impl Iterator<Item = (ModuleId, &ModuleInfo)> + '_ {
        self.modules.iter_enumerated()
    }
}

impl Default for ModuleTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_starts_with_builtin() {
        let mut table = ModuleTable::new();
        assert_eq!(table.len(), 1);
        let builtin = table.get(ModuleId::BUILTIN).unwrap();
        assert_eq!(builtin.path.to_string(), "<builtin>");
        assert_eq!(table.find(&ModulePath::root(BUILTIN_MODULE_NAME)), None);
        let path = ModulePath::root("std").child("io");
        let info = ModuleInfo {
            path: path.clone(),
            package: PackageId::STD,
            file: None,
        };
        let id = table.add(info.clone()).unwrap();
        assert_eq!(id, ModuleId::from_u32(1));
        assert_eq!(table.find(&path), Some(id));
        assert!(table.add(info).is_err());
    }
}
