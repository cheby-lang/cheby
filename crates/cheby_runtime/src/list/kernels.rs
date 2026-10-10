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
use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::panic::fatal;
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
/// # Safety
///
/// `list` is a live list and `elem` a live uniform word, both owned by the
/// caller and given up.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_list_push(ctx: *mut ExecCtx, list: u64, elem: u64) -> u64 {
    let _ = ctx;
    // SAFETY: forwarded from the caller.
    ops::push(unsafe { root(list) }, elem).word()
}

/// `ListAppend` (`[..a, ..b]`): `a` owned, `b` borrowed; result owned.
///
/// # Safety
///
/// `a` and `b` are live lists; the caller gives up its reference to `a`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_list_append(ctx: *mut ExecCtx, a: u64, b: u64) -> u64 {
    let _ = ctx;
    // SAFETY: forwarded from the caller.
    let (a, b) = unsafe { (root(a), root(b)) };
    ops::append(a, b).word()
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
    let _ = ctx;
    // SAFETY: forwarded from the caller.
    let list = unsafe { root(list) };
    let len = ops::len(list);
    ops::slice(list, ops::clamp(start, len), ops::clamp(end, len)).word()
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
    // SAFETY: forwarded from the caller.
    let xs = unsafe { root(xs) };
    match usize::try_from(index).ok().and_then(|i| ops::get(xs, i)) {
        Some(elem) => {
            dup_word(elem);
            values::some(elem)
        }
        None => values::none(),
    }
}

/// `list::push`. O(log n); in place, amortized O(1), at count one.
///
/// # Safety
///
/// `xs` is a live list and `item` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_push(xs: u64, item: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    let xs = unsafe { root(xs) };
    in_place(xs, false, |xs| {
        dup_word(item);
        ops::push(xs, item)
    })
}

/// `list::prepend`. O(log n).
///
/// # Safety
///
/// `xs` is a live list and `item` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_prepend(xs: u64, item: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    let xs = unsafe { root(xs) };
    in_place(xs, false, |xs| {
        dup_word(item);
        ops::prepend(xs, item)
    })
}

/// `list::append`. O(log n) in the combined length.
///
/// # Safety
///
/// `xs` and `other` are live lists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_append(xs: u64, other: u64) -> u64 {
    // SAFETY: forwarded from the caller.
    let (xs, other) = unsafe { (root(xs), root(other)) };
    in_place(xs, xs == other, |xs| ops::append(xs, other))
}

/// `list::set`: unchanged outside the list (D-318). O(log n); in place
/// along the path at count one.
///
/// # Safety
///
/// `xs` is a live list and `item` a live uniform word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_set(xs: u64, index: i64, item: u64) -> u64 {
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
}

/// `list::slice`, clamped (D-298). O(1).
///
/// # Safety
///
/// `xs` is a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_slice(xs: u64, start: i64, end: i64) -> u64 {
    // SAFETY: forwarded from the caller.
    let xs = unsafe { root(xs) };
    let len = ops::len(xs);
    ops::slice(xs, ops::clamp(start, len), ops::clamp(end, len)).word()
}

/// `list::take`, clamped (D-298). O(1).
///
/// # Safety
///
/// `xs` is a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_take(xs: u64, count: i64) -> u64 {
    // SAFETY: forwarded from the caller.
    let xs = unsafe { root(xs) };
    ops::slice(xs, 0, ops::clamp(count, ops::len(xs))).word()
}

/// `list::drop`, clamped (D-298). O(1).
///
/// # Safety
///
/// `xs` is a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_drop(xs: u64, count: i64) -> u64 {
    // SAFETY: forwarded from the caller.
    let xs = unsafe { root(xs) };
    let len = ops::len(xs);
    ops::slice(xs, ops::clamp(count, len), len).word()
}

/// `list::split_at`, clamped (D-298): the tuple `(take, drop)`. O(1).
///
/// # Safety
///
/// `xs` is a live list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_list_split_at(xs: u64, count: i64) -> u64 {
    // SAFETY: forwarded from the caller.
    let xs = unsafe { root(xs) };
    let len = ops::len(xs);
    let count = ops::clamp(count, len);
    let front = ops::slice(xs, 0, count).word();
    let back = ops::slice(xs, count, len).word();
    values::tuple(&[front, back])
}

/// The kernels, with their std symbols. `unique` comes with W3-B's
/// hashing.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
pub(super) fn all() -> [(&'static str, *const u8); 10] {
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
    ]
}
