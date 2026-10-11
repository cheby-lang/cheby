//! `dup` and `drop` (D-273): the reference-count operations generated code
//! calls. Both skip tagged integers with one bit test (D-278) and static
//! objects (count 0, §9.7) without writing them.
//!
//! A positive count is local to its fiber and updated with plain-cost
//! relaxed accesses. A negative count `-n` is shared (M2, D-072): `n`
//! references, updated atomically. In M1 no object is ever shared, but the
//! paths exist so that the counting rules are fixed from the start.

use std::sync::atomic::{Ordering, fence};

use crate::abi::{self, Word};
use crate::alloc;
use crate::object::{load_rc, shared_rc, store_rc, word_to_ptr};

/// Adds a reference to `value`.
///
/// # Safety
///
/// `value` is a tagged integer or a word pointing to a live object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_dup(value: Word) {
    // SAFETY: forwarded from the caller.
    unsafe { dup(value) };
}

/// Removes a reference to `value`, freeing it when none remain.
///
/// # Safety
///
/// `value` is a tagged integer or a word pointing to a live object, and the
/// caller owns the reference it gives up.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_drop(value: Word) {
    // SAFETY: forwarded from the caller.
    unsafe { drop(value) };
}

/// Adds a reference to `value`. See [`cheby_rt_dup`]. The common case, a
/// local count below the maximum, is inlined; the rest is `dup_slow`.
///
/// # Safety
///
/// As for [`cheby_rt_dup`].
#[inline]
pub unsafe fn dup(value: Word) {
    if abi::is_tagged_int(value) || value == 0 {
        return;
    }
    // SAFETY: forwarded from the caller.
    unsafe { alloc::check_live(value, "`dup`") };
    let obj = word_to_ptr(value);
    // SAFETY: `obj` is a live object.
    let rc = unsafe { load_rc(obj) };
    if let Some(next) = rc.checked_add(1).filter(|_| rc > 0) {
        // SAFETY: a positive count is local to this thread.
        unsafe { store_rc(obj, next) };
        return;
    }
    // SAFETY: as above.
    unsafe { dup_slow(obj, rc) };
}

/// [`dup`] of an object whose count `rc` is static, shared or at its
/// maximum.
///
/// # Safety
///
/// `obj` points to a live object whose count is `rc`.
#[inline(never)]
unsafe fn dup_slow(obj: *mut u8, rc: i32) {
    if rc > 0 {
        // The count cannot grow further: the object becomes static and is
        // never freed (D-454).
        // SAFETY: a live object whose positive count is local to this
        // thread.
        unsafe {
            store_rc(obj, abi::RC_STATIC);
            alloc::leave_domain(obj);
        }
    } else if rc < 0 {
        // SAFETY: a negative count marks a live shared heap object.
        let cell = unsafe { shared_rc(obj) };
        let update = cell.try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |current| match current {
                abi::RC_STATIC => None,
                // Overflow: the object becomes static.
                i32::MIN => Some(abi::RC_STATIC),
                _ => current.checked_sub(1),
            },
        );
        if update == Ok(i32::MIN) {
            // SAFETY: the object just became static.
            unsafe { alloc::leave_domain(obj) };
        }
    }
}

/// Removes a reference to `value`, freeing it when none remain. See
/// [`cheby_rt_drop`].
///
/// # Safety
///
/// As for [`cheby_rt_drop`].
#[inline]
pub unsafe fn drop(value: Word) {
    if abi::is_tagged_int(value) || value == 0 {
        return;
    }
    // SAFETY: forwarded from the caller.
    unsafe { alloc::check_live(value, "`drop`") };
    let obj = word_to_ptr(value);
    // SAFETY: `obj` is a live object and the caller gives up a reference.
    if unsafe { release(obj) } {
        // SAFETY: that was the last reference.
        unsafe { alloc::free(obj) };
    }
}

/// Whether `value` is a heap object with exactly one reference (a local
/// count of 1), so its owner may update it in place (D-316, D-525). Never
/// true for tagged integers, 0, static objects or shared objects.
///
/// # Safety
///
/// `value` is a tagged integer, 0, or a word pointing to a live object.
#[must_use]
pub unsafe fn is_unique(value: Word) -> bool {
    if abi::is_tagged_int(value) || value == 0 {
        return false;
    }
    // SAFETY: forwarded from the caller.
    unsafe { alloc::check_live(value, "a uniqueness check") };
    // SAFETY: as above.
    unsafe { load_rc(word_to_ptr(value)) == abi::RC_UNIQUE }
}

/// Removes one reference from an object, and returns whether that was the
/// last one, in which case the caller must free it.
///
/// # Safety
///
/// `obj` points to a live object and the caller gives up a reference it
/// owns.
#[inline]
pub(crate) unsafe fn release(obj: *mut u8) -> bool {
    // SAFETY: forwarded from the caller.
    let rc = unsafe { load_rc(obj) };
    match rc {
        abi::RC_STATIC => false,
        1 => true,
        2.. => {
            // SAFETY: a positive count is local to this thread.
            unsafe { store_rc(obj, rc.saturating_sub(1)) };
            false
        }
        _ => {
            // SAFETY: a negative count marks a live shared heap object.
            let previous = unsafe { shared_rc(obj) }.fetch_add(1, Ordering::Release);
            if previous == -1 {
                // Every other thread's last use happens before the free.
                fence(Ordering::Acquire);
                true
            } else {
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::tests::{serial, tuple};
    use crate::alloc::{LEAK_CHECK_ENABLED, live_objects};
    use crate::object::cheby_rt_static_true;
    use crate::string::tests::StaticImage;

    fn count(word: Word) -> i32 {
        // SAFETY: tests only pass live objects.
        unsafe { load_rc(word_to_ptr(word)) }
    }

    #[test]
    fn tagged_ints_are_skipped() {
        for n in [0, -1, abi::INT63_MAX, abi::INT63_MIN] {
            let word = abi::tag_int(n).unwrap();
            // SAFETY: tagged integers are never dereferenced.
            unsafe {
                cheby_rt_dup(word);
                cheby_rt_drop(word);
            }
        }
    }

    #[test]
    fn static_objects_are_never_written() {
        let word = cheby_rt_static_true.word();
        let before = cheby_rt_static_true.header();
        // SAFETY: a static object; count operations are no-ops.
        unsafe {
            cheby_rt_dup(word);
            cheby_rt_drop(word);
            cheby_rt_drop(word);
        }
        assert_eq!(cheby_rt_static_true.header(), before);
        let image = StaticImage::new("static");
        // SAFETY: a static string object in a live buffer.
        unsafe {
            cheby_rt_dup(image.word());
            cheby_rt_drop(image.word());
            assert_eq!(load_rc(word_to_ptr(image.word())), abi::RC_STATIC);
        }
    }

    #[test]
    fn heap_counts_go_up_and_down() {
        let _guard = serial();
        let before = live_objects();
        let word = tuple(&[]);
        // SAFETY: a live tuple; two references added, three dropped.
        unsafe {
            cheby_rt_dup(word);
            cheby_rt_dup(word);
            assert_eq!(count(word), 3);
            cheby_rt_drop(word);
            cheby_rt_drop(word);
            assert_eq!(count(word), 1);
            cheby_rt_drop(word);
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn uniqueness_is_a_count_of_exactly_one() {
        let word = tuple(&[]);
        let image = StaticImage::new("static");
        // SAFETY: live objects and a tagged integer.
        unsafe {
            assert!(is_unique(word));
            dup(word);
            assert!(!is_unique(word));
            drop(word);
            assert!(is_unique(word));
            assert!(!is_unique(abi::tag_int(1).unwrap()));
            assert!(!is_unique(0));
            assert!(!is_unique(cheby_rt_static_true.word()));
            assert!(!is_unique(image.word()));
            store_rc(word_to_ptr(word), -1);
            assert!(!is_unique(word));
            store_rc(word_to_ptr(word), 1);
            drop(word);
        }
    }

    #[test]
    fn count_overflow_makes_the_object_static() {
        let _guard = serial();
        let before = live_objects();
        let word = tuple(&[]);
        // SAFETY: a live tuple whose count is set just below the limit.
        unsafe {
            store_rc(word_to_ptr(word), i32::MAX);
            cheby_rt_dup(word);
            assert_eq!(count(word), abi::RC_STATIC);
            cheby_rt_drop(word);
            assert_eq!(count(word), abi::RC_STATIC);
        }
        // Static now: never freed and no longer counted.
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn shared_counts_are_negative_and_free_at_zero() {
        let _guard = serial();
        let before = live_objects();
        let word = tuple(&[]);
        // SAFETY: a live tuple marked shared by hand (M2's marking).
        unsafe {
            store_rc(word_to_ptr(word), -1);
            cheby_rt_dup(word);
            assert_eq!(count(word), -2);
            cheby_rt_drop(word);
            assert_eq!(count(word), -1);
            cheby_rt_drop(word);
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn shared_count_overflow_makes_the_object_static() {
        let _guard = serial();
        let before = live_objects();
        let word = tuple(&[]);
        // SAFETY: a live tuple marked shared with the most references.
        unsafe {
            store_rc(word_to_ptr(word), i32::MIN);
            cheby_rt_dup(word);
            assert_eq!(count(word), abi::RC_STATIC);
        }
        assert_eq!(live_objects(), before);
        assert!(LEAK_CHECK_ENABLED || before == 0);
    }
}
