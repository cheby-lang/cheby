//! What a module uses, as the warning passes need it: every written path
//! and name with its resolution and the item that holds it, and every
//! item THIR refers to, implicit references such as satisfying functions
//! of a dictionary included.

use cheby_diag::{Span, TextRange};
use cheby_iface::{
    DefId, DefIndex, DefKind, Ice, ModuleId, ModuleInterface, Name, Namespace, PackageId,
    TypeDefKind,
};
use cheby_resolve::Res;
use cheby_syntax::NodeId;
use cheby_syntax::ast::{AstNode, AstToken, FieldInit, Interpolation, PathNode};

use crate::TypesInput;
use crate::thir::{self, Callee, Dict, ExprKind, InterpPart, OpImpl, PatKind};

/// A written path, or a name that is not a path (the root of an
/// interpolation, a punned field initializer), with its resolution.
#[derive(Clone, Debug)]
pub(super) struct PathUse {
    pub res: Res,
    /// The path or the name, without trivia.
    pub span: Span,
    /// The top-level item whose signature or body holds it.
    pub owner: Option<DefIndex>,
    /// The first segment, if the interner knows it.
    pub first: Option<Name>,
    /// How many segments: 1 for a plain name.
    pub segments: usize,
    /// The path as written, such as `text::yell`.
    pub text: String,
}

impl PathUse {
    /// The module-scope names this use goes through: the first segment in
    /// the namespace it is looked up in. A qualified path starts with a
    /// module (value namespace) or an interface (type namespace); a plain
    /// name is looked up in the namespace of what it resolved to. Locals
    /// and type parameters go through no module-scope name.
    pub fn scope_keys(
        &self,
        input: &TypesInput<'_>,
        own: &ModuleInterface,
    ) -> Vec<(Namespace, Name)> {
        let Some(first) = self.first else {
            return Vec::new();
        };
        let both = || vec![(Namespace::Type, first), (Namespace::Value, first)];
        if self.segments > 1 {
            let upper = input
                .globals
                .names
                .get(first)
                .starts_with(|c: char| c.is_ascii_uppercase());
            let ns = if upper {
                Namespace::Type
            } else {
                Namespace::Value
            };
            return vec![(ns, first)];
        }
        match self.res {
            Res::Local(_) | Res::TyParam(_) | Res::SelfTy => Vec::new(),
            Res::Module(_) => vec![(Namespace::Value, first)],
            Res::Err => both(),
            Res::Def(def) => match input.item(own, def).ok().and_then(|i| i.kind.namespace()) {
                Some(ns) => vec![(ns, first)],
                None => both(),
            },
        }
    }
}

/// Whether every body of the module checked: THIR holds every function
/// with a body, constant and test, so it holds every implicit use. A body
/// with an error is not in THIR, and the uses it would make through
/// dictionaries, operators and interpolation are unknown, so warnings
/// that count them are not given. The tests of a std module do not count,
/// since user builds skip them.
pub(super) fn thir_is_complete(input: &TypesInput<'_>, thir: &thir::Module) -> bool {
    let mut checked: Vec<DefIndex> = thir
        .fns
        .iter()
        .filter(|f| f.body.is_some())
        .map(|f| f.def.index)
        .chain(thir.consts.iter().map(|c| c.def.index))
        .chain(thir.tests.iter().map(|t| t.def.index))
        .collect();
    checked.sort_unstable();
    input.resolved.bodies.keys().all(|index| {
        let is_std_test = input.package == PackageId::STD
            && input
                .resolved
                .item(*index)
                .is_some_and(|i| i.kind == DefKind::Test);
        is_std_test || checked.binary_search(index).is_ok()
    })
}

/// The top-level item that holds `index`: itself, or a constructor's type
/// or an interface function's interface.
pub(super) fn top_level(input: &TypesInput<'_>, index: DefIndex) -> DefIndex {
    input
        .resolved
        .item(index)
        .and_then(|item| item.parent)
        .unwrap_or(index)
}

/// The text ranges of the module's top-level items, sorted by start.
fn item_ranges(input: &TypesInput<'_>) -> Vec<(TextRange, DefIndex)> {
    let mut ranges: Vec<(TextRange, DefIndex)> = input
        .resolved
        .items
        .iter_enumerated()
        .filter(|(_, item)| item.parent.is_none())
        .filter_map(|(index, item)| {
            let node = input.tree.node(item.node)?;
            Some((node.text_range(), index))
        })
        .collect();
    ranges.sort_by_key(|&(range, index)| (range.start(), index));
    ranges
}

/// The top-level item whose text holds `range`.
fn owner_of(ranges: &[(TextRange, DefIndex)], range: TextRange) -> Option<DefIndex> {
    let after = ranges.partition_point(|&(r, _)| r.start() <= range.start());
    let (candidate, index) = ranges.get(after.checked_sub(1)?)?;
    candidate.contains_range(range).then_some(*index)
}

/// Every path of the module's signatures and bodies and every name of
/// its bodies, sorted by position.
///
/// # Errors
///
/// An [`Ice`] when a recorded node is missing from the tree or is not a
/// path or a name.
pub(super) fn path_uses(input: &TypesInput<'_>) -> Result<Vec<PathUse>, Ice> {
    let ranges = item_ranges(input);
    let mut out = Vec::new();
    for (&node, &res) in &input.resolved.sig_paths {
        out.push(path_use(input, node, res, None, &ranges)?);
    }
    for (&index, body) in &input.resolved.bodies {
        for (&node, &res) in &body.paths {
            out.push(path_use(input, node, res, Some(index), &ranges)?);
        }
        for (&node, &res) in &body.names {
            out.push(name_use(input, node, res, index)?);
        }
    }
    out.sort_by_key(|u| {
        (
            u.span.file,
            u.span.range.start(),
            u.span.range.end(),
            u.segments,
        )
    });
    Ok(out)
}

fn path_use(
    input: &TypesInput<'_>,
    node: NodeId,
    res: Res,
    owner: Option<DefIndex>,
    ranges: &[(TextRange, DefIndex)],
) -> Result<PathUse, Ice> {
    let path = input
        .tree
        .node(node)
        .and_then(PathNode::cast)
        .ok_or_else(|| Ice::new("a resolved path is not a path node"))?;
    let segments: Vec<&str> = path.segments().map(AstToken::text).collect();
    let span = path.significant_span();
    let owner = owner.or_else(|| owner_of(ranges, span.range));
    Ok(PathUse {
        res,
        span,
        owner,
        first: segments
            .first()
            .and_then(|text| input.globals.names.lookup(text)),
        segments: segments.len(),
        text: segments.join("::"),
    })
}

fn name_use(
    input: &TypesInput<'_>,
    node: NodeId,
    res: Res,
    owner: DefIndex,
) -> Result<PathUse, Ice> {
    let node = input
        .tree
        .node(node)
        .ok_or_else(|| Ice::new("a resolved name is not in the tree"))?;
    let ident = Interpolation::cast(node)
        .and_then(Interpolation::root)
        .or_else(|| FieldInit::cast(node).and_then(FieldInit::name))
        .ok_or_else(|| Ice::new("a resolved name is neither an interpolation nor a field"))?;
    Ok(PathUse {
        res,
        span: ident.span(),
        owner: Some(owner),
        first: input.globals.names.lookup(ident.text()),
        segments: 1,
        text: ident.text().to_owned(),
    })
}

/// One reference of THIR to an item: from the body or entry of item
/// `owner` of the module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ThirRef {
    pub owner: DefIndex,
    pub def: DefId,
}

/// Every item the module's THIR refers to, in THIR order: callees,
/// function values, constants, constructors in expressions and patterns,
/// fields' constructors, interfaces of `dyn` conversions, and every
/// function and interface of every dictionary (satisfying functions,
/// operators, interpolation, the entry's `Show`).
pub(super) fn thir_refs(thir: &thir::Module) -> Vec<ThirRef> {
    let mut out = Vec::new();
    for f in &thir.fns {
        let mut push = |def| {
            out.push(ThirRef {
                owner: f.def.index,
                def,
            });
        };
        if let Some(body) = &f.body {
            body_refs(body, &mut push);
        }
        if let Some(entry) = &f.entry {
            entry_refs(entry, &mut push);
        }
    }
    for c in &thir.consts {
        body_refs(&c.body, &mut |def| {
            out.push(ThirRef {
                owner: c.def.index,
                def,
            });
        });
    }
    for t in &thir.tests {
        let mut push = |def| {
            out.push(ThirRef {
                owner: t.def.index,
                def,
            });
        };
        body_refs(&t.body, &mut push);
        entry_refs(&t.entry, &mut push);
    }
    out
}

fn entry_refs(entry: &thir::EntryInfo, push: &mut dyn FnMut(DefId)) {
    if let thir::EntryInfo::Result {
        show: Some(dict), ..
    } = entry
    {
        dict_refs(dict, push);
    }
}

fn body_refs(body: &thir::Body, push: &mut dyn FnMut(DefId)) {
    for expr in &body.exprs {
        match &expr.kind {
            ExprKind::Call { callee, .. } => callee_refs(callee, push),
            ExprKind::Ctor { ctor, .. } | ExprKind::Record { ctor, .. } => push(*ctor),
            ExprKind::FnRef { def, dicts, .. } => {
                push(*def);
                for dict in dicts {
                    dict_refs(dict, push);
                }
            }
            ExprKind::ConstRef { def } => push(*def),
            ExprKind::Field { index_per_ctor, .. } => {
                for &(ctor, _) in index_per_ctor {
                    push(ctor);
                }
            }
            ExprKind::RecordUpdate { fields, .. } => {
                for field in fields {
                    for &(ctor, _) in &field.index_per_ctor {
                        push(ctor);
                    }
                }
            }
            ExprKind::Binary { imp, .. } | ExprKind::Neg { imp, .. } => {
                if let OpImpl::Call(callee) = imp {
                    callee_refs(callee, push);
                }
            }
            ExprKind::Interpolate { parts } => {
                for part in parts {
                    if let InterpPart::Value {
                        show: Some(callee), ..
                    } = part
                    {
                        callee_refs(callee, push);
                    }
                }
            }
            ExprKind::DynCoerce { iface, dict, .. } => {
                push(*iface);
                dict_refs(dict, push);
            }
            _ => {}
        }
    }
    for (_, pat) in body.pats() {
        pat.walk(&mut |p| {
            if let PatKind::Ctor { ctor, .. } = &p.kind {
                push(*ctor);
            }
        });
    }
}

fn callee_refs(callee: &Callee, push: &mut dyn FnMut(DefId)) {
    match callee {
        Callee::Fn { def, dicts, .. } => {
            push(*def);
            for dict in dicts {
                dict_refs(dict, push);
            }
        }
        Callee::Ctor { ctor, .. } => push(*ctor),
        Callee::Iface { iface_fn, dict } => {
            push(*iface_fn);
            dict_refs(dict, push);
        }
        Callee::Dyn { iface_fn } => push(*iface_fn),
        _ => {}
    }
}

fn dict_refs(dict: &Dict, push: &mut dyn FnMut(DefId)) {
    match dict {
        Dict::Impl { iface, fns, .. } => {
            push(*iface);
            for f in fns {
                push(f.def);
                for inner in &f.dicts {
                    dict_refs(inner, push);
                }
            }
        }
        Dict::Param { .. } => {}
        Dict::Embedded { of, iface } => {
            push(*iface);
            dict_refs(of, push);
        }
        Dict::Tuple { iface, elems, .. } => {
            push(*iface);
            for elem in elems {
                dict_refs(elem, push);
            }
        }
        Dict::Dyn { iface, .. } => push(*iface),
    }
}

/// The functions of module `module` that its types name as equality,
/// hashing and debug-printing hooks (D-342), each with the type: the type
/// uses them.
pub(super) fn hook_refs(own: &ModuleInterface, module: ModuleId) -> Vec<ThirRef> {
    let mut out = Vec::new();
    for (index, item) in own.items.iter_enumerated() {
        let cheby_iface::ItemSig::Type(def) = &item.sig else {
            continue;
        };
        if matches!(def.kind, TypeDefKind::Builtin(_)) {
            continue;
        }
        let Some(hooks) = def.hooks else {
            continue;
        };
        for name in [hooks.equal, hooks.hash, hooks.debug] {
            if let Some(f) = own.lookup(Namespace::Value, name) {
                out.push(ThirRef {
                    owner: index,
                    def: DefId::new(module, f),
                });
            }
        }
    }
    out
}
