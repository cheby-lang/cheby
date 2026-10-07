//! Closed types, interned per process.
//!
//! Interfaces, THIR and MIR share [`Ty`] handles. Inference has its own
//! body-local representation in `cheby_types` and is zonked into these.
//!
//! [`Ty`] is deliberately not `Ord`: its number is interning order, which
//! depends on thread scheduling once bodies check in parallel (D-263).
//! Anything that sorts or hashes types for output uses the canonical
//! encoding of [`crate::hash`].

use std::fmt::{self, Write as _};
use std::sync::{PoisonError, RwLock};

use rustc_hash::FxHashMap;

use crate::builtin;
use crate::{
    DefId, DepView, Globals, IndexVec, ModuleId, ModuleInterface, ModuleTable, Name, ParamIdx,
};

/// An interned closed type.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ty(u32);

impl fmt::Debug for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ty({})", self.0)
    }
}

/// The structure of a type.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum TyKind {
    /// The type of something that already had a reported error. It unifies
    /// with anything and silences follow-up errors.
    Error,
    /// The type of expressions that never produce a value (§3.10).
    Never,
    /// Every nominal type: `Int`, `String`, `List<T>`, `Option<T>`, user
    /// types and external types. Prelude types use the builtin `DefId`s of
    /// [`builtin`].
    Named {
        def: DefId,
        args: Box<[Ty]>,
    },
    Tuple(Box<[Ty]>),
    Fn {
        params: Box<[Ty]>,
        ret: Ty,
    },
    /// A type parameter of the enclosing item's generics.
    Param(ParamIdx),
    /// `dyn I<…>` (§8.6).
    Dyn {
        iface: DefId,
        args: Box<[Ty]>,
    },
}

macro_rules! preinterned_types {
    ($( $name:ident = $number:literal $kind:expr; )*) => {
        impl Ty {
            $( pub const $name: Ty = Ty($number); )*
        }

        /// The pre-interned types in interning order.
        fn preinterned() -> Vec<(Ty, TyKind)> {
            vec![$( (Ty::$name, $kind), )*]
        }
    };
}

fn named(def: DefId) -> TyKind {
    TyKind::Named {
        def,
        args: Box::new([]),
    }
}

preinterned_types! {
    ERROR = 0 TyKind::Error;
    NEVER = 1 TyKind::Never;
    NIL = 2 named(builtin::NIL_TY);
    BOOL = 3 named(builtin::BOOL);
    INT = 4 named(builtin::INT);
    FLOAT = 5 named(builtin::FLOAT);
    STRING = 6 named(builtin::STRING);
    ORDER = 7 named(builtin::ORDER);
    I8 = 8 named(builtin::I8);
    I16 = 9 named(builtin::I16);
    I32 = 10 named(builtin::I32);
    I64 = 11 named(builtin::I64);
    U8 = 12 named(builtin::U8);
    U16 = 13 named(builtin::U16);
    U32 = 14 named(builtin::U32);
    U64 = 15 named(builtin::U64);
    F32 = 16 named(builtin::F32);
}

static ERROR_KIND: TyKind = TyKind::Error;

/// The process-wide type table. Append-only.
///
/// The slice keeps one lock; I1/I2 may replace it with a chunked arena
/// with lock-free reads behind the same API.
pub struct TyInterner {
    inner: RwLock<TyInner>,
}

#[derive(Default)]
struct TyInner {
    map: FxHashMap<&'static TyKind, Ty>,
    kinds: Vec<&'static TyKind>,
}

impl TyInterner {
    /// A table holding exactly the `Ty::*` constants.
    #[must_use]
    pub fn new() -> Self {
        let interner = Self {
            inner: RwLock::new(TyInner::default()),
        };
        for (_, kind) in preinterned() {
            interner.intern(kind);
        }
        interner
    }

    /// The handle of `kind`, interning it on first use.
    pub fn intern(&self, kind: TyKind) -> Ty {
        if let Some(&ty) = self
            .inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .map
            .get(&kind)
        {
            return ty;
        }
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(&ty) = inner.map.get(&kind) {
            return ty;
        }
        let ty = Ty(u32::try_from(inner.kinds.len()).unwrap_or(u32::MAX));
        let leaked: &'static TyKind = Box::leak(Box::new(kind));
        inner.kinds.push(leaked);
        inner.map.insert(leaked, ty);
        ty
    }

    /// The structure of `ty`. A handle from another table may give the
    /// wrong kind; an unknown handle gives [`TyKind::Error`].
    #[must_use]
    pub fn kind(&self, ty: Ty) -> &'static TyKind {
        let inner = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        usize::try_from(ty.0)
            .ok()
            .and_then(|n| inner.kinds.get(n))
            .copied()
            .unwrap_or(&ERROR_KIND)
    }

    /// `Named { def, args }`.
    pub fn named(&self, def: DefId, args: impl Into<Box<[Ty]>>) -> Ty {
        self.intern(TyKind::Named {
            def,
            args: args.into(),
        })
    }

    /// `fn(params) -> ret`.
    pub fn func(&self, params: impl Into<Box<[Ty]>>, ret: Ty) -> Ty {
        self.intern(TyKind::Fn {
            params: params.into(),
            ret,
        })
    }

    /// The type parameter `param`.
    pub fn param(&self, param: ParamIdx) -> Ty {
        self.intern(TyKind::Param(param))
    }
}

impl Default for TyInterner {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for TyInterner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let len = self
            .inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .kinds
            .len();
        f.debug_struct("TyInterner").field("len", &len).finish()
    }
}

/// A type parameter of an item.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct GenericParam {
    pub name: Name,
    /// The interfaces it must satisfy (§8.4), in written order.
    pub bounds: Box<[DefId]>,
}

/// The type parameters of an item.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct Generics {
    pub params: IndexVec<ParamIdx, GenericParam>,
}

impl Generics {
    /// No type parameters.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            params: IndexVec::new(),
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.params.is_empty()
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.params.len()
    }
}

/// What [`display`] needs to name types.
#[derive(Clone, Copy, Debug)]
pub struct DisplayCx<'a> {
    pub globals: &'a Globals,
    pub modules: &'a ModuleTable,
    /// The module whose diagnostics are being written. Its own types are
    /// shown by bare name, as users write them there.
    pub own: Option<&'a ModuleInterface>,
    /// The interfaces that module may read, the builtin module included.
    pub deps: &'a DepView<'a>,
    /// The generics in scope, to name `Param` types.
    pub generics: Option<&'a Generics>,
}

/// A type as users write it, for diagnostics (D-463): prelude types and
/// the current module's types by bare name (`Option<Int>`,
/// `fn(String) -> Nil`), other types as `<last module segment>::<Name>`
/// (`map::Map<String, Int>`), type parameters by name.
#[must_use]
pub fn display(ty: Ty, cx: &DisplayCx<'_>) -> String {
    let mut out = String::new();
    write_ty(&mut out, ty, cx);
    out
}

fn write_ty(out: &mut String, ty: Ty, cx: &DisplayCx<'_>) {
    match cx.globals.types.kind(ty) {
        TyKind::Error => out.push('_'),
        TyKind::Never => out.push_str("Never"),
        TyKind::Named { def, args } => {
            write_def(out, *def, cx);
            write_args(out, args, cx);
        }
        TyKind::Tuple(elems) => {
            out.push('(');
            write_list(out, elems, cx);
            out.push(')');
        }
        TyKind::Fn { params, ret } => {
            out.push_str("fn(");
            write_list(out, params, cx);
            out.push_str(") -> ");
            write_ty(out, *ret, cx);
        }
        TyKind::Param(param) => {
            let name = cx
                .generics
                .and_then(|g| g.params.get(*param))
                .map(|p| cx.globals.names.get(p.name));
            match name {
                Some(name) => out.push_str(name),
                None => {
                    let _ = write!(out, "T{}", param.as_u32());
                }
            }
        }
        TyKind::Dyn { iface, args } => {
            out.push_str("dyn ");
            write_def(out, *iface, cx);
            write_args(out, args, cx);
        }
    }
}

fn write_args(out: &mut String, args: &[Ty], cx: &DisplayCx<'_>) {
    if !args.is_empty() {
        out.push('<');
        write_list(out, args, cx);
        out.push('>');
    }
}

fn write_list(out: &mut String, tys: &[Ty], cx: &DisplayCx<'_>) {
    for (n, ty) in tys.iter().enumerate() {
        if n > 0 {
            out.push_str(", ");
        }
        write_ty(out, *ty, cx);
    }
}

fn write_def(out: &mut String, def: DefId, cx: &DisplayCx<'_>) {
    let own = cx.own.filter(|own| own.module == def.module);
    let item = match own {
        Some(own) => own.item(def.index),
        None => cx.deps.item(def),
    };
    let name = item.map_or("?", |item| cx.globals.names.get(item.name));
    if def.module == ModuleId::BUILTIN {
        // A builtin item outside the prelude, such as `Map`, is written
        // through its home module, `map::Map`, except in the home module
        // itself (D-463, GAP(kernel-types)).
        if !builtin::is_prelude(def)
            && let Some(segment) = builtin::HOMES
                .iter()
                .find(|(home_def, _)| *home_def == def)
                .map(|&(_, segment)| segment)
        {
            let own_path = cx
                .own
                .and_then(|own| cx.modules.get(own.module))
                .map(|info| &info.path);
            if own_path != Some(&builtin::home_path(segment)) {
                out.push_str(segment);
                out.push_str("::");
            }
        }
    } else if own.is_none() {
        let segment = cx
            .modules
            .get(def.module)
            .and_then(|info| info.path.segments().last())
            .map_or("?", String::as_str);
        out.push_str(segment);
        out.push_str("::");
    }
    out.push_str(name);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InterfaceSet, builtin};

    #[test]
    fn constants_are_preinterned() {
        let types = TyInterner::new();
        for (ty, kind) in preinterned() {
            assert_eq!(types.kind(ty), &kind);
            assert_eq!(types.intern(kind), ty);
        }
        assert_eq!(types.kind(Ty(u32::MAX)), &TyKind::Error);
    }

    #[test]
    fn interning_is_structural() {
        let types = TyInterner::new();
        let list_int = types.named(builtin::LIST, [Ty::INT]);
        assert_eq!(types.named(builtin::LIST, vec![Ty::INT]), list_int);
        assert_ne!(types.named(builtin::LIST, [Ty::STRING]), list_int);
        let f = types.func([list_int], Ty::NIL);
        assert_eq!(types.func([list_int], Ty::NIL), f);
    }

    #[test]
    fn display_prelude_types() {
        let globals = Globals::new();
        let set = InterfaceSet::new(builtin::interface(&globals));
        let modules = ModuleTable::new();
        let allowed = [ModuleId::BUILTIN];
        let deps = DepView::new(&set, &allowed);
        let cx = DisplayCx {
            globals: &globals,
            modules: &modules,
            own: None,
            deps: &deps,
            generics: None,
        };
        let t = &globals.types;
        let opt = t.named(builtin::OPTION, [Ty::INT]);
        let tuple = t.intern(TyKind::Tuple(Box::new([Ty::STRING, Ty::BOOL])));
        let f = t.func([opt, tuple], Ty::NIL);
        assert_eq!(display(f, &cx), "fn(Option<Int>, (String, Bool)) -> Nil");
        assert_eq!(display(Ty::NEVER, &cx), "Never");
        assert_eq!(display(t.param(ParamIdx::from_u32(0)), &cx), "T0");
        // Builtin types outside the prelude are written through their home
        // module.
        let map = t.named(builtin::MAP, [Ty::STRING, Ty::INT]);
        assert_eq!(display(map, &cx), "map::Map<String, Int>");
        let set = t.named(builtin::SET, [t.named(builtin::BYTES, [])]);
        assert_eq!(display(set, &cx), "set::Set<bytes::Bytes>");
    }
}
