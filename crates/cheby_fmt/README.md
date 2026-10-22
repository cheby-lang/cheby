# cheby_fmt

The formatter behind `cheby fmt` (spec §13.7). It prints a file in the one canonical style: a pretty printer driven by one line width, with no options (D-079, D-1030). Comments and single blank lines are kept, runs of blank lines collapse to one, imports are sorted within their block, files use LF and end with one newline, and string literals are never split.

## Place in the pipeline

`cheby_fmt` depends on `cheby_syntax` and `cheby_diag`. It formats from the lossless concrete syntax tree that the compiler also parses (D-272, D-280), so comments and the exact text survive, and it never needs name resolution or types. A source with a grammar error is never formatted: `format_source` returns its errors. `cheby_cli` runs it for `cheby fmt` (D-1031), and `cheby_lsp` will.

The formatter moves only whitespace, line breaks and blank lines. It never adds, removes or reorders a token, with two exceptions that are the style's own: imports are sorted within their block, and a list the printer lays out gets a trailing comma when it breaks over lines and loses it when it fits on one. A one-element tuple keeps its comma as written, since `(x,)` is a tuple and `(x)` is not. A line break it adds or removes is always one the parser ignores (D-231): inside `(`, `[` or a type's `<`, after `{` and before `}`, after `,`, a binary operator, `=`, `=>` or `<-`, and before a leading `|>`, `&&` or `||`.

## Status

The formatter lands in parts beside M3 (D-1029). The first part laid out the file's lines, the imports and the item headers: `fn` signatures, type, record and enum declarations, constants, interfaces, `test` headers, attributes and visibility. The second part lays out what lies below them: expressions, statements, patterns and types, with blocks, `case` arms, pipelines and argument lists broken at the width. The rules it chose are in the module comment of `src/expr.rs`. A node with a comment or a blank line where its layout has no place for one is still printed verbatim: its source text with its own line structure, re-indented, never joined or split. So is a whole item with a comment or a line break where the header layout has no place for one. The third part lays out every remaining node and comment position. The width is `PROVISIONAL_WIDTH`, 100 columns, until the user chooses it at M3's second checkpoint (D-1030).

## Layout

| Path              | What it holds                                                                                                                           |
| ----------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `src/lib.rs`      | `format_source`, `format_file`, `FormatError`, `PROVISIONAL_WIDTH` and `coverage`                                                       |
| `src/doc.rs`      | The document algebra (text, line, soft line, hard line, nesting, groups, flat parts, choices, verbatim fragments) and its printer       |
| `src/source.rs`   | Verbatim fragments of the source, and the line breaks that belong to string literals                                                    |
| `src/layout.rs`   | From the tree to a document: the file's lines, import blocks, item headers, comma-separated lists, and the dispatch of every other node |
| `src/expr.rs`     | Expressions, statements, blocks, `case`, patterns and types, and the rules that break them                                              |
| `tests/corpus.rs` | Every example and std source, and every UI test source that parses, at 80, 100 and 120 columns                                          |

## Tests

- Unit tests `fmt_doc_*` pin the printer, `fmt_trivia_*` comments, blank lines, line endings and strings, and `fmt_items_*` and `fmt_imports_*` the layout of headers and imports. `fmt_expr_*`, `fmt_stmt_*`, `fmt_pattern_*` and `fmt_type_*` (`src/expr_tests.rs`) pin each layout rule of expressions, statements, patterns and types at a narrow and a wide width.
- `tests/corpus.rs` checks that every source formats without error, that a second pass changes nothing, and that formatting keeps the tokens without whitespace, line breaks and comments, the comments' texts in order, and the node kinds (D-1031). It only reads the repository. `fmt_corpus_coverage`, run with `--nocapture`, prints how many nodes of each kind the printer lays out and how many it prints verbatim.
- `crates/cheby_cli/tests/fmt.rs` tests the command, and formats a copy of every example and checks that its output is unchanged.

## Read more

- [`docs/spec/13-tooling.md`](../../docs/spec/13-tooling.md) §13.7, the formatter.
- [`docs/plan/m5-tooling.md`](../../docs/plan/m5-tooling.md) and [`docs/plan/m3-aot-and-cache.md`](../../docs/plan/m3-aot-and-cache.md), where its parts are planned.
- [`crates/README.md`](../README.md) for the whole crate graph.
