# AGENTS.md

Guidance for coding agents working in this repository.

## What this repository is

The home of the Cheby programming language and its reference implementation, in the phase before 1.0. Milestone M1, the single-fiber core, is done (see the `Status` section of `docs/plan/m1-core.md`). M2, the runtime, is next, and `tests/milestone` names it.

Design documents:

- `docs/decisions/LOG.md`: the decision log, `D-001` onward. This is the source of truth.
- `docs/adr/`: ADRs for the significant decisions, `NNNN-kebab-slug.md`.
- `docs/spec/`: the language specification, one chapter per file, with the grammar in Appendix A.
- `docs/stdlib/`: the standard-library specification, one file per module (D-220).
- `docs/plan/`: the implementation plan, milestones M0-M6. Each milestone file has a `Status` section.
- `docs/examples/`: 32 example programs with their goldens, the conformance suite (D-275).
- `docs/diagnostics/`: one explanation per diagnostic code, `E0001.md` and `W0001.md` onward (D-420).

Implementation:

- `crates/`: the Cargo workspace, 20 `cheby_*` crates (D-270, D-284). [`docs/plan/architecture.md`](docs/plan/architecture.md) describes each crate and the compiler pipeline.
- `std/`: the standard library's Cheby sources, with their `test` blocks next to the code (D-479).
- `tests/`: the conformance, UI and REPL harnesses and their inputs, `tests/support` (shared harness code), `tests/milestone` and `tests/tree-sitter-cheby.rev`.
- `xtask/`: developer commands, run as `cargo xtask <command>`.
- `bench/gen`: the synthetic project generator for compile-speed budgets (D-291).
- `fuzz/`: `cargo fuzz` targets. They need a nightly toolchain and are outside the workspace.

Sibling repositories, usually checked out next to this one:

- `../tree-sitter-cheby`: the editor grammar and highlight queries.
- `../zed-cheby`: the Zed extension, which copies the highlight queries and pins a grammar commit.

## Source of truth

1. **Decision log** wins. The spec is derived from it and cites decisions as `(D-NNN)`.
2. **Spec** next. Appendix A wins over grammar snippets in the chapters, and a disagreement between them is a spec bug.
3. **Examples** must follow the spec. If an example needs something the spec does not allow, that is a gap: settle it in the log first, then update the spec, then the example.
4. **Implementation** follows all three. Where the compiler disagrees with the spec, the compiler is wrong, unless the log settles it the other way.

Never change language behavior in the spec, the examples or the compiler without a decision-log entry behind it. If a request implies a new or changed decision, ask the user before writing it down.

## Decision log and ADRs

- The log is append-only. New decisions go under a new `## YYYY-MM-DD: <topic>` session heading at the end, with a one-line `Goal:`.
- Entry format: `- **D-NNN Title**: decision. Why: reason.` IDs are sequential across the whole file and never reused. Mention rejected alternatives in the "Why" when they matter.
- Superseding: strike the old entry through (`~~…~~`) and append `Superseded by D-NNN.`, and end the new entry with `Supersedes D-NNN.` Use `Refines D-NNN.` or `Extends D-NNN.` for narrower changes. Update the old ADR's `status` line only, never its body.
- Each session has an `Open:` list. Settled items are struck through with `→ D-NNN` or `Done: …`, not deleted.
- An ADR is written only for decisions that are hard to reverse, surprising without context, and a real trade-off. Use frontmatter `status`, `date` and `log`, then a short body, optionally followed by `## Considered options` and `## Consequences`. Link it from the log entry with `→ [ADR-NNNN](../adr/…)`.
- Plan-level decisions also go in the decision table of `docs/plan/README.md`.
- Design sessions follow the `grilling` skill if it is available.

## Spec conventions

- Normative words: **must**, **must not**, **may**, **should**. Text marked _Note_ or _Example_ is informative.
- Grammar uses the EBNF notation described in `docs/spec/README.md`.
- An unsettled gap is marked `> **Open (OQ-<chapter>-<n>):** … _Proposed:_ …` and is replaced by normative text that cites the settling decision.
- When a decision lands, update every chapter it affects, the overview tables in `01-overview.md` and Appendix A in the same change.
- Standard-library names are specified in `docs/stdlib/`, one file per module (D-220). Tier 1 is drafted, with open gaps marked `OQ-<module>-<n>` (D-352). Names of later tiers, such as `fiber::spawn`, stay provisional until their tier is written (D-174, D-199).
- Diagnostic codes stay out of the spec. A new code gets an explanation in `docs/diagnostics/` that cites its spec sections, and a UI test whose golden pins it (D-420, D-476).

## Writing style

- Plain, short sentences. State the rule first, then the reason.
- Markdown tables are aligned the way Prettier formats them.

## Writing Cheby code

Code in the spec, the ADRs, the examples, `std/` and the tests must be valid Cheby in the form `cheby fmt` will produce: two-space indentation, no semicolons, trailing commas in multi-line lists. Agents often get these rules wrong:

- No `for`, `while`, `loop`, `if` or `else` (D-002). Iterate with recursion, often through a local `fn`, and branch with `case`, including subjectless `case { cond => … }` and multi-subject `case a, b { … }`. Guards use `when`.
- No mutation and no shadowing (D-017, D-051). Every `let` introduces a new name.
- No methods or `impl` (D-061). Call module functions: `list::map(xs, f)`. Paths use `::`.
- Pipes need an explicit hole (D-244): `xs |> list::map(_, f)`. Only one-parameter functions are piped bare: `xs |> list::length`. `f(_)` is an error everywhere (D-245).
- No positional tuple access such as `t.0` (D-241). Destructure with a pattern instead: `let (a, b) = t` or `fn((x, y)) { … }`.
- Errors use `Result`, `Option` and `use x <- result::try(…)`. There is no `?` and no `null` (D-018, D-091).
- Top-level functions need full signatures, and types are inferred only inside bodies (D-011).
- Comments are `//` only, with no block comments (D-078). `///` documents an item and `//!` documents a module.
- Imports look like `import std::io`, and strings interpolate with `"{name}"`.
- Put `test "…" { assert … }` blocks next to the code they test.

When unsure, find a similar construct in `docs/examples/` or `std/` and check the relevant spec chapter. `cargo run -p cheby_cli -- check` in a package directory confirms that the code checks.

## Examples

- Each example lives in `docs/examples/NNN_snake_name/` with `cheby.toml` (`name`, `version`) and `src/main.cheby`, plus any other modules. Use the next free number.
- Start every module with a `//!` comment that says what it does.
- Goldens sit next to the sources: `expected.stdout`, and optionally `expected.stderr`, `expected.exit`, `stdin`, `args` and `env`. `test.toml` names the milestone in which the example must pass. The layout is in [`docs/plan/testing.md`](docs/plan/testing.md) §1.
- Output must be deterministic. Sort before printing `Map` contents, and pass seeds in explicitly. If an example cannot be made deterministic, change the example, never the harness.

## Syntax changes and tree-sitter

The tree-sitter grammar must accept every example, UI test, REPL input and std source that the compiler parser accepts, and reject the rest (D-272). After changing syntax, run this here:

```sh
cargo xtask cross-check   # needs tree-sitter-cli
```

It uses the grammar at the commit pinned in `tests/tree-sitter-cheby.rev` (D-290). After changing the grammar, also run this from `../tree-sitter-cheby`:

```sh
npm run parse-examples   # must report 0 failed parses
npm test
```

If the grammar or the queries need updating, do that in the sibling repositories and bump `tests/tree-sitter-cheby.rev`. Tell the user that `../zed-cheby/languages/cheby/highlights.scm` and the `rev` in `extension.toml` need to follow.

## Implementation

Follow `docs/plan/`, especially `architecture.md`, `testing.md` and the file of the milestone in progress.

- Rust is pinned at 1.99 on edition 2024 in `rust-toolchain.toml` (D-277). Bump it only in its own change.
- The locality rule (D-246): type-checking a module may use only the interfaces of its dependencies, never their bodies.
- The parser is hand-written over a lossless CST (D-272, D-280).
- Gaps found while implementing go into the decision log and the spec before the code relies on an answer. Never resolve them with ad-hoc compiler behavior.
- Plan-level open questions in `docs/plan/README.md` (OQ-P9 and OQ-P11 to OQ-P14) need a decision before the tasks that depend on them start.

### Building and testing

CI (`.github/workflows/ci.yml`) runs these commands, and a change must pass them:

```sh
cargo build --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked
cargo fmt --all --check
cargo test --workspace --locked   # includes the conformance, UI and REPL harnesses
cargo xtask coverage              # decisions cited in the spec that no test names
cargo xtask cross-check           # compiler parser against tree-sitter
cargo xtask test-std              # std's own test blocks, one module at a time
cargo xtask check-examples        # cheby check in every example
cargo xtask check-determinism     # same outputs across thread counts and paths
```

- The harnesses are test targets of `cheby_cli`: `conformance` (the examples), `ui` (`tests/ui/`) and `repl` (`tests/repl/`). Run one with `cargo test -p cheby_cli --test <name>`.
- `tests/ui/` holds one rule per test, grouped by spec chapter. The header comment says whether the test must compile, fail or run. [`tests/ui/README.md`](tests/ui/README.md) documents every directive, and `// @decisions:` names the decisions a test pins, which `cargo xtask coverage` reads.
- `cargo xtask bless [FILTER]` rewrites goldens from the current `cheby` (D-292). Review blessed changes like code.
- The harnesses follow `tests/milestone` (D-288). Tests of earlier milestones must pass, those of the milestone in progress are pending, and those of later milestones must fail. `CHEBY_MILESTONE`, `CHEBY_PENDING=require` and `CHEBY_PENDING_ALLOW` make a stricter run, for example to check a milestone's exit (D-544, D-567).
- On Windows, until M6, tests that build or run JIT code carry `#[cfg_attr(windows, ignore = …)]` (D-545, D-641). Follow that rule for new tests.
- Compile speed is reported with `cargo xtask bench-compile [--quick]` (D-247).
