//! Tests of standard input's detached refill (D-1065, D-823, D-548,
//! D-1071): every test is named `stdin_detached_…`. Each one reads
//! through a private [`LineReader`] over a pipe or a scripted input, as
//! `stdin_fast_path_serves_only_when_no_read_waits` uses a private reader,
//! never through the process's standard input; reads run as Rust actions
//! on fibers of a runtime (the harness of `fiber_ops::detached_tests`),
//! and a teardown's cancellation stands for `fiber::timeout`'s. A test
//! that waits ends through the pipe, a gate or a channel, never by
//! hanging; time is asserted only as "not before the test wrote".

use std::collections::VecDeque;
use std::io::{self, PipeReader, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use super::{LineReader, read_bytes_on, read_line_on};
use crate::alloc::Domain;
use crate::ctx::ExecCtx;
use crate::entry::EntryOutcome;
use crate::fiber_ops::cancel::Cancelled;
use crate::fiber_ops::detached_tests::{config, detach, runtime, unwinding};
use crate::sched::{DetachedGroup, Runtime, testing};
use crate::values;

/// The thread counts the runtime tests run at.
const THREADS: [usize; 3] = [1, 2, 4];

/// A private reader and its count of waiting reads, for one test.
fn reader() -> (&'static Mutex<LineReader>, &'static AtomicUsize) {
    (
        Box::leak(Box::new(Mutex::new(LineReader::new()))),
        Box::leak(Box::new(AtomicUsize::new(0))),
    )
}

/// A pipe whose read end lives for the rest of the process.
fn pipe() -> (&'static PipeReader, io::PipeWriter) {
    let (read, write) = io::pipe().unwrap();
    (Box::leak(Box::new(read)), write)
}

/// A read's outcome as text: `line:<text>`, `bytes:<text>`, `end`,
/// `failed:<kind>` or `cancelled`.
fn describe(read: Result<io::Result<Option<Vec<u8>>>, Cancelled>, what: &str) -> String {
    match read {
        Ok(Ok(Some(data))) => format!("{what}:{}", String::from_utf8_lossy(&data)),
        Ok(Ok(None)) => "end".to_owned(),
        Ok(Err(error)) => format!("failed:{:?}", error.kind()),
        Err(Cancelled) => "cancelled".to_owned(),
    }
}

/// Waits on the host until `ready` holds, or panics after
/// [`testing::hang`].
fn wait_until(ready: impl Fn() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < testing::hang(),
            "the host waited too long"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// Starts a root on `runtime` that runs `reads` and sends what it saw.
fn reading_root(
    runtime: &Runtime,
    domain: &Arc<Domain>,
    reads: impl FnOnce(&ExecCtx) -> Vec<String> + Send + 'static,
) -> (mpsc::Receiver<EntryOutcome>, mpsc::Receiver<Vec<String>>) {
    let (sender, seen) = mpsc::channel();
    let root = testing::start_root_with(runtime, &config(domain), move |me| {
        sender.send(reads(me)).unwrap();
        values::nil()
    });
    (root, seen)
}

/// A detached fiber in `group` that reads one line through `lines` from
/// `input` and records what it got in `seen`, once it has it.
fn detached_read<I: Read + 'static>(
    runtime: &Runtime,
    domain: &Arc<Domain>,
    group: &Arc<DetachedGroup>,
    lines: (&'static Mutex<LineReader>, &'static AtomicUsize),
    input: impl FnOnce() -> I + Send + 'static,
    seen: Arc<Mutex<Option<String>>>,
) {
    let root = testing::start_root_in(runtime, &config(domain), group, move |me| {
        detach(me, move |me| {
            let read = read_line_on(me, lines.0, lines.1, input);
            *seen.lock() = Some(describe(read, "line"));
            if unwinding(me) { 0 } else { values::nil() }
        });
        values::nil()
    })
    .recv_timeout(testing::hang())
    .unwrap();
    assert!(matches!(root, EntryOutcome::Returned(_)), "{root:?}");
}

/// D-1065, D-548, D-823: a read on a pipe nobody writes is cancelled (a
/// teardown's walk) and returns `Cancelled` at once, while its refill
/// still waits for input; a read on another fiber then waits for that
/// refill, which holds the reader; the line the test writes afterwards
/// lands in the reader, and that read gets it, in order; the next line is
/// served from the reader on the fiber (D-823's fast path, no read waits
/// any more).
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn stdin_detached_a_cancelled_read_leaves_its_line_for_another_fiber() {
    for threads in THREADS {
        let runtime = runtime(threads);
        let domain = Domain::new();
        let group = DetachedGroup::new();
        let (lines, waiters) = reader();
        let (input, mut writer) = pipe();
        let first = Arc::new(Mutex::new(None));
        detached_read(
            &runtime,
            &domain,
            &group,
            (lines, waiters),
            move || input,
            Arc::clone(&first),
        );
        // The refill holds the reader, waiting for input.
        wait_until(|| group.blocked() == 1 && lines.is_locked());
        let report = runtime.teardown(&group);
        assert_eq!(
            (report.waited, report.skipped),
            (1, 0),
            "threads {threads}: the cancelled read waited, or was skipped"
        );
        assert_eq!(first.lock().as_deref(), Some("cancelled"));
        assert!(lines.is_locked(), "the abandoned refill ended");
        // Another fiber's read waits behind the refill still running.
        let (root, seen) = reading_root(&runtime, &domain, move |me| {
            let next = read_line_on(me, lines, waiters, move || input);
            let after = read_line_on(me, lines, waiters, move || input);
            vec![describe(next, "line"), describe(after, "line")]
        });
        assert!(
            seen.recv_timeout(Duration::from_millis(100)).is_err(),
            "threads {threads}: a read returned before the test wrote"
        );
        writer.write_all(b"first\nsecond\n").unwrap();
        writer.flush().unwrap();
        assert_eq!(
            seen.recv_timeout(testing::hang()).unwrap(),
            ["line:first", "line:second"],
            "threads {threads}"
        );
        let outcome = root.recv_timeout(testing::hang()).unwrap();
        assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
        assert_eq!(waiters.load(Ordering::SeqCst), 0, "a waiter was left");
        assert!(!lines.is_locked());
        runtime.shutdown();
        assert_eq!(domain.live_objects(), 0, "objects leaked");
    }
}

/// D-1071, D-298, D-823: bytes and lines come out of one reader in input
/// order, whether served from the reader on the fiber or refilled on the
/// pool: a line, then two bytes, then the rest of that line, then one byte
/// for a limit of 0, then the rest the reader holds up to the limit, then
/// the end of input for bytes and for a line.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn stdin_detached_bytes_and_lines_interleave_in_order() {
    let runtime = runtime(2);
    let domain = Domain::new();
    let (lines, waiters) = reader();
    let (input, mut writer) = pipe();
    writer.write_all(b"ab\ncdef\ngh\nxy").unwrap();
    drop(writer);
    let (root, seen) = reading_root(&runtime, &domain, move |me| {
        let open = move || input;
        vec![
            describe(read_line_on(me, lines, waiters, open), "line"),
            describe(read_bytes_on(me, lines, waiters, open, 2), "bytes"),
            describe(read_line_on(me, lines, waiters, open), "line"),
            describe(read_bytes_on(me, lines, waiters, open, 0), "bytes"),
            describe(read_bytes_on(me, lines, waiters, open, 10), "bytes"),
            describe(read_bytes_on(me, lines, waiters, open, 4), "bytes"),
            describe(read_line_on(me, lines, waiters, open), "line"),
        ]
    });
    assert_eq!(
        seen.recv_timeout(testing::hang()).unwrap(),
        [
            "line:ab",
            "bytes:cd",
            "line:ef",
            "bytes:g",
            "bytes:h\nxy",
            "end",
            "end"
        ]
    );
    let outcome = root.recv_timeout(testing::hang()).unwrap();
    assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
    assert_eq!(waiters.load(Ordering::SeqCst), 0);
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// A gate a scripted input's first read waits at.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
    entered: AtomicBool,
}

impl Gate {
    fn open(&self) {
        *self.open.lock() = true;
        self.opened.notify_all();
    }

    fn pass(&self) {
        self.entered.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let mut open = self.open.lock();
        while !*open && started.elapsed() < testing::hang() {
            self.opened.wait_for(&mut open, Duration::from_millis(50));
        }
    }
}

/// An input that waits at its gate, then gives its parts one read at a
/// time (a part's bytes, or its error), then the end of input.
#[derive(Clone)]
struct Scripted {
    gate: Arc<Gate>,
    parts: Arc<Mutex<VecDeque<Part>>>,
}

/// One read of a [`Scripted`] input: bytes, or an error.
type Part = Result<&'static [u8], io::ErrorKind>;

impl Read for Scripted {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.gate.pass();
        match self.parts.lock().pop_front() {
            Some(Ok(bytes)) => {
                let count = bytes.len().min(buf.len());
                buf[..count].copy_from_slice(&bytes[..count]);
                Ok(count)
            }
            Some(Err(kind)) => Err(io::Error::from(kind)),
            None => Ok(0),
        }
    }
}

/// D-1166, D-1065: a refill whose read was
/// abandoned meets a read error; the reader keeps it, with the bytes of
/// the unfinished line consumed (`hal`, as `BufRead::read_until`), and
/// the next read, on another fiber, gets the error; the read after it gets
/// the next line, whole.
#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn stdin_detached_a_failed_refill_keeps_its_error_for_the_next_read() {
    let runtime = runtime(2);
    let domain = Domain::new();
    let group = DetachedGroup::new();
    let (lines, waiters) = reader();
    let input = Scripted {
        gate: Arc::new(Gate::default()),
        parts: Arc::new(Mutex::new(VecDeque::from([
            Ok(&b"hal"[..]),
            Err(io::ErrorKind::BrokenPipe),
            Ok(&b"next\n"[..]),
        ]))),
    };
    let first = Arc::new(Mutex::new(None));
    let script = input.clone();
    detached_read(
        &runtime,
        &domain,
        &group,
        (lines, waiters),
        move || script,
        Arc::clone(&first),
    );
    wait_until(|| input.gate.entered.load(Ordering::SeqCst));
    let report = runtime.teardown(&group);
    assert_eq!((report.waited, report.skipped), (1, 0));
    assert_eq!(first.lock().as_deref(), Some("cancelled"));
    // The abandoned refill reads on: `hal`, then the error, which it keeps.
    input.gate.open();
    wait_until(|| waiters.load(Ordering::SeqCst) == 0);
    let script = input.clone();
    let (root, seen) = reading_root(&runtime, &domain, move |me| {
        let failed = read_line_on(me, lines, waiters, {
            let script = script.clone();
            move || script
        });
        let next = read_line_on(me, lines, waiters, move || script);
        vec![describe(failed, "line"), describe(next, "line")]
    });
    assert_eq!(
        seen.recv_timeout(testing::hang()).unwrap(),
        ["failed:BrokenPipe", "line:next"]
    );
    let outcome = root.recv_timeout(testing::hang()).unwrap();
    assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-1065: the reader keeps an end that no read took (a refill whose read
/// was abandoned) for the next read: the unfinished line at the end of
/// input, then `None`; bytes taken from the front keep what is known of the
/// rest, so a line after them is still found once.
#[test]
fn stdin_detached_reader_keeps_an_end_nobody_took() {
    let mut reader = LineReader::new();
    let mut input: &[u8] = b"whole\nhalf";
    reader.fill_line(&mut input);
    // One read took the whole input, and a whole line is buffered.
    assert_eq!(input, b"");
    assert_eq!(
        reader.take_bytes(3).unwrap().unwrap(),
        Some(b"who".to_vec())
    );
    assert_eq!(reader.take_line().unwrap().unwrap(), Some(b"le".to_vec()));
    assert!(reader.take_line().is_none(), "no whole line, no end yet");
    // The refill meets the end; its read was abandoned, so the end stays.
    reader.fill_line(&mut input);
    assert!(reader.line_ready());
    assert_eq!(reader.take_line().unwrap().unwrap(), Some(b"half".to_vec()));
    assert_eq!(reader.read_line(&mut input).unwrap(), None);
    // A refill for bytes reads once and keeps the end.
    reader.fill_some(&mut input);
    assert_eq!(reader.take_bytes(1).unwrap().unwrap(), None);
    assert!(reader.take_bytes(1).is_none());
}
