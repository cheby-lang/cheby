# `std::random`

Random numbers, from two sources (D-226):

- **The host.** `int_between`, `fraction` and `from_host` draw from the host's random source, so their results differ on every call and every run. They are for games, jitter, sampling and the like.
- **A seeded generator.** A `Generator` is a value built from a seed and threaded through calls: each draw returns the number and the next generator. The same seed gives the same numbers on every run and every target, so tests, simulations and replays can be reproduced (D-655).

```cheby
pub type Generator
```

- **State.** A generator's state is in the value itself, so two fibers that use one generator value draw the same numbers. The module keeps no global state. Its only other state is a seeded stream that [`with_generator`](#with_generator) provides to a fiber: while one is in the fiber's context, the host functions draw from it instead of the host, and each draw advances the drawing fiber's copy only. Outside a stream, the host functions start from fresh host bits on each call (D-653, D-1048).
- **Bounds include both ends.** `int_between(low, high)` and `next_int_between` draw from `low` to `high` with both ends included, so `int_between(0, 0)` is `0` and `int_between(1, 6)` is a die roll. When `high` is less than `low`, the two bounds swap roles, so every pair of bounds is valid and no call fails or panics (D-297, D-654). This is the exception to "ranges exclude their end" (D-319): a pair of bounds is not a range of positions.
- **The seeded algorithm is fixed.** A generator is SplitMix64, and the way a draw maps its output to a number is given below, bit for bit. A program's seeded draws are therefore the same on every target and with every toolchain from 1.0 on (D-232, D-655).
- **Kernels.** The module has one private kernel, which returns 64 bits from the host's random source. Everything else is written in Cheby over it, so the seeded functions run in constants and the host functions do not: reaching the kernel while computing a constant is E0077 (D-090, D-537, D-656). If the host's random source fails, the kernel ends the program with a fatal runtime error that names the operating system's error. This reaches `int_between`, `fraction` and `from_host`. It is neither an `Err` nor a panic, because no call fails or panics, and a random source that fails is a broken host, not a condition a program can handle (D-794).
- **Not for secrets.** The host source is unpredictable, but this module makes no promise about cryptographic strength. Keys and tokens wait for a cryptography module (D-656).
- **Equality and printing.** `Generator` compares, hashes and debug-prints as the record it is, a single `U64` of state, so two generators are `==` exactly when they will draw the same numbers.

Every function costs O(1), informative. A draw between bounds may repeat a step to stay unbiased, as described under `next_int_between`, which happens with probability below 1 in 2 per step and on average fewer than 2 steps.

## The host source

The examples of the host functions are not `test` blocks that check values, because their results differ on every call. Their tests check only properties that hold for every result.

### `int_between`

```cheby
pub fn int_between(low: Int, high: Int) -> Int
```

Returns an integer from `low` to `high`, both included, drawn from the host's random source, or from the seeded stream in context ([Seeded streams](#seeded-streams)), with every integer in that range equally likely. When `high` is less than `low`, it draws from `high` to `low` (D-654). It is `next_int_between(from_host(), low, high)` without the generator.

- **Panics:** on JS, when the drawn integer is outside ±(2⁵³−1), which needs a bound outside that range (D-037).

```cheby
test "int_between stays within its bounds" {
  let roll = random::int_between(1, 6)
  assert roll >= 1 && roll <= 6
  assert random::int_between(0, 0) == 0
  let reversed = random::int_between(10, 5)
  assert reversed >= 5 && reversed <= 10
}
```

### `fraction`

```cheby
pub fn fraction() -> Float
```

Returns a `Float` from `0.0` up to but not including `1.0`, drawn from the host's random source, or from the seeded stream in context ([Seeded streams](#seeded-streams)). It is one of the 2⁵³ values `k / 2⁵³`, each equally likely. It is `next_fraction(from_host())` without the generator.

```cheby
test "fraction is at least zero and below one" {
  let x = random::fraction()
  assert x >= 0.0 && x < 1.0
}
```

### `from_host`

```cheby
pub fn from_host() -> Generator
```

Returns a generator whose state is 64 bits from the host's random source. Under a seeded stream in context, its state is the output of one step of the stream ([Seeded streams](#seeded-streams), D-1048). Use it to give a program random numbers that it can still log and replay: draw a seed with `int_between`, print it, and build the generator with `from_seed`.

_Example_, not a `test` because its result differs on every call:

```cheby
fn shuffle_seed() -> Int {
  let seed = random::int_between(0, int::max_safe)
  io::println("seed {seed}")
  seed
}
```

## Seeded streams

A seeded stream makes the host functions reproducible for the code a program calls, such as a test of code that draws with `int_between`, without passing a generator through it ([`std::context`](context.md), D-1046, D-1048).

### `with_generator`

```cheby
pub fn with_generator<R>(generator: Generator, body: fn() -> R) -> R
```

Calls `body` on the calling fiber with a seeded stream in context that starts at `generator`, and returns what `body` returns. It is written with `use` (§5.10). While the stream is in context:

1. `int_between(low, high)` is `next_int_between` on the stream, `fraction` is `next_fraction`, and `from_host` takes one step, as described under [`next_int_between`](#next_int_between), and returns the generator whose state is that step's output. Each draw advances the calling fiber's copy of the stream, and no other fiber sees it.
2. A spawn of any kind leaves the spawning fiber's stream alone. Let `s` be the spawning fiber's state and `n` the spawn's number, counted from 1 among the fiber's spawns since its last draw. The new fiber's stream starts from the generator whose state is `s + n × 0xD1B54A32D192ED03` modulo 2⁶⁴, put through the output function of a step: `z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9`, then `z = (z ^ (z >> 27)) * 0x94D049BB133111EB`, then `z ^ (z >> 31)`, all modulo 2⁶⁴. A scope's body, a timeout's function and a selector's mapping function run on the calling fiber, so they draw from its stream ([§10.11](../spec/10-concurrency.md#1011-context-values)).
3. So a fiber's own draws never depend on its spawns. A child's draws depend on its parent's draws before the spawn and on its place among the spawns since then, and nothing depends on how the scheduler interleaves fibers.

A test that checks exact values under a stream stays valid only while the fibers that the code starts stay the same. For code that spawns, checking properties of the results is more robust.

When `body` returns or unwinds, the fiber's stream is again what it was before the call, so an outer stream continues from where it stood. The stream's key is private to this module. Seeded streams belong to tier 4 and are implemented in M4, with `std::context` (D-1049).

- **Cost:** O(log k) in the number of keys the fiber has values for, plus the call of `body`. Each host draw under a stream costs the same lookup. Informative.

```cheby
test "with_generator makes host draws follow the seeded draws" {
  use <- random::with_generator(random::from_seed(7))
  let a = random::fraction()
  let b = random::int_between(1, 6)
  let (x, after) = random::next_fraction(random::from_seed(7))
  let (y, _) = random::next_int_between(after, 1, 6)
  assert (a, b) == (x, y)
}

test "with_generator gives the same draws for the same generator" {
  let inside = random::with_generator(random::from_seed(42), fn() { random::from_host() })
  let again = random::with_generator(random::from_seed(42), fn() { random::from_host() })
  assert inside == again
}

test "a spawn leaves the parent's stream alone" {
  let alone = random::with_generator(random::from_seed(42), fn() { random::fraction() })
  let after_spawn = random::with_generator(random::from_seed(42), fn() {
    let _ = {
      use owner <- fiber::scope()
      fiber::spawn(owner, fn() { random::fraction() })
    }
    random::fraction()
  })
  assert alone == after_spawn
}

test "spawned fibers draw reproducibly and apart" {
  fn draws() {
    use <- random::with_generator(random::from_seed(42))
    use owner <- fiber::scope()
    let first = fiber::spawn(owner, fn() { random::fraction() })
    let second = fiber::spawn(owner, fn() { random::fraction() })
    (fiber::join(first), fiber::join(second))
  }
  let assert Ok((a, b)) = draws()
  assert draws() == Ok((a, b))
  assert a != b
}
```

## Seeded generators

### `from_seed`

```cheby
pub fn from_seed(seed: Int) -> Generator
```

Returns the generator whose state is the 64 bits of `seed` in two's complement. Every `Int` is a valid seed, and different seeds give different generators.

```cheby
test "a seed always gives the same numbers" {
  let (first, _) = random::next_int_between(random::from_seed(42), 1, 6)
  let (again, _) = random::next_int_between(random::from_seed(42), 1, 6)
  assert first == again
  assert random::from_seed(42) == random::from_seed(42)
  assert random::from_seed(42) != random::from_seed(43)
}
```

### `next_int_between`

```cheby
pub fn next_int_between(generator: Generator, low: Int, high: Int) -> (Int, Generator)
```

Returns an integer from `low` to `high`, both included, and the generator that comes after it. Every integer in the range is equally likely. When `high` is less than `low`, it draws from `high` to `low` (D-654).

The draw is defined bit for bit, so that it is the same everywhere (D-655). Let `lo` and `hi` be the smaller and the larger bound, and `span` the number of integers from `lo` to `hi`, computed modulo 2⁶⁴, so a span of all 2⁶⁴ integers is 0:

1. A **step** adds `0x9E3779B97F4A7C15` to the state modulo 2⁶⁴, and outputs `z ^ (z >> 31)`, where `z` is the new state put through `z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9` and then `z = (z ^ (z >> 27)) * 0x94D049BB133111EB`, both modulo 2⁶⁴. This is SplitMix64.
2. If `span` is 0, the result is the output of one step, read as a two's-complement `Int`.
3. Otherwise let `threshold` be `(2⁶⁴ − span) % span`. Steps are taken until one outputs `x >= threshold`, and the result is `lo + x % span`. Rejecting the outputs below `threshold` makes every result equally likely.

The returned generator holds the state after the last step.

- **Panics:** on JS, when the drawn integer is outside ±(2⁵³−1), which needs a bound outside that range (D-037).

```cheby
test "next_int_between rolls a die from a seed" {
  fn roll(generator, count, rolls) {
    case count {
      0 => rolls
      _ => {
        let (n, next) = random::next_int_between(generator, 1, 6)
        roll(next, count - 1, [..rolls, n])
      }
    }
  }
  assert roll(random::from_seed(42), 10, []) == [2, 2, 1, 1, 5, 1, 2, 3, 2, 3]
  assert roll(random::from_seed(2026), 5, []) == [2, 6, 3, 1, 4]
}

test "next_int_between treats its bounds alike in either order" {
  let generator = random::from_seed(7)
  assert random::next_int_between(generator, 6, 1) == random::next_int_between(generator, 1, 6)
  let (zero, _) = random::next_int_between(generator, 0, 0)
  assert zero == 0
}
```

### `next_fraction`

```cheby
pub fn next_fraction(generator: Generator) -> (Float, Generator)
```

Returns a `Float` from `0.0` up to but not including `1.0`, and the generator that comes after it. It takes one step, as described under `next_int_between`, and returns `x >> 11` divided by 2⁵³, which is exact (D-655).

```cheby
test "next_fraction draws from a seed" {
  let (a, after) = random::next_fraction(random::from_seed(42))
  let (b, _) = random::next_fraction(after)
  assert a == 0.7415648787718233
  assert b == 0.1599103928769201
}
```
