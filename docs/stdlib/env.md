# `std::env`

The program's command-line arguments and environment variables. `main` takes no parameters, so any module reads them through this module (D-183, §10.10). Tier 1 only reads the environment. It has no function that changes a variable.

None of these functions can fail. Text from the operating system that is not valid UTF-8 has each invalid sequence replaced with U+FFFD (D-311).

The examples are not `test` blocks where their results depend on how the program was started. They are top-level functions marked _Example_.

### `args`

```cheby
pub fn args() -> List<String>
```

Returns the command-line arguments, in order, without the program name (D-347). A program started with `cheby run -- a b` gets `["a", "b"]`.

- **Cost:** O(n) in the number of arguments. Informative.

_Example_, not a `test` because the arguments depend on how the program was started:

```cheby
fn parse_args(args: List<String>) -> Result<String, String> {
  case args {
    [path] => Ok(path)
    _ => Err("usage: count PATH")
  }
}

fn main() -> Result<Nil, String> {
  use path <- result::try(parse_args(env::args()))
  io::println("counting {path}")
  Ok(Nil)
}
```

### `program_name`

```cheby
pub fn program_name() -> String
```

Returns the name the program was started with, which `args` leaves out (D-347).

- **Cost:** O(n) in the length of the name. Informative.

_Example_, not a `test` because the name depends on how the program was started:

```cheby
fn usage() -> String {
  let name = env::program_name()
  "usage: {name} PATH"
}
```

Under `cheby run` and `cheby test`, `program_name` returns the package's name from `cheby.toml`, so usage messages are the same on every machine. Otherwise it returns the first argument as the operating system passes it, with invalid UTF-8 replaced (D-311, D-369). What it returns on JS hosts is specified in tier 3.

### `get`

```cheby
pub fn get(name: String) -> Option<String>
```

Returns the value of the environment variable `name`. A variable that is set but empty gives `Some("")`, and a variable that is not set gives `None` (D-347). Names are matched as the operating system matches them, so on Windows `get("Path")` finds `PATH`.

- **Fails:** `None` when no variable named `name` is set, including when `name` cannot be a variable's name: when it is empty or contains `=` or U+0000.
- **Cost:** O(n) in the length of the value, plus the lookup. Informative.

```cheby
test "a name that cannot be set gives None" {
  assert env::get("") == None
  assert env::get("A=B") == None
}
```

_Example_, not a `test` because the value depends on the environment:

```cheby
fn port() -> Result<Int, Nil> {
  case env::get("PORT") {
    None => Ok(8080)
    Some(text) => int::parse(text)
  }
}
```

`get` returns `None` for a name no variable can have, such as `""`, `"A=B"` or a name containing U+0000, and never panics (D-370).
