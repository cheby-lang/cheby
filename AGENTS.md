# AGENTS.md

Guidance for coding agents working in this repository.

## What this repository is

The home of the Cheby programming language. It is currently **docs only**, in the design phase before 1.0:

- `docs/decisions/LOG.md`: the decision log, `D-001` onward. This is the source of truth.
- `docs/adr/`: ADRs for the significant decisions, `NNNN-kebab-slug.md`.
- `docs/spec/`: the language specification, one chapter per file, with the grammar in Appendix A.
- `docs/plan/`: the implementation plan, milestones M0-M6.
- `docs/examples/`: 32 example programs, `NNN_name/cheby.toml` plus `src/*.cheby`.

Sibling repositories, usually checked out next to this one:

- `../tree-sitter-cheby`: the editor grammar and highlight queries.
- `../zed-cheby`: the Zed extension, which copies the highlight queries and pins a grammar commit.

## Source of truth

1. **Decision log** wins. The spec is derived from it and cites decisions as `(D-NNN)`.
2. **Spec** next. Appendix A wins over grammar snippets in the chapters, and a disagreement between them is a spec bug.
3. **Examples** must follow the spec. If an example needs something the spec does not allow, that is a gap: settle it in the log first, then update the spec, then the example.

Never change language behavior in the spec or the examples without a decision-log entry behind it. If a request implies a new or changed decision, ask the user before writing it down.

## Decision log and ADRs

- The log is append-only. New decisions go under a new `## YYYY-MM-DD: <topic>` session heading at the end, with a one-line `Goal:`.
- Entry format: `- **D-NNN Title**: decision. Why: reason.` IDs are sequential across the whole file and never reused. Mention rejected alternatives in the "Why" when they matter.
- Superseding: strike the old entry through (`~~…~~`) and append `Superseded by D-NNN.`, and end the new entry with `Supersedes D-NNN.` Use `Refines D-NNN.` or `Extends D-NNN.` for narrower changes. Update the old ADR's `status` line only, never its body.
- Each session has an `Open:` list. Settled items are struck through with `→ D-NNN` or `Done: …`, not deleted.
- An ADR is written only for decisions that are hard to reverse, surprising without context, and a real trade-off. Use frontmatter `status`, `date` and `log`, then a short body, optionally followed by `## Considered options` and `## Consequences`. Link it from the log entry with `→ [ADR-NNNN](../adr/…)`.
- Design sessions follow the `grilling` skill if it is available.

## Spec conventions

- Normative words: **must**, **must not**, **may**, **should**. Text marked _Note_ or _Example_ is informative.
- Grammar uses the EBNF notation described in `docs/spec/README.md`.
- An unsettled gap is marked `> **Open (OQ-<chapter>-<n>):** … _Proposed:_ …` and is replaced by normative text that cites the settling decision.
- When a decision lands, update every chapter it affects, the overview tables in `01-overview.md` and Appendix A in the same change.
- Standard-library names are specified in `docs/stdlib/`, one file per module (D-220). Tier 1 is drafted, with open gaps marked `OQ-<module>-<n>` (D-352). Names of later tiers, such as `fiber::spawn`, stay provisional until their tier is written (D-174, D-199).

## Writing style

- Plain, short sentences. State the rule first, then the reason.
- Markdown tables are aligned the way Prettier formats them.

## Writing Cheby code

Code in the spec, the ADRs and the examples must be valid Cheby in the form `cheby fmt` will produce: two-space indentation, no semicolons, trailing commas in multi-line lists. Agents often get these rules wrong:

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

When unsure, find a similar construct in `docs/examples/` and check the relevant spec chapter.

## Examples

- Each example lives in `docs/examples/NNN_snake_name/` with `cheby.toml` (`name`, `version`) and `src/main.cheby`, plus any other modules. Use the next free number.
- Start every module with a `//!` comment that says what it does.
- Examples are the future conformance suite (D-275), so their output must be deterministic. Sort before printing `Map` contents, and pass seeds in explicitly.

## Syntax changes and tree-sitter

The tree-sitter grammar must accept every example. After changing syntax or adding examples, run this from `../tree-sitter-cheby`:

```sh
npm run parse-examples   # must report 0 failed parses
npm test
```

If the grammar or the queries need updating, do that in the sibling repositories, and tell the user that `../zed-cheby/languages/cheby/highlights.scm` and the `rev` in `extension.toml` need to follow.

## Implementation (planned)

No code exists yet. When work on M0 starts, follow `docs/plan/`, especially `architecture.md` and `testing.md`:

- A Cargo workspace in this repository with crates under `crates/` (D-270), Rust pinned at 1.99 on edition 2024 (D-277).
- The locality rule (D-246): type-checking a module may use only the interfaces of its dependencies, never their bodies.
- A hand-written parser over a lossless CST, cross-checked against tree-sitter in CI (D-272, D-280).
- Goldens live next to each example, compile-time rules are tested in `tests/ui/`, and `cargo xtask bless` updates goldens (D-275).
- Gaps found while implementing go into the decision log and the spec before the code relies on an answer. Never resolve them with ad-hoc compiler behavior.
- Plan-level open questions (OQ-P7 to OQ-P10 in `docs/plan/README.md`) need a decision before the tasks that depend on them start.
