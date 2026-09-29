---
status: accepted
date: 2026-09-30
log: D-244, D-245
---

# Explicit pipe holes

Cheby's pipe followed Gleam (D-050, D-221). `x |> f(a)` put `x` in the first argument, and `x |> f(a, _)` put it in the `_`. To see where a value went, a reader had to check whether the step had a hole and, if it had none, remember that the function takes its data first. A `_` nested in an argument, as in `x |> list::map(format_line(_, width))`, looked like a hole but was not one (D-221), and the examples needed comments to explain this.

We make the hole explicit. A pipe step is either a call with a `_` as a whole argument, which `x` fills in any position, or an expression of function type that is called with `x` alone:

```cheby
text
|> string::split(_, " ")      // x fills the first argument, written explicitly
|> list::map(_, string::trim)
|> string::join(_, ", ")
|> string::length             // one-parameter function, written bare
```

A call without a `_` on the right side of `|>` is a compile error that suggests adding one. To pipe into a function that a call returns, parenthesize the call: `x |> (make_handler(config))`. We also reject `f(_)` everywhere (D-245), because it means exactly `f`. This leaves one way to pipe into a one-parameter function and one way to pass it as a value.

## Considered options

- Keep implicit first-argument insertion (Gleam, Elixir): shorter for data-first functions, but it adds a second, invisible rule, and a nested `_` stays misleading.
- Require `_` only for functions with three or more parameters: two-parameter functions would still depend on the hidden rule.
- Treat a call without `_` as an expression that returns a function: this is consistent, but `xs |> list::take(3)` would then fail with a confusing type error instead of a targeted one.
- Reject `f(_)` only after `|>`: two rules for one construct.

## Consequences

- Every multi-argument pipe step shows a `_`, so the argument order of a function never has to be remembered while reading a pipeline.
- Pipelines get slightly longer: `|> list::map(f)` becomes `|> list::map(_, f)`.
- `x |> f(a, g(_))` is an error. It is written `x |> f(_, a, g(_))`, where the first `_` is the pipe's hole and the second is `g`'s own capture (D-202).
- The syntax does not change. The rule is checked after parsing, so the grammar and tree-sitter parser are unaffected.
- Allowing implicit first-argument insertion or `f(_)` again later breaks no program.
