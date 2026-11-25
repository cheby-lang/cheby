# `std::secret`

Values that never appear in printed text, such as a password field of a record (D-1901).

```cheby
pub type Secret<T>
```

`Secret` is opaque and holds one value of any type. A field of type `Secret<String>` is redacted wherever the record is printed.

- **Printing.** A secret prints as `<redacted>`, whatever it holds, alone and inside any other value: with `{x:?}`, in the operand lines of a failed `assert` (§11.5), with `io::debug`, in a `dyn` value and in the REPL. So `log::inspect("config", config)` logs a record without its secrets. The module supplies this debug text through the standard-library-only attribute of D-342 (§3.13, D-1902).
- **`Show`.** `Secret<T>` satisfies `Show` for every `T`, and its text is `<redacted>` too. So `"{token}"` compiles, `log::field("token", token)` logs the placeholder, and `main` prints it for an error that is a secret (§10.10). Text that must hold the value takes it from [`expose`](#expose) first: `"Bearer {token}"` with a secret `token` is `Bearer <redacted>`, and the compiler does not warn about it (D-1903).
- **Other interfaces.** A secret has no order and does not encode as JSON: it satisfies neither `Compare` nor `json::ToJson` (D-1903).
- **Equality and hashing.** Two secrets are `==` when their values are, and they hash as their values do, so a record with a secret compares and can be a map key. Both panic where they panic for the values, as on a function (§3.13). `==` is not constant-time: how long it takes can tell how much of a guess was right. Checking a guess from outside against a secret waits for a cryptography module (D-656, D-1904).
- **What it does not hide.** Redaction is about text. A value that [`expose`](#expose) returns prints as any other value does. The memory of a secret is not cleared when it is released. A secret in a constant is in the executable and in the build cache, as every literal is, so a program reads a real secret at run time, for example with `env::get` (D-1906).
- **Decoding.** `decode::map(decode::string(), secret::new)` decodes a JSON string into a secret.
- **Constants.** Every function of the module runs in constants, and a constant may hold a secret (D-1906).
- **Written in Cheby.** The module has no kernels and imports nothing, so it runs on JS unchanged (D-1901).

The examples use this type:

```cheby
type Login {
  user: String,
  password: secret::Secret<String>,
}
```

```cheby
test "a secret field prints as <redacted>" {
  let login = Login { user: "ada", password: secret::new("hunter2") }
  assert "{login:?}" == "Login \{ user: \"ada\", password: <redacted> }"
}

test "a secret prints as <redacted> inside other values" {
  let found = Some(secret::new("hunter2"))
  let several = [secret::new(1), secret::new(2)]
  let pair = (secret::new("a"), 1)
  let nested = secret::new(secret::new("hunter2"))
  assert "{found:?}" == "Some(<redacted>)"
  assert "{several:?}" == "[<redacted>, <redacted>]"
  assert "{pair:?}" == "(<redacted>, 1)"
  assert "{nested:?}" == "<redacted>"
}

test "secrets compare by their values" {
  assert secret::new("hunter2") == secret::new("hunter2")
  assert secret::new("hunter2") != secret::new("hunter3")
  let login = Login { user: "ada", password: secret::new("hunter2") }
  assert login == Login { user: "ada", password: secret::new("hunter2") }
  assert login != Login { user: "ada", password: secret::new("other") }
}
```

## Functions

### `new`

```cheby
pub fn new<T>(value: T) -> Secret<T>
```

Returns a secret that holds `value`.

```cheby
test "new hides a value of any type" {
  assert secret::expose(secret::new("hunter2")) == "hunter2"
  assert secret::expose(secret::new(1234)) == 1234
  let pin = secret::new(1234)
  assert "{pin:?}" == "<redacted>"
}
```

### `expose`

```cheby
pub fn expose<T>(s: Secret<T>) -> T
```

Returns the value that `s` holds. Only `expose` and the function given to [`map`](#map) see it, so a search for the two names finds every place that reads a secret (D-1905).

```cheby
test "expose returns the value" {
  let token = secret::new("hunter2")
  let text = secret::expose(token)
  assert text == "hunter2"
  assert "{text:?}" == "\"hunter2\""
}
```

### `map`

```cheby
pub fn map<T, U>(s: Secret<T>, f: fn(T) -> U) -> Secret<U>
```

Returns a secret that holds `f` of the value that `s` holds, so what is made from a secret stays hidden (D-1905).

```cheby
test "map keeps the result hidden" {
  let words = secret::map(secret::new("hunter2"), fn(token) { ["Bearer", token] })
  assert "{words}" == "<redacted>"
  assert secret::expose(words) == ["Bearer", "hunter2"]
}
```

### `show`

```cheby
pub fn show<T>(s: Secret<T>) -> String
```

Returns `<redacted>`, whatever `s` holds. It makes `Secret<T>` satisfy `Show` (§8.8) for every `T`, also one that does not satisfy it (D-1903).

```cheby
test "show hides the value" {
  let token = secret::new("hunter2")
  assert secret::show(token) == "<redacted>"
  assert "{token}" == "<redacted>"
  assert "Bearer {token}" == "Bearer <redacted>"
}

test "show needs no Show of the value" {
  let hidden = secret::new(fn(x: Int) { x + 1 })
  assert secret::show(hidden) == "<redacted>"
}
```
