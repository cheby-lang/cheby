//! Types and items as text for one module's reader (X §1.5, F §6): hover
//! reads [`Printer::type_display`] and [`Printer::item_header`], and code
//! that inserts a type reads [`Printer::type_source`].
//!
//! Every text names what the module imports as the module writes it
//! (D-1384): `import a::b as c` shows `c::T`, never `b::T`. Diagnostics
//! keep D-463's form, so they never come through here.
//!
//! A prelude type or constructor is written by its bare name, which a
//! module may bind to an item of its own (D-146). [`hides_prelude`] tells
//! code that writes such a name, and [`Printer::type_source`] gives no
//! text for a type that names one (D-1678). Displays are unchanged.

use cheby_driver::analysis::{
    DefId, DefKind, DepView, DisplayCx, Generics, ImportNames, ItemSig, ModulePath, ModuleView,
    Namespace, Res, SigOpts, Snapshot, Ty, TyKind, builtin, display, item_signature,
    missing_imports, structure_visible,
};

/// How many variants a type's header shows before `…`, as `StdIndex`
/// shows them.
const MAX_VARIANTS: usize = 16;

/// What [`Printer::type_source`] gives: a type as code, and the modules
/// the code needs that the module does not import.
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
            .map_or_default(|resolved| resolved.scope.import_names());
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
    /// unknown part, a type parameter that `generics` does not name, or a
    /// prelude type the module binds to something else, whose bare name
    /// would name the module's own item (D-1678), so the caller offers
    /// nothing.
    pub(crate) fn type_source(&self, ty: Ty, generics: Option<&Generics>) -> Option<TypeSource> {
        if !self.writable(ty, generics) || self.names_hidden(ty, generics) {
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

    /// Whether `ty` names, at any depth, a prelude type or interface that
    /// the module, or a type parameter of `generics`, hides
    /// ([`hides_prelude`]).
    fn names_hidden(&self, ty: Ty, generics: Option<&Generics>) -> bool {
        let hidden = |def| hides_prelude(self.snapshot, self.view, def, generics);
        match self.snapshot.globals().types.kind(ty) {
            TyKind::Error | TyKind::Never | TyKind::Param(_) => false,
            TyKind::Named { def, args } => {
                hidden(*def) || args.iter().any(|&arg| self.names_hidden(arg, generics))
            }
            TyKind::Dyn { iface, args } => {
                hidden(*iface) || args.iter().any(|&arg| self.names_hidden(arg, generics))
            }
            TyKind::Tuple(elems) => elems.iter().any(|&elem| self.names_hidden(elem, generics)),
            TyKind::Fn { params, ret } => {
                params
                    .iter()
                    .any(|&param| self.names_hidden(param, generics))
                    || self.names_hidden(*ret, generics)
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

/// Whether `def` is an item of the prelude (§7.6), a type, an interface
/// or a constructor such as `Option` or `Some`, whose name the module
/// `view` of `snapshot` binds in the same namespace to something else:
/// an item of its own or an import (D-146), or, for a type or an
/// interface, a type parameter of `generics`, the enclosing item's, as in
/// `fn f<Option>` (D-1719). Code that writes the bare name would then
/// name that other item, so an edit that would write it is not offered
/// (D-1678). False for an item outside the prelude, for one the module
/// imports itself, and for a module with no resolution.
pub(crate) fn hides_prelude(
    snapshot: &Snapshot,
    view: ModuleView<'_>,
    def: DefId,
    generics: Option<&Generics>,
) -> bool {
    if !builtin::is_prelude(def) {
        return false;
    }
    let Some(item) = snapshot
        .interfaces()
        .get(def.module)
        .and_then(|iface| iface.item(def.index))
    else {
        return false;
    };
    let ns = match item.kind {
        DefKind::Type | DefKind::Alias | DefKind::Interface => Namespace::Type,
        DefKind::Ctor | DefKind::Fn | DefKind::Const | DefKind::InterfaceFn => Namespace::Value,
        DefKind::Test => return false,
    };
    if ns == Namespace::Type
        && generics.is_some_and(|g| g.params.iter().any(|param| param.name == item.name))
    {
        return true;
    }
    view.resolved()
        .and_then(|resolved| resolved.scope.get(ns, item.name))
        .is_some_and(|entry| entry.res != Res::Def(def))
}

#[cfg(test)]
mod tests {
    //! The predicate of D-1678 on a module that hides a prelude type, one
    //! that hides a prelude constructor, and one that imports the prelude
    //! item itself. Front end only, so it runs on Windows too (D-1516).

    use cheby_driver::analysis::{DefId, DefKind, ModuleId, builtin};

    use super::hides_prelude;
    use crate::features::ide::hover::tests::{Temp, analyze};

    /// Whether `src/main.cheby` holding `text` hides each prelude item of
    /// `names`, of kind `kind`.
    fn hidden(text: &str, names: &[(&str, DefKind)]) -> Vec<bool> {
        let temp = Temp::package("print", &[("main.cheby", text)]);
        let path = temp.src("main.cheby");
        let analyzed = analyze(&temp.0, &[(path.clone(), text)]);
        let snapshot = &*analyzed.snapshot;
        let view = snapshot
            .module_at(snapshot.file_of(&path).expect("the file"))
            .expect("the module");
        let builtins = snapshot
            .interfaces()
            .get(ModuleId::BUILTIN)
            .expect("the builtin module");
        names
            .iter()
            .map(|&(name, kind)| {
                let (index, _) = builtins
                    .items
                    .iter_enumerated()
                    .find(|(_, item)| {
                        item.kind == kind && snapshot.globals().names.get(item.name) == name
                    })
                    .expect("a prelude item");
                let def = DefId::new(ModuleId::BUILTIN, index);
                assert!(builtin::is_prelude(def), "{name}");
                hides_prelude(snapshot, view, def, None)
            })
            .collect()
    }

    const PRELUDE: [(&str, DefKind); 4] = [
        ("Option", DefKind::Type),
        ("Some", DefKind::Ctor),
        ("Result", DefKind::Type),
        ("Ok", DefKind::Ctor),
    ];

    #[test]
    fn a_module_type_hides_the_prelude_type_only() {
        let text = "type Option {\n  Mine,\n}\n\nfn main() {\n  assert Mine == Mine\n}\n";
        assert_eq!(hidden(text, &PRELUDE), [true, false, false, false]);
    }

    #[test]
    fn a_module_constructor_hides_the_prelude_constructor_only() {
        let text = "type Maybe {\n  Some(Int),\n  Nothing,\n}\n\nfn main() {\n  assert Some(1) != Nothing\n}\n";
        assert_eq!(hidden(text, &PRELUDE), [false, true, false, false]);
    }

    #[test]
    fn nothing_is_hidden_by_default_or_by_importing_the_item_itself() {
        let text = "fn main() {\n  assert Some(1) != None\n}\n";
        assert_eq!(hidden(text, &PRELUDE), [false; 4]);
        let text = "import std::option::{Option}\n\nfn main() {\n  let x: Option<Int> = Some(1)\n  assert x != None\n}\n";
        assert_eq!(hidden(text, &PRELUDE), [false; 4]);
    }

    #[test]
    fn an_item_outside_the_prelude_is_never_hidden() {
        let text = "type Map {\n  Mine,\n}\n\nfn main() {\n  assert Mine == Mine\n}\n";
        let temp = Temp::package("print_map", &[("main.cheby", text)]);
        let path = temp.src("main.cheby");
        let analyzed = analyze(&temp.0, &[(path.clone(), text)]);
        let snapshot = &*analyzed.snapshot;
        let view = snapshot
            .module_at(snapshot.file_of(&path).expect("the file"))
            .expect("the module");
        let builtins = snapshot.interfaces().get(ModuleId::BUILTIN).unwrap();
        let (index, _) = builtins
            .items
            .iter_enumerated()
            .find(|(_, item)| snapshot.globals().names.get(item.name) == "Map")
            .expect("the builtin `Map`");
        let def = DefId::new(ModuleId::BUILTIN, index);
        assert!(!builtin::is_prelude(def));
        assert!(!hides_prelude(snapshot, view, def, None));
    }
}
