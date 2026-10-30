//! `cheby.h`'s functions (D-945, D-946, D-951, ADR-0063), the drop
//! functions of user external types (D-949), and the helpers through
//! which the wrappers of a package's `@external(native, …)` handle
//! foreign results (D-946, D-947, D-590).
//!
//! # The functions of `cheby.h`
//!
//! Each `cheby_ffi_*` function is a `#[no_mangle] extern "C"` export of
//! the runtime, in [`NAMES`]' order, which is `cheby.h`'s declaration
//! order and the order of the forwarder's `cheby_ffi_table`
//! (D-1226): under the JIT a package's C code calls them
//! through the forwarder of its shared library, whose table
//! `cheby_jit::NativeLibrary::open` fills from [`table`] (D-941); an
//! executable's C objects call these definitions in the runtime archive
//! directly. They are runtime exports ([`kernels`], in `symbols()`), so
//! E0055 reserves their names outside std and the C-object check refuses
//! a C object that defines one (D-944, D-1204).
//!
//! - Foreign functions get no `ctx` (ADR-0055): a function finds the
//!   running fiber through `tls.rs`'s accessor (D-686, D-946). A
//!   non-blocking foreign call runs on its fiber's thread and cannot
//!   switch, so the fiber is current for the whole call. A package's
//!   `@blocking` call runs on a pool thread that acts for its parked fiber
//!   through the host slot (D-950, `ffi_pool.rs`): what it builds is
//!   charged to the fiber's domain, a failed allocation is recorded in its
//!   job and raised on the fiber when it resumes, and once a give-up or a
//!   halt abandoned the job, its allocations return NULL, `cheby_ffi_drop`
//!   and `cheby_ffi_dup` do nothing and the output functions return -1
//!   (`ffi_job_guarded`).
//! - Arguments are borrowed and results owned (D-187); a function that
//!   takes a payload (`cheby_ffi_some`, `cheby_ffi_ok`, `cheby_ffi_err`,
//!   `cheby_ffi_list_new`) takes ownership of it; a NULL argument means no
//!   value: readers return NULL, zero or `false`, `cheby_ffi_drop` does
//!   nothing, and a builder given a NULL payload releases what it took and
//!   returns NULL.
//! - `cheby_ffi_string_new` replaces invalid UTF-8 with U+FFFD (D-311).
//! - A value of another type than a function takes is undefined behavior
//!   (§12.5.1). A runtime built with debug assertions checks each one and
//!   stops with `fatal runtime error: <function> was given <what it got>,
//!   not <what it takes>` (D-1227).
//! - An allocation that fails sets the fiber's unwind flag with D-642's
//!   `out of memory` panic and returns NULL (D-946): the allocating
//!   functions run under [`ffi_guarded`], whose guard hands the panic to
//!   the flag, and once the flag is set every later allocating call of the
//!   same foreign call returns NULL without allocating, so a chain of calls
//!   ends quietly. The wrapper tests the flag when the foreign function
//!   returns, releases its owned result and unwinds
//!   ([`cheby_rt_ffi_value`], [`cheby_rt_ffi_external`]).
//! - `cheby_ffi_write_stdout` and `cheby_ffi_write_stderr` write once
//!   through the runtime's stream funnel (`io::write_std_stream`): in order
//!   with `io::println`, inside `cheby test`'s capture of each test, and
//!   for standard output followed by a flush, as `io::write_stdout` writes
//!   (D-951, D-365, D-574). They return 0, or -1 on failure without ending
//!   the program (D-306).
//!
//! # Drop functions of user external types
//!
//! A value of an external type is a 16-byte object of its type's layout,
//! the handle at offset 8 (D-590). The layout keeps the type's native drop
//! function (`layout::register_external_with_drop`, from the module
//! record's `ExternalEntry::drop`, D-1168), and [`finalize_external`],
//! `EXTERNAL_HANDLE`'s finalizer, calls it once, inline on the fiber that
//! released the last reference, from the free worklist, in cleanup pads
//! too, never with a NULL handle, and not while a teardown tears the
//! fiber's group down (D-949, D-710). While it runs, the thread's
//! drop-function mark is set (`tls::with_drop_mark`, D-946):
//!
//! - `cheby_ffi_drop` and every other release a `cheby_ffi_` function
//!   makes is queued under the mark instead of freeing, and the finalizer
//!   gives the queued references to the running worklist once the drop
//!   function returns, so a chain of handles that each hold the next one
//!   is freed without recursion;
//! - a failed allocation is the fatal runtime error `out of memory in a
//!   drop function` (`alloc::out_of_memory` reads the mark), since no
//!   wrapper tests the flag after a drop function (D-946, D-171);
//! - the output functions are allowed: the runtime never frees an object
//!   while it holds a stream lock (`io.rs`'s funnel writes borrowed bytes
//!   only, and the capture's lock covers a push of copied bytes), so a
//!   drop function that writes never waits for a lock its own thread
//!   holds, and the loom models (`ffi_models.rs`) check the queueing and
//!   the lock order.

use std::ffi::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use crate::abi::{self, Word, layout as ids, sym};
use crate::capture::Stream;
use crate::ctx::ExecCtx;
use crate::layout::{self, ExternalDrop, LayoutInfo};
use crate::object::{ptr_to_word, read_field, scan_and_layout, word_to_ptr, write_field};
use crate::panic::{PanicReport, fatal};
use crate::unwind::guard::{Unwound, guarded};
use crate::unwind::raise::{ChebyUnwind, raise_from_runtime_at};
use crate::{alloc, entry, ffi_pool, io, list, rc, string, tls, values};

/// The prefix of `cheby.h`'s functions (D-945).
pub const PREFIX: &str = "cheby_ffi_";

/// The forwarder's table in a package's shared library (D-941, D-1196):
/// [`NAMES`]' functions in order, then a null word.
pub const TABLE_SYMBOL: &str = "cheby_ffi_table";

/// `cheby.h`'s functions in its declaration order, which is also the
/// order of the forwarder's `cheby_ffi_table` (D-1226): the
/// table holds one entry per name, then the null word that ends it
/// (D-1196). The header only grows (D-945), so a new function goes last.
pub const NAMES: [&str; 25] = [
    "cheby_ffi_dup",
    "cheby_ffi_drop",
    "cheby_ffi_string_new",
    "cheby_ffi_string_bytes",
    "cheby_ffi_bytes_new",
    "cheby_ffi_bytes_data",
    "cheby_ffi_int_new",
    "cheby_ffi_int_get",
    "cheby_ffi_float_new",
    "cheby_ffi_float_get",
    "cheby_ffi_bool_new",
    "cheby_ffi_bool_get",
    "cheby_ffi_nil",
    "cheby_ffi_some",
    "cheby_ffi_none",
    "cheby_ffi_option_get",
    "cheby_ffi_ok",
    "cheby_ffi_err",
    "cheby_ffi_result_is_ok",
    "cheby_ffi_result_get",
    "cheby_ffi_list_new",
    "cheby_ffi_list_length",
    "cheby_ffi_list_get",
    "cheby_ffi_write_stdout",
    "cheby_ffi_write_stderr",
];

/// The address of each function of [`NAMES`], in its order: what the
/// forwarder's `cheby_ffi_table` holds under the JIT (D-941).
#[must_use]
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
pub fn table() -> [*const u8; 25] {
    [
        cheby_ffi_dup as *const u8,
        cheby_ffi_drop as *const u8,
        cheby_ffi_string_new as *const u8,
        cheby_ffi_string_bytes as *const u8,
        cheby_ffi_bytes_new as *const u8,
        cheby_ffi_bytes_data as *const u8,
        cheby_ffi_int_new as *const u8,
        cheby_ffi_int_get as *const u8,
        cheby_ffi_float_new as *const u8,
        cheby_ffi_float_get as *const u8,
        cheby_ffi_bool_new as *const u8,
        cheby_ffi_bool_get as *const u8,
        cheby_ffi_nil as *const u8,
        cheby_ffi_some as *const u8,
        cheby_ffi_none as *const u8,
        cheby_ffi_option_get as *const u8,
        cheby_ffi_ok as *const u8,
        cheby_ffi_err as *const u8,
        cheby_ffi_result_is_ok as *const u8,
        cheby_ffi_result_get as *const u8,
        cheby_ffi_list_new as *const u8,
        cheby_ffi_list_length as *const u8,
        cheby_ffi_list_get as *const u8,
        cheby_ffi_write_stdout as *const u8,
        cheby_ffi_write_stderr as *const u8,
    ]
}

/// Appends `cheby.h`'s functions to `out`, the runtime's exports
/// (`symbols()`), in [`NAMES`]' order.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    out.extend(NAMES.iter().copied().zip(table()));
}

/// The two helpers of packages' wrappers, in `sym::ALL`'s order (after
/// the builtin nominal descriptors).
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
pub(crate) fn result_helpers() -> [(&'static str, *const u8); 2] {
    [
        (sym::FFI_EXTERNAL, cheby_rt_ffi_external as *const u8),
        (sym::FFI_VALUE, cheby_rt_ffi_value as *const u8),
    ]
}

// The drop-function mark ----------------------------------------------------

/// Whether a user drop function runs on this thread (D-946): its
/// `cheby_ffi_` releases are queued, and a failed allocation is fatal.
pub(crate) fn in_drop_function() -> bool {
    tls::with_drop_mark(|mark| mark.borrow().is_some()).unwrap_or(false)
}

/// The fatal error of an allocation that fails inside a drop function
/// (D-946).
pub(crate) const OUT_OF_MEMORY_IN_DROP: &str = "out of memory in a drop function";

/// Gives up one reference that a `cheby_ffi_` function releases: inside a
/// drop function it is queued for the running free worklist (D-949), else
/// released at once, which may free and run other drop functions inline.
///
/// # Safety
///
/// `word` is 0, a tagged integer or an owned reference to a live object.
unsafe fn release(word: Word) {
    if word == 0 || abi::is_tagged_int(word) {
        return;
    }
    let queued = tls::with_drop_mark(|mark| match mark.borrow_mut().as_mut() {
        Some(queue) => {
            queue.push(word);
            true
        }
        None => false,
    })
    .unwrap_or(false);
    if !queued {
        // SAFETY: forwarded from the caller.
        unsafe { rc::drop(word) };
    }
}

/// Calls the drop function `drop` with `handle` under the thread's
/// drop-function mark, then gives every reference it released to
/// `release` (the running free worklist, or `rc::drop` outside a free).
///
/// # Safety
///
/// `drop` is a native drop function, `void f(void *handle)`, and `handle`
/// a non-NULL handle it takes; the caller does not hold the mark's cell.
unsafe fn run_drop(drop: ExternalDrop, handle: Word, release: &mut dyn FnMut(Word)) {
    let previous = tls::with_drop_mark(|mark| mark.replace(Some(Vec::new()))).flatten();
    // SAFETY: forwarded from the caller: the foreign drop function of the
    // handle's type, given the handle it owns (D-949).
    unsafe { drop(ptr::with_exposed_provenance_mut(handle_addr(handle))) };
    let queued = tls::with_drop_mark(|mark| mark.replace(previous))
        .flatten()
        .unwrap_or_default();
    for word in queued {
        release(word);
    }
}

/// A handle word as an address.
fn handle_addr(handle: Word) -> usize {
    usize::try_from(handle).unwrap_or(usize::MAX)
}

/// `EXTERNAL_HANDLE`'s finalizer (D-949, D-703): calls the drop function
/// of the dying value's layout, if it has one, with its handle, unless the
/// handle is NULL or a teardown tears the releasing fiber's group down
/// (D-710).
///
/// # Safety
///
/// As for `layout::Finalizer`: `word` is a value of an external type whose
/// count reached zero, finalized once.
pub(crate) unsafe fn finalize_external(word: Word, release: &mut dyn FnMut(Word)) {
    let obj = word_to_ptr(word);
    // SAFETY: forwarded from the caller: a live object until its block is
    // freed after this finalizer.
    let (_, id) = unsafe { scan_and_layout(obj) };
    let Some(drop) = layout::get(id).and_then(LayoutInfo::external_drop) else {
        return;
    };
    // SAFETY: a value of an external type holds its handle at offset 8
    // (D-590).
    let handle = unsafe { read_field(obj, 8) };
    if handle == 0 || tearing_down() {
        return;
    }
    // SAFETY: the type's drop function with the handle its value owned.
    unsafe { run_drop(drop, handle, release) };
}

/// Whether the running fiber's group is being torn down (D-710).
fn tearing_down() -> bool {
    entry::current_fiber()
        .and_then(|fiber| fiber.conc.group())
        .is_some_and(|group| group.tearing_down())
}

// Guards --------------------------------------------------------------------

/// Runs the body of a `cheby_ffi_` function under the entry-point guard
/// (D-697): a panic it raises (an allocation that failed, D-946) sets the
/// running fiber's flag, and the function returns its zero. Inside a drop
/// function a failed allocation has already ended the process
/// (`alloc::out_of_memory`); any other unwinding there is fatal too
/// (D-171).
fn ffi_guarded<R: Unwound>(body: impl FnOnce() -> R) -> R {
    let in_drop = in_drop_function();
    let job = if in_drop {
        None
    } else {
        match ffi_pool::current_job() {
            Some(job) => Some(job),
            None => return guarded(None, body),
        }
    };
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(value) => value,
        // D-950: on a pool thread acting for a parked fiber, a failed
        // allocation recorded itself in the job (`alloc::out_of_memory`);
        // the fiber raises it when it resumes.
        Err(payload) if payload.is::<ChebyUnwind>() && !in_drop => {
            if let Some(job) = job {
                job.fail();
            }
            R::UNWOUND
        }
        Err(payload) if payload.is::<ChebyUnwind>() => {
            fatal(format_args!("{OUT_OF_MEMORY_IN_DROP}"))
        }
        Err(_) => fatal(format_args!("a Rust panic inside the runtime")),
    }
}

/// [`ffi_guarded`] for a `cheby_ffi_` function that changes the runtime's
/// state (an allocation, a count, an output): on a pool thread acting for
/// a package's `@blocking` call, the call first enters its job under the
/// per-job lock, and a job that a give-up or a halt abandoned gets
/// `abandoned` without anything done: NULL, nothing or -1 (D-950,
/// GAP(ffi-job-lock), `ffi_pool.rs`).
fn ffi_job_guarded<R: Unwound>(abandoned: R, body: impl FnOnce() -> R) -> R {
    let Some(job) = ffi_pool::current_job() else {
        return ffi_guarded(body);
    };
    if !job.enter() {
        return abandoned;
    }
    let result = ffi_guarded(body);
    job.leave();
    result
}

/// Whether an earlier allocation of this foreign call failed (D-946), so
/// allocating functions return NULL: the running fiber's unwind flag is
/// set, or on a pool thread acting for a parked fiber, its job recorded
/// the failure (D-950).
fn unwinding() -> bool {
    if let Some(job) = ffi_pool::current_job() {
        return job.failed();
    }
    entry::current_fiber().is_some_and(|fiber| crate::unwind::flag::get(fiber) != 0)
}

/// The word of a `cheby_value *`.
fn word(value: *mut c_void) -> Word {
    ptr_to_word(value.cast_const().cast::<u8>())
}

/// The `cheby_value *` of a word.
fn value(word: Word) -> *mut c_void {
    word_to_ptr(word).cast::<c_void>()
}

// Debug type checks ---------------------------------------------------------

/// The types a `cheby_ffi_` function reads (D-1227).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    String,
    Bytes,
    Int,
    Float,
    Bool,
    Option,
    Result,
    List,
}

impl Want {
    /// What the function takes, for the fatal error.
    const fn text(self) -> &'static str {
        match self {
            Self::String => "a String",
            Self::Bytes => "a Bytes",
            Self::Int => "an Int",
            Self::Float => "a Float",
            Self::Bool => "a Bool",
            Self::Option => "an Option",
            Self::Result => "a Result",
            Self::List => "a List",
        }
    }

    /// Whether a value of layout `id` (`None`: a tagged integer) is one.
    fn accepts(self, id: Option<abi::LayoutId>) -> bool {
        match (self, id) {
            (Self::Int, None | Some(ids::BOXED_INT)) => true,
            (_, None) | (Self::Int, Some(_)) => false,
            (Self::String, Some(id)) => id == ids::STRING,
            (Self::Bytes, Some(id)) => id == ids::BYTES,
            (Self::Float, Some(id)) => id == ids::BOXED_FLOAT,
            (Self::Bool, Some(id)) => id == ids::TRUE || id == ids::FALSE,
            (Self::Option, Some(id)) => id == ids::SOME || id == ids::NONE,
            (Self::Result, Some(id)) => id == ids::OK || id == ids::ERR,
            (Self::List, Some(id)) => (ids::LIST_FIRST..=ids::LIST_LAST).contains(&id),
        }
    }
}

/// What a value is, for the fatal error of a debug type check.
fn describe(id: Option<abi::LayoutId>) -> String {
    let Some(id) = id else {
        return "an Int".to_owned();
    };
    for want in [
        Want::String,
        Want::Bytes,
        Want::Int,
        Want::Float,
        Want::Bool,
        Want::Option,
        Want::Result,
        Want::List,
    ] {
        if want.accepts(Some(id)) {
            return want.text().to_owned();
        }
    }
    match id {
        ids::NIL => "Nil".to_owned(),
        ids::TUPLE => "a tuple".to_owned(),
        ids::CLOSURE => "a function".to_owned(),
        _ => layout::get(id).map_or_else(
            || format!("a value of layout {id}"),
            |info| format!("a value of type `{}`", info.type_name),
        ),
    }
}

/// D-945: checks, in a runtime built with debug assertions, that `word`,
/// a non-NULL argument of `function`, is a `want`; stops with a fatal
/// runtime error otherwise (D-1227). Without debug
/// assertions a wrong value is undefined behavior, as §12.5.1 says.
///
/// # Safety
///
/// `word` is a tagged integer or a live object.
unsafe fn check(function: &str, word: Word, want: Want) {
    if cfg!(debug_assertions) {
        // SAFETY: forwarded from the caller.
        let id = unsafe { values::layout_of(word) };
        if !want.accepts(id) {
            fatal(format_args!(
                "{function} was given {}, not {}",
                describe(id),
                want.text()
            ));
        }
    }
}

/// The bytes `data` and `len` describe: empty for NULL with length 0,
/// `None` for NULL with a length.
///
/// # Safety
///
/// A non-NULL `data` points to `len` readable bytes for the call.
unsafe fn input<'a>(data: *const u8, len: usize) -> Option<&'a [u8]> {
    if data.is_null() {
        return (len == 0).then_some(&[]);
    }
    // SAFETY: forwarded from the caller.
    Some(unsafe { std::slice::from_raw_parts(data, len) })
}

/// Writes `n` through `out` when it is not NULL.
///
/// # Safety
///
/// A non-NULL `out` is writable.
unsafe fn put_len(out: *mut usize, n: usize) {
    if !out.is_null() {
        // SAFETY: forwarded from the caller.
        unsafe { out.write(n) };
    }
}

// Counts --------------------------------------------------------------------

/// `cheby_value *cheby_ffi_dup(cheby_value *v)`: adds a reference to `v`
/// and returns it; NULL gives NULL (D-945).
///
/// # Safety
///
/// `v` is NULL or a live Cheby value (C code's contract, §12.5.1).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_dup(v: *mut c_void) -> *mut c_void {
    value(ffi_job_guarded(word(v), || {
        let w = word(v);
        // SAFETY: forwarded from the caller; `dup` skips 0.
        unsafe { rc::dup(w) };
        w
    }))
}

/// `void cheby_ffi_drop(cheby_value *v)`: releases a reference; NULL does
/// nothing; inside a drop function it is queued onto the running free
/// worklist (D-945, D-949).
///
/// # Safety
///
/// `v` is NULL or a reference to a live Cheby value that C code owns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_drop(v: *mut c_void) {
    ffi_job_guarded((), || {
        // SAFETY: forwarded from the caller.
        unsafe { release(word(v)) };
    });
}

// Text ----------------------------------------------------------------------

/// `cheby_value *cheby_ffi_string_new(const char *bytes, size_t len)`: a
/// new `String` of the bytes, each invalid UTF-8 sequence replaced with
/// U+FFFD (D-311); NULL bytes with a length give NULL.
///
/// # Safety
///
/// A non-NULL `bytes` points to `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_string_new(bytes: *const c_char, len: usize) -> *mut c_void {
    value(ffi_job_guarded(0, || {
        // SAFETY: forwarded from the caller.
        match unsafe { input(bytes.cast::<u8>(), len) } {
            Some(data) if !unwinding() => values::string(&String::from_utf8_lossy(data)),
            _ => 0,
        }
    }))
}

/// `const char *cheby_ffi_string_bytes(cheby_value *s, size_t *len)`: the
/// UTF-8 bytes of the `String` `s`, not NUL-terminated, borrowed while `s`
/// is held, their count written through a non-NULL `len`; NULL gives NULL
/// and 0.
///
/// # Safety
///
/// `s` is NULL or a live `String`; a non-NULL `len` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_string_bytes(s: *mut c_void, len: *mut usize) -> *const c_char {
    let address = ffi_guarded(|| {
        // SAFETY: forwarded from the caller.
        unsafe { text_data("cheby_ffi_string_bytes", word(s), Want::String, len) }
    });
    ptr::with_exposed_provenance(address)
}

/// `cheby_value *cheby_ffi_bytes_new(const uint8_t *data, size_t len)`: a
/// new `Bytes` of the bytes; NULL data with a length gives NULL.
///
/// # Safety
///
/// A non-NULL `data` points to `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_bytes_new(data: *const u8, len: usize) -> *mut c_void {
    value(ffi_job_guarded(0, || {
        // SAFETY: forwarded from the caller.
        match unsafe { input(data, len) } {
            Some(data) if !unwinding() => values::bytes(data),
            _ => 0,
        }
    }))
}

/// `const uint8_t *cheby_ffi_bytes_data(cheby_value *b, size_t *len)`: the
/// bytes of the `Bytes` `b`, borrowed while `b` is held, their count
/// written through a non-NULL `len`; NULL gives NULL and 0.
///
/// # Safety
///
/// `b` is NULL or a live `Bytes`; a non-NULL `len` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_bytes_data(b: *mut c_void, len: *mut usize) -> *const u8 {
    let address = ffi_guarded(|| {
        // SAFETY: forwarded from the caller.
        unsafe { text_data("cheby_ffi_bytes_data", word(b), Want::Bytes, len) }
    });
    ptr::with_exposed_provenance(address)
}

/// The address of the contents of a `String` or `Bytes` for
/// [`cheby_ffi_string_bytes`] and [`cheby_ffi_bytes_data`], 0 for NULL,
/// their count written through a non-NULL `len`.
///
/// # Safety
///
/// `w` is 0 or a live value; a non-NULL `len` is writable.
unsafe fn text_data(function: &str, w: Word, want: Want, len: *mut usize) -> usize {
    if w == 0 {
        // SAFETY: forwarded from the caller.
        unsafe { put_len(len, 0) };
        return 0;
    }
    // SAFETY: forwarded from the caller.
    unsafe { check(function, w, want) };
    // SAFETY: a live value, borrowed for the call; its bytes stay valid
    // while C code holds it.
    let data = unsafe { string::text_bytes(w) }.unwrap_or(&[]);
    // SAFETY: forwarded from the caller.
    unsafe { put_len(len, data.len()) };
    data.as_ptr().expose_provenance()
}

// Scalars -------------------------------------------------------------------

/// `cheby_value *cheby_ffi_int_new(int64_t value)`: the uniform word of an
/// `Int`, for a generic position such as a list element.
///
/// # Safety
///
/// Callable with any argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_int_new(n: i64) -> *mut c_void {
    value(ffi_job_guarded(0, || {
        if unwinding() { 0 } else { values::int_word(n) }
    }))
}

/// `int64_t cheby_ffi_int_get(cheby_value *v)`: the `Int` `v`; NULL gives
/// 0.
///
/// # Safety
///
/// `v` is NULL or a live `Int`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_int_get(v: *mut c_void) -> i64 {
    ffi_guarded(|| {
        let w = word(v);
        if w == 0 {
            return 0;
        }
        // SAFETY: forwarded from the caller.
        unsafe {
            check("cheby_ffi_int_get", w, Want::Int);
            values::word_to_i64(w)
        }
    })
}

/// `cheby_value *cheby_ffi_float_new(double value)`: a `Float`.
///
/// # Safety
///
/// Callable with any argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_float_new(x: f64) -> *mut c_void {
    value(ffi_job_guarded(0, || {
        if unwinding() { 0 } else { values::f64_word(x) }
    }))
}

/// `double cheby_ffi_float_get(cheby_value *v)`: the `Float` `v`; NULL
/// gives 0.
///
/// # Safety
///
/// `v` is NULL or a live `Float`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_float_get(v: *mut c_void) -> f64 {
    ffi_guarded(|| {
        let w = word(v);
        if w == 0 {
            return 0.0;
        }
        // SAFETY: forwarded from the caller.
        unsafe {
            check("cheby_ffi_float_get", w, Want::Float);
            values::word_to_f64(w)
        }
    })
}

/// `cheby_value *cheby_ffi_bool_new(bool value)`: the static `True` or
/// `False`.
///
/// # Safety
///
/// Callable with any argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_bool_new(b: bool) -> *mut c_void {
    value(ffi_guarded(|| values::bool_word(b)))
}

/// `bool cheby_ffi_bool_get(cheby_value *v)`: the `Bool` `v`; NULL gives
/// `false`.
///
/// # Safety
///
/// `v` is NULL or a live `Bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_bool_get(v: *mut c_void) -> bool {
    ffi_guarded(|| {
        let w = word(v);
        if w == 0 {
            return false;
        }
        // SAFETY: forwarded from the caller.
        unsafe {
            check("cheby_ffi_bool_get", w, Want::Bool);
            values::word_to_bool(w)
        }
    })
}

/// `cheby_value *cheby_ffi_nil(void)`: the static `Nil`.
///
/// # Safety
///
/// Callable at any time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_nil() -> *mut c_void {
    value(ffi_guarded(values::nil))
}

// Option and Result ---------------------------------------------------------

/// A one-field constructor (`Some`, `Ok`, `Err`) of the owned `payload`;
/// NULL, or a payload while the flag is set, gives NULL, the payload
/// released (D-945, D-946).
fn wrap_payload(payload: Word, make: fn(Word) -> Word) -> Word {
    if payload == 0 {
        return 0;
    }
    if unwinding() {
        // SAFETY: the caller gave the payload up (D-945).
        unsafe { release(payload) };
        return 0;
    }
    make(payload)
}

/// `cheby_value *cheby_ffi_some(cheby_value *payload)`: `Some(payload)`,
/// taking the payload.
///
/// # Safety
///
/// `payload` is NULL or a reference to a live value that C code gives up.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_some(payload: *mut c_void) -> *mut c_void {
    value(ffi_job_guarded(0, || {
        wrap_payload(word(payload), values::some)
    }))
}

/// `cheby_value *cheby_ffi_none(void)`: the static `None`.
///
/// # Safety
///
/// Callable at any time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_none() -> *mut c_void {
    value(ffi_guarded(values::none))
}

/// `cheby_value *cheby_ffi_option_get(cheby_value *option)`: the payload
/// of `Some`, borrowed from `option`; NULL for `None` and for NULL.
///
/// # Safety
///
/// `option` is NULL or a live `Option`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_option_get(option: *mut c_void) -> *mut c_void {
    value(ffi_guarded(|| {
        let w = word(option);
        if w == 0 {
            return 0;
        }
        // SAFETY: forwarded from the caller.
        unsafe {
            check("cheby_ffi_option_get", w, Want::Option);
            values::read_option(w).unwrap_or(0)
        }
    }))
}

/// `cheby_value *cheby_ffi_ok(cheby_value *payload)`: `Ok(payload)`,
/// taking the payload.
///
/// # Safety
///
/// As for [`cheby_ffi_some`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_ok(payload: *mut c_void) -> *mut c_void {
    value(ffi_job_guarded(0, || {
        wrap_payload(word(payload), values::ok)
    }))
}

/// `cheby_value *cheby_ffi_err(cheby_value *payload)`: `Err(payload)`,
/// taking the payload.
///
/// # Safety
///
/// As for [`cheby_ffi_some`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_err(payload: *mut c_void) -> *mut c_void {
    value(ffi_job_guarded(0, || {
        wrap_payload(word(payload), values::err)
    }))
}

/// `bool cheby_ffi_result_is_ok(cheby_value *result)`: whether `result` is
/// an `Ok`; NULL gives `false`.
///
/// # Safety
///
/// `result` is NULL or a live `Result`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_result_is_ok(result: *mut c_void) -> bool {
    ffi_guarded(|| {
        let w = word(result);
        if w == 0 {
            return false;
        }
        // SAFETY: forwarded from the caller.
        unsafe {
            check("cheby_ffi_result_is_ok", w, Want::Result);
            values::read_result(w).is_ok()
        }
    })
}

/// `cheby_value *cheby_ffi_result_get(cheby_value *result)`: the payload
/// of an `Ok` or an `Err`, borrowed from `result`; NULL gives NULL.
///
/// # Safety
///
/// `result` is NULL or a live `Result`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_result_get(result: *mut c_void) -> *mut c_void {
    value(ffi_guarded(|| {
        let w = word(result);
        if w == 0 {
            return 0;
        }
        // SAFETY: forwarded from the caller.
        unsafe {
            check("cheby_ffi_result_get", w, Want::Result);
            values::read_result(w).unwrap_or_else(|error| error)
        }
    }))
}

// Lists ---------------------------------------------------------------------

/// `cheby_value *cheby_ffi_list_new(cheby_value **items, size_t n)`: a
/// `List` of the `n` items in order, taking each one. NULL `items` with
/// `n > 0`, or a NULL item, gives NULL with every item taken released.
///
/// # Safety
///
/// A non-NULL `items` points to `n` readable pointers, each NULL or a
/// reference to a live value that C code gives up.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_list_new(items: *const *mut c_void, n: usize) -> *mut c_void {
    value(ffi_job_guarded(0, || {
        let words: Vec<Word> = if items.is_null() {
            if n != 0 {
                return 0;
            }
            Vec::new()
        } else {
            // SAFETY: forwarded from the caller.
            unsafe { std::slice::from_raw_parts(items, n) }
                .iter()
                .map(|&item| word(item))
                .collect()
        };
        if words.contains(&0) || unwinding() {
            for w in words {
                // SAFETY: each item was given up (D-945).
                unsafe { release(w) };
            }
            return 0;
        }
        list::from_iter(words)
    }))
}

/// `size_t cheby_ffi_list_length(cheby_value *list)`: the number of
/// elements; NULL gives 0.
///
/// # Safety
///
/// `list` is NULL or a live `List`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_list_length(l: *mut c_void) -> usize {
    ffi_guarded(|| {
        let w = word(l);
        if w == 0 {
            return 0;
        }
        // SAFETY: forwarded from the caller.
        unsafe {
            check("cheby_ffi_list_length", w, Want::List);
            list::length(w)
        }
    })
}

/// `cheby_value *cheby_ffi_list_get(cheby_value *list, size_t index)`: the
/// element at `index`, borrowed from `list`; NULL out of range and for
/// NULL.
///
/// # Safety
///
/// `list` is NULL or a live `List`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_list_get(l: *mut c_void, index: usize) -> *mut c_void {
    value(ffi_guarded(|| {
        let w = word(l);
        if w == 0 {
            return 0;
        }
        // SAFETY: forwarded from the caller.
        unsafe {
            check("cheby_ffi_list_get", w, Want::List);
            list::get(w, index).unwrap_or(0)
        }
    }))
}

// Output --------------------------------------------------------------------

/// One write of `bytes` to `stream` through the funnel (D-951): 0, or -1
/// when it failed or `bytes` is NULL with a length.
///
/// # Safety
///
/// A non-NULL `bytes` points to `len` readable bytes.
unsafe fn write_out(stream: Stream, bytes: *const c_char, len: usize) -> c_int {
    // SAFETY: forwarded from the caller.
    let Some(data) = (unsafe { input(bytes.cast::<u8>(), len) }) else {
        return -1;
    };
    match io::write_std_stream(stream, data) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// `int cheby_ffi_write_stdout(const char *bytes, size_t len)`: one write
/// to standard output, then its flush, as `io::write_stdout` writes, in
/// order with the program's other output and inside `cheby test`'s capture
/// (D-951, D-574): 0, or -1 when the write failed, which never ends the
/// program (D-306).
///
/// # Safety
///
/// A non-NULL `bytes` points to `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_write_stdout(bytes: *const c_char, len: usize) -> c_int {
    // SAFETY: forwarded from the caller.
    ffi_job_guarded(-1, || unsafe { write_out(Stream::Stdout, bytes, len) })
}

/// `int cheby_ffi_write_stderr(const char *bytes, size_t len)`: one write
/// to standard error, as [`cheby_ffi_write_stdout`] (D-951).
///
/// # Safety
///
/// As for [`cheby_ffi_write_stdout`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_ffi_write_stderr(bytes: *const c_char, len: usize) -> c_int {
    // SAFETY: forwarded from the caller.
    ffi_job_guarded(-1, || unsafe { write_out(Stream::Stderr, bytes, len) })
}

// The wrappers' helpers -----------------------------------------------------
//
// The wrapper of a package's `@external(native, …)` whose result is a
// counted value or an external type calls one of these right after the
// foreign function, and code generation tests the flag after the helper
// instead of after the foreign function (`cheby_codegen_clif`'s
// `func.rs`), so the helper sees a flag that a failed `cheby_ffi_`
// allocation set, and the wrapper's cleanup releases the result before it
// unwinds (D-946).

/// Releases the handle `handle` of a value of the external type of layout
/// `id` that was never wrapped: calls the type's drop function, if it has
/// one and `handle` is not NULL (D-947, D-949).
fn release_handle(id: abi::LayoutId, handle: Word) {
    if handle == 0 {
        return;
    }
    if let Some(drop) = layout::get(id).and_then(LayoutInfo::external_drop) {
        // SAFETY: the type's drop function with the handle the foreign call
        // returned, which nothing else holds.
        unsafe {
            run_drop(drop, handle, &mut |word| {
                // SAFETY: a reference the drop function gave up.
                rc::drop(word);
            });
        };
    }
}

/// `uint64_t cheby_rt_ffi_external(ExecCtx *ctx, uint64_t handle, uint64_t
/// type_key)`: a new value of the external type whose key is `type_key`,
/// holding `handle`, the result of a foreign function, NULL included
/// (D-947, D-590). When a `cheby_ffi_` allocation of the call failed, or
/// the new value cannot be allocated, the handle is released (its type's
/// drop function called, unless it is NULL) and 0 returned with the flag
/// set, so the caller unwinds (D-946).
///
/// # Safety
///
/// `ctx` is the running fiber's control block; `handle` is the result of a
/// foreign function whose result type has that key.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_ffi_external(
    ctx: *mut ExecCtx,
    handle: Word,
    type_key: u64,
) -> Word {
    guarded(Some(ctx), || {
        let id = layout::external_by_key(type_key).unwrap_or_else(|| {
            fatal(format_args!(
                "no external type with the key {type_key:#x} is registered"
            ))
        });
        // SAFETY: forwarded from the caller.
        let fiber = unsafe { ctx.as_ref() };
        if fiber.is_some_and(|fiber| crate::unwind::flag::get(fiber) != 0) {
            release_handle(id, handle);
            return 0;
        }
        let allocated = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: forwarded from the caller; the header describes a
            // 16-byte object of the external type's layout (D-590), and
            // its one field is written next.
            unsafe { alloc::alloc_for(ctx, EXTERNAL_SIZE, abi::header(abi::RC_UNIQUE, 0, id)) }
        }));
        let obj = match allocated {
            Ok(obj) => obj,
            Err(payload) => {
                release_handle(id, handle);
                std::panic::resume_unwind(payload)
            }
        };
        // SAFETY: a fresh 16-byte object: the handle is its field at 8.
        unsafe { write_field(obj, 8, handle) };
        ptr_to_word(obj)
    })
}

/// The size of a value of an external type (D-590).
const EXTERNAL_SIZE: u64 = 16;

/// `void cheby_rt_ffi_value(ExecCtx *ctx, uint64_t raw, uint64_t message,
/// uint64_t location)`: checks `raw`, the owned result of a foreign
/// function, which the caller keeps (borrowed here). When a `cheby_ffi_`
/// allocation of the call failed, it returns with the flag still set, and
/// the caller's cleanup releases `raw` and unwinds (D-946). Otherwise a
/// NULL `raw` panics with `message`, ``foreign function `<symbol>`
/// returned null``, at `location`, the `@external` function's declaration
/// (both static `String`s, D-947).
///
/// D-1230: only the wrappers of packages'
/// `@external(native, …)` functions call it, so it is a naked stub as
/// `cheby_rt_panic` is (D-1040): it passes its caller's frame pointer and
/// return address to [`ffi_value_from`] and jumps to it, and the NULL
/// result's backtrace starts at the wrapper, not at the no-frames line.
/// AAPCS64: the arguments are in `x0`-`x3`; `x29` goes to `x4`, `x30` to
/// `x5`.
///
/// # Safety
///
/// `ctx` is the running fiber's control block; `raw` is 0 or a live value;
/// `message` and `location` are live `String`s; the caller is generated
/// code whose frame record is at `x29`.
#[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_ffi_value(
    ctx: *mut ExecCtx,
    raw: Word,
    message: Word,
    location: Word,
) {
    core::arch::naked_asm!(
        "mov x4, x29",
        "mov x5, x30",
        "b {body}",
        body = sym ffi_value_from,
    )
}

/// D-1230: as the AAPCS64 stub, on System V
/// x86-64: the arguments are in `rdi`, `rsi`, `rdx` and `rcx`; `rbp` goes
/// to `r8` and the return address, the word at `[rsp]`, to `r9`.
///
/// # Safety
///
/// As for the AAPCS64 stub; the caller's frame record is at `rbp`.
#[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_ffi_value(
    ctx: *mut ExecCtx,
    raw: Word,
    message: Word,
    location: Word,
) {
    core::arch::naked_asm!(
        "mov r8, rbp",
        "mov r9, qword ptr [rsp]",
        "jmp {body}",
        body = sym ffi_value_from,
    )
}

/// The plain entry under Miri and on Windows until M6 (D-1040): a NULL
/// result's backtrace has no start.
///
/// # Safety
///
/// `ctx` is the running fiber's control block; `raw` is 0 or a live value;
/// `message` and `location` are live `String`s.
#[cfg(not(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    not(miri),
    not(windows)
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_ffi_value(
    ctx: *mut ExecCtx,
    raw: Word,
    message: Word,
    location: Word,
) {
    // SAFETY: forwarded from the caller; 0 and 0 name no frame.
    unsafe { ffi_value_from(ctx, raw, message, location, 0, 0) }
}

/// The body of [`cheby_rt_ffi_value`]: `fp` and `ra` are the generated
/// wrapper's frame pointer and return address, or 0 and 0 (D-1040). It is
/// the outermost Rust frame, so a NULL `raw` hands its panic to the flag
/// directly ([`raise_from_runtime_at`]): it records the report, sets the
/// flag and returns, and the wrapper's cleanup releases `raw` and unwinds.
///
/// # Safety
///
/// As for [`cheby_rt_ffi_value`]; `fp` and `ra` are 0 or name the caller's
/// frame record and the address it returns to.
unsafe extern "C" fn ffi_value_from(
    ctx: *mut ExecCtx,
    raw: Word,
    message: Word,
    location: Word,
    fp: usize,
    ra: usize,
) {
    // SAFETY: forwarded from the caller.
    let fiber = unsafe { ctx.as_ref() };
    if fiber.is_some_and(|fiber| crate::unwind::flag::get(fiber) != 0) || raw != 0 {
        return;
    }
    // SAFETY: forwarded from the caller: both are live `String`s.
    let (message, location) = unsafe {
        (
            string::to_rust_string(message).unwrap_or_default(),
            string::to_rust_string(location).unwrap_or_default(),
        )
    };
    let report = PanicReport::new(abi::PanicKind::Explicit, Some(&message), &location);
    let start = crate::backtrace::start_at(fp, ra);
    // SAFETY: forwarded from the caller, which is the generated wrapper
    // below this outermost Rust frame; `start` names its frame or nothing.
    unsafe { raise_from_runtime_at(ctx, report, start) }
}

#[cfg(test)]
#[path = "ffi_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ffi_models.rs"]
mod models;
