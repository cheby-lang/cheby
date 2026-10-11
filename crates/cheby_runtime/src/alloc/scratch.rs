//! Scratch memory: the Rust-side buffers of the runtime's own walks, such
//! as the worklists of debug printing, equality and hashing, whose size
//! follows the data they walk.
//!
//! Such a buffer is not an object, so the domain does not count it, but
//! constant evaluation's memory limit must still see it: D-617 checks
//! every allocation, those of kernels included, before memory is
//! requested, so the peak never passes the limit. Every buffer here grows
//! through [`grow`], which
//!
//! - adds the new capacity to the bytes of scratch live on this thread and
//!   checks the sum, with the domain's live bytes, against the budget
//!   ([`crate::entry::check_budget`]) before it asks the system, and
//! - asks with `try_reserve_exact`, so a request the system refuses ends
//!   as [`super::out_of_memory`] says, never with an abort.
//!
//! The scratch of an entry lives on the entry's own thread, so the count
//! is per thread. A buffer gives its bytes back when it is dropped; an
//! abandoned entry parks its thread, so its count is never read again.
//!
//! Kernels whose temporary is the size of their result write the result
//! object straight away instead, and need no scratch.

use std::cell::Cell;
use std::collections::TryReserveError;
use std::fmt;
use std::ops::{Deref, DerefMut};

thread_local! {
    /// The bytes of scratch capacity live on this thread.
    static LIVE: Cell<u64> = const { Cell::new(0) };
    /// The most bytes of scratch live on this thread since
    /// [`reset_peak`], for tests that compare the memory a walk asks the
    /// system for with what it accounts for.
    static PEAK: Cell<u64> = const { Cell::new(0) };
}

/// The bytes of scratch capacity live on this thread, which the memory
/// budget adds to the domain's live bytes.
pub(crate) fn live() -> u64 {
    LIVE.try_with(Cell::get).unwrap_or(0)
}

/// The most bytes of scratch live on this thread since [`reset_peak`].
#[cfg(test)]
pub(crate) fn peak() -> u64 {
    PEAK.try_with(Cell::get).unwrap_or(0)
}

/// Restarts the peak at the scratch live now.
#[cfg(test)]
pub(crate) fn reset_peak() {
    PEAK.try_with(|peak| peak.set(live())).ok();
}

/// Moves this thread's count of live scratch from `old` to `new` bytes.
fn recharge(old: u64, new: u64) {
    LIVE.try_with(|live| {
        let now = live.get().saturating_sub(old).saturating_add(new);
        live.set(now);
        PEAK.try_with(|peak| peak.set(peak.get().max(now))).ok();
    })
    .ok();
}

/// Grows a buffer of `cap` items of `item` bytes, holding `len`, so it has
/// room for `extra` more: to twice its capacity, or more when `extra`
/// needs it. `charged` is the bytes the buffer is charged with, and
/// `reserve` asks the system for room for that many more items. A size
/// that overflows, a size over constant evaluation's memory budget and a
/// request the system refuses end as [`super::out_of_memory`] says.
fn grow(
    charged: &mut u64,
    (len, cap, item): (usize, usize, usize),
    extra: usize,
    reserve: impl FnOnce(usize) -> Result<(), TryReserveError>,
) {
    let Some(need) = len.checked_add(extra) else {
        super::out_of_memory(u64::MAX)
    };
    let new_cap = need.max(cap.saturating_mul(2)).max(MIN_CAP);
    let bytes = new_cap
        .checked_mul(item)
        .and_then(|bytes| u64::try_from(bytes).ok())
        .unwrap_or(u64::MAX);
    // The old buffer is still counted in the scratch live now, so a move
    // that holds both for a moment fits the budget too.
    crate::entry::check_budget(bytes);
    if reserve(new_cap.saturating_sub(len)).is_err() {
        super::out_of_memory(bytes);
    }
    recharge(*charged, bytes);
    *charged = bytes;
}

/// The smallest capacity a buffer grows to, in items.
const MIN_CAP: usize = 8;

/// A vector of scratch items. It derefs to a slice, which cannot grow;
/// it grows only through its own methods, which account for the growth.
pub(crate) struct ScratchVec<T> {
    items: Vec<T>,
    charged: u64,
}

impl<T> ScratchVec<T> {
    /// An empty vector, which holds no memory.
    pub(crate) const fn new() -> Self {
        Self {
            items: Vec::new(),
            charged: 0,
        }
    }

    /// A vector holding `item`.
    pub(crate) fn of(item: T) -> Self {
        let mut items = Self::new();
        items.push(item);
        items
    }

    /// Makes room for `extra` more items.
    pub(crate) fn reserve(&mut self, extra: usize) {
        if self.items.capacity().saturating_sub(self.items.len()) >= extra {
            return;
        }
        let shape = (
            self.items.len(),
            self.items.capacity(),
            size_of::<T>().max(1),
        );
        let items = &mut self.items;
        grow(&mut self.charged, shape, extra, |more| {
            items.try_reserve_exact(more)
        });
    }

    /// Appends `item`.
    pub(crate) fn push(&mut self, item: T) {
        self.reserve(1);
        self.items.push(item);
    }

    /// Removes and returns the last item.
    pub(crate) fn pop(&mut self) -> Option<T> {
        self.items.pop()
    }

    /// Appends every item of `items`, in order.
    pub(crate) fn extend(&mut self, items: impl IntoIterator<Item = T>) {
        for item in items {
            self.push(item);
        }
    }

    /// Moves every item of `other` to the end of this vector, last first,
    /// leaving `other` empty with its capacity: the order a worklist pops
    /// them in is then theirs.
    pub(crate) fn push_reversed(&mut self, other: &mut Self) {
        self.reserve(other.items.len());
        self.items.extend(other.items.drain(..).rev());
    }

    /// Removes every item, keeping the capacity.
    pub(crate) fn clear(&mut self) {
        self.items.clear();
    }

    /// Removes every item and returns them in order, keeping the capacity.
    pub(crate) fn drain(&mut self) -> std::vec::Drain<'_, T> {
        self.items.drain(..)
    }
}

impl<T> Default for ScratchVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for ScratchVec<T> {
    fn drop(&mut self) {
        recharge(self.charged, 0);
    }
}

impl<T> Deref for ScratchVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.items
    }
}

impl<T> DerefMut for ScratchVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.items
    }
}

impl<T: fmt::Debug> fmt::Debug for ScratchVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.items.iter()).finish()
    }
}

/// A string of scratch text. It derefs to `str`, which cannot grow; it
/// grows only through [`ScratchString::push_str`], which accounts for the
/// growth.
#[derive(Default)]
pub(crate) struct ScratchString {
    text: String,
    charged: u64,
}

impl ScratchString {
    /// Appends `text`.
    pub(crate) fn push_str(&mut self, text: &str) {
        if self.text.capacity().saturating_sub(self.text.len()) < text.len() {
            let shape = (self.text.len(), self.text.capacity(), 1);
            let buffer = &mut self.text;
            grow(&mut self.charged, shape, text.len(), |more| {
                buffer.try_reserve_exact(more)
            });
        }
        self.text.push_str(text);
    }

    /// Removes the text, keeping the capacity.
    pub(crate) fn clear(&mut self) {
        self.text.clear();
    }
}

impl Drop for ScratchString {
    fn drop(&mut self) {
        recharge(self.charged, 0);
    }
}

impl Deref for ScratchString {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl fmt::Debug for ScratchString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.text, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_counts_its_capacity_while_it_lives() {
        let before = live();
        let mut items = ScratchVec::new();
        items.extend(0_u64..100);
        let cap_bytes = u64::try_from(items.items.capacity() * 8).unwrap();
        assert_eq!(live() - before, cap_bytes);
        let mut text = ScratchString::default();
        text.push_str(&"x".repeat(1000));
        assert_eq!(text.get(..3), Some("xxx"));
        assert_eq!(
            live() - before,
            cap_bytes + u64::try_from(text.text.capacity()).unwrap()
        );
        drop(items);
        drop(text);
        assert_eq!(live(), before);
    }

    #[test]
    fn a_worklist_keeps_its_order() {
        let mut stack = ScratchVec::of(0);
        let mut later = ScratchVec::new();
        later.extend([1, 2, 3]);
        stack.push_reversed(&mut later);
        assert!(later.is_empty());
        let popped: Vec<i32> = std::iter::from_fn(|| stack.pop()).collect();
        assert_eq!(popped, [1, 2, 3, 0]);
    }
}
