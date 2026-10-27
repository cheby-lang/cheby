# cheby_codegen_clif

MIR to Cranelift IR. One lowering serves the JIT, the AOT object writer of M3 and the constant evaluator. It runs in two phases over a batch of MIR units, so units can reference each other's functions: `declare` gives every function, indirection slot and string a symbol in declaration order, which fixes the ids that appear in CLIF text (D-264), and `define` translates and compiles each function in parallel on rayon's pool and installs them in declaration order. A batch that fails defines nothing.

## Place in the pipeline

`cheby_codegen_clif` depends on `cheby_mir`, `cheby_iface`, `cheby_diag` and, from `cheby_runtime`, only the `abi` module (D-465), plus `cranelift-codegen`, `cranelift-frontend` and `cranelift-module`. `cheby_jit`, `cheby_link` and `cheby_driver` depend on it.

- **Fed by** `cheby_mir`: the translator binds each MIR local to one CLIF value, CLIF block n is MIR block n, and blocks are translated in reverse postorder (D-643). Signatures come from `cheby_mir::abi`.
- **Feeds** `cheby_jit`, which owns the `JITModule` the declarations go into, and `cheby_link` once AOT lands. `Decls` persists across batches so the REPL, constant evaluation and M3's lazy compilation find the existing slots.
- **The runtime contract** is `cheby_runtime::abi`: object headers, string images, context offsets and symbol names. This crate never calls the runtime. Generated code calls a fixed set of runtime helpers through the C ABI (D-520), listed in `runtime.rs` with their C signatures. Two MIR helpers compile to no call: `list_length` loads the root's length word (D-910), and `list_uncons_head` loads the stack slot whose address the `list_uncons` right before it passed (D-909).
- **Lowering rules (D-453):** every Cheby-ABI function uses Cranelift's `tail` convention with the hidden `ctx` parameter first, tail calls are `return_call` (D-015), every call of a top-level function loads the callee's slot (D-043), entry thunks use the platform C convention and are the only way the host enters generated code, and every function starts with the stack-limit check whose slow path yields, unwinds or reports overflow (D-124, D-775). A function whose frame exceeds 64 KiB is split in two (D-679).
- **Code model (D-913, D-921, ADR-0064):** one for the JIT, the cache and executables. Code is non-PIC for one fixed ISA per target (`isa::host_isa`). Every generated symbol is referenced colocated, so text reaches it PC-relative. Runtime functions, runtime statics and foreign symbols are reached only through import words `cb$imp$<symbol>`, read-only words with one `Abs8` data relocation, declared in one place (`Decls::import_word`). No Cranelift libcall appears. `define` checks every compiled function's relocations: text has only PC-relative ones, to generated symbols.
- **Units compiled alone (D-917, D-918, D-920):** `compile_unit` compiles one MIR unit into a module that holds nothing else, for code records and objects (D-914, D-915). Items another unit may name (`UnitCx::named_outside`, which the caller answers from the front end), their slots, layout words, `cb$ctor$` statics and constant slots are global hidden; everything else is local. Other units' slots, layout words, `cb$ctor$` statics, constant slots and directly called code are imports. Code and statics fixed by their key (value shims, tuple functions, `dyn` forwarders, `@blocking` shims, dictionaries, descriptors, function values) are local copies in each unit, and layout words hold 0 where the layout id goes, so a unit's bytes depend on no other unit. A slot whose signature borrows a parameter is named `cb$slot$<path>$<item>$$b<hex mask>`, in the JIT too; the `$$` keeps it apart from the plain slot of an item `b<hex>` in the module `<path>::<item>`.
- **Unwinding** is return-flag propagation (D-733, ADR-0061): a frame that unwinds runs its pad and returns 0, and callers test the result, or the flag at `ctx+12` after a C call.

## Layout

| Module        | What it holds                                                                                  |
| ------------- | ---------------------------------------------------------------------------------------------- |
| `decls.rs`    | `Decls`, `FuncDecl`, `SlotDecl`, `SlotKey`: declarations that persist across batches           |
| `unit.rs`     | `compile_unit`, `UnitCx`, `UnitCode`: one unit compiled alone (D-917)                          |
| `jit_unit.rs` | One unit compiled alone into a record for a JIT, with its index and seed (D-914)               |
| `func.rs`     | `translate`: one MIR function to CLIF                                                          |
| `sig.rs`      | Machine types and Cranelift signatures                                                         |
| `values.rs`   | Objects and uniform words: boxing, allocation, field access, tagging                           |
| `arith.rs`    | Checked integer arithmetic, comparisons and float operations                                   |
| `data.rs`     | Data objects: static strings, slots, layout words, static dictionaries and descriptors (D-523) |
| `layout.rs`   | Runtime layout ids of symbolic MIR layouts                                                     |
| `mangle.rs`   | Symbol names, `cb$<kind>$<module path>$<item>…` (D-456)                                        |
| `session.rs`  | Symbols of REPL session items and their generations (D-547)                                    |
| `runtime.rs`  | The runtime functions and data generated code imports, with their C signatures                 |
| `blocking.rs` | `@blocking` foreign calls through the blocking pool and their C-ABI shims (D-818)              |
| `unwind/`     | The return flag: pads, flag tests after calls, entry pads                                      |
| `isa.rs`      | Target flags and the fixed ISA for the JIT, the cache and executables (D-259, D-913, D-921)    |
| `error.rs`    | `CodegenError`                                                                                 |

`tests.rs`, `locs_tests.rs` and `scale_tests.rs` compile small functions in a throwaway `cranelift-jit` module, which is why that crate is a dev-dependency. `code_model_tests.rs` checks the code model's relocations in a recording module, and the ISA's fixed flags. `alone_tests.rs` checks units compiled alone: linkage, imports, identical copies, no layout ids and slot masks.

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §2.6 and §2.7, code generation and value representation.
- [ADR-0006](../../docs/adr/0006-guaranteed-general-tail-calls.md), [ADR-0055](../../docs/adr/0055-native-calling-convention.md) and [ADR-0061](../../docs/adr/0061-return-flag-unwinding.md).
- [`crates/README.md`](../README.md) for the whole crate graph.
