//! Tests of captured output (D-723, ADR-0062, D-684) and of the root starts
//! a host does not block on (`Runtime::start_root_in`,
//! `Runtime::start_root_notify`, D-799, D-802) and `Runtime::exit_status`
//! (D-709). They use the harness of `fiber_ops::detached_tests` (Rust
//! actions run through a test trampoline) and run at 1, 2 and 4 workers.
//! A root's body is a Rust closure that the entry thunk [`run_body`]
//! finds by the root's group, so every root here has a group of its own.
//! A test that could hang ends through a channel or a timeout.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use parking_lot::Mutex;

use super::{Capture, Chunk, Stream};
use crate::abi::{PanicKind, Word};
use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::{EntryConfig, EntryOutcome, RuntimeError};
use crate::fiber_ops::detached_tests::{
    close_scope, config, detach, new_channel, open_scope, receive_on, release, runtime, spawn_into,
    yield_until,
};
use crate::io::{cheby_io_eprint, cheby_io_eprintln, cheby_io_print, cheby_io_println};
use crate::sched::testing;
use crate::{DetachedGroup, RootDone, RootHandle, Runtime, values};

const THREADS: [usize; 3] = [1, 2, 4];

/// What a root runs: its word.
type Body = Box<dyn FnOnce(&ExecCtx) -> Word + Send>;

/// The bodies of roots not started yet, by the address of their group.
static BODIES: Mutex<Option<HashMap<usize, Body>>> = Mutex::new(None);

/// The key of `group` in [`BODIES`].
fn key(group: &Arc<DetachedGroup>) -> usize {
    Arc::as_ptr(group).addr()
}

/// The entry thunk of every root here: runs the body registered for the
/// root's group.
unsafe extern "C" fn run_body(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: the root fiber's live control block.
    let me = unsafe { &*ctx };
    let body = me.conc.group().and_then(|group| {
        BODIES
            .lock()
            .as_mut()
            .and_then(|bodies| bodies.remove(&key(&group)))
    });
    body.map_or_else(values::nil, |body| body(me))
}

/// Registers `body` as the body of the root to start in `group`.
fn register(group: &Arc<DetachedGroup>, body: impl FnOnce(&ExecCtx) -> Word + Send + 'static) {
    BODIES
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(key(group), Box::new(body));
}

/// Whether the body registered for `group` has not run.
fn unstarted(group: &Arc<DetachedGroup>) -> bool {
    BODIES
        .lock()
        .as_mut()
        .and_then(|bodies| bodies.remove(&key(group)))
        .is_some()
}

/// Starts `body` as a root in `group` with `Runtime::start_root_in`.
fn start(
    runtime: &Runtime,
    config: &EntryConfig,
    group: &Arc<DetachedGroup>,
    body: impl FnOnce(&ExecCtx) -> Word + Send + 'static,
) -> RootHandle {
    register(group, body);
    // SAFETY: `run_body` is a Rust entry thunk with the entry ABI, valid
    // for the process's life.
    unsafe { runtime.start_root_in(run_body, config, group) }.unwrap()
}

/// Starts `body` as a root in `group` with `Runtime::start_root_notify`.
fn notify(
    runtime: &Runtime,
    config: &EntryConfig,
    group: &Arc<DetachedGroup>,
    tag: usize,
    done: RootDone,
    body: impl FnOnce(&ExecCtx) -> Word + Send + 'static,
) {
    register(group, body);
    // SAFETY: as in `start`.
    unsafe { runtime.start_root_notify(run_body, config, group, tag, done) }.unwrap();
}

/// The word a root returned.
fn returned(outcome: Result<EntryOutcome, RuntimeError>) -> Word {
    match outcome {
        Ok(EntryOutcome::Returned(word)) => word,
        other => panic!("the root did not return: {other:?}"),
    }
}

/// The text of each written chunk, with its stream; flushes are left out
/// ([`capture_flushes_are_recorded_in_order`] checks them).
fn texts(chunks: Vec<Chunk>) -> Vec<(Stream, String)> {
    chunks
        .into_iter()
        .filter_map(|chunk| match chunk {
            Chunk::Write(stream, bytes) => Some((stream, String::from_utf8(bytes).unwrap())),
            Chunk::Flush => None,
        })
        .collect()
}

/// `(stream, text)` with an owned text.
fn chunk(stream: Stream, text: &str) -> (Stream, String) {
    (stream, text.to_owned())
}

/// Calls the print kernel `kernel` with `text`.
fn print(kernel: unsafe extern "C" fn(Word), text: &str) {
    let word = values::string(text);
    // SAFETY: a live `String`, borrowed for the call.
    unsafe { kernel(word) };
    release(word);
}

/// Shuts `runtime` down and checks that `domain` holds nothing.
fn finish(runtime: Runtime, domain: &Domain) {
    runtime.shutdown();
    if crate::LEAK_CHECK_ENABLED {
        assert_eq!(domain.live_objects(), 0, "objects leaked");
    }
}

/// D-723: a fiber of a group with a capture hands each write to it as one
/// chunk, the parts joined, and its flushes are no-ops; the root's later
/// write follows its earlier one.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn capture_route_takes_a_captured_fibers_writes() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        let capture = Capture::new();
        let group = DetachedGroup::with_capture(Arc::clone(&capture));
        let handle = start(&runtime, &config(&domain), &group, |_| {
            let first = super::route(Stream::Stdout, &[b"a", b"b"]);
            let second = super::route(Stream::Stderr, &[b"c"]);
            let empty = super::route(Stream::Stdout, &[]);
            let all = [first, second, empty]
                .iter()
                .all(|taken| matches!(taken, Some(Ok(()))));
            values::bool_word(all && super::captured())
        });
        assert_eq!(returned(handle.wait()), values::bool_word(true));
        assert_eq!(
            texts(capture.take()),
            [chunk(Stream::Stdout, "ab"), chunk(Stream::Stderr, "c")]
        );
        assert!(capture.take().is_empty(), "take leaves the capture empty");
        finish(runtime, &domain);
    }
}

/// D-723: a captured fiber's flush of standard output is recorded between
/// its writes, once for flushes in a row, so the host's replay flushes
/// where the program did; a host thread's flush is not taken.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn capture_flushes_are_recorded_in_order() {
    assert!(!super::flush(), "a host thread's flush is its own");
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        let capture = Capture::new();
        let group = DetachedGroup::with_capture(Arc::clone(&capture));
        let handle = start(&runtime, &config(&domain), &group, |_| {
            let taken = super::route(Stream::Stdout, &[b"prompt"]).is_some()
                && super::flush()
                && super::flush()
                && super::route(Stream::Stderr, &[b"after"]).is_some();
            values::bool_word(taken)
        });
        assert_eq!(returned(handle.wait()), values::bool_word(true));
        assert_eq!(
            capture.take(),
            [
                Chunk::Write(Stream::Stdout, b"prompt".to_vec()),
                Chunk::Flush,
                Chunk::Write(Stream::Stderr, b"after".to_vec()),
            ]
        );
        finish(runtime, &domain);
    }
}

/// A thunk that reports whether its isolated entry's writes are captured.
unsafe extern "C" fn isolated_route(_ctx: *mut ExecCtx) -> u64 {
    let routed = super::route(Stream::Stdout, &[b"x"]).is_some() || super::captured();
    u64::from(routed)
}

/// D-723: a host thread, an isolated entry and a fiber of a group without
/// a capture write to the real stream: `route` declines them.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn capture_route_declines_hosts_isolated_entries_and_uncaptured_groups() {
    assert!(super::route(Stream::Stdout, &[b"host"]).is_none());
    assert!(!super::captured());
    let domain = Domain::new();
    // SAFETY: a test thunk with the entry ABI.
    let isolated = unsafe { crate::entry::run_entry(isolated_route, &config(&domain)) };
    assert_eq!(isolated.unwrap(), EntryOutcome::Returned(0));
    for threads in THREADS {
        let runtime = runtime(threads);
        let group = DetachedGroup::new();
        let handle = start(&runtime, &config(&domain), &group, |_| {
            let routed = super::route(Stream::Stdout, &[]).is_some() || super::captured();
            values::bool_word(routed)
        });
        assert_eq!(returned(handle.wait()), values::bool_word(false));
        finish(runtime, &domain);
    }
}

/// D-723, D-799: the scoped child of a root and a detached fiber of the
/// root, and a detached fiber spawned by that child, reach the root's
/// capture through the group they inherit; each fiber's chunks keep its
/// write order, whichever workers they run on.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn capture_chunks_keep_each_fibers_order() {
    const WRITES: usize = 20;
    /// Writes `WRITES` numbered chunks as `name`, yielding between some.
    fn write_all(me: &ExecCtx, name: &'static str) {
        for n in 0..WRITES {
            let text = format!("{name}{n};");
            assert!(matches!(
                super::route(Stream::Stdout, &[text.as_bytes()]),
                Some(Ok(()))
            ));
            if n % 5 == 0 {
                crate::sched::park(me, crate::sched::AfterSwitch::Requeue);
            }
        }
    }
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        let capture = Capture::new();
        let group = DetachedGroup::with_capture(Arc::clone(&capture));
        let watched = Arc::clone(&group);
        let handle = start(&runtime, &config(&domain), &group, move |me| {
            let scope = open_scope(me);
            let child = spawn_into(me, scope, |me| {
                detach(me, |me| {
                    write_all(me, "n");
                    values::nil()
                });
                write_all(me, "c");
                values::nil()
            });
            detach(me, |me| {
                write_all(me, "d");
                values::nil()
            });
            write_all(me, "r");
            assert!(close_scope(me, scope));
            release(child);
            yield_until(me, || watched.live() == 0);
            values::nil()
        });
        returned(handle.wait());
        let chunks = texts(capture.take());
        assert_eq!(chunks.len(), 4 * WRITES, "{chunks:?}");
        for name in ["r", "c", "d", "n"] {
            let seen: Vec<&str> = chunks
                .iter()
                .map(|(stream, text)| {
                    assert_eq!(*stream, Stream::Stdout);
                    text.as_str()
                })
                .filter(|text| text.starts_with(name))
                .collect();
            let expected: Vec<String> = (0..WRITES).map(|n| format!("{name}{n};")).collect();
            assert_eq!(seen, expected, "fiber {name} at {threads} workers");
        }
        finish(runtime, &domain);
    }
}

/// D-723, D-684: a root's `print`, `println`, `eprint` and `eprintln` go
/// through the funnel to its group's capture, one chunk per call with the
/// line's end in it, and its flush succeeds without writing.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn capture_print_kernels_of_a_root() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        let capture = Capture::new();
        let group = DetachedGroup::with_capture(Arc::clone(&capture));
        let handle = start(&runtime, &config(&domain), &group, |_| {
            print(cheby_io_print, "a");
            print(cheby_io_println, "b");
            print(cheby_io_eprint, "c");
            print(cheby_io_eprintln, "d");
            values::bool_word(crate::io::flush_stdout().is_ok())
        });
        assert_eq!(returned(handle.wait()), values::bool_word(true));
        assert_eq!(
            texts(capture.take()),
            [
                chunk(Stream::Stdout, "a"),
                chunk(Stream::Stdout, "b\n"),
                chunk(Stream::Stderr, "c"),
                chunk(Stream::Stderr, "d\n"),
            ]
        );
        finish(runtime, &domain);
    }
}

/// D-723, D-799: the prints of a root's scoped child, of its detached
/// fiber and of a detached fiber the child spawns land in the root's
/// capture, each once.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn capture_prints_of_scoped_and_detached_fibers() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        let capture = Capture::new();
        let group = DetachedGroup::with_capture(Arc::clone(&capture));
        let watched = Arc::clone(&group);
        let handle = start(&runtime, &config(&domain), &group, move |me| {
            let scope = open_scope(me);
            let child = spawn_into(me, scope, |me| {
                detach(me, |_| {
                    print(cheby_io_println, "nested");
                    values::nil()
                });
                print(cheby_io_println, "child");
                values::nil()
            });
            detach(me, |_| {
                print(cheby_io_eprintln, "detached");
                values::nil()
            });
            assert!(close_scope(me, scope));
            release(child);
            yield_until(me, || watched.live() == 0);
            print(cheby_io_println, "root");
            values::nil()
        });
        returned(handle.wait());
        let mut chunks = texts(capture.take());
        assert_eq!(chunks.pop(), Some(chunk(Stream::Stdout, "root\n")));
        chunks.sort_by_key(|(stream, text)| (*stream == Stream::Stderr, text.clone()));
        assert_eq!(
            chunks,
            [
                chunk(Stream::Stdout, "child\n"),
                chunk(Stream::Stdout, "nested\n"),
                chunk(Stream::Stderr, "detached\n"),
            ],
            "at {threads} workers"
        );
        finish(runtime, &domain);
    }
}

/// D-723, D-669, D-701: a detached fiber's panic report, written where its
/// unwinding ends after the (captured, so empty) flush, lands in its
/// group's capture after the text printed before it.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn capture_detached_panic_report() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        let capture = Capture::new();
        let group = DetachedGroup::with_capture(Arc::clone(&capture));
        let watched = Arc::clone(&group);
        let handle = start(&runtime, &config(&domain), &group, move |me| {
            print(cheby_io_print, "before|");
            detach(me, |me| {
                let message = values::string("a detached failure");
                let location = values::string("d.cheby:3:4");
                // SAFETY: the running fiber's context and live strings.
                unsafe {
                    crate::panic::cheby_rt_panic(
                        std::ptr::from_ref(me).cast_mut(),
                        PanicKind::Explicit.code(),
                        message,
                        location,
                    );
                }
                release(message);
                release(location);
                0
            });
            yield_until(me, || watched.live() == 0);
            values::nil()
        });
        returned(handle.wait());
        assert_eq!(
            texts(capture.take()),
            [
                chunk(Stream::Stdout, "before|"),
                chunk(
                    Stream::Stderr,
                    "panicked at d.cheby:3:4:\na detached failure\n"
                ),
            ]
        );
        finish(runtime, &domain);
    }
}

/// D-723: a root of a group without a capture prints to the real standard
/// output, while a captured root's print never reaches it. Runs itself in
/// a child process whose standard output and error share one pipe.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn capture_uncaptured_group_writes_through() {
    const CHILD: &str = "CHEBY_RT_TEST_CAPTURE_THROUGH";
    const NAME: &str = "capture::tests::capture_uncaptured_group_writes_through";
    if std::env::var_os(CHILD).is_some() {
        for threads in THREADS {
            let runtime = runtime(threads);
            let domain = Domain::new();
            let capture = Capture::new();
            let hidden = DetachedGroup::with_capture(Arc::clone(&capture));
            let handle = start(&runtime, &config(&domain), &hidden, |_| {
                print(cheby_io_print, "hidden|");
                values::nil()
            });
            returned(handle.wait());
            let plain = DetachedGroup::new();
            let handle = start(&runtime, &config(&domain), &plain, |_| {
                print(cheby_io_print, "through|");
                values::nil()
            });
            returned(handle.wait());
            assert_eq!(texts(capture.take()), [chunk(Stream::Stdout, "hidden|")]);
            finish(runtime, &domain);
        }
        crate::io::flush_stdout().unwrap();
        eprintln!("|child done");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("exec \"$0\" --exact \"$1\" --nocapture --test-threads=1 -q 2>&1")
        .arg(exe)
        .arg(NAME)
        .env(CHILD, "1")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert_eq!(text.matches("through|").count(), THREADS.len(), "{text}");
    assert!(!text.contains("hidden|"), "{text}");
    assert!(text.contains("|child done"), "{text}");
}

/// `Runtime::start_root_in`: several roots started at once, each in its
/// own group, are waited for in any order; each counts one entry in the
/// domain it charges.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn root_handle_several_roots_waited_in_any_order() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        let handles: Vec<(i64, RootHandle)> = (0..6_i64)
            .map(|n| {
                let group = DetachedGroup::new();
                let body = move |_: &ExecCtx| values::int_word(n);
                (n, start(&runtime, &config(&domain), &group, body))
            })
            .collect();
        for (n, handle) in handles.into_iter().rev() {
            assert_eq!(returned(handle.wait()), values::int_word(n));
        }
        assert_eq!(domain.stats().entries().0, 6);
        finish(runtime, &domain);
    }
}

/// `RootHandle::wait_timeout` gives the handle back while the root runs,
/// and the outcome once it has ended.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn root_handle_wait_timeout_returns_the_handle_while_the_root_runs() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        let group = DetachedGroup::new();
        let gate = Arc::new(AtomicBool::new(false));
        let open = Arc::clone(&gate);
        let handle = start(&runtime, &config(&domain), &group, move |me| {
            yield_until(me, || open.load(Ordering::SeqCst));
            values::int_word(7)
        });
        let handle = match handle.wait_timeout(Duration::from_millis(20)) {
            Err(handle) => handle,
            Ok(outcome) => panic!("the root ended before its gate opened: {outcome:?}"),
        };
        gate.store(true, Ordering::SeqCst);
        let Ok(outcome) = handle.wait_timeout(testing::hang()) else {
            panic!("the root hung")
        };
        assert_eq!(returned(outcome), values::int_word(7));
        finish(runtime, &domain);
    }
}

/// `Runtime::start_root_notify`: four roots on one channel report each
/// tag once, in the order they end; the channel then disconnects, so no
/// second report can follow.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn root_handle_notify_reports_each_tag_once_in_completion_order() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        let (done, reports) = mpsc::channel();
        let gates: Vec<Arc<AtomicBool>> =
            (0..4).map(|_| Arc::new(AtomicBool::new(false))).collect();
        let mut groups = Vec::new();
        for (tag, gate) in gates.iter().enumerate() {
            let group = DetachedGroup::new();
            let open = Arc::clone(gate);
            let word = values::int_word(i64::try_from(tag).unwrap());
            let body = move |me: &ExecCtx| {
                yield_until(me, || open.load(Ordering::SeqCst));
                word
            };
            notify(&runtime, &config(&domain), &group, tag, done.clone(), body);
            groups.push(group);
        }
        drop(done);
        assert!(matches!(
            reports.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        for tag in [2_usize, 0, 3, 1] {
            gates[tag].store(true, Ordering::SeqCst);
            let (got, outcome) = reports.recv_timeout(testing::hang()).unwrap();
            assert_eq!(got, tag);
            let expected = values::int_word(i64::try_from(tag).unwrap());
            assert_eq!(returned(outcome), expected);
        }
        let after = reports.recv_timeout(testing::hang());
        assert!(
            matches!(after, Err(mpsc::RecvTimeoutError::Disconnected)),
            "a root reported twice: {after:?}"
        );
        assert_eq!(domain.stats().entries().0, 4);
        drop(groups);
        finish(runtime, &domain);
    }
}

/// `Runtime::start_root_notify` reports a root's panic as its outcome, and
/// a root that cannot start returns the error and sends nothing.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn root_handle_notify_reports_a_panic_and_nothing_for_a_refused_start() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        let (done, reports) = mpsc::channel();
        let group = DetachedGroup::new();
        notify(&runtime, &config(&domain), &group, 9, done, |me| {
            let message = values::string("a root failure");
            let location = values::string("r.cheby:1:2");
            // SAFETY: the running fiber's context and live strings.
            unsafe {
                crate::panic::cheby_rt_panic(
                    std::ptr::from_ref(me).cast_mut(),
                    PanicKind::Explicit.code(),
                    message,
                    location,
                );
            }
            release(message);
            release(location);
            0
        });
        let (tag, outcome) = reports.recv_timeout(testing::hang()).unwrap();
        assert_eq!(tag, 9);
        match outcome {
            Ok(EntryOutcome::Panicked(report)) => {
                assert!(report.to_string().contains("a root failure"), "{report}");
            }
            other => panic!("the root did not panic: {other:?}"),
        }
        assert!(
            reports.recv_timeout(testing::hang()).is_err(),
            "a second report"
        );
        let (done, reports) = mpsc::channel();
        let small = EntryConfig {
            stack_size: 16,
            ..config(&domain)
        };
        let refused = DetachedGroup::new();
        register(&refused, |_| values::nil());
        // SAFETY: as in `start`.
        let error = unsafe { runtime.start_root_notify(run_body, &small, &refused, 1, done) };
        let too_small = matches!(error, Err(RuntimeError::StackTooSmall { .. }));
        assert!(too_small, "{error:?}");
        assert!(unstarted(&refused), "the refused root ran");
        assert!(matches!(
            reports.recv_timeout(testing::hang()),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        finish(runtime, &domain);
    }
}

/// D-709, D-802: `Runtime::exit_status` is `None` until a detached fiber
/// terminates, then the fixed status; the root reports `Exited` through
/// `start_root_notify`; a root started afterwards, through either form,
/// never runs and reports `Exited` at once. The host completes the halt.
/// Nothing ends the process: this test runs in-process.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn root_handle_exit_status_after_a_detached_fibers_termination() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        assert_eq!(runtime.exit_status(), None);
        let calm = DetachedGroup::new();
        returned(start(&runtime, &config(&domain), &calm, |_| values::nil()).wait());
        assert_eq!(runtime.exit_status(), None);
        let (done, reports) = mpsc::channel();
        let group = DetachedGroup::new();
        notify(&runtime, &config(&domain), &group, 4, done, |me| {
            let (tx, rx) = new_channel(me, 1);
            detach(me, |_| crate::sched::terminate(3));
            // The root waits; the termination freezes or strands it.
            let got = receive_on(me, rx);
            release(tx);
            release(rx);
            panic!("the root went on after the termination: {got:?}");
        });
        let (tag, outcome) = reports.recv_timeout(testing::hang()).unwrap();
        assert_eq!(tag, 4);
        let exited = matches!(outcome, Ok(EntryOutcome::Exited(3)));
        assert!(exited, "{outcome:?}");
        let after = reports.recv_timeout(testing::hang());
        assert!(
            matches!(after, Err(mpsc::RecvTimeoutError::Disconnected)),
            "a second report: {after:?}"
        );
        assert_eq!(runtime.exit_status(), Some(3));
        let late = DetachedGroup::new();
        let handle = start(&runtime, &config(&domain), &late, |_| {
            panic!("a root ran after the termination")
        });
        assert!(matches!(handle.wait(), Ok(EntryOutcome::Exited(3))));
        assert!(unstarted(&late), "the late root ran");
        let (done, reports) = mpsc::channel();
        let later = DetachedGroup::new();
        notify(&runtime, &config(&domain), &later, 8, done, |_| {
            panic!("a root ran after the termination")
        });
        let (tag, outcome) = reports.recv_timeout(testing::hang()).unwrap();
        assert_eq!(tag, 8);
        let exited = matches!(outcome, Ok(EntryOutcome::Exited(3)));
        assert!(exited, "{outcome:?}");
        assert!(unstarted(&later), "the later root ran");
        runtime.halt();
    }
}
