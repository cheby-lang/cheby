# M3: AOT output, build cache and cached code

Goal: native executables from `cheby build`, the global build cache with interface artifacts, and the cached-code loader that lets `cheby run` meet its latency budget (D-099, step 3; D-247, D-254).

Prerequisites: M2 done. Tier-3 stdlib (native parts) specified. OQ-P9 (linking foreign code) settled.

Detail level: workstreams with outcomes. This file is refined into tasks when M3 starts.

## Workstreams

### 3.1 Object output and linking

- Cranelift object output per module, from the same lowering as the JIT.
- `cheby_runtime` as a staticlib linked into every executable.
- Linking with the system linker, `cc` or `link.exe` (D-088, D-261).
- DWARF line tables by default, full debug info opt-in (D-184, D-259), and panic backtraces from them.
- Foreign code supplied by packages, per OQ-P9, and the `cheby.h` header for it (D-187).
- `@external(native, …)` symbols resolved at link time instead of in the process.

### 3.2 Interface artifacts and the global cache

- `ModuleInterface` (M1, B8) written to disk as the interface artifact (D-249, ADR-0044).
- One content-addressed cache per machine, keyed by input hashes, compiler version and flags (D-255).
- Early cutoff: a dependent is recompiled only when the hash of an interface it uses changes. Hidden dependencies of D-260 are recorded.
- Constant values linked in as data, only their types in the interface (D-257).
- Cache safety under concurrent `cheby` processes, since every command is its own process (D-268).

### 3.3 Cached code for `cheby run`

- Machine code cached per module, shared with AOT (ADR-0047).
- A loader of our own that maps cached code and relocates it against the indirection table, since Cranelift's JIT cannot reload code (D-254).
- Changed modules are compiled lazily, one function on its first call, through the indirection table (D-043).

### 3.4 Precompiled `std`

- The toolchain build produces `std`'s interface artifacts and native code for the host platform, and from M6 for every supported platform (D-256).

### 3.5 Tier-3 stdlib on native

- `std::file`, `std::net`, `std::http`, `std::time`, `std::process` ([stdlib.md](stdlib.md#tier-3-before-m3-and-m4)).

## Exit criteria

- Every example that passes on the JIT also passes as an executable from `cheby build --target native`.
- 020, 030 and 031 pass, against the harness's local fixtures.
- After a one-function edit in the 1M-line `bench/gen` project, `cheby run` reaches `main` within the D-247 budget, as revised by OQ-P10, on the CI reference machine.
- Two builds of the same inputs produce bit-identical artifacts and executables (D-264).
- Editing a function body recompiles only its module, checked by a cache-hit test.
