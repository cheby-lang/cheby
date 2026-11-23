//! The steps of a direct `list::sort_with` and `heap::to_list`, whose
//! comparison generated code calls.
//!
//! The kernels `cheby_list_sort_with` (D-1448) and `cheby_heap_sys_to_list`
//! (D-1613) call their comparison back through a trampoline from Rust
//! (D-526). The std intrinsics that replace them at a call
//! (`cheby_mir`'s `lower/intrinsic.rs`) run the same algorithm in steps
//! instead, and call the comparison themselves, as an ordinary call that
//! the release profile can clone and inline (D-1780):
//!
//! ```text
//! st = start(xs)                  // SortStart or HeapListStart
//! at = buffer_step(st, -1)
//! while at != 0 {
//!     order = by(pair_left(at), pair_right(at))
//!     at = buffer_step(st, tag(order))
//! }
//! result = buffer_finish(st, xs)
//! ```
//!
//! A step takes the comparison's answer, the tag of its `Order` (0
//! `Less`, 1 `Equal`, 2 `Greater`), and runs the algorithm up to its next
//! comparison: it returns the address of the two elements to compare, the
//! state's [`Pair`], or 0 once the algorithm is done. The elements and the
//! order of the comparisons are the kernels', so every comparison, even one
//! that is not a consistent order, gives the same result and sees the same
//! calls.
//!
//! The state is a `Bytes` object (layout 2) of length 0, with a header
//! after the text's fixed fields and the algorithm's buffers after it, so
//! generated code holds it as an ordinary value: when the comparison
//! panics or is cancelled, the cleanup releases it as it releases any
//! value, and its memory is counted as any object's (D-617). The buffers
//! hold the elements of `xs` borrowed: `xs` is an argument of the step
//! that finishes, so generated code holds it until then. A sort of fewer
//! than two elements has no object: its state is [`NOTHING`], and the
//! finish returns `xs` itself, as the kernel does.

use super::heap;
use super::node::{Root, drop_word, dup_word, register};
use super::ops;
use crate::abi::{self, Word};
use crate::alloc;
use crate::ctx::ExecCtx;
use crate::object::{ptr_to_word, word_to_ptr};
use crate::panic::fatal;
use crate::string;
use crate::unwind::guard::{DropOnUnwind, guarded};

/// The state of a sort of fewer than two elements: the tagged `Int` 0,
/// which is no object.
pub(super) const NOTHING: Word = match abi::tag_int(0) {
    Some(word) => word,
    None => panic!("0 is a tagged Int"),
};

/// The tag of `Greater`, the answer on which a merge takes the right
/// run's element (D-1448).
const GREATER: i64 = 2;

/// The first word of a state: what it is the state of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub(super) enum Kind {
    /// `list::sort_with`.
    Sort = 1,
    /// `heap::to_list` (`heap::steps`).
    HeapList = 2,
}

/// The two elements of a comparison, borrowed, at the address a step
/// returns: generated code reads the first at offset 0 and the second at
/// offset 8 and gives the comparison its own references.
pub(super) type Pair = [Word; 2];

/// The address of `pair`, which a step returns.
pub(super) fn address(pair: &Pair) -> Word {
    ptr_to_word(std::ptr::from_ref(pair).cast::<u8>())
}

/// A new state object whose header is a `T` followed by `words` words,
/// uninitialized but for the text fields: a `Bytes` object of length 0,
/// count one. Raises as an allocation does when it cannot be served.
///
/// # Safety
///
/// `ctx` is null or the running fiber's control block.
pub(super) unsafe fn new_state<T>(ctx: *mut ExecCtx, words: usize) -> Word {
    let bytes = words
        .checked_mul(size_of::<Word>())
        .and_then(|bytes| bytes.checked_add(size_of::<T>()))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .unwrap_or_else(|| alloc::out_of_memory(u64::MAX));
    // SAFETY: forwarded from the caller.
    ptr_to_word(unsafe { string::alloc_text_for(ctx, bytes, abi::layout::BYTES) })
}

/// The header of the state object `st`.
///
/// # Safety
///
/// `st` is a live state object made by [`new_state`] for a `T`, which
/// only the caller reads or writes while the reference lives.
pub(super) unsafe fn header<'a, T>(st: Word) -> &'a mut T {
    let offset = usize::try_from(abi::string::BYTES_OFFSET).unwrap_or(0);
    // SAFETY: forwarded from the caller: the header is at the text's first
    // byte, which is 8-aligned in an object.
    unsafe { &mut *word_to_ptr(st).add(offset).cast::<T>() }
}

/// The `words` words after the header of the state object `st`.
///
/// # Safety
///
/// As for [`header`], and the object holds that many words after it.
#[expect(
    clippy::cast_ptr_alignment,
    reason = "an object's text starts 8-aligned, and every header is a whole number of words"
)]
pub(super) unsafe fn buffers<'a, T>(st: Word, words: usize) -> &'a mut [Word] {
    let offset = usize::try_from(abi::string::BYTES_OFFSET).unwrap_or(0);
    // SAFETY: forwarded from the caller.
    unsafe {
        let start = word_to_ptr(st)
            .add(offset)
            .add(size_of::<T>())
            .cast::<Word>();
        std::slice::from_raw_parts_mut(start, words)
    }
}

/// The kind of the state object `st`.
///
/// # Safety
///
/// `st` is a live state object.
unsafe fn kind_of(st: Word) -> Kind {
    // SAFETY: forwarded from the caller: every header starts with its kind.
    match unsafe { *header::<u64>(st) } {
        1 => Kind::Sort,
        2 => Kind::HeapList,
        other => fatal(format_args!(
            "a comparison step met a state of kind {other}"
        )),
    }
}

/// The header of a sort's state: D-1448's merge sort, stopped at a
/// comparison. The two buffers of `n` words follow it; the elements are
/// merged from the one `flip` names into the other.
#[repr(C)]
struct Sort {
    kind: Kind,
    n: usize,
    /// The length of the runs being merged.
    width: usize,
    /// The merge of `from[i..mid]` and `from[j..end]` into `into[k..]`.
    mid: usize,
    end: usize,
    i: usize,
    j: usize,
    k: usize,
    /// Whether the elements are merged from the second buffer.
    flip: bool,
    /// Whether `pair` is waiting for its answer.
    pending: bool,
    /// Whether the elements are sorted, in the buffer `flip` names.
    done: bool,
    pair: Pair,
}

/// `SortStart`: the state of a direct `list::sort_with` of the borrowed
/// `xs`, or [`NOTHING`] for fewer than two elements. The first step
/// gives the first comparison.
///
/// # Safety
///
/// `ctx` is the running fiber's control block and `xs` a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_sort_start(ctx: *mut ExecCtx, xs: u64) -> u64 {
    guarded(Some(ctx), || {
        register();
        // SAFETY: forwarded from the caller.
        let list = unsafe { Root::from_word(xs) };
        let n = list.len();
        if n < 2 {
            return NOTHING;
        }
        // SAFETY: forwarded from the caller.
        let st = unsafe { new_state::<Sort>(ctx, n.saturating_mul(2)) };
        // SAFETY: a new state for a `Sort` with two buffers of `n`.
        let (head, buffers) =
            unsafe { (header::<Sort>(st), buffers::<Sort>(st, n.saturating_mul(2))) };
        *head = Sort {
            kind: Kind::Sort,
            n,
            width: 1,
            mid: 1,
            end: 2,
            i: 0,
            j: 1,
            k: 0,
            flip: false,
            pending: false,
            done: false,
            pair: [0, 0],
        };
        for (slot, item) in buffers.iter_mut().zip(ops::Iter::new(list)) {
            *slot = item;
        }
        st
    })
}

impl Sort {
    /// Takes the answer to the pending comparison, if any, and merges up
    /// to the next one: the address of the pair, or 0 when done. The
    /// merges are the kernel's: runs of `width` in adjacent pairs, front to
    /// back, a last run without a partner copied as it is, and the right
    /// run's front element taken only on `Greater`.
    fn step(&mut self, buffers: &mut [Word], tag: i64) -> Word {
        let n = self.n;
        if self.pending {
            self.pending = false;
            let take = if tag == GREATER {
                &mut self.j
            } else {
                &mut self.i
            };
            let from = *take;
            *take = from.saturating_add(1);
            self.copy(buffers, from);
        }
        loop {
            if self.done {
                return 0;
            }
            if self.i < self.mid && self.j < self.end {
                let base = self.source_base();
                self.pair = [
                    word_at(buffers, base.saturating_add(self.i)),
                    word_at(buffers, base.saturating_add(self.j)),
                ];
                self.pending = true;
                return address(&self.pair);
            }
            // One run is empty: the other's rest goes as it is.
            while self.i < self.mid {
                let from = self.i;
                self.i = from.saturating_add(1);
                self.copy(buffers, from);
            }
            while self.j < self.end {
                let from = self.j;
                self.j = from.saturating_add(1);
                self.copy(buffers, from);
            }
            let start = if self.end >= n {
                self.flip = !self.flip;
                self.width = self.width.saturating_mul(2);
                if self.width >= n {
                    self.done = true;
                    return 0;
                }
                0
            } else {
                self.end
            };
            self.mid = start.saturating_add(self.width).min(n);
            self.end = self.mid.saturating_add(self.width).min(n);
            self.i = start;
            self.j = self.mid;
            self.k = start;
        }
    }

    /// The index in the buffers of the first element merged from.
    fn source_base(&self) -> usize {
        if self.flip { self.n } else { 0 }
    }

    /// Copies element `from` of the buffer merged from to the next place of
    /// the one merged into.
    fn copy(&mut self, buffers: &mut [Word], from: usize) {
        let (from_base, into_base) = if self.flip { (self.n, 0) } else { (0, self.n) };
        let item = word_at(buffers, from_base.saturating_add(from));
        let Some(slot) = buffers.get_mut(into_base.saturating_add(self.k)) else {
            fatal(format_args!(
                "`list::sort_with` merged past its buffer at {}",
                self.k
            ));
        };
        *slot = item;
        self.k = self.k.saturating_add(1);
    }

    /// The sorted elements, once done.
    fn result<'a>(&self, buffers: &'a [Word]) -> &'a [Word] {
        let base = self.source_base();
        match buffers.get(base..base.saturating_add(self.n)) {
            Some(sorted) if self.done => sorted,
            _ => fatal(format_args!(
                "`list::sort_with` finished before its last merge"
            )),
        }
    }
}

/// Word `i` of a state's buffers.
pub(super) fn word_at(buffers: &[Word], i: usize) -> Word {
    buffers.get(i).copied().unwrap_or_else(|| {
        fatal(format_args!(
            "a comparison step read past its buffer at {i}"
        ))
    })
}

/// `BufferStep`: the next step of the state `st` of a `SortStart` or a
/// `HeapListStart`, given the tag of the answer to its pending comparison
/// (any value for the first step): the address of the next pair to
/// compare, or 0 when the algorithm is done (module documentation).
/// Allocates nothing and cannot raise.
///
/// # Safety
///
/// `st` is a live state or [`NOTHING`], which only the caller holds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_buffer_step(st: u64, tag: i64) -> u64 {
    if st == NOTHING {
        return 0;
    }
    // SAFETY: forwarded from the caller.
    match unsafe { kind_of(st) } {
        Kind::Sort => {
            // SAFETY: a sort's state with its two buffers.
            let head = unsafe { header::<Sort>(st) };
            // SAFETY: as above.
            let buffers = unsafe { buffers::<Sort>(st, head.n.saturating_mul(2)) };
            head.step(buffers, tag)
        }
        // SAFETY: forwarded from the caller.
        Kind::HeapList => unsafe { heap::list_step(st, tag) },
    }
}

/// `BufferFinish`: the result of the state `st`, which it consumes, even
/// when it raises: a new list of the sorted or popped elements, or for
/// [`NOTHING`] the borrowed `source` itself with a reference added.
///
/// # Safety
///
/// `ctx` is the running fiber's control block; `st` is a live state whose
/// last step returned 0, or [`NOTHING`], owned; `source` is the list its
/// start was given, live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_buffer_finish(ctx: *mut ExecCtx, st: u64, source: u64) -> u64 {
    if st == NOTHING {
        dup_word(source);
        return source;
    }
    guarded(Some(ctx), || {
        let owned = DropOnUnwind::new([st]);
        register();
        // SAFETY: forwarded from the caller.
        let result = match unsafe { kind_of(st) } {
            Kind::Sort => {
                // SAFETY: a sort's state with its two buffers.
                let head = unsafe { header::<Sort>(st) };
                // SAFETY: as above.
                let buffers = unsafe { buffers::<Sort>(st, head.n.saturating_mul(2)) };
                head.result(buffers)
            }
            // SAFETY: forwarded from the caller.
            Kind::HeapList => unsafe { heap::list_result(st) },
        };
        for &item in result {
            dup_word(item);
        }
        let list = super::from_iter(result.iter().copied());
        owned.disarm();
        drop_word(st);
        list
    })
}

#[cfg(test)]
#[path = "steps_tests.rs"]
mod tests;
