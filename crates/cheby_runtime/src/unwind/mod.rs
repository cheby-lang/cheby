//! Unwinding (D-077): the runtime side of unwinding by return-flag
//! propagation, the one unwinding backend (D-733, ADR-0061), with the
//! catch points of D-699 and the Rust guards of D-697:
//!
//! - `flag`: the fiber's unwind flag at `ctx+12` (D-685), which generated
//!   code tests after each call that may unwind.
//! - `guard`: every `extern "C"` kernel and helper that may unwind runs
//!   its body in `guarded`, which turns a Cheby unwinding inside Rust
//!   into the flag and a sentinel.
//! - `raise`: `raise_panic` starts a Cheby panic from runtime code, and
//!   ends the process in code compiled without unwind edges
//!   (D-770).
//! - `record`: [`UnwindState`], the record of the unwinding in flight in
//!   a fiber's control block (D-685), and the catch points' accessors.
//! - `entry_slow`: the entry check's slow paths that release a function's
//!   counted parameters themselves when they start an unwinding
//!   (D-775), so code generation need not translate the
//!   entry-only pad chain.

pub(crate) mod entry_slow;
pub(crate) mod flag;
pub(crate) mod guard;
pub(crate) mod raise;
pub(crate) mod record;

pub(crate) use record::UnwindState;

#[cfg(test)]
mod tests {
    //! The shared unwinding runtime under the flag backend, with poisoning
    //! on and no JIT: a raise inside a kernel of each family, the owned
    //! arguments of the runtime helpers (D-698), `callback::call` and
    //! `map::upsert`, the stack the raise path uses (D-700), and the
    //! source test that keeps every entry point guarded (D-697).

    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex, PoisonError};

    use super::guard::{DropOnUnwind, guarded};
    use super::raise::raise_panic;
    use super::record::{self, UnwindRecord};
    use crate::abi::{self, PanicKind, Word, layout as ids, unwind_flag};
    use crate::alloc::{self, Domain, OUT_OF_MEMORY};
    use crate::consteval::{ConstTable, Limit};
    use crate::ctx::ExecCtx;
    use crate::entry::{Callbacks, ConstEvalLimits, EntryConfig, EntryMode, EntryOutcome};
    use crate::layout::{self, FieldKind, LayoutInfo};
    use crate::object::{self, write_field};
    use crate::panic::{PanicReport, cheby_rt_panic, stack_position};
    use crate::{hamt, list, rc, string, values};

    /// A kernel's address from the symbol table, as a function of type
    /// `$ty`.
    macro_rules! kernel {
        ($name:literal as $ty:ty) => {{
            let address = crate::symbols()
                .into_iter()
                .find(|(name, _)| *name == $name)
                .map(|(_, address)| address)
                .unwrap_or_else(|| panic!("no kernel {}", $name));
            // SAFETY: the registered address of the kernel, whose C
            // signature is `$ty` (the symbol tables check the signatures).
            unsafe { std::mem::transmute::<*const u8, $ty>(address) }
        }};
    }

    /// What a body run on an entry compiled for the flag backend returned,
    /// how the entry ended, and its accounting domain.
    struct Ran<T> {
        value: T,
        outcome: EntryOutcome,
        domain: Arc<Domain>,
    }

    /// Runs `body` on an isolated entry of `mode` whose code was compiled
    /// for the return-flag backend, with `callbacks` and a fresh domain.
    fn on_flag_entry<T: Send + 'static>(
        mode: EntryMode,
        consts: Option<Arc<ConstTable>>,
        callbacks: Callbacks,
        body: impl FnOnce(&ExecCtx) -> T + Send + 'static,
    ) -> Ran<T> {
        // Poisoning needs debug assertions.
        assert!(
            alloc::poison_enabled() || !cfg!(debug_assertions),
            "the unwind tests run with poisoning on"
        );
        let domain = Domain::new();
        let config = EntryConfig {
            domain: Arc::clone(&domain),
            callbacks,
            consts,
            ..EntryConfig::new(mode)
        };
        let slot: Arc<Mutex<Option<T>>> = Arc::default();
        let out = Arc::clone(&slot);
        // SAFETY: the task and the test trampolines follow the runtime's
        // conventions, and their code is static.
        let outcome = unsafe {
            crate::entry::run_task(&config, move || {
                let ctx = crate::entry::current_fiber().unwrap();
                let value = body(ctx);
                *out.lock().unwrap_or_else(PoisonError::into_inner) = Some(value);
                0
            })
        }
        .unwrap();
        let value = slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap();
        Ran {
            value,
            outcome,
            domain,
        }
    }

    /// The fiber's unwind flag.
    fn flag_of(ctx: &ExecCtx) -> u32 {
        ctx.unwind.load(Ordering::Acquire)
    }

    /// Lets the fiber allocate nothing more: the budget is what its domain
    /// holds now (D-536).
    fn exhaust(ctx: &ExecCtx) {
        let live = u64::try_from(ctx.domain().live_bytes()).unwrap();
        ctx.set_budget(live);
    }

    /// Lifts the budget again, for the test's own cleanup.
    fn unlimit(ctx: &ExecCtx) {
        ctx.set_budget(u64::MAX);
    }

    /// The report of an entry that panicked.
    fn report(outcome: &EntryOutcome) -> &PanicReport {
        match outcome {
            EntryOutcome::Panicked(report) => report,
            EntryOutcome::Returned(word) => panic!("expected a panic, the entry returned {word}"),
            EntryOutcome::Exited(status) => {
                panic!("expected a panic, the program ended with status {status}")
            }
            EntryOutcome::Deadlocked(report) => {
                panic!("expected a panic, the entry deadlocked:\n{report}")
            }
        }
    }

    /// Asserts that a kernel under an exhausted budget raised the memory
    /// limit, returned `sentinel` with the flag set, and left nothing live.
    fn assert_raised<T: PartialEq + std::fmt::Debug>(ran: &Ran<(T, u32)>, sentinel: &T) {
        let (result, flag) = &ran.value;
        assert_eq!(result, sentinel);
        assert_eq!(*flag, unwind_flag::PANIC);
        assert_eq!(report(&ran.outcome).message, Limit::Memory.message());
        assert_eq!(ran.domain.live_objects(), 0);
    }

    // -----------------------------------------------------------------
    // The pieces

    #[test]
    fn the_runtime_is_built_to_unwind() {
        // D-697: a Cheby unwinding inside Rust is a Rust unwinding.
        const { assert!(cfg!(panic = "unwind")) };
    }

    #[test]
    fn a_guard_turns_the_marker_into_the_flag_and_the_sentinel() {
        let ran = on_flag_entry(EntryMode::Test, None, Callbacks::default(), |ctx| {
            let word: Word = guarded(None, || raise_panic(None, PanicKind::Overflow, None));
            let (): () = guarded(None, || raise_panic(None, PanicKind::Overflow, None));
            let truth: bool = guarded(None, || raise_panic(None, PanicKind::Overflow, None));
            let float: f64 = guarded(None, || raise_panic(None, PanicKind::Overflow, None));
            (word, truth, float.to_bits(), flag_of(ctx))
        });
        assert_eq!(ran.value, (0, false, 0, unwind_flag::PANIC));
        // The first panic is the cause (D-699); the later ones were dropped.
        assert_eq!(report(&ran.outcome).kind, PanicKind::Overflow);
        assert_eq!(
            report(&ran.outcome).location,
            crate::panic::UNKNOWN_LOCATION
        );
    }

    #[test]
    fn the_first_panic_is_the_record_and_a_catch_point_takes_it() {
        let ran = on_flag_entry(EntryMode::Test, None, Callbacks::default(), |ctx| {
            record::start_panic(ctx, PanicReport::new(PanicKind::Assert, None, "a:1:1"));
            record::start_panic(ctx, PanicReport::new(PanicKind::Todo, None, "b:2:2"));
            super::guard::set_flag(ctx);
            let flagged = flag_of(ctx);
            let taken = record::take_unwound(ctx);
            let after = (flag_of(ctx), record::unwinding(ctx));
            let kind = match taken {
                Some(UnwindRecord::Panic(report)) => Some(report.kind),
                Some(UnwindRecord::Cancel) | None => None,
            };
            (flagged, kind, after)
        });
        assert_eq!(
            ran.value,
            (
                unwind_flag::PANIC,
                Some(PanicKind::Assert),
                (unwind_flag::NONE, false)
            )
        );
        // Nothing is in flight at the entry's catch point.
        assert_eq!(ran.outcome, EntryOutcome::Returned(0));
    }

    #[test]
    fn owned_words_are_released_only_by_an_unwinding() {
        let ran = on_flag_entry(EntryMode::Test, None, Callbacks::default(), |ctx| {
            let kept = values::tuple(&[]);
            let held = DropOnUnwind::new([kept, values::int_word(7), 0]);
            held.disarm();
            let lost = values::tuple(&[]);
            let unwound: Word = guarded(None, || {
                let _held = DropOnUnwind::new([lost]);
                raise_panic(None, PanicKind::Explicit, None)
            });
            // SAFETY: the test's reference, kept by the disarmed guard.
            unsafe { rc::drop(kept) };
            (unwound, flag_of(ctx))
        });
        assert_eq!(ran.value, (0, unwind_flag::PANIC));
        assert_eq!(ran.domain.live_objects(), 0);
    }

    // -----------------------------------------------------------------
    // A raise inside a kernel of each family, under the flag backend, with
    // the domain's live count at zero (D-697, I-3, I-4)

    #[test]
    fn a_list_kernel_raises_with_its_scratch_released() {
        let ran = on_flag_entry(EntryMode::Test, None, Callbacks::default(), |ctx| {
            let f = crate::generic::tests::closure(&[]);
            let xs = list::from_iter([values::int_word(1), f, values::int_word(1)]);
            // SAFETY: a live list, borrowed.
            let result = unsafe { list::cheby_list_unique(xs) };
            let flag = flag_of(ctx);
            // SAFETY: the test's reference.
            unsafe { rc::drop(xs) };
            (result, flag)
        });
        let (result, flag) = ran.value;
        assert_eq!((result, flag), (0, unwind_flag::PANIC));
        assert_eq!(report(&ran.outcome).kind, PanicKind::EqualityOnFunction);
        assert_eq!(ran.domain.live_objects(), 0);
    }

    #[test]
    fn map_and_set_kernels_raise_before_they_take_a_reference() {
        let ran = on_flag_entry(EntryMode::Test, None, Callbacks::default(), |ctx| {
            let f = crate::generic::tests::closure(&[]);
            let value = values::string("value");
            let one = values::int_word(1);
            // SAFETY: live words, borrowed; the empty map is static.
            let inserted = unsafe { hamt::cheby_map_insert(hamt::cheby_map_new(), f, value) };
            let first = flag_of(ctx);
            // SAFETY: as above.
            let map = unsafe { hamt::cheby_map_insert(hamt::cheby_map_new(), one, value) };
            // SAFETY: a live map and key, borrowed.
            let again = unsafe { hamt::cheby_map_insert(map, f, value) };
            let items = list::from_iter([values::int_word(2), f]);
            // SAFETY: rc-owned test words given to the list.
            unsafe { rc::dup(f) };
            // SAFETY: a live list, borrowed.
            let set = unsafe { hamt::cheby_set_from_list(items) };
            // SAFETY: the test's references.
            unsafe {
                rc::drop(items);
                rc::drop(map);
                rc::drop(value);
                rc::drop(f);
            }
            (inserted, again, set, first)
        });
        assert_eq!(ran.value, (0, 0, 0, unwind_flag::PANIC));
        assert_eq!(report(&ran.outcome).kind, PanicKind::EqualityOnFunction);
        assert_eq!(ran.domain.live_objects(), 0);
    }

    #[test]
    fn a_string_kernel_raises_out_of_memory_and_the_reserve_comes_back() {
        let ran = on_flag_entry(EntryMode::Test, None, Callbacks::default(), |ctx| {
            let repeat = kernel!("cheby_string_repeat" as unsafe extern "C" fn(Word, i64) -> Word);
            let text = values::string("ab");
            // SAFETY: a live string, borrowed. Its size overflows (D-642).
            let result = unsafe { repeat(text, i64::MAX) };
            let flag = flag_of(ctx);
            // SAFETY: the test's reference.
            unsafe { rc::drop(text) };
            (result, flag)
        });
        assert_eq!(ran.value, (0, unwind_flag::PANIC));
        assert_eq!(report(&ran.outcome).message, OUT_OF_MEMORY);
        // The catch completed: the reserve that out of memory gave back is
        // set aside again (D-642). Other tests may arm it too, so only
        // this direction is checked.
        assert!(alloc::reserve_armed());
        assert_eq!(ran.domain.live_objects(), 0);
    }

    #[test]
    fn a_bytes_kernel_raises_at_the_memory_limit() {
        let ran = on_flag_entry(EntryMode::Test, None, Callbacks::default(), |ctx| {
            let from = kernel!("cheby_bytes_from_string" as unsafe extern "C" fn(Word) -> Word);
            let text = values::string("bytes");
            exhaust(ctx);
            // SAFETY: a live string, borrowed.
            let result = unsafe { from(text) };
            let flag = flag_of(ctx);
            unlimit(ctx);
            // SAFETY: the test's reference.
            unsafe { rc::drop(text) };
            (result, flag)
        });
        assert_raised(&ran, &0);
    }

    #[test]
    fn a_number_kernel_raises_at_the_memory_limit() {
        let ran = on_flag_entry(EntryMode::Test, None, Callbacks::default(), |ctx| {
            let add = kernel!("cheby_int_checked_add" as extern "C" fn(i64, i64) -> Word);
            exhaust(ctx);
            let result = add(2, 3);
            let flag = flag_of(ctx);
            unlimit(ctx);
            (result, flag)
        });
        assert_raised(&ran, &0);
    }

    #[test]
    fn an_io_kernel_raises_at_the_memory_limit() {
        let ran = on_flag_entry(EntryMode::Test, None, Callbacks::default(), |ctx| {
            let read = kernel!(
                "cheby_io_sys_read_file" as unsafe extern "C" fn(*mut ExecCtx, Word) -> Word
            );
            let path = values::string("/nonexistent/cheby/unwind/test");
            exhaust(ctx);
            // SAFETY: the running entry's context (an isolated entry: the
            // read runs inline, D-692) and a live string, borrowed.
            let result = unsafe { read(std::ptr::from_ref(ctx).cast_mut(), path) };
            let flag = flag_of(ctx);
            unlimit(ctx);
            // SAFETY: the test's reference.
            unsafe { rc::drop(path) };
            (result, flag)
        });
        assert_raised(&ran, &0);
    }

    #[test]
    fn an_env_kernel_raises_at_the_memory_limit() {
        let ran = on_flag_entry(EntryMode::Test, None, Callbacks::default(), |ctx| {
            // The name is a new `String` (an empty argument list would be
            // the static empty list).
            let name = kernel!("cheby_env_program_name" as extern "C" fn() -> Word);
            exhaust(ctx);
            let result = name();
            let flag = flag_of(ctx);
            unlimit(ctx);
            (result, flag)
        });
        assert_raised(&ran, &0);
    }

    #[test]
    fn constant_evaluation_raises_its_memory_limit_and_its_thread_ends() {
        let table = ConstTable::new();
        let limits = ConstEvalLimits {
            memory: Some(0),
            ..ConstEvalLimits::default()
        };
        let ran = on_flag_entry(
            EntryMode::ConstEval(limits),
            Some(Arc::clone(&table)),
            Callbacks::default(),
            |ctx| {
                let header = abi::header(1, 0, ids::TUPLE);
                let ptr = std::ptr::from_ref(ctx).cast_mut();
                // SAFETY: the running fiber's block; a 16-byte tuple.
                let result = unsafe { alloc::cheby_rt_alloc(ptr, 16, header) };
                (result, flag_of(ctx))
            },
        );
        // `run_task` returned, so the entry's thread was joined (D-713).
        assert_raised(&ran, &0);
        assert_eq!(table.abandon().limit, Some(Limit::Memory));
    }

    /// The code word of the test hook closures.
    const HOOK: u64 = 0x40;

    /// A hook closure the layout table keeps.
    fn hook() -> Word {
        let obj = alloc::alloc_object(16, abi::header(1, 1, ids::CLOSURE));
        // SAFETY: initializing a fresh closure with no captures.
        unsafe { write_field(obj, 8, HOOK | 1) };
        let word = object::ptr_to_word(obj);
        // SAFETY: the layout table keeps it forever.
        unsafe { alloc::forget(word) };
        word
    }

    /// A trampoline whose closure panics: as generated code does under the
    /// flag backend, it releases its owned arguments in its cleanup pad,
    /// calls `cheby_rt_panic`, which returns with the flag set, and returns
    /// 0 from its catch point (D-699).
    unsafe extern "C" fn panics_with_two(ctx: *mut ExecCtx, _: u64, a: u64, b: u64) -> u64 {
        // SAFETY: the trampoline owns its arguments.
        unsafe {
            rc::drop(a);
            rc::drop(b);
            cheby_rt_panic(ctx, PanicKind::Assert.code(), 0, 0);
        }
        0
    }

    /// [`panics_with_two`] with one argument.
    unsafe extern "C" fn panics_with_one(ctx: *mut ExecCtx, _: u64, a: u64) -> u64 {
        // SAFETY: the trampoline owns its argument.
        unsafe {
            rc::drop(a);
            cheby_rt_panic(ctx, PanicKind::Assert.code(), 0, 0);
        }
        0
    }

    #[test]
    fn an_equality_hook_raises_through_callback_call() {
        let callbacks = Callbacks {
            t2: Some(panics_with_two),
            ..Callbacks::default()
        };
        let ran = on_flag_entry(EntryMode::Test, None, callbacks, |ctx| {
            let key = layout::type_key("unwind_tests", "Hooked");
            let id = layout::register_consecutive(vec![
                LayoutInfo::ctor("unwind_tests.Hooked.Hooked", 0, &[(FieldKind::Int, None)])
                    .named("Hooked", "Hooked", false, key),
            ])
            .unwrap();
            layout::set_hooks(id, [hook(), hook(), hook()]).unwrap();
            let make = || {
                let obj = alloc::alloc_object(16, abi::header(1, 0, id));
                // SAFETY: a fresh object with one raw field.
                unsafe { write_field(obj, 8, 5) };
                object::ptr_to_word(obj)
            };
            let (a, b) = (make(), make());
            let inside = values::tuple(&[a, b]);
            // SAFETY: the tuple owns `a` and `b`; this test keeps its own.
            unsafe {
                rc::dup(a);
                rc::dup(b);
            }
            let other = values::tuple(&[b, a]);
            let ptr = std::ptr::from_ref(ctx).cast_mut();
            // The walk reaches the hook inside the tuples, so the unwinding
            // passes through the walk's worklists and `callback::call`.
            // SAFETY: two live values of one type, borrowed.
            let same = unsafe { crate::helpers::cheby_rt_equal(ptr, inside, other, 0) };
            let flag = flag_of(ctx);
            // `callback::call` left the callback depth as it found it.
            let pinned = ctx.sched.pinned();
            // SAFETY: the test's references.
            unsafe {
                rc::drop(inside);
                rc::drop(other);
            }
            (same, flag, pinned)
        });
        assert_eq!(ran.value, (false, unwind_flag::PANIC, false));
        assert_eq!(report(&ran.outcome).kind, PanicKind::Assert);
        assert_eq!(ran.domain.live_objects(), 0);
    }

    #[test]
    fn upsert_removes_the_moved_entry_when_its_callback_raises() {
        let callbacks = Callbacks {
            t1: Some(panics_with_one),
            ..Callbacks::default()
        };
        let ran = on_flag_entry(EntryMode::Test, None, callbacks, |ctx| {
            let update = crate::generic::tests::closure(&[]);
            let key = values::string("key");
            let value = values::string("stored");
            let other = values::int_word(2);
            // A map of two entries, unique: the second insert updates the
            // first map in place and adds the result's reference.
            let build = || {
                // SAFETY: live words, borrowed; the empty map is static.
                unsafe {
                    let map = hamt::cheby_map_insert(hamt::cheby_map_new(), key, value);
                    let both = hamt::cheby_map_insert(map, other, other);
                    rc::drop(map);
                    both
                }
            };
            let (unique, shared) = (build(), build());
            // SAFETY: a live map; the test now holds two references.
            unsafe { rc::dup(shared) };
            // A unique map: its stored value moves into the callback.
            // SAFETY: a live map, key and closure, borrowed.
            let first = unsafe { hamt::cheby_map_upsert(unique, key, update) };
            let first_flag = flag_of(ctx);
            record::take_unwound(ctx);
            // A shared map (count 2): the path is copied, the map unchanged.
            // SAFETY: as above.
            let second = unsafe { hamt::cheby_map_upsert(shared, key, update) };
            // SAFETY: a live map and key, borrowed.
            let still = unsafe { hamt::cheby_map_get(shared, key) };
            // SAFETY: a live `Some(String)`.
            let kept = unsafe { values::read_option(still) }
                .and_then(|word| unsafe { string::as_str(word) }.map(str::to_owned));
            // SAFETY: the test's references (`unique` was released whole by
            // its upsert's guard and the caller's own reference below).
            unsafe {
                rc::drop(still);
                rc::drop(unique);
                rc::drop(shared);
                rc::drop(shared);
                rc::drop(value);
                rc::drop(key);
                rc::drop(update);
            }
            (first, first_flag, second, kept)
        });
        let (first, first_flag, second, kept) = ran.value;
        assert_eq!((first, first_flag, second), (0, unwind_flag::PANIC, 0));
        assert_eq!(kept.as_deref(), Some("stored"));
        assert_eq!(report(&ran.outcome).kind, PanicKind::Assert);
        assert_eq!(ran.domain.live_objects(), 0);
    }

    /// A one-field value of a test type registered as `name` with the test
    /// hooks: the type's layout id.
    fn hooked_type(name: &str) -> abi::LayoutId {
        let key = layout::type_key("unwind_tests", name);
        let id = layout::register_consecutive(vec![
            LayoutInfo::ctor(
                &format!("unwind_tests.{name}.{name}"),
                0,
                &[(FieldKind::Int, None)],
            )
            .named(name, name, false, key),
        ])
        .unwrap();
        layout::set_hooks(id, [hook(), hook(), hook()]).unwrap();
        id
    }

    /// A fresh value of the hooked type `id`.
    fn hooked_value(id: abi::LayoutId) -> Word {
        let obj = alloc::alloc_object(16, abi::header(1, 0, id));
        // SAFETY: a fresh object with one raw field.
        unsafe { write_field(obj, 8, 5) };
        object::ptr_to_word(obj)
    }

    /// A `debug` hook's trampoline: returns a text longer than the
    /// printer's first 16 bytes, then lets the fiber allocate nothing more,
    /// so writing the text it gave the printer raises the memory limit.
    unsafe extern "C" fn long_text_then_exhausted(ctx: *mut ExecCtx, _: u64, a: u64) -> u64 {
        // SAFETY: the trampoline owns its argument; `ctx` is the running
        // fiber's.
        unsafe { rc::drop(a) };
        let text = values::string("a debug text longer than sixteen bytes");
        // SAFETY: as above.
        exhaust(unsafe { &*ctx });
        text
    }

    #[test]
    fn debug_printing_releases_a_hook_text_when_it_raises() {
        let id = hooked_type("Printed");
        // Alone, the printer's first worklist growth raises with the hook's
        // text in hand; inside a tuple the worklist has room, and writing
        // the text to the result raises.
        for in_tuple in [false, true] {
            let callbacks = Callbacks {
                t1: Some(long_text_then_exhausted),
                ..Callbacks::default()
            };
            let ran = on_flag_entry(EntryMode::Test, None, callbacks, move |ctx| {
                let value = hooked_value(id);
                let printed = if in_tuple {
                    values::tuple(&[value])
                } else {
                    value
                };
                let ptr = std::ptr::from_ref(ctx).cast_mut();
                // SAFETY: a live value, borrowed, on the running entry.
                let text = unsafe { crate::helpers::cheby_rt_debug(ptr, printed) };
                let flag = flag_of(ctx);
                unlimit(ctx);
                // SAFETY: the test's reference (the tuple owns `value`).
                unsafe { rc::drop(printed) };
                (text, flag)
            });
            assert_raised(&ran, &0);
        }
    }

    /// Whether [`hash_hook`] raises.
    static HASH_HOOK_RAISES: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// A `hash` hook's trampoline: the canonical key 7, or a panic once
    /// [`HASH_HOOK_RAISES`] is set.
    unsafe extern "C" fn hash_hook(ctx: *mut ExecCtx, _: u64, a: u64) -> u64 {
        // SAFETY: the trampoline owns its argument.
        unsafe { rc::drop(a) };
        if HASH_HOOK_RAISES.load(Ordering::Acquire) {
            // SAFETY: the running fiber's context.
            unsafe { cheby_rt_panic(ctx, PanicKind::Assert.code(), 0, 0) };
            return 0;
        }
        values::int_word(7)
    }

    #[test]
    fn rebuilding_under_a_seed_releases_the_map_when_a_hash_hook_raises() {
        let callbacks = Callbacks {
            t1: Some(hash_hook),
            ..Callbacks::default()
        };
        let ran = on_flag_entry(EntryMode::Test, None, callbacks, |ctx| {
            let key = hooked_value(hooked_type("Rehashed"));
            let value = values::string("value");
            // SAFETY: live words, borrowed; the empty map is static.
            let map = unsafe { hamt::cheby_map_insert(hamt::cheby_map_new(), key, value) };
            let seed = crate::entry::current_seed();
            HASH_HOOK_RAISES.store(true, Ordering::Release);
            // The map is given up, and the new trie's first key raises.
            // SAFETY: a live map, owned.
            let rebuilt: Word = guarded(None, || unsafe {
                hamt::rebuild_with_seed(map, seed.wrapping_add(1))
            });
            HASH_HOOK_RAISES.store(false, Ordering::Release);
            let flag = flag_of(ctx);
            // SAFETY: the test's references.
            unsafe {
                rc::drop(key);
                rc::drop(value);
            }
            (rebuilt, flag)
        });
        assert_eq!(ran.value, (0, unwind_flag::PANIC));
        assert_eq!(report(&ran.outcome).kind, PanicKind::Assert);
        assert_eq!(ran.domain.live_objects(), 0);
    }

    // -----------------------------------------------------------------
    // Runtime helpers consume their owned arguments even when they raise
    // (D-698)

    /// A helper's second argument: given up with the call, or borrowed
    /// (and released by the test afterwards).
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Second {
        Owned,
        Borrowed,
    }

    /// Runs `call(ctx, first, second)` twice under an exhausted budget:
    /// with a unique `first` argument and with a shared one, whose other
    /// reference the test releases afterwards. Each call must raise and
    /// consume what it owns. The arguments are made before the budget runs
    /// out.
    fn consumes_when_raising(
        make: fn() -> Word,
        second: (fn(Word) -> Word, Second),
        call: fn(*mut ExecCtx, Word, Word) -> Word,
    ) -> Ran<((Word, Word), u32)> {
        let (make_second, kind) = second;
        on_flag_entry(EntryMode::Test, None, Callbacks::default(), move |ctx| {
            let ptr = std::ptr::from_ref(ctx).cast_mut();
            let mut results = [0; 2];
            for (round, result) in results.iter_mut().enumerate() {
                let first = make();
                if round == 1 {
                    // SAFETY: a live object; the test keeps a second
                    // reference.
                    unsafe { rc::dup(first) };
                }
                let other = make_second(first);
                exhaust(ctx);
                *result = call(ptr, first, other);
                unlimit(ctx);
                // SAFETY: the test's own references.
                unsafe {
                    if kind == Second::Borrowed {
                        rc::drop(other);
                    }
                    if round == 1 {
                        rc::drop(first);
                    }
                }
                if round == 0 {
                    // The second raise records a panic of its own.
                    record::take_unwound(ctx);
                }
            }
            let flag = flag_of(ctx);
            ((results[0], results[1]), flag)
        })
    }

    #[test]
    fn str_append_consumes_its_accumulator_when_it_raises() {
        let ran = consumes_when_raising(
            // No room: the append must allocate.
            || object::ptr_to_word(string::alloc_string(0)),
            (|_| values::string("piece"), Second::Borrowed),
            |ctx, acc, piece| {
                // SAFETY: `acc` is given up, `piece` borrowed.
                unsafe { crate::helpers::cheby_rt_str_append(ctx, acc, piece) }
            },
        );
        assert_raised(&ran, &(0, 0));
        // The piece is the accumulator: it is copied first.
        let ran = consumes_when_raising(
            || values::string("same"),
            (
                |acc| {
                    // SAFETY: a live string; a reference for the piece.
                    unsafe { rc::dup(acc) };
                    acc
                },
                Second::Borrowed,
            ),
            |ctx, acc, piece| {
                // SAFETY: one reference given up, one borrowed.
                unsafe { crate::helpers::cheby_rt_str_append(ctx, acc, piece) }
            },
        );
        assert_raised(&ran, &(0, 0));
    }

    /// A list of three elements, one of them an object.
    fn three() -> Word {
        list::from_iter([
            values::int_word(1),
            values::string("two"),
            values::int_word(3),
        ])
    }

    #[test]
    fn list_push_consumes_its_list_and_element_when_it_raises() {
        let ran = consumes_when_raising(
            three,
            (|_| values::string("four"), Second::Owned),
            |ctx, list, elem| {
                // SAFETY: both are given up.
                unsafe { list::cheby_rt_list_push(ctx, list, elem) }
            },
        );
        assert_raised(&ran, &(0, 0));
    }

    #[test]
    fn list_append_consumes_its_first_list_when_it_raises() {
        let ran = consumes_when_raising(
            three,
            (|_| three(), Second::Borrowed),
            |ctx, list, other| {
                // SAFETY: `list` is given up, `other` borrowed.
                unsafe { list::cheby_rt_list_append(ctx, list, other) }
            },
        );
        assert_raised(&ran, &(0, 0));
    }

    #[test]
    fn list_slice_owned_consumes_its_list_when_it_raises() {
        let ran = consumes_when_raising(three, (|_| 0, Second::Owned), |ctx, list, _| {
            // SAFETY: `list` is given up.
            unsafe { list::cheby_rt_list_slice_owned(ctx, list, 1, 2) }
        });
        assert_raised(&ran, &(0, 0));
    }

    #[test]
    fn list_uncons_owned_consumes_its_list_when_it_raises() {
        let ran = consumes_when_raising(three, (|_| 0, Second::Owned), |ctx, list, _| {
            let mut head: Word = 7;
            // SAFETY: `list` is given up; `head` is writable.
            let rest =
                unsafe { list::cheby_rt_list_uncons_owned(ctx, list, 0, 1, 3, &raw mut head) };
            // The element is written only when the call returns normally
            // (D-909).
            assert_eq!(head, 7);
            rest
        });
        assert_raised(&ran, &(0, 0));
    }

    // -----------------------------------------------------------------
    // Marking a value shared (D-705)

    /// The count of the live object `word`.
    fn count_of(word: Word) -> i32 {
        // SAFETY: tests pass live objects.
        unsafe { object::load_rc(object::word_to_ptr(word)) }
    }

    /// Every distinct object reachable from `root`, each with the words of
    /// its children. Iterative.
    fn graph(root: Word) -> Vec<(Word, Vec<Word>)> {
        let mut seen = std::collections::HashSet::new();
        let mut list = vec![root];
        let mut out = Vec::new();
        while let Some(word) = list.pop() {
            if abi::is_tagged_int(word) || word == 0 || !seen.insert(word) {
                continue;
            }
            let mut children = Vec::new();
            // SAFETY: a live object reached from the test's value.
            unsafe { alloc::for_each_child(word, &mut |child| children.push(child)) };
            list.extend(children.iter().copied());
            out.push((word, children));
        }
        out
    }

    /// Review finding MEM-1: growing `mark_shared`'s worklist raises out of
    /// memory (here the memory limit, which takes the same path through
    /// the scratch buffer's growth), and a backend lets the program catch
    /// it and retry the send or spawn. Whatever the raise leaves marked
    /// must reach only marked or static objects, and a retry must mark
    /// everything: before the fix the root was marked first, its children
    /// kept their local counts, and the retry skipped them.
    #[test]
    fn mark_shared_raising_partway_keeps_its_invariant_and_a_retry_completes() {
        /// A tuple of a wide tuple of 200 one-field tuples, then 23 tuples
        /// of one string each. The walk enters the last child first, so it
        /// marks the small ones before the wide one makes its worklist
        /// grow.
        fn value() -> Word {
            let leaf = |text: String| values::tuple(&[values::string(&text)]);
            let wide: Vec<Word> = (0..200).map(|i| leaf(format!("wide {i}"))).collect();
            let mut outer = vec![values::tuple(&wide)];
            outer.extend((0..23).map(|i| leaf(format!("small {i}"))));
            values::tuple(&outer)
        }
        let ran = on_flag_entry(EntryMode::Test, None, Callbacks::default(), |ctx| {
            let (mut raised, mut partial) = (0, 0);
            for extra in (0..8192).step_by(32) {
                let root = value();
                let live = u64::try_from(ctx.domain().live_bytes()).unwrap();
                let scratch = ctx.scratch().0.load(Ordering::Relaxed);
                ctx.set_budget(live + scratch + extra);
                // SAFETY: a live value only this fiber uses.
                guarded(None, || unsafe { rc::mark_shared(root) });
                let flagged = flag_of(ctx) != 0;
                unlimit(ctx);
                let objects = graph(root);
                if flagged {
                    raised += 1;
                    assert!(record::take_unwound(ctx).is_some());
                    for (word, children) in &objects {
                        if count_of(*word) <= 0 {
                            for child in children.iter().filter(|c| !abi::is_tagged_int(**c)) {
                                assert!(
                                    count_of(*child) <= 0,
                                    "a marked object reaches a local one"
                                );
                            }
                        }
                    }
                    if objects.iter().any(|(word, _)| count_of(*word) < 0) {
                        partial += 1;
                    }
                    // The retry.
                    // SAFETY: as above.
                    unsafe { rc::mark_shared(root) };
                }
                for (word, _) in &objects {
                    assert!(count_of(*word) <= 0, "a retry left a local count");
                }
                // SAFETY: the test's reference.
                unsafe { rc::drop(root) };
            }
            (raised, partial)
        });
        let (raised, partial) = ran.value;
        assert!(raised > 0, "the budget never stopped the walk");
        assert!(partial > 0, "no raise came after the walk marked something");
        assert!(matches!(ran.outcome, EntryOutcome::Returned(0)));
        assert_eq!(ran.domain.live_objects(), 0);
    }

    // -----------------------------------------------------------------
    // The stack of the raise path (D-700)

    /// The byte a painted stack holds where nothing has written.
    const PAINT: u8 = 0xA5;
    /// How much stack below the measuring frame is painted.
    const PAINTED: usize = 128 << 10;
    /// The most stack the raise path may use (D-700).
    const RAISE_BOUND: usize = 32 << 10;

    /// Paints `PAINTED` bytes of the stack below the caller's frame and
    /// returns their addresses.
    #[inline(never)]
    fn paint() -> std::ops::Range<usize> {
        #[expect(
            clippy::large_stack_arrays,
            reason = "the test paints the stack on purpose"
        )]
        let mut area = [PAINT; PAINTED];
        std::hint::black_box(&mut area);
        let start = area.as_mut_ptr().expose_provenance();
        start..start.saturating_add(PAINTED)
    }

    /// The bytes of stack that `raise` used: from the measuring frame down
    /// to the lowest painted byte it overwrote.
    #[inline(never)]
    fn stack_used(raise: impl FnOnce()) -> usize {
        let top = stack_position();
        let area = paint();
        raise();
        let lowest = area
            .clone()
            .find(|&at| {
                let byte = std::ptr::with_exposed_provenance::<u8>(at);
                // SAFETY: the painted bytes lie in this thread's stack,
                // below the current frame, mapped and no longer used.
                let value = unsafe { byte.read_volatile() };
                value != PAINT
            })
            .unwrap_or(area.end);
        top.saturating_sub(lowest)
    }

    #[test]
    #[cfg_attr(miri, ignore = "reads the stack below the current frame")]
    fn the_raise_path_uses_at_most_32_kib_of_stack() {
        let ran = on_flag_entry(EntryMode::Test, None, Callbacks::default(), |ctx| {
            let f = crate::generic::tests::closure(&[]);
            let xs = list::from_iter([f]);
            let plain = list::from_iter([values::int_word(1)]);
            // The same kernel when it returns, for comparison.
            let returns = stack_used(|| {
                // SAFETY: a live list, borrowed.
                let result = unsafe { list::cheby_list_unique(plain) };
                // SAFETY: the kernel's result, owned.
                unsafe { rc::drop(std::hint::black_box(result)) };
            });
            // SAFETY: the test's reference.
            unsafe { rc::drop(plain) };
            eprintln!("the kernel without a raise: {returns} bytes");
            // A kernel raises: Rust's unwinder runs from the raise to the
            // kernel's guard.
            let kernel = stack_used(|| {
                // SAFETY: a live list, borrowed.
                let result = unsafe { list::cheby_list_unique(xs) };
                std::hint::black_box(result);
            });
            record::take_unwound(ctx);
            // A stack overflow found by the slow path: no Rust unwinding.
            let ptr = std::ptr::from_ref(ctx).cast_mut();
            let overflow = stack_used(|| {
                // SAFETY: the running fiber's block; no location.
                unsafe { crate::panic::cheby_rt_stack_slow_frame(ptr, 0, u64::MAX) };
            });
            let flag = flag_of(ctx);
            // SAFETY: the test's reference.
            unsafe { rc::drop(xs) };
            (kernel, overflow, flag)
        });
        let (kernel, overflow, flag) = ran.value;
        // For the measurement report (plan §4.5, item 5).
        eprintln!("raise path stack: kernel {kernel} bytes, stack overflow {overflow} bytes");
        assert_eq!(flag, unwind_flag::PANIC);
        assert_eq!(report(&ran.outcome).kind, PanicKind::StackOverflow);
        for used in [kernel, overflow] {
            assert!(used > 0, "nothing was measured");
            assert!(
                used <= RAISE_BOUND,
                "the raise path used {used} bytes of stack"
            );
        }
        assert_eq!(ran.domain.live_objects(), 0);
    }

    // -----------------------------------------------------------------
    // Any other Rust panic stays fatal (D-697)

    /// The child side of [`a_rust_panic_in_a_guard_is_fatal`]: does nothing
    /// unless `CHEBY_RT_TEST_RUST_PANIC` is set.
    #[test]
    fn rust_panic_child() {
        if std::env::var_os("CHEBY_RT_TEST_RUST_PANIC").is_none() {
            return;
        }
        let _: Word = guarded(None, || panic!("a bug in a kernel"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "starts a process")]
    fn a_rust_panic_in_a_guard_is_fatal() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unwind::tests::rust_panic_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("CHEBY_RT_TEST_RUST_PANIC", "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(
            stderr.contains("fatal runtime error: a Rust panic inside the runtime"),
            "{stderr}"
        );
        // The marker never goes through the panic hook; this panic did.
        assert!(stderr.contains("a bug in a kernel"), "{stderr}");
    }

    // -----------------------------------------------------------------
    // Every entry point is guarded (D-697)

    /// Reasons an entry point never unwinds.
    const DUP: &str = "a count overflow makes the object static (D-454); the walk that marks what it reaches shared is not charged to the budget, and a system refusal during it is fatal (D-753, D-765)";
    const DROP: &str = "a panic in a drop function aborts (D-171)";
    const RAISES_ITSELF: &str = "the outermost raise: hands the panic to the backend itself";
    const READS: &str = "reads its arguments and allocates nothing";
    const COUNTS: &str = "adds one to a counter of the current domain (D-768)";
    const LIST_GET: &str = "reads an element whose index lowering checked";
    const STATIC: &str = "returns a static object";
    const READS_STATIC: &str = "reads its arguments and returns a static object (D-902)";
    const ARITHMETIC: &str = "arithmetic on unboxed numbers";
    const FLOAT_MATH: &str = "float arithmetic whose Rust temporaries never raise";
    const PRINTS: &str = "a failed write exits (D-306) and nothing is allocated";
    const FIBER_BASE: &str = "a fiber's base: runs catch points that return (D-699)";
    const SWITCH: &str = "the context switch, in assembly";
    const FIRST_FRAME: &str = "the first frame of a fiber, in assembly";
    const NO_FIBERS: &str = "ends the process: no fibers on this target";
    const EXE_ENTRY: &str = "an executable's entry, called by C `main` with no Cheby frame below it: \
                             `main` runs on a root fiber whose base catches every unwinding (D-699), \
                             and a Rust panic aborts at this `extern \"C\"` boundary (D-925)";
    const RELEASES: &str = "calls the guarded slow path, which sets the flag and returns, tests the \
                            flag, then releases words only through `rc::drop`, which never raises \
                            (D-171, D-775)";
    const STUB: &str = "a naked stub that passes its caller's frame record to its body (D-1040), \
                        or a plain function calling that body";
    const SLOW_BODY: &str = "runs the slow path in its guard (`stack_slow_at`), which sets the flag \
                             and returns (D-700)";
    const SIGNAL_HANDLER: &str = "a signal handler (D-1073): one atomic store, one atomic load, \
                                  `write` and `errno`, nothing that can panic, and never an entry \
                                  from Cheby code";

    /// The `extern "C"` functions of the crate that never unwind, with the
    /// reason, by file (relative to `src/`) and name (`$name` inside a
    /// macro).
    const NEVER_UNWINDS: &[(&str, &str, &str)] = &[
        ("rc.rs", "cheby_rt_dup", DUP),
        ("rc.rs", "cheby_rt_drop", DROP),
        ("reuse.rs", "cheby_rt_release_fields", DROP),
        ("reuse.rs", "cheby_rt_drop_reuse_checked", DROP),
        ("reuse.rs", "cheby_rt_free_unique", DROP),
        ("reuse.rs", "cheby_rt_reuse_hit", COUNTS),
        ("panic.rs", "cheby_rt_panic", RAISES_ITSELF),
        ("panic.rs", "panic_from", RAISES_ITSELF),
        ("panic.rs", "cheby_rt_stack_slow", STUB),
        ("panic.rs", "stack_slow_from", SLOW_BODY),
        ("panic.rs", "cheby_rt_stack_slow_frame", STUB),
        ("panic.rs", "stack_slow_frame_from", SLOW_BODY),
        ("ffi.rs", "cheby_rt_ffi_value", STUB),
        ("ffi.rs", "ffi_value_from", RAISES_ITSELF),
        ("unwind/entry_slow.rs", "stack_slow_unwind_from", RELEASES),
        (
            "unwind/entry_slow.rs",
            "stack_slow_frame_unwind_from",
            RELEASES,
        ),
        (
            "unwind/entry_slow.rs",
            "cheby_rt_stack_slow_unwind",
            RELEASES,
        ),
        (
            "unwind/entry_slow.rs",
            "cheby_rt_stack_slow_frame_unwind",
            RELEASES,
        ),
        ("helpers.rs", "cheby_rt_str_eq", READS),
        ("helpers.rs", "cheby_rt_fmod_f64", ARITHMETIC),
        ("helpers.rs", "cheby_rt_fmod_f32", ARITHMETIC),
        ("list/kernels.rs", "cheby_rt_list_length", READS),
        ("list/kernels.rs", "cheby_rt_list_get", LIST_GET),
        ("list/kernels.rs", "cheby_list_length", READS),
        ("hamt/kernels.rs", "cheby_map_new", STATIC),
        ("hamt/kernels.rs", "cheby_map_size", READS),
        ("hamt/kernels.rs", "cheby_map_is_empty", READS),
        ("hamt/kernels.rs", "cheby_set_new", STATIC),
        ("hamt/kernels.rs", "cheby_set_size", READS),
        ("hamt/kernels.rs", "cheby_set_is_empty", READS),
        ("text/kernels.rs", "cheby_string_length", READS),
        ("text/kernels.rs", "cheby_string_grapheme_length", READS),
        ("text/kernels.rs", "cheby_string_starts_with", READS),
        ("text/kernels.rs", "cheby_string_ends_with", READS),
        ("text/kernels.rs", "cheby_string_contains", READS),
        ("bytes/kernels.rs", "cheby_bytes_length", READS),
        ("bytes/kernels.rs", "cheby_bytes_get", READS_STATIC),
        ("bytes/kernels.rs", "cheby_bytes_read_i8", READS_STATIC),
        ("bytes/kernels.rs", "cheby_bytes_json_plain_end", READS),
        ("num/int.rs", "$wfn", ARITHMETIC),
        ("num/int.rs", "wrapping_add", ARITHMETIC),
        ("num/int.rs", "wrapping_sub", ARITHMETIC),
        ("num/int.rs", "wrapping_mul", ARITHMETIC),
        ("num/int.rs", "wrapping_neg", ARITHMETIC),
        ("num/float.rs", "$lfn", ARITHMETIC),
        ("num/float.rs", "$rfn", ARITHMETIC),
        ("num/float.rs", "sqrt", ARITHMETIC),
        ("num/math.rs", "pow_kernel", FLOAT_MATH),
        ("num/math.rs", "exp_kernel", FLOAT_MATH),
        ("num/math.rs", "ln_kernel", FLOAT_MATH),
        ("num/math.rs", "sin_kernel", FLOAT_MATH),
        ("num/math.rs", "cos_kernel", FLOAT_MATH),
        ("num/math.rs", "tan_kernel", FLOAT_MATH),
        ("num/math.rs", "atan2_kernel", FLOAT_MATH),
        ("io.rs", "cheby_io_print", PRINTS),
        ("io.rs", "cheby_io_println", PRINTS),
        ("io.rs", "cheby_io_eprint", PRINTS),
        ("io.rs", "cheby_io_eprintln", PRINTS),
        ("sched/fiber.rs", "fiber_start", FIBER_BASE),
        ("sched/signal.rs", "on_signal", SIGNAL_HANDLER),
        ("fiber/switch/aarch64.rs", "switch", SWITCH),
        ("fiber/switch/aarch64.rs", "fiber_entry", FIRST_FRAME),
        ("fiber/switch/x86_64_sysv.rs", "switch", SWITCH),
        ("fiber/switch/x86_64_sysv.rs", "fiber_entry", FIRST_FRAME),
        ("fiber/switch/unsupported.rs", "switch", NO_FIBERS),
        ("rt_main.rs", "cheby_rt_main", EXE_ENTRY),
    ];

    /// `source` with its comments, and with `strings` its string and
    /// character literals, replaced by spaces; newlines and every offset
    /// stay as they were.
    fn blank(source: &str, strings: bool) -> Vec<u8> {
        let bytes = source.as_bytes();
        let mut out = bytes.to_vec();
        let mut at = 0_usize;
        let wipe = |out: &mut Vec<u8>, range: std::ops::Range<usize>| {
            for byte in out.get_mut(range).into_iter().flatten() {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
        };
        let at_ = |i: usize| bytes.get(i).copied().unwrap_or(0);
        while at < bytes.len() {
            let (c, next) = (at_(at), at_(at.saturating_add(1)));
            let end = if c == b'/' && next == b'/' {
                let end = (at..bytes.len())
                    .find(|&i| at_(i) == b'\n')
                    .unwrap_or(bytes.len());
                wipe(&mut out, at..end);
                end
            } else if c == b'/' && next == b'*' {
                let mut depth = 0_usize;
                let mut i = at;
                while i < bytes.len() {
                    if at_(i) == b'/' && at_(i.saturating_add(1)) == b'*' {
                        depth = depth.saturating_add(1);
                        i = i.saturating_add(2);
                    } else if at_(i) == b'*' && at_(i.saturating_add(1)) == b'/' {
                        depth = depth.saturating_sub(1);
                        i = i.saturating_add(2);
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i = i.saturating_add(1);
                    }
                }
                wipe(&mut out, at..i);
                i
            } else if c == b'"' {
                let mut i = at.saturating_add(1);
                while i < bytes.len() && at_(i) != b'"' {
                    i = i.saturating_add(if at_(i) == b'\\' { 2 } else { 1 });
                }
                let end = i.saturating_add(1);
                if strings {
                    wipe(&mut out, at..end);
                }
                end
            } else if c == b'r'
                && (next == b'"' || next == b'#')
                && !at_(at.wrapping_sub(1)).is_ascii_alphanumeric()
            {
                let hashes = (at.saturating_add(1)..bytes.len())
                    .take_while(|&i| at_(i) == b'#')
                    .count();
                let open = at.saturating_add(1).saturating_add(hashes);
                if at_(open) == b'"' {
                    let close: Vec<u8> = std::iter::once(b'"')
                        .chain(std::iter::repeat_n(b'#', hashes))
                        .collect();
                    let body = open.saturating_add(1);
                    let end = (body..bytes.len())
                        .find(|&i| bytes.get(i..).is_some_and(|rest| rest.starts_with(&close)))
                        .map_or(bytes.len(), |i| i.saturating_add(close.len()));
                    if strings {
                        wipe(&mut out, at..end);
                    }
                    end
                } else {
                    at.saturating_add(1)
                }
            } else if c == b'\'' && (next == b'\\' || at_(at.saturating_add(2)) == b'\'') {
                // A character literal (a lifetime has no closing quote).
                let mut i = at.saturating_add(1);
                while i < bytes.len() && at_(i) != b'\'' {
                    i = i.saturating_add(if at_(i) == b'\\' { 2 } else { 1 });
                }
                let end = i.saturating_add(1);
                if strings {
                    wipe(&mut out, at..end);
                }
                end
            } else {
                at.saturating_add(1)
            };
            at = end;
        }
        out
    }

    /// The offset just past the brace that closes the one at `open`.
    fn closing(code: &[u8], open: usize) -> usize {
        let mut depth = 0_usize;
        for (i, byte) in code.iter().enumerate().skip(open) {
            match byte {
                b'{' => depth = depth.saturating_add(1),
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return i.saturating_add(1);
                    }
                }
                _ => {}
            }
        }
        code.len()
    }

    /// The `#[cfg(test)]` modules with a body in `code`, as ranges.
    fn test_modules(code: &[u8]) -> Vec<std::ops::Range<usize>> {
        let text = String::from_utf8_lossy(code);
        let mut out = Vec::new();
        for (at, _) in text.match_indices("#[cfg(test)]") {
            let rest = text.get(at..).unwrap_or_default();
            let Some(item) = rest.find("mod ") else {
                continue;
            };
            let head = rest.get(..item).unwrap_or_default();
            // Only attributes and a visibility may come between.
            let before = head.replace("#[cfg(test)]", "");
            let before = before.trim();
            let plain = before.is_empty()
                || before == "pub(crate)"
                || before == "pub"
                || before.starts_with("#[");
            let after = rest.get(item..).unwrap_or_default();
            let (Some(brace), semi) = (after.find('{'), after.find(';')) else {
                continue;
            };
            if !plain || semi.is_some_and(|semi| semi < brace) {
                continue;
            }
            let open = at.saturating_add(item).saturating_add(brace);
            out.push(at..closing(code, open));
        }
        out
    }

    #[test]
    fn guards_cover_entry_points() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        let mut dirs = vec![root.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    files.push(path);
                }
            }
        }
        files.sort();
        let mut unguarded = Vec::new();
        let mut seen = Vec::new();
        let mut found = 0_usize;
        for path in files {
            let relative = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let name = relative.rsplit('/').next().unwrap_or_default();
            if name == "tests.rs"
                || name == "testing.rs"
                || name.ends_with("_tests.rs")
                || relative.contains("/tests/")
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let mut code = blank(&source, false);
            let mut braces = blank(&source, true);
            for range in test_modules(&braces) {
                for buffer in [&mut code, &mut braces] {
                    for byte in buffer.get_mut(range.clone()).into_iter().flatten() {
                        *byte = b' ';
                    }
                }
            }
            let text = String::from_utf8_lossy(&code).into_owned();
            for (at, keyword) in text.match_indices("extern \"C\" fn") {
                let after = at.saturating_add(keyword.len());
                let rest = text.get(after..).unwrap_or_default().trim_start();
                let ident: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                    .collect();
                // A function type, not a function.
                if ident.is_empty() {
                    continue;
                }
                found = found.saturating_add(1);
                let Some(open) = braces
                    .get(after..)
                    .and_then(|tail| tail.iter().position(|&b| b == b'{'))
                    .map(|offset| after.saturating_add(offset))
                else {
                    continue;
                };
                let body = text.get(open..closing(&braces, open)).unwrap_or_default();
                let listed = NEVER_UNWINDS
                    .iter()
                    .find(|(file, name, _)| *file == relative && *name == ident);
                if let Some(entry) = listed {
                    assert!(!entry.2.is_empty(), "{relative}: {ident} needs a reason");
                    seen.push((relative.clone(), ident));
                } else if !body.contains("guarded(") {
                    unguarded.push(format!("{relative}: {ident}"));
                }
            }
        }
        assert!(found > 150, "the scan found only {found} entry points");
        assert!(
            unguarded.is_empty(),
            "every extern \"C\" fn that may unwind runs its body in `guarded` (D-697); \
             these do not, and are not in NEVER_UNWINDS: {unguarded:#?}"
        );
        let stale: Vec<_> = NEVER_UNWINDS
            .iter()
            .filter(|(file, name, _)| !seen.iter().any(|(f, n)| f == file && n == name))
            .collect();
        assert!(
            stale.is_empty(),
            "NEVER_UNWINDS names functions that do not exist: {stale:?}"
        );
    }

    /// The symbols of `cheby_mir::never_raise::NEVER_RAISE_KERNELS`, read
    /// from its source at run time (the MIR crate is not a dependency, and
    /// `include_str!` would stop this crate from compiling without the
    /// file): one `"cheby_…",` per line between the list's opening line
    /// and its `];`.
    fn never_raise_kernels() -> Vec<String> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cheby_mir/src/never_raise.rs"
        );
        let source = std::fs::read_to_string(path).unwrap_or_else(|error| {
            panic!(
                "cannot read the MIR's never-raise table {path} ({error}); \
                 `never_raise_table_matches` cross-checks it (D-767)"
            )
        });
        let open = "pub const NEVER_RAISE_KERNELS: &[&str] = &[";
        let (_, body) = source
            .split_once(open)
            .unwrap_or_else(|| panic!("{path} has no `{open}`"));
        let (list, _) = body
            .split_once("];")
            .unwrap_or_else(|| panic!("{path}: the table has no closing `];`"));
        list.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("//"))
            .map(|line| {
                line.strip_prefix('"')
                    .and_then(|rest| rest.strip_suffix("\","))
                    .unwrap_or_else(|| panic!("{path}: `{line}` is not `\"cheby_…\",`"))
                    .to_owned()
            })
            .collect()
    }

    /// D-767: every kernel of the MIR's never-raise table, whose foreign
    /// calls get no unwind edge, is a registered kernel that is on
    /// `NEVER_UNWINDS` by its exported symbol, and none can suspend (a
    /// fiber or channel kernel, a scope helper): after cancellation
    /// (D-688) a kernel that parks can raise. The list is sorted, as the
    /// MIR searches it.
    #[test]
    fn never_raise_table_matches() {
        let table = never_raise_kernels();
        assert!(!table.is_empty(), "the never-raise table is empty");
        assert!(
            table.windows(2).all(|pair| pair[0] < pair[1]),
            "the never-raise table is not sorted and unique: {table:?}"
        );
        let registered: Vec<&str> = crate::symbols().into_iter().map(|(name, _)| name).collect();
        let mut suspending = Vec::new();
        crate::fiber_ops::kernels(&mut suspending);
        let mut wrong = Vec::new();
        for symbol in &table {
            if !registered.contains(&symbol.as_str()) {
                wrong.push(format!("{symbol}: not a registered kernel"));
            }
            if !NEVER_UNWINDS.iter().any(|(_, name, _)| name == symbol) {
                wrong.push(format!("{symbol}: not on NEVER_UNWINDS"));
            }
            let can_suspend = suspending.iter().any(|(name, _)| name == symbol)
                || ["cheby_fiber_", "cheby_channel_", "cheby_rt_scope_"]
                    .iter()
                    .any(|prefix| symbol.starts_with(prefix));
            if can_suspend {
                wrong.push(format!("{symbol}: a kernel that can suspend"));
            }
        }
        assert!(
            wrong.is_empty(),
            "crates/cheby_mir/src/never_raise.rs lists kernels that may raise: {wrong:#?}"
        );
    }

    #[test]
    fn the_scanner_sees_through_comments_strings_and_test_modules() {
        let source = "fn a() { let s = \"}\"; let c = '{'; } // extern \"C\" fn b() {\n\
                      #[cfg(test)]\nmod tests { extern \"C\" fn c() { } }\n\
                      unsafe extern \"C\" fn d(x: u8) -> u8 { guarded(None, || x) }";
        let braces = blank(source, true);
        let code = blank(source, false);
        let text = String::from_utf8_lossy(&code);
        assert_eq!(text.match_indices("extern \"C\" fn").count(), 2);
        let modules = test_modules(&braces);
        assert_eq!(modules.len(), 1);
        let module = source.get(modules.first().cloned().unwrap()).unwrap();
        assert!(module.ends_with("{ } }"), "{module}");
        assert_eq!(
            closing(&braces, 7),
            source.find("} //").unwrap().saturating_add(1)
        );
    }
}
