//! The kernels of `std::time` (D-985, D-1018 to D-1021, D-1057, D-1058):
//! `sys_now` and `sys_monotonic`, ordinary kernels that read a clock, and
//! the `@std_context` kernel `sys_sleep`, which waits on the runtime's
//! timer heap. `std::time::instant` has no kernel (D-1057).
//!
//! - **The wall clock** (`sys_now`) is the system's real-time clock,
//!   Rust's `SystemTime`, read as the whole seconds since the Unix epoch
//!   and the nanoseconds after them, from 0 to 999 999 999, so a point
//!   before 1970 has negative seconds: the one representation of a
//!   `Duration` (D-650, D-1021).
//! - **The monotonic clock** (`sys_monotonic`) is one process-wide origin,
//!   [`origin`], taken the first time a kernel needs it, and read through
//!   Rust's `Instant`, the clock the timer heap uses (D-1021). It is not a
//!   runtime's own timer epoch (`sched/timer.rs`), so readings taken in
//!   different runtimes of one process, such as the REPL's and a test's,
//!   are comparable (D-690).
//! - **Sleeping** (`sys_sleep`) takes the span as std passes every span,
//!   whole seconds and nanoseconds (D-682), read by
//!   [`deadline_after`](crate::sched::deadline_after): a span of zero or
//!   less returns at once, with or without a scheduler (D-652, D-692). Any
//!   other span needs the scheduler: without one the program ends with a
//!   fatal error that names `time::sleep` (D-692, D-714, D-983). With one,
//!   the fiber records its wait site as `time::sleep` (D-858, D-1058),
//!   publishes a wait header for the cancellation walk (`cancel::publish`,
//!   refused when a cancellation already reached it), arms a timer at its
//!   deadline whose target settles the header and wakes it ([`SleepTimer`]),
//!   and parks; woken, it unpublishes, cancels its timer and unwinds if the
//!   walk won the header (D-670, D-983). A span past D-891's cap
//!   (`Deadline::Never`) arms no timer, so nothing but a cancellation wakes
//!   the fiber and nothing counts as pending work: a domain whose fibers
//!   all wait, one of them in such a sleep, is deadlocked, and §10.4's
//!   report lists the fiber as `blocked in time::sleep` (D-1058). A sleep
//!   up to the cap counts as pending work through its timer (D-857) and
//!   never appears in a report. `sleep_until` is Cheby over the same
//!   kernel: it subtracts a `sys_monotonic` reading from its deadline.
//!
//! **Races** (`fiber_ops/sleep_models.rs` models them): the walk and the
//! timer race for the header with one compare-and-swap each, exactly as a
//! `select` with one timer arm and no channel (`fiber_ops/select.rs`), so
//! exactly one of them wakes the fiber. The timer's domain count falls
//! only after its firing returned, so after the wake (D-857).
//!
//! **Locks**: the fiber lock to publish and unpublish, then the timer
//! heap's leaf lock to arm; none is held across the park (I-5).
//!
//! Each kernel declares its JS binding to `./time.mjs`, written in M4, and
//! `sys_sleep` is also `@async` there (D-986).

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::fiber_ops::{cancel, needs_scheduler, running};
use crate::panic::fatal;
use crate::sched::{
    AfterSwitch, Deadline, TimerHandle, TimerTarget, WaitHeader, WaitOp, WaitState, begin_park,
    cancel_park, deadline_after, park, wake,
};
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_cancel;
use crate::values;

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &["time"];

/// The nanoseconds in a second.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// One kernel: symbol, address, C parameter types (after the context
/// pointer of a `@std_context` kernel) and C result type.
type Entry = (
    &'static str,
    *const u8,
    &'static [&'static str],
    &'static str,
);

/// The kernels of `std::time`.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
fn all() -> [Entry; 3] {
    [
        (
            "cheby_time_sys_now",
            cheby_time_sys_now as *const u8,
            &[],
            "Word",
        ),
        (
            "cheby_time_sys_monotonic",
            cheby_time_sys_monotonic as *const u8,
            &[],
            "Word",
        ),
        (
            "cheby_time_sys_sleep",
            cheby_time_sys_sleep as *const u8,
            &["i64", "i64"],
            "()",
        ),
    ]
}

/// Appends every `std::time` kernel symbol and its address to `out`
/// (plan §4.3.3).
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    out.extend(all().iter().map(|kernel| (kernel.0, kernel.1)));
}

/// The C signatures of the kernels, without the context pointer, for the
/// cross-check against `std/time.cheby` (`time_tests.rs`).
#[cfg(test)]
pub(crate) fn signatures() -> Vec<(&'static str, &'static [&'static str], &'static str)> {
    all()
        .iter()
        .map(|kernel| (kernel.0, kernel.2, kernel.3))
        .collect()
}

/// The kernels that take the running fiber's context (`@std_context`,
/// D-692).
#[cfg(test)]
pub(crate) const CONTEXT_KERNELS: [&str; 1] = ["cheby_time_sys_sleep"];

/// The monotonic clock's origin: one per process, taken the first time a
/// kernel reads the clock (D-1021).
pub(crate) fn origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

/// `span` as `(seconds, nanoseconds)`, the seconds saturated at
/// `i64::MAX` (about 292 billion years).
fn parts(span: Duration) -> (i64, i64) {
    (
        i64::try_from(span.as_secs()).unwrap_or(i64::MAX),
        i64::from(span.subsec_nanos()),
    )
}

/// The wall-clock reading `at` as the span since the Unix epoch, in
/// `Duration`'s one representation: whole seconds, negative before 1970,
/// and nanoseconds from 0 to 999 999 999 (D-650).
pub(crate) fn since_epoch(at: SystemTime) -> (i64, i64) {
    match at.duration_since(UNIX_EPOCH) {
        Ok(after) => parts(after),
        Err(before) => {
            let (seconds, nanoseconds) = parts(before.duration());
            if nanoseconds == 0 {
                (seconds.saturating_neg(), 0)
            } else {
                (
                    seconds.saturating_neg().saturating_sub(1),
                    NANOS_PER_SECOND.saturating_sub(nanoseconds),
                )
            }
        }
    }
}

/// The monotonic reading `at` as the span since [`origin`]: 0 for an
/// instant before it.
pub(crate) fn since_origin(at: Instant) -> (i64, i64) {
    parts(at.saturating_duration_since(origin()))
}

/// A reading as the Cheby tuple `(Int, Int)`, owned.
fn reading((seconds, nanoseconds): (i64, i64)) -> Word {
    values::tuple(&[values::int_word(seconds), values::int_word(nanoseconds)])
}

/// `time::now`'s kernel (`sys_now`): the wall clock now, as the whole
/// seconds since the Unix epoch and the nanoseconds after them (D-1021).
///
/// # Safety
///
/// None beyond the C ABI: it takes no argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_time_sys_now() -> Word {
    guarded(None, || reading(since_epoch(SystemTime::now())))
}

/// `time::monotonic`'s kernel (`sys_monotonic`): the monotonic clock now,
/// as the whole seconds since the process's origin and the nanoseconds
/// after them (D-1021).
///
/// # Safety
///
/// None beyond the C ABI: it takes no argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_time_sys_monotonic() -> Word {
    guarded(None, || reading(since_origin(Instant::now())))
}

/// The timer of a sleep: when it fires first, it settles the sleep's
/// header and wakes the fiber.
#[derive(Debug)]
struct SleepTimer {
    /// The sleep's header.
    header: Arc<WaitHeader>,
}

impl TimerTarget for SleepTimer {
    fn fire(&self) {
        if self.header.try_settle_arm(WaitState::Claimed, 0) {
            // SAFETY: the timer won the sleep's header, which the fiber
            // published before it parks (`begin_park` first), so the fiber
            // is parking or parked on it and nothing else wakes it: it is
            // not retired. The header lives in the `Arc` this target
            // holds, so a timer that fires late touches no dead memory,
            // and its swap fails then.
            unsafe { wake(self.header.fiber(), false) };
        }
    }
}

/// `time::sleep`'s and `time::sleep_until`'s kernel (`sys_sleep`, module
/// documentation): suspends the calling fiber for the span of `seconds`
/// and `nanoseconds` (D-652, D-682, D-983, D-1058).
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_time_sys_sleep(ctx: *mut ExecCtx, seconds: i64, nanoseconds: i64) {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe { sleep(ctx, seconds, nanoseconds) });
}

/// The body of [`cheby_time_sys_sleep`].
///
/// # Safety
///
/// As for [`cheby_time_sys_sleep`].
unsafe fn sleep(ctx: *mut ExecCtx, seconds: i64, nanoseconds: i64) {
    let deadline = deadline_after(seconds, nanoseconds);
    if deadline == Deadline::Elapsed {
        // Completes without waiting, also without a scheduler (D-652,
        // D-692).
        return;
    }
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    let Some(shared) = me.sched.runtime() else {
        needs_scheduler("time::sleep")
    };
    // The wait site (D-856, D-858, D-1058), copied from the published
    // frame before the park; cleared on every way out of the wait below.
    me.sched.note_wait(WaitOp::Sleep, me.wait_frame());
    begin_park(me);
    let header = Arc::new(WaitHeader::new(me));
    // SAFETY: the header lives in this frame's `Arc` until after
    // `unpublish` below.
    if unsafe { cancel::publish(me, &header) }.is_err() {
        // Cancelled before it waits: nothing has seen the header, and no
        // timer is armed.
        cancel_park(me);
        me.sched.clear_wait();
        raise_cancel(Some(ctx));
    }
    // An endless sleep arms nothing (D-891, D-1058): only the walk can
    // wake it, and the detector finds a domain whose fibers all wait.
    let timer = match deadline {
        Deadline::At(at) => Some(shared.arm_timer(
            at,
            Arc::new(SleepTimer {
                header: Arc::clone(&header),
            }),
        )),
        Deadline::Elapsed | Deadline::Never => None,
    };
    park(me, AfterSwitch::Park(None));
    me.sched.clear_wait();
    cancel::unpublish(me);
    if let Some(timer) = timer {
        let _: bool = TimerHandle::cancel(&timer);
    }
    match header.state() {
        WaitState::Claimed => {}
        // The walk won the header: the sleep unwinds at once (D-983).
        WaitState::Cancelled => raise_cancel(Some(ctx)),
        state => fatal(format_args!("a sleep woke in state {state:?}")),
    }
}
