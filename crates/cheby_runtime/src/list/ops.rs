//! List operations on roots: the view logic over [`super::tree`].
//!
//! Ownership is in each signature's doc: "owned" arguments are consumed,
//! "borrowed" ones are not, and every returned root is owned. A unique
//! root is updated in place and returned; so is the tree under it, node
//! by node, where it is unique (D-274, §9.3).

use super::node::{Node, Root, View, drop_word, dup_word, empty};
use super::tree;
use crate::abi::Word;

/// The number of elements of a borrowed list.
pub(crate) fn len(list: Root) -> usize {
    list.len()
}

/// Element `index` of a borrowed list, borrowed, or `None` outside it.
pub(crate) fn get(list: Root, index: usize) -> Option<Word> {
    let view = list.view();
    if index >= view.len {
        return None;
    }
    view.tree
        .map(|tree| tree::get(tree, view.offset.saturating_add(index)))
}

/// The tree holding exactly the view's elements, owned (the view's tree
/// must be owned): cut at both ends where the view is narrower.
fn trimmed(view: View) -> Option<Node> {
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
fn update(list: Root, change: impl FnOnce(View) -> View) -> Root {
    if list.is_unique() {
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
    drop_word(list.word());
    Root::new(change(view))
}

/// `list` (owned) with the owned `elem` added at the end. In place when the
/// list is unique: amortized O(1) allocation per element, O(log n) time.
pub(crate) fn push(list: Root, elem: Word) -> Root {
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

/// `list` (owned) with the owned `elem` added at the front. O(log n); in
/// place along the left edge when it is unique, with amortized O(1)
/// allocation per element.
pub(crate) fn prepend(list: Root, elem: Word) -> Root {
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
    if a.len() == 0 {
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
/// result is empty when `end <= start` (D-298).
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
        dup_word(list.word());
        let view = list.view();
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
    items.into_iter().fold(empty, push)
}
