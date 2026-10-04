# `std::nil`

The module of the built-in `Nil` type, the unit type with the single value `Nil` (§3.6, D-353). `Nil` is in the prelude as both a type and a value (D-112).

### `show`

```cheby
pub fn show(n: Nil) -> String
```

Returns `"Nil"`. It makes `Nil` satisfy `Show` (§8.8), so that types such as `Result<Nil, String>` do too.

- **Cost:** O(1). Informative.

```cheby
test "Nil shows as Nil" {
  let done: Result<Nil, String> = Ok(Nil)
  assert nil::show(Nil) == "Nil"
  assert "{done}" == "Ok(Nil)"
}
```
