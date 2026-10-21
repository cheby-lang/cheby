# cheby_codegen_clif

MIR to Cranelift IR. One lowering serves the JIT, the AOT object writer of M3 and the constant evaluator. It runs in two phases over a batch of MIR units, so units can reference each other's functions: `declare` gives every function, indirection slot and string a symbol in declaration order, which fixes the ids that appear in CLIF text (D-264), and `define` translates and compiles each function in parallel on rayon's pool and installs them in declaration order. A batch that fails defines nothing.

## Place in the pipeline

`cheby_codegen_clif` depends on `cheby_mir`, `cheby_iface`, `cheby_diag` and, from `cheby_runtime`, only the `abi` module (D-465), plus `cranelift-codegen`, `cranelift-frontend` and `cranelift-module`. `cheby_jit`, `cheby_link` and `cheby_driver` depend on it.

- **Fed by** `cheby_mir`: the translator binds each MIR local to one CLIF value, CLIF block n is MIR block n, and blocks are translated in reverse postorder (D-643). Signatures come from `cheby_mir::abi`.
- **Feeds** `cheby_jit`, which owns the `JITModule` the declarations go into, and `cheby_link` once AOT lands. `Decls` persists across batches so the REPL, constant evaluation and M3's lazy compilation find the existing slots.
- **The runtime contract** is `cheby_runtime::abi`: object headers, string images, context offsets and symbol names. This crate never calls the runtime. Generated code calls a fixed set of runtime helpers through the C ABI (D-520), listed in `runtime.rs` with their C signatures. Two MIR helpers compile to no call: `list_length` loads the root's length word (D-910), and `list_uncons_head` loads the stack slot whose address the `list_uncons` right before it passed (D-909).
- **Lowering rules (D-453):** every Cheby-ABI function uses Cranelift's `tail` convention with the hidden `ctx` parameter first, tail calls are `return_call` (D-015), every call of a top-level function loads the callee's slot (D-043), entry thunks use the platform C convention and are the only way the host enters generated code, and every function starts with the stack-limit check whose slow path yields, unwinds or reports overflow (D-124, D-775). A function whose frame exceeds 64 KiB is split in two (D-679).
- **Unwinding** is return-flag propagation (D-733, ADR-0061): a frame that unwinds runs its pad and returns 0, and callers test the result, or the flag at `ctx+12` after a C call.

## Layout

| Module        | What it holds                                                                                  |
| ------------- | ---------------------------------------------------------------------------------------------- |
| `decls.rs`    | `Decls`, `FuncDecl`, `SlotDecl`, `SlotKey`: declarations that persist across batches           |
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
| `isa.rs`      | Target flags for the JIT and, from M3, AOT (D-259)                                             |
| `error.rs`    | `CodegenError`                                                                                 |

`tests.rs` and `scale_tests.rs` compile small functions in a throwaway `cranelift-jit` module, which is why that crate is a dev-dependency.

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §2.6 and §2.7, code generation and value representation.
- [ADR-0006](../../docs/adr/0006-guaranteed-general-tail-calls.md), [ADR-0055](../../docs/adr/0055-native-calling-convention.md) and [ADR-0061](../../docs/adr/0061-return-flag-unwinding.md).
- [`crates/README.md`](../README.md) for the whole crate graph.
