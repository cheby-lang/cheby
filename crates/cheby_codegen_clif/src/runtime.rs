//! The runtime functions and data generated code imports, with their C
//! signatures (plan §4.3.1, D-520). Names come from
//! `cheby_runtime::abi::sym`; this crate never calls the runtime.

use cheby_mir::{CType, RuntimeFn};
use cheby_runtime::abi::sym;

/// A runtime function generated code calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RuntimeFunc {
    /// `uint64_t cheby_rt_alloc(ExecCtx *ctx, uint64_t size, uint64_t header)`.
    Alloc,
    /// `void cheby_rt_dup(uint64_t value)`.
    Dup,
    /// `void cheby_rt_drop(uint64_t value)`.
    Drop,
    /// `_Noreturn void cheby_rt_panic(ExecCtx *ctx, uint32_t kind, uint64_t message, uint64_t location)`.
    Panic,
    /// `void cheby_rt_stack_slow(ExecCtx *ctx, uint64_t location)`.
    StackSlow,
    /// `uint64_t cheby_rt_box_i64(ExecCtx *ctx, int64_t value)`.
    BoxI64,
    /// `uint64_t cheby_rt_box_u64(ExecCtx *ctx, uint64_t value)`.
    BoxU64,
    /// `uint64_t cheby_rt_box_f64(ExecCtx *ctx, double value)`.
    BoxF64,
    /// `uint64_t cheby_rt_box_f32(ExecCtx *ctx, float value)`.
    BoxF32,
    /// `double cheby_rt_fmod_f64(double a, double b)`.
    FmodF64,
    /// `float cheby_rt_fmod_f32(float a, float b)`.
    FmodF32,
    /// `uint64_t cheby_rt_const_missing(ExecCtx *ctx, uint64_t slot_address)`.
    ConstMissing,
    /// `uint64_t cheby_rt_str_with_capacity(ExecCtx *ctx, int64_t cap)`.
    StrWithCapacity,
    /// `uint64_t cheby_rt_str_append(ExecCtx *ctx, uint64_t acc, uint64_t piece)`.
    StrAppend,
    /// `bool cheby_rt_str_eq(uint64_t a, uint64_t b)`.
    StrEq,
    /// `uint64_t cheby_rt_list_push(ExecCtx *ctx, uint64_t list, uint64_t elem)`.
    ListPush,
    /// `uint64_t cheby_rt_list_append(ExecCtx *ctx, uint64_t a, uint64_t b)`.
    ListAppend,
    /// `int64_t cheby_rt_list_length(uint64_t list)`.
    ListLength,
    /// `uint64_t cheby_rt_list_get(uint64_t list, int64_t index)`.
    ListGet,
    /// `uint64_t cheby_rt_list_slice(ExecCtx *ctx, uint64_t list, int64_t start, int64_t end)`.
    ListSlice,
    /// `bool cheby_rt_equal(ExecCtx *ctx, uint64_t a, uint64_t b, uint64_t location)`.
    Equal,
    /// `uint64_t cheby_rt_debug(ExecCtx *ctx, uint64_t value)`.
    Debug,
    /// `void cheby_rt_effect_check(ExecCtx *ctx, uint64_t symbol)`.
    EffectCheck,
}

impl RuntimeFunc {
    /// Every runtime function, in declaration order ([`Self::index`]).
    pub const ALL: [Self; 23] = [
        Self::Alloc,
        Self::Dup,
        Self::Drop,
        Self::Panic,
        Self::StackSlow,
        Self::BoxI64,
        Self::BoxU64,
        Self::BoxF64,
        Self::BoxF32,
        Self::FmodF64,
        Self::FmodF32,
        Self::ConstMissing,
        Self::StrWithCapacity,
        Self::StrAppend,
        Self::StrEq,
        Self::ListPush,
        Self::ListAppend,
        Self::ListLength,
        Self::ListGet,
        Self::ListSlice,
        Self::Equal,
        Self::Debug,
        Self::EffectCheck,
    ];

    /// The symbol.
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Alloc => sym::ALLOC,
            Self::Dup => sym::DUP,
            Self::Drop => sym::DROP,
            Self::Panic => sym::PANIC,
            Self::StackSlow => sym::STACK_SLOW,
            Self::BoxI64 => sym::BOX_I64,
            Self::BoxU64 => sym::BOX_U64,
            Self::BoxF64 => sym::BOX_F64,
            Self::BoxF32 => sym::BOX_F32,
            Self::FmodF64 => sym::FMOD_F64,
            Self::FmodF32 => sym::FMOD_F32,
            Self::ConstMissing => sym::CONST_MISSING,
            Self::StrWithCapacity => sym::STR_WITH_CAPACITY,
            Self::StrAppend => sym::STR_APPEND,
            Self::StrEq => sym::STR_EQ,
            Self::ListPush => sym::LIST_PUSH,
            Self::ListAppend => sym::LIST_APPEND,
            Self::ListLength => sym::LIST_LENGTH,
            Self::ListGet => sym::LIST_GET,
            Self::ListSlice => sym::LIST_SLICE,
            Self::Equal => sym::EQUAL,
            Self::Debug => sym::DEBUG,
            Self::EffectCheck => sym::EFFECT_CHECK,
        }
    }

    /// The C parameter and result types. `ctx` is a [`CType::Ptr`], and so
    /// is every uniform word.
    #[must_use]
    pub const fn ctypes(self) -> (&'static [CType], CType) {
        use CType::{Bool, F32, F64, I64, Ptr, U32, U64, Void};
        match self {
            Self::Alloc | Self::StrAppend | Self::ListPush | Self::ListAppend => {
                (&[Ptr, U64, U64], U64)
            }
            Self::Dup | Self::Drop => (&[Ptr], Void),
            Self::Panic => (&[Ptr, U32, Ptr, Ptr], Void),
            Self::StackSlow | Self::EffectCheck => (&[Ptr, Ptr], Void),
            Self::BoxI64 | Self::StrWithCapacity => (&[Ptr, I64], U64),
            Self::BoxU64 | Self::ConstMissing | Self::Debug => (&[Ptr, U64], U64),
            Self::BoxF64 => (&[Ptr, F64], U64),
            Self::BoxF32 => (&[Ptr, F32], U64),
            Self::FmodF64 => (&[F64, F64], F64),
            Self::FmodF32 => (&[F32, F32], F32),
            Self::StrEq => (&[U64, U64], Bool),
            Self::Equal => (&[Ptr, U64, U64, U64], Bool),
            Self::ListLength => (&[U64], I64),
            Self::ListGet => (&[U64, I64], U64),
            Self::ListSlice => (&[Ptr, U64, I64, I64], U64),
        }
    }

    /// The index in [`Self::ALL`].
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Alloc => 0,
            Self::Dup => 1,
            Self::Drop => 2,
            Self::Panic => 3,
            Self::StackSlow => 4,
            Self::BoxI64 => 5,
            Self::BoxU64 => 6,
            Self::BoxF64 => 7,
            Self::BoxF32 => 8,
            Self::FmodF64 => 9,
            Self::FmodF32 => 10,
            Self::ConstMissing => 11,
            Self::StrWithCapacity => 12,
            Self::StrAppend => 13,
            Self::StrEq => 14,
            Self::ListPush => 15,
            Self::ListAppend => 16,
            Self::ListLength => 17,
            Self::ListGet => 18,
            Self::ListSlice => 19,
            Self::Equal => 20,
            Self::Debug => 21,
            Self::EffectCheck => 22,
        }
    }

    /// The runtime function a MIR helper calls, and whether it takes `ctx`
    /// first (plan §4.3.1: helpers that allocate, may panic or may call
    /// back into Cheby code take it).
    #[must_use]
    pub const fn of(helper: RuntimeFn) -> (Self, bool) {
        match helper {
            RuntimeFn::StrWithCapacity => (Self::StrWithCapacity, true),
            RuntimeFn::StrAppend => (Self::StrAppend, true),
            RuntimeFn::StrEq => (Self::StrEq, false),
            RuntimeFn::ListPush => (Self::ListPush, true),
            RuntimeFn::ListAppend => (Self::ListAppend, true),
            RuntimeFn::ListLength => (Self::ListLength, false),
            RuntimeFn::ListGet => (Self::ListGet, false),
            RuntimeFn::ListSlice => (Self::ListSlice, true),
            RuntimeFn::Equal => (Self::Equal, true),
            RuntimeFn::Debug => (Self::Debug, true),
            RuntimeFn::EffectCheck => (Self::EffectCheck, true),
        }
    }
}

/// The data symbols of the runtime's static objects generated code may
/// reference, in declaration order.
pub const STATIC_DATA: [&str; 8] = [
    sym::STATIC_NIL,
    sym::STATIC_FALSE,
    sym::STATIC_TRUE,
    sym::STATIC_LESS,
    sym::STATIC_EQUAL,
    sym::STATIC_GREATER,
    sym::STATIC_NONE,
    sym::STATIC_EMPTY_LIST,
];

/// Every runtime symbol generated code may import, functions then data.
/// The JIT checks that the runtime's symbol registry lists each one.
#[must_use]
pub fn imported_symbols() -> Vec<&'static str> {
    RuntimeFunc::ALL
        .iter()
        .map(|f| f.symbol())
        .chain(STATIC_DATA)
        .collect()
}
