//! Canonical interface encoding and its hash (B8, D-249).
//!
//! The encoding holds the non-`priv`, non-test, current items, sorted by
//! (namespace, name bytes). Definitions are referenced by module path,
//! namespace and name, types as a pre-order tree. There are no spans,
//! `FileId`s, `ModuleId`s, `DefIndex`es or interning numbers: a
//! `DefIndex` counts private and test items, so encoding it would change
//! dependents' inputs when a private helper or a `test` block is added
//! (D-249, D-265).

use std::fmt;

use cheby_package::{ModulePath, Target, Targets};
use std::hash::Hasher as _;

use siphasher::sip128::{Hasher128, SipHasher13};

use crate::{
    BuiltinType, CtorFields, DefId, DefIndex, DefKind, DepView, Generics, Globals, Ice, IfaceItem,
    ItemSig, ModuleInterface, ModuleTable, Namespace, Ty, TyKind, TypeDefKind, Vis,
};

/// The format version, first in every encoding.
const MAGIC: &[u8] = b"CHBI\x01";

/// SipHash-1-3 128 of an interface encoding, with zero keys.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InterfaceHash(pub [u8; 16]);

impl fmt::Debug for InterfaceHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InterfaceHash(")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        write!(f, ")")
    }
}

/// The hash of `iface`'s encoding. See [`encode`].
///
/// # Errors
///
/// An [`Ice`] if the interface refers to something `deps` cannot name.
pub fn interface_hash(
    iface: &ModuleInterface,
    deps: &DepView<'_>,
    modules: &ModuleTable,
    globals: &Globals,
) -> Result<InterfaceHash, Ice> {
    let bytes = encode(iface, deps, modules, globals)?;
    let mut hasher = SipHasher13::new_with_keys(0, 0);
    hasher.write(&bytes);
    Ok(InterfaceHash(hasher.finish128().as_bytes()))
}

/// The canonical, versioned encoding of `iface`, which refers to other
/// modules' definitions through `deps`.
///
/// # Errors
///
/// An [`Ice`] if the interface refers to something `deps` cannot name.
pub fn encode(
    iface: &ModuleInterface,
    deps: &DepView<'_>,
    modules: &ModuleTable,
    globals: &Globals,
) -> Result<Vec<u8>, Ice> {
    let enc = Encoder {
        iface,
        deps,
        modules,
        globals,
    };
    let mut keyed: Vec<(Namespace, &str, DefIndex)> = iface
        .items
        .iter_enumerated()
        .filter(|(index, item)| {
            item.parent.is_none()
                && item.vis != Vis::Priv
                && item.kind != DefKind::Test
                && iface.is_current(*index)
        })
        .filter_map(|(index, item)| {
            let ns = item.kind.namespace()?;
            Some((ns, globals.names.get(item.name), index))
        })
        .collect();
    keyed.sort_by(|a, b| (a.0, a.1.as_bytes()).cmp(&(b.0, b.1.as_bytes())));
    let mut out = Out::default();
    out.bytes.extend_from_slice(MAGIC);
    out.path(&modules.try_get(iface.module)?.path);
    out.len(keyed.len())?;
    for (_, _, index) in keyed {
        let item = iface.items.try_get(index, "item")?;
        enc.item(&mut out, item)?;
    }
    Ok(out.bytes)
}

#[derive(Default)]
struct Out {
    bytes: Vec<u8>,
}

impl Out {
    fn byte(&mut self, b: u8) {
        self.bytes.push(b);
    }

    fn u32(&mut self, n: u32) {
        self.bytes.extend_from_slice(&n.to_le_bytes());
    }

    fn len(&mut self, n: usize) -> Result<(), Ice> {
        let n = u32::try_from(n).map_err(|_| Ice::new("interface too large to encode"))?;
        self.u32(n);
        Ok(())
    }

    fn str(&mut self, s: &str) -> Result<(), Ice> {
        self.len(s.len())?;
        self.bytes.extend_from_slice(s.as_bytes());
        Ok(())
    }

    fn path(&mut self, path: &ModulePath) {
        // Module paths have few, short segments; a failure here is
        // impossible for paths that came from file names.
        let segments = path.segments();
        self.u32(u32::try_from(segments.len()).unwrap_or(u32::MAX));
        for segment in segments {
            let _ = self.str(segment);
        }
    }
}

struct Encoder<'a> {
    iface: &'a ModuleInterface,
    deps: &'a DepView<'a>,
    modules: &'a ModuleTable,
    globals: &'a Globals,
}

impl Encoder<'_> {
    fn name(&self, out: &mut Out, name: crate::Name) -> Result<(), Ice> {
        out.str(self.globals.names.get(name))
    }

    fn lookup(&self, def: DefId) -> Result<(&ModuleInterface, &IfaceItem), Ice> {
        let iface = if def.module == self.iface.module {
            Some(self.iface)
        } else {
            self.deps.get(def.module)
        }
        .ok_or_else(|| Ice::new(format!("interface refers to unreadable {def:?}")))?;
        let item = iface
            .item(def.index)
            .ok_or_else(|| Ice::new(format!("interface refers to missing {def:?}")))?;
        Ok((iface, item))
    }

    /// A reference to a definition: module path, a tag, and names.
    fn def_ref(&self, out: &mut Out, def: DefId) -> Result<(), Ice> {
        let (iface, item) = self.lookup(def)?;
        out.path(&self.modules.try_get(def.module)?.path);
        match (item.kind, item.kind.namespace(), item.parent) {
            (DefKind::Ctor | DefKind::InterfaceFn, _, Some(parent)) => {
                let parent = iface.items.try_get(parent, "parent item")?;
                out.byte(if item.kind == DefKind::Ctor { 2 } else { 3 });
                self.name(out, parent.name)?;
            }
            (_, Some(Namespace::Type), _) => out.byte(0),
            (_, Some(Namespace::Value), _) => out.byte(1),
            (_, None, _) => return Err(Ice::new("reference to an item in no namespace")),
        }
        self.name(out, item.name)
    }

    fn ty(&self, out: &mut Out, ty: Ty) -> Result<(), Ice> {
        match self.globals.types.kind(ty) {
            TyKind::Error => out.byte(0),
            TyKind::Never => out.byte(1),
            TyKind::Named { def, args } => {
                out.byte(2);
                self.def_ref(out, *def)?;
                self.tys(out, args)?;
            }
            TyKind::Tuple(elems) => {
                out.byte(3);
                self.tys(out, elems)?;
            }
            TyKind::Fn { params, ret } => {
                out.byte(4);
                self.tys(out, params)?;
                self.ty(out, *ret)?;
            }
            TyKind::Param(param) => {
                out.byte(5);
                out.u32(param.as_u32());
            }
            TyKind::Dyn { iface, args } => {
                out.byte(6);
                self.def_ref(out, *iface)?;
                self.tys(out, args)?;
            }
        }
        Ok(())
    }

    fn tys(&self, out: &mut Out, tys: &[Ty]) -> Result<(), Ice> {
        out.len(tys.len())?;
        tys.iter().try_for_each(|&ty| self.ty(out, ty))
    }

    fn generics(&self, out: &mut Out, generics: &Generics) -> Result<(), Ice> {
        out.len(generics.params.len())?;
        for param in &generics.params {
            self.name(out, param.name)?;
            out.len(param.bounds.len())?;
            for &bound in &param.bounds {
                self.def_ref(out, bound)?;
            }
        }
        Ok(())
    }

    fn item(&self, out: &mut Out, item: &IfaceItem) -> Result<(), Ice> {
        out.byte(kind_tag(item.kind));
        out.byte(vis_tag(item.vis));
        self.name(out, item.name)?;
        self.sig(out, &item.sig)
    }

    fn sig(&self, out: &mut Out, sig: &ItemSig) -> Result<(), Ice> {
        match sig {
            ItemSig::Fn(f) => {
                out.byte(0);
                self.generics(out, &f.generics)?;
                out.len(f.params.len())?;
                for param in &f.params {
                    self.name(out, param.name)?;
                    self.ty(out, param.ty)?;
                }
                self.ty(out, f.ret)?;
                out.byte(targets_tag(f.targets));
                match &f.deprecated {
                    Some(message) => {
                        out.byte(1);
                        out.str(message)?;
                    }
                    None => out.byte(0),
                }
            }
            ItemSig::Type(t) => {
                out.byte(1);
                self.generics(out, &t.generics)?;
                out.byte(u8::from(t.exposed));
                match &t.home {
                    Some(home) => {
                        out.byte(1);
                        out.path(home);
                    }
                    None => out.byte(0),
                }
                match &t.kind {
                    TypeDefKind::Builtin(b) => {
                        out.byte(0);
                        out.byte(builtin_tag(*b));
                    }
                    TypeDefKind::Adt { ctors } => {
                        out.byte(1);
                        out.len(ctors.len())?;
                        for &ctor in ctors {
                            let item = self.iface.items.try_get(ctor, "constructor")?;
                            self.name(out, item.name)?;
                            self.sig(out, &item.sig)?;
                        }
                    }
                    TypeDefKind::External => out.byte(2),
                }
            }
            ItemSig::Ctor(c) => {
                out.byte(2);
                out.u32(c.tag);
                match &c.fields {
                    CtorFields::Unit => out.byte(0),
                    CtorFields::Positional(tys) => {
                        out.byte(1);
                        self.tys(out, tys)?;
                    }
                    CtorFields::Named(fields) => {
                        out.byte(2);
                        out.len(fields.len())?;
                        for &(name, ty) in fields {
                            self.name(out, name)?;
                            self.ty(out, ty)?;
                        }
                    }
                }
            }
            ItemSig::Alias(a) => {
                out.byte(3);
                self.generics(out, &a.generics)?;
                self.ty(out, a.target)?;
            }
            ItemSig::Const(c) => {
                out.byte(4);
                self.ty(out, c.ty)?;
            }
            ItemSig::Interface(i) => {
                out.byte(5);
                out.len(i.embeds.len())?;
                for &embed in &i.embeds {
                    self.def_ref(out, embed)?;
                }
                out.len(i.fns.len())?;
                for &f in &i.fns {
                    let item = self.iface.items.try_get(f, "interface function")?;
                    self.name(out, item.name)?;
                    self.sig(out, &item.sig)?;
                }
            }
            ItemSig::InterfaceFn(f) => {
                out.byte(6);
                self.tys(out, &f.params)?;
                self.ty(out, f.ret)?;
            }
            ItemSig::Test => out.byte(7),
            ItemSig::Unsupported => out.byte(8),
            ItemSig::Error => out.byte(9),
        }
        Ok(())
    }
}

const fn kind_tag(kind: DefKind) -> u8 {
    match kind {
        DefKind::Fn => 0,
        DefKind::Type => 1,
        DefKind::Alias => 2,
        DefKind::Const => 3,
        DefKind::Interface => 4,
        DefKind::InterfaceFn => 5,
        DefKind::Ctor => 6,
        DefKind::Test => 7,
    }
}

const fn vis_tag(vis: Vis) -> u8 {
    match vis {
        Vis::Priv => 0,
        Vis::Package => 1,
        Vis::Pub => 2,
    }
}

const fn targets_tag(targets: Targets) -> u8 {
    let native = if targets.contains(Target::Native) {
        1
    } else {
        0
    };
    let js = if targets.contains(Target::Js) { 2 } else { 0 };
    native | js
}

const fn builtin_tag(b: BuiltinType) -> u8 {
    match b {
        BuiltinType::Int => 0,
        BuiltinType::Float => 1,
        BuiltinType::String => 2,
        BuiltinType::List => 3,
        BuiltinType::Never => 4,
        BuiltinType::I8 => 5,
        BuiltinType::I16 => 6,
        BuiltinType::I32 => 7,
        BuiltinType::I64 => 8,
        BuiltinType::U8 => 9,
        BuiltinType::U16 => 10,
        BuiltinType::U32 => 11,
        BuiltinType::U64 => 12,
        BuiltinType::F32 => 13,
    }
}

#[cfg(test)]
mod tests {
    use cheby_diag::{FileId, Span, TextRange, TextSize};

    use super::*;
    use crate::{
        FnSig, IndexVec, InterfaceSet, ModuleId, ModuleInfo, PackageId, ParamSig, builtin,
    };

    struct Setup {
        globals: Globals,
        modules: ModuleTable,
        set: InterfaceSet,
        module: ModuleId,
    }

    fn setup() -> Setup {
        let globals = Globals::new();
        let mut modules = ModuleTable::new();
        let module = modules
            .add(ModuleInfo {
                path: ModulePath::root("app"),
                package: PackageId::LOCAL,
                file: None,
            })
            .unwrap();
        let set = InterfaceSet::new(builtin::interface(&globals));
        Setup {
            globals,
            modules,
            set,
            module,
        }
    }

    fn span(start: u32) -> Span {
        Span::new(FileId::new(0), TextRange::empty(TextSize::from(start)))
    }

    fn func(s: &Setup, name: &str, vis: Vis, param: Ty, at: u32) -> IfaceItem {
        IfaceItem {
            name: s.globals.names.intern(name),
            kind: DefKind::Fn,
            vis,
            parent: None,
            span: Some(span(at)),
            sig: ItemSig::Fn(FnSig {
                generics: Generics::empty(),
                params: Box::new([ParamSig {
                    name: s.globals.names.intern("text"),
                    ty: param,
                }]),
                ret: Ty::NIL,
                targets: Targets::ALL,
                deprecated: None,
            }),
        }
    }

    fn hash_of(s: &Setup, items: Vec<IfaceItem>) -> InterfaceHash {
        let iface = ModuleInterface::new(
            s.module,
            PackageId::LOCAL,
            IndexVec::from_raw(items).unwrap(),
        )
        .unwrap();
        let allowed = [];
        let deps = DepView::new(&s.set, &allowed);
        interface_hash(&iface, &deps, &s.modules, &s.globals).unwrap()
    }

    #[test]
    fn hash_ignores_spans_private_items_and_order() {
        let s = setup();
        let list = s.globals.types.named(builtin::LIST, [Ty::INT]);
        let base = hash_of(
            &s,
            vec![
                func(&s, "print", Vis::Pub, Ty::STRING, 0),
                func(&s, "count", Vis::Package, list, 10),
            ],
        );
        // Spans and declaration order do not matter.
        let moved = hash_of(
            &s,
            vec![
                func(&s, "count", Vis::Package, list, 99),
                func(&s, "print", Vis::Pub, Ty::STRING, 50),
            ],
        );
        assert_eq!(base, moved);
        // A private helper and a test inserted before a pub item, which
        // shift every later DefIndex, do not matter either.
        let test = IfaceItem {
            name: s.globals.names.intern("a test"),
            kind: DefKind::Test,
            vis: Vis::Priv,
            parent: None,
            span: Some(span(0)),
            sig: ItemSig::Test,
        };
        let with_private = hash_of(
            &s,
            vec![
                func(&s, "helper", Vis::Priv, Ty::INT, 0),
                test,
                func(&s, "print", Vis::Pub, Ty::STRING, 0),
                func(&s, "count", Vis::Package, list, 10),
            ],
        );
        assert_eq!(base, with_private);
        // A signature change does matter.
        let changed = hash_of(
            &s,
            vec![
                func(&s, "print", Vis::Pub, Ty::INT, 0),
                func(&s, "count", Vis::Package, list, 10),
            ],
        );
        assert_ne!(base, changed);
        // So does visibility.
        let vis = hash_of(
            &s,
            vec![
                func(&s, "print", Vis::Package, Ty::STRING, 0),
                func(&s, "count", Vis::Package, list, 10),
            ],
        );
        assert_ne!(base, vis);
    }

    #[test]
    fn encoding_is_stable() {
        let s = setup();
        let items = vec![func(&s, "print", Vis::Pub, Ty::STRING, 0)];
        let iface = ModuleInterface::new(
            s.module,
            PackageId::LOCAL,
            IndexVec::from_raw(items).unwrap(),
        )
        .unwrap();
        let allowed = [];
        let deps = DepView::new(&s.set, &allowed);
        let bytes = encode(&iface, &deps, &s.modules, &s.globals).unwrap();
        assert!(bytes.starts_with(MAGIC));
        assert_eq!(
            bytes,
            encode(&iface, &deps, &s.modules, &s.globals).unwrap()
        );
        // The builtin `String` is referenced by path and name.
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("<builtin>"));
        assert!(text.contains("String"));
        assert!(!text.contains("helper"));
    }

    #[test]
    fn builtin_interface_encodes() {
        let s = setup();
        let iface = builtin::interface(&s.globals);
        let allowed = [];
        let deps = DepView::new(&s.set, &allowed);
        let a = interface_hash(&iface, &deps, &s.modules, &s.globals).unwrap();
        let b = interface_hash(&iface, &deps, &s.modules, &s.globals).unwrap();
        assert_eq!(a, b);
    }
}
