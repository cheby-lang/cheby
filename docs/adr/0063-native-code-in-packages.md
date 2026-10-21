---
status: accepted
date: 2026-10-21
log: D-940, D-941
---

# Native code in packages, and how the JIT loads it

A package supplies native code through an optional `[native]` table in `cheby.toml`. It has two keys: `sources`, the package's own C files, and `libraries`, system libraries linked by name. There are no build scripts, no prebuilt archives and no other languages in v1. This settles OQ-P9.

The system C compiler compiles each source once, with fixed flags and `-fPIC`, and the object is a cache entry. The same object serves both paths:

- `cheby build` links it into the executable, with the package's `libraries`.
- `cheby run`, `cheby test` and the REPL link the package's objects, a small forwarder object and its `libraries` into one shared library per package, load it with `dlopen`, and fill the forwarder's table with the process's runtime functions. C code calls `cheby_ffi_*` by name, and in the shared library those names jump through the table.

The manifest freezes at 1.0, so the smallest surface that runs example 020 and §12.5's SQLite example is the safest.

## Considered options

- Build scripts: arbitrary code at build time, against the spirit of D-057 and against deterministic builds (D-264).
- Prebuilt static archives by path: platform binaries in git, and an archive built without `-fPIC` cannot go into the shared library that `cheby run` needs.
- System libraries only: example 020 needs C code of its own.
- A flag or an environment variable outside the manifest: the package would not build by itself.
- Exporting the runtime's symbols from the `cheby` binary instead of a forwarder: stable Rust cannot export symbols from an executable, and the linker flags differ per platform.
- Building an executable for `cheby run` whenever native code is present: loses the REPL and `cheby test`.

## Consequences

- `cheby run`, `cheby test` and the REPL need a C compiler and a system linker when a package has a `[native]` table, as `cheby build` does.
- Loaded libraries are never unloaded. A REPL `:reload` that changes a C source loads a new library from a new path.
- A wrapper `.c` file that defines options and includes a vendored source covers compile-time options without build scripts.
- Dependencies follow the same rules from M5, each package's C compiled in that package.
- A later version can add prebuilt archives or other languages without breaking a manifest.
