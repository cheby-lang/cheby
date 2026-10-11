//! Allocation, freeing, accounting domains and the poisoning mode (F1).
//!
//! Every heap object is allocated 8-aligned through the global allocator.
//! Its size is not stored: freeing recomputes it from the layout table, or
//! from `cap` for strings and bytes, or from a runtime-defined kind's
//! `size` function, exactly as allocation computed it.
//!
//! Freeing an object visits the references it owns (its leading value
//! fields, or a runtime-defined kind's `children`) and releases each one.
//! It is iterative, with an explicit worklist, so freeing a list of a
//! million cells uses no more stack than freeing one (F1, D-094).
//!
//! # Accounting domains
//!
//! Live objects and bytes are counted per [`Domain`] (plan §4.3.4), not
//! per process. Each thread has a current domain: `run_entry` sets it from
//! `EntryConfig::domain`, and a thread without one gets a fresh domain of
//! its own, so tests on parallel threads never see each other's objects.
//! Allocation, freeing and [`forget`] charge the current thread's domain.
//! An object freed on another thread than the one that allocated it is
//! charged to the freeing thread's domain; in M1 objects never cross
//! threads except between a host and the entries it runs with its own
//! domain.
//!
//! # Poisoning
//!
//! With `CHEBY_RT_POISON=1` in a build with debug assertions (and always in
//! this crate's own unit tests), freeing overwrites an object's header
//! with [`POISON_HEADER`] and keeps the block in a bounded per-thread
//! quarantine instead of returning it to the allocator. `cheby_rt_dup`,
//! `cheby_rt_drop`, the readers of [`crate::values`] and the generic
//! routines check the header first and end the process with
//! `use after free: …` on a poisoned or impossible one.

use std::alloc::Layout;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::abi::{self, Word};
use crate::ctx::ExecCtx;
use crate::layout::{self, CustomKind, LayoutKind};
use crate::object::{
    self, load_rc, read_field, scan_and_layout, store_rc, to_u64, to_usize, word_to_ptr,
};
use crate::panic::fatal;
use crate::rc;
use crate::stats::{Event, Stats};

/// Whether this build checks for leaks at exit (debug builds only, D-458).
/// Accounting itself is always on: constant evaluation's memory limit
/// reads it in every build (D-536).
pub const LEAK_CHECK_ENABLED: bool = cfg!(debug_assertions);

/// An accounting domain: the live objects and bytes, and the peak of live
/// bytes, of everything allocated while it was some thread's current
/// domain (plan §4.3.4). One build shares one domain across its constant
/// entries and its `main` or test entries.
#[derive(Debug, Default)]
pub struct Domain {
    objects: AtomicI64,
    bytes: AtomicI64,
    peak: AtomicI64,
    stats: Stats,
}

impl Domain {
    /// A fresh domain with nothing live.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Objects allocated and not yet freed or forgotten.
    #[must_use]
    pub fn live_objects(&self) -> i64 {
        self.objects.load(Ordering::Relaxed)
    }

    /// Bytes of those objects.
    #[must_use]
    pub fn live_bytes(&self) -> i64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The most bytes live at once since the domain was created or
    /// [`Domain::reset_peak`] was last called.
    #[must_use]
    pub fn peak_bytes(&self) -> i64 {
        self.peak.load(Ordering::Relaxed)
    }

    /// Restarts the peak at the current live bytes, for measuring one
    /// evaluation (D-536).
    pub fn reset_peak(&self) {
        self.peak.store(self.live_bytes(), Ordering::Relaxed);
    }

    /// The domain's runtime counters ([`crate::stats`]).
    #[must_use]
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    fn charge(&self, objects: i64, bytes: i64) {
        self.objects.fetch_add(objects, Ordering::Relaxed);
        let before = self.bytes.fetch_add(bytes, Ordering::Relaxed);
        let now = before.saturating_add(bytes);
        // Most allocations stay under the peak: a load, not a write.
        if bytes > 0 && now > self.peak.load(Ordering::Relaxed) {
            self.peak.fetch_max(now, Ordering::Relaxed);
        }
    }
}

thread_local! {
    /// The current thread's accounting domain.
    static DOMAIN: RefCell<Arc<Domain>> = RefCell::new(Domain::new());
}

/// The current thread's accounting domain.
#[must_use]
pub fn current_domain() -> Arc<Domain> {
    DOMAIN
        .try_with(|domain| Arc::clone(&domain.borrow()))
        .unwrap_or_else(|_| Domain::new())
}

/// Makes `domain` the current thread's accounting domain and returns the
/// previous one.
pub fn set_current_domain(domain: Arc<Domain>) -> Arc<Domain> {
    DOMAIN
        .try_with(|current| current.replace(Arc::clone(&domain)))
        .unwrap_or(domain)
}

/// Runs `f` with `domain` as the current thread's domain, then restores
/// the previous one.
pub fn with_domain<R>(domain: Arc<Domain>, f: impl FnOnce() -> R) -> R {
    let previous = set_current_domain(domain);
    let result = f();
    set_current_domain(previous);
    result
}

/// Charges the current thread's domain. During thread teardown, when the
/// domain is gone, nothing is charged.
fn charge(objects: i64, bytes: u64) {
    let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
    let bytes = if objects < 0 {
        bytes.saturating_neg()
    } else {
        bytes
    };
    DOMAIN
        .try_with(|domain| domain.borrow().charge(objects, bytes))
        .ok();
}

/// Calls `f` with the counters of the current thread's domain; during
/// thread teardown, when the domain is gone, does nothing.
pub(crate) fn with_current_stats(f: impl FnOnce(&Stats)) {
    DOMAIN.try_with(|domain| f(&domain.borrow().stats)).ok();
}

/// The number of objects live in the current thread's domain.
#[must_use]
pub fn live() -> i64 {
    DOMAIN
        .try_with(|domain| domain.borrow().live_objects())
        .unwrap_or(0)
}

/// The bytes live in the current thread's domain, which the memory limit
/// of constant evaluation checks after every allocation (D-536).
pub(crate) fn current_live_bytes() -> i64 {
    DOMAIN
        .try_with(|domain| domain.borrow().live_bytes())
        .unwrap_or(0)
}

/// Calls `f` with every reference the object `word` owns (its value
/// fields, or a runtime-defined kind's children), tagged integers and 0
/// included; nothing for a tagged integer or 0. Constant evaluation's
/// rebuild walks results with it (D-539).
///
/// # Safety
///
/// `word` is a tagged integer, 0, or a live object.
pub(crate) unsafe fn for_each_child(word: Word, f: &mut dyn FnMut(Word)) {
    if abi::is_tagged_int(word) || word == 0 {
        return;
    }
    // SAFETY: forwarded from the caller.
    unsafe { check_live(word, "walking a value") };
    let obj = word_to_ptr(word);
    // SAFETY: a live heap object.
    let shape = unsafe { shape_of(obj) };
    match shape.children {
        Children::Fields { first, count } => {
            let mut offset = first;
            for _ in 0..count {
                // SAFETY: the shape lies within the object.
                f(unsafe { read_field(obj, offset) });
                offset = offset.saturating_add(to_usize(abi::FIELD_SIZE));
            }
        }
        // SAFETY: a live object of this kind.
        Children::Custom(kind) => unsafe { (kind.children)(word, f) },
    }
}

/// The number of heap objects live in the current thread's domain: allocated
/// and not yet freed, not counting objects that became static. The same as
/// [`live`].
#[must_use]
pub fn live_objects() -> i64 {
    live()
}

/// Makes a heap object static, so it is never freed, and removes it from
/// the current domain. The object leaves the domain once: forgetting an
/// object that is already static (count 0) does nothing. Constant
/// evaluation's freezing (E6) calls this for every object of a result; the
/// object's children are the caller's to forget.
///
/// # Safety
///
/// `word` is a tagged integer or points to a live object, and no other
/// thread uses it.
pub unsafe fn forget(word: Word) {
    if abi::is_tagged_int(word) || word == 0 {
        return;
    }
    // SAFETY: forwarded from the caller. In poisoning mode, a freed object
    // ends the process here, before its header is written.
    unsafe { check_live(word, "freezing") };
    let obj = word_to_ptr(word);
    // SAFETY: forwarded from the caller.
    if unsafe { load_rc(obj) } == abi::RC_STATIC {
        return;
    }
    // SAFETY: as above; a heap object's count is written only by its
    // owning thread in M1.
    unsafe {
        store_rc(obj, abi::RC_STATIC);
        leave_domain(obj);
    }
}

/// Makes `word` and every object it reaches static in place (D-539): each
/// gets count 0 and leaves the current domain once, so it is never freed
/// and never counted as a leak. Objects that are already static, and what
/// they reach, are left alone. Used for the results of constant evaluation
/// before code reads them. The walk is a worklist on the heap, so a
/// million-deep value freezes without deep Rust recursion (plan §0).
///
/// # Safety
///
/// `word` is a tagged integer, 0, or a live object; no other thread uses
/// the objects it reaches; and the current domain is the one they were
/// charged to.
pub unsafe fn freeze(word: Word) {
    let mut list = vec![word];
    while let Some(word) = list.pop() {
        if abi::is_tagged_int(word) || word == 0 {
            continue;
        }
        // SAFETY: every word on the list is a live object reached from the
        // caller's value. In poisoning mode, a freed one ends the process.
        unsafe { check_live(word, "freezing") };
        let obj = word_to_ptr(word);
        // SAFETY: as above.
        if unsafe { load_rc(obj) } == abi::RC_STATIC {
            continue;
        }
        // SAFETY: a live heap object.
        let shape = unsafe { shape_of(obj) };
        match shape.children {
            Children::Fields { first, count } => {
                let mut offset = first;
                for _ in 0..count {
                    // SAFETY: the shape lies within the object.
                    list.push(unsafe { read_field(obj, offset) });
                    offset = offset.saturating_add(to_usize(abi::FIELD_SIZE));
                }
            }
            Children::Custom(kind) => {
                let mut visit = |child: Word| list.push(child);
                // SAFETY: a live object of this kind.
                unsafe { (kind.children)(word, &mut visit) };
            }
        }
        // SAFETY: a live heap object; its count is written only by this
        // thread in M1. It leaves the domain with its allocation's size.
        unsafe { store_rc(obj, abi::RC_STATIC) };
        charge(-1, to_u64(object_layout(shape.size).size()));
    }
}

/// Removes an object that just became static from the current domain.
///
/// # Safety
///
/// `obj` points to a live heap object that has just been made static.
pub(crate) unsafe fn leave_domain(obj: *mut u8) {
    // SAFETY: forwarded from the caller.
    let shape = unsafe { shape_of(obj) };
    // The allocation's size, as `alloc_object` charged it.
    charge(-1, to_u64(object_layout(shape.size).size()));
}

/// Allocates an object for generated code: `size` bytes including the
/// header, 8-aligned, with `header` written at offset 0. The fields are
/// left uninitialized for the caller to write. Ends the process on
/// allocation failure.
///
/// `ctx` is the running entry's execution context, unused in M1 (the
/// memory limit of constant evaluation reads the domain, D-536); it may be
/// null.
///
/// # Safety
///
/// `header` describes an object of exactly `size` bytes (as the layout
/// table computes it), and the caller initializes every field before the
/// object is freed or read.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_alloc(ctx: *mut ExecCtx, size: u64, header: u64) -> Word {
    let _ = ctx;
    object::ptr_to_word(alloc_object(size, header))
}

/// Allocates an object of `size` bytes (rounded up to 8) with the given
/// header and uninitialized fields, and charges it to the current domain.
/// Ends the process on allocation failure. During constant evaluation, a
/// request that would pass the memory budget abandons the entry with the
/// memory limit before anything is allocated (D-536).
#[must_use]
pub fn alloc_object(size: u64, header: u64) -> *mut u8 {
    crate::entry::check_budget(
        size.max(abi::HEADER_SIZE)
            .checked_next_multiple_of(abi::OBJECT_ALIGN)
            .unwrap_or(u64::MAX),
    );
    let layout = object_layout(size);
    let ptr = cached_block(layout).unwrap_or_else(|| {
        // SAFETY: `object_layout` never returns a zero-sized layout.
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        ptr
    });
    // SAFETY: the allocation is at least 8 bytes and 8-aligned.
    unsafe { object::write_field(ptr, 0, header) };
    let bytes = i64::try_from(layout.size()).unwrap_or(i64::MAX);
    DOMAIN
        .try_with(|domain| {
            let domain = domain.borrow();
            domain.charge(1, bytes);
            domain.stats.record(Event::Alloc);
        })
        .ok();
    ptr
}

/// The allocation layout of an object of `size` bytes.
fn object_layout(size: u64) -> Layout {
    let rounded = size
        .max(abi::HEADER_SIZE)
        .checked_next_multiple_of(abi::OBJECT_ALIGN);
    match rounded.map(|size| Layout::from_size_align(to_usize(size), to_usize(abi::OBJECT_ALIGN))) {
        Some(Ok(layout)) => layout,
        _ => fatal(format_args!("cannot allocate an object of {size} bytes")),
    }
}

thread_local! {
    /// The worklist of objects whose count reached zero and whose fields
    /// are still to be released. Kept per thread so that its capacity is
    /// reused across frees.
    static WORKLIST: Cell<Vec<Word>> = const { Cell::new(Vec::new()) };
}

/// Capacity above which the worklist is not kept for the next free.
const WORKLIST_KEEP: usize = 1 << 16;

/// Frees an object whose count just reached zero, then every object that
/// only its fields kept alive.
///
/// # Safety
///
/// `obj` points to a live heap object whose count has reached zero (no
/// references remain).
pub(crate) unsafe fn free(obj: *mut u8) {
    let word = object::ptr_to_word(obj);
    // During thread teardown the worklist may already be gone; a fresh
    // vector works the same.
    let mut list = WORKLIST.try_with(Cell::take).unwrap_or_default();
    list.push(word);
    // SAFETY: forwarded from the caller; every word pushed later is an
    // object whose count reached zero.
    unsafe { drain(&mut list) };
    list.clear();
    if list.capacity() <= WORKLIST_KEEP {
        WORKLIST.try_with(|cell| cell.set(list)).ok();
    }
}

/// Releases one reference held by a dying object, queueing the child when
/// that was its last reference.
///
/// # Safety
///
/// `child` is a tagged integer, 0, or a live object the dying object holds
/// a reference to.
unsafe fn release_child(child: Word, list: &mut Vec<Word>) {
    if abi::is_tagged_int(child) || child == 0 {
        return;
    }
    // SAFETY: forwarded from the caller.
    unsafe { check_live(child, "freeing an object") };
    // SAFETY: the dying object owned this reference to a live object.
    if unsafe { rc::release(word_to_ptr(child)) } {
        list.push(child);
    }
}

/// Frees every object on the worklist and, transitively, the fields whose
/// count reaches zero. The current domain is charged once, at the end.
///
/// # Safety
///
/// Every word on `list` is a heap object whose count has reached zero.
unsafe fn drain(list: &mut Vec<Word>) {
    let mut objects: i64 = 0;
    let mut bytes: u64 = 0;
    while let Some(word) = list.pop() {
        let obj = word_to_ptr(word);
        // SAFETY: `obj` is a live object with no references left.
        let shape = unsafe { shape_of(obj) };
        match shape.children {
            Children::Fields { first, count } => {
                let mut offset = first;
                for _ in 0..count {
                    // SAFETY: the shape lies within the object.
                    let child = unsafe { read_field(obj, offset) };
                    offset = offset.saturating_add(to_usize(abi::FIELD_SIZE));
                    // SAFETY: a value field holds an owned reference.
                    unsafe { release_child(child, list) };
                }
            }
            Children::Custom(kind) => {
                let mut visit = |child: Word| {
                    // SAFETY: every word the kind visits is a reference the
                    // dying object owns.
                    unsafe { release_child(child, list) };
                };
                // SAFETY: a live object of this kind.
                unsafe { (kind.children)(word, &mut visit) };
            }
        }
        let layout = object_layout(shape.size);
        objects = objects.saturating_sub(1);
        bytes = bytes.saturating_add(to_u64(layout.size()));
        // SAFETY: allocated by `alloc_object` with this size, which
        // `shape_of` recomputes exactly, and no reference remains.
        unsafe { release_block(obj, layout) };
    }
    charge(objects, bytes);
}

/// Where the references an object owns are.
enum Children {
    /// `count` uniform fields from byte offset `first`.
    Fields { first: usize, count: u64 },
    /// A runtime-defined kind's `children` function.
    Custom(&'static CustomKind),
}

/// The references an object owns and its size.
struct Shape {
    children: Children,
    size: u64,
}

/// Computes an object's shape from its header and layout.
///
/// # Safety
///
/// `obj` points to a live object.
unsafe fn shape_of(obj: *mut u8) -> Shape {
    // SAFETY: forwarded from the caller.
    let (scan, id) = unsafe { scan_and_layout(obj) };
    let Some(info) = layout::get(id) else {
        fatal(format_args!("freeing an object with unknown layout {id}"));
    };
    let first = to_usize(abi::HEADER_SIZE);
    let uniform = |count: u64, first: usize| {
        let fields_end = to_u64(first).checked_add(abi::FIELD_SIZE.checked_mul(count)?)?;
        Some(Shape {
            children: Children::Fields { first, count },
            size: fields_end,
        })
    };
    let shape = match info.kind {
        LayoutKind::String | LayoutKind::Bytes => {
            // SAFETY: strings and bytes have `cap` at this offset.
            let cap = unsafe { read_field(obj, object::abi_offset(abi::string::CAP_OFFSET)) };
            abi::string::alloc_size(cap).map(|size| Shape {
                children: Children::Fields { first, count: 0 },
                size,
            })
        }
        LayoutKind::Tuple if scan == abi::SCAN_ESCAPE => {
            // SAFETY: an escaped tuple holds its tagged arity at offset 8.
            let count = abi::untag_int(unsafe { read_field(obj, first) });
            u64::try_from(count)
                .ok()
                .and_then(|count| uniform(count, first.saturating_add(8)))
        }
        LayoutKind::Tuple | LayoutKind::Closure | LayoutKind::Dict | LayoutKind::Desc
            if scan != abi::SCAN_ESCAPE =>
        {
            uniform(u64::from(scan), first)
        }
        LayoutKind::Closure | LayoutKind::Dict | LayoutKind::Desc => {
            // SAFETY: an escaped closure, dictionary or descriptor keeps its
            // first field at offset 8 and the tagged count of the fields
            // after the count at offset 16 (D-279, D-555).
            // The count word is tagged, so visiting it releases nothing.
            let count = abi::untag_int(unsafe { read_field(obj, first.saturating_add(8)) });
            u64::try_from(count)
                .ok()
                .and_then(|count| count.checked_add(2))
                .and_then(|count| uniform(count, first))
        }
        // Both forms of a tuple are matched above.
        LayoutKind::Tuple => None,
        LayoutKind::Custom(kind) => Some(Shape {
            children: Children::Custom(kind),
            // SAFETY: a live object of this kind.
            size: unsafe { (kind.size)(object::ptr_to_word(obj)) },
        }),
        LayoutKind::BoxedInt
        | LayoutKind::BoxedU64
        | LayoutKind::BoxedFloat
        | LayoutKind::BoxedF32
        | LayoutKind::Dyn
        | LayoutKind::DescNominal
        | LayoutKind::Ctor { .. } => info.fixed_size().map(|size| Shape {
            children: Children::Fields {
                first,
                count: u64::from(info.value_fields),
            },
            size,
        }),
    };
    shape.unwrap_or_else(|| fatal(format_args!("freeing a malformed `{}` object", info.name)))
}

// Poisoning ---------------------------------------------------------------

/// The header a freed object carries in poisoning mode: layout 0, which no
/// live object has, so every check recognizes it.
pub const POISON_HEADER: u64 = 0x0000_00EE_DEAD_BEEF;

/// The most blocks one thread's quarantine keeps.
pub const QUARANTINE_BLOCKS: usize = 4096;

/// The most bytes one thread's quarantine keeps.
pub const QUARANTINE_BYTES: usize = 16 << 20;

/// Whether poisoning mode is on: `CHEBY_RT_POISON=1` in a build with debug
/// assertions, read once per process, and in this crate's unit tests
/// unless `CHEBY_RT_POISON=0` turns it off (which runs them on the block
/// cache instead). Code generation emits calls to the checked
/// `cheby_rt_dup` and `cheby_rt_drop` instead of inline fast paths when it
/// is on.
#[must_use]
#[inline]
pub fn poison_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    cfg!(debug_assertions)
        && *ENABLED.get_or_init(|| match std::env::var_os("CHEBY_RT_POISON") {
            Some(value) if value == "1" => true,
            Some(value) if value == "0" => false,
            _ => cfg!(test),
        })
}

/// Freed blocks kept out of the allocator, oldest first.
#[derive(Default)]
struct Quarantine {
    blocks: VecDeque<(*mut u8, Layout)>,
    bytes: usize,
}

impl Quarantine {
    /// Keeps a poisoned block, returning the oldest ones to the allocator
    /// beyond the bounds.
    fn keep(&mut self, obj: *mut u8, layout: Layout) {
        self.blocks.push_back((obj, layout));
        self.bytes = self.bytes.saturating_add(layout.size());
        while self.blocks.len() > QUARANTINE_BLOCKS || self.bytes > QUARANTINE_BYTES {
            let Some((old, old_layout)) = self.blocks.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(old_layout.size());
            // SAFETY: a block allocated with this layout, freed and no
            // longer referenced by anything that may run correctly.
            unsafe { std::alloc::dealloc(old, old_layout) };
        }
    }
}

impl Drop for Quarantine {
    fn drop(&mut self) {
        for (obj, layout) in self.blocks.drain(..) {
            // SAFETY: as in `keep`.
            unsafe { std::alloc::dealloc(obj, layout) };
        }
    }
}

thread_local! {
    static QUARANTINE: RefCell<Quarantine> = RefCell::new(Quarantine::default());
}

/// The number of blocks in the current thread's quarantine.
#[doc(hidden)]
#[must_use]
pub fn quarantine_len() -> usize {
    QUARANTINE
        .try_with(|quarantine| quarantine.borrow().blocks.len())
        .unwrap_or(0)
}

/// The size classes of the block cache: allocations of 8 to 256 bytes, in
/// steps of 8 bytes.
const CACHE_CLASSES: usize = 32;

/// The most free blocks the cache keeps per size class and thread.
pub const CACHE_BLOCKS: usize = 512;

/// Freed blocks of small objects kept per thread for the next allocation
/// of the same size, so that the short-lived objects of a running program
/// do not go to the system allocator each time (W5-D: freeing and
/// allocating were most of 004's run time). Bypassed in poisoning mode,
/// whose quarantine must keep freed blocks out of use.
struct BlockCache {
    classes: [Vec<*mut u8>; CACHE_CLASSES],
}

impl BlockCache {
    const fn new() -> Self {
        Self {
            classes: [const { Vec::new() }; CACHE_CLASSES],
        }
    }

    /// Returns every cached block to the system allocator.
    fn release(&mut self) {
        for (class, blocks) in self.classes.iter_mut().enumerate() {
            let Some(layout) = class_layout(class) else {
                continue;
            };
            for block in blocks.drain(..) {
                // SAFETY: the cache holds only blocks allocated with their
                // class's layout that nothing references.
                unsafe { std::alloc::dealloc(block, layout) };
            }
        }
    }
}

impl Drop for BlockCache {
    fn drop(&mut self) {
        self.release();
    }
}

thread_local! {
    static CACHE: RefCell<BlockCache> = const { RefCell::new(BlockCache::new()) };
}

/// The block cache's size class of an allocation layout, if it has one.
fn size_class(layout: Layout) -> Option<usize> {
    let size = layout.size();
    let step = to_usize(abi::OBJECT_ALIGN);
    if !size.is_multiple_of(step) {
        return None;
    }
    size.checked_div(step)?
        .checked_sub(1)
        .filter(|class| *class < CACHE_CLASSES)
}

/// The allocation layout of a size class.
fn class_layout(class: usize) -> Option<Layout> {
    let step = to_usize(abi::OBJECT_ALIGN);
    Layout::from_size_align(class.checked_add(1)?.checked_mul(step)?, step).ok()
}

/// A cached free block for `layout`, if the current thread has one.
fn cached_block(layout: Layout) -> Option<*mut u8> {
    let class = size_class(layout)?;
    CACHE
        .try_with(|cache| cache.borrow_mut().classes.get_mut(class)?.pop())
        .ok()
        .flatten()
}

/// Keeps a free block of `layout` in the current thread's cache; false
/// when the cache is full, has no class for it, or is gone (thread
/// teardown).
fn cache_block(obj: *mut u8, layout: Layout) -> bool {
    let Some(class) = size_class(layout) else {
        return false;
    };
    CACHE
        .try_with(|cache| {
            let mut cache = cache.borrow_mut();
            match cache.classes.get_mut(class) {
                Some(blocks) if blocks.len() < CACHE_BLOCKS => {
                    blocks.push(obj);
                    true
                }
                _ => false,
            }
        })
        .unwrap_or(false)
}

/// Returns the current thread's cached free blocks to the system allocator,
/// for a thread that will not allocate again (a panicked entry parks).
pub(crate) fn release_block_cache() {
    CACHE.try_with(|cache| cache.borrow_mut().release()).ok();
}

/// The number of blocks in the current thread's cache, for tests.
#[cfg(test)]
fn cached_blocks() -> usize {
    CACHE
        .try_with(|cache| cache.borrow().classes.iter().map(Vec::len).sum())
        .unwrap_or(0)
}

/// Returns a freed block to the block cache or the allocator, or poisons
/// and quarantines it.
///
/// # Safety
///
/// `obj` was allocated by `alloc_object` with `layout` and no reference to
/// it remains.
unsafe fn release_block(obj: *mut u8, layout: Layout) {
    if poison_enabled() {
        // SAFETY: the block is still allocated and nothing else uses it.
        unsafe { object::write_field(obj, 0, POISON_HEADER) };
        let kept = QUARANTINE
            .try_with(|quarantine| quarantine.borrow_mut().keep(obj, layout))
            .is_ok();
        if kept {
            return;
        }
    } else if cache_block(obj, layout) {
        return;
    }
    // SAFETY: forwarded from the caller.
    unsafe { std::alloc::dealloc(obj, layout) };
}

/// In poisoning mode, ends the process if `word` points to a freed
/// (poisoned) object or one whose header is impossible. `what` names the
/// operation for the message. Does nothing otherwise, and for tagged
/// integers and 0.
///
/// # Safety
///
/// `word` is a tagged integer, 0, or points to memory that was allocated
/// as an object (live, static or quarantined).
#[inline]
pub unsafe fn check_live(word: Word, what: &str) {
    if !poison_enabled() || abi::is_tagged_int(word) || word == 0 {
        return;
    }
    // SAFETY: forwarded from the caller.
    unsafe { check_header(word, what) };
}

/// The poisoning mode's check of [`check_live`], out of line so that the
/// inlined check costs one test when the mode is off.
///
/// # Safety
///
/// As for [`check_live`], and `word` is not a tagged integer or 0.
#[inline(never)]
unsafe fn check_header(word: Word, what: &str) {
    let obj = word_to_ptr(word);
    // SAFETY: forwarded from the caller; the header is readable.
    let (rc, (scan, id)) = unsafe { (load_rc(obj), scan_and_layout(obj)) };
    let header = abi::header(rc, scan, id);
    if header == POISON_HEADER {
        fatal(format_args!(
            "use after free: {what} reached the freed object {word:#x}"
        ));
    }
    if layout::get(id).is_none() {
        fatal(format_args!(
            "use after free: {what} reached {word:#x}, whose header {header:#x} is impossible"
        ));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use super::*;
    use crate::abi::layout as ids;
    use crate::object::write_field;

    /// Serializes the old tests that looked at the process-wide leak
    /// counter. Accounting domains make it unnecessary; new tests do not
    /// take it.
    pub(crate) fn serial() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Allocates a tuple of the given fields with count 1.
    pub(crate) fn tuple(fields: &[Word]) -> Word {
        let scan = u8::try_from(fields.len()).unwrap();
        let size = field_offset(fields.len());
        let obj = alloc_object(to_u64(size), abi::header(1, scan, ids::TUPLE));
        for (index, field) in fields.iter().enumerate() {
            // SAFETY: initializing the fields of a fresh tuple.
            unsafe { write_field(obj, field_offset(index), *field) };
        }
        object::ptr_to_word(obj)
    }

    /// The offset of field `index` (counting from 0 after the header).
    pub(crate) fn field_offset(index: usize) -> usize {
        index.checked_add(1).and_then(|n| n.checked_mul(8)).unwrap()
    }

    #[test]
    fn alloc_writes_the_header_and_counts_the_object() {
        let _guard = serial();
        let before = live_objects();
        let bytes = current_domain().live_bytes();
        let word = tuple(&[abi::tag_int(7).unwrap()]);
        assert_eq!(word % 8, 0);
        // SAFETY: a live tuple.
        unsafe {
            assert_eq!(object::load_rc(word_to_ptr(word)), 1);
            assert_eq!(scan_and_layout(word_to_ptr(word)), (1, ids::TUPLE));
        }
        assert_eq!(live_objects(), before.wrapping_add(1));
        assert_eq!(current_domain().live_bytes(), bytes.wrapping_add(16));
        // SAFETY: the only reference.
        unsafe { rc::drop(word) };
        assert_eq!(live_objects(), before);
        assert_eq!(current_domain().live_bytes(), bytes);
    }

    #[test]
    fn freeing_releases_every_field_once() {
        let _guard = serial();
        let before = live_objects();
        let shared = tuple(&[]);
        let inner = tuple(&[shared]);
        // SAFETY: a live object; the second reference goes into `outer`.
        unsafe { rc::dup(shared) };
        let outer = tuple(&[inner, shared, abi::tag_int(-3).unwrap(), object::nil()]);
        assert_eq!(live_objects().wrapping_sub(before), 3);
        // SAFETY: the only reference to `outer`.
        unsafe { rc::drop(outer) };
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn freeing_a_long_chain_uses_no_stack() {
        let _guard = serial();
        let length = if cfg!(miri) { 1_000 } else { 1_000_000 };
        // A small stack: recursion per cell would overflow it quickly.
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || {
                let mut head = object::nil();
                for n in 0..length {
                    head = tuple(&[abi::tag_int(n).unwrap(), head]);
                }
                // SAFETY: the only reference to the head; each cell owns
                // the next.
                unsafe { rc::drop(head) };
                live_objects()
            })
            .unwrap();
        assert_eq!(handle.join().unwrap(), 0);
    }

    #[test]
    fn freeing_a_wide_tuple_with_an_escaped_scan_count() {
        let _guard = serial();
        let before = live_objects();
        let bytes = current_domain().live_bytes();
        let count = 300_usize;
        let children: Vec<Word> = (0..count).map(|_| tuple(&[])).collect();
        let size = to_u64(field_offset(count.wrapping_add(1)));
        let obj = alloc_object(size, abi::header(1, abi::SCAN_ESCAPE, ids::TUPLE));
        // SAFETY: initializing a fresh tuple: the tagged arity, then fields.
        unsafe {
            write_field(obj, 8, abi::tag_int(i64::try_from(count).unwrap()).unwrap());
            for (index, child) in children.iter().enumerate() {
                write_field(obj, field_offset(index.wrapping_add(1)), *child);
            }
            rc::drop(object::ptr_to_word(obj));
        }
        assert_eq!(live_objects(), before);
        assert_eq!(current_domain().live_bytes(), bytes);
    }

    /// An escaped closure, dictionary or descriptor: its first field at
    /// offset 8, the tagged count of the rest at offset 16, then the rest
    /// (D-555). Freeing releases the first field and the rest, and charges
    /// the whole size back.
    #[test]
    fn freeing_escaped_closures_dicts_and_descs() {
        let domain = Domain::new();
        with_domain(Arc::clone(&domain), || {
            for (id, first_is_code) in
                [(ids::CLOSURE, true), (ids::DICT, false), (ids::DESC, false)]
            {
                let rest = 300_usize;
                let first = if first_is_code { 0x1001 } else { tuple(&[]) };
                let children: Vec<Word> = (0..rest).map(|_| tuple(&[])).collect();
                let size = to_u64(field_offset(rest.wrapping_add(2)));
                let obj = alloc_object(size, abi::header(1, abi::SCAN_ESCAPE, id));
                // SAFETY: initializing a fresh object, then dropping its
                // only reference.
                unsafe {
                    write_field(obj, 8, first);
                    write_field(obj, 16, abi::tag_int(i64::try_from(rest).unwrap()).unwrap());
                    for (index, child) in children.iter().enumerate() {
                        write_field(obj, field_offset(index.wrapping_add(2)), *child);
                    }
                    rc::drop(object::ptr_to_word(obj));
                }
                assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
            }
        });
    }

    #[test]
    fn freeing_ctors_with_raw_fields_skips_them() {
        let _guard = serial();
        let before = live_objects();
        let info = layout::LayoutInfo::ctor(
            "t.Mixed",
            0,
            &[
                (layout::FieldKind::Int, None),
                (layout::FieldKind::Value, None),
            ],
        );
        let id = layout::register(info).unwrap();
        let child = tuple(&[]);
        let obj = alloc_object(24, abi::header(1, 1, id));
        // SAFETY: initializing a fresh object: the value field at 8, then
        // a raw `Int` that looks like a pointer (even) and must be skipped.
        unsafe {
            write_field(obj, 8, child);
            write_field(obj, 16, 0x1000);
            rc::drop(object::ptr_to_word(obj));
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn boxed_numbers_free_without_scanning() {
        let _guard = serial();
        let before = live_objects();
        for id in [
            ids::BOXED_INT,
            ids::BOXED_FLOAT,
            ids::BOXED_U64,
            ids::BOXED_F32,
        ] {
            let obj = alloc_object(16, abi::header(1, 0, id));
            // SAFETY: a fresh box; the payload is raw, then the only
            // reference is dropped.
            unsafe {
                write_field(obj, 8, 0x2000);
                rc::drop(object::ptr_to_word(obj));
            }
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn dictionaries_dyn_values_and_descriptors_free_their_fields() {
        let before = live_objects();
        for (id, fields) in [(ids::DICT, 3_u8), (ids::DESC, 2), (ids::DYN, 2)] {
            let obj = alloc_object(
                to_u64(field_offset(usize::from(fields))),
                abi::header(1, fields, id),
            );
            for index in 0..usize::from(fields) {
                // SAFETY: initializing a fresh object's uniform fields.
                unsafe { write_field(obj, field_offset(index), tuple(&[])) };
            }
            // SAFETY: the only reference.
            unsafe { rc::drop(object::ptr_to_word(obj)) };
        }
        assert_eq!(live_objects(), before);
    }

    /// A test node: header, `len`, then `len` owned uniform words.
    unsafe fn node_size(word: Word) -> u64 {
        // SAFETY: a live node.
        let len = unsafe { read_field(word_to_ptr(word), 8) };
        16_u64.saturating_add(len.saturating_mul(8))
    }

    unsafe fn node_children(word: Word, visit: &mut dyn FnMut(Word)) {
        let obj = word_to_ptr(word);
        // SAFETY: a live node.
        let len = unsafe { read_field(obj, 8) };
        for index in 0..to_usize(len) {
            // SAFETY: within the node.
            visit(unsafe { read_field(obj, field_offset(index.wrapping_add(1))) });
        }
    }

    pub(crate) static TEST_NODE: CustomKind = CustomKind {
        name: "test node",
        size: node_size,
        children: node_children,
        equal: None,
        hash: None,
        debug: None,
    };

    /// The id the test node registers at (unused by every runtime module).
    pub(crate) const TEST_NODE_ID: abi::LayoutId = 40;

    /// A test node owning `children`.
    pub(crate) fn node(children: &[Word]) -> Word {
        layout::register_builtin(TEST_NODE_ID, layout::LayoutInfo::custom(&TEST_NODE)).unwrap();
        let len = to_u64(children.len());
        let obj = alloc_object(
            16_u64.saturating_add(len.saturating_mul(8)),
            abi::header(1, 0, TEST_NODE_ID),
        );
        // SAFETY: initializing a fresh node.
        unsafe {
            write_field(obj, 8, len);
            for (index, child) in children.iter().enumerate() {
                write_field(obj, field_offset(index.wrapping_add(1)), *child);
            }
        }
        object::ptr_to_word(obj)
    }

    #[test]
    fn custom_kinds_free_through_the_worklist() {
        let before = live_objects();
        let bytes = current_domain().live_bytes();
        // A million-deep chain of nodes on a 64 KiB stack.
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(|| {
                let mut head = node(&[]);
                for n in 0..1_000_000 {
                    head = node(&[abi::tag_int(n).unwrap(), head, tuple(&[])]);
                }
                // SAFETY: the only reference.
                unsafe { rc::drop(head) };
                (live_objects(), current_domain().live_bytes())
            })
            .unwrap();
        assert_eq!(handle.join().unwrap(), (0, 0));
        let shared = tuple(&[]);
        // SAFETY: two more references, both owned by the node.
        unsafe {
            rc::dup(shared);
            rc::dup(shared);
        }
        let parent = node(&[shared, shared]);
        // SAFETY: the node's own reference; then the test's.
        unsafe {
            rc::drop(parent);
            assert_eq!(object::load_rc(word_to_ptr(shared)), 1);
            rc::drop(shared);
        }
        assert_eq!(
            (live_objects(), current_domain().live_bytes()),
            (before, bytes)
        );
    }

    #[test]
    fn threads_see_only_their_own_objects() {
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = [10_usize, 25]
            .into_iter()
            .map(|count| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let words: Vec<Word> = (0..count).map(|_| tuple(&[])).collect();
                    barrier.wait();
                    let seen = live_objects();
                    barrier.wait();
                    for word in words {
                        // SAFETY: the only reference.
                        unsafe { rc::drop(word) };
                    }
                    (seen, live_objects())
                })
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(results, [(10, 0), (25, 0)]);
    }

    #[test]
    fn domains_track_bytes_and_peaks() {
        let domain = Domain::new();
        let (peak, live) = with_domain(Arc::clone(&domain), || {
            let a = tuple(&[tuple(&[])]);
            // SAFETY: the only reference.
            unsafe { rc::drop(a) };
            (current_domain().peak_bytes(), live_objects())
        });
        assert_eq!((peak, live), (24, 0));
        assert_eq!(domain.live_bytes(), 0);
        domain.reset_peak();
        assert_eq!(domain.peak_bytes(), 0);
    }

    #[test]
    fn forget_leaves_the_domain_once() {
        let domain = Domain::new();
        with_domain(Arc::clone(&domain), || {
            let word = tuple(&[]);
            assert_eq!(domain.live_objects(), 1);
            // SAFETY: a live object only this thread uses.
            unsafe {
                forget(word);
                assert_eq!(object::load_rc(word_to_ptr(word)), abi::RC_STATIC);
                forget(word);
                rc::dup(word);
                rc::drop(word);
            }
            assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
        });
    }

    /// Freezing a constant's result (D-539): a million-deep chain freezes
    /// without deep recursion, shared parts once, statics untouched, and
    /// the whole graph leaves the domain.
    #[test]
    fn freezing_makes_deep_and_shared_values_static() {
        let domain = Domain::new();
        let handle = std::thread::Builder::new()
            // A small stack: a recursive walk would overflow it.
            .stack_size(256 << 10)
            .spawn({
                let domain = Arc::clone(&domain);
                move || {
                    with_domain(Arc::clone(&domain), || {
                        let shared = tuple(&[abi::tag_int(1).unwrap()]);
                        let mut chain = tuple(&[]);
                        for _ in 0..1_000_000 {
                            chain = tuple(&[chain]);
                        }
                        // SAFETY: live objects; the pair takes a second
                        // reference to `shared`.
                        unsafe { rc::dup(shared) };
                        let both = tuple(&[chain, shared, shared, crate::object::nil()]);
                        assert_eq!(domain.live_objects(), 1_000_003);
                        // SAFETY: live objects only this thread uses,
                        // charged to the current domain.
                        unsafe {
                            freeze(both);
                            assert_eq!(object::load_rc(word_to_ptr(both)), abi::RC_STATIC);
                            assert_eq!(object::load_rc(word_to_ptr(shared)), abi::RC_STATIC);
                            // Counting a static value changes nothing.
                            rc::dup(both);
                            rc::drop(both);
                            freeze(both);
                        }
                        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
                    });
                }
            })
            .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn size_classes_cover_small_objects() {
        let layout = |size| Layout::from_size_align(size, 8).unwrap();
        assert_eq!(size_class(layout(8)), Some(0));
        assert_eq!(size_class(layout(24)), Some(2));
        assert_eq!(size_class(layout(256)), Some(CACHE_CLASSES - 1));
        assert_eq!(size_class(layout(264)), None);
        assert_eq!(size_class(layout(12)), None);
        for class in 0..CACHE_CLASSES {
            assert_eq!(class_layout(class).and_then(size_class), Some(class));
        }
    }

    /// Without poisoning, freed small blocks go to the thread's cache, up to
    /// its bound, and the next allocation of their size reuses them; with
    /// poisoning the cache stays empty. Accounting is the same either way.
    #[test]
    fn the_block_cache_reuses_and_bounds_blocks() {
        let handle = std::thread::spawn(|| {
            let objects: Vec<Word> = (0..CACHE_BLOCKS.saturating_mul(2))
                .map(|_| tuple(&[abi::tag_int(1).unwrap()]))
                .collect();
            let addresses: std::collections::BTreeSet<Word> = objects.iter().copied().collect();
            for object in objects {
                // SAFETY: the only reference.
                unsafe { rc::drop(object) };
            }
            assert_eq!(live(), 0);
            if poison_enabled() {
                assert_eq!(cached_blocks(), 0);
                return;
            }
            assert_eq!(cached_blocks(), CACHE_BLOCKS);
            let again = tuple(&[abi::tag_int(2).unwrap()]);
            assert!(addresses.contains(&again), "the block was not reused");
            assert_eq!(cached_blocks(), CACHE_BLOCKS - 1);
            assert_eq!(live(), 1);
            // SAFETY: the only reference.
            unsafe { rc::drop(again) };
            assert_eq!(live(), 0);
            // A panicked entry gives its cached blocks back before parking.
            release_block_cache();
            assert_eq!(cached_blocks(), 0);
        });
        handle.join().unwrap();
    }

    /// Runs the block-cache test and the list, string and map tests in a
    /// child process with poisoning off, so the cache's path is tested in
    /// every run (this crate's tests poison by default).
    #[test]
    #[cfg_attr(miri, ignore = "starts a process")]
    fn the_block_cache_works_without_poisoning() {
        for filter in [
            "alloc::tests::the_block_cache_reuses_and_bounds_blocks",
            "list::tests::",
            "hamt::tests::",
            "text::tests::",
            "string::tests::",
        ] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([filter, "--test-threads=4"])
                .env("CHEBY_RT_POISON", "0")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "{filter}: {stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!stdout.contains("running 0 tests"), "{filter}: {stdout}");
        }
    }

    #[test]
    fn the_quarantine_is_bounded() {
        if !poison_enabled() {
            return;
        }
        let handle = std::thread::spawn(|| {
            for _ in 0..QUARANTINE_BLOCKS.saturating_mul(2) {
                // SAFETY: the only reference.
                unsafe { rc::drop(tuple(&[])) };
            }
            quarantine_len()
        });
        assert_eq!(handle.join().unwrap(), QUARANTINE_BLOCKS);
    }

    /// Runs this test binary's `child` test in a child process with the
    /// given `CHEBY_RT_TEST_UAF` operation, and returns its standard error
    /// and whether it succeeded.
    fn run_child(operation: &str) -> (String, bool) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "alloc::tests::use_after_free_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("CHEBY_RT_POISON", "1")
            .env("CHEBY_RT_TEST_UAF", operation)
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&output.stderr).into_owned(),
            output.status.success(),
        )
    }

    /// The child side of `use_after_free_is_fatal`: does nothing unless
    /// `CHEBY_RT_TEST_UAF` names an operation.
    #[test]
    fn use_after_free_child() {
        let Ok(operation) = std::env::var("CHEBY_RT_TEST_UAF") else {
            return;
        };
        let word = crate::values::int_word(i64::MAX);
        // SAFETY: the only reference: the object is freed (and poisoned).
        unsafe { rc::cheby_rt_drop(word) };
        // SAFETY: deliberately not: each operation reaches a freed object,
        // which the poisoning mode must catch before touching it.
        unsafe {
            match operation.as_str() {
                "dup" => rc::cheby_rt_dup(word),
                "drop" => rc::cheby_rt_drop(word),
                "read" => {
                    let _ = crate::values::word_to_i64(word);
                }
                "forget" => forget(word),
                _ => {}
            }
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "starts a process")]
    fn use_after_free_is_fatal() {
        for operation in ["dup", "drop", "read", "forget"] {
            let (stderr, success) = run_child(operation);
            assert!(!success, "{operation}: the child succeeded");
            assert!(
                stderr.contains("fatal runtime error: use after free: "),
                "{operation}: {stderr}"
            );
        }
    }
}
