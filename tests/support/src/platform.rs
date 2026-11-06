//! Platforms on which the harnesses skip the trials that run programs.
//!
//! M1 is judged on Linux and macOS (D-545). On Windows, CI builds the
//! workspace and runs the tests that execute no generated code, and the
//! harnesses skip every trial that builds a JIT until M6, which brings
//! Windows support: the JIT's memory path has never run there and native
//! symbol lookup has no Windows fallback. That is the UI run tests and the
//! UI check tests that pass the front end
//! ([`crate::ui::UiTest::builds_a_jit`]), every conformance trial and the
//! REPL suite. A skipped trial is reported as ignored, never as passed.
//!
//! From M3, D-641's rule extends to executables (D-935): the conformance
//! harness's `@aot` trials and the UI tests with `// @command: build` are
//! ignored on Windows until M6 ([`executable_skip_reason`]).

/// Why trials that run a compiled program are skipped on this platform,
/// or `None` where they run (D-545).
#[must_use]
pub const fn run_skip_reason() -> Option<&'static str> {
    if cfg!(windows) {
        Some("programs do not run on Windows until M6 (D-545)")
    } else {
        None
    }
}

/// Whether trials that run a compiled program are skipped here (D-545).
#[must_use]
pub const fn runs_skipped() -> bool {
    run_skip_reason().is_some()
}

/// Why trials that build or run an executable, the conformance harness's
/// `@aot` trials and the UI tests with `// @command: build`, are skipped
/// on this platform, or `None` where they run (D-935, D-641).
#[must_use]
pub const fn executable_skip_reason() -> Option<&'static str> {
    if cfg!(windows) {
        Some("executables on Windows are M6 (D-935)")
    } else {
        None
    }
}

/// Whether trials that build or run an executable are skipped here
/// (D-935).
#[must_use]
pub const fn executables_skipped() -> bool {
    executable_skip_reason().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_run_except_on_windows() {
        assert_eq!(runs_skipped(), cfg!(windows));
        assert_eq!(run_skip_reason().is_some(), runs_skipped());
        assert_eq!(executables_skipped(), cfg!(windows));
        assert_eq!(executable_skip_reason().is_some(), executables_skipped());
    }
}
