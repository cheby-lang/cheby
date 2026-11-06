# cheby_rt_exe

The runtime archive of executables (D-928). Built only as a staticlib, `libcheby_rt_exe.a`, it is `cheby_runtime` with mimalloc as Rust's global allocator (D-890), which `cheby build` links into every executable (D-929).

## Place in the pipeline

`cheby_rt_exe` depends on `cheby_runtime` and the external `mimalloc` crate, and no crate depends on it. Its `src/lib.rs` holds two items and nothing else:

- `#[global_allocator] static GLOBAL: mimalloc::MiMalloc`, outside builds with `--cfg cheby_sanitize`, which keep the system allocator that the sanitizers intercept (D-721, D-875). A staticlib fixes its allocator at its own link, and `cheby_cli` already declares one for the `cheby` binary, so the executables' allocator needs this crate root of its own.
- `pub use cheby_runtime;`, so every symbol the runtime exports is in the archive.

Everything else an executable needs from the runtime lives in `cheby_runtime`, so the runtime ID covers it (D-930): `cheby_rt_main`, the entry the program object's C `main` calls (D-925), the image format and `image::install` (D-919, D-920), and the data symbol `cheby_rt_id$<hex>` that ties the archive to the compiler that links it.

## Building

`cargo build -p cheby_rt_exe --locked` writes `target/debug/libcheby_rt_exe.a`, and `--release` writes `target/release/libcheby_rt_exe.a`, next to the `cheby` binary of the same profile, where `cheby build` looks for it (D-929). The archive has the profile of the toolchain: a debug archive keeps the leak check, poisoning and guard pages (D-926). A plain `cargo test` of a crate does not rebuild it; `cargo build` does.

## Tests

`tests/archive.rs` reads the archive from the profile directory of its own target directory with the `object` crate and checks that every name `cheby_runtime::symbols()` returns, `cheby_rt_main` and `abi::sym::RUNTIME_ID_SYMBOL` are defined global symbols of it (D-932, D-930). A missing or stale archive fails the test with a message that names `cargo build -p cheby_rt_exe`; it never skips (D-929). On Windows it is ignored until M6 (D-935).

## Read more

- [`docs/plan/architecture.md`](../../docs/plan/architecture.md) §1, the repository layout, and §3, the runtime.
- [`crates/cheby_runtime/README.md`](../cheby_runtime/README.md) and [`crates/README.md`](../README.md) for the whole crate graph.
