//! Tests of the `std::file` kernels (D-981 to D-983, D-992 to D-997,
//! D-1065 to D-1067, D-1071): files read, written and closed through their
//! work functions and kernels, the error kinds, directories, scratch
//! directories and `cheby test`'s root, the standard streams' handles, a
//! cancellation during a job that waits for it (a write, an open, a close)
//! and during a refill that it abandons.
//!
//! Fibers run the work functions (`open_on`, `read_line_on`, `write_on`,
//! …) on Rust-level roots, and the handle tests call the kernels
//! themselves. A job is held at a point the test chooses by a hook keyed by
//! path and operation (`hooks`), and every wait for an event is bounded by
//! `testing::hang` (D-872). The refill against the next read is modelled
//! in `sched/blocking_models.rs` (model 5) before these tests rely on it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::hooks::{self, JobHook};
use super::*;
use crate::alloc::Domain;
use crate::entry::EntryOutcome;
use crate::fiber_ops::detached_tests::{
    self, close_scope, open_scope, release, spawn_into, yield_until,
};
use crate::fiber_ops::scope::{ScopeHandle, ScopeState};
use crate::generic::{self, Reached};
use crate::{rc, values};

/// The thread counts of every test that starts a runtime.
const THREADS: [usize; 2] = [1, 4];

/// The running fiber's context pointer, as kernels take it.
fn ctx_of(me: &ExecCtx) -> *mut ExecCtx {
    std::ptr::from_ref(me).cast_mut()
}

/// A scratch directory for this test, empty.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cheby-file-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// `dir/name` as a `String`.
fn at(dir: &Path, name: &str) -> String {
    dir.join(name).to_str().unwrap().to_owned()
}

/// The state of a borrowed `Scope` handle.
fn state_of(scope: Word) -> Arc<ScopeState> {
    // SAFETY: a live `Scope` handle, borrowed.
    Arc::clone(&unsafe { handles::payload::<ScopeHandle>(scope, HandleKind::Scope) }.0)
}

/// How a test fiber ends: unwinding for a cancellation, as a kernel's
/// raise would make it (D-670), or with `Nil`.
fn fiber_end(me: &ExecCtx, cancelled: bool) -> Word {
    let ctx = ctx_of(me);
    guarded(Some(ctx), || {
        if cancelled {
            raise_cancel(Some(ctx));
        }
        values::nil()
    })
}

/// Runs `body` as a root at `threads` workers and requires that it returns
/// 0; then stops the runtime.
fn run_root(threads: usize, body: impl FnOnce(&ExecCtx) -> u64 + Send + 'static) {
    let runtime = detached_tests::runtime(threads);
    let domain = Domain::new();
    let outcome = detached_tests::root_outcome(&runtime, &domain, body);
    runtime.shutdown();
    assert_eq!(outcome, EntryOutcome::Returned(0), "at {threads} workers");
}

/// What a test fiber's operation gave.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Got {
    /// The operation was cancelled: the work function returned
    /// `Err(Cancelled)`, and the fiber raised.
    Cancelled,
    /// It failed with this kind.
    Failed(i64),
    /// It succeeded, with these bytes (a line, or nothing).
    Done(Option<Vec<u8>>),
}

/// The slot a fiber records its [`Got`] in, with whether the hook's job
/// had gone on when the work function returned.
type Slot = Arc<Mutex<Option<(Got, bool)>>>;

/// What each case of the error test gave: its name and its kind.
type Kinds = Vec<(&'static str, Option<i64>)>;

/// A directory's names, a file's info and a directory's info.
type Listing = (Vec<String>, Info, Info);

/// [`Got`] of a work function's result.
fn got<T>(result: &Work<T>, data: impl FnOnce(&T) -> Option<Vec<u8>>) -> Got {
    match result {
        Err(Cancelled) => Got::Cancelled,
        Ok(Err(error)) => Got::Failed(error.kind),
        Ok(Ok(value)) => Got::Done(data(value)),
    }
}

/// A file state for the tests, shared by the fibers.
fn opened(me: &ExecCtx, path: &str, mode: i64) -> Arc<FileState> {
    Arc::new(open_on(me, path, mode).unwrap().unwrap())
}

/// The kind of a failed work function, or `None` when it succeeded.
fn kind<T: std::fmt::Debug>(result: Work<T>) -> Option<i64> {
    match result {
        Ok(Ok(_)) => None,
        Ok(Err(error)) => Some(error.kind),
        Err(Cancelled) => panic!("cancelled"),
    }
}

/// D-992, D-994, D-995, D-305: a file created, written in pieces and
/// closed reads back line by line and in blocks of bytes from one buffer;
/// `\r\n` and `\n` are removed, the last line may have no ending, and the
/// end keeps answering `None`; `append` writes after what the file holds;
/// `create` truncates.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn file_kernel_round_trip_through_one_buffer() {
    for threads in THREADS {
        let dir = scratch(&format!("round-trip-{threads}"));
        let path = at(&dir, "notes.txt");
        let seen: Arc<Mutex<Vec<Option<Vec<u8>>>>> = Arc::default();
        {
            let (path, seen) = (path.clone(), Arc::clone(&seen));
            run_root(threads, move |me| {
                let out = opened(me, &path, 1);
                for piece in ["one\r\n", "two\n", "three"] {
                    write_on(me, &out, piece.as_bytes().to_vec())
                        .unwrap()
                        .unwrap();
                }
                close_on(me, &out).unwrap().unwrap();
                let log = opened(me, &path, 2);
                write_on(me, &log, b"\nfour".to_vec()).unwrap().unwrap();
                close_on(me, &log).unwrap().unwrap();
                let source = opened(me, &path, 0);
                let mut lines = Vec::new();
                lines.push(read_line_on(me, &source).unwrap().unwrap());
                lines.push(read_bytes_on(me, &source, 2).unwrap().unwrap());
                lines.push(read_bytes_on(me, &source, 0).unwrap().unwrap());
                for _ in 0..4 {
                    lines.push(read_line_on(me, &source).unwrap().unwrap());
                }
                lines.push(read_bytes_on(me, &source, 9).unwrap().unwrap());
                *seen.lock() = lines;
                let again = opened(me, &path, 1);
                close_on(me, &again).unwrap().unwrap();
                0
            });
        }
        let expected: Vec<Option<Vec<u8>>> = vec![
            Some(b"one".to_vec()),
            Some(b"tw".to_vec()),
            Some(b"o".to_vec()),
            Some(b"".to_vec()),
            Some(b"three".to_vec()),
            Some(b"four".to_vec()),
            None,
            None,
        ];
        assert_eq!(*seen.lock(), expected, "{threads} workers");
        assert_eq!(fs::read(&path).unwrap(), b"", "create truncates");
        fs::remove_dir_all(&dir).unwrap();
    }
}

/// D-981, `file.md`: the failures map to their kinds on every platform:
/// a missing path 1, something that exists 4, a directory where a file was
/// expected 5 (opened for reading, created, removed as a file), a file
/// where a directory was expected 6 (listed, removed as a directory, a
/// path through it), a directory with entries 7, a closed handle 12, a
/// handle in the wrong mode and a path with U+0000 13.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn file_kernel_errors_map_to_their_kinds() {
    let dir = scratch("errors");
    let kinds: Arc<Mutex<Kinds>> = Arc::default();
    {
        let (dir, kinds) = (dir.clone(), Arc::clone(&kinds));
        run_root(2, move |me| {
            let missing = at(&dir, "no/such/file.txt");
            let plain = at(&dir, "plain.txt");
            fs::write(&plain, "plain").unwrap();
            let full = at(&dir, "full");
            fs::create_dir(&full).unwrap();
            fs::write(at(Path::new(&full), "entry.txt"), "").unwrap();
            let folder = dir.to_str().unwrap().to_owned();
            let through = format!("{plain}/below");
            let mut out = vec![
                ("open missing", kind(open_on(me, &missing, 0))),
                ("create missing", kind(open_on(me, &missing, 1))),
                ("append missing", kind(open_on(me, &missing, 2))),
                ("open a directory", kind(open_on(me, &folder, 0))),
                ("create a directory", kind(open_on(me, &folder, 1))),
                ("create_new existing", kind(open_on(me, &plain, 3))),
                ("open through a file", kind(open_on(me, &through, 0))),
                ("open with nul", kind(open_on(me, "a\0b", 0))),
                ("list missing", kind(list_dir_on(me, &missing))),
                ("list a file", kind(list_dir_on(me, &plain))),
                ("create_dir existing", kind(create_dir_on(me, &full))),
                ("create_dir missing", kind(create_dir_on(me, &missing))),
                ("create_dir_all a file", kind(create_dir_all_on(me, &plain))),
                (
                    "create_dir_all below a file",
                    kind(create_dir_all_on(me, &through)),
                ),
                ("remove_file missing", kind(remove_file_on(me, &missing))),
                ("remove_file a directory", kind(remove_file_on(me, &full))),
                ("remove_dir full", kind(remove_dir_on(me, &full))),
                ("remove_dir a file", kind(remove_dir_on(me, &plain))),
                ("remove_dir missing", kind(remove_dir_on(me, &missing))),
                (
                    "remove_dir_all missing",
                    kind(remove_dir_all_on(me, &missing)),
                ),
                ("remove_dir_all a file", kind(remove_dir_all_on(me, &plain))),
                ("rename missing", kind(rename_on(me, &missing, &plain))),
                ("info missing", kind(info_on(me, &missing))),
                ("info with nul", kind(info_on(me, "\0"))),
            ];
            let source = opened(me, &plain, 0);
            out.push((
                "write a reading handle",
                kind(write_on(me, &source, vec![1])),
            ));
            let sink = opened(me, &at(&dir, "out.txt"), 1);
            out.push(("read a writing handle", kind(read_line_on(me, &sink))));
            out.push((
                "read bytes of a writing handle",
                kind(read_bytes_on(me, &sink, 1)),
            ));
            out.push(("close", kind(close_on(me, &sink))));
            out.push(("close twice", kind(close_on(me, &sink))));
            out.push(("write closed", kind(write_on(me, &sink, vec![1]))));
            out.push(("read closed", kind(read_line_on(me, &sink))));
            *kinds.lock() = out;
            0
        });
    }
    let expected: Vec<(&str, Option<i64>)> = vec![
        ("open missing", Some(1)),
        ("create missing", Some(1)),
        ("append missing", Some(1)),
        ("open a directory", Some(5)),
        ("create a directory", Some(5)),
        ("create_new existing", Some(4)),
        ("open through a file", Some(6)),
        ("open with nul", Some(13)),
        ("list missing", Some(1)),
        ("list a file", Some(6)),
        ("create_dir existing", Some(4)),
        ("create_dir missing", Some(1)),
        ("create_dir_all a file", Some(4)),
        ("create_dir_all below a file", Some(4)),
        ("remove_file missing", Some(1)),
        ("remove_file a directory", Some(5)),
        ("remove_dir full", Some(7)),
        ("remove_dir a file", Some(6)),
        ("remove_dir missing", Some(1)),
        ("remove_dir_all missing", Some(1)),
        ("remove_dir_all a file", Some(6)),
        ("rename missing", Some(1)),
        ("info missing", Some(1)),
        ("info with nul", Some(13)),
        ("write a reading handle", Some(13)),
        ("read a writing handle", Some(13)),
        ("read bytes of a writing handle", Some(13)),
        ("close", None),
        ("close twice", Some(12)),
        ("write closed", Some(12)),
        ("read closed", Some(12)),
    ];
    assert_eq!(*kinds.lock(), expected);
    fs::remove_dir_all(&dir).unwrap();
}

/// D-981, D-484: a failure without a predicate is kind 0 with the system's
/// message, without Rust's ` (os error N)`; a kind with a predicate has no
/// message.
#[test]
#[cfg(unix)]
fn file_kernel_other_failures_keep_the_system_message() {
    let other = FileError::of_io(&io::Error::from_raw_os_error(libc::ENOSPC));
    assert_eq!(other.kind, 0);
    assert_ne!(other.message, "");
    assert!(!other.message.contains("os error"), "{}", other.message);
    assert_eq!(
        FileError::of_io(&io::Error::from_raw_os_error(libc::ENOENT)),
        FileError::fixed(1)
    );
    assert_eq!(
        FileError::of_io(&io::Error::from_raw_os_error(libc::ENOTEMPTY)),
        FileError::fixed(7)
    );
}

/// D-1066: `create_new` refuses a symbolic link, dangling or not, and
/// `remove_dir_all` never follows one: a link inside the tree is removed,
/// not its target, and a link at `path` is removed itself.
#[test]
#[cfg(unix)]
fn file_kernel_links_are_never_followed() {
    use std::os::unix::fs::symlink;
    let dir = scratch("links");
    let kept = dir.join("kept");
    fs::create_dir(&kept).unwrap();
    fs::write(kept.join("precious.txt"), "precious").unwrap();
    let tree = dir.join("tree");
    fs::create_dir_all(tree.join("a")).unwrap();
    symlink(&kept, tree.join("a/link")).unwrap();
    symlink(&kept, dir.join("top")).unwrap();
    symlink(dir.join("nowhere"), dir.join("dangling")).unwrap();
    let results: Arc<Mutex<Vec<Option<i64>>>> = Arc::default();
    {
        let (dir, results) = (dir.clone(), Arc::clone(&results));
        run_root(1, move |me| {
            *results.lock() = vec![
                kind(open_on(me, &at(&dir, "dangling"), 3)),
                kind(open_on(me, &at(&dir, "top"), 3)),
                kind(remove_dir_all_on(me, &at(&dir, "tree"))),
                kind(remove_dir_all_on(me, &at(&dir, "top"))),
            ];
            0
        });
    }
    assert_eq!(*results.lock(), [Some(4), Some(4), None, None]);
    assert!(!tree.exists(), "the tree was not removed");
    assert!(
        fs::symlink_metadata(dir.join("top")).is_err(),
        "the link was not removed"
    );
    assert_eq!(
        fs::read_to_string(kept.join("precious.txt")).unwrap(),
        "precious"
    );
    assert!(!dir.join("nowhere").exists(), "create_new followed a link");
    fs::remove_dir_all(&dir).unwrap();
}

/// D-997, D-996: `list_dir` sorts names by their bytes; `info` gives the
/// size in bytes, whether the path is a file or a directory, and a
/// modification time after 2020; a name that is not UTF-8 is kind 3.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn file_kernel_lists_sorted_names_and_reads_info() {
    let dir = scratch("listing");
    fs::write(dir.join("b.txt"), "héllo").unwrap();
    fs::write(dir.join("Zed.txt"), "").unwrap();
    fs::create_dir(dir.join("a")).unwrap();
    let seen: Arc<Mutex<Option<Listing>>> = Arc::default();
    {
        let (dir, seen) = (dir.clone(), Arc::clone(&seen));
        run_root(2, move |me| {
            let names = list_dir_on(me, dir.to_str().unwrap()).unwrap().unwrap();
            let file = info_on(me, &at(&dir, "b.txt")).unwrap().unwrap();
            let folder = info_on(me, &at(&dir, "a")).unwrap().unwrap();
            *seen.lock() = Some((names, file, folder));
            0
        });
    }
    let (names, file, folder) = seen.lock().clone().unwrap();
    assert_eq!(names, ["Zed.txt", "a", "b.txt"]);
    assert_eq!((file.size, file.kind), (6, 1));
    assert_eq!(folder.kind, 2);
    assert!(file.seconds > 1_577_836_800, "{file:?}");
    assert!((0..1_000_000_000).contains(&file.nanoseconds), "{file:?}");
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let odd = dir.join(std::ffi::OsStr::from_bytes(b"odd\xFF"));
        fs::write(odd, "").unwrap();
        let listed: Arc<Mutex<Option<i64>>> = Arc::default();
        {
            let (dir, shared) = (dir.clone(), Arc::clone(&listed));
            run_root(1, move |me| {
                *shared.lock() = kind(list_dir_on(me, dir.to_str().unwrap()));
                0
            });
        }
        assert_eq!(*listed.lock(), Some(3));
    }
    fs::remove_dir_all(&dir).unwrap();
}

/// D-983, D-714, D-692: `std::file` works in an entry without a scheduler,
/// as every entry on Windows is until M6: its jobs run inline, and a file
/// is created, written, closed, read back, listed and removed. No runtime
/// starts, so this test runs on Windows too.
#[test]
fn file_kernel_works_without_a_scheduler() {
    let dir = scratch("no-scheduler");
    let domain = Domain::new();
    let config = crate::entry::EntryConfig {
        domain: Arc::clone(&domain),
        ..crate::entry::EntryConfig::new(crate::entry::EntryMode::Test)
    };
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let task = {
        let (dir, seen) = (dir.clone(), Arc::clone(&seen));
        move || -> Word {
            let me = crate::entry::current_fiber().unwrap();
            assert!(me.sched.runtime().is_none(), "the entry runs a scheduler");
            let path = at(&dir, "inline.txt");
            let sink = open_on(me, &path, 1).unwrap().unwrap();
            write_on(me, &sink, b"first\nsecond\n".to_vec())
                .unwrap()
                .unwrap();
            close_on(me, &sink).unwrap().unwrap();
            let source = open_on(me, &path, 0).unwrap().unwrap();
            let mut lines = Vec::new();
            while let Some(line) = read_line_on(me, &source).unwrap().unwrap() {
                lines.push(String::from_utf8(line).unwrap());
            }
            lines.extend(list_dir_on(me, dir.to_str().unwrap()).unwrap().unwrap());
            remove_file_on(me, &path).unwrap().unwrap();
            lines.push(format!("{:?}", kind(info_on(me, &path))));
            *seen.lock() = lines;
            0
        }
    };
    // SAFETY: the task makes no Cheby value and uses no trampoline.
    let outcome = unsafe { crate::entry::run_task(&config, task) }.unwrap();
    assert!(matches!(outcome, EntryOutcome::Returned(0)), "{outcome:?}");
    assert_eq!(*seen.lock(), ["first", "second", "inline.txt", "Some(1)"]);
    fs::remove_dir_all(&dir).unwrap();
}

/// D-1021's form: a time before 1970 has negative seconds and nanoseconds
/// from 0 to `999_999_999`.
#[test]
fn file_kernel_times_before_1970_count_down() {
    use std::time::UNIX_EPOCH;
    let at = |offset: Duration, before: bool| {
        since_epoch(if before {
            UNIX_EPOCH - offset
        } else {
            UNIX_EPOCH + offset
        })
    };
    assert_eq!(at(Duration::new(5, 250), false), (5, 250));
    assert_eq!(at(Duration::new(1, 0), true), (-1, 0));
    assert_eq!(at(Duration::new(0, 500_000_000), true), (-1, 500_000_000));
    assert_eq!(at(Duration::new(2, 1), true), (-3, 999_999_999));
    let info = |(seconds, nanoseconds)| Info {
        size: 0,
        kind: 0,
        seconds,
        nanoseconds,
    };
    assert_eq!(
        info((-3, 999_999_999)).since_epoch_nanoseconds(),
        -2_000_000_001
    );
    assert_eq!(info((5, 250)).since_epoch_nanoseconds(), 5_000_000_250);
    assert_eq!(info((i64::MAX, 0)).since_epoch_nanoseconds(), i64::MAX);
}

/// D-995: a run of reads costs one refill per buffer, not one per line:
/// 2000 short lines, about 12 KiB, take one refill and the one that meets
/// the end.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn file_kernel_reads_refill_once_per_buffer() {
    let dir = scratch("buffered");
    let path = at(&dir, "lines.txt");
    let text = (0..2000).fold(String::new(), |mut text, index| {
        text.push_str("line ");
        text.push_str(&index.to_string());
        text.push('\n');
        text
    });
    fs::write(&path, &text).unwrap();
    let hook = JobHook::new(&path, "refill");
    hook.open();
    hooks::install(&hook);
    let count: Arc<Mutex<usize>> = Arc::default();
    {
        let (path, count) = (path.clone(), Arc::clone(&count));
        run_root(2, move |me| {
            let source = opened(me, &path, 0);
            let mut lines = 0_usize;
            while read_line_on(me, &source).unwrap().unwrap().is_some() {
                lines = lines.saturating_add(1);
            }
            *count.lock() = lines;
            0
        });
    }
    hooks::remove(&hook);
    assert_eq!(*count.lock(), 2000);
    assert_eq!(hook.passes.load(Ordering::SeqCst), 2);
    fs::remove_dir_all(&dir).unwrap();
}

/// Spawns into `scope` a fiber that runs `work` on `state`; records in
/// `slot` what it gave and whether `hook`'s job had gone on by then.
fn spawn_work(
    me: &ExecCtx,
    scope: Word,
    hook: &Arc<JobHook>,
    slot: &Slot,
    work: impl FnOnce(&ExecCtx) -> Got + Send + 'static,
) -> Word {
    let (hook, slot) = (Arc::clone(hook), Arc::clone(slot));
    spawn_into(me, scope, move |me| {
        let outcome = work(me);
        let passed = hook.passed.load(Ordering::SeqCst);
        let cancelled = outcome == Got::Cancelled;
        *slot.lock() = Some((outcome, passed));
        fiber_end(me, cancelled)
    })
}

/// Checks for 50 ms that the fiber of `slot` has not returned, sleeping
/// the calling worker between looks.
fn still_waiting(slot: &Slot, broken: &str) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(50) {
        assert!(slot.lock().is_none(), "{broken}");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Cancels `scope` from the root while `hook`'s job waits at its gate,
/// checks that the fiber has not returned meanwhile, then opens the gate
/// and closes the scope; returns what the fiber gave.
fn cancel_while_held(me: &ExecCtx, scope: Word, fiber: Word, hook: &JobHook, slot: &Slot) {
    yield_until(me, || hook.started.load(Ordering::SeqCst));
    cancel::cancel(&state_of(scope));
    still_waiting(slot, "the fiber returned before its job");
    hook.open();
    close_scope(me, scope);
    release(fiber);
}

/// D-670, D-983, D-1065: a fiber cancelled during a write on the pool
/// resumes only after the job returned, then raises, and the whole of
/// the data is in the file. At one and four workers.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn file_kernel_cancelled_write_waits_for_its_job() {
    for threads in THREADS {
        let dir = scratch(&format!("cancelled-write-{threads}"));
        let path = at(&dir, "out.bin");
        let data: Vec<u8> = (0..300_000_u32)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect();
        let hook = JobHook::new(&path, "write");
        hooks::install(&hook);
        let slot: Slot = Arc::default();
        {
            let (path, data, hook, slot) = (
                path.clone(),
                data.clone(),
                Arc::clone(&hook),
                Arc::clone(&slot),
            );
            run_root(threads, move |me| {
                let sink = opened(me, &path, 1);
                let scope = open_scope(me);
                let fiber = {
                    let sink = Arc::clone(&sink);
                    spawn_work(me, scope, &hook, &slot, move |me| {
                        got(&write_on(me, &sink, data), |()| None)
                    })
                };
                cancel_while_held(me, scope, fiber, &hook, &slot);
                0
            });
        }
        hooks::remove(&hook);
        assert_eq!(
            slot.lock().clone(),
            Some((Got::Cancelled, true)),
            "{threads} workers"
        );
        assert_eq!(fs::read(&path).unwrap(), data, "the write did not finish");
        fs::remove_dir_all(&dir).unwrap();
    }
}

/// D-670, D-983: a fiber cancelled during an open resumes only after the
/// job returned; the file was created, and the handle released.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn file_kernel_cancelled_write_waits_for_an_open() {
    for threads in THREADS {
        let dir = scratch(&format!("cancelled-open-{threads}"));
        let path = at(&dir, "new.txt");
        let hook = JobHook::new(&path, "open");
        hooks::install(&hook);
        let slot: Slot = Arc::default();
        {
            let (path, hook, slot) = (path.clone(), Arc::clone(&hook), Arc::clone(&slot));
            run_root(threads, move |me| {
                let scope = open_scope(me);
                let fiber = spawn_work(me, scope, &hook, &slot, move |me| {
                    got(&open_on(me, &path, 3), |_| None)
                });
                cancel_while_held(me, scope, fiber, &hook, &slot);
                0
            });
        }
        hooks::remove(&hook);
        assert_eq!(
            slot.lock().clone(),
            Some((Got::Cancelled, true)),
            "{threads} workers"
        );
        assert!(Path::new(&path).exists(), "the open did not happen");
        fs::remove_dir_all(&dir).unwrap();
    }
}

/// D-670, D-983, D-993: a fiber cancelled during a close resumes only
/// after the job returned, and the handle is closed.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn file_kernel_cancelled_write_waits_for_a_close() {
    for threads in THREADS {
        let dir = scratch(&format!("cancelled-close-{threads}"));
        let path = at(&dir, "out.txt");
        let hook = JobHook::new(&path, "close");
        hooks::install(&hook);
        let slot: Slot = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        {
            let (path, hook, slot, closed) = (
                path.clone(),
                Arc::clone(&hook),
                Arc::clone(&slot),
                Arc::clone(&closed),
            );
            run_root(threads, move |me| {
                let sink = opened(me, &path, 1);
                let scope = open_scope(me);
                let fiber = {
                    let sink = Arc::clone(&sink);
                    spawn_work(me, scope, &hook, &slot, move |me| {
                        got(&close_on(me, &sink), |()| None)
                    })
                };
                cancel_while_held(me, scope, fiber, &hook, &slot);
                closed.store(
                    kind(write_on(me, &sink, vec![1])) == Some(12),
                    Ordering::SeqCst,
                );
                0
            });
        }
        hooks::remove(&hook);
        assert_eq!(
            slot.lock().clone(),
            Some((Got::Cancelled, true)),
            "{threads} workers"
        );
        assert!(closed.load(Ordering::SeqCst), "the handle is still open");
        fs::remove_dir_all(&dir).unwrap();
    }
}

/// D-1065, D-995, D-1165: a fiber cancelled while it waits for a refill
/// of a file's buffer unwinds at once, while the refill is still held on
/// its pool thread. The next read, on another fiber, waits for that refill
/// and gets its line, and the read after it the next one: no line is lost
/// or read twice. At one and four workers.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn file_kernel_cancelled_refill_leaves_its_line_for_the_next_read() {
    for threads in THREADS {
        let dir = scratch(&format!("cancelled-refill-{threads}"));
        let path = at(&dir, "lines.txt");
        fs::write(&path, "first\nsecond\n").unwrap();
        let hook = JobHook::new(&path, "refill");
        hooks::install(&hook);
        let first: Slot = Arc::default();
        let next: Slot = Arc::default();
        let rest: Arc<Mutex<Vec<Got>>> = Arc::default();
        {
            let (path, hook, first, next, rest) = (
                path.clone(),
                Arc::clone(&hook),
                Arc::clone(&first),
                Arc::clone(&next),
                Arc::clone(&rest),
            );
            run_root(threads, move |me| {
                let source = opened(me, &path, 0);
                let read = |source: &Arc<FileState>| {
                    let source = Arc::clone(source);
                    move |me: &ExecCtx| got(&read_line_on(me, &source), Clone::clone)
                };
                // The first read waits for its refill, held at the gate,
                // and is cancelled: it returns at once.
                let scope = open_scope(me);
                let fiber = spawn_work(me, scope, &hook, &first, read(&source));
                yield_until(me, || hook.started.load(Ordering::SeqCst));
                cancel::cancel(&state_of(scope));
                close_scope(me, scope);
                release(fiber);
                assert!(
                    !hook.passed.load(Ordering::SeqCst),
                    "the cancelled read waited for its refill"
                );
                // The next read waits for the refill still running.
                let scope = open_scope(me);
                let fiber = spawn_work(me, scope, &hook, &next, read(&source));
                still_waiting(&next, "a read did not wait for the refill");
                hook.open();
                close_scope(me, scope);
                release(fiber);
                let mut after = Vec::new();
                for _ in 0..2 {
                    after.push(got(&read_line_on(me, &source), Clone::clone));
                }
                *rest.lock() = after;
                0
            });
        }
        hooks::remove(&hook);
        assert_eq!(
            first.lock().clone(),
            Some((Got::Cancelled, false)),
            "{threads} workers"
        );
        assert_eq!(
            next.lock().clone(),
            Some((Got::Done(Some(b"first".to_vec())), true)),
            "{threads} workers"
        );
        assert_eq!(
            *rest.lock(),
            [Got::Done(Some(b"second".to_vec())), Got::Done(None)],
            "{threads} workers"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}

/// D-1066, D-1067: a scratch directory is `cheby-<16 hex digits>`, mode
/// 0700, under the run's root when one is set and the host's temporary
/// directory otherwise; [`TestRoot`] is `cheby-test-<16 hex digits>`, and
/// it is removed with what it holds when the last guard goes.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn file_kernel_temp_dirs_live_under_the_run_root() {
    let root = TestRoot::create().unwrap();
    let shared = TestRoot::create().unwrap();
    let path = root.path().unwrap();
    assert_eq!(shared.path(), Some(path.clone()));
    let name = path.file_name().unwrap().to_str().unwrap().to_owned();
    let hex = name.strip_prefix("cheby-test-").unwrap();
    assert!(
        hex.len() == 16
            && hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "{name}"
    );
    let made: Arc<Mutex<Vec<String>>> = Arc::default();
    {
        let made = Arc::clone(&made);
        run_root(1, move |me| {
            for _ in 0..2 {
                made.lock().push(create_temp_dir_on(me).unwrap().unwrap());
            }
            0
        });
    }
    let made = made.lock().clone();
    assert_ne!(made[0], made[1]);
    for dir in &made {
        let dir = Path::new(dir);
        assert_eq!(dir.parent(), Some(path.as_path()), "{}", dir.display());
        let hex = dir
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("cheby-"))
            .unwrap();
        assert_eq!(hex.len(), 16);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let bits = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(bits, 0o700, "{}", dir.display());
        }
    }
    fs::write(Path::new(&made[0]).join("left.txt"), "left").unwrap();
    drop(shared);
    assert!(path.exists(), "the root went with the first guard");
    drop(root);
    assert!(!path.exists(), "the root was not removed");
}

/// D-982, D-233, D-1071: through the kernels, a `File` prints as `<File>`,
/// and `==` and hashing stop on it; a file opened and closed by the
/// kernels, and every standard stream's handle, is released without a
/// leak; a closed stream handle refuses a write while a new one writes,
/// and the wrong direction is kind 13. Releasing the stream handles closes
/// no descriptor: standard error still takes a write.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_stream_handles_print_refuse_equality_and_close_alone() {
    let dir = scratch("handles");
    let path = at(&dir, "handle.txt");
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let runtime = detached_tests::runtime(2);
    let domain = Domain::new();
    let outcome = {
        let seen = Arc::clone(&seen);
        detached_tests::root_outcome(&runtime, &domain, move |me| {
            let ctx = ctx_of(me);
            let name = values::string(&path);
            let text = values::string("");
            // SAFETY: the running fiber's context and a live string.
            let file = result_of(unsafe { cheby_file_sys_open(ctx, name, 1) }).unwrap();
            let streams = [0, 1, 2].map(|which| cheby_file_sys_std_stream(which));
            let mut lines = Vec::new();
            for word in [file, streams[0]] {
                // SAFETY: live handles.
                unsafe {
                    lines.push(generic::debug_string(word));
                    lines.push(format!("{:?}", generic::equal(word, word)));
                    lines.push(format!(
                        "{:?}",
                        generic::hash_with_seed(word, 7).map(|_| ())
                    ));
                }
            }
            let mut kind_of = |word: Word| {
                lines.push(format!(
                    "{:?}",
                    result_of(word).map_err(pair_of).map(|_| ())
                ));
            };
            // SAFETY: the running fiber's context, live handles and a live
            // string, all borrowed.
            unsafe {
                kind_of(cheby_file_sys_write(ctx, streams[0], text));
                kind_of(cheby_file_sys_read_line(ctx, streams[1]));
                kind_of(cheby_file_sys_read_bytes(ctx, streams[2], 4));
                kind_of(cheby_file_sys_close(ctx, streams[2]));
                kind_of(cheby_file_sys_write(ctx, streams[2], text));
                kind_of(cheby_file_sys_close(ctx, streams[2]));
                kind_of(cheby_file_sys_close(ctx, file));
                kind_of(cheby_file_sys_close(ctx, file));
            }
            let other = cheby_file_sys_std_stream(2);
            // SAFETY: as above.
            unsafe { kind_of(cheby_file_sys_write(ctx, other, text)) };
            for word in [name, text, file, other].into_iter().chain(streams) {
                // SAFETY: owned by the test.
                unsafe { rc::drop(word) };
            }
            lines.push(format!(
                "{:?}",
                crate::io::try_write_stderr("").map_err(|error| error.kind())
            ));
            *seen.lock() = lines;
            0
        })
    };
    runtime.shutdown();
    assert_eq!(outcome, EntryOutcome::Returned(0));
    let function = format!("{:?}", Err::<bool, _>(Reached::Function));
    let hashed = format!("{:?}", Err::<(), _>(Reached::Function));
    let ok = format!("{:?}", Ok::<(), (i64, String)>(()));
    let err = |kind: i64| format!("{:?}", Err::<(), _>((kind, String::new())));
    assert_eq!(
        *seen.lock(),
        [
            "<File>".to_owned(),
            function.clone(),
            hashed.clone(),
            "<File>".to_owned(),
            function,
            hashed,
            err(13),
            err(13),
            err(13),
            ok.clone(),
            err(12),
            err(12),
            ok.clone(),
            err(12),
            ok.clone(),
            format!("{:?}", Ok::<(), std::io::ErrorKind>(())),
        ]
    );
    assert_eq!(domain.live_objects(), 0, "objects leaked");
    fs::remove_dir_all(&dir).unwrap();
}

/// The payload of an owned `Result` word, `Ok` or `Err`, dup'd; the
/// result is dropped.
fn result_of(word: Word) -> Result<Word, Word> {
    // SAFETY: a live `Result` owned by the test.
    unsafe {
        let parts = values::read_result(word);
        let payload = match parts {
            Ok(value) | Err(value) => value,
        };
        rc::dup(payload);
        rc::drop(word);
        parts.map(|_| payload).map_err(|_| payload)
    }
}

/// The kind and message of an owned `(Int, String)` pair, dropped.
fn pair_of(word: Word) -> (i64, String) {
    // SAFETY: a live `(Int, String)` owned by the test.
    unsafe {
        let fields = values::read_tuple(word);
        let out = match fields.as_slice() {
            [kind, message] => (
                values::word_to_i64(*kind),
                values::str_of(*message).to_owned(),
            ),
            _ => (-1, String::new()),
        };
        rc::drop(word);
        out
    }
}

/// D-311: through the kernel, a line that is not UTF-8 is kind 3 and is
/// consumed, so the next read gets the line after it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn file_kernel_read_line_consumes_a_line_that_is_not_utf8() {
    let dir = scratch("utf8");
    let path = at(&dir, "mixed.txt");
    fs::write(&path, b"\xFF\nok").unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    {
        let seen = Arc::clone(&seen);
        run_root(1, move |me| {
            let ctx = ctx_of(me);
            let name = values::string(&path);
            // SAFETY: the running fiber's context and a live string.
            let file = result_of(unsafe { cheby_file_sys_open(ctx, name, 0) }).unwrap();
            let mut lines = Vec::new();
            for _ in 0..3 {
                // SAFETY: as above, and a live `File`.
                let line = result_of(unsafe { cheby_file_sys_read_line(ctx, file) });
                lines.push(match line {
                    // SAFETY: a live `Option<String>`, owned.
                    Ok(option) => unsafe {
                        let text =
                            values::read_option(option).map(|text| values::str_of(text).to_owned());
                        rc::drop(option);
                        format!("{text:?}")
                    },
                    Err(pair) => format!("{:?}", pair_of(pair)),
                });
            }
            for word in [name, file] {
                // SAFETY: owned by the test.
                unsafe { rc::drop(word) };
            }
            *seen.lock() = lines;
            0
        });
    }
    assert_eq!(
        *seen.lock(),
        [
            format!("{:?}", (3_i64, String::new())),
            format!("{:?}", Some("ok")),
            format!("{:?}", None::<&str>),
        ]
    );
    fs::remove_dir_all(&dir).unwrap();
}

/// The child side of `std_stream_stdin_mixes_with_io_read_line`: does
/// nothing unless `CHEBY_RT_TEST_FILE_STDIN` is set. On a runtime root it
/// reads standard input through a `stdin()` handle and `io`'s kernel in
/// turn, then as bytes, and writes what it read through a `stdout()`
/// handle and `io::print` in turn.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_stream_child() {
    if std::env::var_os("CHEBY_RT_TEST_FILE_STDIN").is_none() {
        return;
    }
    run_root(2, |me| {
        let ctx = ctx_of(me);
        let input = cheby_file_sys_std_stream(0);
        let output = cheby_file_sys_std_stream(1);
        let mut parts = Vec::new();
        for round in 0..4 {
            // SAFETY: the running fiber's context and a live `File`.
            let word = unsafe {
                if round % 2 == 0 {
                    cheby_file_sys_read_line(ctx, input)
                } else {
                    crate::io::cheby_io_sys_read_line(ctx)
                }
            };
            let option = result_of(word).unwrap();
            // SAFETY: a live `Option<String>`, owned.
            unsafe {
                let line = values::read_option(option).unwrap();
                parts.push(values::str_of(line).to_owned());
                rc::drop(option);
            }
        }
        // SAFETY: as above.
        let rest = result_of(unsafe { cheby_file_sys_read_bytes(ctx, input, 1 << 20) }).unwrap();
        // SAFETY: a live `Option<Bytes>`, owned.
        unsafe {
            let bytes = values::read_option(rest).unwrap();
            parts.push(String::from_utf8(values::bytes_of(bytes).to_vec()).unwrap());
            rc::drop(rest);
        }
        for (index, part) in parts.iter().enumerate() {
            let text = values::string(&format!("<{part}>"));
            // SAFETY: as above, and a live string.
            unsafe {
                if index % 2 == 0 {
                    let written = result_of(cheby_file_sys_write(ctx, output, text));
                    assert!(written.is_ok());
                } else {
                    crate::io::cheby_io_print(text);
                }
                rc::drop(text);
            }
        }
        let empty = values::string("");
        // SAFETY: a live string, borrowed, then the owned words.
        unsafe {
            crate::io::cheby_io_println(empty);
            for word in [empty, input, output] {
                rc::drop(word);
            }
        }
        0
    });
}

/// D-1071, D-823, D-365: `file::read_line(file::stdin())` and
/// `io::read_line()` take lines from one reader in input order, a `stdin()`
/// handle reads the rest as bytes, and output through a `stdout()` handle
/// and `io::print` keeps its order, in a child process (this test
/// binary's `std_stream_child`).
#[test]
#[cfg_attr(miri, ignore = "starts a process")]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn std_stream_stdin_mixes_with_io_read_line() {
    use std::process::{Command, Stdio};
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "file::tests::std_stream_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("CHEBY_RT_TEST_FILE_STDIN", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"one\ntwo\r\nthree\nfour\nfive\nsix")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}{stderr}");
    assert!(
        stdout.contains("<one><two><three><four><five\nsix>\n"),
        "{stdout}"
    );
}
