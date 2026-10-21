//! Tests of structural equality, hashing and debug printing.

use std::sync::Arc;

use super::*;
use crate::abi::{self, Word, layout as ids};
use crate::alloc::{self, Domain, alloc_object, live_objects};
use crate::ctx::ExecCtx;
use crate::entry::{EntryConfig, EntryMode, EntryOutcome, run_entry};
use crate::layout::{self, CustomKind, FieldKind, LayoutInfo};
use crate::object::{self, read_field, to_u64, word_to_ptr, write_field};
use crate::rc;
use crate::values::{
    bool_word, bytes, err, f32_word, f64_word, int_word, nil, none, ok, some, string, tuple,
    u64_word,
};

mod kinds;

/// A closure whose code word is a dummy address and whose captures are
/// `captures` (owned).
pub(crate) fn closure(captures: &[Word]) -> Word {
    let count = captures.len().wrapping_add(1);
    let scan = u8::try_from(count).unwrap();
    let obj = alloc_object(
        to_u64(count.wrapping_add(1).wrapping_mul(8)),
        abi::header(1, scan, ids::CLOSURE),
    );
    // SAFETY: initializing a fresh closure: the code word, then captures.
    unsafe {
        write_field(obj, 8, 0x1000 | 1);
        for (index, capture) in captures.iter().enumerate() {
            write_field(obj, index.wrapping_add(2).wrapping_mul(8), *capture);
        }
    }
    object::ptr_to_word(obj)
}

/// The captures of a closure built by [`closure`], borrowed.
///
/// # Safety
///
/// `word` is a live closure.
pub(crate) unsafe fn captures(word: Word) -> Vec<Word> {
    let obj = word_to_ptr(word);
    // SAFETY: a live closure.
    let (scan, _) = unsafe { object::scan_and_layout(obj) };
    (2..=usize::from(scan))
        // SAFETY: within the closure.
        .map(|index| unsafe { read_field(obj, index.wrapping_mul(8)) })
        .collect()
}

/// The layouts of a test type `shapes.Shape`: `Circle(Float)`,
/// `Point { x: Int, label: T }` and `Empty`, registered consecutively.
struct Shape {
    first: abi::LayoutId,
}

fn shape_infos() -> Vec<LayoutInfo> {
    let key = layout::type_key("shapes", "Shape");
    vec![
        LayoutInfo::ctor("shapes.Shape.Circle", 0, &[(FieldKind::Float, None)])
            .named("Shape", "Circle", false, key),
        LayoutInfo::ctor(
            "shapes.Shape.Point",
            1,
            &[
                (FieldKind::Int, Some("x")),
                (FieldKind::Value, Some("label")),
            ],
        )
        .named("Shape", "Point", true, key),
        LayoutInfo::ctor("shapes.Shape.Empty", 2, &[]).named("Shape", "Empty", false, key),
    ]
}

impl Shape {
    fn register() -> Self {
        Self {
            first: layout::register_consecutive(shape_infos()).unwrap(),
        }
    }

    fn circle(&self, radius: f64) -> Word {
        let obj = alloc_object(16, abi::header(1, 0, self.first));
        // SAFETY: a fresh object with one raw field.
        unsafe { write_field(obj, 8, radius.to_bits()) };
        object::ptr_to_word(obj)
    }

    /// `Point { x, label }`, consuming `label`.
    fn point(&self, x: i64, label: Word) -> Word {
        let obj = alloc_object(24, abi::header(1, 1, self.first.wrapping_add(1)));
        // SAFETY: a fresh object: the value field at 8, the raw `Int` at 16.
        unsafe {
            write_field(obj, 8, label);
            write_field(obj, 16, x.cast_unsigned());
        }
        object::ptr_to_word(obj)
    }

    /// `Empty`, a unit constructor: a heap object here (generated code uses
    /// a static `cb$ctor$`), which prints and compares the same.
    fn empty(&self) -> Word {
        object::ptr_to_word(alloc_object(
            8,
            abi::header(1, 0, self.first.wrapping_add(2)),
        ))
    }
}

fn release(words: impl IntoIterator<Item = Word>) {
    for word in words {
        // SAFETY: tests release the references they own.
        unsafe { rc::drop(word) };
    }
}

fn eq(a: Word, b: Word) -> Result<bool, Reached> {
    // SAFETY: tests pass live words.
    unsafe { equal(a, b) }
}

fn hashed(word: Word) -> Result<u64, Reached> {
    // SAFETY: tests pass live words.
    unsafe { hash_with_seed(word, 42) }
}

fn text(word: Word) -> String {
    // SAFETY: tests pass live words.
    unsafe { debug_string(word) }
}

#[test]
fn numbers_compare_hash_and_print() {
    let before = live_objects();
    let words = [
        int_word(0),
        int_word(-7),
        int_word(i64::MAX),
        int_word(i64::MIN),
        u64_word(u64::MAX),
        f64_word(2.25),
        f32_word(0.1),
    ];
    let again = [
        int_word(0),
        int_word(-7),
        int_word(i64::MAX),
        int_word(i64::MIN),
        u64_word(u64::MAX),
        f64_word(2.25),
        f32_word(0.1),
    ];
    for (a, b) in words.iter().zip(&again) {
        assert_eq!(eq(*a, *b), Ok(true));
        assert_eq!(hashed(*a), hashed(*b));
    }
    assert_eq!(eq(int_word(1), int_word(2)), Ok(false));
    assert_eq!(
        words.map(text),
        [
            "0",
            "-7",
            "9223372036854775807",
            "-9223372036854775808",
            "18446744073709551615",
            "2.25",
            "0.1",
        ]
    );
    release(words);
    release(again);
    assert_eq!(live_objects(), before);
}

#[test]
fn float_equality_is_reflexive_and_ignores_the_zero_sign() {
    let nan = f64_word(f64::NAN);
    let other_nan = f64_word(-f64::NAN);
    let zero = f64_word(0.0);
    let negative_zero = f64_word(-0.0);
    let one = f64_word(1.0);
    assert_eq!(eq(nan, nan), Ok(true));
    assert_eq!(eq(nan, other_nan), Ok(true));
    assert_eq!(hashed(nan), hashed(other_nan));
    assert_eq!(eq(zero, negative_zero), Ok(true));
    assert_eq!(hashed(zero), hashed(negative_zero));
    assert_eq!(eq(zero, one), Ok(false));
    assert_eq!(eq(nan, one), Ok(false));
    let f32_nan = f32_word(f32::NAN);
    let f32_zero = f32_word(-0.0);
    let f32_other_zero = f32_word(0.0);
    assert_eq!(eq(f32_nan, f32_nan), Ok(true));
    assert_eq!(eq(f32_zero, f32_other_zero), Ok(true));
    assert_eq!(hashed(f32_zero), hashed(f32_other_zero));
    assert_eq!(text(negative_zero), "-0.0");
    assert_eq!(text(nan), "NaN");
    release([
        nan,
        other_nan,
        zero,
        negative_zero,
        one,
        f32_nan,
        f32_zero,
        f32_other_zero,
    ]);
}

#[test]
fn strings_print_with_the_literal_escapes() {
    let word = string("say \"hi\"\\ {x}\n\t\r\u{1b}\u{7f}é✓");
    assert_eq!(text(word), r#""say \"hi\"\\ \{x}\n\t\r\u{1b}\u{7f}é✓""#);
    let same = string("say \"hi\"\\ {x}\n\t\r\u{1b}\u{7f}é✓");
    let other = string("say");
    assert_eq!(eq(word, same), Ok(true));
    assert_eq!(hashed(word), hashed(same));
    assert_eq!(eq(word, other), Ok(false));
    assert_ne!(hashed(word), hashed(other));
    let data = bytes(b"fo\x00");
    assert_eq!(text(data), "bytes::from_list([0x66, 0x6f, 0x00])");
    let empty = bytes(b"");
    assert_eq!(text(empty), "bytes::from_list([])");
    release([word, same, other, data, empty]);
}

#[test]
fn builtin_constructors_compare_hash_and_print() {
    let before = live_objects();
    let a = some(int_word(3));
    let b = some(int_word(3));
    let c = some(int_word(4));
    assert_eq!(eq(a, b), Ok(true));
    assert_eq!(hashed(a), hashed(b));
    assert_eq!(eq(a, c), Ok(false));
    assert_eq!(eq(a, none()), Ok(false));
    assert_eq!(text(a), "Some(3)");
    assert_eq!(text(none()), "None");
    assert_eq!(text(nil()), "Nil");
    assert_eq!(text(bool_word(true)), "True");
    assert_eq!(eq(bool_word(true), bool_word(false)), Ok(false));
    assert_ne!(hashed(bool_word(true)), hashed(bool_word(false)));
    let result = ok(string("done"));
    let error = err(tuple(&[int_word(1), string("bad")]));
    assert_eq!(text(result), "Ok(\"done\")");
    assert_eq!(text(error), "Err((1, \"bad\"))");
    assert_eq!(eq(result, error), Ok(false));
    release([a, b, c, result, error]);
    assert_eq!(live_objects(), before);
}

#[test]
fn tuples_compare_element_by_element() {
    let a = tuple(&[int_word(1), string("x"), some(nil())]);
    let b = tuple(&[int_word(1), string("x"), some(nil())]);
    let c = tuple(&[int_word(1), string("y"), some(nil())]);
    assert_eq!(eq(a, b), Ok(true));
    assert_eq!(hashed(a), hashed(b));
    assert_eq!(eq(a, c), Ok(false));
    assert_eq!(text(a), "(1, \"x\", Some(Nil))");
    let wide: Vec<Word> = (0..300).map(int_word).collect();
    let wide = tuple(&wide);
    assert!(text(wide).ends_with(", 298, 299)"));
    release([a, b, c, wide]);
}

#[test]
fn closures_print_as_fn_and_cannot_be_compared_or_hashed() {
    let f = closure(&[int_word(1)]);
    let pair = tuple(&[int_word(1), closure(&[])]);
    assert_eq!(text(f), "<fn>");
    assert_eq!(text(pair), "(1, <fn>)");
    assert_eq!(eq(f, f), Err(Reached::Function));
    assert_eq!(hashed(f), Err(Reached::Function));
    assert_eq!(hashed(pair), Err(Reached::Function));
    // The first difference is found before the function is reached.
    let other = tuple(&[int_word(2), closure(&[])]);
    assert_eq!(eq(pair, other), Ok(false));
    let same_start = tuple(&[int_word(1), closure(&[])]);
    assert_eq!(eq(pair, same_start), Err(Reached::Function));
    release([f, pair, other, same_start]);
}

#[test]
fn user_constructors_use_the_layout_table() {
    let before = live_objects();
    let shape = Shape::register();
    let point = shape.point(-2, string("a"));
    let same = shape.point(-2, string("a"));
    let moved = shape.point(3, string("a"));
    let circle = shape.circle(1.5);
    let empty = shape.empty();
    assert_eq!(text(point), "Point { x: -2, label: \"a\" }");
    assert_eq!(text(circle), "Circle(1.5)");
    assert_eq!(text(empty), "Empty");
    assert_eq!(eq(point, same), Ok(true));
    assert_eq!(hashed(point), hashed(same));
    assert_eq!(eq(point, moved), Ok(false));
    assert_eq!(eq(point, circle), Ok(false));
    let circle_again = shape.circle(1.5);
    assert_eq!(eq(circle, circle_again), Ok(true));
    assert_eq!(hashed(circle), hashed(circle_again));
    let nested = some(shape.point(0, circle));
    assert_eq!(text(nested), "Some(Point { x: 0, label: Circle(1.5) })");
    release([point, same, moved, empty, nested, circle_again]);
    assert_eq!(live_objects(), before);
}

#[test]
fn hashing_uses_the_type_key_and_tag_not_the_layout_id() {
    // The same type registered twice gets two sets of ids (as two JITs in
    // one process would); values of either hash and compare the same.
    let first = Shape::register();
    let second = Shape::register();
    assert_ne!(first.first, second.first);
    let a = first.point(1, string("p"));
    let b = second.point(1, string("p"));
    assert_eq!(hashed(a), hashed(b));
    assert_eq!(eq(a, b), Ok(true));
    let c = first.empty();
    let d = second.empty();
    assert_eq!(hashed(c), hashed(d));
    assert_ne!(hashed(a), hashed(c));
    release([a, b, c, d]);
}

#[test]
fn hashes_depend_on_the_seed() {
    let word = string("seeded");
    // SAFETY: a live string.
    let (one, two) = unsafe { (hash_with_seed(word, 1), hash_with_seed(word, 2)) };
    assert_ne!(one, two);
    // Outside an entry, `hash` uses the per-process seed.
    // SAFETY: as above.
    assert_eq!(unsafe { hash(word) }, unsafe {
        hash_with_seed(word, process_seed())
    });
    release([word]);
}

// A runtime-defined kind: a node holding `len` uniform words.

unsafe fn bag_size(word: Word) -> u64 {
    // SAFETY: a live bag.
    16_u64.saturating_add(unsafe { read_field(word_to_ptr(word), 8) }.saturating_mul(8))
}

/// The elements of a live bag.
unsafe fn bag_items(word: Word) -> Vec<Word> {
    let obj = word_to_ptr(word);
    // SAFETY: a live bag.
    let len = unsafe { read_field(obj, 8) };
    (0..object::to_usize(len))
        // SAFETY: within the bag.
        .map(|index| unsafe { read_field(obj, index.wrapping_add(2).wrapping_mul(8)) })
        .collect()
}

unsafe fn bag_children(word: Word, visit: &mut dyn FnMut(Word)) {
    // SAFETY: a live bag.
    for item in unsafe { bag_items(word) } {
        visit(item);
    }
}

unsafe fn bag_equal(a: Word, b: Word, cx: &mut Cx) -> bool {
    // SAFETY: live bags.
    let (xs, ys) = unsafe { (bag_items(a), bag_items(b)) };
    if xs.len() != ys.len() {
        return false;
    }
    for (x, y) in xs.into_iter().zip(ys) {
        cx.push(x, y);
    }
    true
}

unsafe fn bag_hash(word: Word, h: &mut Hasher) {
    // SAFETY: a live bag.
    let items = unsafe { bag_items(word) };
    h.write_u64(to_u64(items.len()));
    for item in items {
        h.push(item);
    }
}

unsafe fn bag_debug(word: Word, p: &mut Printer) {
    p.text("bag[");
    // SAFETY: a live bag.
    for (index, item) in unsafe { bag_items(word) }.into_iter().enumerate() {
        if index > 0 {
            p.text_owned("; ".to_owned());
        }
        p.value(item);
    }
    p.text("]");
}

static BAG: CustomKind = CustomKind {
    name: "bag",
    size: bag_size,
    children: bag_children,
    equal: Some(bag_equal),
    hash: Some(bag_hash),
    debug: Some(bag_debug),
    handle: false,
    finalize: None,
    shape: layout::KindShape::Opaque,
};

/// An id unused by every runtime module (45 to 63 are free; 40 to 44 are
/// the handle kinds).
const BAG_ID: abi::LayoutId = 46;

fn bag(items: &[Word]) -> Word {
    layout::register_builtin(BAG_ID, LayoutInfo::custom(&BAG)).unwrap();
    let len = to_u64(items.len());
    let obj = alloc_object(
        16_u64.saturating_add(len.saturating_mul(8)),
        abi::header(1, 0, BAG_ID),
    );
    // SAFETY: initializing a fresh bag.
    unsafe {
        write_field(obj, 8, len);
        for (index, item) in items.iter().enumerate() {
            write_field(obj, index.wrapping_add(2).wrapping_mul(8), *item);
        }
    }
    object::ptr_to_word(obj)
}

#[test]
fn custom_kinds_take_part_through_their_functions() {
    let domain = Domain::new();
    alloc::with_domain(Arc::clone(&domain), || {
        let a = bag(&[int_word(1), bag(&[string("x")])]);
        let b = bag(&[int_word(1), bag(&[string("x")])]);
        let c = bag(&[int_word(1), bag(&[string("y")])]);
        assert_eq!(eq(a, b), Ok(true));
        assert_eq!(hashed(a), hashed(b));
        assert_eq!(eq(a, c), Ok(false));
        assert_ne!(hashed(a), hashed(c));
        assert_eq!(text(a), "bag[1; bag[\"x\"]]");
        let with_fn = bag(&[closure(&[])]);
        assert_eq!(eq(with_fn, with_fn), Err(Reached::Function));
        release([a, b, c, with_fn]);
    });
    assert_eq!(domain.live_objects(), 0);
}

// Deep data: a million-deep chain of a user constructor and of tuples.

const DEPTH: i64 = 1_000_000;

/// `Point { x: n, label: <next> }` nested `DEPTH` deep around `Empty`.
fn shape_chain(shape: &Shape) -> Word {
    let mut chain = shape.empty();
    for n in 0..DEPTH {
        chain = shape.point(n, chain);
    }
    chain
}

/// `(n, <next>)` nested `DEPTH` deep around `Nil`.
fn tuple_chain() -> Word {
    let mut chain = nil();
    for n in 0..DEPTH {
        chain = tuple(&[int_word(n), chain]);
    }
    chain
}

/// Equality, hashing and debug text of two equal deep chains; returns 7
/// when every check holds (1 equality, 2 hashing, 4 text).
fn deep_checks(a: Word, b: Word, start: &str, innermost: &str, end: &str) -> u64 {
    let same = eq(a, b) == Ok(true);
    let hashes = hashed(a).is_ok() && hashed(a) == hashed(b);
    let printed = text(a);
    let shape = printed.starts_with(start)
        && printed.ends_with(end)
        && printed.matches(innermost).count() == 1;
    if !shape {
        eprintln!(
            "{:?} … {:?}",
            printed.chars().take(80).collect::<String>(),
            printed.chars().rev().take(40).collect::<String>()
        );
    }
    release([a, b]);
    u64::from(same) | (u64::from(hashes) << 1) | (u64::from(shape) << 2)
}

unsafe extern "C" fn deep_shapes(_: *mut ExecCtx) -> u64 {
    let shape = Shape::register();
    let (a, b) = (shape_chain(&shape), shape_chain(&shape));
    deep_checks(
        a,
        b,
        "Point { x: 999999, label: Point { x: 999998, ",
        "Point { x: 1, label: Point { x: 0, label: Empty } }",
        " } } }",
    )
}

unsafe extern "C" fn deep_tuples(_: *mut ExecCtx) -> u64 {
    let (a, b) = (tuple_chain(), tuple_chain());
    deep_checks(a, b, "(999999, (999998, ", "(1, (0, Nil)))", ")))")
}

#[test]
#[cfg_attr(miri, ignore = "a million-deep value")]
fn deep_values_compare_hash_and_print_on_an_entry_stack() {
    let thunks: [crate::entry::EntryFn; 2] = [deep_shapes, deep_tuples];
    for thunk in thunks {
        let domain = Domain::new();
        let config = EntryConfig {
            domain: Arc::clone(&domain),
            ..EntryConfig::new(EntryMode::Test)
        };
        assert_eq!(config.stack_size, 8 << 20);
        // SAFETY: test thunks with the `EntryFn` ABI and static code.
        let outcome = unsafe { run_entry(thunk, &config) }.unwrap();
        assert_eq!(outcome, EntryOutcome::Returned(7));
        assert_eq!(domain.live_objects(), 0);
    }
}

#[test]
fn the_process_seed_is_drawn_once() {
    assert_eq!(process_seed(), process_seed());
}
