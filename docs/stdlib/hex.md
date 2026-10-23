# `std::hex`

Hex text for `bytes::Bytes`: two digits per byte (D-1089). It has no types. The encoder writes text that the decoder reads back exactly.

- **Why it exists.** Checksums, hashes, keys and binary protocols are written and read as hex. `Bytes` has no `show`, because a program picks the text it writes bytes as (D-388), and this module and [`std::base64`](base64.md) are the two choices besides UTF-8 (D-1089).
- **Lowercase out, either case in.** [`encode`](#encode) writes lowercase digits, as debug printing writes bytes, such as `bytes::from_list([0x66, 0x6f])` (D-388). [`decode`](#decode) accepts digits in either case.
- **Decoding is strict.** `decode` needs an even number of digits and rejects every other character, so a prefix such as `0x`, separators and whitespace are errors (D-1089).
- **Integers are elsewhere.** Hex of an integer is `int::to_base(n, 16)` and the `to_base` of each sized integer module, with the same lowercase digits (D-390). This module writes bytes in order, whatever they mean.
- **Written in Cheby.** Both functions are Cheby over `std::bytes` and `std::string`, with no kernels, so they run in constants and give the same text on every target, JS included (D-232, D-360, D-1089).
- **The name `hex`.** A user's package or module named `hex` binds the same name as `import std::hex`, so a module that imports both renames one with `as`. A module that imports `std::hex` cannot use `hex` as the name of a local or a parameter, since a binding must not have the name of an imported module (D-062).
- **Tier.** `std::hex` ships in M3, so it is a tier-3 module, although it needs no host (D-1054).

Every cost in this module is **required** (D-300).

### `encode`

```cheby
pub fn encode(data: bytes::Bytes) -> String
```

Returns each byte of `data`, in order, as two lowercase hex digits, the high four bits first. Empty `data` gives `""`.

- **Cost:** O(n), where n is the length of `data`, required.

```cheby
test "hex writes lowercase and reads either case" {
  let data = bytes::from_list([0x00, 0xAB, 0x10])
  assert hex::encode(data) == "00ab10"
  assert hex::decode("00AB10") == Ok(data)
  assert hex::decode("0ab") == Err(Nil)
  assert hex::decode("0x00") == Err(Nil)
}
```

### `decode`

```cheby
pub fn decode(text: String) -> Result<bytes::Bytes, Nil>
```

Reads `text` as two hex digits per byte, in either case, and returns the bytes in order.

- **Fails:** `Err(Nil)` when `text` has an odd number of characters, or holds a character other than `0` to `9`, `a` to `f` and `A` to `F`, such as the `x` of a `0x` prefix or whitespace (D-1089).
- **Cost:** O(n), where n is the length of `text`, required.

```cheby
test "decode rejects what is not two hex digits per byte" {
  assert hex::decode("") == Ok(bytes::new())
  assert hex::decode("fF") == Ok(bytes::from_list([0xFF]))
  assert hex::decode("0") == Err(Nil)
  assert hex::decode("0g") == Err(Nil)
  assert hex::decode(" 00") == Err(Nil)
  assert hex::decode("00\n") == Err(Nil)
  assert hex::decode("00 11") == Err(Nil)
  assert hex::decode("é0") == Err(Nil)
}
```
