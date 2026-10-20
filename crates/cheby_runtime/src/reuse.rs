//! Reuse (D-716, D-005): the runtime side of `drop_reuse` and
//! `alloc_reuse`.
//!
//! Generated code tests uniqueness inline: a `drop_reuse` of an object of
//! count exactly 1 calls [`cheby_rt_release_fields`] (when its layout has
//! value fields) and keeps the block as a token, a childless object of
//! count 1; any other count gives a reference up as `drop` does, and the
//! token is 0. A shared (negative count) or static (count 0) object is
//! never unique (I-2), so it never reuses. An `alloc_reuse` writes the
//! new header over a nonzero token, which charges nothing, since the
//! block was charged when it was allocated; with a 0 token it allocates
//! through [`cheby_rt_alloc_miss`]. The block stays in the domain that
//! allocated it: a unique object belongs to the running fiber's domain,
//! because every value that crosses fibers is shared first (D-072), so
//! each domain's charges stay balanced.
//!
//! The counters `reuse hits` and `reuse misses` of `CHEBY_RT_STATS`
//! (D-842, refining D-768 and D-635): a miss is counted by
//! the distinct allocation entry [`cheby_rt_alloc_miss`], which costs what
//! `cheby_rt_alloc` costs; a hit makes no call, so generated code loads
//! the counting flag ([`crate::stats::counting_flag`]) and calls
//! [`cheby_rt_reuse_hit`] out of line only while counting is on or not
//! yet settled: one load and one predictable branch per hit.
//!
//! With checked counting (`checked_rc`, poisoning mode) generated code
//! calls [`cheby_rt_drop_reuse_checked`] and
//! [`cheby_rt_alloc_reuse_checked`] instead, which check every header and
//! that the object size the compiler computed is the one the runtime frees
//! with, for the old object and the new one (the last guard of
//! `cheby_mir::layout::object_size`, D-838): a block freed
//! with another size than it was allocated with is undefined behavior.

use crate::abi::{self, Word};
use crate::alloc::{alloc_for, check_live, release_fields};
use crate::ctx::ExecCtx;
use crate::layout::{self, LayoutKind};
use crate::object::{load_rc, ptr_to_word, read_field, scan_and_layout, to_usize, word_to_ptr};
use crate::panic::fatal;
use crate::stats::{self, Event};

/// Releases the value fields of `obj`, a unique object of a constructor
/// or tuple layout that a `drop_reuse` keeps as its token, and stores 0 in
/// each, so the token owns nothing (D-716). The inline hit path of
/// generated code calls it when the layout has value fields.
///
/// # Safety
///
/// `ctx` is null or the control block of the fiber running on this
/// thread; `obj` is a live heap object of a constructor or tuple layout
/// whose count is 1, and the caller owns that reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_release_fields(ctx: *mut ExecCtx, obj: Word) {
    // SAFETY: forwarded from the caller.
    let released = unsafe { release_fields(ctx.as_ref(), word_to_ptr(obj)) };
    if released.is_none() {
        fatal(format_args!(
            "`drop_reuse` of an object that is not a constructor or a tuple"
        ));
    }
}

/// Frees `obj`, a unique object of a constructor or tuple layout whose
/// value fields the hit path of a drop specialized by constructor already
/// released or moved to its kept projections (D-716, D-850):
/// the block only, charged to the domain as a free is. Never raises
/// (D-171).
///
/// # Safety
///
/// `ctx` is null or the control block of the fiber running on this
/// thread; `obj` is a live heap object of a constructor or tuple layout
/// whose count is 1, the caller owns and gives up that reference, and
/// its value fields own nothing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_free_unique(ctx: *mut ExecCtx, obj: Word) {
    // SAFETY: forwarded from the caller.
    unsafe { check_live(obj, "a specialized drop") };
    // SAFETY: forwarded from the caller.
    let freed = unsafe { crate::alloc::free_unique(ctx.as_ref(), word_to_ptr(obj)) };
    if freed.is_none() {
        fatal(format_args!(
            "a specialized drop of an object that is not a constructor or a tuple"
        ));
    }
}

/// Counts one reuse hit (`CHEBY_RT_STATS`'s `reuse hits`) in the current
/// accounting domain when events are counted. Generated code calls it out
/// of line, only while the counting flag is not settled to off.
#[unsafe(no_mangle)]
pub extern "C" fn cheby_rt_reuse_hit() {
    stats::record(Event::ReuseHit);
}

/// `cheby_rt_alloc` for an `alloc_reuse` whose token is 0 (a reuse miss):
/// counts the miss when events are counted, then allocates an object of
/// `size` bytes with `header` as `cheby_rt_alloc` does, raising when the
/// request cannot be served (the result is then 0).
///
/// # Safety
///
/// As for `cheby_rt_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_alloc_miss(ctx: *mut ExecCtx, size: u64, header: u64) -> Word {
    crate::unwind::guard::guarded(Some(ctx), || {
        stats::record(Event::ReuseMiss);
        // SAFETY: forwarded from the caller.
        ptr_to_word(unsafe { alloc_for(ctx, size, header) })
    })
}

/// The checked `drop_reuse` (poisoning mode): checks that `value` is a
/// live object of a constructor or tuple layout whose allocation has
/// `size` bytes, the size the compiler computed (fatal otherwise); then,
/// when its count is 1, releases its value fields and returns it as the
/// token, and otherwise gives up one reference as `drop` does and returns
/// 0. Never raises (D-171).
///
/// # Safety
///
/// `ctx` is null or the control block of the fiber running on this
/// thread; `value` is a word pointing to a live object, and the caller
/// owns the reference it gives up.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_drop_reuse_checked(
    ctx: *mut ExecCtx,
    value: Word,
    size: u64,
) -> Word {
    if abi::is_tagged_int(value) || value == 0 {
        fatal(format_args!("`drop_reuse` of a word that is not an object"));
    }
    // SAFETY: forwarded from the caller.
    unsafe { check_live(value, "`drop_reuse`") };
    let obj = word_to_ptr(value);
    // Only the header's upper half, the scan count and layout, which are
    // never written after allocation: the count is the lower half, which
    // other fibers change atomically while the object is shared, so a
    // plain read of the whole word would race them (board item SZe-2 of
    // wave 2). The count is read atomically below, as `check_live` does.
    // SAFETY: a live object.
    let (scan, id) = unsafe { scan_and_layout(obj) };
    let header = abi::header(0, scan, id);
    // SAFETY: a live object.
    let old = unsafe { reuse_size(header, Some(obj)) };
    if old != Some(size) {
        fatal(format_args!(
            "`drop_reuse` of an object of {old:?} bytes as {size} bytes"
        ));
    }
    // SAFETY: a live object.
    if unsafe { load_rc(obj) } == abi::RC_UNIQUE {
        // SAFETY: a unique object of a constructor or tuple layout, whose
        // reference the caller owns.
        unsafe { release_fields(ctx.as_ref(), obj) };
        value
    } else {
        // SAFETY: forwarded from the caller.
        unsafe { crate::rc::cheby_rt_drop(ctx, value) };
        0
    }
}

/// The checked `alloc_reuse` (poisoning mode). With a 0 `token`, counts a
/// miss and allocates as `cheby_rt_alloc` does. Otherwise checks that the
/// token is a live object of count 1 whose value fields hold nothing,
/// that its allocation has `size` bytes and that `header` describes an
/// object of `size` bytes (fatal otherwise), counts a hit and writes
/// `header` over it. Returns the object, whose fields the caller stores;
/// 0 when the allocation raised.
///
/// # Safety
///
/// `ctx` is null or the running fiber's control block; `token` is 0 or a
/// token a `drop_reuse` returned, which the caller gives up; `header`
/// describes an object of `size` bytes, and the caller initializes every
/// field before the object is freed or read.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_alloc_reuse_checked(
    ctx: *mut ExecCtx,
    token: Word,
    size: u64,
    header: u64,
) -> Word {
    crate::unwind::guard::guarded(Some(ctx), || {
        if token == 0 {
            stats::record(Event::ReuseMiss);
            // SAFETY: forwarded from the caller.
            return ptr_to_word(unsafe { alloc_for(ctx, size, header) });
        }
        // SAFETY: forwarded from the caller.
        unsafe { check_token(token, size, header) };
        stats::record(Event::ReuseHit);
        // SAFETY: the token is a unique object of `size` bytes that the
        // caller gives up, and `header` describes an object of that size.
        unsafe { crate::object::write_field(word_to_ptr(token), 0, header) };
        token
    })
}

/// The checks of [`cheby_rt_alloc_reuse_checked`] on a nonzero token.
///
/// # Safety
///
/// `token` is a word a `drop_reuse` returned.
unsafe fn check_token(token: Word, size: u64, header: u64) {
    // SAFETY: forwarded from the caller.
    unsafe { check_live(token, "`alloc_reuse`") };
    let obj = word_to_ptr(token);
    // SAFETY: a live object.
    let (old_header, rc) = unsafe { (read_field(obj, 0), load_rc(obj)) };
    if rc != abi::RC_UNIQUE {
        fatal(format_args!("`alloc_reuse` of a token of count {rc}"));
    }
    // SAFETY: a live object.
    let old = unsafe { reuse_size(old_header, Some(obj)) };
    if old != Some(size) {
        fatal(format_args!(
            "`alloc_reuse` of a token of {old:?} bytes for {size} bytes"
        ));
    }
    // An escaped tuple's size depends on its count word, which the caller
    // stores after this returns; every other reusable header fixes it.
    // SAFETY: no object is read for a header alone.
    let new = unsafe { reuse_size(header, None) };
    let escaped_tuple = abi::header_scan(header) == abi::SCAN_ESCAPE
        && layout::get(abi::header_layout(header)).is_some_and(|i| i.kind == LayoutKind::Tuple);
    if new != Some(size) && !(new.is_none() && escaped_tuple) {
        fatal(format_args!(
            "`alloc_reuse` of a {size}-byte token for an object of {new:?} bytes"
        ));
    }
    // SAFETY: a live object of a reusable layout, so its value fields lie
    // within it.
    if !unsafe { fields_are_empty(old_header, obj) } {
        fatal(format_args!(
            "`alloc_reuse` of a token whose fields hold references"
        ));
    }
}

/// The size of the allocation of an object of `header`, rounded as the
/// allocator rounds it, when the header is a constructor's or a tuple's:
/// from the layout for a constructor, from the scan count for a tuple, and
/// from the count word of `obj` for an escaped tuple (`None` without
/// `obj`). `None` for every other layout.
///
/// # Safety
///
/// `obj`, when given, points to a live object with this header.
unsafe fn reuse_size(header: u64, obj: Option<*const u8>) -> Option<u64> {
    let scan = abi::header_scan(header);
    let info = layout::get(abi::header_layout(header))?;
    let size = match info.kind {
        LayoutKind::Ctor { .. } => info.fixed_size()?,
        LayoutKind::Tuple if scan != abi::SCAN_ESCAPE => {
            abi::HEADER_SIZE.checked_add(abi::FIELD_SIZE.checked_mul(u64::from(scan))?)?
        }
        LayoutKind::Tuple => {
            let obj = obj?;
            // SAFETY: an escaped tuple holds its tagged arity at offset 8
            // (forwarded from the caller).
            let count = abi::untag_int(unsafe { read_field(obj, to_usize(abi::HEADER_SIZE)) });
            let count = u64::try_from(count).ok()?;
            abi::HEADER_SIZE
                .checked_add(abi::FIELD_SIZE)?
                .checked_add(abi::FIELD_SIZE.checked_mul(count)?)?
        }
        _ => return None,
    };
    size.max(abi::HEADER_SIZE)
        .checked_next_multiple_of(abi::OBJECT_ALIGN)
}

/// Whether every value field of `obj`, whose header is `header`, holds 0.
///
/// # Safety
///
/// `obj` points to a live object of a constructor or tuple layout with
/// this header.
unsafe fn fields_are_empty(header: u64, obj: *const u8) -> bool {
    let scan = abi::header_scan(header);
    let Some(info) = layout::get(abi::header_layout(header)) else {
        return false;
    };
    let (first, count) = match info.kind {
        LayoutKind::Ctor { .. } => (abi::HEADER_SIZE, u64::from(info.value_fields)),
        LayoutKind::Tuple if scan != abi::SCAN_ESCAPE => (abi::HEADER_SIZE, u64::from(scan)),
        LayoutKind::Tuple => {
            // SAFETY: forwarded from the caller.
            let count = abi::untag_int(unsafe { read_field(obj, to_usize(abi::HEADER_SIZE)) });
            (
                abi::HEADER_SIZE.saturating_add(abi::FIELD_SIZE),
                u64::try_from(count).unwrap_or(0),
            )
        }
        _ => return false,
    };
    (0..count).all(|n| {
        let offset = first.saturating_add(abi::FIELD_SIZE.saturating_mul(n));
        // SAFETY: a value field of the object (forwarded from the caller).
        unsafe { read_field(obj, to_usize(offset)) == 0 }
    })
}

/// The size [`reuse_size`] gives the live object `value`, for tests.
#[cfg(test)]
pub(crate) fn size_of_object(value: Word) -> Option<u64> {
    let obj = word_to_ptr(value);
    // SAFETY: tests pass live objects.
    unsafe { reuse_size(read_field(obj, 0), Some(obj)) }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::alloc::Domain;
    use crate::alloc::tests::tuple;

    fn int(n: i64) -> Word {
        abi::tag_int(n).unwrap()
    }

    /// A hit keeps the block, releases what its fields held and leaves the
    /// domain's count balanced; a miss on a value of count 2 gives up one
    /// reference and returns 0; the checked sizes are the runtime's.
    #[test]
    fn reuse_checked_paths_keep_the_block_or_give_up_a_reference() {
        let domain = Domain::new();
        crate::alloc::with_domain(Arc::clone(&domain), || {
            let inner = tuple(&[int(1)]);
            let outer = tuple(&[inner]);
            assert_eq!(size_of_object(outer), Some(16));
            // SAFETY: `outer` is a live unique tuple this test owns.
            let token = unsafe { cheby_rt_drop_reuse_checked(std::ptr::null_mut(), outer, 16) };
            assert_eq!(token, outer, "a unique object is kept");
            // `inner` was freed with the field that held it.
            assert_eq!(domain.live_objects(), 1);
            let header = abi::header(abi::RC_UNIQUE, 1, abi::layout::TUPLE);
            // SAFETY: the token of a 16-byte tuple, written over as a
            // tuple of one field, which the test stores.
            let again =
                unsafe { cheby_rt_alloc_reuse_checked(std::ptr::null_mut(), token, 16, header) };
            assert_eq!(again, token);
            // SAFETY: the object's one field, then its last reference.
            unsafe {
                crate::object::write_field(word_to_ptr(again), 8, int(2));
                crate::rc::cheby_rt_drop(std::ptr::null_mut(), again);
            }
            assert_eq!(domain.live_objects(), 0);
            let shared = tuple(&[int(3)]);
            // SAFETY: a live object; the test then holds two references.
            unsafe { crate::rc::dup(shared) };
            // SAFETY: one of the two references, given up.
            let token = unsafe { cheby_rt_drop_reuse_checked(std::ptr::null_mut(), shared, 16) };
            assert_eq!(token, 0, "a value of count 2 misses");
            assert_eq!(domain.live_objects(), 1);
            // SAFETY: the other reference.
            unsafe { crate::rc::cheby_rt_drop(std::ptr::null_mut(), shared) };
            assert_eq!(domain.live_objects(), 0);
        });
        assert_eq!(domain.live_objects(), 0);
    }

    /// A static `Some` (D-902) is never unique, so its `drop_reuse` gives
    /// no token and writes nothing, and the `alloc_reuse` that follows
    /// allocates a new object.
    #[test]
    fn reuse_of_a_static_some_falls_back_to_allocating() {
        let domain = Domain::new();
        crate::alloc::with_domain(Arc::clone(&domain), || {
            let some = crate::values::some_u8(7);
            assert_eq!(size_of_object(some), Some(16));
            // SAFETY: a live static object; giving up a reference to it
            // writes nothing.
            let token = unsafe { cheby_rt_drop_reuse_checked(std::ptr::null_mut(), some, 16) };
            assert_eq!(token, 0, "a static object misses");
            // SAFETY: as above.
            unsafe {
                assert_eq!(load_rc(word_to_ptr(some)), abi::RC_STATIC);
                assert_eq!(read_field(word_to_ptr(some), 8), int(7));
            }
            let header = abi::header(abi::RC_UNIQUE, 1, abi::layout::SOME);
            // SAFETY: a 0 token allocates a 16-byte `Some`, whose field the
            // test stores.
            let fresh =
                unsafe { cheby_rt_alloc_reuse_checked(std::ptr::null_mut(), 0, 16, header) };
            assert_ne!(fresh, some);
            assert_eq!(domain.live_objects(), 1);
            // SAFETY: the new object's one field, then its last reference.
            unsafe {
                crate::object::write_field(word_to_ptr(fresh), 8, int(8));
                crate::rc::cheby_rt_drop(std::ptr::null_mut(), fresh);
            }
            assert_eq!(domain.live_objects(), 0);
            // SAFETY: as above.
            assert_eq!(unsafe { read_field(word_to_ptr(some), 8) }, int(7));
        });
    }

    /// Board item SZe-2 (wave 2): the checked `drop_reuse` of a shared
    /// object reads its count only atomically, so a fiber on another
    /// worker that changes the count meanwhile does not race the check. It
    /// read the whole header with a plain 8-byte load, which overlaps the
    /// count, and the thread sanitizer reported that against the count's
    /// atomic updates (the `thread` `conformance` step, 018, 022 and 009 at
    /// 2 and 4 threads). This test is that case for the sanitizer's
    /// `runtime` step; without a sanitizer it checks the counts.
    #[test]
    fn reuse_checked_drop_of_a_shared_object_races_no_count_update() {
        const ROUNDS: usize = 20_000;
        let domain = Domain::new();
        crate::alloc::with_domain(Arc::clone(&domain), || {
            let shared = tuple(&[int(4)]);
            // SAFETY: a live object this thread owns; its count is atomic
            // from now on, as for a value a spawn shares (D-705).
            unsafe { crate::rc::mark_shared(shared) };
            let other = std::thread::spawn(move || {
                for _ in 0..ROUNDS {
                    // SAFETY: the main thread holds a reference until it
                    // has joined this thread; this one takes and gives up
                    // its own.
                    unsafe {
                        crate::rc::dup(shared);
                        crate::rc::cheby_rt_drop(std::ptr::null_mut(), shared);
                    }
                }
            });
            for _ in 0..ROUNDS {
                // SAFETY: a second reference to the live object, which the
                // checked `drop_reuse` gives up: the base reference keeps
                // the count above 1, so it is a miss.
                let token = unsafe {
                    crate::rc::dup(shared);
                    cheby_rt_drop_reuse_checked(std::ptr::null_mut(), shared, 16)
                };
                assert_eq!(token, 0, "a shared value misses");
            }
            other.join().unwrap();
            assert_eq!(domain.live_objects(), 1);
            // SAFETY: the base reference, the last one.
            unsafe { crate::rc::cheby_rt_drop(std::ptr::null_mut(), shared) };
            assert_eq!(domain.live_objects(), 0);
        });
    }
}
