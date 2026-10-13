//! Thread variants of the conformance trials (D-722).
//!
//! Every example's run trial and `::test` trial runs three times, with the
//! same expectation and the same goldens: with `CHEBY_THREADS` unset (the
//! base trial, named as before), with `CHEBY_THREADS=1` and with
//! `CHEBY_THREADS=2`. The variants are named `<name>@threads=N` and
//! `<name>::test@threads=N` (D-747). A `skip-run`
//! example's trials only check it or stay pending, so they get no
//! variants.
//!
//! The child's environment is built in a fixed order ([`child_env`]): the
//! harness's `NO_COLOR=1`, then the example's `env` file, then
//! `CHEBY_RT_POISON=1` (D-544), then the variant's `CHEBY_THREADS`. So
//! neither the example's `env` file nor the developer's shell can change
//! the thread count of a trial: the base trial removes the variable.
//!
//! A `CHEBY_THREADS=N` in the harness's own environment adds the variant
//! `@threads=N` when N is neither 1 nor 2 ([`variants`]), so a run such as
//! `CHEBY_THREADS=4 cargo test -p cheby_cli --test conformance` (the
//! migration stress run) runs every trial at 4 threads as well, instead of
//! ignoring the setting. A value that is not a whole number of at least 1
//! is a harness error, never ignored (D-444).
//!
//! The scheduler threads of the trials compete with each other, so the
//! harness bounds its own parallelism ([`capped_test_threads`]).

use std::ffi::OsString;
use std::process::Command;

use crate::milestone::POISON_ENV;

/// The number of scheduler threads of a program (D-722).
pub const THREADS_ENV: &str = "CHEBY_THREADS";

/// One thread variant of a trial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Variant {
    /// `CHEBY_THREADS` removed: the base trial, which keeps the trial's
    /// plain name.
    Unset,
    /// `CHEBY_THREADS=<n>`.
    Threads(u32),
}

/// The variants every run and `::test` trial runs in, base first (D-722).
pub const VARIANTS: [Variant; 3] = [Variant::Unset, Variant::Threads(1), Variant::Threads(2)];

impl Variant {
    /// Whether this is the base trial, with `CHEBY_THREADS` unset.
    #[must_use]
    pub const fn is_base(self) -> bool {
        matches!(self, Self::Unset)
    }

    /// The name of the variant of the trial `base`: `base` itself for the
    /// unset variant, `base@threads=N` otherwise
    /// (D-747).
    #[must_use]
    pub fn trial_name(self, base: &str) -> String {
        match self {
            Self::Unset => base.to_owned(),
            Self::Threads(n) => format!("{base}@threads={n}"),
        }
    }
}

/// The variants of this run ([`parse_variants`] of the harness's own
/// `CHEBY_THREADS`).
///
/// # Errors
///
/// As [`parse_variants`]. The harness then stops with status 2.
pub fn variants() -> Result<Vec<Variant>, String> {
    parse_variants(std::env::var_os(THREADS_ENV))
}

/// [`VARIANTS`], plus `Threads(n)` when `outer`, the harness's own
/// `CHEBY_THREADS`, is a whole number `n` of at least 1 that is neither 1
/// nor 2, written in decimal digits with surrounding whitespace ignored
/// (D-747). Unset or empty adds nothing, as D-658
/// reads an empty value as no setting.
///
/// # Errors
///
/// Returns a message if `outer` is anything else, so that a thread count
/// the harness cannot honor is never ignored silently (D-444).
pub fn parse_variants(outer: Option<OsString>) -> Result<Vec<Variant>, String> {
    let mut variants = VARIANTS.to_vec();
    let Some(value) = outer else {
        return Ok(variants);
    };
    let text = value
        .into_string()
        .map_err(|v| format!("{THREADS_ENV}: not valid UTF-8: {}", v.display()))?;
    let text = text.trim();
    if text.is_empty() {
        return Ok(variants);
    }
    let invalid = || format!("{THREADS_ENV}: `{text}` is not a whole number of at least 1");
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    match text.parse::<u32>() {
        Ok(0) | Err(_) => return Err(invalid()),
        Ok(n) => {
            let variant = Variant::Threads(n);
            if !variants.contains(&variant) {
                variants.push(variant);
            }
        }
    }
    Ok(variants)
}

/// The thread counts of `variants` for the harness's summary line, such
/// as `unset, 1, 2, 4`.
#[must_use]
pub fn describe(variants: &[Variant]) -> String {
    variants
        .iter()
        .map(|variant| match variant {
            Variant::Unset => "unset".to_owned(),
            Variant::Threads(n) => n.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One change to a child's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvChange {
    Set(String, String),
    Remove(String),
}

/// The changes to a `cheby` child's environment, in the order they are
/// applied: `NO_COLOR=1`, the example's `env` file, `CHEBY_RT_POISON=1`,
/// then `CHEBY_THREADS` set for a variant or removed for the base trial.
/// A later change wins over an earlier one of the same variable.
#[must_use]
pub fn child_env(example_env: &[(String, String)], variant: Variant) -> Vec<EnvChange> {
    let mut changes = vec![EnvChange::Set("NO_COLOR".to_owned(), "1".to_owned())];
    changes.extend(
        example_env
            .iter()
            .map(|(k, v)| EnvChange::Set(k.clone(), v.clone())),
    );
    changes.push(EnvChange::Set(POISON_ENV.to_owned(), "1".to_owned()));
    changes.push(match variant {
        Variant::Unset => EnvChange::Remove(THREADS_ENV.to_owned()),
        Variant::Threads(n) => EnvChange::Set(THREADS_ENV.to_owned(), n.to_string()),
    });
    changes
}

/// Applies `changes` to `cmd`, in order.
pub fn apply(cmd: &mut Command, changes: &[EnvChange]) {
    for change in changes {
        match change {
            EnvChange::Set(key, value) => {
                cmd.env(key, value);
            }
            EnvChange::Remove(key) => {
                cmd.env_remove(key);
            }
        }
    }
}

/// How many trials the conformance harness runs at once: `explicit`, the
/// value of `--test-threads`, when the command line gives one, and
/// otherwise half of `available` (the machine's parallelism), at least 1.
/// `None` for `available` means it is unknown, and the harness then runs
/// one trial at a time (D-722).
#[must_use]
pub fn capped_test_threads(explicit: Option<usize>, available: Option<usize>) -> usize {
    explicit.unwrap_or_else(|| available.map_or(1, |n| n / 2).max(1))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn variant_names() {
        let names: Vec<String> = VARIANTS
            .iter()
            .map(|v| v.trial_name("005_prime_sieve"))
            .collect();
        assert_eq!(
            names,
            [
                "005_prime_sieve",
                "005_prime_sieve@threads=1",
                "005_prime_sieve@threads=2"
            ]
        );
        assert_eq!(
            Variant::Threads(2).trial_name("005_prime_sieve::test"),
            "005_prime_sieve::test@threads=2"
        );
        assert!(Variant::Unset.is_base());
        assert!(!Variant::Threads(1).is_base());
    }

    /// The example's `env` file, then the poison switch, then the thread
    /// count: neither the example nor the shell can change the last two.
    #[test]
    fn the_child_environment_order() {
        let example = vec![
            ("GREETING".to_owned(), "hi".to_owned()),
            (POISON_ENV.to_owned(), "0".to_owned()),
            (THREADS_ENV.to_owned(), "7".to_owned()),
        ];
        let set = |k: &str, v: &str| EnvChange::Set(k.to_owned(), v.to_owned());
        assert_eq!(
            child_env(&example, Variant::Threads(2)),
            [
                set("NO_COLOR", "1"),
                set("GREETING", "hi"),
                set(POISON_ENV, "0"),
                set(THREADS_ENV, "7"),
                set(POISON_ENV, "1"),
                set(THREADS_ENV, "2"),
            ]
        );
        assert_eq!(
            child_env(&[], Variant::Unset),
            [
                set("NO_COLOR", "1"),
                set(POISON_ENV, "1"),
                EnvChange::Remove(THREADS_ENV.to_owned()),
            ]
        );
    }

    /// Applied to a command, the last change of each variable wins, and the
    /// base trial removes `CHEBY_THREADS` even if the example sets it.
    #[test]
    fn applied_changes_end_in_the_variant() {
        let example = vec![
            (POISON_ENV.to_owned(), "0".to_owned()),
            (THREADS_ENV.to_owned(), "7".to_owned()),
        ];
        let final_value = |variant: Variant, key: &str| {
            let mut cmd = Command::new("cheby");
            apply(&mut cmd, &child_env(&example, variant));
            cmd.get_envs()
                .find(|(k, _)| *k == OsStr::new(key))
                .map(|(_, v)| v.map(OsStr::to_owned))
        };
        assert_eq!(
            final_value(Variant::Threads(1), THREADS_ENV),
            Some(Some("1".into()))
        );
        // Removed, not inherited from the harness's environment.
        assert_eq!(final_value(Variant::Unset, THREADS_ENV), Some(None));
        assert_eq!(
            final_value(Variant::Unset, POISON_ENV),
            Some(Some("1".into()))
        );
    }

    /// The harness's own `CHEBY_THREADS` adds a variant instead of being
    /// ignored, and a bad value is an error.
    #[test]
    fn an_outer_thread_count_adds_a_variant() {
        let parse = |value: Option<&str>| parse_variants(value.map(OsString::from));
        assert_eq!(parse(None), Ok(VARIANTS.to_vec()));
        assert_eq!(parse(Some("")), Ok(VARIANTS.to_vec()));
        assert_eq!(parse(Some(" ")), Ok(VARIANTS.to_vec()));
        assert_eq!(parse(Some("1")), Ok(VARIANTS.to_vec()));
        assert_eq!(parse(Some("2")), Ok(VARIANTS.to_vec()));
        let four = parse(Some(" 4\n")).unwrap();
        assert_eq!(
            four,
            [
                Variant::Unset,
                Variant::Threads(1),
                Variant::Threads(2),
                Variant::Threads(4)
            ]
        );
        assert_eq!(describe(&four), "unset, 1, 2, 4");
        assert_eq!(
            Variant::Threads(4).trial_name("005_prime_sieve::test"),
            "005_prime_sieve::test@threads=4"
        );
        for bad in ["0", "four", "-1", "+4", "4.0", "99999999999"] {
            let error = parse(Some(bad)).unwrap_err();
            assert!(error.starts_with("CHEBY_THREADS: "), "{error}");
        }
    }

    #[test]
    fn test_threads_are_capped_unless_explicit() {
        assert_eq!(capped_test_threads(None, Some(8)), 4);
        assert_eq!(capped_test_threads(None, Some(3)), 1);
        assert_eq!(capped_test_threads(None, Some(1)), 1);
        assert_eq!(capped_test_threads(None, Some(0)), 1);
        assert_eq!(capped_test_threads(None, None), 1);
        assert_eq!(capped_test_threads(Some(16), Some(8)), 16);
        assert_eq!(capped_test_threads(Some(1), Some(8)), 1);
    }
}
