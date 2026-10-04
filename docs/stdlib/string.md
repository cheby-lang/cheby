# `std::string`

Functions on the built-in `String`, immutable Unicode text made of Unicode scalar values (§3.5, D-038). `String` is in the prelude (D-112), and this module is where the type is declared (§8.2):

```cheby
pub type String
```

String literals, `+` and interpolation are part of the language (§2.5, §5.3.2, D-067). This module is how text is measured, searched, split and changed, since a string has no indexing (§3.5).

Every length, position and count in this module is in code points, and grapheme clusters appear only in `to_graphemes` and `grapheme_length` (README "Text", D-302, ADR-0050). The Unicode tables that `lowercase`, `uppercase`, `trim` and the grapheme functions need come from the one Unicode version the toolchain fixes (D-310).

`String` satisfies `Add` through `add`, `Compare` through `compare` and `Show` through `show` (§8.7, §8.8).

A conversion between text and bytes lives in the module of its target type, as numeric conversions do (D-308): `bytes::from_string` encodes a string as UTF-8, and `string::from_bytes` decodes UTF-8. There is no `string::to_bytes`.

Every cost in this module is **required** (D-300). `n` is the length of the first argument in code points, unless the entry says otherwise.

## Measuring

### `length`

```cheby
pub fn length(text: String) -> Int
```

Returns the number of code points in `text` (D-302).

- **Cost:** O(n), required. Neither UTF-8 on native nor UTF-16 on JS can count code points without a scan (ADR-0050).

```cheby
test "length counts code points" {
  assert string::length("") == 0
  assert string::length("café") == 4
  assert string::length("e\u{301}") == 2
  assert string::length("🇳🇱") == 2
}
```

### `is_empty`

```cheby
pub fn is_empty(text: String) -> Bool
```

Tells whether `text` is `""`.

- **Cost:** O(1), required.

```cheby
test "is_empty is true only for the empty string" {
  assert string::is_empty("")
  assert !string::is_empty(" ")
}
```

### `grapheme_length`

```cheby
pub fn grapheme_length(text: String) -> Int
```

Returns the number of extended grapheme clusters in `text`, what a reader usually sees as one character, as Unicode's text segmentation rules define them (D-302). The result can change when the toolchain updates its Unicode version (D-310).

- **Cost:** O(n), required.

```cheby
test "grapheme_length counts what a reader sees" {
  assert string::grapheme_length("café") == 4
  assert string::grapheme_length("e\u{301}") == 1
  assert string::grapheme_length("🇳🇱") == 1
}
```

## Comparing and searching

### `compare`

```cheby
pub fn compare(a: String, b: String) -> Order
```

Orders strings by their code points, one after another, on every target (D-136). A string that is a prefix of another comes first. This makes `String` satisfy `Compare`, so `<` and `list::sort` work on strings (§8.7). It is not a language-aware order, so `"Z"` comes before `"a"` and `"é"` after `"z"`.

- **Cost:** O(n), where n is the length of the shorter string, required.

```cheby
test "strings compare by code point" {
  assert string::compare("apple", "banana") == Less
  assert string::compare("app", "apple") == Less
  assert string::compare("Z", "a") == Less
  assert string::compare("é", "z") == Greater
  assert list::sort(["b", "c", "a"]) == ["a", "b", "c"]
}
```

### `starts_with`

```cheby
pub fn starts_with(text: String, prefix: String) -> Bool
```

Tells whether `text` begins with `prefix`. Every string starts with `""`.

- **Cost:** O(m), where m is the length of `prefix`, required.

```cheby
test "starts_with checks the beginning" {
  assert string::starts_with("https://example.com", "https://")
  assert !string::starts_with("http", "https")
  assert string::starts_with("text", "")
}
```

### `ends_with`

```cheby
pub fn ends_with(text: String, suffix: String) -> Bool
```

Tells whether `text` ends with `suffix`. Every string ends with `""`.

- **Cost:** O(m), where m is the length of `suffix`, required.

```cheby
test "ends_with checks the end" {
  assert string::ends_with("report.csv", ".csv")
  assert !string::ends_with("report.csv", ".json")
  assert string::ends_with("text", "")
}
```

### `contains`

```cheby
pub fn contains(text: String, part: String) -> Bool
```

Tells whether `part` occurs anywhere in `text`. Every string contains `""`.

- **Cost:** O(n + m), where m is the length of `part`, required.

```cheby
test "contains finds a part anywhere" {
  assert string::contains("hello world", "o w")
  assert !string::contains("hello", "world")
  assert string::contains("", "")
}
```

## Splitting

### `split`

```cheby
pub fn split(text: String, separator: String) -> List<String>
```

Splits `text` at every occurrence of `separator`, from the start, and returns the parts between them in order, without the separators. Two separators next to each other, or one at either end, give an empty part, so the result always has one more element than there are separators, and `split("", ",")` is `[""]`. An empty `separator` gives the code points of `text`, the same as `to_code_points`, so `split("", "")` is `[]` (D-315).

- **Cost:** O(n + m), where m is the length of `separator`, required.

```cheby
test "split keeps empty parts" {
  assert string::split("a,b,,c", ",") == ["a", "b", "", "c"]
  assert string::split("a,b,", ",") == ["a", "b", ""]
  assert string::split("", ",") == [""]
  assert string::split("abc", "-") == ["abc"]
}

test "split on an empty separator gives code points" {
  assert string::split("abc", "") == ["a", "b", "c"]
  assert string::split("", "") == []
}
```

### `split_once`

```cheby
pub fn split_once(text: String, separator: String) -> Option<(String, String)>
```

Splits `text` at the first occurrence of `separator`, and returns the parts before and after it. An empty `separator` splits after the first code point, as `split` does (D-315).

- **Fails:** `None` when `separator` does not occur in `text`, and when both are `""`.
- **Cost:** O(n + m), where m is the length of `separator`, required.

```cheby
test "split_once splits at the first separator" {
  assert string::split_once("key=value=1", "=") == Some(("key", "value=1"))
  assert string::split_once("key", "=") == None
  assert string::split_once("abc", "") == Some(("a", "bc"))
}
```

There is no `index_of`. A code-point position is useful only with `slice`, which scans the text again, so `split_once` cuts at a marker in one scan (D-371).

### `to_code_points`

```cheby
pub fn to_code_points(text: String) -> List<String>
```

Splits `text` into its code points, each as a one-code-point string, in order (D-302). `string::join(string::to_code_points(s), "")` is `s`.

- **Cost:** O(n), required.

```cheby
test "to_code_points splits into scalar values" {
  assert string::to_code_points("abc") == ["a", "b", "c"]
  assert string::to_code_points("e\u{301}") == ["e", "\u{301}"]
  assert string::to_code_points("") == []
}
```

### `to_graphemes`

```cheby
pub fn to_graphemes(text: String) -> List<String>
```

Splits `text` into its extended grapheme clusters, in order, as `grapheme_length` counts them (D-302, D-310). `string::join(string::to_graphemes(s), "")` is `s`.

- **Cost:** O(n), required.

```cheby
test "to_graphemes keeps clusters together" {
  assert string::to_graphemes("e\u{301}a") == ["e\u{301}", "a"]
  assert string::to_graphemes("🇳🇱!") == ["🇳🇱", "!"]
  assert string::to_graphemes("") == []
}
```

## Building

### `add`

```cheby
pub fn add(a: String, b: String) -> String
```

Returns `a` followed by `b`. This makes `String` satisfy `Add`, so `a + b` concatenates (§5.4.3, D-067).

- **Cost:** O(n + m), where m is the length of `b`, required.

```cheby
test "add concatenates" {
  let first = "Ada"
  assert string::add("con", "cat") == "concat"
  assert first + " " + "Lovelace" == "Ada Lovelace"
}
```

### `join`

```cheby
pub fn join(parts: List<String>, separator: String) -> String
```

Returns the elements of `parts` in order, with `separator` between each two. An empty list gives `""`, and a list of one element gives that element.

- **Cost:** O(n), where n is the length of the result, required.

```cheby
test "join puts the separator between parts" {
  assert string::join(["a", "b", "c"], ", ") == "a, b, c"
  assert string::join(["only"], ", ") == "only"
  assert string::join([], ", ") == ""
  assert ["x", "y"] |> string::join(_, "") == "xy"
}
```

### `repeat`

```cheby
pub fn repeat(text: String, count: Int) -> String
```

Returns `count` copies of `text`, one after another.

- **Clamps:** `count` below 0 is 0, which gives `""` (D-298).
- **Cost:** O(n × count), required.

```cheby
test "repeat copies the text" {
  let negative = -2
  assert string::repeat("ab", 3) == "ababab"
  assert string::repeat("ab", 0) == ""
  assert string::repeat("ab", negative) == ""
}
```

### `pad_start`

```cheby
pub fn pad_start(text: String, width: Int, pad: String) -> String
```

Returns `text` with copies of `pad` in front of it, so that the result is `width` code points long (D-302). The copies are written from the left, and the last one is cut to fit. `text` is returned unchanged when it is already `width` or more code points long, or when `pad` is `""`.

- **Clamps:** `width` below the length of `text`, including a negative `width`, is the length of `text`, so `text` is returned unchanged (D-298).
- **Cost:** O(n + width), required.

```cheby
test "pad_start pads on the left" {
  let negative = -3
  assert string::pad_start("7", 3, "0") == "007"
  assert string::pad_start("121", 6, "ab") == "aba121"
  assert string::pad_start("", 3, "-") == "---"
  assert string::pad_start("é", 3, " ") == "  é"
  assert string::pad_start("long", 2, " ") == "long"
  assert string::pad_start("text", negative, " ") == "text"
}
```

### `pad_end`

```cheby
pub fn pad_end(text: String, width: Int, pad: String) -> String
```

Returns `text` with copies of `pad` after it, so that the result is `width` code points long (D-302). The copies are written from the left, and the last one is cut to fit. `text` is returned unchanged when it is already `width` or more code points long, or when `pad` is `""`.

- **Clamps:** `width` below the length of `text`, including a negative `width`, is the length of `text`, so `text` is returned unchanged (D-298).
- **Cost:** O(n + width), required.

```cheby
test "pad_end pads on the right" {
  assert string::pad_end("200", 8, " ") == "200     "
  assert string::pad_end("121", 6, "ab") == "121aba"
  assert string::pad_end("long", 2, " ") == "long"
}
```

## Changing

### `trim`

```cheby
pub fn trim(text: String) -> String
```

Removes whitespace from both ends of `text`. Whitespace is every code point with Unicode's `White_Space` property, such as space, tab, `\n`, `\r` and U+3000 IDEOGRAPHIC SPACE (D-310).

- **Cost:** O(n), required.

```cheby
test "trim removes whitespace at both ends" {
  assert string::trim("  hi there \n") == "hi there"
  assert string::trim("\t\r\n") == ""
  assert string::trim("\u{3000}x") == "x"
}
```

### `trim_start`

```cheby
pub fn trim_start(text: String) -> String
```

Removes whitespace from the start of `text`, with the same definition of whitespace as `trim` (D-373).

- **Cost:** O(n), required.

```cheby
test "trim_start removes whitespace at the start" {
  assert string::trim_start("  hi ") == "hi "
  assert string::trim_start("\t\nx") == "x"
}
```

### `trim_end`

```cheby
pub fn trim_end(text: String) -> String
```

Removes whitespace from the end of `text`, with the same definition of whitespace as `trim` (D-373).

- **Cost:** O(n), required.

```cheby
test "trim_end removes whitespace at the end" {
  assert string::trim_end(" hi  ") == " hi"
  assert string::trim_end("line\r\n") == "line"
}
```

### `remove_prefix`

```cheby
pub fn remove_prefix(text: String, prefix: String) -> String
```

Removes one occurrence of `prefix` from the start of `text`, and returns `text` unchanged when it does not start with `prefix` (D-373).

- **Cost:** O(m), where m is the length of `prefix`, required.

```cheby
test "remove_prefix removes one occurrence" {
  assert string::remove_prefix("https://example.com", "https://") == "example.com"
  assert string::remove_prefix("0042", "0") == "042"
  assert string::remove_prefix("text", "x") == "text"
}
```

### `remove_suffix`

```cheby
pub fn remove_suffix(text: String, suffix: String) -> String
```

Removes one occurrence of `suffix` from the end of `text`, and returns `text` unchanged when it does not end with `suffix` (D-373).

- **Cost:** O(m), where m is the length of `suffix`, required.

```cheby
test "remove_suffix removes one occurrence" {
  assert string::remove_suffix("https://example.com/", "/") == "https://example.com"
  assert string::remove_suffix("a//", "/") == "a/"
  assert string::remove_suffix("text", "") == "text"
}
```

### `replace`

```cheby
pub fn replace(text: String, pattern: String, replacement: String) -> String
```

Replaces every occurrence of `pattern` in `text` with `replacement`, searching from the start, so occurrences do not overlap. It is the same as `string::join(string::split(text, pattern), replacement)`, so an empty `pattern` puts `replacement` between every two code points (D-315).

- **Cost:** O(n + r), where r is the length of the result, required.

```cheby
test "replace changes every occurrence" {
  assert string::replace("a-b-c", "-", "+") == "a+b+c"
  assert string::replace("aaa", "aa", "b") == "ba"
  assert string::replace("abc", "x", "y") == "abc"
  assert string::replace("abc", "", "-") == "a-b-c"
}
```

An empty `pattern` puts `replacement` between code points, because `replace` is `join(split(text, pattern), replacement)` (D-315, D-374).

### `lowercase`

```cheby
pub fn lowercase(text: String) -> String
```

Converts `text` to lower case with Unicode's full case mapping, the same in every locale (D-310). The result can be longer or shorter than `text`.

- **Cost:** O(n), required.

```cheby
test "lowercase applies Unicode case mapping" {
  assert string::lowercase("Hello, World") == "hello, world"
  assert string::lowercase("ÉCOLE") == "école"
  assert string::lowercase("I") == "i"
}
```

### `uppercase`

```cheby
pub fn uppercase(text: String) -> String
```

Converts `text` to upper case with Unicode's full case mapping, the same in every locale, including mappings that change the length, such as `ß` to `SS` (D-310).

- **Cost:** O(n), required.

```cheby
test "uppercase applies Unicode case mapping" {
  assert string::uppercase("a1b2") == "A1B2"
  assert string::uppercase("straße") == "STRASSE"
  assert string::uppercase("i") == "I"
}
```

### `reverse`

```cheby
pub fn reverse(text: String) -> String
```

Returns the code points of `text` in reverse order (D-302). A combining mark moves to the other side of the character it belonged to, so text that is not ASCII may not read as reversed. `string::join(list::reverse(string::to_graphemes(s)), "")` reverses what a reader sees.

- **Cost:** O(n), required.

```cheby
test "reverse reverses code points" {
  assert string::reverse("stressed") == "desserts"
  assert string::reverse("e\u{301}x") == "x\u{301}e"
  assert string::reverse("") == ""
}
```

### `slice`

```cheby
pub fn slice(text: String, start: Int, end: Int) -> String
```

Returns the code points of `text` from position `start` up to but not including position `end`, counting from 0 (D-302, D-319).

- **Clamps:** `start` and `end` below 0 are 0, and above the length of `text` are its length. When `end` is not greater than `start` after clamping, the result is `""` (D-298).
- **Cost:** O(end), required, because positions are found by a scan (§3.5).

```cheby
test "slice takes code points by position" {
  let negative = -5
  assert string::slice("héllo", 1, 3) == "él"
  assert string::slice("abc", 1, 10) == "bc"
  assert string::slice("abc", negative, 1) == "a"
  assert string::slice("abc", 2, 1) == ""
}
```

`start` and `end` are clamped parameters, so a negative literal for either warns (D-303, D-375).

## Conversions

### `from_bytes`

```cheby
pub fn from_bytes(data: bytes::Bytes) -> Result<String, Nil>
```

Decodes `data` as UTF-8 (§3.5). A byte-order mark at the start is kept as U+FEFF (D-366). It is the inverse of `bytes::from_string`.

- **Fails:** `Err(Nil)` when `data` is not valid UTF-8, including an encoded surrogate, an overlong encoding and a sequence cut off at the end (D-297).
- **Cost:** O(n), where n is the length of `data` in bytes, required.

```cheby
test "from_bytes decodes UTF-8" {
  assert string::from_bytes(bytes::from_list([0x63, 0x61, 0x66, 0xC3, 0xA9])) == Ok("café")
  assert string::from_bytes(bytes::from_string("round trip")) == Ok("round trip")
  assert string::from_bytes(bytes::new()) == Ok("")
}

test "from_bytes rejects invalid UTF-8" {
  assert string::from_bytes(bytes::from_list([0xFF])) == Err(Nil)
  assert string::from_bytes(bytes::from_list([0xC3])) == Err(Nil)
  assert string::from_bytes(bytes::from_list([0xED, 0xA0, 0x80])) == Err(Nil)
}
```

### `to_code_point_values`

```cheby
pub fn to_code_point_values(text: String) -> List<Int>
```

Returns the Unicode scalar value of each code point of `text`, in order, such as 97 for `a`.

- **Cost:** O(n), required.

```cheby
test "to_code_point_values gives scalar values" {
  assert string::to_code_point_values("aé") == [97, 233]
  assert string::to_code_point_values("") == []
}
```

### `from_code_point_values`

```cheby
pub fn from_code_point_values(values: List<Int>) -> Result<String, Nil>
```

Returns the string made of the code points with the scalar values in `values`, in order. It is the inverse of `to_code_point_values`.

- **Fails:** `Err(Nil)` when an element is not a Unicode scalar value: below 0, above `0x10FFFF`, or a surrogate from `0xD800` to `0xDFFF` (§3.5).
- **Cost:** O(n), where n is the length of `values`, required.

```cheby
test "from_code_point_values builds text" {
  assert string::from_code_point_values([104, 105]) == Ok("hi")
  assert string::from_code_point_values([0x1F600]) == Ok("\u{1F600}")
  assert string::from_code_point_values([0xD800]) == Err(Nil)
  assert string::from_code_point_values([0x110000]) == Err(Nil)
}
```

There is no `from_code_points`, because joining one-code-point strings is `string::join(parts, "")` (D-376).

### `show`

```cheby
pub fn show(text: String) -> String
```

Returns `text` unchanged. This makes `String` satisfy `Show`, so `"{name}"` inserts the text without quotes (§8.8). Debug printing with `{name:?}` writes it in quotes with escapes instead (§3.13).

- **Cost:** O(1), required.

```cheby
test "a string shows as itself" {
  let name = "Ada \"the first\""
  assert string::show("plain") == "plain"
  assert "{name}" == "Ada \"the first\""
}
```
