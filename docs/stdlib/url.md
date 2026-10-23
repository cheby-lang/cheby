# `std::url`

Percent-encoding for the parts of a URL that a program builds from its own text: query strings and form bodies, and path segments (D-1069). It has no types. Each encoder writes text that its decoder reads back exactly, and [`std::http`](http.md)'s server decodes requests with the same decoders, so the two cannot drift.

- **Why it exists.** A URL built by interpolation breaks on text that contains the URL's own punctuation: with `term = "a&b=c"`, `"{base}/search?q={term}"` sends two parameters. [`encode_query`](#encode_query) and [`encode_segment`](#encode_segment) escape that punctuation, so `base + "/search?" + url::encode_query([("q", term)])` sends one (D-1069).
- **Two encode sets.** A query is written as `application/x-www-form-urlencoded`, exactly as WHATWG's URL standard serializes a form and as `URLSearchParams` does on JS, so a space becomes `+`. A path segment keeps only RFC 3986's unreserved characters, the strictest set that is safe in a segment, so a space becomes `%20` and a `/` becomes `%2F`. Every other byte of the text's UTF-8 encoding becomes `%XX`, with upper-case hex digits (D-1069).
- **Decoding is strict about text.** The decoders return `Err(Nil)` when the decoded bytes are not valid UTF-8, where WHATWG's decoders insert U+FFFD, since std never repairs text silently (D-311, D-1069).
- **Written in Cheby.** Every function is Cheby over `std::string`, `std::bytes` and `std::list`, with no kernels, so every function runs in constants and gives the same text on every target, JS included (D-232, D-991, D-1069).
- **The name `url`.** A module that imports `std::url` cannot use `url` as the name of a local or a parameter, since a binding must not have the name of an imported module (D-062). The compiler's error suggests a new name. Code that needs `url` as a name imports the module under another one, as `std/http.cheby` writes `import std::url as urls`, because its functions `get` and `send` take a parameter `url` (D-1053).
- **Tier.** `std::url` ships in M3, so it is a tier-3 module, although it needs no host (D-1054).

## Queries

### `encode_query`

```cheby
pub fn encode_query(pairs: List<(String, String)>) -> String
```

Returns `pairs` as an `application/x-www-form-urlencoded` query, as WHATWG's form serializer writes it. In each name and value, ASCII letters, digits, `*`, `-`, `.` and `_` stay as they are, a space becomes `+`, and every other byte of the UTF-8 text becomes `%XX` with upper-case hex digits. Each pair is written `name=value`, also when the value is empty, and the pairs are joined with `&` in order. There is no leading `?` (D-1069).

- **Cost:** O(n) in the total length of the names and values. Informative.

```cheby
test "encode_query escapes what would split a pair" {
  assert url::encode_query([("q", "a&b=c"), ("page", "2")]) == "q=a%26b%3Dc&page=2"
  assert url::encode_query([("q", "two words"), ("lang", "é")]) == "q=two+words&lang=%C3%A9"
}

test "encode_query keeps letters, digits and four marks" {
  assert url::encode_query([("k", "Az09*-._")]) == "k=Az09*-._"
  assert url::encode_query([("k", "~!'()/?+")]) == "k=%7E%21%27%28%29%2F%3F%2B"
}

test "encode_query writes every pair, also an empty one" {
  assert url::encode_query([]) == ""
  assert url::encode_query([("flag", ""), ("", "x")]) == "flag=&=x"
}
```

### `decode_query`

```cheby
pub fn decode_query(text: String) -> Result<List<(String, String)>, Nil>
```

Reads `text` as an `application/x-www-form-urlencoded` query and returns its name and value pairs in order, with repeated names kept. It splits `text` at `&` and skips empty pieces. It splits each piece at its first `=`, and a piece without one has the value `""`. In each name and value, `+` is a space, `%XX` is the byte with that value, with hex digits in either case, and a `%` not followed by two hex digits stays as it is. `text` has no leading `?`.

It is the decoder that [`http::serve`](http.md#serve) uses for `Request.query`, and it also decodes a form body, as `url::decode_query(request.body)` (D-1004, D-1069).

- **Fails:** `Err(Nil)` when the decoded bytes of a name or a value are not valid UTF-8 (D-311).
- **Cost:** O(n) in the length of `text`. Informative.

```cheby
test "decode_query reads what encode_query writes" {
  let pairs = [("q", "a&b=c"), ("note", "50% off"), ("flag", "")]
  assert url::decode_query(url::encode_query(pairs)) == Ok(pairs)
  assert url::decode_query("x=1&&y=two+words&flag") == Ok([
    ("x", "1"),
    ("y", "two words"),
    ("flag", ""),
  ])
  assert url::decode_query("q=%FF") == Err(Nil)
}

test "decode_query keeps a percent sign that starts no escape" {
  assert url::decode_query("a=100%&b=%zz&c=%4") == Ok([("a", "100%"), ("b", "%zz"), ("c", "%4")])
  assert url::decode_query("e=%c3%a9&sum=1%2B1&eq=a=b") == Ok([
    ("e", "é"),
    ("sum", "1+1"),
    ("eq", "a=b"),
  ])
  assert url::decode_query("") == Ok([])
}
```

## Path segments

### `encode_segment`

```cheby
pub fn encode_segment(segment: String) -> String
```

Returns `segment` percent-encoded for use as one segment of a URL's path. ASCII letters, digits, `-`, `.`, `_` and `~`, RFC 3986's unreserved characters, stay as they are, and every other byte of the UTF-8 text becomes `%XX` with upper-case hex digits. So a `/` inside `segment` becomes `%2F`, and the server's `Request.segments` gives it back as one segment (D-1069).

- **Cost:** O(n) in the length of `segment`. Informative.

```cheby
test "encode_segment keeps a slash inside one segment" {
  assert url::encode_segment("a/b c") == "a%2Fb%20c"
  assert url::decode_segment("a%2Fb%20c") == Ok("a/b c")
}

test "encode_segment keeps only unreserved characters" {
  assert url::encode_segment("Az09-._~") == "Az09-._~"
  assert url::encode_segment("a+b?c#d é") == "a%2Bb%3Fc%23d%20%C3%A9"
}
```

### `decode_segment`

```cheby
pub fn decode_segment(segment: String) -> Result<String, Nil>
```

Decodes one percent-encoded segment of a URL's path: `%XX` is the byte with that value, with hex digits in either case, and every other character stays as it is, `+` and a `%` not followed by two hex digits included. It is the decoder that [`http::serve`](http.md#serve) uses for `Request.segments` (D-1004, D-1069).

- **Fails:** `Err(Nil)` when the decoded bytes are not valid UTF-8 (D-311).
- **Cost:** O(n) in the length of `segment`. Informative.

```cheby
test "decode_segment keeps a plus sign" {
  assert url::decode_segment("a+b") == Ok("a+b")
  assert url::decode_segment("100%") == Ok("100%")
  assert url::decode_segment("%FF") == Err(Nil)
}
```

## Not in tier 3

Each of these can be added later without breaking a program:

- parsing a whole URL, `url::parse`, with its scheme, host, port, path, query and fragment: the HTTP client's parser stays private until a caller needs it (D-1008, D-1069);
- building a whole URL from its parts;
- host names outside ASCII, which the HTTP client rejects until it converts them ([OQ-http-1](http.md#get)).
