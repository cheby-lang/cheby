//! The program image `cb$image` and the module records `cb$mod$<path>`
//! (D-919, D-920), and [`install`], the one routine that registers them.
//!
//! An executable's C `main` passes `&cb$image` to `cheby_rt_main`
//! (D-925), which installs it before any worker starts. Code generation
//! writes the records as read-only data with `cranelift_module`'s
//! `DataDescription`: every pointer below is an 8-byte word that one
//! absolute data relocation fills, every count and offset a fixed-width
//! little-endian integer, and every struct is 8-byte aligned with no
//! implicit padding (the `pad` fields are 0). The layout is versioned by
//! [`abi::IMAGE_VERSION`]; any change of it raises the version.
//!
//! # Tables and strings
//!
//! A table is a (count, pointer) pair, [`Table`]: the count of entries as
//! a `u64` at offset 0 and the address of the first entry at offset 8. An
//! empty table has count 0 and any pointer, 0 included.
//!
//! A module record's strings live in its string table (`strings`, a table
//! of bytes). A string is named by its byte offset in that table, a `u32`;
//! at the offset, a `u32` holds the string's length in bytes, then its
//! UTF-8 bytes follow, with no alignment and no terminator.
//! [`NO_STRING`] (`u32::MAX`) names no string.
//!
//! # `cb$image`: [`ProgramImage`], 104 bytes
//!
//! | Offset | Field          | Type                         | Contents                                                                                     |
//! | ------ | -------------- | ---------------------------- | -------------------------------------------------------------------------------------------- |
//! | 0      | `version`      | `u32`                        | [`abi::IMAGE_VERSION`]                                                                       |
//! | 4      | `unwind_edges` | `u32`                        | 1 when the code was compiled with unwind edges, 0 under `CHEBY_UNWIND_EDGES=off` (D-742)     |
//! | 8      | `modules`      | table of `*const` records    | a pointer to each module record `cb$mod$<path>`, in build order (D-919)                      |
//! | 24     | `entry`        | function pointer             | the entry thunk of `main`, `cb$entry$main$<path>` ([`EntryFn`])                              |
//! | 32     | `tramp0`       | function pointer or 0        | `cb$tramp$0` (D-526), then `tramp1` at 40, `tramp2` at 48, `tramp3` at 56, `tramp4` at 64    |
//! | 72     | `main_stack`   | `u64`                        | the root fiber's stack in bytes: `[runtime] main-stack` after D-660's default and rounding    |
//! | 80     | `fiber_stack`  | `u64`                        | the default stack of other fibers in bytes: `[runtime] fiber-stack` likewise (D-660, D-927)  |
//! | 88     | `constants`    | table of bytes               | the constant image; empty until constants reach executables (its format is added then)       |
//!
//! # `cb$mod$<path>`: [`ModuleRecord`], 112 bytes
//!
//! | Offset | Field         | Table of                            | Contents                                                                    |
//! | ------ | ------------- | ----------------------------------- | --------------------------------------------------------------------------- |
//! | 0      | `strings`     | bytes                               | the record's string table                                                   |
//! | 16     | `types`       | [`TypeEntry`], 32 bytes each        | the module's user types, each with its constructors in tag order (D-522)    |
//! | 32     | `externals`   | [`ExternalEntry`], 24 bytes each    | the module's external types                                                 |
//! | 48     | `hooks`       | [`HookEntry`], 32 bytes each        | the `@std_hooks` of the module's types (D-482, D-585)                       |
//! | 64     | `free_shapes` | `u64` words                         | the free-shape word of each user layout, in the order of `types` (D-907)    |
//! | 80     | `functions`   | [`FunctionEntry`], 32 bytes each    | the call-site table (D-856, D-867)                                          |
//! | 96     | `constants`   | `*mut u64` slot pointers            | the module's constant slots `cb$const$…`                                     |
//!
//! [`TypeEntry`] (32 bytes): `name` (`u32` string, the type's name as debug
//! printing shows it) at 0, `pad` (`u32`) at 4, `type_key` (`u64`, D-559)
//! at 8, `ctors` (table of [`CtorEntry`], tag order) at 16.
//!
//! [`CtorEntry`] (48 bytes): `name` (`u32` string, the constructor's path
//! `module::Ctor`, MIR's `LayoutDesc::name`, as the JIT registers it) at 0,
//! `ctor_name` (`u32` string) at 4, `record` (`u32`, 1 when the fields are
//! named) at 8, `pad` (`u32`) at 12, `fields` (table of [`FieldEntry`],
//! declaration order) at 16, `layout_word` (pointer to the writable word
//! `cb$layout$…`) at 32, `ctor_static` (pointer to the writable static
//! `cb$ctor$…` of a nullary constructor, else 0) at 40. Registration
//! writes the header that allocation copies into the layout word and the
//! static header into the constructor static (D-454, D-920).
//!
//! [`FieldEntry`] (16 bytes): `kind` (`u32`, an index into
//! [`FIELD_KINDS`]) at 0, `name` (`u32` string or [`NO_STRING`]) at 4,
//! `offset` (`u32`, the field's byte offset in the object, which
//! registration checks against the runtime's own) at 8, `pad` (`u32`) at
//! 12.
//!
//! [`ExternalEntry`] (24 bytes): `type_name` (`u32` string) at 0, `pad`
//! (`u32`) at 4, `type_key` (`u64`) at 8, `drop` (pointer to the drop
//! function, or 0) at 16.
//!
//! [`HookEntry`] (32 bytes): `type_index` (`u32`, an index into the same
//! record's `types`) at 0, `pad` (`u32`) at 4, then the static function
//! values `cb$fnval$…` of the `equal`, `hash` and `debug` hooks at 8, 16
//! and 24.
//!
//! [`FunctionEntry`] (32 bytes): `code` (pointer to the function's first
//! byte) at 0, `len` (`u32`, its length in bytes) at 8, `flags` (`u32`:
//! [`FN_CHEBY_ABI`] when it has the Cheby ABI, no other bit) at 12,
//! `sites` (table of [`CallSiteEntry`], any order) at 16.
//!
//! [`CallSiteEntry`] (16 bytes, D-867's record): `ret_offset` (`u32`, the
//! return address's offset from the function's start) at 0, `path`
//! (`u32` string, or [`NO_STRING`] for a site without a printable
//! location) at 4, `line` (`u32`) at 8, `column` (`u32`) at 12.
//!
//! # Registering
//!
//! [`register_records`] is the one routine that registers module records
//! (D-919): an executable's start calls it through [`install`], and every
//! JIT host calls it after each batch with the records the batch defined,
//! so `cheby run`, `cheby test`, the REPL and an executable register the
//! same facts the same way. It checks every record (tables readable,
//! strings in range, field kinds known, field offsets equal to the
//! runtime's own, a layout word for every constructor, a constructor
//! static exactly for each constructor without fields, one free-shape word
//! per user layout equal to the one registration derives, every external
//! type named and without a drop function, every hook naming a listed type
//! once with three non-null values) before it registers anything. It then
//! registers, record by record in the order given, each type's
//! constructors with consecutive ids in tag order (D-522, D-1163), writing
//! into each layout word the header that allocation copies and into each
//! constructor static its static header (D-454, D-920), then the record's
//! external types in table order (`layout::register_external`,
//! GAP(record-externals-order)); then the hooks of every record on every
//! constructor of each hooked type (D-482, D-585), and the call-site
//! ranges under the caller's code owner (D-867).
//!
//! [`install`] registers the builtin list and map kinds and then the
//! image's records under one code owner that is never unregistered. The
//! constants (K5a) are not installed yet: a non-empty constant image or
//! constant slot table is refused with [`ImageError::NotYet`], before
//! anything of the image is registered.

use std::fmt;
use std::mem::{offset_of, size_of};
use std::ptr;

use crate::abi::{self, LayoutId, Word};
use crate::entry::{Callbacks, EntryFn, Tramp0, Tramp1, Tramp2, Tramp3, Tramp4};
use crate::layout::{self, FieldKind, LayoutInfo};
use crate::locate::{self, CallSiteDesc, CodeRangeDesc};

/// The string offset that names no string.
pub const NO_STRING: u32 = u32::MAX;

/// [`FunctionEntry::flags`]: the function has the Cheby ABI. A C-ABI
/// function (an entry thunk, a trampoline, a `@blocking` shim) ends a
/// deadlock walk (D-856).
pub const FN_CHEBY_ABI: u32 = 1;

/// The field kinds a [`FieldEntry::kind`] names, by index.
pub const FIELD_KINDS: [FieldKind; 13] = [
    FieldKind::Value,
    FieldKind::Int,
    FieldKind::Float,
    FieldKind::Bool,
    FieldKind::I8,
    FieldKind::I16,
    FieldKind::I32,
    FieldKind::I64,
    FieldKind::U8,
    FieldKind::U16,
    FieldKind::U32,
    FieldKind::U64,
    FieldKind::F32,
];

/// A (count, pointer) table of the image (module documentation).
///
/// A table made in Rust comes from a `'static` slice ([`Table::from_static`])
/// or is empty; one with another pointer is made only through the unsafe
/// [`Table::from_raw`].
#[repr(C)]
pub struct Table<T> {
    len: u64,
    ptr: *const T,
}

impl<T> Table<T> {
    /// The empty table.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            len: 0,
            ptr: ptr::null(),
        }
    }

    /// The table of `items`.
    #[must_use]
    pub fn from_static(items: &'static [T]) -> Self {
        Self {
            len: u64::try_from(items.len()).unwrap_or(0),
            ptr: items.as_ptr(),
        }
    }

    /// The table of `len` entries at `ptr`.
    ///
    /// # Safety
    ///
    /// When `len` is not 0, `ptr` points to `len` initialized entries
    /// that stay valid and unchanged for as long as the table or anything
    /// it is installed into is used.
    #[must_use]
    pub const unsafe fn from_raw(len: u64, ptr: *const T) -> Self {
        Self { len, ptr }
    }

    /// The number of entries.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Whether the table has no entry.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The entries, or `None` for a non-empty table whose pointer is null
    /// or misaligned or whose size does not fit the address space.
    fn items(&self) -> Option<&[T]> {
        if self.len == 0 {
            return Some(&[]);
        }
        let len = usize::try_from(self.len).ok()?;
        let bytes = len.checked_mul(size_of::<T>())?;
        if self.ptr.is_null() || !self.ptr.is_aligned() || isize::try_from(bytes).is_err() {
            return None;
        }
        // SAFETY: a non-empty table comes from a `'static` slice or from
        // `from_raw`, whose caller guarantees `len` initialized entries at
        // `ptr` that outlive the table; the pointer is non-null and aligned
        // and the size fits `isize`, as checked above.
        Some(unsafe { std::slice::from_raw_parts(self.ptr, len) })
    }
}

impl<T> Clone for Table<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Table<T> {}

impl<T> fmt::Debug for Table<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Table")
            .field("len", &self.len)
            .field("ptr", &self.ptr)
            .finish()
    }
}

/// `cb$image`, the program image (module documentation for the layout).
#[repr(C)]
#[derive(Debug)]
pub struct ProgramImage {
    version: u32,
    unwind_edges: u32,
    modules: Table<*const ModuleRecord>,
    entry: EntryFn,
    tramp0: Option<Tramp0>,
    tramp1: Option<Tramp1>,
    tramp2: Option<Tramp2>,
    tramp3: Option<Tramp3>,
    tramp4: Option<Tramp4>,
    main_stack: u64,
    fiber_stack: u64,
    constants: Table<u8>,
}

/// The parts of a [`ProgramImage`] built in Rust ([`ProgramImage::new`]).
#[derive(Debug, Clone, Copy)]
pub struct ImageParts {
    /// The version; [`abi::IMAGE_VERSION`] for a current image.
    pub version: u32,
    /// Whether the code was compiled with unwind edges (D-742).
    pub unwind_edges: bool,
    /// The module records, in build order.
    pub modules: Table<*const ModuleRecord>,
    /// `main`'s entry thunk.
    pub entry: EntryFn,
    /// The trampolines (D-526).
    pub callbacks: Callbacks,
    /// The root fiber's stack in bytes (D-660).
    pub main_stack: u64,
    /// The default stack of other fibers in bytes (D-660).
    pub fiber_stack: u64,
    /// The constant image.
    pub constants: Table<u8>,
}

impl ProgramImage {
    /// An image made of `parts`.
    ///
    /// # Safety
    ///
    /// Every non-null pointer of `parts.modules` points to a
    /// [`ModuleRecord`] that satisfies [`ModuleRecord::new`]'s contract,
    /// and `parts.entry` and the trampolines satisfy `run_main`'s and the
    /// callbacks' contracts, for as long as the image or what it installs
    /// is used.
    #[must_use]
    pub unsafe fn new(parts: ImageParts) -> Self {
        Self {
            version: parts.version,
            unwind_edges: u32::from(parts.unwind_edges),
            modules: parts.modules,
            entry: parts.entry,
            tramp0: parts.callbacks.t0,
            tramp1: parts.callbacks.t1,
            tramp2: parts.callbacks.t2,
            tramp3: parts.callbacks.t3,
            tramp4: parts.callbacks.t4,
            main_stack: parts.main_stack,
            fiber_stack: parts.fiber_stack,
            constants: parts.constants,
        }
    }

    /// The image's version.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// Whether the code was compiled with unwind edges (D-742).
    #[must_use]
    pub const fn unwind_edges(&self) -> bool {
        self.unwind_edges != 0
    }

    /// `main`'s entry thunk.
    #[must_use]
    pub const fn entry(&self) -> EntryFn {
        self.entry
    }

    /// The trampolines (D-526).
    #[must_use]
    pub const fn callbacks(&self) -> Callbacks {
        Callbacks {
            t0: self.tramp0,
            t1: self.tramp1,
            t2: self.tramp2,
            t3: self.tramp3,
            t4: self.tramp4,
        }
    }

    /// The root fiber's stack in bytes (D-660).
    #[must_use]
    pub const fn main_stack(&self) -> u64 {
        self.main_stack
    }

    /// The default stack of other fibers in bytes (D-660).
    #[must_use]
    pub const fn fiber_stack(&self) -> u64 {
        self.fiber_stack
    }

    /// The module records, in build order.
    fn modules(&self) -> Result<Vec<&ModuleRecord>, ImageError> {
        let pointers = self.modules.items().ok_or(ImageError::Malformed {
            module: None,
            what: "the table of module records",
        })?;
        pointers
            .iter()
            .enumerate()
            .map(|(index, record)| {
                // SAFETY: `ProgramImage::new`'s caller guarantees that a
                // non-null pointer of the table points to a valid record
                // that outlives the image.
                unsafe { record.as_ref() }.ok_or(ImageError::Malformed {
                    module: Some(index),
                    what: "a null module record",
                })
            })
            .collect()
    }
}

/// `cb$mod$<path>`, a module record (module documentation for the
/// layout).
#[repr(C)]
#[derive(Debug)]
pub struct ModuleRecord {
    strings: Table<u8>,
    types: Table<TypeEntry>,
    externals: Table<ExternalEntry>,
    hooks: Table<HookEntry>,
    free_shapes: Table<u64>,
    functions: Table<FunctionEntry>,
    constants: Table<*mut u64>,
}

/// The parts of a [`ModuleRecord`] built in Rust ([`ModuleRecord::new`]).
#[derive(Debug, Clone, Copy)]
pub struct ModuleParts {
    /// The string table.
    pub strings: Table<u8>,
    /// The user types, constructors in tag order.
    pub types: Table<TypeEntry>,
    /// The external types.
    pub externals: Table<ExternalEntry>,
    /// The `@std_hooks`.
    pub hooks: Table<HookEntry>,
    /// The free-shape words of the user layouts.
    pub free_shapes: Table<u64>,
    /// The call-site table.
    pub functions: Table<FunctionEntry>,
    /// The constant slots.
    pub constants: Table<*mut u64>,
}

impl ModuleRecord {
    /// A record made of `parts`.
    ///
    /// # Safety
    ///
    /// The pointers of the entries are what the module documentation says:
    /// layout words, constructor statics and constant slots are writable
    /// words no other thread uses during installation; hooks are static
    /// function values and drop functions are functions of their
    /// contracts; each valid for as long as the record or what it
    /// installs is used.
    #[must_use]
    pub const unsafe fn new(parts: ModuleParts) -> Self {
        Self {
            strings: parts.strings,
            types: parts.types,
            externals: parts.externals,
            hooks: parts.hooks,
            free_shapes: parts.free_shapes,
            functions: parts.functions,
            constants: parts.constants,
        }
    }
}

/// A user type of a module record.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TypeEntry {
    /// The type's name as debug printing shows it (a string).
    pub name: u32,
    /// 0.
    pub pad: u32,
    /// The type's key (D-559).
    pub type_key: u64,
    /// Its constructors in tag order.
    pub ctors: Table<CtorEntry>,
}

/// A constructor of a [`TypeEntry`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CtorEntry {
    /// The constructor's path, `module::Ctor` (a string): MIR's
    /// `LayoutDesc::name`, which the JIT registers too.
    pub name: u32,
    /// The constructor's name as debug printing shows it (a string).
    pub ctor_name: u32,
    /// 1 when the constructor's fields are named, else 0.
    pub record: u32,
    /// 0.
    pub pad: u32,
    /// The fields in declaration order.
    pub fields: Table<FieldEntry>,
    /// The constructor's `cb$layout$…` word.
    pub layout_word: *mut u64,
    /// The `cb$ctor$…` static of a nullary constructor, else null.
    pub ctor_static: *mut u64,
}

/// A field of a [`CtorEntry`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FieldEntry {
    /// An index into [`FIELD_KINDS`].
    pub kind: u32,
    /// The field's name (a string), or [`NO_STRING`].
    pub name: u32,
    /// The field's byte offset in the object.
    pub offset: u32,
    /// 0.
    pub pad: u32,
}

/// An external type of a module record.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ExternalEntry {
    /// The type's name (a string).
    pub type_name: u32,
    /// 0.
    pub pad: u32,
    /// The type's key (D-559).
    pub type_key: u64,
    /// The drop function, or null.
    pub drop: *const u8,
}

/// The `@std_hooks` of one type of a module record (D-482).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct HookEntry {
    /// The type, an index into the record's `types`.
    pub type_index: u32,
    /// 0.
    pub pad: u32,
    /// The static function value of the `equal` hook.
    pub equal: *const u8,
    /// The static function value of the `hash` hook.
    pub hash: *const u8,
    /// The static function value of the `debug` hook.
    pub debug: *const u8,
}

/// A function of the call-site table (D-867).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FunctionEntry {
    /// The function's first byte.
    pub code: *const u8,
    /// The function's length in bytes.
    pub len: u32,
    /// [`FN_CHEBY_ABI`] or 0.
    pub flags: u32,
    /// The function's call sites that have a location.
    pub sites: Table<CallSiteEntry>,
}

/// A call site: D-867's 16-byte record.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallSiteEntry {
    /// The return address's offset from the function's start.
    pub ret_offset: u32,
    /// The path (a string), or [`NO_STRING`].
    pub path: u32,
    /// The line, from 1.
    pub line: u32,
    /// The column, from 1.
    pub column: u32,
}

// The layout is the cross-wave contract: every offset the module
// documentation gives.
const _: () = {
    assert!(size_of::<usize>() == 8);
    assert!(size_of::<Table<u8>>() == 16);
    assert!(offset_of!(Table<u8>, len) == 0);
    assert!(offset_of!(Table<u8>, ptr) == 8);

    assert!(size_of::<ProgramImage>() == 104);
    assert!(offset_of!(ProgramImage, version) == 0);
    assert!(offset_of!(ProgramImage, unwind_edges) == 4);
    assert!(offset_of!(ProgramImage, modules) == 8);
    assert!(offset_of!(ProgramImage, entry) == 24);
    assert!(offset_of!(ProgramImage, tramp0) == 32);
    assert!(offset_of!(ProgramImage, tramp1) == 40);
    assert!(offset_of!(ProgramImage, tramp2) == 48);
    assert!(offset_of!(ProgramImage, tramp3) == 56);
    assert!(offset_of!(ProgramImage, tramp4) == 64);
    assert!(offset_of!(ProgramImage, main_stack) == 72);
    assert!(offset_of!(ProgramImage, fiber_stack) == 80);
    assert!(offset_of!(ProgramImage, constants) == 88);

    assert!(size_of::<ModuleRecord>() == 112);
    assert!(offset_of!(ModuleRecord, strings) == 0);
    assert!(offset_of!(ModuleRecord, types) == 16);
    assert!(offset_of!(ModuleRecord, externals) == 32);
    assert!(offset_of!(ModuleRecord, hooks) == 48);
    assert!(offset_of!(ModuleRecord, free_shapes) == 64);
    assert!(offset_of!(ModuleRecord, functions) == 80);
    assert!(offset_of!(ModuleRecord, constants) == 96);

    assert!(size_of::<TypeEntry>() == 32);
    assert!(offset_of!(TypeEntry, name) == 0);
    assert!(offset_of!(TypeEntry, type_key) == 8);
    assert!(offset_of!(TypeEntry, ctors) == 16);

    assert!(size_of::<CtorEntry>() == 48);
    assert!(offset_of!(CtorEntry, name) == 0);
    assert!(offset_of!(CtorEntry, ctor_name) == 4);
    assert!(offset_of!(CtorEntry, record) == 8);
    assert!(offset_of!(CtorEntry, fields) == 16);
    assert!(offset_of!(CtorEntry, layout_word) == 32);
    assert!(offset_of!(CtorEntry, ctor_static) == 40);

    assert!(size_of::<FieldEntry>() == 16);
    assert!(offset_of!(FieldEntry, kind) == 0);
    assert!(offset_of!(FieldEntry, name) == 4);
    assert!(offset_of!(FieldEntry, offset) == 8);

    assert!(size_of::<ExternalEntry>() == 24);
    assert!(offset_of!(ExternalEntry, type_name) == 0);
    assert!(offset_of!(ExternalEntry, type_key) == 8);
    assert!(offset_of!(ExternalEntry, drop) == 16);

    assert!(size_of::<HookEntry>() == 32);
    assert!(offset_of!(HookEntry, type_index) == 0);
    assert!(offset_of!(HookEntry, equal) == 8);
    assert!(offset_of!(HookEntry, hash) == 16);
    assert!(offset_of!(HookEntry, debug) == 24);

    assert!(size_of::<FunctionEntry>() == 32);
    assert!(offset_of!(FunctionEntry, code) == 0);
    assert!(offset_of!(FunctionEntry, len) == 8);
    assert!(offset_of!(FunctionEntry, flags) == 12);
    assert!(offset_of!(FunctionEntry, sites) == 16);

    assert!(size_of::<CallSiteEntry>() == 16);
    assert!(offset_of!(CallSiteEntry, ret_offset) == 0);
    assert!(offset_of!(CallSiteEntry, path) == 4);
    assert!(offset_of!(CallSiteEntry, line) == 8);
    assert!(offset_of!(CallSiteEntry, column) == 12);
};

/// Why an image cannot be installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// The image's version is not [`abi::IMAGE_VERSION`].
    Version {
        /// The image's version.
        found: u32,
        /// The runtime's.
        expected: u32,
    },
    /// A table, string or reference of the image is malformed.
    Malformed {
        /// The module record's index in build order, if one is at fault.
        module: Option<usize>,
        /// What is malformed.
        what: &'static str,
    },
    /// The image holds a table this runtime does not install yet: a
    /// constant image or constant slots, which constants reaching
    /// executables add (K5a).
    NotYet(&'static str),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version { found, expected } => write!(
                f,
                "the program image has version {found}, but this runtime reads version {expected}"
            ),
            Self::Malformed {
                module: Some(module),
                what,
            } => write!(
                f,
                "the program image is malformed: {what} in module record {module}"
            ),
            Self::Malformed { module: None, what } => {
                write!(f, "the program image is malformed: {what}")
            }
            Self::NotYet(what) => write!(
                f,
                "the program image holds {what}, which this runtime cannot install yet"
            ),
        }
    }
}

impl std::error::Error for ImageError {}

impl ImageError {
    /// A malformed part of module record `module`.
    const fn malformed(module: usize, what: &'static str) -> Self {
        Self::Malformed {
            module: Some(module),
            what,
        }
    }
}

/// Installs `image` (module documentation): registers the builtin list
/// and map kinds, then the image's module records in build order with
/// [`register_records`], under a fresh code owner that is never
/// unregistered (D-867). Runs before any worker of the program starts.
///
/// # Errors
///
/// [`ImageError::NotYet`] for a non-empty constant image or constant slot
/// table (K5a), and [`ImageError::Malformed`] as [`register_records`]
/// says; nothing of the image is registered and no word is written then,
/// except the builtin kinds, which every process registers anyway. The
/// version is not checked here (`cheby_rt_main` checks it in debug
/// builds, D-925).
pub fn install(image: &ProgramImage) -> Result<(), ImageError> {
    crate::list::register_kinds();
    crate::hamt::register_kinds();
    let modules = image.modules()?;
    if !image.constants.is_empty() {
        return Err(ImageError::NotYet("a constant image"));
    }
    // One owner for the whole program, never unregistered: an
    // executable's code lives as long as the process (D-867).
    //
    // SAFETY: `ProgramImage::new`'s caller guarantees that every record
    // of the image satisfies `ModuleRecord::new`'s contract for as long as
    // the image or what it installs is used: its tables, words, statics,
    // function values and code stay valid, and its layout words and
    // constructor statics are writable data nothing else writes while the
    // image is installed, before any worker starts (D-920).
    unsafe { register_records(&modules, locate::new_code_owner()) }
}

/// Registers module records (D-919, D-920, D-1163): checks every record
/// first (its tables, strings, field kinds and offsets, layout words,
/// constructor statics, hooks, free-shape words and external entries),
/// then, record by record in the order given (build order), registers its
/// user layouts in D-1163's order and with D-1163's facts and writes each
/// layout word's and constructor static's header (D-454), registers its
/// external types (`layout::register_external`, after the record's user
/// layouts, GAP(record-externals-order)), then sets every record's hooks
/// (D-585) and registers the call-site ranges under `owner` (D-867). The
/// one routine of D-919: an executable's start calls it through
/// [`install`], and every JIT host after each batch.
///
/// # Errors
///
/// [`ImageError::Malformed`] before anything is registered, for a table,
/// string, reference or word that is not what the module documentation
/// says, a free-shape word that is not the one registration derives
/// (GAP(free-shapes-checked)), or an external type with a drop function,
/// which no host passes until user drop functions are registered (D-703);
/// [`ImageError::NotYet`] for a non-empty constant slot table (K5a). The
/// one failure after registration has begun is a full layout table (2^24
/// ids), also [`ImageError::Malformed`].
///
/// # Safety
///
/// Every record, its tables and every word, static, function value and
/// code range they point to stay valid for as long as anything registered
/// may be used; the layout words and constructor statics are writable,
/// no code has used them yet (an executable's before its workers start, a
/// JIT batch's before any code of the batch runs), and nothing else writes
/// them meanwhile.
pub unsafe fn register_records(records: &[&ModuleRecord], owner: u64) -> Result<(), ImageError> {
    // Every record is checked before anything is registered.
    let mut checked: Vec<CheckedRecord> = Vec::with_capacity(records.len());
    let mut ranges: Vec<CodeRangeDesc> = Vec::new();
    for (index, record) in records.iter().enumerate() {
        if !record.constants.is_empty() {
            return Err(ImageError::NotYet("constant slots"));
        }
        let types = user_types(index, record)?;
        check_free_shapes(index, record, &types)?;
        let externals = external_types(index, record)?;
        let hooks = record_hooks(index, record, types.len())?;
        code_ranges(index, record, &mut ranges)?;
        checked.push(CheckedRecord {
            types,
            externals,
            hooks,
        });
    }
    // Then the layouts, record by record in build order: each type's
    // constructors with consecutive ids in tag order (D-522), in the
    // order of the record's `types` (D-1163), then the record's external
    // types in table order (GAP(record-externals-order)).
    let mut hooks: Vec<(LayoutId, [Word; 3])> = Vec::new();
    for (index, record) in checked.into_iter().enumerate() {
        let mut ids: Vec<Vec<LayoutId>> = Vec::with_capacity(record.types.len());
        for ty in record.types {
            ids.push(register_type(index, ty)?);
        }
        for (type_name, type_key) in record.externals {
            layout::register_external(&type_name, type_key).map_err(|_| {
                ImageError::malformed(index, "more user layouts than the layout table holds")
            })?;
        }
        for (type_index, words) in record.hooks {
            let type_ids = ids.get(type_index).map_or(&[][..], Vec::as_slice);
            hooks.extend(type_ids.iter().map(|&id| (id, words)));
        }
    }
    for (id, words) in hooks {
        layout::set_hooks(id, words).map_err(|_| ImageError::Malformed {
            module: None,
            what: "hooks of a layout that already has other hooks",
        })?;
    }
    if !ranges.is_empty() {
        locate::register_code(owner, ranges);
    }
    Ok(())
}

/// One record, checked: its user types, its external types (name and
/// key) and its hooks.
struct CheckedRecord {
    types: Vec<CheckedType>,
    externals: Vec<(String, u64)>,
    hooks: Vec<TypeHooks>,
}

/// Checks the `free_shapes` table of `record`, whose user types are
/// `types`: one word per user constructor layout, types in order and each
/// type's constructors in tag order, each equal to the word registration
/// derives for the layout ([`layout::free_shape_word`], D-907), so a
/// stale or foreign record never sets a wrong shape
/// (GAP(free-shapes-checked)).
fn check_free_shapes(
    index: usize,
    record: &ModuleRecord,
    types: &[CheckedType],
) -> Result<(), ImageError> {
    let words = record
        .free_shapes
        .items()
        .ok_or(ImageError::malformed(index, "the free-shape table"))?;
    let mut derived = types
        .iter()
        .flat_map(|ty| ty.infos.iter().map(layout::free_shape_word));
    let mut given = words.iter();
    loop {
        match (derived.next(), given.next()) {
            (None, None) => return Ok(()),
            (Some(derived), Some(&given)) if derived == given => {}
            (Some(_), Some(_)) => {
                return Err(ImageError::malformed(
                    index,
                    "a free-shape word that is not the one registration derives (D-907)",
                ));
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err(ImageError::malformed(
                    index,
                    "a free-shape table that does not hold one word per user layout",
                ));
            }
        }
    }
}

/// Checks the external types of `record` and returns each one's name and
/// key, in table order. A drop function is refused until user drop
/// functions are registered (D-101, D-703): no code generation writes one
/// yet (GAP(record-externals-order)).
fn external_types(index: usize, record: &ModuleRecord) -> Result<Vec<(String, u64)>, ImageError> {
    let malformed = |what| ImageError::malformed(index, what);
    let strings = record
        .strings
        .items()
        .ok_or(malformed("the string table"))?;
    let externals = record
        .externals
        .items()
        .ok_or(malformed("the external type table"))?;
    let mut out = Vec::with_capacity(externals.len());
    for external in externals {
        let name =
            string(strings, external.type_name).ok_or(malformed("an external type's name"))?;
        if !external.drop.is_null() {
            return Err(malformed(
                "an external type's drop function, which this runtime does not register yet",
            ));
        }
        out.push((name.to_owned(), external.type_key));
    }
    Ok(out)
}

/// The hooks of one type of a record: its index in the record's `types`,
/// and the `equal`, `hash` and `debug` words.
type TypeHooks = (usize, [Word; 3]);

/// A user type of a record, checked: each constructor's runtime layout,
/// in tag order, with the words registration writes.
struct CheckedType {
    infos: Vec<LayoutInfo>,
    /// Per constructor: its layout word and its constructor static (null
    /// for a constructor with fields).
    words: Vec<(*mut u64, *mut u64)>,
}

/// Checks the user types of `record` and makes each constructor's
/// [`LayoutInfo`] from the facts code generation writes from MIR's
/// `LayoutDesc` (D-1163): [`LayoutInfo::ctor`] of its name, tag and
/// fields, then [`LayoutInfo::named`] with its type's name and key and its
/// record flag. The record's field offsets must be the runtime's own
/// (D-454).
fn user_types(index: usize, record: &ModuleRecord) -> Result<Vec<CheckedType>, ImageError> {
    let malformed = |what| ImageError::malformed(index, what);
    let strings = record
        .strings
        .items()
        .ok_or(malformed("the string table"))?;
    let text = |offset: u32, what| string(strings, offset).ok_or(malformed(what));
    let types = record.types.items().ok_or(malformed("the type table"))?;
    let mut out = Vec::with_capacity(types.len());
    for ty in types {
        let type_name = text(ty.name, "a type's name")?;
        let ctors = ty
            .ctors
            .items()
            .ok_or(malformed("a type's constructor table"))?;
        if ctors.is_empty() {
            return Err(malformed("a type without constructors"));
        }
        let mut infos = Vec::with_capacity(ctors.len());
        let mut words = Vec::with_capacity(ctors.len());
        for (tag, ctor) in ctors.iter().enumerate() {
            let tag = u32::try_from(tag).map_err(|_| malformed("a constructor's tag"))?;
            let name = text(ctor.name, "a constructor's name")?;
            let ctor_name = text(ctor.ctor_name, "a constructor's name")?;
            let record_flag = match ctor.record {
                0 => false,
                1 => true,
                _ => return Err(malformed("a constructor's record flag")),
            };
            let entries = ctor
                .fields
                .items()
                .ok_or(malformed("a constructor's field table"))?;
            let mut fields: Vec<(FieldKind, Option<&str>)> = Vec::with_capacity(entries.len());
            for field in entries {
                let kind = usize::try_from(field.kind)
                    .ok()
                    .and_then(|kind| FIELD_KINDS.get(kind))
                    .ok_or(malformed("a field kind"))?;
                let field_name = if field.name == NO_STRING {
                    None
                } else {
                    Some(text(field.name, "a field's name")?)
                };
                fields.push((*kind, field_name));
            }
            let info = LayoutInfo::ctor(name, tag, &fields).named(
                type_name,
                ctor_name,
                record_flag,
                ty.type_key,
            );
            let offsets_agree = info.fields.len() == entries.len()
                && info
                    .fields
                    .iter()
                    .zip(entries)
                    .all(|(runtime, entry)| runtime.offset == entry.offset);
            if !offsets_agree {
                return Err(malformed(
                    "a field offset that is not the runtime's own (D-454)",
                ));
            }
            info.validate()
                .map_err(|_| malformed("a constructor layout the runtime refuses"))?;
            if ctor.layout_word.is_null() || !ctor.layout_word.is_aligned() {
                return Err(malformed("a constructor without a layout word"));
            }
            if entries.is_empty() == ctor.ctor_static.is_null() || !ctor.ctor_static.is_aligned() {
                return Err(malformed(
                    "a constructor static that is not there exactly for a constructor without fields",
                ));
            }
            infos.push(info);
            words.push((ctor.layout_word, ctor.ctor_static));
        }
        out.push(CheckedType { infos, words });
    }
    Ok(out)
}

/// Registers the constructors of one checked type of record `index` with
/// consecutive ids in tag order (D-522) and writes each layout word's and
/// constructor static's header, as code generation writes them in the
/// JIT (`decls::ctor_header` and `decls::header`): the layout word holds
/// count 1, the constructor's value-field count as scan count, escaped
/// above [`abi::SCAN_MAX`] (D-279, D-555), and the id; the static holds
/// count 0, scan count 0 and the id. Returns the ids in tag order.
fn register_type(index: usize, ty: CheckedType) -> Result<Vec<LayoutId>, ImageError> {
    let scans: Vec<u8> = ty.infos.iter().map(LayoutInfo::scan_count).collect();
    let count = u32::try_from(ty.infos.len())
        .map_err(|_| ImageError::malformed(index, "a type with too many constructors"))?;
    let first = layout::register_consecutive(ty.infos).map_err(|_| {
        ImageError::malformed(index, "more user layouts than the layout table holds")
    })?;
    let ids: Vec<LayoutId> = (0..count).map(|k| first.saturating_add(k)).collect();
    for ((&id, &scan), &(word, ctor_static)) in ids.iter().zip(&scans).zip(&ty.words) {
        // SAFETY: `user_types` checked that `word` is non-null and
        // aligned, and `register_records`' contract (code generation's
        // for both of its callers, D-920: `cb$layout$…` is writable data)
        // makes it a writable word that nothing else writes and that no
        // code has used yet: an executable's, before its workers start
        // (`install`), or a JIT batch's, before any code of the batch
        // runs, whatever other fibers run meanwhile.
        unsafe { word.write(abi::header(abi::RC_UNIQUE, scan, id)) };
        if !ctor_static.is_null() {
            // SAFETY: as for `word`: a checked, writable `cb$ctor$…` static
            // (writable in a unit compiled alone and in a JIT batch,
            // D-920) that no code has used yet and nothing else writes.
            unsafe { ctor_static.write(abi::header(abi::RC_STATIC, 0, id)) };
        }
    }
    Ok(ids)
}

/// Checks the hooks of `record`, whose `types` table lists `type_count`
/// types: each names a listed type, at most once, with three non-null
/// static function values (D-482). Returns each hooked type's index with
/// its three words, which installation sets on every constructor of the
/// type (D-585).
fn record_hooks(
    index: usize,
    record: &ModuleRecord,
    type_count: usize,
) -> Result<Vec<TypeHooks>, ImageError> {
    let hooks = record
        .hooks
        .items()
        .ok_or(ImageError::malformed(index, "the hook table"))?;
    let mut out: Vec<TypeHooks> = Vec::with_capacity(hooks.len());
    for hook in hooks {
        let type_index = usize::try_from(hook.type_index)
            .ok()
            .filter(|&type_index| type_index < type_count)
            .ok_or(ImageError::malformed(
                index,
                "a hook names a type the record does not list",
            ))?;
        if out.iter().any(|&(seen, _)| seen == type_index) {
            return Err(ImageError::malformed(
                index,
                "two hook entries for one type",
            ));
        }
        let mut words = [0; 3];
        for (word, value) in words.iter_mut().zip([hook.equal, hook.hash, hook.debug]) {
            if value.is_null() {
                return Err(ImageError::malformed(index, "a null hook"));
            }
            *word = u64::try_from(value.expose_provenance())
                .map_err(|_| ImageError::malformed(index, "a hook address"))?;
        }
        out.push((type_index, words));
    }
    Ok(out)
}

/// Appends the call-site ranges of `record`'s functions to `out`.
fn code_ranges(
    index: usize,
    record: &ModuleRecord,
    out: &mut Vec<CodeRangeDesc>,
) -> Result<(), ImageError> {
    let strings = record
        .strings
        .items()
        .ok_or(ImageError::malformed(index, "the string table"))?;
    let functions = record
        .functions
        .items()
        .ok_or(ImageError::malformed(index, "the call-site table"))?;
    for function in functions {
        if function.code.is_null() {
            return Err(ImageError::malformed(index, "a function without code"));
        }
        if function.flags & !FN_CHEBY_ABI != 0 {
            return Err(ImageError::malformed(index, "unknown function flags"));
        }
        let sites = function
            .sites
            .items()
            .ok_or(ImageError::malformed(index, "a function's call sites"))?;
        let mut descs = Vec::with_capacity(sites.len());
        for site in sites {
            let location = if site.path == NO_STRING {
                // No `path:line:column`: the map records the site without
                // a printable location.
                String::new()
            } else {
                let path = string(strings, site.path)
                    .ok_or(ImageError::malformed(index, "a call site's path"))?;
                format!("{path}:{}:{}", site.line, site.column)
            };
            descs.push(CallSiteDesc {
                ret_offset: site.ret_offset,
                location,
            });
        }
        out.push(CodeRangeDesc {
            start: function.code.addr(),
            len: function.len,
            cheby_abi: function.flags & FN_CHEBY_ABI != 0,
            sites: descs,
        });
    }
    Ok(())
}

/// The string at `offset` of a string table (module documentation), or
/// `None` when it does not fit the table or is not UTF-8.
fn string(strings: &[u8], offset: u32) -> Option<&str> {
    let start = usize::try_from(offset).ok()?;
    let body = start.checked_add(size_of::<u32>())?;
    let len: [u8; 4] = strings.get(start..body)?.try_into().ok()?;
    let end = body.checked_add(usize::try_from(u32::from_le_bytes(len)).ok()?)?;
    std::str::from_utf8(strings.get(body..end)?).ok()
}

/// The version check `cheby_rt_main` runs in debug builds (D-925).
///
/// # Errors
///
/// [`ImageError::Version`] when `image`'s version is not
/// [`abi::IMAGE_VERSION`].
pub const fn check_version(image: &ProgramImage) -> Result<(), ImageError> {
    if image.version == abi::IMAGE_VERSION {
        Ok(())
    } else {
        Err(ImageError::Version {
            found: image.version,
            expected: abi::IMAGE_VERSION,
        })
    }
}
