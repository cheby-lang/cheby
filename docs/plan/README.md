# Cheby implementation plan

Status: **draft**. This plan describes how the reference implementation of Cheby is built, from an empty Cargo workspace to v1. It follows the language specification in [`../spec/`](../spec/README.md) and the decision log in [`../decisions/LOG.md`](../decisions/LOG.md), and cites both rather than restating them. Plan-level decisions are logged as D-270 and later.

The plan is a living document. When a milestone starts, its file is refined into task-level detail. When it ends, its exit criteria are checked off and the file records what changed against the plan.

## Files

| File                                       | Contents                                                         |
| ------------------------------------------ | ---------------------------------------------------------------- |
| [architecture.md](architecture.md)         | Repository layout, crates, compiler pipeline, IRs, runtime shape |
| [testing.md](testing.md)                   | Example goldens, UI tests, benchmarks, determinism, fuzzing      |
| [stdlib.md](stdlib.md)                     | Standard-library workstream: spec and implementation by tier     |
| [m0-foundation.md](m0-foundation.md)       | M0: workspace, CI, harnesses, goldens, first stdlib specs        |
| [m1-core.md](m1-core.md)                   | M1: single-fiber core, JIT and REPL (task-level detail)          |
| [m2-runtime.md](m2-runtime.md)             | M2: Perceus, fibers, M:N scheduler, channels, unwinding          |
| [m3-aot-and-cache.md](m3-aot-and-cache.md) | M3: AOT output, linking, build cache, cached-code loader         |
| [m4-js.md](m4-js.md)                       | M4: JavaScript backend                                           |
| [m5-tooling.md](m5-tooling.md)             | M5: formatter, language server, docs, package manager            |
| [m6-v1.md](m6-v1.md)                       | M6: release profile, bundled linker, platforms, v1 readiness     |

## Goals

1. A reference implementation of the whole spec, shipping as one `cheby` binary with every tool of [§13.1](../spec/13-tooling.md#131-the-cheby-command) (D-031).
2. The build order of D-099: the single-fiber core first, because the REPL gives the fastest feedback on the language design itself.
3. Compile speed from the first milestone: the locality rule (D-246) shapes the compiler's structure, and the budgets of D-247 and D-267 are measured in CI from M1.
4. The 32 examples in [`../examples/`](../examples/) become the end-to-end conformance suite, and every milestone is defined by which of them run.
5. Spec gaps found while implementing go back into the decision log, never into ad-hoc compiler behavior.

## Milestones

```
M0 foundation ─► M1 single-fiber core ─► M2 runtime ─► M3 AOT + cache ─► M4 JS ─► M6 v1
                     │                                                          ▲
                     └──────────── M5 tooling, grown alongside (D-099) ─────────┘
stdlib workstream: tier 1 before M1 ─► tier 2 before M2 ─► tier 3 before M3 / M4
```

| Milestone            | Delivers                                                                                                                   | Examples that must pass at exit                                                           |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| M0 Foundation        | Workspace, CI, test and bench harnesses, golden files for all examples, tier-1 stdlib specs                                | none (harnesses run, examples are marked expected-fail)                                   |
| M1 Single-fiber core | Parser, resolver, type checker, MIR with naive RC, Cranelift JIT, runtime kernels, `cheby check`, `run`, `test`, `repl`    | 001-004, 006-008, 010, 012, 014-017, 021, 023-027, 029, 032 (011 if `std::json` is ready) |
| M2 Runtime           | Perceus (borrowing, reuse), shared marking, fiber stacks, M:N scheduler, scopes, channels, selectors, unwinding, IO poller | adds 005, 009, 013, 018, 019, 022, 028                                                    |
| M3 AOT and cache     | Object output, linking, DWARF line tables, interface artifacts on disk, global cache, cached-code loader, precompiled std  | all of the above through `cheby build --target native`, plus 020, 030, 031                |
| M4 JS                | JS emitter, link-time suspension analysis, JS runtime, handle RC on JS, source maps                                        | every example on Node with the same goldens as native                                     |
| M5 Tooling           | `cheby fmt`, `cheby lsp`, `cheby doc`, `cheby new/add/update/fetch`                                                        | every example is a `cheby fmt` fixed point, doc examples run as tests                     |
| M6 v1                | Release profile, bundled lld, Windows, full platform matrix, budgets met                                                   | every example on every platform of D-074, in both profiles                                |

M5 is listed as a milestone so it has exit criteria, but its work starts as soon as M1's parser exists and runs in parallel with M2-M4 (D-099). [m5-tooling.md](m5-tooling.md) says which slice of it lands next to which milestone.

## Workstreams across milestones

- **Standard library** ([stdlib.md](stdlib.md)): the stdlib spec in `docs/stdlib/` (D-204, D-220) is written tier by tier, each tier before the milestone that needs it (D-276). Data-structure kernels live in the runtime and combinators in Cheby (D-274).
- **Conformance** ([testing.md](testing.md)): goldens next to every example and `tests/ui/` for compile-time rules (D-275).
- **Performance**: the bench harness exists from M0, and the D-247/D-267 budgets are reported from M1 and enforced from M6.
- **Spec feedback**: every gap found while implementing becomes a decision-log entry, and the spec is updated before the implementation relies on the answer.

## Plan decisions

Logged in [`../decisions/LOG.md`](../decisions/LOG.md#2026-10-04-implementation-plan):

| Decision | Summary                                                                                             |
| -------- | --------------------------------------------------------------------------------------------------- |
| D-270    | The implementation is a Cargo workspace in this repository, next to `docs/`                         |
| D-271    | The plan lives in `docs/plan/`, one file per milestone, with the full roadmap and M1 in task detail |
| D-272    | Hand-written lexer and recursive-descent parser over a lossless CST, cross-checked with tree-sitter |
| D-273    | MIR has explicit dup/drop from M1 (naive RC), and Perceus in M2 only optimizes it                   |
| D-274    | Collection and string kernels in the runtime (Rust and JS), combinators in Cheby, migrate later     |
| D-275    | Golden files next to each example, plus `tests/ui/` for diagnostics and focused features            |
| D-276    | The stdlib spec is a parallel workstream, each tier written before the milestone that needs it      |
| D-277    | Pinned stable Rust (1.98, edition 2024), GitHub Actions with the full D-074 matrix on every PR      |
| D-278    | Tagged integers: low bit 1 is a 63-bit integer, low bit 0 a pointer; larger `Int`s boxed            |
| D-279    | 8-byte object header: 32-bit count (sign = shared), 8-bit scan count, 24-bit layout id              |
| D-280    | Event-based parser feeding a flat, arena-allocated CST written for Cheby, not `rowan`               |
| D-281    | Per-target hash behind one routine: SipHash-1-3 on native, a keyed 32-bit hash on JS                |
| D-282    | New permanent `cheby check`; `cheby build` errors for a target until its backend exists             |
| D-283    | Before unwinding, a REPL panic abandons the entry on its own stack and the session continues        |

## Open questions

Each open question is answered through a decision-log entry before the task that depends on it starts. Proposals in the milestone files are marked _Proposed_ and are not decisions.

| ID     | Question                                                                                                                               | Needed by | Status       |
| ------ | -------------------------------------------------------------------------------------------------------------------------------------- | --------- | ------------ |
| OQ-P1  | Toolchain basics: Rust version and edition, CI provider and runner matrix                                                              | M0        | D-277        |
| OQ-P2  | Native value representation: tag scheme, `Int` values outside the tagged range, object header layout, unboxed scalars                  | M1 start  | D-278, D-279 |
| OQ-P3  | CST library: `rowan` or a custom tree tuned for throughput                                                                             | M1 start  | D-280        |
| OQ-P4  | Hash function for `Map`/`Set` and the seeded hasher used by hashing (D-093)                                                            | M1        | D-281        |
| OQ-P5  | What `cheby build` does before AOT exists                                                                                              | M1        | D-282        |
| OQ-P6  | How panics end a REPL entry in M1, before unwinding exists                                                                             | M1        | D-283        |
| OQ-P7  | Fiber context switch: own assembly per platform or a crate such as `corosensei`                                                        | M2        | open         |
| OQ-P8  | IO poller: `mio`, a custom poller, and whether io_uring is used in v1                                                                  | M2        | open         |
| OQ-P9  | How a package supplies native foreign code (C sources, static libraries, build scripts) to be linked, needed by example 020. Spec gap. | M3        | open         |
| OQ-P10 | Whether the D-247/D-267 budgets hold, checked against M1 measurements (LOG open item)                                                  | M1 exit   | open         |
