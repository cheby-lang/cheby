---
status: accepted; refined by D-820, D-833, D-857, D-859
date: 2026-10-11
log: D-723
---

# Parallel tests with ordered output

`cheby test` runs tests in parallel. Each test is a root fiber with its own scope and its own accounting domain, on the run's one runtime. At most the thread count of tests run at once.

A test's standard output and error are captured, and every fiber it spawns inherits the capture. When a test finishes, its captured text and then its result line are printed as soon as every earlier test has been printed, in module path and then source order. The output is therefore exactly the sequential output of M1.

Detached fibers that a test spawns end with the test, in every build. A test is a root like `main` (D-040, D-073), so its detached fibers are torn down when it finishes: they are cancelled and unwound before the test's leak check, and no user code runs while they unwind (ADR-0029).

## Considered options

- Printing in completion order: the goldens would be nondeterministic.
- Parallel output without capture: the tests' lines would interleave.
- Staying sequential: §13.6 and the plan ask for parallel tests.
- Freezing a test's detached fibers instead of unwinding them: their wait nodes and timers would stay linked while other tests keep running on the same workers.

## Consequences

- M1's output and the UI goldens of `cheby test` do not change.
- §13.6 and §10.4 state that a test's detached fibers end when the test ends.
- A test's leak check reads its own domain after teardown, so it needs no quiescence of other tests.
- A test that deadlocks fails with the deadlock report, and its fibers are torn down the same way (D-724). How the deadlock of one test is detected while other tests run is open (OQ-13-8).
