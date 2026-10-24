//! Tests of the program image and its installation (D-919, D-920, D-867):
//! synthetic images built here, installed into the process's tables.

use crate::ExecCtx;
use crate::abi::{self, layout as ids};
use crate::entry::Callbacks;
use crate::image::{
    CallSiteEntry, CtorEntry, ExternalEntry, FIELD_KINDS, FN_CHEBY_ABI, FieldEntry, FunctionEntry,
    HookEntry, ImageError, ImageParts, ModuleParts, ModuleRecord, NO_STRING, ProgramImage, Table,
    TypeEntry, check_version, install,
};
use crate::layout::{self, FieldKind};
use crate::locate::{self, Location};

unsafe extern "C" fn returns_zero(_: *mut ExecCtx) -> u64 {
    0
}

/// `items` as a `'static` table.
fn table<T: 'static>(items: Vec<T>) -> Table<T> {
    Table::from_static(Box::leak(items.into_boxed_slice()))
}

/// A string table holding `texts`, and each text's offset.
fn strings(texts: &[&str]) -> (Table<u8>, Vec<u32>) {
    let mut bytes = Vec::new();
    let mut offsets = Vec::new();
    for text in texts {
        offsets.push(u32::try_from(bytes.len()).unwrap());
        bytes.extend_from_slice(&u32::try_from(text.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(text.as_bytes());
    }
    (table(bytes), offsets)
}

/// A record with only `strings` and `functions`; `with` changes the rest.
fn record(
    strings: Table<u8>,
    functions: Vec<FunctionEntry>,
    with: impl FnOnce(&mut ModuleParts),
) -> &'static ModuleRecord {
    let mut parts = ModuleParts {
        strings,
        types: Table::empty(),
        externals: Table::empty(),
        hooks: Table::empty(),
        free_shapes: Table::empty(),
        functions: table(functions),
        constants: Table::empty(),
    };
    with(&mut parts);
    // SAFETY: the tables are leaked slices, and the tests put no pointer
    // that installation dereferences or keeps into the entries (the
    // functions' code is never run, only located).
    Box::leak(Box::new(unsafe { ModuleRecord::new(parts) }))
}

/// An image of `records` whose `main` returns 0.
fn image(records: &[&'static ModuleRecord]) -> ProgramImage {
    let pointers: Vec<*const ModuleRecord> = records
        .iter()
        .map(|record| std::ptr::from_ref(*record))
        .collect();
    // SAFETY: every pointer is a leaked record made by `record`, and the
    // entry is a thunk with the `EntryFn` ABI.
    unsafe {
        ProgramImage::new(ImageParts {
            version: abi::IMAGE_VERSION,
            unwind_edges: true,
            modules: table(pointers),
            entry: returns_zero,
            callbacks: Callbacks::default(),
            main_stack: 64 << 20,
            fiber_stack: 8 << 20,
            constants: Table::empty(),
        })
    }
}

/// A fresh block of "code": its address is unique in the process, so its
/// ranges never collide with another test's.
fn code(len: usize) -> &'static [u8] {
    Box::leak(vec![0_u8; len].into_boxed_slice())
}

fn site(ret_offset: u32, path: u32, line: u32, column: u32) -> CallSiteEntry {
    CallSiteEntry {
        ret_offset,
        path,
        line,
        column,
    }
}

fn function(code: &'static [u8], sites: Vec<CallSiteEntry>) -> FunctionEntry {
    FunctionEntry {
        code: code.as_ptr(),
        len: u32::try_from(code.len()).unwrap(),
        flags: FN_CHEBY_ABI,
        sites: table(sites),
    }
}

/// A frame record: the caller's frame pointer, then the return address.
#[repr(C, align(16))]
struct Frame([usize; 2]);

/// Where the process's code map locates the return address `ra` of a
/// frame whose caller has no record.
fn locate_return(ra: usize) -> Option<Location> {
    let frame = Frame([0, ra]);
    let at = std::ptr::from_ref(&frame).expose_provenance();
    locate::locate(at, at, at.checked_add(size_of::<Frame>()).unwrap())
}

fn location(path: &str, line: u32, column: u32) -> Location {
    Location {
        path: path.into(),
        line,
        column,
    }
}

/// One record with two call sites and empty tables: both sites are
/// located through the process's map, an address between them is not.
#[test]
fn image_installs_call_sites_and_locates_return_addresses() {
    let (strings, offsets) = strings(&["src/other.cheby", "src/main.cheby"]);
    let main = offsets[1];
    let body = code(0x100);
    let record = record(
        strings,
        vec![function(
            body,
            vec![site(0x40, main, 7, 3), site(0x20, main, 5, 22)],
        )],
        |_| {},
    );
    let image = image(&[record]);
    assert_eq!(install(&image), Ok(()));
    let start = body.as_ptr().addr();
    assert_eq!(
        locate_return(start + 0x20),
        Some(location("src/main.cheby", 5, 22))
    );
    assert_eq!(
        locate_return(start + 0x40),
        Some(location("src/main.cheby", 7, 3))
    );
    // No site at 0x30, and the caller has no record.
    assert_eq!(locate_return(start + 0x30), None);
    // The image's other parts.
    assert_eq!(check_version(&image), Ok(()));
    assert!(image.unwind_edges());
    assert_eq!(
        (image.main_stack(), image.fiber_stack()),
        (64 << 20, 8 << 20)
    );
}

/// Installing registers the builtin list and map kinds, as `symbols()`
/// does for the JIT, even for an image with no module.
#[test]
fn image_without_modules_registers_the_builtin_kinds() {
    assert_eq!(install(&image(&[])), Ok(()));
    for id in [ids::LIST_FIRST, ids::MAP_FIRST] {
        assert!(layout::get(id).is_some(), "layout {id} is not registered");
    }
}

/// A C-ABI function ends the walk; a site without a path is known to be a
/// site but has no printable location.
#[test]
fn image_keeps_the_abi_flag_and_sites_without_a_path() {
    let (strings, offsets) = strings(&["src/main.cheby"]);
    let thunk = code(0x40);
    let user = code(0x40);
    let mut c_abi = function(thunk, vec![site(0x10, offsets[0], 1, 1)]);
    c_abi.flags = 0;
    let record = record(
        strings,
        vec![c_abi, function(user, vec![site(0x10, NO_STRING, 0, 0)])],
        |_| {},
    );
    assert_eq!(install(&image(&[record])), Ok(()));
    assert_eq!(locate_return(thunk.as_ptr().addr() + 0x10), None);
    assert_eq!(locate_return(user.as_ptr().addr() + 0x10), None);
}

/// The table a `NotYet` names, and how a record gets a non-empty one.
type Case = (&'static str, fn(&mut ModuleParts));

/// Each table a later package fills (external types and free shapes,
/// N5b; constants, K5a) is refused with `NotYet` while it is not empty,
/// before any call site of the image is registered.
#[test]
fn image_refuses_tables_it_cannot_install_yet() {
    let (path_strings, offsets) = strings(&["src/main.cheby"]);
    let path = offsets[0];
    let cases: [Case; 3] = [
        ("external types", |parts| {
            parts.externals = table(vec![ExternalEntry {
                type_name: NO_STRING,
                pad: 0,
                type_key: 2,
                drop: std::ptr::null(),
            }]);
        }),
        ("free shapes", |parts| parts.free_shapes = table(vec![0])),
        ("constant slots", |parts| {
            parts.constants = table(vec![std::ptr::null_mut()]);
        }),
    ];
    for (what, with) in cases {
        let body = code(0x40);
        let record = record(
            path_strings,
            vec![function(body, vec![site(0x10, path, 3, 4)])],
            with,
        );
        assert_eq!(
            install(&image(&[record])),
            Err(ImageError::NotYet(what)),
            "{what}"
        );
        assert_eq!(locate_return(body.as_ptr().addr() + 0x10), None, "{what}");
    }
    // The image's constant image.
    let mut constants = image(&[]);
    // SAFETY: an image of no module; its constant table is a leaked slice.
    constants = unsafe {
        ProgramImage::new(ImageParts {
            version: abi::IMAGE_VERSION,
            unwind_edges: false,
            modules: Table::empty(),
            entry: constants.entry(),
            callbacks: constants.callbacks(),
            main_stack: constants.main_stack(),
            fiber_stack: constants.fiber_stack(),
            constants: table(vec![1, 2, 3]),
        })
    };
    assert!(!constants.unwind_edges());
    assert_eq!(
        install(&constants),
        Err(ImageError::NotYet("a constant image"))
    );
}

/// Malformed tables, strings and references are errors, never a crash.
#[test]
fn image_rejects_malformed_records() {
    let (path_strings, offsets) = strings(&["src/main.cheby"]);
    let malformed = |module, what| Err(ImageError::Malformed { module, what });

    // A null module record.
    // SAFETY: the table holds one null pointer, which `install` refuses.
    let null = unsafe {
        ProgramImage::new(ImageParts {
            version: abi::IMAGE_VERSION,
            unwind_edges: true,
            modules: table(vec![std::ptr::null()]),
            entry: returns_zero,
            callbacks: Callbacks::default(),
            main_stack: 64 << 20,
            fiber_stack: 8 << 20,
            constants: Table::empty(),
        })
    };
    assert_eq!(install(&null), malformed(Some(0), "a null module record"));

    // A path past the end of the string table.
    let bad_path = record(
        path_strings,
        vec![function(
            code(0x40),
            vec![site(0x10, offsets[0] + 100, 1, 1)],
        )],
        |_| {},
    );
    let good = record(path_strings, Vec::new(), |_| {});
    assert_eq!(
        install(&image(&[good, bad_path])),
        malformed(Some(1), "a call site's path")
    );

    // A path that is not UTF-8.
    let mut bytes = 2_u32.to_le_bytes().to_vec();
    bytes.extend_from_slice(&[0xff, 0xfe]);
    let not_utf8 = record(
        table(bytes),
        vec![function(code(0x40), vec![site(0x10, 0, 1, 1)])],
        |_| {},
    );
    assert_eq!(
        install(&image(&[not_utf8])),
        malformed(Some(0), "a call site's path")
    );

    // Unknown function flags, and a function without code.
    let mut flagged = function(code(0x40), Vec::new());
    flagged.flags = 2;
    let flags = record(path_strings, vec![flagged], |_| {});
    assert_eq!(
        install(&image(&[flags])),
        malformed(Some(0), "unknown function flags")
    );
    let mut codeless = function(code(0x40), Vec::new());
    codeless.code = std::ptr::null();
    let codeless = record(path_strings, vec![codeless], |_| {});
    assert_eq!(
        install(&image(&[codeless])),
        malformed(Some(0), "a function without code")
    );

    // A hook must name a type of its own record, and there is none.
    let hooked = record(path_strings, Vec::new(), |parts| {
        parts.hooks = table(vec![HookEntry {
            type_index: 0,
            pad: 0,
            equal: std::ptr::null(),
            hash: std::ptr::null(),
            debug: std::ptr::null(),
        }]);
    });
    assert_eq!(
        install(&image(&[hooked])),
        malformed(Some(0), "a hook names a type the record does not list")
    );
}

/// The version is checked on its own (`cheby_rt_main` does it in debug
/// builds, D-925), and the errors read as sentences.
#[test]
fn image_version_and_error_messages() {
    // SAFETY: an image of no module.
    let old = unsafe {
        ProgramImage::new(ImageParts {
            version: abi::IMAGE_VERSION + 1,
            unwind_edges: true,
            modules: Table::empty(),
            entry: returns_zero,
            callbacks: Callbacks::default(),
            main_stack: 64 << 20,
            fiber_stack: 8 << 20,
            constants: Table::empty(),
        })
    };
    let error = check_version(&old).unwrap_err();
    assert_eq!(
        error,
        ImageError::Version {
            found: abi::IMAGE_VERSION + 1,
            expected: abi::IMAGE_VERSION
        }
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the program image has version {}, but this runtime reads version {}",
            abi::IMAGE_VERSION + 1,
            abi::IMAGE_VERSION
        )
    );
    assert_eq!(
        ImageError::NotYet("free shapes").to_string(),
        "the program image holds free shapes, which this runtime cannot install yet"
    );
    assert_eq!(
        ImageError::Malformed {
            module: Some(2),
            what: "the string table"
        }
        .to_string(),
        "the program image is malformed: the string table in module record 2"
    );
    assert_eq!(Table::<u8>::empty().len(), 0);
    assert!(Table::<u8>::empty().is_empty());
}

// ---- User types (D-919, D-920) ----

/// What a layout word or constructor static holds until registration
/// writes it.
const SENTINEL: u64 = 0x5eed_5eed;

/// A fresh writable word holding [`SENTINEL`].
fn word() -> *mut u64 {
    Box::leak(Box::new(SENTINEL))
}

fn read(word: *mut u64) -> u64 {
    // SAFETY: every word of these tests is a leaked box made by `word`.
    unsafe { word.read() }
}

/// A fresh, distinct non-null address that stands for a static function
/// value: registration only stores it (no test calls a hook).
fn fn_value() -> *const u8 {
    std::ptr::from_mut(Box::leak(Box::new(0_u64)))
        .cast_const()
        .cast()
}

/// A constructor of a test type: its path, name, record flag and fields
/// (kind, name, offset) in declaration order.
struct TestCtor {
    name: &'static str,
    ctor_name: &'static str,
    record: bool,
    fields: Vec<(FieldKind, Option<&'static str>, u32)>,
}

/// A test type: its name, its key and its constructors in tag order.
struct TestType {
    name: &'static str,
    key: u64,
    ctors: Vec<TestCtor>,
}

/// The words of a test record: per type, per constructor, the layout word
/// and the constructor static (null for a constructor with fields).
type Words = Vec<Vec<(*mut u64, *mut u64)>>;

/// A changed entry of a test record: (type, constructor, the entry, its
/// fields).
type Tweak<'a> = &'a dyn Fn(usize, usize, &mut CtorEntry, &mut Vec<FieldEntry>);

/// A module record listing `types` and `hooks` (type index, three
/// values), with fresh words, after `tweak` changed each constructor's
/// entry; and the words.
fn types_record(
    types: &[TestType],
    hooks: Vec<(u32, [*const u8; 3])>,
    tweak: Tweak<'_>,
) -> (&'static ModuleRecord, Words) {
    let mut texts: Vec<&str> = Vec::new();
    for ty in types {
        texts.push(ty.name);
        for ctor in &ty.ctors {
            texts.extend([ctor.name, ctor.ctor_name]);
            texts.extend(ctor.fields.iter().filter_map(|(_, name, _)| *name));
        }
    }
    let (strings, offsets) = strings(&texts);
    let at = |text: &str| offsets[texts.iter().position(|t| *t == text).unwrap()];
    let mut words: Words = Vec::new();
    let mut entries = Vec::new();
    for (t, ty) in types.iter().enumerate() {
        let mut type_words = Vec::new();
        let mut ctors = Vec::new();
        for (c, ctor) in ty.ctors.iter().enumerate() {
            let mut fields: Vec<FieldEntry> = ctor
                .fields
                .iter()
                .map(|&(kind, name, offset)| FieldEntry {
                    kind: u32::try_from(FIELD_KINDS.iter().position(|&k| k == kind).unwrap())
                        .unwrap(),
                    name: name.map_or(NO_STRING, at),
                    offset,
                    pad: 0,
                })
                .collect();
            let layout_word = word();
            let ctor_static = if ctor.fields.is_empty() {
                word()
            } else {
                std::ptr::null_mut()
            };
            let mut entry = CtorEntry {
                name: at(ctor.name),
                ctor_name: at(ctor.ctor_name),
                record: u32::from(ctor.record),
                pad: 0,
                fields: Table::empty(),
                layout_word,
                ctor_static,
            };
            tweak(t, c, &mut entry, &mut fields);
            entry.fields = table(fields);
            type_words.push((entry.layout_word, entry.ctor_static));
            ctors.push(entry);
        }
        words.push(type_words);
        entries.push(TypeEntry {
            name: at(ty.name),
            pad: 0,
            type_key: ty.key,
            ctors: table(ctors),
        });
    }
    let hooks = hooks
        .into_iter()
        .map(|(type_index, [equal, hash, debug])| HookEntry {
            type_index,
            pad: 0,
            equal,
            hash,
            debug,
        })
        .collect();
    let record = record(strings, Vec::new(), |parts| {
        parts.types = table(entries);
        parts.hooks = table(hooks);
    });
    (record, words)
}

/// `Shape`, with a record constructor whose raw field follows its value
/// field, a positional one whose value field moves before its raw field,
/// and a nullary one; then `Pair`, positional with two value fields.
fn shape_and_pair() -> Vec<TestType> {
    vec![
        TestType {
            name: "Shape",
            key: layout::type_key("app::shapes", "Shape"),
            ctors: vec![
                TestCtor {
                    name: "app::shapes::Circle",
                    ctor_name: "Circle",
                    record: true,
                    fields: vec![
                        (FieldKind::Value, Some("center"), 8),
                        (FieldKind::Float, Some("radius"), 16),
                    ],
                },
                TestCtor {
                    name: "app::shapes::Square",
                    ctor_name: "Square",
                    record: false,
                    fields: vec![(FieldKind::Int, None, 16), (FieldKind::Value, None, 8)],
                },
                TestCtor {
                    name: "app::shapes::Empty",
                    ctor_name: "Empty",
                    record: false,
                    fields: Vec::new(),
                },
            ],
        },
        TestType {
            name: "Pair",
            key: layout::type_key("app::shapes", "Pair"),
            ctors: vec![TestCtor {
                name: "app::shapes::Pair",
                ctor_name: "Pair",
                record: false,
                fields: vec![(FieldKind::Value, None, 8), (FieldKind::Value, None, 16)],
            }],
        },
    ]
}

/// Installing a record with two types registers each type's constructors
/// with consecutive ids in tag order (D-522), with the facts the JIT's
/// `layout_info` gives (names, type key, record flag, field kinds, names
/// and offsets); writes each layout word's header (count 1, the value
/// fields as scan count, the id) and the nullary constructor's static
/// header (count 0, scan 0, the id); and sets the hooked type's hooks on
/// every one of its constructors, and none on the other type (D-585).
#[test]
fn image_install_registers_types_ids_words_facts_and_hooks() {
    let types = shape_and_pair();
    let values = [fn_value(), fn_value(), fn_value()];
    let (record, words) = types_record(&types, vec![(0, values)], &|_, _, _, _| {});
    assert_eq!(install(&image(&[record])), Ok(()));
    let hook_words = values.map(|v| u64::try_from(v.expose_provenance()).unwrap());
    for ((t, ty), type_words) in types.iter().enumerate().zip(&words) {
        let first = abi::header_layout(read(type_words[0].0));
        for ((tag, ctor), &(layout_word, ctor_static)) in
            ty.ctors.iter().enumerate().zip(type_words)
        {
            let header = read(layout_word);
            let id = abi::header_layout(header);
            assert_eq!(id, first + u32::try_from(tag).unwrap(), "{}", ctor.name);
            assert!(id >= ids::FIRST_USER, "{}", ctor.name);
            assert_eq!(abi::header_rc(header), abi::RC_UNIQUE, "{}", ctor.name);
            let values = ctor
                .fields
                .iter()
                .filter(|(kind, _, _)| *kind == FieldKind::Value)
                .count();
            assert_eq!(
                usize::from(abi::header_scan(header)),
                values,
                "{}",
                ctor.name
            );
            let info = layout::get(id).unwrap();
            let fields: Vec<(FieldKind, Option<&str>)> = ctor
                .fields
                .iter()
                .map(|&(kind, name, _)| (kind, name))
                .collect();
            let expected = layout::LayoutInfo::ctor(
                ctor.name,
                u32::try_from(tag).unwrap(),
                &fields,
            )
            .named(ty.name, ctor.ctor_name, ctor.record, ty.key);
            assert_eq!(&*info.name, ctor.name);
            assert_eq!(&*info.type_name, ty.name);
            assert_eq!(&*info.ctor_name, ctor.ctor_name);
            assert_eq!(info.record, ctor.record);
            assert_eq!(info.type_key, ty.key);
            assert_eq!(info.tag, expected.tag);
            assert_eq!(info.kind, expected.kind);
            assert_eq!(
                (info.value_fields, info.raw_fields),
                (expected.value_fields, expected.raw_fields)
            );
            assert_eq!(info.fields, expected.fields);
            let offsets: Vec<u32> = info.fields.iter().map(|f| f.offset).collect();
            let given: Vec<u32> = ctor.fields.iter().map(|&(_, _, o)| o).collect();
            assert_eq!(offsets, given, "{}", ctor.name);
            if ctor.fields.is_empty() {
                let header = read(ctor_static);
                assert_eq!(header, abi::header(abi::RC_STATIC, 0, id), "{}", ctor.name);
            } else {
                assert!(ctor_static.is_null());
            }
            let hooks = if t == 0 { Some(hook_words) } else { None };
            assert_eq!(info.hooks(), hooks, "{}", ctor.name);
        }
    }
}

/// A constructor with 255 value fields gets the escape value of D-279 as
/// its layout word's scan count, as code generation's `ctor_header`
/// writes it in the JIT; one with 254 keeps its count (D-555).
#[test]
fn image_install_registers_types_escapes_the_scan_count() {
    let fields = |n: u32| {
        (0..n)
            .map(|k| (FieldKind::Value, None, 8 + 8 * k))
            .collect::<Vec<_>>()
    };
    let types = vec![TestType {
        name: "Wide",
        key: layout::type_key("app", "Wide"),
        ctors: vec![
            TestCtor {
                name: "app::Wide",
                ctor_name: "Wide",
                record: false,
                fields: fields(255),
            },
            TestCtor {
                name: "app::Narrow",
                ctor_name: "Narrow",
                record: false,
                fields: fields(254),
            },
        ],
    }];
    let (record, words) = types_record(&types, Vec::new(), &|_, _, _, _| {});
    assert_eq!(install(&image(&[record])), Ok(()));
    let wide = read(words[0][0].0);
    let narrow = read(words[0][1].0);
    assert_eq!(abi::header_scan(wide), abi::SCAN_ESCAPE);
    assert_eq!(abi::header_scan(narrow), abi::SCAN_MAX);
    assert_eq!(
        layout::get(abi::header_layout(wide)).unwrap().value_fields,
        255
    );
    assert_eq!(abi::header_rc(wide), abi::RC_UNIQUE);
}

/// A malformed record is refused before anything is registered: the good
/// record before it keeps its words untouched, and so does the bad one.
#[test]
#[expect(clippy::too_many_lines, reason = "one table of malformed records")]
fn image_install_registers_types_refuses_malformed_with_nothing_registered() {
    type Case = (&'static str, Tweak<'static>, bool);
    let null_hook = [fn_value(), fn_value(), std::ptr::null()];
    let cases: [Case; 8] = [
        (
            "a field kind",
            &|t, c, _, fields| {
                if (t, c) == (0, 0) {
                    fields[1].kind = 13;
                }
            },
            false,
        ),
        ("a null hook", &|_, _, _, _| {}, true),
        (
            "a constructor static that is not there exactly for a constructor without fields",
            &|t, c, entry, _| {
                if (t, c) == (0, 2) {
                    entry.ctor_static = std::ptr::null_mut();
                }
            },
            false,
        ),
        (
            "a constructor static that is not there exactly for a constructor without fields",
            &|t, c, entry, _| {
                if (t, c) == (1, 0) {
                    entry.ctor_static = word();
                }
            },
            false,
        ),
        (
            "a constructor without a layout word",
            &|t, c, entry, _| {
                if (t, c) == (1, 0) {
                    entry.layout_word = std::ptr::null_mut();
                }
            },
            false,
        ),
        (
            "a field offset that is not the runtime's own (D-454)",
            &|t, c, _, fields| {
                if (t, c) == (0, 1) {
                    fields[0].offset = 8;
                    fields[1].offset = 16;
                }
            },
            false,
        ),
        (
            "a constructor's record flag",
            &|t, c, entry, _| {
                if (t, c) == (0, 0) {
                    entry.record = 2;
                }
            },
            false,
        ),
        (
            "a constructor's name",
            &|t, c, entry, _| {
                if (t, c) == (1, 0) {
                    entry.name = 1_000_000;
                }
            },
            false,
        ),
    ];
    for (what, tweak, with_null_hook) in cases {
        let (good, good_words) = types_record(&shape_and_pair(), Vec::new(), &|_, _, _, _| {});
        let hooks = if with_null_hook {
            vec![(0, null_hook)]
        } else {
            Vec::new()
        };
        let (bad, bad_words) = types_record(&shape_and_pair(), hooks, tweak);
        assert_eq!(
            install(&image(&[good, bad])),
            Err(ImageError::Malformed {
                module: Some(1),
                what
            }),
            "{what}"
        );
        for &(layout_word, ctor_static) in good_words.iter().chain(&bad_words).flatten() {
            if !layout_word.is_null() {
                assert_eq!(read(layout_word), SENTINEL, "{what}");
            }
            if !ctor_static.is_null() {
                assert_eq!(read(ctor_static), SENTINEL, "{what}");
            }
        }
    }
    // A hook that names no listed type, and two hooks for one type.
    let values = [fn_value(), fn_value(), fn_value()];
    for (hooks, what) in [
        (
            vec![(2, values)],
            "a hook names a type the record does not list",
        ),
        (
            vec![(1, values), (1, values)],
            "two hook entries for one type",
        ),
    ] {
        let (bad, words) = types_record(&shape_and_pair(), hooks, &|_, _, _, _| {});
        assert_eq!(
            install(&image(&[bad])),
            Err(ImageError::Malformed {
                module: Some(0),
                what
            }),
            "{what}"
        );
        assert_eq!(read(words[0][0].0), SENTINEL, "{what}");
    }
}
