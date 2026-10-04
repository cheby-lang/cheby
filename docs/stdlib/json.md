# `std::json`

JSON text as RFC 8259 defines it: parsing it, writing values as text, and the type `Value` that holds any JSON document (D-210, ADR-0038). Typed data is read with the decoders of [`std::json::decode`](json/decode.md) (D-320), and written with encoding functions that return a `Value`, such as the ones the language server generates (§13.11.1).

```cheby
pub exposed type Value {
  Null,
  Bool(Bool),
  Int(Int),
  Float(Float),
  String(String),
  Array(List<Value>),
  Object(List<(String, Value)>),
}
```

`Value` is `exposed`, because the JSON specification fixes its shapes (D-321, ADR-0052). Values are built with its constructors, such as `json::Int(1965)` and `json::Object([("year", json::Int(1965))])`, and taken apart with `case`. There are no builder functions such as `json::int`. The constructors `Bool`, `Int`, `Float` and `String` hide nothing, because the prelude has types with those names but no constructors (§7.6, D-212).

- Integers and floats stay apart (D-217). `Int` and `Float` are different variants, so `json::Int(1) == json::Float(1.0)` is `False`.
- An `Object` is a list of key and value pairs in the order they were written, not a `Map`, so the same value always prints the same text (D-218). A key may occur more than once (D-223). Two objects with the same members in a different order are not `==` (ADR-0052).
- `Int` holds any `Int`. One outside ±(2⁵³−1) is written as is and reads back as a `Float` (D-321, D-222).

`==`, hashing and debug printing are the built-in structural ones (§3.13).

`Value` does not satisfy `Show`, because a module satisfies `Show` for one type only and `json::show` is `Error`'s (D-045, D-362). A program prints a value with [`to_string`](#to_string) or `{value:?}`.

```cheby
pub exposed type Position { line: Int, column: Int }

pub exposed type Error {
  Syntax(Position),
  Decode(List<decode::Error>),
}
```

`Error` is what `parse` returns. It is `exposed`, because JSON's ways to fail are fixed by its specification, and callers may match on it, as example 011 matches `json::Syntax(_)` (D-304, ADR-0051).

- `Syntax(position)`: the text is not JSON. `position` is where `parse_value` stopped (D-324). `line` and `column` both start at 1. A line ends at `\n`, so `\r\n` also ends one line, and `column` counts code points (D-302).
- `Decode(errors)`: the text is JSON, but the decoder did not accept it. `errors` holds every value that did not fit, in the order the decoder visited them, and is never empty when `parse` returns it (D-322). The fields of `decode::Error` are described in [`std::json::decode`](json/decode.md).

`Error` satisfies `Show` through `show` below. `Position` does not satisfy `Show`, since `json::show` is taken, and `Error`'s text includes it.

```cheby
pub interface ToJson {
  fn to_json(Self) -> Value
}
```

A type satisfies `ToJson` when its module has a `to_json` function with this signature that is at least as visible as the type (§8.2), as the generated code is (§13.11.1). There is no decoding interface, because an interface function cannot mention `Self` only in its return type (D-209, ADR-0037). Decoding uses decoder values instead (§8.8).

## Encoding

### `array`

```cheby
pub fn array<T>(items: List<T>, encode: fn(T) -> Value) -> Value
```

Returns an `Array` holding `encode` applied to each element of `items`, in order (D-321).

- **Cost:** O(n), plus the calls to `encode`. Informative.

```cheby
test "array encodes each element with the given function" {
  assert json::array(["a", "b"], json::String) == json::Array([json::String("a"), json::String("b")])
  assert json::to_string(json::array([1, 2], json::Int)) == "[1,2]"
}
```

### `nullable`

```cheby
pub fn nullable<T>(option: Option<T>, encode: fn(T) -> Value) -> Value
```

Returns `encode` applied to the value of a `Some`, and `Null` for `None` (D-321). Generated code uses it for `Option` fields (§13.11.1).

- **Cost:** O(1), plus the call to `encode`. Informative.

```cheby
test "nullable writes null for None" {
  assert json::nullable(Some(1920), json::Int) == json::Int(1920)
  assert json::nullable(None, json::Int) == json::Null
}
```

### `list`

```cheby
pub fn list<T: ToJson>(items: List<T>) -> Value
```

Returns an `Array` holding the `to_json` of each element of `items`, in order (D-320). It is `array` with each element's own encoding function.

- **Cost:** O(n), plus the calls to `to_json`. Informative.

```cheby
import std::json

type Point { x: Int, y: Int }

fn to_json(point: Point) -> json::Value {
  json::Object([("x", json::Int(point.x)), ("y", json::Int(point.y))])
}

test "list encodes each element with its type's to_json" {
  let points = [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }]
  assert json::to_string(json::list(points)) == r#"[{"x":1,"y":2},{"x":3,"y":4}]"#
}
```

### `to_string`

```cheby
pub fn to_string(value: Value) -> String
```

Returns `value` as compact JSON text, with no whitespace between tokens. The name is not a `show` (D-307): it encodes a JSON value rather than giving a Cheby value's text.

- `Null`, `Bool(True)` and `Bool(False)` are `null`, `true` and `false`.
- An `Int` is its decimal digits, with `-` when negative, as `int::show` writes them, also outside ±(2⁵³−1) (D-321).
- A finite `Float` is written as `float::show` writes it, so `2.0` keeps its `.0` and reads back as a `Float`, and `1.0e21` and `-0.0` are valid JSON numbers (D-325, §3.3.6). `NaN` and the infinities are written as `null`, so encoding never fails (D-325).
- A `String` is written between `"`, with `"` as `\"`, `\` as `\\` and each control character escaped. Every other code point is written unchanged, including `/` and non-ASCII text (D-326).
- An `Array` is `[`, its elements separated by `,`, and `]`. An `Object` is `{`, its members as `"key":value` separated by `,`, and `}`, in order and with every repeated key (D-218, D-223).

`parse_value` reads the text back as an equal `Value`, except for a non-finite `Float`, which comes back as `Null`, and an `Int` outside ±(2⁵³−1), which comes back as a `Float` (D-222). The stack does not grow with the nesting depth of `value` (D-094).

- **Cost:** O(n) in the length of the result, required (D-300).

```cheby
test "to_string writes compact JSON" {
  let value = json::Object([
    ("title", json::String("Dune")),
    ("year", json::Int(1965)),
    ("rating", json::Float(4.0)),
    ("tags", json::Array([json::String("classic"), json::Null])),
    ("read", json::Bool(False)),
  ])
  let text = json::to_string(value)
  assert text == r#"{"title":"Dune","year":1965,"rating":4.0,"tags":["classic",null],"read":false}"#
  assert json::parse_value(text) == Ok(value)
}

test "to_string writes floats as show does, and non-finite ones as null" {
  assert json::to_string(json::Float(1.0e21)) == "1.0e21"
  assert json::to_string(json::Float(-0.0)) == "-0.0"
  assert json::to_string(json::Float(float::nan)) == "null"
  assert json::to_string(json::Float(float::infinity)) == "null"
}

test "to_string escapes only quotes, backslashes and control characters" {
  assert json::to_string(json::String("say \"hi\"\n")) == r#""say \"hi\"\n""#
  assert json::to_string(json::String("a\\b/c")) == r#""a\\b/c""#
  assert json::to_string(json::String("\u{1}é\u{1F600}")) == "\"\\u0001é\u{1F600}\""
}
```

These are the escapes `JSON.stringify` writes, so output matches the JS host (D-398).

### `to_pretty_string`

```cheby
pub fn to_pretty_string(value: Value) -> String
```

Returns `value` as JSON text indented by two spaces, with `": "` after each key (D-327). An empty array or object is `[]` or `{}`. Any other array or object is its opening bracket, then each element or member on a line of its own, indented two spaces more than the line of the bracket and followed by `,` except the last, and then the closing bracket on a line of its own at the bracket's indentation. Scalars are written as `to_string` writes them. There is no final line ending. This is the layout of `JSON.stringify(value, null, 2)`.

- **Cost:** O(n) in the length of the result, required (D-300).

```cheby
test "to_pretty_string indents by two spaces" {
  let value = json::Object([
    ("title", json::String("Dune")),
    ("tags", json::Array([json::String("classic"), json::Int(1)])),
    ("extra", json::Object([])),
    ("none", json::Array([])),
  ])
  let expected = string::join([
    r"{",
    r#"  "title": "Dune","#,
    r#"  "tags": ["#,
    r#"    "classic","#,
    "    1",
    "  ],",
    r#"  "extra": {},"#,
    r#"  "none": []"#,
    r"}",
  ], "\n")
  assert json::to_pretty_string(value) == expected
  assert json::to_pretty_string(json::Int(1)) == "1"
}
```

## Parsing

### `parse`

```cheby
pub fn parse<T>(text: String, decoder: decode::Decoder<T>) -> Result<T, Error>
```

Parses `text` as `parse_value` does, then decodes the value with `decoder` as `decode_value` does, in one step (D-320).

- **Fails:** `Err(Syntax(position))` when `text` is not JSON, as for `parse_value`. The decoder then never runs. `Err(Decode(errors))` when the decoder reports errors, with every one of them in the order it visited the values (D-322).
- **Cost:** O(n) in the length of `text`, required (D-300), plus running `decoder`.

```cheby
test "parse decodes a document in one step" {
  fn point() -> decode::Decoder<(Int, Int)> {
    use x <- decode::field("x", decode::int())
    use y <- decode::field("y", decode::int())
    decode::success((x, y))
  }
  assert json::parse(r#"{"x": 1, "y": 2}"#, point()) == Ok((1, 2))
  let x_problem = decode::Error { expected: "Int", found: "String", path: ["x"] }
  let y_problem = decode::Error { expected: "Int", found: "Null", path: ["y"] }
  assert json::parse(r#"{"x": "1", "y": null}"#, point()) == Err(json::Decode([x_problem, y_problem]))
  assert json::parse(r#"{"x": 1"#, point()) == Err(json::Syntax(json::Position { line: 1, column: 8 }))
}
```

### `parse_value`

```cheby
pub fn parse_value(text: String) -> Result<Value, Error>
```

Parses `text` as one JSON value and returns it without decoding, so that generic tools can walk any document with `case` (D-321). The value may have whitespace (space, tab, `\n` and `\r`) before and after it.

- **Numbers.** A number written without `.` or an exponent is an `Int` when it lies within ±(2⁵³−1), on every target. Every other number is a `Float`, the nearest one to the decimal value, with ties to even (D-217, D-222). `-0` is `Int(0)`.
- **Objects.** Members are kept in the order written, and a repeated key keeps every member (D-218, D-223).
- **Strings.** The escapes are those of RFC 8259. A `\u` escape of a high surrogate followed by one of a low surrogate is one code point. An unpaired surrogate escape, such as `\ud800`, is a syntax error, because a `String` holds only scalar values (D-326, §3.5).
- **Nesting.** Depth is limited only by memory, because the parser keeps its stack on the heap (D-326, D-094).
- Nothing outside RFC 8259 is accepted: no comments, trailing commas, single quotes, `NaN`, leading `+` or zeros, or byte order mark.

- **Fails:** `Err(Syntax(position))` when `text` is not one JSON value. `position` is that of the first code point at which `text` stops being the start of a JSON text, or the position just after the last code point when `text` ends too early, as `""` and `[1,` do (D-324). It never returns `Decode`.
- **Cost:** O(n) in the length of `text`, required (D-300).

```cheby
test "parse_value keeps integers and floats apart" {
  assert json::parse_value("1965") == Ok(json::Int(1965))
  assert json::parse_value("-0") == Ok(json::Int(0))
  assert json::parse_value("2.0") == Ok(json::Float(2.0))
  assert json::parse_value("1e3") == Ok(json::Float(1000.0))
  assert json::parse_value("9007199254740991") == Ok(json::Int(9007199254740991))
  assert json::parse_value("9007199254740992") == Ok(json::Float(9007199254740992.0))
  assert json::Int(1) != json::Float(1.0)
}

test "parse_value keeps object members in order, repeated keys too" {
  let assert Ok(first) = json::parse_value(r#"{"b": 1, "a": [true, null]}"#)
  assert first == json::Object([
    ("b", json::Int(1)),
    ("a", json::Array([json::Bool(True), json::Null])),
  ])
  let assert Ok(reordered) = json::parse_value(r#"{"a": [true, null], "b": 1}"#)
  assert first != reordered
  let twice = json::Object([("a", json::Int(1)), ("a", json::Int(2))])
  assert json::parse_value(r#"{"a": 1, "a": 2}"#) == Ok(twice)
}

test "parse_value reads escapes and rejects unpaired surrogates" {
  assert json::parse_value(r#""tab\t\u00e9 \ud83d\ude00""#) == Ok(json::String("tab\té \u{1F600}"))
  assert json::parse_value(r#""\ud800""#) == Err(json::Syntax(json::Position { line: 1, column: 8 }))
}

test "a syntax error gives the line and the column in code points" {
  let lf = string::join([r#"{"a": 1,"#, r#" "b" 2}"#], "\n")
  let crlf = string::join([r#"{"a": 1,"#, r#" "b" 2}"#], "\r\n")
  let colon_missing = json::Syntax(json::Position { line: 2, column: 6 })
  assert json::parse_value(lf) == Err(colon_missing)
  assert json::parse_value(crlf) == Err(colon_missing)
  assert json::parse_value(r#"["é", x]"#) == Err(json::Syntax(json::Position { line: 1, column: 7 }))
  assert json::parse_value("[1, 2,]") == Err(json::Syntax(json::Position { line: 1, column: 7 }))
  assert json::parse_value("1 2") == Err(json::Syntax(json::Position { line: 1, column: 3 }))
  assert json::parse_value("") == Err(json::Syntax(json::Position { line: 1, column: 1 }))
}

test "parse_value accepts any nesting depth" {
  let deep = string::repeat("[", 100000) + string::repeat("]", 100000)
  assert result::is_ok(json::parse_value(deep))
}

test "parse_value rounds numbers beyond the range of Float" {
  assert json::parse_value("1e400") == Ok(json::Float(float::infinity))
  assert json::parse_value("-1e400") == Ok(json::Float(float::neg_infinity))
}
```

A number beyond `Float`'s range rounds to an infinity or a zero with its sign, as `float::parse` does (D-394).

`parse_value` returns `json::Error` so that it chains with `parse` under `use` (D-399).

### `decode_value`

```cheby
pub fn decode_value<T>(value: Value, decoder: decode::Decoder<T>) -> Result<T, List<decode::Error>>
```

Runs `decoder` on a value already parsed or built in the program, with paths starting at `value` itself. `parse` is `parse_value` followed by `decode_value`, with the errors wrapped in `Decode`.

- **Fails:** `Err(errors)` with every error the decoder reported, in the order it visited the values (D-322). The list is never empty.
- **Cost:** the cost of running `decoder`. Informative.

```cheby
test "decode_value decodes a value built in the program" {
  fn year() -> decode::Decoder<Int> {
    use found <- decode::field("year", decode::int())
    decode::success(found)
  }
  assert json::decode_value(json::Object([("year", json::Int(1965))]), year()) == Ok(1965)
  let problem = decode::Error { expected: "Int", found: "Float", path: ["year"] }
  assert json::decode_value(json::Object([("year", json::Float(1965.0))]), year()) == Err([problem])
}
```

`decode_value` lives in `std::json` because `std::json::decode` cannot import `std::json` (D-145, D-361, ADR-0053). For the same reason, no decoder can return part of a document as a raw `Value`.

## Errors

### `show`

```cheby
pub fn show(error: Error) -> String
```

Makes `Error` satisfy `Show` (§8.8), so `{reason}` prints a JSON error, as example 011 does. `Syntax` gives `invalid JSON at line L, column C`. `Decode` gives `unexpected JSON: ` followed by the `show` of each `decode::Error`, separated by `; `.

- **Cost:** O(n) in the number of decode errors and the length of their paths. Informative.

```cheby
test "a json error shows as a message" {
  let syntax = json::Syntax(json::Position { line: 2, column: 6 })
  assert json::show(syntax) == "invalid JSON at line 2, column 6"
  let year = decode::Error { expected: "Int", found: "String", path: ["year"] }
  let edition = decode::Error { expected: "Format", found: "String", path: ["books", "1", "format"] }
  let wrong = json::Decode([year, edition])
  assert "{wrong}" == "unexpected JSON: expected Int, found String at year; expected Format, found String at books.1.format"
}
```

The wording reads as one line after a file name, as example 011 prints it (D-400).
