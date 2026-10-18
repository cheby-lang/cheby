# M3: AOT output, build cache and cached code

Goal: native executables from `cheby build`, the global build cache with interface artifacts, and the cached-code loader that lets `cheby run` meet its latency budget (D-099, step 3; D-247, D-254).

Prerequisites: M2 done. Tier-3 stdlib (native parts) specified. OQ-P9 (linking foreign code) settled.

Detail level: workstreams with outcomes. This file is refined into tasks when M3 starts.

## Workstreams

### 3.1 Object output and linking

- Cranelift object output per module, from the same lowering as the JIT.
- `cheby_runtime` as a staticlib linked into every executable, with mimalloc as the executable's global allocator, as in `cheby` (D-890).
- Linking with the system linker, `cc` or `link.exe` (D-088, D-261).
- DWARF line tables by default, full debug info opt-in (D-184, D-259), and panic backtraces from them.
- Foreign code supplied by packages, per OQ-P9, and the `cheby.h` header for it (D-187).
- Drop functions of user external types, run from the finalizer slot of M2's handle kinds, and `@blocking` foreign calls from packages, run on M2's blocking pool (D-101, D-647, D-683, D-703).
- Executables unwind Rust frames with the platform unwinder. Cheby frames unwind by return-flag propagation, which keeps unwinding in the generated code itself, so the object writer and the cached-code loader carry nothing extra for them (D-733, ADR-0061).
- `@external(native, …)` symbols resolved at link time instead of in the process.
- An executable's entry that bypasses Rust's startup code reopens standard streams that are closed at start on the null device, so they behave as D-474 requires.

### 3.2 Interface artifacts and the global cache

- `ModuleInterface` (M1, B8) written to disk as the interface artifact (D-249, ADR-0044).
- One content-addressed cache per machine, keyed by input hashes, compiler version and flags (D-255).
- Early cutoff: a dependent is recompiled only when the hash of an interface it uses changes. Hidden dependencies of D-260 are recorded.
- Constant values linked in as data, only their types in the interface (D-257). A serialized image of the evaluated constants replaces M1's results made static in place (D-539), and a constant's seed may use the interface hash instead of the source hash (D-538).
- Cache safety under concurrent `cheby` processes, since every command is its own process (D-268).

### 3.3 Cached code for `cheby run`

- Machine code cached per module, shared with AOT (ADR-0047).
- A loader of our own that maps cached code and relocates it against the indirection table, since Cranelift's JIT cannot reload code (D-254).
- Changed modules are compiled lazily, one function on its first call, through the indirection table (D-043).

### 3.4 Precompiled `std`

- The toolchain build produces `std`'s interface artifacts and native code for the host platform, and from M6 for every supported platform (D-256).

### 3.5 Tier-3 stdlib on native

- `std::file`, `std::net`, `std::http`, `std::time`, `std::process` ([stdlib.md](stdlib.md#tier-3-before-m3-and-m4)).
- Sockets register on M2's `mio` poller, through the seam M2 leaves for them, as `std::net` needs. They moved here from M2's workstream 2.6 (D-680, D-681).

### 3.6 Differential fuzzing

- The programs that M2's generator produces are also compared with expected values computed by a reference evaluator for the generator's subset, which [testing.md](testing.md#8-fuzzing) asked for from M2 (D-719).

## Exit criteria

- Every example that passes on the JIT also passes as an executable from `cheby build --target native`.
- 020 and 031 pass, against the harness's local fixtures. 031 imports `std::http`, so it is checked from this milestone (D-477). 030 needs its git dependency too, so it moves to M5 (D-446).
- After a one-function edit in the 1M-line `bench/gen` project, `cheby run` reaches `main` within the D-247 latency budget, as revised by OQ-P11, on the CI reference machine, and OQ-P11 is settled.
- Checking with code generation is measured again against D-247's 100,000 lines per second per core, on the `bench/gen` shapes of M1's measurement by CI's Linux runner. Like every budget it stays reported, not enforced, until M6 (D-644).
- Two builds of the same inputs produce bit-identical artifacts and executables (D-264).
- Editing a function body recompiles only its module, checked by a cache-hit test.
