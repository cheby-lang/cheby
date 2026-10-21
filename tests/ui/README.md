# UI tests

One test per rule, grouped by spec chapter (D-275, [testing.md](../../docs/plan/testing.md#2-ui-tests)). A test is a single `.cheby` file, or a directory holding a whole package when the rule needs several modules or its own `cheby.toml`. Each test starts with `// @key: value` directives:

| Directive    | Meaning                                                                                                                                    |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `@mode`      | `check-pass`, `check-fail`, `parse-fail`, `run-pass` or `run-fail` (required)                                                              |
| `@decisions` | decision-log entries the test pins, such as `D-244, D-245`, read by `cargo xtask coverage`                                                 |
| `@milestone` | milestone in which the test must pass, `M1` by default                                                                                     |
| `@exit`      | expected exit code of a `run-fail` test, `101` by default                                                                                  |
| `@command`   | `test` makes a run test use `cheby test` (D-543), `build` makes it use `cheby build` and run the executable (D-1026); `run` is the default |
| `@flags`     | words added after the subcommand in every mode, split at whitespace, such as `--deny-warnings` or `--target js`                            |

Goldens sit next to the test: `.stderr` for diagnostics (required for `check-fail` and `parse-fail`) and `.stdout` for program output. `parse-fail` files must also be rejected by `tree-sitter-cheby` (`cargo xtask cross-check`). Record goldens with `cargo xtask bless`.

`// @flags: …` exercises the command-line flags of D-549, D-550 and D-543: `// @flags: --deny-warnings` turns every warning into an error, so a `check-fail` test's `.stderr` shows `error[W…]`, and a run test that warns is `run-fail` with `// @exit: 1` and no `.stdout`; `// @flags: --target js` checks for one target only (`check` modes); `// @flags: --module ui_test::util` selects tests (with `@command: test`). The flags follow the subcommand and come before the program's arguments of a `name.args` file, which are passed after `--`.

`// @flags: --reuse-report` makes `cheby run` or `cheby test` print the reuse report (D-717) to standard error before the program runs, so the test's `.stderr` holds its lines. The report describes Perceus counting, so the harness runs a test whose flags include `--reuse-report` with `CHEBY_RC` removed from its environment. Every other test inherits the harness's `CHEBY_RC`, so `CHEBY_RC=naive cargo test -p cheby_cli --test ui`, which CI's `rc-naive` job runs, checks the suite under naive counting (D-718, D-746).

With `// @command: test`, the goldens hold the output of `cheby test` (D-541): its report on standard output, and warnings or compile errors on standard error. A test that fails makes `cheby test` exit with 1, so such a UI test is `run-fail` with `// @exit: 1`; a compile error also exits with 1. A `name.stdin` file works as for `cheby run`; `cheby test` takes no program arguments, so a `name.args` file beside such a test is an error.

With `// @command: build`, a `run-pass` or `run-fail` test builds its package with `cheby build`, then runs the executable, and compares the same goldens as a run test (D-1026). It pins what `cheby build` reports, such as E0053 at build time, the names E0055 reserves and E0036's errors in the `[native]` table. The UI runner reads it from M3 on.

## Input of run tests

A `run-pass` or `run-fail` test may have input files next to it, named like its goldens (D-543):

| File         | Meaning                                                                                                 |
| ------------ | ------------------------------------------------------------------------------------------------------- |
| `name.stdin` | fed to `cheby run` as standard input, byte for byte; without it, standard input is null (end of input)  |
| `name.args`  | the program's arguments, one per line (an empty line is an empty argument), passed after `--`           |
| `name.env`   | `KEY=VALUE` lines applied to the child's environment, in the format of an example's `env` file (D-1043) |

Input is data, so it sits beside the test like a golden, and `cargo xtask bless` never writes it. A test of another mode with an input file is an error. For a package test the files sit beside the directory, as `name.stdin`, `name.args` and `name.env`. Every test without a `.env` file that sets it runs with `CHEBY_BACKTRACE` removed from its environment, so a developer's own setting never changes a golden (D-1043).

## Single-file tests

The harness copies `name.cheby` into a temporary package as `src/main.cheby`, with a generated `cheby.toml` named `ui_test`. Goldens are `name.stderr` and `name.stdout`, and they show the file as `tests/ui/<chapter>/name.cheby`.

## Package tests

A directory under a chapter that holds a `cheby.toml` is one test, not a chapter:

```text
tests/ui/07-modules-and-packages/
  private_in_sibling/
    cheby.toml           test input, copied as it is
    src/main.cheby       the root module, which holds the directives
    src/greeting.cheby   any other modules
  private_in_sibling.stderr
```

- The harness copies the whole directory, except hidden entries, into a temporary package and runs `cheby check` or `cheby run` there. The package's `cheby.toml` is input like any module, not a configuration of the test: the directives in `src/main.cheby` are the only configuration.
- The directives go in `src/main.cheby`. Another file that starts with a directive is an error.
- Goldens sit beside the directory, as `name.stderr` and `name.stdout`, and show each file as `tests/ui/<chapter>/name/src/….cheby` (or `…/name/cheby.toml`).
- Every `.cheby` file of a package test must parse with both parsers, so a package test cannot be `parse-fail`. Write a syntax error as a single-file test.
- A file and a directory with the same name are an error, since they would share goldens.

Run with `cargo test --test ui`.

## Milestones and strict runs

`tests/milestone` names the milestone in progress (D-288). A test of an earlier milestone must pass, a test of the milestone in progress is pending and only reported, and a test of a later milestone must fail. Both harnesses, this one and the conformance harness (`cargo test --test conformance`), read three environment variables, and CI sets none of them (D-544):

| Variable                     | Effect                                                                                                                  |
| ---------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `CHEBY_MILESTONE=<Mn>`       | overrides `tests/milestone` for this run, so with `M2` every M1 test is required                                        |
| `CHEBY_PENDING=require`      | a failing pending test is a failure, unless the allow list names it; `report` or no value keeps D-288's behavior        |
| `CHEBY_PENDING_ALLOW=<file>` | the allow list of a strict run: one glob per line, the first word of the line, with `#` comments and free text after it |

Every `cheby` the harnesses start runs with `CHEBY_RT_POISON=1`, so a toolchain with debug assertions poisons freed objects and stops with a fatal "use after free" error when one is used again.

`CHEBY_HARNESS_SCALE=<n>`, a whole number from 1 to 100 (1 by default), multiplies the timeouts of this harness (180 s), the conformance harness (60 s) and the REPL harness (60 s), for slow machines, emulators and sanitizer builds (D-722). Any other value is a harness error, and the harness exits with status 2.

The conformance harness also runs every example's run trial and `::test` trial with `CHEBY_THREADS=1` and `CHEBY_THREADS=2`, as `<name>@threads=1` and `<name>::test@threads=2`, with the same goldens and the same milestone (D-722). The base trial runs with `CHEBY_THREADS` removed from the environment. When the harness itself runs with `CHEBY_THREADS=<n>` and n is neither 1 nor 2, as in `CHEBY_THREADS=4 cargo test -p cheby_cli --test conformance`, every run trial and `::test` trial also runs as `<name>@threads=<n>`. A value that is not a whole number of at least 1 is a harness error, and the harness exits with status 2. The harness prints the thread counts it runs. A `skip-run` example gets no variants, and `cargo xtask bless` ignores the variants, so only the base trial writes goldens. Unless `--test-threads` is given, the conformance harness runs at most half as many trials at once as the machine has hardware threads. An allow-list pattern such as `005_prime_sieve*` covers an example's trials and their variants.

Every run trial and `::test` trial of an example that is run also runs under naive reference counting, as `<name>@rc=naive` and `<name>::test@rc=naive`, with `CHEBY_RC=naive`, the same goldens and the same milestone, at the default thread count (D-718). Every other trial removes `CHEBY_RC` from the child's environment, so neither an example's `env` file nor the developer's shell changes how a trial counts. `cargo xtask bless` ignores these variants too.

In an allow-list pattern, `*` matches any run of characters (`/` and `:` included), `?` matches one character, and every other character matches itself. The rest of the line is a note, such as the reason a test is carried and the gate it must pass by:

```text
# Carried tests.
05-expressions/case/*      # W2-B: decision trees, gate 2
024_two_sum::test          needs dictionaries (W3-A), gate 3
```

Only a test pending because its milestone is the one in progress is made strict. The conformance trials that are pending at every milestone stay report-only: a `skip-run` example's check before its milestone (D-445), and an example's `::test` trial before its milestone or without its fixture (D-472). A test of a milestone later than `tests/milestone` that `CHEBY_MILESTONE` made pending also stays report-only: `CHEBY_MILESTONE=M2` requires every M1 test, while the M2 tests may still fail at M1's exit. The allow list never excuses a test that must pass or must fail.

At the end of a run each harness prints how many pending tests passed, then lists them by outcome: `passed`, `allowed failure` (with the pattern that allowed it), `required, failed` (strict runs only) and `still failing` (report-only). A passing test that is still on the allow list can be removed from it.

```sh
CHEBY_PENDING=require CHEBY_PENDING_ALLOW=carried.txt cargo test -p cheby_cli --test ui --test conformance
CHEBY_MILESTONE=M2 cargo test -p cheby_cli --test conformance
```
