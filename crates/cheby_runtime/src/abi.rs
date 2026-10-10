//! The ABI shared by the runtime and code generation: words, object headers,
//! tagged integers, builtin layout ids, string objects, execution-context
//! offsets, exit statuses, panic kinds and symbol names.
//!
//! This module has no statics and no dependencies, so `cheby_codegen_clif`
//! reads it without touching the runtime's state. Everything here is a pure
//! function of its arguments or a constant.
//!
//! Byte offsets that generated code loads from are `i32`, the type Cranelift
//! takes for memory offsets.

/// A Cheby value at the ABI: a tagged 63-bit integer (low bit 1) or a
/// pointer to an 8-byte-aligned heap or static object (low bit 0) (D-278).
pub type Word = u64;

/// An index into the global layout table (D-279). Only the low 24 bits are
/// ever used.
pub type LayoutId = u32;

/// The low bit of a tagged integer word (D-278).
pub const TAG_INT: u64 = 1;

/// The size of the object header in bytes (D-279).
pub const HEADER_SIZE: u64 = 8;

/// The size of one object field in bytes.
pub const FIELD_SIZE: u64 = 8;

/// The alignment of every object, heap or static.
pub const OBJECT_ALIGN: u64 = 8;

/// The reference count that marks a static object. Count operations on it
/// are no-ops (§9.7). A live heap object never has this count: a local
/// count `n > 0` and a shared count `-n` both mean `n` references, so 0 is
/// reached only by freeing (D-454).
pub const RC_STATIC: i32 = 0;

/// The reference count of a freshly allocated, uniquely owned object.
pub const RC_UNIQUE: i32 = 1;

/// The scan count meaning "the scan count does not fit" (D-279). A
/// constructor's fields are then read from the layout table. A tuple holds
/// its arity as a tagged integer at offset 8, then its elements. A
/// closure, dictionary or descriptor keeps its first field (code word,
/// type descriptor or nominal descriptor) at offset 8 and holds the number
/// of the fields after the count as a tagged integer at offset 16, then
/// those fields (D-555).
pub const SCAN_ESCAPE: u8 = 255;

/// The largest scan count stored directly in the header.
pub const SCAN_MAX: u8 = 254;

/// The largest layout id (24 bits, D-279).
pub const LAYOUT_MAX: LayoutId = (1 << 24) - 1;

/// Bit position of the scan count in the header word.
pub const HEADER_SCAN_SHIFT: u32 = 32;

/// Bit position of the layout id in the header word.
pub const HEADER_LAYOUT_SHIFT: u32 = 40;

/// Byte offset of the 32-bit reference count inside an object. Only this
/// half of the header is written after allocation (M2's atomic shared
/// counts must never race a wider store).
pub const HEADER_RC_OFFSET: i32 = 0;

/// Byte offset of the scan count byte inside an object.
pub const HEADER_SCAN_OFFSET: i32 = 4;

/// Byte offset of the first field of an object.
pub const FIRST_FIELD_OFFSET: i32 = 8;

/// Encodes an object header (D-279): bits 0-31 hold the reference count,
/// bits 32-39 the scan count and bits 40-63 the layout id. Bits of `layout`
/// above [`LAYOUT_MAX`] are discarded; the layout table never hands out
/// such ids.
#[must_use]
#[expect(
    clippy::as_conversions,
    reason = "`i32 -> u32` reinterprets the count's bits, and `u32`/`u8 -> u64` widen losslessly; const fn cannot use `From`"
)]
pub const fn header(rc: i32, scan: u8, layout: LayoutId) -> u64 {
    let rc_bits = rc.cast_unsigned() as u64;
    let scan_bits = (scan as u64) << HEADER_SCAN_SHIFT;
    let layout_bits = ((layout & LAYOUT_MAX) as u64) << HEADER_LAYOUT_SHIFT;
    rc_bits | scan_bits | layout_bits
}

/// The reference count of a header word.
#[must_use]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "keeping the low 32 bits is the encoding of the count"
)]
pub const fn header_rc(header: u64) -> i32 {
    (header as u32).cast_signed()
}

/// The scan count of a header word.
#[must_use]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "the shift leaves the scan count in the low 8 bits"
)]
pub const fn header_scan(header: u64) -> u8 {
    (header >> HEADER_SCAN_SHIFT) as u8
}

/// The layout id of a header word.
#[must_use]
#[expect(
    clippy::as_conversions,
    reason = "the shift leaves the 24-bit layout id in the low bits"
)]
pub const fn header_layout(header: u64) -> LayoutId {
    (header >> HEADER_LAYOUT_SHIFT) as u32
}

/// The smallest `Int` a tagged word holds: -2^62.
pub const INT63_MIN: i64 = -(1 << 62);

/// The largest `Int` a tagged word holds: 2^62 - 1.
pub const INT63_MAX: i64 = (1 << 62) - 1;

/// Whether a word is a tagged integer (D-278).
#[must_use]
pub const fn is_tagged_int(word: Word) -> bool {
    word & TAG_INT != 0
}

/// Tags `n` as `(n << 1) | 1`, or `None` when `n` is outside 63 bits and
/// must be boxed instead (layout [`layout::BOXED_INT`]). Only such values
/// are boxed, so a small number is never also a pointer (D-278).
#[must_use]
pub const fn tag_int(n: i64) -> Option<Word> {
    if n < INT63_MIN || n > INT63_MAX {
        None
    } else {
        // In range, so the shift drops only a copy of the sign bit.
        Some(n.wrapping_shl(1).cast_unsigned() | TAG_INT)
    }
}

/// The integer of a tagged word: an arithmetic shift right by one.
#[must_use]
pub const fn untag_int(word: Word) -> i64 {
    word.cast_signed().wrapping_shr(1)
}

/// Fixed builtin layout ids (D-454). User constructors get ids from
/// [`layout::FIRST_USER`] on, in registration order.
pub mod layout {
    use super::LayoutId;

    /// Never written into a header.
    pub const INVALID: LayoutId = 0;
    /// `String`: header, `len`, `cap`, bytes (D-454).
    pub const STRING: LayoutId = 1;
    /// `Bytes` (F2); the same shape as `String`.
    pub const BYTES: LayoutId = 2;
    /// A boxed `Int` or `I64` outside 63 bits: one raw `i64`.
    pub const BOXED_INT: LayoutId = 3;
    /// A boxed `Float`: one raw `f64`.
    pub const BOXED_FLOAT: LayoutId = 4;
    /// A boxed `F32`: one raw `f32` in an 8-byte field.
    pub const BOXED_F32: LayoutId = 5;
    /// A closure: the code word `addr | 1` at offset 8, then the captures.
    pub const CLOSURE: LayoutId = 6;
    /// A tuple of uniform values; the arity is the scan count.
    pub const TUPLE: LayoutId = 7;
    /// A boxed `U64` outside 63 bits: one raw `u64`.
    pub const BOXED_U64: LayoutId = 8;
    /// A dictionary (D-524): header, the type's descriptor, one closure
    /// per interface function, then the embedded interfaces' dictionaries.
    pub const DICT: LayoutId = 9;
    /// A `dyn` value (D-524): header, the dictionary, the value.
    pub const DYN: LayoutId = 10;
    /// A type descriptor (D-524): header, the nominal descriptor, then the
    /// argument descriptors.
    pub const DESC: LayoutId = 11;
    /// A nominal descriptor (D-524): a static object per type with its
    /// printed name and module path as static strings.
    pub const DESC_NOMINAL: LayoutId = 12;
    /// The last id reserved for runtime kinds; 13 to 15 are unassigned.
    pub const RUNTIME_RESERVED_END: LayoutId = 15;
    /// `Nil`.
    pub const NIL: LayoutId = 16;
    /// `False`.
    pub const FALSE: LayoutId = 17;
    /// `True`.
    pub const TRUE: LayoutId = 18;
    /// `Less`.
    pub const LESS: LayoutId = 19;
    /// `Equal`.
    pub const EQUAL: LayoutId = 20;
    /// `Greater`.
    pub const GREATER: LayoutId = 21;
    /// `Some(value)`.
    pub const SOME: LayoutId = 22;
    /// `None`.
    pub const NONE: LayoutId = 23;
    /// `Ok(value)`.
    pub const OK: LayoutId = 24;
    /// `Err(error)`.
    pub const ERR: LayoutId = 25;
    /// The first id of list nodes (F3, D-524); the list runtime chooses
    /// within [`LIST_FIRST`]..=[`LIST_LAST`].
    pub const LIST_FIRST: LayoutId = 26;
    /// The last id of list nodes.
    pub const LIST_LAST: LayoutId = 31;
    /// The first id of map and set nodes (F4, D-524).
    pub const MAP_FIRST: LayoutId = 32;
    /// The last id of map and set nodes.
    pub const MAP_LAST: LayoutId = 39;
    /// The first id of the runtime-defined kinds registered with
    /// `layout::register_builtin` (list and map nodes, then 40 to 63
    /// reserved).
    pub const CUSTOM_FIRST: LayoutId = LIST_FIRST;
    /// The last id reserved for builtin collection nodes (F3, F4).
    pub const BUILTIN_RESERVED_END: LayoutId = 63;
    /// The first id handed out to user constructors.
    pub const FIRST_USER: LayoutId = 64;
}

/// String objects (D-454): the header (layout [`layout::STRING`], scan 0),
/// then `len` (UTF-8 bytes), `cap` (bytes allocated after the fixed part,
/// `cap >= len`), then the bytes. The allocation is `BYTES_OFFSET + cap`
/// rounded up to 8.
pub mod string {
    use super::{OBJECT_ALIGN, RC_STATIC, header, layout};

    /// Byte offset of `len: u64`.
    pub const LEN_OFFSET: i32 = 8;
    /// Byte offset of `cap: u64`.
    pub const CAP_OFFSET: i32 = 16;
    /// Byte offset of the first byte of text.
    pub const BYTES_OFFSET: i32 = 24;

    /// [`BYTES_OFFSET`] as a size.
    const FIXED_SIZE: u64 = 24;
    const _: () = assert!(BYTES_OFFSET == 24);

    /// The allocation size of a string with capacity `cap`, or `None` if it
    /// overflows.
    #[must_use]
    pub const fn alloc_size(cap: u64) -> Option<u64> {
        match FIXED_SIZE.checked_add(cap) {
            Some(size) => size.checked_next_multiple_of(OBJECT_ALIGN),
            None => None,
        }
    }

    /// The bytes of a static string object holding `text`: count
    /// [`RC_STATIC`], `cap == len`, little-endian, padded to a multiple of
    /// 8. Code generation emits this as a read-only data object aligned to
    /// 8; the runtime's tests read it back.
    #[must_use]
    pub fn static_image(text: &str) -> Vec<u8> {
        let len = u64::try_from(text.len()).unwrap_or(u64::MAX);
        let size = alloc_size(len).map_or(0, |size| usize::try_from(size).unwrap_or(0));
        let mut image = Vec::with_capacity(size);
        image.extend_from_slice(&header(RC_STATIC, 0, layout::STRING).to_le_bytes());
        image.extend_from_slice(&len.to_le_bytes());
        image.extend_from_slice(&len.to_le_bytes());
        image.extend_from_slice(text.as_bytes());
        image.resize(image.len().next_multiple_of(8), 0);
        image
    }
}

/// Byte offset of the execution context's `stack_limit: usize` (D-453).
/// The function-entry check is `sp < stack_limit →
/// cheby_rt_stack_slow(ctx, location)`.
pub const CTX_STACK_LIMIT: i32 = 0;

/// Byte offset of the execution context's `mode: u32`.
pub const CTX_MODE: i32 = 8;

/// `mode` bit: the entry is a constant evaluation, so externals and IO are
/// forbidden (§4.5, §13.9).
pub const CTX_MODE_CONST_EVAL: u32 = 1;

/// Exit statuses of a program (§10.10, D-182). Entry thunks for `main`
/// return [`status::SUCCESS`] or [`status::ERR`] as a raw `i64`.
pub mod status {
    /// `main` returned `Nil` or `Ok(Nil)`.
    pub const SUCCESS: i32 = 0;
    /// `main` returned `Err(e)`, or a write to standard output failed
    /// (D-306).
    pub const ERR: i32 = 1;
    /// The root fiber panicked (D-182).
    pub const PANIC: i32 = 101;
    /// Debug builds only: the program ended with live objects (D-458).
    pub const LEAK_CHECK: i32 = 102;
}

/// The kind of a panic, passed to `cheby_rt_panic` as a `u32`.
/// `cheby_mir::PanicKind` mirrors this numbering (D-449).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum PanicKind {
    /// `panic` (§11.6).
    Explicit = 0,
    /// `todo` (§11.6).
    Todo = 1,
    /// Integer overflow in arithmetic, negation or shifts (D-025, D-092).
    Overflow = 2,
    /// Integer division or remainder by zero (D-069).
    DivisionByZero = 3,
    /// A failed `assert` (D-068).
    Assert = 4,
    /// A failed `let assert` (D-096).
    LetAssert = 5,
    /// The fiber's stack limit was reached (§11.8.1).
    StackOverflow = 6,
    /// `==` or hashing reached a function value or a handle (D-233).
    EqualityOnFunction = 7,
    /// A shift by a negative amount or by the width or more (D-092).
    ShiftOutOfRange = 8,
    /// Constant evaluation needed a constant whose initializer is running
    /// (D-535). Internal: constant evaluation reports it as E0069, never
    /// with this message.
    ConstCycle = 9,
    /// Constant evaluation reached an external function or effect it may
    /// not use (D-537, D-532). Internal: reported as E0077.
    ForbiddenEffect = 10,
}

impl PanicKind {
    /// Every kind, in numbering order.
    pub const ALL: [Self; 11] = [
        Self::Explicit,
        Self::Todo,
        Self::Overflow,
        Self::DivisionByZero,
        Self::Assert,
        Self::LetAssert,
        Self::StackOverflow,
        Self::EqualityOnFunction,
        Self::ShiftOutOfRange,
        Self::ConstCycle,
        Self::ForbiddenEffect,
    ];

    /// The number passed at the ABI.
    #[must_use]
    #[expect(
        clippy::as_conversions,
        reason = "reading the discriminant of a `repr(u32)` enum"
    )]
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// The kind with the given number.
    #[must_use]
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::Explicit),
            1 => Some(Self::Todo),
            2 => Some(Self::Overflow),
            3 => Some(Self::DivisionByZero),
            4 => Some(Self::Assert),
            5 => Some(Self::LetAssert),
            6 => Some(Self::StackOverflow),
            7 => Some(Self::EqualityOnFunction),
            8 => Some(Self::ShiftOutOfRange),
            9 => Some(Self::ConstCycle),
            10 => Some(Self::ForbiddenEffect),
            _ => None,
        }
    }

    /// The message printed when the panic carries none (D-449).
    #[must_use]
    pub const fn default_message(self) -> &'static str {
        match self {
            Self::Explicit => "explicit panic",
            Self::Todo => "not implemented",
            Self::Overflow => "integer overflow",
            Self::DivisionByZero => "division by zero",
            Self::Assert => "assertion failed",
            Self::LetAssert => "`let assert` pattern did not match",
            Self::StackOverflow => "stack overflow",
            Self::EqualityOnFunction => "equality or hashing reached a function value or a handle",
            Self::ShiftOutOfRange => "shift amount out of range",
            Self::ConstCycle => "constant depends on itself",
            Self::ForbiddenEffect => "constant evaluation reached an effect",
        }
    }
}

/// Names of the C-ABI symbols generated code imports.
/// Runtime internals are `cheby_rt_*`; kernels are
/// `cheby_<module>_<function>`.
pub mod sym {
    use super::{LayoutId, layout};

    /// `uint64_t cheby_rt_alloc(ExecCtx *ctx, uint64_t size, uint64_t header)`.
    pub const ALLOC: &str = "cheby_rt_alloc";
    /// `void cheby_rt_dup(uint64_t value)`.
    pub const DUP: &str = "cheby_rt_dup";
    /// `void cheby_rt_drop(uint64_t value)`.
    pub const DROP: &str = "cheby_rt_drop";
    /// `_Noreturn void cheby_rt_panic(ExecCtx *ctx, uint32_t kind, uint64_t message, uint64_t location)`.
    pub const PANIC: &str = "cheby_rt_panic";
    /// `void cheby_rt_stack_slow(ExecCtx *ctx, uint64_t location)`.
    pub const STACK_SLOW: &str = "cheby_rt_stack_slow";
    /// Data: the static `Nil` object.
    pub const STATIC_NIL: &str = "cheby_rt_static_nil";
    /// Data: the static `False` object.
    pub const STATIC_FALSE: &str = "cheby_rt_static_false";
    /// Data: the static `True` object.
    pub const STATIC_TRUE: &str = "cheby_rt_static_true";
    /// Data: the static `Less` object.
    pub const STATIC_LESS: &str = "cheby_rt_static_less";
    /// Data: the static `Equal` object.
    pub const STATIC_EQUAL: &str = "cheby_rt_static_equal";
    /// Data: the static `Greater` object.
    pub const STATIC_GREATER: &str = "cheby_rt_static_greater";
    /// Data: the static `None` object.
    pub const STATIC_NONE: &str = "cheby_rt_static_none";
    /// `uint64_t cheby_rt_str_with_capacity(ExecCtx *ctx, int64_t cap)`:
    /// an empty unique `String` with room for `cap` bytes (a negative
    /// `cap` counts as 0). Result owned (D-520).
    pub const STR_WITH_CAPACITY: &str = "cheby_rt_str_with_capacity";
    /// `uint64_t cheby_rt_str_append(ExecCtx *ctx, uint64_t acc, uint64_t piece)`:
    /// `acc` followed by `piece`. Consumes `acc` (updated in place when it
    /// is unique and has room, D-316), borrows `piece`; result owned.
    pub const STR_APPEND: &str = "cheby_rt_str_append";
    /// `bool cheby_rt_str_eq(uint64_t a, uint64_t b)`: byte equality of two
    /// borrowed `String`s.
    pub const STR_EQ: &str = "cheby_rt_str_eq";
    /// `uint64_t cheby_rt_list_push(ExecCtx *ctx, uint64_t list, uint64_t elem)`:
    /// consumes both, in place when the list is unique (W1-E).
    pub const LIST_PUSH: &str = "cheby_rt_list_push";
    /// `uint64_t cheby_rt_list_append(ExecCtx *ctx, uint64_t a, uint64_t b)`:
    /// consumes `a`, borrows `b` (W1-E).
    pub const LIST_APPEND: &str = "cheby_rt_list_append";
    /// `int64_t cheby_rt_list_length(uint64_t list)`, borrowed (W1-E).
    pub const LIST_LENGTH: &str = "cheby_rt_list_length";
    /// `uint64_t cheby_rt_list_get(uint64_t list, int64_t index)`: the
    /// element, dup'd; the index is in range (W1-E).
    pub const LIST_GET: &str = "cheby_rt_list_get";
    /// `uint64_t cheby_rt_list_slice(ExecCtx *ctx, uint64_t list, int64_t start, int64_t end)`:
    /// an O(1) view, list borrowed, result owned (W1-E).
    pub const LIST_SLICE: &str = "cheby_rt_list_slice";
    /// Data: the static empty list (W1-E).
    pub const STATIC_EMPTY_LIST: &str = "cheby_rt_static_empty_list";
    /// `bool cheby_rt_equal(ExecCtx *ctx, uint64_t a, uint64_t b, uint64_t location)`:
    /// structural `==` (§3.13) of two borrowed uniform words; panics
    /// `EqualityOnFunction` (D-233) at `location`, a static `String` or 0
    /// for unknown (D-556).
    pub const EQUAL: &str = "cheby_rt_equal";
    /// `uint64_t cheby_rt_debug(ExecCtx *ctx, uint64_t value)`: the debug
    /// text (§3.13) of a borrowed uniform word, as an owned `String`.
    pub const DEBUG: &str = "cheby_rt_debug";
    /// `void cheby_rt_effect_check(ExecCtx *ctx, uint64_t symbol)`: panics
    /// `ForbiddenEffect` when the entry evaluates a constant and the
    /// borrowed `String` names a forbidden native symbol (W3-D).
    pub const EFFECT_CHECK: &str = "cheby_rt_effect_check";
    /// `uint64_t cheby_rt_box_i64(ExecCtx *ctx, int64_t value)`: the
    /// uniform word of an `Int`/`I64` (a `BoxedInt` outside 63 bits).
    pub const BOX_I64: &str = "cheby_rt_box_i64";
    /// `uint64_t cheby_rt_box_u64(ExecCtx *ctx, uint64_t value)`: the
    /// uniform word of a `U64` (a `BoxedU64` above 2^62 - 1).
    pub const BOX_U64: &str = "cheby_rt_box_u64";
    /// `uint64_t cheby_rt_box_f64(ExecCtx *ctx, double value)`: a
    /// `BoxedFloat`, owned.
    pub const BOX_F64: &str = "cheby_rt_box_f64";
    /// `uint64_t cheby_rt_box_f32(ExecCtx *ctx, float value)`: a
    /// `BoxedF32`, owned.
    pub const BOX_F32: &str = "cheby_rt_box_f32";
    /// `double cheby_rt_fmod_f64(double a, double b)`: `a % b` with the
    /// sign of `a` (D-527).
    pub const FMOD_F64: &str = "cheby_rt_fmod_f64";
    /// `float cheby_rt_fmod_f32(float a, float b)`: `a % b` with the sign
    /// of `a` (D-527).
    pub const FMOD_F32: &str = "cheby_rt_fmod_f32";
    /// `uint64_t cheby_rt_const_missing(ExecCtx *ctx, uint64_t slot_address)`:
    /// called by a `LoadConst` that found an empty slot; returns the
    /// constant's value, borrowed like the slot's (W3-D).
    pub const CONST_MISSING: &str = "cheby_rt_const_missing";
    /// Code bound to every unresolved native symbol of a `cheby check`
    /// build (D-532): takes no `ctx` and panics `ForbiddenEffect` through
    /// the current entry.
    pub const UNRESOLVED: &str = "cheby_rt_unresolved";
    /// `void cheby_io_print(uint64_t text)`, `text` borrowed.
    pub const IO_PRINT: &str = "cheby_io_print";
    /// `void cheby_io_println(uint64_t text)`, `text` borrowed.
    pub const IO_PRINTLN: &str = "cheby_io_println";
    /// `void cheby_io_eprint(uint64_t text)`, `text` borrowed.
    pub const IO_EPRINT: &str = "cheby_io_eprint";
    /// `void cheby_io_eprintln(uint64_t text)`, `text` borrowed.
    pub const IO_EPRINTLN: &str = "cheby_io_eprintln";

    /// The prefix of the builtin nominal descriptors `cheby_rt_desc_<name>`
    /// (D-523), defined by W3-B.
    pub const DESC_PREFIX: &str = "cheby_rt_desc_";

    /// The `<name>` of every builtin nominal descriptor
    /// `cheby_rt_desc_<name>` (plan §4.3.2). `tuple` takes its arity from
    /// its `Desc` arguments, and `fn` takes the parameter types, then the
    /// result.
    pub const DESC_NAMES: [&str; 23] = [
        "int", "float", "f32", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "bool", "nil",
        "string", "bytes", "list", "map", "set", "option", "result", "order", "tuple", "fn",
    ];

    /// Every runtime symbol that is not a kernel (the `cheby_rt_*` helpers
    /// and static objects), in the order `symbols()` lists them first.
    /// Kernels (`cheby_<module>_<function>`) follow in `symbols()`; they
    /// come from each kernel module's registration (plan §4.3.3).
    pub const ALL: [&str; 32] = [
        ALLOC,
        DUP,
        DROP,
        PANIC,
        STACK_SLOW,
        STATIC_NIL,
        STATIC_FALSE,
        STATIC_TRUE,
        STATIC_LESS,
        STATIC_EQUAL,
        STATIC_GREATER,
        STATIC_NONE,
        STR_WITH_CAPACITY,
        STR_APPEND,
        STR_EQ,
        LIST_PUSH,
        LIST_APPEND,
        LIST_LENGTH,
        LIST_GET,
        LIST_SLICE,
        STATIC_EMPTY_LIST,
        EQUAL,
        DEBUG,
        EFFECT_CHECK,
        BOX_I64,
        BOX_U64,
        BOX_F64,
        BOX_F32,
        FMOD_F64,
        FMOD_F32,
        CONST_MISSING,
        UNRESOLVED,
    ];

    /// The data symbol of the static object of a builtin nullary
    /// constructor (D-454).
    #[must_use]
    pub const fn static_ctor(id: LayoutId) -> Option<&'static str> {
        match id {
            layout::NIL => Some(STATIC_NIL),
            layout::FALSE => Some(STATIC_FALSE),
            layout::TRUE => Some(STATIC_TRUE),
            layout::LESS => Some(STATIC_LESS),
            layout::EQUAL => Some(STATIC_EQUAL),
            layout::GREATER => Some(STATIC_GREATER),
            layout::NONE => Some(STATIC_NONE),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_packs_fields_at_their_bit_positions() {
        assert_eq!(header(1, 0, 0), 1);
        assert_eq!(header(0, 1, 0), 1 << 32);
        assert_eq!(header(0, 0, 1), 1 << 40);
        assert_eq!(header(1, 2, layout::TUPLE), 0x0000_0702_0000_0001);
    }

    #[test]
    fn header_round_trips_at_the_boundaries() {
        let counts = [i32::MIN, -1, 0, 1, i32::MAX];
        let scans = [0, 1, SCAN_MAX, SCAN_ESCAPE];
        let layouts = [0, 1, layout::FIRST_USER, LAYOUT_MAX];
        for rc in counts {
            for scan in scans {
                for id in layouts {
                    let word = header(rc, scan, id);
                    assert_eq!(header_rc(word), rc);
                    assert_eq!(header_scan(word), scan);
                    assert_eq!(header_layout(word), id);
                }
            }
        }
    }

    #[test]
    fn negative_counts_do_not_leak_into_other_fields() {
        let word = header(-1, 0, 0);
        assert_eq!(word, 0xFFFF_FFFF);
        assert_eq!(header_scan(word), 0);
        assert_eq!(header_layout(word), 0);
    }

    #[test]
    fn layout_ids_above_24_bits_are_masked() {
        let word = header(0, 0, LAYOUT_MAX + 1);
        assert_eq!(header_layout(word), 0);
        assert_eq!(header_scan(word), 0);
        assert_eq!(header(0, 0, u32::MAX), header(0, 0, LAYOUT_MAX));
    }

    #[test]
    fn tagged_ints_cover_exactly_63_bits() {
        for n in [0, 1, -1, 42, -42, INT63_MIN, INT63_MAX] {
            assert!(tag_int(n).is_some_and(is_tagged_int));
            assert_eq!(tag_int(n).map(untag_int), Some(n));
        }
        assert_eq!(tag_int(0), Some(1));
        assert_eq!(tag_int(-1), Some(u64::MAX));
        assert_eq!(tag_int(INT63_MAX), Some(0x7FFF_FFFF_FFFF_FFFF));
        assert_eq!(tag_int(INT63_MIN), Some(0x8000_0000_0000_0001));
    }

    #[test]
    fn ints_outside_63_bits_are_not_tagged() {
        for n in [INT63_MAX + 1, INT63_MIN - 1, i64::MAX, i64::MIN] {
            assert_eq!(tag_int(n), None);
        }
    }

    #[test]
    fn pointers_are_not_tagged() {
        assert!(!is_tagged_int(0x1000));
        assert!(!is_tagged_int(8));
    }

    #[test]
    fn panic_kinds_round_trip() {
        for (index, kind) in PanicKind::ALL.into_iter().enumerate() {
            assert_eq!(usize::try_from(kind.code()).ok(), Some(index));
            assert_eq!(PanicKind::from_code(kind.code()), Some(kind));
        }
        assert_eq!(PanicKind::from_code(11), None);
        assert_eq!(
            PanicKind::ConstCycle.default_message(),
            "constant depends on itself"
        );
        assert_eq!(
            PanicKind::ForbiddenEffect.default_message(),
            "constant evaluation reached an effect"
        );
    }

    #[test]
    fn static_string_image_has_the_string_layout() {
        let image = static_image_words("Hello");
        assert_eq!(image.len(), 4);
        assert_eq!(
            image.first().copied(),
            Some(header(RC_STATIC, 0, layout::STRING))
        );
        assert_eq!(image.get(1).copied(), Some(5));
        assert_eq!(image.get(2).copied(), Some(5));
        assert_eq!(
            image.get(3).copied(),
            Some(u64::from_le_bytes(*b"Hello\0\0\0"))
        );
        assert_eq!(string::static_image("").len(), 24);
        assert_eq!(string::static_image("12345678").len(), 32);
    }

    fn static_image_words(text: &str) -> Vec<u64> {
        string::static_image(text)
            .as_chunks::<8>()
            .0
            .iter()
            .copied()
            .map(u64::from_le_bytes)
            .collect()
    }

    #[test]
    fn string_alloc_size_rounds_up_and_detects_overflow() {
        assert_eq!(string::alloc_size(0), Some(24));
        assert_eq!(string::alloc_size(1), Some(32));
        assert_eq!(string::alloc_size(8), Some(32));
        assert_eq!(string::alloc_size(u64::MAX), None);
        assert_eq!(string::alloc_size(u64::MAX - 30), None);
    }

    #[test]
    fn layout_ids_follow_the_plan() {
        assert_eq!(
            [
                layout::DICT,
                layout::DYN,
                layout::DESC,
                layout::DESC_NOMINAL
            ],
            [9, 10, 11, 12]
        );
        assert_eq!((layout::LIST_FIRST, layout::LIST_LAST), (26, 31));
        assert_eq!((layout::MAP_FIRST, layout::MAP_LAST), (32, 39));
        assert_eq!(layout::CUSTOM_FIRST, 26);
        assert_eq!(sym::DESC_NAMES.len(), 23);
        assert!(sym::ALL.iter().all(|name| name.starts_with("cheby_rt_")));
    }

    #[test]
    fn static_ctor_symbols_cover_the_nullary_builtins() {
        assert_eq!(sym::static_ctor(layout::NIL), Some(sym::STATIC_NIL));
        assert_eq!(sym::static_ctor(layout::NONE), Some(sym::STATIC_NONE));
        assert_eq!(sym::static_ctor(layout::SOME), None);
        assert_eq!(sym::static_ctor(layout::STRING), None);
    }
}
