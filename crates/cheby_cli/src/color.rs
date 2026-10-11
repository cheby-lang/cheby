//! When the CLI colors its output: the global `--color` flag and the
//! environment.
//!
//! Precedence, first match wins:
//!
//! 1. `--color=always` colors and `--color=never` does not, whatever the
//!    environment says. The last `--color` on the command line counts.
//! 2. With `--color=auto` (the default):
//!    1. `NO_COLOR` set and not empty: no color.
//!    2. `CLICOLOR_FORCE` set, not empty and not `0`: color, even when
//!       stderr is not a terminal.
//!    3. `CLICOLOR=0`: no color.
//!    4. `TERM=dumb`: no color.
//!    5. Otherwise: color when stderr is a terminal.
//!
//! The order of the environment rules is the one clap's own terminal
//! detection uses. When the flag or the environment decides, clap's help
//! and usage errors get that decision; otherwise clap checks the stream it
//! writes to (stdout for help) and also needs `TERM` to be set, so with
//! `TERM` unset on a terminal, diagnostics are colored and help is not.

use std::ffi::{OsStr, OsString};

use clap::ValueEnum;

/// The value of `--color`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ColorWhen {
    /// Color on a terminal, unless `NO_COLOR`, `CLICOLOR=0` or `TERM=dumb`
    /// is set; `CLICOLOR_FORCE` colors anyway
    #[default]
    Auto,
    /// Always color
    Always,
    /// Never color
    Never,
}

/// The value of the last valid `--color` before `--`, read before clap
/// parses the command line, so that clap's own help and errors follow it.
/// An invalid value is skipped here; clap reports it.
pub fn scan_flag<I>(args: I) -> Option<ColorWhen>
where
    I: IntoIterator<Item = OsString>,
{
    let mut found = None;
    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        let value = match arg.to_str() {
            Some("--color") => args.next(),
            Some(arg) => match arg.strip_prefix("--color=") {
                Some(value) => Some(OsString::from(value)),
                None => continue,
            },
            None => continue,
        };
        let parsed = value
            .as_deref()
            .and_then(OsStr::to_str)
            .and_then(|value| ColorWhen::from_str(value, false).ok());
        if parsed.is_some() {
            found = parsed;
        }
    }
    found
}

/// The decision made by the flag and the environment alone: `Some` when
/// they settle it, `None` when it depends on whether stderr is a terminal.
pub fn forced<F>(flag: ColorWhen, var: F) -> Option<bool>
where
    F: Fn(&str) -> Option<OsString>,
{
    match flag {
        ColorWhen::Always => return Some(true),
        ColorWhen::Never => return Some(false),
        ColorWhen::Auto => {}
    }
    if var("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return Some(false);
    }
    if var("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return Some(true);
    }
    if var("CLICOLOR").is_some_and(|v| v == "0") {
        return Some(false);
    }
    if var("TERM").is_some_and(|v| v == "dumb") {
        return Some(false);
    }
    None
}

/// The choice passed to clap: the forced decision, or clap's own `Auto`,
/// which checks the stream it writes to (stdout for help, stderr for
/// errors).
pub const fn clap_choice(forced: Option<bool>) -> clap::ColorChoice {
    match forced {
        Some(true) => clap::ColorChoice::Always,
        Some(false) => clap::ColorChoice::Never,
        None => clap::ColorChoice::Auto,
    }
}

/// Whether diagnostics on stderr are colored.
pub fn diagnostics(forced: Option<bool>, stderr_is_terminal: bool) -> cheby_diag::Colors {
    if forced.unwrap_or(stderr_is_terminal) {
        cheby_diag::Colors::On
    } else {
        cheby_diag::Colors::Off
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{ColorWhen, diagnostics, forced, scan_flag};

    fn args(list: &[&str]) -> Vec<OsString> {
        std::iter::once("cheby")
            .chain(list.iter().copied())
            .map(OsString::from)
            .collect()
    }

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn flag_scanning() {
        assert_eq!(scan_flag(args(&["check"])), None);
        assert_eq!(
            scan_flag(args(&["--color=always", "check"])),
            Some(ColorWhen::Always)
        );
        assert_eq!(
            scan_flag(args(&["check", "--color", "never"])),
            Some(ColorWhen::Never)
        );
        assert_eq!(
            scan_flag(args(&["--color=never", "check", "--color=always"])),
            Some(ColorWhen::Always)
        );
        assert_eq!(
            scan_flag(args(&["--color=never", "--color=bogus"])),
            Some(ColorWhen::Never)
        );
        assert_eq!(scan_flag(args(&["run", "--", "--color=always"])), None);
        assert_eq!(scan_flag(args(&["--color"])), None);
    }

    #[test]
    fn precedence() {
        let all = [
            ("NO_COLOR", "1"),
            ("CLICOLOR_FORCE", "1"),
            ("CLICOLOR", "0"),
            ("TERM", "dumb"),
        ];
        // The flag beats the environment.
        assert_eq!(forced(ColorWhen::Always, env(&all)), Some(true));
        assert_eq!(
            forced(ColorWhen::Never, env(&[("CLICOLOR_FORCE", "1")])),
            Some(false)
        );
        // NO_COLOR beats CLICOLOR_FORCE.
        assert_eq!(forced(ColorWhen::Auto, env(&all)), Some(false));
        assert_eq!(
            forced(ColorWhen::Auto, env(&[("NO_COLOR", "")])),
            None,
            "an empty NO_COLOR is ignored"
        );
        // CLICOLOR_FORCE beats CLICOLOR=0 and TERM=dumb.
        assert_eq!(forced(ColorWhen::Auto, env(&all[1..])), Some(true));
        for off in ["", "0"] {
            assert_eq!(
                forced(ColorWhen::Auto, env(&[("CLICOLOR_FORCE", off)])),
                None
            );
        }
        assert_eq!(forced(ColorWhen::Auto, env(&all[2..])), Some(false));
        assert_eq!(forced(ColorWhen::Auto, env(&all[3..])), Some(false));
        assert_eq!(forced(ColorWhen::Auto, env(&[("CLICOLOR", "1")])), None);
        assert_eq!(
            forced(ColorWhen::Auto, env(&[("TERM", "xterm-256color")])),
            None
        );
        assert_eq!(forced(ColorWhen::Auto, env(&[])), None);
    }

    #[test]
    fn terminal_decides_when_nothing_forces() {
        use cheby_diag::Colors;
        assert_eq!(diagnostics(None, true), Colors::On);
        assert_eq!(diagnostics(None, false), Colors::Off);
        assert_eq!(diagnostics(Some(true), false), Colors::On);
        assert_eq!(diagnostics(Some(false), true), Colors::Off);
    }
}
