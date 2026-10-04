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
  test.toml             optional: skip-run, targets, milestone, reason
```

- The harness in `tests/conformance/` runs each example with `cheby run`, compares outputs and exit code, and from M3 and M4 also builds and runs it with `--target native` and `--target js`. The same goldens apply to every target (spec §1.1 principle 5).
- Examples that need the network (030, 031) or a C library (020) run against local fixtures: a loopback HTTP server started by the harness, and a C fixture library linked once OQ-P9 is settled. Until then they are marked `skip-run` and only checked with `cheby check` (D-282).
- Examples whose output depends on randomness or `Map` iteration order (D-093) must make their output deterministic, for example by sorting or by a fixed seed passed through `args`. Where an example cannot, the decision log gets an entry and the example is changed, never the harness.
- `test.toml` records the milestone in which an example is expected to pass. Before that it is expected-fail, and an unexpected pass fails the run, so the table in [README.md](README.md#milestones) stays accurate.
- `cargo xtask bless` rewrites goldens. A blessed change is reviewed like code.

## 2. UI tests

`tests/ui/` holds small Cheby files, one rule each, grouped by spec chapter:

```
tests/ui/05-expressions/shadowing/let_shadows_import.cheby
tests/ui/05-expressions/shadowing/let_shadows_import.stderr
tests/ui/05-expressions/pipe/call_without_hole.cheby       // D-244
tests/ui/05-expressions/pipe/call_without_hole.stderr
tests/ui/06-patterns/exhaustiveness/missing_variant.cheby
```

- A file starts with a header comment that says whether it must compile, fail with the expected diagnostics, or run with expected output.
- Every "must" and every warning of [§13.8](../spec/13-tooling.md#138-diagnostics-and-warnings) gets at least one test. A coverage script lists D-numbers cited in the spec that no UI test names in its header, and the list shrinks every milestone.
- Diagnostics are rendered in a stable plain-text form for comparison, without colors or absolute paths.

## 3. Unit and property tests

- Each crate has Rust unit tests.
- Runtime kernels (RRB, HAMT, strings) are property-tested against simple reference models (`Vec`, `BTreeMap`), including reuse paths with a count of one.
- The parser is property-tested for losslessness: printing the CST reproduces the input exactly, for any input, including invalid input.
- From M5, the formatter is tested for idempotence and for meaning preservation: formatted output parses to the same AST.

## 4. REPL tests

`tests/repl/` holds scripted sessions: inputs and expected outputs, covering redefinition (D-043), stale types, `let` rebinding (D-193), `:type`, `:module` and `:reload` (D-098).

## 5. Parser cross-check

CI parses every example and every UI test with both the compiler parser and `tree-sitter-cheby`, and fails if one accepts a file the other rejects (D-272). Files that test syntax errors on purpose must be rejected by both. The tree-sitter grammar is pulled in at a pinned commit, and bumping it is a normal change.

## 6. Determinism

CI builds the examples and the synthetic benchmark project twice, with different thread counts and from different absolute paths, and compares all artifacts bit for bit (D-264). In M1 the artifacts are the serialized interfaces and the Cranelift IR dumps. From M3 they are the interface artifacts, objects and executables, and from M4 the JS output.

## 7. Performance

- `bench/gen` generates synthetic Cheby projects of a given size and shape: many small modules, few large modules, deep and wide import graphs, generic-heavy code. It is the input for the compile-speed budgets.
- CI reports, from M1 on, lines per second per core for a clean debug build, scaling with cores, the time from a one-function edit to `main` running, and from M6 the release-to-debug ratio (D-247, D-267). Until M6 they are reported, not enforced, so that OQ-P10 can revise them with real numbers.
- Runtime benchmarks cover RC overhead, reuse hit rate (with the reuse report flag of D-170), fiber spawn and switch cost, channel throughput and collection kernels.

## 8. Fuzzing

From M1, `cargo fuzz` targets the lexer and parser (no panics, losslessness) and the type checker (no panics on any parsed input). From M2, a differential fuzzer generates well-typed, terminating programs and compares JIT output with expected values computed by a reference evaluator in the test crate. From M4 it compares native with JS output.

## 9. Sanitizers and leak checks

- Debug builds of the runtime count live objects, and the harness fails any example that ends with live objects that are not documented runtime-object cycles (D-029).
- The runtime runs under Miri where possible and under AddressSanitizer and ThreadSanitizer in a nightly job, from M2.
