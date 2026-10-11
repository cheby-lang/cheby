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
| M3 AOT and cache     | Object output, linking, DWARF line tables, interface artifacts on disk, global cache, cached-code loader, precompiled std  | all of the above through `cheby build --target native`, plus 020 and 031                  |
| M4 JS                | JS emitter, link-time suspension analysis, JS runtime, handle RC on JS, source maps                                        | every example on Node with the same goldens as native                                     |
| M5 Tooling           | `cheby fmt`, `cheby lsp`, `cheby doc`, `cheby new/add/update/fetch`                                                        | every example is a `cheby fmt` fixed point, doc examples run as tests, 030 passes (D-446) |
| M6 v1                | Release profile, bundled lld, Windows, full platform matrix, budgets met                                                   | every example on every platform of D-074, in both profiles                                |

M5 is listed as a milestone so it has exit criteria, but its work starts as soon as M1's parser exists and runs in parallel with M2-M4 (D-099). [m5-tooling.md](m5-tooling.md) says which slice of it lands next to which milestone.

## Workstreams across milestones

- **Standard library** ([stdlib.md](stdlib.md)): the stdlib spec in `docs/stdlib/` (D-204, D-220) is written tier by tier, each tier before the milestone that needs it (D-276). Data-structure kernels live in the runtime and combinators in Cheby (D-274).
- **Conformance** ([testing.md](testing.md)): goldens next to every example and `tests/ui/` for compile-time rules (D-275).
- **Performance**: the bench harness exists from M0, and the D-247/D-267 budgets are reported from M1 and enforced from M6.
- **Spec feedback**: every gap found while implementing becomes a decision-log entry, and the spec is updated before the implementation relies on the answer.

## Plan decisions

Logged in [`../decisions/LOG.md`](../decisions/LOG.md#2026-10-04-implementation-plan), D-284 onward in [M0 foundation](../decisions/LOG.md#2026-10-04-m0-foundation), D-418 onward in [Diagnostics](../decisions/LOG.md#2026-10-06-diagnostics), and the plan-level ones of [M1 syntax layer](../decisions/LOG.md#2026-10-07-m1-syntax-layer), [M1 vertical slice](../decisions/LOG.md#2026-10-07-m1-vertical-slice), [Open items](../decisions/LOG.md#2026-10-07-open-items), [M1 front end](../decisions/LOG.md#2026-10-10-m1-front-end), [M1 back-end plan](../decisions/LOG.md#2026-10-10-m1-back-end-plan), [M1 back end, wave 1](../decisions/LOG.md#2026-10-10-m1-back-end-wave-1), [M1 back end, wave 2](../decisions/LOG.md#2026-10-10-m1-back-end-wave-2), [M1 back end, wave 3](../decisions/LOG.md#2026-10-10-m1-back-end-wave-3), [M1 back end, wave 4](../decisions/LOG.md#2026-10-11-m1-back-end-wave-4), [M1 back end, wave 5](../decisions/LOG.md#2026-10-11-m1-back-end-wave-5), [M1 back end, wave 6](../decisions/LOG.md#2026-10-11-m1-back-end-wave-6) and [M1 exit](../decisions/LOG.md#2026-10-11-m1-exit):

| Decision  | Summary                                                                                             |
| --------- | --------------------------------------------------------------------------------------------------- |
| D-270     | The implementation is a Cargo workspace in this repository, next to `docs/`                         |
| D-271     | The plan lives in `docs/plan/`, one file per milestone, with the full roadmap and M1 in task detail |
| D-272     | Hand-written lexer and recursive-descent parser over a lossless CST, cross-checked with tree-sitter |
| D-273     | MIR has explicit dup/drop from M1 (naive RC), and Perceus in M2 only optimizes it                   |
| D-274     | Collection and string kernels in the runtime (Rust and JS), combinators in Cheby, migrate later     |
| D-275     | Golden files next to each example, plus `tests/ui/` for diagnostics and focused features            |
| D-276     | The stdlib spec is a parallel workstream, each tier written before the milestone that needs it      |
| D-277     | Pinned stable Rust (1.99, edition 2024), GitHub Actions with the full D-074 matrix on every PR      |
| D-278     | Tagged integers: low bit 1 is a 63-bit integer, low bit 0 a pointer; larger `Int`s boxed            |
| D-279     | 8-byte object header: 32-bit count (sign = shared), 8-bit scan count, 24-bit layout id              |
| D-280     | Event-based parser feeding a flat, arena-allocated CST written for Cheby, not `rowan`               |
| D-281     | Per-target hash behind one routine: SipHash-1-3 on native, a keyed 32-bit hash on JS                |
| D-282     | New permanent `cheby check`; `cheby build` errors for a target until its backend exists             |
| D-283     | Before unwinding, a REPL panic abandons the entry on its own stack and the session continues        |
| D-284     | All 20 crates exist from M0 as skeletons, plus `bench/gen`, `tests/support` and `xtask`             |
| D-285     | License holder is "The Cheby Project Developers"                                                    |
| D-286     | `cheby` parses arguments with clap derive; program arguments of `cheby run` follow `--`             |
| D-287     | Harnesses live under `tests/` but build as test targets of `cheby_cli`                              |
| D-288     | `tests/milestone`: earlier milestones must pass, the current one is pending, later ones must fail   |
| D-289     | UI tests are single `.cheby` files with `// @key: value` headers and goldens beside them            |
| D-290     | The tree-sitter cross-check uses the commit pinned in `tests/tree-sitter-cheby.rev`                 |
| D-291     | `bench/gen` is a deterministic library crate; CI checks its output until M1                         |
| D-292     | Repository scripts are `cargo xtask` commands written in Rust                                       |
| ~~D-293~~ | Superseded by D-423                                                                                 |
| D-418     | Diagnostics are rendered like rustc: header with code, location, snippet, notes and help            |
| D-419     | Codes `E0001` and `W0001` onward, assigned in order, never reused                                   |
| D-420     | `cheby explain <code>`, with explanations in `docs/diagnostics/` embedded in the binary             |
| D-421     | Locations are 1-based lines and code-point columns, paths package-relative with `/`                 |
| D-422     | Lint exemptions are local `#[expect(…, reason)]` on the smallest function or module                 |
| D-423     | Clippy `pedantic` and the no-panic lints are denied workspace-wide, `nursery` is not                |
| D-441     | The parser bounds expression, pattern, type and block nesting with one counter, at 256 levels       |
| D-442     | Codes `E0001` to `E0037` and `W0001` for the syntax layer; tool errors have no code                 |
| D-443     | Rendering details follow rustc; diagnostics are separated by an empty line, with no summary line    |
| D-445     | `skip-run` examples are only checked, pending before their milestone; bless keeps the exit code     |
| D-446     | Example 030 moves to M5, and examples with dependencies are checked from M5                         |
| D-450     | `std` sources are embedded in the binary until M3 and shown as `std/<path>.cheby`                   |
| D-451     | Temporary E0038 for constructs M1 does not support yet, narrowed at M1 exit by D-480                |
| D-453     | Hidden `ctx` first, uniform result word, one convention for unknown callees (ADR-0055)              |
| D-454     | Static objects have count 0, nullary constructors are static, fixed layout ids 1-63                 |
| D-456     | Symbols `cb$<kind>$<path>$<item>`; slots identified by path and ABI signature                       |
| D-457     | Each entry runs on its own stack and a panic parks it; 64 MiB root, 256 KiB headroom                |
| D-458     | Debug toolchains fail with exit status 102 when objects are live at exit                            |
| D-465     | `cheby_resolve` depends on `cheby_iface`; codegen reads only the runtime's `abi` module             |
| D-466     | Codes `E0038` to `E0055` for resolution, types, `main` and native symbols                           |
| D-467     | Diagnostics are colored on a terminal, with `--color` and `NO_COLOR`; goldens hold the plain text   |
| D-468     | A dependency file is shown as `<url>@<version>/<path>`, as in `github.com/x/y@1.2.0/src/a.cheby`    |
| D-471     | A UI test may be a package directory with `cheby.toml` and `src/**`, its header in `src/main.cheby` |
| D-472     | The conformance harness also runs each example's `cheby test`, pending until its milestone ends     |
| D-475     | The JIT imports foreign symbols without a signature; the REPL checks E0054 over live definitions    |
| D-476     | Spec examples of errors name no codes; each gets a UI test once its check exists                    |
| D-477     | Examples that import tier-2 or tier-3 modules are checked from the milestone that ships them        |
| D-478     | Until E6, `cheby check` does not evaluate constants, and `cheby run` gives E0038 at them            |
| D-479     | Std `test` blocks live in `std/`; user builds skip them, and testing std runs them                  |
| D-480     | After M1, E0038 only for imports of unshipped std modules, and in the back end until it is done     |
| D-483     | Std layout: the package-visible `std::json::tree`, and `std::string` above `std::bytes`             |
| D-484     | Per-type numeric kernels, fixed `std::list` kernels, callbacks only in `map::upsert`, fixed names   |
| D-486     | Codes `E0057` to `E0074` and `W0002` to `W0013`; std modules staged until the checker handles them  |
| D-510     | The back end lowers only what `main` reaches and reports all its E0038s before compiling            |
| D-519     | Operator, pipe, call, field and `use` chains count toward the nesting limit of D-441                |
| D-520     | Generated code calls fixed `cheby_rt_` helpers for strings, lists, equality, boxing and `fmod`      |
| D-521     | Each number type has one canonical uniform shape; `U64` above 2⁶² − 1 is boxed                      |
| D-522     | A constructor's tag is its layout id minus that of its type's first constructor                     |
| D-523     | Static dictionaries, descriptors and function values; the new symbol kind `dict`                    |
| D-524     | Layout ids 9-12 for dictionaries, `dyn` values and descriptors; their object shapes                 |
| D-525     | A kernel may update a unique argument in place, `dup` it and return it                              |
| D-526     | Kernels call Cheby closures through per-arity trampolines of the running entry                      |
| D-542     | `cargo xtask test-std` tests std one module at a time                                               |
| D-543     | UI tests take `.stdin` and `.args` files and the `@command` and `@flags` directives                 |
| D-544     | Strict harness runs with `CHEBY_MILESTONE` and `CHEBY_PENDING=require`; `CHEBY_RT_POISON=1`         |
| D-545     | M1 is judged on Linux and macOS; Windows CI builds and runs unit tests only until M6                |
| D-551     | `-j`/`--jobs`, and `cargo xtask check-determinism` across thread counts and paths                   |
| D-552     | Native case mapping and grapheme segmentation follow one Unicode version, checked by a test         |
| D-553     | Correctly rounded math from a pure-Rust CORE-MATH port, verified against 200-bit references         |
| D-554     | Bounds on foreign type parameters are checked by the front end; MIR reports no E0038 at M1 exit     |
| D-555     | Where objects with the escaped scan count 255 keep their fields and their real count                |
| D-556     | `cheby_rt_equal` takes the location of the `==` as a fourth argument, for the panic report          |
| D-559     | A constructor hashes by its type key, SipHash of module path, `0xff` and name, and its tag          |
| D-560     | Symbol kinds `tfn` for built-in tuple functions and `dynfwd` for `dyn` forwarders                   |
| D-561     | Statics, shims, tuple functions, forwarders and trampolines are defined once per JIT                |
| D-562     | A raw constructor field fills its 8-byte slot, sign- or zero-extended by type                       |
| D-563     | List roots, leaves and branches have layout ids 26-28; concatenation and in-place rules             |
| D-564     | Live objects and bytes are counted per accounting domain, one per JIT                               |
| D-565     | CLIF dumps ignore `CHEBY_RT_POISON` and always use the inline fast paths                            |
| D-566     | A strict run requires only pending tests of the milestone in progress, not those of later ones      |
| D-567     | Accepted values of the strict-run variables and the syntax of the allow file                        |
| D-568     | The seven correctly rounded functions are written in Rust with double-double and Ziv's test         |
| D-569     | A float literal is rounded once to each float type, and THIR holds both values                      |
| D-572     | Closures inside a test are `cb$clo$test$<path>$<ordinal>$<n>`                                       |
| D-573     | `std::io` kernels report kinds 0 to 3 and messages without Rust's `(os error N)` suffix             |
| D-575     | `read_input_line` reads REPL lines, replacing invalid UTF-8; a read error ends the input            |
| D-576     | UI tests feed `.stdin` byte for byte and read `.args` as text, one argument per line                |
| D-577     | Which `std::string` kernels update a unique argument in place or return it unchanged                |
| D-578     | `run_main` or `entry::set_program` sets the program's name and arguments for each host              |
| D-579     | Static symbols are `cb$<kind>$<module path>$$<name>`; one defined again differently is an error     |
| D-580     | Static names use fully qualified types; static closures capture static dictionaries                 |
| D-581     | A type without arguments is its nominal descriptor; builtin nominal descriptors and their names     |
| D-583     | Built-in tuple `show` code is located at `<builtin>`                                                |
| D-585     | Every `@std_hooks` hook of a module with a code unit is compiled and registered                     |
| D-586     | Nested maps and sets compare, hash and print on the walk's worklist, never the Rust stack           |
| D-590     | A value of an external type is the header and a raw handle, with a layout of its own                |
| D-591     | A JS-only function stays in the native program, and a constant that reaches it gets E0077           |
| D-595     | A test thunk returns `Nil` or its `Err` text; the host prints a test's panic report                 |
| D-596     | Each test runs on a 64 MiB stack, like `main`                                                       |
| D-598     | When `cheby test` writes its lines and blocks, and the layout of the std self-test report           |
| D-599     | The std self-test checks std through the in-memory package `std_self_test`                          |
| D-600     | `@flags` is split at whitespace into words, appended in order, with no quoting                      |
| D-601     | The `cheby` command runs its subcommand on a 256 MiB thread, a stopgap for deep recursion           |
| D-602     | A `dyn` forwarder is keyed by the interface function and the receiver's interface                   |
| D-603     | The descriptor of `dyn` of a builtin interface keeps the general symbol, with an empty module path  |
| D-607     | A REPL redefinition takes over a slot only with an equal type signature, undone if rejected         |
| D-611     | REPL items and entry statics use the module path `<repl>`, with generations per name and text       |
| D-613     | The body of a REPL entry runs on a 64 MiB stack, its constants on 8 MiB                             |
| D-614     | `< ` lines of a REPL transcript are standard input, fed after the input line before them            |
| D-615     | `cargo xtask cross-check` parses each REPL input as module items and as statements                  |
| D-617     | The constant memory limit counts one evaluation's live bytes, checked before each allocation        |
| D-619     | The JS range check walks a type graph of at most 65,536 nodes                                       |
| D-620     | `CHEBY_DUMP_DIR` dump files, and the builds and outputs that `check-determinism` compares           |
| D-622     | `-j` is global and sizes rayon's pool of workers with 256 MiB stacks                                |
| D-623     | Ziv's loop stops at 2048 bits, and the fast phase reads generated tables                            |
| D-627     | Session functions satisfy interfaces for session types through forwarders                           |
| D-630     | A std spec test that needs a module importing its own lives in that module                          |
| D-631     | Helper items of std tests are named by a written path in a test                                     |
| D-632     | MIR lowering and code generation run in parallel and take results in a fixed order                  |
| D-633     | No compiler pass recurses per element of a list pattern                                             |
| D-634     | `cheby_rt_list_slice_owned` consumes a list at its last use and slices a unique one in place        |
| D-635     | `CHEBY_RT_STATS` prints the runtime's counters, and `CHEBY_RT_POISON=0` turns off unit-test poison  |
| D-638     | REPL transcripts may start with a `// @decisions:` header, which `cargo xtask coverage` counts      |
| D-641     | Until M6, Windows CI builds every test but runs only those that execute no generated code           |
| D-642     | An allocation that cannot be served panics with `out of memory`; over the const limit it is E0076   |
| D-643     | Code generation binds MIR locals to CLIF values directly, with no `cranelift_frontend` variables    |
| D-644     | D-247's throughput budget holds at M1 (104k to 107k lines/s per core) and stays as written          |

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
| OQ-P10 | Whether the D-247 throughput budget holds, checked against M1 measurements                                                             | M1 exit   | D-644        |
| OQ-P11 | Whether the D-247 latency budget, from a one-function edit to `main` running, holds with the M3 cache                                  | M3 exit   | open         |
| OQ-P12 | Whether the release-to-debug ratio of D-267 holds                                                                                      | M6        | open         |
| OQ-P13 | The factor by which a Cheby collection may be slower than a Rust kernel before it moves (D-274, D-336)                                 | M2 exit   | open         |
| OQ-P14 | File IO, standard input and `std::env` on JS hosts, including hosts without a process such as browsers (OQ-io-3, OQ-env-1)             | M4 start  | open         |
