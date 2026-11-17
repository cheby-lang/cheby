//! Tests of the heap kernels on lists built in Rust: the native comparison
//! read from descriptors and closures, D-1613's algorithm on the list tree
//! against the same algorithm on a slice, and ownership: a shared list
//! never changes, a unique one changes in place, and the accounting
//! balances.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_wrap,
    reason = "test arithmetic on small, known values"
)]

use super::order::{Key, key_of, native_key};
use super::*;
use crate::abi::layout;
use crate::alloc::{alloc_object, live};
use crate::desc::{self, StaticNominal, StaticText};
use crate::generic::tests::closure;
use crate::object::{ptr_to_word, write_field};
use crate::string::new_string;

/// Runs `f` and checks that the domain's live count returns to its start.
fn balanced(f: impl FnOnce()) {
    let before = live();
    f();
    assert_eq!(live(), before, "objects leaked or freed twice");
}

/// An object of `layout` holding `fields`, owned.
fn object(id: abi::LayoutId, fields: &[Word]) -> Word {
    let scan = u8::try_from(fields.len()).unwrap();
    let obj = alloc_object((fields.len() as u64 + 1) * 8, abi::header(1, scan, id));
    for (index, field) in fields.iter().enumerate() {
        // SAFETY: initializing a fresh object of that many fields.
        unsafe { write_field(obj, (index + 1) * 8, *field) };
    }
    ptr_to_word(obj)
}

/// The builtin nominal descriptor of `name`.
fn builtin(name: &str) -> Word {
    desc::builtin(name).unwrap()
}

/// A descriptor of the tuple type of `elements` (layout 11), owned.
fn tuple_desc(elements: &[Word]) -> Word {
    let mut fields = vec![builtin("tuple")];
    fields.extend_from_slice(elements);
    object(layout::DESC, &fields)
}

static APP: StaticText<3> = StaticText::new(b"app");
static INT_NAME: StaticText<3> = StaticText::new(b"Int");
/// A user type named `Int` in module `app`, which is not `Int`.
static USER_INT: StaticNominal = StaticNominal::new(
    std::ptr::from_ref(&INT_NAME).cast::<u8>(),
    std::ptr::from_ref(&APP).cast::<u8>(),
);

#[test]
fn builtin_compares_are_read_from_descriptors() {
    for name in ["int", "i8", "i16", "i32", "i64", "u8", "u16", "u32"] {
        // SAFETY: a static nominal descriptor.
        let key = unsafe { key_of(builtin(name)) };
        assert_eq!(key, Some(Key::Signed), "{name}");
    }
    // SAFETY: as above.
    unsafe {
        assert_eq!(key_of(builtin("u64")), Some(Key::Unsigned));
        assert_eq!(key_of(builtin("string")), Some(Key::Text));
        for name in [
            "float", "f32", "bool", "nil", "bytes", "order", "list", "option",
        ] {
            assert_eq!(key_of(builtin(name)), None, "{name}");
        }
        assert_eq!(key_of(USER_INT.word()), None);
    }
    balanced(|| {
        let pair = tuple_desc(&[builtin("int"), builtin("string")]);
        let nested = tuple_desc(&[pair, builtin("u64")]);
        let with_float = tuple_desc(&[builtin("int"), builtin("float")]);
        let with_user = tuple_desc(&[builtin("int"), USER_INT.word()]);
        let list_of_int = object(layout::DESC, &[builtin("list"), builtin("int")]);
        // SAFETY: live descriptors.
        unsafe {
            assert_eq!(
                key_of(nested),
                Some(Key::Tuple(
                    vec![
                        Key::Tuple(vec![Key::Signed, Key::Text].into()),
                        Key::Unsigned
                    ]
                    .into()
                ))
            );
            assert_eq!(key_of(with_float), None);
            assert_eq!(key_of(with_user), None);
            // `List<Int>` compares by `list::compare`, which is not built in.
            assert_eq!(key_of(list_of_int), None);
            for word in [nested, with_float, with_user, list_of_int] {
                rc::drop(word);
            }
        }
    });
}

#[test]
fn the_element_type_is_read_from_the_closure_capture() {
    balanced(|| {
        let code = closure(&[]);
        // A `Compare` dictionary: the descriptor, then `compare`'s closure.
        let dict = object(layout::DICT, &[builtin("int"), code]);
        // SAFETY: a live dictionary, captured three times below.
        unsafe {
            rc::dup(dict);
            rc::dup(dict);
        }
        let by = closure(&[dict]);
        let two = closure(&[dict, dict]);
        let none = closure(&[]);
        let not_a_dict = closure(&[values::int_word(7)]);
        let by_text = closure(&[new_string("text")]);
        // SAFETY: live closures.
        unsafe {
            assert_eq!(native_key(by), Some(Key::Signed));
            assert_eq!(native_key(none), None);
            assert_eq!(native_key(two), None);
            assert_eq!(native_key(not_a_dict), None);
            assert_eq!(native_key(by_text), None);
            for word in [by, none, two, not_a_dict, by_text] {
                rc::drop(word);
            }
        }
    });
}

#[test]
fn native_keys_order_like_compare() {
    balanced(|| {
        let key = Key::Tuple(vec![Key::Signed, Key::Text, Key::Unsigned].into());
        let make = |n: i64, text: &str, u: u64| {
            values::tuple(&[values::int_word(n), new_string(text), values::u64_word(u)])
        };
        let words = [
            make(i64::MIN, "b", 0),
            make(-1, "b", u64::MAX),
            make(-1, "b", 1 << 62),
            make(-1, "ab", 5),
            make(i64::MAX, "", 0),
        ];
        // SAFETY: live tuples of the key's type.
        unsafe {
            assert_eq!(key.compare(words[0], words[1]), Ordering::Less);
            assert_eq!(key.compare(words[1], words[2]), Ordering::Greater);
            assert_eq!(key.compare(words[3], words[2]), Ordering::Less);
            assert_eq!(key.compare(words[4], words[3]), Ordering::Greater);
            assert_eq!(key.compare(words[2], words[2]), Ordering::Equal);
            for word in words {
                rc::drop(word);
            }
        }
    });
}

/// The values of a list of tagged `Int`s.
fn ints(list: Word) -> Vec<i64> {
    // SAFETY: a live list of tagged `Int`s.
    unsafe { super::super::iter(list) }
        .map(abi::untag_int)
        .collect()
}

/// A list of the tagged `Int`s `values`, owned.
fn int_list(values: &[i64]) -> Word {
    super::super::from_iter(values.iter().map(|&v| abi::tag_int(v).unwrap()))
}

/// Pseudo-random values with many repeats.
fn scrambled(n: usize) -> Vec<i64> {
    (0..n as i64)
        .map(|i| (i * 7_919 + 13) % 1_009 - 500)
        .collect()
}

/// The `before` of a native `Int` heap.
fn ints_before(max: bool) -> Signed {
    Signed(if max {
        Ordering::Greater
    } else {
        Ordering::Less
    })
}

/// D-1613 on a slice: `from_list` of `values`, then `pushed` pushed, then
/// every element popped, in the order popped.
fn model(values: &[i64], pushed: &[i64], max: bool) -> Vec<i64> {
    let before = ints_before(max);
    let mut all: Vec<Word> = values.iter().map(|&v| abi::tag_int(v).unwrap()).collect();
    let len = all.len();
    // SAFETY: tagged `Int`s compared natively.
    unsafe {
        heapify(
            &mut Scratch {
                items: &mut all,
                len,
            },
            &before,
        );
    }
    for &v in pushed {
        all.push(abi::tag_int(v).unwrap());
        let len = all.len();
        // SAFETY: as above.
        unsafe {
            sift_up(
                &mut Scratch {
                    items: &mut all,
                    len,
                },
                len - 1,
                &before,
            );
        }
    }
    let mut out = Vec::new();
    let mut len = all.len();
    while len > 0 {
        out.push(abi::untag_int(all[0]));
        all.swap(0, len - 1);
        len -= 1;
        // SAFETY: as above.
        unsafe {
            sift_down(
                &mut Scratch {
                    items: &mut all,
                    len,
                },
                0,
                &before,
            );
        }
    }
    out
}

/// What the kernels' steps give for [`model`]'s, with a native `compare`.
fn kernels(values: &[i64], pushed: &[i64], max: bool) -> Vec<i64> {
    let before = ints_before(max);
    let list = int_list(values);
    // SAFETY: a fresh list.
    let mut array = Array::new(unsafe { Root::from_word(list) });
    // SAFETY: tagged `Int`s compared natively.
    unsafe { heapify(&mut array, &before) };
    for &v in pushed {
        array.push(abi::tag_int(v).unwrap());
        let end = array.len() - 1;
        // SAFETY: as above.
        unsafe { sift_up(&mut array, end, &before) };
    }
    let mut list = {
        let heap = array.into_word();
        drop_word(list);
        heap
    };
    let mut out = Vec::new();
    loop {
        // SAFETY: a live list.
        let root = unsafe { Root::from_word(list) };
        let Some(top) = ops::get(root, 0) else {
            break;
        };
        out.push(abi::untag_int(top));
        if root.len() == 1 {
            drop_word(list);
            break;
        }
        let mut array = Array::new(root);
        let moved = array.pop_last();
        array.set(0, moved);
        // SAFETY: as above.
        unsafe { sift_down(&mut array, 0, &before) };
        let next = array.into_word();
        drop_word(list);
        list = next;
    }
    out
}

#[test]
fn the_tree_runs_the_algorithm_of_a_slice() {
    balanced(|| {
        for n in [0, 1, 2, 3, 31, 32, 33, 100, 1_025, 40_000] {
            let values = scrambled(n);
            let pushed: Vec<i64> = scrambled(n / 3 + 1).iter().map(|v| v * 3).collect();
            for max in [false, true] {
                let got = kernels(&values, &pushed, max);
                assert_eq!(got, model(&values, &pushed, max), "n = {n}, max = {max}");
                let mut sorted = got.clone();
                sorted.sort_unstable();
                if max {
                    sorted.reverse();
                }
                assert_eq!(got, sorted, "n = {n}, max = {max}");
            }
        }
    });
}

#[test]
fn a_shared_list_never_changes() {
    balanced(|| {
        let values = scrambled(3_000);
        let list = int_list(&values);
        dup_word(list);
        // SAFETY: a live list with two references.
        let mut array = Array::new(unsafe { Root::from_word(list) });
        assert!(array.owned, "a shared list gets a root of its own");
        let before = ints_before(false);
        // SAFETY: tagged `Int`s compared natively.
        unsafe { heapify(&mut array, &before) };
        array.push(abi::tag_int(-1_000).unwrap());
        let end = array.len() - 1;
        // SAFETY: as above.
        unsafe { sift_up(&mut array, end, &before) };
        let heap = array.into_word();
        assert_ne!(heap, list);
        assert_eq!(ints(list), values);
        assert_eq!(ints(heap).first(), Some(&-1_000));
        for word in [list, list, heap] {
            drop_word(word);
        }
    });
}

#[test]
fn a_unique_list_changes_in_place() {
    balanced(|| {
        let list = int_list(&scrambled(2_000));
        // SAFETY: a live list with one reference.
        let mut array = Array::new(unsafe { Root::from_word(list) });
        assert!(!array.owned, "a unique list is the array's own");
        // SAFETY: tagged `Int`s compared natively.
        unsafe { heapify(&mut array, &ints_before(false)) };
        let heap = array.into_word();
        assert_eq!(heap, list);
        assert_eq!(ints(heap).first(), Some(&-500));
        drop_word(heap);
        drop_word(list);
    });
}

#[test]
fn a_view_of_part_of_a_tree_is_copied() {
    balanced(|| {
        let values = scrambled(500);
        let list = int_list(&values);
        // SAFETY: a live list.
        let view = ops::slice(unsafe { Root::from_word(list) }, 7, 400);
        let mut array = Array::new(view);
        assert!(array.owned);
        // SAFETY: tagged `Int`s compared natively.
        unsafe { heapify(&mut array, &ints_before(true)) };
        let heap = array.into_word();
        assert_eq!(ints(heap).first(), values[7..400].iter().max());
        assert_eq!(ints(view.word()), values[7..400]);
        for word in [heap, view.word(), list] {
            drop_word(word);
        }
    });
}

#[test]
fn heaps_of_strings_release_what_they_pop() {
    balanced(|| {
        let words: Vec<Word> = scrambled(300)
            .iter()
            .map(|v| new_string(&v.to_string()))
            .collect();
        let mut list = super::super::from_iter(words);
        let before = Native(Key::Text, Ordering::Less);
        {
            // SAFETY: a live list with one reference.
            let mut array = Array::new(unsafe { Root::from_word(list) });
            // SAFETY: strings compared natively.
            unsafe { heapify(&mut array, &before) };
            let heap = array.into_word();
            drop_word(list);
            list = heap;
        }
        let mut previous: Option<String> = None;
        loop {
            // SAFETY: a live list.
            let root = unsafe { Root::from_word(list) };
            // SAFETY: the first element is a live string.
            let top = unsafe { values::str_of(ops::get(root, 0).unwrap()) }.to_owned();
            if let Some(previous) = &previous {
                assert!(previous.as_bytes() <= top.as_bytes(), "{previous} {top}");
            }
            previous = Some(top);
            if root.len() == 1 {
                drop_word(list);
                break;
            }
            let mut array = Array::new(root);
            let moved = array.pop_last();
            array.set(0, moved);
            // SAFETY: as above.
            unsafe { sift_down(&mut array, 0, &before) };
            let next = array.into_word();
            drop_word(list);
            list = next;
        }
    });
}
