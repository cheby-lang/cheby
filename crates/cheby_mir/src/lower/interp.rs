//! Interpolated strings, with `show` (D-086) or debug text for `{x:?}`
//! (D2, C11, §5.3.2).
//!
//! The string is built in one buffer: `str_with_capacity` with the
//! length of the literal text plus a small allowance per value, then one
//! `str_append` per part, left to right. `str_append` takes the buffer
//! owned and appends in place while it is unique, which it always is
//! here, so building the string copies each piece once and never the
//! whole string again. `{x}` is the text of `x`'s `show` function (the
//! callee the checker resolved; a `String` is its own text), `{x:?}` the
//! runtime's debug text of `x` as a uniform word (§3.13). Values are
//! evaluated left to right (§5.3.2), each right before it is appended. A
//! large string continues in continuations that take the buffer (D-780,
//! `split.rs`).
//!
//! A literal of two or more parts whose first part is `{x}` without
//! `:?` of a `String` value (its `show` is `std::string::show`, D-447)
//! starts from that value instead (D-1055): the value, evaluated first
//! as before, is the buffer, with no `str_with_capacity`, and the other
//! parts are appended to it. The buffer is taken owned, as `+` takes its
//! left operand: reference counting moves it at its last use and
//! duplicates it otherwise, so `str_append` appends in place only when
//! nothing else refers to it (D-577) and copies it otherwise (D-849).
//! `"{acc}{piece}"` therefore costs what `acc + piece` costs. A
//! `String` parameter that leads such a literal is consumed, so it is
//! no longer borrowed (D-806). Every other literal (a leading text, a
//! leading `{x:?}`, a leading value of another type, a single part, an
//! empty literal) starts from a fresh buffer as above.

use cheby_diag::Span;
use cheby_iface::{Ice, Ty};
use cheby_types::thir::{Callee, ExprId, InterpPart};

use crate::abi::abi_sig;
use crate::ir::{Const, Operand, RuntimeFn, Rvalue};
use crate::lower::body::BodyCx;
use crate::lower::split::Items;
use crate::repr::Repr;

/// The bytes reserved for each interpolated value, beyond the literal
/// text: enough for most numbers and short names. A longer value makes
/// `str_append` grow the buffer, which doubles it.
const VALUE_ESTIMATE: usize = 16;

impl BodyCx<'_, '_> {
    /// An interpolated string of type `ty`.
    pub(super) fn interpolate(
        &mut self,
        parts: &[InterpPart],
        _ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        match parts {
            [] => return self.string_const("").map(Some),
            [InterpPart::Text(text)] => return self.string_const(text).map(Some),
            // The text of the one value is the whole string.
            [InterpPart::Value { expr, debug, show }] => {
                return self.interp_value(*expr, *debug, show.as_ref());
            }
            _ => {}
        }
        // A large string continues in continuations (D-780, `split.rs`).
        let mut chain = self.chain(span);
        // The buffer, and the index of the first part appended to it.
        let (mut acc, first) = match parts.first() {
            // A leading `String` value is the buffer (D-1055).
            Some(InterpPart::Value {
                expr,
                debug: false,
                show: Some(show),
            }) if self.is_string_value(*expr, show)? => {
                let Some(value) = self.interp_value(*expr, false, Some(show))? else {
                    return self.split_finish(chain, None);
                };
                (value, 1)
            }
            _ => {
                let mut capacity: usize = 0;
                for part in parts {
                    let len = match part {
                        InterpPart::Text(text) => text.len(),
                        InterpPart::Value { .. } => VALUE_ESTIMATE,
                    };
                    capacity = capacity.saturating_add(len);
                }
                let capacity = i64::try_from(capacity).map_err(|_| Ice::new("string too long"))?;
                let buffer = self.define(
                    Repr::Value,
                    Rvalue::CallRuntime {
                        func: RuntimeFn::StrWithCapacity,
                        args: vec![Operand::Const(Const::Int(capacity))],
                    },
                )?;
                (Operand::Local(buffer), 0)
            }
        };
        for (k, part) in parts.iter().enumerate().skip(first) {
            if self.split_due(&chain) {
                acc = Operand::Local(self.split_cut(&mut chain, k, acc, Items::Interp(parts))?);
            }
            let piece = match part {
                InterpPart::Text(text) => self.string_const(text)?,
                InterpPart::Value { expr, debug, show } => {
                    let Some(piece) = self.interp_value(*expr, *debug, show.as_ref())? else {
                        return self.split_finish(chain, None);
                    };
                    piece
                }
            };
            acc = Operand::Local(self.define(
                Repr::Value,
                Rvalue::CallRuntime {
                    func: RuntimeFn::StrAppend,
                    args: vec![acc, piece],
                },
            )?);
        }
        self.split_finish(chain, Some(acc))
    }

    /// Whether the interpolated value `expr` with `show` is a `String`
    /// that is its own text (`std::string::show`, D-447): the case in
    /// which a leading value is the buffer (D-1055), and in which
    /// [`Self::interp_value`] calls no `show`.
    fn is_string_value(&self, expr: ExprId, show: &Callee) -> Result<bool, Ice> {
        if self.expr_data(expr)?.ty != Ty::STRING {
            return Ok(false);
        }
        match show {
            Callee::Fn { def, .. } => self.is_string_show(*def),
            _ => Ok(false),
        }
    }

    /// The text of the value `expr`: its debug text for `{x:?}`, else the
    /// result of its type's `show` function `show`.
    fn interp_value(
        &mut self,
        expr: ExprId,
        debug: bool,
        show: Option<&Callee>,
    ) -> Result<Option<Operand>, Ice> {
        let e = self.expr_data(expr)?;
        let (ty, span) = (e.ty, e.span);
        if debug {
            let Some(value) = self.expr_as(expr, Repr::Value)? else {
                return Ok(None);
            };
            let text = self.define(
                Repr::Value,
                Rvalue::CallRuntime {
                    func: RuntimeFn::Debug,
                    args: vec![value],
                },
            )?;
            return Ok(Some(Operand::Local(text)));
        }
        match show {
            // `string::show` returns its argument (D-447): a `String` is
            // its own text, so no call is needed.
            Some(Callee::Fn { def, .. }) if ty == Ty::STRING && self.is_string_show(*def)? => {
                self.expr(expr, false)
            }
            Some(Callee::Fn { def, dicts, .. }) => {
                // Dictionaries first, as in any direct call (D-453).
                let mut dict_ops = Vec::with_capacity(dicts.len());
                for d in dicts {
                    let Some(op) = self.dict(d, span)? else {
                        return Ok(None);
                    };
                    dict_ops.push(op);
                }
                let sig = abi_sig(self.cx.defs.fn_sig(*def)?, self.cx.defs.globals);
                let repr = match sig.params.get(dicts.len()..) {
                    Some([repr]) => *repr,
                    _ => {
                        return Err(Ice::new(format!(
                            "a `show` function {def:?} without exactly one parameter"
                        )));
                    }
                };
                let Some(value) = self.expr_as(expr, repr)? else {
                    return Ok(None);
                };
                self.call_fn(*def, dict_ops, vec![value], Ty::STRING, false)
            }
            Some(Callee::Iface { iface_fn, dict }) => {
                self.call_iface(*iface_fn, dict, &[expr], Ty::STRING, span, false)
            }
            Some(Callee::Dyn { iface_fn }) => {
                self.call_dyn(*iface_fn, &[expr], Ty::STRING, span, false)
            }
            // THIR's `Callee` is `#[non_exhaustive]`: a kind added later is
            // an internal error until it is lowered (D-554).
            Some(_) => Err(Ice::new(format!(
                "an interpolated value's `show` is a kind of call MIR does not lower, at {span:?}"
            ))),
            None => Err(Ice::new(format!(
                "an interpolated value without its `show` function at {span:?}"
            ))),
        }
    }

    /// Whether `def` is `std::string::show`.
    fn is_string_show(&self, def: cheby_iface::DefId) -> Result<bool, Ice> {
        let name = self.cx.defs.name(def)?;
        Ok(&*name.module == "std::string" && &*name.item == "show")
    }
}
