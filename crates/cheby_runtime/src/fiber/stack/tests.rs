//! Tests of the stack pool: classes, reuse, release, quarantine,
//! reservations and guard pages (D-677, D-678).

#![expect(
    clippy::arithmetic_side_effects,
    reason = "test arithmetic on addresses inside mapped slots"
)]

use std::ptr::{read_volatile, with_exposed_provenance_mut, write_volatile};

use super::*;

/// The environment variable that makes a test run its child half.
const CHILD: &str = "CHEBY_RT_TEST_STACK_CHILD";

/// Writes a byte at `address`.
fn touch(address: usize) {
    // SAFETY: the tests pass addresses inside slots they own.
    unsafe { write_volatile(with_exposed_provenance_mut::<u8>(address), 1) };
}

/// Reads the byte at `address`.
fn peek(address: usize) -> u8 {
    // SAFETY: the tests pass addresses inside mapped regions.
    unsafe { read_volatile(with_exposed_provenance_mut::<u8>(address)) }
}

/// Writes a byte in every page of `[low, high)`.
fn touch_pages(low: usize, high: usize) {
    let page = os::page_size();
    let mut at = low;
    while at < high {
        touch(at);
        at += page;
    }
}

/// Whether each page of `[low, high)` is resident, from `mincore`.
#[cfg(unix)]
fn resident(low: usize, high: usize) -> Vec<bool> {
    let page = os::page_size();
    let len = high - low;
    let mut pages = vec![0; len.div_ceil(page)];
    // SAFETY: `[low, high)` is page-aligned and mapped, and `pages` has
    // one byte per page.
    let status =
        unsafe { libc::mincore(with_exposed_provenance_mut(low), len, pages.as_mut_ptr()) };
    assert_eq!(status, 0, "{}", std::io::Error::last_os_error());
    pages.iter().map(|&flags| flags & 1 != 0).collect()
}

#[cfg(not(unix))]
fn resident(_low: usize, _high: usize) -> Vec<bool> {
    panic!("no regions on this platform")
}

#[test]
fn classes_round_up_to_powers_of_two() {
    assert_eq!(class_size(0), Some(512 << 10));
    assert_eq!(class_size(1), Some(512 << 10));
    assert_eq!(class_size(512 << 10), Some(512 << 10));
    assert_eq!(class_size((512 << 10) + 1), Some(1 << 20));
    assert_eq!(class_size(3 << 20), Some(4 << 20));
    assert_eq!(class_size(8 << 20), Some(8 << 20));
    assert_eq!(class_size(64 << 20), Some(64 << 20));
    assert_eq!(class_size(MAX_STACK), Some(16 << 30));
    assert_eq!(class_size(MAX_STACK + 1), None);
    assert_eq!(class_size(usize::MAX), None);
    assert_eq!(class_index(MIN_STACK), Some(0));
    assert_eq!(class_index(MAX_STACK), Some(CLASSES - 1));
    assert_eq!(class_index(3 << 20), None);
    assert_eq!(class_index(256 << 10), None);
}

#[test]
fn user_sizes_are_clamped() {
    assert_eq!(clamp_user_size(0), 1 << 20);
    assert_eq!(clamp_user_size(5 << 20), 5 << 20);
    assert_eq!(clamp_user_size(1 << 40), 16 << 30);
    assert_eq!(clamp_user_size(u64::MAX), 16 << 30);
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn take_rounds_up_and_reuses_lifo() {
    let pool = StackPool::new(false);
    let a = pool.take(3 << 20).unwrap();
    assert_eq!(a.size(), 4 << 20);
    assert_eq!(a.top() - a.bottom(), 4 << 20);
    assert_eq!(a.bottom() % os::page_size(), 0);
    assert_eq!(a.hard_limit(), a.bottom() + (256 << 10));
    assert!(!a.guarded());
    let b = pool.take(4 << 20).unwrap();
    assert_ne!(a.bottom(), b.bottom());
    let (a_bottom, b_bottom) = (a.bottom(), b.bottom());
    pool.give_back(a, usize::MAX);
    pool.give_back(b, usize::MAX);
    // Last in, first out.
    assert_eq!(pool.take(4 << 20).unwrap().bottom(), b_bottom);
    assert_eq!(pool.take(4 << 20).unwrap().bottom(), a_bottom);
    let counts = pool.counts(4 << 20).unwrap();
    assert_eq!(counts.regions, 1);
    assert_eq!(counts.carved, 2);
    assert_eq!(counts.free, 0);
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn regions_hold_at_least_a_gibibyte_and_eight_slots() {
    let pool = StackPool::new(false);
    // 256 MiB slots: 8 to a region of 2 GiB, so the ninth maps a second.
    let slots: Vec<_> = (0..9).map(|_| pool.take(256 << 20).unwrap()).collect();
    assert_eq!(pool.counts(256 << 20).unwrap().regions, 2);
    // Slots of one region are adjacent, above the region's guard page.
    for pair in slots[..8].windows(2) {
        assert_eq!(pair[1].bottom(), pair[0].top());
    }
    // 512 KiB slots: 2048 to a region of 1 GiB.
    let small: Vec<_> = (0..2049).map(|_| pool.take(1).unwrap()).collect();
    let counts = pool.counts(1).unwrap();
    assert_eq!((counts.regions, counts.carved), (2, 2049));
    assert_eq!(small[2047].top(), small[0].bottom() + (1 << 30));
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn the_largest_class_maps_and_larger_fails() {
    let pool = StackPool::new(false);
    let slot = pool.take(MAX_STACK).unwrap();
    assert_eq!(slot.size(), 16 << 30);
    // Only touched pages cost memory.
    touch(slot.top() - 1);
    touch(slot.usable_bottom());
    assert_eq!(
        pool.take(MAX_STACK + 1).unwrap_err(),
        StackError::TooLarge {
            requested: MAX_STACK + 1
        }
    );
    pool.give_back(slot, usize::MAX);
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_deep_watermark_releases_the_pages_below_the_warm_top() {
    let pool = StackPool::new(false);
    let slot = pool.take(8 << 20).unwrap();
    let (bottom, low, top) = (slot.bottom(), slot.usable_bottom(), slot.top());
    touch_pages(low, top);
    assert!(resident(low, top).iter().all(|&page| page));
    let warm = slot.warm_top(os::page_size());
    pool.give_back(slot, low);
    let below = resident(low, warm);
    assert!(below.iter().all(|&page| !page), "{below:?}");
    let above = resident(warm, top);
    assert!(above.iter().all(|&page| page), "{above:?}");
    // The region stays mapped, and released pages read as zero (I-8).
    assert_eq!(peek(low), 0);
    assert_eq!(peek(warm), 1);
    assert_eq!(pool.take(8 << 20).unwrap().bottom(), bottom);
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_shallow_watermark_keeps_the_pages() {
    let pool = StackPool::new(false);
    let slot = pool.take(8 << 20).unwrap();
    let top = slot.top();
    let used = top - (64 << 10);
    touch_pages(used, top);
    pool.give_back(slot, used);
    assert!(resident(used, top).iter().all(|&page| page));
    assert_eq!(peek(used), 1);
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_crowded_class_releases_whole_slots() {
    let pool = StackPool::new(false);
    let slots: Vec<_> = (0..FREE_KEEP + 2).map(|_| pool.take(1).unwrap()).collect();
    let tops: Vec<usize> = slots.iter().map(StackSlot::top).collect();
    let page = os::page_size();
    for &top in &tops {
        touch(top - 1);
    }
    for slot in slots {
        pool.give_back(slot, usize::MAX);
    }
    for (index, &top) in tops.iter().enumerate() {
        let kept = resident(top - page, top)[0];
        assert_eq!(kept, index < FREE_KEEP, "slot {index}");
    }
    assert_eq!(pool.counts(1).unwrap().free, FREE_KEEP + 2);
    {
        let state = pool.classes[0].lock();
        assert_eq!((state.warm.len(), state.cold.len()), (FREE_KEEP, 2));
    }
    // The last warm slot comes back first, then the other warm ones, and
    // the slots released whole only after them.
    assert_eq!(pool.take(1).unwrap().top(), tops[FREE_KEEP - 1]);
}

/// D-749, D-761 (gate 3's MEM-W3-1 and ROB-W3-2):
/// after a burst of more fibers than [`FREE_KEEP`] that each wrote their
/// whole warm part, the class keeps at most [`FREE_KEEP`] x [`WARM_KEEP`]
/// written bytes in its free slots, D-761's 24 MiB; the rest are released
/// whole.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_burst_keeps_at_most_the_warm_budget_resident() {
    const BUDGET: usize = 24 << 20;
    const { assert!(FREE_KEEP * WARM_KEEP <= BUDGET) };
    let pool = StackPool::new(false);
    let page = os::page_size();
    let slots: Vec<_> = (0..FREE_KEEP + 16).map(|_| pool.take(1).unwrap()).collect();
    let warm: Vec<(usize, usize)> = slots
        .iter()
        .map(|slot| (slot.warm_top(page), slot.top()))
        .collect();
    for &(low, high) in &warm {
        touch_pages(low, high);
    }
    // Given back as fibers that never lowered their first soft limit:
    // nothing below the warm top to release, the warm part kept.
    for slot in slots {
        pool.give_back(slot, usize::MAX);
    }
    let kept: usize = warm
        .iter()
        .map(|&(low, high)| written_offsets(low, high).len() * page)
        .sum();
    assert!(kept <= FREE_KEEP * WARM_KEEP, "{kept} bytes kept");
    assert!(kept <= BUDGET, "{kept} bytes kept");
}

/// The offsets from `low` of the pages of `[low, high)`, page-aligned,
/// whose first byte is not zero: written by [`touch`] and not released.
/// Contents, not `mincore`: with Linux's transparent huge pages a written
/// page makes the pages around it resident, though they read as zero.
fn written_offsets(low: usize, high: usize) -> Vec<usize> {
    let page = os::page_size();
    (low..high)
        .step_by(page)
        .filter(|&at| peek(at) != 0)
        .map(|at| at - low)
        .collect()
}

/// A fiber writes below its soft limit without lowering it: a checked
/// frame of up to 64 KiB and the runtime and foreign calls made from it
/// may use D-457's headroom below the limit. So whatever the watermark
/// (the fiber's lowest soft limit), a slot given back holds no written
/// page below its warm top, and one whose fiber never lowered its first
/// soft limit keeps all its pages.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pages_written_below_the_soft_limit_never_stay_below_the_warm_top() {
    let pool = StackPool::new(false);
    let page = os::page_size();
    for lowered in [0, page, 1 << 20] {
        let slot = pool.take(8 << 20).unwrap();
        let (bottom, low, top) = (slot.bottom(), slot.usable_bottom(), slot.top());
        let watermark = top - WARM_BYTES - lowered;
        let written = page_floor(watermark - STACK_HEADROOM, page).max(low);
        touch_pages(written, top);
        let warm = slot.warm_top(page);
        assert_eq!(warm, top - WARM_KEEP);
        pool.give_back(slot, watermark);
        let below = written_offsets(low, warm);
        assert!(below.is_empty(), "lowered by {lowered}: {below:?}");
        if lowered == 0 {
            // Nothing below the warm top was written, so nothing was
            // released: every written page is still there.
            assert!(resident(written, top).iter().all(|&page| page));
            assert_eq!(written_offsets(written, top).len(), (top - written) / page);
        }
        // The pages above the warm top stay warm.
        assert!(resident(warm, top).iter().all(|&page| page), "{lowered}");
        // 48 KiB below the first soft limit: released or warm, never a
        // dirty page below the warm top.
        let probe = page_floor(top - WARM_BYTES - (48 << 10), page);
        assert!(probe >= warm || peek(probe) == 0, "lowered by {lowered}");
        assert_eq!(pool.take(8 << 20).unwrap().bottom(), bottom);
    }
}

/// After more slots than [`FREE_KEEP`] were given back at once, reuse
/// still hands out warm slots: the slots released whole do not shadow the
/// warm ones.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_burst_does_not_make_reuse_cold() {
    let pool = StackPool::new(false);
    let page = os::page_size();
    let slots: Vec<_> = (0..FREE_KEEP + 8).map(|_| pool.take(1).unwrap()).collect();
    for slot in &slots {
        touch(slot.top() - 1);
    }
    for slot in slots {
        pool.give_back(slot, usize::MAX);
    }
    for round in 0..100 {
        let slot = pool.take(1).unwrap();
        let top = slot.top();
        assert!(
            resident(top - page, top)[0],
            "round {round}: a released slot was handed out before a warm one"
        );
        touch(top - 1);
        pool.give_back(slot, usize::MAX);
    }
    assert_eq!(pool.counts(1).unwrap().free, FREE_KEEP + 8);
    let state = pool.classes[0].lock();
    assert_eq!((state.warm.len(), state.cold.len()), (FREE_KEEP, 8));
}

/// Slots given back from many threads at once keep at most [`FREE_KEEP`]
/// of them warm.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn concurrent_give_backs_keep_at_most_free_keep_warm() {
    const THREADS: usize = 8;
    let pool = StackPool::new(false);
    let page = os::page_size();
    let mut slots: Vec<_> = (0..4 * FREE_KEEP).map(|_| pool.take(1).unwrap()).collect();
    let tops: Vec<usize> = slots.iter().map(StackSlot::top).collect();
    for &top in &tops {
        touch(top - 1);
    }
    let start = std::sync::Barrier::new(THREADS);
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            let mine = slots.split_off(slots.len() - 4 * FREE_KEEP / THREADS);
            let (pool, start) = (&pool, &start);
            scope.spawn(move || {
                start.wait();
                for slot in mine {
                    pool.give_back(slot, usize::MAX);
                }
            });
        }
    });
    let warm = tops
        .iter()
        .filter(|&&top| resident(top - page, top)[0])
        .count();
    assert_eq!(warm, FREE_KEEP);
    assert_eq!(pool.counts(1).unwrap().free, 4 * FREE_KEEP);
    let state = pool.classes[0].lock();
    assert_eq!(
        (state.warm.len(), state.cold.len(), state.releasing),
        (FREE_KEEP, 3 * FREE_KEEP, 0)
    );
}

/// A deep slot given back while the class has room keeps its top pages
/// and counts as warm, also while its deep pages are being released.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deep_slots_given_back_from_many_threads_stay_within_the_bound() {
    const THREADS: usize = 8;
    let pool = StackPool::new(false);
    let mut slots: Vec<_> = (0..2 * FREE_KEEP).map(|_| pool.take(1).unwrap()).collect();
    // The lowest and the highest page of each: the watermark below makes
    // every slot deep, and writing every page would make every slot
    // resident for nothing.
    for slot in &slots {
        touch(slot.usable_bottom());
        touch(slot.top() - 1);
    }
    let start = std::sync::Barrier::new(THREADS);
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            let mine = slots.split_off(slots.len() - 2 * FREE_KEEP / THREADS);
            let (pool, start) = (&pool, &start);
            scope.spawn(move || {
                start.wait();
                for slot in mine {
                    let bottom = slot.bottom();
                    pool.give_back(slot, bottom);
                }
            });
        }
    });
    let state = pool.classes[0].lock();
    assert_eq!(
        (state.warm.len(), state.cold.len(), state.releasing),
        (FREE_KEEP, FREE_KEEP, 0)
    );
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn quarantined_slots_are_never_reused() {
    let pool = StackPool::new(false);
    let slot = pool.take(1 << 20).unwrap();
    let bottom = slot.bottom();
    let top = slot.top();
    let last_page = top - os::page_size();
    touch_pages(top - (64 << 10), top);
    // SAFETY: nothing lives in the slot.
    unsafe { slot.release(top - (64 << 10), top - (32 << 10)) };
    assert_eq!(peek(top - (64 << 10)), 0);
    assert_eq!(peek(last_page), 1);
    pool.quarantine(slot);
    let next = pool.take(1 << 20).unwrap();
    assert_ne!(next.bottom(), bottom);
    let counts = pool.counts(1 << 20).unwrap();
    assert_eq!((counts.carved, counts.free, counts.quarantined), (2, 0, 1));
    // A quarantined slot keeps its pages.
    assert_eq!(peek(last_page), 1);
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn the_control_block_is_reserved_at_the_top() {
    let pool = StackPool::new(false);
    let mut slot = pool.take(8 << 20).unwrap();
    let top = slot.top();
    assert_eq!(slot.stack_top(), top);
    let block = slot.reserve_top(200, 64).unwrap().addr().get();
    assert_eq!(block % 64, 0);
    assert!(block + 200 <= top && block >= top - 256);
    assert_eq!(slot.stack_top(), block & !15);
    assert!(slot.contains(block) && slot.contains(slot.stack_top()));
    // A fresh slot is zeroed.
    assert_eq!(peek(block), 0);
    let second = slot.reserve_top(8, 8).unwrap().addr().get();
    assert_eq!(second, block - 8);
    assert_eq!(
        slot.reserve_top(MAX_RESERVE, 16).unwrap_err(),
        StackError::BadReserve {
            bytes: MAX_RESERVE,
            align: 16
        }
    );
    assert!(slot.reserve_top(8, 3).is_err());
    pool.give_back(slot, usize::MAX);
    // The reservation ends with the slot.
    assert_eq!(pool.take(8 << 20).unwrap().stack_top(), top);
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn the_global_pool_follows_the_build() {
    let pool = StackPool::global();
    let slot = pool.take(8 << 20).unwrap();
    if std::env::var_os("CHEBY_RT_STACK_GUARD").is_none() {
        assert_eq!(slot.guarded(), cfg!(debug_assertions));
    }
    assert_eq!(slot.guarded(), pool.guard_pages());
    pool.give_back(slot, 0);
}

/// D-824: each way a slot is handed out or given back
/// is counted once, when the runtime counts (D-768).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn the_slot_counters_count_each_path() {
    crate::stats::count_events();
    let pool = StackPool::new(false);
    assert_eq!(pool.slot_counts(), SlotCounts::default());
    // FREE_KEEP + 1 fresh slots; given back shallow, all but the last stay
    // warm and the last is released whole.
    let slots: Vec<_> = (0..=FREE_KEEP).map(|_| pool.take(1).unwrap()).collect();
    for slot in slots {
        pool.give_back(slot, usize::MAX);
    }
    // Taken again: the warm ones first, then the one released whole.
    let again: Vec<_> = (0..=FREE_KEEP).map(|_| pool.take(1).unwrap()).collect();
    // A deep slot given back while the class has room.
    let deep = pool.take(1).unwrap();
    let low = deep.usable_bottom();
    pool.give_back(deep, low);
    let n = u64::try_from(FREE_KEEP).unwrap();
    assert_eq!(
        pool.slot_counts(),
        SlotCounts {
            taken_warm: n,
            taken_cold: 1,
            carved: n + 2,
            released_whole: 1,
            deep_released: 1,
        }
    );
    drop(again);
}

/// D-825: a cache serves its own class first, the most
/// recently cached slot first, with its reservations ended; another class
/// comes from the pool.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn slot_cache_serves_its_class_most_recent_first() {
    let pool = StackPool::new(false);
    let mut cache = SlotCache::new();
    assert_eq!(cache.len(), 0);
    let mut a = pool.take(1 << 20).unwrap();
    let b = pool.take(1 << 20).unwrap();
    let small = pool.take(1).unwrap();
    let (a_bottom, b_bottom, small_bottom) = (a.bottom(), b.bottom(), small.bottom());
    let top = a.top();
    a.reserve_top(200, 64).unwrap();
    pool.give_back_cached(&mut cache, a, usize::MAX);
    pool.give_back_cached(&mut cache, small, usize::MAX);
    pool.give_back_cached(&mut cache, b, usize::MAX);
    assert_eq!(cache.len(), 3);
    // Nothing went to the pool's free lists.
    assert_eq!(pool.counts(1 << 20).unwrap().free, 0);
    assert_eq!(pool.counts(1).unwrap().free, 0);
    let first = pool.take_cached(&mut cache, 1 << 20).unwrap();
    assert_eq!(first.bottom(), b_bottom);
    let second = pool.take_cached(&mut cache, (1 << 20) - 1).unwrap();
    assert_eq!(second.bottom(), a_bottom);
    assert_eq!(second.stack_top(), top, "the reservation ended");
    assert_eq!(cache.len(), 1);
    // No slot of this class is cached: a fresh one from the pool.
    let third = pool.take_cached(&mut cache, 1 << 20).unwrap();
    assert!(![a_bottom, b_bottom].contains(&third.bottom()));
    assert_eq!(pool.counts(1 << 20).unwrap().carved, 3);
    assert_eq!(
        pool.take_cached(&mut cache, 1).unwrap().bottom(),
        small_bottom
    );
    assert_eq!(cache.len(), 0);
    assert_eq!(
        pool.take_cached(&mut cache, MAX_STACK + 1).unwrap_err(),
        StackError::TooLarge {
            requested: MAX_STACK + 1
        }
    );
}

/// D-825, D-749: a deep slot entering the cache has its
/// pages below the warm top released first; a full cache gives the slot to
/// the pool as `give_back` does, deep release included.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn slot_cache_releases_deep_pages_and_overflows_to_the_pool() {
    let pool = StackPool::new(false);
    let page = os::page_size();
    let mut cache = SlotCache::new();
    let slots: Vec<_> = (0..=CACHE_SLOTS)
        .map(|_| pool.take(8 << 20).unwrap())
        .collect();
    let ranges: Vec<(usize, usize, usize)> = slots
        .iter()
        .map(|slot| (slot.usable_bottom(), slot.warm_top(page), slot.top()))
        .collect();
    for slot in slots {
        touch_pages(slot.usable_bottom(), slot.top());
        let low = slot.usable_bottom();
        pool.give_back_cached(&mut cache, slot, low);
    }
    assert_eq!(cache.len(), CACHE_SLOTS);
    assert_eq!(
        pool.counts(8 << 20).unwrap().free,
        1,
        "the fifth went to the pool"
    );
    for (index, &(low, warm, top)) in ranges.iter().enumerate() {
        let below = written_offsets(low, warm);
        assert!(below.is_empty(), "slot {index}: {below:?}");
        assert!(resident(warm, top).iter().all(|&page| page), "slot {index}");
    }
    // A shallow slot keeps its pages: no system call.
    let mut cache = SlotCache::new();
    let slot = pool.take(8 << 20).unwrap();
    let top = slot.top();
    let used = top - (64 << 10);
    touch_pages(used, top);
    pool.give_back_cached(&mut cache, slot, used);
    assert_eq!(peek(used), 1);
}

/// D-825: draining gives every cached slot back to the
/// pool's warm list, where `take` finds it.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn slot_cache_drain_gives_every_slot_back() {
    let pool = StackPool::new(false);
    let mut cache = SlotCache::new();
    let slots: Vec<_> = (0..CACHE_SLOTS).map(|_| pool.take(1).unwrap()).collect();
    let mut bottoms: Vec<usize> = slots.iter().map(StackSlot::bottom).collect();
    for slot in slots {
        pool.give_back_cached(&mut cache, slot, usize::MAX);
    }
    pool.drain_cache(&mut cache);
    assert_eq!(cache.len(), 0);
    assert_eq!(pool.counts(1).unwrap().free, CACHE_SLOTS);
    {
        let state = pool.classes[0].lock();
        assert_eq!((state.warm.len(), state.cold.len()), (CACHE_SLOTS, 0));
    }
    let mut again: Vec<usize> = (0..CACHE_SLOTS)
        .map(|_| pool.take(1).unwrap().bottom())
        .collect();
    again.sort_unstable();
    bottoms.sort_unstable();
    assert_eq!(again, bottoms);
    assert_eq!(pool.counts(1).unwrap().carved, CACHE_SLOTS);
}

/// D-825, D-824: a slot served by a
/// cache counts as taken warm, and a deep one entering it as deep-released.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn slot_cache_counts_as_warm() {
    crate::stats::count_events();
    let pool = StackPool::new(false);
    let mut cache = SlotCache::new();
    let slot = pool.take(1).unwrap();
    let low = slot.usable_bottom();
    pool.give_back_cached(&mut cache, slot, low);
    let slot = pool.take_cached(&mut cache, 1).unwrap();
    assert_eq!(
        pool.slot_counts(),
        SlotCounts {
            taken_warm: 1,
            carved: 1,
            deep_released: 1,
            ..SlotCounts::default()
        }
    );
    pool.give_back(slot, usize::MAX);
}

/// The fields of the `/proc/self/smaps` entry of the mapping that holds
/// `address`: its `VmFlags` line and its `Rss` in KiB.
#[cfg(target_os = "linux")]
fn smaps_entry(address: usize) -> Option<(String, u64)> {
    let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
    let mut inside = false;
    let mut rss = None;
    for line in smaps.lines() {
        let range = line
            .split_whitespace()
            .next()
            .and_then(|first| first.split_once('-'))
            .and_then(|(low, high)| {
                Some((
                    usize::from_str_radix(low, 16).ok()?,
                    usize::from_str_radix(high, 16).ok()?,
                ))
            });
        if let Some((low, high)) = range {
            inside = (low..high).contains(&address);
            rss = None;
        } else if inside && let Some(kib) = line.strip_prefix("Rss:") {
            rss = kib.trim().trim_end_matches("kB").trim().parse().ok();
        } else if inside && let Some(flags) = line.strip_prefix("VmFlags:") {
            return Some((flags.trim().to_owned(), rss.unwrap_or(0)));
        }
    }
    None
}

/// Whether this kernel has transparent huge pages at all; without them
/// `MADV_NOHUGEPAGE` fails with `EINVAL`, which the pool ignores (D-761).
#[cfg(target_os = "linux")]
fn kernel_has_thp() -> bool {
    std::path::Path::new("/sys/kernel/mm/transparent_hugepage").exists()
}

/// D-761: every stack region is marked `MADV_NOHUGEPAGE` (`nh` in its
/// `VmFlags`), also after a release with `MADV_DONTNEED`.
#[test]
#[cfg(target_os = "linux")]
fn stack_regions_are_marked_no_huge_pages() {
    if !kernel_has_thp() {
        eprintln!("this kernel has no transparent huge pages: nothing to mark");
        return;
    }
    let pool = StackPool::new(false);
    let slot = pool.take(8 << 20).unwrap();
    let (low, top) = (slot.usable_bottom(), slot.top());
    let (flags, _) = smaps_entry(top - 1).expect("the slot's mapping is in smaps");
    assert!(flags.split_whitespace().any(|flag| flag == "nh"), "{flags}");
    touch_pages(low, top);
    pool.give_back(slot, low);
    let (flags, _) = smaps_entry(top - 1).expect("the slot's mapping is in smaps");
    assert!(flags.split_whitespace().any(|flag| flag == "nh"), "{flags}");
}

/// Informative (D-761, plan S7c): the resident memory of 64 warm free
/// slots of the default fiber class, each written over its whole warm
/// part, printed straight to standard error (past the test harness's
/// capture) for the Linux check's log. Without huge pages it is about
/// 64 x 384 KiB, not 64 x 2 MiB.
#[test]
#[cfg(target_os = "linux")]
fn stack_rss_of_64_warm_slots() {
    use std::io::Write as _;
    const SLOTS: usize = 64;
    let pool = StackPool::new(false);
    let slots: Vec<_> = (0..SLOTS).map(|_| pool.take(8 << 20).unwrap()).collect();
    let probe = slots[0].top() - 1;
    let low = slots.iter().map(StackSlot::bottom).min().unwrap();
    let high = slots.iter().map(StackSlot::top).max().unwrap();
    for slot in &slots {
        touch_pages(slot.top() - WARM_KEEP, slot.top());
    }
    for slot in slots {
        // A shallow watermark: the slot stays warm with its pages.
        let watermark = slot.top() - WARM_BYTES;
        pool.give_back(slot, watermark);
    }
    // `mincore` over the slots' own range is exact; the mapping's `Rss`
    // may also hold a neighbouring region merged into the same mapping.
    let page = u64::try_from(os::page_size()).unwrap();
    let pages = u64::try_from(resident(low, high).iter().filter(|&&page| page).count()).unwrap();
    let resident_kib = pages * page / 1024;
    let (flags, rss) = smaps_entry(probe).expect("the slots' mapping is in smaps");
    let line = format!(
        "stack rss of {SLOTS} warm slots: {resident_kib} KiB resident in their range ({} KiB per slot; warm part {} KiB); mapping Rss {rss} KiB; thp {}; VmFlags {flags}\n",
        resident_kib / u64::try_from(SLOTS).unwrap(),
        WARM_KEEP >> 10,
        kernel_has_thp(),
    );
    let _ = std::io::stderr().write_all(line.as_bytes());
    assert!(resident_kib > 0, "{line}");
}

/// Runs this binary's `stack_child` test in a child process with the
/// given operation and environment.
#[cfg(unix)]
fn run_child(operation: &str, guard: Option<&str>) -> std::process::Output {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "fiber::stack::tests::stack_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, operation);
    match guard {
        Some(value) => command.env("CHEBY_RT_STACK_GUARD", value),
        None => command.env_remove("CHEBY_RT_STACK_GUARD"),
    };
    // A sanitizer build's child (D-721) must die of the guard page's own
    // signal, which the sanitizer would otherwise catch
    // and turn into a report and an abort (D-863: an intended fault is
    // told to the sanitizer, never suppressed).
    for (variable, on) in [
        ("ASAN_OPTIONS", cfg!(cheby_sanitize = "address")),
        ("TSAN_OPTIONS", cfg!(cheby_sanitize = "thread")),
    ] {
        if on {
            let ours = "handle_segv=0:handle_sigbus=0";
            let options = match std::env::var(variable) {
                Ok(caller) if !caller.is_empty() => format!("{caller}:{ours}"),
                _ => ours.to_owned(),
            };
            command.env(variable, options);
        }
    }
    command.output().unwrap()
}

/// The child side of the guard-page tests: does nothing unless
/// `CHEBY_RT_TEST_STACK_CHILD` names an operation.
#[test]
fn stack_child() {
    let Ok(operation) = std::env::var(CHILD) else {
        return;
    };
    let pool = match operation.as_str() {
        "guarded" => StackPool::new(true),
        "env" => StackPool::from_env(),
        _ => panic!("unknown operation {operation}"),
    };
    println!("guard pages: {}", pool.guard_pages());
    let slot = pool.take(1 << 20).unwrap();
    // Above the guard page: always fine.
    touch(slot.usable_bottom());
    println!("touching the lowest page");
    touch(slot.bottom());
    println!("survived");
}

#[test]
#[cfg(unix)]
fn touching_a_guard_page_kills_the_child() {
    use std::os::unix::process::ExitStatusExt;
    let output = run_child("guarded", None);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("touching the lowest page"), "{stdout}");
    assert!(!stdout.contains("survived"), "{stdout}");
    let signal = output.status.signal();
    assert!(
        signal == Some(libc::SIGSEGV) || signal == Some(libc::SIGBUS),
        "{:?}",
        output.status
    );
}

#[test]
#[cfg(unix)]
fn debug_builds_guard_every_slot_unless_disabled() {
    let output = run_child("env", None);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if cfg!(debug_assertions) {
        assert!(stdout.contains("guard pages: true"), "{stdout}");
        assert!(!output.status.success(), "{stdout}");
    } else {
        assert!(stdout.contains("guard pages: false"), "{stdout}");
        assert!(output.status.success(), "{stdout}");
    }
    let output = run_child("env", Some("0"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("guard pages: false"), "{stdout}");
    assert!(stdout.contains("survived"), "{stdout}");
    assert!(output.status.success(), "{:?}", output.status);
}
