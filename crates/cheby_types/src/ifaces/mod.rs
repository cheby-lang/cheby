//! Interfaces (chapter 8, C8-C11): satisfaction, dictionaries, operators
//! through interfaces, `Show`, `dyn` conversions and interface
//! declarations.
//!
//! Satisfaction follows §8.2: a type satisfies an interface when the
//! module that declares it has, for each function of
//! [`cheby_iface::interface_fns`], a function with the matching signature
//! that is at least as visible as the type (D-061, D-164). Builtin types
//! look in their home module (D-447), tuples follow the built-in rule
//! (D-167), and `dyn J` satisfies `J` and the interfaces it embeds (§8.6).
//! A builtin type's home module that the checked module cannot read gives
//! E0063 with a note naming the module to import (D-489).
//!
//! The checker calls these functions at the end of a top-level body, when
//! types are closed (D-234). Conventions:
//!
//! - **`Ty::ERROR`.** A type that contains an error (already reported)
//!   satisfies everything, silently. The result is a placeholder
//!   ([`Dict::Impl`] without functions, [`OpImpl::Builtin`], or
//!   [`Callee::Iface`] with that dictionary). The body already has an
//!   error, so its THIR is never returned. The same holds for a
//!   satisfying function or interface function whose signature had an
//!   error.
//! - **`Never`.** `Never` is declared in no module, so, like a function
//!   type, it satisfies only empty interfaces (D-492).
//!   Where an expected type exists, a `Never` expression has already
//!   taken it (§3.12.5), so this matters only for `Never`-typed values
//!   with no expected type, such as `"{panic}"`.
//! - **`Self` position.** An interface call resolves `Self` from the
//!   argument in the first parameter that mentions `Self`
//!   ([`self_param`], D-173). Interface signatures use `ParamIdx` 0 for
//!   `Self`.
//! - **Type parameters** of the enclosing item satisfy only their bounds
//!   and the interfaces those embed ([`Dict::Param`],
//!   [`Dict::Embedded`]), plus empty interfaces.
//! - **Descriptors.** A dictionary built for a type ([`Dict::Impl`]) and
//!   passed somewhere carries that type's descriptor (§8.5.1, D-236). A
//!   type parameter's descriptor comes from one of its bounds, so a
//!   dictionary for a type that mentions an unbounded parameter cannot be
//!   built: E0065, for conversions to `dyn` as the spec says, and also
//!   for dictionaries passed for bounds (D-502).
//!   The top-level callee of an operator, interpolation or interface call
//!   is not a dictionary and needs no descriptor.

mod decls;
mod diag;
mod solve;
#[cfg(test)]
mod tests;
pub(crate) mod tyutil;

use cheby_iface::{DefId, Generics, Ice, ModuleId, Name, ParamIdx, Ty, Vis};

pub(crate) use decls::{check_interface_decls, check_object_safe};
pub(crate) use diag::unsatisfied_diagnostic;

use crate::TypesInput;
use crate::thir::{BinOpKind, Callee, Dict, OpImpl};

/// What the interface functions need from the place they are called.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IfaceCx<'a> {
    pub input: &'a TypesInput<'a>,
    /// The checked module's own interface.
    pub own: &'a cheby_iface::ModuleInterface,
    /// The enclosing top-level item's generics: the bounds of its `Param`
    /// types.
    pub generics: &'a Generics,
}

/// Why a type does not satisfy an interface or provide an operator, for
/// [`unsatisfied_diagnostic`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Unsatisfied {
    /// The type that does not satisfy the requirement.
    pub ty: Ty,
    /// What was required of it.
    pub want: Want,
    pub reason: Reason,
}

/// A requirement on a type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Want {
    /// Every function of an interface (§8.2).
    Iface(DefId),
    /// The function behind an operator, found by name and signature in the
    /// operand type's module (§8.7, D-169).
    Op(OpFn),
}

/// The functions behind the overloadable operators (§8.7, D-118).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpFn {
    Add,
    Sub,
    Mul,
    Div,
    Neg,
    Compare,
}

impl OpFn {
    /// The function behind binary operator `op`, if it is overloadable.
    #[must_use]
    pub(crate) const fn of_binary(op: BinOpKind) -> Option<Self> {
        Some(match op {
            BinOpKind::Add => Self::Add,
            BinOpKind::Sub => Self::Sub,
            BinOpKind::Mul => Self::Mul,
            BinOpKind::Div => Self::Div,
            BinOpKind::Lt | BinOpKind::Le | BinOpKind::Gt | BinOpKind::Ge => Self::Compare,
            _ => return None,
        })
    }

    /// The function's name, such as `add`.
    #[must_use]
    pub(crate) const fn fn_name(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Sub => "sub",
            Self::Mul => "mul",
            Self::Div => "div",
            Self::Neg => "neg",
            Self::Compare => "compare",
        }
    }

    /// The name of its interface in `std::ops`, such as `Add`.
    #[must_use]
    pub(crate) const fn iface_name(self) -> &'static str {
        match self {
            Self::Add => "Add",
            Self::Sub => "Sub",
            Self::Mul => "Mul",
            Self::Div => "Div",
            Self::Neg => "Neg",
            Self::Compare => "Compare",
        }
    }
}

/// Why a requirement is not met. Nested reasons explain conditional
/// conformance and tuples.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Reason {
    /// The type's module has no function `name`, or the name is not a
    /// function. `required` is the function type it must have (`Self`
    /// replaced), `func` the interface function, if any.
    Missing {
        name: Name,
        func: Option<DefId>,
        required: Ty,
        module: ModuleId,
    },
    /// `found` has the right signature but is less visible than the type
    /// (D-164, ADR-0034).
    LessVisible {
        found: DefId,
        func: Option<DefId>,
        type_vis: Vis,
    },
    /// `found` has the wrong signature: `actual` (with its own type
    /// parameters) instead of `required`.
    Signature {
        found: DefId,
        func: Option<DefId>,
        required: Ty,
        actual: Ty,
    },
    /// The home module `std::<home>` of a builtin type is not readable
    /// from the checked module (D-447, D-489).
    HomeNotInDeps { home: &'static str },
    /// The type is declared in no module: a tuple (for interfaces other
    /// than `Show` and `Compare`), a function type, `Never`, or a `dyn`
    /// value used with an operator.
    NoModule,
    /// A type parameter none of whose bounds gives the requirement.
    Param { param: ParamIdx },
    /// `dyn has`, which satisfies only `has` and what it embeds.
    Dyn { has: DefId },
    /// Element `index` of a tuple does not satisfy the interface.
    Element {
        index: usize,
        inner: Box<Unsatisfied>,
    },
    /// The satisfying function `found` has a bound on its type parameter
    /// `param` that the instantiation does not meet (D-064).
    Bound {
        found: DefId,
        param: ParamIdx,
        inner: Box<Unsatisfied>,
    },
    /// The satisfying function `found` has the type parameter `param`,
    /// which matching its `Self` parameter against the type leaves
    /// undetermined (D-504).
    Undetermined {
        found: DefId,
        param: ParamIdx,
        actual: Ty,
    },
    /// The type mentions the type parameter `param`, which has no bound to
    /// supply its descriptor (D-236): E0065.
    Descriptor { param: ParamIdx },
    /// The requirement nests more than [`solve::DEPTH_LIMIT`] levels.
    TooDeep,
}

/// What required the interface, for the wording of E0063 and E0065.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Need {
    /// A bound of the type parameter of a called function.
    Bound { callee: DefId },
    /// `{x}` interpolation (`Show`, D-086).
    Interpolation,
    /// A conversion to `dyn`.
    Dyn,
    /// An operator through its interface (§8.7).
    Operator(BinOpKind),
    /// Unary `-` (`Neg`).
    Neg,
    /// An interface-qualified call `I::f(…)` (§8.5).
    IfaceCall { iface_fn: DefId },
}

/// Whether `ty` satisfies `iface`, with the dictionary that proves it: a
/// bound of a called function (one dictionary per bound), and
/// the `Show` of an entry point's error type, which is optional there
/// (D-182), so it never needs a diagnostic.
///
/// # Errors
///
/// An [`Ice`] if an interface or type cannot be read.
pub(crate) fn satisfy(
    cx: &IfaceCx<'_>,
    ty: Ty,
    iface: DefId,
) -> Result<Result<Dict, Unsatisfied>, Ice> {
    cx.satisfy_ty(ty, iface, 0)
}

/// The callee of the interface function `iface_fn` for `Self` =
/// `self_ty` (§8.5, D-173): [`Callee::Fn`] for a nominal type, which must
/// satisfy the whole interface, [`Callee::Iface`] for a type parameter or
/// a tuple, [`Callee::Dyn`] for `dyn`. `self_ty` is the type of the
/// argument in the first `Self` position ([`self_param`]).
///
/// # Errors
///
/// An [`Ice`] if `iface_fn` is not an interface function, or an interface
/// or type cannot be read.
pub(crate) fn iface_call(
    cx: &IfaceCx<'_>,
    iface_fn: DefId,
    self_ty: Ty,
) -> Result<Result<Callee, Unsatisfied>, Ice> {
    cx.iface_call(iface_fn, self_ty)
}

/// How the binary operator `op` is implemented on operands of type `ty`
/// (§5.4, §8.7): [`OpImpl::Builtin`] on the built-in numeric types, else a
/// call of the type's `add`, `sub`, `mul`, `div` or `compare`, found by
/// name and signature in its module (no dependency on `std::ops`, D-169),
/// through a bound for a type parameter, and by the tuple rule for
/// comparisons of tuples. Operators that cannot be overloaded (`==`,
/// `%`, the bitwise ones, …) are always [`OpImpl::Builtin`]: whether they
/// apply to `ty` is the caller's check (E0072).
///
/// # Errors
///
/// An [`Ice`] if an interface or type cannot be read.
pub(crate) fn operator(
    cx: &IfaceCx<'_>,
    op: BinOpKind,
    ty: Ty,
) -> Result<Result<OpImpl, Unsatisfied>, Ice> {
    match OpFn::of_binary(op) {
        Some(op_fn) => cx.op_impl(op_fn, ty),
        None => Ok(Ok(OpImpl::Builtin)),
    }
}

/// How unary `-` is implemented on `ty`: built in on the numeric types,
/// else the type's `neg`.
///
/// # Errors
///
/// An [`Ice`] if an interface or type cannot be read.
pub(crate) fn negation(cx: &IfaceCx<'_>, ty: Ty) -> Result<Result<OpImpl, Unsatisfied>, Ice> {
    cx.op_impl(OpFn::Neg, ty)
}

/// The `show` to call for `{x}` with `x: ty` (D-086): the type's `show`
/// for a nominal type (`String` included), through the bound for a type
/// parameter, by the built-in rule for a tuple, dynamically for
/// `dyn Show` or a `dyn` of an interface that embeds it.
///
/// # Errors
///
/// An [`Ice`] if an interface or type cannot be read.
pub(crate) fn show(cx: &IfaceCx<'_>, ty: Ty) -> Result<Result<Callee, Unsatisfied>, Ice> {
    cx.iface_call(cheby_iface::builtin::SHOW_FN, ty)
}

/// The dictionary that packages a value of type `from` as `dyn iface`
/// (§8.6): [`Dict::Dyn`] for a `dyn J` value where `J` is `iface` or
/// embeds it (not repackaged), else the dictionary of [`satisfy`]. A
/// `dyn J` value converts to `dyn` of an empty interface too, keeping its
/// descriptor (§8.6.1); its dictionary is an empty [`Dict::Impl`] for the
/// type `dyn J`. Every type parameter that `from` mentions needs a bound,
/// which supplies its descriptor (D-236): otherwise E0065.
///
/// # Errors
///
/// An [`Ice`] if an interface or type cannot be read.
pub(crate) fn dyn_coerce(
    cx: &IfaceCx<'_>,
    from: Ty,
    iface: DefId,
) -> Result<Result<Dict, Unsatisfied>, Ice> {
    cx.satisfy_ty(from, iface, 0)
}

/// `Self` in interface signatures.
const SELF: ParamIdx = ParamIdx::from_u32(0);

/// Whether `ty` is one of the built-in numeric types, whose operators are
/// built in (§5.4.2).
#[must_use]
pub(crate) fn is_builtin_numeric(ty: Ty) -> bool {
    [
        Ty::INT,
        Ty::FLOAT,
        Ty::I8,
        Ty::I16,
        Ty::I32,
        Ty::I64,
        Ty::U8,
        Ty::U16,
        Ty::U32,
        Ty::U64,
        Ty::F32,
    ]
    .contains(&ty)
}
