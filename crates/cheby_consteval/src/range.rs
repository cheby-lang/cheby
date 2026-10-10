//! The JS range check of evaluated constants (E0078, D-540).
//!
//! A constant that exists on `js` must not hold an `Int` outside
//! ±(2⁵³ − 1), which JS cannot represent exactly (D-037). The value is
//! walked by the constant's type, so `I64` and the other sized integers,
//! which JS represents exactly, are never reported: the type becomes a
//! small graph of [`RangeNode`]s (recursive types stay finite), and the
//! runtime walks the value along it with a worklist
//! ([`cheby_runtime::consteval::find_out_of_js_range`]).

use std::collections::HashMap;

use cheby_iface::{
    BuiltinType, CtorFields, DefId, Ice, IfaceItem, ItemSig, Ty, TyInterner, TyKind, TypeDefKind,
};
use cheby_runtime::consteval::RangeNode;

/// What the JS range check reads about types: the type table and the
/// interface items of every module of the build.
pub struct ConstTypes<'a> {
    pub types: &'a TyInterner,
    /// The interface item `def`, from any module of the build, the builtin
    /// one included; `None` if it is unknown.
    pub item: &'a dyn Fn(DefId) -> Option<&'a IfaceItem>,
}

impl std::fmt::Debug for ConstTypes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConstTypes").finish_non_exhaustive()
    }
}

/// More nodes than this means a type that keeps growing as it is
/// unfolded (polymorphic recursion); the rest of it is not checked.
const MAX_NODES: usize = 1 << 16;

/// The range graph of a constant's type, rooted at index 0, or `None` when
/// the type holds no `Int` anywhere, so nothing needs walking.
///
/// # Errors
///
/// An [`Ice`] if an item the type names is unknown.
pub(crate) fn range_graph(types: &ConstTypes<'_>, ty: Ty) -> Result<Option<Vec<RangeNode>>, Ice> {
    let mut builder = Builder {
        types,
        nodes: Vec::new(),
        memo: HashMap::new(),
    };
    builder.node(ty)?;
    let nodes = builder.nodes;
    Ok(holds_int(&nodes).then_some(nodes))
}

/// Whether an `Int` node is reachable from node 0.
fn holds_int(nodes: &[RangeNode]) -> bool {
    let mut seen = vec![false; nodes.len()];
    let mut work = vec![0_usize];
    while let Some(index) = work.pop() {
        match seen.get_mut(index) {
            Some(seen) if !*seen => *seen = true,
            _ => continue,
        }
        match nodes.get(index) {
            Some(RangeNode::Int) => return true,
            Some(RangeNode::Tuple(parts)) => work.extend(parts.iter().copied()),
            Some(RangeNode::List(element) | RangeNode::Set(element)) => work.push(*element),
            Some(RangeNode::Map(key, value)) => work.extend([*key, *value]),
            Some(RangeNode::Adt(ctors)) => {
                work.extend(ctors.iter().flat_map(|fields| fields.iter().copied()));
            }
            Some(RangeNode::Opaque) | None => {}
        }
    }
    false
}

struct Builder<'b, 'a> {
    types: &'b ConstTypes<'a>,
    nodes: Vec<RangeNode>,
    memo: HashMap<Ty, usize>,
}

impl Builder<'_, '_> {
    /// The node of `ty`, made once per type. A type met again while its
    /// node is being made (a recursive type) refers to the same index.
    fn node(&mut self, ty: Ty) -> Result<usize, Ice> {
        if let Some(&index) = self.memo.get(&ty) {
            return Ok(index);
        }
        let index = self.nodes.len();
        if index >= MAX_NODES {
            // GAP(const-js-range-growing-types): a type that grows without
            // bound as it unfolds is not checked past this point.
            self.nodes.push(RangeNode::Opaque);
            return Ok(index);
        }
        self.nodes.push(RangeNode::Opaque);
        self.memo.insert(ty, index);
        let node = self.make(ty)?;
        if let Some(slot) = self.nodes.get_mut(index) {
            *slot = node;
        }
        Ok(index)
    }

    fn make(&mut self, ty: Ty) -> Result<RangeNode, Ice> {
        let types = self.types.types;
        Ok(match types.kind(ty) {
            TyKind::Tuple(parts) => {
                let parts = parts
                    .iter()
                    .map(|&part| self.node(part))
                    .collect::<Result<Box<[_]>, _>>()?;
                RangeNode::Tuple(parts)
            }
            TyKind::Named { def, args } => self.named(*def, args)?,
            // Functions and `dyn` cannot be in a constant (E0068); type
            // parameters do not occur in a constant's closed type.
            TyKind::Error
            | TyKind::Never
            | TyKind::Fn { .. }
            | TyKind::Param(_)
            | TyKind::Dyn { .. } => RangeNode::Opaque,
        })
    }

    fn item(&self, def: DefId) -> Result<&IfaceItem, Ice> {
        (self.types.item)(def)
            .ok_or_else(|| Ice::new(format!("the JS range check cannot see {def:?}")))
    }

    fn named(&mut self, def: DefId, args: &[Ty]) -> Result<RangeNode, Ice> {
        let arg = |n: usize| {
            args.get(n)
                .copied()
                .ok_or_else(|| Ice::new(format!("{def:?} lacks type argument {n}")))
        };
        let item = self.item(def)?;
        match &item.sig {
            ItemSig::Type(type_def) => match &type_def.kind {
                TypeDefKind::Builtin(BuiltinType::Int) => Ok(RangeNode::Int),
                TypeDefKind::Builtin(BuiltinType::List) => Ok(RangeNode::List(self.node(arg(0)?)?)),
                TypeDefKind::Builtin(BuiltinType::Set) => Ok(RangeNode::Set(self.node(arg(0)?)?)),
                TypeDefKind::Builtin(BuiltinType::Map) => {
                    let key = self.node(arg(0)?)?;
                    let value = self.node(arg(1)?)?;
                    Ok(RangeNode::Map(key, value))
                }
                TypeDefKind::Builtin(_) | TypeDefKind::External => Ok(RangeNode::Opaque),
                TypeDefKind::Adt { ctors } => {
                    let ctors = ctors.clone();
                    let mut by_tag: Vec<(u32, Box<[usize]>)> = Vec::with_capacity(ctors.len());
                    for &ctor in &ctors {
                        let ctor_item = self.item(DefId::new(def.module, ctor))?;
                        let ItemSig::Ctor(sig) = &ctor_item.sig else {
                            return Err(Ice::new(format!(
                                "a constructor of {def:?} has no constructor signature"
                            )));
                        };
                        let tag = sig.tag;
                        let field_types: Vec<Ty> = match &sig.fields {
                            CtorFields::Unit => Vec::new(),
                            CtorFields::Positional(tys) => tys.to_vec(),
                            CtorFields::Named(fields) => fields.iter().map(|(_, ty)| *ty).collect(),
                        };
                        let fields = field_types
                            .into_iter()
                            .map(|field| {
                                let field = subst(self.types.types, field, args)?;
                                self.node(field)
                            })
                            .collect::<Result<Box<[_]>, _>>()?;
                        by_tag.push((tag, fields));
                    }
                    by_tag.sort_by_key(|(tag, _)| *tag);
                    if by_tag.iter().all(|(_, fields)| fields.is_empty()) {
                        // `Bool`, `Order`, `Nil` and other enumerations.
                        return Ok(RangeNode::Opaque);
                    }
                    Ok(RangeNode::Adt(
                        by_tag.into_iter().map(|(_, fields)| fields).collect(),
                    ))
                }
            },
            ItemSig::Alias(alias) => {
                let target = subst(self.types.types, alias.target, args)?;
                let index = self.node(target)?;
                Ok(self.nodes.get(index).cloned().unwrap_or(RangeNode::Opaque))
            }
            _ => Ok(RangeNode::Opaque),
        }
    }
}

/// `ty` with every `Param(i)` replaced by `args[i]` (the type's own
/// parameters in a constructor field or an alias target).
fn subst(types: &TyInterner, ty: Ty, args: &[Ty]) -> Result<Ty, Ice> {
    let all = |tys: &[Ty]| -> Result<Box<[Ty]>, Ice> {
        tys.iter().map(|&t| subst(types, t, args)).collect()
    };
    let kind = match types.kind(ty) {
        TyKind::Error | TyKind::Never => return Ok(ty),
        TyKind::Param(param) => {
            return usize::try_from(param.as_u32())
                .ok()
                .and_then(|n| args.get(n))
                .copied()
                .ok_or_else(|| Ice::new(format!("no type argument for {param:?}")));
        }
        TyKind::Named { def, args: parts } => TyKind::Named {
            def: *def,
            args: all(parts)?,
        },
        TyKind::Tuple(parts) => TyKind::Tuple(all(parts)?),
        TyKind::Fn { params, ret } => TyKind::Fn {
            params: all(params)?,
            ret: subst(types, *ret, args)?,
        },
        TyKind::Dyn { iface, args: parts } => TyKind::Dyn {
            iface: *iface,
            args: all(parts)?,
        },
    };
    Ok(types.intern(kind))
}
