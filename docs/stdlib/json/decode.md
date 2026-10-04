# `std::json::decode`

Decoders, which turn a JSON value into typed data (D-320). A decoder is built from the primitive decoders below and combined with `use` through `field`, `optional_field` and `then`, as in Gleam's `gleam/dynamic/decode`. `json::parse(text, decoder)` parses and decodes in one step, and `json::decode_value(value, decoder)` decodes a value already parsed (D-361). Decoders exist because an interface function cannot return `Self` without taking it, so there is no `FromJson` interface (D-209, ADR-0037).

```cheby
pub type Decoder<T>

pub exposed type Error { expected: String, found: String, path: List<String> }
```

`Decoder<T>` is opaque. It decodes a `T`.

`Error` describes one value that did not fit. It is `exposed`, because its fields follow from JSON's kinds, which do not change (D-304, ADR-0051).

- `expected` says what the decoder wanted. The primitive decoders give a kind name, a missing field gives `"Field"`, and `failure` gives its own text, such as `"Format"` (D-323).
- `found` names the kind of the JSON value that was there: `Null`, `Bool`, `Int`, `Float`, `String`, `Array` or `Object`, as the variants of `json::Value` are named. A missing field gives `"Nothing"` (D-323).
- `path` leads from the decoded value to the one that did not fit. Each object key is one element, and each array index is one element written in decimal, so the third book's format is `["books", "2", "format"]`. The path is `[]` when the value itself did not fit.

## How errors are reported

Decoding reports every value that did not fit, not just the first (D-322). A decoder that fails records its error and still produces a **placeholder** value of its type, and decoding goes on with that value, so later fields are checked too.

- The primitive decoders give `""`, `0`, `0.0` and `False`, `list` and `pairs` give `[]`, and `optional` gives `Some` of its decoder's placeholder.
- `failure(placeholder, expected)` gives its `placeholder`.
- A missing field gives the value its decoder produces when run on `json::Null`, with that run's errors dropped.

Errors are reported in the order the decoder visits the values: a field's errors come before those of the decoder `next` returns, and an array's elements are visited from the first to the last. Fields the decoder does not ask for are ignored (D-323).

```cheby
test "every value that does not fit is reported, in the order visited" {
  fn pair() -> decode::Decoder<(String, Int)> {
    use name <- decode::field("name", decode::string())
    use year <- decode::field("year", decode::int())
    decode::success((name, year))
  }
  let name_problem = decode::Error { expected: "String", found: "Int", path: ["name"] }
  let year_problem = decode::Error { expected: "Field", found: "Nothing", path: ["year"] }
  let text = r#"{"year_of_birth": 1920, "name": 42}"#
  assert json::parse(text, pair()) == Err(json::Decode([name_problem, year_problem]))
}
```

These placeholders need no zero value for each type (D-401).

## Primitive decoders

### `string`

```cheby
pub fn string() -> Decoder<String>
```

Decodes a JSON string.

- **Fails:** `expected: "String"` for any other kind, with the placeholder `""`.
- **Cost:** O(1). Informative.

```cheby
test "string accepts only a JSON string" {
  assert json::parse(r#""Dune""#, decode::string()) == Ok("Dune")
  let problem = decode::Error { expected: "String", found: "Null", path: [] }
  assert json::parse("null", decode::string()) == Err(json::Decode([problem]))
}
```

### `int`

```cheby
pub fn int() -> Decoder<Int>
```

Decodes a JSON number that `json::parse` read as an `Int`: one written without `.` or an exponent and within ±(2⁵³−1) (D-217, D-222). A `Float` with a whole value, such as `1965.0` or `1e3`, is not accepted, so a decoder can tell which kind it found.

- **Fails:** `expected: "Int"` for any other kind, including every `Float`, with the placeholder `0`.
- **Cost:** O(1). Informative.

```cheby
test "int accepts only numbers read as integers" {
  assert json::parse("1965", decode::int()) == Ok(1965)
  let float_problem = decode::Error { expected: "Int", found: "Float", path: [] }
  assert json::parse("1965.0", decode::int()) == Err(json::Decode([float_problem]))
  assert json::parse("1e3", decode::int()) == Err(json::Decode([float_problem]))
  assert json::parse("9007199254740992", decode::int()) == Err(json::Decode([float_problem]))
}
```

### `float`

```cheby
pub fn float() -> Decoder<Float>
```

Decodes a JSON number as a `Float`. A number read as a `Float` is returned as it is. A number read as an `Int` is converted to the nearest `Float`, which is exact within ±(2⁵³−1).

- **Fails:** `expected: "Float"` for any kind other than `Float` and `Int`, with the placeholder `0.0`.
- **Cost:** O(1). Informative.

```cheby
test "float accepts any JSON number" {
  assert json::parse("2.5", decode::float()) == Ok(2.5)
  assert json::parse("2", decode::float()) == Ok(2.0)
  let problem = decode::Error { expected: "Float", found: "String", path: [] }
  assert json::parse(r#""2.5""#, decode::float()) == Err(json::Decode([problem]))
}
```

`float` accepts an `Int`, because `JSON.stringify(1.0)` writes `1` and reading an integer as a float loses nothing, while `int` rejects every `Float` (D-402).

### `bool`

```cheby
pub fn bool() -> Decoder<Bool>
```

Decodes `true` or `false`.

- **Fails:** `expected: "Bool"` for any other kind, with the placeholder `False`.
- **Cost:** O(1). Informative.

```cheby
test "bool accepts only true and false" {
  assert json::parse("true", decode::bool()) == Ok(True)
  let problem = decode::Error { expected: "Bool", found: "Int", path: [] }
  assert json::parse("1", decode::bool()) == Err(json::Decode([problem]))
}
```

## Arrays, objects and null

### `list`

```cheby
pub fn list<T>(decoder: Decoder<T>) -> Decoder<List<T>>
```

Decodes a JSON array, running `decoder` on each element in order. An element's errors have its index added to the front of their paths.

- **Fails:** `expected: "Array"` for any other kind, with the placeholder `[]`. When elements do not fit, every one of their errors is reported, in index order, and the list holds their placeholders.
- **Cost:** O(n) in the number of elements, plus running `decoder` on each. Informative.

```cheby
test "list decodes every element and reports each that does not fit" {
  assert json::parse("[1, 2, 3]", decode::list(decode::int())) == Ok([1, 2, 3])
  let second = decode::Error { expected: "Int", found: "String", path: ["1"] }
  let fourth = decode::Error { expected: "Int", found: "Null", path: ["3"] }
  let text = r#"[1, "two", 3, null]"#
  assert json::parse(text, decode::list(decode::int())) == Err(json::Decode([second, fourth]))
  let not_array = decode::Error { expected: "Array", found: "Object", path: [] }
  assert json::parse(r"{}", decode::list(decode::int())) == Err(json::Decode([not_array]))
}
```

### `optional`

```cheby
pub fn optional<T>(decoder: Decoder<T>) -> Decoder<Option<T>>
```

Decodes `null` as `None`, and any other value as `Some` of what `decoder` gives for it. It adds nothing to the path. It handles only `null`, not a missing key: that is `optional_field`'s part.

- **Fails:** with `decoder`'s errors when the value is not `null` and `decoder` does not accept it. The placeholder is then `Some` of `decoder`'s placeholder.
- **Cost:** O(1), plus running `decoder`. Informative.

```cheby
test "optional turns null into None" {
  assert json::parse("null", decode::optional(decode::int())) == Ok(None)
  assert json::parse("5", decode::optional(decode::int())) == Ok(Some(5))
  let problem = decode::Error { expected: "Int", found: "String", path: [] }
  assert json::parse(r#""5""#, decode::optional(decode::int())) == Err(json::Decode([problem]))
}
```

### `pairs`

```cheby
pub fn pairs<T>(decoder: Decoder<T>) -> Decoder<List<(String, T)>>
```

Decodes a JSON object whose keys are data rather than field names, such as `{"en": "one", "fr": "un"}`. It returns every member in the order written, repeated keys included (D-218, D-223), with `decoder` run on each value. A value's errors have its key added to the front of their paths. `map::from_list` turns the result into a `Map`, where the last occurrence of a key wins, as it does for `field` (D-329).

- **Fails:** `expected: "Object"` for any other kind, with the placeholder `[]`. When values do not fit, every one of their errors is reported, in member order.
- **Cost:** O(n) in the number of members, plus running `decoder` on each. Informative.

```cheby
test "pairs decodes an object with any keys, in order" {
  let text = r#"{"en": "one", "fr": "un", "en": "uno"}"#
  let words = [("en", "one"), ("fr", "un"), ("en", "uno")]
  assert json::parse(text, decode::pairs(decode::string())) == Ok(words)
  let problem = decode::Error { expected: "String", found: "Int", path: ["fr"] }
  let wrong = r#"{"en": "one", "fr": 1}"#
  assert json::parse(wrong, decode::pairs(decode::string())) == Err(json::Decode([problem]))
}
```

Besides the decoders of D-320, the module has `pairs` and `map` (D-403). There is no `one_of`, `at` or `Map`-returning `dict` in tier 1.

### `field`

```cheby
pub fn field<T, U>(name: String, decoder: Decoder<T>, next: fn(T) -> Decoder<U>) -> Decoder<U>
```

Decodes the member `name` of a JSON object with `decoder`, then calls `next` with the result and decodes the same object with the decoder `next` returns. `decoder`'s errors have `name` added to the front of their paths. When `name` occurs more than once, the last occurrence is used (D-223). Members the decoders do not ask for are ignored (D-323). With `use`, the rest of the block is `next` (§5.10).

- **Fails:** `expected: "Field"`, `found: "Nothing"` and `path: [name]` when the object has no member `name`. `decoder`'s errors when it does not accept the member. `expected: "Object"` when the value is not an object. In each case `next` is called with the placeholder (D-401) and its errors are reported after these.
- **Cost:** O(m) in the number of members of the object, plus running `decoder` and the decoder from `next`. Informative.

```cheby
test "field reads a member and passes it on" {
  fn year() -> decode::Decoder<Int> {
    use found <- decode::field("year", decode::int())
    decode::success(found)
  }
  assert json::parse(r#"{"title": "Dune", "year": 1965}"#, year()) == Ok(1965)
  assert json::parse(r#"{"year": 1, "year": 2}"#, year()) == Ok(2)
  let missing = decode::Error { expected: "Field", found: "Nothing", path: ["year"] }
  assert json::parse(r#"{"title": "Dune"}"#, year()) == Err(json::Decode([missing]))
}

test "fields of nested objects add to the path" {
  fn author_name() -> decode::Decoder<String> {
    use name <- decode::field("author", decode::field("name", decode::string(), decode::success))
    decode::success(name)
  }
  assert json::parse(r#"{"author": {"name": "Toni Morrison"}}"#, author_name()) == Ok("Toni Morrison")
  let problem = decode::Error { expected: "String", found: "Int", path: ["author", "name"] }
  assert json::parse(r#"{"author": {"name": 42}}"#, author_name()) == Err(json::Decode([problem]))
}

test "a value that is not an object is reported once" {
  fn point() -> decode::Decoder<(Int, Int)> {
    use x <- decode::field("x", decode::int())
    use y <- decode::field("y", decode::int())
    decode::success((x, y))
  }
  let problem = decode::Error { expected: "Object", found: "Array", path: [] }
  assert json::parse("[1, 2]", point()) == Err(json::Decode([problem]))
}
```

Decoding records an error only when an identical `expected`, `found` and `path` has not been recorded already, so a value that is not an object is reported once however many fields read it (D-404).

### `optional_field`

```cheby
pub fn optional_field<T, U>(name: String, default: T, decoder: Decoder<T>, next: fn(T) -> Decoder<U>) -> Decoder<U>
```

Like `field`, but when the object has no member `name`, it calls `next` with `default` and records no error. When the member is present, `decoder` decodes it, even when its value is `null`. So `optional_field` handles only a missing key, and `null` is `decoder`'s concern: for a field that may be missing or `null`, as generated code writes for an `Option` field, pass `None` as `default` and wrap the decoder in `optional`.

- **Fails:** `decoder`'s errors when the member is present and `decoder` does not accept it, and `expected: "Object"` when the value is not an object. `next` is then called with the placeholder.
- **Cost:** O(m) in the number of members of the object, plus running `decoder` and the decoder from `next`. Informative.

```cheby
test "optional_field handles a missing key, and optional handles null" {
  fn born() -> decode::Decoder<Option<Int>> {
    use year <- decode::optional_field("born", None, decode::optional(decode::int()))
    decode::success(year)
  }
  assert json::parse(r#"{"name": "Homer"}"#, born()) == Ok(None)
  assert json::parse(r#"{"name": "Homer", "born": null}"#, born()) == Ok(None)
  assert json::parse(r#"{"name": "Frank Herbert", "born": 1920}"#, born()) == Ok(Some(1920))
}

test "optional_field passes null to its decoder" {
  fn tags() -> decode::Decoder<List<String>> {
    use found <- decode::optional_field("tags", [], decode::list(decode::string()))
    decode::success(found)
  }
  assert json::parse(r"{}", tags()) == Ok([])
  let problem = decode::Error { expected: "Array", found: "Null", path: ["tags"] }
  assert json::parse(r#"{"tags": null}"#, tags()) == Err(json::Decode([problem]))
}
```

## Combining decoders

### `then`

```cheby
pub fn then<T, U>(decoder: Decoder<T>, next: fn(T) -> Decoder<U>) -> Decoder<U>
```

Decodes the value with `decoder`, calls `next` with the result, and decodes the same value with the decoder `next` returns. It is how a decoder chooses what to do from a value it has read. When `decoder` fails, `next` is still called with the placeholder, so decoding goes on, but only `decoder`'s errors are reported, since the errors of a choice made on a placeholder would be noise.

- **Fails:** with `decoder`'s errors when it fails, and otherwise with the errors of the decoder from `next`.
- **Cost:** O(1), plus running both decoders. Informative.

```cheby
test "then chooses the next decoder from a decoded value" {
  fn shape() -> decode::Decoder<Float> {
    use kind <- decode::then(decode::field("type", decode::string(), decode::success))
    case kind {
      "circle" => decode::field("radius", decode::float(), decode::success)
      "square" => decode::field("side", decode::float(), decode::success)
      _ => decode::failure(0.0, "Shape")
    }
  }
  assert json::parse(r#"{"type": "circle", "radius": 1.5}"#, shape()) == Ok(1.5)
  let unknown = decode::Error { expected: "Shape", found: "Object", path: [] }
  assert json::parse(r#"{"type": "star"}"#, shape()) == Err(json::Decode([unknown]))
  let not_string = decode::Error { expected: "String", found: "Int", path: ["type"] }
  assert json::parse(r#"{"type": 3}"#, shape()) == Err(json::Decode([not_string]))
}
```

`then` reports only `decoder`'s errors when `decoder` fails, because a decoder chosen from a placeholder describes a value that was never there (D-405).

### `map`

```cheby
pub fn map<T, U>(decoder: Decoder<T>, f: fn(T) -> U) -> Decoder<U>
```

Decodes the value with `decoder` and applies `f` to the result. `f` is also applied to the placeholder when `decoder` fails, and the errors are `decoder`'s (D-403).

- **Cost:** O(1), plus running `decoder` and the call to `f`. Informative.

```cheby
test "map changes the decoded value" {
  let length = decode::map(decode::string(), string::length)
  assert json::parse(r#""Dune""#, length) == Ok(4)
  let problem = decode::Error { expected: "String", found: "Bool", path: [] }
  assert json::parse("false", length) == Err(json::Decode([problem]))
}
```

### `success`

```cheby
pub fn success<T>(value: T) -> Decoder<T>
```

A decoder that ignores the value and gives `value`. It ends a chain of `field` calls with the value built from them.

- **Cost:** O(1). Informative.

```cheby
test "success gives its value whatever the input" {
  assert json::parse("null", decode::success(7)) == Ok(7)
  assert json::parse(r#"{"a": [1]}"#, decode::success("done")) == Ok("done")
}
```

### `failure`

```cheby
pub fn failure<T>(placeholder: T, expected: String) -> Decoder<T>
```

A decoder that always fails, recording an error with `expected`, the kind of the value as `found`, and the path of the value. Decoding goes on with `placeholder`, which is never the result of a successful decode (D-322).

- **Fails:** always.
- **Cost:** O(1). Informative.

```cheby
test "failure reports what was expected" {
  fn even() -> decode::Decoder<Int> {
    use n <- decode::then(decode::int())
    case n % 2 {
      0 => decode::success(n)
      _ => decode::failure(0, "EvenInt")
    }
  }
  assert json::parse("[2, 4]", decode::list(even())) == Ok([2, 4])
  let problem = decode::Error { expected: "EvenInt", found: "Int", path: ["1"] }
  assert json::parse("[2, 3]", decode::list(even())) == Err(json::Decode([problem]))
}
```

## Errors

### `show`

```cheby
pub fn show(error: Error) -> String
```

Makes `Error` satisfy `Show` (§8.8). The text is `expected E, found F`, followed by `at` and the path's elements separated by `.` when the path is not empty. `json::show` uses it for each error in a `json::Decode`.

- **Cost:** O(n) in the length of the path. Informative.

```cheby
test "a decode error shows what was expected, found and where" {
  let nested = decode::Error { expected: "Format", found: "String", path: ["books", "1", "format"] }
  assert decode::show(nested) == "expected Format, found String at books.1.format"
  let top = decode::Error { expected: "Array", found: "Null", path: [] }
  assert "{top}" == "expected Array, found Null"
}
```

Programs that need an exact path read the `path` field (D-400).
