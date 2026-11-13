//! Queries about structural satisfaction, for the language server and
//! `cheby doc` (D-1386).
//!
//! Satisfaction is structural, so a module need not import an interface
//! its type satisfies. A query therefore runs over a [`QueryInput`], whose
//! view holds the interface of every loaded module. That view is wider than
//! a module's dependencies, which D-246 allows because a query never feeds
//! a check. It holds interfaces, never bodies.
//!
//! The answers differ from the checker's in two ways:
//!
//! - A bound the type's parameters lack does not end the query. It becomes
//!   a condition of the answer: `Queue<T>` satisfies `Show` when `T: Show`
//!   (D-064). See [`Verdict::Yes`].
//! - No dictionary is built, so a missing descriptor (E0065, D-236) does
//!   not apply.

use cheby_iface::{DefId, Generics, Ice, ItemSig, ModuleInterface, ParamIdx, Ty, TyKind};

pub use crate::ifaces::{OpFn, Reason, Unsatisfied, Want, accepts, accepts_binding};

use crate::TypesInput;
use crate::ifaces::{self, IfaceCx};
use crate::infer::contains_error;
use crate::thir::Dict;

/// The most times [`satisfies`] runs the solver. Each round but the last
/// adds one condition, and no real type needs this many on one interface.
const ROUNDS: u32 = 8;

/// What a query reads: a module's input and its interface, over a view of
/// every loaded module. The solver never reads the input's `tree` or
/// `resolved`, so they may belong to another module than `input.module`.
#[derive(Clone, Copy, Debug)]
pub struct QueryInput<'a> {
    /// The input, whose `deps` is the view of every loaded module.
    pub input: TypesInput<'a>,
    /// The interface of `input.module`.
    pub own: &'a ModuleInterface,
}

/// Whether a type satisfies an interface.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// It satisfies the interface when each bound in `when` holds.
    Yes {
        /// The satisfying functions, in `interface_fns` order. Empty for a
        /// tuple, a parameter or `dyn`, and for a type that contains an
        /// error.
        fns: Vec<DefId>,
        /// The bounds the answer depends on, in the order the solver found
        /// them. Empty for an unconditional yes (D-064).
        when: Vec<(ParamIdx, DefId)>,
    },
    /// It does not, with the reason. A near miss is a matter of this reason.
    No(Unsatisfied),
}

/// Whether `ty` satisfies `iface`. `generics` must declare every `Param` of
/// `ty`: the enclosing item's, or the type's own for a query on a
/// declaration. A type that needs more conditions than the solver's rounds
/// allow gets [`Verdict::No`] with the bound it stopped at.
///
/// # Errors
///
/// An [`Ice`] if an interface or type cannot be read, or `generics` does
/// not declare a `Param` of `ty`.
pub fn satisfies(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    generics: &Generics,
    ty: Ty,
    iface: DefId,
) -> Result<Verdict, Ice> {
    let mut assumed = generics.clone();
    let mut when = Vec::new();
    let mut round = 1;
    loop {
        let cx = IfaceCx {
            input,
            own,
            generics: &assumed,
            query: true,
        };
        let err = match ifaces::satisfy(&cx, ty, iface)? {
            Ok(dict) => {
                return Ok(Verdict::Yes {
                    fns: satisfying_fns(&dict),
                    when,
                });
            }
            Err(err) => err,
        };
        match missing_bound(input, &err) {
            Some((param, want)) if round < ROUNDS => {
                assume(&mut assumed, param, want)?;
                when.push((param, want));
                round = round.saturating_add(1);
            }
            _ => return Ok(Verdict::No(err)),
        }
    }
}

/// The verdict of `ty` for each of `ifaces`: what the server shows on a
/// type. `generics` is as for [`satisfies`]. Empty interfaces are left out,
/// because every type satisfies them (D-166), so listing one says nothing.
/// A type that contains an error has no answer, so the list is empty.
///
/// # Errors
///
/// An [`Ice`] if an interface or type cannot be read, an entry of `ifaces`
/// is not an interface, or `generics` does not declare a `Param` of `ty`.
pub fn interfaces_of(
    cx: &QueryInput<'_>,
    generics: &Generics,
    ty: Ty,
    ifaces: &[DefId],
) -> Result<Vec<(DefId, Verdict)>, Ice> {
    if contains_error(&cx.input.globals.types, ty) {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(ifaces.len());
    for &iface in ifaces {
        if !is_empty(cx, iface)? {
            out.push((iface, satisfies(&cx.input, cx.own, generics, ty, iface)?));
        }
    }
    Ok(out)
}

/// What satisfies `iface` among `types`, each applied to its own type
/// parameters, conditional answers included. An empty interface has no
/// list (D-166), and a `DefId` that is not a type is skipped.
///
/// # Errors
///
/// An [`Ice`] if an interface or type cannot be read, or `iface` is not an
/// interface.
pub fn implementors(
    cx: &QueryInput<'_>,
    iface: DefId,
    types: &[DefId],
) -> Result<Vec<(DefId, Verdict)>, Ice> {
    if is_empty(cx, iface)? {
        return Ok(Vec::new());
    }
    let interner = &cx.input.globals.types;
    let mut out = Vec::new();
    for &def in types {
        let ItemSig::Type(ty_def) = &cx.input.item(cx.own, def)?.sig else {
            continue;
        };
        let generics = &ty_def.generics;
        let args = generics
            .params
            .iter_enumerated()
            .map(|(param, _)| interner.param(param));
        let ty = interner.named(def, args.collect::<Box<[Ty]>>());
        let verdict = satisfies(&cx.input, cx.own, generics, ty, iface)?;
        if matches!(verdict, Verdict::Yes { .. }) {
            out.push((def, verdict));
        }
    }
    Ok(out)
}

/// One line on why `err` holds, for the answer of a query. `generics` names
/// the `Param`s of `err`'s types, as for [`satisfies`].
#[must_use]
pub fn reason_text(cx: &QueryInput<'_>, generics: &Generics, err: &Unsatisfied) -> String {
    let cx = IfaceCx {
        input: &cx.input,
        own: cx.own,
        generics,
        query: true,
    };
    ifaces::reason_text(&cx, err)
}

/// Whether `iface` has no functions, its embedded interfaces included.
fn is_empty(cx: &QueryInput<'_>, iface: DefId) -> Result<bool, Ice> {
    let read = |def| cx.input.item(cx.own, def).ok();
    Ok(cheby_iface::interface_fns(iface, &read)?.is_empty())
}

/// The functions of `dict`, if it was built from a nominal type's module.
fn satisfying_fns(dict: &Dict) -> Vec<DefId> {
    match dict {
        Dict::Impl { fns, .. } => fns.iter().map(|f| f.def).collect(),
        _ => Vec::new(),
    }
}

/// The bound that would settle `err`: the interface a type parameter lacks
/// at the end of its chain of nested reasons.
fn missing_bound(input: &TypesInput<'_>, err: &Unsatisfied) -> Option<(ParamIdx, DefId)> {
    match &err.reason {
        Reason::Bound { inner, .. } | Reason::Element { inner, .. } => missing_bound(input, inner),
        Reason::Param { param } => match (err.want, input.globals.types.kind(err.ty)) {
            (Want::Iface(want), TyKind::Param(on)) if on == param => Some((*param, want)),
            _ => None,
        },
        _ => None,
    }
}

/// Adds the bound `iface` to type parameter `param` of `generics`.
fn assume(generics: &mut Generics, param: ParamIdx, iface: DefId) -> Result<(), Ice> {
    let slot = generics
        .params
        .get_mut(param)
        .ok_or_else(|| Ice::new(format!("type parameter {param:?} not in scope")))?;
    let mut bounds = slot.bounds.to_vec();
    bounds.push(iface);
    slot.bounds = bounds.into();
    Ok(())
}
