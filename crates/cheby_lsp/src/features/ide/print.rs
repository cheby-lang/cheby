//! Types and items as text for one module's reader (X §1.5, F §6): hover
//! reads [`Printer::type_display`] and [`Printer::item_header`], and code
//! that inserts a type reads [`Printer::type_source`].
//!
//! Every text names what the module imports as the module writes it
//! (D-1384): `import a::b as c` shows `c::T`, never `b::T`. Diagnostics
//! keep D-463's form, so they never come through here.

use cheby_driver::analysis::{
    DefId, DefKind, DepView, DisplayCx, Generics, ImportNames, ItemSig, ModulePath, ModuleView,
    SigOpts, Snapshot, Ty, TyKind, display, item_signature, missing_imports, structure_visible,
};

/// How many variants a type's header shows before `…`, as `StdIndex`
/// shows them.
const MAX_VARIANTS: usize = 16;

/// What [`Printer::type_source`] gives: a type as code, and the modules
/// the code needs that the module does not import.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "W8 and W11 write types as code")
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TypeSource {
    /// The type, valid in the module once `imports` are imported.
    pub text: String,
    /// The modules to import first, in the order the type names them. A
    /// module named here is written by its last segment in `text`: when
    /// that name is taken, the caller must alias the import (D-1395) or
    /// offer nothing.
    pub imports: Vec<ModulePath>,
}

/// Prints for a reader of one module: its own items bare, its imports by
/// the names it binds, everything else as D-463 writes it.
pub(crate) struct Printer<'s> {
    snapshot: &'s Snapshot,
    view: ModuleView<'s>,
    /// Every interface of the run, so any item can be read.
    deps: DepView<'s>,
    imports: ImportNames,
}

impl std::fmt::Debug for Printer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Printer")
            .field("module", &self.view)
            .finish_non_exhaustive()
    }
}

impl<'s> Printer<'s> {
    /// A printer for readers of `view`, a module of `snapshot`. A module
    /// with no resolution names its imports as D-463 does.
    pub(crate) fn new(snapshot: &'s Snapshot, view: ModuleView<'s>) -> Self {
        let imports = view
            .resolved()
            .map(|resolved| resolved.scope.import_names())
            .unwrap_or_default();
        Self {
            snapshot,
            view,
            deps: DepView::new(snapshot.interfaces(), snapshot.all_ids()),
            imports,
        }
    }

    /// The context of `cheby_iface::display`, with `generics` naming type
    /// parameters.
    fn cx<'p>(&'p self, generics: Option<&'p Generics>) -> DisplayCx<'p> {
        DisplayCx {
            globals: self.snapshot.globals(),
            modules: self.snapshot.table(),
            own: self.view.interface(),
            deps: &self.deps,
            generics,
            imports: Some(&self.imports),
        }
    }

    /// `ty` for reading (X §1.5): `_` for an unknown part, aliases not
    /// followed back. `generics` names the type parameters of the item the
    /// type belongs to.
    pub(crate) fn type_display(&self, ty: Ty, generics: Option<&Generics>) -> String {
        display(ty, &self.cx(generics))
    }

    /// Whether `ty` is unknown at its head, which hover says as "unknown"
    /// (F §1.8). A type with unknown parts only prints as it is.
    pub(crate) fn is_unknown(&self, ty: Ty) -> bool {
        matches!(self.snapshot.globals().types.kind(ty), TyKind::Error)
    }

    /// `ty` as code the module can hold (X §1.5). `None` when it has an
    /// unknown part, or a type parameter that `generics` does not name, so
    /// the caller offers nothing.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "W8 and W11 write types as code")
    )]
    pub(crate) fn type_source(&self, ty: Ty, generics: Option<&Generics>) -> Option<TypeSource> {
        if !self.writable(ty, generics) {
            return None;
        }
        let cx = self.cx(generics);
        let imports = missing_imports(ty, &cx)
            .into_iter()
            .map(|module| Some(self.snapshot.table().get(module)?.path.clone()))
            .collect::<Option<Vec<_>>>()?;
        Some(TypeSource {
            text: display(ty, &cx),
            imports,
        })
    }

    /// Whether every part of `ty` can be written: no unknown part, and
    /// every type parameter named by `generics`.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "W8 and W11 write types as code")
    )]
    fn writable(&self, ty: Ty, generics: Option<&Generics>) -> bool {
        match self.snapshot.globals().types.kind(ty) {
            TyKind::Error => false,
            TyKind::Never => true,
            TyKind::Param(param) => generics.is_some_and(|g| g.params.get(*param).is_some()),
            TyKind::Named { args, .. } | TyKind::Dyn { args, .. } => {
                args.iter().all(|&arg| self.writable(arg, generics))
            }
            TyKind::Tuple(elems) => elems.iter().all(|&elem| self.writable(elem, generics)),
            TyKind::Fn { params, ret } => {
                params.iter().all(|&param| self.writable(param, generics))
                    && self.writable(*ret, generics)
            }
        }
    }

    /// The declaration of `def` without its body, in `cheby fmt`'s form
    /// (F §6.1, D-1384): `pub fn map<T, U>(xs: List<T>, f: fn(T) -> U) ->
    /// List<U>`. A type shows its variants when the module may see them,
    /// and a constructor or interface function a second line naming its
    /// parent. `None` for an item the run does not have, or one hidden
    /// from it.
    pub(crate) fn item_header(&self, def: DefId) -> Option<String> {
        let iface = self.snapshot.interfaces().get(def.module)?;
        if iface.is_hidden(def.index) {
            return None;
        }
        let item = iface.item(def.index)?;
        let type_item = match item.kind {
            DefKind::Type => Some(item),
            DefKind::Ctor => item.parent.and_then(|parent| iface.item(parent)),
            DefKind::Fn
            | DefKind::Alias
            | DefKind::Const
            | DefKind::Interface
            | DefKind::InterfaceFn
            | DefKind::Test => None,
        };
        let structure = match type_item {
            Some(ty) if matches!(ty.sig, ItemSig::Type(_)) => {
                let viewer = self.snapshot.table().get(self.view.id())?;
                structure_visible(
                    ty,
                    def.module,
                    iface.package,
                    self.view.id(),
                    viewer.package,
                )
            }
            Some(_) | None => true,
        };
        let opts = SigOpts {
            structure,
            max_variants: MAX_VARIANTS,
        };
        Some(item_signature(def, item, &self.cx(None), opts))
    }
}
