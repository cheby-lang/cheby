# UI tests

One `.cheby` file per rule, grouped by spec chapter (D-275, [testing.md](../../docs/plan/testing.md#2-ui-tests)). Each file starts with `// @key: value` directives:

| Directive    | Meaning                                                                                   |
| ------------ | ----------------------------------------------------------------------------------------- |
| `@mode`      | `check-pass`, `check-fail`, `parse-fail`, `run-pass` or `run-fail` (required)             |
| `@decisions` | decision-log entries the test pins, such as `D-244, D-245`, read by `cargo xtask coverage` |
| `@milestone` | milestone in which the test must pass, `M1` by default                                    |
| `@exit`      | expected exit code of a `run-fail` test, `101` by default                                 |

Goldens sit next to the file: `.stderr` for diagnostics (required for `check-fail` and `parse-fail`) and `.stdout` for program output. `parse-fail` files must also be rejected by `tree-sitter-cheby` (`cargo xtask cross-check`). Record goldens with `cargo xtask bless`.

Run with `cargo test --test ui`.
