# `std::ops`

The operator interfaces (§8.7, D-169). A type satisfies one of them when its module declares the function with the matching signature, with no declaration saying so (§8.2). Operators work without importing this module, because an operator is resolved through the operand type's module. Only naming an interface, in a bound or in `dyn`, needs `import std::ops` (D-169).

```cheby
pub interface Add {
  fn add(Self, Self) -> Self
}

pub interface Sub {
  fn sub(Self, Self) -> Self
}

pub interface Mul {
  fn mul(Self, Self) -> Self
}

pub interface Div {
  fn div(Self, Self) -> Self
}

pub interface Neg {
  fn neg(Self) -> Self
}

pub interface Compare {
  fn compare(Self, Self) -> Order
}
```

| Interface | Operators                            | Built-in types that satisfy it                                                                               |
| --------- | ------------------------------------ | ------------------------------------------------------------------------------------------------------------ |
| `Add`     | `a + b`                              | the numeric types, `String` (D-067)                                                                          |
| `Sub`     | `a - b`                              | the numeric types                                                                                            |
| `Mul`     | `a * b`                              | the numeric types                                                                                            |
| `Div`     | `a / b`                              | the numeric types                                                                                            |
| `Neg`     | `-a`                                 | the numeric types                                                                                            |
| `Compare` | `a < b`, `a <= b`, `a > b`, `a >= b` | the numeric types, `String`, `Bool`, `Order`, and `List` and tuples whose elements satisfy it (D-136, D-167) |

`==` and `!=` are built-in structural equality and are not interfaces (D-047). A `compare` that returns `Equal` for values that are not `==` is legal but is a program bug. Sorted collections treat such values as the same key (D-341).

`Show` is in the prelude, not in this module (D-112, §8.8).

_Example:_ a top-level function names `Compare` in a bound, and then uses `>` on its type parameter.

```cheby
import std::list
import std::ops::{Compare}

fn largest<T: Compare>(first: T, rest: List<T>) -> T {
  list::fold(rest, first, fn(best, x) {
    case {
      x > best => x
      _ => best
    }
  })
}

test "a bound names an operator interface" {
  assert largest(3, [7, 2]) == 7
  assert largest("pear", ["apple", "fig"]) == "pear"
}
```
