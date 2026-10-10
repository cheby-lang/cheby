//! Operations on maps and sets: lookup, insertion, removal, the single
//! lookup of `upsert`, iteration and building.
//!
//! A mutation first walks down read-only and records its path (at most 14
//! nodes: 13 bitmap levels and a collision node). When something must
//! change, it makes the path writable from the top: a unique node is kept
//! (and grown in place when full), a shared one is copied with a reference
//! added to each slot, and the copy replaces it in its parent. So an
//! update allocates nothing when every node on the path is unique (§9.3,
//! D-274), and copies only the path otherwise. Path walks are bounded by
//! the hash width; whole-map walks use an explicit stack (plan §0).

use super::compare::{key_hash, keys_equal};
use super::node::{
    BITS, Bitmap, Collection, Collision, HOLE, MAX_SHIFT, Root, drop_word, dup_word,
};
use crate::abi::Word;

/// The bit of `hash` at the level of `shift`.
fn bit_at(hash: u64, shift: u32) -> u32 {
    let index = hash.checked_shr(shift).unwrap_or(0) & 31;
    1_u32 << index
}

/// The position of `bit` among the set bits of `map`.
fn index(map: u32, bit: u32) -> usize {
    super::node::count((map & bit.wrapping_sub(1)).count_ones())
}

// ---------------------------------------------------------------------
// Lookup

/// Where a key was found: the node word (bitmap or collision node, whose
/// slots start at the same offset) and the slot of its key.
#[derive(Debug, Clone, Copy)]
pub(super) struct Found {
    node: Word,
    key_slot: usize,
}

impl Found {
    /// The stored key, borrowed.
    pub(super) fn key(self) -> Word {
        Bitmap(self.node).get(self.key_slot)
    }

    /// The stored value of a map entry, borrowed.
    pub(super) fn value(self) -> Word {
        Bitmap(self.node).get(self.key_slot.wrapping_add(1))
    }
}

/// Looks `key` up in a borrowed root, hashing with the root's own seed.
///
/// The key is hashed even when the collection is empty, so looking up a
/// key that holds a function value or a handle panics whatever the
/// collection holds (§3.13, D-233).
pub(super) fn find(root: Root, coll: Collection, key: Word) -> Option<Found> {
    let hash = key_hash(key, root.seed());
    find_hashed(root.node()?, coll, key, hash)
}

/// The only entry a key whose hash is `hash` can match below `top`,
/// found from the hash alone, without comparing keys.
#[derive(Debug, Clone, Copy)]
pub(super) enum Candidate {
    /// No entry has a key with this hash's path.
    Absent,
    /// This entry's key is the only one the key can equal: the key is in
    /// the collection exactly when the two are equal.
    One(Found),
    /// The path ends in a collision node with this full hash, whose entries
    /// must be compared one by one ([`find_hashed`]).
    Collision,
}

/// Finds the [`Candidate`] for `hash` below `top`. Equality of whole maps
/// and sets uses it to put each key pair on the caller's worklist instead
/// of comparing keys in a nested walk (plan §0, deep data;
/// GAP(nested-collection-walks)).
pub(super) fn candidate(top: Bitmap, coll: Collection, hash: u64) -> Candidate {
    let stride = coll.stride();
    let mut node = top.0;
    let mut shift = 0;
    loop {
        if shift >= MAX_SHIFT {
            return if Collision(node).hash() == hash {
                Candidate::Collision
            } else {
                Candidate::Absent
            };
        }
        let here = Bitmap(node);
        let bit = bit_at(hash, shift);
        let (data, nodes) = here.maps();
        if data & bit != 0 {
            let key_slot = index(data, bit).wrapping_mul(stride);
            return Candidate::One(Found { node, key_slot });
        }
        if nodes & bit == 0 {
            return Candidate::Absent;
        }
        node = here.child(coll, index(nodes, bit));
        shift = shift.wrapping_add(BITS);
    }
}

/// Looks `key`, whose hash is `hash`, up below `top`.
pub(super) fn find_hashed(top: Bitmap, coll: Collection, key: Word, hash: u64) -> Option<Found> {
    let stride = coll.stride();
    let mut node = top.0;
    let mut shift = 0;
    loop {
        if shift >= MAX_SHIFT {
            let leaf = Collision(node);
            if leaf.hash() != hash {
                return None;
            }
            return (0..leaf.len())
                .map(|e| e.wrapping_mul(stride))
                .find(|&s| keys_equal(leaf.get(s), key))
                .map(|key_slot| Found { node, key_slot });
        }
        let here = Bitmap(node);
        let bit = bit_at(hash, shift);
        let (data, nodes) = here.maps();
        if data & bit != 0 {
            let key_slot = index(data, bit).wrapping_mul(stride);
            return keys_equal(here.get(key_slot), key).then_some(Found { node, key_slot });
        }
        if nodes & bit == 0 {
            return None;
        }
        node = here.child(coll, index(nodes, bit));
        shift = shift.wrapping_add(BITS);
    }
}

// ---------------------------------------------------------------------
// Paths

/// What the walk down found at the end of its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spot {
    /// The key is at this slot of the last node (bitmap or collision).
    Found(usize),
    /// Absent; this bit of the last bitmap node is free.
    Free(u32),
    /// Absent; this bit of the last bitmap node holds another key, whose
    /// entry starts at this slot.
    Clash(u32, usize),
    /// Absent from the last node, a collision node.
    Appended,
}

/// The nodes from the top down to where a key is or would go, with the
/// child index taken at each step, and what was found there.
#[derive(Debug)]
pub(super) struct Path {
    hash: u64,
    nodes: Vec<Word>,
    steps: Vec<usize>,
    spot: Spot,
}

impl Path {
    /// Walks down from `top` (read-only) towards `key`.
    fn walk(top: Bitmap, coll: Collection, key: Word, hash: u64) -> Self {
        let stride = coll.stride();
        let mut path = Self {
            hash,
            nodes: vec![top.0],
            steps: Vec::new(),
            spot: Spot::Appended,
        };
        let mut shift = 0;
        let mut node = top.0;
        loop {
            if shift >= MAX_SHIFT {
                let leaf = Collision(node);
                path.spot = (0..leaf.len())
                    .map(|e| e.wrapping_mul(stride))
                    .find(|&s| keys_equal(leaf.get(s), key))
                    .map_or(Spot::Appended, Spot::Found);
                break;
            }
            let here = Bitmap(node);
            let bit = bit_at(hash, shift);
            let (data, nodes) = here.maps();
            if data & bit != 0 {
                let s = index(data, bit).wrapping_mul(stride);
                path.spot = if keys_equal(here.get(s), key) {
                    Spot::Found(s)
                } else {
                    Spot::Clash(bit, s)
                };
                break;
            }
            if nodes & bit == 0 {
                path.spot = Spot::Free(bit);
                break;
            }
            let ni = index(nodes, bit);
            node = here.child(coll, ni);
            path.steps.push(ni);
            path.nodes.push(node);
            shift = shift.wrapping_add(BITS);
        }
        path
    }

    /// The shift of the last node.
    fn last_shift(&self) -> u32 {
        let depth = u32::try_from(self.nodes.len().saturating_sub(1)).unwrap_or(u32::MAX);
        depth.saturating_mul(BITS)
    }

    /// Whether the last node is a collision node.
    fn ends_in_collision(&self) -> bool {
        self.last_shift() >= MAX_SHIFT
    }

    /// Makes every node on the path writable, from the top, under `root`
    /// (owned by this code, unique or fresh): the last node gets room for
    /// `extra` more slots. Replaced nodes are written into their parents.
    fn make_writable(&mut self, root: Root, coll: Collection, extra: usize) {
        let last = self.nodes.len().saturating_sub(1);
        let collision = self.ends_in_collision();
        for i in 0..self.nodes.len() {
            let Some(&old) = self.nodes.get(i) else {
                break;
            };
            let slack = if i == last { extra } else { 0 };
            let new = if collision && i == last {
                Collision(old).writable(coll, slack).0
            } else {
                Bitmap(old).writable(coll, slack).0
            };
            if new == old {
                continue;
            }
            if let Some(slot) = self.nodes.get_mut(i) {
                *slot = new;
            }
            self.relink(root, coll, i, new);
        }
    }

    /// Writes `new` where node `i` of the path hangs: in the root for the
    /// top node, else in its (writable) parent's child slot.
    fn relink(&self, root: Root, coll: Collection, i: usize, new: Word) {
        let Some(parent_index) = i.checked_sub(1) else {
            root.put_node(Bitmap(new));
            return;
        };
        if let (Some(&parent), Some(&ni)) =
            (self.nodes.get(parent_index), self.steps.get(parent_index))
        {
            let parent = Bitmap(parent);
            let at = parent
                .data_len()
                .wrapping_mul(coll.stride())
                .wrapping_add(ni);
            parent.put(at, new);
        }
    }

    /// The last node of the path.
    fn last(&self) -> Word {
        self.nodes.last().copied().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------
// Insertion

/// A root this code may change: the argument itself when it is unique,
/// with a reference added for the result, else a new root sharing its
/// node. The argument must not be empty.
fn writable_root(root: Root, coll: Collection) -> Root {
    if root.is_unique() {
        dup_word(root.word());
        return root;
    }
    let Some(node) = root.node() else {
        return root;
    };
    dup_word(node.0);
    Root::alloc(coll, root.count(), root.seed(), node)
}

/// A one-entry map or set under `seed`, taking the key and value.
fn single(coll: Collection, seed: u64, key: Word, value: Option<Word>) -> Root {
    let hash = key_hash(key, seed);
    let node = Bitmap::alloc(coll, coll.stride());
    node.put(0, key);
    if let Some(value) = value {
        node.put(1, value);
    }
    node.set_maps(bit_at(hash, 0), 0);
    Root::alloc(coll, 1, seed, node)
}

/// The entry `key` (and `value` for a map), taken, written at the end of
/// `path` in `root`, which this code owns and may change. The path was
/// walked in `root` and the key is absent.
fn put_new(root: Root, coll: Collection, path: &mut Path, key: Word, value: Option<Word>) {
    let stride = coll.stride();
    let extra = match path.spot {
        Spot::Free(_) | Spot::Appended => stride,
        Spot::Clash(..) | Spot::Found(_) => 0,
    };
    path.make_writable(root, coll, extra);
    let last = path.last();
    match path.spot {
        Spot::Found(_) => {
            // Callers handle a present key themselves; never reached.
            drop_word(key);
            if let Some(value) = value {
                drop_word(value);
            }
            return;
        }
        Spot::Appended => {
            let leaf = Collision(last);
            let at = leaf.len().wrapping_mul(stride);
            leaf.put(at, key);
            if let Some(value) = value {
                leaf.put(at.wrapping_add(1), value);
            }
            leaf.set_len(leaf.len().wrapping_add(1));
        }
        Spot::Free(bit) => {
            let node = Bitmap(last);
            let (data, nodes) = node.maps();
            let at = index(data, bit).wrapping_mul(stride);
            node.shift(
                at,
                at.wrapping_add(stride),
                node.used(coll).wrapping_sub(at),
            );
            node.put(at, key);
            if let Some(value) = value {
                node.put(at.wrapping_add(1), value);
            }
            node.set_maps(data | bit, nodes);
        }
        Spot::Clash(bit, at) => {
            let node = Bitmap(last);
            let (data, nodes) = node.maps();
            let old_key = node.get(at);
            let old_value = (stride == 2).then(|| node.get(at.wrapping_add(1)));
            let shift = path.last_shift().wrapping_add(BITS);
            let old_hash = key_hash(old_key, root.seed());
            let child = merge(
                coll,
                shift,
                (old_hash, old_key, old_value),
                (path.hash, key, value),
            );
            // Remove the old entry: the data before the child area closes
            // up, then the child goes in at its index.
            let used = node.used(coll);
            node.shift(
                at.wrapping_add(stride),
                at,
                used.wrapping_sub(at).wrapping_sub(stride),
            );
            let data_after = data & !bit;
            let first_child = super::node::count(data_after.count_ones()).wrapping_mul(stride);
            let ci = first_child.wrapping_add(index(nodes, bit));
            let tail = used.wrapping_sub(stride).wrapping_sub(ci);
            node.shift(ci, ci.wrapping_add(1), tail);
            node.put(ci, child);
            node.set_maps(data_after, nodes | bit);
        }
    }
    root.set_count(root.count().wrapping_add(1));
}

/// A new node (a chain of bitmap nodes ending in two entries, or in a
/// collision node) holding two entries whose hashes agree below `shift`,
/// taking their words.
fn merge(
    coll: Collection,
    shift: u32,
    first: (u64, Word, Option<Word>),
    second: (u64, Word, Option<Word>),
) -> Word {
    let stride = coll.stride();
    let (h0, h1) = (first.0, second.0);
    // The shift where the two hashes part, or the collision level.
    let mut split = shift;
    while split < MAX_SHIFT && bit_at(h0, split) == bit_at(h1, split) {
        split = split.wrapping_add(BITS);
    }
    let put =
        |write: &dyn Fn(usize, Word), at: usize, (_, key, value): (u64, Word, Option<Word>)| {
            write(at, key);
            if let Some(value) = value {
                write(at.wrapping_add(1), value);
            }
        };
    let mut node = if split >= MAX_SHIFT {
        let leaf = Collision::alloc(coll, h0, stride.wrapping_mul(2));
        put(&|i, w| leaf.put(i, w), 0, first);
        put(&|i, w| leaf.put(i, w), stride, second);
        leaf.set_len(2);
        leaf.0
    } else {
        let pair = Bitmap::alloc(coll, stride.wrapping_mul(2));
        let (b0, b1) = (bit_at(h0, split), bit_at(h1, split));
        let (low, high) = if b0 < b1 {
            (first, second)
        } else {
            (second, first)
        };
        put(&|i, w| pair.put(i, w), 0, low);
        put(&|i, w| pair.put(i, w), stride, high);
        pair.set_maps(b0 | b1, 0);
        pair.0
    };
    // Wrap in one-child nodes up to `shift`.
    while split > shift {
        split = split.wrapping_sub(BITS);
        let parent = Bitmap::alloc(coll, 1);
        parent.put(0, node);
        parent.set_maps(0, bit_at(h0, split));
        node = parent.0;
    }
    node
}

/// `insert` on a borrowed root: the key (and value) borrowed, the result
/// owned. Keeps the stored key on a replacement, so of two `==` keys that
/// print differently (`0.0` and `-0.0`) the first one stays, and
/// `from_list` keeps the first key with the last value
/// (GAP(map-insert-keeps-key)); a set returns itself when the key is
/// present.
pub(super) fn insert(root: Root, coll: Collection, key: Word, value: Option<Word>) -> Root {
    dup_word(key);
    if let Some(value) = value {
        dup_word(value);
    }
    let Some(top) = root.node() else {
        return single(coll, crate::entry::current_seed(), key, value);
    };
    let mut path = Path::walk(top, coll, key, key_hash(key, root.seed()));
    if let (Spot::Found(_), None) = (path.spot, value) {
        drop_word(key);
        dup_word(root.word());
        return root;
    }
    let target = writable_root(root, coll);
    put_owned(target, coll, &mut path, key, value);
    target
}

/// Writes the entry `key` (and `value`), taken, at the end of `path` in
/// `root`, which this code owns and may change: replaces the value of a
/// present key (keeping the stored key) or adds the entry.
fn put_owned(root: Root, coll: Collection, path: &mut Path, key: Word, value: Option<Word>) {
    let Spot::Found(at) = path.spot else {
        put_new(root, coll, path, key, value);
        return;
    };
    drop_word(key);
    if let Some(value) = value {
        path.make_writable(root, coll, 0);
        // Bitmap and collision nodes keep their slots at the same offsets.
        let node = Bitmap(path.last());
        let slot = at.wrapping_add(1);
        let old = node.get(slot);
        node.put(slot, value);
        drop_word(old);
    }
}

/// Inserts into a root this code owns uniquely, taking the key and value:
/// the building block of `from_list` and rebuilding.
pub(super) fn insert_owned(root: Root, coll: Collection, key: Word, value: Option<Word>) {
    let Some(top) = root.node() else {
        return;
    };
    let mut path = Path::walk(top, coll, key, key_hash(key, root.seed()));
    put_owned(root, coll, &mut path, key, value);
}

/// Builds a map or set under `seed` from owned entries, in place.
pub(super) fn build(
    coll: Collection,
    seed: u64,
    entries: impl IntoIterator<Item = (Word, Option<Word>)>,
) -> Root {
    let mut root: Option<Root> = None;
    for (key, value) in entries {
        match root {
            None => root = Some(single(coll, seed, key, value)),
            Some(root) => insert_owned(root, coll, key, value),
        }
    }
    root.unwrap_or_else(|| Root::empty(coll))
}

// ---------------------------------------------------------------------
// Upsert

/// `map::upsert` on a borrowed root with one lookup (D-484, §9.3): calls
/// `update` once with the stored value (moved out when this code owns the
/// path) or `None`, and stores its result. `update` takes ownership of
/// its argument and returns an owned value.
pub(super) fn upsert(root: Root, key: Word, update: impl FnOnce(Option<Word>) -> Word) -> Root {
    let coll = Collection::Map;
    let Some(top) = root.node() else {
        let value = update(None);
        dup_word(key);
        return single(coll, crate::entry::current_seed(), key, Some(value));
    };
    let mut path = Path::walk(top, coll, key, key_hash(key, root.seed()));
    let target = writable_root(root, coll);
    let Spot::Found(at) = path.spot else {
        path.make_writable(target, coll, coll.stride());
        let value = update(None);
        dup_word(key);
        put_new(target, coll, &mut path, key, Some(value));
        return target;
    };
    path.make_writable(target, coll, 0);
    // Bitmap and collision nodes keep their slots at the same offsets.
    let node = Bitmap(path.last());
    let slot = at.wrapping_add(1);
    let old = node.get(slot);
    // The path is this code's alone now, so nothing else can see the hole
    // while `update` runs.
    node.put(slot, HOLE);
    let value = update(Some(old));
    node.put(slot, value);
    target
}

// ---------------------------------------------------------------------
// Removal

/// `remove` on a borrowed root: the key borrowed, the result owned.
pub(super) fn remove(root: Root, coll: Collection, key: Word) -> Root {
    // Hashed first, as in `find`, so a key holding a function panics even
    // when the collection is empty (§3.13, D-233).
    let hash = key_hash(key, root.seed());
    let Some(top) = root.node() else {
        return root;
    };
    let mut path = Path::walk(top, coll, key, hash);
    let Spot::Found(at) = path.spot else {
        dup_word(root.word());
        return root;
    };
    if root.count() <= 1 {
        return Root::empty(coll);
    }
    let target = writable_root(root, coll);
    path.make_writable(target, coll, 0);
    let stride = coll.stride();
    let last = path.last();
    if path.ends_in_collision() {
        let leaf = Collision(last);
        for i in 0..stride {
            drop_word(leaf.get(at.wrapping_add(i)));
        }
        let used = leaf.len().wrapping_mul(stride);
        leaf.shift(
            at.wrapping_add(stride),
            at,
            used.wrapping_sub(at).wrapping_sub(stride),
        );
        leaf.set_len(leaf.len().wrapping_sub(1));
    } else {
        let node = Bitmap(last);
        let (data, nodes) = node.maps();
        for i in 0..stride {
            drop_word(node.get(at.wrapping_add(i)));
        }
        let used = node.used(coll);
        node.shift(
            at.wrapping_add(stride),
            at,
            used.wrapping_sub(at).wrapping_sub(stride),
        );
        let bit = bit_at(path.hash, path.last_shift());
        node.set_maps(data & !bit, nodes);
    }
    target.set_count(target.count().wrapping_sub(1));
    compact(target, coll, &mut path);
    target
}

/// The bit index of the `n`th set bit of `map`.
fn index_of_slot(map: u32, n: usize) -> u32 {
    let mut rest = map;
    for _ in 0..n {
        rest &= rest.wrapping_sub(1);
    }
    rest.trailing_zeros()
}

/// After a removal at the end of `path` (all writable): while the last
/// node below the top holds exactly one entry and no children, moves that
/// entry up into its parent in place of the node (CHAMP's canonical form),
/// so the trie stays as small as if the entry had never been there.
fn compact(root: Root, coll: Collection, path: &mut Path) {
    let stride = coll.stride();
    while path.nodes.len() > 1 {
        let depth = path.nodes.len().wrapping_sub(1);
        let last = path.last();
        let collision = path.ends_in_collision();
        let lone = if collision {
            Collision(last).len() == 1
        } else {
            let node = Bitmap(last);
            node.data_len() == 1 && node.node_len() == 0
        };
        if !lone {
            return;
        }
        // Take the entry's words out of the node and free it.
        let words: Vec<Word> = (0..stride)
            .map(|i| {
                if collision {
                    Collision(last).get(i)
                } else {
                    Bitmap(last).get(i)
                }
            })
            .collect();
        if collision {
            Collision(last).free_shell();
        } else {
            Bitmap(last).free_shell();
        }
        path.nodes.pop();
        let Some(ni) = path.steps.pop() else {
            return;
        };
        // In the parent: the child slot becomes a data entry.
        let parent = Bitmap(path.last()).writable(coll, stride.saturating_sub(1));
        if parent.0 != path.last() {
            if let Some(slot) = path.nodes.last_mut() {
                *slot = parent.0;
            }
            path.relink(root, coll, depth.wrapping_sub(1), parent.0);
        }
        let (data, nodes) = parent.maps();
        let bit = 1_u32 << index_of_slot(nodes, ni);
        let used = parent.used(coll);
        let first_child = parent.data_len().wrapping_mul(stride);
        let ci = first_child.wrapping_add(ni);
        // Close up the child slot.
        parent.shift(
            ci.wrapping_add(1),
            ci,
            used.wrapping_sub(ci).wrapping_sub(1),
        );
        // Open room for the entry among the data.
        let at = index(data, bit).wrapping_mul(stride);
        let moved = used.wrapping_sub(1).wrapping_sub(at);
        parent.shift(at, at.wrapping_add(stride), moved);
        for (i, word) in words.into_iter().enumerate() {
            parent.put(at.wrapping_add(i), word);
        }
        parent.set_maps(data | bit, nodes & !bit);
    }
}

// ---------------------------------------------------------------------
// Iteration

/// The entries of a borrowed root in node order (a node's own entries,
/// then its children's, depth first), each borrowed, with an explicit
/// stack. Valid while the root is.
#[derive(Debug)]
pub(super) struct Entries {
    coll: Collection,
    stack: Vec<(Word, u32)>,
    node: Word,
    collision: bool,
    next: usize,
    end: usize,
}

impl Entries {
    pub(super) fn new(root: Root, coll: Collection) -> Self {
        Self {
            coll,
            stack: root.node().map(|top| (top.0, 0)).into_iter().collect(),
            node: 0,
            collision: false,
            next: 0,
            end: 0,
        }
    }
}

impl Iterator for Entries {
    type Item = (Word, Option<Word>);

    fn next(&mut self) -> Option<Self::Item> {
        let stride = self.coll.stride();
        while self.next >= self.end {
            let (node, shift) = self.stack.pop()?;
            self.node = node;
            self.next = 0;
            if shift >= MAX_SHIFT {
                self.collision = true;
                self.end = Collision(node).len().wrapping_mul(stride);
                continue;
            }
            self.collision = false;
            let here = Bitmap(node);
            self.end = here.data_len().wrapping_mul(stride);
            let children = here.node_len();
            let below = shift.wrapping_add(BITS);
            for ni in (0..children).rev() {
                self.stack.push((here.child(self.coll, ni), below));
            }
        }
        let get = |i: usize| {
            if self.collision {
                Collision(self.node).get(i)
            } else {
                Bitmap(self.node).get(i)
            }
        };
        let key = get(self.next);
        let value = (stride == 2).then(|| get(self.next.wrapping_add(1)));
        self.next = self.next.wrapping_add(stride);
        Some((key, value))
    }
}
