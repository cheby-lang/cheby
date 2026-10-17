//! Allocation, freeing, accounting domains and the poisoning mode (F1).
//!
//! Every heap object is allocated 8-aligned through the global allocator.
//! Its size is not stored: freeing recomputes it from the layout table, or
//! from `cap` for strings and bytes, or from a runtime-defined kind's
//! `size` function, exactly as allocation computed it.
//!
//! Freeing an object visits the references it owns (its leading value
//! fields, or a runtime-defined kind's `children`) and releases each one,
//! then runs the kind's finalizer, if it has one, which releases what the
//! object's Rust payload owns the same way (the handle kinds, D-703). It
//! is iterative, with an explicit worklist, so freeing a list of a million
//! cells, or a chain of a million handles, uses no more stack than freeing
//! one (F1, D-094).
//!
//! # Accounting domains
//!
//! Live objects and bytes are counted per [`Domain`] (plan §4.3.4, D-564,
//! D-691), not per process. Allocation charges the allocating fiber's
//! domain, and freeing and [`forget`] the freeing fiber's: the domain in
//! its control block, which `run_entry` sets from `EntryConfig::domain`
//! (D-685). A thread that runs no fiber, host code, charges its current
//! domain, kept in `tls.rs`'s host slot: a fresh domain of its own unless
//! [`set_current_domain`] or [`with_domain`] chose one, so tests on
//! parallel threads never see each other's objects. An object freed by
//! another fiber or thread than the one that allocated it is charged to
//! the freeing one's domain; today objects never cross fibers except
//! between a host and the entries it runs with its own domain.
//!
//! A domain's counters are sharded (D-691): each thread charges its own
//! cache line, and reading sums the shards, which is exact whenever no
//! thread is charging the domain meanwhile (at exit, at a test's end, or
//! during a constant evaluation, which runs on one thread). A thread that
//! runs fibers holds an exclusive shard while one is free, and charges it
//! with plain relaxed stores; other threads share the remaining shards
//! and charge them with atomic adds (D-773).
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
use std::collections::VecDeque;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use crossbeam_utils::CachePadded;

use crate::abi::{self, Word};
use crate::ctx::ExecCtx;
use crate::fiber::sanitizer;
use crate::layout::{self, CustomKind, LayoutKind};
use crate::object::{
    self, load_rc, read_field, scan_and_layout, store_rc, to_u64, to_usize, word_to_ptr,
};
use crate::panic::fatal;
use crate::rc;
use crate::stats::{Event, Stats};
use crate::tls;

/// Whether this build checks for leaks at exit (debug builds only, D-458).
/// Accounting itself is always on: constant evaluation's memory limit
/// reads it in every build (D-536).
pub const LEAK_CHECK_ENABLED: bool = cfg!(debug_assertions);

/// An accounting domain: the live objects and bytes, and the peak of live
/// bytes, of everything allocated by the fibers and host threads that
/// charge it (plan §4.3.4, D-564). One build shares one domain across its
/// constant entries and its `main` or test entries.
///
/// The counters are sharded (D-691): each thread charges the shard its
/// `tls.rs` shard index selects, so threads charging one domain do not
/// share a cache line, and reading sums the shards. The peak is sampled:
/// when a shard's own bytes pass its high, the sum of all shards is
/// taken, which is exact when one thread charges the domain (a constant
/// evaluation, D-617) and a lower bound otherwise (D-635 allows it). Once
/// several shards change, a shard's high also moves ahead of its bytes,
/// so the sum is taken only now and then ([`Domain::sample_peak`]).
#[derive(Debug)]
pub struct Domain {
    shards: Box<[CachePadded<Shard>]>,
    peak: AtomicI64,
    stats: Stats,
    /// Whether the system allocator refused a request of a fiber or host
    /// thread charging this domain (D-771): the update
    /// in flight may then have leaked what it held, so `run_main` skips
    /// the debug leak check.
    system_refused: AtomicBool,
    /// The live objects of each runtime kind, Sender to Selector (layouts
    /// 40 to 44), for D-711's line; debug builds only, so release builds
    /// pay nothing (D-784). Not sharded: only handle constructors and the
    /// free worklist's custom-kind branch touch it.
    #[cfg(debug_assertions)]
    kinds: [AtomicI64; RUNTIME_KINDS],
    /// How often a charge summed the shards to sample the peak.
    #[cfg(test)]
    samples: std::sync::atomic::AtomicU64,
}

/// The number of runtime kinds whose live objects debug builds count per
/// domain (D-711): `Sender`, `Receiver`, `Fiber`, `Scope` and `Selector`,
/// the layouts 40 to 44, in that order.
pub const RUNTIME_KINDS: usize = 5;

/// The debug text of each runtime kind, in kind order (D-233, D-784).
const RUNTIME_KIND_NAMES: [&str; RUNTIME_KINDS] =
    ["<Sender>", "<Receiver>", "<Fiber>", "<Scope>", "<Selector>"];

/// The index of the runtime kind of layout `id` among the counted kinds,
/// or `None` for any other layout.
#[cfg(debug_assertions)]
fn runtime_kind_index(id: abi::LayoutId) -> Option<usize> {
    id.checked_sub(abi::layout::SENDER)
        .and_then(|index| usize::try_from(index).ok())
        .filter(|&index| index < RUNTIME_KINDS)
}

/// D-711's line, which follows D-458's when the debug leak check fails:
/// the counts of the leaked objects of each runtime kind, in kind order,
/// then the other leaked objects, for example `note: leaked 1 <Receiver>,
/// and 1 object of other kinds` (D-784). The tail is left out when every
/// leaked object is of a runtime kind. `kinds` are the live objects of
/// each runtime kind ([`Domain::live_kinds`], or a test's delta of it) and
/// `live` all live objects, those included. `None` when no runtime-kind
/// object leaked, and always in a release build, which does not count
/// kinds.
#[must_use]
pub fn leaked_kinds_line(kinds: [i64; RUNTIME_KINDS], live: i64) -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let mut parts = Vec::new();
    let mut counted: i64 = 0;
    for (count, name) in kinds.into_iter().zip(RUNTIME_KIND_NAMES) {
        if count > 0 {
            parts.push(format!("{count} {name}"));
            counted = counted.saturating_add(count);
        }
    }
    if parts.is_empty() {
        return None;
    }
    match live.saturating_sub(counted) {
        1 => parts.push("and 1 object of other kinds".to_owned()),
        others if others > 1 => parts.push(format!("and {others} objects of other kinds")),
        _ => {}
    }
    Some(format!("note: leaked {}", parts.join(", ")))
}

/// One shard of a domain's counters.
#[derive(Debug, Default)]
struct Shard {
    objects: AtomicI64,
    bytes: AtomicI64,
    /// The bytes above which this shard's next charge samples the peak:
    /// the most it held since the last peak reset, or more once another
    /// shard changed ([`Domain::sample_peak`]).
    high: AtomicI64,
}

/// The number of shards of every domain (D-691): the next power of two of
/// the available parallelism, at most 64, read once per process.
pub(crate) fn shard_count() -> usize {
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| {
        std::thread::available_parallelism()
            .map_or(1, std::num::NonZero::get)
            .checked_next_power_of_two()
            .unwrap_or(64)
            .min(64)
    })
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            // The exclusive shards, then as many shared ones
            // (D-773).
            shards: (0..shard_count().saturating_mul(2))
                .map(|_| CachePadded::new(Shard::default()))
                .collect(),
            peak: AtomicI64::new(0),
            stats: Stats::default(),
            system_refused: AtomicBool::new(false),
            #[cfg(debug_assertions)]
            kinds: Default::default(),
            #[cfg(test)]
            samples: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl Domain {
    /// A fresh domain with nothing live.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The shard of index `index` (`tls::shard_index`), and whether it is
    /// exclusive to the charging thread (D-773): an
    /// index below the number of exclusive shards is one, and any other
    /// is reduced to one of the shared shards that follow them (a power
    /// of two).
    fn shard(&self, index: usize) -> (&Shard, bool) {
        let exclusive = self.shards.len() / 2;
        let (slot, alone) = match index.checked_sub(exclusive) {
            None => (index, true),
            Some(shared) => (
                exclusive.saturating_add(shared & exclusive.saturating_sub(1)),
                false,
            ),
        };
        match self.shards.get(slot) {
            Some(shard) => (shard, alone),
            None => fatal(format_args!("an accounting domain has no shards")),
        }
    }

    /// Objects allocated and not yet freed or forgotten: the sum of the
    /// shards.
    #[must_use]
    pub fn live_objects(&self) -> i64 {
        self.shards
            .iter()
            .map(|shard| shard.objects.load(Ordering::Relaxed))
            .fold(0, i64::wrapping_add)
    }

    /// Live objects of the runtime kinds 40 to 44 (Sender, Receiver,
    /// Fiber, Scope, Selector), counted in debug builds only; zeros in
    /// release builds (D-711). They are among [`Domain::live_objects`].
    #[cfg(debug_assertions)]
    #[must_use]
    pub fn live_kinds(&self) -> [i64; RUNTIME_KINDS] {
        self.kinds
            .each_ref()
            .map(|count| count.load(Ordering::Relaxed))
    }

    /// Live objects of the runtime kinds 40 to 44 (Sender, Receiver,
    /// Fiber, Scope, Selector), counted in debug builds only; zeros in
    /// release builds (D-711).
    #[cfg(not(debug_assertions))]
    #[must_use]
    pub fn live_kinds(&self) -> [i64; RUNTIME_KINDS] {
        [0; RUNTIME_KINDS]
    }

    /// Adds `delta` to the live objects of the runtime kinds (debug builds
    /// only, D-711).
    #[cfg(debug_assertions)]
    fn count_kinds(&self, delta: &[i64; RUNTIME_KINDS]) {
        for (count, &delta) in self.kinds.iter().zip(delta) {
            if delta != 0 {
                count.fetch_add(delta, Ordering::Relaxed);
            }
        }
    }

    /// Bytes of those objects: the sum of the shards.
    #[must_use]
    pub fn live_bytes(&self) -> i64 {
        self.shards
            .iter()
            .map(|shard| shard.bytes.load(Ordering::Relaxed))
            .fold(0, i64::wrapping_add)
    }

    /// The most bytes live at once since the domain was created or
    /// [`Domain::reset_peak`] was last called, as sampled.
    #[must_use]
    pub fn peak_bytes(&self) -> i64 {
        self.peak.load(Ordering::Relaxed).max(self.live_bytes())
    }

    /// Restarts the peak at the current live bytes, for measuring one
    /// evaluation (D-536).
    pub fn reset_peak(&self) {
        for shard in &self.shards {
            shard
                .high
                .store(shard.bytes.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        self.peak.store(self.live_bytes(), Ordering::Relaxed);
    }

    /// The domain's runtime counters ([`crate::stats`]).
    #[must_use]
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Whether the system allocator refused a request charged to this
    /// domain (D-771). An in-place update of a list,
    /// map or text that it stopped halfway may have leaked the structure
    /// in flight, so the debug leak check after such a run would report a
    /// leak the program did not make.
    #[must_use]
    pub fn system_refused(&self) -> bool {
        self.system_refused.load(Ordering::Relaxed)
    }

    /// Marks the domain: the system allocator refused one of its requests.
    fn mark_system_refused(&self) {
        self.system_refused.store(true, Ordering::Relaxed);
    }

    /// Adds `objects` and `bytes` to shard `index`. An exclusive shard has
    /// one writer, the thread that holds it, so it is charged with a
    /// relaxed load and store, which cost less than an atomic add even
    /// uncontended (D-773); readers sum relaxed loads
    /// as before. A shared shard takes atomic adds.
    fn charge(&self, index: usize, objects: i64, bytes: i64) {
        let (shard, alone) = self.shard(index);
        let before = if alone {
            debug_assert!(
                tls::holds_exclusive_shard(index),
                "an exclusive shard charged by a thread that does not hold it"
            );
            let count = shard.objects.load(Ordering::Relaxed);
            shard
                .objects
                .store(count.wrapping_add(objects), Ordering::Relaxed);
            let before = shard.bytes.load(Ordering::Relaxed);
            shard
                .bytes
                .store(before.wrapping_add(bytes), Ordering::Relaxed);
            before
        } else {
            shard.objects.fetch_add(objects, Ordering::Relaxed);
            shard.bytes.fetch_add(bytes, Ordering::Relaxed)
        };
        let now = before.saturating_add(bytes);
        // Most allocations stay under the shard's high: a load of its own
        // line, not a write.
        if bytes > 0 && now > shard.high.load(Ordering::Relaxed) {
            self.sample_peak(shard, now);
        }
    }

    /// Sums the shards to sample the peak, now that `shard` holds `now`
    /// bytes, above its high, and moves its high to where the next sample
    /// is due.
    ///
    /// With one shard charged, the sum exceeds the peak at every new high
    /// of that shard, and the high stays at `now`, so the peak is exact. A
    /// sum that does not exceed the peak shows that another shard changed
    /// since, and the peak is a lower bound from then on (D-635). The next
    /// sample is then due once this shard could have made a new peak with
    /// the others unchanged, or has grown by an eighth, whichever is later.
    /// So a shard that only allocates while another only frees, as across
    /// a channel between two workers, reads the other's cache line only a
    /// logarithmic number of times, not on every allocation (D-691).
    fn sample_peak(&self, shard: &Shard, now: i64) {
        #[cfg(test)]
        self.samples.fetch_add(1, Ordering::Relaxed);
        let total = self.live_bytes();
        let peak = self.peak.load(Ordering::Relaxed);
        let next = if total > peak {
            self.peak.fetch_max(total, Ordering::Relaxed);
            now
        } else {
            now.saturating_add(peak.saturating_sub(total).max(now / 8))
        };
        shard.high.fetch_max(next, Ordering::Relaxed);
    }
}

/// The control block of the fiber running on this thread, if any.
fn current_fiber<'a>() -> Option<&'a ExecCtx> {
    crate::entry::current_fiber()
}

/// Calls `f` with the host slot's current domain, creating a fresh one on
/// first use; `None` during thread teardown. `f` must not reach the host
/// slot's domain again.
fn with_host_domain<R>(f: impl FnOnce(&Arc<Domain>) -> R) -> Option<R> {
    tls::with_host(|host| {
        let Ok(mut slot) = host.domain.try_borrow_mut() else {
            fatal(format_args!("the host's accounting domain is in use"));
        };
        f(slot.get_or_insert_with(Domain::new))
    })
}

/// Calls `f` with what `ctx` charges, its domain and shard, or, without a
/// fiber, with the host slot's domain and this thread's shard. `None`
/// during thread teardown, when the host slot is gone.
fn with_charged<R>(ctx: Option<&ExecCtx>, f: impl FnOnce(&Domain, usize) -> R) -> Option<R> {
    if let Some(ctx) = ctx {
        Some(f(ctx.domain(), ctx.shard()))
    } else {
        let shard = tls::shard_index();
        with_host_domain(|domain| f(domain, shard))
    }
}

/// The current accounting domain: the running fiber's, or on a thread that
/// runs no fiber, its own current domain.
#[must_use]
pub fn current_domain() -> Arc<Domain> {
    match current_fiber() {
        Some(ctx) => Arc::clone(ctx.domain()),
        None => with_host_domain(Arc::clone).unwrap_or_else(Domain::new),
    }
}

/// Makes `domain` the current accounting domain of this thread's host
/// code and returns the previous one. It does not change the domain of a
/// fiber running on the thread, which its entry chose.
pub fn set_current_domain(domain: Arc<Domain>) -> Arc<Domain> {
    let mut domain = Some(domain);
    let previous = tls::with_host(|host| {
        let Ok(mut slot) = host.domain.try_borrow_mut() else {
            fatal(format_args!("the host's accounting domain is in use"));
        };
        std::mem::replace(&mut *slot, domain.take())
    })
    .flatten();
    previous.or(domain).unwrap_or_else(Domain::new)
}

/// Runs `f` with `domain` as the current domain of this thread's host
/// code, then restores the previous one.
pub fn with_domain<R>(domain: Arc<Domain>, f: impl FnOnce() -> R) -> R {
    let previous = set_current_domain(domain);
    let result = f();
    set_current_domain(previous);
    result
}

/// Charges the current domain. During thread teardown, when the host slot
/// is gone, nothing is charged.
fn charge(objects: i64, bytes: u64) {
    let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
    let bytes = if objects < 0 {
        bytes.saturating_neg()
    } else {
        bytes
    };
    with_charged(current_fiber(), |domain, shard| {
        domain.charge(shard, objects, bytes);
    });
}

/// Records `event` in the counters of the current domain; during thread
/// teardown, when the host slot is gone, does nothing. `stats::record`
/// calls it only when events are counted (D-768).
pub(crate) fn record_event(event: Event) {
    with_charged(current_fiber(), |domain, shard| {
        domain.stats.record(shard, event);
    });
}

/// The number of objects live in the current domain.
#[must_use]
pub fn live() -> i64 {
    match current_fiber() {
        Some(ctx) => ctx.domain().live_objects(),
        None => with_host_domain(|domain| domain.live_objects()).unwrap_or(0),
    }
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
                // A runtime kind cannot reach a constant (E0077); should
                // one, it leaves the kind counts with the object.
                #[cfg(debug_assertions)]
                {
                    // SAFETY: a live heap object.
                    let (_, id) = unsafe { scan_and_layout(obj) };
                    let mut delta = [0; RUNTIME_KINDS];
                    tally_kind(&mut delta, id, -1);
                    charge_kinds(current_fiber(), &delta);
                }
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
    #[cfg(debug_assertions)]
    {
        // SAFETY: forwarded from the caller.
        let (_, id) = unsafe { scan_and_layout(obj) };
        let mut delta = [0; RUNTIME_KINDS];
        tally_kind(&mut delta, id, -1);
        charge_kinds(current_fiber(), &delta);
    }
}

/// Allocates an object for generated code: `size` bytes including the
/// header, 8-aligned, with `header` written at offset 0. The fields are
/// left uninitialized for the caller to write. A request that cannot be
/// served ends as [`out_of_memory`] says.
///
/// `ctx` is the allocating fiber's control block, whose domain the object
/// is charged to (D-691); it may be null, and then the current fiber's
/// domain, or a host thread's current domain, is charged.
///
/// # Safety
///
/// `ctx` is null or the running fiber's control block; `header` describes
/// an object of exactly `size` bytes (as the layout table computes it),
/// and the caller initializes every field before the object is freed or
/// read.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_alloc(ctx: *mut ExecCtx, size: u64, header: u64) -> Word {
    // A request that cannot be served raises (D-642); under the flag
    // backend the result is then 0, which generated code tests (D-697).
    crate::unwind::guard::guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        object::ptr_to_word(unsafe { alloc_for(ctx, size, header) })
    })
}

/// [`cheby_rt_alloc`] for kernels that are given their fiber's control
/// block: allocates an object of `size` bytes with `header`, charged to
/// `ctx`'s domain, or when `ctx` is null to the current fiber's or the
/// host's. A request that cannot be served ends as [`out_of_memory`] says.
///
/// # Safety
///
/// As for [`cheby_rt_alloc`].
pub(crate) unsafe fn alloc_for(ctx: *mut ExecCtx, size: u64, header: u64) -> *mut u8 {
    // SAFETY: forwarded from the caller.
    let ctx = unsafe { ctx.as_ref() }.or_else(current_fiber);
    alloc_in(ctx, size, header)
}

/// Allocates an object of `size` bytes (rounded up to 8) with the given
/// header and uninitialized fields, and charges it to the current domain:
/// the running fiber's, or a host thread's. During constant evaluation, a
/// request that would pass the memory budget abandons the entry with the
/// memory limit before anything is allocated (D-536, D-617). A request
/// that cannot be served, because its size does not fit the address space
/// or the system refuses it, ends as [`out_of_memory`] says. Kernels
/// allocate through it.
#[must_use]
pub fn alloc_object(size: u64, header: u64) -> *mut u8 {
    let layout = try_object_layout(size);
    let class = layout.and_then(size_class);
    // One `tls.rs` accessor call finds both the fiber and a cached block
    // (GATE1-1). A fiber under a memory budget checks it before anything
    // is taken, in `alloc_in`.
    let (ctx, cached, tried) = with_fiber_cache(None, |ctx, cache| {
        let unlimited = ctx.is_none_or(|ctx| ctx.budget() == u64::MAX) && !refusal_armed();
        match (cache, class) {
            (Some(cache), Some(class)) if unlimited => (ctx, cache.take(class), true),
            _ => (ctx, None, false),
        }
    });
    match (cached, layout) {
        (Some(ptr), Some(layout)) => init_object(ctx, ptr, layout, header),
        _ => alloc_from(ctx, size, header, !tried),
    }
}

/// [`alloc_object`] for the fiber `ctx`, or for host code without one.
fn alloc_in(ctx: Option<&ExecCtx>, size: u64, header: u64) -> *mut u8 {
    alloc_from(ctx, size, header, true)
}

/// Allocates as [`alloc_in`] says: checks the memory budget, then takes
/// a block from the block cache when `try_cache` (the caller did not just
/// find it empty) or else from the system.
fn alloc_from(ctx: Option<&ExecCtx>, size: u64, header: u64, try_cache: bool) -> *mut u8 {
    let request = size
        .max(abi::HEADER_SIZE)
        .checked_next_multiple_of(abi::OBJECT_ALIGN)
        .unwrap_or(u64::MAX);
    crate::entry::check_budget_of(ctx, request);
    let Some(layout) = try_object_layout(size) else {
        out_of_memory(request)
    };
    let cached = if try_cache && !refusal_armed() {
        cached_block(ctx, layout)
    } else {
        None
    };
    let ptr = cached.unwrap_or_else(|| {
        let ptr = if test_refuses() {
            std::ptr::null_mut()
        } else {
            // SAFETY: `try_object_layout` never returns a zero-sized layout.
            unsafe { std::alloc::alloc(layout) }
        };
        if ptr.is_null() {
            system_refused(ctx, request);
        }
        ptr
    });
    init_object(ctx, ptr, layout, header)
}

/// Ends a request of `request` bytes that the system allocator refused,
/// as [`out_of_memory`] says, after marking the domain `ctx` charges
/// (D-771): the refusal may stop an in-place update
/// halfway and leak the structure in flight, so `run_main` skips the
/// debug leak check of that domain and counts the skip.
fn system_refused(ctx: Option<&ExecCtx>, request: u64) -> ! {
    with_charged(ctx, |domain, _| domain.mark_system_refused());
    out_of_memory(request)
}

/// The test hook of [`refuse_next_system_allocation`]: the thread it
/// refuses on and how many system allocations of that thread to let
/// through first. `None` when nothing is armed.
#[cfg(test)]
static REFUSAL: std::sync::Mutex<Option<(std::thread::ThreadId, u32)>> =
    std::sync::Mutex::new(None);

/// Whether [`REFUSAL`] holds anything: a relaxed load before its lock.
#[cfg(test)]
static REFUSAL_ARMED: AtomicBool = AtomicBool::new(false);

/// Makes the next allocation the runtime requests from the system
/// allocator fail, as if the system had refused it. A real atomic flag: it
/// arms the calling thread (the tests of this crate share the process), and
/// while it is armed that thread's allocations bypass the block cache, so
/// the next one reaches the system path, which tests and clears the flag
/// and then ends the request as a refusal does (D-771).
/// One thread is armed at a time: tests that arm it hold
/// `alloc::tests::serial()`. It does not reach `alloc::scratch`, which
/// grows with
/// `try_reserve_exact` through the global allocator and has a hook of its
/// own.
#[cfg(test)]
pub(crate) fn refuse_next_system_allocation() {
    refuse_system_allocation_after(0);
}

/// [`refuse_next_system_allocation`] after `allowed` further system
/// allocations of the calling thread succeed: a refusal in the middle of
/// an update that allocates several objects.
#[cfg(test)]
pub(crate) fn refuse_system_allocation_after(allowed: u32) {
    *REFUSAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((std::thread::current().id(), allowed));
    REFUSAL_ARMED.store(true, Ordering::SeqCst);
}

/// Whether a refusal is armed for this thread, which makes its
/// allocations bypass the block cache. Always false outside tests.
#[inline]
fn refusal_armed() -> bool {
    #[cfg(test)]
    {
        if REFUSAL_ARMED.load(Ordering::Relaxed) {
            let armed = *REFUSAL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            return armed.is_some_and(|(thread, _)| thread == std::thread::current().id());
        }
    }
    false
}

/// Whether the test hook refuses this system allocation: it counts down
/// the allocations let through on the armed thread, then refuses one and
/// clears itself. Always false outside tests.
#[inline]
fn test_refuses() -> bool {
    #[cfg(test)]
    {
        if REFUSAL_ARMED.load(Ordering::Relaxed) {
            let mut armed = REFUSAL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match *armed {
                Some((thread, 0)) if thread == std::thread::current().id() => {
                    *armed = None;
                    REFUSAL_ARMED.store(false, Ordering::SeqCst);
                    return true;
                }
                Some((thread, allowed)) if thread == std::thread::current().id() => {
                    *armed = Some((thread, allowed.saturating_sub(1)));
                }
                Some(_) | None => {}
            }
        }
    }
    false
}

/// Writes the header of a fresh block of `layout` and charges it to
/// `ctx`'s domain, or without a fiber to the host's, counting the
/// allocation when events are counted (D-768).
fn init_object(ctx: Option<&ExecCtx>, ptr: *mut u8, layout: Layout, header: u64) -> *mut u8 {
    // SAFETY: the allocation is at least 8 bytes and 8-aligned.
    unsafe { object::write_field(ptr, 0, header) };
    let bytes = i64::try_from(layout.size()).unwrap_or(i64::MAX);
    with_charged(ctx, |domain, shard| {
        domain.charge(shard, 1, bytes);
        if crate::stats::counting() {
            domain.stats.record(shard, Event::Alloc);
        }
    });
    ptr
}

/// Checks constant evaluation's memory budget of the fiber `ctx` for
/// `bound()`, an upper bound of what an in-place update about to start
/// allocates: a runtime helper calls it with its own `ctx` before it takes
/// its owned arguments over, so that running out of budget raises while
/// it can still release them whole (D-698). The limit can then end an
/// evaluation by at most the bound earlier than the update's own requests
/// would (D-738). Without a budget, and without a fiber (host code and
/// tests that pass a null `ctx`), it returns before computing the bound,
/// and it never reads a thread-local (D-686).
#[inline]
pub(crate) fn preflight(ctx: Option<&ExecCtx>, bound: impl FnOnce() -> u64) {
    let Some(ctx) = ctx else {
        return;
    };
    if ctx.budget() == u64::MAX {
        return;
    }
    crate::entry::check_budget_of(Some(ctx), bound());
}

/// The message of the panic that an allocation request which cannot be
/// served raises when it is not over constant evaluation's memory limit
/// (D-642).
pub const OUT_OF_MEMORY: &str = "out of memory";

/// Ends a request for `bytes` bytes that cannot be served: its size
/// overflows 64 bits or the address space, or the system allocator
/// refused it. Kernels call it too when the size of their result cannot
/// even be computed, with `u64::MAX`, and so do the scratch buffers of the
/// runtime's walks ([`scratch`]).
///
/// During constant evaluation a request over the memory budget abandons
/// the evaluation with the memory limit, E0076, like any request over the
/// limit (D-617); a size that overflows is over every budget. Anywhere
/// else, and for a request under the budget that the system refuses
/// during constant evaluation, it panics the running entry with the
/// message [`OUT_OF_MEMORY`] at an unknown location: `cheby run` exits
/// with 101, `cheby test` fails the test, the REPL abandons the entry and
/// continues (D-457), and a constant gives E0075, since raising the limit
/// would not help (D-642). A panic is what §11.8 makes of every other
/// condition a program's values cause, and it keeps a REPL session alive.
/// Nothing has been allocated for the request, so the panic itself can
/// still allocate its report.
/// Outside any entry, which only the runtime's own host-side callers
/// reach, it ends the process.
pub fn out_of_memory(bytes: u64) -> ! {
    // The system may have nothing left at all, not even for the report of
    // the panic or the memory-limit error: give it the reserve first.
    release_reserve();
    crate::entry::check_budget(bytes);
    if crate::entry::current_ctx().is_none() {
        fatal(format_args!("{OUT_OF_MEMORY}"));
    }
    // The reserve is set aside again when the unwinding's catch completes
    // (D-642, `unwind::record::take_record`), or when the next entry
    // starts.
    crate::unwind::raise::raise_panic(None, abi::PanicKind::Explicit, Some(OUT_OF_MEMORY))
}

/// The memory set aside so that an entry whose request the system refused
/// can still end in order: [`out_of_memory`] frees it before it reports,
/// so a program that used up all memory still panics with `out of memory`
/// and does not abort while it reports. Each entry sets it aside again
/// when it starts ([`ensure_reserve`]).
static RESERVE: std::sync::Mutex<Option<Vec<u8>>> = std::sync::Mutex::new(None);

/// The size of [`RESERVE`].
const RESERVE_BYTES: usize = 4 << 20;

/// Sets the reserve aside if it is not, when the system can spare it.
pub(crate) fn ensure_reserve() {
    let mut reserve = RESERVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if reserve.is_none() {
        let mut block = Vec::new();
        if block.try_reserve_exact(RESERVE_BYTES).is_ok() {
            *reserve = Some(block);
        }
    }
}

/// Whether the reserve is set aside now.
#[cfg(test)]
pub(crate) fn reserve_armed() -> bool {
    RESERVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
}

/// Gives the reserve back to the system.
fn release_reserve() {
    let block = RESERVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    drop(block);
}

/// The allocation layout of an object of `size` bytes, or `None` when the
/// rounded size overflows or is too large for the address space.
fn try_object_layout(size: u64) -> Option<Layout> {
    let rounded = size
        .max(abi::HEADER_SIZE)
        .checked_next_multiple_of(abi::OBJECT_ALIGN)?;
    let rounded = usize::try_from(rounded).ok()?;
    Layout::from_size_align(rounded, to_usize(abi::OBJECT_ALIGN)).ok()
}

/// The allocation layout of an existing object of `size` bytes. A size that
/// has no layout was never allocated, so it breaks an invariant.
fn object_layout(size: u64) -> Layout {
    try_object_layout(size)
        .unwrap_or_else(|| fatal(format_args!("an object of {size} bytes has no layout")))
}

/// Capacity above which the worklist is not kept for the next free. The
/// worklist of objects whose count reached zero and whose fields are still
/// to be released is kept per thread, in `tls.rs`, so that its capacity is
/// reused across frees.
const WORKLIST_KEEP: usize = 1 << 16;

/// How many dying objects one free keeps on the Rust stack before it takes
/// the thread's worklist. Most frees never need more: freeing a chain
/// holds one object at a time, and most objects free no child at all.
const PENDING_INLINE: usize = 16;

/// The objects of one free whose count reached zero and whose references
/// are still to be released: a few on the Rust stack, then the thread's
/// worklist, taken from `tls.rs` only when they do not fit. Freeing a
/// string or a tuple whose fields stay alive then touches no thread-local
/// for the list at all (GATE1-1: each `tls.rs` accessor is a call that
/// is never inlined, D-686).
struct Pending {
    /// The first `len` slots are written; the rest are not initialized, so
    /// a free does not clear the whole array first.
    inline: [MaybeUninit<Word>; PENDING_INLINE],
    len: usize,
    spill: Option<Vec<Word>>,
    /// The objects of each runtime kind freed so far, negative, charged
    /// with the objects once the free ends (debug builds only, D-711).
    #[cfg(debug_assertions)]
    kinds: [i64; RUNTIME_KINDS],
}

impl Pending {
    /// A list holding `first`.
    fn new(first: Word) -> Self {
        let mut inline = [MaybeUninit::uninit(); PENDING_INLINE];
        inline[0] = MaybeUninit::new(first);
        Self {
            inline,
            len: 1,
            spill: None,
            #[cfg(debug_assertions)]
            kinds: [0; RUNTIME_KINDS],
        }
    }

    /// Adds an object whose count reached zero.
    fn push(&mut self, word: Word) {
        if let Some(slot) = self.inline.get_mut(self.len) {
            *slot = MaybeUninit::new(word);
            self.len = self.len.saturating_add(1);
            return;
        }
        // During thread teardown the worklist may already be gone; a fresh
        // vector works the same. The list is taken out of the thread-local
        // while it is used, so nothing else reaches it meanwhile.
        self.spill
            .get_or_insert_with(|| tls::with_worklist(std::cell::Cell::take).unwrap_or_default())
            .push(word);
    }

    /// Takes an object off the list, in no particular order.
    fn pop(&mut self) -> Option<Word> {
        if let Some(word) = self.spill.as_mut().and_then(Vec::pop) {
            return Some(word);
        }
        self.len = self.len.checked_sub(1)?;
        // SAFETY: every slot below the previous `len` was written by `new`
        // or `push`.
        self.inline
            .get(self.len)
            .map(|slot| unsafe { slot.assume_init() })
    }

    /// Gives the thread's worklist back, if it was taken and is not too
    /// large to keep.
    fn finish(self) {
        if let Some(mut list) = self.spill {
            list.clear();
            if list.capacity() <= WORKLIST_KEEP {
                tls::with_worklist(|cell| cell.set(list));
            }
        }
    }
}

/// Frees an object whose count just reached zero, then every object that
/// only its fields kept alive, running the finalizer of each object of a
/// kind that has one (D-703).
///
/// It reads the thread-locals it needs once per call, not once per freed
/// object: the running fiber, whose domain is charged, and the block
/// cache, borrowed for the whole walk, which never suspends, unwinds or
/// raises (a broken invariant or a panicking finalizer ends the process).
///
/// # Safety
///
/// `obj` points to a live heap object whose count has reached zero (no
/// references remain).
pub(crate) unsafe fn free(obj: *mut u8) {
    // SAFETY: forwarded from the caller.
    unsafe { free_in(None, obj) };
}

/// [`free`] by the fiber `ctx`, given by a caller that has its control
/// block: generated code's `cheby_rt_drop` (D-772). The block
/// cache is then read through the control block's record and the domain
/// charged through `ctx`, with no `tls.rs` accessor call (D-739). With
/// `None` the running fiber and the cache are looked up, as [`free`] does.
///
/// # Safety
///
/// As for [`free`]; `ctx`, when given, is the control block of the fiber
/// running on this thread.
pub(crate) unsafe fn free_in(ctx: Option<&ExecCtx>, obj: *mut u8) {
    // SAFETY: forwarded from the caller.
    let (scan, id) = unsafe { scan_and_layout(obj) };
    // A scan count of 0 outside the runtime-defined kinds (26 to 63) is an
    // object that probably owns no reference: a string, a boxed number, or
    // a constructor with raw fields only. `free_leaf` checks its shape.
    if scan == 0
        && !(abi::layout::CUSTOM_FIRST..=abi::layout::BUILTIN_RESERVED_END).contains(&id)
        // SAFETY: forwarded from the caller.
        && unsafe { free_leaf(ctx, obj) }
    {
        return;
    }
    let mut pending = Pending::new(object::ptr_to_word(obj));
    let (ctx, (objects, bytes)) = with_fiber_cache(ctx, |ctx, cache| {
        // SAFETY: forwarded from the caller; every word pushed later is an
        // object whose count reached zero.
        (ctx, unsafe { drain(&mut pending, cache) })
    });
    charge_freed(ctx, objects, bytes);
    #[cfg(debug_assertions)]
    charge_kinds(ctx, &pending.kinds);
    pending.finish();
}

/// Releases the reference each value field of `obj` holds and stores 0 in
/// the field, so that `obj`, a unique object of a constructor or tuple
/// layout that a reuse keeps (D-716, `crate::reuse`), owns nothing and
/// its block can be written over or freed as a childless object. What
/// the fields alone kept alive is freed as [`free_in`] frees, charged to
/// `ctx`'s domain (or the running fiber's). Returns the size of `obj`'s
/// allocation, or `None`, touching nothing, when `obj` is not of a
/// constructor or tuple layout. Never raises: a panicking finalizer of a
/// child ends the process, as in every free (D-171).
///
/// # Safety
///
/// `obj` points to a live heap object whose count is 1 and whose one
/// reference the caller owns; `ctx`, when given, is the control block of
/// the fiber running on this thread.
pub(crate) unsafe fn release_fields(ctx: Option<&ExecCtx>, obj: *mut u8) -> Option<u64> {
    // SAFETY: forwarded from the caller.
    let (_, id) = unsafe { scan_and_layout(obj) };
    let reusable = layout::get(id)
        .is_some_and(|info| matches!(info.kind, LayoutKind::Ctor { .. } | LayoutKind::Tuple));
    if !reusable {
        return None;
    }
    // SAFETY: forwarded from the caller.
    let shape = unsafe { shape_of(obj) };
    let Children::Fields { first, count } = shape.children else {
        return None;
    };
    let mut pending: Option<Pending> = None;
    let mut offset = first;
    for _ in 0..count {
        // SAFETY: the shape lies within the object, which the caller owns
        // uniquely, so no other reference reads the field.
        let child = unsafe { read_field(obj, offset) };
        // SAFETY: as above.
        unsafe { object::write_field(obj, offset, 0) };
        offset = offset.saturating_add(to_usize(abi::FIELD_SIZE));
        if abi::is_tagged_int(child) || child == 0 {
            continue;
        }
        // SAFETY: a value field holds an owned reference to a live object.
        unsafe { check_live(child, "reusing an object") };
        // SAFETY: as above; the field no longer holds it.
        if unsafe { rc::release(word_to_ptr(child)) } {
            match pending.as_mut() {
                Some(list) => list.push(child),
                None => pending = Some(Pending::new(child)),
            }
        }
    }
    if let Some(mut pending) = pending {
        let (ctx, (objects, bytes)) = with_fiber_cache(ctx, |ctx, cache| {
            // SAFETY: every word on the list is an object whose count
            // reached zero.
            (ctx, unsafe { drain(&mut pending, cache) })
        });
        charge_freed(ctx, objects, bytes);
        #[cfg(debug_assertions)]
        charge_kinds(ctx, &pending.kinds);
        pending.finish();
    }
    Some(to_u64(object_layout(shape.size).size()))
}

/// Frees the block of `obj`, a unique object of a constructor or tuple
/// layout whose value fields no longer own anything: generated code
/// released each of them or moved it to a kept projection (a drop
/// specialized by constructor, D-716, `crate::reuse`). The fields are not
/// read. Charges `ctx`'s domain (or the running fiber's) as
/// [`free_leaf`] does. Returns `None`, touching nothing, when `obj` is
/// not of a constructor or tuple layout.
///
/// # Safety
///
/// `obj` points to a live heap object whose count is 1, whose one
/// reference the caller owns and gives up, and whose value fields hold
/// no reference the object owns; `ctx`, when given, is the control block
/// of the fiber running on this thread.
pub(crate) unsafe fn free_unique(ctx: Option<&ExecCtx>, obj: *mut u8) -> Option<()> {
    // SAFETY: forwarded from the caller.
    let (_, id) = unsafe { scan_and_layout(obj) };
    let reusable = layout::get(id)
        .is_some_and(|info| matches!(info.kind, LayoutKind::Ctor { .. } | LayoutKind::Tuple));
    if !reusable {
        return None;
    }
    // SAFETY: forwarded from the caller.
    let shape = unsafe { shape_of(obj) };
    let layout = object_layout(shape.size);
    let ctx = with_fiber_cache(ctx, |ctx, cache| {
        // SAFETY: allocated by `alloc_object` with this size, which
        // `shape_of` recomputes exactly, and no reference remains: the
        // caller gives up the only one, and the fields own nothing.
        unsafe { release_block(obj, layout, cache) };
        ctx
    });
    charge_freed(ctx, -1, to_u64(layout.size()));
    Some(())
}

/// Frees `obj` if it owns no reference and has no finalizer, with no
/// worklist and no walk, and returns whether it did.
///
/// # Safety
///
/// As for [`free_in`].
unsafe fn free_leaf(ctx: Option<&ExecCtx>, obj: *mut u8) -> bool {
    // SAFETY: forwarded from the caller.
    let shape = unsafe { shape_of(obj) };
    let Children::Fields { count: 0, .. } = shape.children else {
        // A value of an external type, whose kind may have a finalizer.
        return false;
    };
    let layout = object_layout(shape.size);
    let ctx = with_fiber_cache(ctx, |ctx, cache| {
        // SAFETY: allocated by `alloc_object` with this size, which
        // `shape_of` recomputes exactly, and no reference remains.
        unsafe { release_block(obj, layout, cache) };
        ctx
    });
    charge_freed(ctx, -1, to_u64(layout.size()));
    true
}

/// Calls `f` with the control block of the fiber running on this thread
/// and this thread's block cache, borrowed for the call: `None` when no
/// fiber runs, and no cache when it is in use (a finalizer freeing
/// directly) or gone (thread teardown). Given `known`, the running fiber's
/// control block, the cache comes from its record and no `tls.rs`
/// accessor is called (D-739, D-772); otherwise one accessor
/// call finds both (GATE1-1). `f` never suspends (D-686).
fn with_fiber_cache<'a, R>(
    known: Option<&'a ExecCtx>,
    f: impl FnOnce(Option<&'a ExecCtx>, Option<&mut BlockCache>) -> R,
) -> R {
    let mut f = Some(f);
    if let Some(ctx) = known {
        let result = tls::with_cache_of(Some(ctx), |cache| {
            let f = f.take()?;
            let mut cache = cache.try_borrow_mut().ok();
            Some(f(Some(ctx), cache.as_deref_mut()))
        })
        .flatten();
        return match (result, f.take()) {
            (Some(result), _) => result,
            // Thread teardown: the cache is gone.
            (None, Some(f)) => f(Some(ctx), None),
            (None, None) => fatal(format_args!("the block cache's closure returned nothing")),
        };
    }
    let result = tls::with_fiber_and_cache(|fiber, cache| {
        let f = f.take()?;
        // SAFETY: as in `entry::current_fiber`: the current fiber is null
        // or the control block of the fiber running on this thread, which
        // stays live while it runs, and only shared references to it are
        // made.
        let ctx = unsafe { fiber.as_ref() };
        let mut cache = cache.try_borrow_mut().ok();
        Some(f(ctx, cache.as_deref_mut()))
    })
    .flatten();
    match (result, f.take()) {
        (Some(result), _) => result,
        // Thread teardown: the cache is gone.
        (None, Some(f)) => f(current_fiber(), None),
        (None, None) => fatal(format_args!("the block cache's closure returned nothing")),
    }
}

/// Charges `ctx`'s domain, or without a fiber the host's, for `objects`
/// objects of `bytes` bytes freed (a negative `objects`).
fn charge_freed(ctx: Option<&ExecCtx>, objects: i64, bytes: u64) {
    let bytes = i64::try_from(bytes).unwrap_or(i64::MAX).saturating_neg();
    with_charged(ctx, |domain, shard| domain.charge(shard, objects, bytes));
}

/// Adds `delta` to the live runtime-kind objects of the domain `ctx`
/// charges, or without a fiber the host's (debug builds only, D-711), as
/// [`charge_freed`] charges the objects.
#[cfg(debug_assertions)]
fn charge_kinds(ctx: Option<&ExecCtx>, delta: &[i64; RUNTIME_KINDS]) {
    if delta.iter().any(|&count| count != 0) {
        with_charged(ctx, |domain, _| domain.count_kinds(delta));
    }
}

/// Adds `by` to `delta`'s count of the runtime kind of layout `id`, if it
/// is one (debug builds only, D-711).
#[cfg(debug_assertions)]
fn tally_kind(delta: &mut [i64; RUNTIME_KINDS], id: abi::LayoutId, by: i64) {
    if let Some(count) = runtime_kind_index(id).and_then(|index| delta.get_mut(index)) {
        *count = count.saturating_add(by);
    }
}

/// Counts a new object of the runtime kind of layout `id` (40 to 44) in
/// the domain it was charged to: `ctx`'s, or the running fiber's, or
/// without a fiber the host's, as [`alloc_for`] chooses (debug builds
/// only, D-711). The handle constructors call it right after allocating.
#[cfg(debug_assertions)]
pub(crate) fn count_new_kind(ctx: Option<&ExecCtx>, id: abi::LayoutId) {
    let mut delta = [0; RUNTIME_KINDS];
    tally_kind(&mut delta, id, 1);
    charge_kinds(ctx.or_else(current_fiber), &delta);
}

/// Releases one reference held by a dying object, queueing the child when
/// that was its last reference.
///
/// # Safety
///
/// `child` is a tagged integer, 0, or a live object the dying object holds
/// a reference to.
#[expect(
    clippy::inline_always,
    reason = "called once per field of every freed object; left out of line, the call was a visible share of freeing (GATE1-1)"
)]
#[inline(always)]
unsafe fn release_child(child: Word, list: &mut Pending) {
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

/// Frees every object on the list and, transitively, the fields whose
/// count reaches zero, running each finalizer once, before its object's
/// block is freed (D-703). Returns the objects freed (negative) and their
/// bytes, for the caller to charge once. Freed blocks go to `cache` when
/// there is one.
///
/// # Safety
///
/// Every word on `list` is a heap object whose count has reached zero.
unsafe fn drain(list: &mut Pending, mut cache: Option<&mut BlockCache>) -> (i64, u64) {
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
                #[cfg(debug_assertions)]
                {
                    // SAFETY: `obj` is still a live object (its block is
                    // released below).
                    let (_, id) = unsafe { scan_and_layout(obj) };
                    tally_kind(&mut list.kinds, id, -1);
                }
                let mut visit = |child: Word| {
                    // SAFETY: every word the kind visits, or its finalizer
                    // gives up, is a reference the dying object owns.
                    unsafe { release_child(child, list) };
                };
                // SAFETY: a live object of this kind.
                unsafe { (kind.children)(word, &mut visit) };
                if let Some(finalize) = kind.finalize {
                    // SAFETY: an object of this kind whose count reached
                    // zero; it is taken off the list once, so this runs
                    // once.
                    unsafe { run_finalizer(kind, finalize, word, &mut visit) };
                }
            }
        }
        let layout = object_layout(shape.size);
        objects = objects.saturating_sub(1);
        bytes = bytes.saturating_add(to_u64(layout.size()));
        // SAFETY: allocated by `alloc_object` with this size, which
        // `shape_of` recomputes exactly, and no reference remains.
        unsafe { release_block(obj, layout, cache.as_deref_mut()) };
    }
    (objects, bytes)
}

/// Runs a kind's finalizer on a dying object (D-703). A Rust panic in it
/// ends the process (D-171): nothing could report it, and unwinding out of
/// a free would leave the rest of the worklist unreleased.
///
/// # Safety
///
/// As for [`CustomKind::finalize`].
unsafe fn run_finalizer(
    kind: &CustomKind,
    finalize: layout::Finalizer,
    word: Word,
    release: &mut dyn FnMut(Word),
) {
    let finished = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: forwarded from the caller.
        unsafe { finalize(word, release) };
    }));
    if finished.is_err() {
        fatal(format_args!(
            "the finalizer of a `{}` value panicked",
            kind.name
        ));
    }
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

/// Freed blocks kept out of the allocator, oldest first: one per thread,
/// in `tls.rs`.
#[derive(Debug, Default)]
pub(crate) struct Quarantine {
    blocks: VecDeque<(*mut u8, Layout)>,
    bytes: usize,
}

impl Quarantine {
    /// An empty quarantine.
    pub(crate) const fn new() -> Self {
        Self {
            blocks: VecDeque::new(),
            bytes: 0,
        }
    }

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

/// The number of blocks in the current thread's quarantine.
#[doc(hidden)]
#[must_use]
pub fn quarantine_len() -> usize {
    tls::with_quarantine(|quarantine| quarantine.try_borrow().map_or(0, |q| q.blocks.len()))
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
/// whose quarantine must keep freed blocks out of use. One per thread, in
/// `tls.rs`.
#[derive(Debug)]
pub(crate) struct BlockCache {
    classes: [Vec<*mut u8>; CACHE_CLASSES],
}

impl BlockCache {
    /// An empty cache.
    pub(crate) const fn new() -> Self {
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

/// A cached free block for `layout`, if the current thread has one. With
/// the running fiber `ctx`, the cache is found through its control block,
/// with no thread-local lookup (D-686, GATE1-1).
fn cached_block(ctx: Option<&ExecCtx>, layout: Layout) -> Option<*mut u8> {
    let class = size_class(layout)?;
    tls::with_cache_of(ctx, |cache| cache.try_borrow_mut().ok()?.take(class)).flatten()
}

impl BlockCache {
    /// A cached free block of size class `class`, if there is one.
    fn take(&mut self, class: usize) -> Option<*mut u8> {
        let block = self.classes.get_mut(class)?.pop()?;
        // GAP(sanitize-block-cache): addressable again under the address
        // cfg (`keep` poisoned it); a no-op otherwise.
        if let Some(layout) = class_layout(class) {
            sanitizer::unpoison(block, layout.size());
        }
        Some(block)
    }

    /// Keeps a free block of `layout`; false when the cache is full or has
    /// no class for it.
    fn keep(&mut self, obj: *mut u8, layout: Layout) -> bool {
        let Some(class) = size_class(layout) else {
            return false;
        };
        match self.classes.get_mut(class) {
            Some(blocks) if blocks.len() < CACHE_BLOCKS => {
                // GAP(sanitize-block-cache): under the address cfg a use
                // after free into a cached block is reported; the
                // quarantine is not poisoned, since the runtime's own
                // checks read its poisoned headers.
                sanitizer::poison(obj, layout.size());
                blocks.push(obj);
                true
            }
            _ => false,
        }
    }
}

/// Returns the current thread's cached free blocks to the system allocator,
/// for tests of the cache.
#[cfg(test)]
pub(crate) fn release_block_cache() {
    tls::with_cache(|cache| {
        if let Ok(mut cache) = cache.try_borrow_mut() {
            cache.release();
        }
    });
}

/// The number of blocks in the current thread's cache, for tests.
#[cfg(test)]
fn cached_blocks() -> usize {
    tls::with_cache(|cache| {
        cache
            .try_borrow()
            .map_or(0, |cache| cache.classes.iter().map(Vec::len).sum())
    })
    .unwrap_or(0)
}

/// Returns a freed block to the block cache `cache` (the current thread's,
/// when the caller could borrow it) or the allocator, or poisons and
/// quarantines it.
///
/// # Safety
///
/// `obj` was allocated by `alloc_object` with `layout` and no reference to
/// it remains.
unsafe fn release_block(obj: *mut u8, layout: Layout, cache: Option<&mut BlockCache>) {
    if keeps_freed_blocks() {
        if poison_enabled() {
            // SAFETY: the block is still allocated and nothing else uses
            // it.
            unsafe { object::write_field(obj, 0, POISON_HEADER) };
            let kept = tls::with_quarantine(|quarantine| {
                quarantine
                    .try_borrow_mut()
                    .map(|mut quarantine| quarantine.keep(obj, layout))
                    .is_ok()
            })
            .unwrap_or(false);
            if kept {
                return;
            }
        } else if let Some(cache) = cache
            && cache.keep(obj, layout)
        {
            return;
        }
    }
    // SAFETY: forwarded from the caller.
    unsafe { std::alloc::dealloc(obj, layout) };
}

/// Whether freed blocks may be kept, in the quarantine or the block cache.
#[cfg(not(test))]
#[inline]
const fn keeps_freed_blocks() -> bool {
    true
}

/// Whether freed blocks may be kept: tests that measure what a thread asks
/// the system for turn it off on their thread.
#[cfg(test)]
fn keeps_freed_blocks() -> bool {
    temporaries_tests::keeps_freed()
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

pub(crate) mod scratch;

#[cfg(test)]
mod domain_tests;

#[cfg(test)]
mod finalize_tests;

#[cfg(test)]
mod out_of_memory_tests;

#[cfg(test)]
mod temporaries_tests;

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use super::*;
    use crate::abi::layout as ids;
    use crate::object::write_field;

    /// D-784's text: kinds in kind order with their debug text, the tail
    /// for the other objects with its grammar, no tail when every leaked
    /// object is of a runtime kind, and no line without a runtime kind.
    #[test]
    fn the_leaked_kinds_line_follows_d784() {
        let line = |kinds, live| leaked_kinds_line(kinds, live);
        if !cfg!(debug_assertions) {
            assert_eq!(line([1, 0, 0, 0, 0], 3), None);
            return;
        }
        assert_eq!(
            line([0, 1, 0, 0, 0], 2).as_deref(),
            Some("note: leaked 1 <Receiver>, and 1 object of other kinds")
        );
        assert_eq!(
            line([1, 0, 2, 0, 0], 17).as_deref(),
            Some("note: leaked 1 <Sender>, 2 <Fiber>, and 14 objects of other kinds")
        );
        assert_eq!(
            line([0, 0, 0, 3, 1], 4).as_deref(),
            Some("note: leaked 3 <Scope>, 1 <Selector>")
        );
        assert_eq!(line([0; RUNTIME_KINDS], 5), None);
        // A test's delta may be negative for a kind it freed.
        assert_eq!(line([-1, 0, 0, 0, 0], 2), None);
        assert_eq!(
            line([-1, 1, 0, 0, 0], 1).as_deref(),
            Some("note: leaked 1 <Receiver>")
        );
    }

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
        handle: false,
        finalize: None,
    };

    /// The id the test node registers at (unused by every runtime module;
    /// 40 to 44 are the handle kinds).
    pub(crate) const TEST_NODE_ID: abi::LayoutId = 45;

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

    /// D-738: the preflight of the list helpers reads the budget from the
    /// helper's `ctx` and computes its bound only under a budget; without
    /// one, or without a fiber, the bound is never computed.
    #[test]
    fn preflight_computes_its_bound_only_under_a_budget() {
        use crate::ctx::{ExecCtx, FiberInit};
        let computed = std::cell::Cell::new(0_u32);
        let bound = || {
            computed.set(computed.get() + 1);
            64
        };
        preflight(None, bound);
        let ctx = ExecCtx::new(FiberInit::placeholder());
        preflight(Some(&ctx), bound);
        assert_eq!(computed.get(), 0, "the bound was computed without a budget");
        // Under a budget with room, the bound is computed and passes.
        ctx.set_budget(u64::MAX - 1);
        preflight(Some(&ctx), bound);
        assert_eq!(computed.get(), 1);
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
            // Releasing the cache gives every block back to the system.
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
        unsafe { rc::cheby_rt_drop(std::ptr::null_mut(), word) };
        // SAFETY: deliberately not: each operation reaches a freed object,
        // which the poisoning mode must catch before touching it.
        unsafe {
            match operation.as_str() {
                "dup" => rc::cheby_rt_dup(word),
                "drop" => rc::cheby_rt_drop(std::ptr::null_mut(), word),
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
