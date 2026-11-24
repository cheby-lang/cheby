//! The block cache (D-739, D-907): freed blocks of small objects kept per
//! thread for the next allocation of the same size class, laid out so that
//! generated code takes and gives back blocks without a call
//! ([`abi::block_cache`]).
//!
//! Each of the 32 classes has a stack of up to [`CACHE_BLOCKS`] blocks, a
//! word array whose first slot holds 0 and whose `top` points to the last
//! block kept. A pop reads the word at `top` and stops at the 0; a push
//! compares `top` with `limit`, the last slot. A class's array is
//! allocated the first time the class keeps a block or is refilled, and
//! until then `top` points to a static 0 and `limit` is null. Every field
//! is a [`Cell`]: the runtime's paths and generated code take and keep
//! blocks with a few loads and stores that call nothing, so no borrow is
//! needed, and a free that frees again inside a finalizer uses the same
//! cache.
//!
//! An empty class is refilled with [`REFILL_BLOCKS`] blocks from the
//! system at once, so that a program that allocates more than it frees
//! reaches the runtime once per batch, not once per block. A full class
//! gives the system as many blocks back at once before it keeps another,
//! for the same reason at the other end. Blocks in the cache are charged to
//! no domain: an allocation charges its block when it takes it, and a free
//! when it keeps it, as before.
//!
//! The cache is bypassed in poisoning mode, whose quarantine must keep
//! freed blocks out of use; under the address sanitizer its blocks are
//! poisoned while they are kept (D-880), and it is not refilled.

use std::alloc::Layout;
use std::cell::Cell;
use std::mem::offset_of;
use std::ptr;

use super::{Shard, keeps_freed_blocks, poison_enabled};
use crate::abi::{self, block_cache as cache_abi};
use crate::ctx::ExecCtx;
use crate::fiber::sanitizer;
use crate::object::to_usize;

/// The size classes of the block cache: allocations of 8 to 256 bytes, in
/// steps of 8 bytes.
pub(super) const CACHE_CLASSES: usize = cache_abi::CLASSES;

/// The most free blocks the cache keeps per size class and thread.
pub const CACHE_BLOCKS: usize = 512;

/// How many blocks an empty class takes from the system at once, and a
/// full one gives back at once.
pub(super) const REFILL_BLOCKS: usize = 32;

/// The word an absent stack's `top` points to: 0, so a pop finds no
/// block. Read, never written.
static NO_BLOCK: usize = 0;

/// The address of [`NO_BLOCK`] as a stack slot.
const fn no_block() -> *mut *mut u8 {
    (&raw const NO_BLOCK).cast_mut().cast()
}

/// One size class's stack (`abi::block_cache`).
#[repr(C)]
#[derive(Debug)]
struct ClassStack {
    /// The slot of the last block kept: the stack's first slot, which
    /// holds 0, when it is empty, or [`NO_BLOCK`] before it exists.
    top: Cell<*mut *mut u8>,
    /// The last slot of the stack, where an inline push stops; null while
    /// generated code is not to push: before the stack exists, in a
    /// runtime with debug assertions, and in the cache that never serves.
    limit: Cell<*mut *mut u8>,
}

impl ClassStack {
    /// An absent stack.
    const fn new() -> Self {
        Self {
            top: Cell::new(no_block()),
            limit: Cell::new(ptr::null_mut()),
        }
    }
}

/// Freed blocks of small objects kept per thread for the next allocation
/// of the same size (W5-D: freeing and allocating were most of 004's run
/// time). One per thread, in `tls.rs`; generated code reaches it through
/// the inline allocation record of the fiber current on the thread
/// (`abi::CTX_ALLOC_CACHE`).
#[repr(C)]
#[derive(Debug)]
pub(crate) struct BlockCache {
    /// At 0, generated code's part: each class's stack.
    classes: [ClassStack; CACHE_CLASSES],
    /// At [`cache_abi::SHARD_OFFSET`]: the shard of the current fiber's
    /// domain that it charges, exclusive to this thread, which inline
    /// allocations and frees charge ([`inline_record`]); null before a
    /// fiber is served.
    shard: Cell<*const Shard>,
    /// The first slot of each class's stack, null until it exists.
    stacks: [Cell<*mut *mut u8>; CACHE_CLASSES],
}

const _: () = assert!(offset_of!(BlockCache, classes) == 0);
const _: () = assert!(offset_of!(ClassStack, top) == 0 && cache_abi::TOP_OFFSET == 0);
const _: () = assert!(offset_of!(ClassStack, limit) == 8 && cache_abi::LIMIT_OFFSET == 8);
const _: () = assert!(size_of::<ClassStack>() == 16 && cache_abi::CLASS_SIZE == 16);
const _: () = assert!(offset_of!(BlockCache, shard) == 512 && cache_abi::SHARD_OFFSET == 512);
const _: () = assert!(offset_of!(Shard, objects) == 0 && cache_abi::SHARD_OBJECTS_OFFSET == 0);
const _: () = assert!(offset_of!(Shard, bytes) == 8 && cache_abi::SHARD_BYTES_OFFSET == 8);
const _: () = assert!(offset_of!(Shard, high) == 16 && cache_abi::SHARD_HIGH_OFFSET == 16);

/// The layout of a class's stack: the 0 slot, then [`CACHE_BLOCKS`] slots.
fn stack_layout() -> Option<Layout> {
    Layout::array::<*mut u8>(CACHE_BLOCKS.checked_add(1)?).ok()
}

/// The allocation layout of the blocks of size class `class`.
pub(super) fn class_layout(class: usize) -> Option<Layout> {
    let step = to_usize(abi::OBJECT_ALIGN);
    Layout::from_size_align(class.checked_add(1)?.checked_mul(step)?, step).ok()
}

impl BlockCache {
    /// An empty cache with no stacks.
    pub(crate) const fn new() -> Self {
        Self {
            classes: [const { ClassStack::new() }; CACHE_CLASSES],
            shard: Cell::new(ptr::null()),
            stacks: [const { Cell::new(ptr::null_mut()) }; CACHE_CLASSES],
        }
    }

    /// A cached free block of size class `class`, if there is one.
    /// Inlined into the fast paths (D-907).
    #[expect(
        clippy::inline_always,
        reason = "a step of every allocation by a fiber; the gain depends on it being inline (D-907)"
    )]
    #[inline(always)]
    pub(super) fn take(&self, class: usize) -> Option<*mut u8> {
        let stack = self.classes.get(class)?;
        let top = stack.top.get();
        // SAFETY: `top` is `NO_BLOCK` or a slot of this class's stack, a
        // readable word either way.
        let block = unsafe { top.read() };
        if block.is_null() {
            return None;
        }
        // A slot that holds a block is past the stack's first, whose 0
        // stops every pop.
        stack.top.set(top.wrapping_sub(1));
        // D-880: addressable again under the address cfg (`keep_sized`
        // poisoned it); a no-op otherwise.
        sanitizer::unpoison(block, super::class_bytes(class));
        Some(block)
    }

    /// Keeps a free block of `size` bytes, a nonzero multiple of the
    /// alignment, inlined into the fast free paths (D-907). A full class
    /// first gives [`REFILL_BLOCKS`] blocks back to the system. False when
    /// the cache has no class for the size or could not make the class's
    /// stack.
    #[expect(
        clippy::inline_always,
        reason = "a step of every free by a fiber; the gain depends on it being inline (D-907)"
    )]
    #[inline(always)]
    pub(super) fn keep_sized(&self, obj: *mut u8, size: usize) -> bool {
        let step = to_usize(abi::OBJECT_ALIGN);
        debug_assert!(
            size.is_multiple_of(step),
            "a block of {size} bytes kept in the block cache"
        );
        let Some(class) = size.checked_div(step).and_then(|n| n.checked_sub(1)) else {
            return false;
        };
        let Some(stack) = self.classes.get(class) else {
            return false;
        };
        let Some(first) = self.stack(class) else {
            return false;
        };
        let mut top = stack.top.get();
        if top == first.wrapping_add(CACHE_BLOCKS) {
            top = self.give_back(class, top);
        }
        // D-880: under the address cfg a use after free into a cached
        // block is reported; the quarantine is not poisoned, since the
        // runtime's own checks read its poisoned headers.
        sanitizer::poison(obj, size);
        let next = top.wrapping_add(1);
        // SAFETY: `top` is below the stack's last slot, so `next` is a
        // slot of the stack.
        unsafe { next.write(obj) };
        stack.top.set(next);
        true
    }

    /// Keeps a free block of `layout`; false when the cache has no class
    /// for it.
    pub(super) fn keep(&self, obj: *mut u8, layout: Layout) -> bool {
        super::size_class(layout).is_some() && self.keep_sized(obj, layout.size())
    }

    /// The first slot of class `class`'s stack, made the first time: a
    /// zeroed array, so its first slot holds 0. `None` when the system
    /// refuses it.
    #[cold]
    #[inline(never)]
    fn make_stack(&self, class: usize) -> Option<*mut *mut u8> {
        let stack = self.classes.get(class)?;
        let layout = stack_layout()?;
        #[expect(
            clippy::cast_ptr_alignment,
            reason = "the layout is an array of pointers, so the block is aligned for them"
        )]
        // SAFETY: a nonzero size.
        let first = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<*mut u8>();
        if first.is_null() {
            return None;
        }
        self.stacks.get(class)?.set(first);
        // The stack was absent, so it is empty.
        stack.top.set(first);
        let limit = if cfg!(debug_assertions) {
            // Each sized free reaches the runtime, which checks its size
            // (D-1772).
            ptr::null_mut()
        } else {
            first.wrapping_add(CACHE_BLOCKS)
        };
        stack.limit.set(limit);
        Some(first)
    }

    /// The first slot of class `class`'s stack, made if it is absent.
    #[expect(
        clippy::inline_always,
        reason = "a step of every free by a fiber; the gain depends on it being inline (D-907)"
    )]
    #[inline(always)]
    fn stack(&self, class: usize) -> Option<*mut *mut u8> {
        let first = self.stacks.get(class)?.get();
        if first.is_null() {
            return self.make_stack(class);
        }
        Some(first)
    }

    /// Gives the [`REFILL_BLOCKS`] blocks below `top`, the full stack of
    /// class `class`, back to the system, and returns the new top.
    #[cold]
    #[inline(never)]
    fn give_back(&self, class: usize, top: *mut *mut u8) -> *mut *mut u8 {
        let Some(layout) = class_layout(class) else {
            return top;
        };
        let mut top = top;
        for _ in 0..REFILL_BLOCKS {
            // SAFETY: a slot of the full stack above its first.
            let block = unsafe { top.read() };
            if block.is_null() {
                break;
            }
            top = top.wrapping_sub(1);
            sanitizer::unpoison(block, layout.size());
            // SAFETY: the cache holds only blocks allocated with their
            // class's layout that nothing references.
            unsafe { std::alloc::dealloc(block, layout) };
        }
        if let Some(stack) = self.classes.get(class) {
            stack.top.set(top);
        }
        top
    }

    /// A new block of class `class` from the system for an allocation that
    /// found the class empty, after which the class holds up to
    /// [`REFILL_BLOCKS`] - 1 more, so the next allocations of the class
    /// are served from the cache. `None` when the system refuses the first
    /// block; a refusal of a later one only ends the refill. Not in
    /// poisoning mode, under the address sanitizer or while a test keeps
    /// freed blocks out of the cache: one block then.
    #[inline(never)]
    pub(super) fn refill(&self, class: usize) -> Option<*mut u8> {
        let layout = class_layout(class)?;
        // SAFETY: a nonzero size.
        let block = unsafe { std::alloc::alloc(layout) };
        if block.is_null() {
            return None;
        }
        let refills =
            !cfg!(cheby_sanitize = "address") && !poison_enabled() && keeps_freed_blocks();
        if refills
            && let Some(first) = self.stack(class)
            && let Some(stack) = self.classes.get(class)
        {
            let last = first.wrapping_add(CACHE_BLOCKS);
            let mut top = stack.top.get();
            for _ in 1..REFILL_BLOCKS {
                if top == last {
                    break;
                }
                // SAFETY: a nonzero size.
                let more = unsafe { std::alloc::alloc(layout) };
                if more.is_null() {
                    break;
                }
                top = top.wrapping_add(1);
                // SAFETY: below the stack's last slot.
                unsafe { top.write(more) };
            }
            stack.top.set(top);
        }
        Some(block)
    }

    /// Returns every cached block to the system allocator; the stacks stay.
    pub(super) fn release(&self) {
        for class in 0..CACHE_CLASSES {
            let Some(layout) = class_layout(class) else {
                continue;
            };
            while let Some(block) = self.take(class) {
                // SAFETY: the cache holds only blocks allocated with their
                // class's layout that nothing references.
                unsafe { std::alloc::dealloc(block, layout) };
            }
        }
    }

    /// The number of cached blocks, for tests.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.classes
            .iter()
            .zip(&self.stacks)
            .map(|(stack, first)| {
                let first = first.get();
                if first.is_null() {
                    0
                } else {
                    // SAFETY: `top` is a slot of the stack that starts at
                    // `first`, at or above it.
                    let above = unsafe { stack.top.get().offset_from(first) };
                    usize::try_from(above).unwrap_or(0)
                }
            })
            .sum()
    }
}

impl Drop for BlockCache {
    fn drop(&mut self) {
        self.release();
        let Some(layout) = stack_layout() else {
            return;
        };
        for (stack, first) in self.classes.iter().zip(&self.stacks) {
            let first = first.replace(ptr::null_mut());
            stack.top.set(no_block());
            stack.limit.set(ptr::null_mut());
            if !first.is_null() {
                // SAFETY: allocated by `make_stack` with this layout.
                unsafe { std::alloc::dealloc(first.cast(), layout) };
            }
        }
    }
}

/// The cache that never serves: every class absent, with no shard, so an
/// inline pop finds no block and an inline push finds the class full. It
/// is never written: its pops find 0 before they store and its pushes
/// stop at the null limit, and the runtime's own paths never reach it.
struct NeverServing(BlockCache);

// SAFETY: never written (above), so threads only ever read it.
unsafe impl Sync for NeverServing {}

/// See [`NeverServing`].
static NEVER_SERVING: NeverServing = NeverServing(BlockCache::new());

/// The inline allocation record of a fiber that the inline path must not
/// serve (`abi::CTX_ALLOC_CACHE`).
#[must_use]
pub(crate) const fn never_serving_cache() -> *const BlockCache {
    &raw const NEVER_SERVING.0
}

/// The inline allocation record of `ctx`, which becomes current on the
/// thread whose block cache is `cache` (null when it stops being current
/// or the cache is gone): `cache` itself when generated code may take and
/// keep its blocks there, with the shard `ctx` charges recorded in it,
/// and [`never_serving_cache`] otherwise, so every allocation and sized
/// free then calls the runtime. The inline path needs a fiber without a
/// memory budget (D-617), events not counted (D-768), no poisoning and no
/// address sanitizer (D-880), and a shard the fiber's thread holds alone,
/// which takes plain stores (D-773). A test that keeps freed blocks out of
/// the cache turns it off too.
pub(crate) fn inline_record(ctx: &ExecCtx, cache: *const BlockCache) -> *const BlockCache {
    let slot = ctx.charge_slot();
    let serves = !cache.is_null()
        && !cfg!(cheby_sanitize = "address")
        && !poison_enabled()
        && ctx.budget() == u64::MAX
        && slot & 1 != 0
        && !crate::stats::counting()
        && keeps_freed_blocks();
    if !serves {
        return never_serving_cache();
    }
    // SAFETY: a non-null cache is the block cache of the thread `ctx`
    // becomes current on (`tls::set_current_fiber`), which lives as long
    // as the thread.
    let cache_ref = unsafe { &*cache };
    cache_ref
        .shard
        .set(ptr::with_exposed_provenance::<Shard>(slot & !1));
    cache
}
