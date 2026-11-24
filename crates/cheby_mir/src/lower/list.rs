//! List literals and spreads (D2, D-157).
//!
//! `[]` is the runtime's static empty list. Otherwise the items are
//! evaluated left to right (§5.13) into one accumulator, and the result is
//! built from three helpers that take the accumulator owned and update it
//! in place when it is unique (F3): `list_push` adds an element (as a
//! uniform word) at the end, `list_prepend` adds one at the front, and
//! `list_append` adds a spread's elements at the end.
//!
//! - A leading spread is the accumulator itself, so `[..found, item]`
//!   pushes onto `found` without copying when `found` is not used again.
//! - Up to [`PREPEND_MAX`] elements before the first spread are held
//!   until that spread is evaluated and then prepended onto it, the last
//!   first: `[a, b, ..xs]` evaluates `a`, `b` and `xs`, then prepends
//!   `b` and `a` onto `xs`, in place when `xs` is not used again. More
//!   elements than that, or elements with no spread after them, are
//!   pushed onto the empty list as they come, so a literal holds at most
//!   [`PREPEND_MAX`] values besides its accumulator.
//! - Every other element is pushed and every other spread appended.
//!
//! So a literal of `n` elements makes one list, not `n`. A large literal
//! continues in continuations that take the accumulator (D-780,
//! `split.rs`); held elements are pushed onto the empty list first, so a
//! cut always passes one list.

use cheby_diag::Span;
use cheby_iface::{Ice, Ty};
use cheby_types::thir::ListItem;

use crate::ir::{Const, Operand, RuntimeFn, Rvalue};
use crate::lower::body::BodyCx;
use crate::lower::split::Items;
use crate::repr::Repr;

/// The most elements before a list literal's first spread that are held
/// and prepended onto it. Each one held is a value alive until the spread
/// is evaluated, with a release on every unwind edge before it; real code
/// conses one or two.
pub(crate) const PREPEND_MAX: usize = 8;

impl BodyCx<'_, '_> {
    /// A list literal of type `ty`, items in order.
    pub(super) fn list(
        &mut self,
        items: &[ListItem],
        _ty: Ty,
        span: Span,
    ) -> Result<Option<Operand>, Ice> {
        // The elements before the first spread, prepended onto it when
        // there are few enough.
        let lead = match items
            .iter()
            .position(|item| matches!(item, ListItem::Spread(_)))
        {
            Some(n) if n <= PREPEND_MAX => n,
            Some(_) | None => 0,
        };
        let mut held: Vec<Operand> = Vec::with_capacity(lead);
        let mut acc: Option<Operand> = None;
        // A large literal continues in continuations (D-780, `split.rs`).
        let mut chain = self.chain(span);
        for (k, item) in items.iter().enumerate() {
            if self.split_due(&chain) {
                // The held elements become the accumulator, so the cut
                // passes one list.
                if !held.is_empty() {
                    acc = Some(self.push_all(Operand::Const(Const::EmptyList), &mut held)?);
                }
                if let Some(list) = acc.take() {
                    let cont = self.split_cut(&mut chain, k, list, Items::List(items))?;
                    acc = Some(Operand::Local(cont));
                }
            }
            acc = match *item {
                ListItem::Elem(e) => {
                    let Some(elem) = self.expr_as(e, Repr::Value)? else {
                        return self.split_finish(chain, None);
                    };
                    match acc {
                        None if k < lead => {
                            held.push(elem);
                            None
                        }
                        None => Some(self.list_op(
                            RuntimeFn::ListPush,
                            Operand::Const(Const::EmptyList),
                            elem,
                        )?),
                        Some(list) => Some(self.list_op(RuntimeFn::ListPush, list, elem)?),
                    }
                }
                ListItem::Spread(e) => {
                    let Some(spread) = self.expr(e, false)? else {
                        return self.split_finish(chain, None);
                    };
                    Some(match acc {
                        // An owned operand: unique when it was the last
                        // reference, so the held elements and later items
                        // extend it in place.
                        None => {
                            let mut list = spread;
                            while let Some(elem) = held.pop() {
                                list = self.list_op(RuntimeFn::ListPrepend, list, elem)?;
                            }
                            list
                        }
                        Some(list) => self.list_op(RuntimeFn::ListAppend, list, spread)?,
                    })
                }
            };
        }
        // Only a literal that ends in its first spread holds elements,
        // and that spread took them.
        if !held.is_empty() {
            return Err(Ice::new(
                "list elements held past the literal's first spread",
            ));
        }
        let value = acc.unwrap_or(Operand::Const(Const::EmptyList));
        self.split_finish(chain, Some(value))
    }

    /// `list` with the `held` elements pushed in order, which leaves
    /// `held` empty.
    fn push_all(&mut self, list: Operand, held: &mut Vec<Operand>) -> Result<Operand, Ice> {
        held.drain(..).try_fold(list, |list, elem| {
            self.list_op(RuntimeFn::ListPush, list, elem)
        })
    }

    /// `func` (`list_push`, `list_prepend` or `list_append`) on the
    /// accumulator `list`.
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
