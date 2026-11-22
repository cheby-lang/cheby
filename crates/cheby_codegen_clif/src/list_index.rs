//! `list::get` and `list::set` at an index of a list that is one leaf,
//! inline: the reads of `list_get` and the writes of `list_set` that need
//! no descent of the tree, with the helper's call for every other list.
//!
//! A list of up to 32 elements is one leaf that the root reaches directly
//! (`cheby_runtime::abi::list`): a root whose layout is not a big root's,
//! whose tree is a leaf, holds element `i` in slot `offset + i` of that
//! leaf. Big roots and deeper trees take the helper.
//!
//! - `list_get` (a list pattern's element, or a `list::get` taken apart
//!   at once, `cheby_mir::lookup`), whose index lowering checked, loads the
//!   slot and `dup`s the element, as the helper does. Whose list it is
//!   does not matter, so any count, a shared one or a static list's
//!   included, takes this path.
//! - `list_set` (a direct call of `list::set`) owns its list and its
//!   item. When the root and the leaf are both unique and the index is in
//!   range, it stores the item in the slot and releases the element that
//!   was there, which is what the helper's update in place does. It reads
//!   everything it tests before it writes anything, so a list it cannot
//!   serve reaches the helper unchanged; an index outside the list,
//!   negative included, is the helper's, which releases the item.
//!
//! The helper counts in-place updates for `CHEBY_RT_STATS`, so while the
//! counting flag is not off a write calls it (D-768). With `checked_rc`
//! neither path is emitted, as for the inline count operations (plan
//! §4.3.4).

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{BlockArg, InstBuilder, Value, types};

use cheby_mir::{Operand, RuntimeFn};
use cheby_runtime::abi;

use crate::CodegenError;
use crate::func::Translator;
use crate::runtime::RuntimeFunc;

/// A header word as a CLIF constant.
const fn imm(word: u64) -> i64 {
    word.cast_signed()
}

impl Translator<'_, '_> {
    /// `helper(args)` inline where this module has a fast path for it, or
    /// `None` when the helper is to be called.
    pub(crate) fn list_index_inline(
        &mut self,
        helper: RuntimeFn,
        args: &[Operand],
    ) -> Result<Option<Value>, CodegenError> {
        if self.opts.checked_rc {
            return Ok(None);
        }
        match (helper, args) {
            (RuntimeFn::ListGet, [list, index]) => self.list_get(list, index).map(Some),
            (RuntimeFn::ListSet, [list, index, item]) => self.list_set(list, index, item).map(Some),
            _ => Ok(None),
        }
    }

    /// The layout id of the object `word` points to.
    fn layout_is(&mut self, word: Value, id: u32) -> Value {
        let layout = self.layout_id(word);
        self.b.ins().icmp_imm_s(IntCC::Equal, layout, i64::from(id))
    }

    /// `list_get(list, index)`, `index` in range: element `index`, dup'd.
    fn list_get(&mut self, list: &Operand, index: &Operand) -> Result<Value, CodegenError> {
        let list = self.operand(list)?;
        let index = self.operand(index)?;
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let join = self.b.create_block();
        let result = self.b.append_block_param(join, types::I64);
        let root = self.layout_is(list, abi::list::ROOT_LAYOUT);
        self.unless(root, slow);
        let leaf = self.load_word(list, abi::list::ROOT_TREE_OFFSET);
        self.unless(leaf, slow);
        let is_leaf = self.layout_is(leaf, abi::list::LEAF_LAYOUT);
        self.unless(is_leaf, slow);
        let offset = self.load_word(list, abi::list::ROOT_OFFSET_OFFSET);
        let at = self.b.ins().iadd(offset, index);
        let place = self.slot(leaf, at);
        let elem = self.load_word(place, abi::list::LEAF_SLOTS_OFFSET);
        self.dup(elem)?;
        self.b.ins().jump(join, &[BlockArg::Value(elem)]);
        self.b.switch_to_block(slow);
        let called = self.runtime_value(RuntimeFunc::ListGet, &[list, index])?;
        self.b.ins().jump(join, &[BlockArg::Value(called)]);
        self.b.switch_to_block(join);
        Ok(result)
    }

    /// `list_set(list, index, item)`: the list with element `index`
    /// replaced, written in place when the root and its one leaf are
    /// unique and `index` is in range.
    fn list_set(
        &mut self,
        list: &Operand,
        index: &Operand,
        item: &Operand,
    ) -> Result<Value, CodegenError> {
        let list = self.operand(list)?;
        let index = self.operand(index)?;
        let item = self.operand(item)?;
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let join = self.b.create_block();
        let result = self.b.append_block_param(join, types::I64);
        self.unless_counting_off(slow)?;
        let header = self.load_word(list, abi::HEADER_RC_OFFSET);
        let unique_root =
            self.b
                .ins()
                .icmp_imm_s(IntCC::Equal, header, imm(abi::list::UNIQUE_ROOT));
        self.unless(unique_root, slow);
        // Unsigned: a negative index is above every length.
        let len = self.load_word(list, abi::list::ROOT_LEN_OFFSET);
        let in_range = self.b.ins().icmp(IntCC::UnsignedLessThan, index, len);
        self.unless(in_range, slow);
        let leaf = self.load_word(list, abi::list::ROOT_TREE_OFFSET);
        self.unless(leaf, slow);
        let leaf_header = self.load_word(leaf, abi::HEADER_RC_OFFSET);
        let unique_leaf =
            self.b
                .ins()
                .icmp_imm_s(IntCC::Equal, leaf_header, imm(abi::list::UNIQUE_LEAF));
        self.unless(unique_leaf, slow);
        let offset = self.load_word(list, abi::list::ROOT_OFFSET_OFFSET);
        let at = self.b.ins().iadd(offset, index);
        let place = self.slot(leaf, at);
        let old = self.load_word(place, abi::list::LEAF_SLOTS_OFFSET);
        self.store_word(item, place, abi::list::LEAF_SLOTS_OFFSET);
        self.drop_value(old)?;
        self.b.ins().jump(join, &[BlockArg::Value(list)]);
        self.b.switch_to_block(slow);
        let ctx = self.ctx()?;
        let called = self.edge_runtime_value(RuntimeFunc::ListSet, &[ctx, list, index, item])?;
        self.b.ins().jump(join, &[BlockArg::Value(called)]);
        self.b.switch_to_block(join);
        Ok(result)
    }
}
