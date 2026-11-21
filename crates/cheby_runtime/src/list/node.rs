//! The objects a list is made of (layout ids 26-29, D-524) and the only
//! code that reads or writes their memory.
//!
//! - **Root** (id 26, [`ROOT`]): the list value. Header, then the tree
//!   (a leaf or branch word, 0 for none), the view's offset into the tree
//!   and its length, both raw. A root is a view, so slicing allocates one
//!   root and shares the tree (D-053). An owned slice clears the slots it
//!   cuts to the tagged `Int` 0 where their path is unique, or, for a cut
//!   of more than four elements from a unique tree, rebuilds the tree to
//!   hold just the slice (D-893); a slot under a shared node keeps its
//!   element. A heap root always has a
//!   length above 0 and a tree; the empty list is the static root
//!   `cheby_rt_static_empty_list`. Only a unique root that a list
//!   outgrew is ever empty, for the moment between the move of its
//!   contents to a big root and its release.
//! - **Big root** (id 29, [`BIG_ROOT`], D-1757): the list value too, for
//!   a list of more than one leaf that grows or shrinks at an end. A root's
//!   fields, then the head leaf (0 for none), the index in it of the
//!   list's first element, raw, and the tail leaf (0 for none). The list
//!   is the head leaf's elements from that index, then the elements the
//!   root's offset and length take from the tree, then the tail leaf's
//!   ([`Pieces`]). The two leaves keep the list's ends out of the tree,
//!   so a push, a prepend or a pattern step at an end reads and writes
//!   one leaf, and the tree changes once per leaf (`buffers`). A big
//!   root has a head or tail leaf only while its count is positive.
//! - **Leaf** (id 27, [`LEAF`]): header, a raw word holding the length
//!   (low 32 bits) and the capacity (high 32 bits), then `capacity`
//!   element slots. The first `length` slots hold owned uniform words.
//!   Leaves hold at most [`M`] elements; a unique leaf grows its capacity
//!   by doubling, so pushing at count one copies each element O(1) times.
//! - **Branch** (id 28, [`BRANCH`]): header, a raw word holding the child
//!   count (low 32 bits) and the height (high 32 bits; leaves are height
//!   0), then [`M`] child slots (the first `count` own a node of height
//!   `height - 1`), then [`M`] cumulative sizes (raw element counts: slot
//!   `i` holds the number of elements in children `0..=i`).
//!
//! Every branch is relaxed: it keeps its size table, and a lookup starts
//! from the radix position and searches the table, so balanced and
//! relaxed nodes share one kind.
//!
//! A big root's tree may hold holes: child slots holding the word 0,
//! where a walk from the front took a leaf, or a branch whose leaves it
//! took, out of a unique tree (`buffers`). The sizes still count the
//! elements that were there, and the root's offset lies past them, so a
//! lookup from the offset never reaches a hole, and freeing a node skips
//! the word 0. Holes are only ever the first children of the branches on
//! the path to the offset's leaf, and only in unique branches. A big
//! root's tree loses its holes when its leaves are folded
//! (`buffers::settle`): before any operation but `get` and those at the
//! list's ends reads it, and before it is shared or made static.
//!
//! Ids 30-31 stay unused. The [`CustomKind`]s below give the allocator
//! each object's size and owned children, so freeing a list of any length
//! goes through the allocator's worklist without recursion (plan §0).
//!
//! # Invariant
//!
//! A [`Node`] or [`Root`] handle refers to a live object of its kind for
//! as long as the code holding it uses it. Handles are made from foreign
//! words only by the `unsafe` constructors, whose callers (the C entry
//! points) guarantee that; every other handle comes from a fresh
//! allocation or from a slot of a live object.

use std::cell::UnsafeCell;
use std::sync::Once;

use crate::abi::{self, LayoutId, Word};
use crate::alloc;
use crate::generic;
use crate::layout::{self, CustomKind, KindShape, LayoutInfo};
use crate::object::{self, ptr_to_word, read_field, to_u64, to_usize, word_to_ptr, write_field};
use crate::panic::fatal;
use crate::rc;

/// Bits of an index consumed per tree level.
pub(super) const BITS: u32 = 5;
/// The branching factor: children per branch, elements per leaf.
pub(super) const M: usize = 1 << BITS;

/// The layout id of list roots.
pub(super) const ROOT: LayoutId = abi::layout::LIST_FIRST;
/// The layout id of leaves.
pub(super) const LEAF: LayoutId = ROOT.wrapping_add(1);
/// The layout id of branches.
pub(super) const BRANCH: LayoutId = ROOT.wrapping_add(2);
/// The layout id of big roots.
pub(super) const BIG_ROOT: LayoutId = ROOT.wrapping_add(3);

const _: () = assert!(BIG_ROOT <= abi::layout::LIST_LAST);

/// Offset of the first field after the header.
const META: usize = 8;
/// Offset of the first slot of a leaf or branch.
const SLOTS: usize = 16;
/// Offset of a branch's size table.
const SIZES: usize = SLOTS + 8 * M;
/// Size of a branch in bytes.
const BRANCH_SIZE: usize = SIZES + 8 * M;
/// Offsets of a root's fields.
const ROOT_TREE: usize = 8;
const ROOT_OFFSET: usize = 16;
const ROOT_LEN: usize = 24;
// Generated code reads the length there (D-910).
const _: () = assert!(ROOT_LEN == 24 && abi::list::ROOT_LEN_OFFSET == 24);
// Generated code works on the leaf at either end of a unique list
// (`abi::list`).
const _: () = assert!(
    abi::list::ROOT_TREE_OFFSET == 8
        && ROOT_TREE == 8
        && abi::list::ROOT_OFFSET_OFFSET == 16
        && ROOT_OFFSET == 16
        && abi::list::LEAF_META_OFFSET == 8
        && META == 8
        && abi::list::LEAF_SLOTS_OFFSET == 16
        && SLOTS == 16
        && abi::list::LEAF_LAYOUT == LEAF
        && abi::list::BIG_ROOT_LAYOUT == BIG_ROOT
);
// A big root's further fields: the head leaf (0 for none), the index in
// it of the list's first element, and the tail leaf (0 for none).
const ROOT_HEAD: usize = 32;
const ROOT_HEAD_POS: usize = 40;
const ROOT_TAIL: usize = 48;
const _: () = assert!(
    abi::list::BIG_HEAD_OFFSET == 32
        && abi::list::BIG_HEAD_POS_OFFSET == 40
        && abi::list::BIG_TAIL_OFFSET == 48
);
/// Size of a root in bytes.
pub(super) const ROOT_SIZE: usize = 32;
/// Size of a big root in bytes.
pub(super) const BIG_ROOT_SIZE: usize = 56;
/// The most bytes one node takes: a branch, larger than any leaf.
pub(super) const MAX_NODE_SIZE: usize = BRANCH_SIZE;

/// The byte offset of slot `i` of a leaf or branch.
const fn slot(i: usize) -> usize {
    SLOTS.wrapping_add(i.wrapping_mul(8))
}

/// The byte offset of size entry `i` of a branch.
const fn size_slot(i: usize) -> usize {
    SIZES.wrapping_add(i.wrapping_mul(8))
}

/// Packs two 32-bit halves into a meta word.
fn pack(low: usize, high: usize) -> u64 {
    let low = u32::try_from(low).unwrap_or_else(|_| fatal(format_args!("list meta overflow")));
    let high = u32::try_from(high).unwrap_or_else(|_| fatal(format_args!("list meta overflow")));
    u64::from(low) | (u64::from(high) << 32)
}

/// Splits a meta word into its halves.
fn unpack(meta: u64) -> (usize, usize) {
    let [l0, l1, l2, l3, h0, h1, h2, h3] = meta.to_le_bytes();
    let low = u32::from_le_bytes([l0, l1, l2, l3]);
    let high = u32::from_le_bytes([h0, h1, h2, h3]);
    (to_usize(u64::from(low)), to_usize(u64::from(high)))
}

// ---------------------------------------------------------------------
// Layout registration

/// Frees a root: its size.
unsafe fn root_size(_: Word) -> u64 {
    to_u64(ROOT_SIZE)
}

/// A root owns its tree.
unsafe fn root_children(word: Word, visit: &mut dyn FnMut(Word)) {
    // SAFETY: the allocator passes a live root.
    visit(unsafe { read_field(word_to_ptr(word), ROOT_TREE) });
}

/// Frees a big root: its size.
unsafe fn big_root_size(_: Word) -> u64 {
    to_u64(BIG_ROOT_SIZE)
}

/// A big root owns its tree and its head and tail leaves.
unsafe fn big_root_children(word: Word, visit: &mut dyn FnMut(Word)) {
    let ptr = word_to_ptr(word);
    // SAFETY: the allocator passes a live big root.
    unsafe {
        visit(read_field(ptr, ROOT_TREE));
        visit(read_field(ptr, ROOT_HEAD));
        visit(read_field(ptr, ROOT_TAIL));
    }
}

/// A leaf's size follows its capacity.
unsafe fn leaf_size(word: Word) -> u64 {
    // SAFETY: the allocator passes a live leaf.
    let (_, cap) = unpack(unsafe { read_field(word_to_ptr(word), META) });
    to_u64(slot(cap))
}

/// A leaf owns its first `length` elements.
unsafe fn leaf_children(word: Word, visit: &mut dyn FnMut(Word)) {
    let ptr = word_to_ptr(word);
    // SAFETY: the allocator passes a live leaf; its first `len` slots are
    // initialized.
    let (len, _) = unpack(unsafe { read_field(ptr, META) });
    for i in 0..len {
        // SAFETY: as above.
        visit(unsafe { read_field(ptr, slot(i)) });
    }
}

/// Branches have one size.
unsafe fn branch_size(_: Word) -> u64 {
    to_u64(BRANCH_SIZE)
}

/// A branch owns its first `count` children.
unsafe fn branch_children(word: Word, visit: &mut dyn FnMut(Word)) {
    let ptr = word_to_ptr(word);
    // SAFETY: the allocator passes a live branch; its first `count` child
    // slots are initialized.
    let (count, _) = unpack(unsafe { read_field(ptr, META) });
    for i in 0..count {
        // SAFETY: as above.
        visit(unsafe { read_field(ptr, slot(i)) });
    }
}

/// Equality of two lists (§3.13): the same length, and equal elements in
/// order, which the walk compares after this call.
///
/// # Safety `a` and `b` are live list roots.
unsafe fn root_equal(a: Word, b: Word, cx: &mut generic::Cx) -> bool {
    // SAFETY: forwarded from the caller.
    let (xs, ys) = unsafe { (super::iter(a), super::iter(b)) };
    if xs.len() != ys.len() {
        return false;
    }
    // The elements stay valid after the iterators release their references:
    // the walk borrows both lists for its whole run.
    for (x, y) in xs.zip(ys) {
        cx.push(x, y);
    }
    true
}

/// Hashing of a list: its length, then its elements in order.
///
/// # Safety `list` is a live list root.
unsafe fn root_hash(list: Word, h: &mut generic::Hasher) {
    // SAFETY: forwarded from the caller.
    let items = unsafe { super::iter(list) };
    h.write_u8(generic::mark::LIST);
    h.write_u64(to_u64(items.len()));
    for item in items {
        h.push(item);
    }
}

/// Debug text of a list: `[1, 2, 3]` (§3.13).
///
/// # Safety `list` is a live list root.
unsafe fn root_debug(list: Word, p: &mut generic::Printer) {
    // SAFETY: forwarded from the caller.
    let items = unsafe { super::iter(list) };
    p.text("[");
    p.separated(items);
    p.text("]");
}

/// The list root kind: a list value, compared, hashed and printed element
/// by element through [`super::iter`].
static ROOT_KIND: CustomKind = CustomKind {
    name: "List",
    size: root_size,
    children: root_children,
    equal: Some(root_equal),
    hash: Some(root_hash),
    debug: Some(root_debug),
    handle: false,
    finalize: None,
    shape: KindShape::Fixed {
        size: ROOT_SIZE,
        fields: 1,
    },
};

/// The big root kind: a list value like a root. Its owned words are not
/// the fields from offset 8 on, so it has no declared shape.
static BIG_ROOT_KIND: CustomKind = CustomKind {
    name: "List",
    size: big_root_size,
    children: big_root_children,
    equal: Some(root_equal),
    hash: Some(root_hash),
    debug: Some(root_debug),
    handle: false,
    finalize: None,
    shape: KindShape::Opaque,
};

/// The leaf kind: never a value of its own.
static LEAF_KIND: CustomKind = CustomKind {
    name: "List.Leaf",
    size: leaf_size,
    children: leaf_children,
    equal: None,
    hash: None,
    debug: None,
    handle: false,
    finalize: None,
    shape: KindShape::Counted { size: None },
};

/// The branch kind: never a value of its own.
static BRANCH_KIND: CustomKind = CustomKind {
    name: "List.Branch",
    size: branch_size,
    children: branch_children,
    equal: None,
    hash: None,
    debug: None,
    handle: false,
    finalize: None,
    shape: KindShape::Counted {
        size: Some(BRANCH_SIZE),
    },
};

/// Registers the four kinds once per process. Every entry point that may
/// allocate or free list objects calls it first.
pub(crate) fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for (id, kind) in [
            (ROOT, &ROOT_KIND),
            (LEAF, &LEAF_KIND),
            (BRANCH, &BRANCH_KIND),
            (BIG_ROOT, &BIG_ROOT_KIND),
        ] {
            if let Err(error) = layout::register_builtin(id, LayoutInfo::custom(kind)) {
                fatal(format_args!("cannot register list layout {id}: {error}"));
            }
        }
    });
}

// ---------------------------------------------------------------------
// The static empty list

/// A static list root: count 0, never written.
#[derive(Debug)]
#[repr(C, align(8))]
pub struct StaticList {
    // In writable memory, like every static object's header, for the
    // atomic count load (see `object::StaticObject`).
    header: UnsafeCell<u64>,
    tree: u64,
    offset: u64,
    len: u64,
}

// A heap root's layout, which generated code reads the length from
// (D-910).
const _: () = assert!(std::mem::offset_of!(StaticList, len) == ROOT_LEN);

// SAFETY: the header is never written (its count is `RC_STATIC`, which
// every count operation checks before writing), and the fields are plain
// immutable data.
unsafe impl Sync for StaticList {}

/// The empty list: a static root with no tree (plan §4.3.1, D-524). Every
/// operation that produces an empty list returns it.
#[unsafe(no_mangle)]
pub static cheby_rt_static_empty_list: StaticList = StaticList {
    header: UnsafeCell::new(abi::header(abi::RC_STATIC, 0, ROOT)),
    tree: 0,
    offset: 0,
    len: 0,
};

/// The word of the static empty list.
pub(crate) fn empty() -> Word {
    ptr_to_word(std::ptr::from_ref(&cheby_rt_static_empty_list).cast::<u8>())
}

// ---------------------------------------------------------------------
// Counts

/// Whether the object behind `word` has count exactly one.
fn unique(word: Word) -> bool {
    // SAFETY: handles refer to live objects (module invariant).
    unsafe { rc::is_unique(word) }
}

/// Adds a reference to an element or node word.
pub(super) fn dup_word(word: Word) {
    // SAFETY: element and node words are tagged integers or live objects
    // (module invariant).
    unsafe { rc::dup(word) };
}

/// Gives up an owned reference to an element or node word.
pub(super) fn drop_word(word: Word) {
    // SAFETY: as for `dup_word`; the caller owns the reference.
    unsafe { rc::drop(word) };
}

// ---------------------------------------------------------------------
// Roots

/// A list root: the static empty list or a heap root (module invariant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Root(Word);

/// The fields of a root.
#[derive(Debug, Clone, Copy)]
pub(super) struct View {
    /// The tree, absent only for the empty list.
    pub(super) tree: Option<Node>,
    /// The index in the tree of the view's first element.
    pub(super) offset: usize,
    /// The number of elements in the view.
    pub(super) len: usize,
}

/// What a big root holds: the list is the head leaf's elements from `pos`
/// on, then the tree's elements from `offset` on, as many as the length
/// leaves for them, then the tail leaf's elements.
#[derive(Debug, Clone, Copy)]
pub(super) struct Pieces {
    /// The tree, present only while it holds a part of the list.
    pub(super) tree: Option<Node>,
    /// The index in the tree of the first element the list takes from it.
    pub(super) offset: usize,
    /// The number of elements of the whole list.
    pub(super) len: usize,
    /// The head leaf: never one with no element left.
    pub(super) head: Option<Node>,
    /// The index in the head leaf of the list's first element.
    pub(super) pos: usize,
    /// The tail leaf: unique, and never empty.
    pub(super) tail: Option<Node>,
}

impl Pieces {
    /// The pieces of a list that is a view of a tree.
    pub(super) fn of(view: View) -> Self {
        Self {
            tree: view.tree,
            offset: view.offset,
            len: view.len,
            head: None,
            pos: 0,
            tail: None,
        }
    }

    /// The elements the head leaf still holds.
    pub(super) fn head_left(&self) -> usize {
        self.head
            .map_or(0, |head| head.slots().saturating_sub(self.pos))
    }

    /// The elements the tail leaf holds.
    pub(super) fn tail_len(&self) -> usize {
        self.tail.map_or(0, Node::slots)
    }

    /// The elements the list takes from the tree.
    pub(super) fn tree_len(&self) -> usize {
        self.len
            .saturating_sub(self.head_left())
            .saturating_sub(self.tail_len())
    }
}

/// A unique heap root by its kind, for the fast paths of the C entry
/// points ([`Root::fast`]).
#[derive(Debug, Clone, Copy)]
pub(super) enum Fast {
    /// A root: a view of a tree.
    Small(Root),
    /// A big root.
    Big(Big),
}

impl Fast {
    /// The list value.
    pub(super) fn root(self) -> Root {
        match self {
            Self::Small(root) => root,
            Self::Big(big) => big.root(),
        }
    }
}

impl Root {
    /// The root behind a list word.
    ///
    /// # Safety
    ///
    /// `word` is a live list value (a root, static or heap), and stays live
    /// while the handle is used.
    pub(crate) unsafe fn from_word(word: Word) -> Self {
        if abi::is_tagged_int(word) || word == 0 {
            fatal(format_args!("expected a list, found the word {word:#x}"));
        }
        // The poisoning check needs the layouts.
        register();
        // SAFETY: forwarded from the caller.
        unsafe { alloc::check_live(word, "a list operation") };
        // SAFETY: as above.
        let (_, id) = unsafe { object::scan_and_layout(word_to_ptr(word)) };
        if !is_root(id) {
            fatal(format_args!(
                "expected a list, found an object of layout {id}"
            ));
        }
        Self(word)
    }

    /// The root behind a list word whose count is exactly one, by its
    /// kind; `None` for any other word. The count is read first: the
    /// static empty list has none, and the poisoning mode's check needs
    /// the list layouts, which are registered once a heap root exists. A
    /// freed object's poisoned header has another count, so it takes the
    /// caller's checked path.
    ///
    /// # Safety
    ///
    /// `word` is a tagged integer, 0 or a live object.
    #[expect(
        clippy::inline_always,
        reason = "the first test of every list fast path, which is inlined into its C entry point"
    )]
    #[inline(always)]
    pub(super) unsafe fn fast(word: Word) -> Option<Fast> {
        if abi::is_tagged_int(word) || word == 0 {
            return None;
        }
        let ptr = word_to_ptr(word);
        // SAFETY: a live object (the caller's guarantee).
        if unsafe { object::load_rc(ptr) } != abi::RC_UNIQUE {
            return None;
        }
        // SAFETY: as above; a heap object's layout is registered.
        unsafe { alloc::check_live(word, "a list operation") };
        // SAFETY: as above.
        let (_, id) = unsafe { object::scan_and_layout(ptr) };
        match id {
            ROOT => Some(Fast::Small(Self(word))),
            BIG_ROOT => Some(Fast::Big(Big(Self(word)))),
            _ => None,
        }
    }

    /// The root's word.
    pub(crate) fn word(self) -> Word {
        self.0
    }

    /// Whether this reference is the only one (never for the static empty
    /// list).
    pub(super) fn is_unique(self) -> bool {
        unique(self.0)
    }

    /// Whether the count is positive: a heap root that only this fiber
    /// holds, so a big root's leaves may be folded into its tree in place.
    pub(super) fn is_local(self) -> bool {
        // SAFETY: a live root (module invariant).
        unsafe { object::load_rc(word_to_ptr(self.0)) > 0 }
    }

    /// This root as a big root, if it is one.
    pub(super) fn big(self) -> Option<Big> {
        // SAFETY: a live root (module invariant).
        let (_, id) = unsafe { object::scan_and_layout(word_to_ptr(self.0)) };
        (id == BIG_ROOT).then_some(Big(self))
    }

    /// The tree, offset and length as stored, borrowing the tree. For a
    /// big root with a head or tail leaf, the length is the whole list's
    /// and the tree holds only a part of it.
    pub(super) fn raw(self) -> View {
        let ptr = word_to_ptr(self.0);
        // SAFETY: a live root has these fields (module invariant).
        let (tree, offset, len) = unsafe {
            (
                read_field(ptr, ROOT_TREE),
                read_field(ptr, ROOT_OFFSET),
                read_field(ptr, ROOT_LEN),
            )
        };
        View {
            tree: (tree != 0).then_some(Node(tree)),
            offset: to_usize(offset),
            len: to_usize(len),
        }
    }

    /// Reads the fields, borrowing the tree. A big root's head and tail
    /// leaves are folded into its tree first ([`super::buffers::settle`]),
    /// so the view is the whole list.
    pub(super) fn view(self) -> View {
        if let Some(big) = self.big() {
            super::buffers::settle(big);
        }
        self.raw()
    }

    /// The number of elements.
    pub(super) fn len(self) -> usize {
        // SAFETY: a live root has a length field.
        to_usize(unsafe { read_field(word_to_ptr(self.0), ROOT_LEN) })
    }

    /// Writes the length of a unique heap root.
    pub(super) fn set_len(self, len: usize) {
        // SAFETY: the caller holds the only reference to this heap root.
        unsafe { write_field(word_to_ptr(self.0), ROOT_LEN, to_u64(len)) };
    }

    /// Writes the offset of a unique heap root.
    pub(super) fn set_offset(self, offset: usize) {
        // SAFETY: the caller holds the only reference to this heap root.
        unsafe { write_field(word_to_ptr(self.0), ROOT_OFFSET, to_u64(offset)) };
    }

    /// Moves the tree out of a unique heap root, leaving 0 behind, so the
    /// root can be rewritten or freed without releasing it.
    pub(super) fn take_tree(self) -> Option<Node> {
        let tree = self.view().tree;
        // SAFETY: the caller holds the only reference to this heap root.
        unsafe { write_field(word_to_ptr(self.0), ROOT_TREE, 0) };
        tree
    }

    /// Writes the tree, offset and length of a unique heap root; the root
    /// takes ownership of `tree` (and must hold no tree before).
    pub(super) fn store(self, view: View) {
        let ptr = word_to_ptr(self.0);
        // SAFETY: the caller holds the only reference to this heap root.
        unsafe {
            write_field(ptr, ROOT_TREE, view.tree.map_or(0, Node::word));
            write_field(ptr, ROOT_OFFSET, to_u64(view.offset));
            write_field(ptr, ROOT_LEN, to_u64(view.len));
        }
    }

    /// A new list owning `view.tree`: the static empty list when the view is
    /// empty (the tree is dropped), a fresh root otherwise.
    pub(super) fn new(view: View) -> Self {
        if view.len == 0 {
            if let Some(tree) = view.tree {
                tree.release();
            }
            return Self(empty());
        }
        register();
        let root = Self(ptr_to_word(alloc::alloc_object(
            to_u64(ROOT_SIZE),
            abi::header(1, 0, ROOT),
        )));
        root.store(view);
        root
    }
}

/// Whether `id` is the layout of a list value: a root or a big root.
pub(super) fn is_root(id: LayoutId) -> bool {
    id == ROOT || id == BIG_ROOT
}

/// A big root (module invariant, as for [`Root`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Big(Root);

impl Big {
    /// The list value.
    pub(super) fn root(self) -> Root {
        self.0
    }

    fn ptr(self) -> *mut u8 {
        word_to_ptr(self.0.0)
    }

    /// A new big root that takes `pieces` over (their length is above 0).
    pub(super) fn new(pieces: Pieces) -> Self {
        register();
        let big = Self(Root(ptr_to_word(alloc::alloc_object(
            to_u64(BIG_ROOT_SIZE),
            abi::header(1, 0, BIG_ROOT),
        ))));
        big.attach(pieces);
        big
    }

    /// The fields, borrowing the tree and the leaves.
    pub(super) fn pieces(self) -> Pieces {
        let view = self.0.raw();
        Pieces {
            tree: view.tree,
            offset: view.offset,
            len: view.len,
            head: self.head(),
            pos: self.head_pos(),
            tail: self.tail(),
        }
    }

    /// Moves the tree and the leaves out of a heap root that only this
    /// fiber holds, which is left an empty list holding nothing until
    /// [`Self::attach`]. If the work in between unwinds, the root can be
    /// released as it is, and what was moved out is leaked (D-738).
    pub(super) fn detach(self) -> Pieces {
        let pieces = self.pieces();
        self.attach(Pieces::of(View {
            tree: None,
            offset: 0,
            len: 0,
        }));
        pieces
    }

    /// Writes all fields; the root takes `pieces` over (and must hold
    /// nothing before).
    pub(super) fn attach(self, pieces: Pieces) {
        self.0.store(View {
            tree: pieces.tree,
            offset: pieces.offset,
            len: pieces.len,
        });
        self.set_head(pieces.head, pieces.pos);
        self.set_tail(pieces.tail);
    }

    /// The head leaf, borrowed, if any.
    pub(super) fn head(self) -> Option<Node> {
        // SAFETY: a live big root has this field.
        let word = unsafe { read_field(self.ptr(), ROOT_HEAD) };
        (word != 0).then_some(Node(word))
    }

    /// The index in the head leaf of the list's first element.
    pub(super) fn head_pos(self) -> usize {
        // SAFETY: a live big root has this field.
        to_usize(unsafe { read_field(self.ptr(), ROOT_HEAD_POS) })
    }

    /// The tail leaf, borrowed, if any.
    pub(super) fn tail(self) -> Option<Node> {
        // SAFETY: a live big root has this field.
        let word = unsafe { read_field(self.ptr(), ROOT_TAIL) };
        (word != 0).then_some(Node(word))
    }

    /// Whether the root has a head or a tail leaf.
    pub(super) fn has_buffers(self) -> bool {
        self.head().is_some() || self.tail().is_some()
    }

    /// Writes the head leaf and its position; the root takes the leaf over
    /// and what it held before is not released.
    pub(super) fn set_head(self, head: Option<Node>, pos: usize) {
        let ptr = self.ptr();
        // SAFETY: only this fiber holds this heap root.
        unsafe {
            write_field(ptr, ROOT_HEAD, head.map_or(0, Node::word));
            write_field(ptr, ROOT_HEAD_POS, to_u64(pos));
        }
    }

    /// Writes the head position.
    pub(super) fn set_head_pos(self, pos: usize) {
        // SAFETY: only this fiber holds this heap root.
        unsafe { write_field(self.ptr(), ROOT_HEAD_POS, to_u64(pos)) };
    }

    /// Writes the tail leaf; the root takes it over and what it held
    /// before is not released.
    pub(super) fn set_tail(self, tail: Option<Node>) {
        // SAFETY: only this fiber holds this heap root.
        unsafe { write_field(self.ptr(), ROOT_TAIL, tail.map_or(0, Node::word)) };
    }
}

// ---------------------------------------------------------------------
// Tree nodes

/// A leaf or branch (module invariant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Node(Word);

impl Node {
    /// The node a branch slot or a root's tree field held, after it was
    /// moved out of that live object.
    pub(super) fn from_slot(word: Word) -> Self {
        Self(word)
    }

    /// The node's word.
    pub(super) fn word(self) -> Word {
        self.0
    }

    fn ptr(self) -> *mut u8 {
        word_to_ptr(self.0)
    }

    fn meta(self) -> (usize, usize) {
        // SAFETY: a live node has a meta word.
        unpack(unsafe { read_field(self.ptr(), META) })
    }

    fn set_meta(self, low: usize, high: usize) {
        // SAFETY: the caller owns this node uniquely (or is initializing it).
        unsafe { write_field(self.ptr(), META, pack(low, high)) };
    }

    /// Whether this is a leaf.
    pub(super) fn is_leaf(self) -> bool {
        // SAFETY: a live node.
        let (_, id) = unsafe { object::scan_and_layout(self.ptr()) };
        id == LEAF
    }

    /// Whether this reference is the only one.
    pub(super) fn is_unique(self) -> bool {
        unique(self.0)
    }

    /// Adds a reference.
    pub(super) fn dup(self) -> Self {
        dup_word(self.0);
        self
    }

    /// Gives up an owned reference.
    pub(super) fn release(self) {
        drop_word(self.0);
    }

    /// The height: 0 for a leaf.
    pub(super) fn height(self) -> usize {
        if self.is_leaf() { 0 } else { self.meta().1 }
    }

    /// Slots in use: elements of a leaf, children of a branch.
    pub(super) fn slots(self) -> usize {
        self.meta().0
    }

    /// The number of elements below this node.
    pub(super) fn len(self) -> usize {
        if self.is_leaf() {
            return self.slots();
        }
        match self.slots().checked_sub(1) {
            Some(last) => self.size_at(last),
            None => 0,
        }
    }

    // --- leaves

    /// A new empty leaf with room for `cap` elements (1 to [`M`]).
    pub(super) fn new_leaf(cap: usize) -> Self {
        register();
        let cap = cap.clamp(1, M);
        let node = Self(ptr_to_word(alloc::alloc_object(
            to_u64(slot(cap)),
            abi::header(1, 0, LEAF),
        )));
        node.set_meta(0, cap);
        node
    }

    /// A leaf's capacity.
    pub(super) fn cap(self) -> usize {
        self.meta().1
    }

    /// A leaf's length and capacity, in one read.
    pub(super) fn len_cap(self) -> (usize, usize) {
        self.meta()
    }

    /// Writes a leaf's length and capacity, which the caller has read.
    pub(super) fn put_len_cap(self, len: usize, cap: usize) {
        self.set_meta(len, cap);
    }

    /// Element `i` (below the length), borrowed.
    pub(super) fn elem(self, i: usize) -> Word {
        // SAFETY: slot `i` of a live leaf is initialized below its length.
        unsafe { read_field(self.ptr(), slot(i)) }
    }

    /// Writes element slot `i` (below the capacity) without releasing what
    /// was there.
    pub(super) fn put_elem(self, i: usize, word: Word) {
        // SAFETY: the caller owns this leaf uniquely and `i` is below its
        // capacity.
        unsafe { write_field(self.ptr(), slot(i), word) };
    }

    /// Sets a leaf's length (at most its capacity).
    pub(super) fn set_len(self, len: usize) {
        let cap = self.cap();
        self.set_meta(len, cap);
    }

    /// Moves elements `from..from + count` to `to..to + count` within one
    /// unique leaf.
    pub(super) fn move_elems(self, from: usize, to: usize, count: usize) {
        let ptr = self.ptr();
        // SAFETY: both ranges lie within the leaf's capacity, and
        // `ptr::copy` allows overlap.
        unsafe {
            std::ptr::copy(
                ptr.add(slot(from)),
                ptr.add(slot(to)),
                count.wrapping_mul(8),
            );
        }
    }

    // --- branches

    /// A new empty branch of the given height (1 or more).
    pub(super) fn new_branch(height: usize) -> Self {
        register();
        let node = Self(ptr_to_word(alloc::alloc_object(
            to_u64(BRANCH_SIZE),
            abi::header(1, 0, BRANCH),
        )));
        node.set_meta(0, height);
        node
    }

    /// Child `i` (below the count), borrowed.
    pub(super) fn child(self, i: usize) -> Self {
        // SAFETY: child slot `i` of a live branch is initialized below its
        // count and holds a live node.
        Self(unsafe { read_field(self.ptr(), slot(i)) })
    }

    /// Writes child slot `i` without releasing what was there.
    pub(super) fn put_child(self, i: usize, child: Self) {
        // SAFETY: the caller owns this branch uniquely and `i < M`.
        unsafe { write_field(self.ptr(), slot(i), child.0) };
    }

    /// Whether child slot `i` (below the count) holds a hole: the word 0
    /// that a walk from the front leaves where it took a child out
    /// (`buffers`).
    pub(super) fn is_hole(self, i: usize) -> bool {
        // SAFETY: child slot `i` of a live branch is initialized below its
        // count.
        unsafe { read_field(self.ptr(), slot(i)) == 0 }
    }

    /// Writes a hole into child slot `i` (below the count) of a unique
    /// branch, without releasing what was there.
    pub(super) fn put_hole(self, i: usize) {
        // SAFETY: the caller owns this branch uniquely and `i < M`.
        unsafe { write_field(self.ptr(), slot(i), 0) };
    }

    /// Removes the first `k` children of a unique branch, which has more,
    /// without releasing them: the rest move down `k` slots, and their
    /// sizes lose `gone`, the elements the removed children held.
    pub(super) fn remove_first(self, k: usize, gone: usize) {
        let left = self.slots().saturating_sub(k);
        let ptr = self.ptr();
        // SAFETY: the source and target ranges lie within the child slots
        // and within the size table, below `M`; `ptr::copy` allows
        // overlap.
        unsafe {
            std::ptr::copy(ptr.add(slot(k)), ptr.add(slot(0)), left.wrapping_mul(8));
            std::ptr::copy(
                ptr.add(size_slot(k)),
                ptr.add(size_slot(0)),
                left.wrapping_mul(8),
            );
        }
        self.set_count(left);
        self.shrink_sizes(gone);
    }

    /// Subtracts `gone` from every size of a unique branch.
    pub(super) fn shrink_sizes(self, gone: usize) {
        for i in 0..self.slots() {
            self.put_size(i, self.size_at(i).saturating_sub(gone));
        }
    }

    /// The cumulative size of children `0..=i`.
    pub(super) fn size_at(self, i: usize) -> usize {
        // SAFETY: size slot `i` of a live branch is initialized below its
        // count.
        to_usize(unsafe { read_field(self.ptr(), size_slot(i)) })
    }

    /// Writes size slot `i`.
    pub(super) fn put_size(self, i: usize, size: usize) {
        // SAFETY: the caller owns this branch uniquely and `i < M`.
        unsafe { write_field(self.ptr(), size_slot(i), to_u64(size)) };
    }

    /// Sets a branch's child count.
    pub(super) fn set_count(self, count: usize) {
        let height = self.meta().1;
        self.set_meta(count, height);
    }

    /// Appends an owned child to a unique branch that has room.
    pub(super) fn push_child(self, child: Self) {
        let count = self.slots();
        if count >= M {
            fatal(format_args!("list branch overflow"));
        }
        let before = match count.checked_sub(1) {
            Some(last) => self.size_at(last),
            None => 0,
        };
        self.put_child(count, child);
        self.put_size(count, before.saturating_add(child.len()));
        self.set_count(count.saturating_add(1));
    }

    /// The child holding element `i` (below the length), and the number of
    /// elements before that child. Starts at the radix position, a lower
    /// bound because no child holds more than `M^height` elements, scans
    /// forward a few children, which finds it in a balanced or nearly
    /// balanced node, and searches the rest of the size table only after
    /// that.
    pub(super) fn locate(self, i: usize) -> (usize, usize) {
        self.locate_at(i, self.height())
    }

    /// [`Self::locate`] in a branch whose height the caller knows.
    // Inline in every pattern step's descent and in `locate` (D-909). It
    // carries no `expect(clippy::inline_always)`: the lint skips a body
    // whose first statement is an item, here `SCAN`, so the expectation
    // would go unfulfilled.
    #[inline(always)]
    pub(super) fn locate_at(self, i: usize, height: usize) -> (usize, usize) {
        /// Children scanned forward from the radix position before the
        /// binary search.
        const SCAN: usize = 4;
        let count = self.slots();
        let shift = BITS.saturating_mul(u32::try_from(height).unwrap_or(u32::MAX));
        let guess = i.checked_shr(shift).unwrap_or(0);
        let (mut lo, mut hi) = (guess.min(count.saturating_sub(1)), count);
        if lo > 0 && self.size_at(lo.saturating_sub(1)) > i {
            lo = 0;
        }
        let scan_end = lo.saturating_add(SCAN).min(count);
        while lo < scan_end && self.size_at(lo) <= i {
            lo = lo.saturating_add(1);
        }
        if lo < scan_end {
            hi = lo;
        }
        // The first child whose cumulative size exceeds `i`.
        while lo < hi {
            let mid = lo.midpoint(hi);
            if self.size_at(mid) > i {
                hi = mid;
            } else {
                lo = mid.saturating_add(1);
            }
        }
        let before = match lo.checked_sub(1) {
            Some(prev) => self.size_at(prev),
            None => 0,
        };
        (lo, before)
    }
}
