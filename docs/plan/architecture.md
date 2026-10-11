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
  fuzz/                   cargo-fuzz targets: lex, parse and check
  xtask/                  developer commands: bless, gen-bench, check-determinism, test-std, bench-compile
  docs/                   spec, ADRs, decision log, examples, plan, stdlib spec
```

Crate boundaries enforce the locality rule (D-246): `cheby_types` checks a module given its own syntax and the `ModuleInterface` values of its dependencies, and has no API that returns another module's bodies. A feature that needs one would not compile against these crates, which is the point.

The crates of the compiler depend on each other in pipeline order (D-284, D-465):

- `cheby_diag` and `cheby_syntax` are at the bottom, with `cheby_package` above them.
- `cheby_iface`, the interface model, depends only on `cheby_diag` and `cheby_package`. It is the shared vocabulary of every later phase.
- `cheby_resolve` depends on `cheby_iface`, because name resolution reads the interfaces of dependencies. `cheby_types` depends on both.
- `cheby_mir` depends on `cheby_types` and `cheby_iface`.
- `cheby_codegen_clif` depends on `cheby_mir`, `cheby_iface`, `cheby_diag` and `cheby_runtime`. From the runtime it uses only the `abi` module, which has no state and no dependencies and is the single source of header bits, layout ids, context offsets and runtime symbol names.
- `cheby_jit` depends on `cheby_codegen_clif`, `cheby_mir`, `cheby_iface` and `cheby_runtime`, and `cheby_driver` on all of the above.

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

Per-module steps run once per module in import-graph order, with independent modules in parallel. Per-function steps run in parallel inside a module (D-263). The exceptions are the per-module steps named by D-263: borrowing inference (D-250) and per-type layout tables (D-252). In M1 a build lowers, counts and checks the MIR of each needed module in parallel on rayon's global pool and takes the units in build order. Code generation then translates and compiles the functions of each batch in parallel on the same pool, collects them in declaration order and defines them in that order. When several fail, the error of the first module or function in that order is reported, so CLIF dumps, the JIT image and diagnostics are the same for any number of threads (D-264, D-632).

### 2.1 Syntax

- Hand-written lexer and recursive-descent parser over a lossless concrete syntax tree (D-272). The same tree serves the compiler, the formatter and the language server.
- The parser emits events (start node, token, finish node), and a builder turns them into a flat, arena-allocated tree written for Cheby (D-280). Later phases see it only through typed AST views. Cursors, parent links and incremental reparsing for the language server live in `cheby_syntax`.
- The lexer emits an `NL` for every line break outside strings and comments, and the parser decides continuation (D-231).
- String interpolation is lexed into segments, so `"{x:?}"` produces tokens the parser can check ([§2.5.4](../spec/02-lexical-structure.md)).
- Error recovery is required from the start, because the language server parses incomplete code.
- Parsing is checked against `tree-sitter-cheby` on every example and UI test, so the editor grammar and the compiler cannot drift silently ([testing.md](testing.md#5-parser-cross-check)).

### 2.2 Names and modules

- `cheby_package` reads `cheby.toml` ([§13.3](../spec/13-tooling.md#133-manifest)), maps `src/` files to module paths ([§7.2](../spec/07-modules-and-packages.md#72-modules-and-files)) and builds the import graph, rejecting cycles (D-145).
- `cheby_resolve` collects items per namespace ([§7.7](../spec/07-modules-and-packages.md#77-namespaces-and-name-resolution)), applies visibility (`pub`, package-visible, `priv`, D-162) and `exposed` (D-048), the prelude and its hiding warning (D-146), naming conventions and reserved words at bindings and declarations (D-059, D-469) and the shadowing ban (D-051, D-062).
- The prelude's items are declarations of a builtin module in `cheby_iface`, with no source. Each type records its home module in `std`, which re-exports it, and every module of the local package depends implicitly on the home modules the toolchain ships, so interface satisfaction for builtin types stays within its dependencies (D-447). `Map`, `Set` and `Bytes` are items of the same builtin module that are not in the prelude, named through their home modules `std::map`, `std::set` and `std::bytes`, which do not declare them again (D-481).

### 2.3 Module interfaces

A `ModuleInterface` holds exactly what [§13.12.1](../spec/13-tooling.md#13121-the-locality-rule) lists: function signatures, types, interfaces, aliases and constant types. In M1 it lives in memory and is hashed. In M3 it is serialized to the cache as the interface artifact (D-249), and release builds add small inlinable bodies (D-251).

Interface hashing is designed in M1, even though nothing uses it for caching yet, so that the REPL and the language server can already skip re-checking dependents whose inputs did not change. The canonical encoding holds the items that are not `priv`, not tests and not replaced by a REPL redefinition, sorted by namespace and name, and refers to other definitions by module path and name, never by internal indexes. Its hash is SipHash-1-3 with a 128-bit result and zero keys (D-460).

### 2.4 Type checking

- Inference is local to one top-level body (D-011, D-234). Statements are checked in order, closures last and operators deferred, as in [§3.12.4](../spec/03-types.md#3124-order-of-inference).
- Bidirectional checking supplies expected types ([§3.12.5](../spec/03-types.md#3125-expected-types-and-conversions)), which is also where implicit `dyn` conversion happens (D-168, D-235).
- Deferred constraints (operators, interface calls, `dyn` conversions) are solved at the end of the body after numeric literals are defaulted.
- Interface satisfaction looks up same-named functions in the type's own module, one type per interface per module (D-045), with conditional conformance (D-064) and embedding. For a type entered at the REPL prompt, the session's current functions count, whichever entries declared them (D-610).
- Exhaustiveness and reachability use a usefulness algorithm (Maranget) over constructors, literals, list patterns and multiple subjects ([§6.9](../spec/06-patterns.md#69-exhaustiveness-and-reachability)). Each pass has a fixed step budget, and rows are grouped by their first constructor or literal (D-508). The search keeps its pending calls on an explicit heap stack, in the order a recursion would make them, so a list pattern of thousands of elements never deepens the Rust stack (D-633).
- `use`, pipes (D-244, D-245) and captures are desugared during checking, so later stages see only calls and closures.
- Cross-target checks of `@target` (D-056, D-190) run on every build, whatever the selected target.
- The output is THIR, typed bodies that MIR lowering reads without inference or interface search. Inside a local function's body every reference to its own name is a self reference, never a capture. A `dyn I` type is closed during inference, and every dictionary carries its type's descriptor, a type parameter's coming from the dictionary of its first bound (D-499). A float literal holds two values, its text rounded once to the nearest `Float` and once to the nearest `F32`, and every later phase, constant evaluation and the REPL included, reads the one of the literal's type and never narrows the `Float` value itself (D-569).
- The warnings of [§13.8](../spec/13-tooling.md#138-diagnostics-and-warnings) that need only one module run per module after checking. The whole-package pass for unused package-visible items runs once per `cheby check` (D-266, D-516).

### 2.5 MIR

MIR is the single mid-level IR that feeds every backend (D-019). It is a control-flow graph of basic blocks per function, in a form close to SSA, with operations at the level of the runtime model:

- allocate a constructor, read a field, test a tag, build a tuple or closure,
- direct, indirect and tail calls, with tail position marked by lowering ([§5.15.2](../spec/05-expressions.md#5152-tail-position)),
- checked arithmetic and comparisons on concrete numeric types,
- dictionary (function package) construction and calls through it ([§8.5.1](../spec/08-interfaces.md#851-implementation-informative)),
- `dyn` packaging with type descriptors (D-236),
- explicit `dup` and `drop`, from M1 (D-273), with `drop_reuse` and `reuse` added by Perceus in M2,
- panics with message and source location, and cleanup edges for unwinding in M2 (D-262).

Lowering compiles patterns to decision trees, which it builds, lowers to blocks and frees with explicit stacks, so no step recurses per element of a list pattern (D-633). It converts closures and local functions to environment records plus code pointers, and lowers interpolation to string-builder calls.

From M1, `dup` and `drop` already follow the last-use rule (D-205, ADR-0036): a binding is dropped right after its last use, and a binding unused in a `case` branch is dropped at the start of that branch. M1 places them naively, one `dup` per extra use. M2 only optimizes the same instructions: borrowing removes pairs inside a module (D-250), and reuse turns `drop` plus allocation into in-place updates (D-005).

The MIR also carries what the JS backend needs: which calls are tail calls, which operations suspend directly, and which values may contain handles (D-172).

Every machine signature comes from one module, `cheby_mir::abi`, used by both lowering and code generation, so a slot, a `return_call` and its callee never disagree (D-453, [ADR-0055](../adr/0055-native-calling-convention.md)):

- A top-level function, or the wrapper of an `@external`, takes one uniform word per dictionary, then its parameters in the representation of their declared types, and returns one uniform word.
- A closure, local function, function value, dictionary entry or `dyn` call uses the uniform convention: the owned closure object `self`, then uniform words, returning a uniform word. A local function refers to itself through `self`, and a function value of a top-level function is a closure whose code calls the function through its slot.
- Entry thunks and one trampoline per arity use the platform C convention. They are the only way the runtime enters generated code.
- The hidden `ctx` parameter is not part of MIR signatures. Code generation adds it first to every signature except foreign ones.

A function with `@external(native, …)` lowers to a Cheby-ABI wrapper in its own module that makes the foreign call, so callers call it through its slot like any function (D-455). On native, a function with such an attribute and a body lowers to the wrapper only, and its body, which is for the other targets, is not lowered (D-510). A function whose only implementation is `@external(js, …)` stays in the native program: its body calls the effect check with the function's path, then panics with "`<path>` exists only on the `js` target", so a constant whose initializer reaches it gets E0077 instead of an internal compiler error (D-591).

`cheby run` and the driver's MIR and CLIF dumps lower only the items reachable from the `main` being run, across modules, with the constructor layouts of every module that reachable code uses. Every reachable item is lowered before anything is compiled, so one run reports every E0038 of a back end that cannot compile a construct yet, after the front end's diagnostics (D-510). When a module gets a code unit, the `@std_hooks` functions of every type it declares, and what they call, are reachable too, whether or not reachable code mentions the type's constructors, because values of a hooked type reach `==` and debug printing through std code that never names them (D-585).

### 2.6 Native code generation

- One Cranelift lowering serves the JIT, the AOT object writer and the constant evaluator.
- Tail calls use the `tail` calling convention and `return_call` (D-015, ADR-0006).
- Every Cheby function takes the hidden `ctx` pointer first and passes it on in every call (D-453). It starts with the combined yield and stack-limit check (D-028, D-124), which compares the stack pointer with the limit at offset 0 of `ctx`. Setting the limit to the highest address forces the next function entry into the runtime's slow path, which is how yields and the deadline of constant evaluation (D-195) will interrupt running code. In M1 it is only a stack-limit check against the entry's stack (D-457).
- Every top-level function is called through an indirection table in JIT code (D-043). The REPL redefines through it in M1, lazy compilation uses it in M3 (D-254). A slot is identified by the item's path and its ABI signature: a redefinition with the same signature repoints the slot, and one with another signature gets a new slot, so a slot never points at code of another signature (D-456). Because two type signatures can share an ABI, a REPL function takes over the old slot only when its type signature is equal, and a constant only when its type is the same. Any other redefinition gets a new slot ending in `$r<gen>`, a constructor never takes over anything, and session values never share a slot. A rejected entry points the slots it repointed back at their old code (D-607).
- Symbols are `cb$<kind>$<module path>$<item>…`, built from paths and names only, with the kind first: `fn`, `slot`, `shim`, `fnval`, `clo`, `cinit`, `const`, `test`, `entry`, `tramp`, `str`, `ctor`, `desc`, `dict`, `layout`, `tfn` and `dynfwd`. A REPL redefinition with a new signature appends `$r<generation>` (D-456). Closure `n` of a function or constant is `cb$clo$<module path>$<item>$<n>`, and closure `n` of a test, local functions included, is `cb$clo$test$<module path>$<ordinal>$<n>`, with the test's ordinal as in `cb$test$<module path>$<ordinal>`, so a test's free-text name never collides with an item (D-572). The code of a built-in tuple interface function (D-167) is `cb$tfn$<module path>$<interface function>$<arity>`, with the interface function's module path (D-560). The forwarder through which a `dyn J` value satisfies a bound is keyed by the interface function `f` and by `J`, because `f`'s field differs between a `J` dictionary and a dictionary of `f`'s own interface `I`. It is `cb$dynfwd$<module path of f>$<I>$<f>$$<module path of J>$<J>`, as in `cb$dynfwd$geo$Shape$area$$geo$Named`. It takes the closure `self`, the `dyn J` receiver and the other arguments, loads `f`'s closure from the `J` dictionary inside the receiver and tail-calls it with the inner value (D-602). The built-in tuple `Show::show` has no source, so its function-entry location is `<builtin>`, while the tuple code of `Compare::compare` takes the location of `Compare::compare` in `std::ops` (D-583). Every symbol of a redefined item, not only its slot, carries the item's generation when it is above 0, and `:reload` gives every item of a changed module a new generation (D-547). REPL session items use the module path `<repl>` and their session name, as in `cb$fn$<repl>$f$r1`. An item's generation is the smallest number above that of every item compiled before it with the same top-level name or the same text, so the constructors `Red` of two session types get `cb$layout$<repl>$Red` and `cb$layout$<repl>$Red$r1`. An item counts as compiled even when its entry is then rejected, so no symbol is defined twice (D-611). The forwarder through which a session function satisfies an interface for a session type uses the session name `T::f` with a fresh generation, as in `cb$fn$<repl>$Cat$show` (D-627). A module version made by `:reload` has an internal module path whose last segment carries the reload's generation, as in `store::prices<r1>`, so its symbols never collide with the old version's (D-625).
- Dictionaries, type descriptors and function values that need nothing from their caller are static objects with count 0, emitted by code generation (D-523). A static dictionary, descriptor or dictionary closure is `cb$<kind>$<module path>$$<name>`: the module path joined with `$`, then `$$`, then the name exactly as MIR built it, `::` paths included, as in `cb$dict$app$app$$app::m::I$app::t::T` and `cb$desc$geo$$Point`. A module path never holds `$$`, so the first `$$` ends it, and two modules can no longer produce one symbol (D-579). So a nominal descriptor is `cb$desc$<module path>$$<Type>` and `dyn I` is `cb$desc$<module path of I>$$dyn$<I>`, one static per JIT. The builtin `Show` has an empty module path, so `dyn Show` is `cb$desc$$$dyn$Show`, and the MIR validator accepts an empty module path for these descriptors only (D-603). The static dictionary through which `dyn J` satisfies `I` is `<I path>$dyn <J path>`: the descriptor of `dyn J`, a static `Closure(0)` per function of `I` that holds its forwarder, then the forwarding dictionaries of `I`'s embedded interfaces (D-602). In a REPL session, every `<repl:N>::X` of a session item in a static's name becomes `<repl>::X` with its generation, so a session type's descriptor is `cb$desc$<repl>$$X$r<gen>`. The dictionaries, dictionary closures and structural descriptors that entries build are statics of the module `<repl>`, so an identical static of two entries is one symbol (D-611). The static function value of a top-level function keeps `cb$fnval$<module path>$<item>`, a closure whose code is the shim `cb$shim$<module path>$<item>` (D-523, D-579). Builtin types have the runtime statics `cheby_rt_desc_<name>`.
- Static names are built from types printed with full module paths, such as `List<app::shapes::Circle>`, `(Int, String)` and `fn(Int) -> Bool`, with builtin types and interfaces bare. A dictionary is `<interface path>$<type>`, as in `std::ops::Compare$(Int, String)`, closure `n` of a static dictionary is `<dictionary name>$<n>` with kind `fnval`, `n` being the function's position in the dictionary, and the descriptor of an applied type is `<type>` with kind `desc`. The module path is the unit's own, and in the entries unit, which has no module, that of the entry item whose error type's `show` needs the dictionary. A satisfying function that needs dictionaries (D-064) becomes a static closure whose layout is `Closure(k)` for its `k` captures: its value shim's code, then the static dictionaries it needs. A tuple dictionary's function (D-167) is a static closure of the tuple function's code and the static element dictionaries (D-580).
- A static is defined once per JIT, the program image of one build or one REPL session, and separate JITs in one process define their own (D-523, D-561). The first unit or batch of a JIT that names a static defines it. A later one that names it again compares the new object's layout, strings and fields with the first one's, and different contents are an internal compiler error (D-579). Value shims, built-in tuple functions, `dyn` forwarders and trampolines, whose code depends only on their key, follow the same rule: a later declaration with the same signature reuses the first code, and any other function declared twice is an internal compiler error (D-561).
- Besides kernels, generated code calls a fixed set of runtime helpers through the C ABI: string building and equality, list pushes, appends, lengths, indexing and slices, structural equality, debug text, the effect check of constant evaluation, boxing outside the tagged range, float `%` (`fmod`) and the forcing of a constant (D-520). Structural equality is `cheby_rt_equal(ctx, a, b, location) -> bool`, where `location` is a static `String` holding the source location of the `==` or `!=`, or 0 when unknown, so a panic on a function value reports where the comparison is written. Like every operator panic, that location is the start of the whole comparison, its left operand (D-570). MIR passes it as a third operand of `RuntimeFn::Equal`, and the MIR validator accepts only a string constant there (D-556). `cheby_rt_list_slice_owned(ctx, list, start, end) -> u64` consumes `list` and returns an owned slice, clamped as by `cheby_rt_list_slice` (D-298). It returns the list itself for the whole range and changes the root in place when the list's count is one. MIR's RC pass emits it, as `RuntimeFn::ListSliceOwned` printed `list_slice_owned`, instead of `list_slice` followed by a drop of the list when the slice is the list's last use, so a recursive walk over `[x, ..rest]` allocates nothing per step (D-634). A helper takes `ctx` first when it allocates, may panic or may call back into Cheby code. Tagging, untagging, unboxing and the fast paths of boxing are inline.
- Native symbols are resolved against the runtime and the process before any code runs. Every missing one is E0053 at its `@external`, sorted by position (D-455, D-532). Under `cheby check`, which compiles code only to evaluate constants, a missing symbol is bound to the stub `cheby_rt_unresolved`, and only a constant that calls it is an error (D-532).
- The JIT imports a native symbol without a signature, as a data import whose address the call site loads and calls indirectly with its own C signature, so REPL entries, including failed ones, may bind one symbol with different signatures (D-475). AOT objects may keep typed function imports, since a program gives each symbol one signature.
- JIT code and data live in one address-space reservation of 1 GiB per JIT, so that every PC-relative relocation reaches on x86_64 and aarch64. Pages are committed per segment as they are allocated and made read-execute or read-only when finalized (`cheby_jit::memory`).
- In M2, calls that may panic get `try_call` landing pads with cleanup blocks shared between calls (D-077, D-262).

### 2.7 Value representation

Native values (D-278, D-279):

- A value is one 64-bit word in the uniform representation (D-021). A word with low bit 1 is a 63-bit tagged integer. A word with low bit 0 is a pointer to a heap object, or to a static object for constants (D-090).
- Monomorphic code keeps `Int`, `Float` and sized numbers unboxed in registers, and boxes them only when they flow into the uniform representation. An `Int` outside the 63-bit range is boxed there, and a boxed `Float` is a heap object.
- Every number has exactly one uniform shape (D-521). `Int` and `I64` are tagged when they fit in 63 bits and a boxed `Int` otherwise. `I8` to `I32` and `U8` to `U32` are always tagged, sign-extended for the signed types. `U64` is tagged up to 2⁶² − 1 and a boxed `U64` above. `Float` and `F32` are always boxed, and `Bool` is the static `True` or `False`. Word equality therefore decides `==` for tagged values.
- `dup` and `drop` skip tagged integers with one bit test.
- Every heap object starts with an 8-byte header:

  ```
  bits 0-31   reference count   n > 0 local, -n shared and atomic (D-072), 0 static (§9.7, D-454)
  bits 32-39  scan count        leading pointer fields visited when freeing, with an escape value for more
  bits 40-63  layout id         index into the global layout table
  ```

  For example, `Some(x)` is 16 bytes: the header with scan count 1 and the layout id of `Option.Some`, then `x`.

  Only the 32-bit count is written after allocation, and never for a static object. A count that would overflow makes the object static (D-454).

  An object with more than 254 leading uniform fields has the scan count 255, the escape value (D-279, D-555). A constructor keeps its fields where its layout puts them, and freeing, equality, hashing and debug text read them from the layout table. A tuple holds its arity as a tagged integer at offset 8, then its elements. A closure, dictionary or descriptor keeps its first field, the code word, type descriptor or nominal descriptor, at offset 8, then the number of the fields after it as a tagged integer at offset 16, then those fields, so indirect calls and readers of a dictionary's descriptor never check for the escape. Static dictionaries and descriptors have the same shape (D-555).

- Nullary constructors (`Nil`, `True`, `False`, `None`, `Less`, `Equal`, `Greater` and user unit variants) are static objects, so every uniform value is self-describing. `Bool` is unboxed, one byte, in monomorphic code (D-454).
- A `String` is the header, `len: u64`, `cap: u64`, then the UTF-8 bytes. Literals are static, with `cap` equal to `len`. `Bytes` has the same shape (D-454).
- A closure is the header, the code address stored as `addr | 1`, then its captures (D-453).
- Constructor fields of type `Int`, `Float`, `Bool` or a sized number are stored unboxed, after the uniform fields, so freeing needs only the scan count. Each such raw field fills its whole 8-byte slot: `Int`, `I64`, `U64` and `Float` as they are, `I8` to `I32` sign-extended, `U8` to `U32` and `Bool` zero-extended, and an `F32` in the low 32 bits with the high 32 bits zero (D-562).
- A non-empty list is an RRB tree (D-039) of uniform words with branching factor 32 (D-563). A root, layout id 26, is the list value and a view: the header, the tree, then the view's offset into it and its length, both raw. A heap root always has a length above 0, and the empty list is the static `cheby_rt_static_empty_list`. A leaf, id 27, holds a raw word with the length in its low 32 bits and the capacity in its high 32 bits, then up to 32 element slots, and a unique leaf doubles its capacity when it grows. A branch, id 28, holds a raw word with the child count in its low 32 bits and the height in its high 32 bits, leaves having height 0, then 32 child slots and 32 cumulative sizes, so balanced and relaxed nodes are one kind. Concatenation merges the trees along the seam (Bagwell and Rompf) and repacks the nodes next to it when they use more than 2 nodes beyond the fewest that could hold their slots. A kernel never updates an argument in place when another argument is the same object, as in `list::append(xs, xs)`, and `list::prepend` updates a unique left edge in place. `cheby_rt_list_get` out of range is a fatal error, because lowering checks the length first, and `cheby_rt_list_slice` clamps its bounds like `list::slice` (D-563).

- The layout table gives each layout's type, constructor tag and field layout. One runtime routine walks it for equality, hashing and debug printing in debug builds (D-252), and the JS runtime uses the equivalent to find handles in flagged values (D-172). REPL type redefinitions (D-043) add new layouts.
- Layout ids 1 to 63 are fixed (D-454):

  ```
  1 String     2 Bytes      3 boxed Int   4 boxed Float   5 boxed F32   6 closure   7 tuple   8 boxed U64
  9 dictionary 10 dyn value 11 descriptor 12 nominal descriptor         13-15 reserved
  16 Nil       17 False     18 True       19 Less         20 Equal      21 Greater
  22 Some      23 None      24 Ok         25 Err
  26 list root 27 list leaf 28 list branch  29-31 unused
  32-39 map and set nodes                 40-63 reserved
  ```

  Layout ids 9 to 12 and the collection ranges are assigned by D-524, and the list nodes by D-563. A dictionary holds the header, its type's descriptor, one closure per function of the interface, embedded interfaces' functions first, then the dictionaries of the directly embedded interfaces. A `dyn` value holds the header, its dictionary and the value. A descriptor holds the header, a nominal descriptor and the descriptors of its type arguments, and a nominal descriptor, one static per type, holds the type's printed name and module path (D-524).

  A type without type arguments is described by its nominal descriptor itself, and any other type by a descriptor of its nominal descriptor and one or more argument descriptors. A tuple uses `cheby_rt_desc_tuple` with its elements, and a function type `cheby_rt_desc_fn` with its parameters, then its result. A closed descriptor is static, and one that mentions a type parameter is built at run time from field 0 of the parameter's first bound's dictionary (D-499). A builtin nominal descriptor's printed name is the type's name, or `Tuple` and `Fn`, and its module path is empty. Debug text writes a descriptor as `Name<A, B>`, `(A, B)` or `fn(A, B) -> C` (D-581). Two descriptors are equal when their nominal descriptors are the same object and their arguments are pairwise equal, and hashing writes the nominal descriptors' module paths and names, the argument count and the arguments, never an address (D-582).

  Constructor tags follow declaration order. User layouts get ids from 64 on, registered in build order before the code that uses them. Generated code never embeds a user layout id, because ids come from a process-wide table: it loads the header from a per-layout data symbol, `cb$layout$<module path>$<Ctor>`, which the JIT or loader fills in at registration. The static object of a user nullary constructor, `cb$ctor$…`, gets its header filled in the same way (D-454). The constructors of one type are registered consecutively, so a value's tag is its layout id minus the id in the `cb$layout$` word of the type's first constructor. For `Option`, `Result` and `Order` it is the layout id minus 22, 24 or 19 (D-522).

- A value of an external type is a 16-byte object: the header, then the foreign handle as a raw word at offset 8. Each external type gets a user layout of the runtime kind `EXTERNAL_HANDLE`, registered by `cheby_runtime::layout::register_external(type_name, type_key)`, so it prints as `<Name>` and stops `==` and hashing with `EqualityOnFunction`. Its drop function (D-101) is not called yet, since M1 creates no external values and the native host library comes with M3 (D-590).

- Hashing goes through one routine with a per-target hash function: keyed SipHash-1-3 on native, a keyed 32-bit hash on JS (D-281). Every non-empty map or set records the seed it was built with, a constant's own seed during constant evaluation and the per-process seed otherwise, and hashing never uses layout ids (D-538). A constructor value hashes its type key and its tag, and equality compares them. A type key is SipHash-1-3 with a zero key over the module path written with `::`, empty for builtin types, then one `0xff` byte, then the type's bare name. `cheby_mir::type_key` and `cheby_runtime::layout::type_key` compute the same function (D-559).
- Two values of a type with `@std_hooks`, meaning equal type keys, are compared by the type's `equal` hook alone, whatever their constructors. Hashing one writes the constructor mark and the type key, then hashes, in the same walk, the value that the `hash` hook returns, never the tag. Its debug text is the `debug` hook's `String`, without quotes (D-584). Every hook of a module with a code unit is compiled, and the JIT registers the three function values in the layout table for every constructor of the type (D-585).
- Equality, hashing and debug text of a `Map` or `Set` run on the explicit worklist of the walk that reached it, so collections nested at any depth never deepen the Rust stack (D-586). Hashing one writes a mark byte, 16 for a map and 17 for a set, and the entry count, then the wrapping sum of per-entry hashes, each in a fresh SipHash-1-3 state keyed with the walk's seed. A map entry's hash is SipHash-1-3, keyed with the seed and the fixed second key `0x6d61_7065_6e74_7279`, over the key's hash, then the value's hash. Equality looks up each key of the first collection in the second by its hash under the second's seed and pushes the key pair and the value pair on the worklist. Only keys whose full 64-bit hashes are equal are compared in a nested walk. Debug text keeps each sorted group's items as segments of one text arena, nested groups by reference, and sorts them by their bytes (D-587), so it writes every character once.

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

Kernels check uniqueness at run time where Perceus cannot see inside them: an insert into a `List`, `Map` or `Set` whose count is one updates in place. The Cheby wrapper of a kernel owns its arguments and drops them after the call, and the kernel borrows them, so a kernel that sees an argument with count one may update it in place, `dup` it and return it (D-525). It never does so when another argument is the same object (D-563). The `std::string` kernels append, trim and slice a unique text in place, grow a copy to at least twice the old capacity, and return the argument itself when the result equals it (D-577), and so does `list::unique` when no element repeats (D-589). `map::insert` and `map::upsert` of a key `==` to a stored one replace the value and keep the stored key (D-588).

A kernel that calls a Cheby closure, such as `map::upsert`, does so through the runtime, which keeps a thread-local record of the running entry: its `ctx` and the trampolines that the JIT passes with the entry, one for each arity from 0 to 4. A call with more arguments is a fatal runtime error (D-526).

Every entry (`main`, a test, a constant's initializer, a REPL input) runs on its own stack with a fresh execution context, the `ctx` of generated code (D-457). In M1 the stack is a dedicated thread of the requested size, at least 512 KiB, whose limit leaves 256 KiB of headroom for the runtime and foreign calls. `main`, each test and the body of each REPL entry get 64 MiB, and other entries, constants entered at the prompt included, 8 MiB (D-125, D-596, D-613). A panic flushes standard output, prints its report on standard error (D-449), hands it to the host and parks the entry's thread forever. For a test or a constant's initializer, the runtime hands the report over without printing it, and `cheby test` prints it in the test's failure block (D-595). A `main` thunk returns its status of 0 or 1, and a test thunk prints nothing and returns one uniform word: the static `Nil` when the test passed, and otherwise the text of its `Err` as an owned `String`, made as for `main` (D-529, D-595). The host decides what follows: exit status 101, the next test, the next REPL input, or a compile error for a constant. OQ-P7 replaces the thread with a runtime-owned stack behind the same interface.

The runtime keeps one process-wide program state, the arguments and the name that `env::args` and `env::program_name` return. `run_main` sets it from its `RunConfig`, and `cheby run` passes the package name. Any other host that runs entries as the program, `cheby test` with the package name and the REPL, calls `cheby_runtime::entry::set_program(args, name)` before they run. An empty name selects D-369's fallback (D-578). The REPL calls it when the session starts, with no arguments and the package's name, or `repl` outside a package (D-612). The REPL reads its input lines with `cheby_runtime::io::read_input_line() -> Option<String>`, through the buffered reader of `io::read_line` (D-548). It flushes standard output first, removes a final `\n` or `\r\n`, replaces invalid UTF-8 with U+FFFD, and returns `None` at the end of input or when standard input cannot be read (D-575).

Debug builds of the runtime count live objects. When `main` returns with status 0 or 1 and objects are still live, `cheby run` prints `error: leak check failed: …` and exits with 102, so goldens never record a leak (D-445, D-458). There is no check after a panic or in the REPL. `cheby test` checks each passing test the same way (D-541), and with `CHEBY_RT_POISON=1` a debug runtime poisons freed objects and stops with a fatal error when one is used again (D-544). Poisoning changes only running code, which then calls the checked `cheby_rt_dup` and `cheby_rt_drop`. The driver's CLIF dump always compiles with the default options and the inline fast paths, so a dump never depends on the environment (D-565).

The runtime counts live objects and bytes per accounting domain, not in one process-wide counter, so parallel tests and several JITs in one process never count each other's objects. Each JIT has its own domain, which every entry of its build charges. Running an entry makes the JIT's domain the thread's current one, and allocation, freeing and `forget` charge the current domain. `forget` takes a frozen object out of its domain exactly once. The leak check at exit reads the domain's live-object count as an absolute number, since constants frozen before `main` (D-539) have already left it, and so it also catches objects leaked by earlier entries of the build. The per-test check compares the domain before and after the test, and the memory limit of constant evaluation (D-536) counts the domain's live bytes minus those live when the top-level evaluation started, so constants evaluated earlier and REPL session values do not count. Every allocation, those of kernels included, is checked against the limit before memory is requested, so a single request over the limit is E0076, never an allocation failure (D-564, D-617).

Each accounting domain also counts allocations and the path each in-place operation took: list pushes and other root updates in place or copied, string appends in place, grown or copied, `String` and `Bytes` kernels in place, grown or copied, and map and set updates in place or copied. Counting is always on. With `CHEBY_RT_STATS=1`, read once per process, `cheby run` writes to standard error after `main` the line `runtime stats: main <time>, <n> entries <time>, peak <bytes> bytes`, then one line per counter. `CHEBY_RT_POISON=0` turns off the poisoning that the runtime's own unit tests use by default. Both are developer tools whose output is not a stable interface, and no program can observe the counters (D-635).

The JS runtime in `runtime-js/` mirrors this split: JS kernels for collections and strings (D-274), a cooperative fiber scheduler, channels, selectors, timers and handle reference counting (D-100).

## 4. Driver, cache and processes

- `cheby_driver` schedules modules over the import graph with a work-stealing pool, and functions inside a module in parallel (D-263). `-j N` is a global option of the `cheby` command. `check`, `run`, `test` and `repl` start rayon's global pool before they build, with N threads or by default the available parallelism, each named `cheby-worker-<i>` with a 256 MiB stack of reserved address space, because the checker recurses on the workers as it does on the command's own thread. Sizing the pool a second time with another count is the tool error "cannot start N worker threads: …" (D-622).
- The `cheby` command runs its subcommand on a thread named `cheby` with a 256 MiB stack, of which only address space is reserved up front, because parts of the compiler recurse over the source's structure. If the thread cannot start, the command prints "error: cannot start the compiler's thread: …" and exits with status 1, and a panic of the thread exits with 101 (D-601). No pass recurses per element of a list pattern any more: the usefulness search and the decision trees use explicit stacks, so 3,000 elements check on rayon's default 2 MiB workers and lower on a 1 MiB stack, and hosts that never size the pool, such as the driver's tests, the in-process benchmark and a future language server, handle such patterns on default stacks. The 256 MiB stacks of the command thread and of the workers stay as a margin for the passes that still recurse over the source's structure, such as along chains (D-519, D-633).
- `cheby check` runs the front end and evaluates constants with the JIT, but generates no other code and writes no output (D-282, D-459). Until E6 lands it checks constants without evaluating them (D-478). It is the command CI and editors use for checking. Errors of phase A (manifest, discovery, lexing, parsing, validation) stop the later phases (D-452). A phase-A warning denied by `--deny-warnings` does not: name resolution and type checking still run, and then the command fails before anything after the front end runs (D-621).
- `cheby repl` checks each entry as the module `<repl:N>` of the local package, or of a package `repl`, version `0.0.0`, outside one, for the `native` target only. When the session starts, it loads every shipped std module through a module `<package>::<repl>` that imports them all and that no entry can import (D-606). A definition checked again after a redefinition (D-609) is compiled as the module `<repl:K:n>`, where `K` is its entry and `n` counts the session's re-checks, and its items keep their session names (D-611). Entries are checked for `native` also in a package whose targets lack it (D-628). `:module` changes only checking: entries are checked against interfaces in which the entered module's `priv` items are package-visible, and code is generated from the real interfaces (D-624). Session functions satisfy interfaces for session types through forwarders: when an entry defines a non-`priv` session function whose parameters mention a session type, each such type's entry module gets a function with the same name, signature, visibility and targets that calls the session function through its slot. A redefinition with the same signature keeps the forwarder, any other definition of the name in the value namespace retires every forwarder of the name, and types of the package and std never get one (D-627). `:reload` checks a changed module and its importers as new versions under marked module paths, all or nothing, and a rejected reload takes its versions out of the build order and the table of displayed paths again (D-625).
- In M1 every build is from scratch, in memory. M3 adds the global content-addressed cache (D-255), interface artifacts with early cutoff (D-249) and cached code for `cheby run` (D-254).
- Every command is its own process, and the cache is the only shared state (D-268). The language server keeps its own in-memory state and may use queries internally (D-249).
- Output is deterministic from M1 on: no iteration over unordered maps when producing output, no absolute paths, no time stamps, and results independent of thread count (D-264). CI checks it ([testing.md](testing.md#6-determinism)).

## 5. Standard library location

`std/` holds the standard library sources: Cheby modules and the JS kernels. In M1 and M2 the sources are embedded in the `cheby` binary, and each build parses and checks only the std modules its imports reach and the shipped home modules (D-447), with the result cached in memory. A user build neither checks nor lowers the `test` blocks of std modules, which are checked and run only when std itself is tested (D-479), through an in-memory package `std_self_test` whose root module imports every shipped std module, in a front-end mode that only the self-test uses (D-599). Std files are shown as `std/<module path>.cheby` in diagnostics and panic locations. Std modules may declare `@external(js, …)` bindings before their `.mjs` files exist, since nothing checks JS module files before M4 (D-450). Every kernel is bound as `@external(native, "cheby_<module>_<function>")` and `@external(js, "./<module>.mjs", "<function>")`, and a kernel never calls a Cheby closure (D-484). A std module written before the checker can check it is listed in `STAGED_STD_MODULES` of `cheby_driver`: it is embedded and parsed in tests but never loaded, and importing it stays E0038 until it moves to `STD_MODULES` (D-486). From M3 `std` ships precompiled (D-256). [stdlib.md](stdlib.md) describes the content.
