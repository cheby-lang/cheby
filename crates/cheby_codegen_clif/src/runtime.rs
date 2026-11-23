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
    /// `void cheby_rt_drop(ExecCtx *ctx, uint64_t value)`: the free path
    /// reads the block cache through `ctx` (D-772).
    Drop,
    /// `void cheby_rt_panic(ExecCtx *ctx, uint32_t kind, uint64_t message, uint64_t location)`:
    /// records the panic, sets the flag and returns (D-733).
    Panic,
    /// `void cheby_rt_stack_slow(ExecCtx *ctx, uint64_t location)`, which
    /// saves every register it uses ([`Self::preserves_all`]).
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
    /// `uint64_t cheby_rt_list_slice_owned(ExecCtx *ctx, uint64_t list, int64_t start, int64_t end)`.
    ListSliceOwned,
    /// `void cheby_rt_stack_slow_frame(ExecCtx *ctx, uint64_t location, uint64_t frame_size)`:
    /// the slow path of a large function's entry check (D-679, D-743).
    StackSlowFrame,
    /// `uint64_t cheby_rt_scope_open(ExecCtx *ctx, bool timed, int64_t seconds, int64_t nanoseconds)`
    /// (D-693).
    ScopeOpen,
    /// `bool cheby_rt_scope_close(ExecCtx *ctx, uint64_t scope, bool unwound)` (D-693).
    ScopeClose,
    /// `void cheby_rt_stack_slow_unwind(ExecCtx *ctx, uint64_t location, uint64_t p0,
    /// uint64_t p1, uint64_t p2, uint64_t p3)`: the entry check's slow path that releases up
    /// to four counted parameters itself, 0 for each one the function lacks, when it starts
    /// an unwinding, which the caller reads from the flag (D-775). Six arguments, so all
    /// travel in registers on System V x86-64 too.
    StackSlowUnwind,
    /// `void cheby_rt_stack_slow_frame_unwind(ExecCtx *ctx, uint64_t location, uint64_t p0,
    /// uint64_t p1, uint64_t p2, uint64_t p3)`: the same for a large function's outer entry
    /// (D-743, D-775), which writes its frame size at `ctx` + `CTX_FRAME_SIZE` right before
    /// the call.
    StackSlowFrameUnwind,
    /// `uint64_t cheby_rt_blocking_call(ExecCtx *ctx, void (*shim)(const uint64_t *, uint64_t *),
    /// const uint64_t *args, uintptr_t nargs)`: runs an `@blocking` call's shim on the blocking
    /// pool and returns its result word unchanged; a cancellation pending after the return
    /// raises through the flag (D-683, D-818).
    BlockingCall,
    /// `void cheby_rt_release_fields(ExecCtx *ctx, uint64_t obj)`: the hit
    /// path of an inline `drop_reuse` (D-716).
    ReleaseFields,
    /// `void cheby_rt_reuse_hit(void)`: counts a reuse hit, called only
    /// while the counting flag is not off (D-768).
    ReuseHit,
    /// `uint64_t cheby_rt_alloc_miss(ExecCtx *ctx, uint64_t size, uint64_t header)`:
    /// `Alloc` for an `alloc_reuse` whose token is 0, counting the miss.
    AllocMiss,
    /// `uint64_t cheby_rt_drop_reuse_checked(ExecCtx *ctx, uint64_t value, uint64_t size)`:
    /// `drop_reuse` under `checked_rc`.
    DropReuseChecked,
    /// `uint64_t cheby_rt_alloc_reuse_checked(ExecCtx *ctx, uint64_t token, uint64_t size,
    /// uint64_t header)`: `alloc_reuse` under `checked_rc`; 0 when it raised.
    AllocReuseChecked,
    /// `void cheby_rt_free_unique(ExecCtx *ctx, uint64_t obj, uint64_t size)`: the hit
    /// path of a drop specialized by constructor frees the block, of the
    /// layout's allocation size, whose fields it released or kept (D-716,
    /// D-850).
    FreeUnique,
    /// `uint64_t cheby_rt_list_uncons(ExecCtx *ctx, uint64_t list, int64_t at, int64_t start,
    /// int64_t end, uint64_t *head)`.
    ListUncons,
    /// `uint64_t cheby_rt_list_uncons_owned(ExecCtx *ctx, uint64_t list, int64_t at,
    /// int64_t start, int64_t end, uint64_t *head)`.
    ListUnconsOwned,
    /// `uint64_t cheby_rt_list_prepend(ExecCtx *ctx, uint64_t list, uint64_t elem)`.
    ListPrepend,
    /// `uint64_t cheby_rt_map_find(ExecCtx *ctx, uint64_t map, uint64_t key)`.
    MapFind,
    /// `uint64_t cheby_rt_map_value_at(uint64_t address)`.
    MapValueAt,
    /// `uint64_t cheby_rt_list_set(ExecCtx *ctx, uint64_t list, int64_t index, uint64_t item)`.
    ListSet,
    /// `void cheby_rt_alloc_slow(ExecCtx *ctx, uint64_t size, uint64_t header)`: what an
    /// inline allocation does not serve; leaves the object, or 0 when it raised, at
    /// `CTX_ALLOC_RESULT` (`crate::alloc_inline`).
    AllocSlow,
    /// `void cheby_rt_alloc_miss_slow(ExecCtx *ctx, uint64_t size, uint64_t header)`:
    /// [`Self::AllocSlow`] for an `alloc_reuse` whose token is 0.
    AllocMissSlow,
    /// `void cheby_rt_free_unique_slow(ExecCtx *ctx, uint64_t obj, uint64_t size)`: what an
    /// inline sized free does not serve.
    FreeUniqueSlow,
    /// `void cheby_rt_drop_saving(ExecCtx *ctx, uint64_t value)`: `Drop`, for the cold
    /// drop of a shared object on the paths of a reuse and of a specialized drop.
    DropSaving,
    /// `uint64_t cheby_rt_map_upsert(ExecCtx *ctx, uint64_t map, uint64_t key, uint64_t *out)`.
    MapUpsert,
    /// `uint64_t cheby_rt_map_slot_take(uint64_t address)`.
    MapSlotTake,
    /// `uint64_t cheby_rt_map_upsert_found(ExecCtx *ctx, uint64_t address)`.
    MapUpsertFound,
    /// `uint64_t cheby_rt_map_upsert_store(ExecCtx *ctx, uint64_t map, uint64_t address,
    /// uint64_t key, uint64_t value)`.
    MapUpsertStore,
    /// `uint64_t cheby_rt_sort_start(ExecCtx *ctx, uint64_t xs)`.
    SortStart,
    /// `uint64_t cheby_rt_heap_list_start(ExecCtx *ctx, uint64_t items, bool max, uint64_t by)`.
    HeapListStart,
    /// `uint64_t cheby_rt_buffer_step(uint64_t state, int64_t tag)`.
    BufferStep,
    /// `uint64_t cheby_rt_buffer_finish(ExecCtx *ctx, uint64_t state, uint64_t source)`.
    BufferFinish,
    /// `uint64_t cheby_rt_heap_push(ExecCtx *ctx, uint64_t items, uint64_t item, bool max,
    /// uint64_t by, uint64_t *slot)`.
    HeapPush,
    /// `uint64_t cheby_rt_heap_pop(ExecCtx *ctx, uint64_t items, bool max, uint64_t by,
    /// uint64_t *slot)`.
    HeapPop,
    /// `uint64_t cheby_rt_heap_heapify(ExecCtx *ctx, uint64_t items, bool max, uint64_t by,
    /// uint64_t *slot)`.
    HeapHeapify,
    /// `uint64_t cheby_rt_heap_step(ExecCtx *ctx, uint64_t *slot, int64_t tag)`.
    HeapStep,
}

impl RuntimeFunc {
    /// Every runtime function, in declaration order ([`Self::index`]).
    pub const ALL: [Self; 58] = [
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
        Self::ListSliceOwned,
        Self::StackSlowFrame,
        Self::ScopeOpen,
        Self::ScopeClose,
        Self::StackSlowUnwind,
        Self::StackSlowFrameUnwind,
        Self::BlockingCall,
        Self::ReleaseFields,
        Self::ReuseHit,
        Self::AllocMiss,
        Self::DropReuseChecked,
        Self::AllocReuseChecked,
        Self::FreeUnique,
        Self::ListUncons,
        Self::ListUnconsOwned,
        Self::ListPrepend,
        Self::MapFind,
        Self::MapValueAt,
        Self::ListSet,
        Self::AllocSlow,
        Self::AllocMissSlow,
        Self::FreeUniqueSlow,
        Self::DropSaving,
        Self::MapUpsert,
        Self::MapSlotTake,
        Self::MapUpsertFound,
        Self::MapUpsertStore,
        Self::SortStart,
        Self::HeapListStart,
        Self::BufferStep,
        Self::BufferFinish,
        Self::HeapPush,
        Self::HeapPop,
        Self::HeapHeapify,
        Self::HeapStep,
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
            Self::ListSliceOwned => sym::LIST_SLICE_OWNED,
            Self::StackSlowFrame => sym::STACK_SLOW_FRAME,
            Self::ScopeOpen => sym::SCOPE_OPEN,
            Self::ScopeClose => sym::SCOPE_CLOSE,
            Self::StackSlowUnwind => sym::STACK_SLOW_UNWIND,
            Self::StackSlowFrameUnwind => sym::STACK_SLOW_FRAME_UNWIND,
            Self::BlockingCall => sym::BLOCKING_CALL,
            Self::ReleaseFields => sym::RELEASE_FIELDS,
            Self::ReuseHit => sym::REUSE_HIT,
            Self::AllocMiss => sym::ALLOC_MISS,
            Self::DropReuseChecked => sym::DROP_REUSE_CHECKED,
            Self::AllocReuseChecked => sym::ALLOC_REUSE_CHECKED,
            Self::FreeUnique => sym::FREE_UNIQUE,
            Self::ListUncons => sym::LIST_UNCONS,
            Self::ListUnconsOwned => sym::LIST_UNCONS_OWNED,
            Self::ListPrepend => sym::LIST_PREPEND,
            Self::MapFind => sym::MAP_FIND,
            Self::MapValueAt => sym::MAP_VALUE_AT,
            Self::ListSet => sym::LIST_SET,
            Self::AllocSlow => sym::ALLOC_SLOW,
            Self::AllocMissSlow => sym::ALLOC_MISS_SLOW,
            Self::FreeUniqueSlow => sym::FREE_UNIQUE_SLOW,
            Self::DropSaving => sym::DROP_SAVING,
            Self::MapUpsert => sym::MAP_UPSERT,
            Self::MapSlotTake => sym::MAP_SLOT_TAKE,
            Self::MapUpsertFound => sym::MAP_UPSERT_FOUND,
            Self::MapUpsertStore => sym::MAP_UPSERT_STORE,
            Self::SortStart => sym::SORT_START,
            Self::HeapListStart => sym::HEAP_LIST_START,
            Self::BufferStep => sym::BUFFER_STEP,
            Self::BufferFinish => sym::BUFFER_FINISH,
            Self::HeapPush => sym::HEAP_PUSH,
            Self::HeapPop => sym::HEAP_POP,
            Self::HeapHeapify => sym::HEAP_HEAPIFY,
            Self::HeapStep => sym::HEAP_STEP,
        }
    }

    /// The C parameter and result types. `ctx` is a [`CType::Ptr`], and so
    /// is every uniform word.
    #[must_use]
    pub const fn ctypes(self) -> (&'static [CType], CType) {
        use CType::{Bool, F32, F64, I64, Ptr, U32, U64, Void};
        match self {
            Self::Alloc
            | Self::StrAppend
            | Self::ListPush
            | Self::ListPrepend
            | Self::ListAppend
            | Self::MapFind
            | Self::AllocMiss
            | Self::DropReuseChecked
            | Self::BufferFinish => (&[Ptr, U64, U64], U64),
            Self::ReleaseFields => (&[Ptr, U64], Void),
            Self::FreeUnique | Self::AllocSlow | Self::AllocMissSlow | Self::FreeUniqueSlow => {
                (&[Ptr, U64, U64], Void)
            }
            Self::ReuseHit => (&[], Void),
            Self::AllocReuseChecked => (&[Ptr, U64, U64, U64], U64),
            Self::Dup => (&[Ptr], Void),
            Self::Panic => (&[Ptr, U32, Ptr, Ptr], Void),
            Self::Drop | Self::StackSlow | Self::EffectCheck | Self::DropSaving => {
                (&[Ptr, Ptr], Void)
            }
            Self::BoxI64 | Self::StrWithCapacity => (&[Ptr, I64], U64),
            Self::BoxU64
            | Self::ConstMissing
            | Self::Debug
            | Self::MapUpsertFound
            | Self::SortStart => (&[Ptr, U64], U64),
            Self::HeapListStart => (&[Ptr, U64, Bool, U64], U64),

            Self::HeapPush => (&[Ptr, U64, U64, Bool, U64, Ptr], U64),
            Self::HeapPop | Self::HeapHeapify => (&[Ptr, U64, Bool, U64, Ptr], U64),
            Self::HeapStep => (&[Ptr, Ptr, I64], U64),
            Self::BoxF64 => (&[Ptr, F64], U64),
            Self::BoxF32 => (&[Ptr, F32], U64),
            Self::FmodF64 => (&[F64, F64], F64),
            Self::FmodF32 => (&[F32, F32], F32),
            Self::StrEq => (&[U64, U64], Bool),
            Self::Equal => (&[Ptr, U64, U64, U64], Bool),
            Self::ListLength => (&[U64], I64),
            Self::ListGet | Self::BufferStep => (&[U64, I64], U64),
            Self::MapValueAt | Self::MapSlotTake => (&[U64], U64),
            Self::MapUpsertStore => (&[Ptr, U64, U64, U64, U64], U64),
            Self::MapUpsert => (&[Ptr, U64, U64, Ptr], U64),
            Self::ListSet => (&[Ptr, U64, I64, U64], U64),
            Self::ListSlice | Self::ListSliceOwned => (&[Ptr, U64, I64, I64], U64),
            Self::StackSlowFrame => (&[Ptr, Ptr, U64], Void),
            Self::ScopeOpen => (&[Ptr, Bool, I64, I64], U64),
            Self::ScopeClose => (&[Ptr, U64, Bool], Bool),
            Self::StackSlowUnwind | Self::StackSlowFrameUnwind => {
                (&[Ptr, Ptr, Ptr, Ptr, Ptr, Ptr], Void)
            }
            Self::BlockingCall => (&[Ptr, Ptr, Ptr, U64], U64),
            Self::ListUncons | Self::ListUnconsOwned => (&[Ptr, U64, I64, I64, I64, Ptr], U64),
        }
    }

    /// Whether the function is one of the runtime's saving stubs, which
    /// save every register generated code may keep a value in and load them
    /// back before they return (`cheby_runtime::panic`): the entry check's
    /// slow paths, the slow paths of inline allocations and sized frees
    /// (`crate::alloc_inline`), the cold drop of a shared object on the
    /// paths of a reuse and the count of a reuse hit. On a target whose
    /// runtime has those stubs ([`crate::decls::Decls::saving_slow_paths`]),
    /// generated code calls them with Cranelift's `preserve_all`
    /// convention, so the call clobbers nothing, and the values that live
    /// across the entry check stay in registers on the fast path: in a
    /// self tail call's loop, every value it carries. They return no
    /// result, as `preserve_all` allows none.
    #[must_use]
    pub const fn preserves_all(self) -> bool {
        matches!(
            self,
            Self::StackSlow
                | Self::StackSlowFrame
                | Self::StackSlowUnwind
                | Self::StackSlowFrameUnwind
                | Self::ReuseHit
                | Self::AllocSlow
                | Self::AllocMissSlow
                | Self::FreeUniqueSlow
                | Self::DropSaving
        )
    }

    /// Whether the function is declared only when a unit first uses it.
    /// The functions M2 added are, so that the function ids, and with
    /// them the CLIF, of code that does not use them stay as before.
    #[must_use]
    pub const fn on_demand(self) -> bool {
        matches!(
            self,
            Self::StackSlowFrame
                | Self::ScopeOpen
                | Self::ScopeClose
                | Self::StackSlowUnwind
                | Self::StackSlowFrameUnwind
                | Self::BlockingCall
                | Self::ReleaseFields
                | Self::ReuseHit
                | Self::AllocMiss
                | Self::DropReuseChecked
                | Self::AllocReuseChecked
                | Self::FreeUnique
                | Self::ListUncons
                | Self::ListUnconsOwned
                | Self::ListPrepend
                | Self::MapFind
                | Self::MapValueAt
                | Self::ListSet
                | Self::AllocSlow
                | Self::AllocMissSlow
                | Self::FreeUniqueSlow
                | Self::DropSaving
                | Self::MapUpsert
                | Self::MapSlotTake
                | Self::MapUpsertFound
                | Self::MapUpsertStore
                | Self::SortStart
                | Self::HeapListStart
                | Self::BufferStep
                | Self::BufferFinish
                | Self::HeapPush
                | Self::HeapPop
                | Self::HeapHeapify
                | Self::HeapStep
        )
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
            Self::ListSliceOwned => 23,
            Self::StackSlowFrame => 24,
            Self::ScopeOpen => 25,
            Self::ScopeClose => 26,
            Self::StackSlowUnwind => 27,
            Self::StackSlowFrameUnwind => 28,
            Self::BlockingCall => 29,
            Self::ReleaseFields => 30,
            Self::ReuseHit => 31,
            Self::AllocMiss => 32,
            Self::DropReuseChecked => 33,
            Self::AllocReuseChecked => 34,
            Self::FreeUnique => 35,
            Self::ListUncons => 36,
            Self::ListUnconsOwned => 37,
            Self::ListPrepend => 38,
            Self::MapFind => 39,
            Self::MapValueAt => 40,
            Self::ListSet => 41,
            Self::AllocSlow => 42,
            Self::AllocMissSlow => 43,
            Self::FreeUniqueSlow => 44,
            Self::DropSaving => 45,
            Self::MapUpsert => 46,
            Self::MapSlotTake => 47,
            Self::MapUpsertFound => 48,
            Self::MapUpsertStore => 49,
            Self::SortStart => 50,
            Self::HeapListStart => 51,
            Self::BufferStep => 52,
            Self::BufferFinish => 53,
            Self::HeapPush => 54,
            Self::HeapPop => 55,
            Self::HeapHeapify => 56,
            Self::HeapStep => 57,
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
            // A `Bytes`'s bounds test and byte are loads (`values.rs`), as a
            // list's length is: they call nothing, and `list_length` is
            // always declared.
            RuntimeFn::ListLength | RuntimeFn::BytesInRange | RuntimeFn::BytesByte => {
                (Self::ListLength, false)
            }
            // A read of the element as a raw number calls the read of its
            // word, where it calls anything (`list_index`).
            RuntimeFn::ListGet | RuntimeFn::ListGetFloat | RuntimeFn::ListGetF32 => {
                (Self::ListGet, false)
            }
            RuntimeFn::ListSlice => (Self::ListSlice, true),
            RuntimeFn::ListSliceOwned => (Self::ListSliceOwned, true),
            RuntimeFn::Equal => (Self::Equal, true),
            RuntimeFn::Debug => (Self::Debug, true),
            RuntimeFn::EffectCheck => (Self::EffectCheck, true),
            RuntimeFn::ScopeOpen => (Self::ScopeOpen, true),
            RuntimeFn::ScopeClose => (Self::ScopeClose, true),
            // The head calls nothing: it loads the stack slot that the
            // `list_uncons` right before it wrote (D-909), so it needs that
            // call's declaration only.
            RuntimeFn::ListUncons
            | RuntimeFn::ListUnconsHead
            | RuntimeFn::ListUnconsHeadFloat
            | RuntimeFn::ListUnconsHeadF32 => (Self::ListUncons, true),
            RuntimeFn::ListUnconsOwned => (Self::ListUnconsOwned, true),
            RuntimeFn::ListPrepend => (Self::ListPrepend, true),
            RuntimeFn::MapFind => (Self::MapFind, true),
            // A pair's element is loaded and dup'd inline, as a map's value
            // is; under `checked_rc` the runtime's read of a word at an
            // address does both.
            RuntimeFn::MapValueAt | RuntimeFn::PairLeft | RuntimeFn::PairRight => {
                (Self::MapValueAt, false)
            }
            RuntimeFn::ListSet => (Self::ListSet, true),
            // The address calls nothing: it loads the stack slot that the
            // `map_upsert` before it wrote, so it needs that call's
            // declaration only.
            RuntimeFn::MapUpsert | RuntimeFn::MapUpsertAddress => (Self::MapUpsert, true),
            RuntimeFn::MapSlotTake => (Self::MapSlotTake, false),
            RuntimeFn::MapUpsertFound => (Self::MapUpsertFound, true),
            RuntimeFn::MapUpsertStore => (Self::MapUpsertStore, true),
            RuntimeFn::SortStart => (Self::SortStart, true),
            RuntimeFn::HeapListStart => (Self::HeapListStart, true),
            RuntimeFn::BufferStep => (Self::BufferStep, false),
            RuntimeFn::BufferFinish => (Self::BufferFinish, true),
            // The slot and its first pair call nothing: the address of a
            // stack slot, which the start after it takes, and a load from it.
            RuntimeFn::HeapSlot | RuntimeFn::HeapFirst | RuntimeFn::HeapPush => {
                (Self::HeapPush, true)
            }
            RuntimeFn::HeapPop => (Self::HeapPop, true),
            RuntimeFn::HeapHeapify => (Self::HeapHeapify, true),
            RuntimeFn::HeapStep => (Self::HeapStep, true),
            // The loop's check is the entry check, whose slow path it calls.
            RuntimeFn::LoopCheck => (Self::StackSlow, true),
        }
    }
}

/// The data symbols of the runtime's static objects generated code may
/// reference, in declaration order.
pub const STATIC_DATA: [&str; 9] = [
    sym::STATIC_NIL,
    sym::STATIC_FALSE,
    sym::STATIC_TRUE,
    sym::STATIC_LESS,
    sym::STATIC_EQUAL,
    sym::STATIC_GREATER,
    sym::STATIC_NONE,
    sym::STATIC_EMPTY_LIST,
    sym::STATIC_ERR_NIL,
];

/// The runtime data generated code imports only when a unit first uses it,
/// so that the data ids, and with them the CLIF, of code that does not use
/// it stay as before: the counting flag of reuse hits (D-716, D-768).
pub const ON_DEMAND_DATA: [&str; 1] = [sym::STATS_COUNTING];

/// Every runtime symbol generated code may import, functions then data.
/// The JIT checks that the runtime's symbol registry lists each one.
#[must_use]
pub fn imported_symbols() -> Vec<&'static str> {
    RuntimeFunc::ALL
        .iter()
        .map(|f| f.symbol())
        .chain(STATIC_DATA)
        .chain(ON_DEMAND_DATA)
        .collect()
}
