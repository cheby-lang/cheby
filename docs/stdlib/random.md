# `std::random`

Random numbers, from two sources (D-226):

- **The host.** `int_between`, `fraction` and `from_host` draw from the host's random source, so their results differ on every call and every run. They are for games, jitter, sampling and the like.
- **A seeded generator.** A `Generator` is a value built from a seed and threaded through calls: each draw returns the number and the next generator. The same seed gives the same numbers on every run and every target, so tests, simulations and replays can be reproduced (D-655).

```cheby
pub type Generator
```

- **No hidden state.** The module keeps no state that a program can observe, global or per fiber. The host functions start from fresh host bits on each call, and a generator's state is in the value itself, so two fibers that use one generator value draw the same numbers (D-653).
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

Returns an integer from `low` to `high`, both included, drawn from the host's random source with every integer in that range equally likely. When `high` is less than `low`, it draws from `high` to `low` (D-654). It is `next_int_between(from_host(), low, high)` without the generator.

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

Returns a `Float` from `0.0` up to but not including `1.0`, drawn from the host's random source. It is one of the 2⁵³ values `k / 2⁵³`, each equally likely. It is `next_fraction(from_host())` without the generator.

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

Returns a generator whose state is 64 bits from the host's random source. Use it to give a program random numbers that it can still log and replay: draw a seed with `int_between`, print it, and build the generator with `from_seed`.

_Example_, not a `test` because its result differs on every call:

```cheby
fn shuffle_seed() -> Int {
  let seed = random::int_between(0, int::max_safe)
  io::println("seed {seed}")
  seed
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
