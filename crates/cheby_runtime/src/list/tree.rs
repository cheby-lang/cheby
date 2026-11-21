//! Operations on RRB trees of uniform words (D-039, ADR-0018).
//!
//! Every function that changes a tree takes owned node references and
//! returns owned ones. A node with count one is changed in place; a shared
//! node is copied first ([`make_unique`]), so only the path that changes
//! is copied and the rest stays shared. Recursion follows the tree's
//! height, which is logarithmic in its length (at most 13 levels for
//! 2^64 elements), never the data.
//!
//! Concatenation follows Bagwell and Rompf's RRB trees: the two trees are
//! merged along the seam, level by level, and the nodes next to the seam
//! are repacked when they hold more than [`EXTRA`] nodes beyond the
//! fewest that could hold their slots. That keeps every level close to
//! full, so the height stays O(log n) and a concatenation touches O(log n)
//! nodes of at most [`M`] slots each.

use super::node::{M, Node, drop_word, dup_word};
use crate::abi::Word;

/// The slack the rebalancing allows next to a seam: a run of nodes is
/// repacked only when it uses more than `EXTRA` nodes beyond the minimum.
const EXTRA: usize = 2;

/// Element `i` (below the length), borrowed.
pub(super) fn get(tree: Node, i: usize) -> Word {
    let (mut node, mut i) = (tree, i);
    while !node.is_leaf() {
        let (j, before) = node.locate(i);
        node = node.child(j);
        i = i.saturating_sub(before);
    }
    node.elem(i)
}

/// A unique version of `node`: itself at count one, otherwise a copy that
/// shares its children (each dup'd), with the reference to the original
/// given up. A copied leaf gets room for `need` elements.
fn make_unique(node: Node, need: usize) -> Node {
    if node.is_unique() {
        return node;
    }
    let copy = if node.is_leaf() {
        let len = node.slots();
        let copy = Node::new_leaf(need.max(len));
        for i in 0..len {
            let elem = node.elem(i);
            dup_word(elem);
            copy.put_elem(i, elem);
        }
        copy.set_len(len);
        copy
    } else {
        let copy = Node::new_branch(node.height());
        let count = node.slots();
        for i in 0..count {
            copy.put_child(i, node.child(i).dup());
            copy.put_size(i, node.size_at(i));
        }
        copy.set_count(count);
        copy
    };
    node.release();
    copy
}

/// A unique leaf with room for `need` elements: `leaf` itself when it is
/// unique and has room; when it is unique and full, a leaf of doubled
/// capacity (at most [`M`]) that its elements move to; a copy otherwise.
pub(super) fn leaf_with_room(leaf: Node, need: usize) -> Node {
    if !leaf.is_unique() {
        return make_unique(leaf, need);
    }
    let cap = leaf.cap();
    if cap >= need {
        return leaf;
    }
    let len = leaf.slots();
    let grown = Node::new_leaf(need.max(cap.saturating_mul(2)).min(M));
    for i in 0..len {
        grown.put_elem(i, leaf.elem(i));
    }
    grown.set_len(len);
    // The elements moved: free the old leaf without releasing them.
    leaf.set_len(0);
    leaf.release();
    grown
}

/// A leaf holding one owned element.
pub(super) fn singleton(elem: Word) -> Node {
    let leaf = Node::new_leaf(1);
    leaf.put_elem(0, elem);
    leaf.set_len(1);
    leaf
}

/// A branch of height `height` whose only child is `child`.
fn wrap(child: Node, height: usize) -> Node {
    let branch = Node::new_branch(height);
    branch.push_child(child);
    branch
}

/// Replaces element `i` (below the length) with the owned `elem`.
pub(super) fn set(tree: Node, i: usize, elem: Word) -> Node {
    let node = make_unique(tree, 0);
    if node.is_leaf() {
        let old = node.elem(i);
        node.put_elem(i, elem);
        drop_word(old);
    } else {
        let (j, before) = node.locate(i);
        let child = set(node.child(j), i.saturating_sub(before), elem);
        node.put_child(j, child);
    }
    node
}

/// `tree` with every node on the path to element `i` (below the length)
/// unique: a shared node is copied ([`make_unique`]), so only that path
/// changes and the rest stays shared. Nothing changes where the path is
/// already unique. For writing slots in place (`std::heap`, D-1613).
pub(super) fn unique_path(tree: Node, i: usize) -> Node {
    let node = make_unique(tree, 0);
    if !node.is_leaf() {
        let (j, before) = node.locate(i);
        let child = unique_path(node.child(j), i.saturating_sub(before));
        node.put_child(j, child);
    }
    node
}

/// Element `i` (below the length), owned, in one descent: moved out and
/// replaced with the immediate `word` when every node on its path is
/// unique, so the tree no longer holds it; dup'd and left in place when a
/// node on the path is shared (another tree holds that node and its
/// elements).
pub(super) fn take_or_get(tree: Node, i: usize, word: Word) -> Word {
    let (mut node, mut i) = (tree, i);
    let mut unique = node.is_unique();
    // A branch of height `h` has children of height `h - 1`, so the
    // height is read once.
    let mut height = node.height();
    while height > 0 {
        let (j, before) = node.locate_at(i, height);
        node = node.child(j);
        i = i.saturating_sub(before);
        unique = unique && node.is_unique();
        height = height.saturating_sub(1);
    }
    let elem = node.elem(i);
    if unique {
        node.put_elem(i, word);
    } else {
        dup_word(elem);
    }
    elem
}

/// Replaces element `i` (below the length) with the immediate `word` and
/// releases the old element, in place, when every node on its path is
/// unique; returns whether it did. A shared node on the path leaves the
/// tree unchanged: another tree holds that node and its elements.
pub(super) fn clear_if_unique(tree: Node, i: usize, word: Word) -> bool {
    let (mut node, mut i) = (tree, i);
    loop {
        if !node.is_unique() {
            return false;
        }
        if node.is_leaf() {
            let old = node.elem(i);
            node.put_elem(i, word);
            drop_word(old);
            return true;
        }
        let (j, before) = node.locate(i);
        node = node.child(j);
        i = i.saturating_sub(before);
    }
}

/// Appends the owned `elem` at the end. In place along the right edge at
/// count one, so a unique tree grows without copying (amortized O(1)
/// allocation per element, O(height) time).
pub(super) fn push(tree: Node, elem: Word) -> Node {
    let height = tree.height();
    match push_rec(tree, elem) {
        (node, None) => node,
        (node, Some(sibling)) => {
            let top = Node::new_branch(height.saturating_add(1));
            top.push_child(node);
            top.push_child(sibling);
            top
        }
    }
}

/// [`push`] below the top: returns the node and, when it was full, a new
/// sibling of the same height holding the element.
fn push_rec(node: Node, elem: Word) -> (Node, Option<Node>) {
    if node.is_leaf() {
        let len = node.slots();
        if len >= M {
            return (node, Some(singleton(elem)));
        }
        let leaf = leaf_with_room(node, len.saturating_add(1));
        leaf.put_elem(len, elem);
        leaf.set_len(len.saturating_add(1));
        return (leaf, None);
    }
    let node = make_unique(node, 0);
    let count = node.slots();
    let last = count.saturating_sub(1);
    let (child, sibling) = push_rec(node.child(last), elem);
    node.put_child(last, child);
    match sibling {
        None => {
            node.put_size(last, node.size_at(last).saturating_add(1));
            (node, None)
        }
        Some(sibling) if count < M => {
            node.push_child(sibling);
            (node, None)
        }
        Some(sibling) => {
            let height = node.height();
            (node, Some(wrap(sibling, height)))
        }
    }
}

// ---------------------------------------------------------------------
// Whole leaves moved in and out, for the head and tail leaves of big roots

/// Whether the rightmost leaf holds [`M`] elements, so a push would start
/// a new leaf.
pub(super) fn last_leaf_full(tree: Node) -> bool {
    let mut node = tree;
    while !node.is_leaf() {
        node = node.child(node.slots().saturating_sub(1));
    }
    node.slots() >= M
}

/// Appends the owned leaf `leaf` (not empty) after the last element. In
/// place along the right edge where it is unique, like [`push`], but one
/// descent adds up to [`M`] elements.
pub(super) fn push_leaf(tree: Node, leaf: Node) -> Node {
    let height = tree.height();
    if height == 0 {
        let top = Node::new_branch(1);
        top.push_child(tree);
        top.push_child(leaf);
        return top;
    }
    match push_leaf_rec(tree, leaf) {
        (node, None) => node,
        (node, Some(sibling)) => {
            let top = Node::new_branch(height.saturating_add(1));
            top.push_child(node);
            top.push_child(sibling);
            top
        }
    }
}

/// [`push_leaf`] in a branch: returns the node and, when its right edge
/// was full, a new sibling of the same height holding the leaf.
fn push_leaf_rec(node: Node, leaf: Node) -> (Node, Option<Node>) {
    let node = make_unique(node, 0);
    let count = node.slots();
    let height = node.height();
    if height == 1 {
        if count < M {
            node.push_child(leaf);
            return (node, None);
        }
        return (node, Some(wrap(leaf, 1)));
    }
    let last = count.saturating_sub(1);
    let added = leaf.slots();
    let (child, sibling) = push_leaf_rec(node.child(last), leaf);
    node.put_child(last, child);
    match sibling {
        None => {
            node.put_size(last, node.size_at(last).saturating_add(added));
            (node, None)
        }
        Some(sibling) if count < M => {
            node.push_child(sibling);
            (node, None)
        }
        Some(sibling) => (node, Some(wrap(sibling, height))),
    }
}

/// Takes the first leaf out of an owned tree whose left edge is unique,
/// every node from the top down to the first leaf: the leaf, owned, and
/// the tree without it, or `None` when the leaf was all of it. In place,
/// with no allocation.
pub(super) fn pop_front_leaf(tree: Node) -> (Node, Option<Node>) {
    if tree.is_leaf() {
        return (tree, None);
    }
    let leaf = pop_front_rec(tree);
    if tree.slots() == 0 {
        tree.release();
        return (leaf, None);
    }
    (leaf, Some(collapse(tree)))
}

/// [`pop_front_leaf`] in a unique branch, which may be left with no
/// children: the caller then releases it.
fn pop_front_rec(node: Node) -> Node {
    let child = node.child(0);
    let (leaf, removed) = if child.is_leaf() {
        (child, true)
    } else {
        let leaf = pop_front_rec(child);
        let emptied = child.slots() == 0;
        if emptied {
            child.release();
        }
        (leaf, emptied)
    };
    let gone = leaf.slots();
    let count = node.slots();
    if removed {
        for i in 1..count {
            let to = i.saturating_sub(1);
            node.put_child(to, node.child(i));
            node.put_size(to, node.size_at(i).saturating_sub(gone));
        }
        node.set_count(count.saturating_sub(1));
    } else {
        for i in 0..count {
            node.put_size(i, node.size_at(i).saturating_sub(gone));
        }
    }
    leaf
}

/// Puts the owned leaf `leaf` (not empty) before the first element: the
/// mirror of [`push_leaf`]. In place along the left edge where it is
/// unique, with the children of one branch per level moved up a slot.
pub(super) fn prepend_leaf(tree: Node, leaf: Node) -> Node {
    let height = tree.height();
    if height == 0 {
        let top = Node::new_branch(1);
        top.push_child(leaf);
        top.push_child(tree);
        return top;
    }
    match prepend_leaf_rec(tree, leaf) {
        (node, None) => node,
        (node, Some(sibling)) => {
            let top = Node::new_branch(height.saturating_add(1));
            top.push_child(sibling);
            top.push_child(node);
            top
        }
    }
}

/// [`prepend_leaf`] in a branch: returns the node and, when its left edge
/// was full, a new sibling of the same height that holds the leaf and
/// goes before it.
fn prepend_leaf_rec(node: Node, leaf: Node) -> (Node, Option<Node>) {
    let node = make_unique(node, 0);
    let height = node.height();
    let added = leaf.slots();
    let before = if height == 1 {
        Some(leaf)
    } else {
        let (child, sibling) = prepend_leaf_rec(node.child(0), leaf);
        node.put_child(0, child);
        sibling
    };
    let count = node.slots();
    match before {
        // The leaf went in below: every size counts it.
        None => {
            for i in 0..count {
                node.put_size(i, node.size_at(i).saturating_add(added));
            }
            (node, None)
        }
        Some(first) if count < M => {
            for i in (0..count).rev() {
                let to = i.saturating_add(1);
                node.put_child(to, node.child(i));
                node.put_size(to, node.size_at(i).saturating_add(added));
            }
            node.put_child(0, first);
            node.put_size(0, added);
            node.set_count(count.saturating_add(1));
            (node, None)
        }
        Some(first) => (node, Some(wrap(first, height))),
    }
}

/// Whether the leftmost leaf holds [`M`] elements, so a prepend would
/// start a new leaf.
pub(super) fn first_leaf_full(tree: Node) -> bool {
    let mut node = tree;
    while !node.is_leaf() {
        node = node.child(0);
    }
    node.slots() >= M
}

/// Takes the last leaf out of an owned tree whose right edge is unique,
/// every node from the top down to the last leaf: the tree without it, or
/// `None` when the leaf was all of it, and the leaf, owned. In place, with
/// no allocation.
pub(super) fn pop_back_leaf(tree: Node) -> (Option<Node>, Node) {
    if tree.is_leaf() {
        return (None, tree);
    }
    let leaf = pop_back_rec(tree);
    if tree.slots() == 0 {
        tree.release();
        return (None, leaf);
    }
    (Some(collapse(tree)), leaf)
}

/// [`pop_back_leaf`] in a unique branch, which may be left with no
/// children: the caller then releases it.
fn pop_back_rec(node: Node) -> Node {
    let last = node.slots().saturating_sub(1);
    let child = node.child(last);
    let (leaf, removed) = if child.is_leaf() {
        (child, true)
    } else {
        let leaf = pop_back_rec(child);
        let emptied = child.slots() == 0;
        if emptied {
            child.release();
        }
        (leaf, emptied)
    };
    if removed {
        node.set_count(last);
    } else {
        node.put_size(last, node.size_at(last).saturating_sub(leaf.slots()));
    }
    leaf
}

/// A unique leaf holding the elements of the unique leaf `leaf` at the
/// end of its slots, with at least one slot free before them, and the
/// index of the first: the slots before it hold the immediate `word`. The
/// new leaf has twice the capacity, at most [`M`], and `leaf`, whose
/// capacity is below [`M`], is freed. So prepending to a unique leaf
/// moves each element O(1) times while the leaf grows.
pub(super) fn gap_front(leaf: Node, word: Word) -> (Node, usize) {
    let len = leaf.slots();
    let room = len
        .saturating_add(1)
        .max(leaf.cap().saturating_mul(2))
        .min(M);
    let first = room.saturating_sub(len);
    let wide = Node::new_leaf(room);
    for i in 0..first {
        wide.put_elem(i, word);
    }
    for i in 0..len {
        wide.put_elem(first.saturating_add(i), leaf.elem(i));
    }
    wide.set_len(room);
    // The elements moved: free the old leaf without releasing them.
    leaf.set_len(0);
    leaf.release();
    (wide, first)
}

/// Moves the elements `offset..` of the unique leaf `leaf`, which are its
/// last, to start at slot `to`, so the free slots of a full-size leaf
/// lie where the next elements go. What the slots before `offset` held is
/// released, and the slots before `to` then hold the immediate `word`.
/// `to` plus the number of elements is at most the capacity.
pub(super) fn slide(leaf: Node, offset: usize, to: usize, word: Word) {
    let len = leaf.slots().saturating_sub(offset);
    for i in 0..offset {
        drop_word(leaf.elem(i));
    }
    leaf.move_elems(offset, to, len);
    for i in 0..to {
        leaf.put_elem(i, word);
    }
    leaf.set_len(to.saturating_add(len));
}

/// Adds the owned `elem` at the front. In place when the left edge is
/// unique and its leaf has room (O(M log n) slot moves, no allocation but
/// a leaf's growth); otherwise a concatenation with a new leaf, after
/// which the new leftmost leaf has room again. So prepending at count one
/// allocates amortized O(1) per element.
pub(super) fn prepend(tree: Node, elem: Word) -> Node {
    let mut node = tree;
    let in_place = loop {
        if !node.is_unique() {
            break false;
        }
        if node.is_leaf() {
            break node.slots() < M;
        }
        node = node.child(0);
    };
    if !in_place {
        return concat(singleton(elem), tree);
    }
    prepend_in_place(tree, elem)
}

/// [`prepend`] along a unique left edge whose leaf has room.
fn prepend_in_place(node: Node, elem: Word) -> Node {
    if node.is_leaf() {
        let len = node.slots();
        let leaf = leaf_with_room(node, len.saturating_add(1));
        leaf.move_elems(0, 1, len);
        leaf.put_elem(0, elem);
        leaf.set_len(len.saturating_add(1));
        return leaf;
    }
    let child = prepend_in_place(node.child(0), elem);
    node.put_child(0, child);
    for i in 0..node.slots() {
        node.put_size(i, node.size_at(i).saturating_add(1));
    }
    node
}

/// The first `count` elements (1 to the length). Keeps the height; see
/// [`collapse`].
pub(super) fn take(tree: Node, count: usize) -> Node {
    if count >= tree.len() {
        return tree;
    }
    if tree.is_leaf() {
        if tree.is_unique() {
            for i in count..tree.slots() {
                drop_word(tree.elem(i));
            }
            tree.set_len(count);
            return tree;
        }
        let copy = Node::new_leaf(count);
        for i in 0..count {
            let elem = tree.elem(i);
            dup_word(elem);
            copy.put_elem(i, elem);
        }
        copy.set_len(count);
        tree.release();
        return copy;
    }
    let (j, before) = tree.locate(count.saturating_sub(1));
    let rest = count.saturating_sub(before);
    if tree.is_unique() {
        for i in j.saturating_add(1)..tree.slots() {
            tree.child(i).release();
        }
        tree.set_count(j.saturating_add(1));
        let child = take(tree.child(j), rest);
        tree.put_child(j, child);
        tree.put_size(j, count);
        return tree;
    }
    let copy = Node::new_branch(tree.height());
    for i in 0..j {
        copy.put_child(i, tree.child(i).dup());
        copy.put_size(i, tree.size_at(i));
    }
    copy.set_count(j);
    copy.push_child(take(tree.child(j).dup(), rest));
    tree.release();
    copy
}

/// Everything after the first `count` elements (0 to the length - 1).
/// Keeps the height; see [`collapse`].
pub(super) fn drop_front(tree: Node, count: usize) -> Node {
    if count == 0 {
        return tree;
    }
    let len = tree.len();
    if tree.is_leaf() {
        let left = len.saturating_sub(count);
        if tree.is_unique() {
            for i in 0..count {
                drop_word(tree.elem(i));
            }
            tree.move_elems(count, 0, left);
            tree.set_len(left);
            return tree;
        }
        let copy = Node::new_leaf(left);
        for i in 0..left {
            let elem = tree.elem(count.saturating_add(i));
            dup_word(elem);
            copy.put_elem(i, elem);
        }
        copy.set_len(left);
        tree.release();
        return copy;
    }
    let (j, before) = tree.locate(count);
    let first = drop_front(
        if tree.is_unique() {
            tree.child(j)
        } else {
            tree.child(j).dup()
        },
        count.saturating_sub(before),
    );
    let total = tree.slots();
    if tree.is_unique() {
        for i in 0..j {
            tree.child(i).release();
        }
        let node = tree;
        node.put_child(0, first);
        node.put_size(0, first.len());
        let mut size = first.len();
        for (to, from) in (1..).zip(j.saturating_add(1)..total) {
            let child = node.child(from);
            size = size.saturating_add(child.len());
            node.put_child(to, child);
            node.put_size(to, size);
        }
        node.set_count(total.saturating_sub(j));
        return node;
    }
    let copy = Node::new_branch(tree.height());
    copy.push_child(first);
    for i in j.saturating_add(1)..total {
        copy.push_child(tree.child(i).dup());
    }
    tree.release();
    copy
}

/// Removes branches with a single child from the top of a tree.
pub(super) fn collapse(tree: Node) -> Node {
    let mut node = tree;
    while !node.is_leaf() && node.slots() == 1 {
        let child = node.child(0);
        if node.is_unique() {
            node.set_count(0);
        } else {
            child.dup();
        }
        node.release();
        node = child;
    }
    node
}

/// The slots of a node, owned, and the node given up: moved out of a
/// unique node, dup'd from a shared one.
fn take_slots(node: Node) -> Vec<Word> {
    let count = node.slots();
    let leaf = node.is_leaf();
    let unique = node.is_unique();
    let slots: Vec<Word> = (0..count)
        .map(|i| {
            let word = if leaf {
                node.elem(i)
            } else {
                node.child(i).word()
            };
            if !unique {
                dup_word(word);
            }
            word
        })
        .collect();
    if unique {
        if leaf {
            node.set_len(0);
        } else {
            node.set_count(0);
        }
    }
    node.release();
    slots
}

/// `a` followed by `b`, both owned. O(log n): see the module doc.
pub(super) fn concat(a: Node, b: Node) -> Node {
    let (mut a, mut b) = (a, b);
    while a.height() < b.height() {
        let height = a.height().saturating_add(1);
        a = wrap(a, height);
    }
    while b.height() < a.height() {
        let height = b.height().saturating_add(1);
        b = wrap(b, height);
    }
    let height = a.height();
    let mut nodes = concat_rec(a, b);
    let top = match (nodes.pop(), nodes.pop()) {
        (Some(only), None) => only,
        (Some(second), Some(first)) => {
            let top = Node::new_branch(height.saturating_add(1));
            top.push_child(first);
            top.push_child(second);
            top
        }
        _ => crate::panic::fatal(format_args!("list concatenation lost its nodes")),
    };
    collapse(top)
}

/// Merges two nodes of one height along the seam between them, giving one
/// or two nodes of that height.
fn concat_rec(a: Node, b: Node) -> Vec<Node> {
    if a.is_leaf() {
        let (la, lb) = (a.slots(), b.slots());
        let total = la.saturating_add(lb);
        if total > M {
            return vec![a, b];
        }
        let merged = leaf_with_room(a, total);
        for (i, elem) in (la..).zip(take_slots(b)) {
            merged.put_elem(i, elem);
        }
        merged.set_len(total);
        return vec![merged];
    }
    let height = a.height();
    let mut left = take_slots(a);
    let mut right = take_slots(b).into_iter();
    let (Some(last), Some(first)) = (left.pop(), right.next()) else {
        crate::panic::fatal(format_args!("list concatenation of an empty branch"));
    };
    let middle = concat_rec(node_of(last), node_of(first));
    let mut all: Vec<Node> = left.into_iter().map(node_of).collect();
    all.extend(middle);
    all.extend(right.map(node_of));
    let all = rebalance(all);
    all.chunks(M)
        .map(|chunk| {
            let branch = Node::new_branch(height);
            for child in chunk {
                branch.push_child(*child);
            }
            branch
        })
        .collect()
}

/// A node handle for a word taken from a branch slot.
fn node_of(word: Word) -> Node {
    Node::from_slot(word)
}

/// Repacks a run of sibling nodes when it holds more than [`EXTRA`] nodes
/// beyond the fewest that could hold its slots: every node but the last
/// is then full. Otherwise returns the run unchanged.
fn rebalance(nodes: Vec<Node>) -> Vec<Node> {
    let slots: usize = nodes.iter().map(|n| n.slots()).sum();
    let fewest = slots.div_ceil(M);
    if nodes.len() <= fewest.saturating_add(EXTRA) {
        return nodes;
    }
    let Some(height) = nodes.first().map(|n| n.height()) else {
        return nodes;
    };
    let all: Vec<Word> = nodes.into_iter().flat_map(take_slots).collect();
    all.chunks(M)
        .map(|chunk| {
            if height == 0 {
                let leaf = Node::new_leaf(chunk.len());
                for (i, elem) in chunk.iter().enumerate() {
                    leaf.put_elem(i, *elem);
                }
                leaf.set_len(chunk.len());
                leaf
            } else {
                let branch = Node::new_branch(height);
                for child in chunk {
                    branch.push_child(node_of(*child));
                }
                branch
            }
        })
        .collect()
}
