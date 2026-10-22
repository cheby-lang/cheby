//! Target flags and the ISA (E1, D-259, D-913, D-921): the JIT, the cache
//! and executables build their ISA with [`host_isa`], from [`flags`] and
//! one fixed feature set per target.

use cranelift_codegen::ir::LibCall;
use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_codegen::settings::{self, Configurable};

use crate::CodegenError;

/// The shared Cranelift flags: no optimization (D-259), frame pointers kept
/// for stack walking (the deadlock report's walk, D-856, and profilers),
/// and the verifier in debug builds of the compiler.
///
/// The code model is D-913's, one for the JIT, the cache and executables:
///
/// - `is_pic=false`: cranelift-jit refuses PIC, and under PIC Cranelift
///   loads every name through the GOT, colocated ones included. Every
///   generated symbol is referenced colocated, so text reaches it
///   PC-relative, and runtime and foreign symbols are reached through
///   import words `cb$imp$<symbol>` (data with an absolute relocation).
/// - `use_colocated_libcalls=false`: Cranelift libcalls never appear
///   ([`libcall_names`]); one would be an absolute relocation in text,
///   which [`crate::define_with_stats`] rejects.
/// - `unwind_info=false`: Cheby frames unwind by return flags (D-733,
///   ADR-0061), so no unwind tables are produced. It changes no machine
///   code, only the unwind pseudo-instructions Cranelift records.
///
/// # Errors
///
/// A [`CodegenError`] if Cranelift does not know a flag.
pub fn flags() -> Result<settings::Flags, CodegenError> {
    let mut builder = settings::builder();
    let verifier = if cfg!(debug_assertions) {
        "true"
    } else {
        "false"
    };
    for (name, value) in [
        ("opt_level", "none"),
        ("preserve_frame_pointers", "true"),
        ("is_pic", "false"),
        ("use_colocated_libcalls", "false"),
        ("unwind_info", "false"),
        ("enable_verifier", verifier),
    ] {
        builder
            .set(name, value)
            .map_err(|e| CodegenError::ice(format!("Cranelift flag `{name}`: {e}")))?;
    }
    Ok(settings::Flags::new(builder))
}

/// The target triple this build of the compiler generates code for, which
/// is the one it runs on (`None` on a target M3 does not support, macOS on
/// x86-64 included, D-892).
const TARGET_TRIPLE: Option<&str> = if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
    Some("x86_64-unknown-linux-gnu")
} else if cfg!(all(target_arch = "x86_64", target_os = "windows")) {
    Some("x86_64-pc-windows-msvc")
} else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
    Some("aarch64-unknown-linux-gnu")
} else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
    Some("aarch64-apple-darwin")
} else if cfg!(all(target_arch = "aarch64", target_os = "windows")) {
    Some("aarch64-pc-windows-msvc")
} else {
    None
};

/// D-921's x86-64-v2: SSE3, SSSE3, SSE4.1, SSE4.2, POPCNT and CMPXCHG16B
/// (LAHF/SAHF, the rest of the level, has no Cranelift flag).
const X86_64_V2: &[(&str, &str)] = &[
    ("has_sse3", "true"),
    ("has_ssse3", "true"),
    ("has_cmpxchg16b", "true"),
    ("has_sse41", "true"),
    ("has_sse42", "true"),
    ("has_popcnt", "true"),
];

/// D-921's Apple M1 base: LSE atomics, pointer authentication, FP16 and
/// the dot product; no Int8 matrix multiplication (Apple M2 onward).
/// Return-address signing stays off: `has_pauth` only allows non-HINT
/// instructions and turns no signing on, so frames hold plain return
/// addresses, which the deadlock walk still strips with `xpaclri` as a
/// safeguard (D-856).
const AARCH64_APPLE_M1: &[(&str, &str)] = &[
    ("has_lse", "true"),
    ("has_pauth", "true"),
    ("has_fp16", "true"),
    ("has_dotprod", "true"),
    ("sign_return_address", "false"),
    ("sign_return_address_all", "false"),
    ("sign_return_address_with_bkey", "false"),
    ("use_bti", "false"),
];

/// D-921's Armv8.0 for aarch64 Linux: no LSE, no pointer authentication,
/// no FP16, no dot product; return-address signing off.
const AARCH64_V8_0: &[(&str, &str)] = &[
    ("sign_return_address", "false"),
    ("sign_return_address_all", "false"),
    ("sign_return_address_with_bkey", "false"),
    ("use_bti", "false"),
];

/// The targets M3 generates code for, each with its fixed feature set
/// (D-921), as Cranelift ISA flags set on top of their defaults (every ISA
/// flag defaults to off). Every triple here is a valid literal, so
/// Cranelift's lookup never panics on one.
///
/// macOS is aarch64 only (D-892): macOS on x86-64 is not here.
///
/// GAP(isa-windows-targets): the two Windows targets use x86-64-v2 and
/// Armv8.0 until M6 settles their sets (D-935).
const TARGETS: &[(&str, &[(&str, &str)])] = &[
    ("x86_64-unknown-linux-gnu", X86_64_V2),
    ("x86_64-pc-windows-msvc", X86_64_V2),
    ("aarch64-unknown-linux-gnu", AARCH64_V8_0),
    ("aarch64-apple-darwin", AARCH64_APPLE_M1),
    ("aarch64-pc-windows-msvc", AARCH64_V8_0),
];

/// The targets of [`TARGETS`], as the unsupported-host error names them
/// (D-892).
const SUPPORTED: &str =
    "code generation supports x86-64 on Linux and Windows, and aarch64 on Linux, macOS and Windows";

/// The ISA the JIT, the cache and executables compile for (D-921): the
/// target this build of the compiler runs on, as [`isa_for`] builds it.
///
/// # Errors
///
/// [`CodegenError::UnsupportedHost`] on a target outside the five of
/// [`isa_for`] (macOS on x86-64 among them, D-892), and a
/// [`CodegenError`] if Cranelift refuses a flag.
pub fn host_isa() -> Result<OwnedTargetIsa, CodegenError> {
    let triple =
        TARGET_TRIPLE.ok_or_else(|| CodegenError::UnsupportedHost(SUPPORTED.to_owned()))?;
    isa_for(triple)
}

/// The ISA for `triple` (D-921): [`flags`] and the target's one fixed CPU
/// feature set, written here, never the features the host detects. Code
/// compiled on one machine then runs on every machine of the target, and
/// the build ID covers the set because it is in the source. Only the
/// host's architecture is compiled into Cranelift (D-936), so a triple of
/// another architecture is an error here, never a panic.
///
/// GAP(isa-feature-flags): the exact Cranelift ISA flags per target, every
/// other ISA flag at its default (off):
///
/// - x86-64 (Linux, Windows): x86-64-v2, `has_sse3`, `has_ssse3`,
///   `has_cmpxchg16b`, `has_sse41`, `has_sse42`, `has_popcnt`; no AVX,
///   FMA, BMI or LZCNT. macOS is aarch64 only (D-892): macOS on x86-64 is
///   not a target, so its triple is [`CodegenError::UnsupportedHost`].
/// - aarch64 macOS: the Apple M1's base, `has_lse`, `has_pauth`,
///   `has_fp16`, `has_dotprod`; `has_i8mm` off.
/// - aarch64 Linux and Windows: Armv8.0, no flag on.
/// - every aarch64 target: `sign_return_address`, `sign_return_address_all`,
///   `sign_return_address_with_bkey` and `use_bti` off, so return-address
///   signing stays off even where `has_pauth` is on.
///
/// # Errors
///
/// [`CodegenError::UnsupportedHost`] on a triple outside those five, or
/// whose Cranelift backend is not built in, and a [`CodegenError`] if
/// Cranelift refuses a flag.
pub fn isa_for(triple: &str) -> Result<OwnedTargetIsa, CodegenError> {
    let (triple, features) = TARGETS
        .iter()
        .find(|&&(known, _)| known == triple)
        .copied()
        .ok_or_else(|| CodegenError::UnsupportedHost(format!("{triple}: {SUPPORTED}")))?;
    let mut builder = cranelift_codegen::isa::lookup_by_name(triple)
        .map_err(|e| CodegenError::UnsupportedHost(format!("{triple}: {e}")))?;
    for &(name, value) in features {
        builder
            .set(name, value)
            .map_err(|e| CodegenError::ice(format!("Cranelift ISA flag `{name}`: {e}")))?;
    }
    builder
        .finish(flags()?)
        .map_err(|e| CodegenError::UnsupportedHost(format!("{triple}: {e}")))
}

/// The names a module gives Cranelift's libcalls: `cb$libcall$<name>`,
/// which nothing defines or resolves (D-913). Code generation never asks
/// for a libcall, and [`crate::define_with_stats`] rejects a function whose
/// text has one before anything is defined, so a libcall is an internal
/// error, never a silent call into whatever the process names `memcpy`
/// or `ceil`.
pub fn libcall_names() -> Box<dyn Fn(LibCall) -> String + Send + Sync> {
    Box::new(|libcall| format!("{}libcall${libcall}", crate::mangle::PREFIX))
}
