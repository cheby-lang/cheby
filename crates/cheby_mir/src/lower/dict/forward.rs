//! The forwarders of `dyn` dictionaries ([`FnKey::DynForward`], §8.6,
//! D-560): the code of each closure in the static dictionary through
//! which a `dyn J` value satisfies `J` or an interface `J` embeds, as a
//! bound's dictionary (D-502).
//!
//! A forwarder has the uniform signature (D-453): the closure `self`, the
//! `dyn J` receiver (`Self` is only ever the first parameter, D-117),
//! then the other arguments as uniform words. It reads the dictionary
//! and the value inside the receiver (D-524), loads the closure of the
//! interface function from that dictionary, and tail-calls it with the
//! value inside in place of the receiver. Where the function sits in a
//! dictionary of `J` depends on `J` (D-524 puts embedded interfaces'
//! functions first), so the key names `J` too (D-602).

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, Ice, ItemSig};

use super::IfaceShape;
use crate::builder::FunctionBuilder;
use crate::ir::{Abi, Callee, FnKey, Function, LayoutRef, Operand, Rvalue, Terminator};
use crate::lower::LowerCx;
use crate::repr::Repr;

/// The forwarder of `iface_fn` for receivers of type `dyn dyn_iface`.
///
/// # Errors
///
/// An [`Ice`] if `iface_fn` is not a function of `dyn_iface` (directly or
/// through embedding) readable through `cx`.
pub(in crate::lower) fn dyn_forward(
    iface_fn: DefId,
    dyn_iface: DefId,
    cx: &LowerCx<'_>,
) -> Result<Function, Ice> {
    let item = cx.defs.item(iface_fn)?;
    let ItemSig::InterfaceFn(sig) = &item.sig else {
        return Err(Ice::new(format!(
            "a `dyn` forwarder for {iface_fn:?}, which is not an interface function"
        )));
    };
    let params = sig.params.len();
    if params == 0 {
        return Err(Ice::new(format!(
            "a `dyn` forwarder for {iface_fn:?}, which has no `Self` parameter"
        )));
    }
    let shape = IfaceShape::of(cx, dyn_iface)?;
    let layout = shape.layout()?;
    let field = shape.fn_field(cx, iface_fn)?;
    // The builtin `Show::show` has no source; like its tuple code, its
    // forwarder reports a stack overflow at `<builtin>` (D-583).
    let (span, location) = match item.span {
        Some(span) => (span, cx.location(span)?),
        None => (
            Span::new(FileId::new(0), TextRange::empty(0)),
            "<builtin>".to_owned(),
        ),
    };
    let mut b = FunctionBuilder::new(
        FnKey::DynForward {
            iface_fn,
            dyn_iface,
        },
        Abi::Cheby,
        Repr::Value,
        span,
    )?;
    b.set_entry_loc(&location)?;
    // The forwarder's own closure (`self`) holds nothing it needs.
    let _this = b.add_param(Repr::Value, None, None)?;
    let receiver = b.add_param(Repr::Value, None, None)?;
    let mut rest = Vec::with_capacity(params.saturating_sub(1));
    for _ in 1..params {
        rest.push(b.add_param(Repr::Value, None, None)?);
    }
    let dict = b.assign(
        Repr::Value,
        Rvalue::Field {
            base: receiver,
            layout: LayoutRef::Dyn,
            field: 0,
        },
    )?;
    let closure = b.assign(
        Repr::Value,
        Rvalue::Field {
            base: dict,
            layout,
            field,
        },
    )?;
    let inner = b.assign(
        Repr::Value,
        Rvalue::Field {
            base: receiver,
            layout: LayoutRef::Dyn,
            field: 1,
        },
    )?;
    let mut args = Vec::with_capacity(params);
    args.push(Operand::Local(inner));
    args.extend(rest.into_iter().map(Operand::Local));
    b.terminate(Terminator::TailCall {
        callee: Callee::Indirect(closure),
        args,
    })?;
    b.finish()
}
