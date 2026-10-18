# cheby_repl

The interactive session of `cheby repl` (spec §13.5). `run` starts a `Session` in the current directory and reads entries until the end of input or `:quit`. Inside a package the package's modules can be imported. The prompt is `> `, and `. ` while a bracket, a block or a string is open, so the lines read so far form one entry.

## Place in the pipeline

`cheby_repl` depends on `cheby_driver`, `cheby_runtime`, `cheby_diag` and `rustyline`. Only `cheby_cli` depends on it.

- **`cheby_driver::session`** does the compiler's work: each entry is checked as a module `<repl:N>` of the local package, compiled as a batch into the session's one JIT, and redefinitions repoint or allocate slots (D-456, D-607). This crate owns the loop, the prompt, the commands and the transcript format.
- **`cheby_runtime`** provides the session's one `Runtime`, on which entries run as root fibers and the detached fibers they start keep running between entries (D-194, D-712). On a terminal, `rustyline` edits the lines with an in-memory history. Otherwise the lines come from the runtime's one buffered reader of standard input, the same one `io::read_line` uses, so an entry that reads standard input consumes the lines that follow it (D-548, D-575). Every line read as input is echoed after its prompt, so the output reads as a transcript, which is what the REPL harness compares.
- Standard output is flushed before anything is written to standard error (D-548), and a termination while no entry runs ends the REPL before the next prompt with its status (D-709, D-828). At the end of the session the detached fibers are torn down and the runtime stops.
- Commands start with `:`: `:type expr`, `:module path`, `:reload`, `:help` and `:quit`. An unknown command is a tool error (D-464).

## Layout

The crate is one `lib.rs`: `run`, `Session`, the open-bracket test `is_open`, the command table `HELP` and the unit tests.

## Tests

The transcripts under [`tests/repl/`](../../tests/repl/README.md) are run by the `repl` test target of `cheby_cli` (`cargo test -p cheby_cli --test repl`), and `crates/cheby_cli/tests/repl_runtime.rs` covers the runtime behaviour of a session end to end.

## Read more

- [`docs/spec/13-tooling.md`](../../docs/spec/13-tooling.md) §13.5.
- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §3.8 and §4, the REPL's runtime and its module model.
- [`crates/README.md`](../README.md) for the whole crate graph.
