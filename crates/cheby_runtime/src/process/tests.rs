//! Tests of the `std::process` kernels of children (D-1907 to D-1919):
//! starting and waiting, signals, the pipes and their error kinds, a
//! cancelled wait, `finish`, the finalizer and the reaper, a `File` as
//! standard input, the checks, the captured output of `cheby test` and the
//! registry's kill.
//!
//! The children are `/bin/sh` and the tools every Unix system has. Fibers
//! run the work functions (`spawn::spawn`, `ChildState::wait`, …) on
//! Rust-level roots, and one test calls the kernels with Cheby words. Every
//! wait is bounded by `testing::hang` (D-872).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::child::{ChildState, Status};
use super::error::ProcError;
use super::registry;
use super::spawn::{self, Slot, Spec};
use super::{
    cheby_process_sys_finish, cheby_process_sys_output, cheby_process_sys_start,
    cheby_process_sys_stdin, cheby_process_sys_wait,
};
use crate::abi::Word;
use crate::alloc::Domain;
use crate::capture::{Capture, Chunk, Stream};
use crate::ctx::ExecCtx;
use crate::entry::EntryOutcome;
use crate::fiber_ops::cancel::{self, Cancelled};
use crate::fiber_ops::detached_tests::{
    self, close_scope, open_scope, release, spawn_into, yield_until,
};
use crate::fiber_ops::scope::{ScopeHandle, ScopeState};
use crate::file::{ChildStream, FileState};
use crate::handles::{self, HandleKind};
use crate::io::KIND_BROKEN_PIPE;
use crate::sched::Shared;
use crate::sched::testing;
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_cancel;
use crate::{generic, list, rc, values};

/// The thread counts of every test that starts a runtime.
const THREADS: [usize; 2] = [1, 4];

/// The running fiber's runtime.
fn rt(me: &ExecCtx) -> Arc<Shared> {
    Arc::clone(me.sched.runtime().unwrap())
}

/// The running fiber's context pointer, as kernels take it.
fn ctx_of(me: &ExecCtx) -> *mut ExecCtx {
    std::ptr::from_ref(me).cast_mut()
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

/// A description of `sh -c script` with `slots`.
fn sh(script: &str, slots: [Slot; 3]) -> Spec {
    Spec {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), script.to_owned()],
        dir: None,
        clear_env: false,
        env: Vec::new(),
        slots,
        capture: false,
        temp_parent: std::env::temp_dir(),
        owner: 0,
    }
}

/// Every stream discarded but those `piped` names (0 to 2).
fn slots(piped: &[usize]) -> [Slot; 3] {
    let slot = |index| {
        if piped.contains(&index) {
            Slot::Piped
        } else {
            Slot::Discard
        }
    };
    [slot(0), slot(1), slot(2)]
}

/// The started child of `spec`, its descriptors on `me`'s poller.
fn start(me: &ExecCtx, spec: &Spec, capture: Option<Arc<Capture>>) -> Arc<ChildState> {
    let spawned = spawn::spawn(spec).unwrap();
    Arc::new(ChildState::new(&rt(me), spawned, capture).unwrap())
}

/// The status of a wait that must succeed.
fn waited(me: &ExecCtx, child: &ChildState) -> Status {
    child.wait(me).unwrap().unwrap()
}

/// The kind of a failed operation.
fn kind_of<T: std::fmt::Debug>(result: Result<Result<T, ProcError>, Cancelled>) -> i64 {
    result.unwrap().unwrap_err().kind
}

/// Whether the process `pid` is gone: reaped, so even a zombie is not
/// left.
fn reaped(pid: libc::pid_t) -> bool {
    // SAFETY: `kill` with signal 0 only checks for the process.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    !alive && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
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
        thread::sleep(Duration::from_millis(2));
    }
}

/// D-1913, D-1910: an exit status and a signal; a second wait returns the
/// same status at once; a reaped child leaves the registry.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_wait_gives_the_exit_status_or_the_signal() {
    for threads in THREADS {
        run_root(threads, |me| {
            let child = start(me, &sh("exit 3", slots(&[])), None);
            let pid = child.process().pid();
            assert!(registry::is_open(pid));
            assert_eq!(waited(me, &child), Status::Exited(3));
            assert_eq!(waited(me, &child), Status::Exited(3));
            assert!(!registry::is_open(pid));
            assert!(reaped(pid));
            // Signalling a reaped child is `Ok` and sends nothing.
            assert_eq!(child.signal(libc::SIGTERM), Ok(()));
            let child = start(me, &sh("kill -9 $$", slots(&[])), None);
            assert_eq!(waited(me, &child), Status::Signaled(libc::SIGKILL));
            assert_eq!(Status::Signaled(9).pair(), (1, 9));
            assert_eq!(Status::Exited(3).pair(), (0, 3));
            0
        });
    }
}

/// D-1913: `terminate` reaches a running child.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_signals_reach_a_running_child() {
    run_root(2, |me| {
        for (signal, expected) in [(libc::SIGTERM, libc::SIGTERM), (libc::SIGINT, libc::SIGINT)] {
            let child = start(me, &sh("exec sleep 30", slots(&[])), None);
            assert_eq!(child.signal(signal), Ok(()));
            assert_eq!(waited(me, &child), Status::Signaled(expected));
        }
        0
    });
}

/// D-1914, D-305: lines without `\n` or `\r\n`, a read of bytes capped at
/// its limit, the rest to the end, then the end; standard error on its own
/// pipe.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_reads_lines_bytes_and_the_rest() {
    for threads in THREADS {
        run_root(threads, |me| {
            let script = "printf 'one\\r\\ntwo\\nthree and more'; printf oops >&2";
            let child = start(me, &sh(script, slots(&[1, 2])), None);
            let line = |child: &ChildState| child.stdout.read_line(me).unwrap().unwrap();
            assert_eq!(line(&child), Some(b"one".to_vec()));
            assert_eq!(line(&child), Some(b"two".to_vec()));
            assert_eq!(
                child.stdout.read_bytes(me, 5).unwrap().unwrap(),
                Some(b"three".to_vec())
            );
            assert_eq!(
                child.stdout.read_all(me).unwrap().unwrap(),
                b" and more".to_vec()
            );
            assert_eq!(line(&child), None);
            assert_eq!(child.stdout.read_bytes(me, 0).unwrap().unwrap(), None);
            assert_eq!(
                child.stderr.read_all(me).unwrap().unwrap(),
                b"oops".to_vec()
            );
            assert_eq!(waited(me, &child), Status::Exited(0));
            0
        });
    }
}

/// D-1914, D-1915: a write reaches the child, `close` gives it its end,
/// a second close and a later write are kind 12; a write to a child that
/// stopped reading is kind 14.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_writes_close_and_meet_a_broken_pipe() {
    for threads in THREADS {
        run_root(threads, |me| {
            let child = start(me, &sh("exec cat", slots(&[0, 1])), None);
            child.stdin.write(me, b"hello\n").unwrap().unwrap();
            // More than a pipe holds, so the write waits for `cat`.
            let big = vec![b'x'; 1 << 20];
            let reader = Arc::clone(&child.stdout);
            let total = Arc::new(Mutex::new(0_usize));
            let scope = open_scope(me);
            let fiber = {
                let total = Arc::clone(&total);
                spawn_into(me, scope, move |me| {
                    let data = reader.read_all(me).unwrap().unwrap();
                    *total.lock() = data.len();
                    values::nil()
                })
            };
            child.stdin.write(me, &big).unwrap().unwrap();
            child.stdin.close().unwrap();
            assert_eq!(child.stdin.close().unwrap_err().kind, 12);
            assert_eq!(kind_of(child.stdin.write(me, b"late")), 12);
            assert!(close_scope(me, scope));
            release(fiber);
            assert_eq!(*total.lock(), 6 + (1 << 20));
            assert_eq!(waited(me, &child), Status::Exited(0));
            // A child that never reads: its exit closes the pipe's end.
            let child = start(me, &sh("exit 0", slots(&[0])), None);
            assert_eq!(waited(me, &child), Status::Exited(0));
            assert_eq!(kind_of(child.stdin.write(me, &big)), KIND_BROKEN_PIPE);
            0
        });
    }
}

/// D-1916: the child gets the default action of SIGPIPE, which the
/// program ignores, and descriptors 0 to 2 only, also when the program
/// holds one without close-on-exec.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_children_get_default_sigpipe_and_three_descriptors() {
    run_root(2, |me| {
        // A writer whose reader is gone ends by SIGPIPE.
        let child = start(me, &sh("read _ ; echo late", slots(&[0, 1])), None);
        child.stdout.finish();
        child.stdin.write(me, b"go\n").unwrap().unwrap();
        assert_eq!(waited(me, &child), Status::Signaled(libc::SIGPIPE));
        // SAFETY: `dup` makes a descriptor without close-on-exec.
        let open = unsafe { libc::dup(1) };
        assert!(open > 2);
        let child = start(me, &sh("echo /dev/fd/*", slots(&[1])), None);
        let listed = String::from_utf8(child.stdout.read_all(me).unwrap().unwrap()).unwrap();
        assert_eq!(waited(me, &child), Status::Exited(0));
        // SAFETY: the descriptor `dup` made, closed once.
        unsafe { libc::close(open) };
        let fds: Vec<i32> = listed
            .split_whitespace()
            .map(|fd| fd.trim_start_matches("/dev/fd/").parse().unwrap())
            .collect();
        // The glob's own directory descriptor is 3; the program's
        // descriptor without close-on-exec stays out (D-1923 on Linux).
        assert!(
            fds.iter().all(|&fd| fd <= 3),
            "{fds:?}, the program's {open}"
        );
        0
    });
}

/// D-1914: a stream that is not piped is kind 13 for every operation.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_streams_that_are_not_piped_are_invalid_input() {
    run_root(1, |me| {
        let child = start(me, &sh("exit 0", slots(&[])), None);
        assert_eq!(kind_of(child.stdout.read_line(me)), 13);
        assert_eq!(kind_of(child.stderr.read_bytes(me, 4)), 13);
        assert_eq!(kind_of(child.stdout.read_all(me)), 13);
        assert_eq!(kind_of(child.stdin.write(me, b"x")), 13);
        assert_eq!(child.stdin.close().unwrap_err().kind, 13);
        assert_eq!(waited(me, &child), Status::Exited(0));
        0
    });
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

/// D-1913: a wait cancelled while it waits on the poller takes nothing:
/// the child runs on, and `finish` then kills it, waits and reaps it, and
/// every later operation on it or its pipes is kind 12 (D-1912).
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_cancelled_wait_takes_nothing_and_finish_kills() {
    for threads in THREADS {
        run_root(threads, |me| {
            let child = start(me, &sh("exec sleep 30", slots(&[0, 1])), None);
            let got: Arc<Mutex<Option<bool>>> = Arc::default();
            let scope = open_scope(me);
            let fiber = {
                let (child, got) = (Arc::clone(&child), Arc::clone(&got));
                spawn_into(me, scope, move |me| {
                    let cancelled = matches!(child.wait(me), Err(Cancelled));
                    *got.lock() = Some(cancelled);
                    fiber_end(me, cancelled)
                })
            };
            yield_until(me, || child.exit_armed());
            cancel::cancel(&state_of(scope));
            close_scope(me, scope);
            release(fiber);
            assert_eq!(*got.lock(), Some(true));
            assert_eq!(child.process().status(), None);
            assert!(registry::is_open(child.process().pid()));
            assert_eq!(child.signal(0), Ok(()));
            child.finish(me).unwrap();
            assert_eq!(
                child.process().status(),
                Some(Status::Signaled(libc::SIGKILL))
            );
            assert!(reaped(child.process().pid()));
            assert_eq!(kind_of(child.wait(me)), 12);
            assert_eq!(child.signal(libc::SIGTERM).unwrap_err().kind, 12);
            assert_eq!(kind_of(child.stdout.read_line(me)), 12);
            assert_eq!(kind_of(child.stdin.write(me, b"x")), 12);
            assert_eq!(child.stdin.close().unwrap_err().kind, 12);
            // A second finish does nothing.
            child.finish(me).unwrap();
            0
        });
    }
}

/// D-1914: `close` wakes a fiber parked in a write, which a full pipe
/// holds, with kind 12; its prefix may have been written (D-1000).
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_close_wakes_a_parked_write() {
    for threads in THREADS {
        run_root(threads, |me| {
            let child = start(me, &sh("exec sleep 30", slots(&[0])), None);
            let got: Arc<Mutex<Option<i64>>> = Arc::default();
            let scope = open_scope(me);
            let fiber = {
                let (writer, got) = (Arc::clone(&child.stdin), Arc::clone(&got));
                spawn_into(me, scope, move |me| {
                    *got.lock() = Some(kind_of(writer.write(me, &vec![b'x'; 1 << 20])));
                    values::nil()
                })
            };
            yield_until(me, || {
                child
                    .stdin
                    .piped()
                    .is_some_and(|pipe| pipe.io().armed(crate::sched::io::Direction::Write))
            });
            child.stdin.close().unwrap();
            assert!(close_scope(me, scope));
            release(fiber);
            assert_eq!(*got.lock(), Some(12));
            child.finish(me).unwrap();
            0
        });
    }
}

/// D-1919: dropping a child's state, as its finalizer does, kills a child
/// still running and the reaper reaps it: no zombie is left.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_finalizer_kills_and_the_reaper_reaps() {
    let pid = Arc::new(Mutex::new(0));
    {
        let pid = Arc::clone(&pid);
        run_root(1, move |me| {
            registry::start_reaper();
            let child = start(me, &sh("exec sleep 30", slots(&[0, 1, 2])), None);
            *pid.lock() = child.process().pid();
            drop(child);
            0
        });
    }
    let pid = *pid.lock();
    assert!(!registry::is_open(pid));
    wait_until(|| reaped(pid));
}

/// D-1917: the registry's kill reaches every open child, and waits for
/// none.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_kill_open_children_kills_every_open_child() {
    run_root(2, |me| {
        let first = start(me, &sh("exec sleep 30", slots(&[])), None);
        let second = start(me, &sh("exec sleep 30", slots(&[0])), None);
        super::kill_open_children();
        assert_eq!(waited(me, &first), Status::Signaled(libc::SIGKILL));
        assert_eq!(waited(me, &second), Status::Signaled(libc::SIGKILL));
        0
    });
}

/// A new scratch directory under the host's temporary directory.
fn scratch(name: &str) -> PathBuf {
    let bits = getrandom::u64().unwrap();
    let dir = std::env::temp_dir().join(format!("cheby-process-{name}-{bits:016x}"));
    std::fs::create_dir(&dir).unwrap();
    dir
}

/// Opens `path` for reading as `file::open` does.
fn opened(me: &ExecCtx, path: &Path) -> FileState {
    crate::file::open_on(me, path.to_str().unwrap(), 0)
        .unwrap()
        .unwrap()
}

/// D-1909: a `File` that read a header line ahead gives the child the rest:
/// the descriptor moves back over the read-ahead. One that cannot seek and
/// holds read-ahead is kind 13; a closed one is kind 12.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_a_file_as_standard_input_gives_the_child_the_rest() {
    let dir = scratch("stdin");
    let data = dir.join("data.txt");
    std::fs::write(&data, "header\nrest one\nrest two\n").unwrap();
    let fifo = dir.join("fifo");
    let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    {
        run_root(2, move |me| {
            let file = opened(me, &data);
            let header = crate::file::read_line_on(me, &file).unwrap().unwrap();
            assert_eq!(header, Some(b"header".to_vec()));
            let stream = file.child_stream().unwrap();
            let spec = sh("exec cat", [Slot::File(stream), Slot::Piped, Slot::Discard]);
            let child = start(me, &spec, None);
            assert_eq!(
                child.stdout.read_all(me).unwrap().unwrap(),
                b"rest one\nrest two\n".to_vec()
            );
            assert_eq!(waited(me, &child), Status::Exited(0));
            // The handle shares the position: at the end now.
            assert_eq!(crate::file::read_line_on(me, &file).unwrap().unwrap(), None);
            // A pipe cannot seek.
            let writer = {
                let fifo = fifo.clone();
                thread::spawn(move || {
                    let mut end = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
                    end.write_all(b"first\nsecond\n").unwrap();
                    end
                })
            };
            let piped = opened(me, &fifo);
            let first = crate::file::read_line_on(me, &piped).unwrap().unwrap();
            assert_eq!(first, Some(b"first".to_vec()));
            let spec = sh(
                "exec cat",
                [
                    Slot::File(piped.child_stream().unwrap()),
                    Slot::Discard,
                    Slot::Discard,
                ],
            );
            assert_eq!(spawn::spawn(&spec).unwrap_err().kind, 13);
            drop(writer.join().unwrap());
            // A closed file.
            crate::file::close_on(me, &file).unwrap().unwrap();
            assert_eq!(file.child_stream().unwrap_err().kind, 12);
            0
        });
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// D-1908, D-1909: the checks made before anything starts, and the errors
/// of a start.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_checks_and_start_errors_have_their_kinds() {
    let check = |edit: &dyn Fn(&mut Spec)| {
        let mut spec = sh("exit 0", slots(&[]));
        edit(&mut spec);
        spec.check()
    };
    assert_eq!(check(&|_| {}), Ok(()));
    let invalid: [&dyn Fn(&mut Spec); 8] = [
        &|spec| spec.program = String::new(),
        &|spec| spec.program = "s\0h".to_owned(),
        &|spec| spec.args.push("a\0b".to_owned()),
        &|spec| spec.dir = Some("/tmp\0".to_owned()),
        &|spec| spec.env.push(("NAME\0".to_owned(), None)),
        &|spec| {
            spec.env
                .push(("NAME".to_owned(), Some("va\0lue".to_owned())));
        },
        &|spec| spec.env.push((String::new(), Some("value".to_owned()))),
        &|spec| spec.env.push(("A=B".to_owned(), Some("value".to_owned()))),
    ];
    for edit in invalid {
        assert_eq!(check(edit), Err(ProcError::invalid_input()));
    }
    for (code, index) in [(-1, 0), (5, 2), (4, 0), (4, 1), (3, 1)] {
        assert_eq!(
            Slot::of(code, None, index).unwrap_err().kind,
            13,
            "{code} {index}"
        );
    }
    assert!(matches!(Slot::of(4, None, 2), Ok(Slot::SameAsStdout)));
    assert!(matches!(
        Slot::of(3, Some(ChildStream::Std(1)), 1),
        Ok(Slot::File(ChildStream::Std(1)))
    ));
    let dir = scratch("errors");
    let plain = dir.join("plain");
    std::fs::write(&plain, "#!/bin/sh\n").unwrap();
    let start_kind = |program: &str, dir: Option<&Path>| {
        let mut spec = sh("exit 0", slots(&[]));
        spec.program = program.to_owned();
        spec.args.clear();
        spec.dir = dir.map(|dir| dir.to_str().unwrap().to_owned());
        spawn::spawn(&spec).unwrap_err().kind
    };
    assert_eq!(start_kind("cheby-no-such-program", None), 1);
    assert_eq!(start_kind(plain.to_str().unwrap(), None), 2);
    assert_eq!(start_kind("sh", Some(&dir.join("missing"))), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// D-1908: the environment's edits apply in order, a relative program with
/// `/` is the program's own working directory's, and `dir` sets the
/// child's.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_environment_program_and_directory() {
    let dir = scratch("env");
    let work = dir.join("work");
    std::fs::create_dir(&work).unwrap();
    let dir_text = std::fs::canonicalize(&work)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    run_root(2, move |me| {
        let mut spec = sh(
            "printf '%s|%s|%s|' \"$CHEBY_A\" \"$CHEBY_B\" \"$HOME\"; pwd -P",
            slots(&[1]),
        );
        spec.clear_env = true;
        spec.env = vec![
            ("CHEBY_A".to_owned(), Some("one".to_owned())),
            ("CHEBY_B".to_owned(), Some("two".to_owned())),
            ("CHEBY_A".to_owned(), None),
            ("CHEBY_A".to_owned(), Some("three".to_owned())),
            ("PATH".to_owned(), Some("/usr/bin:/bin".to_owned())),
        ];
        spec.dir = Some(dir_text.clone());
        let child = start(me, &spec, None);
        let out = child.stdout.read_all(me).unwrap().unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("three|two||{dir_text}\n")
        );
        assert_eq!(waited(me, &child), Status::Exited(0));
        0
    });
    std::fs::remove_dir_all(&dir).unwrap();
}

/// D-1918: the capture's temporary file is unlinked at once, under the
/// given root.
#[test]
fn process_capture_files_are_unlinked_under_the_root() {
    let dir = scratch("capture");
    let mut file = spawn::capture_file(&dir).unwrap();
    file.write_all(b"kept").unwrap();
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    drop(file);
    std::fs::remove_dir(&dir).unwrap();
}

/// D-1918: under `cheby test` a child's inherited standard output and
/// error join the test's capture when it is reaped, standard output's bytes
/// first; a merged standard error goes with standard output.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_captured_output_joins_the_capture_when_reaped() {
    let dir = scratch("captured");
    {
        let dir = dir.clone();
        run_root(2, move |me| {
            let script = "printf err >&2; printf out";
            let capture = Capture::new();
            let mut spec = sh(script, [Slot::Discard, Slot::Inherit, Slot::Inherit]);
            spec.capture = true;
            spec.temp_parent = dir.clone();
            let child = start(me, &spec, Some(Arc::clone(&capture)));
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
            assert_eq!(waited(me, &child), Status::Exited(0));
            assert_eq!(
                capture.take(),
                [
                    Chunk::Write(Stream::Stdout, b"out".to_vec()),
                    Chunk::Write(Stream::Stderr, b"err".to_vec()),
                ]
            );
            // Merged, and joined by `finish`.
            spec.slots = [Slot::Discard, Slot::Inherit, Slot::SameAsStdout];
            let child = start(me, &spec, Some(Arc::clone(&capture)));
            let pid = child.process().pid();
            wait_until(|| {
                // SAFETY: `waitid` with `WNOWAIT` only looks at the child.
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let id = libc::id_t::try_from(pid).unwrap();
                // SAFETY: `info` is a live slot the call writes.
                unsafe {
                    libc::waitid(
                        libc::P_PID,
                        id,
                        &raw mut info,
                        libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                    ) == 0
                        && info.si_pid() == pid
                }
            });
            child.finish(me).unwrap();
            assert_eq!(
                capture.take(),
                [Chunk::Write(Stream::Stdout, b"errout".to_vec())]
            );
            0
        });
    }
    std::fs::remove_dir(&dir).unwrap();
}

/// The payload of a `Result` word that must be `Ok`.
fn ok(word: Word) -> Word {
    // SAFETY: a live `Result`, borrowed.
    unsafe { values::read_result(word) }.unwrap()
}

/// The kernels with Cheby words (D-1919): a start, the handles' debug
/// text, a wait's pair, `finish`, and an empty program's kind 13.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_kernels_start_wait_and_finish() {
    run_root(2, |me| {
        let ctx = ctx_of(me);
        let call = |program: &str| {
            let program = values::string(program);
            let args = list::from_iter([values::string("-c"), values::string("exit 7")]);
            let (dir, env) = (values::none(), list::from_iter([]));
            let none = values::none();
            // SAFETY: the running fiber's context and live words, borrowed.
            let started = unsafe {
                cheby_process_sys_start(
                    ctx, program, args, dir, false, env, 1, none, 1, none, 1, none,
                )
            };
            for word in [program, args, dir, env, none] {
                // SAFETY: the references this closure owns.
                unsafe { rc::drop(word) };
            }
            started
        };
        let started = call("sh");
        let child = ok(started);
        // SAFETY: live words, borrowed.
        let text = |word| unsafe { generic::debug_string(word) };
        assert_eq!(text(child), "<Child>");
        // SAFETY: the running fiber's context and a live `Child`.
        let (stdin, stdout) = unsafe {
            (
                cheby_process_sys_stdin(ctx, child),
                cheby_process_sys_output(ctx, child, 1),
            )
        };
        assert_eq!(
            (text(stdin), text(stdout)),
            ("<Writer>".to_owned(), "<Reader>".to_owned())
        );
        // SAFETY: as above.
        let status = unsafe { cheby_process_sys_wait(ctx, child) };
        // SAFETY: a live tuple, borrowed.
        let pair = unsafe { values::read_tuple(ok(status)) };
        // SAFETY: tagged integers.
        let pair: Vec<i64> = pair
            .into_iter()
            .map(|word| unsafe { values::word_to_i64(word) })
            .collect();
        assert_eq!(pair, [0, 7]);
        // SAFETY: as above.
        unsafe { cheby_process_sys_finish(ctx, child) };
        for word in [status, stdin, stdout, started] {
            // SAFETY: the references this test owns.
            unsafe { rc::drop(word) };
        }
        let failed = call("");
        // SAFETY: a live `Result`, borrowed.
        let error = unsafe { values::read_result(failed) }.unwrap_err();
        // SAFETY: a live pair, borrowed.
        let parts = unsafe { values::read_tuple(error) };
        // SAFETY: a tagged integer.
        assert_eq!(unsafe { values::word_to_i64(parts[0]) }, 13);
        // SAFETY: the reference this test owns.
        unsafe { rc::drop(failed) };
        0
    });
}

/// D-1921: a child that inherits standard input first gets the bytes the
/// program's reader read ahead, where descriptor 0 can seek; from a pipe
/// they stay with the program's reader.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_inherited_stdin_gets_the_read_ahead_back() {
    let dir = scratch("own-stdin");
    let data = dir.join("input.txt");
    std::fs::write(&data, "first\nsecond\nthird\n").unwrap();
    let input = std::fs::File::open(&data).unwrap();
    let raw = std::os::fd::AsRawFd::as_raw_fd(&input);
    // SAFETY: `dup2` onto descriptor 0 of this test's own process.
    let made = unsafe { libc::dup2(raw, 0) };
    assert_eq!(made, 0);
    let first = crate::io::stdin_reader()
        .lock()
        .read_line(&mut std::io::stdin())
        .unwrap();
    assert_eq!(first, Some(b"first".to_vec()));
    run_root(1, |me| {
        let child = start(
            me,
            &sh("exec cat", [Slot::Inherit, Slot::Piped, Slot::Discard]),
            None,
        );
        assert_eq!(
            child.stdout.read_all(me).unwrap().unwrap(),
            b"second\nthird\n".to_vec()
        );
        assert_eq!(waited(me, &child), Status::Exited(0));
        0
    });
    assert_eq!(crate::io::stdin_reader().lock().buffered_len(), 0);
    // A pipe cannot seek: the bytes stay with the program.
    let mut fds = [0; 2];
    // SAFETY: `fds` holds the two descriptors the call writes.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let [read, write] = fds;
    // SAFETY: the write end this test made, written and closed once; then
    // the read end onto descriptor 0.
    unsafe {
        assert_eq!(libc::write(write, b"a\nb\n".as_ptr().cast(), 4), 4);
        libc::close(write);
        assert_eq!(libc::dup2(read, 0), 0);
        libc::close(read);
    }
    let a = crate::io::stdin_reader()
        .lock()
        .read_line(&mut std::io::stdin())
        .unwrap();
    assert_eq!(a, Some(b"a".to_vec()));
    run_root(1, |me| {
        let child = start(
            me,
            &sh("exec cat", [Slot::Inherit, Slot::Piped, Slot::Discard]),
            None,
        );
        assert_eq!(
            child.stdout.read_all(me).unwrap().unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(waited(me, &child), Status::Exited(0));
        0
    });
    assert_eq!(crate::io::stdin_reader().lock().buffered_len(), 2);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// D-1922: the captured output of a child that an unwinding body left to
/// the reaper joins the capture when the host takes it, after the test's
/// other output.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_reaped_child_output_joins_the_capture_at_the_end() {
    let dir = scratch("reaped");
    let capture = Capture::new();
    {
        let (dir, capture) = (dir.clone(), Arc::clone(&capture));
        run_root(2, move |me| {
            registry::start_reaper();
            let mut spec = sh(
                "printf early; printf ready >&2; exec sleep 30",
                [Slot::Discard, Slot::Inherit, Slot::Piped],
            );
            spec.capture = true;
            spec.temp_parent = dir;
            let child = start(me, &spec, Some(Arc::clone(&capture)));
            let ready = child.stderr.read_bytes(me, 5).unwrap().unwrap();
            assert_eq!(ready, Some(b"ready".to_vec()));
            let pid = child.process().pid();
            capture.append_output(Stream::Stdout, b"own".to_vec());
            // As the finalizer of an unwinding body's `Child` does.
            drop(child);
            wait_until(|| reaped(pid));
            0
        });
    }
    assert_eq!(
        capture.take(),
        [
            Chunk::Write(Stream::Stdout, b"own".to_vec()),
            Chunk::Write(Stream::Stdout, b"early".to_vec()),
        ]
    );
    assert_eq!(capture.take(), Vec::new());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// D-1924: a `File` opened for reading given as standard output, one
/// opened for writing given as standard input, and standard input's
/// handle given as standard output are kind 13.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_files_in_the_wrong_direction_are_invalid_input() {
    let dir = scratch("direction");
    let path = dir.join("file.txt");
    std::fs::write(&path, "x").unwrap();
    {
        run_root(1, move |me| {
            let reading = opened(me, &path);
            let writing = crate::file::open_on(me, path.to_str().unwrap(), 2)
                .unwrap()
                .unwrap();
            let kind = |slots: [Slot; 3]| spawn::spawn(&sh("exit 0", slots)).unwrap_err().kind;
            let stream = |file: &FileState| Slot::File(file.child_stream().unwrap());
            assert_eq!(kind([Slot::Discard, stream(&reading), Slot::Discard]), 13);
            assert_eq!(kind([Slot::Discard, Slot::Discard, stream(&reading)]), 13);
            assert_eq!(kind([stream(&writing), Slot::Discard, Slot::Discard]), 13);
            let std_stream = |stream| Slot::File(ChildStream::Std(stream));
            assert_eq!(kind([std_stream(1), Slot::Discard, Slot::Discard]), 13);
            assert_eq!(kind([Slot::Discard, std_stream(0), Slot::Discard]), 13);
            // The right directions start.
            let spec = sh(
                "exit 0",
                [stream(&reading), stream(&writing), std_stream(2)],
            );
            let child = start(me, &spec, None);
            assert_eq!(waited(me, &child), Status::Exited(0));
            0
        });
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Whether the child `pid` runs: no exit is waiting to be reaped.
fn running(pid: libc::pid_t) -> bool {
    // SAFETY: a zeroed `siginfo_t` is valid.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let id = libc::id_t::try_from(pid).unwrap();
    // SAFETY: `info` is a live slot the call writes; `WNOWAIT` reaps
    // nothing.
    let done = unsafe {
        libc::waitid(
            libc::P_PID,
            id,
            &raw mut info,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    };
    // SAFETY: `waitid` filled `info` or left it zeroed.
    done == 0 && unsafe { info.si_pid() } == 0
}

/// D-1917: the kill at a run's end reaches only the children of that run's
/// runtime; the whole-process kill reaches every child.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_a_runs_end_kills_only_its_own_children() {
    run_root(2, |me| {
        let mut mine = sh("exec sleep 30", slots(&[]));
        mine.owner = super::owner_of(&rt(me));
        let mut other = sh("exec sleep 30", slots(&[]));
        other.owner = mine.owner.wrapping_add(8);
        let mine = start(me, &mine, None);
        let other = start(me, &other, None);
        super::kill_children_of_shared(&rt(me));
        assert_eq!(waited(me, &mine), Status::Signaled(libc::SIGKILL));
        assert!(running(other.process().pid()));
        super::kill_open_children();
        assert_eq!(waited(me, &other), Status::Signaled(libc::SIGKILL));
        0
    });
}

/// D-1925: a signal the program ignores stays ignored in the child; only
/// SIGPIPE gets its default action back.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_ignored_signals_stay_ignored() {
    // SAFETY: this test's own process ignores SIGUSR2 for the test.
    unsafe { libc::signal(libc::SIGUSR2, libc::SIG_IGN) };
    run_root(1, |me| {
        let child = start(me, &sh("kill -USR2 $$; printf alive", slots(&[1])), None);
        assert_eq!(
            child.stdout.read_all(me).unwrap().unwrap(),
            b"alive".to_vec()
        );
        assert_eq!(waited(me, &child), Status::Exited(0));
        0
    });
    // SAFETY: as above, back to the default.
    unsafe { libc::signal(libc::SIGUSR2, libc::SIG_DFL) };
}

/// D-1917: once a run ended, a start of its runtime is refused, and a
/// child whose start was under way is killed by its starter: none
/// outlives the run.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_a_start_after_the_runs_end_leaves_no_child() {
    let owner = 0x7e57_0001;
    assert!(!registry::is_ended(owner));
    registry::kill_owned(owner);
    assert!(registry::is_ended(owner));
    let mut spec = sh("exec sleep 30", slots(&[]));
    spec.owner = owner;
    let refused = spawn::spawn(&spec).unwrap_err();
    assert_eq!(refused, ProcError::other("the program is ending"));
}

/// D-1926: an executable file without `#!` runs through `/bin/sh` with its
/// path and arguments, as `execvp` does, on every platform.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_a_script_without_a_shebang_runs_through_sh() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("script");
    let script = dir.join("plain-script");
    std::fs::write(&script, "printf '%s|%s' \"$0\" \"$1\"\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = script.to_str().unwrap().to_owned();
    {
        run_root(1, move |me| {
            let mut spec = sh("unused", slots(&[1]));
            spec.program.clone_from(&path);
            spec.args = vec!["x".to_owned()];
            let child = start(me, &spec, None);
            let out = child.stdout.read_all(me).unwrap().unwrap();
            assert_eq!(String::from_utf8(out).unwrap(), format!("{path}|x"));
            assert_eq!(waited(me, &child), Status::Exited(0));
            0
        });
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// D-1927: a program that inherited SIGCHLD as ignored gets the default
/// action back with its first child, so `wait` reports the status.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn process_an_ignored_sigchld_is_set_back_to_the_default() {
    registry::check_sigchld_again();
    // SAFETY: this test's own process ignores SIGCHLD, as under `nohup`
    // or `trap '' CHLD`.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
    run_root(1, |me| {
        let child = start(me, &sh("exit 3", slots(&[])), None);
        assert_eq!(waited(me, &child), Status::Exited(3));
        0
    });
    // SAFETY: only reads the action.
    let now = unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) };
    assert_eq!(now, libc::SIG_DFL);
}

/// D-1928: the reaper never waits on one child: a child handed over while
/// it still runs holds up no later one.
#[test]
#[cfg_attr(windows, ignore = "children on Windows are M6 (D-1919)")]
#[cfg_attr(miri, ignore = "starts a process")]
#[expect(
    clippy::zombie_processes,
    reason = "the reaper, under test, reaps both children"
)]
fn process_the_reaper_reaps_past_a_child_that_runs() {
    registry::start_reaper();
    let pid_of = |child: &std::process::Child| libc::pid_t::try_from(child.id()).unwrap();
    let long = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let short = std::process::Command::new("true").spawn().unwrap();
    let (long, short) = (pid_of(&long), pid_of(&short));
    registry::hand_over(long);
    registry::hand_over(short);
    wait_until(|| reaped(short));
    assert!(!reaped(long));
    // SAFETY: `kill` has no memory effects; `long` is this test's child,
    // not reaped yet.
    unsafe { libc::kill(long, libc::SIGKILL) };
    wait_until(|| reaped(long));
}
