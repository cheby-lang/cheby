---
status: accepted
date: 2026-11-08
log: D-1433
---

# Path separators are the same on every platform

`std::filepath` reads both `/` and `\` as separators, and `X:/` and `//server/share` as roots, on every platform and target. It never asks the host. Every path it returns is normalized and written with `/`.

```cheby
import std::filepath

test "a Windows path reads the same on every host" {
  assert filepath::parent("C:\\Temp\\x") == Some("C:/Temp")
  assert filepath::is_absolute("C:/x")
  assert filepath::join("C:\\Temp", "x") == "C:/Temp/x"
}
```

Paths in the host's form already reach programs: `file::with_temp_dir` and `env::current_dir` give `C:\Users\…` on Windows, and so does whatever a shell passes in `env::args`. A module that splits only at `/` takes those apart wrongly. A module that follows the host gives different results on different machines, so a golden written on Linux fails on Windows, and a constant takes the value of the machine that builds it, not the one that runs it. Cheby already keeps results independent of the host where it can, as directory listings are sorted by bytes (D-997).

## Considered options

- Only `/`, as Go's `path`. Unix names that hold `\` stay intact, but Windows paths in the host's form are one component.
- The host's separators, as Go's `path/filepath` and Rust. Exact on each operating system, but results depend on it, and JS needs a kernel to learn it.
- Only `/`, with a host-dependent `from_host` that converts a Windows path. The core stays simple, but the conversion needs a kernel, and forgetting it fails silently.

## Consequences

- On Unix, a file name that holds `\`, or a first component such as `C:`, is read as separators or a drive. `join("dir", "a\\b")` gives `"dir/a/b"`, another file. `filepath::child` appends a listed name without reading it (D-1436), and a directory walk must build every level with it.
- `/` is the only separator in output, so `==` on normalized paths works, and Windows accepts `/` in every `std::file` function (D-996).
