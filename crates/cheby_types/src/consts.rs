//! Constant restrictions and cycles (§4.5, C13): a constant's declared
//! type contains no function, `dyn`, external or handle type (E0068,
//! D-139, D-237), and constants do not refer to each other in a cycle
//! through direct references (E0069, D-511).
//!
//! "Contains" looks through tuples, the elements of the builtin
//! collections and the fields of every constructor of the types it meets,
//! transitively, so `Option<Db>` and a record with a function field are
//! rejected like `Db` and `fn() -> Int`. A type argument that no field
//! uses holds no value, so it is not looked at (D-511).
//! Handle types other than external types (channels, fibers) arrive with
//! the tier-2 standard library and join the check then.
//!
//! Cycles are found among the constants of this module only: a constant
//! of another module cannot refer back, since imports are acyclic
//! (§7.3). A cycle through a function call, such as `const a: Int = f()`
//! where `f` reads `a`, is found when constants are evaluated (E6).
//!
//! Constants are not evaluated until E6 (D-459, D-478):
//! until then `cheby check` checks their types, restrictions and direct
//! cycles only, and `cheby run` reports E0038 for a reachable constant.

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{
    DefId, DefIndex, DefKind, Ice, ItemSig, ModuleId, ModuleInterface, Ty, TyKind, TypeDefKind,
};
use cheby_syntax::ast::{AstNode, ConstDecl};
use rustc_hash::FxHashSet;

use crate::TypesInput;
use crate::ifaces::tyutil::subst;
use crate::thir::{self, ExprKind};

/// The constant errors of the module. Called by [`crate::check_bodies`]
/// after every item is checked.
///
/// # Errors
///
/// An [`Ice`] if THIR and the interfaces disagree.
pub(crate) fn check_module(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    thir: &thir::Module,
) -> Result<Vec<Diagnostic>, Ice> {
    let mut diags = Vec::new();
    for (index, item) in own.items.iter_enumerated() {
        if item.kind != DefKind::Const {
            continue;
        }
        // A signature with errors is already reported.
        let ItemSig::Const(sig) = &item.sig else {
            continue;
        };
        if let Some(found) = Search::new(input, own).find(sig.ty)? {
            diags.push(invalid_type(input, own, index, sig.ty, &found)?);
        }
    }
    diags.extend(cycles(input, own, thir)?);
    Ok(diags)
}

/// What a constant's type must not contain.
enum Forbidden {
    /// A function type.
    Function(Ty),
    /// A `dyn` type.
    Dyn(Ty),
    /// An external type (§12.5.3).
    External(Ty),
}

/// A forbidden type found inside a constant's type, with the last
/// constructor whose field holds it, if it was found through one.
struct Found {
    what: Forbidden,
    /// The type whose constructor field holds the forbidden type, and that
    /// field's type.
    through: Option<(Ty, Ty)>,
}

/// A depth-first search for a forbidden type.
struct Search<'a, 'i> {
    input: &'a TypesInput<'i>,
    own: &'a ModuleInterface,
    seen: FxHashSet<Ty>,
}

impl<'a, 'i> Search<'a, 'i> {
    fn new(input: &'a TypesInput<'i>, own: &'a ModuleInterface) -> Self {
        Self {
            input,
            own,
            seen: FxHashSet::default(),
        }
    }

    fn find(&mut self, ty: Ty) -> Result<Option<Found>, Ice> {
        if !self.seen.insert(ty) {
            return Ok(None);
        }
        let types = &self.input.globals.types;
        let direct = |what| {
            Ok(Some(Found {
                what,
                through: None,
            }))
        };
        match types.kind(ty) {
            TyKind::Error | TyKind::Never | TyKind::Param(_) => Ok(None),
            TyKind::Fn { .. } => direct(Forbidden::Function(ty)),
            TyKind::Dyn { .. } => direct(Forbidden::Dyn(ty)),
            TyKind::Tuple(parts) => self.find_all(parts),
            TyKind::Named { def, args } => {
                let item = self.input.item(self.own, *def)?;
                let ItemSig::Type(def_sig) = &item.sig else {
                    // An erroneous type: already reported.
                    return self.find_all(args);
                };
                match &def_sig.kind {
                    TypeDefKind::External => direct(Forbidden::External(ty)),
                    TypeDefKind::Builtin(_) => self.find_all(args),
                    // Only the fields hold values: a type argument that no
                    // field uses holds nothing.
                    TypeDefKind::Adt { ctors } => {
                        // The prelude's own types, such as `Option`, need
                        // no note about their fields.
                        let note = def.module != ModuleId::BUILTIN;
                        for &ctor in ctors {
                            let ctor = DefId::new(def.module, ctor);
                            let ItemSig::Ctor(ctor_sig) = &self.input.item(self.own, ctor)?.sig
                            else {
                                continue;
                            };
                            for &field in ctor_sig.fields.types() {
                                let field = subst(types, field, args)?;
                                if let Some(found) = self.find(field)? {
                                    let here = note.then_some((ty, field));
                                    return Ok(Some(Found {
                                        through: found.through.or(here),
                                        what: found.what,
                                    }));
                                }
                            }
                        }
                        Ok(None)
                    }
                }
            }
        }
    }

    fn find_all(&mut self, tys: &[Ty]) -> Result<Option<Found>, Ice> {
        for &ty in tys {
            if let Some(found) = self.find(ty)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }
}

/// The field types of a constructor, in declaration order.
trait FieldTypes {
    fn types(&self) -> Vec<&Ty>;
}

impl FieldTypes for cheby_iface::CtorFields {
    fn types(&self) -> Vec<&Ty> {
        match self {
            Self::Unit => Vec::new(),
            Self::Positional(tys) => tys.iter().collect(),
            Self::Named(fields) => fields.iter().map(|(_, ty)| ty).collect(),
        }
    }
}

/// E0068 at the constant's written type.
fn invalid_type(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    index: DefIndex,
    declared: Ty,
    found: &Found,
) -> Result<Diagnostic, Ice> {
    let item = input
        .resolved
        .item(index)
        .ok_or_else(|| Ice::new("a constant is missing from the resolution"))?;
    let name = input.globals.names.get(item.name);
    let span = input
        .tree
        .node(item.node)
        .and_then(ConstDecl::cast)
        .and_then(ConstDecl::ty)
        .map_or(item.name_span, AstNode::significant_span);
    let cx = input.display_cx(own, None);
    let show = |ty: Ty| cheby_iface::display(ty, &cx);
    let (inner, message, help) = match found.what {
        Forbidden::Function(ty) => (
            ty,
            format!("the constant `{name}` has a type that contains a function"),
            "declare a function instead, which can be called wherever the constant would be \
             used",
        ),
        Forbidden::Dyn(ty) => (
            ty,
            format!("the constant `{name}` has a type that contains `dyn`"),
            "give the constant a concrete type",
        ),
        Forbidden::External(ty) => (
            ty,
            format!(
                "the constant `{name}` has a type that contains the external type `{}`",
                show(ty)
            ),
            "create the value at run time, in a function",
        ),
    };
    let label = if inner == declared {
        String::new()
    } else {
        format!("this type contains `{}`", show(inner))
    };
    let mut diag = Diagnostic::new(codes::INVALID_CONSTANT_TYPE, message).with_primary(span, label);
    if let Some((holder, field)) = found.through {
        diag = diag.with_note(format!(
            "`{}` has a field of type `{}`",
            show(holder),
            show(field)
        ));
    }
    let what = match found.what {
        Forbidden::Function(_) => "functions",
        Forbidden::Dyn(_) => "`dyn` values",
        Forbidden::External(_) => "values of external types",
    };
    Ok(diag
        .with_note(format!(
            "a constant's value is computed at compile time and embedded in the program, which \
             cannot hold {what}"
        ))
        .with_help(help))
}

/// E0069 for every cycle of constants of this module that name each other
/// in their initializers, once per strongly connected component.
fn cycles(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    thir: &thir::Module,
) -> Result<Vec<Diagnostic>, Ice> {
    // The constants of this module that refer to others of this module,
    // in `DefIndex` order, with their references sorted.
    let nodes: Vec<&thir::ConstDef> = thir.consts.iter().collect();
    let position = |def: DefId| -> Option<usize> {
        (def.module == input.module)
            .then(|| nodes.iter().position(|c| c.def == def))
            .flatten()
    };
    let edges: Vec<Vec<usize>> = nodes
        .iter()
        .map(|c| c.deps.iter().filter_map(|&d| position(d)).collect())
        .collect();
    let mut diags = Vec::new();
    for component in strongly_connected(&edges) {
        let members: Vec<&thir::ConstDef> = component
            .iter()
            .filter_map(|&n| nodes.get(n).copied())
            .collect();
        let [first, ..] = members.as_slice() else {
            continue;
        };
        let cyclic = members.len() > 1 || first.deps.contains(&first.def);
        if !cyclic {
            continue;
        }
        diags.push(cycle_diagnostic(input, own, &members)?);
    }
    Ok(diags)
}

fn cycle_diagnostic(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    members: &[&thir::ConstDef],
) -> Result<Diagnostic, Ice> {
    let name = |def: DefId| input.name_of(own, def);
    let in_cycle: Vec<DefId> = members.iter().map(|c| c.def).collect();
    let Some(first) = members.first() else {
        return Err(Ice::new("an empty cycle of constants"));
    };
    let message = if let [only] = members {
        format!("the constant `{}` refers to itself", name(only.def))
    } else {
        let names: Vec<String> = members
            .iter()
            .map(|c| format!("`{}`", name(c.def)))
            .collect();
        format!(
            "the constants {} refer to each other in a cycle",
            join_and(&names)
        )
    };
    let mut diag = Diagnostic::new(codes::CONSTANT_CYCLE, message).with_primary(first.span, "");
    for member in members {
        if let Some((target, span)) = first_reference(&member.body, &in_cycle) {
            let label = if target == member.def {
                format!("`{}` uses itself here", name(member.def))
            } else {
                format!("`{}` uses `{}` here", name(member.def), name(target))
            };
            diag = diag.with_secondary(span, label);
        }
    }
    let help = if members.len() == 1 {
        "give it a value that does not depend on itself"
    } else {
        "give one of them a value that does not depend on the others"
    };
    Ok(diag
        .with_note(
            "a constant is evaluated from the constants it uses, at compile time, so a cycle \
             has no value",
        )
        .with_help(help))
}

/// The first reference, by position, in `body` to one of `targets`.
fn first_reference(body: &thir::Body, targets: &[DefId]) -> Option<(DefId, Span)> {
    body.exprs
        .iter()
        .filter_map(|expr| match expr.kind {
            ExprKind::ConstRef { def } if targets.contains(&def) => Some((def, expr.span)),
            _ => None,
        })
        .min_by_key(|(_, span)| (span.range.start(), span.range.end()))
}

/// "`a`", "`a` and `b`", "`a`, `b` and `c`".
fn join_and(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [only] => only.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// The strongly connected components of a graph given as adjacency lists,
/// each sorted, in the order of their smallest node (Tarjan's algorithm,
/// iterative).
fn strongly_connected(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = edges.len();
    let mut index: Vec<Option<usize>> = vec![None; n];
    let mut low: Vec<usize> = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next = 0_usize;
    let mut components: Vec<Vec<usize>> = Vec::new();
    for root in 0..n {
        if index.get(root).copied().flatten().is_some() {
            continue;
        }
        // (node, next edge to visit)
        let mut work: Vec<(usize, usize)> = vec![(root, 0)];
        while let Some(&(node, edge)) = work.last() {
            if edge == 0 {
                set(&mut index, node, Some(next));
                set(&mut low, node, next);
                next = next.saturating_add(1);
                stack.push(node);
                set(&mut on_stack, node, true);
            }
            let succ = edges.get(node).and_then(|e| e.get(edge)).copied();
            if let Some(last) = work.last_mut() {
                last.1 = edge.saturating_add(1);
            }
            if let Some(succ) = succ {
                match index.get(succ).copied().flatten() {
                    None => work.push((succ, 0)),
                    Some(succ_index) if on_stack.get(succ).copied().unwrap_or(false) => {
                        let l = low.get(node).copied().unwrap_or(0).min(succ_index);
                        set(&mut low, node, l);
                    }
                    Some(_) => {}
                }
                continue;
            }
            // Every edge of `node` is done.
            work.pop();
            let node_low = low.get(node).copied().unwrap_or(0);
            if let Some(&(parent, _)) = work.last() {
                let l = low.get(parent).copied().unwrap_or(0).min(node_low);
                set(&mut low, parent, l);
            }
            if Some(node_low) == index.get(node).copied().flatten() {
                let mut component = Vec::new();
                while let Some(member) = stack.pop() {
                    set(&mut on_stack, member, false);
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                component.sort_unstable();
                components.push(component);
            }
        }
    }
    components.sort_by_key(|c| c.first().copied());
    components
}

fn set<T>(slots: &mut [T], at: usize, value: T) {
    if let Some(slot) = slots.get_mut(at) {
        *slot = value;
    }
}

#[cfg(test)]
mod tests {
    use super::strongly_connected;

    #[test]
    fn components_in_order_of_their_smallest_node() {
        // 0 → 1 → 2 → 0, 3 → 3, 4 → 0
        let edges = vec![vec![1], vec![2], vec![0], vec![3], vec![0]];
        assert_eq!(
            strongly_connected(&edges),
            [vec![0, 1, 2], vec![3], vec![4]]
        );
        assert_eq!(strongly_connected(&[]), Vec::<Vec<usize>>::new());
    }
}

#[cfg(test)]
mod check_tests {
    use crate::test_support::{Build, build};

    /// The errors, as `CODE: message [labels]`, sorted; warnings left out.
    fn errors(b: &Build) -> Vec<String> {
        b.messages()
            .into_iter()
            .filter(|m| m.starts_with('E'))
            .collect()
    }

    #[test]
    fn valid_constants() {
        let b = build(&[(
            "",
            "pub type Point {\n  Point { x: Int, y: Int },\n}\n\npub type Tree {\n  Leaf,\n  Node(Tree, Int, Tree),\n}\n\npub const origin: Point = Point { x: 0, y: 0 }\n\npub const names: List<String> = [\"a\", \"b\"]\n\npub const pair: (Int, Option<String>) = (1, None)\n\npub const empty: Tree = Leaf\n\npub const twice: Int = once * 2\n\npub const once: Int = 1\n",
        )]);
        assert_eq!(errors(&b), Vec::<String>::new());
    }

    #[test]
    fn function_dyn_and_external_types() {
        let b = build(&[(
            "",
            "pub interface Named {\n  fn name(Self) -> String\n}\n\n@external(native, \"db_close\")\n@external(js, \"./db.mjs\", \"close\")\npub type Db\n\n@external(native, \"db_open\")\n@external(js, \"./db.mjs\", \"open\")\npub fn open() -> Db\n\npub fn double(n: Int) -> Int {\n  n * 2\n}\n\npub type Handler {\n  Handler { run: fn(Int) -> Int },\n}\n\npub type Holder<T> {\n  Holder(T),\n}\n\npub type Tag<T> {\n  Tag,\n}\n\npub const tag: Tag<fn() -> Int> = Tag\n\npub const step: fn(Int) -> Int = double\n\npub const steps: List<fn(Int) -> Int> = [double]\n\npub const handler: Handler = Handler { run: double }\n\npub const db: Db = open()\n\npub const maybe: Option<Db> = None\n\npub const held: Holder<Db> = Holder(open())\n\npub const any: List<dyn Named> = []\n",
        )]);
        assert_eq!(
            errors(&b),
            [
                "E0068: the constant `any` has a type that contains `dyn` [\"this type contains `dyn Named`\"]",
                "E0068: the constant `db` has a type that contains the external type `Db` [\"\"]",
                "E0068: the constant `handler` has a type that contains a function [\"this type contains `fn(Int) -> Int`\"]",
                "E0068: the constant `held` has a type that contains the external type `Db` [\"this type contains `Db`\"]",
                "E0068: the constant `maybe` has a type that contains the external type `Db` [\"this type contains `Db`\"]",
                "E0068: the constant `step` has a type that contains a function [\"\"]",
                "E0068: the constant `steps` has a type that contains a function [\"this type contains `fn(Int) -> Int`\"]",
            ]
        );
        let rendered = b.rendered();
        assert!(
            rendered.contains("note: `Handler` has a field of type `fn(Int) -> Int`"),
            "{rendered}"
        );
        assert!(
            rendered.contains("note: `Holder<Db>` has a field of type `Db`"),
            "{rendered}"
        );
    }

    #[test]
    fn cycles_of_constants() {
        let b = build(&[(
            "",
            "pub const width: Int = height * 2\n\npub const height: Int = width / 2\n\npub const me: Int = me + 1\n\npub const a: Int = b\n\npub const b: Int = c\n\npub const c: Int = a + width\n\npub const tail: Int = a\n\npub const through: Int = read()\n\npub fn read() -> Int {\n  through\n}\n",
        )]);
        assert_eq!(
            errors(&b),
            [
                "E0069: the constant `me` refers to itself [\"\", \"`me` uses itself here\"]",
                "E0069: the constants `a`, `b` and `c` refer to each other in a cycle [\"\", \"`a` uses `b` here\", \"`b` uses `c` here\", \"`c` uses `a` here\"]",
                "E0069: the constants `width` and `height` refer to each other in a cycle [\"\", \"`width` uses `height` here\", \"`height` uses `width` here\"]",
            ]
        );
    }
}
