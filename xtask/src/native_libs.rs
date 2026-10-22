//! `cargo xtask check-native-libs` (D-933, D-943; GAP(aot-native-libs)):
//! checks the system-library list `cheby_link` records for the host
//! against what Rust says the runtime archive needs, and that the system
//! loader opens every recorded file, so the list never drifts from Rust
//! and E0053's build resolver never misses a library (a false E0053).
//!
//! 1. `cargo rustc -p cheby_rt_exe --lib -- --print native-static-libs`:
//!    the libraries of the note, in order, must be the list's names.
//! 2. On macOS, `rustc --print deployment-target` for the host, without
//!    `MACOSX_DEPLOYMENT_TARGET`, must be the recorded minimum version.
//! 3. Each recorded file opens with `dlopen`.
//!
//! On Windows it prints that executables are M6's (D-935) and passes.

use std::path::Path;
use std::process::Command;

use cheby_link::native_libs;

pub fn run(root: &Path) -> Result<(), String> {
    if cfg!(windows) {
        println!("check-native-libs: executables on Windows are M6 (D-935); nothing to check");
        return Ok(());
    }
    let mut failures: usize = 0;
    let printed = printed_libs(root)?;
    let recorded: Vec<String> = native_libs::native_libs()
        .iter()
        .map(|lib| lib.name.to_owned())
        .collect();
    if printed == recorded {
        println!("ok    native-static-libs: {}", printed.join(" "));
    } else {
        failures = failures.saturating_add(1);
        println!(
            "FAIL  native-static-libs: rustc prints `{}`, `cheby_link::native_libs` records `{}`",
            printed.join(" "),
            recorded.join(" ")
        );
    }
    if cfg!(target_os = "macos") {
        let min = deployment_target()?;
        if native_libs::macos_min_version() == Some(min.as_str()) {
            println!("ok    deployment target: {min}");
        } else {
            failures = failures.saturating_add(1);
            println!(
                "FAIL  deployment target: rustc says {min}, `cheby_link::native_libs` records {:?}",
                native_libs::macos_min_version()
            );
        }
    }
    for lib in native_libs::native_libs() {
        match native_libs::open(lib) {
            Ok(()) => println!("ok    -l{} opens as {}", lib.name, lib.file),
            Err(e) => {
                failures = failures.saturating_add(1);
                println!("FAIL  -l{}: {e}", lib.name);
            }
        }
    }
    match failures {
        0 => {
            println!("check-native-libs: the list matches the archive, and every file opens");
            Ok(())
        }
        n => Err(format!("{n} checks of the native-library list failed")),
    }
}

/// The libraries `rustc` prints for the archive crate, as `-l` names,
/// with a macOS `-framework <name>` kept as `framework=<name>`.
fn printed_libs(root: &Path) -> Result<Vec<String>, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let out = Command::new(cargo)
        .current_dir(root)
        .args([
            "rustc",
            "--package",
            "cheby_rt_exe",
            "--lib",
            "--",
            "--print",
            "native-static-libs",
        ])
        .output()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(format!("cannot build `cheby_rt_exe`:\n{stderr}"));
    }
    let line = stderr
        .lines()
        .find_map(|line| line.split_once("native-static-libs:").map(|(_, libs)| libs))
        .ok_or_else(|| format!("rustc printed no `native-static-libs` note:\n{stderr}"))?;
    let mut libs = Vec::new();
    let mut words = line.split_whitespace();
    while let Some(word) = words.next() {
        if word == "-framework" {
            libs.push(format!("framework={}", words.next().unwrap_or("")));
        } else if let Some(name) = word.strip_prefix("-l") {
            libs.push(name.to_owned());
        } else {
            libs.push(word.to_owned());
        }
    }
    Ok(libs)
}

/// `rustc --print deployment-target`'s version for the host, without
/// `MACOSX_DEPLOYMENT_TARGET`, as the archive is built in a clean
/// environment.
fn deployment_target() -> Result<String, String> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let out = Command::new(rustc)
        .args(["--print", "deployment-target"])
        .env_remove("MACOSX_DEPLOYMENT_TARGET")
        .output()
        .map_err(|e| format!("cannot run rustc: {e}"))?;
    // `<VARIABLE>=<version>`: `MACOSX_DEPLOYMENT_TARGET=11.0` on macOS.
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim()
        .split_once('=')
        .map(|(_, version)| version.to_owned())
        .ok_or_else(|| format!("rustc --print deployment-target printed `{}`", text.trim()))
}
