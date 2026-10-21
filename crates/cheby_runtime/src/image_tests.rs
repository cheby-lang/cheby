//! Tests of the program image and its installation (D-919, D-920, D-867):
//! synthetic images built here, installed into the process's tables.

use crate::ExecCtx;
use crate::abi::{self, layout as ids};
use crate::entry::Callbacks;
use crate::image::{
    CallSiteEntry, CtorEntry, ExternalEntry, FN_CHEBY_ABI, FunctionEntry, HookEntry, ImageError,
    ImageParts, ModuleParts, ModuleRecord, NO_STRING, ProgramImage, Table, TypeEntry,
    check_version, install,
};
use crate::layout;
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

/// Each table a later package fills is refused with `NotYet` while it is
/// not empty, before any call site of the image is registered.
#[test]
fn image_refuses_tables_it_cannot_install_yet() {
    let (path_strings, offsets) = strings(&["src/main.cheby"]);
    let path = offsets[0];
    let cases: [Case; 4] = [
        ("user layouts", |parts| {
            parts.types = table(vec![TypeEntry {
                name: NO_STRING,
                pad: 0,
                type_key: 1,
                ctors: table(vec![CtorEntry {
                    name: NO_STRING,
                    ctor_name: NO_STRING,
                    record: 0,
                    pad: 0,
                    fields: Table::empty(),
                    layout_word: std::ptr::null_mut(),
                    ctor_static: std::ptr::null_mut(),
                }]),
            }]);
        }),
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
