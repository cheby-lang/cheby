# M0: Foundation

Goal: everything M1 needs before the first line of compiler code, so that every later change lands in a repository that already builds, tests and measures itself.

Prerequisites: none. The toolchain and CI are fixed by D-277.

## Tasks

| #    | Task                                                                                                                                                                                      | Done when                                                           |
| ---- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------- |
| 0.1  | Cargo workspace with the crate skeletons of [architecture.md](architecture.md#1-repository-layout), `rust-toolchain.toml` pinned to 1.98 with edition 2024 (D-277), licence files (D-089) | `cargo build` and `cargo test` pass on an empty workspace           |
| 0.2  | GitHub Actions on macOS, Linux and Windows, x86_64 and aarch64, on every pull request (D-074, D-277): build, test, `clippy`, `rustfmt`                                                    | a pull request runs the full matrix                                 |
| 0.3  | `cheby_cli` skeleton with every command of [§13.1](../spec/13-tooling.md#131-the-cheby-command), unimplemented ones reporting so                                                          | `cheby --help` lists all commands                                   |
| 0.4  | Golden files for all 32 examples (D-275), with `test.toml` naming the milestone each must pass in                                                                                         | every example has `expected.stdout` or a recorded `skip-run` reason |
| 0.5  | Make example output deterministic where it depends on randomness or map order ([testing.md](testing.md#1-example-goldens))                                                                | goldens do not depend on run order; any example change is logged    |
| 0.6  | Conformance harness in `tests/conformance/`, with expected-fail tracking                                                                                                                  | the harness runs and reports all examples as expected-fail          |
| 0.7  | UI-test harness in `tests/ui/` and the D-number coverage script                                                                                                                           | a trivial UI test passes; coverage lists every D-number             |
| 0.8  | Tree-sitter cross-check job, pinned to a `tree-sitter-cheby` commit ([testing.md](testing.md#5-parser-cross-check))                                                                       | the job parses every example with tree-sitter and reports results   |
| 0.9  | `bench/gen` synthetic project generator and a CI job that reports (not enforces) its results                                                                                              | a 1M-line project can be generated deterministically                |
| 0.10 | `xtask`: `bless`, `gen-bench`, `check-determinism`                                                                                                                                        | each command runs                                                   |
| 0.11 | Tier-1 stdlib spec files in `docs/stdlib/` ([stdlib.md](stdlib.md#tier-1-before-m1)), decisions logged                                                                                    | spec reviewed; examples renamed to match it                         |

Tasks 0.4 and 0.5 depend on 0.11: an example's expected output can only be written once the functions it calls are specified.

## Exit criteria

- CI is green on the platform matrix and runs the conformance, UI, cross-check and bench jobs.
- All tier-1 stdlib modules are specified.
