//! Names the sanitizer's runtime for a `--cfg cheby_sanitize` build
//! (D-1337): `CHEBY_SANITIZER_RUNTIME`, the file in the toolchain that
//! built this crate, which `link::SANITIZER_RUNTIME` reads. Every other
//! build sets nothing, so a shipped `cheby` links as D-933 says.
//!
//! rustc links a sanitizer's runtime into executables and leaves it out of
//! a staticlib, so `libcheby_rt_exe.a` does not hold it, and `cheby build`
//! has to name it. The toolchain's own runtime is the one that matches the
//! instrumentation; a C compiler's `-fsanitize=` would bring another.

use std::env;
use std::error::Error;
use std::path::Path;
use std::process::Command;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo::rerun-if-changed=build.rs");
    let Ok(sanitizer) = env::var("CARGO_CFG_CHEBY_SANITIZE") else {
        return Ok(());
    };
    let short = match sanitizer.as_str() {
        "address" => "asan",
        "thread" => "tsan",
        other => return Err(format!("no runtime known for `cheby_sanitize={other}`").into()),
    };
    let extension = if env::var("CARGO_CFG_TARGET_OS")? == "macos" {
        "dylib"
    } else {
        "a"
    };
    let out = Command::new(env::var("RUSTC")?)
        .args(["--print", "sysroot"])
        .output()?;
    if !out.status.success() {
        return Err("`rustc --print sysroot` failed".into());
    }
    let sysroot = String::from_utf8(out.stdout)?;
    let runtime = Path::new(sysroot.trim())
        .join("lib/rustlib")
        .join(env::var("TARGET")?)
        .join(format!("lib/librustc-nightly_rt.{short}.{extension}"));
    if !runtime.is_file() {
        return Err(format!("{}: no such runtime", runtime.display()).into());
    }
    println!(
        "cargo::rustc-env=CHEBY_SANITIZER_RUNTIME={}",
        runtime.display()
    );
    Ok(())
}
