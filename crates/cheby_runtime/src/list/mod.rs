//! Persistent lists (F3, `std::list`): relaxed radix balanced trees of
//! uniform words (D-039, ADR-0018) whose root is also a view, so a pattern
//! step on a unique list changes its root and one slot in place (D-053,
//! D-634), releasing the element it cuts (D-893) or, when the pattern
//! binds that element, moving it out in the same descent (D-909).
//!
//! - [`node`]: the objects (layout ids 26-29) and their [`CustomKind`]s,
//!   and the static empty list `cheby_rt_static_empty_list`.
//! - [`tree`]: get, set, push, take, drop and concatenation on trees, in
//!   place along the path where nodes are unique.
//! - [`ops`]: the operations on list values (views over a tree).
//! - [`buffers`]: the head and tail leaves that a big root keeps outside
//!   its tree, and the fast paths of `push`, `prepend` and the pattern
//!   steps at either end of a list (D-1757).
//! - `kernels`: the runtime helpers for generated code (plan §4.1) and
//!   the `std::list` kernels (D-484).
//! - `heap`: the `std::heap` kernels, a binary heap kept in a list's tree
//!   and updated in place (D-1612, D-1613).
//!
//! Costs, as `docs/stdlib/list.md` requires (D-300) and the tests in
//! `tests.rs` pin: `length` O(1); `get` and `set` O(log n); `push` and
//! `prepend` O(log n), and at count one in place with amortized O(1) time
//! and allocation; `append` O(log n); `slice`, `take`, `drop` and
//! `split_at` O(1) views, after an O(log n) fold of a big root's leaves
//! into its tree when it has any.
//!
//! For other runtime modules: [`iter`] walks a list's elements and
//! [`from_iter`] builds one (plan §4.3.4).
//!
//! [`CustomKind`]: crate::layout::CustomKind

mod buffers;
mod heap;
pub(crate) mod kernels;
mod node;
mod ops;
mod tree;

#[cfg(test)]
mod tests;

pub use heap::{
    cheby_heap_sys_from_list, cheby_heap_sys_pop, cheby_heap_sys_push, cheby_heap_sys_to_list,
};
pub use kernels::{
    cheby_list_append, cheby_list_drop, cheby_list_get, cheby_list_length, cheby_list_set,
    cheby_list_slice, cheby_list_sort_with, cheby_list_split_at, cheby_list_take,
    cheby_list_unique, cheby_rt_list_append, cheby_rt_list_get, cheby_rt_list_length,
    cheby_rt_list_prepend, cheby_rt_list_push, cheby_rt_list_slice, cheby_rt_list_slice_owned,
    cheby_rt_list_uncons, cheby_rt_list_uncons_owned,
};
pub use node::{StaticList, cheby_rt_static_empty_list};
pub(crate) use ops::Iter;

use crate::abi::Word;

/// The `std::list` and `std::heap` kernels this module registers, as
/// `(symbol, address)` pairs. The `cheby_rt_list_*` helpers and the static empty list are
/// runtime helpers, registered by `symbols.rs` itself.
///
/// Also registers the list layouts (26-29), so generated code that only
/// `dup`s or prints the static empty list finds layout 26 even before any
/// list function ran: `symbols()` calls this whenever a JIT is built.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    node::register();
    out.extend(kernels::all());
    out.extend(heap::all());
}

/// Registers the list layouts (26-29) once per process, as `kernels`
/// does: an executable's start calls it through `image::install`, since
/// it never builds the symbol table (D-919).
pub(crate) fn register_kinds() {
    node::register();
}

/// The std module paths whose kernels this module registers completely.
pub(crate) const COMPLETE: &[&str] = &["list", "heap"];

/// The C signatures of the `std::heap` kernels, for the test that compares
/// them with their declarations.
#[cfg(test)]
pub(crate) use heap::signatures as heap_signatures;

/// The elements of a borrowed list, in order, each borrowed: valid while
/// the iterator lives (it holds a reference to the list). O(1) amortized
/// per element, with an explicit stack: equality, hashing and debug text
/// of lists (W3-B) walk lists with it.
///
/// # Safety
///
/// `list` is a live list value.
pub(crate) unsafe fn iter(list: Word) -> Iter {
    node::register();
    // SAFETY: forwarded from the caller.
    Iter::new(unsafe { node::Root::from_word(list) })
}

/// Folds a big root's head and tail leaves into its tree
/// ([`buffers`]) before the value reaches another fiber or becomes
/// static, since such a root is never written again. Does nothing for
/// any other value.
///
/// # Safety
///
/// `word` is a tagged integer, 0, or a live object that only this fiber
/// holds.
pub(crate) unsafe fn settle_value(word: Word) {
    // SAFETY: forwarded from the caller.
    unsafe { buffers::settle_value(word) };
}

/// Whether `id` is the layout of a list value: a root or a big root.
pub(crate) fn is_root(id: crate::abi::LayoutId) -> bool {
    node::is_root(id)
}

/// The number of elements of a borrowed list. O(1).
///
/// # Safety
///
/// `list` is a live list value.
pub(crate) unsafe fn length(list: Word) -> usize {
    // SAFETY: forwarded from the caller.
    ops::len(unsafe { node::Root::from_word(list) })
}

/// Element `index` of a borrowed list, borrowed, or `None` outside it
/// (`cheby_ffi_list_get`, D-945).
///
/// # Safety
///
/// `list` is a live list value; the element is valid while it is.
pub(crate) unsafe fn get(list: Word, index: usize) -> Option<Word> {
    // SAFETY: forwarded from the caller.
    ops::get(unsafe { node::Root::from_word(list) }, index)
}

/// A list of the given owned elements, in order; the result is owned.
/// Pushes in place, so the cost is O(n) with amortized O(1) allocation per
/// element.
pub(crate) fn from_iter(items: impl IntoIterator<Item = Word>) -> Word {
    node::register();
    ops::from_iter(items).word()
}
