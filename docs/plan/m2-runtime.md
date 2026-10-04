# M2: Runtime

Goal: Perceus, fibers on an M:N scheduler, structured concurrency, channels, selectors, cancellation, unwinding and non-blocking IO, so that every concurrent example runs on the JIT (D-099, step 2).

Prerequisites: M1 done. Tier-2 stdlib specified. OQ-P7 (context switch) and OQ-P8 (poller) settled.

Detail level: workstreams with outcomes. This file is refined into tasks when M2 starts.

## Workstreams

### 2.1 Perceus

- Borrowing inference for calls inside a module (D-072, D-250), run as a per-module step (D-263). Calls across modules keep the owned convention.
- Reuse analysis: `drop` plus an allocation of the same size becomes `drop_reuse`/`reuse`, covering record updates, pattern-and-rebuild in one arm, and kernel calls with a count of one (§9.3).
- Dup/drop fusion and specialization of `drop` for known constructors, on the MIR, before Cranelift (D-262).
- The reuse report flag (D-170).
- Outcome: the RC benchmarks of [testing.md](testing.md#7-performance) improve measurably, the leak counter stays at zero, and reuse never changes a program's output (differential fuzzing against M1's naive RC).

### 2.2 Shared values

- The shared mark in the object header, atomic count operations for shared objects, and the marking traversal when a value is sent, captured by a spawned fiber or stored in a global (D-072, ADR-0024).
- Shared objects are never reused in place.
- Outcome: ThreadSanitizer is clean on the concurrent examples.

### 2.3 Unwinding

- `try_call` landing pads with cleanup blocks shared between calls (D-077, D-262).
- Handle drop functions run once, on the releasing fiber, and a panic inside one aborts the process (D-101, D-171).
- The REPL switches from the interim behavior of D-283 to unwinding the entry's fiber.
- Outcome: a panic in any example frees everything the fiber held (leak counter at zero).

### 2.4 Fibers and scheduling

- Stack regions with fixed reservations, 8 MiB by default and more for the root fiber, configurable per spawn and at build time, pages returned with `madvise` on reuse (D-124, D-125, ADR-0032).
- The combined yield and stack-limit check at every function entry (D-028).
- M:N work-stealing scheduler, one thread per core by default, overridable through an environment variable and `std::runtime` (D-012, D-175).
- Scopes, `spawn`, `join`, detached fibers, the first-panic rule and `fiber::Panic` (D-040, D-073, D-176, D-177, D-178).
- Cancellation delivered at suspension points and entry checks, unwinding like a panic and uncatchable (D-102, ADR-0029).
- Timeouts with `TimedOut` and `Panicked` (D-103, D-181).
- Program exit: wait for `main`'s scopes, kill detached fibers without unwinding (D-073).

### 2.5 Channels and selectors

- Bounded channels with capacity 0 handoff, `send` returning `Err(value)` and `receive` returning `Err(Nil)` (D-054, D-087, D-179).
- Closing when the last `Sender` or `Receiver` is dropped, through handle reference counting (D-054).
- Selectors with receive and timer arms, closed-channel arms, fair choice, and a waiting selector counting as a receiver (D-041, D-180, D-206).

### 2.6 IO

- Poller for sockets and timers, per OQ-P8 (D-042).
- Blocking thread pool for file IO and `@blocking` foreign calls (D-042, §12.7).
- `std::io` functions from M1 move onto the poller or the blocking pool without API changes.

### 2.7 Runtime-object cycles

- Debug builds report leaked runtime objects at exit, and the conformance harness checks the report (D-029).

### 2.8 Tests in parallel

- `cheby test` runs each test in its own fiber and scope, in parallel (§13.6).

## Exit criteria

- 005, 009, 013, 018, 019, 022 and 028 pass on the JIT, in addition to the M1 examples.
- Every example passes with 1, 2 and the default number of scheduler threads.
- ThreadSanitizer and AddressSanitizer jobs are clean on the conformance suite.
- Reuse is reported for the record-update and kernel cases of §9.3 in the examples.
- The tier-2 stdlib is implemented on native.
