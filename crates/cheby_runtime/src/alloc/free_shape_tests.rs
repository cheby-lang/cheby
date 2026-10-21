//! The free-shape table (D-907): every layout the runtime registers gets
//! the word it should, and for every shape of object the word gives what
//! the layout table and the kinds' own functions give. Debug builds check
//! that on every free as well (`check_fast_shape`), so these tests run the
//! check on each object they build, then free it.

use super::tests::{field_offset, tuple};
use super::*;
use crate::abi::layout as ids;
use crate::layout::{FieldKind, LayoutInfo};
use crate::object::write_field;

/// The free-shape table's answer for `word`, checked against the layout
/// table in debug builds. The builtins' words are written when the layout
/// table is first read, which a test running alone may not have done yet.
fn shape(word: Word) -> Option<Fast> {
    assert!(layout::get(ids::TUPLE).is_some());
    // SAFETY: the tests pass live objects.
    unsafe { fast_shape(word_to_ptr(word)) }
}

/// Drops the only reference to `word`.
fn drop_word(word: Word) {
    // SAFETY: the tests own the only reference.
    unsafe { rc::drop(word) };
}

/// A tagged integer field.
fn int(n: usize) -> Word {
    abi::tag_int(i64::try_from(n).unwrap()).unwrap()
}

#[test]
fn the_runtime_layouts_have_their_words() {
    // The builtins, with the handle kinds, and the list kinds.
    assert!(layout::get(ids::TUPLE).is_some());
    drop_word(crate::list::from_iter([int(1)]));
    for id in ids::STRING..ids::CUSTOM_FIRST {
        if let Some(info) = layout::get(id) {
            assert_ne!(layout::fast_word(id), 0, "{}", info.name);
        }
    }
    for id in ids::LIST_FIRST..ids::LIST_FIRST + 3 {
        assert_ne!(layout::fast_word(id), 0, "list layout {id}");
    }
    // Kinds with a finalizer, and handles, are never freed from a shape.
    for id in ids::SENDER..=ids::SELECTOR {
        assert!(layout::get(id).is_some(), "{id}");
        assert_eq!(layout::fast_word(id), 0, "handle layout {id}");
    }
    // Ids past the table take the layout table's path.
    assert_eq!(layout::fast_word(abi::LAYOUT_MAX), 0);
}

#[test]
fn texts_are_sized_by_their_capacity() {
    let domain = Domain::new();
    with_domain(Arc::clone(&domain), || {
        for len in 0..=40 {
            let text = "x".repeat(len);
            for word in [
                crate::string::new_string(&text),
                crate::string::new_bytes(text.as_bytes()),
            ] {
                let fast = shape(word).unwrap();
                assert_eq!((fast.count, fast.reusable), (0, false), "{len}");
                assert_eq!(fast.size, (24 + len).next_multiple_of(8), "{len}");
                drop_word(word);
            }
        }
        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    });
}

#[test]
fn uniform_objects_follow_their_scan_count() {
    let domain = Domain::new();
    with_domain(Arc::clone(&domain), || {
        for count in (0..=16).chain([253, 254]) {
            let fields: Vec<Word> = (0..count).map(int).collect();
            let word = tuple(&fields);
            let fast = shape(word).unwrap();
            assert_eq!((fast.first, fast.count), (8, count));
            assert_eq!((fast.size, fast.reusable), (field_offset(count), true));
            drop_word(word);
            for id in [ids::CLOSURE, ids::DICT, ids::DESC] {
                let obj = alloc_object(
                    to_u64(field_offset(count)),
                    abi::header(1, u8::try_from(count).unwrap(), id),
                );
                for index in 0..count {
                    // SAFETY: initializing a fresh object.
                    unsafe { write_field(obj, field_offset(index), int(index)) };
                }
                let word = object::ptr_to_word(obj);
                let fast = shape(word).unwrap();
                assert_eq!((fast.count, fast.reusable), (count, false), "{id}");
                drop_word(word);
            }
        }
        // The escaped form takes the layout table's path.
        let count = 300_usize;
        let obj = alloc_object(
            to_u64(field_offset(count + 1)),
            abi::header(1, abi::SCAN_ESCAPE, ids::TUPLE),
        );
        // SAFETY: initializing a fresh tuple: the tagged arity, then fields.
        unsafe {
            write_field(obj, 8, int(count));
            for index in 0..count {
                write_field(obj, field_offset(index + 1), int(index));
            }
        }
        let word = object::ptr_to_word(obj);
        assert!(shape(word).is_none());
        drop_word(word);
        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    });
}

#[test]
fn constructors_and_boxes_have_a_fixed_shape() {
    let domain = Domain::new();
    with_domain(Arc::clone(&domain), || {
        // More value fields than the scan count holds, then raw fields
        // after the value fields.
        let wide = layout::register(LayoutInfo::ctor(
            "t.FreeShapeWide",
            0,
            &vec![(FieldKind::Value, None); 300],
        ))
        .unwrap();
        let mixed = layout::register(LayoutInfo::ctor(
            "t.FreeShapeMixed",
            0,
            &[
                (FieldKind::Value, None),
                (FieldKind::Int, None),
                (FieldKind::Float, None),
            ],
        ))
        .unwrap();
        for (id, scan, values, raws) in [(wide, abi::SCAN_ESCAPE, 300, 0), (mixed, 1, 1, 2)] {
            let size = field_offset(values + raws);
            let obj = alloc_object(to_u64(size), abi::header(1, scan, id));
            for index in 0..values + raws {
                // SAFETY: initializing a fresh object; the raw fields hold
                // even words that must not be read as pointers.
                unsafe {
                    let field = if index < values { int(index) } else { 0x1000 };
                    write_field(obj, field_offset(index), field);
                }
            }
            let word = object::ptr_to_word(obj);
            let fast = shape(word).unwrap();
            assert_eq!((fast.first, fast.count, fast.size), (8, values, size));
            assert!(fast.reusable);
            drop_word(word);
        }
        for id in [
            ids::BOXED_INT,
            ids::BOXED_FLOAT,
            ids::BOXED_U64,
            ids::BOXED_F32,
        ] {
            let obj = alloc_object(16, abi::header(1, 0, id));
            // SAFETY: a fresh box with a raw payload.
            unsafe { write_field(obj, 8, 0x2000) };
            let word = object::ptr_to_word(obj);
            let fast = shape(word).unwrap();
            assert_eq!((fast.count, fast.size, fast.reusable), (0, 16, false));
            drop_word(word);
        }
        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    });
}

/// A list root, then its tree: a leaf while the list fits one, which
/// grows through every capacity up to 32, then branches. The root's shape
/// is checked here, and every node's on the free that walks the tree.
#[test]
fn list_nodes_are_freed_from_their_declared_shapes() {
    let domain = Domain::new();
    with_domain(Arc::clone(&domain), || {
        for len in (1..=40).chain([1_000, 40_000]) {
            let list = crate::list::from_iter((0..len).map(int));
            let root = shape(list).unwrap();
            assert_eq!((root.first, root.count, root.size), (8, 1, 32));
            assert!(!root.reusable);
            // SAFETY: a live root holds its tree at offset 8.
            let tree = unsafe { read_field(word_to_ptr(list), 8) };
            let node = shape(tree).unwrap();
            assert_eq!((node.first, node.reusable), (16, false));
            if len <= 32 {
                assert_eq!(node.count, len);
                assert!((16 + 8 * len..=16 + 8 * 32).contains(&node.size), "{len}");
            } else {
                assert_eq!(node.size, 16 + 8 * 64);
            }
            drop_word(list);
        }
        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    });
}
