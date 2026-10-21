# The Cheby Language Specification

Status: **draft**, pre-1.0. Breaking changes are allowed until 1.0 (D-075).

This specification describes the Cheby programming language: its syntax, static semantics, dynamic semantics, runtime model, targets and standard tooling. It is derived from the decision log in [`../decisions/LOG.md`](../decisions/LOG.md) and the ADRs in [`../adr/`](../adr/). Where a rule comes from a logged decision, the decision is cited as `(D-NNN)`.

## Chapters

1. [Overview](01-overview.md)
2. [Lexical structure](02-lexical-structure.md)
3. [Types](03-types.md)
4. [Declarations](04-declarations.md)
5. [Expressions](05-expressions.md)
6. [Patterns](06-patterns.md)
7. [Modules and packages](07-modules-and-packages.md)
8. [Interfaces](08-interfaces.md)
9. [Memory model](09-memory-model.md)
10. [Concurrency](10-concurrency.md)
11. [Errors and panics](11-errors-and-panics.md)
12. [Targets and FFI](12-targets-and-ffi.md)
13. [Tooling](13-tooling.md)

Appendix: [A. Grammar](appendix-a-grammar.md)

The standard library is specified separately, one file per module, in [`../stdlib/`](../stdlib/) (D-204, D-220). This specification fixes the language and the semantics the runtime must provide. Standard-library names used in its examples are those of the standard-library spec where it specifies them, as it does for `fiber::scope`, `channel::new`, `file::with_create` and `http::get` (D-646, D-992, D-1009). Names of modules it has not specified yet are provisional until it does (D-174).

## Conventions

### Normative language

- **must** / **must not**: a requirement on programs or implementations. A program that breaks a "must" rule is rejected at compile time unless the rule says the violation is a runtime panic.
- **may**: permitted but not required.
- **should**: a recommendation, usually for implementations or tooling.
- Text marked _Note_ or _Example_ is informative, not normative.
- A comment in an example that marks a line as an error describes the rule informally and names no diagnostic code. The codes and the rules they report are listed in [`../diagnostics/`](../diagnostics/), and each such example gets a UI test once its check exists (D-420, D-476).

### Grammar notation

Grammar snippets use EBNF:

| Notation                | Meaning                                                 |
| ----------------------- | ------------------------------------------------------- |
| `a = b ;`               | rule definition                                         |
| `"fn"`                  | terminal (keyword or punctuation)                       |
| `UPPER`, `LOWER`, `INT` | token classes from [chapter 2](02-lexical-structure.md) |
| `NL`                    | a statement-separating newline token (D-107, D-231)     |
| `a b`                   | sequence                                                |
| `a \| b`                | alternative                                             |
| `[ a ]`                 | optional                                                |
| `{ a }`                 | zero or more repetitions                                |
| `( a )`                 | grouping                                                |
| `(* … *)`               | comment                                                 |

The complete grammar is in [Appendix A](appendix-a-grammar.md). Chapter snippets are excerpts of it; if they disagree, the appendix wins and the disagreement is a spec bug.

### Open questions

Where the decision log does not settle something that the spec needs, the spec marks the gap and gives a proposed answer:

> **Open (OQ-05-2):** Short description of the undecided point. _Proposed:_ the answer this draft assumes.

The identifier is `OQ-<chapter>-<n>`. Text that depends on an open question is written as if the proposal were accepted, but it is **provisional** until the question is settled in the decision log. When a question is settled, the marker is replaced by normative text that cites the new decision. If the decision differs from the proposal, the dependent text is rewritten.

All gaps marked in the first draft were settled by D-126 to D-197, and later gaps are marked as they are found. The first draft's last open question, OQ-13-7, was settled by D-239. The M2 design left two open, settled at H1's plan: OQ-10-11, the locations in a deadlock report, by D-856, and OQ-13-8, deadlocks while tests run in parallel, by D-857. The M3 design left one open, OQ-11-4, the details of the panic backtrace, settled by D-1038 to D-1042.

### Examples

Examples are written in Cheby with the formatting `cheby fmt` is expected to produce: two-space indentation, no semicolons, trailing commas in multi-line lists.

```cheby
import std::io
import std::list

fn main() {
  [1, 2, 3]
  |> list::map(_, fn(x) { x * 2 })
  |> list::fold(_, 0, add)
  |> show_total
}

fn add(acc: Int, x: Int) -> Int {
  acc + x
}

fn show_total(total: Int) {
  io::println("total: {total}")
}
```
