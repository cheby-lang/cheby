//! Tests of lists: unit tests, property tests against `Vec` (testing.md
//! §3, plan §6), the multi-version state machine, deep freeing, the C
//! entry points with their ownership, and the costs of
//! `docs/stdlib/list.md`.
//!
//! Tests run with poisoning on (every `cfg(test)` build), and each one
//! checks that its thread's accounting domain returns to where it started.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "test arithmetic on small, known values"
)]

use proptest::prelude::*;

use super::kernels::*;
use super::node::{M, Node, Root, View, drop_word, dup_word, empty};
use super::{from_iter, iter, ops, tree};
use crate::abi::{self, Word};
use crate::alloc::live;
use crate::string::{new_string, to_rust_string};

// ---------------------------------------------------------------------
// Elements and models

/// The element for model value `v`: a heap `String` for multiples of 3, so
/// element ownership is checked by the accounting, a tagged `Int`
/// otherwise. Owned.
fn elem(v: i64) -> Word {
    if v % 3 == 0 {
        new_string(&v.to_string())
    } else {
        abi::tag_int(v).unwrap()
    }
}

/// The model value of an element.
fn value(word: Word) -> i64 {
    if abi::is_tagged_int(word) {
        return abi::untag_int(word);
    }
    // SAFETY: elements are live strings.
    unsafe { to_rust_string(word) }.unwrap().parse().unwrap()
}

/// A list of the model values, owned.
fn list(values: &[i64]) -> Root {
    root(from_iter(values.iter().map(|&v| elem(v))))
}

fn root(word: Word) -> Root {
    // SAFETY: tests pass live lists only.
    unsafe { Root::from_word(word) }
}

/// The elements of a borrowed list, read with the iterator.
fn contents(list: Root) -> Vec<i64> {
    // SAFETY: a live list.
    unsafe { iter(list.word()) }.map(value).collect()
}

/// Checks a borrowed list against its model: length, iteration, every
/// `get` on short lists and a sample on long ones, and the tree's
/// invariants.
fn check(list: Root, model: &[i64]) {
    assert_eq!(ops::len(list), model.len());
    assert_eq!(contents(list), model);
    let step = (model.len() / 64).max(1);
    for i in (0..model.len()).step_by(step) {
        assert_eq!(ops::get(list, i).map(value), Some(model[i]), "get({i})");
    }
    if let Some(&last) = model.last() {
        assert_eq!(ops::get(list, model.len() - 1).map(value), Some(last));
    }
    assert_eq!(ops::get(list, model.len()), None);
    let view = list.view();
    if model.is_empty() {
        assert_eq!(list.word(), empty(), "the empty list is the static one");
        return;
    }
    let tree = view.tree.expect("a non-empty list has a tree");
    assert!(view.offset + view.len <= tree.len());
    check_tree(tree);
}

/// The structural invariants of a tree; returns its length.
fn check_tree(node: Node) -> usize {
    let mut stack = vec![node];
    let mut total = 0;
    let top = node.len();
    while let Some(node) = stack.pop() {
        if node.is_leaf() {
            assert!(node.slots() >= 1 && node.slots() <= node.cap() && node.cap() <= M);
            total += node.slots();
            continue;
        }
        let count = node.slots();
        assert!((1..=M).contains(&count), "branch with {count} children");
        let mut size = 0;
        for i in 0..count {
            let child = node.child(i);
            assert_eq!(child.height() + 1, node.height(), "uneven heights");
            size += child.len();
            assert_eq!(node.size_at(i), size, "size table");
            stack.push(child);
        }
    }
    assert_eq!(total, top);
    total
}

/// The height of a list's tree (0 for a leaf or the empty list).
fn height(list: Root) -> usize {
    list.view().tree.map_or(0, Node::height)
}

/// Runs `f` and checks that the domain's live count returns to its start.
fn balanced(f: impl FnOnce()) {
    let before = live();
    f();
    assert_eq!(live(), before, "objects leaked or freed twice");
}

fn release(list: Root) {
    drop_word(list.word());
}

// ---------------------------------------------------------------------
// Unit tests

#[test]
fn the_empty_list_is_static() {
    balanced(|| {
        let nothing = list(&[]);
        assert_eq!(nothing.word(), empty());
        assert_eq!(
            nothing.word(),
            std::ptr::from_ref(&super::cheby_rt_static_empty_list).addr() as u64
        );
        // SAFETY: a static object.
        assert_eq!(
            unsafe { crate::object::load_rc(abi_ptr(nothing.word())) },
            abi::RC_STATIC
        );
        check(nothing, &[]);
        release(nothing);
    });
}

fn abi_ptr(word: Word) -> *mut u8 {
    crate::object::word_to_ptr(word)
}

#[test]
fn every_operation_on_small_lists() {
    balanced(|| {
        let xs = list(&[1, 2, 3, 4, 5]);
        check(xs, &[1, 2, 3, 4, 5]);
        dup_word(xs.word());
        let pushed = ops::push(xs, elem(6));
        check(pushed, &[1, 2, 3, 4, 5, 6]);
        check(xs, &[1, 2, 3, 4, 5]);
        dup_word(xs.word());
        let prepended = ops::prepend(xs, elem(0));
        check(prepended, &[0, 1, 2, 3, 4, 5]);
        dup_word(xs.word());
        let set = ops::set(xs, 2, elem(30));
        check(set, &[1, 2, 30, 4, 5]);
        dup_word(xs.word());
        let unchanged = ops::set(xs, 5, elem(9));
        assert_eq!(unchanged, xs);
        release(unchanged);
        let sliced = ops::slice(xs, 1, 4);
        check(sliced, &[2, 3, 4]);
        let nested = ops::slice(sliced, 1, 10);
        check(nested, &[3, 4]);
        dup_word(sliced.word());
        let joined = ops::append(sliced, nested);
        check(joined, &[2, 3, 4, 3, 4]);
        let empty = ops::slice(xs, 3, 2);
        check(empty, &[]);
        for list in [pushed, prepended, set, sliced, nested, joined, empty, xs] {
            release(list);
        }
    });
}

#[test]
fn clamping_follows_d298() {
    assert_eq!(ops::clamp(-1, 5), 0);
    assert_eq!(ops::clamp(3, 5), 3);
    assert_eq!(ops::clamp(9, 5), 5);
    assert_eq!(ops::clamp(i64::MIN, 0), 0);
    assert_eq!(ops::clamp(i64::MAX, 7), 7);
}

#[test]
fn a_unique_slice_view_reaching_past_its_end_is_updated_in_place() {
    balanced(|| {
        let model: Vec<i64> = (0..2_000).collect();
        let xs = list(&model);
        let view = ops::slice(xs, 100, 1_100);
        release(xs);
        // The view is unique, and its tree (now unique too) holds 900
        // elements past the view's end, which a push must not expose.
        assert!(view.is_unique());
        let pushed = ops::push(view, elem(-1));
        assert_eq!(pushed, view, "pushed in place");
        let mut expected: Vec<i64> = (100..1_100).collect();
        expected.push(-1);
        check(pushed, &expected);
        let set = ops::set(pushed, 0, elem(-2));
        assert_eq!(set, pushed, "set in place");
        expected[0] = -2;
        check(set, &expected);
        release(set);
    });
}

#[test]
fn a_shared_tree_under_a_unique_view_is_copied_not_changed() {
    balanced(|| {
        let model: Vec<i64> = (0..2_000).collect();
        let xs = list(&model);
        let view = ops::slice(xs, 100, 1_100);
        // `xs` keeps the tree alive: the view's root is unique, its tree is
        // shared.
        let pushed = ops::push(view, elem(-1));
        let set = ops::set(pushed, 5, elem(-2));
        let mut expected: Vec<i64> = (100..1_100).collect();
        expected.push(-1);
        expected[5] = -2;
        check(set, &expected);
        check(xs, &model);
        release(set);
        release(xs);
    });
}

#[test]
fn push_and_set_are_in_place_at_count_one_and_copy_when_shared() {
    balanced(|| {
        let mut xs = list(&[]);
        let mut model = Vec::new();
        let mut first_heap_root = None;
        for v in 0..5_000 {
            xs = ops::push(xs, elem(v));
            model.push(v);
            match first_heap_root {
                None => first_heap_root = Some(xs),
                Some(first) => assert_eq!(xs, first, "push at count one keeps the root"),
            }
        }
        check(xs, &model);
        // A unique leaf is updated where it is.
        let before = super::node::Node::word(xs.view().tree.unwrap());
        xs = ops::set(xs, 4_000, elem(-4));
        model[4_000] = -4;
        assert_eq!(
            xs.view().tree.unwrap().word(),
            before,
            "set at count one keeps the tree"
        );
        // Shared: the copy changes, the original does not.
        dup_word(xs.word());
        let copy = ops::set(xs, 10, elem(-10));
        assert_ne!(copy, xs);
        let mut copied = model.clone();
        copied[10] = -10;
        check(copy, &copied);
        check(xs, &model);
        dup_word(xs.word());
        let pushed = ops::push(xs, elem(7));
        assert_ne!(pushed, xs);
        copied = model.clone();
        copied.push(7);
        check(pushed, &copied);
        check(xs, &model);
        for list in [copy, pushed, xs] {
            release(list);
        }
    });
}

#[test]
fn prepend_is_in_place_at_count_one_and_copies_when_shared() {
    balanced(|| {
        let n = 20_000_i64;
        let mut xs = ops::prepend(list(&[]), elem(n));
        let first_root = xs;
        let mut model = vec![n];
        let before = live();
        for v in (0..n).rev() {
            xs = ops::prepend(xs, elem(v));
            model.insert(0, v);
            assert_eq!(xs, first_root, "prepend at count one keeps the root");
        }
        check(xs, &model);
        // Elements that are strings are a third of the objects; the tree
        // adds about one leaf per 32 elements and few branches.
        let strings = (0..n).filter(|v| v % 3 == 0).count() as i64;
        let nodes = live() - before - strings;
        assert!(
            nodes <= n / 32 + n / 512 + 8,
            "{nodes} nodes for {n} elements"
        );
        assert!(height(xs) <= 3, "height {}", height(xs));
        dup_word(xs.word());
        let other = ops::prepend(xs, elem(-1));
        assert_ne!(other, xs);
        check(xs, &model);
        model.insert(0, -1);
        check(other, &model);
        release(other);
        release(xs);
    });
}

#[test]
fn appending_many_small_lists_keeps_the_tree_shallow() {
    balanced(|| {
        let mut acc = list(&[]);
        let mut model = Vec::new();
        for n in 0..3_000_i64 {
            let len = (n % 7) as usize;
            let piece: Vec<i64> = (0..len as i64).map(|k| n * 10 + k).collect();
            let other = list(&piece);
            acc = ops::append(acc, other);
            release(other);
            model.extend(piece);
        }
        check(acc, &model);
        // ~9,000 elements: three levels when full, a little slack allowed.
        assert!(height(acc) <= 4, "height {}", height(acc));
        // Prepending element by element too.
        for v in 0..1_000 {
            acc = ops::prepend(acc, elem(-v));
            model.insert(0, -v);
        }
        check(acc, &model);
        assert!(height(acc) <= 4, "height {}", height(acc));
        release(acc);
    });
}

#[test]
fn appending_a_list_to_itself() {
    balanced(|| {
        let model: Vec<i64> = (0..100).collect();
        let xs = list(&model);
        // The helper's `a` owned, `b` borrowed, and the same object.
        // SAFETY: a live list; the caller's reference is given up.
        let joined =
            root(unsafe { cheby_rt_list_append(std::ptr::null_mut(), xs.word(), xs.word()) });
        let doubled: Vec<i64> = model.iter().chain(&model).copied().collect();
        check(joined, &doubled);
        release(joined);
    });
}

#[test]
fn iteration_starts_inside_the_tree_and_from_iter_builds_lists() {
    balanced(|| {
        let model: Vec<i64> = (0..10_000).collect();
        let xs = list(&model);
        let view = ops::slice(xs, 1_234, 9_876);
        // SAFETY: a live list.
        let it = unsafe { iter(view.word()) };
        assert_eq!(it.len(), 9_876 - 1_234);
        assert_eq!(it.map(value).collect::<Vec<_>>(), &model[1_234..9_876]);
        // The iterator holds the list: it outlives the caller's reference.
        // SAFETY: a live list.
        let it = unsafe { iter(view.word()) };
        release(view);
        assert_eq!(it.map(value).sum::<i64>(), model[1_234..9_876].iter().sum());
        release(xs);
    });
}

#[test]
fn freeing_a_million_elements_uses_no_stack() {
    // A small stack: recursion over nodes or elements would overflow it.
    let handle = std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(|| {
            let before = live();
            let xs = from_iter((0..1_000_000).map(|v| abi::tag_int(v).unwrap()));
            let ys = ops::slice(root(xs), 10, 999_990);
            // SAFETY: a live list.
            assert_eq!(unsafe { super::length(xs) }, 1_000_000);
            release(root(xs));
            release(ys);
            assert_eq!(live(), before);
        })
        .unwrap();
    handle.join().unwrap();
}

// ---------------------------------------------------------------------
// The C entry points and their ownership

/// What an `@external` wrapper does: owns the argument it passes, calls
/// the kernel with it borrowed, and drops it after the call.
fn as_wrapper(xs: Root, call: impl FnOnce(Word) -> Word) -> Root {
    let result = call(xs.word());
    release(xs);
    root(result)
}

#[test]
fn helpers_follow_the_ownership_of_plan_4_1() {
    balanced(|| {
        let null = std::ptr::null_mut();
        // SAFETY (every block below): live lists and elements with the
        // ownership each helper documents.
        let xs = unsafe { cheby_rt_list_push(null, empty(), elem(1)) };
        let xs = unsafe { cheby_rt_list_push(null, xs, elem(2)) };
        let xs = unsafe { cheby_rt_list_push(null, xs, elem(3)) };
        check(root(xs), &[1, 2, 3]);
        assert_eq!(unsafe { cheby_rt_list_length(xs) }, 3);
        assert_eq!(unsafe { cheby_rt_list_length(empty()) }, 0);
        let second = unsafe { cheby_rt_list_get(xs, 2) };
        assert_eq!(value(second), 3);
        drop_word(second);
        let rest = unsafe { cheby_rt_list_slice(null, xs, 1, 3) };
        check(root(rest), &[2, 3]);
        let joined = unsafe { cheby_rt_list_append(null, rest, xs) };
        check(root(joined), &[2, 3, 1, 2, 3]);
        check(root(xs), &[1, 2, 3]);
        release(root(joined));
        release(root(xs));
    });
}

/// `cheby_rt_list_slice_owned` changes a unique root in place and leaves a
/// shared one alone; the stats counters see which path each update took.
#[test]
fn owned_slices_reuse_a_unique_root() {
    use crate::stats::Event;
    balanced(|| {
        let domain = crate::alloc::current_domain();
        let count = |event| domain.stats().count(event);
        let null = std::ptr::null_mut();
        let xs = list(&[1, 2, 3, 4]);
        let (in_place, copied) = (
            count(Event::ListUpdateInPlace),
            count(Event::ListUpdateCopied),
        );
        // SAFETY: a live list, given up.
        let rest = unsafe { cheby_rt_list_slice_owned(null, xs.word(), 1, 4) };
        assert_eq!(rest, xs.word(), "a unique root is reused");
        check(root(rest), &[2, 3, 4]);
        assert_eq!(count(Event::ListUpdateInPlace), in_place + 1);
        // A shared list keeps its view; the slice is a new root.
        dup_word(rest);
        // SAFETY: as above; the test keeps its other reference.
        let tail = unsafe { cheby_rt_list_slice_owned(null, rest, 2, 9) };
        assert_ne!(tail, rest);
        check(root(tail), &[4]);
        check(root(rest), &[2, 3, 4]);
        assert_eq!(count(Event::ListUpdateCopied), copied + 1);
        // The whole list is passed through; an empty slice is the static
        // empty list.
        // SAFETY: as above.
        let same = unsafe { cheby_rt_list_slice_owned(null, tail, 0, 1) };
        assert_eq!(same, tail);
        // SAFETY: as above.
        let none = unsafe { cheby_rt_list_slice_owned(null, same, 1, 1) };
        assert_eq!(none, empty());
        // SAFETY: as above.
        let none = unsafe { cheby_rt_list_slice_owned(null, rest, 2, 1) };
        assert_eq!(none, empty());
    });
}

/// A push onto a unique list is counted as in place, onto a shared one as
/// copied, and onto the empty list as neither.
#[test]
fn pushes_are_counted_by_path() {
    use crate::stats::Event;
    balanced(|| {
        let domain = crate::alloc::current_domain();
        let count = |event| domain.stats().count(event);
        let (in_place, copied) = (count(Event::ListPushInPlace), count(Event::ListPushCopied));
        let xs = ops::push(root(empty()), elem(1));
        let xs = ops::push(xs, elem(2));
        assert_eq!(count(Event::ListPushInPlace), in_place + 1);
        dup_word(xs.word());
        let ys = ops::push(xs, elem(3));
        assert_eq!(count(Event::ListPushCopied), copied + 1);
        check(xs, &[1, 2]);
        check(ys, &[1, 2, 3]);
        release(xs);
        release(ys);
    });
}

#[test]
fn kernels_borrow_and_update_in_place_at_count_one() {
    balanced(|| {
        let xs = list(&[1, 2, 3]);
        // Unique: the wrapper's reference is the only one.
        // SAFETY (every kernel call): live borrowed arguments.
        let pushed = as_wrapper(xs, |w| unsafe { cheby_list_push(w, elem(4)) });
        assert_eq!(pushed, xs, "pushed in place");
        check(pushed, &[1, 2, 3, 4]);
        let set = as_wrapper(pushed, |w| unsafe {
            cheby_list_set(w, 0, abi::tag_int(10).unwrap())
        });
        assert_eq!(set, xs);
        check(set, &[10, 2, 3, 4]);
        let prepended = as_wrapper(set, |w| unsafe {
            cheby_list_prepend(w, abi::tag_int(0).unwrap())
        });
        assert_eq!(prepended, xs);
        check(prepended, &[0, 10, 2, 3, 4]);
        // Shared: the kernel leaves the argument alone.
        dup_word(prepended.word());
        let other = as_wrapper(prepended, |w| unsafe {
            cheby_list_push(w, abi::tag_int(5).unwrap())
        });
        assert_ne!(other, prepended);
        check(other, &[0, 10, 2, 3, 4, 5]);
        check(prepended, &[0, 10, 2, 3, 4]);
        // Appending a list to itself through the kernel.
        dup_word(prepended.word());
        let doubled = as_wrapper(prepended, |w| unsafe { cheby_list_append(w, w) });
        check(doubled, &[0, 10, 2, 3, 4, 0, 10, 2, 3, 4]);
        check(prepended, &[0, 10, 2, 3, 4]);
        for list in [other, doubled, prepended] {
            release(list);
        }
    });
}

#[test]
fn kernels_read_and_cut_with_clamping() {
    balanced(|| {
        let xs = list(&[10, 20, 30, 40]);
        let word = xs.word();
        // SAFETY (every kernel call): live borrowed arguments.
        unsafe {
            assert_eq!(cheby_list_length(word), 4);
            assert_eq!(cheby_list_length(empty()), 0);
            assert_eq!(option_value(cheby_list_get(word, 3)), Some(40));
            for index in [4, -1, i64::MIN] {
                assert_eq!(option_value(cheby_list_get(word, index)), None);
            }
            for (start, end, expected) in [
                (1, 3, &[20, 30][..]),
                (-5, 2, &[10, 20]),
                (2, 10, &[30, 40]),
                (3, 1, &[]),
                (0, 4, &[10, 20, 30, 40]),
            ] {
                let sliced = root(cheby_list_slice(word, start, end));
                check(sliced, expected);
                release(sliced);
            }
            for (count, front, back) in [
                (-1, &[][..], &[10, 20, 30, 40][..]),
                (1, &[10], &[20, 30, 40]),
                (9, &[10, 20, 30, 40], &[]),
            ] {
                let taken = root(cheby_list_take(word, count));
                check(taken, front);
                release(taken);
                let dropped = root(cheby_list_drop(word, count));
                check(dropped, back);
                release(dropped);
                let pair = cheby_list_split_at(word, count);
                let parts = crate::values::read_tuple(pair);
                assert_eq!(parts.len(), 2);
                check(root(parts[0]), front);
                check(root(parts[1]), back);
                drop_word(pair);
            }
            // `set` outside the list returns it unchanged (D-318).
            for index in [4, -1] {
                let unchanged = cheby_list_set(word, index, abi::tag_int(0).unwrap());
                assert_eq!(unchanged, word);
                drop_word(unchanged);
            }
        }
        check(xs, &[10, 20, 30, 40]);
        release(xs);
    });
}

/// The payload of an owned `Option` word, which is consumed.
fn option_value(option: Word) -> Option<i64> {
    // SAFETY: a live `Option`.
    let result = unsafe { crate::values::read_option(option) }.map(value);
    drop_word(option);
    result
}

#[test]
fn kernels_are_registered_with_their_std_symbols() {
    let mut table = Vec::new();
    super::kernels(&mut table);
    // Registering the kernels registers the layouts too, so the static
    // empty list's layout is known before any list function runs.
    for id in [26, 27, 28] {
        assert!(crate::layout::get(id).is_some(), "layout {id}");
    }
    let names: Vec<_> = table.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        names,
        [
            "cheby_list_length",
            "cheby_list_get",
            "cheby_list_push",
            "cheby_list_prepend",
            "cheby_list_append",
            "cheby_list_set",
            "cheby_list_slice",
            "cheby_list_take",
            "cheby_list_drop",
            "cheby_list_split_at",
            "cheby_list_unique",
        ]
    );
    assert!(table.iter().all(|(_, address)| !address.is_null()));
    assert_eq!(super::COMPLETE, ["list"]);
}

// ---------------------------------------------------------------------
// Costs (docs/stdlib/list.md, D-300), documented by counting objects

/// Builds a list of `n` tagged elements by pushing at count one.
fn pushed(n: i64) -> Root {
    root(from_iter((0..n).map(|v| abi::tag_int(v).unwrap())))
}

#[test]
fn cost_slices_are_o1_views() {
    balanced(|| {
        for n in [100, 10_000, 100_000] {
            let xs = pushed(n);
            let before = live();
            let outer = ops::slice(xs, 1, n as usize - 1);
            let inner = ops::slice(outer, 1, n as usize - 3);
            // One root each, whatever the length: the tree is shared.
            assert_eq!(live() - before, 2);
            assert_eq!(outer.view().tree, xs.view().tree);
            assert_eq!(inner.view().tree, xs.view().tree);
            for list in [inner, outer, xs] {
                release(list);
            }
        }
    });
}

#[test]
fn cost_push_at_count_one_is_amortized_o1() {
    balanced(|| {
        let n = 100_000_usize;
        let before = live();
        let xs = pushed(n as i64);
        // Every push reused the root and wrote along the right edge; the
        // objects left are the full leaves and the branches above them:
        // n/32 leaves, n/1024 branches, ... plus the root. No push
        // allocated more than a constant amount: leaves grow by doubling
        // (at most 6 allocations per 32 elements) and branches are created
        // once per 32 children.
        let objects = live() - before;
        let expected = 1 + n / 32 + (n / 32).div_ceil(32) + 8;
        assert!(
            objects <= expected as i64,
            "{objects} objects for {n} elements"
        );
        release(xs);
    });
}

#[test]
fn cost_get_and_set_are_o_log_n() {
    balanced(|| {
        for n in [1_000, 100_000, 1_000_000] {
            let xs = pushed(n);
            // The height is the number of nodes a lookup visits below the
            // root: ceil(log32 n) - 1 for a list built by pushing.
            let h = height(xs);
            let levels = (0..13).find(|&l| 32_i64.pow(l) >= n).unwrap() as usize;
            assert!(h < levels.max(1), "height {h} for {n} elements");
            // A set on a shared list copies the path only: one leaf, one
            // branch per level and the root.
            dup_word(xs.word());
            let before = live();
            let ys = ops::set(xs, n as usize / 2, abi::tag_int(-1).unwrap());
            assert_eq!(live() - before, h as i64 + 2);
            assert_eq!(ops::get(ys, n as usize / 2), abi::tag_int(-1));
            release(ys);
            release(xs);
        }
    });
}

#[test]
fn cost_append_and_prepend_are_o_log_n() {
    balanced(|| {
        for n in [1_000, 100_000] {
            let a = pushed(n);
            let b = pushed(n);
            let h = height(a).max(height(b));
            // Both shared: concatenation copies the seam only. Each level
            // makes at most two branches, plus nodes a repack makes next to
            // the seam (at most 2M per level, rarely).
            dup_word(a.word());
            let before = live();
            let joined = ops::append(a, b);
            let made = live() - before;
            assert!(made <= ((2 * M + 2) * (h + 2)) as i64, "{made} objects");
            assert!(height(joined) <= h + 1);
            dup_word(joined.word());
            let before = live();
            let front = ops::prepend(joined, abi::tag_int(-1).unwrap());
            let made = live() - before;
            assert!(made <= ((2 * M + 2) * (h + 3)) as i64, "{made} objects");
            for list in [front, joined, a, b] {
                release(list);
            }
        }
    });
}

// ---------------------------------------------------------------------
// Property tests against `Vec`

/// One operation on a list and its model.
#[derive(Debug, Clone)]
enum Op {
    Push(i64),
    Prepend(i64),
    Set(usize, i64),
    Append(usize),
    Slice(usize, usize),
    SliceOwned(usize, usize),
    Take(i64),
    Drop(i64),
    SplitAt(i64, bool),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => any::<i64>().prop_map(|v| Op::Push(v % 1_000_000)),
        2 => any::<i64>().prop_map(|v| Op::Prepend(v % 1_000_000)),
        3 => (any::<usize>(), any::<i64>()).prop_map(|(i, v)| Op::Set(i, v % 1_000_000)),
        2 => prop_oneof![0..40_usize, 0..3_000_usize].prop_map(Op::Append),
        2 => (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Op::Slice(a, b)),
        2 => (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Op::SliceOwned(a, b)),
        1 => (-5..200_000_i64).prop_map(Op::Take),
        1 => (-5..200_000_i64).prop_map(Op::Drop),
        1 => ((-5..200_000_i64), any::<bool>()).prop_map(|(n, front)| Op::SplitAt(n, front)),
    ]
}

/// Applies `op` to an owned list (through the kernels where they borrow)
/// and to its model; with `shared`, another reference keeps the old list
/// alive, which must not change.
fn apply(xs: Root, model: &mut Vec<i64>, op: &Op, shared: bool) -> Root {
    let old = shared.then(|| {
        dup_word(xs.word());
        (xs, model.clone())
    });
    let len = model.len();
    let result = match *op {
        Op::Push(v) => {
            model.push(v);
            ops::push(xs, elem(v))
        }
        Op::Prepend(v) => {
            model.insert(0, v);
            // SAFETY: a live borrowed list and element.
            let item = elem(v);
            let r = as_wrapper(xs, |w| unsafe { cheby_list_prepend(w, item) });
            drop_word(item);
            r
        }
        Op::Set(i, v) => {
            let i = if len == 0 { i } else { i % (len + 1) };
            if i < len {
                model[i] = v;
            }
            ops::set(xs, i, elem(v))
        }
        Op::Append(n) => {
            let start = 1_000_000 + len as i64;
            let piece: Vec<i64> = (start..start + n as i64).collect();
            let other = list(&piece);
            model.extend(&piece);
            // SAFETY: live lists.
            let r = as_wrapper(xs, |w| unsafe { cheby_list_append(w, other.word()) });
            release(other);
            r
        }
        Op::Slice(start, end) => {
            let (start, end) = (start % (len + 2), end % (len + 2));
            let (from, to) = (start.min(len), end.min(len));
            *model = if from < to {
                model[from..to].to_vec()
            } else {
                Vec::new()
            };
            // SAFETY: a live borrowed list.
            as_wrapper(xs, |w| unsafe {
                cheby_list_slice(w, start as i64, end as i64)
            })
        }
        Op::SliceOwned(start, end) => {
            let (start, end) = (start % (len + 2), end % (len + 2));
            let (from, to) = (start.min(len), end.min(len));
            *model = if from < to {
                model[from..to].to_vec()
            } else {
                Vec::new()
            };
            let null = std::ptr::null_mut();
            // SAFETY: a live list, given up.
            root(unsafe { cheby_rt_list_slice_owned(null, xs.word(), start as i64, end as i64) })
        }
        Op::Take(n) => {
            let k = n.clamp(0, len as i64) as usize;
            model.truncate(k);
            // SAFETY: a live borrowed list.
            as_wrapper(xs, |w| unsafe { cheby_list_take(w, n) })
        }
        Op::Drop(n) => {
            let k = n.clamp(0, len as i64) as usize;
            model.drain(..k);
            // SAFETY: a live borrowed list.
            as_wrapper(xs, |w| unsafe { cheby_list_drop(w, n) })
        }
        Op::SplitAt(n, front) => {
            let k = n.clamp(0, len as i64) as usize;
            if front {
                model.truncate(k);
            } else {
                model.drain(..k);
            }
            // SAFETY: a live borrowed list; the pair is owned.
            let pair = unsafe { cheby_list_split_at(xs.word(), n) };
            // SAFETY: a live tuple of two lists.
            let parts = unsafe { crate::values::read_tuple(pair) };
            let part = parts[usize::from(!front)];
            dup_word(part);
            drop_word(pair);
            release(xs);
            root(part)
        }
    };
    if let Some((old, old_model)) = old {
        check(old, &old_model);
        release(old);
    }
    result
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// Every operation against `Vec`, on lists up to 100,000 elements,
    /// including slices of slices, at count one and shared.
    #[test]
    fn operations_match_vec(
        start in prop_oneof![0..40_usize, 0..2_000_usize, 0..100_000_usize],
        ops in prop::collection::vec((op(), any::<bool>()), 1..24),
    ) {
        balanced(|| {
            let mut model: Vec<i64> = (0..start as i64).collect();
            let mut xs = list(&model);
            for (op, shared) in &ops {
                xs = apply(xs, &mut model, op, *shared);
                check(xs, &model);
            }
            release(xs);
        });
    }
}

// ---------------------------------------------------------------------
// The multi-version state machine (plan §6)

/// One step of the state machine over live versions.
#[derive(Debug, Clone)]
enum Step {
    /// Another reference to version `i` becomes a version of its own.
    Share(usize),
    /// Version `i` is dropped.
    Drop(usize),
    Push(usize, i64),
    Set(usize, usize, i64),
    /// A slice of version `i` (borrowed) becomes a new version.
    Slice(usize, usize, usize),
    /// Version `i` (owned) is replaced by its slice (`[x, ..rest]` on a
    /// list that dies at the match).
    SliceOwned(usize, usize, usize),
    /// Version `i` (owned) followed by version `j` (borrowed).
    Append(usize, usize),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        2 => any::<usize>().prop_map(Step::Share),
        2 => any::<usize>().prop_map(Step::Drop),
        4 => (any::<usize>(), -1_000..1_000_i64).prop_map(|(i, v)| Step::Push(i, v)),
        3 => (any::<usize>(), any::<usize>(), -1_000..1_000_i64)
            .prop_map(|(i, k, v)| Step::Set(i, k, v)),
        3 => (any::<usize>(), any::<usize>(), any::<usize>())
            .prop_map(|(i, a, b)| Step::Slice(i, a, b)),
        2 => (any::<usize>(), any::<usize>(), any::<usize>())
            .prop_map(|(i, a, b)| Step::SliceOwned(i, a, b)),
        2 => (any::<usize>(), any::<usize>()).prop_map(|(i, j)| Step::Append(i, j)),
    ]
}

/// The most versions kept live at once.
const VERSIONS: usize = 6;

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    /// N live versions under random sharing, dropping and updating, each
    /// checked against its own model after every step; the domain returns
    /// to its start. Slices followed by drops of the original make unique
    /// views whose tree reaches past their end, then pushed and set in
    /// place.
    #[test]
    fn versions_stay_independent(
        start in prop_oneof![0..40_usize, 0..3_000_usize],
        steps in prop::collection::vec(step(), 1..60),
    ) {
        balanced(|| {
            let model: Vec<i64> = (0..start as i64).collect();
            let mut versions: Vec<(Root, Vec<i64>)> = vec![(list(&model), model)];
            for step in &steps {
                let n = versions.len();
                match *step {
                    Step::Share(i) if n < VERSIONS => {
                        let (xs, model) = versions[i % n].clone();
                        dup_word(xs.word());
                        versions.push((xs, model));
                    }
                    Step::Drop(i) if n > 1 => {
                        let (xs, _) = versions.swap_remove(i % n);
                        release(xs);
                    }
                    Step::Push(i, v) => {
                        let (xs, model) = &mut versions[i % n];
                        model.push(v);
                        *xs = ops::push(*xs, elem(v));
                    }
                    Step::Set(i, k, v) => {
                        let (xs, model) = &mut versions[i % n];
                        let k = k % (model.len() + 1);
                        if k < model.len() {
                            model[k] = v;
                        }
                        *xs = ops::set(*xs, k, elem(v));
                    }
                    Step::Slice(i, a, b) if n < VERSIONS => {
                        let (xs, model) = &versions[i % n];
                        let len = model.len();
                        let (a, b) = (a % (len + 1), b % (len + 1));
                        let (from, to) = (a.min(b), a.max(b));
                        let sliced = ops::slice(*xs, from, to);
                        let part = model[from..to].to_vec();
                        versions.push((sliced, part));
                    }
                    Step::SliceOwned(i, a, b) => {
                        let (xs, model) = &mut versions[i % n];
                        let len = model.len();
                        let (a, b) = (a % (len + 1), b % (len + 1));
                        let (from, to) = (a.min(b), a.max(b));
                        *model = model[from..to].to_vec();
                        *xs = ops::slice_owned(*xs, from, to);
                    }
                    Step::Append(i, j) => {
                        let (other, other_model) = versions[j % n].clone();
                        let (xs, model) = &mut versions[i % n];
                        model.extend(other_model);
                        *xs = ops::append(*xs, other);
                    }
                    _ => {}
                }
                for (xs, model) in &versions {
                    check(*xs, model);
                }
            }
            for (xs, _) in versions {
                release(xs);
            }
        });
    }
}

#[test]
fn trees_can_be_cut_and_joined_directly() {
    balanced(|| {
        let model: Vec<i64> = (0..5_000).collect();
        let xs = list(&model);
        let tree = xs.view().tree.unwrap().dup();
        let front = tree::take(tree.dup(), 1_000);
        let back = tree::drop_front(tree, 1_000);
        let joined = tree::collapse(tree::concat(back, front));
        let rotated: Vec<i64> = (1_000..5_000).chain(0..1_000).collect();
        let r = Root::new(View {
            tree: Some(joined),
            offset: 0,
            len: 5_000,
        });
        check(r, &rotated);
        check(xs, &model);
        release(r);
        release(xs);
    });
}

// ---------------------------------------------------------------------
// `list::unique`, and equality, hashing and debug text of lists (W3-B)

/// The model of `unique`: the first of each group of equal values, in
/// order.
fn unique_model(model: &[i64]) -> Vec<i64> {
    let mut seen = std::collections::HashSet::new();
    model.iter().copied().filter(|v| seen.insert(*v)).collect()
}

#[test]
fn unique_keeps_the_first_of_each_element() {
    balanced(|| {
        let xs = list(&[3, 1, 3, 2, 1]);
        // SAFETY: the kernel borrows its list.
        let unique = root(unsafe { cheby_list_unique(xs.word()) });
        check(unique, &[3, 1, 2]);
        release(unique);
        // Without repeats, the result is the list itself.
        let distinct = list(&[1, 2, 3]);
        // SAFETY: as above.
        let same = unsafe { cheby_list_unique(distinct.word()) };
        assert_eq!(same, distinct.word());
        drop_word(same);
        // SAFETY: as above.
        let empty_result = unsafe { cheby_list_unique(empty()) };
        assert_eq!(empty_result, empty());
        // Strings equal by contents are repeats, whatever their objects.
        let texts = root(from_iter(["a", "b", "a"].map(new_string)));
        // SAFETY: as above.
        let unique_texts = root(unsafe { cheby_list_unique(texts.word()) });
        // SAFETY: a live list of strings.
        let read: Vec<String> = unsafe { iter(unique_texts.word()) }
            // SAFETY: live strings.
            .map(|word| unsafe { to_rust_string(word) }.unwrap())
            .collect();
        assert_eq!(read, ["a", "b"]);
        for list in [xs, distinct, texts, unique_texts] {
            release(list);
        }
    });
}

#[test]
fn unique_of_a_view_reads_only_the_view() {
    balanced(|| {
        let model: Vec<i64> = (0..200).map(|v| v % 7).collect();
        let xs = list(&model);
        // SAFETY: the kernels borrow their lists.
        let view = root(unsafe { cheby_list_slice(xs.word(), 50, 120) });
        // SAFETY: as above.
        let unique = root(unsafe { cheby_list_unique(view.word()) });
        check(unique, &unique_model(&model[50..120]));
        for list in [xs, view, unique] {
            release(list);
        }
    });
}

/// `unique` of a list holding a function value panics `EqualityOnFunction`
/// (D-233) on the running entry.
unsafe extern "C" fn unique_of_functions(_: *mut crate::ctx::ExecCtx) -> u64 {
    let f = crate::generic::tests::closure(&[]);
    let xs = from_iter([f]);
    // SAFETY: the kernel borrows its list; it panics, so nothing returns.
    unsafe { cheby_list_unique(xs) }
}

#[test]
fn unique_panics_on_function_values() {
    let config = crate::entry::EntryConfig::new(crate::entry::EntryMode::Test);
    // SAFETY: a test thunk with the `EntryFn` ABI.
    let outcome = unsafe { crate::entry::run_entry(unique_of_functions, &config) }.unwrap();
    let crate::entry::EntryOutcome::Panicked(report) = outcome else {
        panic!("expected a panic, got {outcome:?}");
    };
    assert_eq!(report.kind, abi::PanicKind::EqualityOnFunction);
}

#[test]
fn lists_compare_hash_and_print_through_their_root_kind() {
    balanced(|| {
        let model: Vec<i64> = (0..1_000).collect();
        let pushed = list(&model);
        let mut prepended = root(empty());
        for &v in model.iter().rev() {
            prepended = ops::prepend(prepended, elem(v));
        }
        // SAFETY: live lists of one type.
        unsafe {
            assert_eq!(
                crate::generic::equal(pushed.word(), prepended.word()),
                Ok(true)
            );
            assert_eq!(
                crate::generic::hash_with_seed(pushed.word(), 7),
                crate::generic::hash_with_seed(prepended.word(), 7)
            );
            let text = crate::generic::debug_string(pushed.word());
            assert!(text.starts_with("[\"0\", 1, 2, \"3\", "), "{text}");
            assert!(text.ends_with(", 998, \"999\"]"), "{text}");
        }
        let shorter = list(&model[..999]);
        // SAFETY: as above.
        unsafe {
            assert_eq!(
                crate::generic::equal(pushed.word(), shorter.word()),
                Ok(false)
            );
            assert_eq!(crate::generic::debug_string(empty()), "[]");
        }
        for list in [pushed, prepended, shorter] {
            release(list);
        }
    });
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn unique_matches_the_model(model in prop::collection::vec(0_i64..40, 0..300)) {
        balanced(|| {
            let xs = list(&model);
            // SAFETY: the kernel borrows its list.
            let unique = root(unsafe { cheby_list_unique(xs.word()) });
            check(unique, &unique_model(&model));
            release(unique);
            release(xs);
        });
    }
}
