# Testing and measurement

Correctness is checked at three levels: the examples run end to end, UI tests pin every compile-time rule, and unit tests cover each crate. Performance and determinism are measured in CI from M1 (D-247, D-264).

## 1. Example goldens

Every example in [`../examples/`](../examples/) gets golden files next to its sources (D-275):

```
docs/examples/003_fizzbuzz/
  cheby.toml
  src/main.cheby
  expected.stdout       required unless the example is compile-only
  expected.stderr       optional
  expected.exit         optional, defaults to 0
  stdin                 optional input fed to the program
  args                  optional, one argument per line
  env                   optional, KEY=VALUE per line
  data files            optional, read by the program and named in args (D-415)
  test.toml             optional: skip-run, targets, milestone, reason
```

- The harness in `tests/conformance/` runs each example with `cheby run`, compares outputs and exit code, and from M3 and M4 also builds and runs it with `--target native` and `--target js`. The same goldens apply to every target (spec §1.1 principle 5).
- The harness also runs `cheby test` in each example's directory as a trial of its own, which passes when it exits with status 0 and has no goldens. It is pending until the example's milestone has ended and must pass after it. A `skip-run` example's trial stays pending until the harness gives `cheby test` the example's fixture, which its tests need as much as its run does (D-472).
- Examples that need the network (030, 031) or a C library (020) run against local fixtures: a loopback HTTP server started by the harness, and a C fixture library linked once OQ-P9 is settled. Until then they are marked `skip-run` and only checked with `cheby check` (D-282). 030 also has a git dependency, so it waits for M5 (D-446).
- A `skip-run` example's milestone names when its fixture arrives, not when `cheby check` can handle it. Before that milestone a passing check is pending, not an unexpected pass (D-445).
- Examples whose output depends on randomness or `Map` iteration order (D-093) must make their output deterministic, for example by sorting or by a fixed seed passed through `args`. Where an example cannot, the decision log gets an entry and the example is changed, never the harness.
- `test.toml` records the milestone in which an example is expected to pass. Before that it is expected-fail, and an unexpected pass fails the run, so the table in [README.md](README.md#milestones) stays accurate. `tests/milestone` names the milestone in progress: examples of earlier milestones must pass, examples of the milestone in progress are pending and only reported, and examples of later milestones must fail. Bumping the file at a milestone's exit makes its examples binding (D-288). An example that imports standard-library modules of a later tier is checked as well as run from the milestone that ships those modules, which its `test.toml` names, and until then it reports only E0038 for those imports (D-477).
- The UI and conformance harnesses read three environment variables, and CI sets none of them (D-544). `CHEBY_MILESTONE=<Mn>` overrides `tests/milestone` for one run, so with `M2` every M1 test is required, which is how M1's exit is checked before `tests/milestone` is bumped. `CHEBY_PENDING=require` turns a failing pending test into a failure, except tests whose name matches a line of the file that `CHEBY_PENDING_ALLOW` names, one glob per line with `#` comments. At the end the harness lists the pending tests that passed and the failures it allowed.
- On Windows, CI builds the workspace and runs the unit tests, and the harnesses skip the UI run tests, the conformance runs and `::test` trials and the REPL suite until M6, which brings Windows support. M1 is judged on Linux and macOS (D-545).
- _Note:_ CI runs only on pull requests and pushes to `main`, and nothing is pushed before M1's exit. So Linux is checked locally instead: the gates of waves 3, 5 and 6 of M1's back end run the build, the tests and the conformance suite in a local Linux container, as the user decided on 2026-10-10.
- `cargo xtask bless` rewrites goldens (D-292). A blessed change is reviewed like code. It refuses to bless an example whose exit code differs from `expected.exit` (0 by default), so a toolchain error never replaces good goldens. An intended change of exit code is edited by hand (D-445).

## 2. UI tests

`tests/ui/` holds small Cheby files, one rule each, grouped by spec chapter. A rule that needs several modules or its own manifest gets a package directory instead (D-471):

```
tests/ui/05-expressions/shadowing/let_shadows_import.cheby
tests/ui/05-expressions/shadowing/let_shadows_import.stderr
tests/ui/05-expressions/pipe/call_without_hole.cheby       // D-244
tests/ui/05-expressions/pipe/call_without_hole.stderr
tests/ui/06-patterns/exhaustiveness/missing_variant.cheby
```

- A file starts with a header comment that says whether it must compile, fail with the expected diagnostics, or run with expected output (D-289). The format is described in [`tests/ui/README.md`](../../tests/ui/README.md).
- A run test may have input and arguments beside it, like an example's: `<name>.stdin` is fed to `cheby run` as standard input, which is null without it, and `<name>.args` holds one argument per line, passed after `--`. In the header, `// @command: test` makes a `run-pass` or `run-fail` test run `cheby test` instead, and its goldens hold that output, and `// @flags: …` adds command-line flags after the subcommand, such as `// @flags: --deny-warnings` or `// @flags: --target js`. `tests/ui/README.md` documents every directive (D-543).
- A package test is a directory `tests/ui/<chapter>/<name>/` with `cheby.toml` and `src/**`. Its header is in `src/main.cheby`, its goldens sit beside the directory as `<name>.stderr` and `<name>.stdout`, and it cannot be `parse-fail` (D-471).
- Each example of a compile error in the spec gets a UI test once its check exists, and the golden pins the code (D-476).
- Every "must" and every warning of [§13.8](../spec/13-tooling.md#138-diagnostics-and-warnings) gets at least one test. A coverage script, `cargo xtask coverage` (D-292), lists D-numbers cited in the spec that no UI test names in its header, and the list shrinks every milestone.
- Diagnostics are rendered in a stable plain-text form for comparison, without colors or absolute paths (D-418, D-421). The harnesses run `cheby --color=never` and set `NO_COLOR=1`, so no environment can color a golden (D-467).

## 3. Unit and property tests

- Each crate has Rust unit tests.
- Runtime kernels (RRB, HAMT, strings) are property-tested against simple reference models (`Vec`, `BTreeMap`), including reuse paths with a count of one.
- The parser is property-tested for losslessness: printing the CST reproduces the input exactly, for any input, including invalid input.
- From M5, the formatter is tested for idempotence and for meaning preservation: formatted output parses to the same AST.
- The standard library's `test` blocks sit in `std/`, next to the code they test. User builds skip them, and testing std itself checks and runs them, from G4 on (D-479). Until then only driver unit tests that use stand-in std modules check how std tests are handled, and the real ones are checked by no test.
- `cargo xtask test-std [FILTER]`, which CI runs, tests std (D-542). It checks every shipped std module with its tests, then builds and runs the tests one module at a time. When a module's tests do not build together, each test is built alone to find the ones that do. A module or test that does not build is reported as `module <path> did not build: <codes>` or `test <module path> "<name>" did not build: <codes>`, is counted, and does not stop the other modules. The report ends with counts per module and in total, and the command fails if any test failed or did not build.

## 4. REPL tests

`tests/repl/` holds scripted sessions: inputs and expected outputs, covering redefinition (D-043), stale types, `let` rebinding (D-193), `:type`, `:module` and `:reload` (D-098).

A session is a transcript `tests/repl/<name>.repl`. Lines that start with `> ` or `. ` are input, and every other line is expected output. A directory `tests/repl/<name>/` with a `cheby.toml` runs its session inside that package (D-546). The harness gives `cheby repl` one pipe for standard output and standard error, so the expected output is both streams merged in the order of writes (D-548).

## 5. Parser cross-check

CI parses every example and every source file of every UI test, including each module of a package test (D-471), with both the compiler parser and `tree-sitter-cheby`, and fails if one accepts a file the other rejects (D-272). Only grammar errors count. A rule that the spec enforces and tree-sitter does not is checked by a validation pass after parsing, so the compiler parser accepts the file too (D-424). Files that test syntax errors on purpose must be rejected by both, so a `parse-fail` UI test must contain a grammar error, and the other syntax rules are `check-fail` tests. Where the compiler reports a grammar error that tree-sitter does not, the decision log lists a grammar mismatch to fix in tree-sitter. The tree-sitter grammar is pulled in at the commit pinned in `tests/tree-sitter-cheby.rev`, and bumping it is a normal change (D-290).

## 6. Determinism

CI builds the examples and the synthetic benchmark project twice, with different thread counts and from different absolute paths, and compares all artifacts bit for bit (D-264). In M1 the artifacts are the serialized interfaces and the Cranelift IR dumps. `cargo xtask check-determinism` builds every example and the benchmark projects with `--jobs 1` and with the default, from two different absolute paths, and compares their interface encodings, MIR dumps, CLIF dumps and evaluated constant values byte for byte (D-551). From M3 they are the interface artifacts, objects and executables, and from M4 the JS output.

## 7. Performance

- `bench/gen` generates synthetic Cheby projects of a given size and shape: many small modules, few large modules, deep and wide import graphs, generic-heavy code. It is the input for the compile-speed budgets, and its output depends only on size, shape and seed (D-291).
- CI reports, from M1 on, lines per second per core for a clean debug build, scaling with cores, the time from a one-function edit to `main` running, and from M6 the release-to-debug ratio (D-247, D-267). Until M6 they are reported, not enforced, so that OQ-P10 (throughput), OQ-P11 (latency) and OQ-P12 (release ratio) can revise them with real numbers.
- `cargo xtask bench-compile [--quick]` generates `bench/gen` projects of 10k, 100k and 1M lines and reports, as a Markdown table, the checking throughput, the throughput of checking with code generation, both with `--jobs 1` and the default, and the edit-to-run latency, which is a full rebuild until M3. `--quick` runs the 10k project only.
- Runtime benchmarks cover RC overhead, reuse hit rate (with the reuse report flag of D-170), fiber spawn and switch cost, channel throughput and collection kernels, and from M2 the sorted collections against a Rust tree (OQ-P13).

## 8. Fuzzing

From M1, `cargo fuzz` targets the lexer and parser (no panics, losslessness) and the whole front end (no panics on any input). The targets are `fuzz/fuzz_targets/lex.rs`, `parse.rs` and `check.rs`. `cargo-fuzz` needs a nightly toolchain, so CI does not run them yet, and `check` has had only short local runs. From M2, a differential fuzzer generates well-typed, terminating programs and compares JIT output with expected values computed by a reference evaluator in the test crate. From M4 it compares native with JS output.

## 9. Sanitizers and leak checks

- Debug builds of the runtime count live objects, and the harness fails any example that ends with live objects that are not documented runtime-object cycles (D-029).
- The harnesses start every `cheby` with `CHEBY_RT_POISON=1`. A toolchain with debug assertions then poisons freed objects, keeps them in a bounded quarantine and stops with a fatal "use after free" error when one is used again (D-544).
- `cheby test` in a toolchain with debug assertions counts live objects before and after each passing test, and a difference fails that test (D-541).
- The runtime runs under Miri where possible and under AddressSanitizer and ThreadSanitizer in a nightly job, from M2.
