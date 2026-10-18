//! A persistent weight-balanced tree with `Rc` nodes: the algorithm of
//! `std/sorted_map.cheby` (Adams' tree, `delta` 3 and `ratio` 2), step for
//! step. Every update copies the path it changes and shares the rest, as
//! a runtime kernel that must stay persistent would (D-039); nothing is
//! updated in place.

#![expect(
    clippy::arithmetic_side_effects,
    reason = "subtree sizes are at most the number of entries, far below i64::MAX"
)]

use std::cmp::Ordering;
use std::rc::Rc;

/// The weight of one side against the other that the tree allows.
const DELTA: i64 = 3;

/// The bound between a single and a double rotation.
const RATIO: i64 = 2;

/// A tree: empty, or a shared node.
#[derive(Debug)]
pub(crate) enum Tree<K, V> {
    Tip,
    Bin(Rc<Node<K, V>>),
}

impl<K, V> Clone for Tree<K, V> {
    fn clone(&self) -> Self {
        match self {
            Tree::Tip => Tree::Tip,
            Tree::Bin(node) => Tree::Bin(Rc::clone(node)),
        }
    }
}

/// A node: the size of its subtree, its entry and its two sides.
#[derive(Debug)]
pub(crate) struct Node<K, V> {
    size: i64,
    key: K,
    value: V,
    left: Tree<K, V>,
    right: Tree<K, V>,
}

impl<K: Ord + Clone, V: Clone> Tree<K, V> {
    /// The number of entries, O(1).
    pub(crate) fn size(&self) -> i64 {
        match self {
            Tree::Tip => 0,
            Tree::Bin(node) => node.size,
        }
    }

    /// The value stored under `key`.
    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        match self {
            Tree::Tip => None,
            Tree::Bin(node) => match key.cmp(&node.key) {
                Ordering::Less => node.left.get(key),
                Ordering::Greater => node.right.get(key),
                Ordering::Equal => Some(&node.value),
            },
        }
    }

    /// Whether `key` is present.
    pub(crate) fn has_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// The entry with the smallest key.
    pub(crate) fn first(&self) -> Option<(&K, &V)> {
        match self {
            Tree::Tip => None,
            Tree::Bin(node) => match &node.left {
                Tree::Tip => Some((&node.key, &node.value)),
                left @ Tree::Bin(_) => left.first(),
            },
        }
    }

    /// The entry with the largest key.
    pub(crate) fn last(&self) -> Option<(&K, &V)> {
        match self {
            Tree::Tip => None,
            Tree::Bin(node) => match &node.right {
                Tree::Tip => Some((&node.key, &node.value)),
                right @ Tree::Bin(_) => right.last(),
            },
        }
    }

    /// The tree with `value` under `key`; a present key stays and only its
    /// value changes.
    pub(crate) fn insert(&self, key: K, value: V) -> Self {
        self.upsert(key, &mut |_| value.clone())
    }

    /// The tree with `update(found)` under `key`, in one descent.
    pub(crate) fn upsert(&self, key: K, update: &mut dyn FnMut(Option<&V>) -> V) -> Self {
        match self {
            Tree::Tip => leaf(key, update(None)),
            Tree::Bin(node) => match key.cmp(&node.key) {
                Ordering::Less => {
                    let grown = node.left.upsert(key, update);
                    let (sl, sr) = (grown.size(), node.right.size());
                    if balanced(sl, sr) {
                        with_size(
                            sl + sr + 1,
                            &node.key,
                            &node.value,
                            grown,
                            node.right.clone(),
                        )
                    } else {
                        rotate_right(&node.key, &node.value, grown, node.right.clone())
                    }
                }
                Ordering::Greater => {
                    let grown = node.right.upsert(key, update);
                    let (sl, sr) = (node.left.size(), grown.size());
                    if balanced(sl, sr) {
                        with_size(
                            sl + sr + 1,
                            &node.key,
                            &node.value,
                            node.left.clone(),
                            grown,
                        )
                    } else {
                        rotate_left(&node.key, &node.value, node.left.clone(), grown)
                    }
                }
                Ordering::Equal => {
                    let value = update(Some(&node.value));
                    with_size(
                        node.size,
                        &node.key,
                        &value,
                        node.left.clone(),
                        node.right.clone(),
                    )
                }
            },
        }
    }

    /// The tree without `key`.
    pub(crate) fn remove(&self, key: &K) -> Self {
        match self {
            Tree::Tip => Tree::Tip,
            Tree::Bin(node) => match key.cmp(&node.key) {
                Ordering::Less => {
                    let shrunk = node.left.remove(key);
                    let (sl, sr) = (shrunk.size(), node.right.size());
                    if balanced(sl, sr) {
                        with_size(
                            sl + sr + 1,
                            &node.key,
                            &node.value,
                            shrunk,
                            node.right.clone(),
                        )
                    } else {
                        rotate_left(&node.key, &node.value, shrunk, node.right.clone())
                    }
                }
                Ordering::Greater => {
                    let shrunk = node.right.remove(key);
                    let (sl, sr) = (node.left.size(), shrunk.size());
                    if balanced(sl, sr) {
                        with_size(
                            sl + sr + 1,
                            &node.key,
                            &node.value,
                            node.left.clone(),
                            shrunk,
                        )
                    } else {
                        rotate_right(&node.key, &node.value, node.left.clone(), shrunk)
                    }
                }
                Ordering::Equal => glue(&node.left, &node.right),
            },
        }
    }

    /// Combines the entries in key order.
    pub(crate) fn fold<A>(&self, initial: A, f: &mut dyn FnMut(A, &K, &V) -> A) -> A {
        match self {
            Tree::Tip => initial,
            Tree::Bin(node) => {
                let left = node.left.fold(initial, f);
                let here = f(left, &node.key, &node.value);
                node.right.fold(here, f)
            }
        }
    }

    /// The entries whose keys are at least `start` and less than `end`, in
    /// key order, appended to `found`.
    pub(crate) fn range(&self, start: &K, end: &K, found: &mut Vec<(K, V)>) {
        if let Tree::Bin(node) = self {
            let after_start = node.key >= *start;
            let before_end = node.key < *end;
            if after_start {
                node.left.range(start, end, found);
            }
            if after_start && before_end {
                found.push((node.key.clone(), node.value.clone()));
            }
            if before_end {
                node.right.range(start, end, found);
            }
        }
    }

    /// The entries for which `keep` returns true, joined by `link` and
    /// `concat`.
    pub(crate) fn filter(&self, keep: &mut dyn FnMut(&K, &V) -> bool) -> Self {
        match self {
            Tree::Tip => Tree::Tip,
            Tree::Bin(node) => {
                let kept_left = node.left.filter(keep);
                let here = keep(&node.key, &node.value);
                let kept_right = node.right.filter(keep);
                if here {
                    link(&node.key, &node.value, &kept_left, &kept_right)
                } else {
                    concat(&kept_left, &kept_right)
                }
            }
        }
    }
}

/// Whether subtrees of sizes `a` and `b` may be the two sides of a node.
fn balanced(a: i64, b: i64) -> bool {
    a + b <= 1 || (a <= DELTA * b && b <= DELTA * a)
}

fn leaf<K, V>(key: K, value: V) -> Tree<K, V> {
    Tree::Bin(Rc::new(Node {
        size: 1,
        key,
        value,
        left: Tree::Tip,
        right: Tree::Tip,
    }))
}

fn with_size<K: Clone, V: Clone>(
    size: i64,
    key: &K,
    value: &V,
    left: Tree<K, V>,
    right: Tree<K, V>,
) -> Tree<K, V> {
    Tree::Bin(Rc::new(Node {
        size,
        key: key.clone(),
        value: value.clone(),
        left,
        right,
    }))
}

/// A node over `left` and `right`, with its size.
fn node<K: Ord + Clone, V: Clone>(
    key: &K,
    value: &V,
    left: Tree<K, V>,
    right: Tree<K, V>,
) -> Tree<K, V> {
    let size = left.size() + right.size() + 1;
    with_size(size, key, value, left, right)
}

/// A node over `l` and `r`, rotated when one side outweighs the other.
fn balance<K: Ord + Clone, V: Clone>(
    key: &K,
    value: &V,
    l: Tree<K, V>,
    r: Tree<K, V>,
) -> Tree<K, V> {
    let (sl, sr) = (l.size(), r.size());
    if sl + sr <= 1 {
        with_size(sl + sr + 1, key, value, l, r)
    } else if sr > DELTA * sl {
        rotate_left(key, value, l, r)
    } else if sl > DELTA * sr {
        rotate_right(key, value, l, r)
    } else {
        with_size(sl + sr + 1, key, value, l, r)
    }
}

/// A node over `l` and a heavier `r`, by a single or a double rotation.
fn rotate_left<K: Ord + Clone, V: Clone>(
    key: &K,
    value: &V,
    l: Tree<K, V>,
    r: Tree<K, V>,
) -> Tree<K, V> {
    let Tree::Bin(right) = &r else {
        return node(key, value, l, r);
    };
    if right.left.size() < RATIO * right.right.size() {
        return node(
            &right.key,
            &right.value,
            node(key, value, l, right.left.clone()),
            right.right.clone(),
        );
    }
    match &right.left {
        Tree::Tip => node(
            &right.key,
            &right.value,
            node(key, value, l, Tree::Tip),
            right.right.clone(),
        ),
        Tree::Bin(middle) => node(
            &middle.key,
            &middle.value,
            node(key, value, l, middle.left.clone()),
            node(
                &right.key,
                &right.value,
                middle.right.clone(),
                right.right.clone(),
            ),
        ),
    }
}

/// A node over a heavier `l` and `r`, by a single or a double rotation.
fn rotate_right<K: Ord + Clone, V: Clone>(
    key: &K,
    value: &V,
    l: Tree<K, V>,
    r: Tree<K, V>,
) -> Tree<K, V> {
    let Tree::Bin(left) = &l else {
        return node(key, value, l, r);
    };
    if left.right.size() < RATIO * left.left.size() {
        return node(
            &left.key,
            &left.value,
            left.left.clone(),
            node(key, value, left.right.clone(), r),
        );
    }
    match &left.right {
        Tree::Tip => node(
            &left.key,
            &left.value,
            left.left.clone(),
            node(key, value, Tree::Tip, r),
        ),
        Tree::Bin(middle) => node(
            &middle.key,
            &middle.value,
            node(
                &left.key,
                &left.value,
                left.left.clone(),
                middle.left.clone(),
            ),
            node(key, value, middle.right.clone(), r),
        ),
    }
}

/// Joins the two sides of a removed node; the larger side gives up its
/// nearest entry as the new node.
fn glue<K: Ord + Clone, V: Clone>(l: &Tree<K, V>, r: &Tree<K, V>) -> Tree<K, V> {
    match (l, r) {
        (Tree::Tip, _) => r.clone(),
        (_, Tree::Tip) => l.clone(),
        (Tree::Bin(a), Tree::Bin(b)) => {
            if a.size > b.size {
                let (key, value, rest) = max_view(&a.key, &a.value, &a.left, &a.right);
                balance(&key, &value, rest, r.clone())
            } else {
                let (key, value, rest) = min_view(&b.key, &b.value, &b.left, &b.right);
                balance(&key, &value, l.clone(), rest)
            }
        }
    }
}

/// The smallest entry of the node `key`, `value`, `l`, `r`, and the tree
/// without it.
fn min_view<K: Ord + Clone, V: Clone>(
    key: &K,
    value: &V,
    l: &Tree<K, V>,
    r: &Tree<K, V>,
) -> (K, V, Tree<K, V>) {
    match l {
        Tree::Tip => (key.clone(), value.clone(), r.clone()),
        Tree::Bin(left) => {
            let (k, x, rest) = min_view(&left.key, &left.value, &left.left, &left.right);
            (k, x, balance(key, value, rest, r.clone()))
        }
    }
}

/// The largest entry of the node `key`, `value`, `l`, `r`, and the tree
/// without it.
fn max_view<K: Ord + Clone, V: Clone>(
    key: &K,
    value: &V,
    l: &Tree<K, V>,
    r: &Tree<K, V>,
) -> (K, V, Tree<K, V>) {
    match r {
        Tree::Tip => (key.clone(), value.clone(), l.clone()),
        Tree::Bin(right) => {
            let (k, x, rest) = max_view(&right.key, &right.value, &right.left, &right.right);
            (k, x, balance(key, value, l.clone(), rest))
        }
    }
}

/// The tree with the entry before all its keys.
fn insert_min<K: Ord + Clone, V: Clone>(key: &K, value: &V, t: &Tree<K, V>) -> Tree<K, V> {
    match t {
        Tree::Tip => leaf(key.clone(), value.clone()),
        Tree::Bin(n) => balance(
            &n.key,
            &n.value,
            insert_min(key, value, &n.left),
            n.right.clone(),
        ),
    }
}

/// The tree with the entry after all its keys.
fn insert_max<K: Ord + Clone, V: Clone>(key: &K, value: &V, t: &Tree<K, V>) -> Tree<K, V> {
    match t {
        Tree::Tip => leaf(key.clone(), value.clone()),
        Tree::Bin(n) => balance(
            &n.key,
            &n.value,
            n.left.clone(),
            insert_max(key, value, &n.right),
        ),
    }
}

/// A tree of `l`, the entry and `r`, whose keys are in that order.
fn link<K: Ord + Clone, V: Clone>(
    key: &K,
    value: &V,
    l: &Tree<K, V>,
    r: &Tree<K, V>,
) -> Tree<K, V> {
    match (l, r) {
        (Tree::Tip, _) => insert_min(key, value, r),
        (_, Tree::Tip) => insert_max(key, value, l),
        (Tree::Bin(a), Tree::Bin(b)) => {
            if DELTA * a.size < b.size {
                balance(
                    &b.key,
                    &b.value,
                    link(key, value, l, &b.left),
                    b.right.clone(),
                )
            } else if DELTA * b.size < a.size {
                balance(
                    &a.key,
                    &a.value,
                    a.left.clone(),
                    link(key, value, &a.right, r),
                )
            } else {
                node(key, value, l.clone(), r.clone())
            }
        }
    }
}

/// A tree of `l` and `r`, whose keys are in that order.
fn concat<K: Ord + Clone, V: Clone>(l: &Tree<K, V>, r: &Tree<K, V>) -> Tree<K, V> {
    match (l, r) {
        (Tree::Tip, _) => r.clone(),
        (_, Tree::Tip) => l.clone(),
        (Tree::Bin(a), Tree::Bin(b)) => {
            if DELTA * a.size < b.size {
                balance(&b.key, &b.value, concat(l, &b.left), b.right.clone())
            } else if DELTA * b.size < a.size {
                balance(&a.key, &a.value, a.left.clone(), concat(&a.right, r))
            } else {
                glue(l, r)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether every node has its size, balanced sides and ordered keys.
    fn valid(t: &Tree<i64, i64>, low: i64, high: i64) -> bool {
        match t {
            Tree::Tip => true,
            Tree::Bin(n) => {
                let (sl, sr) = (n.left.size(), n.right.size());
                let weighed = sl + sr <= 1 || (sl <= 3 * sr && sr <= 3 * sl);
                n.size == sl + sr + 1
                    && weighed
                    && low <= n.key
                    && n.key < high
                    && valid(&n.left, low, n.key)
                    && valid(&n.right, n.key + 1, high)
            }
        }
    }

    #[test]
    fn the_tree_stays_balanced_and_agrees_with_a_btree_map() {
        let mut tree = Tree::Tip;
        let mut reference = std::collections::BTreeMap::new();
        let mut seed: i64 = 1;
        for step in 0..3_000 {
            seed = (seed * 1_103_515_245 + 12_345) % 2_147_483_648;
            let key = seed / 65_536 % 500;
            if step % 3 == 0 {
                tree = tree.remove(&key);
                reference.remove(&key);
            } else {
                tree = tree.insert(key, step);
                reference.insert(key, step);
            }
            assert!(valid(&tree, 0, 500));
        }
        let entries = tree.fold(Vec::new(), &mut |mut found, k, v| {
            found.push((*k, *v));
            found
        });
        let expected: Vec<(i64, i64)> = reference.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(entries, expected);
        let evens = tree.filter(&mut |_, v| v % 2 == 0);
        assert!(valid(&evens, 0, 500));
        assert_eq!(
            evens.size(),
            i64::try_from(reference.values().filter(|v| *v % 2 == 0).count()).unwrap()
        );
        let mut ranged = Vec::new();
        tree.range(&100, &200, &mut ranged);
        assert_eq!(ranged.len(), reference.range(100..200).count());
        assert_eq!(
            tree.first().map(|(k, _)| *k),
            reference.keys().next().copied()
        );
        assert_eq!(
            tree.last().map(|(k, _)| *k),
            reference.keys().next_back().copied()
        );
    }
}
