# M1: Single-fiber core

Goal: the whole front end, the MIR with naive reference counting, the Cranelift JIT and the runtime kernels, enough to check every example whose standard-library modules exist with `cheby check` and run every example that needs no fibers through `cheby run`, `cheby test` and `cheby repl` (D-099, step 1). Examples that import modules of later tiers are checked from the milestone that ships them (D-477).

Prerequisites: M0 done. Tier-1 stdlib specified. The value representation (D-278, D-279), syntax tree (D-280) and hash function (D-281) are settled.

Out of scope: fibers, channels and scopes (M2), unwinding (M2: a panic ends the process, or abandons the REPL entry per D-283), Perceus borrowing and reuse (M2), AOT and the disk cache (M3), JS (M4).

## Phases

The tasks are grouped into phases. Phases overlap: a later phase can start once the tasks it depends on are done, not when the whole earlier phase is.

```
A syntax ─► B names ─► C types ─► D MIR ─► E codegen ─► G driver and CLI
                                     ▲            ▲
                         F runtime kernels ───────┘
H stdlib tier 1 (Cheby parts) starts once C can check it
I measurement runs from the first end-to-end program
```

## A. Syntax (`cheby_syntax`, `cheby_diag`)

| #   | Task                                                                                                                                                                                                                                                    | Spec                | Done when                                                       |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------- | --------------------------------------------------------------- |
| A1  | Source map, spans, diagnostic type and plain-text renderer with suggestions (D-418, D-421), terminal color (D-467), code registry checked against `docs/diagnostics/` (D-419, D-420)                                                                    | §13.8               | UI-test stderr format fixed                                     |
| A2  | Lexer: identifiers, keywords including reserved `go` (D-240), numeric literals (D-084), strings with interpolation segments, `//`, `///`, `//!`, attributes, `NL` tokens (D-231)                                                                        | ch. 2               | lexer is lossless on all examples; fuzz target added            |
| A3  | Syntax kinds, parser events and the flat arena tree builder (D-280)                                                                                                                                                                                     | -                   | CST round-trips any input; tree size and build time benchmarked |
| A4  | Parser for items: `fn`, `type` with variants and shorthand, aliases, `const`, `interface`, `test`, `import`, attributes, visibility                                                                                                                     | ch. 4, App. A.1-A.2 | declaration corpus parses                                       |
| A5  | Parser for types and patterns                                                                                                                                                                                                                           | App. A.3, A.6       | pattern corpus parses                                           |
| A6  | Parser for expressions: precedence (§5.4.1), `case` with subjects and guards, subjectless `case`, `use`, pipes, captures, closures with pattern parameters (D-242), local functions, records and updates, lists, tuples                                 | ch. 5, App. A.4-A.5 | all examples parse                                              |
| A7  | Newline continuation rules decided by the parser (D-231)                                                                                                                                                                                                | §2.8                | `newlines` corpus from tree-sitter passes                       |
| A8  | Error recovery: synchronize on items and statements, never fail to produce a tree, one nesting limit of 256 levels that also counts operator, pipe, call and field chains and `use` statements (D-441, D-519)                                           | -                   | fuzzing finds no panics; partial trees for broken files         |
| A9  | Typed AST views over the CST                                                                                                                                                                                                                            | -                   | later phases use only views, never raw nodes                    |
| A10 | Syntax-level errors that are not grammar errors (D-424): `x.0` (D-241), `f(_)` (D-245), pipe without a hole (D-244), tuple-shape warning (D-243, D-440), spaced negative literals (D-470); only token-level checks on files with grammar errors (D-425) | §5.12, §5.16        | UI tests pass                                                   |

## B. Packages, names and interfaces (`cheby_package`, `cheby_resolve`, `cheby_iface`)

| #   | Task                                                                                                                                                                                                                      | Spec         | Done when                                               |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------ | ------------------------------------------------------- |
| B1  | `cheby.toml` reading (name, version, targets; unknown keys rejected (D-444); dependencies parsed but rejected until M5)                                                                                                   | §13.3        | all example manifests load                              |
| B2  | Module discovery from `src/`, root module rules (D-142), file naming rules, `.mjs` files exempt (D-208)                                                                                                                   | §7.2         | module paths of all examples correct                    |
| B3  | Import graph, cycle detection (D-145), `std` resolution, implicit edges to shipped prelude home modules (D-447), E0038 for planned but unshipped std modules (D-451, D-480)                                               | §7.5         | cycle UI test passes                                    |
| B4  | Item collection per namespace, duplicate detection                                                                                                                                                                        | §4.1.2, §7.7 | UI tests pass                                           |
| B5  | Visibility: `pub`, package-visible, `priv` (D-162), `exposed` (D-048), interface satisfaction visibility (ADR-0034)                                                                                                       | §7.3, §7.4   | UI tests pass                                           |
| B6  | Prelude and hiding with warning (D-112, D-146, D-212), home modules re-exporting prelude types (D-447), `Map`, `Set` and `Bytes` as builtin items outside the prelude (D-481), hiding reported at the declaration (D-487) | §7.6         | UI tests pass                                           |
| B7  | Local name resolution, shadowing ban (D-051, D-062), naming conventions and reserved words where names are bound (D-059, D-469)                                                                                           | §5.2.3, §2.3 | UI tests pass, with suggested names                     |
| B8  | `ModuleInterface`: data model, stable hash referring to definitions by path and name (D-460), deterministic serialization (unused on disk until M3)                                                                       | §13.12.1     | hash unchanged by body edits; serialization byte-stable |

## C. Type checking (`cheby_types`)

| #   | Task                                                                                                                                                                                                                                                                              | Spec               | Done when                                                |
| --- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------ | -------------------------------------------------------- |
| C1  | Type representation, interning, unification with occurs check                                                                                                                                                                                                                     | §3.1, §3.12        | unit tests                                               |
| C2  | Signature checking: required top-level signatures (D-011), `-> Nil` omission (D-083), type aliases (D-081), `Never` (D-131), entry-point signature of every `main` (D-448), type arity and kind errors (D-462)                                                                    | §3.9-§3.12, ch. 4  | interfaces of all examples built                         |
| C3  | Bodies: bidirectional checking, order of inference (D-234), closures last, deferred operators, numeric literal defaulting (D-084), result type before arguments (D-497), a code of its own for types nothing fixes (E0057, D-480)                                                 | §3.12.3-§3.12.5    | all example bodies check                                 |
| C4  | Constructors, records, field access on multi-variant types (D-071), record update                                                                                                                                                                                                 | §5.11              | UI tests pass                                            |
| C5  | Patterns and `let assert`, irrefutability (D-159), pattern parameters (D-242)                                                                                                                                                                                                     | ch. 6, §11.4       | UI tests pass                                            |
| C6  | Exhaustiveness and reachability, including multiple subjects, guards, list patterns, literals                                                                                                                                                                                     | §6.9               | UI tests pass; unreachable-arm warning                   |
| C7  | Desugaring of `use`, pipes and captures into calls and closures (D-498), interface functions and positional constructors used as values (D-500)                                                                                                                                   | §5.6, §5.10, §5.16 | REPL `:type` shows desugared types                       |
| C8  | Interfaces: declarations, satisfaction by module functions (D-061), one type per module (D-045), conditional conformance (D-064), embedding                                                                                                                                       | ch. 8              | 012, 016, 017 check                                      |
| C9  | Bounds and their dictionaries, operator interfaces (D-047, D-169), `compare` and ordering, the first bound that provides an operator (D-503)                                                                                                                                      | §8.4, §8.7, §3.14  | UI tests for bounds, `compare` and ordering pass (D-477) |
| C10 | `dyn I`: object safety (D-117), implicit conversion at expected types (D-168, D-235), descriptor requirements on bounds (D-236, D-237)                                                                                                                                            | §8.6               | UI tests pass                                            |
| C11 | Interpolation: `Show` requirement (D-086), `{x:?}`                                                                                                                                                                                                                                | §5.3.2             | UI tests pass                                            |
| C12 | `@external`, `@target`, `@!target`, `@blocking`, `@async`, `@deprecated` attribute checks (D-461); cross-target use check (D-056, D-190, D-207), which replaces the E0038 gate on bodiless functions (D-451), reported once per pair of items (D-512); items on no target (D-513) | §12.5, §12.6       | 010 and 020 check for both targets                       |
| C13 | Constants: types, restrictions (no IO, no FFI), dependency order; containment and direct cycles in the checker (D-511), the rest at evaluation (E6, D-478)                                                                                                                        | §4.5               | UI tests pass                                            |
| C14 | Warnings of §13.8 that are local to a module; whole-package warnings as a separate pass in `cheby check` only (D-266, D-516); uses by written name (D-515, D-517), W0011 and W0013 scope (D-518)                                                                                  | §13.8              | every warning has a UI test                              |
| C15 | Typed bodies (THIR) as the output, with every expression's type and every resolved dictionary, dictionaries carrying descriptors (D-499)                                                                                                                                          | -                  | MIR lowering needs nothing else                          |

## D. MIR (`cheby_mir`)

| #   | Task                                                                                                                                                                               | Spec / decision | Done when                                           |
| --- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------- | --------------------------------------------------- |
| D1  | MIR data model, builder, validator and textual dump (used in tests and determinism checks)                                                                                         | D-019           | dumps are stable                                    |
| D2  | Lowering of expressions and statements, evaluation order (§5.15.1), tail-position marking                                                                                          | §5.15           | dumps of examples reviewed                          |
| D3  | Pattern decision trees                                                                                                                                                             | ch. 6           | no duplicated tests on examples                     |
| D4  | Closure conversion and local named functions, including mutual recursion                                                                                                           | §5.7, §5.8      | 003, 024 lower                                      |
| D5  | Dictionaries for bounds, `dyn` packaging with type descriptors, descriptors of generic types built at run time                                                                     | §8.5.1, §8.6.1  | 016, 017 lower                                      |
| D6  | Uniform-representation boxing and unboxing at generic boundaries, tagged integers and boxed `Int`s outside 63 bits (D-278), static nullary constructors and unboxed `Bool` (D-454) | D-021           | unit tests, including values at the 63-bit boundary |
| D7  | Explicit `dup`/`drop` placed by last use (D-205), including drops at the start of branches that do not use a binding                                                               | D-273, §9.2     | leak counter reports zero on all M1 examples        |
| D8  | Per-type layout tables for equality, hashing and debug printing (D-252)                                                                                                            | §3.13           | `==` and `{x:?}` work on every type                 |
| D9  | Runtime panics: overflow (D-025), division by zero, failed `let assert`, `assert` with both sides (D-096), `todo`, `==` on functions (D-233), with the default messages of D-449   | ch. 11          | UI run tests pass                                   |

## E. Native code generation and JIT (`cheby_codegen_clif`, `cheby_jit`)

| #   | Task                                                                                                                                                                                                                       | Decision | Done when                                                  |
| --- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------- | ---------------------------------------------------------- |
| E1  | MIR to Cranelift IR for all MIR operations, with Cranelift optimization off (D-259), the hidden `ctx` parameter and the calling conventions of D-453, symbols of D-456, user layout ids bound through data symbols (D-454) | D-019    | 001 runs                                                   |
| E2  | `tail` calling convention and `return_call` for every tail call, including indirect and mutual                                                                                                                             | D-015    | a ten-million-step mutual recursion runs in constant stack |
| E3  | Entry stack-limit check against the limit in `ctx` (D-453, D-457), panicking on overflow (§11.8.1)                                                                                                                         | D-124    | deep non-tail recursion panics cleanly                     |
| E4  | Indirection table for top-level functions, used by every call between top-level functions, slots identified by path and ABI signature (D-456)                                                                              | D-043    | REPL redefinition test passes                              |
| E5  | `@external(native, …)` wrappers calling through the C ABI with the mapping of §12.5.1, symbols resolved in the process and imported without a signature (D-475), E0053 to E0055 (D-455)                                    | D-030    | 010 runs on native                                         |
| E6  | Constant evaluation with the JIT, also under `cheby check` (D-459), time and memory limits (D-195), derived hash seed (D-269), results as static objects (D-454); the interim before it follows D-478                      | D-090    | constants in examples evaluate; limit UI tests pass        |

Until E6 lands, `cheby check` checks a constant's declared type, its restrictions and cycles of direct references between constants, but does not evaluate it, and `cheby run` reports E0038 at each constant that `main` reaches (D-478). Panics and exceeded limits in initializers, cycles through function calls and the JS range of a constant's value are found from E6. Most M1-exit examples use constants, so they run only once E6 is done.

## F. Runtime kernels (`cheby_runtime`)

| #   | Task                                                                                                                                                                                                                                        | Decision     | Done when                         |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------ | --------------------------------- |
| F1  | Allocator, 8-byte object header (D-279) with static count 0 and fixed builtin layout ids (D-454), global layout table, `dup`/`drop` skipping tagged integers, recursive free by scan count without stack growth, debug leak counter (D-458) | D-005        | unit tests                        |
| F2  | `String` (UTF-8, grapheme and code-point iteration, no indexing) and `Bytes`, both with `len` and `cap` (D-454)                                                                                                                             | D-038, D-095 | property tests                    |
| F3  | RRB `List` with O(1) slices for list patterns (D-053), in-place updates at count one                                                                                                                                                        | D-039        | property tests against `Vec`      |
| F4  | HAMT `Map` and `Set`, keyed SipHash-1-3 with a per-process seed (D-093, D-281), order-independent `==`                                                                                                                                      | D-039        | property tests against `BTreeMap` |
| F5  | Generic equality, hashing and debug printing driven by layout tables                                                                                                                                                                        | D-252, D-233 | output matches §3.13 examples     |
| F6  | Float text (§3.3.6), integer and float parsing                                                                                                                                                                                              | -            | unit tests                        |
| F7  | Process entry, `main` result to exit code (§10.10), panic printing (D-449), entries on their own stacks (D-457), leak check at exit (D-458), stdout and stdin buffering                                                                     | D-073, D-182 | exit-code UI tests pass           |

## G. Driver and CLI (`cheby_driver`, `cheby_repl`, `cheby_cli`)

| #   | Task                                                                                                                                                                                               | Spec         | Done when                                                |
| --- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------ | -------------------------------------------------------- |
| G1  | Build graph over modules in import order, parallel modules and parallel functions inside a module (D-263), deterministic output (D-264)                                                            | §13.12.2     | results identical across thread counts                   |
| G2  | `std` embedded in the binary, compiled from source and kept in memory per process (D-450)                                                                                                          | -            | `import std::list` works                                 |
| G3  | `cheby run [module_path]` (D-137), `main` of any visibility (D-163), tool errors for a bad module path (D-464)                                                                                     | §13.1        | M1 examples pass through the harness                     |
| G4  | `cheby test`: `test` blocks as separate units (D-265), `Nil` or `Result` bodies (D-140), filtering, sequential in M1                                                                               | §13.6        | stdlib `test` blocks pass; user builds skip them (D-479) |
| G5  | `cheby repl`: expressions, `let`, declarations, imports, `:type`, `:module`, `:reload`, redefinition and stale types (D-043, D-098, D-193), `@external` redefined with another C signature (D-475) | §13.5        | `tests/repl/` passes                                     |
| G5b | REPL panics: each entry on its own stack, a panic abandons the entry and the session continues (D-283, D-457)                                                                                      | §13.5.4      | REPL test with a panicking entry passes                  |
| G6  | `cheby check` with `--target` (D-282), phase-A errors stopping later phases (D-452), and `cheby build` reporting that no backend exists yet                                                        | §13.1        | CI uses `cheby check` on all examples                    |
| G7  | `--deny-warnings` (D-080)                                                                                                                                                                          | §13.8        | UI test passes                                           |
| G8  | `cheby explain <code>` printing the embedded file from `docs/diagnostics/` (D-420)                                                                                                                 | §13.1, §13.8 | every assigned code is explained                         |

## H. Standard library, tier 1

| #   | Task                                                                                                                                                                                  | Done when                                         |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| H1  | Prelude types as compiler-known declarations with home modules (D-447): `Bool`, `Result`, `Option`, `Order`, `Nil`, `Never`, and `Map`, `Set` and `Bytes` outside the prelude (D-481) | prelude UI tests pass                             |
| H2  | Kernel `@external` bindings for `list`, `map`, `set`, `string`, `bytes`, `io` and the numeric modules, named `cheby_<module>_<function>` and `./<module>.mjs` (D-484)                 | every kernel reachable from Cheby                 |
| H3  | Cheby parts of tier-1 modules ([stdlib.md](stdlib.md#tier-1-before-m1)), laid out per D-483, with `test` blocks next to the code (D-479)                                              | module `test` blocks pass                         |
| H4  | `std::json`                                                                                                                                                                           | 011 passes (otherwise it moves to M2's exit list) |

## I. Measurement

| #   | Task                                                                                                                    | Done when                               |
| --- | ----------------------------------------------------------------------------------------------------------------------- | --------------------------------------- |
| I1  | Report lines per second per core on `bench/gen` projects of 10k, 100k and 1M lines (D-247)                              | numbers published by CI on every merge  |
| I2  | Profile the front end on the 1M-line project, fix the top hot spots                                                     | profile recorded in the milestone notes |
| I3  | Settle OQ-P10: confirm or revise the D-247 throughput budget through a log entry; OQ-P11 and OQ-P12 follow in M3 and M6 | log entry exists                        |

## Exit criteria

- These examples pass through `cheby run` with their goldens: 001, 002, 003, 004, 006, 007, 008, 010, 012, 014, 015, 016, 017, 021, 023, 024, 025, 026, 027, 029, 032, and 011 if H4 is done.
- Every other example passes `cheby check` for both targets, with two exceptions. Examples with dependencies are checked from M5, when dependencies resolve, and 030 is the only one (D-446). Examples that import standard-library modules of later tiers are checked from the milestone that ships those modules, and until then report only E0038 for those imports: 005, 009, 013, 018, 019, 022 and 028 from M2, and 031 from M3 (D-477). The examples that pass `cheby check` at M1 exit are therefore 001-004, 006-008, 010-012, 014-017, 020, 021, 023-027, 029 and 032.
- All tier-1 stdlib modules are implemented on native, except `std::multimap`, `std::sorted_*` and `std::ordered_*`, which are still unshipped at M1 exit (D-480).
- The leak counter reports zero live objects at exit for every passing example (D-458).
- E0038 is emitted only for imports of standard-library modules that are planned but not shipped, and its explanation says so (D-480).
- Every warning of §13.8 and every "must" of chapters 2 to 8 that M1 implements has a UI test.
- The REPL test suite passes.
- The `test` blocks of the examples above pass through `cheby test` in the conformance suite (D-472).
- Builds are deterministic across thread counts.
- Compile-speed numbers are reported, and OQ-P10, the throughput budget, is settled.

## Status

In progress. The syntax layer, the vertical slice and the front end are done. The back end runs only 001 so far: every other example stops with E0038 in MIR lowering, which lowers only what `main` reaches (D-510), and the native and JS kernels do not exist yet.

### Front end

Done on 2026-10-10, at commit `6e34ac6`. The gaps it found are settled by D-477 to D-519, and its follow-ups are open items of the [M1 front end](../decisions/LOG.md#2026-10-10-m1-front-end) session.

| #   | State  | Notes                                                                                                                                                                                                                           |
| --- | ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| B4  | done   | Items per namespace and duplicate names.                                                                                                                                                                                        |
| B5  | done   | Opacity errors at the use (D-487). Less visible types in signatures, with aliases transparent (D-488).                                                                                                                          |
| B6  | done   | Hiding warned at the declaration or import (D-487). `Map`, `Set` and `Bytes` are builtin items outside the prelude, named through their home modules (D-481).                                                                   |
| B7  | done   | Local resolution, the shadowing ban, and naming and reserved words where names are bound.                                                                                                                                       |
| B8  | done   | Two runs over every example give byte-identical interface encodings and hashes.                                                                                                                                                 |
| C1  | done   |                                                                                                                                                                                                                                 |
| C2  | done   | An erroneous signature makes its uses silent (D-490). `main` takes no type parameters, and a bodiless type needs no `@external` (D-491).                                                                                        |
| C3  | done   | E0057 for types nothing fixes (D-480), the result before the arguments and the types of `case` arms (D-497), test bodies (D-493), recovery after errors (D-494), float literal range (D-496), codes for uncoded checks (D-495). |
| C4  | done   | Record update through aliases, never changing type arguments (D-501).                                                                                                                                                           |
| C5  | done   | Irrefutability by §6.1's list (D-509), constructor pattern shapes (D-495), alternatives (D-506).                                                                                                                                |
| C6  | done   | Witness text (D-507), two passes with their own budgets (D-508), `Never` as an opaque type (D-492).                                                                                                                             |
| C7  | partly | Desugaring is done (D-498, D-500). The done-when, REPL `:type`, waits for G5.                                                                                                                                                   |
| C8  | done   | 012, 016 and 017 check. How a satisfying function is matched (D-504), own and embedded names (D-505), unreadable home modules in std (D-489).                                                                                   |
| C9  | done   | UI tests for bounds, `compare` and ordering pass (D-477). Operators through any bound (D-503).                                                                                                                                  |
| C10 | done   | Descriptors for dictionaries, and `dyn` of empty interfaces (D-502).                                                                                                                                                            |
| C11 | done   |                                                                                                                                                                                                                                 |
| C12 | done   | 010 and 020 check for both targets. E0067 once per pair of items (D-512), items on no target (D-513), member attributes (D-514), std-only attributes (D-482).                                                                   |
| C13 | partly | Types, restrictions and direct cycles are checked (D-511). Evaluation waits for E6 (D-478).                                                                                                                                     |
| C14 | done   | Every warning of §13.8 has a UI test. Imports are used by written names (D-515), unused items (D-516), deprecated uses (D-517), the scope of W0011 and W0013 (D-518).                                                           |
| C15 | done   | THIR carries descriptors in dictionaries and needs no inference later (D-499).                                                                                                                                                  |
| H1  | done   | Prelude types, and `Map`, `Set` and `Bytes` (D-447, D-481).                                                                                                                                                                     |
| H2  | partly | Every kernel is declared with its `@external` bindings (D-484). The symbols come with F2 to F4.                                                                                                                                 |
| H3  | partly | Every shipped tier-1 module is written and checks clean, laid out per D-483. `std::multimap`, `std::sorted_*` and `std::ordered_*` are not written (D-480). The `test` blocks run with G4 (D-479).                              |
| H4  | partly | `std::json`, `std::json::decode` and the internal `std::json::tree` are written, and 011 checks. 011 runs once the back end is done.                                                                                            |

Changes against the plan:

- The examples that import tier-2 and tier-3 modules are checked from the milestone that ships those modules, and C9's done-when became UI tests (D-477).
- E0038 stays after M1 for imports of planned standard-library modules that are not shipped, instead of being retired (D-480).
- Codes E0057 to E0074 and W0002 to W0013 were added, and standard-library modules were staged until the checker could check them (D-486).
