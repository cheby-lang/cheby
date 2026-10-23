# The Cheby Standard Library

Status: **draft**, pre-1.0. Breaking changes are allowed until 1.0 (D-075).

This directory specifies the standard library, one file per module (D-204, D-220). The language specification in [`../spec/`](../spec/) fixes the language and the semantics the runtime provides. This specification fixes the names, signatures and behavior of the modules under `std`. Where a rule comes from a logged decision, the decision is cited as `(D-NNN)`.

The library is written in tiers, each before the milestone that needs it (D-276). **Tier 1** holds the modules M1 needs (D-294), and **tier 2** the modules of the runtime milestone M2: fibers, channels, selectors, durations, randomness and the scheduler's settings (D-645). **Tier 3** holds files, networking, HTTP, URLs, time and processes, whose native side comes with M3 and whose JS side comes with M4 (D-985, D-986, D-987). **Tier 4** holds the modules that M4 implements on both targets, starting with `std::context` (D-1049).

An addition to an existing module keeps that module's tier. A new module joins the tier of the milestone that ships it, so `std::url`, `std::base64` and `std::hex`, which ship in M3, are in tier 3, although they are Cheby with no kernels (D-1054).

## Modules

### Tier 1

| Module                                                                 | Contents                                                                |
| ---------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| [`std::io`](io.md)                                                     | standard input and output, whole files and byte streams                 |
| [`std::env`](env.md)                                                   | command-line arguments and environment variables                        |
| [`std::list`](list.md)                                                 | the built-in `List<T>`                                                  |
| [`std::map`](map.md)                                                   | `Map<K, V>`, the hashed map                                             |
| [`std::set`](set.md)                                                   | `Set<T>`, the hashed set                                                |
| [`std::multimap`](multimap.md)                                         | a hashed map with a list of values per key                              |
| [`std::sorted_map`](sorted_map.md)                                     | a map ordered by key                                                    |
| [`std::sorted_set`](sorted_set.md)                                     | a set ordered by element                                                |
| [`std::sorted_multimap`](sorted_multimap.md)                           | a multimap ordered by key                                               |
| [`std::ordered_map`](ordered_map.md)                                   | a map in insertion order                                                |
| [`std::ordered_set`](ordered_set.md)                                   | a set in insertion order                                                |
| [`std::ordered_multimap`](ordered_multimap.md)                         | a multimap in insertion order                                           |
| [`std::string`](string.md)                                             | the built-in `String`                                                   |
| [`std::bytes`](bytes.md)                                               | `Bytes`, immutable byte sequences                                       |
| [`std::int`](int.md)                                                   | the built-in `Int`                                                      |
| [`std::float`](float.md)                                               | the built-in `Float`                                                    |
| [`std::i8`](i8.md) … [`std::u64`](u64.md), [`std::f32`](f32.md)        | the sized numeric types, defined by [Numeric modules](#numeric-modules) |
| [`std::bool`](bool.md), [`std::nil`](nil.md), [`std::order`](order.md) | the remaining built-in types                                            |
| [`std::result`](result.md), [`std::option`](option.md)                 | helpers for `Result` and `Option`                                       |
| [`std::ops`](ops.md)                                                   | the operator interfaces                                                 |
| [`std::json`](json.md), [`std::json::decode`](json/decode.md)          | JSON values, encoding and decoders                                      |

A submodule's file sits in a directory named after its parent, following the import path (D-351).

`Bool`, `Nil` and `Order` are declared in `std::bool`, `std::nil` and `std::order`, with the functions drafted in their files (D-353).

### Tier 2

| Module                         | Contents                                                                              |
| ------------------------------ | ------------------------------------------------------------------------------------- |
| [`std::fiber`](fiber.md)       | scopes, spawning, joining, cancellation, timeouts, parallel work and panics of fibers |
| [`std::channel`](channel.md)   | bounded channels, with their `Sender` and `Receiver` ends                             |
| [`std::selector`](selector.md) | waiting on several channels and timers at once                                        |
| [`std::duration`](duration.md) | `Duration`, spans of time with nanosecond resolution                                  |
| [`std::random`](random.md)     | random numbers from the host and from seeded generators                               |
| [`std::runtime`](runtime.md)   | the scheduler's thread count and the machine's CPU count                              |

These files fix the names that chapter 10 of the language specification left provisional (D-174, D-199, D-646).

Tier 2 is implemented on native in M2. Its JS side comes with M4: until then every kernel and intrinsic of a tier-2 module declares its `@external(js, …)` binding to a `.mjs` file that does not exist yet, as tier 1 does, so every function exists on both targets for the cross-target check (D-450, D-661). `std::duration` and the seeded half of `std::random` are written in Cheby with no kernels, so they run in constants (D-650, D-656). The tier-2 additions that need runtime work, `fiber::raise`, `map`, `try_map` and `yield_now`, `channel::send_now` and `receive_now`, and `selector::select_now`, are specified now and implemented after M3, `yield_now` with M4's JS scheduler. Their tests are pending until then (D-741, D-P18 to D-P20, D-P23).

### Tier 3

| Module                                  | Contents                                                                                      |
| --------------------------------------- | --------------------------------------------------------------------------------------------- |
| [`std::file`](file.md)                  | file handles, reading and writing in pieces, scoped opening, directories, scratch directories |
| [`std::net`](net.md)                    | TCP listeners and connections                                                                 |
| [`std::http`](http.md)                  | an HTTP/1.1 client with HTTPS, a server, and response builders                                |
| [`std::url`](url.md)                    | encoding and decoding URL queries and path segments                                           |
| [`std::base64`](base64.md)              | Base64 text for `Bytes`, standard and URL-safe, with strict decoding                          |
| [`std::hex`](hex.md)                    | hex text for `Bytes`, lowercase out and either case in, with strict decoding                  |
| [`std::time`](time.md)                  | `Timestamp` on the wall clock, readings of the monotonic clock, RFC 3339 text, `sleep`        |
| [`std::time::instant`](time/instant.md) | `Instant`, a point on the monotonic clock, with its comparison and arithmetic                 |
| [`std::process`](process.md)            | `exit`, and `until_interrupted` for stopping on SIGINT and SIGTERM                            |

Tier 3 is implemented on native in M3, in the order `std::time` with `std::time::instant`, `std::net`, `std::file`, `std::http` with `std::url`, and `std::process` (D-987, D-1057, D-1069). Every public function is written in Cheby over private kernels, and `std::http` is written over `std::net` and `std::url` (D-985, D-1003, D-1069). `std::time::instant`, `std::url`, `std::base64` and `std::hex` have no kernels at all (D-1057, D-1069, D-P10). Kernels never call a Cheby closure. File operations run on the blocking pool, and socket waits and `time::sleep` on the runtime's poller and timers, where a cancellation interrupts them (D-983). The failures of tier 3 are `io::Error`s, with a predicate in `std::io` for each kind its functions cause, and `http::Error` for HTTP's own kinds (D-981, D-1016). As in tier 2, every kernel declares its JS binding now, and the `.mjs` files follow in M4 (D-986). What a host without files, sockets or processes does, such as a browser, is OQ-P14. No tier-3 kernel runs in constants, but the parts of `std::time` and `std::http` that are Cheby without kernels do, and so do the whole of `std::time::instant`, `std::url`, `std::base64` and `std::hex` (D-991, D-1057, D-1069, D-P10).

### Tier 4

| Module                       | Contents                                                                                     |
| ---------------------------- | -------------------------------------------------------------------------------------------- |
| [`std::context`](context.md) | values provided for the duration of a callback, which every fiber spawned meanwhile inherits |

Tier 4 is implemented in M4, natively and on JS together (D-1049). It also changes three earlier modules: `std::time` reads its clock from context (D-1047), `std::random`'s host functions draw from a seeded stream when one is provided (D-1048), and every spawn of `std::fiber` copies the spawning fiber's context (D-1044). Until M4, these parts are drafts with no code, and their tests in the pages of shipped modules are pending for M4 (D-741).

## Entry format

Each module file starts with a short description of the module and its types, followed by its functions grouped under `##` headings. Each function is one entry (D-295, D-349):

````markdown
### `get`

```cheby
pub fn get<T>(xs: List<T>, index: Int) -> Option<T>
```

Returns the element at `index`, counting from 0.

- **Fails:** `None` when `index` is outside `0` to `length - 1`, including negative indexes (D-318).
- **Cost:** O(log n), required (D-300).

```cheby
test "get reads by position" {
  assert list::get([10, 20, 30], 1) == Some(20)
  assert list::get([10, 20, 30], -1) == None
}
```
````

- **Signature.** A declaration without a body, in the form `cheby doc` prints (D-349). It is not a complete item on its own, because a function without a body needs `@external` (§4.2). Types are declared the same way, with their variants when they are `exposed` and without a body when they are opaque.
- **Description.** What the function returns, in one or two sentences.
- **Fails.** For functions that return `Option` or `Result`: every case that gives `None` or `Err`.
- **Clamps.** The count, width and length parameters that are clamped (D-298). The source marks them with the standard-library-only attribute `@std_clamped` (§4.8, D-482), and passing a negative integer literal for one of them is a warning (D-303, §13.8).
- **Panics.** Present only where a language rule makes the function panic, such as overflow (D-025). Never because of a bad argument (D-297).
- **Cost.** In terms of `n`, the size of the first argument, unless the entry says otherwise. A cost marked **required** is an upper bound every implementation must meet. One marked **informative** is not binding (D-300).
- **Example.** At least one `test` block. The examples are written as if the module that contains them imports every module they name, as in `import std::list`. Test names are unique within a file, so a whole file's examples can run as one test module. A test that holds on only one target carries `@target(…)` ([§12.6](../spec/12-targets-and-ffi.md#126-target-specific-code)).
- **Environment-dependent examples.** A function whose result depends on the environment, such as standard input and output, files it did not create or the operating system, cannot have a deterministic test. Its example is a top-level function introduced by `_Example_, not a \`test\` because …:`. Where the function can fail, a `test` that checks only the failure, which is the same everywhere, is added when one exists.
- **Examples that need files.** A function that needs files has a `test` that works in a temporary directory, inside `file::with_temp_dir`, instead of an _Example_. It keeps an _Example_ only where the environment still decides the result, such as permissions (D-1068).

Fields that do not apply are left out. Every `cheby` block in this directory must parse as module items with the compiler's parser and with the tree-sitter grammar, as the examples do, and `cargo xtask cross-check` checks this. Validation errors, such as a signature without a body (E0046), are not consulted (D-424, D-662).

The implementation keeps its `test` blocks in the standard library's source files, next to the code they test. A build of a user package neither checks nor runs them, and testing the standard library itself checks and runs them (§13.6, D-479). Every test of this specification is among them (D-542):

- A test of `std::M` that needs a module `N` which imports `M` lives in `N`, since imports between std modules must be acyclic (D-145). It goes under the comment "Tests of `std::M` that need this module." and keeps its name and body, with module prefixes adjusted. When two moved tests would share a name in `N`, each gets its module's name in front, as "int add is plus as a function" and "float add is plus as a function" in `std::list`. A test that needs `N` only to show or compare a builtin type goes to a module that already depends on that type's home module, so `nil.md`'s "Nil shows as Nil" lives in `std::result` (D-630).
- A helper item of a test, such as a `priv` type, function or import, is named by a written path in a test. User builds skip the tests but still resolve their written paths, which count as uses, while a use through satisfaction does not count. So a helper used only through satisfaction is also named once, as `std::json`'s test "list encodes each element with its type's to_json" does with an extra `assert list(points) == array(points, to_json)`. Otherwise a user package that imports the module would get W0003 or W0004 in std (D-631).
- A test is written so that it checks unchanged, apart from module prefixes, in the std module where it lives. So it binds no name that module declares, since a binding must not reuse the name of a top-level function or constant of its module (D-051, D-062): the tests of `std::fiber` bind a scope as `owner`, never `scope` (D-662).
- A module may ship before all of its behavior exists. A test of a shipped module that cannot pass yet is listed as pending, with the milestone that delivers it, and the spec-test guard accepts it as absent while that milestone is in progress or still to come (D-741). An entry fails the guard once its milestone has ended, and at any time when its test is present, when it names no spec test of a shipped module, or when it repeats another entry (D-728, D-741).

## Conventions

These rules apply to every module. Entries do not repeat them.

### Arguments

- **Subject first.** Every function takes the value it works on, such as the list, map, string or number, as its first argument (D-296), so `xs |> list::map(_, f)` and `list::map(xs, f)` read the same way.
- **Callbacks last.** A function that takes a callback takes it as its last argument, so it works with `use` (§5.10).
- **Early exit is `try_fold`.** Every module that has `fold` also has `try_fold(c, initial, f)`, with `fold`'s parameters in `fold`'s order and a callback that returns `Result<A, E>`, taking the accumulator first and then the module's element shape: `T` in `list`, the sets and `channel`, `K, V` in the maps, `K, List<V>` in the multimaps, and `U8` in `bytes`. It calls `f` in the module's order, stops at the first `Err` and returns it, and otherwise returns `Ok` of the last accumulator, `Ok(initial)` for an empty collection. A search stops with its answer as `Err(answer)`, and `find`, `any`, `all` and `list::find_map` cover the common searches. Code that is generic over collections takes a walk function, such as `fn(initial, step) { set::try_fold(s, initial, step) }` (D-P01, ADR-P1). The twelve modules are `list`, `map`, `set`, `multimap`, `sorted_map`, `sorted_set`, `sorted_multimap`, `ordered_map`, `ordered_set`, `ordered_multimap`, `bytes` and `channel`.

### Failure

- A function returns `Option` when the answer may simply be absent, `Result<T, Nil>` when there is only one way to fail, and `Result<T, E>` with the module's own error type when callers need the reason (D-297).
- A function never panics because of a bad argument. It panics only where a language rule already does, such as overflow (D-025) or `==` reaching a function value (D-233) (D-297). A function that does an operator's arithmetic panics where that operator would, as `int::floor_div` does for a zero divisor, like `/` (D-1056). Any function that compares or hashes values, such as `list::contains` or `map::insert`, panics where `==` or hashing would ([§3.13](../spec/03-types.md#313-equality-hashing-and-debug-printing)).
- On native targets, any function that allocates panics with `out of memory` when its result cannot be allocated, as `string::repeat`, `pad_start` and `pad_end` can with a large count or width ([§11.8.2](../spec/11-errors-and-panics.md#1182-out-of-memory), D-642). Entries do not list this panic.
- **Counts are clamped.** A count, width or length outside its useful range is clamped, so `list::take(xs, -1)` is `[]` and `list::take(xs, 100)` on a shorter list is the whole list (D-298).
- **Indexes are not counts.** An index outside `0` to `length - 1`, including a negative one, makes the function return `None` or leave its input unchanged, as the entry says. Indexes never count from the end (D-318).
- **Ranges exclude their end.** A range given as `start` and `end` includes `start` and excludes `end` (D-319). A pair of bounds given as `low` and `high`, as in `int::clamp` and `random::int_between`, includes both, and is not a range of positions (D-654).
- An error type whose cases an outside specification fixes is `exposed` (`json::Error`). One whose cases follow the operating system is opaque, with predicate functions and `show` (`io::Error`) (D-304, ADR-0051).

### Names

- Sequences (`List`, `String`, `Bytes`) have `length`. Maps and sets have `size` (D-301).
- A type's text comes only from its `show` function. There are no `to_string` functions for this (D-307). `json::to_string` encodes a JSON value, which is a different operation.
- Numeric conversions are named `target::from_source` (D-308).
- Three affixes have one meaning each (D-1050). `try_x` is the form of `x` whose callback returns a `Result`, and which stops at the first `Err` and returns it, as `result::try` does. `checked_x` is the form of `x` that returns `Err` where `x` panics, as `int::checked_add` does (D-309, D-356). A form of a waiting operation that never waits takes the suffix `_now`, as `channel::send_now`, `channel::receive_now`, `selector::select_now` and `fiber::yield_now` do (D-P20, D-P23). A join that does not panic, if one is ever added, is not named `try_join`.
- `with_` has two meanings, told apart by the last parameter (D-1051). A `with_` function whose last parameter is a callback scopes a resource for the duration of that callback, as `file::with_open` and `random::with_generator` do (D-993, D-1048). Any other `with_` function returns a copy of its first argument with one part replaced, as `http::with_header` does (D-1006). Every new `with_` name follows one of the two.
- Modules keep their natural names, even where the name is a common local name, such as `url` (D-1053). A module may not bind a local or a parameter with the name of a module it imports (D-062), so std's own sources import such a module under a plural alias, as `std/http.cheby` writes `import std::json as jsons` and `import std::url as urls`, and the module's page says so in its introduction.

### Text

- Every length, position and count in `std::string` is in code points (Unicode scalar values) (D-302, ADR-0050). Grapheme clusters are available only through `string::to_graphemes` and `string::grapheme_length`.
- Case mapping and every other Unicode table come from one Unicode version fixed by the toolchain (D-310).

### Behavior on every target

- Every function gives the same result on every target (D-232, §12.4). Functions whose result could depend on the host, such as case mapping, use tables shipped with the runtime (D-310).
- No function grows the stack with the size of its input (D-094).
- Sorting is stable (D-299).

A function that visits elements calls its callback exactly once per element, front to back in the collection's order, unless its entry says it stops early, as `try_fold`, `any`, `all` and `find` do (D-P01, D-P03). A function that calls a comparison function, such as `list::sort_with`, uses one algorithm fixed by the toolchain on every target, so even a comparison function that is not a consistent order gives the same result everywhere, and the number of its calls is not specified. Functions on `Map`, `Set` and `multimap` visit entries in their per-process order (D-093, D-354).

### Equality, printing and `Show`

- `List`, `Map`, `Set` and every collection in this library compare with `==` by contents. Insertion-ordered collections also compare their order (D-343).
- The sorted, insertion-ordered and multimap collections are written in Cheby (D-336), and supply their own equality, hashing and debug printing through the standard-library-only attribute `@std_hooks` (D-342, D-482, §3.13, §4.8).
- Debug printing of `Map` and `Set` lists entries sorted by their printed text, compared as UTF-8 bytes (D-344, D-587). A `multimap` sorts its keys by their printed text and keeps each key's pairs in insertion order. Keys with the same printed text are ordered by the text of their pairs (D-847). These three types do not satisfy `Show`. The sorted and insertion-ordered collections satisfy `Show` when their elements do (D-344).
- The handles of tier 2, `channel::Sender`, `channel::Receiver`, `fiber::Scope`, `fiber::Fiber` and `selector::Selector`, and those of tier 3, `file::File`, `net::Listener` and `net::Connection`, are external types of their modules. They print as `<Sender>`, `<Receiver>`, `<Scope>`, `<Fiber>`, `<Selector>`, `<File>`, `<Listener>` and `<Connection>`, `==` and hashing panic when they reach one, and no constant may hold one (§3.13, D-233, D-647, D-982).

`show` of `List`, `Option`, `Result`, tuples and the sorted and insertion-ordered collections uses the punctuation of debug printing (D-134), with each element written by its own `show`, so strings inside appear without quotes: `[1, 2]`, `[a, b]` for `["a", "b"]`, `Some(3)`, `Err(not found)`, `(1, a)`, and `sorted_map::from_list([(a, 1), (b, 2)])` (D-355).

## Numeric modules

`std::int`, `std::float` and the sized modules `std::i8`, `std::i16`, `std::i32`, `std::i64`, `std::u8`, `std::u16`, `std::u32`, `std::u64` and `std::f32` share the families of functions below. [`int.md`](int.md) and [`float.md`](float.md) are written in full. Each sized file gives its type's range and a table of its functions, with one example per family (D-350).

In this section, `T` is the module's type and `t` its module name, such as `U8` and `u8`.

### Constants

Each integer module has the constants `min_value` and `max_value`, its type's smallest and largest values, such as `pub const max_value: U8 = 255`:

| Type  | `min_value`                 | `max_value`                  |
| ----- | --------------------------- | ---------------------------- |
| `Int` | −2⁶³ (−9223372036854775808) | 2⁶³−1 (9223372036854775807)  |
| `I8`  | −128                        | 127                          |
| `I16` | −32768                      | 32767                        |
| `I32` | −2147483648                 | 2147483647                   |
| `I64` | −2⁶³                        | 2⁶³−1                        |
| `U8`  | 0                           | 255                          |
| `U16` | 0                           | 65535                        |
| `U32` | 0                           | 4294967295                   |
| `U64` | 0                           | 2⁶⁴−1 (18446744073709551615) |

`Int`'s limits are the 64-bit ones, but on JS an `Int` outside ±(2⁵³−1) panics (D-037), so `int::min_value` and `int::max_value` exist only on native (D-358). `std::int` also has `min_safe` and `max_safe`, ±(2⁵³−1), the range in which `Int` works on every target. The floating-point constants are listed in [`float.md`](float.md) and [`f32.md`](f32.md).

### Conversions

Every numeric type has a conversion from each of the other numeric types, named `t::from_source`, such as `u8::from_int` and `int::from_u32` (D-308):

- A **widening** conversion, where every source value fits `T` exactly, returns `T`, for example `pub fn from_u8(n: U8) -> U32`.
- A **narrowing** conversion returns `Result<T, Nil>`, with `Err(Nil)` when the value does not fit, for example `pub fn from_int(n: Int) -> Result<U8, Nil>` (D-132).
- Each narrowing conversion between integer types has a wrapping twin, `t::wrapping_from_source`, which never fails. It keeps the low bits of the source's two's-complement value that fit `T` and reads them as `T`, so `u8::wrapping_from_u32(0x1234)` is `0x34`, `i8::wrapping_from_int(200)` is `-56` and `u64::wrapping_from_int(-1)` is `u64::max_value` (D-P12). There are 47, one for each narrowing pair with `Int` counted as a 64-bit type, so `std::int` has `wrapping_from_i64` and `wrapping_from_u64`. Those two return `Int`, so on JS they panic where the result is outside ±(2⁵³−1), like every `Int` result (D-037).
- `Int` from `I64` or `U64` is narrowing on every target, because it can fail on JS (ADR-0030).
- A conversion from an integer type to a float type, and `f32::from_float`, never fails. It returns the target type, rounded to the nearest value with ties to even, and `f32::from_float` gives an infinity for a value beyond `F32`'s range (D-359).
- Integer types have no `from_float` or `from_f32`. A float becomes an integer through `truncate`, `round`, `floor` or `ceil`, which return `Result<Int, Nil>`, followed by a narrowing conversion if needed (D-132, D-359).

### Parsing and text

```cheby
pub fn parse(text: String) -> Result<T, Nil>
pub fn show(n: T) -> String
```

- `parse` for an integer type accepts an optional `-` followed by one or more ASCII decimal digits, and nothing else. A value out of range is `Err(Nil)` (D-312). Integer types also have `parse_base(text: String, base: Int) -> Result<T, Nil>`. Unsigned types accept the `-` too in `parse` and `parse_base`, because only the value's range is checked: `u8::parse("-0")` is `Ok(0)`, and `u8::parse("-1")` is `Err(Nil)` (D-558).
- Integer types also have `to_base(n: T, base: Int) -> Result<String, Nil>`, the inverse of `parse_base`. It writes the digits `0` to `9` and then lowercase `a` to `z`, `-` for a negative value, no prefix and no leading zeros, and gives `Err(Nil)` for a base outside 2 to 36 (D-390, D-P11). So `u64::to_base(18446744073709551615, 16)` is `Ok("ffffffffffffffff")` and `i8::to_base(-128, 2)` is `Ok("-10000000")`. The two's-complement hex of a negative `I64` is `u64::to_base(u64::wrapping_from_i64(n), 16)` (D-P12).
- `parse` for a float type accepts an optional `-`, decimal digits, an optional fraction of `.` followed by digits, and an optional exponent, as well as `NaN`, `inf` and `-inf`. `_`, `.5`, `1.`, a leading `+` and whitespace are rejected. Every text that `show` gives parses back to the same value (D-313).
- `show` for an integer type gives the decimal digits, with `-` for negative values. `show` for a float type follows §3.3.6 (D-203, D-219, D-228).

### Comparison

```cheby
pub fn compare(a: T, b: T) -> Order
pub fn min(a: T, b: T) -> T
pub fn max(a: T, b: T) -> T
```

`compare` is the built-in order, which on floats is total and consistent with `==`: `-0.0` and `0.0` are `Equal`, and every NaN is `Equal` to every other NaN and `Greater` than every other value (D-135). `min` and `max` follow `compare`, and return `a` when the two are `Equal`.

### Arithmetic

```cheby
pub fn add(a: T, b: T) -> T
pub fn sub(a: T, b: T) -> T
pub fn mul(a: T, b: T) -> T
pub fn div(a: T, b: T) -> T
pub fn rem(a: T, b: T) -> T
pub fn neg(n: T) -> T
pub fn abs(n: T) -> T
```

These are the operators as functions, so that the numeric types satisfy `Add`, `Sub`, `Mul`, `Div` and `Neg` (§8.7) and can be passed as values. They behave exactly like `+`, `-`, `*`, `/`, `%` and unary `-` (§5.4.2), including their panics on integer overflow and division by zero (D-025, D-069). `abs` panics for an integer type's `min_value`, like `neg`.

### Checked arithmetic (integer types)

```cheby
pub fn checked_add(a: T, b: T) -> Result<T, Nil>
pub fn checked_sub(a: T, b: T) -> Result<T, Nil>
pub fn checked_mul(a: T, b: T) -> Result<T, Nil>
pub fn checked_div(a: T, b: T) -> Result<T, Nil>
pub fn checked_rem(a: T, b: T) -> Result<T, Nil>
pub fn checked_neg(n: T) -> Result<T, Nil>
```

Each returns `Err(Nil)` where the matching operator would panic, including for a zero divisor, so no `checked_*` function panics (D-309, D-356).

### `Int` results on JS

A function that returns `Result<Int, Nil>`, such as `checked_add`, `parse`, `int::from_i64` or `float::truncate`, returns `Err(Nil)` only for a value outside the 64-bit range, on every target. A value inside that range but outside ±(2⁵³−1) panics on JS like any other `Int` result (D-037, D-357), so each function returns the same value wherever it returns (D-232).

### Float-only functions

`std::float` and `std::f32` also share functions that integer types do not have. They are defined in [`float.md`](float.md), and [`f32.md`](f32.md) refers to them:

- the constants `infinity`, `neg_infinity`, `nan` and the others listed in `float.md`,
- `truncate`, `round`, `floor` and `ceil`, which return `Result<Int, Nil>`, with `Err(Nil)` for NaN, infinities and values outside `Int`'s range (D-132). `round` breaks ties away from zero (D-314),
- `is_nan`, `is_infinite` and `is_finite`.

`float::to_fixed`, which writes fixed decimals, is in `std::float` only (D-P06, D-397).

### Wrapping arithmetic (integer types)

```cheby
pub fn wrapping_add(a: T, b: T) -> T
pub fn wrapping_sub(a: T, b: T) -> T
pub fn wrapping_mul(a: T, b: T) -> T
pub fn wrapping_neg(n: T) -> T
```

Each computes the result modulo 2ⁿ, where n is the type's width in bits (D-025). For `Int` the width is 64 on every target, and on JS a result outside the safe-integer range panics (D-037).

## Gaps

A gap found while drafting is marked in the module file as `> **Open (OQ-<module>-<n>):** … _Proposed:_ …`, and the draft is written as if the proposal were accepted (D-352). The markers are settled together in a review round, and each settled marker is replaced by normative text that cites its decision.
