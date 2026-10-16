# cheby_link

AOT object output and linker invocation. In M3 it will write the Cranelift IR that `cheby_codegen_clif` produces into object files, link them with the runtime's staticlib and the `cheby.h` host library (D-187), and give `cheby build --target native` its executable.

## Place in the pipeline

`cheby_link` depends on `cheby_codegen_clif`, whose single lowering serves the JIT, AOT and constant evaluation (architecture §2.6). AOT objects may keep typed function imports, since a program gives each native symbol one signature, where the JIT imports symbols as data (D-475). Only `cheby_driver` depends on it. The native runtime it links is `cheby_runtime`, built as a staticlib for this purpose.

## Status

Planned for M3. The crate is a skeleton: one `lib.rs` with a crate-level doc comment and no items. It exists so the workspace has its final shape from the start (D-284), and so the dependency edges above are fixed before the code lands. Its first real contents are described in [`docs/plan/m3-aot-and-cache.md`](../../docs/plan/m3-aot-and-cache.md).

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §1, the repository layout.
- [`crates/README.md`](../README.md) for the whole crate graph.
