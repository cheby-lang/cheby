//! The `@aot` trial of the conformance harness (D-1025) and the build
//! tests of the UI harness (D-1026): build once with `cheby build`, run
//! the executable, and compare with the goldens of `cheby run`.
//!
//! Every example that is run, not `skip-run`, gets one trial
//! `<name>@aot` ([`trial_name`]). In the example's directory it runs
//! `cheby --color=never build -o <tmp>/<name>`, with the base trial's
//! child environment, into a fresh temporary directory per trial
//! ([`crate::cache::FreshDir`]), so no trial writes into `docs/examples/`.
//! A failed build fails the trial with the build's status and standard
//! error, and nothing runs. Otherwise it runs the executable with the
//! example's `args`, `stdin` and `env` and the example's directory as its
//! working directory (D-415), once per thread variant of the run trials
//! ([`variants`]: `CHEBY_THREADS` unset, 1, 2 and the harness's own extra
//! count, D-722), never with naive counting: `@rc=naive` and the `::test`
//! trials stay on the JIT. Each run is compared with the example's goldens,
//! standard error as the build's followed by the executable's, since
//! `cheby run` prints the build's warnings before the program's output
//! (022's W0009); standard output likewise, where the build, which prints
//! nothing on success (D-937), adds nothing. The first mismatch is
//! reported with its variant ([`check`]).
//!
//! The trial's milestone is M3 for an example of M3 or earlier and the
//! example's own otherwise ([`milestone`]). It runs in the serial group of
//! a `timing` example (D-725, D-888), is never blessed and is ignored under
//! `CHEBY_BLESS=1` (D-853), and is ignored on Windows until M6 (D-935). It
//! never skips because the runtime archive is missing (D-929): a missing
//! or stale archive is `cheby build`'s tool error, which is the trial's
//! result, so `cargo build -p cheby_rt_exe` comes before the harnesses.
//!
//! The logic here takes the build and the runs as closures, so it is
//! tested without `cheby`; the harnesses start the processes.

use crate::milestone::Milestone;
use crate::process::RunOutput;
use crate::threads::Variant;

/// What the trial's name adds to the example's.
pub const SUFFIX: &str = "@aot";

/// The milestone from which every example also runs as an executable.
pub const FIRST_MILESTONE: Milestone = Milestone::M3;

/// The trial's name: `<example>@aot`.
#[must_use]
pub fn trial_name(example: &str) -> String {
    format!("{example}{SUFFIX}")
}

/// The trial's milestone: M3 for an example of M3 or earlier, the
/// example's own otherwise (D-1025).
#[must_use]
pub fn milestone(example: Milestone) -> Milestone {
    example.max(FIRST_MILESTONE)
}

/// The variants the executable runs in: the thread variants of the run
/// trials, in their order, without naive counting, which stays on the
/// JIT (D-1025).
#[must_use]
pub fn variants(run_variants: &[Variant]) -> Vec<Variant> {
    run_variants
        .iter()
        .copied()
        .filter(|variant| !matches!(variant, Variant::RcNaive))
        .collect()
}

/// The variant in messages: `CHEBY_THREADS unset` or `CHEBY_THREADS=N`.
#[must_use]
pub fn describe_variant(variant: Variant) -> String {
    match variant {
        Variant::Unset => "CHEBY_THREADS unset".to_owned(),
        Variant::Threads(n) => format!("CHEBY_THREADS={n}"),
        Variant::RcNaive => "CHEBY_RC=naive".to_owned(),
    }
}

/// The goldens a run is compared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expected<'a> {
    pub stdout: &'a str,
    pub stderr: &'a str,
    pub exit: i32,
}

/// What the build and one run of the executable printed together: the
/// run's status, and each stream as the build's followed by the run's.
///
/// GAP(aot-build-stdout): D-1025 names standard error; standard output is
/// joined the same way, so a build that prints anything on success, which
/// D-937 rules out, shows as a mismatch instead of passing unseen.
#[must_use]
pub fn after_build(build: &RunOutput, run: &RunOutput) -> RunOutput {
    RunOutput {
        status: run.status,
        stdout: format!("{}{}", build.stdout, run.stdout),
        stderr: format!("{}{}", build.stderr, run.stderr),
        timed_out: run.timed_out,
    }
}

/// Whether the build succeeded: it exited with 0 before the timeout.
#[must_use]
pub fn built(build: &RunOutput) -> bool {
    !build.timed_out && build.status == Some(0)
}

/// Compares `actual` with `expected`: the exit code, then standard output,
/// then standard error.
///
/// # Errors
///
/// Describes the first difference.
pub fn compare(expected: &Expected<'_>, actual: &RunOutput) -> Result<(), String> {
    if actual.timed_out || actual.status != Some(expected.exit) {
        return Err(format!(
            "expected exit code {}, got {}\nstderr:\n{}",
            expected.exit,
            actual.describe_status(),
            actual.stderr
        ));
    }
    crate::golden::compare("stdout", expected.stdout, &actual.stdout)?;
    crate::golden::compare("stderr", expected.stderr, &actual.stderr)
}

/// The `@aot` trial: `build` once, then `run` the executable at each of
/// `variants`, in order, comparing each with `expected` ([`after_build`]).
/// A failed build runs nothing.
///
/// # Errors
///
/// Returns the build's status and standard error if it failed or could
/// not start, or the first mismatch, or the first run that could not
/// start, with its variant.
pub fn check<B, R>(
    expected: &Expected<'_>,
    variants: &[Variant],
    build: B,
    mut run: R,
) -> Result<(), String>
where
    B: FnOnce() -> Result<RunOutput, String>,
    R: FnMut(Variant) -> Result<RunOutput, String>,
{
    let build = build().map_err(|e| format!("`cheby build` did not run: {e}"))?;
    if !built(&build) {
        return Err(format!(
            "`cheby build` failed ({}), so the executable was not run\nstdout:\n{}\nstderr:\n{}",
            build.describe_status(),
            build.stdout,
            build.stderr
        ));
    }
    for &variant in variants {
        let label = describe_variant(variant);
        let out = run(variant).map_err(|e| format!("the executable with {label}: {e}"))?;
        compare(expected, &after_build(&build, &out))
            .map_err(|e| format!("the executable with {label}: {e}"))?;
    }
    Ok(())
}

/// A UI build test's result (D-1026): `build`, then, if it succeeded,
/// `run` the executable once, and what both printed ([`after_build`]). A
/// failed build is the result itself, with its status and output, so a
/// test can pin an error that `cheby build` reports, such as E0053.
///
/// # Errors
///
/// Returns a message if the build or the executable could not start.
pub fn build_then_run<B, R>(build: B, run: R) -> Result<RunOutput, String>
where
    B: FnOnce() -> Result<RunOutput, String>,
    R: FnOnce() -> Result<RunOutput, String>,
{
    let build = build().map_err(|e| format!("`cheby build` did not run: {e}"))?;
    if !built(&build) {
        return Ok(build);
    }
    let out = run().map_err(|e| format!("the executable did not run: {e}"))?;
    Ok(after_build(&build, &out))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::threads::{VARIANTS, parse_variants, with_rc_naive};

    fn output(status: i32, stdout: &str, stderr: &str) -> RunOutput {
        RunOutput {
            status: Some(status),
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            timed_out: false,
        }
    }

    const HELLO: Expected<'static> = Expected {
        stdout: "Hello, world!\n",
        stderr: "",
        exit: 0,
    };

    #[test]
    fn aot_names_and_milestones() {
        assert_eq!(trial_name("001_hello_world"), "001_hello_world@aot");
        assert_eq!(milestone(Milestone::M1), Milestone::M3);
        assert_eq!(milestone(Milestone::M2), Milestone::M3);
        assert_eq!(milestone(Milestone::M3), Milestone::M3);
        assert_eq!(milestone(Milestone::M5), Milestone::M5);
    }

    /// Every thread variant of the run trials, in order, and never naive
    /// counting.
    #[test]
    fn aot_variants_are_the_thread_variants() {
        let all = with_rc_naive(VARIANTS.to_vec());
        assert_eq!(variants(&all), VARIANTS);
        let four = with_rc_naive(parse_variants(Some("4".into())).unwrap());
        assert_eq!(
            variants(&four),
            [
                Variant::Unset,
                Variant::Threads(1),
                Variant::Threads(2),
                Variant::Threads(4)
            ]
        );
        assert_eq!(describe_variant(Variant::Unset), "CHEBY_THREADS unset");
        assert_eq!(describe_variant(Variant::Threads(4)), "CHEBY_THREADS=4");
    }

    /// The executable runs once per variant, in order, after one build.
    #[test]
    fn aot_the_variant_loop() {
        let builds = RefCell::new(0);
        let mut ran = Vec::new();
        let result = check(
            &HELLO,
            &[Variant::Unset, Variant::Threads(1), Variant::Threads(2)],
            || {
                *builds.borrow_mut() += 1;
                Ok(output(0, "", ""))
            },
            |variant| {
                ran.push(variant);
                Ok(output(0, "Hello, world!\n", ""))
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(*builds.borrow(), 1);
        assert_eq!(
            ran,
            [Variant::Unset, Variant::Threads(1), Variant::Threads(2)]
        );
    }

    /// The build's standard error comes first, then the executable's, as
    /// `cheby run` prints a warning before the program's output (022).
    #[test]
    fn aot_standard_error_is_the_builds_then_the_executables() {
        let warning = "warning[W0009]: unused\n";
        let expected = Expected {
            stdout: "ok\n",
            stderr: "warning[W0009]: unused\nprogram says\n",
            exit: 3,
        };
        let build = || Ok(output(0, "", warning));
        let run = |_| Ok(output(3, "ok\n", "program says\n"));
        assert_eq!(check(&expected, &VARIANTS, build, run), Ok(()));

        // The executable's standard error alone is not enough.
        let err = check(
            &Expected {
                stderr: "program says\n",
                ..expected
            },
            &[Variant::Unset],
            build,
            run,
        )
        .unwrap_err();
        assert!(err.contains("stderr differs at line 1"), "{err}");

        let combined = after_build(&output(0, "b\n", "w\n"), &output(7, "r\n", "e\n"));
        assert_eq!(combined, output(7, "b\nr\n", "w\ne\n"));
    }

    /// The first mismatch stops the loop and names its variant.
    #[test]
    fn aot_the_first_mismatch_names_its_variant() {
        let mut ran = Vec::new();
        let err = check(
            &HELLO,
            &[Variant::Unset, Variant::Threads(1), Variant::Threads(2)],
            || Ok(output(0, "", "")),
            |variant| {
                ran.push(variant);
                Ok(match variant {
                    Variant::Threads(1) => output(0, "Hello, wrld!\n", ""),
                    Variant::Threads(2) => output(101, "", "panic\n"),
                    _ => output(0, "Hello, world!\n", ""),
                })
            },
        )
        .unwrap_err();
        assert!(
            err.starts_with("the executable with CHEBY_THREADS=1: stdout differs at line 1"),
            "{err}"
        );
        assert_eq!(ran, [Variant::Unset, Variant::Threads(1)]);

        let err = check(
            &HELLO,
            &[Variant::Unset, Variant::Threads(2)],
            || Ok(output(0, "", "")),
            |variant| {
                Ok(match variant {
                    Variant::Threads(2) => output(101, "", "panic\n"),
                    _ => output(0, "Hello, world!\n", ""),
                })
            },
        )
        .unwrap_err();
        assert!(
            err.starts_with(
                "the executable with CHEBY_THREADS=2: expected exit code 0, got exit code 101"
            ),
            "{err}"
        );

        // A run that times out is a mismatch, whatever it printed.
        let err = check(
            &HELLO,
            &[Variant::Unset],
            || Ok(output(0, "", "")),
            |_| {
                Ok(RunOutput {
                    timed_out: true,
                    status: None,
                    ..output(0, "Hello, world!\n", "")
                })
            },
        )
        .unwrap_err();
        assert!(err.contains("got timed out"), "{err}");

        // A run that cannot start names its variant too.
        let err = check(
            &HELLO,
            &[Variant::Threads(2)],
            || Ok(output(0, "", "")),
            |_| Err("cannot start".to_owned()),
        )
        .unwrap_err();
        assert_eq!(err, "the executable with CHEBY_THREADS=2: cannot start");
    }

    /// A failed build fails the trial with its status and standard error,
    /// and runs nothing: today's `cheby build -o`, which clap rejects with
    /// status 2, is such a build.
    #[test]
    fn aot_a_failed_build_runs_nothing() {
        let mut ran = 0;
        let err = check(
            &HELLO,
            &VARIANTS,
            || Ok(output(2, "", "error: unexpected argument '-o' found\n")),
            |_| {
                ran += 1;
                Ok(output(0, "Hello, world!\n", ""))
            },
        )
        .unwrap_err();
        assert_eq!(ran, 0);
        assert!(
            err.starts_with("`cheby build` failed (exit code 2), so the executable was not run"),
            "{err}"
        );
        assert!(err.contains("unexpected argument '-o'"), "{err}");

        // A build that timed out or could not start fails it as well.
        let timed_out = RunOutput {
            timed_out: true,
            status: None,
            ..output(0, "", "")
        };
        let err = check(
            &HELLO,
            &VARIANTS,
            || Ok(timed_out),
            |_| {
                ran += 1;
                Err("ran".to_owned())
            },
        )
        .unwrap_err();
        assert!(err.contains("failed (timed out)"), "{err}");
        let err = check(
            &HELLO,
            &VARIANTS,
            || Err("no such file".to_owned()),
            |_| {
                ran += 1;
                Err("ran".to_owned())
            },
        )
        .unwrap_err();
        assert_eq!(err, "`cheby build` did not run: no such file");
        assert_eq!(ran, 0);
    }

    /// A UI build test: a failed build is the result, and a built one
    /// runs once.
    #[test]
    fn aot_build_then_run_for_ui_tests() {
        let failed = build_then_run(
            || Ok(output(1, "", "error[E0053]: missing\n")),
            || Err("ran".to_owned()),
        );
        assert_eq!(failed, Ok(output(1, "", "error[E0053]: missing\n")));
        let ran = build_then_run(
            || Ok(output(0, "", "warning\n")),
            || Ok(output(0, "hi\n", "")),
        );
        assert_eq!(ran, Ok(output(0, "hi\n", "warning\n")));
    }
}
