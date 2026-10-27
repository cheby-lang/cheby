//! Tests of `cheby.h`'s functions (D-945, D-946, D-951), of drop functions
//! of user external types (D-949) and of the wrappers' helpers (D-946,
//! D-947). Whatever must end the process or write to the real standard
//! streams runs in a child process: this test binary run with one test and
//! `CHEBY_RT_TEST_FFI` naming what the child does.

use std::collections::HashMap;
use std::ffi::c_void;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::*;
use crate::abi::PanicKind;
use crate::alloc::Domain;
use crate::capture::{Capture, Chunk};
use crate::entry::{EntryConfig, EntryMode, EntryOutcome, MIN_STACK_SIZE};
use crate::io::cheby_io_println;
use crate::{DetachedGroup, Runtime, RuntimeConfig};

/// The variable that tells a child what to do.
const CHILD: &str = "CHEBY_RT_TEST_FFI";

/// Runs this test binary's `ffi::tests::<test>` alone with `CHILD` set to
/// `what`, its output piped.
fn child(test: &str, what: &str) -> Output {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("ffi::tests::{test}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, what)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

/// What `CHILD` asks this process to do, if anything.
fn child_task() -> Option<String> {
    std::env::var(CHILD).ok()
}

/// A `String` word of `text`.
fn string_word(text: &str) -> Word {
    values::string(text)
}

/// The text of a `String` word.
fn text(word: Word) -> String {
    // SAFETY: a live `String`.
    unsafe { string::to_rust_string(word) }.unwrap()
}

/// Runs `task` as an isolated test entry with a stack of `stack_size` bytes
/// in a fresh domain, and returns its outcome and the domain.
fn on_entry(stack_size: usize, task: fn() -> Word) -> (EntryOutcome, Arc<Domain>) {
    let domain = Domain::new();
    let config = EntryConfig {
        stack_size,
        domain: Arc::clone(&domain),
        ..EntryConfig::new(EntryMode::Test)
    };
    // SAFETY: the task builds and reads only its own runtime objects.
    let outcome = unsafe { entry::run_task(&config, task) }.unwrap();
    (outcome, domain)
}

/// The running entry's control block.
fn me() -> *mut ExecCtx {
    entry::current_ctx().unwrap()
}

// The table and the header ---------------------------------------------------

/// D-1226: `NAMES` is `cheby.h`'s declaration order, every
/// function the header declares is listed once, and each is a runtime
/// export (D-944, D-1204), so E0055 and the C-object check reserve it.
#[test]
fn ffi_names_follow_the_headers_declaration_order() {
    let header = include_str!("../../cheby_link/include/cheby.h");
    let declared: Vec<&str> = header
        .match_indices(PREFIX)
        .filter_map(|(at, _)| {
            let name = header.get(at..)?;
            let end = name.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
            // A declaration: the name is followed by its parameters.
            name.get(end..)?.starts_with('(').then(|| name.get(..end))?
        })
        .collect();
    assert_eq!(declared, NAMES, "cheby.h declares {declared:?}");
    let exports = crate::symbols();
    for (name, address) in NAMES.iter().zip(table()) {
        assert!(
            exports
                .iter()
                .any(|(export, at)| export == name && *at == address),
            "{name} is not a runtime export"
        );
    }
}

// Values -----------------------------------------------------------------------

/// D-945, D-311: strings and bytes round-trip, invalid UTF-8 is replaced,
/// and the bytes are borrowed from the value.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_strings_and_bytes_round_trip() {
    let (outcome, domain) = on_entry(1 << 20, || {
        // SAFETY: valid pointers and lengths, and values made here.
        unsafe {
            let s = cheby_ffi_string_new(b"caf\xc3\xa9".as_ptr().cast(), 5);
            let mut len = 0;
            let data = cheby_ffi_string_bytes(s, &raw mut len);
            assert_eq!(
                std::slice::from_raw_parts(data.cast::<u8>(), len),
                "café".as_bytes()
            );
            assert_eq!(text(word(s)), "café");
            cheby_ffi_drop(s);
            let bad = cheby_ffi_string_new(b"a\xffb".as_ptr().cast(), 3);
            assert_eq!(text(word(bad)), "a\u{fffd}b");
            cheby_ffi_drop(bad);
            let b = cheby_ffi_bytes_new([0_u8, 255, 7].as_ptr(), 3);
            let data = cheby_ffi_bytes_data(b, &raw mut len);
            assert_eq!(std::slice::from_raw_parts(data, len), [0, 255, 7]);
            cheby_ffi_drop(b);
            let empty = cheby_ffi_string_new(ptr::null(), 0);
            assert_eq!(text(word(empty)), "");
            cheby_ffi_drop(empty);
        }
        values::nil()
    });
    assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-945: scalars in generic positions, `Option`, `Result` and `List`
/// build and read back; payloads are taken, read-outs borrowed.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_scalars_options_results_and_lists() {
    let (outcome, domain) = on_entry(1 << 20, || {
        // SAFETY: values made here, each released once.
        unsafe {
            let big = cheby_ffi_int_new(i64::MAX);
            assert_eq!(cheby_ffi_int_get(big), i64::MAX);
            let small = cheby_ffi_int_new(-3);
            assert_eq!(cheby_ffi_int_get(small), -3);
            let x = cheby_ffi_float_new(2.5);
            assert!((cheby_ffi_float_get(x) - 2.5).abs() < f64::EPSILON);
            let yes = cheby_ffi_bool_new(true);
            assert!(cheby_ffi_bool_get(yes));
            assert!(!cheby_ffi_bool_get(cheby_ffi_bool_new(false)));
            assert_eq!(word(cheby_ffi_nil()), values::nil());
            let some = cheby_ffi_some(cheby_ffi_string_new(b"in".as_ptr().cast(), 2));
            assert_eq!(text(word(cheby_ffi_option_get(some))), "in");
            assert!(cheby_ffi_option_get(cheby_ffi_none()).is_null());
            let ok = cheby_ffi_ok(x);
            assert!(cheby_ffi_result_is_ok(ok));
            assert_eq!(cheby_ffi_result_get(ok), x);
            let err = cheby_ffi_err(cheby_ffi_dup(big));
            assert!(!cheby_ffi_result_is_ok(err));
            assert_eq!(cheby_ffi_result_get(err), big);
            let items = [big, small, some];
            let list = cheby_ffi_list_new(items.as_ptr(), 3);
            assert_eq!(cheby_ffi_list_length(list), 3);
            assert_eq!(cheby_ffi_int_get(cheby_ffi_list_get(list, 1)), -3);
            assert!(cheby_ffi_list_get(list, 3).is_null());
            let empty = cheby_ffi_list_new(ptr::null(), 0);
            assert_eq!(cheby_ffi_list_length(empty), 0);
            for value in [list, ok, err, empty, yes] {
                cheby_ffi_drop(value);
            }
        }
        values::nil()
    });
    assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-945: a NULL argument means no value: readers give NULL, zero or
/// `false`, `cheby_ffi_drop` does nothing, a builder given a NULL payload
/// gives NULL and releases what it took.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_null_arguments_mean_nothing() {
    let (outcome, domain) = on_entry(1 << 20, || {
        let null = ptr::null_mut();
        // SAFETY: NULL is a valid argument of every function.
        unsafe {
            assert!(cheby_ffi_dup(null).is_null());
            cheby_ffi_drop(null);
            let mut len = 9;
            assert!(cheby_ffi_string_bytes(null, &raw mut len).is_null());
            assert_eq!(len, 0);
            assert!(cheby_ffi_bytes_data(null, ptr::null_mut()).is_null());
            assert!(cheby_ffi_string_new(ptr::null(), 3).is_null());
            assert_eq!(cheby_ffi_int_get(null), 0);
            assert!(cheby_ffi_float_get(null).abs() < f64::EPSILON);
            assert!(!cheby_ffi_bool_get(null));
            assert!(cheby_ffi_some(null).is_null());
            assert!(cheby_ffi_ok(null).is_null());
            assert!(cheby_ffi_err(null).is_null());
            assert!(cheby_ffi_option_get(null).is_null());
            assert!(!cheby_ffi_result_is_ok(null));
            assert!(cheby_ffi_result_get(null).is_null());
            assert_eq!(cheby_ffi_list_length(null), 0);
            assert!(cheby_ffi_list_get(null, 0).is_null());
            assert!(cheby_ffi_list_new(ptr::null(), 2).is_null());
            // The other items of a list with a NULL one are released.
            let items = [cheby_ffi_string_new(b"x".as_ptr().cast(), 1), null];
            assert!(cheby_ffi_list_new(items.as_ptr(), 2).is_null());
            assert_eq!(cheby_ffi_write_stdout(ptr::null(), 0), 0);
            assert_eq!(cheby_ffi_write_stderr(ptr::null(), 1), -1);
        }
        values::nil()
    });
    assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// The child side of the debug type check: `cheby_ffi_int_get` of a
/// `String` stops the process.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_debug_type_check_child() {
    if child_task().as_deref() != Some("type") {
        return;
    }
    let (outcome, _) = on_entry(1 << 20, || {
        let s = string_word("text");
        // SAFETY: a value of another type, which the debug check stops on.
        unsafe { cheby_ffi_int_get(value(s)) };
        values::nil()
    });
    panic!("the entry went on: {outcome:?}");
}

/// D-945, D-1227: a value of another type is a fatal
/// runtime error in a runtime built with debug assertions.
#[test]
#[cfg_attr(windows, ignore = "starts a child process that runs fibers (D-545)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn ffi_debug_type_check_stops_on_a_value_of_another_type() {
    if !cfg!(debug_assertions) || child_task().is_some() {
        return;
    }
    let output = child("ffi_debug_type_check_child", "type");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("fatal runtime error: cheby_ffi_int_get was given a String, not an Int\n"),
        "{stderr}"
    );
}

// Failed allocations ---------------------------------------------------------------

/// D-946, D-642: a `cheby_ffi_` allocation that the system refuses sets the
/// fiber's flag with `out of memory` and returns NULL; later allocations of
/// the call return NULL without allocating, a payload given meanwhile is
/// released, and the wrapper's helpers leave the flag set (the wrapper's
/// cleanup then releases the owned result), so the entry panics with `out
/// of memory`; an external result's handle reaches its drop function.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_alloc_failure_sets_the_flag_and_returns_null() {
    node_key();
    let _serial = crate::alloc::tests::serial();
    let before = DROPPED.load(Ordering::SeqCst);
    let (outcome, domain) = on_entry(1 << 20, || {
        let kept = string_word("an owned result");
        crate::alloc::refuse_next_system_allocation();
        // SAFETY: valid arguments; the values are released below.
        unsafe {
            assert!(cheby_ffi_string_new(b"abc".as_ptr().cast(), 3).is_null());
            let fiber = &*me();
            assert_ne!(crate::unwind::flag::get(fiber), 0, "the flag is set");
            assert!(cheby_ffi_int_new(i64::MAX).is_null());
            assert!(cheby_ffi_some(value(string_word("taken"))).is_null());
            let message = string_word("foreign function `make` returned null");
            let location = string_word("src/a.cheby:1:4");
            cheby_rt_ffi_value(me(), kept, message, location);
            assert_ne!(crate::unwind::flag::get(fiber), 0, "the flag stays set");
            let handle = Box::into_raw(Box::new(Node { next: 0 }));
            let handle = Word::try_from(handle.expose_provenance()).unwrap();
            assert_eq!(cheby_rt_ffi_external(me(), handle, node_key()), 0);
            // What the wrapper's cleanup does with its owned result.
            rc::drop(kept);
            rc::drop(message);
            rc::drop(location);
            crate::unwind::raise::rethrow_if_unwinding(fiber);
        }
        values::nil()
    });
    assert_eq!(
        DROPPED.load(Ordering::SeqCst) - before,
        1,
        "the handle was dropped"
    );
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("the entry did not panic: {outcome:?}");
    };
    assert_eq!(report.message, alloc::OUT_OF_MEMORY);
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

// Drop functions --------------------------------------------------------------------

/// A node of a test chain: the handle of a test external type.
struct Node {
    /// The next value of the chain (owned), or 0.
    next: Word,
}

/// How many nodes [`drop_node`] has dropped.
static DROPPED: AtomicUsize = AtomicUsize::new(0);

/// The drop function of the test external type `Node`: releases the next
/// value with `cheby_ffi_drop` and frees the node.
unsafe extern "C" fn drop_node(handle: *mut c_void) {
    assert!(!handle.is_null(), "a drop function was given NULL");
    // SAFETY: every handle of `Node` is a leaked `Box<Node>`.
    let node = unsafe { Box::from_raw(handle.cast::<Node>()) };
    DROPPED.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the node owned this reference.
    unsafe { cheby_ffi_drop(value(node.next)) };
}

/// The key of the test external type `Node`, registered once.
fn node_key() -> u64 {
    static KEY: OnceLock<u64> = OnceLock::new();
    *KEY.get_or_init(|| {
        let key = layout::type_key("ffi_tests", "Node");
        layout::register_external_with_drop("Node", key, Some(drop_node)).unwrap();
        key
    })
}

/// A new `Node` value on the running entry that owns `next`.
fn node(next: Word) -> Word {
    let handle = Box::into_raw(Box::new(Node { next }));
    let handle = Word::try_from(handle.expose_provenance()).unwrap();
    // SAFETY: the running entry's control block and a handle of `Node`.
    unsafe { cheby_rt_ffi_external(me(), handle, node_key()) }
}

/// D-949: a chain of 100,000 handles, each holding the next, is released
/// without recursion on an entry with the smallest stack: each drop
/// function's `cheby_ffi_drop` queues onto the running free worklist.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_drop_chain_of_handles_is_freed_without_recursion() {
    node_key();
    let _serial = crate::alloc::tests::serial();
    let before = DROPPED.load(Ordering::SeqCst);
    let (outcome, domain) = on_entry(MIN_STACK_SIZE, || {
        let mut chain = 0;
        for _ in 0..100_000 {
            chain = node(chain);
        }
        // SAFETY: the chain's head, owned.
        unsafe { rc::drop(chain) };
        values::nil()
    });
    assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
    assert_eq!(DROPPED.load(Ordering::SeqCst) - before, 100_000);
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-947, D-949: a NULL handle is wrapped like any other, debug-prints as
/// its type, and never reaches its drop function; the drop function runs
/// once for a non-NULL one.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_drop_never_sees_a_null_handle() {
    node_key();
    let _serial = crate::alloc::tests::serial();
    let before = DROPPED.load(Ordering::SeqCst);
    let (outcome, domain) = on_entry(1 << 20, || {
        // SAFETY: the running entry and the key of a registered type.
        let null = unsafe { cheby_rt_ffi_external(me(), 0, node_key()) };
        assert_ne!(null, 0, "a NULL handle is still a value (D-947)");
        // SAFETY: a value of an external type holds its handle at offset 8
        // (D-590).
        assert_eq!(unsafe { read_field(word_to_ptr(null), 8) }, 0);
        // SAFETY: a live value.
        assert_eq!(unsafe { crate::generic::debug_string(null) }, "<Node>");
        // SAFETY: owned values.
        unsafe {
            rc::drop(null);
            rc::drop(node(0));
        }
        values::nil()
    });
    assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
    assert_eq!(DROPPED.load(Ordering::SeqCst) - before, 1);
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

/// D-946: the child side: a drop function whose `cheby_ffi_` allocation the
/// system refuses ends the process.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_alloc_in_a_drop_function_child() {
    unsafe extern "C" fn allocating_drop(_: *mut c_void) {
        crate::alloc::refuse_next_system_allocation();
        // SAFETY: valid arguments.
        let s = unsafe { cheby_ffi_string_new(b"lost".as_ptr().cast(), 4) };
        // SAFETY: as above.
        unsafe { cheby_ffi_drop(s) };
    }
    if child_task().as_deref() != Some("alloc-in-drop") {
        return;
    }
    let (outcome, _) = on_entry(1 << 20, || {
        let key = layout::type_key("ffi_tests", "Allocating");
        layout::register_external_with_drop("Allocating", key, Some(allocating_drop)).unwrap();
        // SAFETY: the running entry and a registered type's key.
        let value = unsafe { cheby_rt_ffi_external(me(), 1, key) };
        // SAFETY: owned.
        unsafe { rc::drop(value) };
        values::nil()
    });
    panic!("the entry went on: {outcome:?}");
}

/// D-946, D-171: a failed allocation inside a drop function is the fatal
/// runtime error `out of memory in a drop function`.
#[test]
#[cfg_attr(windows, ignore = "starts a child process that runs fibers (D-545)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn ffi_alloc_in_a_drop_function_is_fatal() {
    if child_task().is_some() {
        return;
    }
    let output = child("ffi_alloc_in_a_drop_function_child", "alloc-in-drop");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("fatal runtime error: out of memory in a drop function\n"),
        "{stderr}"
    );
}

// The wrappers' helpers ------------------------------------------------------------

/// D-947: a NULL result of a counted type panics with D-947's message at
/// the `@external`'s declaration; a non-NULL one passes.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs entries on fibers; Windows support is M6 (D-545)"
)]
fn ffi_value_helper_panics_on_null() {
    let (outcome, domain) = on_entry(1 << 20, || {
        let message = string_word("foreign function `make_name` returned null");
        let location = string_word("src/names.cheby:7:4");
        let name = string_word("kept");
        // SAFETY: the running entry, a borrowed result and strings
        // released below.
        unsafe {
            cheby_rt_ffi_value(me(), name, message, location);
            assert_eq!(crate::unwind::flag::get(&*me()), 0, "a value passes");
            rc::drop(name);
            cheby_rt_ffi_value(me(), 0, message, location);
            assert_ne!(crate::unwind::flag::get(&*me()), 0, "NULL panics");
            rc::drop(message);
            rc::drop(location);
            crate::unwind::raise::rethrow_if_unwinding(&*me());
        }
        values::nil()
    });
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("the entry did not panic: {outcome:?}");
    };
    assert_eq!(report.kind, PanicKind::Explicit);
    assert_eq!(report.message, "foreign function `make_name` returned null");
    assert_eq!(report.location, "src/names.cheby:7:4");
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}

// Output ----------------------------------------------------------------------------

/// The bodies of roots, by the address of their group.
type Bodies = HashMap<usize, fn() -> Word>;

/// The bodies of the captured roots, by the address of their group.
static BODIES: Mutex<Option<Bodies>> = Mutex::new(None);

/// The entry thunk of the captured roots: runs the body of the root's
/// group.
unsafe extern "C" fn run_body(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: the root fiber's live control block.
    let me = unsafe { &*ctx };
    let body = me.conc.group().and_then(|group| {
        BODIES
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|bodies| bodies.remove(&Arc::as_ptr(&group).addr()))
    });
    body.map_or_else(values::nil, |body| body())
}

/// Runs `body` as a root of a runtime of `threads` workers in a group whose
/// writes `capture` takes (D-723), and returns its outcome.
fn captured_root(threads: usize, capture: &Arc<Capture>, body: fn() -> Word) -> EntryOutcome {
    let runtime = Runtime::new(&RuntimeConfig {
        threads: Some(threads),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let group = DetachedGroup::with_capture(Arc::clone(capture));
    BODIES
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(Arc::as_ptr(&group).addr(), body);
    let domain = Domain::new();
    let config = EntryConfig {
        domain: Arc::clone(&domain),
        ..EntryConfig::new(EntryMode::Test)
    };
    // SAFETY: `run_body` is a Rust entry thunk, valid for the process.
    let handle = unsafe { runtime.start_root_in(run_body, &config, &group) }.unwrap();
    let outcome = handle.wait().unwrap();
    runtime.shutdown();
    assert_eq!(domain.live_objects(), 0, "objects leaked");
    outcome
}

/// `io::println(text)`.
fn println(text: &str) {
    let word = string_word(text);
    // SAFETY: a live `String`, borrowed.
    unsafe {
        cheby_io_println(word);
        rc::drop(word);
    }
}

/// D-951: the output functions write once each through the funnel, in
/// order with `io::println` and inside `cheby test`'s capture, standard
/// output's write followed by its flush, as `io::write_stdout` writes; and
/// they return 0.
#[test]
#[cfg_attr(windows, ignore = "runs fibers; Windows support is M6 (D-545)")]
fn ffi_output_is_captured_in_order() {
    let capture = Capture::new();
    let outcome = captured_root(2, &capture, || {
        println("before");
        // SAFETY: valid bytes.
        let (out, err) = unsafe {
            (
                cheby_ffi_write_stdout(b"from C\n".as_ptr().cast(), 7),
                cheby_ffi_write_stderr(b"to stderr\n".as_ptr().cast(), 10),
            )
        };
        println("after");
        values::bool_word(out == 0 && err == 0)
    });
    assert!(
        matches!(outcome, EntryOutcome::Returned(word) if word == values::bool_word(true)),
        "{outcome:?}"
    );
    assert_eq!(
        capture.take(),
        [
            Chunk::Write(Stream::Stdout, b"before\n".to_vec()),
            Chunk::Write(Stream::Stdout, b"from C\n".to_vec()),
            Chunk::Flush,
            Chunk::Write(Stream::Stderr, b"to stderr\n".to_vec()),
            Chunk::Write(Stream::Stdout, b"after\n".to_vec()),
        ]
    );
}

/// The drop function of the test external type `Loud`: one write of its
/// line through `cheby_ffi_write_stdout`, which it checks succeeded.
unsafe extern "C" fn loud_drop(_: *mut c_void) {
    let line = b"closing loud\n";
    // SAFETY: valid bytes.
    let status = unsafe { cheby_ffi_write_stdout(line.as_ptr().cast(), line.len()) };
    assert_eq!(status, 0);
}

/// Lines each side of the deadlock child writes.
const LINES: usize = 2_000;

/// D-949, D-684: the child side: on a runtime of two workers, one root
/// releases values whose drop function writes to standard output, while
/// another prints with `io::println`. Neither waits forever: the runtime
/// never frees under a stream lock.
#[test]
#[cfg_attr(windows, ignore = "runs fibers; Windows support is M6 (D-545)")]
fn ffi_drop_writing_child() {
    static KEY: OnceLock<u64> = OnceLock::new();
    if child_task().as_deref() != Some("drop-writes") {
        return;
    }
    let key = layout::type_key("ffi_tests", "Loud");
    layout::register_external_with_drop("Loud", key, Some(loud_drop)).unwrap();
    KEY.set(key).unwrap();
    let runtime = Runtime::new(&RuntimeConfig {
        threads: Some(2),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let domain = Domain::new();
    let config = EntryConfig {
        domain: Arc::clone(&domain),
        ..EntryConfig::new(EntryMode::Test)
    };
    let dropping = crate::sched::testing::start_root_with(&runtime, &config, |ctx| {
        let ctx = ptr::from_ref(ctx).cast_mut();
        let key = *KEY.get().unwrap();
        for _ in 0..LINES {
            // SAFETY: the running root and a registered type's key.
            let loud = unsafe { cheby_rt_ffi_external(ctx, 1, key) };
            // SAFETY: owned.
            unsafe { rc::drop(loud) };
        }
        values::nil()
    });
    let printing = crate::sched::testing::start_root_with(&runtime, &config, |_| {
        for _ in 0..LINES {
            println("printing");
        }
        values::nil()
    });
    for root in [dropping, printing] {
        let outcome = root
            .recv_timeout(crate::sched::testing::hang())
            .expect("a root deadlocked");
        assert!(matches!(outcome, EntryOutcome::Returned(_)), "{outcome:?}");
    }
    runtime.shutdown();
}

/// D-949, D-684, D-951: a drop function that writes while another fiber
/// prints does not deadlock, and every line is written whole.
#[test]
#[cfg_attr(windows, ignore = "starts a child process that runs fibers (D-545)")]
#[cfg_attr(miri, ignore = "starts a process")]
fn ffi_drop_writing_while_another_fiber_prints_does_not_deadlock() {
    if child_task().is_some() {
        return;
    }
    let output = child("ffi_drop_writing_child", "drop-writes");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    // Counted as whole lines; the first may follow libtest's `test … `.
    let closing = stdout.matches("closing loud\n").count();
    let printing = stdout.matches("printing\n").count();
    assert_eq!((closing, printing), (LINES, LINES), "{stderr}");
}

/// D-949, D-710: values a teardown releases call no user drop function.
#[test]
#[cfg_attr(windows, ignore = "runs fibers; Windows support is M6 (D-545)")]
fn ffi_drop_is_not_called_while_a_group_is_torn_down() {
    node_key();
    let _serial = crate::alloc::tests::serial();
    let group = DetachedGroup::new();
    let before = DROPPED.load(Ordering::SeqCst);
    group.set_tearing_down(true);
    let runtime = Runtime::new(&RuntimeConfig {
        threads: Some(1),
        ..RuntimeConfig::default()
    })
    .unwrap();
    BODIES
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(Arc::as_ptr(&group).addr(), || {
            // SAFETY: owned; the node's box leaks, as a teardown's
            // handles do (D-710).
            unsafe { rc::drop(node(0)) };
            values::nil()
        });
    let domain = Domain::new();
    let config = EntryConfig {
        domain: Arc::clone(&domain),
        ..EntryConfig::new(EntryMode::Test)
    };
    // SAFETY: `run_body` is a Rust entry thunk, valid for the process.
    let handle = unsafe { runtime.start_root_in(run_body, &config, &group) }.unwrap();
    assert!(matches!(handle.wait(), Ok(EntryOutcome::Returned(_))));
    runtime.shutdown();
    assert_eq!(
        DROPPED.load(Ordering::SeqCst),
        before,
        "a drop function ran"
    );
    assert_eq!(domain.live_objects(), 0, "objects leaked");
}
