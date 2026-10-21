//! Tests of requests that no memory can serve: a size that overflows 64
//! bits, one beyond the address space, and one the system refuses. During
//! constant evaluation a request over the memory limit ends the evaluation
//! with the memory limit, E0076, before anything is allocated (D-617).
//! Anywhere else, and for a refusal under the limit, it panics the entry
//! with `out of memory` (D-642), so a REPL session or a test run goes on.
//! One test per path: every kernel that computes a size from a count it is
//! given, the growth of text objects (interpolation included), scratch
//! buffers, the debug-text arena, and raw allocation.
//!
//! `string::join` and `bytes::concat` sum the sizes of existing values,
//! which cannot overflow 64 bits in a real process, so only the shared
//! paths they end in ([`string::alloc_string`], [`string::alloc_bytes`])
//! are tested.

use std::sync::Arc;

use crate::abi::{PanicKind, Word};
use crate::alloc::scratch::ScratchVec;
use crate::alloc::{Domain, OUT_OF_MEMORY, alloc_object};
use crate::consteval::{ConstTable, Limit};
use crate::entry::{self, ConstEvalLimits, EntryConfig, EntryMode, EntryOutcome};
use crate::hamt::kernels::{cheby_map_insert, cheby_map_new};
use crate::object::ptr_to_word;
use crate::panic::UNKNOWN_LOCATION;
use crate::text::kernels::{cheby_string_pad_end, cheby_string_pad_start, cheby_string_repeat};
use crate::{generic, helpers, rc, string, values};

/// Runs `request` on an entry of `mode` in a fresh domain and returns the
/// report of its panic.
fn panic_report(mode: EntryMode, consts: Option<Arc<ConstTable>>, request: fn() -> Word) -> String {
    let config = EntryConfig {
        consts,
        domain: Domain::new(),
        ..EntryConfig::new(mode)
    };
    // SAFETY: the task builds and reads only its own runtime objects.
    let outcome = unsafe { entry::run_task(&config, request) }.unwrap();
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("the request returned: {outcome:?}");
    };
    assert_eq!(report.kind, PanicKind::Explicit);
    assert_eq!(report.location, UNKNOWN_LOCATION);
    report.message
}

/// Checks that `request`, run as a constant evaluation with a 1 GiB memory
/// limit, ends with the memory limit (D-617).
fn exceeds_the_limit(request: fn() -> Word) {
    let table = ConstTable::new();
    let limits = ConstEvalLimits {
        memory: Some(1 << 30),
        ..ConstEvalLimits::default()
    };
    let message = panic_report(
        EntryMode::ConstEval(limits),
        Some(Arc::clone(&table)),
        request,
    );
    assert_eq!(message, Limit::Memory.message());
    assert_eq!(table.abandon().limit, Some(Limit::Memory));
}

/// Checks that `request`, run as a test entry, panics with `out of memory`
/// (D-642).
fn panics_out_of_memory(request: fn() -> Word) {
    assert_eq!(panic_report(EntryMode::Test, None, request), OUT_OF_MEMORY);
}

/// Both of the above.
fn refused(request: fn() -> Word) {
    exceeds_the_limit(request);
    panics_out_of_memory(request);
}

/// Calls a text kernel with a new string argument and releases it.
fn with_string(text: &str, kernel: impl FnOnce(Word) -> Word) -> Word {
    let word = values::string(text);
    let result = kernel(word);
    // SAFETY: the word was created above and is no longer used.
    unsafe { rc::drop(word) };
    result
}

/// `string::repeat(text, count)`.
fn repeat(text: &str, count: i64) -> Word {
    with_string(text, |text| {
        // SAFETY: a live string argument.
        unsafe { cheby_string_repeat(text, count) }
    })
}

/// `string::pad_start(text, width, pad)`.
fn pad_start(text: &str, width: i64, pad: &str) -> Word {
    with_string(pad, |pad| {
        with_string(text, |text| {
            // SAFETY: live string arguments.
            unsafe { cheby_string_pad_start(text, width, pad) }
        })
    })
}

/// `string::pad_end(text, width, pad)`.
fn pad_end(text: &str, width: i64, pad: &str) -> Word {
    with_string(pad, |pad| {
        with_string(text, |text| {
            // SAFETY: live string arguments.
            unsafe { cheby_string_pad_end(text, width, pad) }
        })
    })
}

#[test]
fn repeat_whose_size_overflows_64_bits() {
    // The skeptic's reproduction: 3 × 9e18 bytes cannot be counted.
    refused(|| repeat("abc", 9_000_000_000_000_000_000));
}

#[test]
fn repeat_whose_object_size_overflows_64_bits() {
    // 2 × (2^63 - 1) bytes fit 64 bits, but the object with its header
    // does not.
    refused(|| repeat("ab", i64::MAX));
}

#[test]
fn repeat_beyond_the_address_space() {
    // 8e18 bytes fit 64 bits but no allocation layout.
    refused(|| repeat("ab", 4_000_000_000_000_000_000));
}

#[test]
fn repeat_the_system_refuses() {
    // 2^56 bytes have a layout, which the budget refuses during constant
    // evaluation and the system allocator refuses at run time: no 64-bit
    // platform maps that much.
    refused(|| repeat("ab", 1 << 55));
}

#[test]
fn pad_start_whose_fill_overflows_64_bits() {
    // (2^63 - 2) copies of a 4-byte pad.
    refused(|| pad_start("x", i64::MAX, "😀"));
}

#[test]
fn pad_start_whose_fill_and_text_overflow_64_bits() {
    // The fill of 2-byte code points fits 64 bits; adding the 16 bytes of
    // the text does not.
    refused(|| pad_start("😀😀😀😀", i64::MAX, "é"));
}

#[test]
fn pad_start_beyond_the_address_space() {
    refused(|| pad_start("x", i64::MAX, "ab"));
}

#[test]
fn pad_end_whose_fill_overflows_64_bits() {
    refused(|| pad_end("x", i64::MAX, "😀"));
}

#[test]
fn pad_end_whose_growth_overflows_64_bits() {
    // The fill fits 64 bits; growing the 16-byte text by it does not.
    refused(|| pad_end("😀😀😀😀", i64::MAX, "é"));
}

#[test]
fn interpolation_buffer_beyond_the_address_space() {
    refused(|| {
        // SAFETY: the helper ignores `ctx` and takes any capacity.
        unsafe { helpers::cheby_rt_str_with_capacity(std::ptr::null_mut(), i64::MAX) }
    });
}

#[test]
fn text_objects_whose_size_overflows_64_bits() {
    refused(|| ptr_to_word(string::alloc_string(u64::MAX)));
    refused(|| ptr_to_word(string::alloc_bytes(u64::MAX - 7)));
}

#[test]
fn raw_objects_that_cannot_be_allocated() {
    refused(|| ptr_to_word(alloc_object(u64::MAX, 0)));
    refused(|| ptr_to_word(alloc_object(1 << 63, 0)));
}

/// A request under the memory limit that the system refuses is no
/// memory-limit error, which raising the limit would not cure, but the
/// `out of memory` panic, E0075 for a constant (D-642).
#[test]
fn a_refusal_under_the_limit_panics() {
    let table = ConstTable::new();
    let limits = ConstEvalLimits {
        memory: Some(1 << 62),
        ..ConstEvalLimits::default()
    };
    // 2^56 bytes are under the limit, and no 64-bit platform maps them.
    let message = panic_report(
        EntryMode::ConstEval(limits),
        Some(Arc::clone(&table)),
        || repeat("ab", 1 << 55),
    );
    assert_eq!(message, OUT_OF_MEMORY);
    assert_eq!(table.abandon().limit, None);
}

/// A scratch buffer of the runtime's walks: its growth counts against the
/// memory limit, and a growth the system refuses panics.
#[test]
fn scratch_buffers_that_cannot_grow() {
    refused(|| {
        let mut items = ScratchVec::<u64>::new();
        items.reserve(1 << 60);
        values::int_word(0)
    });
    exceeds_the_limit(|| {
        // 2 GiB, which a 1 GiB limit refuses before the system is asked.
        let mut items = ScratchVec::<u64>::new();
        items.reserve(1 << 28);
        values::int_word(0)
    });
}

/// The debug text of a map of 16 entries that share one 8 KiB string.
fn debug_of_shared_values() -> Word {
    let big = values::string(&"x".repeat(8 << 10));
    let mut map = cheby_map_new();
    for key in 0..16 {
        let key = values::int_word(key);
        // SAFETY: a live map and live key and value words, borrowed.
        let next = unsafe { cheby_map_insert(map, key, big) };
        // SAFETY: the old map is no longer used.
        unsafe { rc::drop(map) };
        map = next;
    }
    // SAFETY: a live map without hooks.
    let text = unsafe { generic::debug_object(map) };
    // SAFETY: the map and the string are no longer used.
    unsafe {
        rc::drop(map);
        rc::drop(big);
    }
    text
}

/// D-617: the arena that holds the text of a map's sorted entries counts
/// against the memory limit, although the domain does not count it, so
/// text far larger than the values it prints stops at the limit.
#[test]
fn the_debug_arena_counts_against_the_memory_limit() {
    let table = ConstTable::new();
    let limits = ConstEvalLimits {
        memory: Some(64 << 10),
        ..ConstEvalLimits::default()
    };
    let message = panic_report(
        EntryMode::ConstEval(limits),
        Some(Arc::clone(&table)),
        debug_of_shared_values,
    );
    assert_eq!(message, Limit::Memory.message());
    assert_eq!(table.abandon().limit, Some(Limit::Memory));
}

/// Without a limit the same text is built, once per entry.
#[test]
#[cfg_attr(miri, ignore = "runs an entry thread")]
fn the_debug_arena_builds_the_text_without_a_limit() {
    let config = EntryConfig {
        domain: Domain::new(),
        ..EntryConfig::new(EntryMode::Test)
    };
    let task = || {
        let text = debug_of_shared_values();
        // SAFETY: the debug text is a live string, released after reading.
        let length = unsafe { string::as_str(text) }.map_or(0, str::len);
        // SAFETY: as above.
        unsafe { rc::drop(text) };
        u64::try_from(length).unwrap()
    };
    // SAFETY: the task builds and reads only its own runtime objects.
    let outcome = unsafe { entry::run_task(&config, task) }.unwrap();
    assert!(
        matches!(outcome, EntryOutcome::Returned(length) if length > 16 * (8 << 10)),
        "{outcome:?}"
    );
}

/// Runs a task that arms [`super::refuse_next_fast_allocation`], and
/// [`super::refuse_next_system_allocation`] too when `twice`, then
/// allocates a tuple of two fields, a size the block cache has a class
/// for, and frees it. Returns the outcome, the domain, whether the inline
/// path reached its hook, and the live objects and bytes the allocation
/// added, read by the task.
fn refuse_on_the_inline_path(twice: bool) -> (EntryOutcome, Arc<Domain>, bool, (i64, i64)) {
    // The hooks arm one thread of the process at a time.
    let _guard = super::tests::serial();
    let domain = Domain::new();
    let config = EntryConfig {
        domain: Arc::clone(&domain),
        ..EntryConfig::new(EntryMode::Test)
    };
    let charged = Arc::new(std::sync::Mutex::new((0, 0)));
    let task = {
        let domain = Arc::clone(&domain);
        let charged = Arc::clone(&charged);
        move || {
            super::refuse_next_fast_allocation();
            if twice {
                super::refuse_next_system_allocation();
            }
            let (objects, bytes) = (domain.live_objects(), domain.live_bytes());
            let one = crate::abi::tag_int(1).unwrap();
            let pair = super::tests::tuple(&[one, one]);
            *charged.lock().unwrap() = (
                domain.live_objects().saturating_sub(objects),
                domain.live_bytes().saturating_sub(bytes),
            );
            // SAFETY: the only reference.
            unsafe { rc::drop(pair) };
            0
        }
    };
    // SAFETY: the task builds and reads only its own runtime objects.
    let outcome = unsafe { entry::run_task(&config, task) }.unwrap();
    let reached = !super::FAST_REFUSAL_ARMED.load(std::sync::atomic::Ordering::SeqCst);
    let charged = *charged.lock().unwrap();
    (outcome, domain, reached, charged)
}

/// D-907: a refusal on the inline path that the general path's second
/// request does not repeat raises nothing and leaves the domain unmarked,
/// and the block is charged once, at its size class.
#[test]
#[cfg_attr(miri, ignore = "runs an entry thread")]
fn a_refusal_the_retry_serves_raises_nothing() {
    let (outcome, domain, reached, charged) = refuse_on_the_inline_path(false);
    assert!(reached, "the inline path did not reach its hook");
    assert!(matches!(outcome, EntryOutcome::Returned(0)), "{outcome:?}");
    assert!(!domain.system_refused());
    assert_eq!(charged, (1, 24));
    assert_eq!(domain.live_objects(), 0);
}

/// D-907, D-771: when the second request is refused too, it raises
/// `out of memory` and marks the domain.
#[test]
#[cfg_attr(miri, ignore = "runs an entry thread")]
fn a_refusal_the_retry_repeats_raises_and_marks_the_domain() {
    let (outcome, domain, reached, _) = refuse_on_the_inline_path(true);
    assert!(reached, "the inline path did not reach its hook");
    let EntryOutcome::Panicked(report) = outcome else {
        panic!("expected a panic, got {outcome:?}");
    };
    assert_eq!(report.message, OUT_OF_MEMORY);
    assert!(domain.system_refused());
}
