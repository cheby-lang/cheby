//! List literals and spreads (D2, D-157).
//!
//! `[]` is the runtime's static empty list. Otherwise the items are
//! evaluated left to right (§5.13) into one accumulator: a leading spread
//! is the accumulator itself, else it starts as the empty list; each
//! element is pushed (`list_push`, as a uniform word) and each later
//! spread appended (`list_append`). Both helpers take the accumulator
//! owned and update it in place when it is unique (F3), so `[..found,
//! item]` pushes onto `found` without copying when `found` is not used
//! again, and a literal of `n` elements makes one list, not `n`.

use cheby_diag::Span;
use cheby_iface::{Ice, Ty};
use cheby_types::thir::ListItem;

use crate::ir::{Const, Operand, RuntimeFn, Rvalue};
use crate::lower::body::BodyCx;
use crate::repr::Repr;

impl BodyCx<'_, '_> {
    /// A list literal of type `ty`, items in order.
    pub(super) fn list(
        &mut self,
        items: &[ListItem],
        _ty: Ty,
        _span: Span,
    ) -> Result<Option<Operand>, Ice> {
        let mut acc: Option<Operand> = None;
        for item in items {
            acc = Some(match *item {
                ListItem::Elem(e) => {
                    let Some(elem) = self.expr_as(e, Repr::Value)? else {
                        return Ok(None);
                    };
                    let list = acc.unwrap_or(Operand::Const(Const::EmptyList));
                    self.list_op(RuntimeFn::ListPush, list, elem)?
                }
                ListItem::Spread(e) => {
                    let Some(spread) = self.expr(e, false)? else {
                        return Ok(None);
                    };
                    match acc {
                        // An owned operand: unique when it was the last
                        // reference, so later items extend it in place.
                        None => spread,
                        Some(list) => self.list_op(RuntimeFn::ListAppend, list, spread)?,
                    }
                }
            });
        }
        Ok(Some(acc.unwrap_or(Operand::Const(Const::EmptyList))))
    }

    /// `func` (`list_push` or `list_append`) on the accumulator `list`.
    fn list_op(&mut self, func: RuntimeFn, list: Operand, arg: Operand) -> Result<Operand, Ice> {
        let local = self.define(
            Repr::Value,
            Rvalue::CallRuntime {
                func,
                args: vec![list, arg],
            },
        )?;
        Ok(Operand::Local(local))
    }
}
