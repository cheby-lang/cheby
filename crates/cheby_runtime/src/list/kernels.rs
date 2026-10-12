//! The C entry points of lists: the runtime helpers generated code calls
//! (plan §4.1, §4.3.1) and the `std::list` kernels (D-484).
//!
//! Helpers follow the ownership of plan §4.1's table. Kernels borrow their
//! arguments and return owned results (D-187, D-455). A kernel that may
//! update its list in place does so only when the list's count is exactly
//! one: the `@external` wrapper owns that reference and drops it after the
//! call, so the kernel updates the root in place, dups it and returns it,
//! and the wrapper's drop leaves it at count one (plan §6, D-522).

use super::node::{Root, dup_word, register};
use super::ops;
use crate::abi::{PanicKind, Word};
use crate::alloc::{self, scratch::ScratchVec};
use crate::ctx::ExecCtx;
use crate::entry;
use crate::generic::{self, Reached};
use crate::panic::fatal;
use crate::unwind::guard::{DropOnUnwind, guarded};
use crate::unwind::raise::raise_panic;
use crate::values;

/// The root behind a list word from generated code.
///
/// # Safety
///
/// `list` is a live list value.
unsafe fn root(list: Word) -> Root {
    register();
    // SAFETY: forwarded from the caller.
    unsafe { Root::from_word(list) }
}

/// A length as an `Int`.
fn int(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Runs an operation that may update its owned list in place on a
/// borrowed list (see the module doc). `alias` is true when another
/// argument is the same object, which must then not change.
fn in_place(list: Root, alias: bool, op: impl FnOnce(Root) -> Root) -> Word {
    if list.is_unique() && !alias {
        let result = op(list);
        if result != list {
            fatal(format_args!(
                "a list kernel replaced the root it updates in place"
            ));
        }
        dup_word(result.word());
        return result.word();
    }
    dup_word(list.word());
    op(list).word()
}

// ---------------------------------------------------------------------
// Runtime helpers for generated code (plan §4.3.1)

/// `ListPush`: `list` and `elem` owned; result owned. In place when the
/// list is unique (F3).
///
/// Both are consumed even when it raises (D-698): the memory budget is
/// checked for the whole update before the list is taken over
/// (`alloc::preflight`), and if it runs out there, both are released.
///
/// # Safety
///
/// `list` is a live list and `elem` a live uniform word, both owned by the
/// caller and given up.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_list_push(ctx: *mut ExecCtx, list: u64, elem: u64) -> u64 {
    guarded(Some(ctx), || {
        let owned = DropOnUnwind::new([list, elem]);
        // SAFETY: forwarded from the caller.
        let list = unsafe { root(list) };
        alloc::preflight(ops::push_bound(list));
        owned.disarm();
        ops::push(list, elem).word()
    })
}

/// `ListAppend` (`[..a, ..b]`): `a` owned, `b` borrowed; result owned.
/// `a` is consumed even when it raises, as [`cheby_rt_list_push`] says.
///
/// # Safety
///
/// `a` and `b` are live lists; the caller gives up its reference to `a`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_list_append(ctx: *mut ExecCtx, a: u64, b: u64) -> u64 {
    guarded(Some(ctx), || {
        let owned = DropOnUnwind::new([a]);
        // SAFETY: forwarded from the caller.
        let (a, b) = unsafe { (root(a), root(b)) };
        alloc::preflight(ops::append_bound(a, b));
        owned.disarm();
        ops::append(a, b).word()
    })
}

/// `ListLength`: the list borrowed.
///
/// # Safety
///
/// `list` is a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_list_length(list: u64) -> i64 {
    // SAFETY: forwarded from the caller.
    int(ops::len(unsafe { root(list) }))
}

/// `ListGet`: the list borrowed, the index in range (lowering checked the
/// length); the element dup'd.
///
/// # Safety
///
/// `list` is a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_list_get(list: u64, index: i64) -> u64 {
    // SAFETY: forwarded from the caller.
    let list = unsafe { root(list) };
    let elem = usize::try_from(index)
        .ok()
        .and_then(|index| ops::get(list, index))
        .unwrap_or_else(|| {
            fatal(format_args!(
                "list index {index} out of range for length {}",
                ops::len(list)
            ))
        });
    dup_word(elem);
    elem
}

/// `ListSlice` (`..rest` in a list pattern): the list borrowed; an O(1)
/// view of elements `start..end` (D-053), clamped like `list::slice`.
///
/// # Safety
///
/// `list` is a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_list_slice(
    ctx: *mut ExecCtx,
    list: u64,
    start: i64,
    end: i64,
) -> u64 {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let list = unsafe { root(list) };
        let len = ops::len(list);
        ops::slice(list, ops::clamp(start, len), ops::clamp(end, len)).word()
    })
}

/// `ListSlice` on a list that dies at the match: `list` owned; the O(1)
/// view of [`cheby_rt_list_slice`], with the root changed in place when
/// the list's count is one. `list` is consumed even when it raises, as
/// [`cheby_rt_list_push`] says.
///
/// # Safety
///
/// `list` is a live list owned by the caller and given up.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_list_slice_owned(
    ctx: *mut ExecCtx,
    list: u64,
    start: i64,
    end: i64,
) -> u64 {
    guarded(Some(ctx), || {
        let owned = DropOnUnwind::new([list]);
        // SAFETY: forwarded from the caller.
        let list = unsafe { root(list) };
        alloc::preflight(ops::slice_owned_bound());
        owned.disarm();
        let len = ops::len(list);
        ops::slice_owned(list, ops::clamp(start, len), ops::clamp(end, len)).word()
    })
}

// ---------------------------------------------------------------------
// `std::list` kernels (D-484): arguments borrowed, results owned

/// `list::length`. O(1).
///
/// # Safety
///
/// `xs` is a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_length(xs: u64) -> i64 {
    // SAFETY: forwarded from the caller.
    int(ops::len(unsafe { root(xs) }))
}

/// `list::get`: `Some(element)` or `None` outside the list, including
/// negative indexes (D-318). O(log n).
///
/// # Safety
///
/// `xs` is a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_get(xs: u64, index: i64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let xs = unsafe { root(xs) };
        match usize::try_from(index).ok().and_then(|i| ops::get(xs, i)) {
            Some(elem) => {
                dup_word(elem);
                // The reference is released if the `Some` cannot be
                // allocated.
                let held = DropOnUnwind::new([elem]);
                let some = values::some(elem);
                held.disarm();
                some
            }
            None => values::none(),
        }
    })
}

/// `list::push`. O(log n); in place, amortized O(1), at count one.
///
/// # Safety
///
/// `xs` is a live list and `item` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_push(xs: u64, item: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let xs = unsafe { root(xs) };
        in_place(xs, false, |xs| {
            dup_word(item);
            ops::push(xs, item)
        })
    })
}

/// `list::prepend`. O(log n).
///
/// # Safety
///
/// `xs` is a live list and `item` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_prepend(xs: u64, item: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let xs = unsafe { root(xs) };
        in_place(xs, false, |xs| {
            dup_word(item);
            ops::prepend(xs, item)
        })
    })
}

/// `list::append`. O(log n) in the combined length.
///
/// # Safety
///
/// `xs` and `other` are live lists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_append(xs: u64, other: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let (xs, other) = unsafe { (root(xs), root(other)) };
        in_place(xs, xs == other, |xs| ops::append(xs, other))
    })
}

/// `list::set`: unchanged outside the list (D-318). O(log n); in place
/// along the path at count one.
///
/// # Safety
///
/// `xs` is a live list and `item` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_set(xs: u64, index: i64, item: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let xs = unsafe { root(xs) };
        let Some(index) = usize::try_from(index).ok().filter(|&i| i < ops::len(xs)) else {
            dup_word(xs.word());
            return xs.word();
        };
        in_place(xs, false, |xs| {
            dup_word(item);
            ops::set(xs, index, item)
        })
    })
}

/// `list::slice`, clamped (D-298). O(1).
///
/// # Safety
///
/// `xs` is a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_slice(xs: u64, start: i64, end: i64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let xs = unsafe { root(xs) };
        let len = ops::len(xs);
        ops::slice(xs, ops::clamp(start, len), ops::clamp(end, len)).word()
    })
}

/// `list::take`, clamped (D-298). O(1).
///
/// # Safety
///
/// `xs` is a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_take(xs: u64, count: i64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let xs = unsafe { root(xs) };
        ops::slice(xs, 0, ops::clamp(count, ops::len(xs))).word()
    })
}

/// `list::drop`, clamped (D-298). O(1).
///
/// # Safety
///
/// `xs` is a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_drop(xs: u64, count: i64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let xs = unsafe { root(xs) };
        let len = ops::len(xs);
        ops::slice(xs, ops::clamp(count, len), len).word()
    })
}

/// `list::split_at`, clamped (D-298): the tuple `(take, drop)`. O(1).
///
/// # Safety
///
/// `xs` is a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_split_at(xs: u64, count: i64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let xs = unsafe { root(xs) };
        let len = ops::len(xs);
        let count = ops::clamp(count, len);
        let front = DropOnUnwind::new([ops::slice(xs, 0, count).word()]);
        let back = DropOnUnwind::new([ops::slice(xs, count, len).word()]);
        let ([front_word], [back_word]) = (front.words(), back.words());
        let pair = values::tuple(&[front_word, back_word]);
        front.disarm();
        back.disarm();
        pair
    })
}

/// `list::unique`: `xs` without repeated elements, keeping the first of
/// each group of `==` elements, in order. Elements are hashed like `Set`
/// elements (§3.13) with the running entry's seed, so an element that
/// holds a function value or a handle panics `EqualityOnFunction` at an
/// unknown location (D-233, D-556). O(n) expected. When no element repeats,
/// the result is `xs` itself with one more reference, as D-577 does for
/// string kernels (D-589).
///
/// # Safety
///
/// `xs` is a live list, on an entry's thread when its elements may hold a
/// type with `@std_hooks`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_unique(xs: u64) -> u64 {
    fn reached<T>(_: Reached) -> T {
        raise_panic(None, PanicKind::EqualityOnFunction, None)
    }
    guarded(None, || {
        register();
        let seed = entry::current_seed();
        // Elements are borrowed from `xs`, which the caller keeps alive. The
        // kept elements and the table that finds them are scratch buffers,
        // so the memory limit counts them (D-617), and an unwinding drops
        // them (D-697).
        let mut kept: ScratchVec<Word> = ScratchVec::new();
        let mut table = Table::default();
        let mut repeated = false;
        // SAFETY: forwarded from the caller.
        for item in unsafe { super::iter(xs) } {
            // SAFETY: a live element of a live list.
            let hash = unsafe { generic::hash_with_seed(item, seed) }.unwrap_or_else(reached);
            let found = table.find_or_insert(hash, kept.len(), |index| {
                let other = kept.get(index).copied().unwrap_or_default();
                // SAFETY: live elements of one type.
                unsafe { generic::equal(other, item) }.unwrap_or_else(reached)
            });
            if found {
                repeated = true;
            } else {
                kept.push(item);
            }
        }
        if !repeated {
            dup_word(xs);
            return xs;
        }
        for &item in kept.iter() {
            dup_word(item);
        }
        super::from_iter(kept.drain())
    })
}

/// An open-addressing hash table of the indices of the kept elements of
/// [`cheby_list_unique`], keyed by their hashes, at most half full. A slot
/// holds a hash and an index plus one; 0 marks an empty slot.
#[derive(Default)]
struct Table {
    slots: ScratchVec<(u64, usize)>,
    len: usize,
}

impl Table {
    /// Whether an element with `hash` that `same` says is equal to the new
    /// element is in the table. If none is, the new element is inserted
    /// as the element at `index`.
    fn find_or_insert(&mut self, hash: u64, index: usize, same: impl Fn(usize) -> bool) -> bool {
        if self.len.saturating_add(1).saturating_mul(2) > self.slots.len() {
            self.grow();
        }
        let mask = self.slots.len().saturating_sub(1);
        // The low bits of a SipHash are as good as any.
        let mut at = usize::try_from(hash).unwrap_or(usize::MAX) & mask;
        loop {
            let Some(slot) = self.slots.get_mut(at) else {
                fatal(format_args!("the table of `list::unique` has no slot {at}"));
            };
            match *slot {
                (_, 0) => {
                    *slot = (hash, index.saturating_add(1));
                    self.len = self.len.saturating_add(1);
                    return false;
                }
                (other, kept) if other == hash && same(kept.saturating_sub(1)) => return true,
                _ => at = at.wrapping_add(1) & mask,
            }
        }
    }

    /// Doubles the slots, at least 16, and inserts the entries again.
    fn grow(&mut self) {
        let size = self.slots.len().saturating_mul(2).max(16);
        let mut slots = ScratchVec::new();
        slots.reserve(size);
        slots.extend(std::iter::repeat_n((0, 0), size));
        let mask = size.saturating_sub(1);
        for &(hash, kept) in self.slots.iter().filter(|(_, kept)| *kept != 0) {
            let mut at = usize::try_from(hash).unwrap_or(usize::MAX) & mask;
            while let Some(slot) = slots.get_mut(at) {
                if slot.1 == 0 {
                    *slot = (hash, kept);
                    break;
                }
                at = at.wrapping_add(1) & mask;
            }
        }
        self.slots = slots;
    }
}

/// The kernels, with their std symbols.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
pub(super) fn all() -> [(&'static str, *const u8); 11] {
    [
        ("cheby_list_length", cheby_list_length as *const u8),
        ("cheby_list_get", cheby_list_get as *const u8),
        ("cheby_list_push", cheby_list_push as *const u8),
        ("cheby_list_prepend", cheby_list_prepend as *const u8),
        ("cheby_list_append", cheby_list_append as *const u8),
        ("cheby_list_set", cheby_list_set as *const u8),
        ("cheby_list_slice", cheby_list_slice as *const u8),
        ("cheby_list_take", cheby_list_take as *const u8),
        ("cheby_list_drop", cheby_list_drop as *const u8),
        ("cheby_list_split_at", cheby_list_split_at as *const u8),
        ("cheby_list_unique", cheby_list_unique as *const u8),
    ]
}
