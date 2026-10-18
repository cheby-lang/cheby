//! Tests that kernels and the runtime's walks hold no Rust-side memory
//! that the memory limit of constant evaluation does not see (D-617).
//!
//! The test binary's global allocator counts the bytes each thread holds.
//! A kernel runs on an entry thread, and the peak of what that thread
//! asked the system for while it ran must stay within the peak the domain
//! counted plus the peak of scratch buffers ([`super::scratch`]), up to a
//! small constant. A temporary the size of the data, such as a buffer of
//! the parts of a split or the escaped text of a `String`, breaks that.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::{Arc, Mutex, PoisonError};

use super::{Domain, scratch};
use crate::abi::Word;
use crate::entry::{self, EntryConfig, EntryMode, EntryOutcome};
use crate::{generic, rc, values};

/// The system allocator, counting the bytes each thread holds.
struct Counting;

thread_local! {
    /// Whether freed blocks may be kept in the quarantine or the block
    /// cache ([`super::keeps_freed_blocks`]). A measuring thread turns it
    /// off, so the memory it holds is the memory that is live.
    static KEEP_FREED: Cell<bool> = const { Cell::new(true) };
    /// The bytes this thread holds: allocated here, less those freed here.
    static HELD: Cell<i64> = const { Cell::new(0) };
    /// The most bytes this thread held since [`reset_peak`].
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

/// Whether freed blocks may be kept on this thread
/// ([`super::keeps_freed_blocks`]).
pub(super) fn keeps_freed() -> bool {
    KEEP_FREED.try_with(Cell::get).unwrap_or(true)
}

fn count(bytes: usize, grows: bool) {
    let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
    HELD.try_with(|held| {
        let now = if grows {
            held.get().wrapping_add(bytes)
        } else {
            held.get().wrapping_sub(bytes)
        };
        held.set(now);
        PEAK.try_with(|peak| peak.set(peak.get().max(now))).ok();
    })
    .ok();
}

// SAFETY: every request goes to the system allocator unchanged; the
// counting touches only constant-initialized thread-locals without
// destructors, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded from the caller.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            count(layout.size(), true);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded from the caller.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            count(layout.size(), true);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded from the caller.
        unsafe { System.dealloc(ptr, layout) };
        count(layout.size(), false);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded from the caller.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            // Both blocks may be held for a moment.
            count(new_size, true);
            count(layout.size(), false);
        }
        new
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// The bytes this thread holds now.
fn held() -> i64 {
    HELD.try_with(Cell::get).unwrap_or(0)
}

/// Restarts this thread's peak at what it holds now.
fn reset_peak() {
    PEAK.try_with(|peak| peak.set(held())).ok();
}

/// The bytes the system gave the thread beyond what the domain and the
/// scratch buffers accounted for, at the peaks of each, while `kernel` ran
/// on the input `input` builds, on an entry thread of a fresh domain.
fn unaccounted(input: fn() -> Word, kernel: fn(Word) -> Word) -> i64 {
    let domain = Domain::new();
    let config = EntryConfig {
        domain: Arc::clone(&domain),
        ..EntryConfig::new(EntryMode::Test)
    };
    let result = Arc::new(Mutex::new(None));
    let out = Arc::clone(&result);
    let task = move || {
        KEEP_FREED.with(|keep| keep.set(false));
        let input = input();
        domain.reset_peak();
        scratch::reset_peak();
        reset_peak();
        let (start, live) = (held(), super::current_domain().live_bytes());
        let output = kernel(input);
        let system = PEAK.try_with(Cell::get).unwrap_or(0).saturating_sub(start);
        let counted = domain.peak_bytes().saturating_sub(live);
        let scratch = i64::try_from(scratch::peak()).unwrap();
        // The allocator does count: every kernel here asks for at least
        // half the size of its data.
        assert!(system >= i64::try_from(SIZE / 2).unwrap(), "{system}");
        *out.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(system.saturating_sub(counted).saturating_sub(scratch));
        // SAFETY: both are owned here and no longer used.
        unsafe {
            rc::drop(output);
            rc::drop(input);
        }
        0
    };
    // SAFETY: the task builds and reads only its own runtime objects.
    let outcome = unsafe { entry::run_task(&config, task) }.unwrap();
    assert!(matches!(outcome, EntryOutcome::Returned(0)), "{outcome:?}");
    result.lock().unwrap().unwrap()
}

/// What a kernel may hold beyond what is accounted for: fixed-size
/// buffers, the block cache, and the like, never the data.
const SLACK: i64 = 256 << 10;

/// The size of the data each test works on: far above [`SLACK`].
const SIZE: usize = 1 << 20;

fn assert_accounted(input: fn() -> Word, kernel: fn(Word) -> Word) {
    let extra = unaccounted(input, kernel);
    assert!(
        extra <= SLACK,
        "{extra} bytes held beyond what the memory limit sees"
    );
}

fn ascii() -> Word {
    values::string(&"ab\"{\u{1}".repeat(SIZE / 5))
}

fn greek() -> Word {
    values::string(&"ΑΣ ΣΑΣ. ".repeat(SIZE / 13))
}

fn data() -> Word {
    values::bytes(&vec![0x66; SIZE])
}

fn ints() -> Word {
    crate::list::from_iter((0..i64::try_from(SIZE / 8).unwrap()).map(values::int_word))
}

/// `string::split(text, "")` and `string::split(text, "b")`.
#[test]
#[cfg_attr(miri, ignore = "runs an entry thread")]
fn split_holds_no_buffer_of_its_parts() {
    assert_accounted(ascii, |text| {
        let sep = values::string("");
        // SAFETY: live string arguments.
        let parts = unsafe { crate::text::kernels::cheby_string_split(text, sep) };
        // SAFETY: owned here.
        unsafe { rc::drop(sep) };
        parts
    });
    assert_accounted(ascii, |text| {
        let sep = values::string("b");
        // SAFETY: live string arguments.
        let parts = unsafe { crate::text::kernels::cheby_string_split(text, sep) };
        // SAFETY: owned here.
        unsafe { rc::drop(sep) };
        parts
    });
}

/// `string::reverse`, `string::uppercase` and `string::lowercase`.
#[test]
#[cfg_attr(miri, ignore = "runs an entry thread")]
fn text_kernels_write_their_result_in_place() {
    // SAFETY (each kernel): a live string argument.
    assert_accounted(greek, |text| unsafe {
        crate::text::kernels::cheby_string_reverse(text)
    });
    assert_accounted(greek, |text| unsafe {
        crate::text::kernels::cheby_string_uppercase(text)
    });
    assert_accounted(greek, |text| unsafe {
        crate::text::kernels::cheby_string_lowercase(text)
    });
}

/// `string::from_code_point_values` and `bytes::from_list`.
#[test]
#[cfg_attr(miri, ignore = "runs an entry thread")]
fn list_conversions_write_their_result_in_place() {
    assert_accounted(
        || {
            crate::list::from_iter(
                (0..SIZE).map(|i| values::int_word(0x3B1 + i64::try_from(i % 25).unwrap())),
            )
        },
        // SAFETY: a live list argument.
        |list| unsafe { crate::text::kernels::cheby_string_from_code_point_values(list) },
    );
    assert_accounted(
        || {
            crate::list::from_iter(
                (0..SIZE).map(|i| values::int_word(i64::try_from(i % 256).unwrap())),
            )
        },
        // SAFETY: a live list argument.
        |list| unsafe { crate::bytes::kernels::cheby_bytes_from_list(list) },
    );
}

/// The debug text of a `String`, of `Bytes` and of a long list.
#[test]
#[cfg_attr(miri, ignore = "runs an entry thread")]
fn debug_text_holds_no_copy_of_the_data() {
    // SAFETY (each walk): a live value without hooks.
    assert_accounted(ascii, |text| unsafe { generic::debug_object(text) });
    assert_accounted(data, |data| unsafe { generic::debug_object(data) });
    assert_accounted(ints, |list| unsafe { generic::debug_object(list) });
}

/// Equality and hashing of long lists.
#[test]
#[cfg_attr(miri, ignore = "runs an entry thread")]
fn equality_and_hashing_account_for_their_worklists() {
    assert_accounted(ints, |list| {
        // SAFETY: a live value, compared with itself.
        let same = unsafe { generic::equal(list, list) };
        assert_eq!(same, Ok(true));
        values::int_word(0)
    });
    assert_accounted(ints, |list| {
        // SAFETY: a live value without hooks.
        let hash = unsafe { generic::hash_with_seed(list, 7) };
        assert!(hash.is_ok());
        values::int_word(0)
    });
}

/// `list::unique`, whose table of the kept elements is scratch.
#[test]
#[cfg_attr(miri, ignore = "runs an entry thread")]
fn unique_accounts_for_its_table() {
    assert_accounted(
        || {
            crate::list::from_iter(
                (0..SIZE / 4).map(|i| values::int_word(i64::try_from(i % (SIZE / 8)).unwrap())),
            )
        },
        // SAFETY: a live list of `Int`s.
        |list| unsafe { crate::list::kernels::cheby_list_unique(list) },
    );
}
