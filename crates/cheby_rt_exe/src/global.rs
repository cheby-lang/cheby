//! The global allocator of `cheby` and of executables: mimalloc (D-890),
//! called through its C API. `cheby_rt_exe` declares it for the runtime
//! archive, and `cheby_cli` includes this file by path for the `cheby`
//! binary, so both use the same code.

use std::alloc::{GlobalAlloc, Layout};
use std::ffi::c_void;

use libmimalloc_sys::{
    mi_free, mi_malloc, mi_malloc_aligned, mi_realloc, mi_realloc_aligned, mi_zalloc,
    mi_zalloc_aligned,
};

/// The alignment of every block mimalloc returns: its block sizes are
/// multiples of a word, and its pages start at least word-aligned.
const WORD_ALIGN: usize = size_of::<usize>();

/// mimalloc as Rust's global allocator. A request aligned to at most a
/// word, which is every runtime object (`abi::OBJECT_ALIGN`), goes to
/// `mi_malloc`, `mi_zalloc` or `mi_realloc`, since every block is
/// word-aligned; only a larger alignment takes the `_aligned` calls. Why:
/// `mi_malloc_aligned`, which the `mimalloc` crate calls for every
/// request, has a fast path only up to 1 KiB. Above that, a size that is
/// not a power of two takes its general path and allocates `size +
/// align - 1` bytes, so a larger string, list leaf or buffer cost more
/// time and often a block of the next size class.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mimalloc;

// SAFETY: every method forwards to mimalloc, which returns blocks of at
// least the size asked for, aligned as `WORD_ALIGN` or the `_aligned`
// call's alignment says, or null, and frees or resizes only the blocks it
// returned, which `GlobalAlloc`'s callers guarantee.
unsafe impl GlobalAlloc for Mimalloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() <= WORD_ALIGN {
            // SAFETY: any size; the block is word-aligned.
            unsafe { mi_malloc(layout.size()) }.cast()
        } else {
            // SAFETY: the alignment is a power of two (`Layout`).
            unsafe { mi_malloc_aligned(layout.size(), layout.align()) }.cast()
        }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.align() <= WORD_ALIGN {
            // SAFETY: any size; the block is word-aligned.
            unsafe { mi_zalloc(layout.size()) }.cast()
        } else {
            // SAFETY: the alignment is a power of two (`Layout`).
            unsafe { mi_zalloc_aligned(layout.size(), layout.align()) }.cast()
        }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // SAFETY: the caller passes a block this allocator returned.
        unsafe { mi_free(ptr.cast::<c_void>()) };
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if layout.align() <= WORD_ALIGN {
            // SAFETY: the caller passes a block this allocator returned;
            // the new block is word-aligned.
            unsafe { mi_realloc(ptr.cast::<c_void>(), new_size) }.cast()
        } else {
            // SAFETY: the caller passes a block this allocator returned
            // with this alignment, a power of two.
            unsafe { mi_realloc_aligned(ptr.cast::<c_void>(), new_size, layout.align()) }.cast()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::{GlobalAlloc as _, Layout};

    use super::Mimalloc;

    /// Sizes around mimalloc's bins and its 1 KiB small limit, and some
    /// larger ones.
    const SIZES: [usize; 16] = [
        1,
        7,
        8,
        9,
        24,
        40,
        100,
        1000,
        1024,
        1025,
        1500,
        4096,
        5000,
        10_000,
        70_000,
        1 << 20,
    ];

    #[test]
    fn every_block_is_aligned_and_keeps_its_bytes() {
        for align in [1, 2, 4, 8, 16, 32, 64, 4096] {
            for size in SIZES {
                let layout = Layout::from_size_align(size, align).expect("a valid layout");
                // SAFETY: a nonzero size.
                let block = unsafe { Mimalloc.alloc(layout) };
                assert!(!block.is_null(), "{size} bytes aligned to {align} refused");
                assert!(
                    block.addr().is_multiple_of(align),
                    "{size} bytes misaligned to {align}"
                );
                // SAFETY: the block has `size` bytes.
                unsafe { block.write_bytes(0xA5, size) };
                let grown = size.saturating_mul(3);
                // SAFETY: allocated with `layout`; a nonzero size.
                let moved = unsafe { Mimalloc.realloc(block, layout, grown) };
                assert!(!moved.is_null(), "{size} bytes not grown to {grown}");
                assert!(
                    moved.addr().is_multiple_of(align),
                    "{grown} bytes misaligned to {align}"
                );
                // SAFETY: the first `size` bytes were copied.
                let kept = unsafe { std::slice::from_raw_parts(moved, size) };
                assert!(
                    kept.iter().all(|&byte| byte == 0xA5),
                    "{size} bytes not kept"
                );
                let grown_layout = Layout::from_size_align(grown, align).expect("a valid layout");
                // SAFETY: allocated with `grown_layout` by the realloc.
                unsafe { Mimalloc.dealloc(moved, grown_layout) };
            }
        }
    }

    #[test]
    fn zeroed_blocks_are_zero() {
        for align in [8, 16, 64] {
            for size in SIZES {
                let layout = Layout::from_size_align(size, align).expect("a valid layout");
                // SAFETY: a nonzero size.
                let block = unsafe { Mimalloc.alloc_zeroed(layout) };
                assert!(
                    !block.is_null(),
                    "{size} zeroed bytes aligned to {align} refused"
                );
                assert!(
                    block.addr().is_multiple_of(align),
                    "{size} bytes misaligned to {align}"
                );
                // SAFETY: the block has `size` initialized bytes.
                let bytes = unsafe { std::slice::from_raw_parts(block, size) };
                assert!(
                    bytes.iter().all(|&byte| byte == 0),
                    "{size} bytes not zeroed"
                );
                // SAFETY: allocated with `layout`.
                unsafe { Mimalloc.dealloc(block, layout) };
            }
        }
    }
}
