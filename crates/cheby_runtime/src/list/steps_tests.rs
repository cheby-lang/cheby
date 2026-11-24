//! Tests of the steps of a direct `list::sort_with`: driven with a
//! comparison written in Rust, they give D-1448's merge sort on a slice
//! for any comparison, and release what they hold.

#![expect(
    clippy::arithmetic_side_effects,
    reason = "test arithmetic on small, known values"
)]
#![expect(clippy::cast_ptr_alignment, reason = "a step's pair is 8-aligned")]

use super::*;
use crate::alloc::live;

/// A comparison of two `Int`s, as the tag of its `Order`.
type Compare = fn(i64, i64) -> i64;

/// The tag of `Ordering`'s `Order`.
fn tag(order: std::cmp::Ordering) -> i64 {
    match order {
        std::cmp::Ordering::Less => 0,
        std::cmp::Ordering::Equal => 1,
        std::cmp::Ordering::Greater => 2,
    }
}

/// The comparisons the tests sort with: in order, reversed, always
/// `Equal`, always `Greater`, and one that is no consistent order.
const COMPARES: [Compare; 5] = [
    |a, b| tag(a.cmp(&b)),
    |a, b| tag(b.cmp(&a)),
    |_, _| 1,
    |_, _| 2,
    |a, b| (a * 7 + b * 3).rem_euclid(3),
];

/// Pseudo-random values with many repeats.
fn scrambled(n: i64) -> Vec<i64> {
    (0..n).map(|i| (i * 7_919 + 13) % 211 - 100).collect()
}

/// D-1448's merge sort of `values` on a slice.
fn model(values: &[i64], compare: Compare) -> Vec<i64> {
    let len = values.len();
    let mut from = values.to_vec();
    let mut into = from.clone();
    let mut width = 1;
    while width < len {
        let mut start = 0;
        while start < len {
            let mid = (start + width).min(len);
            let end = (mid + width).min(len);
            let (mut left, mut right) = (start, mid);
            for slot in into.iter_mut().take(end).skip(start) {
                let take_right = match (from.get(left), from.get(right)) {
                    (Some(&first), Some(&second)) if left < mid && right < end => {
                        compare(first, second) == GREATER
                    }
                    _ => left >= mid,
                };
                let next = if take_right { &mut right } else { &mut left };
                *slot = from.get(*next).copied().unwrap();
                *next += 1;
            }
            start = end;
        }
        std::mem::swap(&mut from, &mut into);
        width *= 2;
    }
    from
}

/// The steps of a direct `sort_with` of `values`, driven with `compare`.
fn stepped(values: &[i64], compare: Compare) -> Vec<i64> {
    let xs = super::super::from_iter(values.iter().map(|&v| abi::tag_int(v).unwrap()));
    let ctx = std::ptr::null_mut();
    // SAFETY: a live list; no fiber runs here.
    let st = unsafe { cheby_rt_sort_start(ctx, xs) };
    // SAFETY: the state just made, held only here.
    let mut at = unsafe { cheby_rt_buffer_step(st, -1) };
    while at != 0 {
        // SAFETY: a step's pair of live tagged `Int`s.
        let [a, b] = unsafe { *word_to_ptr(at).cast::<Pair>() };
        let order = compare(abi::untag_int(a), abi::untag_int(b));
        // SAFETY: as above.
        at = unsafe { cheby_rt_buffer_step(st, order) };
    }
    // SAFETY: the state is done, and `xs` is the list it was given.
    let sorted = unsafe { cheby_rt_buffer_finish(ctx, st, xs) };
    // SAFETY: a live list of tagged `Int`s.
    let out = unsafe { super::super::iter(sorted) }
        .map(abi::untag_int)
        .collect();
    drop_word(sorted);
    drop_word(xs);
    out
}

#[test]
fn the_steps_sort_as_the_merge_sort_of_a_slice() {
    let before = live();
    let sizes: &[i64] = if cfg!(miri) {
        &[0, 1, 2, 3, 5, 33]
    } else {
        &[0, 1, 2, 3, 5, 31, 32, 33, 34, 1_000, 1_025]
    };
    for &n in sizes {
        let values = scrambled(n);
        for compare in COMPARES {
            assert_eq!(
                stepped(&values, compare),
                model(&values, compare),
                "n = {n}"
            );
        }
        let mut sorted = values.clone();
        sorted.sort_unstable();
        assert_eq!(stepped(&values, COMPARES[0]), sorted, "n = {n}");
    }
    assert_eq!(live(), before, "objects leaked or freed twice");
}

/// A comparison that stops the sort, as a panic does, leaves a state that
/// the cleanup releases like any value: nothing leaks, and the list keeps
/// its elements.
#[test]
fn a_sort_stopped_midway_releases_its_state() {
    let before = live();
    let values = scrambled(100);
    let xs = super::super::from_iter(values.iter().map(|&v| abi::tag_int(v).unwrap()));
    // SAFETY: a live list; no fiber runs here.
    let st = unsafe { cheby_rt_sort_start(std::ptr::null_mut(), xs) };
    for _ in 0..40 {
        // SAFETY: the state just made, held only here.
        let at = unsafe { cheby_rt_buffer_step(st, 2) };
        assert_ne!(at, 0);
    }
    drop_word(st);
    // SAFETY: a live list of tagged `Int`s.
    let kept: Vec<i64> = unsafe { super::super::iter(xs) }
        .map(abi::untag_int)
        .collect();
    assert_eq!(kept, values);
    drop_word(xs);
    assert_eq!(live(), before, "objects leaked or freed twice");
}

/// A list of fewer than two elements has no state, and the finish gives
/// the list itself back, as the kernel does.
#[test]
fn a_short_list_is_its_own_result() {
    let xs = super::super::from_iter([abi::tag_int(7).unwrap()]);
    // SAFETY: a live list; no fiber runs here.
    let st = unsafe { cheby_rt_sort_start(std::ptr::null_mut(), xs) };
    assert_eq!(st, NOTHING);
    // SAFETY: as above.
    unsafe {
        assert_eq!(cheby_rt_buffer_step(st, -1), 0);
        assert_eq!(cheby_rt_buffer_finish(std::ptr::null_mut(), st, xs), xs);
    }
    drop_word(xs);
    drop_word(xs);
}
