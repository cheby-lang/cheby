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
//! | 88     | `constants`    | table of bytes               | the constants table: the program's constant images (D-979), empty for a program without any |
//!
//! The constants table is `consteval::image::encode_table`'s bytes: every
//! image of the constants the program reaches, in D-535's order (its format
//! is documented in [`crate::consteval::image`]).
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
//! | 80     | `functions`   | [`FunctionEntry`], 40 bytes each    | the call-site table (D-856, D-867), with each function's name (D-1041)      |
//! | 96     | `constants`   | [`ConstEntry`], 16 bytes each       | the module's constants with their slots `cb$const$…`, `DefIndex` order      |
//!
//! [`ConstEntry`] (16 bytes, D-1189): `path` (`u32`
//! string, the constant's path `module::name`, which names its image) at
//! 0, `pad` (`u32`) at 4, `slot` (pointer to the writable slot
//! `cb$const$…`) at 8. A record lists every constant its unit initializes
//! (a unit compiled alone defines their slots); a JIT batch's records list
//! none, since a JIT host finds slots by constant (`Jit::const_slot`).
//! Version 2 of the layout gave the table these entries; version 1 held
//! bare slot pointers and was always empty.
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
//! [`FunctionEntry`] (40 bytes): `code` (pointer to the function's first
//! byte) at 0, `len` (`u32`, its length in bytes) at 8, `flags` (`u32`:
//! [`FN_CHEBY_ABI`] when it has the Cheby ABI, [`FN_LARGE_BODY`] when it is
//! a large function's inner body, no other bit) at 12, `name` (`u32`
//! string, the function's display name, D-1041, or [`NO_STRING`] for code
//! without a name: value shims, `dyn` forwarders, trampolines, entry
//! thunks, `@blocking` shims) at 16, `pad` (`u32`) at 20, `sites` (table of
//! [`CallSiteEntry`], any order) at 24. Version 3 of the layout gave the
//! entry its name (GAP(backtrace-record-layout)); version 2's entry was 32
//! bytes, without `name` and `pad`.
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
//! type named, every hook naming a listed type
//! once with three non-null values, every function name in range) before
//! it registers anything. It then
//! registers, record by record in the order given, each type's
//! constructors with consecutive ids in tag order (D-522, D-1163), writing
//! into each layout word the header that allocation copies and into each
//! constructor static its static header (D-454, D-920), then the record's
//! external types in table order, each with its drop function
//! (`layout::register_external_with_drop`, D-1168, D-949); then the hooks of every record on every
//! constructor of each hooked type (D-482, D-585), and the call-site
//! ranges under the caller's code owner (D-867), each pointing at its
//! function's name in the record, nothing copied (D-1041).
//!
//! [`register_records`] also checks every record's constants (a path
//! string and an aligned, non-null slot each) and registers nothing for
//! them: slots are filled by installation.
//!
//! # Installing
//!
//! [`install`] registers the builtin list and map kinds and then the
//! image's records under one code owner that is never unregistered. Then,
//! when the constants table is not empty, it installs the constants
//! (D-979) with the one routine every host calls,
//! [`consteval::image::install`](crate::consteval::image::install): the
//! table's images, in order, each root written into the slot that the
//! records' constants tables give for its path (D-1189),
//! each user constructor's type key resolved to the layout id the records
//! registered ([`type_layouts`], D-920), on an entry with the image's
//! trampolines, since building a map may hash a key with `@std_hooks`.
//! All before any worker of the program starts.

use std::collections::HashMap;
use std::fmt;
use std::mem::{offset_of, size_of};
use std::ptr;
use std::sync::{Arc, Mutex, PoisonError};

use crate::abi::{self, LayoutId, Word};
use crate::alloc::Domain;
use crate::consteval::{
    self,
    image::{ConstImageError, TypeLayouts},
};
use crate::entry::{
    self, Callbacks, EntryConfig, EntryFn, EntryMode, EntryOutcome, Tramp0, Tramp1, Tramp2, Tramp3,
    Tramp4,
};
use crate::layout::{self, FieldKind, LayoutInfo};
use crate::locate::{self, CallSiteDesc, CodeRangeDesc};

/// The string offset that names no string.
pub const NO_STRING: u32 = u32::MAX;

/// [`FunctionEntry::flags`]: the function has the Cheby ABI. A C-ABI
/// function (an entry thunk, a trampoline, a `@blocking` shim) ends a
/// deadlock walk (D-856).
pub const FN_CHEBY_ABI: u32 = 1;

/// [`FunctionEntry::flags`]: the function is the inner body `cb$big$…` of a
/// function with a large frame (D-679), named as that function; a
/// backtrace does not show the outer frame when its inner body is the next
/// frame inward (D-1039). GAP(backtrace-record-layout).
pub const FN_LARGE_BODY: u32 = 2;

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
    constants: Table<ConstEntry>,
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
    /// The constants with their slots.
    pub constants: Table<ConstEntry>,
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
    /// [`FN_CHEBY_ABI`], [`FN_LARGE_BODY`], both or 0.
    pub flags: u32,
    /// D-1041: the function's display name, an offset in the record's string
    /// table, or [`NO_STRING`] for code without a name (value shims, `dyn`
    /// forwarders, trampolines, entry thunks, `@blocking` shims).
    pub name: u32,
    /// 0.
    pub pad: u32,
    /// The function's call sites that have a location.
    pub sites: Table<CallSiteEntry>,
}

/// A constant of a module record and its slot (D-1189).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ConstEntry {
    /// The constant's path, `module::name` (a string).
    pub path: u32,
    /// 0.
    pub pad: u32,
    /// The constant's writable slot `cb$const$…`.
    pub slot: *mut u64,
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

    assert!(size_of::<FunctionEntry>() == 40);
    assert!(offset_of!(FunctionEntry, code) == 0);
    assert!(offset_of!(FunctionEntry, len) == 8);
    assert!(offset_of!(FunctionEntry, flags) == 12);
    assert!(offset_of!(FunctionEntry, name) == 16);
    assert!(offset_of!(FunctionEntry, pad) == 20);
    assert!(offset_of!(FunctionEntry, sites) == 24);

    assert!(size_of::<ConstEntry>() == 16);
    assert!(offset_of!(ConstEntry, path) == 0);
    assert!(offset_of!(ConstEntry, slot) == 8);

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
    /// The constants table cannot be installed (D-979).
    Constants(ConstImageError),
    /// The entry that installs the constants failed: its message.
    Entry(String),
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
            Self::Constants(error) => write!(f, "the program's constants: {error}"),
            Self::Entry(message) => {
                write!(f, "installing the program's constants failed: {message}")
            }
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
/// unregistered (D-867), then the constants table's images into the slots
/// the records name (D-979, D-535). Runs before any worker of the program
/// starts.
///
/// # Errors
///
/// [`ImageError::Malformed`] as [`register_records`] says, or for a
/// constants table that is not one; nothing of the image is registered and
/// no word is written then, except the builtin kinds, which every process
/// registers anyway. [`ImageError::Constants`] when an image cannot be
/// installed (checked whole before any slot is written), and
/// [`ImageError::Entry`] when its entry fails. The version is not checked
/// here (`cheby_rt_main` checks it in debug builds, D-925).
pub fn install(image: &ProgramImage) -> Result<(), ImageError> {
    crate::list::register_kinds();
    crate::hamt::register_kinds();
    let modules = image.modules()?;
    let table = image.constants.items().ok_or(ImageError::Malformed {
        module: None,
        what: "the constants table",
    })?;
    // The headers are checked here, before anything is registered; the
    // images are read in place from the table when installed, never copied.
    let images = consteval::image::table_len(table).map_err(ImageError::Constants)?;
    // One owner for the whole program, never unregistered: an
    // executable's code lives as long as the process (D-867).
    //
    // SAFETY: `ProgramImage::new`'s caller guarantees that every record
    // of the image satisfies `ModuleRecord::new`'s contract for as long as
    // the image or what it installs is used: its tables, words, statics,
    // function values and code stay valid, and its layout words and
    // constructor statics are writable data nothing else writes while the
    // image is installed, before any worker starts (D-920).
    unsafe { register_records(&modules, locate::new_code_owner()) }?;
    if images == 0 {
        return Ok(());
    }
    let mut slots: HashMap<String, usize> = HashMap::new();
    for (index, record) in modules.iter().enumerate() {
        for (path, slot) in record_constants(index, record)? {
            slots.insert(path, slot.expose_provenance());
        }
    }
    // SAFETY: the records were registered just above, so their layout
    // words hold their ids; their statics live as long as the image.
    let types = unsafe { type_layouts(&modules) }?;
    let config = EntryConfig {
        callbacks: image.callbacks(),
        unwind_edges: image.unwind_edges(),
        domain: Domain::new(),
        ..EntryConfig::new(EntryMode::Run)
    };
    // SAFETY: the image's slots are writable data no code has read yet,
    // the layouts and statics are the records' registered ones, the
    // trampolines are the image's, valid for the process's life, and the
    // table outlives this call, as `ProgramImage::new`'s caller
    // guarantees.
    unsafe { install_constants(&config, table, slots, types) }
}

/// Installs the images of the `constants` table `table` on an entry of
/// `config`, reading them in place, finding each slot in `slots` by path,
/// with `types`' layouts (D-979).
///
/// # Safety
///
/// As [`consteval::image::install`]: every slot of `slots` is a writable
/// constant slot no code reads yet, and `types` names registered layouts
/// and live statics; `config`'s trampolines are valid during the call,
/// and `table` stays valid and unchanged until it returns.
unsafe fn install_constants(
    config: &EntryConfig,
    table: &[u8],
    slots: HashMap<String, usize>,
    types: TypeLayouts,
) -> Result<(), ImageError> {
    let result: Arc<Mutex<Option<Result<(), ConstImageError>>>> = Arc::default();
    let out = Arc::clone(&result);
    // The entry's task must be `'static`: it gets the table's address and
    // length, and reads it in place while this call waits for the entry.
    let (address, len) = (table.as_ptr().expose_provenance(), table.len());
    let task = move || {
        let mut slot_of = |path: &str| {
            slots
                .get(path)
                .map(|&address| ptr::with_exposed_provenance_mut::<u64>(address))
        };
        // SAFETY: the caller keeps `table` valid and unchanged until this
        // function returns, and `run_task` returns only once the task has
        // ended (an entry that ended the program never comes back to the
        // table); `address` and `len` are its pointer and length.
        let table =
            unsafe { std::slice::from_raw_parts(ptr::with_exposed_provenance::<u8>(address), len) };
        // SAFETY: forwarded from the caller.
        let installed = unsafe { consteval::image::install_table(table, &mut slot_of, &types) };
        *out.lock().unwrap_or_else(PoisonError::into_inner) = Some(installed);
        0
    };
    // SAFETY: forwarded from the caller: the trampolines stay valid while
    // the entry runs.
    match unsafe { entry::run_task(config, task) } {
        Ok(EntryOutcome::Returned(_)) => {
            match result.lock().unwrap_or_else(PoisonError::into_inner).take() {
                Some(installed) => installed.map_err(ImageError::Constants),
                None => Err(ImageError::Entry("it returned no result".to_owned())),
            }
        }
        Ok(EntryOutcome::Panicked(report)) => Err(ImageError::Entry(report.message)),
        Ok(EntryOutcome::Exited(status)) => {
            Err(ImageError::Entry(format!("it ended with status {status}")))
        }
        Ok(EntryOutcome::Deadlocked(_)) => Err(ImageError::Entry("it deadlocked".to_owned())),
        Err(error) => Err(ImageError::Entry(error.to_string())),
    }
}

/// The constants of record `index` with their slots, checked: a path
/// string and an aligned, non-null slot each.
fn record_constants(
    index: usize,
    record: &ModuleRecord,
) -> Result<Vec<(String, *mut u64)>, ImageError> {
    let malformed = |what| ImageError::malformed(index, what);
    let strings = record
        .strings
        .items()
        .ok_or(malformed("the string table"))?;
    let entries = record
        .constants
        .items()
        .ok_or(malformed("the constants table"))?;
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let path = string(strings, entry.path).ok_or(malformed("a constant's path"))?;
        if entry.slot.is_null() || !entry.slot.is_aligned() {
            return Err(malformed("a constant without its slot"));
        }
        out.push((path.to_owned(), entry.slot));
    }
    Ok(out)
}

/// The user layouts `records` registered, by type key (D-920, D-559):
/// each constructor's layout id, read from its layout word's header, and
/// its constructor static. A later record's type replaces an earlier
/// one's with the same key (a new REPL generation, D-1169).
///
/// # Errors
///
/// [`ImageError::Malformed`] for a record whose tables are not what the
/// module documentation says.
///
/// # Safety
///
/// Every record was registered ([`register_records`]), so its layout
/// words hold their ids, and its words and statics are live.
pub unsafe fn type_layouts(records: &[&ModuleRecord]) -> Result<TypeLayouts, ImageError> {
    let mut out = TypeLayouts::new();
    for (index, record) in records.iter().enumerate() {
        let malformed = |what| ImageError::malformed(index, what);
        let types = record.types.items().ok_or(malformed("the type table"))?;
        for ty in types {
            let ctors = ty
                .ctors
                .items()
                .ok_or(malformed("a type's constructor table"))?;
            let mut layouts = Vec::with_capacity(ctors.len());
            for ctor in ctors {
                if ctor.layout_word.is_null() || !ctor.layout_word.is_aligned() {
                    return Err(malformed("a constructor without a layout word"));
                }
                // SAFETY: a registered layout word, live and aligned
                // (checked), written before this call.
                let header = unsafe { ctor.layout_word.read() };
                let static_word = if ctor.ctor_static.is_null() {
                    0
                } else {
                    u64::try_from(ctor.ctor_static.expose_provenance())
                        .map_err(|_| malformed("a constructor static's address"))?
                };
                layouts.push((abi::header_layout(header), static_word));
            }
            out.insert(ty.type_key, layouts);
        }
    }
    Ok(out)
}

/// [`type_layouts`] of the records at `addresses`: a JIT's
/// (`Jit::module_records`), whose addresses it keeps.
///
/// # Errors
///
/// As [`type_layouts`], and [`ImageError::Malformed`] for a null or
/// misaligned address.
///
/// # Safety
///
/// Every address is that of a registered [`ModuleRecord`] that stays live
/// during the call, as [`type_layouts`] requires.
pub unsafe fn type_layouts_at(addresses: &[usize]) -> Result<TypeLayouts, ImageError> {
    let mut records: Vec<&ModuleRecord> = Vec::with_capacity(addresses.len());
    for (index, &address) in addresses.iter().enumerate() {
        let pointer = ptr::with_exposed_provenance::<ModuleRecord>(address);
        if !pointer.is_aligned() {
            return Err(ImageError::malformed(index, "a misaligned module record"));
        }
        // SAFETY: the caller's contract: a live, registered record.
        let record = unsafe { pointer.as_ref() }
            .ok_or(ImageError::malformed(index, "a null module record"))?;
        records.push(record);
    }
    // SAFETY: forwarded from the caller.
    unsafe { type_layouts(&records) }
}

/// Registers module records (D-919, D-920, D-1163): checks every record
/// first (its tables, strings, field kinds and offsets, layout words,
/// constructor statics, hooks, free-shape words and external entries),
/// then, record by record in the order given (build order), registers its
/// user layouts in D-1163's order and with D-1163's facts and writes each
/// layout word's and constructor static's header (D-454), registers its
/// external types (`layout::register_external`, after the record's user
/// layouts, D-1168), then sets every record's hooks
/// (D-585) and registers the call-site ranges under `owner` (D-867). The
/// one routine of D-919: an executable's start calls it through
/// [`install`], and every JIT host after each batch.
///
/// # Errors
///
/// [`ImageError::Malformed`] before anything is registered, for a table,
/// string, reference or word that is not what the module documentation
/// says, a free-shape word that is not the one registration derives
/// (D-1167), or a constant without a path or a slot. The one failure after
/// registration has begun is a full layout table (2^24 ids), also
/// [`ImageError::Malformed`].
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
        // Slots are filled by installation (D-979); only checked here.
        record_constants(index, record)?;
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
    // types in table order (D-1168).
    let mut hooks: Vec<(LayoutId, [Word; 3])> = Vec::new();
    for (index, record) in checked.into_iter().enumerate() {
        let mut ids: Vec<Vec<LayoutId>> = Vec::with_capacity(record.types.len());
        for ty in record.types {
            ids.push(register_type(index, ty)?);
        }
        for (type_name, type_key, drop) in record.externals {
            // SAFETY: a record's drop word is null or its type's native drop
            // function, `void f(void *handle)` (D-949), which the caller
            // keeps valid as long as anything registered may be used.
            let drop = unsafe { layout::external_drop_from(drop) };
            layout::register_external_with_drop(&type_name, type_key, drop).map_err(|_| {
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
    externals: Vec<(String, u64, *const u8)>,
    hooks: Vec<TypeHooks>,
}

/// Checks the `free_shapes` table of `record`, whose user types are
/// `types`: one word per user constructor layout, types in order and each
/// type's constructors in tag order, each equal to the word registration
/// derives for the layout ([`layout::free_shape_word`], D-907), so a
/// stale or foreign record never sets a wrong shape
/// (D-1167).
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

/// Checks the external types of `record` and returns each one's name, key
/// and drop function (null for none), in table order (D-1168, D-949).
fn external_types(
    index: usize,
    record: &ModuleRecord,
) -> Result<Vec<(String, u64, *const u8)>, ImageError> {
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
        out.push((name.to_owned(), external.type_key, external.drop));
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
        if function.flags & !(FN_CHEBY_ABI | FN_LARGE_BODY) != 0 {
            return Err(ImageError::malformed(index, "unknown function flags"));
        }
        let name = if function.name == NO_STRING {
            None
        } else {
            let text = string(strings, function.name)
                .ok_or(ImageError::malformed(index, "a function's name"))?;
            // SAFETY: `text` lies in the record's string table, which
            // `register_records`' caller keeps valid and unchanged for as
            // long as anything registered may be used: the range this name
            // goes with included, until its owner is unregistered (D-867).
            // The map reads it only while it holds the range.
            Some(unsafe { locate::FunctionName::in_record(text) })
        };
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
            name,
            large_body: function.flags & FN_LARGE_BODY != 0,
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
