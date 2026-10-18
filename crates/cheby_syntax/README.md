# cheby_syntax

The lexer, the event-based parser, the flat lossless concrete syntax tree (CST) and the typed AST views over it (D-272, D-280). One `parse` call turns a file into a `SyntaxTree` plus its grammar errors. The tree always covers the whole text, whatever the errors, because the language server will parse incomplete code. `validate` then reports the spec rules that are not grammar errors.

## Place in the pipeline

`cheby_syntax` depends only on `cheby_diag`. It is the first phase of every build and the only crate that reads source text.

- **Used by** `cheby_resolve` and `cheby_types`, which read the tree through the `ast` views and never through raw nodes. `cheby_driver` calls `parse` and `validate` in phase A, `cheby_fmt` and `cheby_lsp` will reuse the same lossless tree in M5, and `xtask cross-check` and the fuzz targets parse with it.
- **Grammar errors (class G)** are the inputs that both the spec and `tree-sitter-cheby` reject. The parser cross-check (D-272, `cargo xtask cross-check`) compares only those, so this crate and the editor grammar cannot drift apart silently. Stricter rules are **validation errors (class V)**, tested by `check-fail` UI tests.
- The lexer emits a line-break token outside strings and comments, and the parser decides continuation (D-231). Interpolated strings are lexed into segments the parser can check.

## Layout

| Module           | What it holds                                                                                                     |
| ---------------- | ----------------------------------------------------------------------------------------------------------------- |
| `lexer/`         | The hand-written scanner, numeric and string literal shapes, and the lexical class-V checks                       |
| `syntax_kind.rs` | The kinds of tokens and nodes                                                                                     |
| `token_set.rs`   | Bit sets of token kinds for lookahead and recovery                                                                |
| `event.rs`       | Parser events, the input of the tree builder                                                                      |
| `parser/`        | The recursive-descent parser: `grammar/` has one function per rule of Appendix A, `recovery.rs` the recovery sets |
| `tree/`          | The arena-allocated tree, its builder, borrowed cursors and the indented dump used by snapshot tests              |
| `ast/`           | Typed views of items, statements, expressions, patterns, types and tokens                                         |
| `validate/`      | The class-V checks, grouped as the grammar is                                                                     |
| `names.rs`       | Spelling rules for names: case conventions and reserved words                                                     |

## Tests

- `tests/corpus.rs` parses every example and UI test and compares the result with the test's expectation.
- `tests/lossless.rs` holds the property tests: for any input, lexing and parsing round-trip the text.
- `benches/parse.rs` lexes and parses the generated projects of `bench/gen` for `cargo xtask bench-compile`.

## Read more

- [`docs/spec/02-lexical-structure.md`](../../docs/spec/02-lexical-structure.md) and [Appendix A](../../docs/spec/appendix-a-grammar.md), the grammar this crate implements.
- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §2.1.
- [`crates/README.md`](../README.md) for the whole crate graph.
