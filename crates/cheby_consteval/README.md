# cheby_consteval

Compile-time evaluation of constants (spec §4.5 and §13.9). Every command evaluates the constants it reaches in a JIT of their own on `plain` code (D-978): the driver compiles their initializers and entry thunks there, and `evaluate` runs them in dependency order, each on an entry of its own with a deadline and a memory limit (D-536), and stores the results in their slots. `images` then writes each result as an image that describes values (D-979, `cheby_runtime::consteval::image`), `release` frees the results, the evaluation JIT is dropped, and `install` puts the images into the slots of the JIT that runs code before any entry runs, with maps and sets built with the process seed (D-196); `cheby build` puts them into the program object, whose start installs them with the same routine, and `cheby check` installs nothing (`freeze` keeps its results in place). The REPL evaluates in its session JIT and `replace_with_images` installs in-process images, which share earlier entries' installed constants by address instead of copying them.

## Place in the pipeline

`cheby_consteval` depends on `cheby_jit`, `cheby_mir`, `cheby_iface`, `cheby_package`, `cheby_diag` and `cheby_runtime`. Only `cheby_driver` depends on it: `cheby check` evaluates every constant of the package after the front end (D-459, D-534), and `cheby run` and `cheby test` evaluate the constants that `main` or the tests reach before anything runs.

- Every constant is registered in a `ConstTable` by its `cb$const$` slot with its entry thunk and its own hash seed (D-538), so a map or set in a constant hashes the same on every run.
- An initializer that reads a constant whose slot is still empty forces it on the same entry, so each constant is evaluated exactly once and its value does not depend on who reached it first.
- A panic becomes a diagnostic at the constant's name (D-533): E0069 for a cycle through a function call, E0077 for a forbidden effect or a native symbol missing under `cheby check`, E0076 when the evaluation runs out of time or memory, and E0075 for any other panic. A panicked evaluation unwinds by the return flag and releases what its frames hold (D-713, D-733).
- When everything evaluated, `range.rs` walks each constant that exists on `js` for an `Int` outside the range JavaScript can represent exactly (E0078, D-540).
- The runtime side, the constant table and the forcing hook, lives in `cheby_runtime::consteval`. Evaluation stays on a dedicated thread with a fresh accounting domain per evaluation, also in M2 (D-713).

## Layout

| Module     | What it holds                                                          |
| ---------- | ---------------------------------------------------------------------- |
| `lib.rs`   | `evaluate`, `evaluate_with`, `ConstTable` handling and the diagnostics |
| `range.rs` | `ConstTypes` and the JS range check of evaluated constants             |

## Read more

- [`docs/spec/04-declarations.md`](../../docs/spec/04-declarations.md) §4.5 and [`docs/spec/13-tooling.md`](../../docs/spec/13-tooling.md) §13.9.
- [`docs/plan/m1-core.md`](../../docs/plan/m1-core.md) §7 and package E6.
- [`crates/README.md`](../README.md) for the whole crate graph.
