//! How an item reads: its declaration without a body, for hover and for
//! `cheby doc` (D-1384). Types are written through [`DisplayCx`], so
//! [`DisplayCx::imports`] decides how they are named.

use crate::ty::{read_item, write_def, write_ty};
use crate::{
    CtorFields, DefId, DefIndex, DisplayCx, FnSig, GenericParam, Generics, IfaceItem, IndexVec,
    InterfaceDef, InterfaceFnSig, ItemSig, Ty, TypeDef, TypeDefKind, Vis, sym,
};

/// What a viewer of [`item_signature`]'s text may see.
#[derive(Clone, Copy, Debug)]
pub struct SigOpts {
    /// Whether the viewer may see constructors and fields
    /// ([`crate::structure_visible`]).
    pub structure: bool,
    /// How many variants a type shows before `…`.
    pub max_variants: usize,
}

/// How an item reads, for hover and docs: its declaration without a body.
///
/// Visibility prints as it is written, and `-> Nil` is left out (D-083).
/// A type's body is in `cheby fmt`'s form, and `…` stands for the variants
/// past [`SigOpts::max_variants`]. A constructor or an interface function
/// has a second line that names its parent. A test, or an item whose
/// signature had errors, is its name.
///
/// `def` is `item`'s id. The item's parent and members are read through
/// `cx`, and an unreadable one prints as `?`, as in [`crate::display`].
#[must_use]
pub fn item_signature(def: DefId, item: &IfaceItem, cx: &DisplayCx<'_>, opts: SigOpts) -> String {
    let mut out = String::new();
    match &item.sig {
        ItemSig::Fn(sig) => write_fn(&mut out, item, sig, cx),
        ItemSig::Const(sig) => {
            out.push_str(vis_text(item.vis));
            out.push_str("const ");
            out.push_str(name_of(item, cx));
            out.push_str(": ");
            write_ty(&mut out, sig.ty, cx);
        }
        ItemSig::Type(ty) => write_type(&mut out, def, item, ty, cx, opts),
        ItemSig::Alias(alias) => {
            let cx = DisplayCx {
                generics: Some(&alias.generics),
                ..*cx
            };
            out.push_str(vis_text(item.vis));
            out.push_str("type ");
            out.push_str(name_of(item, &cx));
            write_generics(&mut out, &alias.generics, &cx);
            out.push_str(" = ");
            write_ty(&mut out, alias.target, &cx);
        }
        ItemSig::Ctor(ctor) => {
            let parent = DefId::new(def.module, ctor.ty);
            let generics = read_item(parent, cx).and_then(|parent| match &parent.sig {
                ItemSig::Type(ty) => Some(&ty.generics),
                _ => None,
            });
            let cx = DisplayCx { generics, ..*cx };
            write_ctor(&mut out, item, &ctor.fields, &cx);
            out.push_str("\nof type ");
            write_def(&mut out, parent, &cx);
            if let Some(generics) = generics {
                write_generics(&mut out, generics, &cx);
            }
        }
        ItemSig::Interface(iface) => write_interface(&mut out, def, item, iface, cx),
        ItemSig::InterfaceFn(sig) => {
            write_interface_fn(&mut out, item, sig, cx);
            if let Some(parent) = item.parent {
                out.push_str("\nof interface ");
                write_def(&mut out, DefId::new(def.module, parent), cx);
            }
        }
        ItemSig::Test | ItemSig::Error => out.push_str(name_of(item, cx)),
    }
    out
}

/// The name of `item`.
fn name_of(item: &IfaceItem, cx: &DisplayCx<'_>) -> &'static str {
    cx.globals.names.get(item.name)
}

/// The keyword of `vis` with its space. Package visibility has no keyword.
const fn vis_text(vis: Vis) -> &'static str {
    match vis {
        Vis::Pub => "pub ",
        Vis::Priv => "priv ",
        Vis::Package => "",
    }
}

/// `<K: Compare + Show, V>`, or nothing without type parameters.
fn write_generics(out: &mut String, generics: &Generics, cx: &DisplayCx<'_>) {
    if generics.is_empty() {
        return;
    }
    out.push('<');
    for (n, param) in generics.params.iter().enumerate() {
        if n > 0 {
            out.push_str(", ");
        }
        out.push_str(cx.globals.names.get(param.name));
        if !param.bounds.is_empty() {
            out.push_str(": ");
            write_bound(out, &param.bounds, cx);
        }
    }
    out.push('>');
}

/// Interfaces joined with ` + `.
fn write_bound(out: &mut String, bound: &[DefId], cx: &DisplayCx<'_>) {
    for (n, iface) in bound.iter().enumerate() {
        if n > 0 {
            out.push_str(" + ");
        }
        write_def(out, *iface, cx);
    }
}

/// ` -> ret`, left out for `Nil` (D-083).
fn write_ret(out: &mut String, ret: Ty, cx: &DisplayCx<'_>) {
    if ret != Ty::NIL {
        out.push_str(" -> ");
        write_ty(out, ret, cx);
    }
}

fn write_fn(out: &mut String, item: &IfaceItem, sig: &FnSig, cx: &DisplayCx<'_>) {
    let cx = DisplayCx {
        generics: Some(&sig.generics),
        ..*cx
    };
    out.push_str(vis_text(item.vis));
    out.push_str("fn ");
    out.push_str(name_of(item, &cx));
    write_generics(out, &sig.generics, &cx);
    out.push('(');
    for (n, param) in sig.params.iter().enumerate() {
        if n > 0 {
            out.push_str(", ");
        }
        out.push_str(cx.globals.names.get(param.name));
        out.push_str(": ");
        write_ty(out, param.ty, &cx);
    }
    out.push(')');
    write_ret(out, sig.ret, &cx);
}

fn write_type(
    out: &mut String,
    def: DefId,
    item: &IfaceItem,
    ty: &TypeDef,
    cx: &DisplayCx<'_>,
    opts: SigOpts,
) {
    let cx = DisplayCx {
        generics: Some(&ty.generics),
        ..*cx
    };
    if !matches!(ty.kind, TypeDefKind::Builtin(_)) {
        out.push_str(vis_text(item.vis));
        if ty.exposed && opts.structure {
            out.push_str("exposed ");
        }
    }
    out.push_str("type ");
    out.push_str(name_of(item, &cx));
    write_generics(out, &ty.generics, &cx);
    if let TypeDefKind::Adt { ctors } = &ty.kind
        && opts.structure
        && !ctors.is_empty()
    {
        out.push_str(" {\n");
        write_type_body(out, def, item, ctors, &cx, opts);
        out.push('}');
    }
}

/// The lines of a type's body. A single constructor named like the type
/// with named fields is the shorthand of §4.3.3, and shows its fields.
fn write_type_body(
    out: &mut String,
    def: DefId,
    item: &IfaceItem,
    ctors: &[DefIndex],
    cx: &DisplayCx<'_>,
    opts: SigOpts,
) {
    let read = |index: DefIndex| read_item(DefId::new(def.module, index), cx);
    if let [only] = ctors
        && let Some(ctor) = read(*only)
        && ctor.name == item.name
        && let ItemSig::Ctor(sig) = &ctor.sig
        && let CtorFields::Named(fields) = &sig.fields
    {
        for (name, ty) in fields {
            out.push_str("  ");
            out.push_str(cx.globals.names.get(*name));
            out.push_str(": ");
            write_ty(out, *ty, cx);
            out.push_str(",\n");
        }
        return;
    }
    for index in ctors.iter().take(opts.max_variants) {
        out.push_str("  ");
        match read(*index) {
            Some(ctor) => match &ctor.sig {
                ItemSig::Ctor(sig) => write_ctor(out, ctor, &sig.fields, cx),
                _ => out.push_str(name_of(ctor, cx)),
            },
            None => out.push('?'),
        }
        out.push_str(",\n");
    }
    if ctors.len() > opts.max_variants {
        out.push_str("  …\n");
    }
}

/// `None`, `Some(T)` or `Circle { radius: Float }`.
fn write_ctor(out: &mut String, item: &IfaceItem, fields: &CtorFields, cx: &DisplayCx<'_>) {
    out.push_str(name_of(item, cx));
    match fields {
        CtorFields::Unit => {}
        CtorFields::Positional(tys) => {
            out.push('(');
            for (n, ty) in tys.iter().enumerate() {
                if n > 0 {
                    out.push_str(", ");
                }
                write_ty(out, *ty, cx);
            }
            out.push(')');
        }
        CtorFields::Named(fields) => {
            out.push_str(" { ");
            for (n, (name, ty)) in fields.iter().enumerate() {
                if n > 0 {
                    out.push_str(", ");
                }
                out.push_str(cx.globals.names.get(*name));
                out.push_str(": ");
                write_ty(out, *ty, cx);
            }
            out.push_str(" }");
        }
    }
}

/// An interface. One without functions has no body, as §8.1 writes it.
fn write_interface(
    out: &mut String,
    def: DefId,
    item: &IfaceItem,
    iface: &InterfaceDef,
    cx: &DisplayCx<'_>,
) {
    out.push_str(vis_text(item.vis));
    out.push_str("interface ");
    out.push_str(name_of(item, cx));
    if !iface.embeds.is_empty() {
        out.push_str(": ");
        write_bound(out, &iface.embeds, cx);
    }
    if iface.fns.is_empty() {
        return;
    }
    out.push_str(" {\n");
    for index in &iface.fns {
        out.push_str("  ");
        match read_item(DefId::new(def.module, *index), cx) {
            Some(function) => match &function.sig {
                ItemSig::InterfaceFn(sig) => write_interface_fn(out, function, sig, cx),
                _ => out.push_str(name_of(function, cx)),
            },
            None => out.push('?'),
        }
        out.push('\n');
    }
    out.push('}');
}

/// `fn add(Self, Self) -> Self`. `Self` is `ParamIdx` 0.
fn write_interface_fn(
    out: &mut String,
    item: &IfaceItem,
    sig: &InterfaceFnSig,
    cx: &DisplayCx<'_>,
) {
    let mut params = IndexVec::new();
    // An empty vector always has room for one element.
    let _ = params.push(GenericParam {
        name: sym::SELF_TY,
        bounds: Box::new([]),
    });
    let generics = Generics { params };
    let cx = DisplayCx {
        generics: Some(&generics),
        ..*cx
    };
    out.push_str("fn ");
    out.push_str(name_of(item, &cx));
    out.push('(');
    for (n, ty) in sig.params.iter().enumerate() {
        if n > 0 {
            out.push_str(", ");
        }
        write_ty(out, *ty, &cx);
    }
    out.push(')');
    write_ret(out, sig.ret, &cx);
}

#[cfg(test)]
mod tests {
    use cheby_package::{ModulePath, Targets};

    use super::*;
    use crate::{
        AliasDef, ConstSig, CtorSig, DefKind, DepView, Globals, ImportNames, InterfaceSet,
        ModuleId, ModuleInfo, ModuleInterface, ModuleTable, PackageId, ParamIdx, ParamSig, TyKind,
        builtin, display, missing_imports,
    };

    /// Module `app` reads `a::b` and `std::map`. `own`'s items are named by
    /// `ix`, and `a::b` has the type `T` and the interface `Compare`.
    struct Fixture {
        globals: Globals,
        modules: ModuleTable,
        set: InterfaceSet,
        own: ModuleInterface,
        dep: ModuleId,
        home: ModuleId,
        allowed: Vec<ModuleId>,
    }

    const SHOW: usize = 0;
    const LOG: usize = 1;
    const HIDDEN: usize = 2;
    const SIZE: usize = 3;
    const SHAPE: usize = 4;
    const CIRCLE: usize = 5;
    const PAIR: usize = 6;
    const EMPTY: usize = 7;
    const BOXED: usize = 8;
    const HANDLE: usize = 10;
    const ENV: usize = 11;
    const TWICE: usize = 12;
    const NOTIFIER: usize = 13;
    const NOTIFY: usize = 14;
    const ADD: usize = 15;
    const NUM: usize = 16;
    const ADDS: usize = 17;
    const BROKEN: usize = 18;

    fn ix(n: usize) -> DefIndex {
        DefIndex::from_u32(u32::try_from(n).unwrap())
    }

    fn item(g: &Globals, name: &str, kind: DefKind, vis: Vis, sig: ItemSig) -> IfaceItem {
        IfaceItem {
            name: g.names.intern(name),
            kind,
            vis,
            parent: None,
            span: None,
            targets: Targets::ALL,
            deprecated: None,
            sig,
        }
    }

    fn child(g: &Globals, name: &str, kind: DefKind, parent: usize, sig: ItemSig) -> IfaceItem {
        IfaceItem {
            parent: Some(ix(parent)),
            ..item(g, name, kind, Vis::Pub, sig)
        }
    }

    fn generics(g: &Globals, params: &[(&str, &[DefId])]) -> Generics {
        let params = params
            .iter()
            .map(|(name, bounds)| GenericParam {
                name: g.names.intern(name),
                bounds: (*bounds).into(),
            })
            .collect();
        Generics {
            params: IndexVec::from_raw(params).unwrap(),
        }
    }

    fn adt(generics: Generics, ctors: &[usize], exposed: bool) -> ItemSig {
        ItemSig::Type(TypeDef {
            generics,
            kind: TypeDefKind::Adt {
                ctors: ctors.iter().copied().map(ix).collect(),
            },
            exposed,
            home: None,
            hooks: None,
        })
    }

    fn ctor(ty: usize, tag: u32, fields: CtorFields) -> ItemSig {
        ItemSig::Ctor(CtorSig {
            ty: ix(ty),
            tag,
            fields,
        })
    }

    fn fn_sig(generics: Generics, params: Vec<ParamSig>, ret: Ty) -> ItemSig {
        ItemSig::Fn(FnSig {
            generics,
            params: params.into(),
            ret,
            clamped: Box::new([]),
        })
    }

    fn own_items(g: &Globals, dep: ModuleId) -> Vec<IfaceItem> {
        let mut items = value_items(g, dep);
        items.extend(type_items(g, dep));
        items.extend(alias_items(g));
        items.extend(interface_items(g, dep));
        items.push(item(g, "adds", DefKind::Test, Vis::Package, ItemSig::Test));
        items.push(item(g, "broken", DefKind::Fn, Vis::Pub, ItemSig::Error));
        items
    }

    fn value_items(g: &Globals, dep: ModuleId) -> Vec<IfaceItem> {
        let t = &g.types;
        let p0 = t.param(ParamIdx::from_u32(0));
        let p1 = t.param(ParamIdx::from_u32(1));
        let compare = DefId::new(dep, ix(1));
        let name = |text| g.names.intern(text);
        vec![
            item(
                g,
                "show",
                DefKind::Fn,
                Vis::Pub,
                fn_sig(
                    generics(
                        g,
                        &[("K", &[compare, builtin::SHOW]), ("V", &[builtin::SHOW])],
                    ),
                    vec![ParamSig {
                        name: name("m"),
                        ty: t.named(builtin::MAP, [p0, p1]),
                    }],
                    Ty::STRING,
                ),
            ),
            item(
                g,
                "log",
                DefKind::Fn,
                Vis::Package,
                fn_sig(
                    Generics::empty(),
                    vec![
                        ParamSig {
                            name: name("text"),
                            ty: Ty::STRING,
                        },
                        ParamSig {
                            name: name("level"),
                            ty: Ty::INT,
                        },
                    ],
                    Ty::NIL,
                ),
            ),
            item(
                g,
                "hidden",
                DefKind::Fn,
                Vis::Priv,
                fn_sig(Generics::empty(), Vec::new(), Ty::NEVER),
            ),
            item(
                g,
                "size",
                DefKind::Const,
                Vis::Package,
                ItemSig::Const(ConstSig { ty: Ty::INT }),
            ),
        ]
    }

    fn type_items(g: &Globals, dep: ModuleId) -> Vec<IfaceItem> {
        let t = &g.types;
        let p0 = t.param(ParamIdx::from_u32(0));
        let dep_t = t.named(DefId::new(dep, ix(0)), []);
        let name = |text| g.names.intern(text);
        vec![
            item(
                g,
                "Shape",
                DefKind::Type,
                Vis::Pub,
                adt(Generics::empty(), &[CIRCLE, PAIR, EMPTY], true),
            ),
            child(
                g,
                "Circle",
                DefKind::Ctor,
                SHAPE,
                ctor(
                    SHAPE,
                    0,
                    CtorFields::Named(Box::new([(name("radius"), Ty::FLOAT)])),
                ),
            ),
            child(
                g,
                "Pair",
                DefKind::Ctor,
                SHAPE,
                ctor(SHAPE, 1, CtorFields::Positional(Box::new([Ty::INT, dep_t]))),
            ),
            child(
                g,
                "Empty",
                DefKind::Ctor,
                SHAPE,
                ctor(SHAPE, 2, CtorFields::Unit),
            ),
            item(
                g,
                "Boxed",
                DefKind::Type,
                Vis::Pub,
                adt(generics(g, &[("T", &[])]), &[BOXED + 1], false),
            ),
            child(
                g,
                "Boxed",
                DefKind::Ctor,
                BOXED,
                ctor(
                    BOXED,
                    0,
                    CtorFields::Named(Box::new([
                        (name("value"), p0),
                        (name("next"), t.named(builtin::OPTION, [p0])),
                    ])),
                ),
            ),
            item(
                g,
                "Handle",
                DefKind::Type,
                Vis::Pub,
                ItemSig::Type(TypeDef {
                    generics: Generics::empty(),
                    kind: TypeDefKind::External,
                    exposed: false,
                    home: None,
                    hooks: None,
                }),
            ),
        ]
    }

    fn alias_items(g: &Globals) -> Vec<IfaceItem> {
        let t = &g.types;
        let p0 = t.param(ParamIdx::from_u32(0));
        vec![
            item(
                g,
                "Env",
                DefKind::Alias,
                Vis::Package,
                ItemSig::Alias(AliasDef {
                    generics: Generics::empty(),
                    target: t.named(builtin::MAP, [Ty::STRING, Ty::INT]),
                }),
            ),
            item(
                g,
                "Twice",
                DefKind::Alias,
                Vis::Pub,
                ItemSig::Alias(AliasDef {
                    generics: generics(g, &[("T", &[])]),
                    target: t.intern(TyKind::Tuple(Box::new([p0, p0]))),
                }),
            ),
        ]
    }

    fn interface_items(g: &Globals, dep: ModuleId) -> Vec<IfaceItem> {
        let p0 = g.types.param(ParamIdx::from_u32(0));
        let compare = DefId::new(dep, ix(1));
        vec![
            item(
                g,
                "Notifier",
                DefKind::Interface,
                Vis::Pub,
                ItemSig::Interface(InterfaceDef {
                    embeds: Box::new([builtin::SHOW]),
                    fns: Box::new([ix(NOTIFY), ix(ADD)]),
                }),
            ),
            child(
                g,
                "notify",
                DefKind::InterfaceFn,
                NOTIFIER,
                ItemSig::InterfaceFn(InterfaceFnSig {
                    params: Box::new([p0, Ty::STRING]),
                    ret: Ty::NIL,
                }),
            ),
            child(
                g,
                "add",
                DefKind::InterfaceFn,
                NOTIFIER,
                ItemSig::InterfaceFn(InterfaceFnSig {
                    params: Box::new([p0, p0]),
                    ret: p0,
                }),
            ),
            item(
                g,
                "Num",
                DefKind::Interface,
                Vis::Package,
                ItemSig::Interface(InterfaceDef {
                    embeds: Box::new([compare, builtin::SHOW]),
                    fns: Box::new([]),
                }),
            ),
        ]
    }

    fn fixture() -> Fixture {
        let globals = Globals::new();
        let mut modules = ModuleTable::new();
        let mut add = |path: ModulePath| {
            modules
                .add(ModuleInfo {
                    path,
                    package: PackageId::LOCAL,
                    file: None,
                })
                .unwrap()
        };
        let dep = add(ModulePath::root("a").child("b"));
        let home = add(builtin::home_path("map"));
        let app = add(ModulePath::root("app"));
        let dep_items = vec![
            item(
                &globals,
                "T",
                DefKind::Type,
                Vis::Pub,
                adt(Generics::empty(), &[], false),
            ),
            item(
                &globals,
                "Compare",
                DefKind::Interface,
                Vis::Pub,
                ItemSig::Interface(InterfaceDef {
                    embeds: Box::new([]),
                    fns: Box::new([]),
                }),
            ),
        ];
        let mut set = InterfaceSet::new(builtin::interface(&globals));
        let iface = |module, items| {
            ModuleInterface::new(module, PackageId::LOCAL, IndexVec::from_raw(items).unwrap())
                .unwrap()
        };
        set.insert(iface(dep, dep_items).into()).unwrap();
        let own = iface(app, own_items(&globals, dep));
        Fixture {
            globals,
            modules,
            set,
            own,
            dep,
            home,
            allowed: vec![dep, home],
        }
    }

    impl Fixture {
        /// Calls `f` with the context of `app` and these imports.
        fn with_cx<R>(
            &self,
            imports: Option<&ImportNames>,
            f: impl FnOnce(&DisplayCx<'_>) -> R,
        ) -> R {
            let deps = DepView::new(&self.set, &self.allowed);
            f(&DisplayCx {
                globals: &self.globals,
                modules: &self.modules,
                own: Some(&self.own),
                deps: &deps,
                generics: None,
                imports,
            })
        }

        fn own_sig(&self, n: usize, imports: Option<&ImportNames>, opts: SigOpts) -> String {
            let def = DefId::new(self.own.module, ix(n));
            let item = self.own.item(def.index).unwrap();
            self.with_cx(imports, |cx| item_signature(def, item, cx, opts))
        }

        fn sig(&self, n: usize) -> String {
            self.own_sig(n, None, ALL)
        }

        fn builtin_sig(&self, def: DefId) -> String {
            self.with_cx(None, |cx| {
                let item = cx.deps.item(def).unwrap();
                item_signature(def, item, cx, ALL)
            })
        }

        /// `import a::b as c` and `import std::map as m`.
        fn aliases(&self) -> ImportNames {
            let mut names = ImportNames::default();
            let c = self.globals.names.intern("c");
            let m = self.globals.names.intern("m");
            names.modules.insert(self.dep, c);
            names.modules.insert(self.home, m);
            names
        }

        fn dep_t(&self) -> Ty {
            self.globals.types.named(DefId::new(self.dep, ix(0)), [])
        }
    }

    const ALL: SigOpts = SigOpts {
        structure: true,
        max_variants: 8,
    };

    #[test]
    fn functions() {
        let f = fixture();
        assert_eq!(
            f.sig(SHOW),
            "pub fn show<K: b::Compare + Show, V: Show>(m: map::Map<K, V>) -> String"
        );
        // `-> Nil` is left out (D-083), and package visibility has no word.
        assert_eq!(f.sig(LOG), "fn log(text: String, level: Int)");
        assert_eq!(f.sig(HIDDEN), "priv fn hidden() -> Never");
    }

    #[test]
    fn constants() {
        assert_eq!(fixture().sig(SIZE), "const size: Int");
    }

    #[test]
    fn types_with_constructors() {
        let f = fixture();
        assert_eq!(
            f.sig(SHAPE),
            "pub exposed type Shape {\n  Circle { radius: Float },\n  Pair(Int, b::T),\n  \
             Empty,\n}"
        );
        // The single-variant shorthand of §4.3.3.
        assert_eq!(
            f.sig(BOXED),
            "pub type Boxed<T> {\n  value: T,\n  next: Option<T>,\n}"
        );
    }

    #[test]
    fn a_type_stops_at_max_variants() {
        let opts = SigOpts {
            max_variants: 2,
            ..ALL
        };
        assert_eq!(
            fixture().own_sig(SHAPE, None, opts),
            "pub exposed type Shape {\n  Circle { radius: Float },\n  Pair(Int, b::T),\n  …\n}"
        );
        let opts = SigOpts {
            max_variants: 3,
            ..ALL
        };
        assert!(!fixture().own_sig(SHAPE, None, opts).contains('…'));
    }

    #[test]
    fn a_type_without_structure_is_its_head() {
        let opts = SigOpts {
            structure: false,
            ..ALL
        };
        let f = fixture();
        assert_eq!(f.own_sig(SHAPE, None, opts), "pub type Shape");
        assert_eq!(f.own_sig(BOXED, None, opts), "pub type Boxed<T>");
    }

    #[test]
    fn external_and_builtin_types() {
        let f = fixture();
        assert_eq!(f.sig(HANDLE), "pub type Handle");
        assert_eq!(f.builtin_sig(builtin::MAP), "type Map<K, V>");
        assert_eq!(f.builtin_sig(builtin::INT), "type Int");
        assert_eq!(
            f.builtin_sig(builtin::OPTION),
            "pub exposed type Option<T> {\n  Some(T),\n  None,\n}"
        );
    }

    #[test]
    fn aliases() {
        let f = fixture();
        assert_eq!(f.sig(ENV), "type Env = map::Map<String, Int>");
        assert_eq!(f.sig(TWICE), "pub type Twice<T> = (T, T)");
    }

    #[test]
    fn constructors() {
        let f = fixture();
        assert_eq!(f.sig(CIRCLE), "Circle { radius: Float }\nof type Shape");
        assert_eq!(f.sig(PAIR), "Pair(Int, b::T)\nof type Shape");
        assert_eq!(f.sig(EMPTY), "Empty\nof type Shape");
        assert_eq!(
            f.sig(BOXED + 1),
            "Boxed { value: T, next: Option<T> }\nof type Boxed<T>"
        );
        assert_eq!(f.builtin_sig(builtin::SOME), "Some(T)\nof type Option<T>");
    }

    #[test]
    fn interfaces() {
        let f = fixture();
        assert_eq!(
            f.sig(NOTIFIER),
            "pub interface Notifier: Show {\n  fn notify(Self, String)\n  fn add(Self, Self) -> \
             Self\n}"
        );
        assert_eq!(f.sig(NUM), "interface Num: b::Compare + Show");
    }

    #[test]
    fn interface_functions() {
        let f = fixture();
        assert_eq!(
            f.sig(ADD),
            "fn add(Self, Self) -> Self\nof interface Notifier"
        );
        assert_eq!(
            f.sig(NOTIFY),
            "fn notify(Self, String)\nof interface Notifier"
        );
        assert_eq!(
            f.builtin_sig(builtin::SHOW_FN),
            "fn show(Self) -> String\nof interface Show"
        );
    }

    #[test]
    fn tests_and_errors_are_their_names() {
        let f = fixture();
        assert_eq!(f.sig(ADDS), "adds");
        assert_eq!(f.sig(BROKEN), "broken");
    }

    #[test]
    fn a_module_alias_names_what_it_reaches() {
        let f = fixture();
        let names = f.aliases();
        // D-463's form, which does not resolve after `import a::b as c`.
        assert_eq!(f.with_cx(None, |cx| display(f.dep_t(), cx)), "b::T");
        assert_eq!(f.with_cx(Some(&names), |cx| display(f.dep_t(), cx)), "c::T");
        assert_eq!(
            f.own_sig(SHOW, Some(&names), ALL),
            "pub fn show<K: c::Compare + Show, V: Show>(m: m::Map<K, V>) -> String"
        );
        assert_eq!(
            f.own_sig(PAIR, Some(&names), ALL),
            "Pair(Int, c::T)\nof type Shape"
        );
        assert_eq!(
            f.own_sig(NUM, Some(&names), ALL),
            "interface Num: c::Compare + Show"
        );
        assert_eq!(
            f.own_sig(ENV, Some(&names), ALL),
            "type Env = m::Map<String, Int>"
        );
    }

    #[test]
    fn an_item_alias_is_a_bare_name() {
        let f = fixture();
        let mut names = f.aliases();
        let u = f.globals.names.intern("U");
        names.items.insert(DefId::new(f.dep, ix(0)), u);
        names.items.insert(builtin::MAP, u);
        // The item's own binding wins over its module's.
        assert_eq!(f.with_cx(Some(&names), |cx| display(f.dep_t(), cx)), "U");
        let map = f.globals.types.named(builtin::MAP, [Ty::INT, Ty::INT]);
        assert_eq!(
            f.with_cx(Some(&names), |cx| display(map, cx)),
            "U<Int, Int>"
        );
    }

    #[test]
    fn imports_leave_the_rest_in_d463_form() {
        let f = fixture();
        let names = ImportNames::default();
        assert_eq!(f.with_cx(Some(&names), |cx| display(f.dep_t(), cx)), "b::T");
        let map = f.globals.types.named(builtin::MAP, [Ty::INT, Ty::INT]);
        assert_eq!(
            f.with_cx(Some(&names), |cx| display(map, cx)),
            "map::Map<Int, Int>"
        );
        // A prelude type stays bare after an import of its home module.
        let aliases = f.aliases();
        let option = f.globals.types.named(builtin::OPTION, [Ty::INT]);
        assert_eq!(
            f.with_cx(Some(&aliases), |cx| display(option, cx)),
            "Option<Int>"
        );
    }

    #[test]
    fn missing_imports_lists_each_module_once() {
        let f = fixture();
        let t = &f.globals.types;
        let map = t.named(builtin::MAP, [f.dep_t(), Ty::INT]);
        let own = t.named(DefId::new(f.own.module, ix(SHAPE)), []);
        let list = t.named(builtin::LIST, [own]);
        let ty = t.func([map, list], t.intern(TyKind::Tuple(Box::new([f.dep_t()]))));
        // `map` comes first, as `ty` names it. `own` and `List` need nothing.
        assert_eq!(
            f.with_cx(None, |cx| missing_imports(ty, cx)),
            [f.home, f.dep]
        );
        let names = f.aliases();
        assert_eq!(f.with_cx(Some(&names), |cx| missing_imports(ty, cx)), []);
        let mut only_map = ImportNames::default();
        only_map
            .modules
            .insert(f.home, f.globals.names.intern("map"));
        assert_eq!(
            f.with_cx(Some(&only_map), |cx| missing_imports(ty, cx)),
            [f.dep]
        );
    }

    #[test]
    fn missing_imports_counts_an_imported_item_and_a_dyn() {
        let f = fixture();
        let t = &f.globals.types;
        let compare = DefId::new(f.dep, ix(1));
        let boxed = t.intern(TyKind::Dyn {
            iface: compare,
            args: Box::new([]),
        });
        assert_eq!(f.with_cx(None, |cx| missing_imports(boxed, cx)), [f.dep]);
        let mut names = ImportNames::default();
        names
            .items
            .insert(compare, f.globals.names.intern("Compare"));
        assert_eq!(f.with_cx(Some(&names), |cx| missing_imports(boxed, cx)), []);
        assert_eq!(
            f.with_cx(None, |cx| missing_imports(
                t.param(ParamIdx::from_u32(0)),
                cx
            )),
            []
        );
    }
}
