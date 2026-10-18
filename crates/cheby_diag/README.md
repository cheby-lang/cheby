# cheby_diag

Spans, the source map, diagnostics and their rendering (spec §13.8). Every other crate reports its errors and warnings through this one, and the `cheby` binary renders them in the layout of D-418 that the UI-test goldens pin.

## Place in the pipeline

`cheby_diag` is the bottom of the dependency graph. It depends on no other `cheby_*` crate, and every crate except `cheby_runtime` depends on it, from `cheby_syntax` up to `cheby_cli`. `xtask` and the fuzz targets use it too, to print what they parse.

- **Spans** are byte offsets in a file (D-421). Phases pass them around and never compute lines or columns until output.
- **Codes** are registered here once (D-419), and the explanation of each code in `docs/diagnostics/` is embedded in this crate for `cheby explain` (D-420). A new code needs a file there and a UI test that pins it (D-476).
- **Rendering** produces the plain text of D-418, and a colored variant for terminals whose escapes `strip_colors` removes again. Color only adds escapes, the text is the same.

## Layout

| Module          | What it holds                                                                                    |
| --------------- | ------------------------------------------------------------------------------------------------ |
| `text.rs`       | `TextSize`, `TextRange`, `FileId` and `Span`                                                     |
| `source.rs`     | `SourceMap` and `SourceFile`: every file of a build with its display path, text and line index   |
| `diagnostic.rs` | `Diagnostic`: a `Code`, a message, primary and secondary labels, notes and help lines with edits |
| `codes.rs`      | The registry of diagnostic codes and their embedded explanations                                 |
| `render.rs`     | `render`, `render_all` and their colored variants, with the layout tests in `render/tests.rs`    |

## Read more

- [`docs/spec/13-tooling.md`](../../docs/spec/13-tooling.md) §13.8, diagnostics and warnings.
- [`docs/diagnostics/`](../../docs/diagnostics/), one explanation per code.
- [`crates/README.md`](../README.md) for the whole crate graph.
