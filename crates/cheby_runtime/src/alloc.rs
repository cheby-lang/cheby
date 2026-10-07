//! Allocation, freeing and the debug leak counter (F1).
//!
//! Every heap object is allocated 8-aligned through the global allocator.
//! Its size is not stored: freeing recomputes it from the layout table, or
//! from `cap` for strings and bytes, exactly as allocation computed it.
//!
//! Freeing an object visits its leading value fields (the scan count) and
//! releases each one. It is iterative, with an explicit worklist, so
//! freeing a list of a million cells uses no more stack than freeing one
//! (F1, D-094).

use std::alloc::Layout;
use std::cell::Cell;

use crate::abi::{self, Word};
use crate::ctx::ExecCtx;
use crate::layout::{self, LayoutKind};
use crate::object::{self, read_field, scan_and_layout, to_u64, to_usize, word_to_ptr};
use crate::panic::fatal;
use crate::rc;

#[cfg(debug_assertions)]
static LIVE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Whether this build counts live objects (debug builds only).
pub const LEAK_CHECK_ENABLED: bool = cfg!(debug_assertions);

/// The number of heap objects allocated and not yet freed, not counting
/// objects that became static. Always 0 when [`LEAK_CHECK_ENABLED`] is
/// false.
#[must_use]
pub fn live_objects() -> i64 {
    #[cfg(debug_assertions)]
    {
        LIVE.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(debug_assertions))]
    {
        0
    }
}

fn count_live(delta: i64) {
    #[cfg(debug_assertions)]
    LIVE.fetch_add(delta, std::sync::atomic::Ordering::Relaxed);
    #[cfg(not(debug_assertions))]
    let _ = delta;
}

/// Removes one object from the leak count without freeing it: an object
/// whose count overflowed and that became static (D-454). E6's
/// `freeze` uses this for constants.
pub(crate) fn forget_one() {
    count_live(-1);
}

/// Allocates an object for generated code: `size` bytes including the
/// header, 8-aligned, with `header` written at offset 0. The fields are
/// left uninitialized for the caller to write. Ends the process on
/// allocation failure.
///
/// `ctx` is the running entry's execution context. It will carry the
/// memory budget of constant evaluation (D-195); it is unused in M1 and
/// may be null.
///
/// # Safety
///
/// `header` describes an object of exactly `size` bytes (as the layout
/// table computes it), and the caller initializes every field before the
/// object is freed or read.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_alloc(ctx: *mut ExecCtx, size: u64, header: u64) -> Word {
    let _ = ctx;
    object::ptr_to_word(alloc_object(size, header))
}

/// Allocates an object of `size` bytes (rounded up to 8) with the given
/// header and uninitialized fields, and counts it as live. Ends the process
/// on allocation failure.
#[must_use]
pub fn alloc_object(size: u64, header: u64) -> *mut u8 {
    let layout = object_layout(size);
    // SAFETY: `object_layout` never returns a zero-sized layout.
    let ptr = unsafe { std::alloc::alloc(layout) };
    if ptr.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    // SAFETY: the allocation is at least 8 bytes and 8-aligned.
    unsafe { object::write_field(ptr, 0, header) };
    count_live(1);
    ptr
}

/// The allocation layout of an object of `size` bytes.
fn object_layout(size: u64) -> Layout {
    let rounded = size
        .max(abi::HEADER_SIZE)
        .checked_next_multiple_of(abi::OBJECT_ALIGN);
    match rounded.map(|size| Layout::from_size_align(to_usize(size), to_usize(abi::OBJECT_ALIGN))) {
        Some(Ok(layout)) => layout,
        _ => fatal(format_args!("cannot allocate an object of {size} bytes")),
    }
}

thread_local! {
    /// The worklist of objects whose count reached zero and whose fields
    /// are still to be released. Kept per thread so that its capacity is
    /// reused across frees.
    static WORKLIST: Cell<Vec<Word>> = const { Cell::new(Vec::new()) };
}

/// Capacity above which the worklist is not kept for the next free.
const WORKLIST_KEEP: usize = 1 << 16;

/// Frees an object whose count just reached zero, then every object that
/// only its fields kept alive.
///
/// # Safety
///
/// `obj` points to a live heap object whose count has reached zero (no
/// references remain).
pub(crate) unsafe fn free(obj: *mut u8) {
    let word = object::ptr_to_word(obj);
    // During thread teardown the worklist may already be gone; a fresh
    // vector works the same.
    let mut list = WORKLIST.try_with(Cell::take).unwrap_or_default();
    list.push(word);
    // SAFETY: forwarded from the caller; every word pushed later is an
    // object whose count reached zero.
    unsafe { drain(&mut list) };
    list.clear();
    if list.capacity() <= WORKLIST_KEEP {
        WORKLIST.try_with(|cell| cell.set(list)).ok();
    }
}

/// Frees every object on the worklist and, transitively, the fields whose
/// count reaches zero.
///
/// # Safety
///
/// Every word on `list` is a heap object whose count has reached zero.
unsafe fn drain(list: &mut Vec<Word>) {
    while let Some(word) = list.pop() {
        let obj = word_to_ptr(word);
        // SAFETY: `obj` is a live object with no references left.
        let shape = unsafe { shape_of(obj) };
        let mut offset = shape.first_value;
        for _ in 0..shape.value_fields {
            // SAFETY: the shape lies within the object.
            let child = unsafe { read_field(obj, offset) };
            offset = offset.saturating_add(to_usize(abi::FIELD_SIZE));
            if abi::is_tagged_int(child) || child == 0 {
                continue;
            }
            let child_ptr = word_to_ptr(child);
            // SAFETY: a value field holds an owned reference to a live
            // object, which this object no longer needs.
            if unsafe { rc::release(child_ptr) } {
                list.push(child);
            }
        }
        // SAFETY: allocated by `alloc_object` with this size, which
        // `shape_of` recomputes exactly.
        unsafe { std::alloc::dealloc(obj, object_layout(shape.size)) };
        count_live(-1);
    }
}

/// Where an object's value fields are and how big it is.
struct Shape {
    first_value: usize,
    value_fields: u64,
    size: u64,
}

/// Computes an object's shape from its header and layout.
///
/// # Safety
///
/// `obj` points to a live object.
unsafe fn shape_of(obj: *mut u8) -> Shape {
    // SAFETY: forwarded from the caller.
    let (scan, id) = unsafe { scan_and_layout(obj) };
    let Some(info) = layout::get(id) else {
        fatal(format_args!("freeing an object with unknown layout {id}"));
    };
    let first = to_usize(abi::HEADER_SIZE);
    let uniform = |count: u64, first_value: usize| {
        let fields_end = to_u64(first_value).checked_add(abi::FIELD_SIZE.checked_mul(count)?)?;
        Some(Shape {
            first_value,
            value_fields: count,
            size: fields_end,
        })
    };
    let shape = match info.kind {
        LayoutKind::String | LayoutKind::Bytes => {
            // SAFETY: strings and bytes have `cap` at this offset.
            let cap = unsafe { read_field(obj, object::abi_offset(abi::string::CAP_OFFSET)) };
            abi::string::alloc_size(cap).map(|size| Shape {
                first_value: first,
                value_fields: 0,
                size,
            })
        }
        LayoutKind::Tuple if scan == abi::SCAN_ESCAPE => {
            // SAFETY: an escaped tuple holds its tagged arity at offset 8.
            let count = abi::untag_int(unsafe { read_field(obj, first) });
            u64::try_from(count)
                .ok()
                .and_then(|count| uniform(count, first.saturating_add(8)))
        }
        LayoutKind::Tuple | LayoutKind::Closure if scan != abi::SCAN_ESCAPE => {
            uniform(u64::from(scan), first)
        }
        LayoutKind::Tuple | LayoutKind::Closure => None,
        LayoutKind::BoxedInt
        | LayoutKind::BoxedU64
        | LayoutKind::BoxedFloat
        | LayoutKind::BoxedF32
        | LayoutKind::Ctor { .. } => info.fixed_size().map(|size| Shape {
            first_value: first,
            value_fields: u64::from(info.value_fields),
            size,
        }),
    };
    shape.unwrap_or_else(|| fatal(format_args!("freeing a malformed `{}` object", info.name)))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use super::*;
    use crate::abi::layout as ids;
    use crate::object::write_field;

    /// Serializes the tests that look at the global leak counter.
    pub(crate) fn serial() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Allocates a tuple of the given fields with count 1.
    pub(crate) fn tuple(fields: &[Word]) -> Word {
        let scan = u8::try_from(fields.len()).unwrap();
        let size = field_offset(fields.len());
        let obj = alloc_object(to_u64(size), abi::header(1, scan, ids::TUPLE));
        for (index, field) in fields.iter().enumerate() {
            // SAFETY: initializing the fields of a fresh tuple.
            unsafe { write_field(obj, field_offset(index), *field) };
        }
        object::ptr_to_word(obj)
    }

    /// The offset of field `index` (counting from 0 after the header).
    pub(crate) fn field_offset(index: usize) -> usize {
        index.checked_add(1).and_then(|n| n.checked_mul(8)).unwrap()
    }

    #[test]
    fn alloc_writes_the_header_and_counts_the_object() {
        let _guard = serial();
        let before = live_objects();
        let word = tuple(&[abi::tag_int(7).unwrap()]);
        assert_eq!(word % 8, 0);
        // SAFETY: a live tuple.
        unsafe {
            assert_eq!(object::load_rc(word_to_ptr(word)), 1);
            assert_eq!(scan_and_layout(word_to_ptr(word)), (1, ids::TUPLE));
        }
        if LEAK_CHECK_ENABLED {
            assert_eq!(live_objects(), before.wrapping_add(1));
        }
        // SAFETY: the only reference.
        unsafe { rc::drop(word) };
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn freeing_releases_every_field_once() {
        let _guard = serial();
        let before = live_objects();
        let shared = tuple(&[]);
        let inner = tuple(&[shared]);
        // SAFETY: a live object; the second reference goes into `outer`.
        unsafe { rc::dup(shared) };
        let outer = tuple(&[inner, shared, abi::tag_int(-3).unwrap(), object::nil()]);
        assert_eq!(
            live_objects().wrapping_sub(before),
            if LEAK_CHECK_ENABLED { 3 } else { 0 }
        );
        // SAFETY: the only reference to `outer`.
        unsafe { rc::drop(outer) };
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn freeing_a_long_chain_uses_no_stack() {
        let _guard = serial();
        let before = live_objects();
        let length = if cfg!(miri) { 1_000 } else { 1_000_000 };
        // A small stack: recursion per cell would overflow it quickly.
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || {
                let mut head = object::nil();
                for n in 0..length {
                    head = tuple(&[abi::tag_int(n).unwrap(), head]);
                }
                // SAFETY: the only reference to the head; each cell owns
                // the next.
                unsafe { rc::drop(head) };
            })
            .unwrap();
        handle.join().unwrap();
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn freeing_a_wide_tuple_with_an_escaped_scan_count() {
        let _guard = serial();
        let before = live_objects();
        let count = 300_usize;
        let children: Vec<Word> = (0..count).map(|_| tuple(&[])).collect();
        let size = to_u64(field_offset(count.wrapping_add(1)));
        let obj = alloc_object(size, abi::header(1, abi::SCAN_ESCAPE, ids::TUPLE));
        // SAFETY: initializing a fresh tuple: the tagged arity, then fields.
        unsafe {
            write_field(obj, 8, abi::tag_int(i64::try_from(count).unwrap()).unwrap());
            for (index, child) in children.iter().enumerate() {
                write_field(obj, field_offset(index.wrapping_add(1)), *child);
            }
            rc::drop(object::ptr_to_word(obj));
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn freeing_ctors_with_raw_fields_skips_them() {
        let _guard = serial();
        let before = live_objects();
        let info = layout::LayoutInfo::ctor(
            "t.Mixed",
            0,
            &[
                (layout::FieldKind::Int, None),
                (layout::FieldKind::Value, None),
            ],
        );
        let id = layout::register(info).unwrap();
        let child = tuple(&[]);
        let obj = alloc_object(24, abi::header(1, 1, id));
        // SAFETY: initializing a fresh object: the value field at 8, then
        // a raw `Int` that looks like a pointer (even) and must be skipped.
        unsafe {
            write_field(obj, 8, child);
            write_field(obj, 16, 0x1000);
            rc::drop(object::ptr_to_word(obj));
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn boxed_numbers_free_without_scanning() {
        let _guard = serial();
        let before = live_objects();
        for id in [
            ids::BOXED_INT,
            ids::BOXED_FLOAT,
            ids::BOXED_U64,
            ids::BOXED_F32,
        ] {
            let obj = alloc_object(16, abi::header(1, 0, id));
            // SAFETY: a fresh box; the payload is raw, then the only
            // reference is dropped.
            unsafe {
                write_field(obj, 8, 0x2000);
                rc::drop(object::ptr_to_word(obj));
            }
        }
        assert_eq!(live_objects(), before);
    }
}
