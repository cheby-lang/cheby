//! Large literals (D-720, D-780): tuples filled one field at a time, and
//! literals split into chains of continuations.
//!
//! - A tuple literal of more than [`TUPLE_INIT_THRESHOLD`] fields starts
//!   as one allocation whose fields all hold the static `Nil`
//!   ([`crate::ir::Rvalue::AllocPlaceholder`]); each field is then
//!   computed, boxed and stored ([`crate::ir::Rvalue::InitField`]) before
//!   the next one is computed, so only the tuple and the field being
//!   computed are live (D-810). A smaller tuple keeps its
//!   one `alloc` of all its fields, so ordinary code does not change.
//! - A list, interpolation or tuple literal (a map is built from a list
//!   literal of pairs) whose lowering has added [`SPLIT_SIZE`] statements
//!   and blocks ([`crate::builder::FunctionBuilder::size`]) to the
//!   function it is lowered in is cut before its next item: the rest goes
//!   into a private [`crate::ir::FnKey::Continuation`] of the body's owner (D-780,
//!   D-811). A continuation takes the value built so
//!   far (the list, the string buffer, the partly filled tuple) and then
//!   the locals and dictionaries the rest of the literal uses, in
//!   [`LocalId`] order and then bound order, each in its own
//!   representation, and lowers the next items. The function holding the
//!   literal calls the first continuation (an ordinary call: one extra
//!   frame, and no source tail call changes, D-015); each continuation
//!   tail-calls the next at its own cut, and the last returns the value.
//!   An item that diverges ends the chain there. The cuts and the
//!   numbering depend only on the lowered MIR (D-264).
//!
//! So no function a literal makes holds much more than [`SPLIT_SIZE`] of
//! its lowered statements, plus one item's: a literal of ordinary items
//! leaves every function within [`STATEMENT_BUDGET`] after counting and
//! unwind edges, which keeps the register allocator's superlinear cost
//! and D-679's frames small. Continuations are never borrowed (they are not
//! items, D-715): a borrowed parameter passed to one is an owned use, and
//! counting gives it a `dup`. Each continuation's entry pad releases its
//! parameters (D-698, D-777), so a panic in one unwinds its frame and the
//! call in the function holding the literal releases what that function
//! holds. Splitting long statement sequences in general waits for a later
//! package (D-780).

use std::collections::BTreeMap;

use cheby_diag::Span;
use cheby_iface::Ice;
use cheby_resolve::LocalId;
use cheby_types::thir::{ExprId, InterpPart, ListItem};

use crate::builder::FunctionBuilder;
use crate::ir::{Abi, Callee, Local, Operand, Rvalue, Terminator};
use crate::lower::body::BodyCx;
use crate::lower::closure::{free_locals, owner_of};
use crate::repr::Repr;

/// Tuple literals of more fields than this are filled one field at a
/// time (D-720, D-810); smaller ones are one `alloc`.
pub const TUPLE_INIT_THRESHOLD: usize = 32;

/// The statements and blocks of a literal's items that one function holds
/// before the literal is cut (D-780): the next item goes to a
/// continuation. Informative readings at 100k elements (a busy machine,
/// `bench-build --jobs 1`): 512 builds within about 5% of 256 and 5% to
/// 15% faster than 1024, with less memory than 256.
pub const SPLIT_SIZE: usize = 512;

/// The statement budget of D-780: with items of ordinary size, no function
/// that lowering a literal makes holds more statements than this after
/// counting and unwind edges, its pads' included. Four times
/// [`SPLIT_SIZE`]: a cut comes after the item that reaches it, counting
/// adds a `dup` or `drop` per use at most, and the unwind pass a pad
/// block per held set, which in an interpolation changes at every call
/// (about 2.7 times the lowered statements).
pub const STATEMENT_BUDGET: usize = 4 * SPLIT_SIZE;

/// What the items of a literal use from the function they are lowered
/// in, computed at its first cut: for each local and dictionary, the
/// last item that uses it.
struct Uses {
    /// The THIR locals bound outside the items.
    locals: BTreeMap<LocalId, usize>,
    /// The enclosing item's dictionary parameters, by index
    /// ([`BodyCx::dict_params`]).
    dicts: BTreeMap<usize, usize>,
}

/// The function that holds a split literal, set aside while its
/// continuations are lowered.
struct Owner {
    b: FunctionBuilder,
    locals: Vec<Option<Local>>,
    dict_params: Vec<Option<Local>>,
    /// The result of its call of the first continuation: the literal.
    result: Local,
}

/// The state of one literal while it is lowered: where its current piece
/// started and, once cut, the function that holds it.
pub(super) struct Chain {
    span: Span,
    /// [`FunctionBuilder::size`] when the current piece started.
    start: usize,
    uses: Option<Uses>,
    owner: Option<Owner>,
}

impl BodyCx<'_, '_> {
    /// Starts lowering a literal at `span`.
    pub(super) fn chain(&self, span: Span) -> Chain {
        Chain {
            span,
            start: self.b.size(),
            uses: None,
            owner: None,
        }
    }

    /// Whether the literal's current piece has reached [`SPLIT_SIZE`], so
    /// the next item starts a continuation.
    pub(super) fn split_due(&self, chain: &Chain) -> bool {
        self.b.size().saturating_sub(chain.start) >= self.pending.split_size()
            && !self.b.is_terminated()
    }

    /// Cuts the literal of `items` before item `next`: the current function
    /// passes `acc` (the value built so far) and what the items from `next`
    /// on use to a new continuation, which becomes the function being
    /// lowered. Returns the continuation's parameter that holds `acc`.
    pub(super) fn split_cut(
        &mut self,
        chain: &mut Chain,
        next: usize,
        acc: Operand,
        items: Items<'_>,
    ) -> Result<Local, Ice> {
        if chain.uses.is_none() {
            chain.uses = Some(self.item_uses(items)?);
        }
        let uses = chain
            .uses
            .as_ref()
            .ok_or_else(|| Ice::new("a cut literal without its uses"))?;
        let locals: Vec<LocalId> = uses
            .locals
            .iter()
            .filter(|&(_, &last)| last >= next)
            .map(|(&l, _)| l)
            .collect();
        let dicts: Vec<usize> = uses
            .dicts
            .iter()
            .filter(|&(_, &last)| last >= next)
            .map(|(&d, _)| d)
            .collect();
        // The arguments, and the parameters they become.
        let mut args =
            Vec::with_capacity(locals.len().saturating_add(dicts.len()).saturating_add(1));
        args.push(acc);
        let mut params = Vec::with_capacity(locals.len());
        for &l in &locals {
            let value = self.lookup(l)?;
            let repr = self
                .b
                .local(
                    value
                        .local()
                        .ok_or_else(|| Ice::new("a local bound to a constant"))?,
                )?
                .repr;
            let decl = self
                .body
                .local(l)
                .ok_or_else(|| Ice::new("a free local without its THIR local"))?;
            params.push((l, repr, decl.name, decl.ty));
            args.push(value);
        }
        for &d in &dicts {
            args.push(Operand::Local(self.dict_param_at(d)?));
        }
        let key = self.pending.continuation_key(owner_of(self.b.key())?)?;
        let mut b = FunctionBuilder::new(key, Abi::Cheby, Repr::Value, chain.span)?;
        b.set_entry_loc(&self.cx.location(chain.span)?)?;
        let fresh_locals = vec![None; self.body.locals.len()];
        let fresh_dicts = vec![None; self.dict_params.len()];
        let callee = Callee::Code(key);
        if chain.owner.is_none() {
            let result = self.define(Repr::Value, Rvalue::Call { callee, args })?;
            chain.owner = Some(Owner {
                b: std::mem::replace(&mut self.b, b),
                locals: std::mem::replace(&mut self.locals, fresh_locals),
                dict_params: std::mem::replace(&mut self.dict_params, fresh_dicts),
                result,
            });
        } else {
            self.terminate(Terminator::TailCall { callee, args })?;
            let done = std::mem::replace(&mut self.b, b);
            self.pending.add_function(done.finish()?);
            self.locals = fresh_locals;
            self.dict_params = fresh_dicts;
        }
        let acc = self.b.add_param(Repr::Value, None, None)?;
        for (l, repr, name, ty) in params {
            let param = self.b.add_param(repr, Some(name), Some(ty))?;
            self.bind(l, param)?;
        }
        for d in dicts {
            let param = self.b.add_param(Repr::Value, None, None)?;
            let slot = self
                .dict_params
                .get_mut(d)
                .ok_or_else(|| Ice::new("a dictionary outside the item's bounds"))?;
            *slot = Some(param);
        }
        chain.start = self.b.size();
        Ok(acc)
    }

    /// Ends the literal with `value`, or `None` when an item diverged.
    /// Without a cut, that is the result. After one, the last continuation
    /// returns `value`, the function holding the literal becomes the one
    /// being lowered again, and its call's result is the literal.
    pub(super) fn split_finish(
        &mut self,
        chain: Chain,
        value: Option<Operand>,
    ) -> Result<Option<Operand>, Ice> {
        let Some(owner) = chain.owner else {
            return Ok(value);
        };
        if let Some(value) = value {
            self.terminate(Terminator::Return(value))?;
        }
        let last = std::mem::replace(&mut self.b, owner.b);
        self.pending.add_function(last.finish()?);
        self.locals = owner.locals;
        self.dict_params = owner.dict_params;
        Ok(Some(Operand::Local(owner.result)))
    }

    /// For each local and dictionary the items use, the last item that
    /// uses it: what its expression uses (closures in it included), and
    /// for an interpolated value the dictionaries of its `show`.
    fn item_uses(&self, items: Items<'_>) -> Result<Uses, Ice> {
        let mut uses = Uses {
            locals: BTreeMap::new(),
            dicts: BTreeMap::new(),
        };
        let count = match items {
            Items::List(list) => list.len(),
            Items::Interp(parts) => parts.len(),
            Items::Tuple(elems) => elems.len(),
        };
        for n in 0..count {
            let (expr, show) = match items {
                Items::List(list) => match list.get(n) {
                    Some(ListItem::Elem(e) | ListItem::Spread(e)) => (Some(*e), None),
                    None => (None, None),
                },
                Items::Interp(parts) => match parts.get(n) {
                    Some(InterpPart::Value { expr, show, .. }) => (Some(*expr), show.as_ref()),
                    Some(InterpPart::Text(_)) | None => (None, None),
                },
                Items::Tuple(elems) => (elems.get(n).copied(), None),
            };
            if let Some(e) = expr {
                for l in free_locals(self.body, &[], e, None)? {
                    uses.locals.insert(l, n);
                }
                for d in self.dict_params_used(e)? {
                    uses.dicts.insert(d, n);
                }
            }
            if let Some(show) = show {
                for d in self.callee_dict_params(show)? {
                    uses.dicts.insert(d, n);
                }
            }
        }
        Ok(uses)
    }
}

/// The items of a literal being split.
#[derive(Clone, Copy)]
pub(super) enum Items<'a> {
    List(&'a [ListItem]),
    Interp(&'a [InterpPart]),
    Tuple(&'a [ExprId]),
}
