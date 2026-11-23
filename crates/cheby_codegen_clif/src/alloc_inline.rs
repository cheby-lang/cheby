//! Allocations of a cached size and sized frees, written inline
//! (`cheby_runtime::abi::block_cache`): the block cache of the thread the
//! fiber runs on, reached through the control block's inline allocation
//! record, serves them without a call, and everything else calls the
//! runtime.
//!
//! An allocation pops a block from its size class's stack, checks that the
//! domain's shard stays at or under its high (above it the runtime samples
//! the peak), charges the shard and writes the header. A sized free, the hit
//! path of a drop specialized by constructor (D-716, D-850), pushes the block
//! while the stack has room and charges the shard back. Each reads
//! everything it tests before it writes. The record is reloaded at every
//! use: a call between two uses may switch the fiber to another thread,
//! and the runtime rewrites the record at every switch (D-686, D-739).
//!
//! What the inline code does not serve goes to a slow path in a cold block:
//! a stub that saves every register (`cheby_runtime::alloc::slow`), called
//! with Cranelift's `preserve_all` convention, as the entry check's slow
//! paths are (D-1775), so a value live across the allocation stays in its
//! register. The record of a fiber the inline path must not serve, one with
//! a memory budget, while events are counted, under poisoning or the address
//! sanitizer, names a cache that never serves, so all its allocations and
//! sized frees reach the runtime as before (D-617, D-768, D-880). An
//! allocation's slow path leaves its result in the control block, since that
//! convention returns nothing, and the code tests it as it tested the
//! result of `cheby_rt_alloc` (D-733).
//!
//! With `checked_rc`, or on a target whose runtime has no saving stubs
//! (Windows until M6), every allocation and sized free is the helper's
//! call, as before.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{BlockArg, InstBuilder, MemFlagsData, Value, types};

use cheby_mir::Repr;
use cheby_runtime::abi::{self, block_cache};

use crate::CodegenError;
use crate::func::Translator;
use crate::runtime::RuntimeFunc;

/// Which allocation an inline allocation stands for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AllocKind {
    /// `cheby_rt_alloc`.
    Fresh,
    /// `cheby_rt_alloc_miss`: an `alloc_reuse` whose token is 0, whose
    /// miss the runtime counts while events are counted.
    ReuseMiss,
}

impl Translator<'_, '_> {
    /// Whether allocations and sized frees are written inline: not with
    /// `checked_rc`, and only where the runtime's slow paths are stubs
    /// that save every register.
    fn inline_blocks(&self) -> bool {
        !self.opts.checked_rc && self.decls.saving_slow_paths()
    }

    /// A new object of `size` bytes with `header` for the fiber `ctx`, as
    /// `cheby_rt_alloc` (or `cheby_rt_alloc_miss`) allocates it, with the
    /// statement's unwind edge: inline when the size has a class in the
    /// block cache, with the slow path's stub in a cold block, and the
    /// helper's call otherwise.
    pub(crate) fn alloc_object(
        &mut self,
        ctx: Value,
        size: i64,
        header: Value,
        kind: AllocKind,
    ) -> Result<Value, CodegenError> {
        let (helper, slow_path) = match kind {
            AllocKind::Fresh => (RuntimeFunc::Alloc, RuntimeFunc::AllocSlow),
            AllocKind::ReuseMiss => (RuntimeFunc::AllocMiss, RuntimeFunc::AllocMissSlow),
        };
        let class = u64::try_from(size).ok().and_then(block_cache::class_of);
        let (Some(class), true) = (class, self.inline_blocks()) else {
            let size = self.b.ins().iconst(types::I64, size);
            return self.edge_runtime_value(helper, &[ctx, size, header]);
        };
        let (top_at, _, bytes) = class_entry(class)?;
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let join = self.b.create_block();
        let object = self.b.append_block_param(join, types::I64);
        let cache = self.load_word(ctx, abi::CTX_ALLOC_CACHE);
        let top = self.load_word(cache, top_at);
        let block = self.load_word(top, 0);
        self.unless(block, slow);
        let shard = self.load_word(cache, block_cache::SHARD_OFFSET);
        let live = self.load_word(shard, block_cache::SHARD_BYTES_OFFSET);
        let high = self.load_word(shard, block_cache::SHARD_HIGH_OFFSET);
        let after = self.b.ins().iadd_imm_s(live, bytes);
        // Past the high the runtime samples the domain's peak.
        let under = self.b.ins().icmp(IntCC::SignedLessThanOrEqual, after, high);
        self.unless(under, slow);
        let objects = self.load_word(shard, block_cache::SHARD_OBJECTS_OFFSET);
        let objects = self.b.ins().iadd_imm_s(objects, 1);
        self.store_word(objects, shard, block_cache::SHARD_OBJECTS_OFFSET);
        self.store_word(after, shard, block_cache::SHARD_BYTES_OFFSET);
        let below = self.b.ins().iadd_imm_s(top, -8);
        self.store_word(below, cache, top_at);
        self.store_word(header, block, abi::HEADER_RC_OFFSET);
        self.b.ins().jump(join, &[BlockArg::Value(block)]);
        self.b.switch_to_block(slow);
        let size = self.b.ins().iconst(types::I64, size);
        self.runtime_call(slow_path, &[ctx, size, header])?;
        let fresh = self.load_word(ctx, abi::CTX_ALLOC_RESULT);
        match self.pad {
            // An object is never 0: 0 means the allocation raised.
            Some(pad) => {
                self.b
                    .ins()
                    .brif(fresh, join, &[BlockArg::Value(fresh)], pad, &[]);
            }
            None => {
                self.b.ins().jump(join, &[BlockArg::Value(fresh)]);
            }
        }
        self.b.switch_to_block(join);
        Ok(object)
    }

    /// `cheby_rt_free_unique(ctx, v, size)`: frees the block of `v`, a
    /// unique object of `size` bytes, rounded as allocation rounds it,
    /// whose value fields own nothing. Inline while its class's stack has
    /// room, with the slow path's stub in a cold block, and the helper's
    /// call when the size has no class.
    pub(crate) fn free_sized(
        &mut self,
        ctx: Value,
        v: Value,
        size: i64,
    ) -> Result<(), CodegenError> {
        let class = u64::try_from(size)
            .ok()
            .filter(|bytes| bytes.is_multiple_of(abi::OBJECT_ALIGN))
            .and_then(block_cache::class_of);
        let (Some(class), true) = (class, self.inline_blocks()) else {
            let size = self.b.ins().iconst(types::I64, size);
            self.runtime_call(RuntimeFunc::FreeUnique, &[ctx, v, size])?;
            return Ok(());
        };
        let (top_at, limit_at, bytes) = class_entry(class)?;
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let done = self.b.create_block();
        let cache = self.load_word(ctx, abi::CTX_ALLOC_CACHE);
        let top = self.load_word(cache, top_at);
        let limit = self.load_word(cache, limit_at);
        let room = self.b.ins().icmp(IntCC::UnsignedLessThan, top, limit);
        self.unless(room, slow);
        let shard = self.load_word(cache, block_cache::SHARD_OFFSET);
        let objects = self.load_word(shard, block_cache::SHARD_OBJECTS_OFFSET);
        let objects = self.b.ins().iadd_imm_s(objects, -1);
        self.store_word(objects, shard, block_cache::SHARD_OBJECTS_OFFSET);
        let live = self.load_word(shard, block_cache::SHARD_BYTES_OFFSET);
        let live = self.b.ins().iadd_imm_s(live, bytes.wrapping_neg());
        self.store_word(live, shard, block_cache::SHARD_BYTES_OFFSET);
        let above = self.b.ins().iadd_imm_s(top, 8);
        self.store_word(v, above, 0);
        self.store_word(above, cache, top_at);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(slow);
        let size = self.b.ins().iconst(types::I64, size);
        self.runtime_call(RuntimeFunc::FreeUniqueSlow, &[ctx, v, size])?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        Ok(())
    }

    /// A new `BoxedFloat` or `BoxedF32` holding `v`, a `Float` or an `F32`
    /// as `from` says: the 16-byte object of `cheby_rt_box_f64` and
    /// `cheby_rt_box_f32`, allocated as [`Self::alloc_object`] allocates,
    /// with the number's bits in its one field, an `F32`'s zero-extended.
    /// The helper's call where allocations are not inline.
    pub(crate) fn box_float(&mut self, v: Value, from: Repr) -> Result<Value, CodegenError> {
        let (helper, layout) = if from == Repr::Float {
            (RuntimeFunc::BoxF64, abi::layout::BOXED_FLOAT)
        } else {
            (RuntimeFunc::BoxF32, abi::layout::BOXED_F32)
        };
        let ctx = self.ctx()?;
        if !self.inline_blocks() {
            return self.edge_runtime_value(helper, &[ctx, v]);
        }
        let header = abi::header(abi::RC_UNIQUE, 0, layout);
        let header = self.b.ins().iconst(types::I64, header.cast_signed());
        let object = self.alloc_object(ctx, BOX_SIZE, header, AllocKind::Fresh)?;
        let bits = if from == Repr::Float {
            v
        } else {
            let bits = self.b.ins().bitcast(types::I32, MemFlagsData::new(), v);
            self.b.ins().uextend(types::I64, bits)
        };
        self.b
            .ins()
            .store(MemFlagsData::trusted(), bits, object, BOX_FIELD_OFFSET);
        Ok(object)
    }

    /// The cold drop of `v`, a shared object, on the paths of a reuse and of
    /// a drop specialized by constructor: through the stub that saves every
    /// register where allocations are inline, so the values live there stay
    /// in registers, and `cheby_rt_drop` otherwise.
    pub(crate) fn drop_shared(&mut self, v: Value) -> Result<(), CodegenError> {
        if !self.inline_blocks() {
            return self.drop_call(v);
        }
        let ctx = self.ctx()?;
        self.runtime_call(RuntimeFunc::DropSaving, &[ctx, v])?;
        Ok(())
    }
}

/// The size of a boxed number: its header and its one field.
const BOX_SIZE: i64 = 16;

/// The byte offset of a boxed number's field.
const BOX_FIELD_OFFSET: i32 = 8;

/// The byte offsets in the block cache of class `class`'s `top` and
/// `limit`, and the size of its blocks.
fn class_entry(class: usize) -> Result<(i32, i32, i64), CodegenError> {
    let ice = || CodegenError::ice(format!("the block cache has no class {class}"));
    let entry = block_cache::class_offset(class).ok_or_else(ice)?;
    let top = entry.checked_add(block_cache::TOP_OFFSET).ok_or_else(ice)?;
    let limit = entry
        .checked_add(block_cache::LIMIT_OFFSET)
        .ok_or_else(ice)?;
    let bytes = i64::try_from(block_cache::class_bytes(class)).map_err(|_| ice())?;
    Ok((top, limit, bytes))
}
