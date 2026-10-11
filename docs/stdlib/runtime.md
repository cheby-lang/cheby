# `std::runtime`

The scheduler's settings, as a program sees them. By default the scheduler runs one OS thread per CPU core, and a program changes that with an environment variable or with `set_thread_count` (D-175, §10.2).

- **Thread count.** The scheduler starts with the number of threads that `CHEBY_THREADS` gives, or `cpu_count()` when it is unset or empty. It is always from 1 to 1024 (D-657, D-658). The number of threads changes only timing and scheduling, never a result that does not depend on them (D-232).
- **`CHEBY_THREADS`.** The runtime reads the variable once, before `main` starts. Its value must be a decimal whole number of at least 1, which is clamped to 1024. Any other non-empty value, such as `0`, `-2` or `four`, stops the program before `main` runs, with `error: CHEBY_THREADS must be a whole number of at least 1, not "four"` on standard error and exit status 1. `cheby run`, `cheby test` and the REPL read it the same way (D-658).
- **Stack sizes.** Fiber stacks are not set here. A package sets the default sizes at build time with the `[runtime]` table of `cheby.toml`, and a single spawn sets its own size with `fiber::spawn_with_stack` (D-125, D-660).
- **On JS.** Fibers share the one JS thread (§12.3.1), so `thread_count` returns 1, `set_thread_count` has no effect and `CHEBY_THREADS` is not read. `cpu_count` returns what the host reports (D-657).
- **Kernels.** Every function is a kernel, bound as `@external(native, "cheby_runtime_<function>")` and `@external(js, "./runtime.mjs", "<function>")` (D-484, D-661). None of them runs in constants: reaching one while computing a constant is E0077 (D-090, D-537).

None of these functions can fail. Their results depend on the machine and on how the program was started, so their examples are top-level functions marked _Example_, apart from the properties that hold everywhere.

## Threads

### `thread_count`

```cheby
pub fn thread_count() -> Int
```

Returns the number of OS threads the scheduler runs fibers on now, from 1 to 1024.

- **Cost:** O(1). Informative.

```cheby
test "thread_count is at least one" {
  let count = runtime::thread_count()
  assert count >= 1 && count <= 1_024
}
```

### `set_thread_count`

```cheby
pub fn set_thread_count(count: Int)
```

Sets the number of OS threads the scheduler runs fibers on. The change takes effect without waiting for any fiber: added threads start taking fibers at once, and a removed thread stops after the fiber it runs reaches its next suspension point (§10.2). It applies to the whole program, including every `cheby test` test that runs at the same time (D-657).

- **Clamps:** `count` to `1` to `1024`, so `0` or a negative count gives one thread (D-298).
- **Cost:** O(n) in the change of the thread count, to start or stop threads. Informative.

_Example_, not a `test` because it changes the whole program's scheduler:

```cheby
fn main() {
  // A program whose fibers mostly wait on IO keeps one thread per core,
  // and this one runs on two whatever the machine.
  runtime::set_thread_count(2)
  let count = runtime::thread_count()
  io::println("{count} threads")
}
```

## The machine

### `cpu_count`

```cheby
pub fn cpu_count() -> Int
```

Returns the number of CPU cores the program may use, at least 1. On native it takes the process's CPU affinity and quotas into account where the operating system reports them, as Rust's `std::thread::available_parallelism` does, and is 1 when the system reports nothing. It is the default thread count, before the clamp to 1024.

- **Cost:** O(1). Informative.

```cheby
test "cpu_count is at least one" {
  assert runtime::cpu_count() >= 1
}
```
