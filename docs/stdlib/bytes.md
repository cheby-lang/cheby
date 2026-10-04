# `std::bytes`

`Bytes`, an immutable sequence of bytes, for binary data such as file contents and network formats (§3.5, D-095). `Bytes` is not in the prelude, so it is written `bytes::Bytes` after `import std::bytes` (§3.2). This module is where the type is declared (§8.2):

```cheby
pub type Bytes
```

`Bytes` is opaque. Each byte is a `U8`, and positions count bytes from 0. Two `Bytes` values are `==` when they hold the same bytes in the same order, and they hash by their contents (§3.13).

There is no separate builder type. `append` and the `push` functions update their first argument in place when nothing else refers to it, so a sequence of them runs in amortized O(1) per byte, and copy it otherwise, as `List` does (D-316, §9.3).

Multi-byte integers are read and written by functions that name their byte order, `be` for big-endian (most significant byte first) and `le` for little-endian (D-334). The functions for one width are documented together in one entry, since they differ only in their type and byte order.

Text becomes bytes with `bytes::from_string`, which always encodes UTF-8 (§3.5). Bytes become text with [`string::from_bytes`](string.md#from_bytes), which can fail, because decoding is a conversion to `String` and lives in `std::string` (D-308). There is no `bytes::to_string`: decoding can fail, and the library has no `to_string` functions, since a type's text comes only from `show` (D-307).

Every cost in this module is **required** (D-300). `n` is the length of the first argument in bytes, unless the entry says otherwise.

## Creating

### `new`

```cheby
pub fn new() -> Bytes
```

Returns an empty `Bytes`.

- **Cost:** O(1), required.

```cheby
test "new is empty" {
  assert bytes::length(bytes::new()) == 0
  assert bytes::new() == bytes::from_list([])
}
```

### `from_list`

```cheby
pub fn from_list(values: List<U8>) -> Bytes
```

Returns the bytes in `values`, in order. Integer literals in the list take the type `U8` from the parameter, so `bytes::from_list([0xFF, 0x00])` needs no conversions (§3.3.4).

- **Cost:** O(n), where n is the length of `values`, required.

```cheby
test "from_list keeps the order" {
  let data = bytes::from_list([0xFF, 0x00, 0x7F])
  assert bytes::to_list(data) == [0xFF, 0x00, 0x7F]
  assert bytes::length(data) == 3
}
```

### `from_string`

```cheby
pub fn from_string(text: String) -> Bytes
```

Returns the UTF-8 encoding of `text`, on every target (§3.5). No byte-order mark is added. `string::from_bytes` turns the result back into `text`.

- **Cost:** O(n), where n is the length of the result, required.

```cheby
test "from_string encodes UTF-8" {
  assert bytes::to_list(bytes::from_string("hi")) == [0x68, 0x69]
  assert bytes::length(bytes::from_string("café")) == 5
  assert string::from_bytes(bytes::from_string("café")) == Ok("café")
  assert bytes::from_string("") == bytes::new()
}
```

## Reading

### `length`

```cheby
pub fn length(data: Bytes) -> Int
```

Returns the number of bytes in `data` (D-301).

- **Cost:** O(1), required.

```cheby
test "length counts bytes" {
  assert bytes::length(bytes::from_list([1, 2, 3])) == 3
  assert bytes::length(bytes::new()) == 0
}
```

### `is_empty`

```cheby
pub fn is_empty(data: Bytes) -> Bool
```

Tells whether `data` holds no bytes.

- **Cost:** O(1), required.

```cheby
test "is_empty is true only without bytes" {
  assert bytes::is_empty(bytes::new())
  assert !bytes::is_empty(bytes::from_list([0]))
}
```

### `get`

```cheby
pub fn get(data: Bytes, index: Int) -> Option<U8>
```

Returns the byte at `index`, counting from 0 (D-334). It is how a single `U8` is read, so there is no `read_u8`.

- **Fails:** `None` when `index` is outside `0` to `length - 1`, including negative indexes (D-318).
- **Cost:** O(1), required.

```cheby
test "get reads one byte" {
  let data = bytes::from_list([10, 20, 30])
  assert bytes::get(data, 1) == Some(20)
  assert bytes::get(data, 3) == None
  assert bytes::get(data, -1) == None
}
```

### `slice`

```cheby
pub fn slice(data: Bytes, start: Int, end: Int) -> Bytes
```

Returns the bytes of `data` from position `start` up to but not including position `end` (D-319, D-334).

- **Clamps:** `start` and `end` below 0 are 0, and above the length of `data` are its length. When `end` is not greater than `start` after clamping, the result is empty (D-298, D-334).
- **Cost:** O(end - start), required.

```cheby
test "slice takes a range of bytes" {
  let data = bytes::from_list([1, 2, 3, 4])
  let negative = -2
  assert bytes::slice(data, 1, 3) == bytes::from_list([2, 3])
  assert bytes::slice(data, 2, 100) == bytes::from_list([3, 4])
  assert bytes::slice(data, negative, 1) == bytes::from_list([1])
  assert bytes::slice(data, 3, 1) == bytes::new()
}
```

### `to_list`

```cheby
pub fn to_list(data: Bytes) -> List<U8>
```

Returns the bytes of `data` as a list, in order.

- **Cost:** O(n), required.

```cheby
test "to_list gives every byte" {
  assert bytes::to_list(bytes::from_list([0x01, 0xFE])) == [0x01, 0xFE]
  assert bytes::to_list(bytes::new()) == []
}
```

### `fold`

```cheby
pub fn fold<A>(data: Bytes, initial: A, f: fn(A, U8) -> A) -> A
```

Calls `f` with the result so far and each byte of `data`, front to back, starting from `initial`, and returns the last result. With no bytes it returns `initial`.

- **Cost:** O(n), plus the calls to `f`, required.

```cheby
test "fold visits bytes front to back" {
  let data = bytes::from_list([1, 2, 3])
  let sum: U32 = bytes::fold(data, 0, fn(total, byte) { total + u32::from_u8(byte) })
  assert sum == 6
  assert bytes::fold(data, [], fn(found, byte) { [byte, ..found] }) == [3, 2, 1]
}
```

## Building

### `push`

```cheby
pub fn push(data: Bytes, byte: U8) -> Bytes
```

Returns `data` with `byte` added at the end. It updates `data` in place when nothing else refers to it (D-316).

- **Cost:** amortized O(1) when `data` is not shared, and O(n) when it is, required.

```cheby
test "push adds one byte" {
  let data = bytes::new() |> bytes::push(_, 0x41) |> bytes::push(_, 0x42)
  assert data == bytes::from_string("AB")
}
```

### `append`

```cheby
pub fn append(data: Bytes, other: Bytes) -> Bytes
```

Returns the bytes of `data` followed by the bytes of `other`. It updates `data` in place when nothing else refers to it (D-316).

- **Cost:** amortized O(m), where m is the length of `other`, when `data` is not shared, and O(n + m) when it is, required.

```cheby
test "append joins two sequences" {
  let head = bytes::from_list([1, 2])
  assert bytes::append(head, bytes::from_list([3])) == bytes::from_list([1, 2, 3])
  assert bytes::append(head, bytes::new()) == head
}
```

### `concat`

```cheby
pub fn concat(parts: List<Bytes>) -> Bytes
```

Returns the bytes of every element of `parts`, in order.

- **Cost:** O(n), where n is the length of the result, required.

```cheby
test "concat joins many sequences" {
  let parts = [bytes::from_string("ab"), bytes::new(), bytes::from_string("c")]
  assert bytes::concat(parts) == bytes::from_string("abc")
  assert bytes::concat([]) == bytes::new()
}
```

## Integers in a byte order

### `push_i8`

```cheby
pub fn push_i8(data: Bytes, value: I8) -> Bytes
```

Returns `data` with `value` added at the end as one byte in two's complement (D-334). `push` adds a `U8`.

- **Cost:** the same as `push`, required.

```cheby
test "push_i8 writes two's complement" {
  assert bytes::push_i8(bytes::new(), -1) == bytes::from_list([0xFF])
  assert bytes::push_i8(bytes::new(), 5) == bytes::from_list([0x05])
}
```

### `read_i8`

```cheby
pub fn read_i8(data: Bytes, offset: Int) -> Option<I8>
```

Reads the byte at `offset` as an `I8` in two's complement (D-334). `get` reads a `U8`.

- **Fails:** `None` when `offset` is outside `0` to `length - 1`, including negative offsets (D-318).
- **Cost:** O(1), required.

```cheby
test "read_i8 reads two's complement" {
  let data = bytes::from_list([0xFF, 0x05])
  assert bytes::read_i8(data, 0) == Some(-1)
  assert bytes::read_i8(data, 1) == Some(5)
  assert bytes::read_i8(data, 2) == None
}
```

### `push_u16_be`, `push_u16_le`, `push_i16_be`, `push_i16_le`

```cheby
pub fn push_u16_be(data: Bytes, value: U16) -> Bytes
pub fn push_u16_le(data: Bytes, value: U16) -> Bytes
pub fn push_i16_be(data: Bytes, value: I16) -> Bytes
pub fn push_i16_le(data: Bytes, value: I16) -> Bytes
```

Return `data` with `value` added at the end as 2 bytes, in big-endian (`be`) or little-endian (`le`) order, and signed values in two's complement (D-334). They update `data` in place when nothing else refers to it (D-316).

- **Cost:** the same as `push` for each byte, required.

```cheby
test "16-bit pushes name their byte order" {
  assert bytes::push_u16_be(bytes::new(), 0x0102) == bytes::from_list([0x01, 0x02])
  assert bytes::push_u16_le(bytes::new(), 0x0102) == bytes::from_list([0x02, 0x01])
  assert bytes::push_i16_be(bytes::new(), -2) == bytes::from_list([0xFF, 0xFE])
  assert bytes::push_i16_le(bytes::new(), -2) == bytes::from_list([0xFE, 0xFF])
}
```

### `read_u16_be`, `read_u16_le`, `read_i16_be`, `read_i16_le`

```cheby
pub fn read_u16_be(data: Bytes, offset: Int) -> Option<U16>
pub fn read_u16_le(data: Bytes, offset: Int) -> Option<U16>
pub fn read_i16_be(data: Bytes, offset: Int) -> Option<I16>
pub fn read_i16_le(data: Bytes, offset: Int) -> Option<I16>
```

Read the 2 bytes starting at `offset` as one integer, in big-endian (`be`) or little-endian (`le`) order, and signed values in two's complement (D-334).

- **Fails:** `None` when `offset` is negative or `offset + 2` is greater than the length of `data` (D-318, D-334).
- **Cost:** O(1), required.

```cheby
test "16-bit reads name their byte order" {
  let data = bytes::from_list([0x01, 0x02, 0xFF])
  assert bytes::read_u16_be(data, 0) == Some(0x0102)
  assert bytes::read_u16_le(data, 0) == Some(0x0201)
  assert bytes::read_i16_le(data, 1) == Some(-254)
  assert bytes::read_i16_be(data, 1) == Some(0x02FF)
  assert bytes::read_u16_be(data, 2) == None
  assert bytes::read_u16_be(data, -1) == None
}
```

### `push_u32_be`, `push_u32_le`, `push_i32_be`, `push_i32_le`

```cheby
pub fn push_u32_be(data: Bytes, value: U32) -> Bytes
pub fn push_u32_le(data: Bytes, value: U32) -> Bytes
pub fn push_i32_be(data: Bytes, value: I32) -> Bytes
pub fn push_i32_le(data: Bytes, value: I32) -> Bytes
```

Return `data` with `value` added at the end as 4 bytes, in big-endian (`be`) or little-endian (`le`) order, and signed values in two's complement (D-334). They update `data` in place when nothing else refers to it (D-316).

- **Cost:** the same as `push` for each byte, required.

```cheby
test "32-bit pushes name their byte order" {
  assert bytes::push_u32_be(bytes::new(), 0xCBF4_3926) == bytes::from_list([0xCB, 0xF4, 0x39, 0x26])
  assert bytes::push_u32_le(bytes::new(), 0xCBF4_3926) == bytes::from_list([0x26, 0x39, 0xF4, 0xCB])
  assert bytes::push_i32_be(bytes::new(), -1) == bytes::from_list([0xFF, 0xFF, 0xFF, 0xFF])
}
```

### `read_u32_be`, `read_u32_le`, `read_i32_be`, `read_i32_le`

```cheby
pub fn read_u32_be(data: Bytes, offset: Int) -> Option<U32>
pub fn read_u32_le(data: Bytes, offset: Int) -> Option<U32>
pub fn read_i32_be(data: Bytes, offset: Int) -> Option<I32>
pub fn read_i32_le(data: Bytes, offset: Int) -> Option<I32>
```

Read the 4 bytes starting at `offset` as one integer, in big-endian (`be`) or little-endian (`le`) order, and signed values in two's complement (D-334).

- **Fails:** `None` when `offset` is negative or `offset + 4` is greater than the length of `data` (D-318, D-334).
- **Cost:** O(1), required.

```cheby
test "32-bit reads name their byte order" {
  let data = bytes::from_list([0x89, 0x50, 0x4E, 0x47])
  assert bytes::read_u32_be(data, 0) == Some(0x8950_4E47)
  assert bytes::read_u32_le(data, 0) == Some(0x474E_5089)
  assert bytes::read_i32_be(data, 0) == Some(-1991225785)
  assert bytes::read_u32_be(data, 1) == None
}
```

### `push_u64_be`, `push_u64_le`, `push_i64_be`, `push_i64_le`

```cheby
pub fn push_u64_be(data: Bytes, value: U64) -> Bytes
pub fn push_u64_le(data: Bytes, value: U64) -> Bytes
pub fn push_i64_be(data: Bytes, value: I64) -> Bytes
pub fn push_i64_le(data: Bytes, value: I64) -> Bytes
```

Return `data` with `value` added at the end as 8 bytes, in big-endian (`be`) or little-endian (`le`) order, and signed values in two's complement (D-334). They update `data` in place when nothing else refers to it (D-316). `I64` and `U64` are exact on every target (§3.3.3), so these work on JS too.

- **Cost:** the same as `push` for each byte, required.

```cheby
test "64-bit pushes name their byte order" {
  let big = bytes::push_u64_be(bytes::new(), 0x0102_0304_0506_0708)
  let little = bytes::push_u64_le(bytes::new(), 0x0102_0304_0506_0708)
  assert bytes::to_list(big) == [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]
  assert bytes::to_list(little) == [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
  assert bytes::push_i64_le(bytes::new(), -1) == bytes::from_list([0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
}
```

### `read_u64_be`, `read_u64_le`, `read_i64_be`, `read_i64_le`

```cheby
pub fn read_u64_be(data: Bytes, offset: Int) -> Option<U64>
pub fn read_u64_le(data: Bytes, offset: Int) -> Option<U64>
pub fn read_i64_be(data: Bytes, offset: Int) -> Option<I64>
pub fn read_i64_le(data: Bytes, offset: Int) -> Option<I64>
```

Read the 8 bytes starting at `offset` as one integer, in big-endian (`be`) or little-endian (`le`) order, and signed values in two's complement (D-334).

- **Fails:** `None` when `offset` is negative or `offset + 8` is greater than the length of `data` (D-318, D-334).
- **Cost:** O(1), required.

```cheby
test "64-bit reads name their byte order" {
  let data = bytes::from_list([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08])
  let all_ones = bytes::from_list([0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
  assert bytes::read_u64_be(data, 0) == Some(0x0102_0304_0506_0708)
  assert bytes::read_u64_le(data, 0) == Some(0x0807_0605_0403_0201)
  assert bytes::read_i64_be(all_ones, 0) == Some(-1)
  assert bytes::read_u64_le(all_ones, 0) == Some(0xFFFF_FFFF_FFFF_FFFF)
  assert bytes::read_u64_be(data, 1) == None
}
```

There are no reads or writes of `F32` or `Float` values in tier 1. They come later together with bit conversions such as `f32::from_bits` (D-387).

## Text

`Bytes` does not satisfy `Show`, because binary data has no single readable text, so a program picks hex, Base64 or UTF-8 itself. Debug printing writes it as `bytes::from_list([0x66, 0x6f])`, each byte as two lower-case hex digits (D-388).
