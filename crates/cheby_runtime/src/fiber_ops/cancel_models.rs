//! A `loom` model of `fiber::cancel`'s walk (D-1060, ADR-0069) against a
//! concurrent spawn into the same scope and a member's panic, as
//! `models.rs` models the walks of D-666 and D-667: the protocol, not the
//! runtime's code. A few `loom::sync::Mutex`es and atomics stand for the
//! scope's lock and the fiber locks, and each step names the function it
//! mirrors:
//!
//! ```text
//! cargo test -p cheby_runtime fiber_ops::cancel_models
//! ```
//!
//! | Model step              | Mirrors (path under `crates/cheby_runtime/src/`, function)                              |
//! | ----------------------- | --------------------------------------------------------------------------------------- |
//! | the canceller           | `fiber_ops/cancel.rs` `cancel_spawned` → `fiber_ops/scope.rs` `cancel_spawned_step`     |
//! | its reach of a member   | `fiber_ops/cancel.rs` `reach` (scope lock, then the fiber lock)                         |
//! | reserve, fill           | `fiber_ops/scope.rs` `ScopeState::join`, `ScopeState::fill`, from `fiber_ops/spawn.rs`  |
//! | the new fiber's start   | `fiber_ops/base.rs` `fiber_main` (`cancel::check` before `f`)                           |
//! | the panicker            | `unwind/record.rs` `start_panic` → `fiber_ops/scope.rs` `ScopeState::record_panic`     |
//! | its target check        | `fiber_ops/scope.rs` `panicker_is_cancelled_here` → `fiber_ops/cancel.rs` `targeted_within` |
//!
//! loom runs each model on generator coroutines that switch stacks, so the
//! models are ignored on Windows until M6, like the fiber tests.

// The probes: `std` atomics for what a model only records, never for its
// protocol (as in `models.rs`).
use std::sync::atomic as probe;

use loom::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;

/// A fiber's cancellation target: none, or the scope `cancel` targets.
const NONE: u8 = 0;
const OWNER: u8 = 1;

/// The scope's slot for the new fiber.
const EMPTY: u8 = 0;
const RESERVED: u8 = 1;
const FILLED: u8 = 2;

/// What the scope holds under its lock.
#[derive(Debug, Default)]
struct ScopeInner {
    /// Set only by a walk that targets the scope from outside or by its
    /// failure: `cancel` never sets it (D-1060).
    cancelled: bool,
    /// Whether the member's panic was recorded as the first cause.
    first_cause: bool,
    /// The new fiber's slot.
    slot: u8,
    /// Visits of the scope's lock, numbered.
    ticket: usize,
    /// The ticket of the canceller's visit, once it ran.
    walk_at: Option<usize>,
    /// The ticket at which the new fiber's slot was filled.
    filled_at: Option<usize>,
    /// The ticket at which the member's panic started under the lock.
    panicked_at: Option<usize>,
}

impl ScopeInner {
    fn next_ticket(&mut self) -> usize {
        self.ticket = self.ticket.wrapping_add(1);
        self.ticket
    }
}

/// A fiber's lock and target (`fiber_ops/cancel.rs` `FiberLock`).
#[derive(Debug)]
struct Fiber {
    lock: Mutex<()>,
    target: AtomicU8,
    cancel_requested: AtomicBool,
}

impl Fiber {
    fn new() -> Self {
        Self {
            lock: Mutex::new(()),
            target: AtomicU8::new(NONE),
            cancel_requested: AtomicBool::new(false),
        }
    }

    /// `cancel::reach` and `cancel::set_target`, under the fiber lock.
    fn reach(&self) {
        let _fiber = self.lock.lock().unwrap();
        self.target.store(OWNER, Ordering::Relaxed);
        self.cancel_requested.store(true, Ordering::SeqCst);
    }

    /// `cancel::targeted_within`, under the fiber lock.
    fn targeted(&self) -> bool {
        let _fiber = self.lock.lock().unwrap();
        self.target.load(Ordering::Relaxed) != NONE
    }
}

/// How the panicker decides whether to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Record {
    /// `record_panic` reads the panicker's target under the scope lock
    /// (`panicker_is_cancelled_here`).
    ChecksTheTarget,
    /// The bug: only the scope's marks count, which `cancel` never sets.
    MarksOnly,
}

/// Model 6. The scope's body cancels it with `fiber::cancel` while another
/// fiber spawns into it and a member that works without an entry check
/// panics. Invariants: the scope is never marked and the body never gets a
/// target; the member always ends with the target, and its panic is
/// recorded only if it started before the walk's visit; the new fiber is
/// reached when its slot was filled before the visit, and otherwise runs
/// normally, with no target.
fn cancel_against_spawn_and_panic(record: Record) {
    loom::model(move || {
        let scope = Arc::new(Mutex::new(ScopeInner::default()));
        let body = Arc::new(Fiber::new());
        let member = Arc::new(Fiber::new());
        let new = Arc::new(Fiber::new());
        let recorded = Arc::new(probe::AtomicBool::new(false));

        let canceller = {
            let (scope, member, new) = (Arc::clone(&scope), Arc::clone(&member), Arc::clone(&new));
            thread::spawn(move || {
                // `cancel_spawned_step`: the members under the scope's
                // lock, never the owner, and no mark of the scope.
                let mut inner = scope.lock().unwrap();
                if inner.cancelled || inner.first_cause {
                    return;
                }
                let at = inner.next_ticket();
                inner.walk_at = Some(at);
                member.reach();
                // A reserved slot has no fiber yet: the spawner fills it
                // after the walk, and the fiber is spawned afterwards.
                if inner.slot == FILLED {
                    new.reach();
                }
            })
        };

        let panicker = {
            let (scope, member, recorded) = (
                Arc::clone(&scope),
                Arc::clone(&member),
                Arc::clone(&recorded),
            );
            thread::spawn(move || {
                // `start_panic` → `record_panic`: under the scope's lock,
                // then the panicker's fiber lock.
                let mut inner = scope.lock().unwrap();
                let at = inner.next_ticket();
                inner.panicked_at = Some(at);
                let reached = match record {
                    Record::ChecksTheTarget => member.targeted(),
                    Record::MarksOnly => false,
                };
                if !inner.first_cause && !inner.cancelled && !reached {
                    inner.first_cause = true;
                    recorded.store(true, probe::Ordering::SeqCst);
                }
            })
        };

        // The spawner: `ScopeState::join` reserves, the fiber is created
        // outside the lock, `ScopeState::fill` fills and reads the marks.
        {
            let mut inner = scope.lock().unwrap();
            assert_eq!(inner.slot, EMPTY);
            inner.slot = RESERVED;
        }
        let marked_at_fill = {
            let mut inner = scope.lock().unwrap();
            inner.slot = FILLED;
            let at = inner.next_ticket();
            inner.filled_at = Some(at);
            inner.cancelled || inner.first_cause
        };
        if marked_at_fill {
            new.reach();
        }
        canceller.join().unwrap();
        panicker.join().unwrap();

        let inner = scope.lock().unwrap();
        assert!(!inner.cancelled, "cancel marked its target");
        assert_eq!(
            body.target.load(Ordering::Relaxed),
            NONE,
            "cancel reached the body"
        );
        let walk_at = inner.walk_at;
        if let Some(walk_at) = walk_at {
            assert_eq!(
                member.target.load(Ordering::Relaxed),
                OWNER,
                "a member escaped the cancellation"
            );
            let panicked_at = inner.panicked_at.unwrap();
            if recorded.load(probe::Ordering::SeqCst) {
                assert!(
                    panicked_at < walk_at,
                    "a panic after the cancel reached the fiber was recorded"
                );
            }
            let filled_at = inner.filled_at.unwrap();
            let target = new.target.load(Ordering::Relaxed);
            if filled_at < walk_at {
                assert_eq!(
                    target, OWNER,
                    "a fiber spawned before the cancel escaped it"
                );
            } else if !inner.first_cause {
                assert_eq!(
                    target, NONE,
                    "a fiber spawned after the cancel was cancelled"
                );
            }
        } else {
            // The member's panic came first and failed the scope, whose own
            // walk (not modelled) cancels everything; `cancel` did nothing.
            assert!(inner.first_cause, "cancel did nothing on an open scope");
        }
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
fn cancel_kernel_model_6_the_walk_against_a_spawn_and_a_panic() {
    cancel_against_spawn_and_panic(Record::ChecksTheTarget);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; fibers on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a panic after the cancel reached the fiber was recorded")]
fn cancel_kernel_model_6_twin_recording_by_the_marks_alone() {
    cancel_against_spawn_and_panic(Record::MarksOnly);
}
