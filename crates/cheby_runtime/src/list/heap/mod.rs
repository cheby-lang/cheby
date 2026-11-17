//! The kernels of `std::heap` (D-1612, D-1613): a binary heap in a list's
//! tree (ADR-0018), updated in place when the list is unique (D-525).
//!
//! `std/heap.cheby` keeps a heap's elements in a `List<T>` and passes it
//! to these kernels with the direction and the element type's `compare`
//! as a closure. With `before(x, y)` meaning that `compare(x, y)` is
//! `Less` for a min-heap and `Greater` for a max-heap, every kernel runs
//! D-1613's algorithm, so a `compare` that is not a consistent order gives
//! the same heap and sees the same calls in the same order on every
//! target:
//!
//! - `sys_push` puts the element at the end and, while it is not at the
//!   root and `before(x, parent)` holds, swaps it with its parent, the
//!   element at `(i - 1) / 2` ([`sift_up`]).
//! - `sys_pop` drops the root (the caller has read it), moves the last
//!   element to the root and sifts it down: it chooses the right child
//!   `2i + 2` over the left `2i + 1` only when `before(right, left)`
//!   holds, and swaps while `before(child, element)` holds
//!   ([`sift_down`]).
//! - `sys_from_list` keeps the list's order and sifts down each position
//!   from `n / 2 - 1` to `0`.
//! - `sys_to_list` lists the elements in the order repeated pops give,
//!   popping a scratch copy.
//!
//! `compare` is called back through `callback::call`, as `list::sort_with`
//! calls its comparison (D-1448, D-526), except when the element type's
//! `compare` is built in, which [`order`] reads from the type's
//! descriptor: then the kernel compares natively, which gives the same
//! `Order` and so the same heap.
//!
//! A kernel borrows its list (D-187). At count one, with a tree that holds
//! exactly the list's elements, it updates the list in place and returns
//! it with one more reference, which the wrapper's drop takes back
//! (D-525). Otherwise it works on a new root that shares the tree, and the
//! first write under a shared node copies the path to it
//! ([`tree::unique_path`]), so the argument never changes. A sift moves
//! elements only by exchanging two slots, so whenever `compare` is called
//! back the tree holds each element once: a panic in `compare` unwinds
//! through the kernel's guard (D-697) and leaves a tree that the wrapper's
//! drop, or [`Array`]'s, releases whole.

mod order;
#[cfg(test)]
mod tests;

use std::cmp::Ordering;

use super::node::{BITS, M, Node, Root, View, drop_word, dup_word, empty, register};
use super::{ops, tree};
use crate::abi::{self, Word};
use crate::alloc::scratch::ScratchVec;
use crate::callback;
use crate::panic::fatal;
use crate::rc;
use crate::unwind::guard::guarded;
use crate::values;

/// What a slot whose element moved out holds until the tree is cut: a
/// tagged `Int`, which owns nothing (D-521).
const CLEARED: Word = match abi::tag_int(0) {
    Some(word) => word,
    None => panic!("0 is a tagged Int"),
};

/// D-1613's `before`: whether one element goes before another in the
/// heap's direction, which is the `Order` it holds, `Less` for a min-heap
/// and `Greater` for a max-heap. One type per way of comparing, so each
/// sift is compiled for it (`with_before!`).
trait Before {
    /// `before(a, b)`.
    ///
    /// # Safety
    ///
    /// `a` and `b` are live elements; on an entry's thread when the
    /// comparison is called back.
    unsafe fn holds(&self, a: Word, b: Word) -> bool;
}

/// The built-in `compare` of `Int` and the integer types like it.
#[derive(Debug)]
struct Signed(Ordering);

impl Before for Signed {
    unsafe fn holds(&self, a: Word, b: Word) -> bool {
        // SAFETY: forwarded from the caller: two such integers.
        unsafe { values::word_to_i64(a).cmp(&values::word_to_i64(b)) == self.0 }
    }
}

/// Any other built-in `compare` (D-1613).
#[derive(Debug)]
struct Native(order::Key, Ordering);

impl Before for Native {
    unsafe fn holds(&self, a: Word, b: Word) -> bool {
        // SAFETY: forwarded from the caller: elements of the key's type.
        unsafe { self.0.compare(a, b) == self.1 }
    }
}

/// `compare` called back through `callback::call` (D-526), each call with
/// its own references to its arguments.
#[derive(Debug)]
struct Callback(Word, Ordering);

impl Before for Callback {
    unsafe fn holds(&self, a: Word, b: Word) -> bool {
        dup_word(a);
        dup_word(b);
        // SAFETY: on the caller's entry thread; the closure is borrowed and
        // its arguments given away.
        let result = unsafe { callback::call(self.0, &[a, b]) };
        // SAFETY: the closure returns an `Order`, owned.
        let order = unsafe { values::word_to_order(result) };
        // SAFETY: as above.
        unsafe { rc::drop(result) };
        order == self.1
    }
}

/// Evaluates `$body` with `$before` bound to the [`Before`] of the closure
/// `$by` for a max-heap when `$max` holds: built in when the element
/// type's descriptor says so ([`order::native_key`]), called back
/// otherwise. `$by` must be a live closure of two elements that returns an
/// `Order`.
macro_rules! with_before {
    ($by:expr, $max:expr, |$before:ident| $body:expr) => {{
        let first = if $max {
            Ordering::Greater
        } else {
            Ordering::Less
        };
        // SAFETY: the caller's: `$by` is a live closure.
        match unsafe { order::native_key($by) } {
            Some(order::Key::Signed) => {
                let $before = &Signed(first);
                $body
            }
            Some(key @ (order::Key::Unsigned | order::Key::Text | order::Key::Tuple(_))) => {
                let $before = &Native(key, first);
                $body
            }
            None => {
                let $before = &Callback($by, first);
                $body
            }
        }
    }};
}

/// The slots of a heap. A [`Slots::Slot`] names where element `i` is, so
/// that a sift finds each element once: reading it, comparing it and
/// swapping it then go to the same place.
trait Slots {
    /// Where an element is.
    type Slot: Copy;
    /// The number of elements.
    fn len(&self) -> usize;
    /// Where element `i` (below the length) is.
    fn slot(&mut self, i: usize) -> Self::Slot;
    /// The element in `slot`, borrowed.
    fn read(&self, slot: Self::Slot) -> Word;
    /// Swaps the elements in `a` and `b`, and returns where `b` is now,
    /// which holds the element that was in `a`.
    fn swap(&mut self, a: Self::Slot, b: Self::Slot) -> Self::Slot;
}

/// D-1613's sift-up of the element at `i`: while it is not at the root and
/// `before(x, parent)` holds, swap it with its parent.
///
/// # Safety
///
/// As for [`Before::holds`] on the elements of `slots`.
unsafe fn sift_up(slots: &mut impl Slots, at: usize, before: &impl Before) {
    let mut i = at;
    let mut here = slots.slot(i);
    let x = slots.read(here);
    while i > 0 {
        let parent = i.saturating_sub(1) / 2;
        let above = slots.slot(parent);
        // SAFETY: forwarded from the caller.
        if !unsafe { before.holds(x, slots.read(above)) } {
            break;
        }
        here = slots.swap(here, above);
        i = parent;
    }
}

/// D-1613's sift-down of the element at `i`: take the right child over the
/// left only when `before(right, left)` holds, and swap while
/// `before(child, element)` holds.
///
/// # Safety
///
/// As for [`Before::holds`] on the elements of `slots`.
unsafe fn sift_down(slots: &mut impl Slots, at: usize, before: &impl Before) {
    let count = slots.len();
    let mut position = at;
    let mut here = slots.slot(position);
    let element = slots.read(here);
    loop {
        let left = position.saturating_mul(2).saturating_add(1);
        if left >= count {
            break;
        }
        let right = left.saturating_add(1);
        let (mut child, mut below) = (left, slots.slot(left));
        let mut chosen = slots.read(below);
        if right < count {
            let other = slots.slot(right);
            let candidate = slots.read(other);
            // SAFETY: forwarded from the caller.
            if unsafe { before.holds(candidate, chosen) } {
                (child, below, chosen) = (right, other, candidate);
            }
        }
        // SAFETY: as above.
        if !unsafe { before.holds(chosen, element) } {
            break;
        }
        here = slots.swap(here, below);
        position = child;
    }
}

/// D-1613's `from_list`: sifts down each position from `n / 2 - 1` to `0`.
///
/// # Safety
///
/// As for [`sift_down`].
unsafe fn heapify(slots: &mut impl Slots, before: &impl Before) {
    for start in (0..slots.len() / 2).rev() {
        // SAFETY: forwarded from the caller.
        unsafe { sift_down(slots, start, before) };
    }
}

/// A leaf found for an index: the elements `start..end` of the tree are
/// its slots, and `writable` tells whether every node on its path is
/// unique, so its slots may be written in place.
#[derive(Debug, Clone, Copy)]
struct Found {
    start: usize,
    end: usize,
    leaf: Node,
    writable: bool,
}

/// A node on a descent: the elements `start..end` of the tree are below
/// it, `writable` tells whether every node from the root to it is unique,
/// and `full` whether it holds as many elements as its height allows, so
/// that every node below it is full too.
#[derive(Debug, Clone, Copy)]
struct Step {
    node: Node,
    start: usize,
    end: usize,
    writable: bool,
    full: bool,
}

/// The leaves an [`Array`] keeps: those of an element and of its
/// children.
const FOUND: usize = 2;

/// A list whose elements a kernel reads and swaps in place: a unique root
/// whose tree holds exactly its elements, the argument itself or a new
/// root sharing its tree (module documentation).
///
/// The two leaves found last are kept, since a sift reads an element's
/// children and then writes the element and the child it moved: in every
/// level below the first leaf's, those are two leaves. A write under a
/// shared node copies the path, which may replace any node a kept leaf
/// was found through, so it forgets them.
#[derive(Debug)]
struct Array {
    root: Root,
    /// Whether this array holds its own reference to `root`, released when
    /// it is dropped (by an unwinding) and given to the result otherwise.
    owned: bool,
    /// The root's tree; `None` only for the empty list.
    tree: Option<Node>,
    /// The tree's height.
    height: usize,
    len: usize,
    found: [Option<Found>; FOUND],
    /// The entry of `found` the next leaf replaces.
    next: usize,
}

impl Array {
    /// The array of the borrowed list `list`: the list itself at count one
    /// when its tree holds exactly its elements, otherwise a new root
    /// sharing its tree, or a copy of its elements when the list is a view
    /// of part of its tree.
    fn new(list: Root) -> Self {
        let view = list.view();
        let Some(tree) = view.tree else {
            return Self::with(list, false, None, 0);
        };
        let exact = view.offset == 0 && tree.len() == view.len;
        if exact && list.is_unique() {
            return Self::with(list, false, Some(tree), view.len);
        }
        if exact {
            tree.dup();
            return Self::with(Root::new(view), true, Some(tree), view.len);
        }
        let copy = ops::from_iter(ops::Iter::new(list).inspect(|&elem| dup_word(elem)));
        Self::with(copy, true, copy.view().tree, view.len)
    }

    fn with(root: Root, owned: bool, tree: Option<Node>, len: usize) -> Self {
        Self {
            root,
            owned,
            tree,
            height: tree.map_or(0, Node::height),
            len,
            found: [None; FOUND],
            next: 0,
        }
    }

    /// The list, owned by the caller.
    fn into_word(self) -> Word {
        let word = self.root.word();
        if !self.owned {
            dup_word(word);
        }
        std::mem::forget(self);
        word
    }

    /// Stores a changed tree and length in the root, which this array
    /// may write: unique, and not the static empty list. Forgets the
    /// leaves found.
    fn store(&mut self, tree: Node, len: usize) {
        self.tree = Some(tree);
        self.height = tree.height();
        self.len = len;
        self.found = [None; FOUND];
        if self.len_of_root_is_static() {
            // The static empty list: a new root, which this array owns. The
            // borrowed empty list is not released.
            self.root = Root::new(View {
                tree: Some(tree),
                offset: 0,
                len,
            });
            self.owned = true;
            return;
        }
        self.root.store(View {
            tree: Some(tree),
            offset: 0,
            len,
        });
    }

    /// Whether the root is the static empty list.
    fn len_of_root_is_static(&self) -> bool {
        self.root.word() == empty()
    }

    /// The leaf holding element `i` (below the length).
    ///
    /// A tree built by pushes and cut at its end, as a heap's is, is
    /// regular: every node off its right edge is full. So a branch whose children before the
    /// last hold `M^h` elements each has full subtrees there, and below
    /// such a child the path is read off the index, a radix digit per
    /// level, with no size table read. Any other branch is searched by
    /// [`Node::locate_at`].
    fn find(&mut self, i: usize) -> Found {
        if let Some(found) = self
            .found
            .iter()
            .flatten()
            .find(|found| found.start <= i && i < found.end)
        {
            return *found;
        }
        let Some(tree) = self.tree else {
            fatal(format_args!(
                "`std::heap` read element {i} of an empty list"
            ));
        };
        let mut step = Step {
            node: tree,
            start: 0,
            end: self.len,
            writable: tree.is_unique(),
            full: false,
        };
        let mut height = self.height;
        while height > 0 {
            let shift = BITS.saturating_mul(u32::try_from(height).unwrap_or(u32::MAX));
            let span = 1_usize.checked_shl(shift).unwrap_or(usize::MAX);
            let rest = i.saturating_sub(step.start);
            let (j, before, end) = if step.full {
                let j = rest.checked_shr(shift).unwrap_or(0) & (M - 1);
                let before = j.checked_shl(shift).unwrap_or(0);
                (
                    j,
                    before,
                    step.start.saturating_add(before).saturating_add(span),
                )
            } else {
                let count = step.node.slots();
                let last = count.saturating_sub(1);
                let prefix = last.checked_shl(shift).unwrap_or(usize::MAX);
                if last == 0 {
                    (0, 0, step.end)
                } else if step.node.size_at(last.saturating_sub(1)) == prefix {
                    let j = rest.checked_shr(shift).unwrap_or(0).min(last);
                    let before = j.checked_shl(shift).unwrap_or(0);
                    let end = if j < last {
                        step.start.saturating_add(before).saturating_add(span)
                    } else {
                        step.end
                    };
                    (j, before, end)
                } else {
                    let (j, before) = step.node.locate_at(rest, height);
                    (j, before, step.start.saturating_add(step.node.size_at(j)))
                }
            };
            let child = step.node.child(j);
            let start = step.start.saturating_add(before);
            step = Step {
                node: child,
                start,
                end,
                writable: step.writable && child.is_unique(),
                // A full subtree's children are full, and so is a child
                // before the last of a regular branch.
                full: step.full || end.saturating_sub(start) == span,
            };
            height = height.saturating_sub(1);
        }
        let found = Found {
            start: step.start,
            end: step.start.saturating_add(step.node.slots()),
            leaf: step.node,
            writable: step.writable,
        };
        if let Some(entry) = self.found.get_mut(self.next) {
            *entry = Some(found);
        }
        self.next = (self.next.saturating_add(1)) % FOUND;
        found
    }

    /// The leaf holding element `i` (below the length), with every node on
    /// its path unique, copying the shared ones.
    fn writable(&mut self, i: usize) -> Found {
        let found = self.find(i);
        if found.writable {
            return found;
        }
        let Some(tree) = self.tree else {
            fatal(format_args!(
                "`std::heap` wrote element {i} of an empty list"
            ));
        };
        let tree = tree::unique_path(tree, i);
        self.store(tree, self.len);
        let found = self.find(i);
        if !found.writable {
            fatal(format_args!(
                "`std::heap` could not make element {i} unique"
            ));
        }
        found
    }

    /// Replaces element `i` (below the length) with the owned `elem` and
    /// releases the element it held.
    fn set(&mut self, i: usize, elem: Word) {
        let found = self.writable(i);
        let slot = i.saturating_sub(found.start);
        let old = found.leaf.elem(slot);
        found.leaf.put_elem(slot, elem);
        drop_word(old);
    }

    /// Adds the owned `elem` at the end.
    fn push(&mut self, elem: Word) {
        let tree = match self.tree {
            Some(tree) => tree::push(tree, elem),
            None => tree::singleton(elem),
        };
        self.store(tree, self.len.saturating_add(1));
    }

    /// Removes the last element (of at least two) and returns it, owned.
    fn pop_last(&mut self) -> Word {
        let Some(tree) = self.tree.filter(|_| self.len > 1) else {
            fatal(format_args!(
                "`std::heap` cut a list of {} elements",
                self.len
            ));
        };
        let last = self.len.saturating_sub(1);
        let elem = tree::take_or_get(tree, last, CLEARED);
        self.store(tree::take(tree, last), last);
        elem
    }
}

/// Where an element of an [`Array`] is: its index and its leaf.
#[derive(Debug, Clone, Copy)]
struct Place {
    i: usize,
    found: Found,
}

impl Place {
    /// The element's slot in its leaf.
    fn at(self) -> usize {
        self.i.saturating_sub(self.found.start)
    }
}

impl Slots for Array {
    type Slot = Place;

    fn len(&self) -> usize {
        self.len
    }

    fn slot(&mut self, i: usize) -> Place {
        Place {
            i,
            found: self.find(i),
        }
    }

    fn read(&self, slot: Place) -> Word {
        slot.found.leaf.elem(slot.at())
    }

    fn swap(&mut self, a: Place, b: Place) -> Place {
        let (a, b) = if a.found.writable && b.found.writable {
            (a, b)
        } else {
            // The path to `a` is unique once written, so making `b`'s
            // unique copies no node on it: `a` stays in the tree.
            let a = Place {
                i: a.i,
                found: self.writable(a.i),
            };
            let b = Place {
                i: b.i,
                found: self.writable(b.i),
            };
            (a, b)
        };
        let (x, y) = (self.read(a), self.read(b));
        a.found.leaf.put_elem(a.at(), y);
        b.found.leaf.put_elem(b.at(), x);
        b
    }
}

impl Drop for Array {
    /// Runs only when an unwinding leaves the kernel (`into_word` forgets
    /// the array otherwise): releases the root this array made.
    fn drop(&mut self) {
        if self.owned {
            drop_word(self.root.word());
        }
    }
}

/// The borrowed elements of a scratch copy, for `sys_to_list`.
struct Scratch<'a> {
    items: &'a mut [Word],
    len: usize,
}

impl Slots for Scratch<'_> {
    type Slot = usize;

    fn len(&self) -> usize {
        self.len
    }

    fn slot(&mut self, i: usize) -> usize {
        i
    }

    fn read(&self, slot: usize) -> Word {
        self.items
            .get(slot)
            .copied()
            .unwrap_or_else(|| fatal(format_args!("`std::heap` read past its copy at {slot}")))
    }

    fn swap(&mut self, a: usize, b: usize) -> usize {
        self.items.swap(a, b);
        b
    }
}

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

/// `heap::push`: `items` with `item` added and sifted up (D-1613). In
/// place at count one; O(log n) time and calls of `compare`.
///
/// # Safety
///
/// `items` is a live list, `item` a live element and `by` a live closure
/// of two elements that returns an `Order`; on an entry's thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_heap_sys_push(items: u64, item: u64, max: bool, by: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let mut array = Array::new(unsafe { root(items) });
        dup_word(item);
        array.push(item);
        let last = array.len().saturating_sub(1);
        with_before!(by, max, |before| {
            // SAFETY: forwarded from the caller.
            unsafe { sift_up(&mut array, last, before) }
        });
        array.into_word()
    })
}

/// `heap::pop` after the caller has read the root: `items` without its
/// root, the last element moved there and sifted down (D-1613). In place
/// at count one; O(log n) time and calls of `compare`.
///
/// # Safety
///
/// As for [`cheby_heap_sys_push`], without `item`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_heap_sys_pop(items: u64, max: bool, by: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let list = unsafe { root(items) };
        if list.len() <= 1 {
            return empty();
        }
        let mut array = Array::new(list);
        let moved = array.pop_last();
        array.set(0, moved);
        with_before!(by, max, |before| {
            // SAFETY: forwarded from the caller.
            unsafe { sift_down(&mut array, 0, before) }
        });
        array.into_word()
    })
}

/// `heap::from_list`: `items` in their order, each position from
/// `n / 2 - 1` down to `0` sifted down (D-1613). In place at count one;
/// O(n) time and calls of `compare`.
///
/// # Safety
///
/// As for [`cheby_heap_sys_pop`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_heap_sys_from_list(items: u64, max: bool, by: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let mut array = Array::new(unsafe { root(items) });
        with_before!(by, max, |before| {
            // SAFETY: forwarded from the caller.
            unsafe { heapify(&mut array, before) }
        });
        array.into_word()
    })
}

/// `heap::to_list`: the elements in the order repeated pops give, each
/// pop on a scratch copy as `sys_pop` does it (D-1613). Never changes
/// `items`; O(n log n) time and calls of `compare`.
///
/// # Safety
///
/// As for [`cheby_heap_sys_pop`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_heap_sys_to_list(items: u64, max: bool, by: u64) -> u64 {
    guarded(None, || {
        // SAFETY: forwarded from the caller.
        let list = unsafe { root(items) };
        // Borrowed from `items`, which the caller keeps alive; the buffers
        // are scratch, which the memory limit counts (D-617) and an
        // unwinding drops (D-697).
        let mut copy: ScratchVec<Word> = ScratchVec::new();
        copy.reserve(list.len());
        copy.extend(ops::Iter::new(list));
        let mut popped: ScratchVec<Word> = ScratchVec::new();
        popped.reserve(copy.len());
        let mut slots = Scratch {
            len: copy.len(),
            items: &mut copy,
        };
        with_before!(by, max, |before| {
            while slots.len > 0 {
                popped.push(slots.read(0));
                let end = slots.len.saturating_sub(1);
                slots.swap(0, end);
                slots.len = end;
                // SAFETY: forwarded from the caller.
                unsafe { sift_down(&mut slots, 0, before) };
            }
        });
        for &item in popped.iter() {
            dup_word(item);
        }
        super::from_iter(popped.drain())
    })
}

/// The kernels, with their std symbols.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
pub(super) fn all() -> [(&'static str, *const u8); 4] {
    [
        ("cheby_heap_sys_push", cheby_heap_sys_push as *const u8),
        ("cheby_heap_sys_pop", cheby_heap_sys_pop as *const u8),
        (
            "cheby_heap_sys_from_list",
            cheby_heap_sys_from_list as *const u8,
        ),
        (
            "cheby_heap_sys_to_list",
            cheby_heap_sys_to_list as *const u8,
        ),
    ]
}

/// The kernels' C signatures, for the test that compares them with their
/// declarations in `std/heap.cheby`.
#[cfg(test)]
pub(crate) fn signatures() -> Vec<(&'static str, &'static [&'static str], &'static str)> {
    vec![
        (
            "cheby_heap_sys_push",
            &["Word", "Word", "bool", "Word"],
            "Word",
        ),
        ("cheby_heap_sys_pop", &["Word", "bool", "Word"], "Word"),
        (
            "cheby_heap_sys_from_list",
            &["Word", "bool", "Word"],
            "Word",
        ),
        ("cheby_heap_sys_to_list", &["Word", "bool", "Word"], "Word"),
    ]
}
