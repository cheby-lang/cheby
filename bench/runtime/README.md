# Runtime benchmarks

Cheby programs whose run time `cargo xtask bench-run` measures (D-718). They show what a change to
reference counting, unwinding or the runtime costs at run time, against a baseline recorded before
the change. D-702 reads the unwinding backends' numbers against a 3% reference. The numbers are
reported, never enforced. The one thing the command checks is that each checksum matches the
baseline's.

## Layout

| Path               | What it holds                                                 |
| ------------------ | ------------------------------------------------------------- |
| `<group>/<name>/`  | One benchmark: a Cheby package with `cheby.toml` and `src/`   |
| `rc/`              | The RC group: reference counting and in-place updates (D-718) |
| `fiber/`           | The fiber group, added with M2's scheduler                    |
| `baseline-m1.json` | M1's results for the RC group, the committed reference        |

The RC group has five benchmarks:

| Benchmark         | What it does                                                         |
| ----------------- | -------------------------------------------------------------------- |
| `list_pipeline`   | `list::range`, `map`, `filter`, `map` and `fold`, ten rounds         |
| `record_updates`  | a thousand particles moved with record updates, step by step         |
| `map_updates`     | `map::upsert` and `map::remove` on a map of up to 65,537 keys        |
| `string_building` | interpolated lines appended to a string, then split and joined again |
| `tree_inserts`    | pseudo-random keys inserted into a persistent binary search tree     |

A group is a directory of `bench/runtime`, and its benchmarks are the subdirectories that hold a
`cheby.toml`. A new group or benchmark needs no change to the command.

Each program follows these rules:

- It uses only what M1 ships, so the M1 baseline can run it.
- It reads its size from the first program argument. Without one, it uses a default that runs for
  0.5 to 2 seconds on a release build.
- It prints `size: <n>` and then `checksum: <n>`. The checksum depends only on the size, never on
  timing, addresses, thread count or a map's iteration order.
- It passes `cheby check` and both parsers: `cargo xtask cross-check` parses `bench/runtime`.

## Commands

```sh
cargo xtask bench-run                      # every group, 5 runs each, release `cheby` built first
cargo xtask bench-run --quick              # one run each
cargo xtask bench-run --group rc --threads 1 --runs 5 \
  --baseline bench/runtime/baseline-m1.json --json out.json
cargo xtask bench-run --cheby path/to/cheby --commit 587f8a9   # another binary
cargo xtask bench-run --env CHEBY_RC=naive                     # a variable for every run
```

| Option            | Meaning                                                                                     |
| ----------------- | ------------------------------------------------------------------------------------------- |
| `--quick`         | One timed run per benchmark, unless `--runs` says otherwise                                 |
| `--group G`       | `rc`, `fiber` or another directory of `bench/runtime`; `all` (the default) runs every group |
| `--threads N`     | Sets `CHEBY_THREADS=N` for every run                                                        |
| `--runs N`        | Timed runs per benchmark (default 5)                                                        |
| `--baseline FILE` | Compares with the JSON of an earlier run                                                    |
| `--cheby PATH`    | Measures this binary instead of building `cheby_cli` with `--release`                       |
| `--commit SHA`    | The commit the measured binary was built from, for the metadata                             |
| `--env K=V`       | Sets a variable for every run (repeatable)                                                  |
| `--json OUT`      | Writes the results as JSON                                                                  |

For each benchmark the command runs `cheby run` in its directory `--runs` times and times each run
from spawn to exit, compilation included. It reports the median, the minimum and the maximum. Then it
runs the benchmark once more with `CHEBY_RT_STATS=1` and reads the runtime's counters from standard
error (D-635). That text is not a stable interface, so it is parsed tolerantly, and lines it does not
recognize are kept under `unparsed`. Every run must exit with status 0 and print the same checksum.

The Markdown table shows each benchmark's median, minimum and maximum, the baseline's median, the
change `Δ%` against it, the checksum, the allocation count and the peak bytes. Below the table come
the geometric mean of the ratios (median over baseline median) over the benchmarks the baseline
holds. The command fails after printing the table if a size or a checksum differs from the
baseline's.

`cargo xtask bench-compile` reports compile speed on generated projects (D-247). Since D-718 its
build rows also show `code KiB`, the machine code the JIT defined, and `RC ops`, the `dup` and `drop`
statements of every MIR unit. It takes `--shapes 10k,100k,1m`, `--cheby PATH`, `--env K=V` and
`--json OUT` as well.

## JSON

```json
{
  "meta": {
    "commit": "587f8a9",
    "cheby": "/path/to/cheby",
    "rustc": "rustc 1.99.0 (…)",
    "os": "macos",
    "arch": "aarch64",
    "cpu": "Apple M3",
    "cores": 8,
    "date": "2026-10-12T00:00:00Z",
    "load": [1.2, 1.5, 1.7],
    "load_after": [1.1, 1.4, 1.7],
    "env": { "CHEBY_RC": "naive" },
    "threads": 1,
    "runs": 5
  },
  "results": [
    {
      "group": "rc",
      "name": "list_pipeline",
      "n": 400000,
      "median_ns": 1000000000,
      "min_ns": 990000000,
      "max_ns": 1010000000,
      "runs_ns": [1000000000, 990000000, 1010000000, 1000000000, 1000000000],
      "checksum": "583798564",
      "stats": {
        "main_ms": 970.1,
        "entries_ms": 971.0,
        "peak_bytes": 18280,
        "counters": { "allocations": 5887, "list push in place": 29960 }
      }
    }
  ]
}
```

- `commit` is the short hash of the measured `cheby`'s commit: `HEAD` (with `+dirty` when tracked
  files changed) for a binary built here, the `--commit` value, or `unknown`.
- `load` and `load_after` are the 1, 5 and 15-minute load averages before and after the runs, or
  `null` where the system does not report them.
- `env` holds the `--env` variables and `CHEBY_THREADS` of `--threads`.
- `n` is the size the program printed, or `null` if it printed none.
- `checksum` is a string, as the program printed it.

## How `baseline-m1.json` was recorded

`baseline-m1.json` is M1's reference, recorded once, before any Perceus or unwinding change landed
(D-718). Nobody records it again. It is the reference for checksums and for the `Δ%` column. It was
recorded while other builds ran on the same machine (see its `load` and `load_after`), so a report
quotes M1's times from a quiet run of the same binary, not from this file.

1. `git diff --name-only 587f8a9 HEAD -- crates std xtask Cargo.toml Cargo.lock rust-toolchain.toml`
   listed nothing: the code at `HEAD` was M1's.
2. The sources of M1's last commit were extracted and built in a scratch directory outside version
   control (`.m2-work/` is excluded from git), where the binary stays for later runs:

   ```sh
   git archive 587f8a9 | tar -x -C .m2-work/slice/P0/m1-src
   cd .m2-work/slice/P0/m1-src
   CARGO_TARGET_DIR=../m1-target cargo build --release --locked -p cheby_cli
   ```

3. From the repository, with that binary:

   ```sh
   cargo xtask bench-run --group rc --threads 1 --runs 5 \
     --cheby .m2-work/slice/P0/m1-target/release/cheby --commit 587f8a9 \
     --json bench/runtime/baseline-m1.json
   ```

M1 ignores `CHEBY_THREADS`. `--threads 1` is recorded so the baseline matches the runs it is
compared with: D-702's measurement runs the RC group on one thread.
