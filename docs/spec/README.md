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

The standard library is specified separately, one file per module, in [`../stdlib/`](../stdlib/) (D-204, D-220). This specification fixes the language and the semantics the runtime must provide. Standard-library names used in its examples, such as `fiber::scope` or `channel::new`, are provisional until the standard-library spec fixes them (D-174, D-199).

## Conventions

### Normative language

- **must** / **must not**: a requirement on programs or implementations. A program that breaks a "must" rule is rejected at compile time unless the rule says the violation is a runtime panic.
- **may**: permitted but not required.
- **should**: a recommendation, usually for implementations or tooling.
- Text marked _Note_ or _Example_ is informative, not normative.

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

All gaps marked in the first draft were settled by D-126 to D-197, and no open questions remain.

### Examples

Examples are written in Cheby with the formatting `cheby fmt` is expected to produce: two-space indentation, no semicolons, trailing commas in multi-line lists.

```cheby
import std::list
import std::io

fn main() {
  [1, 2, 3]
  |> list::map(fn(x) { x * 2 })
  |> list::fold(0, add)
  |> show_total
}

fn add(acc: Int, x: Int) -> Int {
  acc + x
}

fn show_total(total: Int) {
  io::println("total: {total}")
}
```
