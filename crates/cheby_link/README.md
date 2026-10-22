# cheby_link

AOT object output, and in M3's later waves linker invocation. It replays the code records that `cheby_codegen_clif` captures (D-914) into relocatable objects (D-915); with N4 it will add the program object, link the objects with the runtime archive (`cheby_rt_exe`, D-929) and give `cheby build` its executable.

## Place in the pipeline

`cheby_link` depends on `cheby_codegen_clif`, whose single lowering serves the JIT, AOT and constant evaluation (architecture §2.6), and on `cranelift-object` at the workspace's Cranelift version (D-1036). Only `cheby_driver` depends on it. Its Cranelift crates have their default features only: an architecture feature here would reach `cheby` through `cheby_driver`, and M3 builds for the host only (D-936).

## What it holds

- `object::replay(record, isa, name)`: one code record into one ELF (Linux targets) or Mach-O (macOS) object. Every symbol keeps its recorded name and linkage, local ones included: `Local` is a local symbol, `Hidden` a global hidden one (ELF `STV_HIDDEN`, Mach-O private extern, D-917), `Import` an undefined one. Functions are defined with `define_function_bytes` and data with `define_data`, in record order, so the bytes depend only on the record, the ISA and `name`, the module path (D-264). Unwind info is off (D-913, D-733). Replay checks the record first, and refuses a relocation of another architecture or a format other than ELF and Mach-O (COFF is M6's, D-935), so it never panics on a record.
- `LinkError`: a malformed record, or an object `cranelift-object` refuses.

The records themselves (`cheby_codegen_clif::record`: `RecordingModule`, `CodeRecord` and its encoding, `record_unit` with the module record `cb$mod$<path>`, D-919) live in `cheby_codegen_clif`, since the JIT loads them too (D-914).

## Status

M3, wave 2: replay and its tests (`link_replay_*`: linkage in ELF and Mach-O of the host architecture, PC-relative text, `.note.GNU-stack`, byte-stable output, two units compiled alone that import from each other). Nothing writes a record or an object to disk yet: N4 replays records into objects and links 001, and the cache stores records with K4c. See [`docs/plan/m3-aot-and-cache.md`](../../docs/plan/m3-aot-and-cache.md).

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §1, the repository layout.
- [`crates/README.md`](../README.md) for the whole crate graph.
