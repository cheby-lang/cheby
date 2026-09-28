# 6. Patterns

Patterns take values apart and bind names to their pieces. They are used in `case` arms ([§5.9](05-expressions.md#59-case-expressions)), `let` ([§5.2](05-expressions.md#52-let-bindings)), `let assert` ([§11.4](11-errors-and-panics.md#114-let-assert)) and `use` binders ([§5.10](05-expressions.md#510-use)).

## 6.1 Pattern syntax

```ebnf
pattern       = alt_pattern [ "as" LOWER ] ;
alt_pattern   = primary_pat { "|" primary_pat } ;
primary_pat   = "_"
              | LOWER                                   (* binding *)
              | literal_pat
              | ctor_pattern
              | tuple_pattern
              | list_pattern
              | "(" pattern ")" ;
literal_pat   = [ "-" ] INT | STRING ;
ctor_pattern  = ctor_path
              | ctor_path "(" pattern { "," pattern } [ "," ] ")"
              | ctor_path "{" [ field_pat { "," field_pat } ] [ "," ".." | ".." ] [ "," ] "}" ;
field_pat     = LOWER [ ":" pattern ] ;
tuple_pattern = "(" pattern "," pattern { "," pattern } [ "," ] ")" ;
list_pattern  = "[" [ list_pat_item { "," list_pat_item } [ "," ] ] "]" ;
list_pat_item = pattern | ".." [ LOWER ] ;
```

The pattern forms are exactly those listed in D-113. In particular, there are **no** float literal patterns, range patterns or string-prefix patterns in v1 (D-113).

A pattern is **irrefutable** if it matches every value of its type. The irrefutable patterns are:

- `_` and bindings,
- constructor patterns for a type with exactly one variant, when all sub-patterns are irrefutable,
- tuple patterns whose sub-patterns are all irrefutable,
- `[..]` and `[..rest]`,
- alternatives with at least one irrefutable alternative, and `p as x` when `p` is irrefutable.

Every other pattern is **refutable**. `let` and `use` binders require irrefutable patterns. `case` arms and `let assert` accept both.

## 6.2 Wildcards and bindings

- `_` matches any value and binds nothing.
- A `LOWER` name matches any value and binds it. The name follows the no-shadowing rule ([§5.2.3](05-expressions.md#523-no-shadowing)) (D-062). A name must not be bound twice in the same pattern, except across alternatives ([§6.7](#67-alternatives)).

Because `UPPER` names are always constructors and `LOWER` names are always bindings, there is never any doubt about which one a pattern means (D-059).

## 6.3 Literal patterns

An integer literal pattern (optionally negative, [§5.3.1](05-expressions.md#531-negative-literals)) matches an integer of the subject's type that is equal to it. A string literal pattern matches an equal string. String literal patterns must not contain interpolation. Raw strings are allowed.

`True` and `False` are constructor patterns, not literal patterns ([§3.4](03-types.md#34-bool)).

Float literal patterns are not allowed, because float equality is reflexive for NaN and treats `-0.0` and `0.0` as equal, so a literal pattern would be misleading (D-069, D-113). Use a guard such as `when x == 0.0`.

```cheby
fn http_reason(code: Int) -> String {
  case code {
    200 => "OK"
    404 => "Not Found"
    -1 => "unknown"
    _ => "other"
  }
}
```

## 6.4 Constructor patterns

A constructor pattern matches values built with that constructor:

- A **unit** constructor pattern is the name alone: `None`, `Red`, `True`.
- A **positional** constructor pattern has one sub-pattern per field, in order: `Some(x)`, `Node(left, value, right)`.
- A **named-field** constructor pattern lists fields by name, in any order:
  - `field: pattern` matches the field against the pattern,
  - `field` alone is punning and binds the field to a variable of the same name (D-113),
  - `..` at the end ignores all fields not listed. Without `..`, every field must be listed.

```cheby
case shape {
  Circle { radius } => radius * radius * 3.14159
  Rect { width: w, height: h } => w * h
  Dot => 0.0
}

case event {
  Click { x, .. } when x < 0 => "off-screen"
  Click { .. } => "click"
  Key { code: 27, .. } => "escape"
  Key { .. } => "key"
}
```

The constructor may be qualified with a module: `shape::Circle { radius }`.

Matching on the constructors of a `priv` type is possible only in its own module (D-151). Matching on the constructors of a type from another package requires the type to be `exposed` ([§4.3.4](04-declarations.md#434-opacity-and-exposed)) (D-048). Values of opaque types from other packages can only be matched with `_`, bindings, and `as`.

## 6.5 Tuple patterns

`(p1, p2, …)` matches a tuple with the same number of elements, matching each element against the corresponding pattern (D-076, D-113).

```cheby
let (name, age) = person
```

## 6.6 List patterns

A list pattern matches a `List` (D-053):

- `[]` matches the empty list.
- `[p1, p2]` matches a list of exactly two elements.
- A **spread** item `..` matches zero or more elements, and `..name` binds them to `name` as a `List`.

A list pattern has at most one spread, in any position, with any number of fixed elements before and after it: `[a, b, ..rest]`, `[..init, last]`, `[first, ..middle, last]` (D-158). A pattern with two or more spreads is a compile error. This generalizes D-053, which names `[first, ..rest]` and `[.., last]`.

A pattern with `n` fixed items and no spread matches lists of length exactly `n`. With a spread, it matches lists of length at least `n`.

The list bound by a spread shares structure with the subject. Binding it must not copy the elements, so recursing over a list with `[first, ..rest]` does work proportional to the list's length overall (D-053, ADR-0018).

```cheby
fn last_of(xs: List<Int>) -> Option<Int> {
  case xs {
    [] => None
    [.., last] => Some(last)
  }
}
```

## 6.7 Alternatives

`p1 | p2 | …` matches if any alternative matches, trying them from left to right (D-113). Every alternative must bind exactly the same set of names, each with the same type in every alternative.

```cheby
case day {
  Saturday | Sunday => "weekend"
  _ => "weekday"
}

case pair {
  (0, n) | (n, 0) => n
  (a, b) => a * b
}
```

Inside a pattern, `|` always means alternatives. It is never the bitwise-or operator.

## 6.8 As bindings

`pattern as name` matches `pattern` and additionally binds the whole matched value to `name` (D-113). `as` has the lowest precedence in patterns, so `A | B as x` binds `x` whichever alternative matched.

```cheby
case items {
  [_, ..] as non_empty => process(non_empty)
  [] => Nil
}
```

## 6.9 Exhaustiveness and reachability

The compiler checks every `case` with subjects for exhaustiveness and reachability (D-022):

- **Exhaustive:** for every combination of subject values, at least one arm's patterns match. Guards are ignored for this purpose, so an arm with a guard never counts towards covering its patterns. A non-exhaustive `case` is a compile error that lists example values that are not covered.
- **Reachable:** an arm whose patterns are fully covered by earlier arms without guards can never match. It is a warning (D-080).

For the purposes of exhaustiveness:

- A type's variants are exactly those in its declaration.
- An opaque type from another package ([§6.4](#64-constructor-patterns)) can only be covered by `_` or a binding.
- `Int`, the sized integer types and `String` have infinitely many values in practice, so a `case` that matches literals of these types needs a catch-all arm.
- A list is covered when every length is covered, for example by `[]` and `[_, ..]`.

`let assert` uses a refutable pattern and panics when it does not match instead of requiring exhaustiveness ([§11.4](11-errors-and-panics.md#114-let-assert)).
