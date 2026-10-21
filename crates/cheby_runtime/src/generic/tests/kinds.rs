//! Equality, hashing and debug text of lists, types with `@std_hooks`,
//! external types, descriptors, `dyn` values and sorted groups, the
//! `String` object printer, and a property test that equal values hash
//! equally.

use proptest::prelude::*;

use super::*;
use crate::desc::{self, StaticNominal, StaticText};
use crate::entry::Callbacks;
use crate::list::{
    cheby_list_append, cheby_list_prepend, cheby_list_slice, cheby_list_unique,
    cheby_rt_static_empty_list,
};

/// A list of the given owned elements.
fn list(items: Vec<Word>) -> Word {
    crate::list::from_iter(items)
}

/// The empty list.
fn empty_list() -> Word {
    object::ptr_to_word(std::ptr::from_ref(&cheby_rt_static_empty_list).cast::<u8>())
}

/// `items` as a list built by prepending from the end, shaped differently
/// from [`list`]'s pushes; the items are consumed.
fn prepended(items: Vec<Word>) -> Word {
    let mut xs = empty_list();
    for item in items.into_iter().rev() {
        // SAFETY: the kernel borrows its arguments.
        let next = unsafe { cheby_list_prepend(xs, item) };
        release([xs, item]);
        xs = next;
    }
    xs
}

/// `items` as a view in the middle of a longer list made by appending:
/// `[0, …items, 0]` sliced to the items.
fn viewed(items: Vec<Word>) -> Word {
    let len = i64::try_from(items.len()).unwrap();
    let front = list(vec![int_word(0)]);
    let middle = list(items);
    let back = list(vec![int_word(0)]);
    // SAFETY: the kernels borrow their arguments.
    let whole = unsafe {
        let joined = cheby_list_append(front, middle);
        let all = cheby_list_append(joined, back);
        release([joined]);
        all
    };
    // SAFETY: as above.
    let view = unsafe { cheby_list_slice(whole, 1, len.wrapping_add(1)) };
    release([front, middle, back, whole]);
    view
}

#[test]
fn lists_compare_hash_and_print_element_by_element() {
    let domain = Domain::new();
    alloc::with_domain(Arc::clone(&domain), || {
        let items = || vec![int_word(1), string("two"), some(int_word(3))];
        let pushed = list(items());
        let shapes = [prepended(items()), viewed(items())];
        assert_eq!(text(pushed), "[1, \"two\", Some(3)]");
        for other in shapes {
            assert_eq!(eq(pushed, other), Ok(true));
            assert_eq!(hashed(pushed), hashed(other));
            assert_eq!(text(other), text(pushed));
            release([other]);
        }
        let shorter = list(vec![int_word(1), string("two")]);
        let changed = list(vec![int_word(1), string("two"), some(int_word(4))]);
        assert_eq!(eq(pushed, shorter), Ok(false));
        assert_eq!(eq(pushed, changed), Ok(false));
        assert_ne!(hashed(pushed), hashed(shorter));
        let empty = empty_list();
        assert_eq!(text(empty), "[]");
        let made_empty = viewed(Vec::new());
        assert_eq!(eq(empty, made_empty), Ok(true));
        assert_eq!(hashed(empty), hashed(made_empty));
        assert_eq!(eq(empty, pushed), Ok(false));
        let nested = list(vec![
            list(vec![int_word(1)]),
            empty_list(),
            tuple(&[empty_list()]),
        ]);
        assert_eq!(text(nested), "[[1], [], ([])]");
        let with_fn = list(vec![int_word(1), closure(&[])]);
        assert_eq!(text(with_fn), "[1, <fn>]");
        assert_eq!(hashed(with_fn), Err(Reached::Function));
        assert_eq!(eq(with_fn, with_fn), Err(Reached::Function));
        // Under Miri, which interprets every step, a shorter list that
        // still spans several tree levels (D-721, D-884).
        let n: i64 = if cfg!(miri) { 2_000 } else { 100_000 };
        let long = list((0..n).map(int_word).collect());
        let long_again = prepended((0..n).map(int_word).collect());
        assert_eq!(eq(long, long_again), Ok(true));
        assert_eq!(hashed(long), hashed(long_again));
        let end = format!(", {}, {}]", n.saturating_sub(2), n.saturating_sub(1));
        assert!(text(long).ends_with(&end));
        release([
            pushed, shorter, changed, made_empty, nested, with_fn, long, long_again,
        ]);
    });
    assert_eq!(domain.live_objects(), 0);
}

// ---------------------------------------------------------------------
// `@std_hooks`: a test type `bags.Bag` with `Small(Int)` and `Big(Int)`
// whose hooks compare, hash and print the number alone.

/// The code word of a test hook closure: the hook's number, tagged like a
/// code address.
const HOOK_EQUAL: u64 = 0x10;
const HOOK_HASH: u64 = 0x20;
const HOOK_DEBUG: u64 = 0x30;

/// A static-like hook closure (count 0 is not needed: the table keeps it).
fn hook(code: u64) -> Word {
    let obj = alloc_object(16, abi::header(1, 1, ids::CLOSURE));
    // SAFETY: initializing a fresh closure with no captures.
    unsafe { write_field(obj, 8, code | 1) };
    object::ptr_to_word(obj)
}

/// The code number of a closure built by [`hook`].
fn code_of(closure: Word) -> u64 {
    // SAFETY: test closures.
    let code = unsafe { read_field(word_to_ptr(closure), 8) };
    code & !1
}

/// The number inside a `Bag`.
fn bag_number(word: Word) -> i64 {
    // SAFETY: a live `Small` or `Big`, whose raw `Int` is at offset 8.
    unsafe { read_field(word_to_ptr(word), 8) }.cast_signed()
}

unsafe extern "C" fn tramp1(_: *mut ExecCtx, closure: u64, a: u64) -> u64 {
    let n = bag_number(a);
    release([a]);
    match code_of(closure) {
        HOOK_HASH => int_word(n),
        HOOK_DEBUG => string(&format!("bag of {n}")),
        other => panic!("no one-argument hook {other:#x}"),
    }
}

unsafe extern "C" fn tramp2(_: *mut ExecCtx, closure: u64, a: u64, b: u64) -> u64 {
    assert_eq!(code_of(closure), HOOK_EQUAL);
    let same = bag_number(a) == bag_number(b);
    release([a, b]);
    bool_word(same)
}

/// The layouts of `bags.Bag`, with hooks.
struct Bags {
    first: abi::LayoutId,
}

impl Bags {
    fn register() -> Self {
        let key = layout::type_key("bags", "Bag");
        let first = layout::register_consecutive(vec![
            LayoutInfo::ctor("bags.Bag.Small", 0, &[(FieldKind::Int, None)])
                .named("Bag", "Small", false, key),
            LayoutInfo::ctor("bags.Bag.Big", 1, &[(FieldKind::Int, None)])
                .named("Bag", "Big", false, key),
        ])
        .unwrap();
        let hooks = [hook(HOOK_EQUAL), hook(HOOK_HASH), hook(HOOK_DEBUG)];
        for hook in hooks {
            // SAFETY: the layout table keeps these words forever.
            unsafe { alloc::forget(hook) };
        }
        for id in [first, first.wrapping_add(1)] {
            layout::set_hooks(id, hooks).unwrap();
        }
        Self { first }
    }

    fn make(&self, big: bool, n: i64) -> Word {
        let id = self.first.wrapping_add(u32::from(big));
        let obj = alloc_object(16, abi::header(1, 0, id));
        // SAFETY: a fresh object with one raw field.
        unsafe { write_field(obj, 8, n.cast_unsigned()) };
        object::ptr_to_word(obj)
    }
}

unsafe extern "C" fn hooked_checks(_: *mut ExecCtx) -> u64 {
    let bags = Bags::register();
    let small = bags.make(false, 3);
    let big = bags.make(true, 3);
    let other = bags.make(false, 4);
    let mut ok = eq(small, big) == Ok(true) && eq(small, other) == Ok(false);
    // `hash` hashes the key its hook returns, so different shapes with the
    // same contents hash equally.
    ok &= hashed(small).is_ok() && hashed(small) == hashed(big);
    ok &= hashed(small) != hashed(other);
    ok &= text(small) == "bag of 3" && text(big) == "bag of 3";
    let pair = tuple(&[small, other]);
    let listed = list(vec![big, string("x")]);
    ok &= text(pair) == "(bag of 3, bag of 4)";
    ok &= text(listed) == "[bag of 3, \"x\"]";
    // SAFETY: a live value; the object printer gives the same text.
    let object = unsafe { debug_object(pair) };
    // SAFETY: a live string.
    ok &= unsafe { crate::string::as_str(object) } == Some("(bag of 3, bag of 4)");
    let ones = list(vec![bags.make(false, 1), bags.make(true, 1)]);
    // SAFETY: the kernel borrows its list.
    let unique = unsafe { cheby_list_unique(ones) };
    ok &= text(unique) == "[bag of 1]";
    release([pair, listed, object, ones, unique]);
    u64::from(ok)
}

#[test]
fn std_hooks_compare_hash_and_print_through_their_closures() {
    let domain = Domain::new();
    let config = EntryConfig {
        domain: Arc::clone(&domain),
        callbacks: Callbacks {
            t1: Some(tramp1),
            t2: Some(tramp2),
            ..Callbacks::default()
        },
        ..EntryConfig::new(EntryMode::Test)
    };
    // SAFETY: a test thunk and trampolines with their declared ABIs.
    let outcome = unsafe { run_entry(hooked_checks, &config) }.unwrap();
    assert_eq!(outcome, EntryOutcome::Returned(1));
    assert_eq!(domain.live_objects(), 0);
}

// ---------------------------------------------------------------------
// External types (§12.5.3, D-233)

#[test]
fn external_values_print_their_type_and_stop_equality() {
    let domain = Domain::new();
    alloc::with_domain(Arc::clone(&domain), || {
        let id = layout::register_external("Db", layout::type_key("app", "Db")).unwrap();
        assert!(layout::get(id).is_some_and(LayoutInfo::is_external));
        let handle = || {
            let obj = alloc_object(16, abi::header(1, 0, id));
            // SAFETY: a fresh handle object with its raw handle.
            unsafe { write_field(obj, 8, 0xdead_beef) };
            object::ptr_to_word(obj)
        };
        let db = handle();
        let pair = tuple(&[int_word(1), handle()]);
        assert_eq!(text(db), "<Db>");
        assert_eq!(text(pair), "(1, <Db>)");
        assert_eq!(eq(db, db), Err(Reached::Function));
        assert_eq!(hashed(pair), Err(Reached::Function));
        let other = tuple(&[int_word(2), handle()]);
        assert_eq!(eq(pair, other), Ok(false));
        release([db, pair, other]);
    });
    assert_eq!(domain.live_objects(), 0);
}

// ---------------------------------------------------------------------
// Descriptors and `dyn` values (D-524, D-530)

static GEO: StaticText<3> = StaticText::new(b"geo");
static POINT: StaticText<5> = StaticText::new(b"Point");
static POINT_DESC: StaticNominal = StaticNominal::new(
    std::ptr::from_ref(&POINT).cast::<u8>(),
    std::ptr::from_ref(&GEO).cast::<u8>(),
);
/// Another type with the same name and module, such as a REPL
/// redefinition: a different nominal type (§8.6.1).
static POINT_AGAIN: StaticNominal = StaticNominal::new(
    std::ptr::from_ref(&POINT).cast::<u8>(),
    std::ptr::from_ref(&GEO).cast::<u8>(),
);

fn builtin(name: &str) -> Word {
    desc::builtin(name).unwrap()
}

/// A descriptor `nominal<args…>`, consuming `args`.
fn applied(nominal: Word, args: &[Word]) -> Word {
    let count = args.len().wrapping_add(1);
    let obj = alloc_object(
        to_u64(count.wrapping_add(1).wrapping_mul(8)),
        abi::header(1, u8::try_from(count).unwrap(), ids::DESC),
    );
    // SAFETY: initializing a fresh descriptor.
    unsafe {
        write_field(obj, 8, nominal);
        for (index, arg) in args.iter().enumerate() {
            write_field(obj, index.wrapping_add(2).wrapping_mul(8), *arg);
        }
    }
    object::ptr_to_word(obj)
}

/// A dictionary holding only its descriptor (consumed).
fn dict(desc: Word) -> Word {
    let obj = alloc_object(16, abi::header(1, 1, ids::DICT));
    // SAFETY: initializing a fresh dictionary.
    unsafe { write_field(obj, 8, desc) };
    object::ptr_to_word(obj)
}

/// A `dyn` value of `value` with a dictionary for `desc` (both consumed).
fn dyn_value(desc: Word, value: Word) -> Word {
    let obj = alloc_object(24, abi::header(1, 2, ids::DYN));
    // SAFETY: initializing a fresh `dyn` value.
    unsafe {
        write_field(obj, 8, dict(desc));
        write_field(obj, 16, value);
    }
    object::ptr_to_word(obj)
}

#[test]
fn descriptors_compare_by_structure_and_print_as_types() {
    let domain = Domain::new();
    alloc::with_domain(Arc::clone(&domain), || {
        let option_int = applied(builtin("option"), &[builtin("int")]);
        let option_int_again = applied(builtin("option"), &[applied(builtin("int"), &[])]);
        let option_string = applied(builtin("option"), &[builtin("string")]);
        assert_eq!(eq(option_int, option_int_again), Ok(true));
        assert_eq!(hashed(option_int), hashed(option_int_again));
        assert_eq!(eq(option_int, option_string), Ok(false));
        assert_eq!(eq(builtin("int"), builtin("i64")), Ok(false));
        let point = POINT_DESC.word();
        assert_eq!(eq(point, POINT_AGAIN.word()), Ok(false));
        let point_applied = applied(point, &[]);
        assert_eq!(eq(point, point_applied), Ok(true));
        assert_eq!(hashed(point), hashed(point_applied));
        assert_eq!(text(option_int), "Option<Int>");
        let pair = applied(builtin("tuple"), &[builtin("int"), point]);
        assert_eq!(text(pair), "(Int, Point)");
        let function = applied(
            builtin("fn"),
            &[builtin("int"), builtin("string"), builtin("bool")],
        );
        assert_eq!(text(function), "fn(Int, String) -> Bool");
        let list_of_pairs = applied(builtin("list"), &[pair]);
        assert_eq!(text(list_of_pairs), "List<(Int, Point)>");
        release([
            option_int,
            option_int_again,
            option_string,
            point_applied,
            function,
            list_of_pairs,
        ]);
    });
    assert_eq!(domain.live_objects(), 0);
}

#[test]
fn dyn_values_compare_their_types_then_their_values() {
    let domain = Domain::new();
    alloc::with_domain(Arc::clone(&domain), || {
        let a = dyn_value(builtin("int"), int_word(1));
        let b = dyn_value(applied(builtin("int"), &[]), int_word(1));
        let c = dyn_value(builtin("int"), int_word(2));
        // The same value under another type is another value.
        let d = dyn_value(builtin("i64"), int_word(1));
        let some_list = dyn_value(
            applied(
                builtin("option"),
                &[applied(builtin("list"), &[builtin("int")])],
            ),
            some(list(vec![int_word(1)])),
        );
        assert_eq!(eq(a, b), Ok(true));
        assert_eq!(hashed(a), hashed(b));
        assert_eq!(eq(a, c), Ok(false));
        assert_eq!(eq(a, d), Ok(false));
        assert_ne!(hashed(a), hashed(d));
        assert_eq!(text(a), "1");
        assert_eq!(text(some_list), "Some([1])");
        let holding_fn = dyn_value(applied(builtin("fn"), &[builtin("int")]), closure(&[]));
        assert_eq!(text(holding_fn), "<fn>");
        assert_eq!(eq(holding_fn, holding_fn), Err(Reached::Function));
        release([a, b, c, d, some_list, holding_fn]);
    });
    assert_eq!(domain.live_objects(), 0);
}

static APP: StaticText<3> = StaticText::new(b"app");
static DYN_ANY: StaticText<7> = StaticText::new(b"dyn Any");
/// The nominal descriptor of the type `dyn app::Any`.
static DYN_ANY_DESC: StaticNominal = StaticNominal::new(
    std::ptr::from_ref(&DYN_ANY).cast::<u8>(),
    std::ptr::from_ref(&APP).cast::<u8>(),
);

#[test]
fn a_dyn_value_inside_a_dyn_value_is_that_value() {
    // Generic code with `T = dyn Shape` converting to `dyn Any` wraps the
    // `dyn Shape` value again, with the descriptor of `dyn Shape`; it is
    // the same value as a direct conversion, which keeps the inner
    // descriptor (D-502, D-604).
    let domain = Domain::new();
    alloc::with_domain(Arc::clone(&domain), || {
        let point = POINT_DESC.word();
        let direct = dyn_value(point, int_word(7));
        let inner = dyn_value(point, int_word(7));
        let wrapped = dyn_value(DYN_ANY_DESC.word(), inner);
        let twice = dyn_value(
            DYN_ANY_DESC.word(),
            dyn_value(DYN_ANY_DESC.word(), dyn_value(point, int_word(7))),
        );
        let other = dyn_value(DYN_ANY_DESC.word(), dyn_value(point, int_word(8)));
        let as_any = dyn_value(DYN_ANY_DESC.word(), int_word(7));
        assert_eq!(eq(direct, wrapped), Ok(true));
        assert_eq!(eq(wrapped, twice), Ok(true));
        assert_eq!(hashed(direct), hashed(wrapped));
        assert_eq!(hashed(direct), hashed(twice));
        assert_eq!(eq(wrapped, other), Ok(false));
        // An `Int` is not a `Point`, even with the same number inside.
        assert_eq!(eq(as_any, direct), Ok(false));
        assert_eq!(text(twice), "7");
        release([direct, wrapped, twice, other, as_any]);
    });
    assert_eq!(domain.live_objects(), 0);
}

/// `dyn (n, <next>)` nested `DEPTH` deep around `Nil`: every tuple is
/// inside a `dyn` value whose descriptor is `(Int, dyn Any)` (`Nil` for
/// the innermost).
fn dyn_chain() -> Word {
    let mut chain = dyn_value(builtin("nil"), nil());
    for n in 0..super::DEPTH {
        let desc = applied(builtin("tuple"), &[builtin("int"), DYN_ANY_DESC.word()]);
        chain = dyn_value(desc, tuple(&[int_word(n), chain]));
    }
    chain
}

unsafe extern "C" fn deep_dyn_values(_: *mut ExecCtx) -> u64 {
    let (a, b) = (dyn_chain(), dyn_chain());
    // A different innermost value, under the same descriptors.
    let mut c = dyn_value(builtin("nil"), int_word(0));
    for n in 0..super::DEPTH {
        let desc = applied(builtin("tuple"), &[builtin("int"), DYN_ANY_DESC.word()]);
        c = dyn_value(desc, tuple(&[int_word(n), c]));
    }
    let differs = eq(a, c) == Ok(false);
    release([c]);
    let checks = super::deep_checks(a, b, "(999999, (999998, ", "(1, (0, Nil)))", ")))");
    checks | (u64::from(differs) << 3)
}

#[test]
#[cfg_attr(miri, ignore = "a million-deep value")]
fn deep_chains_through_dyn_values_compare_hash_and_print_on_an_entry_stack() {
    let domain = Domain::new();
    let config = EntryConfig {
        domain: Arc::clone(&domain),
        ..EntryConfig::new(EntryMode::Test)
    };
    // SAFETY: a test thunk with the `EntryFn` ABI and static code.
    let outcome = unsafe { run_entry(deep_dyn_values, &config) }.unwrap();
    assert_eq!(outcome, EntryOutcome::Returned(15));
    assert_eq!(domain.live_objects(), 0);
}

// ---------------------------------------------------------------------
// Sorted groups (D-344, D-382), through a test kind that prints like a
// map: `table([(k, v), …])`.

unsafe fn table_debug(word: Word, p: &mut Printer) {
    // SAFETY: a live bag of alternating keys and values.
    let items = unsafe { super::bag_items(word) };
    let pairs = items
        .chunks(2)
        .filter_map(|pair| match *pair {
            [k, v] => Some(SortItem::Pair(k, v)),
            [k] => Some(SortItem::Value(k)),
            _ => None,
        })
        .collect::<Vec<_>>();
    p.text("table([");
    p.sorted(pairs);
    p.text("])");
}

static TABLE: CustomKind = CustomKind {
    name: "table",
    size: super::bag_size,
    children: super::bag_children,
    equal: None,
    hash: None,
    debug: Some(table_debug),
    handle: false,
    finalize: None,
    shape: crate::layout::KindShape::Opaque,
};

/// An id unused by every runtime module.
const TABLE_ID: abi::LayoutId = 47;

fn table(items: &[Word]) -> Word {
    layout::register_builtin(TABLE_ID, LayoutInfo::custom(&TABLE)).unwrap();
    let word = super::bag(items);
    let obj = word_to_ptr(word);
    // Re-label the bag as a table: same shape.
    // SAFETY: a fresh, unique bag.
    unsafe { write_field(obj, 0, abi::header(1, 0, TABLE_ID)) };
    word
}

#[test]
fn sorted_groups_print_by_their_whole_text_without_recursion() {
    let domain = Domain::new();
    alloc::with_domain(Arc::clone(&domain), || {
        let inner = table(&[string("b"), int_word(2), string("a"), int_word(1)]);
        let outer = table(&[
            int_word(9),
            string("nine"),
            int_word(10),
            inner,
            int_word(-1),
            list(vec![]),
        ]);
        assert_eq!(
            text(outer),
            "table([(-1, []), (10, table([(\"a\", 1), (\"b\", 2)])), (9, \"nine\")])"
        );
        let in_tuple = tuple(&[outer, string("after")]);
        assert!(text(in_tuple).ends_with("(9, \"nine\")]), \"after\")"));
        release([in_tuple]);
        // Deeply nested groups print without deepening the Rust stack, and
        // a finished group is never copied into its parent's text, so the
        // cost grows linearly with the nesting depth (100,000 groups took
        // minutes when each was copied up).
        let depth = if cfg!(miri) { 100 } else { 100_000 };
        let mut deep = int_word(0);
        for _ in 0..depth {
            deep = table(&[deep]);
        }
        let printed = text(deep);
        assert_eq!(
            printed,
            format!("{}0{}", "table([".repeat(depth), "])".repeat(depth))
        );
        release([deep]);
    });
    assert_eq!(domain.live_objects(), 0);
}

// ---------------------------------------------------------------------
// The `String` object printer

#[test]
fn debug_text_is_written_into_a_string_object() {
    let domain = Domain::new();
    alloc::with_domain(Arc::clone(&domain), || {
        // A shorter list under Miri (D-721, D-884).
        let length: i64 = if cfg!(miri) { 500 } else { 10_000 };
        let value = list((0..length).map(|n| some(int_word(n))).collect());
        // SAFETY: a live value.
        let object = unsafe { debug_object(value) };
        // SAFETY: a live string.
        let written = unsafe { crate::string::as_str(object) }.unwrap().to_owned();
        assert_eq!(written, text(value));
        // SAFETY: as above.
        assert!(unsafe { rc::is_unique(object) });
        // SAFETY: a live scalar.
        let small = unsafe { debug_object(int_word(-5)) };
        // SAFETY: a live string.
        assert_eq!(unsafe { crate::string::as_str(small) }, Some("-5"));
        release([value, object, small]);
    });
    assert_eq!(domain.live_objects(), 0);
}

// ---------------------------------------------------------------------
// Maps and sets (W3-C's kinds): equality across seeds, hashing with the
// caller's key, and sorted debug text (plan §4.3.5, D-344, D-382, D-538).

/// `map::from_list` of owned `(key, value)` pairs, under the current seed.
fn map_of(pairs: Vec<(Word, Word)>) -> Word {
    let pairs = list(pairs.into_iter().map(|(k, v)| tuple(&[k, v])).collect());
    // SAFETY: the kernel borrows its list.
    let map = unsafe { crate::hamt::cheby_map_from_list(pairs) };
    release([pairs]);
    map
}

/// `set::from_list` of owned items, under the current seed.
fn set_of(items: Vec<Word>) -> Word {
    let items = list(items);
    // SAFETY: the kernel borrows its list.
    let set = unsafe { crate::hamt::cheby_set_from_list(items) };
    release([items]);
    set
}

unsafe extern "C" fn hashed_collections(_: *mut ExecCtx) -> u64 {
    let entries = || vec![(int_word(10), string("ten")), (int_word(9), string("nine"))];
    let numbers = crate::entry::with_seed(1, || map_of(entries()));
    let mut reversed = entries();
    reversed.reverse();
    let again = crate::entry::with_seed(2, || map_of(reversed));
    let fewer = crate::entry::with_seed(3, || map_of(vec![(int_word(10), string("ten"))]));
    let mut ok = eq(numbers, again) == Ok(true) && eq(numbers, fewer) == Ok(false);
    // Hashing with one caller key gives one hash, whatever the seeds the
    // maps were built with.
    ok &= hashed(numbers).is_ok() && hashed(numbers) == hashed(again);
    ok &= text(numbers) == "map::from_list([(10, \"ten\"), (9, \"nine\")])";
    let words = set_of(vec![string("pear"), string("apple"), string("pear")]);
    ok &= text(words) == "set::from_list([\"apple\", \"pear\"])";
    let nested = set_of(vec![numbers, again, map_of(Vec::new())]);
    ok &= text(nested)
        == "set::from_list([map::from_list([(10, \"ten\"), (9, \"nine\")]), map::from_list([])])";
    let in_list = list(vec![words, fewer]);
    ok &=
        text(in_list) == "[set::from_list([\"apple\", \"pear\"]), map::from_list([(10, \"ten\")])]";
    let holding_fn = map_of(vec![(int_word(1), closure(&[]))]);
    ok &= text(holding_fn) == "map::from_list([(1, <fn>)])";
    ok &= hashed(holding_fn) == Err(Reached::Function);
    release([nested, in_list, holding_fn]);
    u64::from(ok)
}

#[test]
fn maps_and_sets_compare_across_seeds_and_print_sorted() {
    let domain = Domain::new();
    let config = EntryConfig {
        domain: Arc::clone(&domain),
        ..EntryConfig::new(EntryMode::Test)
    };
    // SAFETY: a test thunk with the `EntryFn` ABI.
    let outcome = unsafe { run_entry(hashed_collections, &config) }.unwrap();
    assert_eq!(outcome, EntryOutcome::Returned(1));
    assert_eq!(domain.live_objects(), 0);
}

// ---------------------------------------------------------------------
// Deep data in the shape of 025's `LinkedList<T>`: `Node(T, LinkedList<T>)`
// with a generic (boxed) first field, a million deep, on an entry's 8 MiB
// stack (plan §0, §6).

/// The layouts of `linked.LinkedList`: `Empty`, `Node(T, LinkedList<T>)`.
fn linked_list_layouts() -> abi::LayoutId {
    let key = layout::type_key("linked", "LinkedList");
    layout::register_consecutive(vec![
        LayoutInfo::ctor("linked.LinkedList.Empty", 0, &[]).named(
            "LinkedList",
            "Empty",
            false,
            key,
        ),
        LayoutInfo::ctor(
            "linked.LinkedList.Node",
            1,
            &[(FieldKind::Value, None), (FieldKind::Value, None)],
        )
        .named("LinkedList", "Node", false, key),
    ])
    .unwrap()
}

/// `Node(0, Node(1, … Node(DEPTH - 1, Empty)))`, with `Float` items, which
/// are boxed in the generic field.
fn linked_chain(first: abi::LayoutId) -> Word {
    let mut chain = object::ptr_to_word(alloc_object(8, abi::header(1, 0, first)));
    for n in (0..DEPTH).rev() {
        let obj = alloc_object(24, abi::header(1, 2, first.wrapping_add(1)));
        let item = f64::from(i32::try_from(n).unwrap());
        // SAFETY: a fresh node: the item, then the rest.
        unsafe {
            write_field(obj, 8, f64_word(item));
            write_field(obj, 16, chain);
        }
        chain = object::ptr_to_word(obj);
    }
    chain
}

unsafe extern "C" fn deep_linked_lists(_: *mut ExecCtx) -> u64 {
    let first = linked_list_layouts();
    let (a, b) = (linked_chain(first), linked_chain(first));
    deep_checks(
        a,
        b,
        "Node(0.0, Node(1.0, Node(2.0, ",
        "Node(999999.0, Empty)",
        ")))",
    )
}

/// `[[…[0]…]]`: lists nested a million deep.
unsafe extern "C" fn deep_lists(_: *mut ExecCtx) -> u64 {
    let nested = || {
        let mut value = int_word(0);
        for _ in 0..DEPTH {
            value = list(vec![value]);
        }
        value
    };
    deep_checks(nested(), nested(), "[[[[", "[0]", "]]]]")
}

#[test]
#[cfg_attr(miri, ignore = "a million-deep value")]
fn million_deep_linked_lists_and_lists_compare_hash_and_print() {
    let thunks: [crate::entry::EntryFn; 2] = [deep_linked_lists, deep_lists];
    for thunk in thunks {
        let domain = Domain::new();
        let config = EntryConfig {
            domain: Arc::clone(&domain),
            ..EntryConfig::new(EntryMode::Test)
        };
        assert_eq!(config.stack_size, 8 << 20);
        // SAFETY: a test thunk with the `EntryFn` ABI.
        let outcome = unsafe { run_entry(thunk, &config) }.unwrap();
        assert_eq!(outcome, EntryOutcome::Returned(7));
        assert_eq!(domain.live_objects(), 0);
    }
}

// ---------------------------------------------------------------------
// Property: equal values hash equally, and `==` is the model's equality.

/// A value of the property test's types, and its model.
#[derive(Debug, Clone)]
enum Tree {
    Int(i64),
    Float(f64),
    Text(String),
    Tuple(Vec<Tree>),
    List(Vec<Tree>),
    Some(Box<Tree>),
    None,
    /// `shapes.Shape.Point { x, label }` of the parent module's tests.
    Point(i64, Box<Tree>),
}

fn tree() -> impl Strategy<Value = Tree> {
    let leaf = prop_oneof![
        (-3_i64..3).prop_map(Tree::Int),
        prop_oneof![
            Just(f64::NAN),
            Just(-0.0),
            Just(0.0),
            Just(1.5),
            Just(-f64::NAN)
        ]
        .prop_map(Tree::Float),
        prop_oneof![Just(""), Just("a"), Just("b{")].prop_map(|s| Tree::Text(s.to_owned())),
        Just(Tree::None),
    ];
    leaf.prop_recursive(4, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(Tree::Tuple),
            prop::collection::vec(inner.clone(), 0..4).prop_map(Tree::List),
            inner.clone().prop_map(|t| Tree::Some(Box::new(t))),
            ((-2_i64..2), inner).prop_map(|(x, t)| Tree::Point(x, Box::new(t))),
        ]
    })
}

/// The model's equality (§3.13, §3.3.2).
fn model_eq(left: &Tree, right: &Tree) -> bool {
    model_with(left, right, &|x, y| x == y || (x.is_nan() && y.is_nan()))
}

/// Whether two trees print the same: equal, with floats that also have the
/// same sign, since `-0.0 == 0.0` but they print differently.
fn model_same_text(left: &Tree, right: &Tree) -> bool {
    model_with(left, right, &|x, y| {
        x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan())
    })
}

/// Structural comparison of two trees, with `floats` comparing floats.
fn model_with(left: &Tree, right: &Tree, floats: &dyn Fn(f64, f64) -> bool) -> bool {
    let model_eq = |x: &Tree, y: &Tree| model_with(x, y, floats);
    let all = |xs: &[Tree], ys: &[Tree]| {
        xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| model_eq(x, y))
    };
    match (left, right) {
        (Tree::Int(x), Tree::Int(y)) => x == y,
        (Tree::Float(x), Tree::Float(y)) => floats(*x, *y),
        (Tree::Text(x), Tree::Text(y)) => x == y,
        (Tree::Tuple(xs), Tree::Tuple(ys)) | (Tree::List(xs), Tree::List(ys)) => all(xs, ys),
        (Tree::Some(x), Tree::Some(y)) => model_eq(x, y),
        (Tree::None, Tree::None) => true,
        (Tree::Point(x, label), Tree::Point(y, other)) => x == y && model_eq(label, other),
        _ => false,
    }
}

/// Builds `tree`, with lists shaped by `shape` (0 pushed, 1 prepended, 2 a
/// view), so equal trees become objects of different shapes.
fn build(tree: &Tree, shape: &Shape, how: usize) -> Word {
    match tree {
        Tree::Int(n) => int_word(*n),
        Tree::Float(x) => f64_word(*x),
        Tree::Text(s) => string(s),
        Tree::Tuple(items) => {
            let words: Vec<Word> = items.iter().map(|t| build(t, shape, how)).collect();
            tuple(&words)
        }
        Tree::List(items) => {
            let words = items.iter().map(|t| build(t, shape, how)).collect();
            match how % 3 {
                0 => list(words),
                1 => prepended(words),
                _ => viewed(words),
            }
        }
        Tree::Some(inner) => some(build(inner, shape, how)),
        Tree::None => none(),
        Tree::Point(x, label) => shape.point(*x, build(label, shape, how)),
    }
}

/// Builds `a` and `b` in different shapes and checks `==` against the
/// model, and that equal values hash and print the same.
fn check_pair(shape: &Shape, a: &Tree, b: &Tree, how: usize) -> Result<(), TestCaseError> {
    let (x, y) = (build(a, shape, how), build(b, shape, how.wrapping_add(1)));
    let same = eq(x, y);
    prop_assert_eq!(same, Ok(model_eq(a, b)));
    if same == Ok(true) {
        prop_assert_eq!(hashed(x), hashed(y));
    }
    if model_same_text(a, b) {
        prop_assert_eq!(text(x), text(y));
    }
    release([x, y]);
    Ok(())
}

proptest! {
    // 4 cases under Miri, as the Miri job's `PROPTEST_CASES` (D-721).
    #![proptest_config(ProptestConfig::with_cases(if cfg!(miri) { 4 } else { 256 }))]

    #[test]
    fn equal_values_hash_equally(a in tree(), b in tree(), how in 0_usize..3) {
        let shape = Shape::register();
        let domain = Domain::new();
        alloc::with_domain(Arc::clone(&domain), || -> Result<(), TestCaseError> {
            check_pair(&shape, &a, &a, how)?;
            check_pair(&shape, &a, &b, how)?;
            Ok(())
        })?;
        prop_assert_eq!(domain.live_objects(), 0);
    }
}
