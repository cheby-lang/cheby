# Diagnostic codes

Every compiler diagnostic has a code: `E` and four digits for an error, `W` and four digits for a warning (D-419). This directory holds one Markdown file per code, `E0001.md`, `W0001.md` and so on, and `cheby explain <code>` prints the file's text (D-420). The rendered format of a diagnostic is described in [§13.8](../spec/13-tooling.md#138-diagnostics-and-warnings).

## Rules

- Codes are assigned in order, starting at `E0001` and `W0001`, in the same change that adds the check.
- A code is never reused or renumbered. When a check is removed, its file stays and says that the code is no longer emitted.
- The files embedded in the `cheby` binary and the codes the compiler can emit must be the same set, and a test checks this.

## File format

Each file starts with a heading that names the code and the diagnostic, then explains when it is reported and why, with a short example that triggers it and the fixed version. Examples are valid Cheby in `cheby fmt` form, and each cites the decisions behind the rule as `(D-NNN)`.

The compiler's registry of codes is `crates/cheby_diag/src/codes.rs`. A new code is a line there plus its file here, in the same change, and a unit test checks that the two sets match.
