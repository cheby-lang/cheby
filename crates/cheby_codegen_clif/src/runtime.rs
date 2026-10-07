//! The runtime functions and data generated code imports,
//! with their C signatures. Names come from `cheby_runtime::abi::sym`.

use cheby_mir::CType;
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
}

impl RuntimeFunc {
    /// Every runtime function, in declaration order.
    pub const ALL: [Self; 5] = [
        Self::Alloc,
        Self::Dup,
        Self::Drop,
        Self::Panic,
        Self::StackSlow,
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
        }
    }

    /// The C parameter and result types.
    #[must_use]
    pub const fn ctypes(self) -> (&'static [CType], CType) {
        match self {
            Self::Alloc => (&[CType::Ptr, CType::U64, CType::U64], CType::U64),
            Self::Dup | Self::Drop => (&[CType::Ptr], CType::Void),
            Self::Panic => (
                &[CType::Ptr, CType::U32, CType::Ptr, CType::Ptr],
                CType::Void,
            ),
            Self::StackSlow => (&[CType::Ptr, CType::Ptr], CType::Void),
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
        }
    }
}

/// The data symbols of the builtin static objects generated code may
/// reference, in declaration order.
pub const STATIC_DATA: [&str; 7] = [
    sym::STATIC_NIL,
    sym::STATIC_FALSE,
    sym::STATIC_TRUE,
    sym::STATIC_LESS,
    sym::STATIC_EQUAL,
    sym::STATIC_GREATER,
    sym::STATIC_NONE,
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
