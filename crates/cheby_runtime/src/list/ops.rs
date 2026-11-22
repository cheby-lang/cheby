//! List operations on roots: the view logic over [`super::tree`].
//!
//! Ownership is in each signature's doc: "owned" arguments are consumed,
//! "borrowed" ones are not, and every returned root is owned. A unique
//! root is updated in place and returned; so is the tree under it, node
//! by node, where it is unique (D-274, §9.3).

use super::buffers;
use super::node::{BIG_ROOT_SIZE, M, MAX_NODE_SIZE, Node, Root, View, drop_word, dup_word, empty};
use super::tree;
use crate::abi::{self, Word};
use crate::stats::{self, Event};

/// The number of elements of a borrowed list.
pub(crate) fn len(list: Root) -> usize {
    list.len()
}

/// Element `index` of a borrowed list, borrowed, or `None` outside it.
/// A big root's head and tail leaves are read where they are, so a lookup
/// between two pushes leaves the tail leaf alone.
pub(crate) fn get(list: Root, index: usize) -> Option<Word> {
    if index >= list.len() {
        return None;
    }
    let Some(big) = list.big() else {
        let view = list.raw();
        return view
            .tree
            .map(|tree| tree::get(tree, view.offset.saturating_add(index)));
    };
    let pieces = big.pieces();
    let head_left = pieces.head_left();
    if index < head_left {
        return pieces
            .head
            .map(|head| head.elem(pieces.pos.saturating_add(index)));
    }
    let index = index.saturating_sub(head_left);
    let tree_len = pieces.tree_len();
    if index < tree_len {
        return pieces
            .tree
            .map(|tree| tree::get(tree, pieces.offset.saturating_add(index)));
    }
    pieces
        .tail
        .map(|tail| tail.elem(index.saturating_sub(tree_len)))
}

/// The bytes of `nodes` nodes of the largest size and a big root.
fn nodes_and_root(nodes: usize) -> u64 {
    let bytes = nodes
        .saturating_mul(MAX_NODE_SIZE)
        .saturating_add(BIG_ROOT_SIZE);
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

/// The height `h` of `list`'s tree once it holds the whole list, and the
/// most nodes that folding a big root's head and tail leaves into the
/// tree allocates ([`buffers::settle`]), 0 for a list that has neither.
/// A head leaf goes in like an [`append`]: its copy (1), the tree trimmed
/// at both ends (`2(h + 1)`), the leaf wrapped up to `h` (`h`), the seam
/// repacked at each level (`2M + 3`) and a new top (1). A tail leaf goes
/// in like a [`push`]: the cut of a part that ends early (`h + 1`), a copy
/// and a new sibling per level and a new top (`2h + 1`).
fn height_and_fold(list: Root) -> (usize, usize) {
    let h = list.raw().tree.map_or(0, Node::height);
    let Some(big) = list.big().filter(|big| big.has_buffers()) else {
        return (h, 0);
    };
    let h = h.saturating_add(1);
    let head = if big.head().is_some() {
        let per_level = M.saturating_mul(2).saturating_add(3);
        h.saturating_mul(per_level)
            .saturating_add(h.saturating_mul(3))
            .saturating_add(4)
    } else {
        0
    };
    let tail = if big.tail().is_some() {
        h.saturating_mul(3).saturating_add(2)
    } else {
        0
    };
    (h, head.saturating_add(tail))
}

/// The most bytes that folding `list`'s head and tail leaves into its
/// tree allocates, for the memory-budget check before the fold.
pub(super) fn fold_bound(list: Root) -> u64 {
    let (_, fold) = height_and_fold(list);
    u64::try_from(fold.saturating_mul(MAX_NODE_SIZE)).unwrap_or(u64::MAX)
}

// Upper bounds of what an update allocates, for the memory-budget check
// a runtime helper makes before it takes its owned arguments over (D-698,
// D-738, D-1760). A request a bound missed would only leak
// what the update held, if the budget then ran out halfway.

/// [`push`] onto `list` of height `h`: the cut of a view that ends early
/// (one node per level), a copy and a new sibling per level, a new top, a
/// new tail leaf and a root, after the fold of a big root that is not
/// unique: at most `3h + 5` nodes and the fold's. For a unique big root,
/// `h` counts the level its tail leaf may add.
pub(crate) fn push_bound(list: Root) -> u64 {
    let (h, fold) = height_and_fold(list);
    // A unique big root takes the element without the fold.
    let fold = if list.is_unique() { 0 } else { fold };
    nodes_and_root(h.saturating_mul(3).saturating_add(5).saturating_add(fold))
}

/// [`set`] of an element of `list` of height `h`: a copy of each node
/// on its path (`h + 1`) and a root, after the fold of a big root's head
/// and tail leaves.
pub(crate) fn set_bound(list: Root) -> u64 {
    let (h, fold) = height_and_fold(list);
    nodes_and_root(h.saturating_add(1).saturating_add(fold))
}

/// [`append`] of `a` and `b`, the taller of height `h`: both trimmed at
/// both ends (`4(h + 1)` nodes), the shorter wrapped up to `h` (`h`), the
/// merged leaves (1), at each branch level the seam's children repacked
/// and their new parents (`2M + 3`), a new top (1), and a root, after the
/// folds of big roots.
pub(crate) fn append_bound(a: Root, b: Root) -> u64 {
    let ((ha, fold_a), (hb, fold_b)) = (height_and_fold(a), height_and_fold(b));
    let h = ha.max(hb);
    let per_level = M.saturating_mul(2).saturating_add(3);
    let nodes = h
        .saturating_mul(per_level)
        .saturating_add(h.saturating_mul(5))
        .saturating_add(6)
        .saturating_add(fold_a)
        .saturating_add(fold_b);
    nodes_and_root(nodes)
}

/// [`prepend`] onto `list` of height `h`: the tree trimmed at both ends
/// (`2(h + 1)` nodes), then an element added in place or concatenated as
/// a new leaf (as [`append`]: the leaf wrapped up to `h`, the seam
/// repacked at each level, a new top), a new head leaf and a root, after
/// the fold of a big root that is not unique or whose head leaf is full.
pub(crate) fn prepend_bound(list: Root) -> u64 {
    let (h, fold) = height_and_fold(list);
    let per_level = M.saturating_mul(2).saturating_add(3);
    let nodes = h
        .saturating_mul(per_level)
        .saturating_add(h.saturating_mul(3))
        .saturating_add(5)
        .saturating_add(fold);
    nodes_and_root(nodes)
}

/// [`slice_owned`] and [`uncons_owned`] of `list` of height `h`: a cut
/// of a unique tree may copy the shared nodes along its two edges
/// ([`trimmed`]), a path each, a pattern step may copy one leaf that
/// another list holds, and a root, after the fold of a big root: at most
/// `2(h + 1) + 1` nodes and the fold's.
pub(crate) fn slice_owned_bound(list: Root) -> u64 {
    let (h, fold) = height_and_fold(list);
    nodes_and_root(
        h.saturating_add(1)
            .saturating_mul(2)
            .saturating_add(1)
            .saturating_add(fold),
    )
}

/// The most slots [`narrow`] clears one by one; a larger cut restructures
/// the tree instead. Below it, a pattern step writes one slot in place;
/// above it, cutting whole subtrees is cheaper than clearing each slot.
const CLEAR_MAX: usize = 4;

/// What a cleared slot holds: a tagged `Int`, which owns nothing (D-521).
/// It sits outside the view, so nothing reads it.
pub(super) const CLEARED: Word = match abi::tag_int(0) {
    Some(word) => word,
    None => panic!("0 is a tagged Int"),
};

/// `view` (its tree owned) narrowed to its elements `start..end`
/// (`start <= end <= view.len`), releasing the cut elements that only this
/// list holds (D-893, for D-205).
///
/// A cut of at most [`CLEAR_MAX`] elements overwrites each cut slot with
/// [`CLEARED`] where every node on its path is unique, and keeps the
/// offset: O(log n) per slot, no allocation, so a pattern step on a unique
/// list writes one slot in place. A slot under a shared node is left
/// alone: another list holds that node, and the element lives until the
/// last list sharing it dies. A larger cut of a unique tree restructures
/// it to hold exactly the view ([`trimmed`]), which releases whole
/// subtrees and copies at most the shared nodes on its two edges; a larger
/// cut of a shared tree keeps the view.
fn narrow(view: View, start: usize, end: usize) -> View {
    let len = end.saturating_sub(start);
    let Some(tree) = view.tree else {
        return View {
            tree: None,
            offset: 0,
            len: 0,
        };
    };
    if len == 0 {
        tree.release();
        return View {
            tree: None,
            offset: 0,
            len: 0,
        };
    }
    let offset = view.offset.saturating_add(start);
    let view_end = view.offset.saturating_add(view.len);
    let new_end = offset.saturating_add(len);
    let cut = start.saturating_add(view_end.saturating_sub(new_end));
    if cut <= CLEAR_MAX {
        for i in (view.offset..offset).chain(new_end..view_end) {
            tree::clear_if_unique(tree, i, CLEARED);
        }
    } else if tree.is_unique() {
        return View {
            tree: trimmed(View {
                tree: Some(tree),
                offset,
                len,
            }),
            offset: 0,
            len,
        };
    }
    View {
        tree: Some(tree),
        offset,
        len,
    }
}

/// The tree holding exactly the view's elements, owned (the view's tree
/// must be owned): cut at both ends where the view is narrower.
pub(super) fn trimmed(view: View) -> Option<Node> {
    let tree = view.tree?;
    if view.len == 0 {
        tree.release();
        return None;
    }
    let end = view.offset.saturating_add(view.len);
    let tree = if end < tree.len() {
        tree::take(tree, end)
    } else {
        tree
    };
    let tree = if view.offset > 0 {
        tree::drop_front(tree, view.offset)
    } else {
        tree
    };
    Some(tree::collapse(tree))
}

/// Applies `change` to an owned list's view, whose tree it receives owned.
/// A unique root is rewritten in place and returned (unless the result is
/// empty); a shared root gives up one reference and a new root is made.
#[expect(
    clippy::inline_always,
    reason = "a `[x, ..rest]` step runs it; once the slicing kernels shared it with the pattern-step helper, it was left out of line, which made a walk a third slower (D-894)"
)]
#[inline(always)]
fn update(list: Root, change: impl FnOnce(View) -> View) -> Root {
    if list.is_unique() {
        stats::record(Event::ListUpdateInPlace);
        let view = list.view();
        let tree = list.take_tree();
        let changed = change(View { tree, ..view });
        if changed.len == 0 {
            drop_word(list.word());
            return Root::new(changed);
        }
        list.store(changed);
        return list;
    }
    let view = list.view();
    if let Some(tree) = view.tree {
        tree.dup();
    }
    if view.len > 0 {
        stats::record(Event::ListUpdateCopied);
    }
    drop_word(list.word());
    Root::new(change(view))
}

/// `list` (owned) with the owned `elem` added at the end. In place when the
/// list is unique: amortized O(1) allocation per element, and O(1) time
/// but for one element in [`M`], which costs O(log n). A unique root whose
/// list outgrows its last leaf is replaced by a big root.
pub(crate) fn push(list: Root, elem: Word) -> Root {
    // A push onto an empty list builds a new root either way.
    if list.is_unique() {
        stats::record(Event::ListPushInPlace);
        if list.big().is_some() {
            return buffers::push(list, elem);
        }
        let view = list.raw();
        if let Some(tree) = view.tree
            && view.offset.saturating_add(view.len) == tree.len()
            && tree::last_leaf_full(tree)
        {
            if tree.is_leaf() && tree.is_unique() && spare_is_worth_a_move(view.offset, view.len) {
                // One full-size leaf whose free slots all lie before its
                // elements: the elements move towards the front, once for
                // several pushes.
                let offset = view.offset / 2;
                tree::slide(tree, view.offset, offset, CLEARED);
                tree.put_elem(offset.saturating_add(view.len), elem);
                tree.set_len(offset.saturating_add(view.len).saturating_add(1));
                list.set_offset(offset);
                list.set_len(view.len.saturating_add(1));
                return list;
            }
            // The list outgrows its last leaf: a tail leaf takes over.
            return buffers::push(list, elem);
        }
    } else if list.len() > 0 {
        stats::record(Event::ListPushCopied);
    }
    update(list, |view| {
        let Some(tree) = view.tree else {
            return View {
                tree: Some(tree::singleton(elem)),
                offset: 0,
                len: 1,
            };
        };
        // A view that ends before its tree does is cut there first, so the
        // element lands right after the view.
        let end = view.offset.saturating_add(view.len);
        let tree = if end < tree.len() {
            tree::take(tree, end)
        } else {
            tree
        };
        View {
            tree: Some(tree::push(tree, elem)),
            offset: view.offset,
            len: view.len.saturating_add(1),
        }
    })
}

/// Whether moving the `len` elements of a full-size leaf is worth its
/// `spare` free slots, which all lie at the end the list does not grow
/// at: when they are at least a quarter of the elements, so a move of
/// each element pays for a quarter of an addition or more. A list that
/// stays closer to a full leaf, such as a queue of 30 elements, would
/// move its elements at almost every step, and gets a big root instead
/// (D-1757).
fn spare_is_worth_a_move(spare: usize, len: usize) -> bool {
    spare.saturating_mul(4) >= len
}

/// `list` (owned) with the owned `elem` added at the front. In place when
/// the list is unique, as [`push`] is at the other end.
pub(crate) fn prepend(list: Root, elem: Word) -> Root {
    if list.is_unique() {
        if list.big().is_some() {
            stats::record(Event::ListUpdateInPlace);
            return buffers::prepend(list, elem);
        }
        let view = list.raw();
        if let Some(tree) = view.tree
            && view.offset == 0
        {
            if tree::first_leaf_full(tree) {
                // The list outgrows its first leaf: a head leaf takes
                // over.
                stats::record(Event::ListUpdateInPlace);
                return buffers::prepend(list, elem);
            }
            if tree.is_leaf() && tree.is_unique() && tree.slots() == view.len {
                // One unique leaf with no slot free before its first
                // element: the elements move towards the end of a leaf of
                // twice the capacity, or of this leaf once it has its
                // full size, once for several prepends.
                let free = M.saturating_sub(view.len);
                let moved = if tree.cap() < M {
                    list.take_tree();
                    Some(tree::gap_front(tree, CLEARED))
                } else if spare_is_worth_a_move(free, view.len) {
                    let first = free.saturating_sub(free / 2);
                    tree::slide(tree, 0, first, CLEARED);
                    Some((tree, first))
                } else {
                    None
                };
                if let Some((leaf, first)) = moved {
                    stats::record(Event::ListUpdateInPlace);
                    let before = first.saturating_sub(1);
                    leaf.put_elem(before, elem);
                    list.store(View {
                        tree: Some(leaf),
                        offset: before,
                        len: view.len.saturating_add(1),
                    });
                    return list;
                }
            }
        }
    }
    update(list, |view| {
        let len = view.len.saturating_add(1);
        let tree = match trimmed(view) {
            Some(rest) => tree::prepend(rest, elem),
            None => tree::singleton(elem),
        };
        View {
            tree: Some(tree),
            offset: 0,
            len,
        }
    })
}

/// `list` (owned) with element `index` replaced by the owned `elem`, or
/// unchanged (and `elem` dropped) when `index` is outside it (D-318).
/// O(log n); in place along the path where the tree is unique.
pub(crate) fn set(list: Root, index: usize, elem: Word) -> Root {
    if index >= list.len() {
        drop_word(elem);
        return list;
    }
    update(list, |view| match view.tree {
        Some(tree) => View {
            tree: Some(tree::set(tree, view.offset.saturating_add(index), elem)),
            ..view
        },
        None => view,
    })
}

/// The elements of `a` (owned) followed by those of `b` (borrowed).
/// O(log n) in the combined length.
pub(crate) fn append(a: Root, b: Root) -> Root {
    let bview = b.view();
    if bview.len == 0 {
        return a;
    }
    // Both lists' leaves are folded before anything is held.
    if a.view().len == 0 {
        drop_word(a.word());
        dup_word(b.word());
        return b;
    }
    // Take `b`'s tree before `a` changes: they may be the same root.
    if let Some(tree) = bview.tree {
        tree.dup();
    }
    let second = trimmed(bview);
    update(a, |view| {
        let len = view.len.saturating_add(bview.len);
        let tree = match (trimmed(view), second) {
            (Some(first), Some(second)) => Some(tree::concat(first, second)),
            (first, second) => first.or(second),
        };
        View {
            tree,
            offset: 0,
            len,
        }
    })
}

/// Elements `start..end` of a borrowed list, as a new view sharing its
/// tree: O(1) (D-053). Both bounds are clamped to `0..=len`, and the
/// result is empty when `end <= start` (D-298). The list it was cut from
/// still holds the tree, so nothing is released here; an element outside
/// the view lives until the last list sharing its node dies (D-893).
pub(crate) fn slice(list: Root, start: usize, end: usize) -> Root {
    let view = list.view();
    let end = end.min(view.len);
    let start = start.min(end);
    if start == 0 && end == view.len {
        dup_word(list.word());
        return list;
    }
    if start == end {
        return Root::new(View {
            tree: None,
            offset: 0,
            len: 0,
        });
    }
    if let Some(tree) = view.tree {
        tree.dup();
    }
    Root::new(View {
        tree: view.tree,
        offset: view.offset.saturating_add(start),
        len: end.saturating_sub(start),
    })
}

/// Elements `start..end` of an owned list, which is consumed: the same view
/// as [`slice`], but a unique root is changed in place instead of a new
/// one being allocated (and the old one freed), and the cut elements that
/// only this list holds are released ([`narrow`]). So a pattern step on a
/// unique list (`[x, ..rest]`) writes one slot and allocates nothing
/// (D-634, D-893). O(log n).
#[expect(
    clippy::inline_always,
    reason = "as `update`: the pattern-step helper and the slicing kernels both call it, and out of line it made a `[x, ..rest]` walk a third slower (D-894)"
)]
#[inline(always)]
pub(crate) fn slice_owned(list: Root, start: usize, end: usize) -> Root {
    let len = list.len();
    let end = end.min(len);
    let start = start.min(end);
    if start == 0 && end == len {
        return list;
    }
    update(list, |view| narrow(view, start, end))
}

/// Element `at` of a borrowed list (below its length), dup'd, and the
/// view of elements `start..end`, clamped as by [`slice`]: a list
/// pattern's element and spread in one call (D-909). The view is made
/// first, so when its root cannot be allocated nothing is held.
pub(crate) fn uncons(list: Root, at: usize, start: usize, end: usize) -> (Word, Root) {
    let rest = slice(list, start, end);
    let head = get(list, at).unwrap_or_else(|| {
        crate::panic::fatal(format_args!(
            "list index {at} out of range for length {}",
            list.len()
        ))
    });
    dup_word(head);
    (head, rest)
}

/// [`uncons`] of an owned list, which is consumed: element `at`, owned,
/// and [`slice_owned`] of `start..end` (D-909). When the slice is the list
/// without `at`, at either end, and the root is unique, one descent moves
/// the element out of its slot where every node on the path is unique (no
/// `dup` and no `drop`: D-893's release of the cut element, done by a
/// move) or dups it under a shared node, and the root becomes the view in
/// place (D-634). Any other shape, or a shared root, is [`get`] and
/// [`slice_owned`]. O(log n). The caller has run [`slice_owned_bound`]'s
/// preflight, so nothing here raises.
#[expect(
    clippy::inline_always,
    reason = "the pattern-step helper's only body, as `slice_owned` is (D-894)"
)]
#[inline(always)]
pub(crate) fn uncons_owned(list: Root, at: usize, start: usize, end: usize) -> (Word, Root) {
    let len = list.len();
    let end = end.min(len);
    let start = start.min(end);
    let front = at == 0 && start == 1 && end == len;
    let back = start == 0 && end == at && at.saturating_add(1) == len;
    if (front || back) && len > 1 && list.is_unique() {
        // SAFETY: a live list (module invariant of `Root`).
        let fast = unsafe {
            if front {
                buffers::uncons_front_fast(list.word())
            } else {
                buffers::uncons_back_fast(list.word())
            }
        };
        return match fast {
            Some(elem) => (elem, list),
            None => buffers::uncons(list, front),
        };
    }
    let head = get(list, at).unwrap_or_else(|| {
        crate::panic::fatal(format_args!(
            "list index {at} out of range for length {len}"
        ))
    });
    dup_word(head);
    (head, slice_owned(list, start, end))
}

/// A count or index argument clamped to `0..=len` (D-298).
pub(crate) fn clamp(n: i64, len: usize) -> usize {
    usize::try_from(n).map_or(0, |n| n.min(len))
}

/// The elements of a list, in order, borrowed. Built for the runtime's
/// other modules (equality, hashing and debug text in W3-B, conversions
/// such as `map::from_list`).
///
/// The iterator holds a reference to the list, so the elements it yields
/// stay valid while it lives; a caller that keeps an element longer must
/// dup it. O(1) amortized per element: it walks the tree with an explicit
/// stack, never recursion.
#[derive(Debug)]
pub(crate) struct Iter {
    root: Word,
    /// The branches above the current leaf, each with the index of the
    /// next child to visit.
    stack: Vec<(Node, usize)>,
    /// The current leaf and the index of its next element.
    leaf: Option<(Node, usize)>,
    remaining: usize,
}

impl Iter {
    /// Iterates over a borrowed list.
    pub(crate) fn new(list: Root) -> Self {
        // The view first: folding a big root's leaves may raise.
        let view = list.view();
        dup_word(list.word());
        let mut stack = Vec::new();
        let leaf = view.tree.filter(|_| view.len > 0).map(|tree| {
            let (mut node, mut i) = (tree, view.offset);
            while !node.is_leaf() {
                let (j, before) = node.locate(i);
                stack.push((node, j.saturating_add(1)));
                node = node.child(j);
                i = i.saturating_sub(before);
            }
            (node, i)
        });
        Self {
            root: list.word(),
            stack,
            leaf,
            remaining: view.len,
        }
    }

    /// Moves to the first leaf after the current one.
    fn next_leaf(&mut self) -> Option<(Node, usize)> {
        loop {
            let (branch, next) = self.stack.pop()?;
            if next < branch.slots() {
                self.stack.push((branch, next.saturating_add(1)));
                let mut node = branch.child(next);
                while !node.is_leaf() {
                    self.stack.push((node, 1));
                    node = node.child(0);
                }
                return Some((node, 0));
            }
        }
    }
}

impl Iterator for Iter {
    type Item = Word;

    fn next(&mut self) -> Option<Word> {
        if self.remaining == 0 {
            return None;
        }
        loop {
            let (leaf, pos) = self.leaf?;
            if pos < leaf.slots() {
                self.leaf = Some((leaf, pos.saturating_add(1)));
                self.remaining = self.remaining.saturating_sub(1);
                return Some(leaf.elem(pos));
            }
            self.leaf = self.next_leaf();
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for Iter {}

impl Drop for Iter {
    fn drop(&mut self) {
        drop_word(self.root);
    }
}

/// A list of the given owned elements, in order. Pushes in place, so the
/// cost is amortized O(1) allocation per element.
pub(crate) fn from_iter(items: impl IntoIterator<Item = Word>) -> Root {
    // SAFETY: the static empty list is a live list.
    let empty = unsafe { Root::from_word(empty()) };
    let list = items.into_iter().fold(empty, push);
    // A list the runtime builds may be made static or shared without the
    // walk that folds a big root's leaves into its tree, so it leaves
    // here with them folded.
    if let Some(big) = list.big() {
        buffers::settle(big);
    }
    list
}
