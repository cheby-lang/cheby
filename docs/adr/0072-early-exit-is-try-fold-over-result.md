---
status: accepted
date: 2026-10-23
log: D-1080
---

# Early exit is `try_fold` over `Result`

Every standard-library module that has `fold` also has `try_fold`. It takes `fold`'s arguments in `fold`'s order, and its callback returns a `Result`. It calls the callback in the module's order, stops at the first `Err` and returns it, and otherwise returns `Ok` of the last accumulator. A search stops by returning its answer as `Err`, and `find`, `any`, `all` and `find_map` hide that for the common cases.

```cheby
fn total(texts: List<String>) -> Result<Int, Nil> {
  use sum, text <- list::try_fold(texts, 0)
  use n <- result::try(int::parse(text))
  Ok(sum + n)
}
```

Cheby has no loops (D-002), so stopping early must come from the library. It also has no interfaces over type constructors, so no single iteration type can serve every collection. Each module therefore offers the same shape on its own. `Result` already is a two-way control type, and inside the callback `use` chains failing steps with `result::try`, so the shape needs no new type. Code that is generic over collections takes a walk function, such as `fn(initial, step) { set::try_fold(s, initial, step) }`, which works as a push iterator, as Go's `iter.Seq` does.

## Considered options

- `fold_until` with a `Continue` and `Stop` type, as Gleam has it. Search code reads better, but it is a second protocol, it stops with the accumulator's type, so a search for another type still needs `Result`, and failing callbacks need `Result` anyway.
- Both `try_fold` and `fold_until`. Each case reads best, but twelve modules carry two protocols, and every caller must choose.
- A lazy `std::seq`. Pipelines compose, but without interfaces over type constructors every collection needs its own adapters, each element costs a closure, against Perceus reuse (D-894), and Gleam moved its iterator out of its standard library.
- A `ControlFlow` type in a module of its own. It is `Result` under another name, which every collection would import.

## Consequences

- An answer that stops a search travels as `Err(answer)`, which reads oddly in hand-written searches. The short forms `find`, `any`, `all` and `find_map` cover the common ones.
- After 1.0 every collection module carries this shape (D-075). A lazy sequence type, if one ever comes, would be an addition beside it.
- A module that gains `fold` later, such as `std::channel` (D-1100), gains `try_fold` with it.
- The hashed modules still build their entry list before the first call (D-1082). A kernel that walks without allocating can come later without changing the API.
