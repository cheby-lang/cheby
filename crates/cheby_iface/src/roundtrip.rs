//! The debug round trip of D-954: decode(encode(i)) must equal i's
//! non-`priv`, non-test, current part, spans aside.
//!
//! The decoder numbers items in encoded order ([`crate::encoded_order`]),
//! so the comparison pairs the original items with the decoded ones by
//! that order and maps the module's own `DefIndex`es through it; every
//! other definition must be the same `DefId`. Names and types live in the
//! same [`Globals`], so names compare as they are and types structurally.
//! A difference means the encoding misses a fact a dependent could read,
//! and early cutoff would not be sound: it is an [`Ice`] naming the module
//! and the first differing item.

use rustc_hash::FxHashMap;

use crate::{
    CtorFields, DefId, DefIndex, DepView, Generics, Globals, Ice, IfaceItem, ItemSig,
    ModuleInterface, ModuleTable, Ty, TyKind, TypeDefKind, decode_interface, encode, encoded_order,
};

/// Encodes `iface`, decodes the encoding with `deps` and compares the
/// result with `iface`'s encoded part (D-954).
///
/// `deps` must read what `iface` refers to, as for [`encode`]. Pass a
/// view that does not record reads: the round trip is a check, not a phase.
///
/// # Errors
///
/// An [`Ice`] naming the module and the first differing item if the
/// decoded interface differs, if it does not decode, or if the encoding
/// fails.
pub fn check_roundtrip(
    iface: &ModuleInterface,
    deps: &DepView<'_>,
    modules: &ModuleTable,
    globals: &Globals,
) -> Result<(), Ice> {
    let path = modules.try_get(iface.module)?.path.to_string();
    let bytes = encode(iface, deps, modules, globals)?;
    let decoded = decode_interface(iface.module, iface.package, &bytes, deps, modules, globals)
        .map_err(|e| Ice::new(format!("the interface of `{path}` does not decode: {e}")))?;
    if iface.hash.is_some() && decoded.hash != iface.hash {
        return Err(Ice::new(format!(
            "the interface of `{path}` hashes differently once decoded"
        )));
    }
    let order = encoded_order(iface, globals)?;
    if order.len() != decoded.items.len() {
        return Err(Ice::new(format!(
            "the interface of `{path}` decodes to {} items instead of {}",
            decoded.items.len(),
            order.len()
        )));
    }
    let map: FxHashMap<DefIndex, DefIndex> = order
        .iter()
        .zip(decoded.items.indices())
        .map(|(&original, new)| (original, new))
        .collect();
    let cx = Compare {
        iface,
        map: &map,
        globals,
    };
    for (&original, new) in order.iter().zip(decoded.items.indices()) {
        let a = iface.items.try_get(original, "item")?;
        let b = decoded.items.try_get(new, "decoded item")?;
        if let Some(field) = cx.item(a, b) {
            return Err(Ice::new(format!(
                "the interface of `{path}` does not round-trip: item `{}` differs in {field}",
                globals.names.get(a.name)
            )));
        }
    }
    Ok(())
}

struct Compare<'a> {
    iface: &'a ModuleInterface,
    /// The module's own items: original index to decoded index.
    map: &'a FxHashMap<DefIndex, DefIndex>,
    globals: &'a Globals,
}

impl Compare<'_> {
    /// The first field in which `a` and `b` differ, if any.
    fn item(&self, a: &IfaceItem, b: &IfaceItem) -> Option<&'static str> {
        if a.name != b.name {
            return Some("its name");
        }
        if a.kind != b.kind {
            return Some("its kind");
        }
        if a.vis != b.vis {
            return Some("its visibility");
        }
        if !self.opt_index(a.parent, b.parent) {
            return Some("its parent");
        }
        if a.targets != b.targets {
            return Some("its targets");
        }
        if a.deprecated != b.deprecated {
            return Some("its deprecation");
        }
        (!self.sig(&a.sig, &b.sig)).then_some("its signature")
    }

    fn index(&self, a: DefIndex, b: DefIndex) -> bool {
        self.map.get(&a) == Some(&b)
    }

    fn opt_index(&self, a: Option<DefIndex>, b: Option<DefIndex>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => self.index(a, b),
            _ => false,
        }
    }

    fn indexes(&self, a: &[DefIndex], b: &[DefIndex]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(&a, &b)| self.index(a, b))
    }

    fn def(&self, a: DefId, b: DefId) -> bool {
        if a.module == self.iface.module {
            b.module == a.module && self.index(a.index, b.index)
        } else {
            a == b
        }
    }

    fn defs(&self, a: &[DefId], b: &[DefId]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(&a, &b)| self.def(a, b))
    }

    fn ty(&self, a: Ty, b: Ty) -> bool {
        let types = &self.globals.types;
        match (types.kind(a), types.kind(b)) {
            (TyKind::Error, TyKind::Error) | (TyKind::Never, TyKind::Never) => true,
            (TyKind::Named { def: da, args: aa }, TyKind::Named { def: db, args: ab })
            | (
                TyKind::Dyn {
                    iface: da,
                    args: aa,
                },
                TyKind::Dyn {
                    iface: db,
                    args: ab,
                },
            ) => self.def(*da, *db) && self.tys(aa, ab),
            (TyKind::Tuple(ea), TyKind::Tuple(eb)) => self.tys(ea, eb),
            (
                TyKind::Fn {
                    params: pa,
                    ret: ra,
                },
                TyKind::Fn {
                    params: pb,
                    ret: rb,
                },
            ) => self.tys(pa, pb) && self.ty(*ra, *rb),
            (TyKind::Param(pa), TyKind::Param(pb)) => pa == pb,
            _ => false,
        }
    }

    fn tys(&self, a: &[Ty], b: &[Ty]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(&a, &b)| self.ty(a, b))
    }

    fn generics(&self, a: &Generics, b: &Generics) -> bool {
        a.params.len() == b.params.len()
            && a.params
                .iter()
                .zip(b.params.iter())
                .all(|(a, b)| a.name == b.name && self.defs(&a.bounds, &b.bounds))
    }

    fn sig(&self, a: &ItemSig, b: &ItemSig) -> bool {
        match (a, b) {
            (ItemSig::Fn(a), ItemSig::Fn(b)) => {
                self.generics(&a.generics, &b.generics)
                    && a.params.len() == b.params.len()
                    && a.params
                        .iter()
                        .zip(b.params.iter())
                        .all(|(a, b)| a.name == b.name && self.ty(a.ty, b.ty))
                    && self.ty(a.ret, b.ret)
                    && a.clamped == b.clamped
            }
            (ItemSig::Type(a), ItemSig::Type(b)) => {
                self.generics(&a.generics, &b.generics)
                    && a.exposed == b.exposed
                    && a.home == b.home
                    && a.hooks == b.hooks
                    && match (&a.kind, &b.kind) {
                        (TypeDefKind::Builtin(x), TypeDefKind::Builtin(y)) => x == y,
                        (TypeDefKind::Adt { ctors: x }, TypeDefKind::Adt { ctors: y }) => {
                            self.indexes(x, y)
                        }
                        (TypeDefKind::External, TypeDefKind::External) => true,
                        _ => false,
                    }
            }
            (ItemSig::Ctor(a), ItemSig::Ctor(b)) => {
                self.index(a.ty, b.ty)
                    && a.tag == b.tag
                    && match (&a.fields, &b.fields) {
                        (CtorFields::Unit, CtorFields::Unit) => true,
                        (CtorFields::Positional(x), CtorFields::Positional(y)) => self.tys(x, y),
                        (CtorFields::Named(x), CtorFields::Named(y)) => {
                            x.len() == y.len()
                                && x.iter()
                                    .zip(y.iter())
                                    .all(|(a, b)| a.0 == b.0 && self.ty(a.1, b.1))
                        }
                        _ => false,
                    }
            }
            (ItemSig::Alias(a), ItemSig::Alias(b)) => {
                self.generics(&a.generics, &b.generics) && self.ty(a.target, b.target)
            }
            (ItemSig::Const(a), ItemSig::Const(b)) => self.ty(a.ty, b.ty),
            (ItemSig::Interface(a), ItemSig::Interface(b)) => {
                self.defs(&a.embeds, &b.embeds) && self.indexes(&a.fns, &b.fns)
            }
            (ItemSig::InterfaceFn(a), ItemSig::InterfaceFn(b)) => {
                self.tys(&a.params, &b.params) && self.ty(a.ret, b.ret)
            }
            (ItemSig::Test, ItemSig::Test) | (ItemSig::Error, ItemSig::Error) => true,
            _ => false,
        }
    }
}
