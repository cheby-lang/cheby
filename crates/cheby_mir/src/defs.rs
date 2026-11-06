//! Reading the interfaces a unit may see (D-246): the unit's own module
//! and its dependencies, never another module's bodies.

use std::collections::BTreeMap;

use cheby_iface::{
    BuiltinType, CtorFields, DefId, DefKind, DepView, FnSig, Globals, Ice, Idx, IfaceItem, ItemSig,
    ModuleId, ModuleInterface, ModuleTable, Ty, TyKind, TypeDef, TypeDefKind, Vis,
};

use crate::ir::DefName;

/// The interfaces lowering reads.
#[derive(Clone, Copy, Debug)]
pub struct Defs<'a> {
    /// The module being lowered; `None` for entry thunks.
    pub own: Option<&'a ModuleInterface>,
    /// Its dependencies (for entry thunks: the modules they call).
    pub deps: &'a DepView<'a>,
    pub modules: &'a ModuleTable,
    pub globals: &'a Globals,
}

impl<'a> Defs<'a> {
    /// The interface item `def`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `def` is not readable from here.
    pub fn item(&self, def: DefId) -> Result<&'a IfaceItem, Ice> {
        let iface = match self.own {
            Some(own) if own.module == def.module => Some(own),
            _ => self.deps.get(def.module),
        };
        iface
            .and_then(|iface| iface.item(def.index))
            .ok_or_else(|| Ice::new(format!("MIR lowering cannot see {def:?}")))
    }

    /// The signature of the function `def`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `def` is not a function with a valid signature.
    pub fn fn_sig(&self, def: DefId) -> Result<&'a FnSig, Ice> {
        match &self.item(def)?.sig {
            ItemSig::Fn(sig) => Ok(sig),
            other => Err(Ice::new(format!(
                "{def:?} is called as a function but its signature is {other:?}"
            ))),
        }
    }

    /// The path of `def`, split into module and item (as `def_path` writes
    /// it).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `def` or its module is unknown.
    pub fn name(&self, def: DefId) -> Result<DefName, Ice> {
        let item = self.item(def)?;
        let names = &self.globals.names;
        let module = if def.module == ModuleId::BUILTIN {
            String::new()
        } else {
            self.modules.try_get(def.module)?.path.to_string()
        };
        let mut text = String::new();
        if item.kind == DefKind::InterfaceFn {
            let parent = item
                .parent
                .map(|p| DefId::new(def.module, p))
                .ok_or_else(|| Ice::new("interface function without its interface"))?;
            text.push_str(names.get(self.item(parent)?.name));
            text.push_str("::");
        }
        text.push_str(names.get(item.name));
        Ok(DefName {
            def,
            module: module.into(),
            item: text.into(),
        })
    }

    /// Whether no value of the closed type `ty` can hold a handle, as the
    /// unit's own module sees it through interfaces (D-715, D-205, D-246):
    /// what borrowing a parameter of type `ty` needs.
    ///
    /// Handle-free are the scalars, `String`, `Bytes` and `Never`; `List`,
    /// `Map`, `Set` and tuples of handle-free types; and ADTs whose
    /// constructor fields are handle-free for the type's arguments, with
    /// cycles counted as handle-free (the greatest fixed point). Possibly
    /// holding a handle are type variables, `fn` types, `dyn`, external
    /// types, and the types this module may not look into: another
    /// module's `priv` type (its constructors are not in the hashed
    /// interface, D-249) and another package's type that is not `exposed`
    /// (§4.3.4). A type the walk cannot read, or one that needs more than
    /// [`HANDLE_WALK_LIMIT`] instantiations (polymorphic recursion), is
    /// not handle-free either.
    #[must_use]
    pub fn handle_free(&self, ty: Ty) -> bool {
        let mut walk = HandleWalk {
            defs: self,
            known: BTreeMap::new(),
            active: Vec::new(),
            pending: Vec::new(),
            steps: 0,
        };
        walk.ty(ty, None)
    }

    /// The interface of `module` as this unit sees it.
    pub(crate) fn interface(&self, module: ModuleId) -> Option<&'a ModuleInterface> {
        match self.own {
            Some(own) if own.module == module => Some(own),
            _ => self.deps.get(module),
        }
    }

    /// Whether this unit may look into the constructors of the type `def`
    /// (D-246, §4.3.4).
    fn transparent(&self, def: DefId, item: &IfaceItem, ty: &TypeDef) -> bool {
        if def.module == ModuleId::BUILTIN {
            return true;
        }
        let Some(own) = self.own else {
            return false;
        };
        if own.module == def.module {
            return true;
        }
        if item.vis == Vis::Priv {
            return false;
        }
        let same_package = self
            .interface(def.module)
            .is_some_and(|iface| iface.package == own.package);
        same_package || ty.exposed
    }
}

/// How many ADT instantiations [`Defs::handle_free`] visits at most
/// before it answers "not handle-free".
pub const HANDLE_WALK_LIMIT: u32 = 4096;

/// The walk of [`Defs::handle_free`]. An instantiation is an ADT with the
/// handle-freedom of each type argument, so there are finitely many.
struct HandleWalk<'w, 'a> {
    defs: &'w Defs<'a>,
    /// Settled answers. `false` is settled at once; `true` only when the
    /// outermost instantiation is (no assumption remains open).
    known: BTreeMap<(DefId, Vec<bool>), bool>,
    /// The instantiations being walked, assumed handle-free meanwhile.
    active: Vec<(DefId, Vec<bool>)>,
    /// `true` answers that rest on an open assumption.
    pending: Vec<(DefId, Vec<bool>)>,
    steps: u32,
}

impl HandleWalk<'_, '_> {
    /// Whether `ty` is handle-free; `env` gives the handle-freedom of the
    /// enclosing ADT's type parameters, `None` outside one.
    fn ty(&mut self, ty: Ty, env: Option<&[bool]>) -> bool {
        match self.defs.globals.types.kind(ty) {
            TyKind::Never => true,
            TyKind::Error | TyKind::Fn { .. } | TyKind::Dyn { .. } => false,
            TyKind::Param(p) => env.and_then(|e| e.get(p.index())).copied() == Some(true),
            TyKind::Tuple(elems) => elems.iter().all(|&e| self.ty(e, env)),
            TyKind::Named { def, args } => self.named(*def, args, env),
        }
    }

    fn named(&mut self, def: DefId, args: &[Ty], env: Option<&[bool]>) -> bool {
        let Some(item) = self
            .defs
            .interface(def.module)
            .and_then(|iface| iface.item(def.index))
        else {
            return false;
        };
        let ItemSig::Type(ty) = &item.sig else {
            return false;
        };
        match &ty.kind {
            TypeDefKind::External => false,
            TypeDefKind::Builtin(b) => match b {
                BuiltinType::Int
                | BuiltinType::Float
                | BuiltinType::String
                | BuiltinType::Never
                | BuiltinType::I8
                | BuiltinType::I16
                | BuiltinType::I32
                | BuiltinType::I64
                | BuiltinType::U8
                | BuiltinType::U16
                | BuiltinType::U32
                | BuiltinType::U64
                | BuiltinType::F32
                | BuiltinType::Bytes => true,
                BuiltinType::List | BuiltinType::Map | BuiltinType::Set => {
                    args.iter().all(|&a| self.ty(a, env))
                }
            },
            TypeDefKind::Adt { ctors } => {
                if !self.defs.transparent(def, item, ty) {
                    return false;
                }
                let free: Vec<bool> = args.iter().map(|&a| self.ty(a, env)).collect();
                self.adt(def, ctors, free)
            }
        }
    }

    /// The instantiation of the ADT `def` whose type arguments have the
    /// handle-freedom `free`.
    fn adt(&mut self, def: DefId, ctors: &[cheby_iface::DefIndex], free: Vec<bool>) -> bool {
        let key = (def, free);
        if let Some(&known) = self.known.get(&key) {
            return known;
        }
        if self.active.contains(&key) {
            // Coinductive: a cycle alone holds no handle.
            return true;
        }
        self.steps = self.steps.saturating_add(1);
        if self.steps > HANDLE_WALK_LIMIT {
            return false;
        }
        self.active.push(key.clone());
        let mut result = true;
        for &ctor in ctors {
            let Some(ItemSig::Ctor(sig)) = self
                .defs
                .interface(def.module)
                .and_then(|iface| iface.item(ctor))
                .map(|item| &item.sig)
            else {
                result = false;
                break;
            };
            let fields: Vec<Ty> = match &sig.fields {
                CtorFields::Unit => Vec::new(),
                CtorFields::Positional(tys) => tys.to_vec(),
                CtorFields::Named(named) => named.iter().map(|&(_, t)| t).collect(),
            };
            if !fields.iter().all(|&f| self.ty(f, Some(&key.1))) {
                result = false;
                break;
            }
        }
        self.active.pop();
        if !result {
            // A reachable field holds a handle, whatever is assumed.
            self.known.insert(key, false);
        } else if self.active.is_empty() {
            // Every assumption made below this point was discharged.
            for k in self.pending.drain(..) {
                self.known.insert(k, true);
            }
            self.known.insert(key, true);
        } else {
            self.pending.push(key);
        }
        if !result && self.active.is_empty() {
            self.pending.clear();
        }
        result
    }
}
