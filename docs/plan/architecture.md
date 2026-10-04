# Architecture

This file describes the shape of the reference implementation: where the code lives, how the compiler is split into crates, what each intermediate representation is for, and how the runtime is organized. It is a plan, not a specification. Where the spec fixes something, this file cites it.

## 1. Repository layout

The implementation is a Cargo workspace in this repository, next to `docs/` (D-270).

```
cheby/
  Cargo.toml              workspace
  rust-toolchain.toml     pinned stable toolchain, edition 2024 (D-277)
  LICENSE-MIT, LICENSE-APACHE (D-089)
  crates/
    cheby_syntax/         lexer, parser, lossless CST, typed AST views
    cheby_diag/           spans, source map, diagnostics, rendering
    cheby_package/        cheby.toml, module discovery, import graph
    cheby_resolve/        item collection, name resolution, visibility
    cheby_iface/          module interfaces, hashing, (de)serialization
    cheby_types/          type checker, interfaces, exhaustiveness
    cheby_mir/            MIR, lowering from typed bodies, RC and Perceus passes
    cheby_codegen_clif/   MIR to Cranelift IR, shared by JIT and AOT
    cheby_jit/            JIT module, indirection table, cached-code loader
    cheby_link/           AOT objects, linker invocation (M3)
    cheby_js/             JS emitter and suspension analysis (M4)
    cheby_runtime/        native runtime, built as rlib and staticlib
    cheby_driver/         build graph, parallel scheduling, build cache
    cheby_consteval/      compile-time evaluation of constants
    cheby_repl/           REPL session
    cheby_fmt/            formatter (M5)
    cheby_lsp/            language server (M5)
    cheby_doc/            documentation generator (M5)
    cheby_pkg/            dependency resolution and fetching (M5)
    cheby_cli/            the `cheby` binary
  runtime-js/             JS runtime, ES modules (M4)
  std/                    standard library sources (Cheby, plus .mjs kernels)
  tests/
    ui/                   compile-time rule tests (D-275)
    conformance/          harness that runs docs/examples against goldens
    repl/                 scripted REPL sessions
  bench/                  synthetic project generator and runtime benchmarks
  xtask/                  developer commands: bless, gen-bench, check-determinism
  docs/                   spec, ADRs, decision log, examples, plan, stdlib spec
```

Crate boundaries enforce the locality rule (D-246): `cheby_types` checks a module given its own syntax and the `ModuleInterface` values of its dependencies, and has no API that returns another module's bodies. A feature that needs one would not compile against these crates, which is the point.

## 2. Compiler pipeline

```
source ─► lex ─► parse ─► CST ─► AST views
                                   │
                         item collection, imports, name resolution
                                   │
                         module interface (signatures, types, interfaces, const types)
                                   │                          ▲ interfaces of dependencies
                         per-function type checking (parallel)
                                   │
                         typed bodies (THIR)
                                   │
                         MIR lowering (per function): pattern decision trees,
                         closure conversion, dictionaries, explicit dup/drop
                                   │
                         RC passes: naive (M1), Perceus borrowing and reuse (M2)
                                   │
              ┌────────────────────┼─────────────────────┐
     Cranelift IR ─► JIT     Cranelift IR ─► object    JS emitter ─► ES modules
     (run, repl, const)      (build, cache)            (link-time suspension analysis)
```

Per-module steps run once per module in import-graph order, with independent modules in parallel. Per-function steps run in parallel inside a module (D-263). The exceptions are the per-module steps named by D-263: borrowing inference (D-250) and per-type layout tables (D-252).

### 2.1 Syntax

- Hand-written lexer and recursive-descent parser over a lossless concrete syntax tree (D-272). The same tree serves the compiler, the formatter and the language server.
- The parser emits events (start node, token, finish node), and a builder turns them into a flat, arena-allocated tree written for Cheby (D-280). Later phases see it only through typed AST views. Cursors, parent links and incremental reparsing for the language server live in `cheby_syntax`.
- The lexer emits an `NL` for every line break outside strings and comments, and the parser decides continuation (D-231).
- String interpolation is lexed into segments, so `"{x:?}"` produces tokens the parser can check ([§2.5.4](../spec/02-lexical-structure.md)).
- Error recovery is required from the start, because the language server parses incomplete code.
- Parsing is checked against `tree-sitter-cheby` on every example and UI test, so the editor grammar and the compiler cannot drift silently ([testing.md](testing.md#5-parser-cross-check)).

### 2.2 Names and modules

- `cheby_package` reads `cheby.toml` ([§13.3](../spec/13-tooling.md#133-manifest)), maps `src/` files to module paths ([§7.2](../spec/07-modules-and-packages.md#72-modules-and-files)) and builds the import graph, rejecting cycles (D-145).
- `cheby_resolve` collects items per namespace ([§7.7](../spec/07-modules-and-packages.md#77-namespaces-and-name-resolution)), applies visibility (`pub`, package-visible, `priv`, D-162) and `exposed` (D-048), the prelude and its hiding warning (D-146), naming conventions (D-059) and the shadowing ban (D-051, D-062).

### 2.3 Module interfaces

A `ModuleInterface` holds exactly what [§13.12.1](../spec/13-tooling.md#13121-the-locality-rule) lists: function signatures, types, interfaces, aliases and constant types. In M1 it lives in memory and is hashed. In M3 it is serialized to the cache as the interface artifact (D-249), and release builds add small inlinable bodies (D-251).

Interface hashing is designed in M1, even though nothing uses it for caching yet, so that the REPL and the language server can already skip re-checking dependents whose inputs did not change.

### 2.4 Type checking

- Inference is local to one top-level body (D-011, D-234). Statements are checked in order, closures last and operators deferred, as in [§3.12.4](../spec/03-types.md#3124-order-of-inference).
- Bidirectional checking supplies expected types ([§3.12.5](../spec/03-types.md#3125-expected-types-and-conversions)), which is also where implicit `dyn` conversion happens (D-168, D-235).
- Deferred constraints (operators, interface calls, `dyn` conversions) are solved at the end of the body after numeric literals are defaulted.
- Interface satisfaction looks up same-named functions in the type's own module, one type per interface per module (D-045), with conditional conformance (D-064) and embedding.
- Exhaustiveness and reachability use a usefulness algorithm (Maranget) over constructors, literals, list patterns and multiple subjects ([§6.9](../spec/06-patterns.md#69-exhaustiveness-and-reachability)).
- `use`, pipes (D-244, D-245) and captures are desugared during checking, so later stages see only calls and closures.
- Cross-target checks of `@target` (D-056, D-190) run on every build, whatever the selected target.

### 2.5 MIR

MIR is the single mid-level IR that feeds every backend (D-019). It is a control-flow graph of basic blocks per function, in a form close to SSA, with operations at the level of the runtime model:

- allocate a constructor, read a field, test a tag, build a tuple or closure,
- direct, indirect and tail calls, with tail position marked by lowering ([§5.15.2](../spec/05-expressions.md#5152-tail-position)),
- checked arithmetic and comparisons on concrete numeric types,
- dictionary (function package) construction and calls through it ([§8.5.1](../spec/08-interfaces.md#851-implementation-informative)),
- `dyn` packaging with type descriptors (D-236),
- explicit `dup` and `drop`, from M1 (D-273), with `drop_reuse` and `reuse` added by Perceus in M2,
- panics with message and source location, and cleanup edges for unwinding in M2 (D-262).

Lowering compiles patterns to decision trees, converts closures and local functions to environment records plus code pointers, and lowers interpolation to string-builder calls.

From M1, `dup` and `drop` already follow the last-use rule (D-205, ADR-0036): a binding is dropped right after its last use, and a binding unused in a `case` branch is dropped at the start of that branch. M1 places them naively, one `dup` per extra use. M2 only optimizes the same instructions: borrowing removes pairs inside a module (D-250), and reuse turns `drop` plus allocation into in-place updates (D-005).

The MIR also carries what the JS backend needs: which calls are tail calls, which operations suspend directly, and which values may contain handles (D-172).

### 2.6 Native code generation

- One Cranelift lowering serves the JIT, the AOT object writer and the constant evaluator.
- Tail calls use the `tail` calling convention and `return_call` (D-015, ADR-0006).
- Every function starts with the combined yield and stack-limit check (D-028, D-124). In M1 it is only a stack-limit check against the main thread's stack.
- Every top-level function is called through an indirection table in JIT code (D-043). The REPL redefines through it in M1, lazy compilation uses it in M3 (D-254).
- In M2, calls that may panic get `try_call` landing pads with cleanup blocks shared between calls (D-077, D-262).

### 2.7 Value representation

Native values (D-278, D-279):

- A value is one 64-bit word in the uniform representation (D-021). A word with low bit 1 is a 63-bit tagged integer. A word with low bit 0 is a pointer to a heap object, or to a static object for constants (D-090).
- Monomorphic code keeps `Int`, `Float` and sized numbers unboxed in registers, and boxes them only when they flow into the uniform representation. An `Int` outside the 63-bit range is boxed there, and a boxed `Float` is a heap object.
- `dup` and `drop` skip tagged integers with one bit test.
- Every heap object starts with an 8-byte header:

  ```
  bits 0-31   reference count   > 0 local, < 0 shared and atomic (D-072), one reserved value static (§9.7)
  bits 32-39  scan count        leading pointer fields visited when freeing, with an escape value for more
  bits 40-63  layout id         index into the global layout table
  ```

  For example, `Some(x)` is 16 bytes: the header with scan count 1 and the layout id of `Option.Some`, then `x`.

- The layout table gives each layout's type, constructor tag and field layout. One runtime routine walks it for equality, hashing and debug printing in debug builds (D-252), and the JS runtime uses the equivalent to find handles in flagged values (D-172). REPL type redefinitions (D-043) add new layouts.
- Hashing goes through one routine with a per-target hash function: keyed SipHash-1-3 on native, a keyed 32-bit hash on JS (D-281).

## 3. Runtime

`cheby_runtime` is Rust, built as an rlib for the JIT and as a staticlib for AOT linking. It exposes a C ABI to generated code.

| Area                   | Milestone | Contents                                                                                 |
| ---------------------- | --------- | ---------------------------------------------------------------------------------------- |
| Memory                 | M1        | allocator, object header, `dup`/`drop`, recursive free, leak counter in debug builds     |
| Kernels                | M1        | `String`, `Bytes`, RRB `List`, HAMT `Map` and `Set` (D-039, D-274)                       |
| Generic operations     | M1        | equality, hashing (seeded per process, D-093), debug printing from layout tables (D-252) |
| Panics and exit        | M1        | panic messages with location, exit codes of §10.10, stack-overflow detection             |
| Shared values          | M2        | shared marking traversal, atomic counts (D-072)                                          |
| Fibers                 | M2        | stack regions (D-124, D-125), context switch, M:N scheduler, scopes, cancellation        |
| Channels and selectors | M2        | bounded channels, close on last handle (D-054), selectors (D-041, D-206)                 |
| Unwinding              | M2        | landing pads, handle drop functions (D-101, D-171)                                       |
| IO                     | M2        | poller, timers, blocking pool (D-042)                                                    |
| FFI support            | M3        | `cheby.h` for foreign code (D-187)                                                       |

Kernels check uniqueness at run time where Perceus cannot see inside them: an insert into a `List`, `Map` or `Set` whose count is one updates in place.

The JS runtime in `runtime-js/` mirrors this split: JS kernels for collections and strings (D-274), a cooperative fiber scheduler, channels, selectors, timers and handle reference counting (D-100).

## 4. Driver, cache and processes

- `cheby_driver` schedules modules over the import graph with a work-stealing pool, and functions inside a module in parallel (D-263).
- `cheby check` runs the front end without code generation or output (D-282), and is the command CI and editors use for checking.
- In M1 every build is from scratch, in memory. M3 adds the global content-addressed cache (D-255), interface artifacts with early cutoff (D-249) and cached code for `cheby run` (D-254).
- Every command is its own process, and the cache is the only shared state (D-268). The language server keeps its own in-memory state and may use queries internally (D-249).
- Output is deterministic from M1 on: no iteration over unordered maps when producing output, no absolute paths, no time stamps, and results independent of thread count (D-264). CI checks it ([testing.md](testing.md#6-determinism)).

## 5. Standard library location

`std/` holds the standard library sources: Cheby modules and the JS kernels. In M1 and M2 the toolchain compiles `std` from source on each build, with the result cached in memory. From M3 it ships precompiled (D-256). [stdlib.md](stdlib.md) describes the content.
