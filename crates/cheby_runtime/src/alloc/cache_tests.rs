//! Tests of the block cache as generated code sees it (`abi::block_cache`):
//! its stacks, refills and give-backs, the inline allocation record of a
//! fiber across switches and migrations, inline allocations and frees as
//! the code generator writes them, and the stubs of the slow paths, which
//! must give back every register and leave an allocation's result in the
//! control block, `0` when it raised.
//!
//! This crate's tests poison freed blocks by default, which turns the
//! inline path off; `alloc::tests::the_block_cache_works_without_poisoning`
//! runs these tests again with poisoning off.

use std::ptr;
use std::sync::{Arc, Mutex, PoisonError};

use super::cache::{BlockCache, CACHE_BLOCKS, REFILL_BLOCKS, class_layout, never_serving_cache};
use super::{Domain, OUT_OF_MEMORY, keeps_freed_blocks, poison_enabled};
use crate::abi::{self, block_cache as cache_abi};
use crate::ctx::ExecCtx;
use crate::entry::{self, EntryConfig, EntryMode, EntryOutcome};
use crate::fiber::sanitizer;
use crate::sched::testing;
use crate::{rc, tls};

/// The size class of a tuple of two fields: 24 bytes.
const CLASS: usize = 2;

/// The header of a tuple of two fields with count one.
const PAIR: u64 = abi::header(abi::RC_UNIQUE, 2, abi::layout::TUPLE);

/// What fibers saw that a test rejects.
type Problems = Arc<Mutex<Vec<String>>>;

fn note(problems: &Problems, problem: String) {
    problems
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(problem);
}

fn assert_no_problems(problems: &Problems) {
    let problems = problems.lock().unwrap_or_else(PoisonError::into_inner);
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The value at `offset` bytes into `base`, read as generated code reads
/// it.
///
/// # Safety
///
/// A readable `T` lies there.
unsafe fn read<T>(base: *const u8, offset: i32) -> T {
    let offset = isize::try_from(offset).unwrap();
    // SAFETY: forwarded from the caller.
    unsafe { base.byte_offset(offset).cast::<T>().read() }
}

/// Writes `value` at `offset` bytes into `base`, as generated code
/// writes it.
///
/// # Safety
///
/// A writable `T` lies there, inside a `Cell` or a block.
unsafe fn store<T>(base: *const u8, offset: i32, value: T) {
    let offset = isize::try_from(offset).unwrap();
    // SAFETY: forwarded from the caller.
    unsafe { base.cast_mut().byte_offset(offset).cast::<T>().write(value) };
}

/// The byte offset of class `class`'s `top`.
fn top_of(class: usize) -> i32 {
    cache_abi::class_offset(class)
        .and_then(|entry| entry.checked_add(cache_abi::TOP_OFFSET))
        .unwrap()
}

/// The byte offset of class `class`'s `limit`.
fn limit_of(class: usize) -> i32 {
    cache_abi::class_offset(class)
        .and_then(|entry| entry.checked_add(cache_abi::LIMIT_OFFSET))
        .unwrap()
}

/// An inline pop from class `class` of the cache at `cache`, without the
/// charge: the stack part of an inline allocation.
///
/// # Safety
///
/// `cache` is a block cache that no other thread uses.
unsafe fn inline_pop(cache: *const u8, class: usize) -> Option<*mut u8> {
    // SAFETY: the cache's layout (`abi::block_cache`).
    let top: *mut *mut u8 = unsafe { read(cache, top_of(class)) };
    // SAFETY: `top` is a readable slot.
    let block = unsafe { top.read() };
    if block.is_null() {
        return None;
    }
    // SAFETY: as above.
    unsafe { store(cache, top_of(class), top.wrapping_sub(1)) };
    sanitizer::unpoison(block, class_layout(class).unwrap().size());
    Some(block)
}

/// Whether an inline push into class `class` has room.
///
/// # Safety
///
/// As for [`inline_pop`].
unsafe fn inline_room(cache: *const u8, class: usize) -> bool {
    // SAFETY: the cache's layout.
    let top: *mut *mut u8 = unsafe { read(cache, top_of(class)) };
    // SAFETY: as above.
    let limit: *mut *mut u8 = unsafe { read(cache, limit_of(class)) };
    top.addr() < limit.addr()
}

/// The inline allocation of the code generator (`alloc_inline`), step by
/// step: the object, or `None` where generated code takes the slow path.
///
/// # Safety
///
/// `cache` is the inline allocation record of the fiber running here.
unsafe fn inline_alloc(cache: *const u8, class: usize, header: u64) -> Option<*mut u8> {
    let bytes = i64::try_from(cache_abi::class_bytes(class)).unwrap();
    // SAFETY: the cache's layout.
    let top: *mut *mut u8 = unsafe { read(cache, top_of(class)) };
    // SAFETY: `top` is a readable slot.
    let block = unsafe { top.read() };
    if block.is_null() {
        return None;
    }
    // SAFETY: a serving cache records its fiber's shard.
    let shard: *const u8 = unsafe { read(cache, cache_abi::SHARD_OFFSET) };
    // SAFETY: a shard's layout.
    let live: i64 = unsafe { read(shard, cache_abi::SHARD_BYTES_OFFSET) };
    // SAFETY: as above.
    let high: i64 = unsafe { read(shard, cache_abi::SHARD_HIGH_OFFSET) };
    let after = live.checked_add(bytes).unwrap();
    if after > high {
        return None;
    }
    // SAFETY: as above.
    let objects: i64 = unsafe { read(shard, cache_abi::SHARD_OBJECTS_OFFSET) };
    // SAFETY: the shard's counters are atomics this thread charges alone.
    unsafe {
        store(
            shard,
            cache_abi::SHARD_OBJECTS_OFFSET,
            objects.checked_add(1).unwrap(),
        );
        store(shard, cache_abi::SHARD_BYTES_OFFSET, after);
        store(cache, top_of(class), top.wrapping_sub(1));
        crate::object::write_field(block, 0, header);
    }
    Some(block)
}

/// The inline sized free of the code generator, step by step: false where
/// generated code takes the slow path.
///
/// # Safety
///
/// As for [`inline_alloc`]; `block` is a unique object of class `class`
/// whose fields own nothing.
unsafe fn inline_free(cache: *const u8, class: usize, block: *mut u8) -> bool {
    let bytes = i64::try_from(cache_abi::class_bytes(class)).unwrap();
    // SAFETY: forwarded from the caller.
    if !unsafe { inline_room(cache, class) } {
        return false;
    }
    // SAFETY: the cache's layout.
    let top: *mut *mut u8 = unsafe { read(cache, top_of(class)) };
    // SAFETY: a serving cache records its fiber's shard.
    let shard: *const u8 = unsafe { read(cache, cache_abi::SHARD_OFFSET) };
    // SAFETY: a shard's layout; the slot after `top` is below the limit.
    unsafe {
        let objects: i64 = read(shard, cache_abi::SHARD_OBJECTS_OFFSET);
        store(
            shard,
            cache_abi::SHARD_OBJECTS_OFFSET,
            objects.checked_sub(1).unwrap(),
        );
        let live: i64 = read(shard, cache_abi::SHARD_BYTES_OFFSET);
        store(
            shard,
            cache_abi::SHARD_BYTES_OFFSET,
            live.checked_sub(bytes).unwrap(),
        );
        let above = top.wrapping_add(1);
        above.write(block);
        store(cache, top_of(class), above);
    }
    true
}

/// Whether an empty class takes a batch from the system here.
fn refills() -> bool {
    !cfg!(cheby_sanitize = "address") && !poison_enabled() && keeps_freed_blocks()
}

/// A class's stack: absent at first, so inline code finds nothing and no
/// room; a refill serves a block and keeps the rest of its batch; it keeps
/// blocks up to its bound, and a full stack gives a batch back to the
/// system before it keeps another; pops are last in, first out; a release
/// empties it. Inline pushes have room only without debug assertions.
#[test]
fn a_class_stack_refills_keeps_and_gives_back() {
    let cache = BlockCache::new();
    let base = ptr::from_ref(&cache).cast::<u8>();
    let layout = class_layout(CLASS).unwrap();
    // SAFETY: `cache` is this thread's alone.
    unsafe {
        assert_eq!(inline_pop(base, CLASS), None);
        assert!(!inline_room(base, CLASS));
    }
    assert_eq!(cache.take(CLASS), None);
    let first = cache.refill(CLASS).unwrap();
    let batch = if refills() { REFILL_BLOCKS - 1 } else { 0 };
    assert_eq!(cache.len(), batch);
    // SAFETY: as above.
    let limit: *mut *mut u8 = unsafe { read(base, limit_of(CLASS)) };
    assert_eq!(limit.is_null(), cfg!(debug_assertions), "{limit:?}");
    assert!(cache.keep_sized(first, layout.size()));
    while cache.len() < CACHE_BLOCKS {
        // SAFETY: a nonzero size.
        let block = unsafe { std::alloc::alloc(layout) };
        assert!(!block.is_null());
        assert!(cache.keep_sized(block, layout.size()));
    }
    // SAFETY: as above.
    assert!(!unsafe { inline_room(base, CLASS) });
    // SAFETY: a nonzero size.
    let extra = unsafe { std::alloc::alloc(layout) };
    assert!(cache.keep_sized(extra, layout.size()));
    assert_eq!(cache.len(), CACHE_BLOCKS - REFILL_BLOCKS + 1);
    assert_eq!(cache.take(CLASS), Some(extra));
    // SAFETY: as above.
    let popped = unsafe { inline_pop(base, CLASS) }.unwrap();
    assert_eq!(cache.len(), CACHE_BLOCKS - REFILL_BLOCKS - 1);
    if !cfg!(debug_assertions) {
        // SAFETY: as above.
        assert!(unsafe { inline_room(base, CLASS) });
    }
    for block in [extra, popped] {
        // SAFETY: allocated with this layout, not cached.
        unsafe { std::alloc::dealloc(block, layout) };
    }
    cache.release();
    assert_eq!(cache.len(), 0);
    // SAFETY: as above.
    assert_eq!(unsafe { inline_pop(base, CLASS) }, None);
    // Another class's stack is still absent.
    // SAFETY: as above.
    assert!(!unsafe { inline_room(base, CLASS + 1) });
}

/// The cache that never serves has no block and no room in any class.
#[test]
fn the_cache_that_never_serves_has_nothing() {
    let base = never_serving_cache().cast::<u8>();
    for class in 0..cache_abi::CLASSES {
        // SAFETY: a static cache that is never written; the pops find 0
        // before they would store.
        unsafe {
            assert_eq!(inline_pop(base, class), None);
            assert!(!inline_room(base, class));
        }
    }
}

/// Whether the inline path may serve `ctx`, by `inline_record`'s rule.
fn may_serve(ctx: &ExecCtx) -> bool {
    !cfg!(cheby_sanitize = "address")
        && !poison_enabled()
        && ctx.budget() == u64::MAX
        && ctx.charge_slot() & 1 != 0
        && !crate::stats::counting()
        && keeps_freed_blocks()
}

/// Checks `ctx`'s record on the thread it runs on: its thread's cache with
/// its shard when it may be served, the cache that never serves otherwise.
fn check_record(ctx: &ExecCtx, problems: &Problems, context: &str) {
    let here = tls::with_cache(ptr::from_ref).unwrap();
    if ctx.block_cache() != here {
        note(
            problems,
            format!("{context}: the fiber holds another thread's cache"),
        );
    }
    let record = ctx.alloc_cache();
    if may_serve(ctx) {
        if record != here {
            note(
                problems,
                format!("{context}: the record is not this thread's cache"),
            );
        }
        // SAFETY: this thread's cache.
        let shard: usize =
            unsafe { read::<*const u8>(here.cast(), cache_abi::SHARD_OFFSET) }.addr();
        if shard != ctx.charge_slot() & !1 {
            note(
                problems,
                format!("{context}: the cache records another shard"),
            );
        }
    } else if record != never_serving_cache() {
        note(
            problems,
            format!("{context}: a fiber the path may not serve has a cache"),
        );
    }
}

/// D-686, D-739: a fiber's inline allocation record is its current
/// thread's cache after every switch, on either worker; a budget turns it
/// off at once, and lifting the budget turns it on at the next switch.
#[test]
#[cfg_attr(miri, ignore = "runs a runtime")]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn the_record_follows_the_fiber_across_switches() {
    let problems = Problems::default();
    let seen = Arc::clone(&problems);
    let runtime = testing::runtime(2);
    let outcome = testing::start_root_with(&runtime, &testing::config(1 << 20), move |ctx| {
        let me = ptr::from_ref(ctx).cast_mut();
        check_record(ctx, &seen, "at the start");
        for round in 0..20 {
            crate::ctx::request(ctx, crate::ctx::reason::YIELD);
            // SAFETY: the running fiber's context and no location.
            unsafe { crate::panic::cheby_rt_stack_slow(me, 0) };
            check_record(ctx, &seen, &format!("after switch {round}"));
        }
        ctx.set_budget(1 << 30);
        if ctx.alloc_cache() != never_serving_cache() {
            note(&seen, "a budget left the inline path on".to_owned());
        }
        ctx.set_budget(u64::MAX);
        if ctx.alloc_cache() != never_serving_cache() {
            note(
                &seen,
                "lifting the budget turned the path on before a switch".to_owned(),
            );
        }
        crate::ctx::request(ctx, crate::ctx::reason::YIELD);
        // SAFETY: as above.
        unsafe { crate::panic::cheby_rt_stack_slow(me, 0) };
        check_record(ctx, &seen, "after the budget");
        7
    })
    .recv_timeout(testing::hang())
    .expect("the root fiber hung");
    runtime.shutdown();
    assert_eq!(outcome, EntryOutcome::Returned(7));
    assert_no_problems(&problems);
}

/// Inline allocations and frees, as the code generator writes them, charge
/// the domain as the runtime's own paths do; where the record does not
/// serve, the inline code finds nothing to take and no room.
#[test]
#[cfg_attr(miri, ignore = "runs a runtime")]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn inline_allocations_charge_as_the_runtime_does() {
    let problems = Problems::default();
    let seen = Arc::clone(&problems);
    let runtime = testing::runtime(1);
    let domain = Domain::new();
    let config = EntryConfig {
        stack_size: 1 << 20,
        domain: Arc::clone(&domain),
        ..EntryConfig::new(EntryMode::Test)
    };
    let counted = Arc::clone(&domain);
    let outcome = testing::start_root_with(&runtime, &config, move |ctx| {
        let record = ctx.alloc_cache().cast::<u8>();
        if !may_serve(ctx) {
            // SAFETY: the cache that never serves.
            unsafe {
                if inline_alloc(record, CLASS, PAIR).is_some() || inline_room(record, CLASS) {
                    note(&seen, "the cache that never serves served".to_owned());
                }
            }
            return 7;
        }
        // Two tuples through the runtime raise the high, then go back to
        // the cache.
        let one = abi::tag_int(1).unwrap();
        let first = super::tests::tuple(&[one, one]);
        let second = super::tests::tuple(&[one, one]);
        for word in [first, second] {
            // SAFETY: the only reference.
            unsafe { rc::drop(word) };
        }
        let (objects, bytes) = (counted.live_objects(), counted.live_bytes());
        // SAFETY: the record of the fiber running here.
        let Some(block) = (unsafe { inline_alloc(record, CLASS, PAIR) }) else {
            note(&seen, "the inline allocation found no block".to_owned());
            return 7;
        };
        // SAFETY: a fresh block of 24 bytes.
        unsafe {
            crate::object::write_field(block, 8, one);
            crate::object::write_field(block, 16, one);
        }
        let charged = (
            counted.live_objects() - objects,
            counted.live_bytes() - bytes,
        );
        if charged != (1, 24) {
            note(&seen, format!("an inline allocation charged {charged:?}"));
        }
        // SAFETY: a unique tuple whose fields own nothing.
        let freed = unsafe { inline_free(record, CLASS, block) };
        if freed == cfg!(debug_assertions) {
            note(&seen, format!("an inline free had room: {freed}"));
        }
        if !freed {
            let me = ptr::from_ref(ctx).cast_mut();
            // SAFETY: as above.
            unsafe {
                super::slow::cheby_rt_free_unique_slow(me, crate::object::ptr_to_word(block), 24);
            };
        }
        let left = (
            counted.live_objects() - objects,
            counted.live_bytes() - bytes,
        );
        if left != (0, 0) {
            note(&seen, format!("an inline free left {left:?}"));
        }
        7
    })
    .recv_timeout(testing::hang())
    .expect("the root fiber hung");
    runtime.shutdown();
    assert_eq!(outcome, EntryOutcome::Returned(7));
    assert_eq!(domain.live_objects(), 0, "objects leaked");
    assert_no_problems(&problems);
}

/// D-642, D-697: an allocation the system refuses on the slow path leaves
/// 0 and the flag set, and the entry panics with `out of memory`.
#[test]
#[cfg_attr(miri, ignore = "runs an entry thread")]
fn the_slow_path_leaves_0_when_it_raises() {
    let _guard = super::tests::serial();
    let config = EntryConfig {
        domain: Domain::new(),
        ..EntryConfig::new(EntryMode::Test)
    };
    let left = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&left);
    let task = move || {
        let ctx = entry::current_ctx().unwrap();
        super::refuse_next_fast_allocation();
        super::refuse_next_system_allocation();
        // SAFETY: the running entry's context; a tuple of two fields.
        unsafe { super::slow::cheby_rt_alloc_slow(ctx, 24, PAIR) };
        // SAFETY: as above.
        let fiber = unsafe { &*ctx };
        *seen.lock().unwrap() = Some((fiber.alloc_result(), crate::unwind::flag::get(fiber)));
        0
    };
    // SAFETY: the task builds and reads only its own runtime objects.
    let outcome = unsafe { entry::run_task(&config, task) }.unwrap();
    assert_eq!(*left.lock().unwrap(), Some((0, abi::unwind_flag::PANIC)));
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("expected a panic, got {outcome:?}");
    };
    assert_eq!(report.message, OUT_OF_MEMORY);
}

/// The slow paths' stubs give back every register they save, the vector
/// registers whole: generated code calls them with `preserve_all`.
/// The allocation's stub leaves the object in the control block, and the
/// free's, the drop's and the reuse count's leave the domain as it was.
#[test]
#[cfg(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    not(miri),
    not(windows)
))]
fn the_slow_path_stubs_preserve_every_register() {
    use crate::abi::Word;
    use crate::unwind::entry_slow::tests::{Regs, call_saving};

    #[expect(clippy::as_conversions, reason = "functions' addresses")]
    fn stubs() -> [usize; 4] {
        let alloc: unsafe extern "C" fn(*mut ExecCtx, u64, u64) = super::slow::cheby_rt_alloc_slow;
        let miss: unsafe extern "C" fn(*mut ExecCtx, u64, u64) =
            super::slow::cheby_rt_alloc_miss_slow;
        let free: unsafe extern "C" fn(*mut ExecCtx, Word, u64) =
            super::slow::cheby_rt_free_unique_slow;
        let drop: unsafe extern "C" fn(*mut ExecCtx, Word) = super::slow::cheby_rt_drop_saving;
        [
            (alloc as *const ()).addr(),
            (miss as *const ()).addr(),
            (free as *const ()).addr(),
            (drop as *const ()).addr(),
        ]
    }

    let problems = Problems::default();
    let seen = Arc::clone(&problems);
    let runtime = testing::runtime(1);
    let domain = Domain::new();
    let config = EntryConfig {
        stack_size: 1 << 20,
        domain: Arc::clone(&domain),
        ..EntryConfig::new(EntryMode::Test)
    };
    let outcome = testing::start_root_with(&runtime, &config, move |ctx| {
        let [alloc, miss, free, drop] = stubs();
        let address = u64::try_from(ptr::from_ref(ctx).expose_provenance()).unwrap();
        let input = |x0: u64, x1: u64, x2: u64| {
            let mut regs = Regs::patterned();
            regs.set_argument(0, x0);
            regs.set_argument(1, x1);
            regs.set_argument(2, x2);
            regs
        };
        let call = |stub: usize, regs: &Regs, what: &str| {
            // SAFETY: a stub with arguments it accepts.
            let output = unsafe { call_saving(stub, regs) };
            if output != *regs {
                note(&seen, format!("{what}: {regs:x?} came back as {output:x?}"));
            }
        };
        for (stub, what) in [(alloc, "alloc"), (miss, "alloc miss")] {
            ctx.set_alloc_result(0);
            call(stub, &input(address, 24, PAIR), what);
            let object = ctx.alloc_result();
            if object == 0 {
                note(&seen, format!("{what}: no result"));
                continue;
            }
            let one = abi::tag_int(1).unwrap();
            // SAFETY: a fresh tuple of two fields.
            unsafe {
                let block = crate::object::word_to_ptr(object);
                if crate::object::read_field(block, 0) != PAIR {
                    note(&seen, format!("{what}: another header"));
                }
                crate::object::write_field(block, 8, one);
                crate::object::write_field(block, 16, one);
            }
            let stub = if what == "alloc" { free } else { drop };
            call(stub, &input(address, object, 24), "free");
        }
        call(
            drop,
            &input(address, abi::tag_int(5).unwrap(), 0),
            "drop of an Int",
        );
        7
    })
    .recv_timeout(testing::hang())
    .expect("the root fiber hung");
    runtime.shutdown();
    assert_eq!(outcome, EntryOutcome::Returned(7));
    assert_eq!(domain.live_objects(), 0, "objects leaked");
    assert_no_problems(&problems);
}
