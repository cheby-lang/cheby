//! `loom` models of the signal path of `process::until_interrupted`
//! (D-1073, ADR-0071, `sched/signal.rs`): the handler, the poll holder's
//! drain and walk, and scopes that arm and close meanwhile. As in
//! `fiber_ops/cancel_models.rs`, they model the protocol, not the
//! runtime's code: `loom` mutexes and atomics stand for the registry's
//! lock, the scopes' locks, the self-pipe, the handler's `FIRED` and the
//! process's disposition, and each step names the function it mirrors:
//!
//! ```text
//! cargo test -p cheby_runtime until_interrupted_model_
//! ```
//!
//! | Model step                    | Mirrors (path under `crates/cheby_runtime/src/`, function)                     |
//! | ----------------------------- | ------------------------------------------------------------------------------ |
//! | the kernel's delivery         | `SA_RESETHAND` in `sched/signal.rs` `install`, then `on_signal`'s store and `write` |
//! | the drain                     | `sched/signal.rs` `drain` (the poll holder's dispatch, `sched/io.rs`)          |
//! | the walk                      | `sched/signal.rs` `run_pending` → `fiber_ops/cancel.rs` `cancel` → `cancel_step` |
//! | the arm                       | `fiber_ops/interrupt.rs` `cheby_fiber_sys_arm_interrupt` → `sched/signal.rs` `arm` |
//! | the close                     | `fiber_ops/scope.rs` `cheby_rt_scope_close` (step 1, step 2's `disarm`, then `settle`) |
//!
//! A scope's memory is modelled by a strong count: the owner's share,
//! dropped after its close disarmed, and the walk's, taken under the
//! registry's lock (`Weak::upgrade`). A walk that touches a scope whose
//! count is 0 is a use after free.
//!
//! Each model also runs the protocol before the review's RB10-1
//! ([`Protocol::ChargesTheArmed`]) where a twin shows the loss it had: the
//! handler installed again at every arm from 0, and a signal charged to
//! whichever scopes were armed when it was drained.
//!
//! loom runs each model on generator coroutines that switch stacks, so the
//! models are ignored on Windows until M6, like the fiber tests.

// The probes: `std` atomics for what a model only records, never for its
// protocol.
use std::sync::atomic as probe;

use loom::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;

/// The process's disposition of a signal.
const DEFAULT: u8 = 0;
/// The runtime's handler, installed with `SA_RESETHAND`.
const HANDLER: u8 = 1;

/// The number of scopes a model has.
const SCOPES: usize = 2;

/// The protocol a model runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    /// `sched/signal.rs` as it is: a signal's targets are the scopes armed
    /// before it arrived, the handler is never installed again once one
    /// arrived, and a signal that cancelled only scopes that returned `Ok`
    /// takes its action again (`settle`).
    Targets,
    /// The bug (RB10-1): every arm from 0 installs, and a signal is charged
    /// to the scopes armed at the drain, with nothing settled.
    ChargesTheArmed,
}

/// How the walk holds the scopes it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Walk {
    /// `Weak::upgrade` under the registry's lock.
    Upgrades,
    /// The bug: a pointer kept past the lock, with no share.
    RawPointer,
}

/// A slot of the registry (`sched/signal.rs` `Slot`).
#[derive(Debug, Default, Clone, Copy)]
struct Slot {
    armed: bool,
    target: bool,
    reached: bool,
}

/// The first signal's progress (`sched/signal.rs` `First`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum First {
    #[default]
    Waiting,
    Walking {
        left: usize,
        stopped: bool,
    },
    Done,
}

/// The registry (`sched/signal.rs` `Registry`), under its lock.
#[derive(Debug, Default)]
struct Registry {
    slots: [Slot; SCOPES],
    /// The number of armed scopes.
    open: usize,
    /// Whether the handlers are installed.
    installed: bool,
    first: First,
}

/// A scope, under its lock.
#[derive(Debug, Default)]
struct ScopeInner {
    /// Set by the close's step 1.
    ended: bool,
    /// Set by the walk while the scope has not ended.
    cancelled: bool,
}

/// What the models share.
struct World {
    protocol: Protocol,
    walk: Walk,
    registry: Mutex<Registry>,
    scopes: [Mutex<ScopeInner>; SCOPES],
    /// Each scope's strong count: the owner's share and the walk's.
    strong: [AtomicUsize; SCOPES],
    /// The disposition of SIGINT, and of SIGTERM.
    disposition: [AtomicU8; 2],
    /// The handler's `FIRED`.
    fired: AtomicBool,
    /// The bytes in the self-pipe, one per signal number.
    pipe: [AtomicUsize; 2],
    /// Set when a signal took the default action: the process ended.
    killed: probe::AtomicBool,
    /// Set when a walk found a scope armed and not ended.
    walked_open: [probe::AtomicBool; SCOPES],
}

impl World {
    fn new(protocol: Protocol, walk: Walk) -> Self {
        Self {
            protocol,
            walk,
            registry: Mutex::new(Registry::default()),
            scopes: [
                Mutex::new(ScopeInner::default()),
                Mutex::new(ScopeInner::default()),
            ],
            strong: [AtomicUsize::new(1), AtomicUsize::new(1)],
            disposition: [AtomicU8::new(DEFAULT), AtomicU8::new(DEFAULT)],
            fired: AtomicBool::new(false),
            pipe: [AtomicUsize::new(0), AtomicUsize::new(0)],
            killed: probe::AtomicBool::new(false),
            walked_open: [probe::AtomicBool::new(false), probe::AtomicBool::new(false)],
        }
    }

    /// The kernel delivers signal `which`: with the handler installed,
    /// `SA_RESETHAND` restores the default in the same step and the
    /// handler sets `FIRED` and writes one byte (`on_signal`); otherwise
    /// the default action ends the process.
    fn deliver(&self, which: usize) {
        let disposition = &self.disposition[which];
        if disposition
            .compare_exchange(HANDLER, DEFAULT, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            if self.protocol == Protocol::Targets {
                self.fired.store(true, Ordering::SeqCst);
            }
            self.pipe[which].fetch_add(1, Ordering::SeqCst);
        } else {
            self.killed.store(true, probe::Ordering::SeqCst);
        }
    }

    /// `install`: both handlers, under the registry's lock.
    fn install(&self, registry: &mut Registry) {
        for disposition in &self.disposition {
            disposition.store(HANDLER, Ordering::SeqCst);
        }
        registry.installed = true;
    }

    /// `restore`: both defaults, under the registry's lock.
    fn restore(&self, registry: &mut Registry) {
        for disposition in &self.disposition {
            disposition.store(DEFAULT, Ordering::SeqCst);
        }
        registry.installed = false;
    }

    /// `arm`: scope `which` joins the registry, as a target of the first
    /// signal unless one arrived or was acted on; the first armed scope
    /// installs.
    fn arm(&self, which: usize) {
        let mut registry = self.registry.lock().unwrap();
        let live = match self.protocol {
            Protocol::Targets => {
                registry.first == First::Waiting && !self.fired.load(Ordering::SeqCst)
            }
            Protocol::ChargesTheArmed => true,
        };
        registry.slots[which] = Slot {
            armed: true,
            target: live,
            reached: false,
        };
        registry.open = registry.open.saturating_add(1);
        let install = match self.protocol {
            Protocol::Targets => live && !registry.installed,
            Protocol::ChargesTheArmed => registry.open == 1,
        };
        if install {
            self.install(&mut registry);
        }
    }

    /// The close of scope `which`: step 1 ends it under its lock and reads
    /// whether a walk cancelled it; step 2 disarms (the last one restores),
    /// then the owner drops its share; the scope is interrupted when the
    /// walk's mark reached a body that had not returned (`body_returns`
    /// false), else it returns `Ok` (D-671's rule); a scope the first
    /// signal's walk reached settles it. Returns whether it was interrupted
    /// and whether the walk had marked it.
    fn close(&self, which: usize, body_returns: bool) -> (bool, bool) {
        let cancelled = {
            let mut scope = self.scopes[which].lock().unwrap();
            scope.ended = true;
            scope.cancelled
        };
        let reached = {
            let mut registry = self.registry.lock().unwrap();
            let reached = registry.slots[which].reached;
            registry.slots[which] = Slot::default();
            registry.open = registry.open.saturating_sub(1);
            if registry.open == 0 && registry.installed {
                self.restore(&mut registry);
            }
            reached
        };
        self.strong[which].fetch_sub(1, Ordering::SeqCst);
        let interrupted = cancelled && !body_returns;
        if self.protocol == Protocol::Targets && reached {
            self.settle(!interrupted);
        }
        (interrupted, cancelled)
    }

    /// `settle`: the last scope the first signal's walk reached that
    /// settles, all of them `Ok`, sends the signal again.
    fn settle(&self, ok: bool) {
        let reraise = {
            let mut registry = self.registry.lock().unwrap();
            match registry.first {
                First::Walking { left, stopped } => {
                    let left = left.saturating_sub(1);
                    let stopped = stopped || !ok;
                    if left == 0 {
                        registry.first = First::Done;
                        !stopped
                    } else {
                        registry.first = First::Walking { left, stopped };
                        false
                    }
                }
                First::Waiting | First::Done => false,
            }
        };
        if reraise {
            self.killed.store(true, probe::Ordering::SeqCst);
        }
    }

    /// Takes a walk's share of scope `which`, under the registry's lock.
    fn share(&self, which: usize) -> bool {
        match self.walk {
            Walk::Upgrades => self.strong[which]
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |strong| {
                    (strong > 0).then_some(strong.saturating_add(1))
                })
                .is_ok(),
            Walk::RawPointer => true,
        }
    }

    /// `drain` then `run_pending`: takes the pipe's bytes; under the
    /// registry's lock, the first signal restores the defaults, and the
    /// walk takes a share of every armed scope that is its target; with
    /// none, or for every byte after the first, the signal takes the
    /// default action (`reraise`). Then, with no lock held, the walk visits
    /// the scopes.
    fn poll(&self) {
        let bytes = [
            self.pipe[0].swap(0, Ordering::SeqCst),
            self.pipe[1].swap(0, Ordering::SeqCst),
        ];
        let total = bytes[0].saturating_add(bytes[1]);
        if total == 0 {
            return;
        }
        let mut held = [false; SCOPES];
        let reraise = {
            let mut registry = self.registry.lock().unwrap();
            match self.protocol {
                Protocol::Targets if registry.first == First::Waiting => {
                    if registry.installed {
                        self.restore(&mut registry);
                    }
                    let mut left = 0_usize;
                    for (which, held) in held.iter_mut().enumerate() {
                        let slot = registry.slots[which];
                        if slot.armed && slot.target && self.share(which) {
                            registry.slots[which].reached = true;
                            *held = true;
                            left = left.saturating_add(1);
                        }
                    }
                    if left == 0 {
                        registry.first = First::Done;
                        total
                    } else {
                        registry.first = First::Walking {
                            left,
                            stopped: false,
                        };
                        total.saturating_sub(1)
                    }
                }
                Protocol::Targets => total,
                Protocol::ChargesTheArmed => {
                    let first = registry.installed;
                    if first {
                        self.restore(&mut registry);
                    }
                    for (which, held) in held.iter_mut().enumerate() {
                        *held = registry.slots[which].armed && self.share(which);
                    }
                    if registry.open == 0 || !first {
                        total
                    } else {
                        total.saturating_sub(1)
                    }
                }
            }
        };
        for (which, &held) in held.iter().enumerate() {
            if !held {
                continue;
            }
            assert!(
                self.strong[which].load(Ordering::SeqCst) > 0,
                "a walk reached a scope that was freed"
            );
            let mut scope = self.scopes[which].lock().unwrap();
            if !scope.ended {
                scope.cancelled = true;
                self.walked_open[which].store(true, probe::Ordering::SeqCst);
            }
            drop(scope);
            if self.walk == Walk::Upgrades {
                self.strong[which].fetch_sub(1, Ordering::SeqCst);
            }
        }
        if reraise > 0 {
            // `reraise`: the default action is back, so the process ends.
            self.killed.store(true, probe::Ordering::SeqCst);
        }
    }

    /// The end of every model: nothing armed, the handlers not installed,
    /// the defaults back and the pipe empty.
    fn check_idle(&self) {
        let registry = self.registry.lock().unwrap();
        assert!(
            registry.open == 0 && registry.slots.iter().all(|slot| !slot.armed),
            "a scope stayed armed"
        );
        assert!(!registry.installed, "the handlers stayed installed");
        for disposition in &self.disposition {
            assert_eq!(
                disposition.load(Ordering::SeqCst),
                DEFAULT,
                "the default action was not restored"
            );
        }
        assert_eq!(self.pipe[0].load(Ordering::SeqCst), 0);
    }

    /// One signal that came was either an interruption or the end of the
    /// process, never both and never neither.
    fn check_one_signal(&self, interrupted: bool) {
        let killed = self.killed.load(probe::Ordering::SeqCst);
        assert!(
            !(interrupted && killed),
            "one signal both cancelled a scope and ended the process"
        );
        assert!(interrupted || killed, "a signal was lost");
    }
}

/// Model 1. A scope arms and closes while one SIGINT arrives at any point
/// and the poll holder drains at any point; a later poll drains whatever
/// is left. Invariants: the scope is never cancelled after it ended and a
/// walk never touches it once freed; a signal that arrived while it was
/// armed either cancelled it, or, when it was drained after the close or
/// found the body returned (D-671's rule), took the default action; a
/// signal that arrived while nothing was armed took the default action; at
/// the end nothing is armed and the defaults are back.
fn signal_against_a_closing_scope(walk: Walk) {
    loom::model(move || {
        let world = Arc::new(World::new(Protocol::Targets, walk));
        let sender = {
            let world = Arc::clone(&world);
            thread::spawn(move || world.deliver(0))
        };
        let holder = {
            let world = Arc::clone(&world);
            thread::spawn(move || world.poll())
        };
        world.arm(0);
        let (interrupted, cancelled) = world.close(0, false);
        sender.join().unwrap();
        holder.join().unwrap();
        // A later poll.
        world.poll();

        world.check_idle();
        let walked = world.walked_open[0].load(probe::Ordering::SeqCst);
        assert_eq!(cancelled, walked, "the close missed the walk's mark");
        world.check_one_signal(interrupted);
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; signals on Windows are M6 (D-545, D-714)"
)]
fn until_interrupted_model_1_a_signal_against_a_closing_scope() {
    signal_against_a_closing_scope(Walk::Upgrades);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; signals on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a walk reached a scope that was freed")]
fn until_interrupted_model_1_twin_a_walk_without_a_share() {
    signal_against_a_closing_scope(Walk::RawPointer);
}

/// Model 2. The scope is armed throughout; two signals arrive one after
/// the other (SIGINT then `second`) while the poll holder drains at any
/// point; a later poll drains whatever is left. Invariants: the first
/// signal cancels the scope, and the second always ends the process,
/// whether the kernel's `SA_RESETHAND` or the drain's restore put the
/// default back first, or its byte was drained after the first's.
fn two_signals(second: usize) {
    loom::model(move || {
        let world = Arc::new(World::new(Protocol::Targets, Walk::Upgrades));
        world.arm(0);
        let sender = {
            let world = Arc::clone(&world);
            thread::spawn(move || {
                world.deliver(0);
                world.deliver(second);
            })
        };
        let holder = {
            let world = Arc::clone(&world);
            thread::spawn(move || world.poll())
        };
        sender.join().unwrap();
        holder.join().unwrap();
        world.poll();
        let (interrupted, _) = world.close(0, false);
        assert!(interrupted, "the first signal did not cancel the scope");
        assert!(
            world.killed.load(probe::Ordering::SeqCst),
            "the second signal did not end the process"
        );
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; signals on Windows are M6 (D-545, D-714)"
)]
fn until_interrupted_model_2_a_second_sigint_ends_the_process() {
    two_signals(0);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; signals on Windows are M6 (D-545, D-714)"
)]
fn until_interrupted_model_2_a_sigterm_after_a_sigint_ends_the_process() {
    two_signals(1);
}

/// Model 3 (the review's RB10-1). Two scopes arm and close one after the
/// other, as a loop of short `until_interrupted` calls does, while one
/// SIGINT arrives at any point and the poll holder drains at any point; a
/// later poll drains whatever is left. `body_returns` says whether a body
/// returns before a walk's mark reaches it, so its scope gives `Ok`.
/// Invariants: the signal either interrupted a scope or ended the process,
/// never both and never neither; at the end nothing is armed and the
/// defaults are back.
fn signal_between_two_scopes(protocol: Protocol, body_returns: bool) {
    loom::model(move || {
        let world = Arc::new(World::new(protocol, Walk::Upgrades));
        let sender = {
            let world = Arc::clone(&world);
            thread::spawn(move || world.deliver(0))
        };
        let holder = {
            let world = Arc::clone(&world);
            thread::spawn(move || world.poll())
        };
        world.arm(0);
        let (first, _) = world.close(0, body_returns);
        world.arm(1);
        let (second, _) = world.close(1, body_returns);
        sender.join().unwrap();
        holder.join().unwrap();
        world.poll();

        world.check_idle();
        world.check_one_signal(first || second);
    });
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; signals on Windows are M6 (D-545, D-714)"
)]
fn until_interrupted_model_3_a_signal_between_two_scopes() {
    signal_between_two_scopes(Protocol::Targets, false);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; signals on Windows are M6 (D-545, D-714)"
)]
fn until_interrupted_model_3_a_signal_between_two_scopes_whose_bodies_return() {
    signal_between_two_scopes(Protocol::Targets, true);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "loom models switch stacks; signals on Windows are M6 (D-545, D-714)"
)]
#[should_panic(expected = "a signal was lost")]
fn until_interrupted_model_3_twin_a_signal_charged_to_the_armed() {
    signal_between_two_scopes(Protocol::ChargesTheArmed, true);
}
