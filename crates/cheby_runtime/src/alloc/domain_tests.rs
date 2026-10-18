//! Sharded accounting domains (D-691): threads charging one domain count
//! in their own shards, and reading sums the shards, exactly once nobody
//! charges any more.

use std::sync::{Arc, Barrier, Mutex, PoisonError};

use super::tests::tuple;
use super::*;
use crate::entry::{self, EntryConfig, EntryMode, EntryOutcome};
use crate::stats::Event;

#[test]
fn shards_are_a_power_of_two_up_to_64() {
    let domain = Domain::new();
    let shards = domain.shards.len();
    // As many shared shards as exclusive ones (D-773).
    assert_eq!(shards, 2 * shard_count());
    let exclusive = shard_count();
    assert!(
        exclusive.is_power_of_two() && exclusive <= 64,
        "{exclusive}"
    );
    // Any shared index lands in some shared shard, and reading sums them.
    domain.charge(exclusive, 1, 8);
    domain.charge(usize::MAX >> 1, 2, 16);
    domain.charge(exclusive.wrapping_add(shards).wrapping_add(3), -1, -8);
    assert_eq!((domain.live_objects(), domain.live_bytes()), (2, 16));
    // Each rise to a shard's own high samples the sum.
    assert_eq!(domain.peak_bytes(), 24);
    domain.reset_peak();
    assert_eq!(domain.peak_bytes(), 16);
    // An index below the exclusive count is exclusive; the others are
    // reduced into the shared half.
    assert!(domain.shard(0).1);
    assert!(domain.shard(exclusive - 1).1);
    for index in [exclusive, exclusive + 1, usize::MAX >> 1, usize::MAX] {
        let (shard, alone) = domain.shard(index);
        assert!(!alone, "{index}");
        let at = domain
            .shards
            .iter()
            .position(|other| std::ptr::eq(&raw const **other, shard))
            .unwrap();
        assert!((exclusive..shards).contains(&at), "{index} at {at}");
    }
}

/// One thread charges a domain: the peak is exact, whatever it frees in
/// between.
#[test]
fn one_writer_samples_the_exact_peak() {
    let domain = Domain::new();
    let shard = shard_count();
    domain.charge(shard, 1, 100);
    domain.charge(shard, -1, -60);
    domain.charge(shard, 1, 50);
    assert_eq!(domain.peak.load(Ordering::Relaxed), 100);
    domain.charge(shard, 1, 11);
    assert_eq!(domain.peak.load(Ordering::Relaxed), 101);
    for _ in 0..1000 {
        domain.charge(shard, 1, 8);
        domain.charge(shard, -1, -8);
    }
    domain.charge(shard, 1, 1);
    assert_eq!(domain.peak.load(Ordering::Relaxed), 109);
}

/// A producer that only allocates and a consumer that only frees, on two
/// shards of one domain, as a channel between two workers does: the
/// producer's shard reaches a new high on every allocation, but the sum
/// of the shards, which reads the consumer's cache line, is taken only
/// now and then (D-691). The counts stay exact.
#[test]
fn a_producer_and_a_consumer_rarely_sum_the_shards() {
    const PAIRS: u64 = 100_000;
    let domain = Domain::new();
    let (producer, consumer) = (shard_count() + 1, shard_count() + 2);
    for _ in 0..PAIRS {
        domain.charge(producer, 1, 16);
        domain.charge(consumer, -1, -16);
    }
    assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    let samples = domain.samples.load(Ordering::Relaxed);
    assert!(samples <= 200, "{samples} samples for {PAIRS} allocations");
    assert_eq!(domain.peak_bytes(), 16);
}

/// Four entries, on four threads at once, allocate into one domain and
/// free part of what they allocated; the count is exact when they are
/// done. The rest is freed by the host, on yet another shard.
#[test]
fn four_threads_count_exactly_in_one_domain() {
    const PER_THREAD: usize = 10_000;
    crate::stats::count_events();
    let domain = Domain::new();
    let kept = Arc::new(Mutex::new(Vec::new()));
    let start = Arc::new(Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (domain, kept, start) =
                (Arc::clone(&domain), Arc::clone(&kept), Arc::clone(&start));
            std::thread::spawn(move || {
                let config = EntryConfig {
                    domain,
                    ..EntryConfig::new(EntryMode::Test)
                };
                let task = move || {
                    start.wait();
                    let mut mine = Vec::new();
                    for n in 0..PER_THREAD {
                        let word = tuple(&[]);
                        if n.is_multiple_of(3) {
                            mine.push(word);
                        } else {
                            // SAFETY: the only reference.
                            unsafe { crate::rc::drop(word) };
                        }
                    }
                    kept.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .extend(mine);
                    0
                };
                // SAFETY: the task handles only its own tuples.
                unsafe { entry::run_task(&config, task) }.unwrap()
            })
        })
        .collect();
    for thread in threads {
        assert_eq!(thread.join().unwrap(), EntryOutcome::Returned(0));
    }
    let kept = std::mem::take(&mut *kept.lock().unwrap());
    let expected = i64::try_from(kept.len()).unwrap();
    assert_eq!(expected, 4 * 3334);
    assert_eq!(domain.live_objects(), expected);
    // An empty tuple is its 8-byte header.
    assert_eq!(domain.live_bytes(), expected.wrapping_mul(8));
    // Counting was turned on before the entries started.
    assert_eq!(domain.stats().count(Event::Alloc), 4 * 10_000);
    with_domain(Arc::clone(&domain), || {
        for word in kept {
            // SAFETY: the only reference, handed over by its entry.
            unsafe { crate::rc::drop(word) };
        }
    });
    assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    assert!(domain.peak_bytes() >= 3334 * 8, "{}", domain.peak_bytes());
}

/// D-773: more fiber threads than exclusive shards,
/// and host threads beside them, charge one domain at once. The threads
/// that hold an exclusive shard charge it with plain stores, the others
/// share the shared shards with atomic adds, and the totals are exact.
#[test]
fn more_threads_than_shards_count_exactly() {
    const PER_THREAD: usize = 3_000;
    let fibers = shard_count() + 3;
    let hosts = 2;
    let domain = Domain::new();
    let kept = Arc::new(Mutex::new(Vec::new()));
    let start = Arc::new(Barrier::new(fibers + hosts));
    let shards = Arc::new(Mutex::new(Vec::new()));
    // Allocates `PER_THREAD` tuples into the current domain, frees two in
    // three, and keeps the rest.
    let work = |kept: &Mutex<Vec<Word>>| {
        let mut mine = Vec::new();
        for n in 0..PER_THREAD {
            let word = tuple(&[]);
            if n.is_multiple_of(3) {
                mine.push(word);
            } else {
                // SAFETY: the only reference.
                unsafe { crate::rc::drop(word) };
            }
        }
        kept.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(mine);
    };
    let mut threads: Vec<_> = (0..fibers)
        .map(|_| {
            let (domain, kept, start, shards) = (
                Arc::clone(&domain),
                Arc::clone(&kept),
                Arc::clone(&start),
                Arc::clone(&shards),
            );
            std::thread::spawn(move || {
                let config = EntryConfig {
                    domain,
                    ..EntryConfig::new(EntryMode::Test)
                };
                let task = move || {
                    start.wait();
                    let shard = entry::current_fiber().map_or(usize::MAX, ExecCtx::shard);
                    shards
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(shard);
                    work(&kept);
                    0
                };
                // SAFETY: the task handles only its own tuples.
                unsafe { entry::run_task(&config, task) }.unwrap()
            })
        })
        .collect();
    threads.extend((0..hosts).map(|_| {
        let (domain, kept, start) = (Arc::clone(&domain), Arc::clone(&kept), Arc::clone(&start));
        std::thread::spawn(move || {
            with_domain(domain, || {
                start.wait();
                assert!(
                    tls::shard_index() >= shard_count(),
                    "a host thread is shared"
                );
                work(&kept);
            });
            EntryOutcome::Returned(0)
        })
    }));
    for thread in threads {
        assert_eq!(thread.join().unwrap(), EntryOutcome::Returned(0));
    }
    // The fiber threads ran at once: those that hold an exclusive shard
    // hold distinct ones, and at most as many as there are.
    let shards = std::mem::take(&mut *shards.lock().unwrap());
    assert_eq!(shards.len(), fibers);
    let mut exclusive: Vec<usize> = shards
        .into_iter()
        .filter(|&shard| shard < shard_count())
        .collect();
    exclusive.sort_unstable();
    let held = exclusive.len();
    exclusive.dedup();
    assert_eq!(exclusive.len(), held, "an exclusive shard held twice");
    assert!(held <= shard_count());
    let kept = std::mem::take(&mut *kept.lock().unwrap());
    let expected = i64::try_from(kept.len()).unwrap();
    assert_eq!(
        expected,
        i64::try_from((fibers + hosts) * PER_THREAD.div_ceil(3)).unwrap()
    );
    assert_eq!(domain.live_objects(), expected);
    assert_eq!(domain.live_bytes(), expected.wrapping_mul(8));
    with_domain(Arc::clone(&domain), || {
        for word in kept {
            // SAFETY: the only reference, handed over by its thread.
            unsafe { crate::rc::drop(word) };
        }
    });
    assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
}

/// A thread that runs fibers charges an exclusive shard that it holds, or
/// when every one is held a shared one; a host thread charges a shared
/// one (D-773).
#[test]
fn a_fiber_thread_charges_a_shard_it_holds() {
    assert!(
        tls::shard_index() >= shard_count(),
        "a host thread is shared"
    );
    let config = EntryConfig::new(EntryMode::Test);
    let task = || {
        let Some(shard) = entry::current_fiber().map(ExecCtx::shard) else {
            return 1;
        };
        // The fiber charges its thread's shard, and an exclusive one is
        // held by this thread.
        if shard != tls::shard_index() {
            return 2;
        }
        if shard < shard_count() && !tls::holds_exclusive_shard(shard) {
            return 3;
        }
        0
    };
    // SAFETY: the task allocates nothing.
    let outcome = unsafe { entry::run_task(&config, task) }.unwrap();
    assert_eq!(outcome, EntryOutcome::Returned(0));
}

/// `cheby_rt_alloc` charges the domain of the control block it is given,
/// whatever the calling thread's current domain is.
#[test]
fn generated_code_allocates_in_its_fibers_domain() {
    unsafe extern "C" fn allocates_through_ctx(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: `ctx` is the running entry's; a 16-byte tuple with one
        // tagged field.
        unsafe {
            let word = cheby_rt_alloc(ctx, 16, abi::header(1, 1, abi::layout::TUPLE));
            object::write_field(word_to_ptr(word), 8, 1);
            word
        }
    }
    let domain = Domain::new();
    let config = EntryConfig {
        domain: Arc::clone(&domain),
        ..EntryConfig::new(EntryMode::Test)
    };
    // SAFETY: the thunk has the entry ABI and static code.
    let outcome = unsafe { entry::run_entry(allocates_through_ctx, &config) }.unwrap();
    let EntryOutcome::Returned(word) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!((domain.live_objects(), domain.live_bytes()), (1, 16));
    with_domain(Arc::clone(&domain), || {
        // SAFETY: the only reference.
        unsafe { crate::rc::drop(word) };
    });
    assert_eq!(domain.live_objects(), 0);
}
