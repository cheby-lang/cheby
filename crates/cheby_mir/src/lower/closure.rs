//! Closures, local functions, function values, indirect calls, value
//! shims and trampolines (D4, D-453, D-499).
//!
//! W2-C fills this file (plan §4.2). Today every construct here is E0038
//! and the free functions are not implemented.

use std::collections::BTreeSet;

use cheby_diag::Span;
use cheby_iface::{DefId, Ice, Ty};
use cheby_resolve::LocalId;
use cheby_types::thir::{Dict, ExprId, Param, ParamOrigin};

use crate::ir::{Function, Operand};
use crate::lower::LowerCx;
use crate::lower::body::BodyCx;

impl BodyCx<'_, '_> {
    /// An anonymous function of type `ty` (also `use`, captures and pipes,
    /// C7).
    pub(super) fn closure(
        &mut self,
        params: &[Param],
        _body: ExprId,
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let what = match params.first().map(|p| p.origin) {
            Some(ParamOrigin::Use) => "`use` statements",
            Some(ParamOrigin::Capture) => "function captures",
            _ => "anonymous functions",
        };
        self.unsupported(span, what, ty)
    }

    /// A local function statement (§5.8): binds `local` for the rest of
    /// the block. Its value is `Nil`.
    pub(super) fn local_fn(
        &mut self,
        local: LocalId,
        _params: &[Param],
        _body: ExprId,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        // Its name is in scope after the statement.
        let fn_ty = self.body.local(local).map_or(Ty::ERROR, |l| l.ty);
        let placeholder = self.placeholder(self.repr_of(fn_ty))?;
        if let Operand::Local(l) = placeholder {
            self.bind(local, l)?;
        }
        self.unsupported(span, "local functions", Ty::NIL)
    }

    /// A local function's own name inside its body: its `self` (D-453,
    /// D-499).
    pub(super) fn self_ref(
        &mut self,
        _local: LocalId,
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "local functions", ty)
    }

    /// A top-level function as a value of type `ty`, with its
    /// dictionaries.
    pub(super) fn fn_ref(
        &mut self,
        _def: DefId,
        _ty_args: &[Ty],
        _dicts: &[Dict],
        ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "function values", ty)
    }

    /// A call of the function value `callee` (evaluated before the
    /// arguments, §5.15.1) with `args`, through the uniform convention
    /// (D-453).
    pub(super) fn call_value(
        &mut self,
        _callee: ExprId,
        _args: &[ExprId],
        ret_ty: Ty,
        span: Span,
        _tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        self.unsupported(span, "calls of function values", ret_ty)
    }
}

/// The trampolines of `arities`: C-ABI functions with key
/// `FnKey::Trampoline(n)` and `abi::trampoline_sig(n)` (plan §4.3.4),
/// for the entries unit.
///
/// # Errors
///
/// An [`Ice`] until W2-C implements it, when `arities` is not empty.
#[expect(
    dead_code,
    reason = "W2-A's entry thunks call it for `Pending::trampolines` (plan §4.2)"
)]
pub(super) fn trampolines(arities: &BTreeSet<u32>) -> Result<Vec<Function>, Ice> {
    if arities.is_empty() {
        Ok(Vec::new())
    } else {
        Err(Ice::new("not implemented yet: closure::trampolines"))
    }
}

/// The `FnKey::ValueShim(def)` of the top-level function `def`: the
/// uniform-convention adapter that calls `def` through its slot (D-453).
///
/// # Errors
///
/// An [`Ice`] until W2-C implements it.
pub(super) fn value_shim(def: DefId, _cx: &LowerCx<'_>) -> Result<Function, Ice> {
    Err(Ice::new(format!(
        "not implemented yet: closure::value_shim for {def:?}"
    )))
}
