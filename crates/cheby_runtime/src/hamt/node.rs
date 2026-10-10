//! The objects a map or set is made of (layout ids 32-37) and the only
//! code that reads or writes their memory.
//!
//! - **Root** (id 32 for a `Map`, 33 for a `Set`): the value. Header, then
//!   the entry count and the seed (both raw), then the top node (a bitmap
//!   node, 0 for none). A heap root always holds at least one entry; the
//!   empty map and set are the static roots of [`empty`], which have no
//!   seed (plan §4.3.5, D-538).
//! - **Bitmap node** (34 map, 35 set): a CHAMP node (Steindorfer and
//!   Vinju, 2015). Header, then one raw word holding the data map (low 32
//!   bits) and the node map (high 32 bits), one raw word holding the slot
//!   capacity, then the slots: `stride` words per entry for the entries
//!   whose hash bits at this level are set in the data map (key, then the
//!   value for a map), in bit order, then one word per child node for the
//!   bits set in the node map, in bit order. A level consumes 5 bits of the
//!   64-bit hash, so levels 0 to 12 are bitmap nodes.
//! - **Collision node** (36 map, 37 set): below level 12, where every hash
//!   bit is used. Header, the hash all its entries share (raw), a raw word
//!   holding the entry count (low 32 bits) and the slot capacity (high 32
//!   bits), then `stride` words per entry.
//!
//! Ids 38 and 39 stay unused. A node's capacity may exceed what it uses, so
//! a unique node grows in place by doubling; the allocator learns sizes
//! from the capacity and children from the maps or the count, so freeing
//! any map goes through its worklist without recursion (plan §0).
//!
//! # Invariant
//!
//! A [`Root`], [`Bitmap`] or [`Collision`] handle refers to a live object
//! of its kind while the code holding it uses it. Handles are made from
//! foreign words only by [`Root::from_word`], whose callers guarantee that;
//! every other handle comes from a fresh allocation or a slot of a live
//! object.

use std::cell::UnsafeCell;
use std::sync::Once;

use super::compare;
use crate::abi::{self, LayoutId, Word};
use crate::alloc;
use crate::layout::{self, CustomKind, LayoutInfo};
use crate::object::{self, ptr_to_word, read_field, to_u64, to_usize, word_to_ptr, write_field};
use crate::panic::fatal;
use crate::rc;

/// Hash bits consumed per level.
pub(super) const BITS: u32 = 5;
/// The shift at which bitmap nodes end and collision nodes start.
pub(super) const MAX_SHIFT: u32 = 64;

/// Which of the two collections a root or node belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collection {
    /// `Map<K, V>`: entries are a key and a value.
    Map,
    /// `Set<T>`: entries are a key alone.
    Set,
}

impl Collection {
    /// Words per entry.
    pub(super) const fn stride(self) -> usize {
        match self {
            Self::Map => 2,
            Self::Set => 1,
        }
    }

    const fn root_id(self) -> LayoutId {
        match self {
            Self::Map => MAP_ROOT,
            Self::Set => SET_ROOT,
        }
    }

    const fn bitmap_id(self) -> LayoutId {
        match self {
            Self::Map => MAP_NODE,
            Self::Set => SET_NODE,
        }
    }

    const fn collision_id(self) -> LayoutId {
        match self {
            Self::Map => MAP_COLLISION,
            Self::Set => SET_COLLISION,
        }
    }
}

/// The layout ids (plan §4.3.2: 32-39 for maps and sets).
pub(super) const MAP_ROOT: LayoutId = abi::layout::MAP_FIRST;
pub(super) const SET_ROOT: LayoutId = MAP_ROOT.wrapping_add(1);
pub(super) const MAP_NODE: LayoutId = MAP_ROOT.wrapping_add(2);
pub(super) const SET_NODE: LayoutId = MAP_ROOT.wrapping_add(3);
pub(super) const MAP_COLLISION: LayoutId = MAP_ROOT.wrapping_add(4);
pub(super) const SET_COLLISION: LayoutId = MAP_ROOT.wrapping_add(5);

const _: () = assert!(SET_COLLISION <= abi::layout::MAP_LAST);

/// Offsets of a root's fields, and its size.
const ROOT_COUNT: usize = 8;
const ROOT_SEED: usize = 16;
const ROOT_NODE: usize = 24;
const ROOT_SIZE: usize = 32;
/// Offsets of a node's two raw words and of its first slot.
const META: usize = 8;
const META2: usize = 16;
const SLOTS: usize = 24;

/// The byte offset of slot `i` of a node.
const fn slot(i: usize) -> usize {
    SLOTS.wrapping_add(i.wrapping_mul(8))
}

/// Packs two 32-bit halves into a meta word.
fn pack(low: usize, high: usize) -> u64 {
    let low = u32::try_from(low).unwrap_or_else(|_| fatal(format_args!("map meta overflow")));
    let high = u32::try_from(high).unwrap_or_else(|_| fatal(format_args!("map meta overflow")));
    u64::from(low) | (u64::from(high) << 32)
}

/// Splits a meta word into its 32-bit halves.
fn unpack(meta: u64) -> (u32, u32) {
    let [l0, l1, l2, l3, h0, h1, h2, h3] = meta.to_le_bytes();
    (
        u32::from_le_bytes([l0, l1, l2, l3]),
        u32::from_le_bytes([h0, h1, h2, h3]),
    )
}

/// A 32-bit count as a `usize`.
pub(super) fn count(bits: u32) -> usize {
    to_usize(u64::from(bits))
}

/// The layout id of a live object.
///
/// # Safety
///
/// `word` is a live heap object.
unsafe fn layout_id(word: Word) -> LayoutId {
    // SAFETY: forwarded from the caller.
    unsafe { object::scan_and_layout(word_to_ptr(word)) }.1
}

// ---------------------------------------------------------------------
// Layout registration

unsafe fn root_size(_: Word) -> u64 {
    to_u64(ROOT_SIZE)
}

unsafe fn root_children(word: Word, visit: &mut dyn FnMut(Word)) {
    // SAFETY: the allocator passes a live root.
    visit(unsafe { read_field(word_to_ptr(word), ROOT_NODE) });
}

/// A node's size follows its capacity (bitmap and collision nodes alike).
unsafe fn node_size(word: Word) -> u64 {
    // SAFETY: the allocator passes a live node.
    let id = unsafe { layout_id(word) };
    let cap = if id == MAP_NODE || id == SET_NODE {
        // SAFETY: a live bitmap node.
        to_usize(unsafe { read_field(word_to_ptr(word), META2) })
    } else {
        // SAFETY: a live collision node.
        count(unpack(unsafe { read_field(word_to_ptr(word), META2) }).1)
    };
    to_u64(slot(cap))
}

/// A node owns its used slots.
unsafe fn node_children(word: Word, visit: &mut dyn FnMut(Word)) {
    // SAFETY: the allocator passes a live node.
    let used = match unsafe { layout_id(word) } {
        MAP_NODE => Bitmap(word).used(Collection::Map),
        SET_NODE => Bitmap(word).used(Collection::Set),
        MAP_COLLISION => Collision(word).len().wrapping_mul(2),
        _ => Collision(word).len(),
    };
    let ptr = word_to_ptr(word);
    for i in 0..used {
        // SAFETY: the used slots of a live node are initialized.
        visit(unsafe { read_field(ptr, slot(i)) });
    }
}

/// The map root kind: equality and hashing ignore order and seeds
/// (D-538). Debug text is W3-B's sorted printing (D-344, D-382).
static MAP_ROOT_KIND: CustomKind = CustomKind {
    name: "Map",
    size: root_size,
    children: root_children,
    equal: Some(compare::equal),
    hash: Some(compare::hash),
    debug: None,
};

/// The set root kind.
static SET_ROOT_KIND: CustomKind = CustomKind {
    name: "Set",
    size: root_size,
    children: root_children,
    equal: Some(compare::equal),
    hash: Some(compare::hash),
    debug: None,
};

/// Node kinds are never values of their own.
static MAP_NODE_KIND: CustomKind = node_kind("Map.Node");
static SET_NODE_KIND: CustomKind = node_kind("Set.Node");
static MAP_COLLISION_KIND: CustomKind = node_kind("Map.Collision");
static SET_COLLISION_KIND: CustomKind = node_kind("Set.Collision");

const fn node_kind(name: &'static str) -> CustomKind {
    CustomKind {
        name,
        size: node_size,
        children: node_children,
        equal: None,
        hash: None,
        debug: None,
    }
}

/// Registers the six kinds once per process. Every entry point that may
/// allocate, read or free map objects calls it first.
pub(crate) fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for (id, kind) in [
            (MAP_ROOT, &MAP_ROOT_KIND),
            (SET_ROOT, &SET_ROOT_KIND),
            (MAP_NODE, &MAP_NODE_KIND),
            (SET_NODE, &SET_NODE_KIND),
            (MAP_COLLISION, &MAP_COLLISION_KIND),
            (SET_COLLISION, &SET_COLLISION_KIND),
        ] {
            if let Err(error) = layout::register_builtin(id, LayoutInfo::custom(kind)) {
                fatal(format_args!("cannot register map layout {id}: {error}"));
            }
        }
    });
}

// ---------------------------------------------------------------------
// The static empty map and set

/// A static root: count 0, no seed, no node, never written.
#[derive(Debug)]
#[repr(C, align(8))]
pub struct StaticRoot {
    // In writable memory, like every static object's header, for the
    // atomic count load (see `object::StaticObject`).
    header: UnsafeCell<u64>,
    count: u64,
    seed: u64,
    node: u64,
}

// SAFETY: the header is never written (its count is `RC_STATIC`, which
// every count operation checks before writing), and the fields are plain
// immutable data.
unsafe impl Sync for StaticRoot {}

/// The empty map: every operation that produces an empty map returns it.
pub static EMPTY_MAP: StaticRoot = StaticRoot {
    header: UnsafeCell::new(abi::header(abi::RC_STATIC, 0, MAP_ROOT)),
    count: 0,
    seed: 0,
    node: 0,
};

/// The empty set.
pub static EMPTY_SET: StaticRoot = StaticRoot {
    header: UnsafeCell::new(abi::header(abi::RC_STATIC, 0, SET_ROOT)),
    count: 0,
    seed: 0,
    node: 0,
};

/// The word of the empty map or set.
pub(super) fn empty(coll: Collection) -> Word {
    register();
    let image = match coll {
        Collection::Map => &EMPTY_MAP,
        Collection::Set => &EMPTY_SET,
    };
    ptr_to_word(std::ptr::from_ref(image).cast::<u8>())
}

// ---------------------------------------------------------------------
// Counts

/// Adds a reference to a key, value or node word.
pub(super) fn dup_word(word: Word) {
    // SAFETY: slot words are tagged integers or live objects (module
    // invariant), and callers pass only such words.
    unsafe { rc::dup(word) };
}

/// Gives up an owned reference to a key, value, node or root word.
pub(super) fn drop_word(word: Word) {
    // SAFETY: as for `dup_word`; the caller owns the reference.
    unsafe { rc::drop(word) };
}

/// Whether the object behind `word` has count exactly one.
fn unique(word: Word) -> bool {
    // SAFETY: handles refer to live objects (module invariant).
    unsafe { rc::is_unique(word) }
}

/// The word left in a slot whose value was moved out (a tagged 0, which
/// freeing skips).
pub(super) const HOLE: Word = 1;

// ---------------------------------------------------------------------
// Roots

/// A map or set root (static or heap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Root(Word);

impl Root {
    /// The root behind a map or set word; `None` when the word is neither.
    ///
    /// # Safety
    ///
    /// `word` is a tagged integer or a live object, live while the handle
    /// is used.
    pub(super) unsafe fn probe(word: Word) -> Option<(Self, Collection)> {
        if abi::is_tagged_int(word) || word == 0 {
            return None;
        }
        register();
        // SAFETY: forwarded from the caller.
        unsafe { alloc::check_live(word, "a map or set operation") };
        // SAFETY: as above.
        match unsafe { layout_id(word) } {
            MAP_ROOT => Some((Self(word), Collection::Map)),
            SET_ROOT => Some((Self(word), Collection::Set)),
            _ => None,
        }
    }

    /// The root behind a word that must be a `coll`.
    ///
    /// # Safety
    ///
    /// `word` is a live map or set value, live while the handle is used.
    pub(super) unsafe fn from_word(word: Word, coll: Collection) -> Self {
        // SAFETY: forwarded from the caller.
        match unsafe { Self::probe(word) } {
            Some((root, found)) if found == coll => root,
            _ => fatal(format_args!(
                "expected a {coll:?}, found the word {word:#x}"
            )),
        }
    }

    /// A new unique heap root holding `count` entries under `node`, which
    /// it takes.
    pub(super) fn alloc(coll: Collection, count: usize, seed: u64, node: Bitmap) -> Self {
        register();
        let ptr = alloc::alloc_object(to_u64(ROOT_SIZE), abi::header(1, 0, coll.root_id()));
        // SAFETY: a fresh root of this size.
        unsafe {
            write_field(ptr, ROOT_COUNT, to_u64(count));
            write_field(ptr, ROOT_SEED, seed);
            write_field(ptr, ROOT_NODE, node.0);
        }
        Self(ptr_to_word(ptr))
    }

    /// The static empty root of `coll` (count 0, needs no reference).
    pub(super) fn empty(coll: Collection) -> Self {
        Self(empty(coll))
    }

    pub(super) fn word(self) -> Word {
        self.0
    }

    fn ptr(self) -> *mut u8 {
        word_to_ptr(self.0)
    }

    /// Whether this reference is the only one (never for the statics).
    pub(super) fn is_unique(self) -> bool {
        unique(self.0)
    }

    /// The number of entries.
    pub(super) fn count(self) -> usize {
        // SAFETY: a live root has this field (module invariant).
        to_usize(unsafe { read_field(self.ptr(), ROOT_COUNT) })
    }

    /// The seed; meaningful only when the root is not empty.
    pub(super) fn seed(self) -> u64 {
        // SAFETY: as above.
        unsafe { read_field(self.ptr(), ROOT_SEED) }
    }

    /// The top node, borrowed; `None` for the empty statics.
    pub(super) fn node(self) -> Option<Bitmap> {
        // SAFETY: as above.
        let node = unsafe { read_field(self.ptr(), ROOT_NODE) };
        (node != 0).then_some(Bitmap(node))
    }

    /// Sets the count of a root this code owns uniquely or just made.
    pub(super) fn set_count(self, count: usize) {
        // SAFETY: a live heap root (the statics are never passed here).
        unsafe { write_field(self.ptr(), ROOT_COUNT, to_u64(count)) };
    }

    /// Replaces the top node without releasing the old one.
    pub(super) fn put_node(self, node: Bitmap) {
        // SAFETY: as above.
        unsafe { write_field(self.ptr(), ROOT_NODE, node.0) };
    }
}

// ---------------------------------------------------------------------
// Bitmap nodes

/// A bitmap node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Bitmap(pub(super) Word);

impl Bitmap {
    /// A new unique node with no entries and room for `cap` slots.
    pub(super) fn alloc(coll: Collection, cap: usize) -> Self {
        register();
        let ptr = alloc::alloc_object(to_u64(slot(cap)), abi::header(1, 0, coll.bitmap_id()));
        // SAFETY: a fresh node of this size.
        unsafe {
            write_field(ptr, META, 0);
            write_field(ptr, META2, to_u64(cap));
        }
        Self(ptr_to_word(ptr))
    }

    fn ptr(self) -> *mut u8 {
        word_to_ptr(self.0)
    }

    pub(super) fn is_unique(self) -> bool {
        unique(self.0)
    }

    /// The data map and the node map.
    pub(super) fn maps(self) -> (u32, u32) {
        // SAFETY: a live node (module invariant).
        unpack(unsafe { read_field(self.ptr(), META) })
    }

    /// Sets both maps of a node this code owns uniquely.
    pub(super) fn set_maps(self, data: u32, nodes: u32) {
        // SAFETY: as above.
        unsafe {
            write_field(self.ptr(), META, u64::from(data) | (u64::from(nodes) << 32));
        };
    }

    /// The slot capacity.
    pub(super) fn cap(self) -> usize {
        // SAFETY: as above.
        to_usize(unsafe { read_field(self.ptr(), META2) })
    }

    /// The number of entries stored in this node.
    pub(super) fn data_len(self) -> usize {
        count(self.maps().0.count_ones())
    }

    /// The number of child nodes.
    pub(super) fn node_len(self) -> usize {
        count(self.maps().1.count_ones())
    }

    /// The number of slots in use.
    pub(super) fn used(self, coll: Collection) -> usize {
        self.data_len()
            .wrapping_mul(coll.stride())
            .wrapping_add(self.node_len())
    }

    /// Slot `i` (below `used`), borrowed.
    pub(super) fn get(self, i: usize) -> Word {
        // SAFETY: used slots of a live node are initialized.
        unsafe { read_field(self.ptr(), slot(i)) }
    }

    /// Writes slot `i` (below the capacity) without releasing what was
    /// there.
    pub(super) fn put(self, i: usize, word: Word) {
        // SAFETY: the caller owns this node uniquely and `i` is below its
        // capacity.
        unsafe { write_field(self.ptr(), slot(i), word) };
    }

    /// Moves slots `from..from + n` to `to..to + n`, within the capacity
    /// of a node this code owns uniquely.
    pub(super) fn shift(self, from: usize, to: usize, n: usize) {
        let ptr = self.ptr();
        // SAFETY: both ranges lie within the capacity; `ptr::copy` allows
        // overlap.
        unsafe { std::ptr::copy(ptr.add(slot(from)), ptr.add(slot(to)), n.wrapping_mul(8)) };
    }

    /// Child `ni`, borrowed.
    pub(super) fn child(self, coll: Collection, ni: usize) -> Word {
        self.get(self.data_len().wrapping_mul(coll.stride()).wrapping_add(ni))
    }

    /// Copies this node's used slots into a new unique node with room for
    /// `extra` more, adding a reference to each, then gives up this
    /// reference.
    pub(super) fn copy(self, coll: Collection, extra: usize) -> Self {
        let used = self.used(coll);
        let copy = Self::alloc(coll, used.saturating_add(extra));
        for i in 0..used {
            let word = self.get(i);
            dup_word(word);
            copy.put(i, word);
        }
        let (data, nodes) = self.maps();
        copy.set_maps(data, nodes);
        drop_word(self.0);
        copy
    }

    /// Moves this unique node's slots into a new node of capacity `cap`
    /// and frees this one's memory alone.
    pub(super) fn grow(self, coll: Collection, cap: usize) -> Self {
        let used = self.used(coll);
        let grown = Self::alloc(coll, cap);
        // SAFETY: both nodes hold at least `used` slots and are distinct.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.ptr().add(SLOTS),
                grown.ptr().add(SLOTS),
                used.wrapping_mul(8),
            );
        }
        let (data, nodes) = self.maps();
        grown.set_maps(data, nodes);
        self.free_shell();
        grown
    }

    /// Frees this unique node's memory without releasing its slots, which
    /// the caller has moved elsewhere.
    pub(super) fn free_shell(self) {
        self.set_maps(0, 0);
        drop_word(self.0);
    }

    /// This node, unique and with room for `extra` more slots: itself when
    /// it already is, grown when it is unique but full, else a copy. The
    /// caller owns the reference and gets the result's.
    pub(super) fn writable(self, coll: Collection, extra: usize) -> Self {
        let need = self.used(coll).saturating_add(extra);
        if !self.is_unique() {
            return self.copy(coll, extra);
        }
        if need <= self.cap() {
            return self;
        }
        let most = coll.stride().wrapping_mul(32).max(need);
        self.grow(coll, self.cap().saturating_mul(2).clamp(need, most))
    }
}

// ---------------------------------------------------------------------
// Collision nodes

/// A collision node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Collision(pub(super) Word);

impl Collision {
    /// A new unique collision node for `hash` with no entries and room for
    /// `cap` slots.
    pub(super) fn alloc(coll: Collection, hash: u64, cap: usize) -> Self {
        register();
        let ptr = alloc::alloc_object(to_u64(slot(cap)), abi::header(1, 0, coll.collision_id()));
        // SAFETY: a fresh node of this size.
        unsafe {
            write_field(ptr, META, hash);
            write_field(ptr, META2, pack(0, cap));
        }
        Self(ptr_to_word(ptr))
    }

    fn ptr(self) -> *mut u8 {
        word_to_ptr(self.0)
    }

    pub(super) fn is_unique(self) -> bool {
        unique(self.0)
    }

    /// The hash every entry shares.
    pub(super) fn hash(self) -> u64 {
        // SAFETY: a live node (module invariant).
        unsafe { read_field(self.ptr(), META) }
    }

    /// The number of entries.
    pub(super) fn len(self) -> usize {
        // SAFETY: as above.
        count(unpack(unsafe { read_field(self.ptr(), META2) }).0)
    }

    /// The slot capacity.
    pub(super) fn cap(self) -> usize {
        // SAFETY: as above.
        count(unpack(unsafe { read_field(self.ptr(), META2) }).1)
    }

    /// Sets the entry count of a node this code owns uniquely.
    pub(super) fn set_len(self, len: usize) {
        let cap = self.cap();
        // SAFETY: as above.
        unsafe { write_field(self.ptr(), META2, pack(len, cap)) };
    }

    /// Slot `i`, borrowed.
    pub(super) fn get(self, i: usize) -> Word {
        // SAFETY: used slots of a live node are initialized.
        unsafe { read_field(self.ptr(), slot(i)) }
    }

    /// Writes slot `i` without releasing what was there.
    pub(super) fn put(self, i: usize, word: Word) {
        // SAFETY: the caller owns this node uniquely and `i` is below its
        // capacity.
        unsafe { write_field(self.ptr(), slot(i), word) };
    }

    /// Moves slots within a node this code owns uniquely.
    pub(super) fn shift(self, from: usize, to: usize, n: usize) {
        let ptr = self.ptr();
        // SAFETY: both ranges lie within the capacity.
        unsafe { std::ptr::copy(ptr.add(slot(from)), ptr.add(slot(to)), n.wrapping_mul(8)) };
    }

    /// This node, unique and with room for `extra` more slots (see
    /// [`Bitmap::writable`]).
    pub(super) fn writable(self, coll: Collection, extra: usize) -> Self {
        let used = self.len().wrapping_mul(coll.stride());
        let need = used.saturating_add(extra);
        if self.is_unique() && need <= self.cap() {
            return self;
        }
        let cap = if self.is_unique() {
            self.cap().saturating_mul(2).max(need)
        } else {
            need
        };
        let copy = Self::alloc(coll, self.hash(), cap);
        let shared = !self.is_unique();
        for i in 0..used {
            let word = self.get(i);
            if shared {
                dup_word(word);
            }
            copy.put(i, word);
        }
        copy.set_len(self.len());
        if shared {
            drop_word(self.0);
        } else {
            self.set_len(0);
            drop_word(self.0);
        }
        copy
    }

    /// Frees this unique node's memory without releasing its slots.
    pub(super) fn free_shell(self) {
        self.set_len(0);
        drop_word(self.0);
    }
}
