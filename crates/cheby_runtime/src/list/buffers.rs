//! The head and tail leaves of big roots, and the fast paths of the list
//! operations that work at an end of a list: `push`, `prepend` and the
//! pattern steps `[x, ..rest]` and `[..rest, x]` (D-1757).
//!
//! A list of up to [`M`] elements is one leaf under a root, and these
//! operations read and write that leaf directly. A longer list would need
//! a descent of its tree for each of them, so a unique root of such a
//! list is replaced by a big root, which also owns a head leaf and a tail
//! leaf outside the tree ([`Pieces`]). A push writes the tail leaf and a
//! step from the front reads the head leaf; the tree is touched once per
//! leaf, when a full tail leaf goes into it or the next head leaf comes
//! out of it, where it leaves a hole (module `node`). Every other
//! operation sees a big root through [`Root::view`], which folds both
//! leaves back into the tree and removes its holes first ([`settle`]).
//!
//! A big root has a head or tail leaf only while its count is positive,
//! so only one fiber reads it: a value is settled before it is shared
//! with another fiber or made static ([`settle_value`]).
//!
//! The fast paths ([`push_fast`], [`prepend_fast`], [`uncons_front_fast`]
//! and [`uncons_back_fast`]) run on a root whose count is one, allocate
//! nothing and raise nothing, so the C entry points call them before
//! their guard. Everything else here may allocate. It works on the pieces
//! after moving them out of the root ([`Big::detach`]), so an allocation
//! that fails halfway leaks them and leaves a root that can be released
//! (D-738, D-1760).

use super::node::{Big, Fast, M, Node, Pieces, Root, View, drop_word, dup_word};
use super::ops::{CLEARED, trimmed};
use super::tree;
use crate::abi::{self, Word};
use crate::panic::fatal;
use crate::stats::{self, Event};

impl Pieces {
    /// Releases the tree when the list takes no element from it any more:
    /// the pieces hold a tree only while it holds a part of the list.
    fn drop_dead_tree(&mut self) {
        if self.tree_len() == 0 {
            if let Some(dead) = self.tree.take() {
                dead.release();
            }
            self.offset = 0;
        }
    }

    /// The tree holding exactly the elements the list takes from it,
    /// moved out: `None` when there are none.
    fn take_tree_part(&mut self) -> Option<Node> {
        let view = View {
            tree: self.tree.take(),
            offset: self.offset,
            len: self.tree_len(),
        };
        self.offset = 0;
        trimmed(view)
    }

    /// Puts the head leaf's elements back at the front of the tree.
    fn fold_head(&mut self) {
        let Some(head) = self.head else {
            return;
        };
        let rest = self.take_tree_part();
        // The leaf without the slots before the position: in place when
        // it is unique, a copy of the rest otherwise.
        let front = tree::drop_front(head, self.pos);
        self.head = None;
        self.pos = 0;
        self.tree = Some(match rest {
            Some(rest) => tree::prepend_leaf(rest, front),
            None => front,
        });
    }

    /// Puts the tail leaf at the end of the tree.
    fn fold_tail(&mut self) {
        let Some(tail) = self.tail else {
            return;
        };
        let end = self.offset.saturating_add(self.tree_len());
        self.tail = None;
        self.tree = Some(match self.tree {
            Some(tree) => {
                let tree = if end < tree.len() {
                    tree::take(tree, end)
                } else {
                    tree
                };
                tree::push_leaf(tree, tail)
            }
            None => tail,
        });
    }

    /// Folds both leaves into the tree, which then holds the whole list,
    /// with no holes.
    fn settle(&mut self) {
        self.close_holes();
        self.fold_head();
        self.fold_tail();
    }

    /// Removes the tree's holes ([`Self::refill_head`]), and moves the
    /// offset back by the elements they counted. In place, with no
    /// allocation.
    fn close_holes(&mut self) {
        let Some(tree) = self.tree.filter(|_| self.offset > 0) else {
            return;
        };
        let gone = tree::close_holes(tree);
        if gone > 0 {
            self.offset = self.offset.saturating_sub(gone);
            self.tree = Some(tree::collapse(tree));
        }
    }

    /// Starts a tail leaf of capacity `cap` with the owned `elem`.
    fn start_tail(&mut self, elem: Word, cap: usize) {
        let leaf = Node::new_leaf(cap);
        leaf.put_elem(0, elem);
        leaf.set_len(1);
        self.tail = Some(leaf);
        self.len = self.len.saturating_add(1);
    }

    /// Adds the owned `elem` at the end. The tail leaf takes it; a full
    /// one goes into the tree first. A list without a tail leaf starts
    /// one only after a full leaf, so the tree's leaves stay full, and
    /// otherwise the element goes into the tree's last leaf.
    fn push(&mut self, elem: Word) {
        if let Some(tail) = self.tail {
            let n = tail.slots();
            if n < M {
                // Room, or a leaf that grows by doubling.
                let leaf = tree::leaf_with_room(tail, n.saturating_add(1));
                leaf.put_elem(n, elem);
                leaf.set_len(n.saturating_add(1));
                self.tail = Some(leaf);
                self.len = self.len.saturating_add(1);
                return;
            }
            self.fold_tail();
            self.start_tail(elem, M);
            return;
        }
        let Some(tree) = self.tree else {
            // The head leaf holds the whole list.
            self.start_tail(elem, 1);
            return;
        };
        let end = self.offset.saturating_add(self.tree_len());
        if end == tree.len() && tree::last_leaf_full(tree) {
            self.start_tail(elem, M);
            return;
        }
        let tree = if end < tree.len() {
            tree::take(tree, end)
        } else {
            tree
        };
        self.tree = Some(tree::push(tree, elem));
        self.len = self.len.saturating_add(1);
    }

    /// Starts a head leaf with the owned `elem` in its last slot, so the
    /// slots before it take the next prepends.
    fn start_head(&mut self, elem: Word) {
        let last = M.saturating_sub(1);
        let leaf = Node::new_leaf(M);
        for i in 0..last {
            leaf.put_elem(i, CLEARED);
        }
        leaf.put_elem(last, elem);
        leaf.set_len(M);
        self.head = Some(leaf);
        self.pos = last;
        self.len = self.len.saturating_add(1);
    }

    /// Adds the owned `elem` at the front: the mirror of [`Self::push`].
    fn prepend(&mut self, elem: Word) {
        if let Some(head) = self.head {
            if head.is_unique()
                && let Some(before) = self.pos.checked_sub(1)
            {
                let old = head.elem(before);
                head.put_elem(before, elem);
                drop_word(old);
                self.pos = before;
                self.len = self.len.saturating_add(1);
                return;
            }
            // A leaf with no slot free, or one another list holds.
            self.fold_head();
        }
        let Some(tree) = self.tree else {
            // The tail leaf holds the whole list.
            self.start_head(elem);
            return;
        };
        if self.offset == 0 && tree::first_leaf_full(tree) {
            self.start_head(elem);
            return;
        }
        let rest = self.take_tree_part();
        self.tree = Some(match rest {
            Some(rest) => tree::prepend(rest, elem),
            None => tree::singleton(elem),
        });
        self.len = self.len.saturating_add(1);
    }

    /// Gives a list without a head leaf one, holding its first elements.
    /// The list is not empty.
    ///
    /// When the tree's first leaf is unique along its whole path, the leaf
    /// moves out of the tree, so the elements a step then takes from it
    /// are released from the list (D-893): with no slot moved where
    /// [`Self::take_head_leaf`] applies, and otherwise after the tree is cut
    /// to the list's part. When another list holds a node on that path,
    /// the leaf is shared instead and the tree stays whole.
    fn refill_head(&mut self) {
        let tree_len = self.tree_len();
        let Some(tree) = self.tree else {
            // The tail leaf holds the whole list: it becomes the head.
            self.head = self.tail.take();
            self.pos = 0;
            return;
        };
        if self.take_head_leaf() {
            return;
        }
        // The leaf holding the first element, and whether its path is
        // unique.
        let (mut node, mut i) = (tree, self.offset);
        let mut unique = node.is_unique();
        while !node.is_leaf() {
            let (j, before) = node.locate(i);
            node = node.child(j);
            i = i.saturating_sub(before);
            unique = unique && node.is_unique();
        }
        if unique {
            // Cut the tree to the list's part of it, then take the first
            // leaf out: all in place along a unique path.
            let Some(part) = self.take_tree_part() else {
                fatal(format_args!("a list lost the tree of its elements"));
            };
            let (leaf, rest) = tree::pop_front_leaf(part);
            self.tree = rest;
            self.head = Some(leaf);
            self.pos = 0;
            return;
        }
        let in_leaf = node.slots().saturating_sub(i);
        if in_leaf <= tree_len {
            // Another list holds the leaf: share it, and move past it.
            self.head = Some(node.dup());
            self.pos = i;
            self.offset = self.offset.saturating_add(in_leaf);
            self.drop_dead_tree();
            return;
        }
        // The list's part of the tree ends inside this shared leaf: copy
        // that part.
        let leaf = Node::new_leaf(tree_len);
        for k in 0..tree_len {
            let elem = node.elem(i.saturating_add(k));
            dup_word(elem);
            leaf.put_elem(k, elem);
        }
        leaf.set_len(tree_len);
        self.head = Some(leaf);
        self.pos = 0;
        self.drop_dead_tree();
    }

    /// Gives a list without a head leaf the tree's first leaf as its head
    /// leaf, when the tree holds only the list's part after any holes, the
    /// list's part starts at that leaf's first element, and the path to
    /// the leaf is unique: the leaf moves out and leaves a hole in its slot
    /// (module `node`), with no slot moved and no size changed. Returns
    /// whether it did; nothing changes otherwise. Allocates nothing and
    /// raises nothing, so the fast path of a step runs it too
    /// ([`refill_head_in_place`]).
    fn take_head_leaf(&mut self) -> bool {
        let Some(tree) = self.tree else {
            return false;
        };
        if self.offset.saturating_add(self.tree_len()) != tree.len() {
            // The tree holds elements past the list's end.
            return false;
        }
        // The path to the leaf holding the first element: each branch with
        // the slot of the child taken.
        let mut path = [(tree, 0_usize); tree::MAX_DEPTH];
        let (mut node, mut i) = (tree, self.offset);
        let mut depth = 0;
        let mut height = tree.height();
        while height > 0 {
            if !node.is_unique() {
                return false;
            }
            let (j, before) = node.locate_at(i, height);
            // Holes come first in a branch, so the child before is enough
            // to tell that every child before the path is one.
            if j.checked_sub(1).is_some_and(|prev| !node.is_hole(prev)) {
                return false;
            }
            let Some(step) = path.get_mut(depth) else {
                return false;
            };
            *step = (node, j);
            depth = depth.saturating_add(1);
            node = node.child(j);
            i = i.saturating_sub(before);
            height = height.saturating_sub(1);
        }
        if i != 0 || !node.is_unique() {
            return false;
        }
        match path.get(..depth) {
            Some(path) if !path.is_empty() => {
                // A top that lost its last child holds only holes, and is
                // released below, with the tree's part of the list gone.
                tree::leave_hole(path);
                self.offset = self.offset.saturating_add(node.slots());
            }
            _ => {
                // The tree is the leaf.
                self.tree = None;
                self.offset = 0;
            }
        }
        self.head = Some(node);
        self.pos = 0;
        self.drop_dead_tree();
        true
    }

    /// Gives a list without a tail leaf one, holding its last elements:
    /// the mirror of [`Self::refill_head`], except that a tail leaf is
    /// always unique, so a leaf another list holds is copied.
    fn refill_tail(&mut self) {
        let tree_len = self.tree_len();
        let Some(tree) = self.tree else {
            // The head leaf holds the whole list: it becomes the tail,
            // with its elements from slot 0.
            if let Some(head) = self.head.take() {
                self.tail = Some(tree::drop_front(head, self.pos));
            }
            self.pos = 0;
            return;
        };
        let end = self.offset.saturating_add(tree_len);
        // The leaf holding the last element, and whether its path is
        // unique.
        let (mut node, mut i) = (tree, end.saturating_sub(1));
        let mut unique = node.is_unique();
        while !node.is_leaf() {
            let (j, before) = node.locate(i);
            node = node.child(j);
            i = i.saturating_sub(before);
            unique = unique && node.is_unique();
        }
        if unique {
            let Some(part) = self.take_tree_part() else {
                fatal(format_args!("a list lost the tree of its elements"));
            };
            let (rest, leaf) = tree::pop_back_leaf(part);
            self.tree = rest;
            self.tail = Some(leaf);
            return;
        }
        // Another list holds the leaf: copy the list's part of it.
        let count = i.saturating_add(1).min(tree_len);
        let first = i.saturating_add(1).saturating_sub(count);
        let leaf = Node::new_leaf(count);
        for k in 0..count {
            let elem = node.elem(first.saturating_add(k));
            dup_word(elem);
            leaf.put_elem(k, elem);
        }
        leaf.set_len(count);
        self.tail = Some(leaf);
        self.drop_dead_tree();
    }

    /// Takes the first element out of the head leaf, which the list has.
    fn take_front(&mut self) -> Word {
        let Some(head) = self.head else {
            fatal(format_args!("a list step found no head leaf"));
        };
        let elem = head.elem(self.pos);
        if head.is_unique() {
            head.put_elem(self.pos, CLEARED);
        } else {
            dup_word(elem);
        }
        self.pos = self.pos.saturating_add(1);
        if self.pos >= head.slots() {
            self.head = None;
            self.pos = 0;
            head.release();
        }
        self.len = self.len.saturating_sub(1);
        elem
    }

    /// Takes the last element out of the tail leaf, which the list has.
    fn take_back(&mut self) -> Word {
        let Some(tail) = self.tail else {
            fatal(format_args!("a list step found no tail leaf"));
        };
        let last = tail.slots().saturating_sub(1);
        let elem = tail.elem(last);
        tail.set_len(last);
        if last == 0 {
            self.tail = None;
            tail.release();
        }
        self.len = self.len.saturating_sub(1);
        elem
    }
}

/// Folds a big root's head and tail leaves into its tree and removes its
/// holes, in place, so the tree holds the whole list again. The list's
/// value does not change, so this is done on a root that other references
/// hold too, as long as they are all on this fiber (a positive count).
pub(super) fn settle(list: Big) {
    if !list.has_buffers() && !has_holes(list) {
        return;
    }
    if !list.root().is_local() {
        fatal(format_args!(
            "a shared or static list root holds a head or tail leaf"
        ));
    }
    // The memory limit of constant evaluation is checked for the whole
    // fold first, so running out of it leaves the list as it is (D-1760).
    crate::entry::check_budget(super::ops::fold_bound(list.root()));
    let mut pieces = list.detach();
    pieces.settle();
    list.attach(pieces);
}

/// Whether a big root's tree holds holes ([`Pieces::refill_head`]): they
/// count elements before the offset.
fn has_holes(list: Big) -> bool {
    let view = list.root().raw();
    view.offset > 0 && view.tree.is_some_and(tree::has_holes)
}

/// [`settle`] for a value about to be shared with another fiber or made
/// static, when it is a big root.
///
/// # Safety
///
/// `word` is a tagged integer, 0, or a live object that only this fiber
/// holds.
pub(crate) unsafe fn settle_value(word: Word) {
    if abi::is_tagged_int(word) || word == 0 {
        return;
    }
    // SAFETY: a live object.
    let (_, id) = unsafe { crate::object::scan_and_layout(crate::object::word_to_ptr(word)) };
    if id != super::node::BIG_ROOT {
        return;
    }
    // SAFETY: a live big root, by its layout.
    let root = unsafe { Root::from_word(word) };
    if let Some(big) = root.big()
        && root.is_local()
    {
        settle(big);
    }
}

/// Runs `change` on the pieces of a unique, owned root and returns its
/// result with the list: a big root is changed in place; a root's
/// contents move to a new big root, and the root is freed. `change` must
/// leave the list with an element.
fn with_pieces<T>(list: Root, change: impl FnOnce(&mut Pieces) -> T) -> (T, Root) {
    if let Some(big) = list.big() {
        let mut pieces = big.detach();
        let out = change(&mut pieces);
        big.attach(pieces);
        return (out, list);
    }
    let view = list.raw();
    list.store(View {
        tree: None,
        offset: 0,
        len: 0,
    });
    drop_word(list.word());
    let mut pieces = Pieces::of(view);
    let out = change(&mut pieces);
    (out, Big::new(pieces).root())
}

/// The unique root `list` with the owned `elem` added at its end, through
/// a tail leaf: a big root in place, and a root by moving its contents to
/// a big root ([`with_pieces`]).
pub(super) fn push(list: Root, elem: Word) -> Root {
    with_pieces(list, |pieces| pieces.push(elem)).1
}

/// The unique root `list` with the owned `elem` added at its front,
/// through a head leaf: the mirror of [`push`].
pub(super) fn prepend(list: Root, elem: Word) -> Root {
    with_pieces(list, |pieces| pieces.prepend(elem)).1
}

/// The first (`front`) or last element of the unique, owned root `list`,
/// owned, and the list without it, through a head or tail leaf. The list
/// has more than one element, and [`uncons_front_fast`] or
/// [`uncons_back_fast`] has declined it: the leaf it needs is still in
/// the tree.
pub(super) fn uncons(list: Root, front: bool) -> (Word, Root) {
    if list.len() <= 1 {
        fatal(format_args!(
            "a pattern step through a head or tail leaf would empty its list"
        ));
    }
    stats::record(Event::ListUpdateInPlace);
    with_pieces(list, |pieces| {
        if front {
            if pieces.head.is_none() {
                pieces.refill_head();
            }
            pieces.take_front()
        } else {
            if pieces.tail.is_none() {
                pieces.refill_tail();
            }
            pieces.take_back()
        }
    })
}

// ---------------------------------------------------------------------
// The fast paths

/// Stores the owned `elem` at the end of the list `word` and returns
/// true, when the list's count is one and the leaf at its end is unique
/// and has room: the tail leaf of a big root, or the leaf of a root whose
/// view ends where the leaf does. Changes nothing otherwise.
///
/// # Safety
///
/// `word` is a live list value.
#[expect(
    clippy::inline_always,
    reason = "the whole body of a push that needs no allocation; out of line it costs a second call per element"
)]
#[inline(always)]
pub(crate) unsafe fn push_fast(word: Word, elem: Word) -> bool {
    // SAFETY: forwarded from the caller.
    match unsafe { Root::fast(word) } {
        Some(Fast::Small(list)) => {
            let view = list.raw();
            let Some(leaf) = view.tree else {
                return false;
            };
            if !leaf.is_leaf() || !leaf.is_unique() {
                return false;
            }
            let (n, cap) = leaf.len_cap();
            if view.offset.saturating_add(view.len) != n || n >= cap {
                return false;
            }
            leaf.put_elem(n, elem);
            leaf.put_len_cap(n.saturating_add(1), cap);
            list.set_len(view.len.saturating_add(1));
        }
        Some(Fast::Big(big)) => {
            let Some(tail) = big.tail() else {
                return false;
            };
            // A tail leaf is unique ([`Pieces::tail`]).
            let (n, cap) = tail.len_cap();
            if n >= cap {
                return false;
            }
            tail.put_elem(n, elem);
            tail.put_len_cap(n.saturating_add(1), cap);
            let list = big.root();
            list.set_len(list.len().saturating_add(1));
        }
        None => return false,
    }
    stats::record(Event::ListPushInPlace);
    true
}

/// Stores the owned `elem` at the front of the list `word` and returns
/// true, when the list's count is one and the leaf at its front is unique
/// and has a slot free before the first element: the head leaf of a big
/// root, or the leaf of a root. Changes nothing otherwise. What the slot
/// held, an element outside the list, is released.
///
/// # Safety
///
/// `word` is a live list value.
#[expect(clippy::inline_always, reason = "as `push_fast`")]
#[inline(always)]
pub(crate) unsafe fn prepend_fast(word: Word, elem: Word) -> bool {
    // SAFETY: forwarded from the caller.
    let (leaf, pos) = match unsafe { Root::fast(word) } {
        Some(Fast::Small(list)) => {
            let view = list.raw();
            let (Some(leaf), Some(before)) = (view.tree, view.offset.checked_sub(1)) else {
                return false;
            };
            if !leaf.is_leaf() || !leaf.is_unique() {
                return false;
            }
            list.set_offset(before);
            list.set_len(view.len.saturating_add(1));
            (leaf, before)
        }
        Some(Fast::Big(big)) => {
            let (Some(head), Some(before)) = (big.head(), big.head_pos().checked_sub(1)) else {
                return false;
            };
            if !head.is_unique() {
                return false;
            }
            big.set_head_pos(before);
            let list = big.root();
            list.set_len(list.len().saturating_add(1));
            (head, before)
        }
        None => return false,
    };
    let old = leaf.elem(pos);
    leaf.put_elem(pos, elem);
    drop_word(old);
    stats::record(Event::ListUpdateInPlace);
    true
}

/// The first element of the list `word`, owned, when the list's count is
/// one, it has more than one element, and the first is in the head leaf of
/// a big root or in the leaf of a root; the list is then the rest. The
/// element is moved out of a unique leaf and dup'd under a leaf that
/// another list holds (D-893, D-909). `None`, with nothing changed,
/// otherwise.
///
/// # Safety
///
/// `word` is a live list value.
pub(crate) unsafe fn uncons_front_fast(word: Word) -> Option<Word> {
    // SAFETY: forwarded from the caller.
    front_fast(unsafe { Root::fast(word) }?)
}

/// [`uncons_front_fast`] on a unique root.
#[expect(
    clippy::inline_always,
    reason = "as `uncons_fast`, which is all it serves on the hot path"
)]
#[inline(always)]
fn front_fast(list: Fast) -> Option<Word> {
    let elem = match list {
        Fast::Small(list) => {
            let view = list.raw();
            let leaf = view.tree.filter(|tree| view.len > 1 && tree.is_leaf())?;
            let elem = leaf.elem(view.offset);
            if leaf.is_unique() {
                leaf.put_elem(view.offset, CLEARED);
            } else {
                dup_word(elem);
            }
            list.set_offset(view.offset.saturating_add(1));
            list.set_len(view.len.saturating_sub(1));
            elem
        }
        Fast::Big(big) => {
            let list = big.root();
            let len = list.len();
            let head = big.head().filter(|_| len > 1)?;
            let pos = big.head_pos();
            let elem = head.elem(pos);
            if head.is_unique() {
                head.put_elem(pos, CLEARED);
            } else {
                dup_word(elem);
            }
            let next = pos.saturating_add(1);
            list.set_len(len.saturating_sub(1));
            if next >= head.slots() {
                big.set_head(None, 0);
                head.release();
                refill_head_in_place(big);
            } else {
                big.set_head_pos(next);
            }
            elem
        }
    };
    stats::record(Event::ListUpdateInPlace);
    Some(elem)
}

/// Gives a unique big root whose head leaf a step used up the tree's next
/// leaf as its head leaf, when [`Pieces::take_head_leaf`] applies, so the
/// next step finds it in place. Changes nothing otherwise. The pieces
/// stay in the root while they change: nothing in between allocates or
/// raises. Out of line, since a step runs it once per leaf.
#[inline(never)]
fn refill_head_in_place(big: Big) {
    let mut pieces = big.pieces();
    if pieces.take_head_leaf() {
        big.attach(pieces);
    }
}

/// The last element of the list `word`, owned, when the list's count is
/// one, it has more than one element, and the last is in the tail leaf of
/// a big root or in the leaf of a root; the list is then the rest: the
/// mirror of [`uncons_front_fast`].
///
/// # Safety
///
/// `word` is a live list value.
pub(crate) unsafe fn uncons_back_fast(word: Word) -> Option<Word> {
    // SAFETY: forwarded from the caller.
    back_fast(unsafe { Root::fast(word) }?)
}

/// The element of the pattern step that reads element `at` of the list
/// `word` and leaves its elements `start..end`, when that is the list's
/// first element and the rest ([`uncons_front_fast`]) or its last and the
/// rest ([`uncons_back_fast`]) and the fast path serves it. `None`, with
/// nothing changed, otherwise. The bounds are the helper's own, not yet
/// clamped.
///
/// # Safety
///
/// `word` is a live list value.
#[expect(
    clippy::inline_always,
    reason = "the whole body of a pattern step that needs no descent; out of line it costs a second call per element"
)]
#[inline(always)]
pub(crate) unsafe fn uncons_fast(word: Word, at: i64, start: i64, end: i64) -> Option<Word> {
    // SAFETY: forwarded from the caller.
    let list = unsafe { Root::fast(word) }?;
    let len = i64::try_from(list.root().len()).ok()?;
    if at == 0 && start == 1 && end >= len {
        front_fast(list)
    } else if start <= 0 && end == at && at.checked_add(1) == Some(len) {
        back_fast(list)
    } else {
        None
    }
}

/// [`uncons_back_fast`] on a unique root.
#[expect(
    clippy::inline_always,
    reason = "as `uncons_fast`, which is all it serves on the hot path"
)]
#[inline(always)]
fn back_fast(list: Fast) -> Option<Word> {
    let elem = match list {
        Fast::Small(list) => {
            let view = list.raw();
            let leaf = view.tree.filter(|tree| view.len > 1 && tree.is_leaf())?;
            let at = view.offset.saturating_add(view.len).saturating_sub(1);
            let elem = leaf.elem(at);
            if !leaf.is_unique() {
                dup_word(elem);
            } else if at.saturating_add(1) == leaf.slots() {
                // The leaf's last slot: free again for a push.
                leaf.set_len(at);
            } else {
                leaf.put_elem(at, CLEARED);
            }
            list.set_len(view.len.saturating_sub(1));
            elem
        }
        Fast::Big(big) => {
            let list = big.root();
            let len = list.len();
            // A tail leaf is unique ([`Pieces::tail`]).
            let tail = big.tail().filter(|_| len > 1)?;
            let (n, cap) = tail.len_cap();
            let at = n.saturating_sub(1);
            let elem = tail.elem(at);
            tail.put_len_cap(at, cap);
            if at == 0 {
                big.set_tail(None);
                tail.release();
            }
            list.set_len(len.saturating_sub(1));
            elem
        }
    };
    stats::record(Event::ListUpdateInPlace);
    Some(elem)
}
