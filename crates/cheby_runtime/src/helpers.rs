//! Runtime helpers that generated code calls besides kernels (D-520,
//! plan §4.3.1): string building for interpolation and string patterns,
//! structural equality and debug text, boxing outside the tagged range,
//! and float remainder. A helper takes `ctx` first when it allocates, may
//! panic or may call back into Cheby code; arguments are borrowed unless a
//! helper's documentation says it consumes them, and results are owned.

use crate::abi::{PanicKind, Word};
use crate::ctx::ExecCtx;
use crate::generic::{self, Reached};
use crate::object;
use crate::panic::{fatal, report_of};
use crate::stats::{self, Event};
use crate::string;
use crate::unwind::guard::{DropOnUnwind, guarded};
use crate::unwind::raise::raise;
use crate::values;

/// `cheby_rt_str_with_capacity(ctx, cap)`: an empty unique `String` with
/// room for `cap` bytes (0 when `cap` is negative). Result owned.
///
/// # Safety
///
/// Callable from generated code with any arguments; `ctx` is null or the
/// running fiber's context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_str_with_capacity(ctx: *mut ExecCtx, cap: i64) -> Word {
    guarded(Some(ctx), || {
        let cap = u64::try_from(cap).unwrap_or(0);
        object::ptr_to_word(string::alloc_string(cap))
    })
}

/// `cheby_rt_str_append(ctx, acc, piece)`: `acc` followed by `piece`.
/// Consumes `acc`, updated in place when it is unique and has room and
/// moved or copied otherwise (D-316): to twice its capacity when unique,
/// twice its length when shared or static (GAP(text-copy-capacity),
/// refining D-577); borrows `piece`.
/// Result owned. It consumes `acc` even when it raises (D-698): `append`
/// raises only while it allocates, before it gives `acc` up.
///
/// # Safety
///
/// `acc` is an owned `String` (heap or static) and `piece` a borrowed live
/// `String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_str_append(ctx: *mut ExecCtx, acc: Word, piece: Word) -> Word {
    guarded(Some(ctx), || {
        let owned = DropOnUnwind::new([acc]);
        // SAFETY: forwarded from the caller.
        let Some(bytes) = (unsafe { string::bytes(piece) }) else {
            fatal(format_args!(
                "`cheby_rt_str_append` got a piece that is not a String"
            ));
        };
        // SAFETY: `acc` is live (the caller's contract) and checked to be a
        // text object before its fields are read.
        if unsafe { string::bytes(acc) }.is_some() {
            // SAFETY: as above.
            let path = unsafe { string::growth(acc, object::to_u64(bytes.len()), true) };
            stats::record(match path {
                string::Growth::InPlace => Event::StrAppendInPlace,
                string::Growth::Grown => Event::StrAppendGrown,
                string::Growth::Copied => Event::StrAppendCopied,
            });
        }
        if acc == piece {
            // The piece aliases the accumulator, which this call may grow or
            // free: copy it first. (The caller owns two references, so the
            // accumulator is shared and is copied anyway.)
            // The copy is an object, so the memory limit counts it (D-617).
            // It is released once it is appended, or by an unwinding.
            let copy = DropOnUnwind::new([string::new_text(bytes, crate::abi::layout::STRING)]);
            let [copy_word] = copy.words();
            // SAFETY: forwarded from the caller; the copy is a live object of
            // its own.
            let out =
                unsafe { string::append(acc, string::text_bytes(copy_word).unwrap_or_default()) };
            owned.disarm();
            drop(copy);
            return out;
        }
        // SAFETY: forwarded from the caller; `piece` is another object, so its
        // bytes stay valid while `acc` changes.
        let out = unsafe { string::append(acc, bytes) };
        owned.disarm();
        out
    })
}

/// `cheby_rt_str_eq(a, b)`: whether two borrowed `String`s hold the same
/// bytes (string literal patterns).
///
/// # Safety
///
/// `a` and `b` are live `String`s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_str_eq(a: Word, b: Word) -> bool {
    // SAFETY: forwarded from the caller.
    match unsafe { (string::bytes(a), string::bytes(b)) } {
        (Some(a), Some(b)) => a == b,
        _ => fatal(format_args!(
            "`cheby_rt_str_eq` got a value that is not a String"
        )),
    }
}

/// `cheby_rt_equal(ctx, a, b, location)`: structural `==` of two borrowed
/// uniform words (§3.13). Panics `EqualityOnFunction` at `location`, the
/// `==`'s source location as a static `String`, when it reaches a function
/// value or a handle (D-233, §11.6, D-556). A `location` of 0 is unknown.
/// A type with `@std_hooks` compares through its `equal` hook, called on
/// the running entry, whose context is `ctx` ([`crate::callback::call`]).
///
/// # Safety
///
/// `ctx` is the running entry's context; `a` and `b` are live uniform
/// words of one type; `location` is 0 or a live `String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_equal(
    ctx: *mut ExecCtx,
    a: Word,
    b: Word,
    location: Word,
) -> bool {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        match unsafe { generic::equal(a, b) } {
            Ok(same) => same,
            Err(Reached::Function) => {
                // SAFETY: `location` is 0 or a live string.
                let report =
                    unsafe { report_of(PanicKind::EqualityOnFunction.code(), 0, location) };
                raise(Some(ctx), report)
            }
        }
    })
}

/// `cheby_rt_debug(ctx, value)`: the debug text (§3.13) of a borrowed
/// uniform word, written straight into a new `String`, owned. A type with
/// `@std_hooks` prints through its `debug` hook, called on the running
/// entry, whose context is `ctx`.
///
/// # Safety
///
/// `value` is a live uniform word; `ctx` is the running entry's context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_debug(ctx: *mut ExecCtx, value: Word) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { generic::debug_object(value) })
}

/// `cheby_rt_box_i64(ctx, value)`: the uniform word of an `Int` or `I64`.
/// Generated code calls it only outside the tagged range, where it
/// allocates a `BoxedInt`; inside it, it tags.
///
/// # Safety
///
/// Callable with any arguments; `ctx` is null or the running fiber's
/// context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_box_i64(ctx: *mut ExecCtx, value: i64) -> Word {
    guarded(Some(ctx), || values::int_word(value))
}

/// `cheby_rt_box_u64(ctx, value)`: the uniform word of a `U64` (a
/// `BoxedU64` above 2^62 - 1, tagged otherwise).
///
/// # Safety
///
/// Callable with any arguments; `ctx` is null or the running fiber's
/// context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_box_u64(ctx: *mut ExecCtx, value: u64) -> Word {
    guarded(Some(ctx), || values::u64_word(value))
}

/// `cheby_rt_box_f64(ctx, value)`: a new `BoxedFloat`.
///
/// # Safety
///
/// Callable with any arguments; `ctx` is null or the running fiber's
/// context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_box_f64(ctx: *mut ExecCtx, value: f64) -> Word {
    guarded(Some(ctx), || values::f64_word(value))
}

/// `cheby_rt_box_f32(ctx, value)`: a new `BoxedF32`.
///
/// # Safety
///
/// Callable with any arguments; `ctx` is null or the running fiber's
/// context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_box_f32(ctx: *mut ExecCtx, value: f32) -> Word {
    guarded(Some(ctx), || values::f32_word(value))
}

/// `cheby_rt_fmod_f64(a, b)`: `a % b` on `Float`, the exact remainder of
/// truncating division with the sign of `a` (D-527).
#[unsafe(no_mangle)]
pub extern "C" fn cheby_rt_fmod_f64(a: f64, b: f64) -> f64 {
    a % b
}

/// `cheby_rt_fmod_f32(a, b)`: `a % b` on `F32` (D-527).
#[unsafe(no_mangle)]
pub extern "C" fn cheby_rt_fmod_f32(a: f32, b: f32) -> f32 {
    a % b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{self, layout};
    use crate::alloc::{self, live_objects};
    use crate::entry::{EntryConfig, EntryMode, EntryOutcome, run_entry};
    use crate::object::to_u64;
    use crate::rc;
    use crate::string::tests::StaticImage;

    /// The length of a `String`.
    fn len_of(word: Word) -> u64 {
        // SAFETY: tests pass live strings.
        to_u64(unsafe { string::bytes(word) }.map_or(0, <[u8]>::len))
    }

    #[test]
    fn strings_build_in_place_at_count_one() {
        let domain = alloc::Domain::new();
        alloc::with_domain(std::sync::Arc::clone(&domain), || {
            let null = std::ptr::null_mut();
            let hello = StaticImage::new("Hello, ");
            let name = StaticImage::new("world");
            // SAFETY: the helpers' ownership rules: `acc` passed along,
            // pieces borrowed.
            unsafe {
                let acc = cheby_rt_str_with_capacity(null, 12);
                let first = cheby_rt_str_append(null, acc, hello.word());
                assert_eq!(first, acc);
                let second = cheby_rt_str_append(null, first, name.word());
                assert_eq!(second, acc);
                assert_eq!(string::as_str(second), Some("Hello, world"));
                // A shared accumulator is copied, not written.
                rc::dup(second);
                let third = cheby_rt_str_append(null, second, hello.word());
                assert_ne!(third, second);
                assert_eq!(string::as_str(second), Some("Hello, world"));
                assert_eq!(string::as_str(third), Some("Hello, worldHello, "));
                // Appending a string to itself.
                rc::dup(third);
                let doubled = cheby_rt_str_append(null, third, third);
                assert_eq!(len_of(doubled), 38);
                // A static accumulator is copied too.
                let from_static = cheby_rt_str_append(null, hello.word(), name.word());
                assert_eq!(string::as_str(from_static), Some("Hello, world"));
                // Negative capacities count as zero.
                let empty = cheby_rt_str_with_capacity(null, -5);
                assert_eq!(len_of(empty), 0);
                for word in [second, third, doubled, from_static, empty] {
                    rc::drop(word);
                }
            }
        });
        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    }

    #[test]
    fn string_equality_compares_bytes() {
        let a = StaticImage::new("abc");
        let b = string::new_string("abc");
        let c = string::new_string("abd");
        // SAFETY: live strings.
        unsafe {
            assert!(cheby_rt_str_eq(a.word(), b));
            assert!(!cheby_rt_str_eq(b, c));
            assert!(cheby_rt_str_eq(c, c));
            rc::drop(b);
            rc::drop(c);
        }
    }

    #[test]
    fn box_helpers_build_canonical_words() {
        let before = live_objects();
        let null = std::ptr::null_mut();
        // SAFETY: the helpers accept any numbers; results are released.
        unsafe {
            let small = cheby_rt_box_i64(null, abi::INT63_MAX);
            assert!(abi::is_tagged_int(small));
            for (word, id) in [
                (cheby_rt_box_i64(null, i64::MIN), layout::BOXED_INT),
                (cheby_rt_box_u64(null, 1 << 62), layout::BOXED_U64),
                (cheby_rt_box_f64(null, -0.0), layout::BOXED_FLOAT),
                (cheby_rt_box_f32(null, 1.5), layout::BOXED_F32),
            ] {
                assert_eq!(values::layout_of(word), Some(id));
                rc::drop(word);
            }
            assert_eq!(values::word_to_u64(cheby_rt_box_u64(null, 7)), 7);
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn float_remainder_keeps_the_dividends_sign() {
        assert_eq!(cheby_rt_fmod_f64(5.5, 2.0).to_bits(), 1.5_f64.to_bits());
        assert_eq!(cheby_rt_fmod_f64(-5.5, 2.0).to_bits(), (-1.5_f64).to_bits());
        assert_eq!(cheby_rt_fmod_f64(-4.0, 2.0).to_bits(), (-0.0_f64).to_bits());
        assert!(cheby_rt_fmod_f64(1.0, 0.0).is_nan());
        assert_eq!(cheby_rt_fmod_f32(-7.0, 3.0).to_bits(), (-1.0_f32).to_bits());
    }

    unsafe extern "C" fn compares_closures(ctx: *mut ExecCtx) -> u64 {
        let closure = crate::generic::tests::closure(&[]);
        // SAFETY: the running entry's context and a live closure.
        let location = values::string("src/main.cheby:3:7");
        // SAFETY: the running entry's context, a live closure and a live
        // string.
        unsafe { u64::from(cheby_rt_equal(ctx, closure, closure, location)) }
    }

    unsafe extern "C" fn compares_and_prints(ctx: *mut ExecCtx) -> u64 {
        let a = values::some(values::int_word(3));
        let b = values::some(values::int_word(3));
        // SAFETY: live values of one type.
        unsafe {
            let same = cheby_rt_equal(ctx, a, b, 0);
            let text = cheby_rt_debug(ctx, a);
            let printed = string::as_str(text) == Some("Some(3)");
            for word in [a, b, text] {
                rc::drop(word);
            }
            u64::from(same && printed)
        }
    }

    #[test]
    fn equality_and_debug_through_the_c_entry_points() {
        // SAFETY: test thunks with the `EntryFn` ABI and static code.
        unsafe {
            let config = EntryConfig::new(EntryMode::Test);
            assert_eq!(
                run_entry(compares_and_prints, &config).unwrap(),
                EntryOutcome::Returned(1)
            );
            let EntryOutcome::Panicked(report) =
                run_entry(compares_closures, &EntryConfig::new(EntryMode::Test)).unwrap()
            else {
                panic!("comparing closures did not panic");
            };
            assert_eq!(report.kind, PanicKind::EqualityOnFunction);
            assert_eq!(
                report.message,
                "equality or hashing reached a function value or a handle"
            );
            // The `==`'s location, not `<unknown location>` (§11.6).
            assert_eq!(report.location, "src/main.cheby:3:7");
        }
    }
}
