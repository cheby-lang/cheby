//! Closures, local functions, function values, indirect calls, value
//! shims and trampolines (D4, D-453, D-499, D-526).
//!
//! Shapes (plan §4.1, §5.4):
//!
//! - A closure object is `Alloc Closure(n)`: the code word
//!   [`Const::Code`]`(FnKey::Closure { owner, index })`, then its
//!   captured locals as uniform words, in first-use order
//!   ([`free::free_locals`]), then the dictionaries of the enclosing
//!   item's bounds that its body uses, in (type parameter, bound) order
//!   (`BodyCx::dict_params_used`, D-453). Closures with no captures are
//!   allocated too.
//! - The code of a closure or local function is a function with the
//!   uniform signature (D-453): the owned `self`, then one uniform word per
//!   parameter, returning a uniform word. It reads each capture with
//!   `Field` of `self` (field `i + 1`; the RC pass dups it) and converts it
//!   from the uniform word, then binds each parameter pattern through
//!   `bind_param` (W2-B), then lowers the body in tail position (D-015).
//! - A local function binds its name to its closure. Inside its body the
//!   name is `self` ([`cheby_types::thir::ExprKind::SelfRef`]); a closure
//!   nested in it that names it captures that `self`, so the local
//!   function never captures itself and no cycle exists (D-453, D-499).
//! - A call of a function value is `Callee::Indirect` with uniform
//!   arguments and a `from_uniform` of the result, or a tail call in tail
//!   position.
//! - A top-level function as a value: with no dictionaries, the static
//!   closure [`Const::FnValue`] (`cb$fnval$…`, D-523), whose code is the
//!   function's [`FnKey::ValueShim`]; with `k` dictionaries, an
//!   `Alloc Closure(k)` of the shim's code word and the dictionaries. The
//!   shim ([`value_shim`]) reads the `k` dictionaries from `self`, unboxes
//!   the arguments to the declared representations and tail-calls the
//!   function through its slot (D-043). An external function as a value is
//!   the same shim over its wrapper (D-455).
//! - A trampoline ([`trampolines`]) is how the runtime calls a closure
//!   (D-526): C ABI, `ctx`, the closure, then `n` uniform words.

mod free;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, Ice, Ty};
use cheby_resolve::LocalId;
use cheby_types::thir::{Dict, ExprId, Param};

use crate::abi::abi_sig;
use crate::builder::FunctionBuilder;
use crate::ir::{Abi, Callee, Const, FnKey, Function, LayoutRef, Operand, Rvalue, Terminator};
use crate::lower::LowerCx;
use crate::lower::body::BodyCx;
use crate::repr::Repr;

pub(crate) use free::{free_locals, subtree};

impl BodyCx<'_, '_> {
    /// An anonymous function of type `ty`, also the result of desugaring
    /// `use`, captures and pipes (C7), and a constructor or interface
    /// function used as a value (D-500).
    pub(super) fn closure(
        &mut self,
        params: &[Param],
        body: ExprId,
        _ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        self.make_closure(None, params, body, span)
    }

    /// A local function statement (§5.8): binds `local` to its closure
    /// for the rest of the block. Its value is `Nil`.
    pub(super) fn local_fn(
        &mut self,
        local: LocalId,
        params: &[Param],
        body: ExprId,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let decl = self
            .body
            .local(local)
            .ok_or_else(|| Ice::new("a local function without its local"))?;
        let (name, ty) = (decl.name, decl.ty);
        let Some(closure) = self.make_closure(Some(local), params, body, span)? else {
            return Ok(None);
        };
        self.bind_name(local, name, ty, closure)?;
        Ok(Some(Operand::Const(Const::Nil)))
    }

    /// A local function's own name inside its body: its `self` (D-453,
    /// D-499), which the function's code binds to the local function's
    /// local, as a closure nested in it binds its capture.
    pub(super) fn self_ref(&self, local: LocalId) -> Result<Option<Operand>, Ice> {
        self.lookup(local).map(Some)
    }

    /// A top-level function as a value, with its dictionaries (D-453).
    pub(super) fn fn_ref(
        &mut self,
        def: DefId,
        dicts: &[Dict],
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let Some(dict_ops) = self.call_dicts(dicts, span)? else {
            return Ok(None);
        };
        self.fn_value(def, dict_ops).map(Some)
    }

    /// The value of the top-level function `def` given its lowered
    /// dictionary objects `dicts` (owned, in `def`'s bound order): the
    /// static [`Const::FnValue`] when there are none, else a closure of
    /// `def`'s value shim holding them. Also how a dictionary entry is
    /// built for a satisfying function (W3-A, D-453).
    pub(super) fn fn_value(&mut self, def: DefId, dicts: Vec<Operand>) -> Result<Operand, Ice> {
        self.pending.request_shim(def);
        if dicts.is_empty() {
            return Ok(Operand::Const(Const::FnValue(def)));
        }
        let layout = LayoutRef::Closure(count(dicts.len())?);
        let mut fields = Vec::with_capacity(dicts.len().saturating_add(1));
        fields.push(Operand::Const(Const::Code(FnKey::ValueShim(def))));
        fields.extend(dicts);
        let closure = self.define(Repr::Value, Rvalue::Alloc { layout, fields })?;
        Ok(Operand::Local(closure))
    }

    /// A call of the function value `callee` (evaluated before the
    /// arguments, §5.15.1) with `args`, through the uniform convention
    /// (D-453); a tail call in tail position (D-015).
    pub(super) fn call_value(
        &mut self,
        callee: ExprId,
        args: &[ExprId],
        ret_ty: Ty,
        _span: Span,
        tail: bool,
    ) -> Result<Option<Operand>, Ice> {
        let Some(f) = self.expr_as(callee, Repr::Value)? else {
            return Ok(None);
        };
        let mut ops = Vec::with_capacity(args.len());
        for &arg in args {
            let Some(op) = self.expr(arg, false)? else {
                return Ok(None);
            };
            let ty = self.ty(arg)?;
            ops.push(self.to_uniform(op, ty)?);
        }
        let f = match f {
            Operand::Local(l) => l,
            op @ Operand::Const(_) => self.define(Repr::Value, Rvalue::Use(op))?,
        };
        let callee = Callee::Indirect(f);
        if tail && self.b.abi() == Abi::Cheby {
            self.terminate(Terminator::TailCall { callee, args: ops })?;
            return Ok(None);
        }
        let result = self.define(Repr::Value, Rvalue::Call { callee, args: ops })?;
        self.from_uniform(Operand::Local(result), ret_ty).map(Some)
    }

    /// The closure object of a closure (`own` is `None`) or of the local
    /// function `own`, after lowering its code into a function of its own.
    fn make_closure(
        &mut self,
        own: Option<LocalId>,
        params: &[Param],
        body: ExprId,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let captures = free_locals(self.body, params, body, own)?;
        // The enclosing item's dictionaries the closure needs follow the
        // captured locals (D-453).
        let dicts = self.dict_params_used(body)?;
        let owner = owner_of(self.b.key())?;
        let key = self.pending.closure_key(owner)?;
        let layout = LayoutRef::Closure(count(captures.len().saturating_add(dicts.len()))?);
        let shape = Captures {
            layout,
            own,
            locals: &captures,
            dicts: &dicts,
        };
        let code = self.closure_function(key, &shape, params, body, span)?;
        self.pending.add_function(code);
        let mut fields =
            Vec::with_capacity(captures.len().saturating_add(dicts.len()).saturating_add(1));
        fields.push(Operand::Const(Const::Code(key)));
        for &l in &captures {
            let ty = self
                .body
                .local(l)
                .ok_or_else(|| Ice::new("a capture without its local"))?
                .ty;
            let value = self.lookup(l)?;
            fields.push(self.to_uniform(value, ty)?);
        }
        for &n in &dicts {
            fields.push(Operand::Local(self.dict_param_at(n)?));
        }
        let closure = self.define(Repr::Value, Rvalue::Alloc { layout, fields })?;
        Ok(Some(Operand::Local(closure)))
    }

    /// Lowers the code of a closure into a new function with key `key`.
    fn closure_function(
        &mut self,
        key: FnKey,
        shape: &Captures<'_>,
        params: &[Param],
        body: ExprId,
        span: Span,
    ) -> Result<Function, Ice> {
        let mut b = FunctionBuilder::new(key, Abi::Cheby, Repr::Value, span)?;
        b.set_entry_loc(&self.cx.location(span)?)?;
        let mut inner = BodyCx {
            cx: self.cx,
            body: self.body,
            generics: self.generics,
            b,
            locals: vec![None; self.body.locals.len()],
            pending: &mut *self.pending,
            dict_params: vec![None; self.dict_params.len()],
        };
        inner.closure_body(shape, params, body)?;
        inner.b.finish()
    }

    /// The body of a closure's code: captures from `self` (locals, then
    /// dictionaries), parameters, then the body in tail position,
    /// returning a uniform word.
    fn closure_body(
        &mut self,
        shape: &Captures<'_>,
        params: &[Param],
        body: ExprId,
    ) -> Result<(), Ice> {
        let Captures {
            layout,
            own,
            locals: captures,
            dicts,
        } = *shape;
        let this = self.b.add_param(Repr::Value, None, None)?;
        let mut args = Vec::with_capacity(params.len());
        for _ in params {
            args.push(self.b.add_param(Repr::Value, None, None)?);
        }
        for (k, &n) in dicts.iter().enumerate() {
            let field = count(captures.len().saturating_add(k).saturating_add(1))?;
            let dict = self.define(
                Repr::Value,
                Rvalue::Field {
                    base: this,
                    layout,
                    field,
                },
            )?;
            let slot = self
                .dict_params
                .get_mut(n)
                .ok_or_else(|| Ice::new("a captured dictionary outside the item's bounds"))?;
            *slot = Some(dict);
        }
        for (n, &l) in captures.iter().enumerate() {
            let decl = self
                .body
                .local(l)
                .ok_or_else(|| Ice::new("a capture without its local"))?;
            let (name, ty) = (decl.name, decl.ty);
            let field = count(n.saturating_add(1))?;
            let word = self.define(
                Repr::Value,
                Rvalue::Field {
                    base: this,
                    layout,
                    field,
                },
            )?;
            let value = self.from_uniform(Operand::Local(word), ty)?;
            self.bind_name(l, name, ty, value)?;
        }
        if let Some(own) = own {
            // SelfRef (D-453): the function's own closure.
            let decl = self
                .body
                .local(own)
                .ok_or_else(|| Ice::new("a local function without its local"))?;
            self.b.name_local(this, decl.name, decl.ty)?;
            self.bind(own, this)?;
        }
        for (param, arg) in params.iter().zip(args) {
            self.bind_param(&param.pat, Operand::Local(arg))?;
        }
        if let Some(value) = self.expr(body, true)? {
            let ty = self.ty(body)?;
            let value = self.to_uniform(value, ty)?;
            self.terminate(Terminator::Return(value))?;
        }
        Ok(())
    }
}

/// What a closure object holds besides its code word.
#[derive(Clone, Copy)]
struct Captures<'c> {
    /// `Closure(locals + dicts)`.
    layout: LayoutRef,
    /// The local function's own name, if it is one.
    own: Option<LocalId>,
    /// The captured locals, fields `1..=locals.len()`.
    locals: &'c [LocalId],
    /// The captured dictionaries of the enclosing item's bounds, by
    /// their index in [`BodyCx::dict_params`], in the fields after the
    /// locals.
    dicts: &'c [usize],
}

/// The item that owns the closures of a body with key `key`: the item,
/// constant or test whose body it is, or the owner of an enclosing
/// closure.
fn owner_of(key: FnKey) -> Result<DefId, Ice> {
    match key {
        FnKey::Item(d) | FnKey::ConstInit(d) | FnKey::Test(d) | FnKey::Closure { owner: d, .. } => {
            Ok(d)
        }
        other => Err(Ice::new(format!("a closure inside {other:?}"))),
    }
}

/// A count of fields or captures as a `u32`.
fn count(n: usize) -> Result<u32, Ice> {
    u32::try_from(n).map_err(|_| Ice::new("too many captures"))
}

/// The trampolines of `arities` (D-526, plan §4.3.4), for the entries
/// unit: key `FnKey::Trampoline(n)`, C ABI, `abi::trampoline_sig(n)` (the
/// closure, then `n` uniform words), with the body
/// `r = call indirect closure(a0, …); return r`. An ordinary call, not a
/// tail call: the trampoline is C ABI.
///
/// The C caller (`cheby_runtime::callback::call`) lends the closure and
/// gives the arguments. MIR parameters are owned, so code generation adds
/// the one reference the call consumes at the trampoline's entry; the MIR
/// here goes through the RC pass like any other.
///
/// # Errors
///
/// An [`Ice`] only if a function cannot be built.
pub(super) fn trampolines(arities: &BTreeSet<u32>) -> Result<Vec<Function>, Ice> {
    arities.iter().map(|&n| trampoline(n)).collect()
}

/// The trampoline of `arity` arguments.
pub(crate) fn trampoline(arity: u32) -> Result<Function, Ice> {
    // Trampolines have no source; nothing reads their span.
    let span = Span::new(FileId::new(0), TextRange::empty(0));
    let mut b = FunctionBuilder::new(FnKey::Trampoline(arity), Abi::C, Repr::Value, span)?;
    let closure = b.add_param(Repr::Value, None, None)?;
    let mut args = Vec::new();
    for _ in 0..arity {
        args.push(Operand::Local(b.add_param(Repr::Value, None, None)?));
    }
    let result = b.assign(
        Repr::Value,
        Rvalue::Call {
            callee: Callee::Indirect(closure),
            args,
        },
    )?;
    b.terminate(Terminator::Return(Operand::Local(result)))?;
    b.finish()
}

/// The `FnKey::ValueShim(def)` of the top-level function `def`: the
/// uniform-convention adapter that calls `def` through its slot (D-453).
/// It takes the owned `self` (the static `cb$fnval$…`, or a closure whose
/// fields are `def`'s dictionaries) and one uniform word per parameter,
/// converts each argument to its declared representation and tail-calls
/// `def` with the dictionaries first.
///
/// # Errors
///
/// An [`Ice`] if `def` is not a function readable through `cx`.
pub(super) fn value_shim(def: DefId, cx: &LowerCx<'_>) -> Result<Function, Ice> {
    let sig = cx.defs.fn_sig(def)?;
    let abi = abi_sig(sig, cx.defs.globals);
    let dicts = abi.params.len().saturating_sub(sig.params.len());
    let span = cx
        .defs
        .item(def)?
        .span
        .ok_or_else(|| Ice::new(format!("function {def:?} has no span")))?;
    let mut b = FunctionBuilder::new(FnKey::ValueShim(def), Abi::Cheby, Repr::Value, span)?;
    b.set_entry_loc(&cx.location(span)?)?;
    let this = b.add_param(Repr::Value, None, None)?;
    let mut words = Vec::with_capacity(sig.params.len());
    for p in &sig.params {
        words.push(b.add_param(Repr::Value, Some(p.name), Some(p.ty))?);
    }
    let layout = LayoutRef::Closure(count(dicts)?);
    let mut args = Vec::with_capacity(abi.params.len());
    for n in 0..dicts {
        let field = count(n.saturating_add(1))?;
        let d = b.assign(
            Repr::Value,
            Rvalue::Field {
                base: this,
                layout,
                field,
            },
        )?;
        args.push(Operand::Local(d));
    }
    let reprs = abi.params.get(dicts..).unwrap_or_default();
    for (&word, &repr) in words.iter().zip(reprs) {
        let arg = if repr == Repr::Value {
            Operand::Local(word)
        } else {
            Operand::Local(b.assign(
                repr,
                Rvalue::Unbox {
                    value: Operand::Local(word),
                    to: repr,
                },
            )?)
        };
        args.push(arg);
    }
    b.terminate(Terminator::TailCall {
        callee: Callee::Direct(def),
        args,
    })?;
    b.finish()
}
