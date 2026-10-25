//! Tests of constant images (D-979): round trips of every kind of value,
//! sharing across images (D-539), the same bytes in two processes
//! (D-264), and installation's checks and seeds (D-196).

use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use super::*;
use crate::layout::type_key;
use crate::object::StaticObject;
use crate::rc;

/// A user type `app::Shape` with a constructor of fields and a nullary
/// one, registered once per process: `Shape.Pair { a: value, n: Int }`
/// (tag 0) and `Shape.Empty` (tag 1), with a static for `Empty`.
struct Shape {
    pair: LayoutId,
    empty: LayoutId,
    empty_static: Word,
}

fn shape() -> &'static Shape {
    static SHAPE: OnceLock<Shape> = OnceLock::new();
    SHAPE.get_or_init(|| {
        let key = type_key("app", "Shape");
        let pair = LayoutInfo::ctor(
            "app::Pair",
            0,
            &[(FieldKind::Value, Some("a")), (FieldKind::Int, Some("n"))],
        )
        .named("Shape", "Pair", true, key);
        let empty = LayoutInfo::ctor("app::Empty", 1, &[]).named("Shape", "Empty", false, key);
        let first = layout::register_consecutive(vec![pair, empty]).unwrap();
        let empty = first.checked_add(1).unwrap();
        let empty_static: &'static StaticObject = Box::leak(Box::new(StaticObject::new(empty)));
        Shape {
            pair: first,
            empty,
            empty_static: empty_static.word(),
        }
    })
}

/// The registered layouts of `app::Shape`.
fn types() -> TypeLayouts {
    let s = shape();
    let mut types = TypeLayouts::new();
    types.insert(
        type_key("app", "Shape"),
        vec![(s.pair, 0), (s.empty, s.empty_static)],
    );
    types
}

/// `Shape.Pair { a, n }`, owning `a`.
fn pair(a: Word, n: i64) -> Word {
    let info = layout::get(shape().pair).unwrap();
    let obj = alloc::alloc_object(
        info.fixed_size().unwrap(),
        abi::header(abi::RC_UNIQUE, info.scan_count(), shape().pair),
    );
    // SAFETY: a fresh object of the layout, fields at their offsets.
    unsafe {
        write_field(obj, 8, a);
        write_field(obj, 16, n.cast_unsigned());
    }
    object::ptr_to_word(obj)
}

fn map_of(entries: &[(Word, Word)], seed: u64) -> Word {
    let mut map = hamt::cheby_map_new();
    for &(k, v) in entries {
        // SAFETY: live words; the kernel takes what it keeps.
        let next = unsafe { hamt::cheby_map_insert(map, k, v) };
        // SAFETY: owned words given up.
        unsafe {
            rc::drop(map);
            rc::drop(k);
            rc::drop(v);
        }
        map = next;
    }
    // SAFETY: an owned map, given up.
    unsafe { hamt::rebuild_with_seed(map, seed) }
}

fn set_of(items: &[Word], seed: u64) -> Word {
    let mut set = hamt::cheby_set_new();
    for &item in items {
        // SAFETY: a live set and item; the kernel takes what it keeps.
        let next = unsafe { hamt::cheby_set_insert(set, item) };
        // SAFETY: owned words given up.
        unsafe {
            rc::drop(set);
            rc::drop(item);
        }
        set = next;
    }
    // SAFETY: an owned set, given up.
    unsafe { hamt::rebuild_with_seed(set, seed) }
}

/// A value holding every kind an image describes, nested.
fn every_kind() -> Word {
    let strings = list::from_iter([values::string("é"), values::string("")]);
    let boxed = values::tuple(&[
        values::int_word(i64::MAX),
        values::f64_word(-0.5),
        values::f32_word(1.25),
        values::u64_word(u64::MAX),
    ]);
    let statics = list::from_iter([
        values::nil(),
        values::bool_word(true),
        values::bool_word(false),
        values::none(),
        values::order(std::cmp::Ordering::Less),
        values::order(std::cmp::Ordering::Equal),
        values::order(std::cmp::Ordering::Greater),
        empty_list(),
        values::some_u8(200),
        values::some_i8(-3),
    ]);
    let results = values::tuple(&[
        values::ok(values::bytes(&[0, 255, 7])),
        values::err(values::string("no")),
        values::some(values::int_word(1 << 40)),
    ]);
    let user = list::from_iter([
        pair(values::string("inner"), -7),
        shape().empty_static,
        pair(pair(values::int_word(3), 4), i64::MIN),
    ]);
    let map = map_of(
        &[
            (values::string("one"), values::int_word(1)),
            (values::string("two"), values::some(values::int_word(2))),
        ],
        0xA,
    );
    let set = set_of(&[values::int_word(5), values::string("five")], 0xB);
    values::tuple(&[strings, boxed, statics, results, user, map, set])
}

/// The debug text of a value, for comparisons.
fn text(word: Word) -> String {
    // SAFETY: a live value without hooks.
    unsafe { generic::debug_string(word) }
}

/// Installs `images` into fresh slots, one per image, and returns them.
fn install_all(images: &[ConstImage], types: &TypeLayouts) -> (Vec<Word>, Box<[u64]>) {
    let mut slots: Box<[u64]> = vec![0; images.len()].into_boxed_slice();
    let base = slots.as_mut_ptr();
    let paths: Vec<Box<str>> = images.iter().map(|i| i.path.clone()).collect();
    let mut slot_of = |path: &str| {
        let index = paths.iter().position(|p| &**p == path)?;
        // SAFETY: an index within `slots`.
        Some(unsafe { base.add(index) })
    };
    // SAFETY: the slots are writable words nothing reads; the layouts are
    // registered for the process.
    unsafe { install(images, &mut slot_of, types) }.unwrap();
    (slots.to_vec(), slots)
}

/// D-979: every kind of value survives a round trip, nested: the installed
/// value prints as the evaluated one, is static, and leaves nothing in the
/// domain.
#[test]
fn const_image_round_trip_of_every_kind_nested() {
    let domain = alloc::Domain::new();
    alloc::with_domain(Arc::clone(&domain), || {
        let value = every_kind();
        // SAFETY: a live value of this thread.
        let images = unsafe { serialize(&[("app::every", value)]) }.unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(&*images[0].path, "app::every");
        assert!(images[0].bytes.starts_with(b"CHBV\x01"));
        let live = domain.live_objects();
        let (words, _slots) = install_all(&images, &types());
        assert_eq!(
            domain.live_objects(),
            live,
            "installed objects left the domain"
        );
        let installed = words[0];
        assert_ne!(installed, value);
        assert_eq!(text(installed), text(value));
        // SAFETY: live values.
        unsafe {
            assert!(is_static(installed));
            assert_eq!(generic::equal(installed, value), Ok(true));
            // An image of the installed value differs only in the maps'
            // and sets' order, now the process seed's (D-196).
            let again = serialize(&[("app::every", installed)]).unwrap();
            assert_eq!(again[0].bytes.len(), images[0].bytes.len());
            rc::drop(value);
        }
        assert_eq!(domain.live_objects(), 0);
    });
}

/// D-539: two constants that share a map share one collection after
/// installation; the later image refers to the earlier one's object.
#[test]
fn const_image_shared_map_stays_one_collection_after_install() {
    let domain = alloc::Domain::new();
    alloc::with_domain(Arc::clone(&domain), || {
        let map = map_of(&[(values::int_word(1), values::string("a"))], 3);
        // SAFETY: a live map, shared by the tuple.
        unsafe { rc::dup(map) };
        let holder = values::tuple(&[map, values::int_word(9)]);
        // SAFETY: live values.
        let images = unsafe { serialize(&[("app::table", map), ("app::holder", holder)]) }.unwrap();
        // The holder's image refers to the table's object by path.
        let reference = b"\x03\x0aapp::table";
        assert!(
            images[1]
                .bytes
                .windows(reference.len())
                .any(|w| w == reference)
        );
        let (words, _slots) = install_all(&images, &types());
        // SAFETY: installed values.
        let parts = unsafe { values::read_tuple(words[1]) };
        assert_eq!(parts[0], words[0]);
        // SAFETY: owned values given up.
        unsafe {
            rc::drop(map);
            rc::drop(holder);
        }
        assert_eq!(domain.live_objects(), 0);
    });
}

/// GAP(repl-constant-images), the wave-6 review's R-2: an in-process
/// image (the REPL's) names an earlier entry's installed constant by its
/// address, so a later constant that holds it costs a word, not a copy,
/// and the installed value shares it (D-539). `install` refuses such an
/// image before writing anything.
#[test]
fn const_image_in_process_shares_installed_statics() {
    let domain = alloc::Domain::new();
    alloc::with_domain(Arc::clone(&domain), || {
        // An earlier entry: a large constant, installed (static).
        let big = list::from_iter((0..10_000).map(|n| values::string(&format!("s{n}"))));
        // SAFETY: a live value of this thread.
        let first = unsafe { serialize(&[("app::big", big)]) }.unwrap();
        let (words, _first_slots) = install_all(&first, &types());
        let installed_big = words[0];
        // A later entry's value holds it, as an evaluation would after
        // reading the slot.
        let holder = values::tuple(&[installed_big, values::int_word(1)]);
        // SAFETY: live values; the installed list is static for the
        // process.
        let copied = unsafe { serialize(&[("app::holder", holder)]) }.unwrap();
        // SAFETY: as above.
        let shared = unsafe { serialize_in_process(&[("app::holder", holder)]) }.unwrap();
        assert!(copied[0].bytes.len() > 10_000, "{}", copied[0].bytes.len());
        assert!(shared[0].bytes.len() < 64, "{:?}", shared[0].bytes);
        let mut address = vec![tag::ADDRESS];
        address.extend_from_slice(&installed_big.to_le_bytes());
        assert!(
            shared[0]
                .bytes
                .windows(address.len())
                .any(|w| w == address.as_slice())
        );
        let mut slot = [0_u64; 1];
        let base = slot.as_mut_ptr();
        let mut slot_of = |path: &str| (path == "app::holder").then_some(base);
        // SAFETY: a writable slot; the image leaves no process.
        let refused = unsafe { install(&shared, &mut slot_of, &types()) };
        assert!(
            matches!(refused, Err(ConstImageError::Malformed { .. })),
            "{refused:?}"
        );
        assert_eq!(slot, [0]);
        let live = domain.live_objects();
        // SAFETY: as above; the address is the installed list, static.
        unsafe { install_in_process(&shared, &mut slot_of, &types()) }.unwrap();
        assert_eq!(domain.live_objects(), live);
        // SAFETY: the installed tuple.
        unsafe {
            assert!(is_static(slot[0]));
            assert_eq!(
                values::read_tuple(slot[0]),
                [installed_big, values::int_word(1)]
            );
            rc::drop(holder);
            rc::drop(big);
        }
        assert_eq!(domain.live_objects(), 0);
    });
}

/// The child half of [`const_image_bytes_are_the_same_in_two_processes`].
const CHILD_VAR: &str = "CHEBY_IMAGE_TEST_CHILD";

/// The images of a fixed value, in hex.
fn fixed_images() -> String {
    let domain = alloc::Domain::new();
    alloc::with_domain(domain, || {
        // Another layout first in this process only, so ids differ.
        if std::env::var_os(CHILD_VAR).is_some() {
            layout::register_consecutive(vec![LayoutInfo::ctor("other::Other", 0, &[]).named(
                "Other",
                "Other",
                false,
                type_key("other", "Other"),
            )])
            .unwrap();
        }
        let shared = values::string("shared");
        let map = map_of(
            &[
                (values::string("x"), pair(values::f64_word(0.25), 1)),
                (values::string("y"), shape().empty_static),
                (values::int_word(7), list::from_iter([values::int_word(1)])),
            ],
            0x5EED,
        );
        // SAFETY: live values shared below.
        unsafe { rc::dup(shared) };
        let first = values::tuple(&[shared, map]);
        let second = list::from_iter([shared, values::int_word(2)]);
        // SAFETY: live values.
        let images =
            unsafe { serialize(&[("app::first", first), ("app::second", second)]) }.unwrap();
        // SAFETY: owned values given up.
        unsafe {
            rc::drop(first);
            rc::drop(second);
        }
        let mut out = String::new();
        for image in &images {
            write!(out, "{} ", image.path).unwrap();
            for b in &image.bytes {
                write!(out, "{b:02x}").unwrap();
            }
            out.push('\n');
        }
        out
    })
}

/// D-264: the image bytes of the same evaluation are identical in two
/// processes, whose layout ids and addresses differ.
#[test]
fn const_image_bytes_are_the_same_in_two_processes() {
    let ours = fixed_images();
    if std::env::var_os(CHILD_VAR).is_some() {
        println!("IMAGES-BEGIN\n{ours}IMAGES-END");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let name = "consteval::image::tests::const_image_bytes_are_the_same_in_two_processes";
    for _ in 0..2 {
        let out = std::process::Command::new(&exe)
            .args([name, "--exact", "--nocapture", "--test-threads", "1"])
            .env(CHILD_VAR, "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let stdout = String::from_utf8(out.stdout).unwrap();
        let (_, after) = stdout.split_once("IMAGES-BEGIN\n").unwrap();
        let (theirs, _) = after.split_once("IMAGES-END").unwrap();
        assert_eq!(theirs, ours);
    }
}

/// The table of the program image holds every image in order.
#[test]
fn const_image_table_round_trip() {
    assert_eq!(encode_table(&[]), Vec::<u8>::new());
    assert_eq!(decode_table(&[]).unwrap(), Vec::new());
    let images = vec![
        ConstImage {
            path: "app::a".into(),
            // SAFETY: an immediate.
            bytes: unsafe { serialize(&[("app::a", values::int_word(1))]) }.unwrap()[0]
                .bytes
                .clone(),
        },
        // SAFETY: a static.
        unsafe { serialize(&[("app::b", values::nil())]) }.unwrap()[0].clone(),
    ];
    let table = encode_table(&images);
    assert!(table.starts_with(b"CHBT"));
    assert_eq!(decode_table(&table).unwrap(), images);
    let mut cut = table.clone();
    cut.pop();
    assert!(matches!(
        decode_table(&cut),
        Err(ConstImageError::Malformed { .. })
    ));
}

/// A malformed image, a missing slot or an unknown type key is refused
/// before anything is written: the slots of the images before it stay
/// empty.
#[test]
fn const_install_refuses_bad_images_before_writing() {
    let domain = alloc::Domain::new();
    alloc::with_domain(Arc::clone(&domain), || {
        let value = pair(values::string("x"), 1);
        let good = values::string("fine");
        // SAFETY: live values.
        let images = unsafe { serialize(&[("app::good", good), ("app::user", value)]) }.unwrap();
        let mut slots = [0_u64; 2];
        let base = slots.as_mut_ptr();
        let mut slot_of = |path: &str| {
            let index = ["app::good", "app::user"].iter().position(|p| *p == path)?;
            // SAFETY: an index within `slots`.
            Some(unsafe { base.add(index) })
        };
        // SAFETY: writable slots; no layout is named.
        let unknown = unsafe { install(&images, &mut slot_of, &TypeLayouts::new()) };
        assert!(
            matches!(unknown, Err(ConstImageError::UnknownTypeKey { ref path, ctor: 0, .. }) if &**path == "app::user"),
            "{unknown:?}"
        );
        let mut cut = images.clone();
        let len = cut[1].bytes.len();
        cut[1].bytes.truncate(len.saturating_sub(3));
        // SAFETY: as above.
        let malformed = unsafe { install(&cut, &mut slot_of, &types()) };
        assert!(matches!(malformed, Err(ConstImageError::Malformed { .. })));
        let mut no_slot = |_: &str| None;
        // SAFETY: no slot is written.
        let missing = unsafe { install(&images, &mut no_slot, &types()) };
        assert!(matches!(missing, Err(ConstImageError::NoSlot { .. })));
        assert_eq!(slots, [0, 0]);
        // SAFETY: owned values given up.
        unsafe {
            rc::drop(value);
            rc::drop(good);
        }
        assert_eq!(domain.live_objects(), 0);
    });
}

/// D-196: an installed map is built with the process seed, so it iterates
/// as the evaluated map rebuilt with that seed does (as at 64ffc31's
/// rebuild), whatever seed it was evaluated under.
#[test]
fn const_install_map_order_follows_the_process_seed() {
    let domain = alloc::Domain::new();
    alloc::with_domain(Arc::clone(&domain), || {
        let entries: Vec<(Word, Word)> = (0..64)
            .map(|n| (values::int_word(n), values::string(&format!("v{n}"))))
            .collect();
        let map = map_of(&entries, 0x1234);
        // SAFETY: live values.
        let images = unsafe { serialize(&[("app::table", map)]) }.unwrap();
        let (words, _slots) = install_all(&images, &types());
        let keys = |m: Word| {
            let mut keys = Vec::new();
            // SAFETY: a live map.
            unsafe { hamt::for_each_entry(m, &mut |k, _| keys.push(k)) };
            keys
        };
        // SAFETY: live maps.
        unsafe {
            assert_eq!(hamt::seed_of(words[0]), Some(generic::process_seed()));
            let rebuilt = hamt::rebuild_with_seed(map, generic::process_seed());
            assert_eq!(keys(words[0]), keys(rebuilt));
            rc::drop(rebuilt);
        }
        assert_eq!(domain.live_objects(), 0);
    });
}

/// Plan §0 (deep data): a million-deep value is written and installed on
/// a small stack.
#[test]
fn const_image_deep_values_need_no_deep_stack() {
    let worker = std::thread::Builder::new()
        .stack_size(256 << 10)
        .spawn(|| {
            let domain = alloc::Domain::new();
            alloc::with_domain(Arc::clone(&domain), || {
                let mut chain = values::string("bottom");
                for n in 0..1_000_000 {
                    chain = values::tuple(&[chain, values::int_word(n)]);
                }
                // SAFETY: a live value of this thread.
                let images = unsafe { serialize(&[("app::deep", chain)]) }.unwrap();
                let (words, _slots) = install_all(&images, &types());
                // SAFETY: live values.
                unsafe {
                    assert_eq!(values::read_tuple(words[0])[1], values::int_word(999_999));
                    rc::drop(chain);
                }
                assert_eq!(domain.live_objects(), 0);
            });
        })
        .unwrap();
    worker.join().unwrap();
}
