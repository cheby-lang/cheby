//! The owned pattern steps `[x, ..rest]` and `[..rest, x]`
//! (`list_uncons_owned`) in place (D-1759): written inline for the lists the
//! runtime serves without a descent of the tree, with the helper's call
//! for every other list.
//!
//! The runtime keeps the elements at an end of a unique list in one leaf
//! that the root reaches directly (`cheby_runtime::abi::list`): the tree
//! itself when a root's list is one leaf, and a big root's head or tail
//! leaf. The inline code tests the root's whole header against the two
//! headers of a unique root, takes that leaf, tests that it is a leaf,
//! checks that the element is there and is not the leaf's last, and then
//! takes the element and writes the position and the length. From a
//! leaf whose count is one the element moves out and its slot is cleared
//! (or the leaf shortened, at the back). From a leaf that another list
//! holds, as when a function walks a list its caller still uses, the
//! element is dup'd and the leaf is not written: the root's view moves
//! past it. It reads everything it tests before it writes anything, so a
//! list it cannot serve reaches the helper unchanged. What it does is
//! what the helper's own fast path does
//! (`cheby_runtime::list::buffers`), without the call.
//!
//! When the head after the step reads the element as a raw number
//! (`list_uncons_head_float` and its `F32` form, made by `cheby_mir::rc`
//! of a head whose only use is an `unbox`), the inline step yields the
//! number: it reads it through a shared leaf's element with no count
//! touched, and drops an element it moved out of a unique leaf.
//!
//! `list_push` and `list_prepend` stay calls: written inline in the
//! bodies of `std::list`'s `push` and `prepend`, they measured no faster
//! than the helpers' fast paths.
//!
//! The helper counts its in-place updates for `CHEBY_RT_STATS`, so
//! while the counting flag is not off the inline code calls it (D-768),
//! and with `checked_rc` it is not emitted at all, as for the inline
//! count operations (plan §4.3.4).

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Block, BlockArg, InstBuilder, MemFlagsData, StackSlot, Value, types};

use cheby_mir::{Const, Operand, Repr, RuntimeFn};
use cheby_runtime::abi;

use crate::CodegenError;
use crate::func::Translator;
use crate::runtime::RuntimeFunc;
use crate::sig::repr_type;

/// A header or an immediate word as a CLIF constant.
const fn imm(word: u64) -> i64 {
    word.cast_signed()
}

/// The end of a list an operation works at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Front,
    Back,
}

/// The leaf at an end of a unique list, for the block that changes it.
struct EndLeaf {
    /// Whether the list is under a root, not a big root.
    small: Value,
    /// The address of the root's field that holds the leaf.
    field: Value,
    /// The leaf.
    leaf: Value,
    /// Whether the leaf's count is one: the element moves out of it.
    /// Otherwise another list holds the leaf, and the element is dup'd.
    unique: Value,
}

/// What both paths of an inline pattern step work with (`list_step`).
struct Step {
    /// The list, and the end of the spread its arguments give.
    list: Value,
    end: Value,
    /// The list's length, and whether it is above one.
    len: Value,
    several: Value,
    /// The leaf's meta word, and its length.
    meta: Value,
    slots: Value,
    /// The function's `list_uncons` slot.
    out: StackSlot,
    /// The helper's block, and the block after the step, which takes the
    /// rest and, with `scalar`, the number.
    slow: Block,
    join: Block,
    /// The representation of the number the head after the step reads,
    /// when it reads one.
    scalar: Option<Repr>,
}

impl Translator<'_, '_> {
    /// `helper(args)` inline where this module has a fast path for it, or
    /// `None` when the helper is to be called.
    pub(crate) fn list_end_inline(
        &mut self,
        helper: RuntimeFn,
        args: &[Operand],
    ) -> Result<Option<Value>, CodegenError> {
        if self.opts.checked_rc {
            return Ok(None);
        }
        match (helper, args) {
            // `[x, ..rest]`: element 0 and the elements from 1.
            (
                RuntimeFn::ListUnconsOwned,
                [
                    list,
                    Operand::Const(Const::Int(0)),
                    Operand::Const(Const::Int(1)),
                    end,
                ],
            ) => self.list_step(End::Front, list, args, end).map(Some),
            // `[..rest, x]`: the last element and the elements before it.
            (RuntimeFn::ListUnconsOwned, [list, at, Operand::Const(Const::Int(0)), end])
                if at == end =>
            {
                self.list_step(End::Back, list, args, end).map(Some)
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn load_word(&mut self, base: Value, offset: i32) -> Value {
        self.b
            .ins()
            .load(types::I64, MemFlagsData::trusted(), base, offset)
    }

    pub(crate) fn store_word(&mut self, value: Value, base: Value, offset: i32) {
        self.b
            .ins()
            .store(MemFlagsData::trusted(), value, base, offset);
    }

    /// Branches to `slow` unless `cond` holds, and continues in a new
    /// block.
    pub(crate) fn unless(&mut self, cond: Value, slow: Block) {
        let next = self.b.create_block();
        self.b.ins().brif(cond, next, &[], slow, &[]);
        self.b.switch_to_block(next);
    }

    /// The leaf at `end` of `list` when the list's count is one, with a
    /// branch to `slow` for any other list: a root's tree, when it is a
    /// leaf, or a big root's head or tail leaf. The leaf's own count may
    /// be any. Nothing is written.
    fn end_leaf(&mut self, list: Value, end: End, slow: Block) -> Result<EndLeaf, CodegenError> {
        // The helper counts the update while events are counted.
        self.unless_counting_off(slow)?;
        let header = self.load_word(list, abi::HEADER_RC_OFFSET);
        let small = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, header, imm(abi::list::UNIQUE_ROOT));
        let big = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, header, imm(abi::list::UNIQUE_BIG_ROOT));
        // A branch for each root, not a select: the leaf's address then
        // depends on no comparison, so the loads and stores through it
        // are not held up behind one.
        let in_big = match end {
            End::Front => abi::list::BIG_HEAD_OFFSET,
            End::Back => abi::list::BIG_TAIL_OFFSET,
        };
        let from_small = self.b.create_block();
        let other = self.b.create_block();
        let from_big = self.b.create_block();
        let found = self.b.create_block();
        let field = self.b.append_block_param(found, types::I64);
        self.b.ins().brif(small, from_small, &[], other, &[]);
        self.b.switch_to_block(from_small);
        let in_root = self
            .b
            .ins()
            .iadd_imm_s(list, i64::from(abi::list::ROOT_TREE_OFFSET));
        self.b.ins().jump(found, &[BlockArg::Value(in_root)]);
        self.b.switch_to_block(other);
        self.b.ins().brif(big, from_big, &[], slow, &[]);
        self.b.switch_to_block(from_big);
        let in_big_root = self.b.ins().iadd_imm_s(list, i64::from(in_big));
        self.b.ins().jump(found, &[BlockArg::Value(in_big_root)]);
        self.b.switch_to_block(found);
        let leaf = self.load_word(field, 0);
        self.unless(leaf, slow);
        let leaf_header = self.load_word(leaf, abi::HEADER_RC_OFFSET);
        let unique =
            self.b
                .ins()
                .icmp_imm_s(IntCC::Equal, leaf_header, imm(abi::list::UNIQUE_LEAF));
        // A root's tree may be a branch, whose words are not slots.
        let layout = self
            .b
            .ins()
            .ushr_imm_u(leaf_header, i64::from(abi::HEADER_LAYOUT_SHIFT));
        let is_leaf =
            self.b
                .ins()
                .icmp_imm_s(IntCC::Equal, layout, i64::from(abi::list::LEAF_LAYOUT));
        self.unless(is_leaf, slow);
        Ok(EndLeaf {
            small,
            field,
            leaf,
            unique,
        })
    }

    /// Branches to `slow` unless the runtime's event counting is off: a
    /// helper counts its in-place updates while it is on (D-768).
    pub(crate) fn unless_counting_off(&mut self, slow: Block) -> Result<(), CodegenError> {
        let flag_address = self.static_object(abi::sym::STATS_COUNTING)?;
        let flag = self
            .b
            .ins()
            .load(types::I8, MemFlagsData::trusted(), flag_address, 0);
        let off = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, flag, i64::from(abi::sym::COUNTING_OFF));
        self.unless(off, slow);
        Ok(())
    }

    /// A leaf's meta word and the length in its low half.
    fn leaf_meta(&mut self, leaf: Value) -> (Value, Value) {
        let meta = self.load_word(leaf, abi::list::LEAF_META_OFFSET);
        let len = self.b.ins().band_imm_u(meta, 0xffff_ffff);
        (meta, len)
    }

    /// The address of slot `index` of `leaf`, less the slots' offset.
    pub(crate) fn slot(&mut self, leaf: Value, index: Value) -> Value {
        let bytes = self.b.ins().ishl_imm_u(index, 3);
        self.b.ins().iadd(leaf, bytes)
    }

    /// The length of `list` plus `by`, stored.
    fn bump_len(&mut self, list: Value, len: Value, by: i64) {
        let next = self.b.ins().iadd_imm_s(len, by);
        self.store_word(next, list, abi::list::ROOT_LEN_OFFSET);
    }

    /// For the back of a list: whether the leaf's last slot holds the
    /// list's last element, which a root's view may end before.
    fn ends_with_leaf(&mut self, at: &EndLeaf, list: Value, len: Value, slots: Value) -> Value {
        let offset = self.load_word(list, abi::list::ROOT_OFFSET_OFFSET);
        let end = self.b.ins().iadd(offset, len);
        let want = self.b.ins().select(at.small, end, slots);
        self.b.ins().icmp(IntCC::Equal, want, slots)
    }

    /// `list_uncons_owned(list, at, start, end)` for the step that takes
    /// the first or the last element and leaves the rest, given what the
    /// arguments' shape shows of it; `end` is checked against the length
    /// here. The element goes into the function's `list_uncons` slot
    /// (D-909), and the list is the rest. It moves out of a unique leaf;
    /// from a leaf that another list holds it is dup'd, and the leaf is
    /// not written. The step that would use up the leaf goes to the
    /// helper, which releases the leaf and finds the next one.
    ///
    /// When the head after the step reads the element as a raw number
    /// (`list_uncons_head_float`, [`Translator::next_scalar_head`]), the
    /// step yields the number instead ([`Translator::uncons_scalar`]) and
    /// writes no slot on its inline paths. From a shared leaf it loads the
    /// element's word and then the number through it, and touches no
    /// count: the leaf holds the box, and the list, which this step owns,
    /// holds the leaf, and nothing runs between the two loads that could
    /// release either, no call and no yield point. From a unique leaf the
    /// element is moved out as before, so its reference is the step's: it
    /// loads the number and then drops the word, which frees a box that
    /// only the list held. The helper's path writes the slot as always,
    /// and the number is unboxed from the word and the word dropped there.
    fn list_step(
        &mut self,
        which: End,
        list: &Operand,
        args: &[Operand],
        end: &Operand,
    ) -> Result<Value, CodegenError> {
        let scalar = self.next_scalar_head;
        let list = self.operand(list)?;
        let end = self.operand(end)?;
        let out = self.make_uncons_slot();
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let join = self.b.create_block();
        let result = self.b.append_block_param(join, types::I64);
        let number = scalar.map(|repr| self.b.append_block_param(join, repr_type(repr)));
        let at = self.end_leaf(list, which, slow)?;
        let (meta, slots) = self.leaf_meta(at.leaf);
        let len = self.load_word(list, abi::list::ROOT_LEN_OFFSET);
        let several = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, len, 1);
        let st = Step {
            list,
            end,
            len,
            several,
            meta,
            slots,
            out,
            slow,
            join,
            scalar,
        };
        match which {
            End::Front => self.step_front(&at, &st)?,
            End::Back => self.step_back(&at, &st)?,
        }
        self.b.switch_to_block(slow);
        let mut vals = vec![self.ctx()?];
        vals.extend(self.operands(args)?);
        vals.push(self.b.ins().stack_addr(types::I64, out, 0));
        let rest = self.edge_runtime_value(RuntimeFunc::ListUnconsOwned, &vals)?;
        let mut join_args = vec![BlockArg::Value(rest)];
        if let Some(repr) = scalar {
            // The helper wrote the element, owned, to the slot.
            let word = self.uncons_word()?;
            let value = self.unbox_word(word, repr);
            self.drop_value(word)?;
            join_args.push(BlockArg::Value(value));
        }
        self.b.ins().jump(join, &join_args);
        self.b.switch_to_block(join);
        self.uncons_scalar = number;
        Ok(result)
    }

    /// Clears the slot at `place` that a step moved its element out of.
    fn clear_slot(&mut self, place: Value) {
        let cleared = self.b.ins().iconst(types::I64, imm(abi::list::CLEARED));
        self.store_word(cleared, place, abi::list::LEAF_SLOTS_OFFSET);
    }

    /// The inline step that takes the first element (`list_step`).
    fn step_front(&mut self, at: &EndLeaf, st: &Step) -> Result<(), CodegenError> {
        let first = self.load_word(at.field, 8);
        let next = self.b.ins().iadd_imm_s(first, 1);
        let more = self.b.ins().icmp(IntCC::UnsignedLessThan, next, st.slots);
        // The spread reaches the list's end.
        let whole = self
            .b
            .ins()
            .icmp(IntCC::SignedGreaterThanOrEqual, st.end, st.len);
        let fits = self.b.ins().band(st.several, more);
        let fits = self.b.ins().band(fits, whole);
        self.unless(fits, st.slow);
        let place = self.slot(at.leaf, first);
        let elem = self.load_word(place, abi::list::LEAF_SLOTS_OFFSET);
        let moved = self.b.create_block();
        let shared = self.b.create_block();
        match st.scalar {
            None => {
                let taken = self.b.create_block();
                self.b.ins().brif(at.unique, moved, &[], shared, &[]);
                self.b.switch_to_block(moved);
                self.clear_slot(place);
                self.b.ins().jump(taken, &[]);
                self.b.switch_to_block(shared);
                self.dup(elem)?;
                self.b.ins().jump(taken, &[]);
                self.b.switch_to_block(taken);
                self.store_word(next, at.field, 8);
                self.bump_len(st.list, st.len, -1);
                self.b.ins().stack_store(types::I64, elem, st.out, 0);
                self.b.ins().jump(st.join, &[BlockArg::Value(st.list)]);
            }
            Some(repr) => {
                // The number, right after its box's word (`list_step`).
                let value = self.unbox_word(elem, repr);
                self.b.ins().brif(at.unique, moved, &[], shared, &[]);
                self.b.switch_to_block(moved);
                self.clear_slot(place);
                self.store_word(next, at.field, 8);
                self.bump_len(st.list, st.len, -1);
                self.yield_number(Some(elem), value, st.list, st.join)?;
                self.b.switch_to_block(shared);
                self.store_word(next, at.field, 8);
                self.bump_len(st.list, st.len, -1);
                self.yield_number(None, value, st.list, st.join)?;
            }
        }
        Ok(())
    }

    /// The inline step that takes the last element (`list_step`).
    fn step_back(&mut self, at: &EndLeaf, st: &Step) -> Result<(), CodegenError> {
        // The element is the list's last, and the spread is
        // everything before it.
        let others = self.b.ins().iadd_imm_s(st.len, -1);
        let whole = self.b.ins().icmp(IntCC::Equal, st.end, others);
        let fits = self.b.ins().band(st.several, whole);
        let moved = self.b.create_block();
        let shared = self.b.create_block();
        // Where the word goes into the slot, when the step yields
        // no number.
        let taken = st.scalar.is_none().then(|| {
            let taken = self.b.create_block();
            let elem = self.b.append_block_param(taken, types::I64);
            (taken, elem)
        });
        self.b.ins().brif(at.unique, moved, &[], shared, &[]);
        self.b.switch_to_block(moved);
        let at_end = self.ends_with_leaf(at, st.list, st.len, st.slots);
        let more = self
            .b
            .ins()
            .icmp_imm_s(IntCC::UnsignedGreaterThan, st.slots, 1);
        let fits_here = self.b.ins().band(fits, more);
        let fits_here = self.b.ins().band(fits_here, at_end);
        self.unless(fits_here, st.slow);
        let index = self.b.ins().iadd_imm_s(st.slots, -1);
        let place = self.slot(at.leaf, index);
        let elem = self.load_word(place, abi::list::LEAF_SLOTS_OFFSET);
        let value = st.scalar.map(|repr| self.unbox_word(elem, repr));
        let shrunk = self.b.ins().iadd_imm_s(st.meta, -1);
        self.store_word(shrunk, at.leaf, abi::list::LEAF_META_OFFSET);
        self.back_tail(st, elem, true, value, taken)?;
        // A big root's tail leaf is always unique, so a shared
        // leaf is a root's tree: the element is at `offset +
        // len - 1`, and only the length changes.
        self.b.switch_to_block(shared);
        let fits_here = self.b.ins().band(fits, at.small);
        self.unless(fits_here, st.slow);
        let offset = self.load_word(st.list, abi::list::ROOT_OFFSET_OFFSET);
        let index = self.b.ins().iadd(offset, others);
        let place = self.slot(at.leaf, index);
        let elem = self.load_word(place, abi::list::LEAF_SLOTS_OFFSET);
        // The number, right after its box's word (`list_step`).
        let value = st.scalar.map(|repr| self.unbox_word(elem, repr));
        if value.is_none() {
            self.dup(elem)?;
        }
        self.back_tail(st, elem, false, value, taken)?;
        if let Some((taken, taken_elem)) = taken {
            self.b.switch_to_block(taken);
            self.bump_len(st.list, st.len, -1);
            self.b.ins().stack_store(types::I64, taken_elem, st.out, 0);
            self.b.ins().jump(st.join, &[BlockArg::Value(st.list)]);
        }
        Ok(())
    }

    /// The end of a path of the step at the back, from a unique leaf
    /// (`moved`) or a shared one: a jump to `taken` with the element's
    /// word, or, when the step yields the element's number `value`, the
    /// length written and the number yielded.
    fn back_tail(
        &mut self,
        st: &Step,
        elem: Value,
        moved: bool,
        value: Option<Value>,
        taken: Option<(Block, Value)>,
    ) -> Result<(), CodegenError> {
        match (value, taken) {
            (Some(value), _) => {
                self.bump_len(st.list, st.len, -1);
                self.yield_number(moved.then_some(elem), value, st.list, st.join)
            }
            (None, Some((taken, _))) => {
                self.b.ins().jump(taken, &[BlockArg::Value(elem)]);
                Ok(())
            }
            (None, None) => Err(CodegenError::ice(
                "a pattern step with neither a slot nor a number",
            )),
        }
    }

    /// The end of an inline step that yields the element's number
    /// `value`, once the root says the element is gone: a jump to `join`
    /// with the list and the number, after a drop of the element's word
    /// when it was `moved` out of its leaf, whose reference is then the
    /// step's.
    fn yield_number(
        &mut self,
        moved: Option<Value>,
        value: Value,
        list: Value,
        join: Block,
    ) -> Result<(), CodegenError> {
        if let Some(elem) = moved {
            self.drop_value(elem)?;
        }
        self.b
            .ins()
            .jump(join, &[BlockArg::Value(list), BlockArg::Value(value)]);
        Ok(())
    }
}
