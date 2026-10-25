//! Imports, item collection, scopes and name resolution (B3-B7, §7.5-§7.7).
//!
//! - [`collect_imports`] and [`resolve_import_targets`] turn a module's
//!   `import`s into module ids through the driver's [`ModuleLookup`], which
//!   loads standard-library modules on first use (B3). The driver builds the
//!   import graph and reports cycles with [`cycle_diagnostic`] (E0040).
//! - [`resolve_module`] collects the module's items (B4), builds its scope
//!   from its items, its imports and the prelude (B5, B6), and resolves every
//!   path in signatures and bodies (B7). It reads other modules only through
//!   their interfaces in a [`DepView`] (D-246).
//! - Names are checked where they are bound or declared: the case
//!   conventions (E0006) and the reserved words (E0007). A use adds
//!   nothing, except that a reserved word that nothing binds is E0007
//!   where it is used (D-469).
//! - [`repl::SessionScope`] resolves a REPL entry, a module of its own,
//!   with the names of its session (§13.5, D-547).
//! - E0055 reserves the `cheby_rt_` and `cb$` prefixes here; for the other
//!   names the runtime exports, the driver calls
//!   [`runtime_export_reserved`] with the runtime's names after resolution
//!   (D-944), so this crate never depends on the runtime.
//! - The only part of the support gate (D-451) left in the front end is
//!   E0038 for an import of a standard-library module that the spec plans
//!   but the toolchain does not ship yet (tiers 2 and 3). Paths through such an import
//!   are silent [`Res::Err`]. Every item and every construct resolves.
//!
//! Later phases read syntax only through `cheby_syntax::ast` views, keyed
//! by the `NodeId`s stored here.

mod body;
mod imports;
mod items;
mod naming;
mod paths;
pub mod repl;
mod scope;

use cheby_diag::{Diagnostic, Span};
use cheby_iface::{
    DefId, DefIndex, DefKind, DepView, Globals, Ice, IndexVec, ModuleId, ModuleTable, Name,
    PackageId, ParamIdx, Vis,
};
use cheby_package::Targets;
use cheby_syntax::{NodeId, SyntaxTree};
use rustc_hash::FxHashMap;

pub use imports::{
    ImportItemRequest, ImportRequest, ImportTarget, Lookup, ModuleLookup, closest_name,
    collect_imports, cycle_diagnostic, resolve_import_targets,
};
pub use items::runtime_export_reserved;
pub use scope::{ModuleScope, ScopeEntry, ScopeSource};

cheby_iface::newtype_index! {
    /// A local binding of a top-level item's body: a parameter, a `let`,
    /// a pattern binding, a `use` binder, a closure parameter or a local
    /// function. All locals of an item, those of nested closures and local
    /// functions included, share one numbering.
    pub struct LocalId;
}

/// What resolution of module M needs.
#[derive(Clone, Copy, Debug)]
pub struct ResolveInput<'a> {
    pub module: ModuleId,
    pub package: PackageId,
    pub tree: &'a SyntaxTree,
    /// The module's imports, from [`collect_imports`].
    pub imports: &'a [ImportRequest],
    /// Same length and order as `imports`.
    pub targets: &'a [ImportTarget],
    /// The interfaces of M's dependencies and the builtin module.
    pub deps: DepView<'a>,
    pub modules: &'a ModuleTable,
    pub globals: &'a Globals,
    /// What the toolchain knows about the standard library.
    pub std: StdCatalog<'a>,
    /// The targets the module's package declares (`targets` in
    /// `cheby.toml`, both by default; both for the standard library).
    pub package_targets: Targets,
}

/// The standard library as the toolchain knows it, by module path under
/// `std`, such as `list` or `json::decode` (D-451).
#[derive(Clone, Copy, Debug, Default)]
pub struct StdCatalog<'a> {
    /// Every module the spec defines, shipped or not, for the help line of
    /// "`list` is not imported".
    pub known: &'a [&'a str],
}

/// The result of resolving one module.
#[derive(Clone, Debug)]
pub struct ResolvedModule {
    pub module: ModuleId,
    /// Every item, in `DefIndex` order: source order,
    /// with a type's constructors right after it and an interface's
    /// functions right after it.
    pub items: IndexVec<DefIndex, ItemDef>,
    pub scope: ModuleScope,
    /// Where the module's code is built: the package's targets
    /// intersected with the module's `@!target` (§4.8, §12.6).
    pub targets: Targets,
    /// Resolutions of paths in the signatures of the items, keyed by
    /// the `PATH` node.
    pub sig_paths: FxHashMap<NodeId, Res>,
    /// The type parameters of each generic item, by name.
    pub generics: FxHashMap<DefIndex, Vec<(Name, ParamIdx)>>,
    /// One per `Fn`, `Const` or `Test` item with a body.
    pub bodies: FxHashMap<DefIndex, BodyRes>,
}

impl ResolvedModule {
    #[must_use]
    pub fn item(&self, index: DefIndex) -> Option<&ItemDef> {
        self.items.get(index)
    }

    /// The `DefId` of item `index` of this module.
    #[must_use]
    pub const fn def_id(&self, index: DefIndex) -> DefId {
        DefId::new(self.module, index)
    }
}

/// An item of the module being resolved.
#[derive(Clone, Debug)]
pub struct ItemDef {
    pub name: Name,
    pub name_span: Span,
    pub kind: DefKind,
    pub vis: Vis,
    /// A constructor's type, an interface function's interface.
    pub parent: Option<DefIndex>,
    /// The declaration node (`FN_DECL`, `TYPE_DECL`, `VARIANT`,
    /// `INTERFACE_FN`, …), for `AstId::get` through the typed views.
    pub node: NodeId,
    pub attrs: ItemAttrs,
}

/// The attributes of an item (§4.8), checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemAttrs {
    /// At most one per target (E0045).
    pub externals: Vec<ExternalAttr>,
    /// Where the item exists: from `@target`, `@!target`, and the
    /// externals of a function without a body (§12.5, §12.6, D-461). A
    /// constructor and an interface function have their parent's.
    pub targets: Targets,
    pub blocking: bool,
    pub is_async: bool,
    pub deprecated: Option<Box<str>>,
    /// The parameters named by the standard library's `@std_clamped(param,
    /// …)` on a function, in written order (D-298, D-303, D-482).
    pub clamped: Vec<(Name, Span)>,
    /// The functions named by the standard library's `@std_hooks(equal_fn,
    /// hash_fn, debug_fn)` on a type, in that order (D-342, D-482).
    pub hooks: Option<[(Name, Span); 3]>,
    /// The standard library's `@std_context` on a function with an
    /// `@external`: its native kernel receives the running fiber's context
    /// as its first C argument (D-692, D-482).
    pub std_context: bool,
}

impl Default for ItemAttrs {
    fn default() -> Self {
        Self {
            externals: Vec::new(),
            targets: Targets::ALL,
            blocking: false,
            is_async: false,
            deprecated: None,
            clamped: Vec::new(),
            hooks: None,
            std_context: false,
        }
    }
}

impl ItemAttrs {
    /// The symbol of the `@external(native, "symbol")`, if any, with the
    /// span of its string literal.
    #[must_use]
    pub fn native_symbol(&self) -> Option<(&str, Span)> {
        self.externals.iter().find_map(|e| match e {
            ExternalAttr::Native {
                symbol,
                symbol_span,
                ..
            } => Some((&**symbol, *symbol_span)),
            ExternalAttr::Js { .. } => None,
        })
    }

    /// The `@external(native, "symbol")`, if any.
    #[must_use]
    pub fn native_external(&self) -> Option<(&str, Span)> {
        self.externals.iter().find_map(|e| match e {
            ExternalAttr::Native { symbol, span, .. } => Some((&**symbol, *span)),
            ExternalAttr::Js { .. } => None,
        })
    }
}

/// `@external(…)` (§12.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExternalAttr {
    Native {
        symbol: Box<str>,
        /// The whole attribute.
        span: Span,
        /// The symbol's string literal.
        symbol_span: Span,
    },
    Js {
        module: Box<str>,
        name: Box<str>,
        span: Span,
    },
}

/// What a name refers to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Res {
    Local(LocalId),
    Def(DefId),
    Module(ModuleId),
    TyParam(ParamIdx),
    /// `Self` in an interface declaration (C8).
    SelfTy,
    /// Already reported, or a path through an import of an unshipped std
    /// module (E0038): silent from here on.
    Err,
}

/// The name resolution of one body.
#[derive(Clone, Debug, Default)]
pub struct BodyRes {
    /// Every local of the item, nested closures and local functions
    /// included.
    pub locals: IndexVec<LocalId, LocalDecl>,
    /// The parameters of the item, in order.
    pub params: Vec<LocalId>,
    /// Binding nodes: `PARAM` (top-level), `IDENT_PAT`, `AS_PAT`,
    /// `REST_PAT` with a name, punned `FIELD_PAT`, and `LOCAL_FN`.
    pub bindings: FxHashMap<NodeId, LocalId>,
    /// Every `PATH` node of the body, in expressions, patterns and types.
    pub paths: FxHashMap<NodeId, Res>,
    /// Name references that are not `PATH` nodes: the root of an
    /// `INTERPOLATION` and a punned `FIELD_INIT`, keyed by that node.
    pub names: FxHashMap<NodeId, Res>,
}

/// A local binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalDecl {
    pub name: Name,
    pub span: Span,
    pub kind: LocalKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LocalKind {
    Param,
    Let,
    CaseArm,
    UseBinder,
    ClosureParam,
    /// A local named function (§5.8). Inside its own body, its name
    /// resolves to this same local; THIR turns those references into
    /// `SelfRef`, never a capture (D-110, D-453).
    LocalFn,
}

/// Resolves module `input.module`: items, scope, signatures and bodies.
/// Diagnostics are in no particular order; the driver sorts them.
///
/// # Errors
///
/// An [`Ice`] if an invariant is broken, such as `targets` and `imports`
/// having different lengths.
pub fn resolve_module(input: &ResolveInput<'_>) -> Result<(ResolvedModule, Vec<Diagnostic>), Ice> {
    if input.imports.len() != input.targets.len() {
        return Err(Ice::new("import targets do not match the imports"));
    }
    let mut diags = Vec::new();
    naming::check_names(input.tree, &mut diags);
    let collected = items::collect(input, &mut diags)?;
    let items = collected.items;
    let scope = scope::build(input, &items, &mut diags)?;
    let mut module = ResolvedModule {
        module: input.module,
        items,
        scope,
        targets: collected.module_targets,
        sig_paths: FxHashMap::default(),
        generics: FxHashMap::default(),
        bodies: FxHashMap::default(),
    };
    body::resolve_items(input, &mut module, &mut diags)?;
    Ok((module, diags))
}

#[cfg(test)]
mod tests;
