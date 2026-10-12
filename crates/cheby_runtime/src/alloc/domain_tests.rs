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
    assert_eq!(shards, shard_count());
    assert!(shards.is_power_of_two() && shards <= 64, "{shards}");
    // Any index lands in some shard, and reading sums them.
    domain.charge(0, 1, 8);
    domain.charge(usize::MAX >> 1, 2, 16);
    domain.charge(shards.wrapping_add(3), -1, -8);
    assert_eq!((domain.live_objects(), domain.live_bytes()), (2, 16));
    // Each rise to a shard's own high samples the sum.
    assert_eq!(domain.peak_bytes(), 24);
    domain.reset_peak();
    assert_eq!(domain.peak_bytes(), 16);
}

/// One thread charges a domain: the peak is exact, whatever it frees in
/// between.
#[test]
fn one_writer_samples_the_exact_peak() {
    let domain = Domain::new();
    domain.charge(0, 1, 100);
    domain.charge(0, -1, -60);
    domain.charge(0, 1, 50);
    assert_eq!(domain.peak.load(Ordering::Relaxed), 100);
    domain.charge(0, 1, 11);
    assert_eq!(domain.peak.load(Ordering::Relaxed), 101);
    for _ in 0..1000 {
        domain.charge(0, 1, 8);
        domain.charge(0, -1, -8);
    }
    domain.charge(0, 1, 1);
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
    for _ in 0..PAIRS {
        domain.charge(1, 1, 16);
        domain.charge(2, -1, -16);
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
