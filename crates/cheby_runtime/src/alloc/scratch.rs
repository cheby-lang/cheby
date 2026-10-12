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
//! - adds the new capacity to the bytes of scratch live in the running
//!   fiber and checks the sum, with the domain's live bytes, against the
//!   budget ([`crate::entry::check_budget`]) before it asks the system,
//!   and
//! - asks with `try_reserve_exact`, so a request the system refuses ends
//!   as [`super::out_of_memory`] says, never with an abort.
//!
//! The count is the running fiber's, kept in its control block (D-685); on
//! a thread that runs no fiber it is the thread's, kept in `tls.rs`'s host
//! slot. A buffer gives its bytes back when it is dropped; an abandoned
//! entry never runs again, so its count is never read again.
//!
//! Kernels whose temporary is the size of their result write the result
//! object straight away instead, and need no scratch.

use std::collections::TryReserveError;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::Ordering;

use crate::ctx::ExecCtx;
use crate::tls;

/// The bytes of scratch capacity live in the running fiber, or on this
/// thread when it runs none. The memory budget adds the fiber's to the
/// domain's live bytes ([`crate::entry::check_budget`]).
#[cfg(test)]
pub(crate) fn live() -> u64 {
    match super::current_fiber() {
        Some(ctx) => ctx.scratch().0.load(Ordering::Relaxed),
        None => tls::with_host(|host| host.scratch_live.get()).unwrap_or(0),
    }
}

/// The most bytes of scratch live since [`reset_peak`], for tests that
/// compare the memory a walk asks the system for with what it accounts
/// for.
#[cfg(test)]
pub(crate) fn peak() -> u64 {
    match super::current_fiber() {
        Some(ctx) => ctx.scratch().1.load(Ordering::Relaxed),
        None => tls::with_host(|host| host.scratch_peak.get()).unwrap_or(0),
    }
}

/// Restarts the peak at the scratch live now.
#[cfg(test)]
pub(crate) fn reset_peak() {
    let now = live();
    match super::current_fiber() {
        Some(ctx) => ctx.scratch().1.store(now, Ordering::Relaxed),
        None => {
            tls::with_host(|host| host.scratch_peak.set(now));
        }
    }
}

/// Whose scratch count a buffer charges: found once, when the buffer first
/// grows, so a buffer reads `tls.rs` once in its life rather than at every
/// growth and again when it is dropped (GATE1-1: each accessor is a call
/// that is never inlined, D-686).
#[derive(Clone, Copy, Debug)]
enum Owner {
    /// The buffer has not grown yet and charges nothing.
    None,
    /// The control block of the fiber the buffer was grown on. A buffer
    /// lives in that fiber's frames, which run only on that fiber, so the
    /// block stays the right one, and live, even when the fiber moves to
    /// another thread: it is the fiber's own, not the thread's.
    /// GAP(fcb-pointer-across-switch): it is read from `tls.rs` once and
    /// may be kept across a switch (a debug hook that parks); D-686's rule
    /// is read as covering thread state, not a fiber's own control block.
    Fiber(*const ExecCtx),
    /// A thread that runs no fiber: its host slot.
    Host,
}

/// The bytes a buffer is charged with, and to whom. Dropping it gives the
/// bytes back.
#[derive(Debug)]
struct Charge {
    bytes: u64,
    owner: Owner,
}

impl Charge {
    /// Nothing charged yet.
    const fn new() -> Self {
        Self {
            bytes: 0,
            owner: Owner::None,
        }
    }

    /// The fiber the buffer charges, `None` on a host thread; found on the
    /// first call.
    fn fiber<'a>(&mut self) -> Option<&'a ExecCtx> {
        let ctx: Option<&'a ExecCtx> = match self.owner {
            Owner::None => {
                let ctx = super::current_fiber();
                self.owner = ctx.map_or(Owner::Host, |ctx| Owner::Fiber(ctx));
                return ctx;
            }
            // SAFETY: the control block of the fiber this buffer was grown
            // on, which runs the code that owns the buffer (see `Owner`).
            Owner::Fiber(ctx) => Some(unsafe { &*ctx }),
            Owner::Host => None,
        };
        ctx
    }

    /// Moves the owner's count of live scratch from the bytes charged now
    /// to `new` bytes.
    fn recharge(&mut self, new: u64) {
        let old = self.bytes;
        let next = |live: u64| live.saturating_sub(old).saturating_add(new);
        match self.fiber() {
            Some(ctx) => {
                // Only the owning fiber writes its counts.
                let (live, peak) = ctx.scratch();
                let now = next(live.load(Ordering::Relaxed));
                live.store(now, Ordering::Relaxed);
                peak.fetch_max(now, Ordering::Relaxed);
            }
            None => {
                tls::with_host(|host| {
                    let now = next(host.scratch_live.get());
                    host.scratch_live.set(now);
                    host.scratch_peak.set(host.scratch_peak.get().max(now));
                });
            }
        }
        self.bytes = new;
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        if self.bytes != 0 {
            self.recharge(0);
        }
    }
}

/// Grows a buffer of `cap` items of `item` bytes, holding `len`, so it has
/// room for `extra` more: to twice its capacity, or more when `extra`
/// needs it. `charge` is what the buffer is charged with, and `reserve`
/// asks the system for room for that many more items. A size that
/// overflows, a size over constant evaluation's memory budget and a
/// request the system refuses end as [`super::out_of_memory`] says.
fn grow(
    charge: &mut Charge,
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
    crate::entry::check_budget_of(charge.fiber(), bytes);
    if reserve(new_cap.saturating_sub(len)).is_err() {
        super::out_of_memory(bytes);
    }
    charge.recharge(bytes);
}

/// The smallest capacity a buffer grows to, in items.
const MIN_CAP: usize = 8;

/// A vector of scratch items. It derefs to a slice, which cannot grow;
/// it grows only through its own methods, which account for the growth.
pub(crate) struct ScratchVec<T> {
    items: Vec<T>,
    charge: Charge,
}

impl<T> ScratchVec<T> {
    /// An empty vector, which holds no memory.
    pub(crate) const fn new() -> Self {
        Self {
            items: Vec::new(),
            charge: Charge::new(),
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
        grow(&mut self.charge, shape, extra, |more| {
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
pub(crate) struct ScratchString {
    text: String,
    charge: Charge,
}

impl Default for ScratchString {
    fn default() -> Self {
        Self {
            text: String::new(),
            charge: Charge::new(),
        }
    }
}

impl ScratchString {
    /// Appends `text`.
    pub(crate) fn push_str(&mut self, text: &str) {
        if self.text.capacity().saturating_sub(self.text.len()) < text.len() {
            let shape = (self.text.len(), self.text.capacity(), 1);
            let buffer = &mut self.text;
            grow(&mut self.charge, shape, text.len(), |more| {
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
