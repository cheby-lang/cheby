//! Tests of the scheduler (D-687 to D-690): Rust-level fibers that park
//! and wake each other at several thread counts, the runnext hand-off,
//! stealing, lost wakeups at the idle protocol, preemption, pinning and the
//! migration stress mode, root fibers, code tokens and `CHEBY_THREADS`.
//!
//! `CHEBY_RT_TEST_EMULATED=1` (set by the container and Rosetta runs)
//! keeps every check but the time bounds.

use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use super::testing::{
    HANG, Mailbox, config, emulated, entry_check, root, runtime, spawn, start_root, tuned,
};
use super::*;
use crate::abi::PanicKind;
use crate::entry::DEFAULT_STACK_SIZE;
use crate::fiber::StackPool;
use crate::panic::cheby_rt_panic;

#[test]
fn the_threads_variable_parses_as_d658_says() {
    let parse = |value: Option<&str>| parse_threads(value.map(OsStr::new), 8);
    let cases: [(Option<&str>, Result<usize, &str>); 13] = [
        (None, Ok(8)),
        (Some(""), Ok(8)),
        (Some("1"), Ok(1)),
        (Some("4"), Ok(4)),
        (Some("007"), Ok(7)),
        (Some("1024"), Ok(1024)),
        (Some("1025"), Ok(1024)),
        (Some("99999999999999999999999"), Ok(1024)),
        (Some("0"), Err("0")),
        (Some("000"), Err("000")),
        (Some("four"), Err("four")),
        (Some("-1"), Err("-1")),
        (Some(" 4"), Err(" 4")),
    ];
    for (value, expected) in cases {
        match (parse(value), expected) {
            (Ok(threads), Ok(want)) => assert_eq!(threads, want, "{value:?}"),
            (Err(RuntimeError::InvalidThreads(seen)), Err(want)) => {
                assert_eq!(seen, want);
            }
            (other, want) => panic!("{value:?}: {other:?}, expected {want:?}"),
        }
    }
    for value in ["+4", "4.0", "0x4", "4 "] {
        assert!(parse(Some(value)).is_err(), "{value}");
    }
    // The CPU count is capped too, and never below 1.
    assert_eq!(parse_threads(None, 4096).unwrap(), MAX_THREADS);
    assert_eq!(parse_threads(None, 0).unwrap(), 1);
    // D-658's message, which the host prints after `error: `.
    let error = parse(Some("four")).unwrap_err();
    assert_eq!(
        error.to_string(),
        "CHEBY_THREADS must be a whole number of at least 1, not \"four\""
    );
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_configured_thread_count_is_clamped() {
    let config = |threads| RuntimeConfig {
        threads: Some(threads),
        ..RuntimeConfig::default()
    };
    assert_eq!(Runtime::new(&config(0)).unwrap().threads(), 1);
    assert_eq!(Runtime::new(&config(3)).unwrap().threads(), 3);
}

/// 10,000 fibers in rings of 100: each takes a word from its mailbox and
/// passes it on, three times round, at 1, 2 and 4 workers.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn ten_thousand_fibers_park_and_wake_each_other() {
    const RING: usize = 100;
    const RINGS_PER_BATCH: usize = 10;
    const BATCHES: usize = 10;
    const ROUNDS: u64 = 3;
    for threads in [1, 2, 4] {
        let runtime = runtime(threads);
        let mut fibers = 0;
        for _ in 0..BATCHES {
            let total = root(&runtime, |me| {
                let done = Arc::new(Mailbox::default());
                let left = Arc::new(AtomicUsize::new(RING * RINGS_PER_BATCH));
                let mut firsts = Vec::new();
                for _ in 0..RINGS_PER_BATCH {
                    let boxes: Arc<Vec<Mailbox>> =
                        Arc::new((0..RING).map(|_| Mailbox::default()).collect());
                    for index in 0..RING {
                        let (boxes, done, left) =
                            (Arc::clone(&boxes), Arc::clone(&done), Arc::clone(&left));
                        spawn(me, false, move |fiber| {
                            let next = (index + 1) % RING;
                            for _ in 0..ROUNDS {
                                let word = boxes[index].receive(fiber);
                                boxes[next].send(word + 1, true);
                            }
                            if left.fetch_sub(1, Ordering::SeqCst) == 1 {
                                done.send(1, false);
                            }
                        });
                    }
                    boxes[0].send(0, false);
                    firsts.push(boxes);
                }
                done.receive(me);
                // Each ring's word went round three times and is back in
                // its first mailbox.
                firsts.iter().map(|boxes| boxes[0].peek().unwrap()).sum()
            });
            let expected = u64::try_from(RING * RINGS_PER_BATCH).unwrap() * ROUNDS;
            assert_eq!(total, expected);
            fibers += RING * RINGS_PER_BATCH;
        }
        assert_eq!(fibers, 10_000);
        // A fiber reports before it exits; once the workers are joined,
        // every one has been retired.
        let shared = Arc::clone(&runtime.shared);
        runtime.shutdown();
        assert_eq!(shared.live.load(Ordering::SeqCst), 0);
    }
}

/// D-689: a capacity-0-style ping-pong goes through the runnext slot, so
/// the two fibers stay on one worker even with four.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn ping_pong_stays_on_one_worker_through_runnext() {
    const ROUNDS: u64 = 10_000;
    let runtime = runtime(4);
    let together = root(&runtime, |me| {
        let request = Arc::new(Mailbox::default());
        let reply = Arc::new(Mailbox::default());
        let done = Arc::new(Mailbox::default());
        let same = Arc::new(AtomicU64::new(0));
        let last = Arc::new(AtomicUsize::new(usize::MAX));
        {
            let (request, reply, same, last) = (
                Arc::clone(&request),
                Arc::clone(&reply),
                Arc::clone(&same),
                Arc::clone(&last),
            );
            spawn(me, false, move |fiber| {
                for _ in 0..ROUNDS {
                    let word = request.receive(fiber);
                    if last.load(Ordering::SeqCst) == fiber.sched.worker() {
                        same.fetch_add(1, Ordering::SeqCst);
                    }
                    reply.send(word, true);
                }
            });
        }
        {
            let (request, reply, done, last) = (
                Arc::clone(&request),
                Arc::clone(&reply),
                Arc::clone(&done),
                Arc::clone(&last),
            );
            spawn(me, false, move |fiber| {
                for round in 0..ROUNDS {
                    last.store(fiber.sched.worker(), Ordering::SeqCst);
                    request.send(round, true);
                    assert_eq!(reply.receive(fiber), round);
                }
                done.send(0, false);
            });
        }
        done.receive(me);
        same.load(Ordering::SeqCst)
    });
    // A thief may take one of them once at the start; after the first
    // hand-off through runnext they share a worker.
    assert!(together >= ROUNDS - ROUNDS / 10, "{together} of {ROUNDS}");
}

/// D-688, §10.1: on one worker, a ping-pong through the runnext slot does
/// not starve a fiber waiting in the local deque. The pair inherits one
/// time slice, so the monitor asks it to yield after one quantum, and the
/// deque runs (review finding CONC3-1: the third fiber never ran).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_ping_pong_through_runnext_lets_the_local_deque_run() {
    /// How long the pair may run before it gives up waiting for the third
    /// fiber; the third should run within a few quanta.
    const BOUND: Duration = Duration::from_secs(10);
    const STOP: u64 = u64::MAX;
    let runtime = runtime(1);
    let word = root(&runtime, |me| {
        let request = Arc::new(Mailbox::default());
        let reply = Arc::new(Mailbox::default());
        let done = Arc::new(Mailbox::default());
        let stop = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        {
            let (request, reply) = (Arc::clone(&request), Arc::clone(&reply));
            spawn(me, false, move |fiber| {
                loop {
                    entry_check(fiber);
                    let word = request.receive(fiber);
                    if word == STOP {
                        break;
                    }
                    reply.send(word, true);
                }
            });
        }
        {
            let (request, reply, done, stop) = (
                Arc::clone(&request),
                Arc::clone(&reply),
                Arc::clone(&done),
                Arc::clone(&stop),
            );
            spawn(me, false, move |fiber| {
                let mut round = 0;
                while !stop.load(Ordering::SeqCst) && started.elapsed() < BOUND {
                    entry_check(fiber);
                    request.send(round, true);
                    assert_eq!(reply.receive(fiber), round);
                    round += 1;
                }
                request.send(STOP, true);
                let waited = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                done.send(waited, false);
            });
        }
        {
            let stop = Arc::clone(&stop);
            spawn(me, false, move |_| stop.store(true, Ordering::SeqCst));
        }
        let waited = done.receive(me);
        u64::from(stop.load(Ordering::SeqCst)) | (waited << 1)
    });
    let (stopped_by_third, waited) = (word & 1 == 1, Duration::from_millis(word >> 1));
    assert!(stopped_by_third, "the third fiber never ran in {waited:?}");
    if !emulated() {
        assert!(waited < Duration::from_secs(2), "{waited:?}");
    }
}

/// D-689: idle workers steal from the deque of the worker that spawns.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn every_worker_steals_some_of_a_thousand_tasks() {
    const TASKS: usize = 1_000;
    const WORKERS: usize = 4;
    let runtime = runtime(WORKERS);
    let ran: Arc<Vec<AtomicUsize>> = Arc::new((0..WORKERS).map(|_| AtomicUsize::new(0)).collect());
    let seen = Arc::clone(&ran);
    root(&runtime, move |me| {
        let done = Arc::new(Mailbox::default());
        let left = Arc::new(AtomicUsize::new(TASKS));
        for _ in 0..TASKS {
            let (ran, done, left) = (Arc::clone(&seen), Arc::clone(&done), Arc::clone(&left));
            spawn(me, false, move |fiber| {
                let started = Instant::now();
                while started.elapsed() < Duration::from_micros(500) {
                    std::hint::spin_loop();
                }
                ran[fiber.sched.worker()].fetch_add(1, Ordering::SeqCst);
                if left.fetch_sub(1, Ordering::SeqCst) == 1 {
                    done.send(0, false);
                }
            });
        }
        done.receive(me)
    });
    let counts: Vec<usize> = ran.iter().map(|n| n.load(Ordering::SeqCst)).collect();
    assert_eq!(counts.iter().sum::<usize>(), TASKS);
    assert!(counts.iter().all(|&n| n > 0), "{counts:?}");
}

/// Tokio's idle protocol loses no wakeup: a host thread wakes a fiber at
/// random moments around its worker going idle, 1,000 times, with the
/// monitor's safety net off, so a lost wakeup would hang.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_wake_racing_a_worker_going_idle_is_never_lost() {
    let runtime = tuned(2, false, false);
    let mut random = 0x2545_f491_4f6c_dd1d_u64;
    for round in 0..1_000_u64 {
        let mailbox = Arc::new(Mailbox::default());
        let inbox = Arc::clone(&mailbox);
        let outcome = start_root(&runtime, MIN_STACK_SIZE, move |me| inbox.receive(me));
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let delay = Duration::from_nanos(random % 50_000);
        let started = Instant::now();
        while started.elapsed() < delay {
            std::hint::spin_loop();
        }
        mailbox.send(round, false);
        match outcome.recv_timeout(Duration::from_secs(10)) {
            Ok(EntryOutcome::Returned(word)) => assert_eq!(word, round),
            other => panic!("round {round}: the wakeup was lost: {other:?}"),
        }
    }
}

/// How long a fiber looping through entry checks on a one-worker runtime
/// keeps a spawned fiber waiting, inside a kernel callback (pinned, D-687)
/// with `pinned`. Panics if the spawned fiber never runs.
fn preemption_latency(pinned: bool) -> Duration {
    // Without preemption the loop would never end; natively 2 s is far
    // past the 50 ms D-688 allows, under an emulator only a hang counts.
    let give_up = if emulated() {
        HANG
    } else {
        Duration::from_secs(2)
    };
    let runtime = runtime(1);
    // A Rust panic cannot unwind out of a fiber, so the fiber reports
    // `u64::MAX` and the test thread fails.
    let waited = root(&runtime, move |me| {
        let flag = Arc::new(AtomicBool::new(false));
        let set = Arc::clone(&flag);
        spawn(me, false, move |_| set.store(true, Ordering::SeqCst));
        if pinned {
            me.sched.enter_callback();
        }
        let started = Instant::now();
        while !flag.load(Ordering::SeqCst) && started.elapsed() < give_up {
            entry_check(me);
        }
        let waited = started.elapsed();
        if pinned {
            me.sched.leave_callback();
        }
        if flag.load(Ordering::SeqCst) {
            u64::try_from(waited.as_micros()).unwrap_or(u64::MAX)
        } else {
            u64::MAX
        }
    });
    assert_ne!(waited, u64::MAX, "never preempted (pinned: {pinned})");
    Duration::from_micros(waited)
}

/// D-688: the monitor asks a fiber that loops through entry checks to
/// yield, so another fiber on its worker runs within 50 ms, and not before
/// 5 ms. The monitor and the worker are OS threads, so on a loaded machine
/// one measurement can miss the bound by an OS scheduling delay (review
/// finding R2-ROB-2): the bound must hold in one of five measurements, and
/// the lower bound and the hang check in all of them.
fn preempted_within_fifty_milliseconds(pinned: bool) {
    let mut missed = Vec::new();
    for _ in 0..5 {
        let waited = preemption_latency(pinned);
        assert!(waited >= Duration::from_millis(5), "{waited:?}");
        if emulated() || waited < Duration::from_millis(50) {
            return;
        }
        missed.push(waited);
    }
    panic!("never preempted within 50 ms: {missed:?}");
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_looping_fiber_is_preempted_within_fifty_milliseconds() {
    preempted_within_fifty_milliseconds(false);
}

/// D-687 and D-688: a pinned fiber's yield lets the other fibers of its
/// worker run, though it goes back to the worker's own targeted queue
/// (review finding CONC2-2: it was resumed at once, and the spawned fiber
/// never ran while the callback looped).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_looping_pinned_fiber_is_preempted_within_fifty_milliseconds() {
    preempted_within_fifty_milliseconds(true);
}

/// I-8: the monitor never requests a yield from a retired control block,
/// also when the fiber it read moves to another worker and ends there
/// while the monitor still looks (review finding CONC2-1). The probe
/// delays the monitor between reading the running fiber and its request.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn the_monitor_never_touches_a_fiber_another_worker_retired() {
    let runtime = tuned(2, true, true);
    runtime.shared.probe.delay_ms.store(60, Ordering::SeqCst);
    root(&runtime, |me| {
        // Long enough for the monitor to look twice and request a yield,
        // which no entry check consumes.
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(30) {
            std::hint::spin_loop();
        }
        // In the stress mode the fiber resumes on the other worker and
        // ends there.
        park(me, AfterSwitch::Requeue);
        0
    });
    // Lets a delayed request finish.
    std::thread::sleep(Duration::from_millis(150));
    let shared = Arc::clone(&runtime.shared);
    runtime.shutdown();
    assert_eq!(
        shared.probe.stale.load(Ordering::SeqCst),
        0,
        "the monitor requested a yield from a retired control block"
    );
}

/// D-687: a pinned fiber resumes on its worker; in the migration stress
/// mode an unpinned one resumes on another.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn pinned_fibers_stay_and_the_stress_mode_migrates_the_others() {
    let runtime = tuned(4, true, true);
    let moves = root(&runtime, |me| {
        me.sched.enter_callback();
        for _ in 0..100 {
            let before = me.sched.worker();
            park(me, AfterSwitch::Requeue);
            assert_eq!(me.sched.worker(), before, "a pinned fiber moved");
        }
        me.sched.leave_callback();
        let mut moves = 0;
        for _ in 0..100 {
            let before = me.sched.worker();
            park(me, AfterSwitch::Requeue);
            if me.sched.worker() != before {
                moves += 1;
            }
        }
        moves
    });
    assert_eq!(moves, 100);
}

/// The monitor leaves a lone fiber alone, so it keeps its worker; in the
/// migration stress mode it preempts it, and it moves between workers.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn lone_fibers_are_preempted_only_in_the_stress_mode() {
    let loop_for = |me: &ExecCtx, time| {
        let started = Instant::now();
        let first = me.sched.worker();
        let mut moves = 0;
        let mut last = first;
        while started.elapsed() < time {
            entry_check(me);
            if me.sched.worker() != last {
                moves += 1;
                last = me.sched.worker();
            }
        }
        moves
    };
    let calm = runtime(2);
    let moves = root(&calm, move |me| loop_for(me, Duration::from_millis(60)));
    assert_eq!(moves, 0);
    let stressed = tuned(2, true, true);
    let moves = root(&stressed, move |me| {
        loop_for(me, Duration::from_millis(200))
    });
    assert!(moves >= 2, "{moves}");
}

unsafe extern "C" fn returns_seven(_: *mut ExecCtx) -> u64 {
    7
}

unsafe extern "C" fn panics(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: `ctx` is the running fiber's; no message or location. Under
    // the default backend it never returns.
    unsafe { cheby_rt_panic(ctx, PanicKind::DivisionByZero.code(), 0, 0) };
    0
}

/// D-690 and GAP(interim-panics): a root fiber's word reaches the host; a
/// root that panics is reported, its slot quarantined, and the runtime
/// goes on running roots.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn roots_return_or_panic_and_the_runtime_goes_on() {
    let runtime = runtime(2);
    let run = |entry: EntryFn, stack_size| {
        // SAFETY: test thunks with the `EntryFn` ABI and static code.
        unsafe { runtime.run_root(entry, &config(stack_size)) }.unwrap()
    };
    assert_eq!(
        run(returns_seven, MIN_STACK_SIZE),
        EntryOutcome::Returned(7)
    );
    // A class no other test of this module uses, so the count is ours.
    let size = 4 << 20;
    let before = StackPool::global().counts(size).unwrap().quarantined;
    let EntryOutcome::Panicked(report) = run(panics, size) else {
        panic!("expected a panic");
    };
    assert_eq!(report.kind, PanicKind::DivisionByZero);
    // The worker quarantines the slot after the host heard of the panic.
    let deadline = Instant::now() + HANG;
    while StackPool::global().counts(size).unwrap().quarantined == before {
        assert!(Instant::now() < deadline, "the slot was not quarantined");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(run(returns_seven, size), EntryOutcome::Returned(7));
    // SAFETY: as above.
    let too_small = unsafe { runtime.run_root(returns_seven, &config(4096)) };
    assert!(matches!(too_small, Err(RuntimeError::StackTooSmall { .. })));
    runtime.shutdown();
}

/// GAP(interim-panics): until unwinding exists, a panic in a fiber that is
/// not a root ends the process with a fatal runtime error. The test runs
/// itself in a child process, which aborts.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_child_fibers_panic_ends_the_process() {
    const CHILD: &str = "CHEBY_RT_TEST_CHILD_FIBER_PANIC";
    const NAME: &str = "sched::tests::a_child_fibers_panic_ends_the_process";
    if std::env::var_os(CHILD).is_some() {
        let runtime = runtime(2);
        root(&runtime, |me| {
            spawn(me, false, |fiber| {
                // SAFETY: `fiber` is the running fiber; no message or
                // location.
                unsafe {
                    cheby_rt_panic(
                        std::ptr::from_ref(fiber).cast_mut(),
                        PanicKind::DivisionByZero.code(),
                        0,
                        0,
                    );
                }
            });
            // Waits for ever: the child's panic ends the process first.
            Mailbox::default().receive(me)
        });
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains(
            "fatal runtime error: a fiber panicked (unwinding arrives with package U4)\n"
        ),
        "{stderr}"
    );
}

/// D-677: a root fiber that goes deep lowers its soft limit, in doubling
/// steps, and one that goes too deep overflows at its hard limit.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn deep_fibers_lower_their_soft_limit_and_overflow_at_the_hard_one() {
    fn recurse(ctx: &ExecCtx, depth: u64) -> u64 {
        entry_check(ctx);
        let frame = std::hint::black_box([depth; 64]);
        if depth == 0 {
            return 0;
        }
        recurse(ctx, depth - 1).wrapping_add(frame[1])
    }
    let runtime = runtime(1);
    let lowered = start_root(&runtime, 2 << 20, |me| {
        let first = me.real_stack_limit();
        // Several hundred KiB of frames in a 2 MiB slot, whose first soft
        // limit is 128 KiB below its top.
        recurse(me, 1_000);
        let now = me.real_stack_limit();
        u64::from(now < first && now >= me.hard_limit())
    })
    .recv_timeout(HANG)
    .unwrap();
    assert_eq!(lowered, EntryOutcome::Returned(1));
    let outcome = start_root(&runtime, DEFAULT_STACK_SIZE, |me| {
        recurse(me, u64::MAX >> 1)
    })
    .recv_timeout(HANG)
    .unwrap();
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("expected a stack overflow, got {outcome:?}");
    };
    assert_eq!(report.kind, PanicKind::StackOverflow);
}

/// D-690: a runtime's fibers read its program arguments.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn fibers_read_their_runtimes_program_arguments() {
    let runtime = Runtime::new(&RuntimeConfig {
        threads: Some(1),
        args: vec!["a".to_owned(), "b".to_owned()],
        program_name: "prog".to_owned(),
    })
    .unwrap();
    let ok = root(&runtime, |_| {
        u64::from(
            crate::entry::program_args() == ["a", "b"] && crate::entry::program_name() == "prog",
        )
    });
    assert_eq!(ok, 1);
}

/// D-708: a runtime keeps the code tokens attached to it, once each,
/// until it stops.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn code_tokens_are_released_after_the_runtime_stops() {
    struct KeepAlive(Arc<StdMutex<bool>>);
    impl Drop for KeepAlive {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = true;
        }
    }
    let freed = Arc::new(StdMutex::new(false));
    let token = CodeToken::new(Arc::new(KeepAlive(Arc::clone(&freed))));
    let runtime = runtime(1);
    runtime.attach(token.clone());
    runtime.attach(token.clone());
    assert_eq!(runtime.code.lock().len(), 1);
    assert!(token.same(&token.clone()));
    drop(token);
    assert!(!*freed.lock().unwrap());
    runtime.shutdown();
    assert!(*freed.lock().unwrap());
}
