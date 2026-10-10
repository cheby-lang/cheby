# UI tests

One test per rule, grouped by spec chapter (D-275, [testing.md](../../docs/plan/testing.md#2-ui-tests)). A test is a single `.cheby` file, or a directory holding a whole package when the rule needs several modules or its own `cheby.toml`. Each test starts with `// @key: value` directives:

| Directive    | Meaning                                                                                                            |
| ------------ | ------------------------------------------------------------------------------------------------------------------ |
| `@mode`      | `check-pass`, `check-fail`, `parse-fail`, `run-pass` or `run-fail` (required)                                      |
| `@decisions` | decision-log entries the test pins, such as `D-244, D-245`, read by `cargo xtask coverage`                         |
| `@milestone` | milestone in which the test must pass, `M1` by default                                                             |
| `@exit`      | expected exit code of a `run-fail` test, `101` by default                                                          |
| `@command`   | `test` makes a `run-pass` or `run-fail` test run `cheby test` instead of `cheby run` (D-543); `run` is the default |
| `@flags`     | words added after the subcommand in every mode, split at whitespace, such as `--deny-warnings` or `--target js`    |

Goldens sit next to the test: `.stderr` for diagnostics (required for `check-fail` and `parse-fail`) and `.stdout` for program output. `parse-fail` files must also be rejected by `tree-sitter-cheby` (`cargo xtask cross-check`). Record goldens with `cargo xtask bless`.

`// @flags: …` exercises the command-line flags of D-549, D-550 and D-543: `// @flags: --deny-warnings` turns every warning into an error, so a `check-fail` test's `.stderr` shows `error[W…]`, and a run test that warns is `run-fail` with `// @exit: 1` and no `.stdout`; `// @flags: --target js` checks for one target only (`check` modes); `// @flags: --module ui_test::util` selects tests (with `@command: test`). The flags follow the subcommand and come before the program's arguments of a `name.args` file, which are passed after `--`.

With `// @command: test`, the goldens hold the output of `cheby test` (D-541): its report on standard output, and warnings or compile errors on standard error. A test that fails makes `cheby test` exit with 1, so such a UI test is `run-fail` with `// @exit: 1`; a compile error also exits with 1. A `name.stdin` file works as for `cheby run`; `cheby test` takes no program arguments, so a `name.args` file beside such a test is an error.

## Input of run tests

A `run-pass` or `run-fail` test may have input files next to it, named like its goldens (D-543):

| File         | Meaning                                                                                                |
| ------------ | ------------------------------------------------------------------------------------------------------ |
| `name.stdin` | fed to `cheby run` as standard input, byte for byte; without it, standard input is null (end of input) |
| `name.args`  | the program's arguments, one per line (an empty line is an empty argument), passed after `--`          |

Input is data, so it sits beside the test like a golden, and `cargo xtask bless` never writes it. A test of another mode with an input file is an error. For a package test the files sit beside the directory, as `name.stdin` and `name.args`.

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
