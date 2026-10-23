# `std::base64`

Base64 text for `bytes::Bytes`, as RFC 4648 defines it (D-P10). It has no types. Each encoder writes text that its decoder reads back exactly, and a decoder accepts only text that its encoder could write.

- **Why it exists.** HTTP Basic authentication, JWTs, data URLs, binary fields in JSON and keys in configuration files carry bytes as Base64. `Bytes` has no `show`, because a program picks the text it writes bytes as (D-388), and this module and [`std::hex`](hex.md) are the two choices besides UTF-8 (D-P10).
- **Two alphabets.** [`encode`](#encode) and [`decode`](#decode) use the standard alphabet, `A` to `Z`, `a` to `z`, `0` to `9`, `+` and `/`, with `=` padding to a multiple of four characters (RFC 4648 §4). [`url_encode`](#url_encode) and [`url_decode`](#url_decode) use the URL-safe alphabet, with `-` and `_` in place of `+` and `/`, and write no padding (§5), as JWTs do.
- **Decoding is strict.** Both decoders reject a character outside their alphabet, whitespace and line breaks included, a length that no encoding gives, and unused bits of the last digit that are not zero, so a text has at most one decoding: `"aGVsbG8="` is `hello`, and `"aGVsbG9="` is `Err(Nil)`. MIME's lenient decoding, which skips whitespace, is not offered, since both targets would have to agree on a larger rule (D-P10).
- **Written in Cheby.** Every function is Cheby over `std::bytes` and `std::string`, with no kernels, so every function runs in constants and gives the same text on every target, JS included (D-232, D-360, D-P10).
- **The name `base64`.** A user's package or module named `base64`, as example 021 is, binds the same name as `import std::base64`, so a module that imports both renames one with `as`. A module that imports `std::base64` cannot use `base64` as the name of a local or a parameter, since a binding must not have the name of an imported module (D-062).
- **Tier.** `std::base64` ships in M3, so it is a tier-3 module, although it needs no host (D-1054).

Every cost in this module is **required** (D-300).

## Encoding

### `encode`

```cheby
pub fn encode(data: bytes::Bytes) -> String
```

Returns `data` as standard Base64: each group of three bytes becomes four digits of six bits each, and a last group of one or two bytes becomes two or three digits followed by `==` or `=` (RFC 4648 §4). Empty `data` gives `""`.

- **Cost:** O(n), where n is the length of `data`, required.

```cheby
test "base64 pads its output and decodes strictly" {
  assert base64::encode(bytes::from_string("hello")) == "aGVsbG8="
  assert base64::decode("aGVsbG8=") == Ok(bytes::from_string("hello"))
  assert base64::decode("aGVsbG8") == Err(Nil)
  assert base64::decode("aGVsbG9=") == Err(Nil)
  assert base64::encode(bytes::new()) == ""
}
```

`"aGVsbG9="` is rejected because its last digit, `9`, leaves unused bits that are not zero.

### `url_encode`

```cheby
pub fn url_encode(data: bytes::Bytes) -> String
```

Returns `data` as URL-safe Base64, with `-` and `_` for the digits that `encode` writes as `+` and `/`, and with no padding (RFC 4648 §5). The text needs no escaping in a URL's path or query, or in a file name.

- **Cost:** O(n), where n is the length of `data`, required.

```cheby
test "url_encode uses the URL alphabet without padding" {
  let data = bytes::from_list([0xFB, 0xFF])
  assert base64::encode(data) == "+/8="
  assert base64::url_encode(data) == "-_8"
  assert base64::url_decode("-_8") == Ok(data)
  assert base64::url_decode("-_8=") == Ok(data)
}
```

## Decoding

### `decode`

```cheby
pub fn decode(text: String) -> Result<bytes::Bytes, Nil>
```

Reads standard Base64 with its padding, as `encode` writes it, and returns the bytes.

- **Fails:** `Err(Nil)` when `text` holds a character outside the standard alphabet other than its padding, whitespace included; when its length is not a multiple of four; when its padding is missing, longer than two characters, or not at the end; and when the unused bits of its last digit are not zero (D-P10).
- **Cost:** O(n), where n is the length of `text`, required.

```cheby
test "decode rejects what no encoding gives" {
  assert base64::decode("Zm9vYg==") == Ok(bytes::from_string("foob"))
  assert base64::decode("Zm9v YmFy") == Err(Nil)
  assert base64::decode("Zm9vYmFy\n") == Err(Nil)
  assert base64::decode("Zm9vYg") == Err(Nil)
  assert base64::decode("Zm9vY===") == Err(Nil)
  assert base64::decode("====") == Err(Nil)
  assert base64::decode("Zg=a") == Err(Nil)
  assert base64::decode("Zh==") == Err(Nil)
  assert base64::decode("Zm9-") == Err(Nil)
  assert base64::decode("Zm9é") == Err(Nil)
  assert base64::decode("") == Ok(bytes::new())
}
```

### `url_decode`

```cheby
pub fn url_decode(text: String) -> Result<bytes::Bytes, Nil>
```

Reads URL-safe Base64 and returns the bytes. It accepts the text with the padding that `encode` would write or with none, since producers of URL-safe text differ there, but not with part of it.

- **Fails:** `Err(Nil)` when `text` holds a character outside the URL-safe alphabet other than its padding, `+`, `/` and whitespace included; when its length without padding leaves one digit over; when it has padding but its length is not a multiple of four; and when the unused bits of its last digit are not zero (D-P10).
- **Cost:** O(n), where n is the length of `text`, required.

```cheby
test "url_decode accepts the padding or its absence, never half of it" {
  assert base64::url_decode("Zm9vYg") == Ok(bytes::from_string("foob"))
  assert base64::url_decode("Zm9vYg==") == Ok(bytes::from_string("foob"))
  assert base64::url_decode("Zm9vYg=") == Err(Nil)
  assert base64::url_decode("Zm9vY") == Err(Nil)
  assert base64::url_decode("Zm9vYmE") == Ok(bytes::from_string("fooba"))
  assert base64::url_decode("+/8=") == Err(Nil)
  assert base64::url_decode("-_9") == Err(Nil)
}
```

## Not in this module

Each of these can be added later without breaking a program:

- lenient decoding that skips whitespace and line breaks, as MIME writes Base64 (D-P10);
- kernels, if a measurement shows that the Cheby loops are too slow for large inputs (D-P10).
