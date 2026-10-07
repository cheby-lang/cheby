# UI tests

One test per rule, grouped by spec chapter (D-275, [testing.md](../../docs/plan/testing.md#2-ui-tests)). A test is a single `.cheby` file, or a directory holding a whole package when the rule needs several modules or its own `cheby.toml`. Each test starts with `// @key: value` directives:

| Directive    | Meaning                                                                                    |
| ------------ | ------------------------------------------------------------------------------------------ |
| `@mode`      | `check-pass`, `check-fail`, `parse-fail`, `run-pass` or `run-fail` (required)              |
| `@decisions` | decision-log entries the test pins, such as `D-244, D-245`, read by `cargo xtask coverage` |
| `@milestone` | milestone in which the test must pass, `M1` by default                                     |
| `@exit`      | expected exit code of a `run-fail` test, `101` by default                                  |

Goldens sit next to the test: `.stderr` for diagnostics (required for `check-fail` and `parse-fail`) and `.stdout` for program output. `parse-fail` files must also be rejected by `tree-sitter-cheby` (`cargo xtask cross-check`). Record goldens with `cargo xtask bless`.

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
