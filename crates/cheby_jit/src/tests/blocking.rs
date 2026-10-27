//! `@blocking` calls under the JIT (D-042, D-683, D-818):
//! hand-built wrappers whose foreign call carries the blocking flag, bound
//! to Rust kernels with [`Jit::define_symbol`], since no package supplies
//! native code before OQ-P9 (M3). The call goes through the generated shim
//! `cb$blk$<symbol>` and `cheby_rt_blocking_call`: the kernel runs on a
//! pool thread while its fiber is parked, so a one-worker runtime runs
//! other fibers meanwhile; integers, floats and narrow values cross the
//! shim in their own registers; a result of 0 is a result, not an
//! unwinding (D-734); and a cancellation that arrives during the call
//! unwinds after it returns (D-670, D-683). Without a runtime the kernel
//! runs inline (D-692).
//!
//! Each test has kernels and statics of its own, since tests run in
//! parallel in one process. A kernel never panics: a panic in an
//! `extern "C"` function aborts the process.

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Condvar, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use cheby_codegen_clif::CodegenOptions;
use cheby_mir::{CType, ForeignSig};
use cheby_runtime::{DetachedGroup, ExecCtx, Runtime, RuntimeConfig};

use super::*;

/// How long a kernel or the test waits for an event that is due at once;
/// only a broken pool or scheduler waits this long.
const PATIENCE: Duration = Duration::from_secs(10);

/// A one-shot flag that one thread sets and another waits for.
struct Signal {
    set: Mutex<bool>,
    cond: Condvar,
}

impl Signal {
    const fn new() -> Self {
        Self {
            set: Mutex::new(false),
            cond: Condvar::new(),
        }
    }

    fn set(&self) {
        *self.set.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.cond.notify_all();
    }

    /// Waits at most `limit`; whether the flag was set.
    fn wait(&self, limit: Duration) -> bool {
        let guard = self.set.lock().unwrap_or_else(PoisonError::into_inner);
        let (guard, _) = self
            .cond
            .wait_timeout_while(guard, limit, |set| !*set)
            .unwrap_or_else(PoisonError::into_inner);
        *guard
    }
}

/// Where a kernel records the thread it ran on.
struct Seen(Mutex<Option<(ThreadId, bool)>>);

impl Seen {
    const fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Records the calling thread, and whether it runs a fiber or an
    /// entry ([`cheby_runtime::entry::current_ctx`]).
    fn record(&self) {
        let here = (
            std::thread::current().id(),
            cheby_runtime::entry::current_ctx().is_some(),
        );
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(here);
    }

    fn get(&self) -> Option<(ThreadId, bool)> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A function's address, for [`Jit::define_symbol`].
macro_rules! address {
    ($f:expr, $t:ty) => {{
        let f: $t = $f;
        #[expect(clippy::as_conversions, reason = "a function's address")]
        let address = f as *const u8;
        address
    }};
}

/// `void <symbol>()`, a plain (not blocking) kernel the wrapper calls on
/// its worker before the blocking call.
fn plain_call(symbol: &str) -> Stmt {
    Stmt::Eval(Rvalue::CallForeign {
        symbol: symbol.into(),
        sig: ForeignSig {
            params: Vec::new(),
            ret: CType::Void,
        },
        args: Vec::new(),
        blocking: false,
    })
}

/// The blocking call of `symbol` with the C signature `params -> ret`.
fn blocking_call(symbol: &str, params: &[CType], ret: CType, args: Vec<Operand>) -> Rvalue {
    Rvalue::CallForeign {
        symbol: symbol.into(),
        sig: ForeignSig {
            params: params.to_vec(),
            ret,
        },
        args,
        blocking: true,
    }
}

/// `app::<item>(params…) { before(); _r = call_foreign blocking
/// "<symbol>"(params…); return box _r }`: an `@blocking` wrapper of
/// unboxed parameters, as lowering builds it (without the effect check),
/// with an optional plain call first.
fn blocking_item(
    p: &mut Prog,
    index: u32,
    item: &str,
    symbol: &str,
    params: &[CType],
    ret: CType,
    before: Option<&str>,
) -> DefId {
    let reprs: Vec<Repr> = params.iter().map(|c| c.repr().unwrap()).collect();
    let n = u32::try_from(params.len()).unwrap();
    let boxed = n.checked_add(1).unwrap();
    let ret_repr = ret.repr().unwrap();
    let mut stmts: Vec<Stmt> = before.map(plain_call).into_iter().collect();
    stmts.push(assign(
        n,
        blocking_call(symbol, params, ret, (0..n).map(op).collect()),
    ));
    stmts.push(assign(
        boxed,
        Rvalue::Box {
            value: op(n),
            from: ret_repr,
        },
    ));
    p.item(
        index,
        item,
        &reprs,
        &[ret_repr, Repr::Value],
        straight(stmts, boxed),
        &[],
    )
}

/// `p` given its unwind edges and pads (D-698), compiled with the kernels
/// `symbols` bound.
fn compile(p: Prog, symbols: &[(&str, *const u8)]) -> Jit {
    let mut jit = Jit::configured(CodegenOptions::default(), JitOptions::from_env()).unwrap();
    for &(name, address) in symbols {
        // SAFETY: every address is an `extern "C"` kernel of this file with
        // the C signature its calls use, and it lives as long as the
        // process.
        unsafe { jit.define_symbol(name, address) };
    }
    jit.compile(&p.unwound_units()).unwrap();
    jit
}

/// A missing symbol that only an `@blocking` call names, which only its
/// shim's import word reaches (D-913), is reported before finalizing,
/// never as a panic in Cranelift (D-532), and the failed batch leaves no
/// import word behind.
#[test]
fn a_missing_blocking_symbol_is_reported_before_finalizing() {
    let mut p = Prog::default();
    blocking_item(
        &mut p,
        0,
        "wait",
        "cheby_test_missing_blocking",
        &[CType::I64],
        CType::I64,
        None,
    );
    let mut jit = Jit::configured(CodegenOptions::default(), JitOptions::from_env()).unwrap();
    let err = jit.compile(&p.unwound_units()).unwrap_err();
    let JitError::UnresolvedSymbols(symbols) = &err else {
        panic!("{err}");
    };
    assert_eq!(symbols, &["cheby_test_missing_blocking"]);
    // Nothing of the failed batch is declared (K4c, GAP(record-batch-load)).
    assert_eq!(jit.declared(), 0);
}

/// A runtime with one worker: a fiber that blocked its worker would stop
/// every other fiber.
fn one_worker() -> Runtime {
    Runtime::new(&RuntimeConfig {
        threads: Some(1),
        ..RuntimeConfig::default()
    })
    .unwrap()
}

/// An entry configuration charging `jit`'s domain.
fn config(jit: &Jit) -> EntryConfig {
    EntryConfig {
        domain: jit.domain(),
        ..EntryConfig::new(EntryMode::Test)
    }
}

/// Runs `target`'s thunk as a root fiber of `runtime` and returns its
/// word.
fn run_on(runtime: &Runtime, jit: &Jit, target: DefId) -> u64 {
    let entry = jit.entry(EntryKind::Const(target)).unwrap();
    returned(entry.run_root(runtime, &config(jit)).unwrap())
}

fn assert_no_leak(jit: &Jit) {
    if cheby_runtime::LEAK_CHECK_ENABLED {
        assert_eq!(jit.domain().live_objects(), 0, "objects leaked");
    }
}

fn float(x: f64) -> Operand {
    Operand::Const(Const::Float(x.to_bits()))
}

// (a) `(Int, Float) -> Int` on a pool thread.

static T1_WORKER: Seen = Seen::new();
static T1_KERNEL: Seen = Seen::new();

extern "C" fn blocking_t1_worker() {
    T1_WORKER.record();
}

/// `n * 10 + 2` when `x` is 2.5, else -1.
extern "C" fn blocking_t1_kernel(n: i64, x: f64) -> i64 {
    T1_KERNEL.record();
    if x.to_bits() == 2.5_f64.to_bits() {
        n.wrapping_mul(10).wrapping_add(2)
    } else {
        -1
    }
}

/// The kernel of an `(Int, Float) -> Int` blocking call gets both
/// arguments in their registers and runs on a thread other than the
/// worker's, one that runs no fiber: a pool thread (D-683).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn blocking_int_float_runs_on_a_pool_thread() {
    let mut p = Prog::default();
    let f = blocking_item(
        &mut p,
        0,
        "slow",
        "blocking_t1_kernel",
        &[CType::I64, CType::F64],
        CType::I64,
        Some("blocking_t1_worker"),
    );
    let target = p.call(f, vec![Operand::Const(Const::Int(4)), float(2.5)]);
    let jit = compile(
        p,
        &[
            (
                "blocking_t1_kernel",
                address!(blocking_t1_kernel, extern "C" fn(i64, f64) -> i64),
            ),
            (
                "blocking_t1_worker",
                address!(blocking_t1_worker, extern "C" fn()),
            ),
        ],
    );
    let runtime = one_worker();
    assert_eq!(run_on(&runtime, &jit, target), abi::tag_int(42).unwrap());
    runtime.shutdown();
    let (worker, worker_fiber) = T1_WORKER.get().unwrap();
    let (kernel, kernel_fiber) = T1_KERNEL.get().unwrap();
    assert!(worker_fiber, "the plain kernel runs on the fiber's worker");
    assert_ne!(kernel, worker, "the blocking kernel ran on the worker");
    assert!(!kernel_fiber, "the blocking kernel's thread runs a fiber");
    assert_no_leak(&jit);
}

// (b) A result of 0.

extern "C" fn blocking_t2_kernel(n: i64, x: f64) -> i64 {
    if n == 7 && x.to_bits() == 0.5_f64.to_bits() {
        0
    } else {
        -1
    }
}

/// `blocking_returns_zero`: an `(Int, Float) -> Int` kernel that returns
/// 0 does not unwind, and its caller sees 0. The edge tests the flag at
/// `ctx+12`, never the result (D-734), on a runtime and inline alike.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn blocking_returns_zero() {
    let mut p = Prog::default();
    let f = blocking_item(
        &mut p,
        0,
        "zero",
        "blocking_t2_kernel",
        &[CType::I64, CType::F64],
        CType::I64,
        None,
    );
    let target = p.call(f, vec![Operand::Const(Const::Int(7)), float(0.5)]);
    let jit = compile(
        p,
        &[(
            "blocking_t2_kernel",
            address!(blocking_t2_kernel, extern "C" fn(i64, f64) -> i64),
        )],
    );
    let runtime = one_worker();
    assert_eq!(run_on(&runtime, &jit, target), abi::tag_int(0).unwrap());
    runtime.shutdown();
    assert_eq!(returned(run_const(&jit, target)), abi::tag_int(0).unwrap());
    assert_no_leak(&jit);
}

// (c) A borrowed `String`.

/// The byte length of the `String` `text`, or -1.
extern "C" fn blocking_t3_length(text: u64) -> i64 {
    // SAFETY: `text` is a live string the parked fiber holds (D-187); the
    // kernel only reads it.
    unsafe { cheby_runtime::string::bytes(text) }
        .and_then(|b| i64::try_from(b.len()).ok())
        .unwrap_or(-1)
}

/// A borrowed `String` built on the heap crosses the shim as a pointer:
/// the parked fiber holds it, so the kernel on the pool thread reads it
/// (D-187), and the wrapper drops it after the call.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn blocking_borrowed_string_length() {
    let mut p = Prog::default();
    let s = |n: u32| Operand::Const(Const::Str(StrId::from_u32(n)));
    // `length() { _0 = with_capacity(4); _1 = append(_0, "héllo, pool");
    // _2 = call_foreign blocking length(borrow _1); drop _1; return box _2 }`.
    let f = p.item(
        0,
        "length",
        &[],
        &[Repr::Value, Repr::Value, Repr::Int, Repr::Value],
        straight(
            vec![
                assign(
                    0,
                    Rvalue::CallRuntime {
                        func: RuntimeFn::StrWithCapacity,
                        args: vec![Operand::Const(Const::Int(4))],
                    },
                ),
                assign(
                    1,
                    Rvalue::CallRuntime {
                        func: RuntimeFn::StrAppend,
                        args: vec![op(0), s(1)],
                    },
                ),
                assign(
                    2,
                    blocking_call("blocking_t3_length", &[CType::Ptr], CType::I64, vec![op(1)]),
                ),
                Stmt::Drop(local(1)),
                assign(
                    3,
                    Rvalue::Box {
                        value: op(2),
                        from: Repr::Int,
                    },
                ),
            ],
            3,
        ),
        &["héllo, pool"],
    );
    let target = p.call(f, Vec::new());
    let jit = compile(
        p,
        &[(
            "blocking_t3_length",
            address!(blocking_t3_length, extern "C" fn(u64) -> i64),
        )],
    );
    let runtime = one_worker();
    let expected = i64::try_from("héllo, pool".len()).unwrap();
    assert_eq!(
        run_on(&runtime, &jit, target),
        abi::tag_int(expected).unwrap()
    );
    runtime.shutdown();
    assert_no_leak(&jit);
}

// (d) Floats and narrow values, as arguments and results.

/// 1.0 when every argument is the one the test passes, else minus the
/// position of the first that is not.
extern "C" fn blocking_t4_mixed(
    small: i8,
    wide: u16,
    int: i32,
    single: f32,
    double: f64,
    flag: bool,
) -> f64 {
    let checks = [
        small == -3,
        wide == 65_000,
        int == -100_000,
        single.to_bits() == 1.5_f32.to_bits(),
        double.to_bits() == (-2.25_f64).to_bits(),
        flag,
    ];
    match checks.iter().position(|ok| !ok) {
        None => 1.0,
        Some(0) => -1.0,
        Some(1) => -2.0,
        Some(2) => -3.0,
        Some(3) => -4.0,
        Some(4) => -5.0,
        Some(_) => -6.0,
    }
}

/// `x * 2` as an `f32` when `n` is 3, else -1.
extern "C" fn blocking_t4_f32(x: f32, n: i64) -> f32 {
    if n == 3 { x * 2.0 } else { -1.0 }
}

/// Each argument word is unpacked by its C type, so floats reach the
/// kernel in float registers and narrow integers keep their sign; an `f64`
/// and an `f32` result come back as their bit patterns (aarch64 here; the
/// gate's emulated amd64 leg checks the `SysV` registers).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn blocking_float_and_narrow_arguments_and_results() {
    let mut p = Prog::default();
    let mixed = blocking_item(
        &mut p,
        0,
        "mixed",
        "blocking_t4_mixed",
        &[
            CType::I8,
            CType::U16,
            CType::I32,
            CType::F32,
            CType::F64,
            CType::Bool,
        ],
        CType::F64,
        None,
    );
    let sized = |repr: Repr, bits: u64| Operand::Const(Const::Sized { repr, bits });
    let mixed_target = p.call(
        mixed,
        vec![
            sized(Repr::I8, u64::from((-3_i8).cast_unsigned())),
            sized(Repr::U16, 65_000),
            sized(Repr::I32, u64::from((-100_000_i32).cast_unsigned())),
            sized(Repr::F32, u64::from(1.5_f32.to_bits())),
            float(-2.25),
            Operand::Const(Const::Bool(true)),
        ],
    );
    let halves = blocking_item(
        &mut p,
        1,
        "halves",
        "blocking_t4_f32",
        &[CType::F32, CType::I64],
        CType::F32,
        None,
    );
    let halves_target = p.call(
        halves,
        vec![
            sized(Repr::F32, u64::from(0.75_f32.to_bits())),
            Operand::Const(Const::Int(3)),
        ],
    );
    let jit = compile(
        p,
        &[
            (
                "blocking_t4_mixed",
                address!(
                    blocking_t4_mixed,
                    extern "C" fn(i8, u16, i32, f32, f64, bool) -> f64
                ),
            ),
            (
                "blocking_t4_f32",
                address!(blocking_t4_f32, extern "C" fn(f32, i64) -> f32),
            ),
        ],
    );
    let runtime = one_worker();
    let word = run_on(&runtime, &jit, mixed_target);
    assert_eq!(layout_of(word), abi::layout::BOXED_FLOAT);
    assert_eq!(
        f64::from_bits(read_word(word, 8)).to_bits(),
        1.0_f64.to_bits()
    );
    release(&jit, word);
    let word = run_on(&runtime, &jit, halves_target);
    assert_eq!(layout_of(word), abi::layout::BOXED_F32);
    assert_eq!(
        read_word(word, 8) & u64::from(u32::MAX),
        u64::from(1.5_f32.to_bits())
    );
    release(&jit, word);
    runtime.shutdown();
    assert_no_leak(&jit);
}

// (e) Another fiber runs while the kernel waits.

static T5_STARTED: Signal = Signal::new();
static T5_SIGNAL: Signal = Signal::new();

/// Waits for the other fiber's signal: 1 when it came, 0 after
/// [`PATIENCE`].
extern "C" fn blocking_t5_wait(_n: i64, _x: f64) -> i64 {
    T5_STARTED.set();
    i64::from(T5_SIGNAL.wait(PATIENCE))
}

extern "C" fn blocking_t5_signal() {
    T5_SIGNAL.set();
}

/// With one worker, a fiber blocked in a kernel does not hold its worker:
/// the kernel waits for a signal that only another fiber of the runtime
/// sends, and it comes. Inline, the second fiber could not run until the
/// kernel gave up after [`PATIENCE`], and the kernel would return 0.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn blocking_lets_another_fiber_run() {
    let mut p = Prog::default();
    let wait = blocking_item(
        &mut p,
        0,
        "wait",
        "blocking_t5_wait",
        &[CType::I64, CType::F64],
        CType::I64,
        None,
    );
    let signal = p.item(
        1,
        "signal",
        &[],
        &[],
        vec![block(
            vec![plain_call("blocking_t5_signal")],
            Terminator::Return(Operand::Const(Const::Nil)),
        )],
        &[],
    );
    let wait_target = p.call(wait, vec![Operand::Const(Const::Int(0)), float(0.0)]);
    let signal_target = p.call(signal, Vec::new());
    let jit = compile(
        p,
        &[
            (
                "blocking_t5_wait",
                address!(blocking_t5_wait, extern "C" fn(i64, f64) -> i64),
            ),
            (
                "blocking_t5_signal",
                address!(blocking_t5_signal, extern "C" fn()),
            ),
        ],
    );
    let runtime = one_worker();
    let group = DetachedGroup::new();
    let waiting = jit
        .entry(EntryKind::Const(wait_target))
        .unwrap()
        .start_root_in(&runtime, &config(&jit), &group)
        .unwrap();
    assert!(T5_STARTED.wait(PATIENCE), "the kernel never started");
    // The only worker runs this root while the first one's kernel waits.
    assert_eq!(run_on(&runtime, &jit, signal_target), static_nil());
    let outcome = waiting.wait().unwrap();
    assert_eq!(
        returned(outcome),
        abi::tag_int(1).unwrap(),
        "the kernel gave up: the other fiber did not run while it waited"
    );
    runtime.shutdown();
    assert_no_leak(&jit);
}

// (f) A cancellation during the call.

/// The context of the fiber that makes the call, which the kernel watches.
static T6_CTX: AtomicPtr<ExecCtx> = AtomicPtr::new(std::ptr::null_mut());
/// The kernel saw the cancellation's request while it ran.
static T6_SAW_REQUEST: AtomicBool = AtomicBool::new(false);
/// The kernel returned.
static T6_RETURNED: AtomicBool = AtomicBool::new(false);

/// On the worker: notes the running fiber's context.
extern "C" fn blocking_t6_note() {
    if let Some(ctx) = cheby_runtime::entry::current_ctx() {
        T6_CTX.store(ctx, Ordering::SeqCst);
    }
}

/// On the pool: waits until the fiber has an interrupt requested, which
/// the timeout's cancellation does (D-688), then returns 7.
extern "C" fn blocking_t6_kernel(_n: i64, _x: f64) -> i64 {
    let start = Instant::now();
    let ctx = T6_CTX.load(Ordering::SeqCst);
    while !ctx.is_null() && start.elapsed() < PATIENCE {
        // SAFETY: the fiber is parked on this call, so its context lives
        // until the call returns; only an atomic is read.
        let limit = unsafe { (*ctx).stack_limit.load(Ordering::SeqCst) };
        if limit == usize::MAX {
            T6_SAW_REQUEST.store(true, Ordering::SeqCst);
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    T6_RETURNED.store(true, Ordering::SeqCst);
    7
}

/// In the pad: 100, plus 1 if the kernel had returned.
extern "C" fn blocking_t6_after() -> i64 {
    100_i64.wrapping_add(i64::from(T6_RETURNED.load(Ordering::SeqCst)))
}

/// `fiber::timeout(20 ms, …)` around a blocking call, written out as
/// `scope_run`'s body with the call in place of the closure (D-693),
/// precounted with its own edges:
///
/// ```text
///   entry unwind bb2
///   bb0:
///     eval call_foreign blocking_t6_note()
///     _2 = scope_open(const true, const 0, const 20000000) ; unwind bb2
///     _3 = call_foreign blocking blocking_t6_kernel(_0, _1) -> i64 ; unwind bb1
///     _4 = scope_close(_2, const false) ; unwind bb2
///     _5 = box int _3 ; unwind bb2
///     return _5
///   bb1:
///     clear_unwind
///     _6 = scope_close(_2, const true) ; unwind bb2
///     _7 = call_foreign blocking_t6_after() -> i64 ; unwind bb2
///     _8 = box int _7 ; unwind bb2
///     return _8
///   bb2:
///     resume
/// ```
#[expect(clippy::too_many_lines, reason = "one hand-built MIR function")]
fn timed_call() -> Function {
    let runtime = |func, args| Rvalue::CallRuntime { func, args };
    let mut f = function(
        FnKey::Item(def(1, 0)),
        Abi::Cheby,
        &[Repr::Int, Repr::Float],
        &[
            Repr::Value,
            Repr::Int,
            Repr::Bool,
            Repr::Value,
            Repr::Bool,
            Repr::Int,
            Repr::Value,
        ],
        Repr::Value,
        vec![
            Block {
                pos: Vec::new(),
                params: Vec::new(),
                stmts: vec![
                    plain_call("blocking_t6_note"),
                    assign(
                        2,
                        runtime(
                            RuntimeFn::ScopeOpen,
                            vec![
                                Operand::Const(Const::Bool(true)),
                                Operand::Const(Const::Int(0)),
                                Operand::Const(Const::Int(20_000_000)),
                            ],
                        ),
                    ),
                    assign(
                        3,
                        blocking_call(
                            "blocking_t6_kernel",
                            &[CType::I64, CType::F64],
                            CType::I64,
                            vec![op(0), op(1)],
                        ),
                    ),
                    assign(
                        4,
                        runtime(
                            RuntimeFn::ScopeClose,
                            vec![op(2), Operand::Const(Const::Bool(false))],
                        ),
                    ),
                    assign(
                        5,
                        Rvalue::Box {
                            value: op(3),
                            from: Repr::Int,
                        },
                    ),
                ],
                term: ret(5),
                unwind: vec![(0, bb(2)), (1, bb(2)), (2, bb(1)), (3, bb(2)), (4, bb(2))],
            },
            Block {
                pos: Vec::new(),
                params: Vec::new(),
                stmts: vec![
                    Stmt::ClearUnwind,
                    assign(
                        6,
                        runtime(
                            RuntimeFn::ScopeClose,
                            vec![op(2), Operand::Const(Const::Bool(true))],
                        ),
                    ),
                    assign(
                        7,
                        Rvalue::CallForeign {
                            symbol: "blocking_t6_after".into(),
                            sig: ForeignSig {
                                params: Vec::new(),
                                ret: CType::I64,
                            },
                            args: Vec::new(),
                            blocking: false,
                        },
                    ),
                    assign(
                        8,
                        Rvalue::Box {
                            value: op(7),
                            from: Repr::Int,
                        },
                    ),
                ],
                term: ret(8),
                unwind: vec![(1, bb(2)), (2, bb(2)), (3, bb(2))],
            },
            block(Vec::new(), Terminator::Resume),
        ],
        &[],
    );
    f.entry_unwind = Some(bb(2));
    f.precounted = true;
    f
}

/// A cancellation that reaches a fiber parked on a blocking call does not
/// abandon the call: the kernel runs to its end, and the fiber unwinds
/// after it returns, through the flag (D-670, D-683, D-734), to the
/// timeout's scope, which catches it. The kernel saw the request while it
/// ran, the pad found it returned, and nothing leaks.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn blocking_cancellation_unwinds_after_the_return() {
    let mut p = Prog::default();
    p.functions.push(timed_call());
    p.declare(def(1, 0), "timed", &[Repr::Int, Repr::Float]);
    let target = p.call(def(1, 0), vec![Operand::Const(Const::Int(1)), float(1.0)]);
    let jit = compile(
        p,
        &[
            (
                "blocking_t6_note",
                address!(blocking_t6_note, extern "C" fn()),
            ),
            (
                "blocking_t6_kernel",
                address!(blocking_t6_kernel, extern "C" fn(i64, f64) -> i64),
            ),
            (
                "blocking_t6_after",
                address!(blocking_t6_after, extern "C" fn() -> i64),
            ),
        ],
    );
    let runtime = one_worker();
    let word = run_on(&runtime, &jit, target);
    runtime.shutdown();
    assert!(
        T6_SAW_REQUEST.load(Ordering::SeqCst),
        "the timeout's cancellation did not reach the fiber during the call"
    );
    assert_eq!(
        word,
        abi::tag_int(101).unwrap(),
        "the call did not unwind to the scope after the kernel returned"
    );
    assert_no_leak(&jit);
}

// (g) Without a runtime.

static T7_ENTRY: Seen = Seen::new();
static T7_KERNEL: Seen = Seen::new();

extern "C" fn blocking_t7_entry() {
    T7_ENTRY.record();
}

extern "C" fn blocking_t7_kernel(n: i64, x: f64) -> i64 {
    T7_KERNEL.record();
    if x.to_bits() == 0.25_f64.to_bits() {
        n.wrapping_add(1)
    } else {
        -1
    }
}

/// An isolated entry has no runtime: the call runs its kernel inline on
/// the entry's thread (D-692), with the same shim and result.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn blocking_runs_inline_without_a_runtime() {
    let mut p = Prog::default();
    let f = blocking_item(
        &mut p,
        0,
        "inline",
        "blocking_t7_kernel",
        &[CType::I64, CType::F64],
        CType::I64,
        Some("blocking_t7_entry"),
    );
    let target = p.call(f, vec![Operand::Const(Const::Int(41)), float(0.25)]);
    let jit = compile(
        p,
        &[
            (
                "blocking_t7_kernel",
                address!(blocking_t7_kernel, extern "C" fn(i64, f64) -> i64),
            ),
            (
                "blocking_t7_entry",
                address!(blocking_t7_entry, extern "C" fn()),
            ),
        ],
    );
    assert_eq!(returned(run_const(&jit, target)), abi::tag_int(42).unwrap());
    let (entry, _) = T7_ENTRY.get().unwrap();
    let (kernel, kernel_entry) = T7_KERNEL.get().unwrap();
    assert_eq!(kernel, entry, "the kernel left the entry's thread");
    assert!(kernel_entry, "the kernel ran outside the entry");
    assert_no_leak(&jit);
}
