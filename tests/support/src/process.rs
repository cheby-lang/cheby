//! Running a command with optional input, captured output and a timeout.
//!
//! The harnesses' timeouts are multiplied by `CHEBY_HARNESS_SCALE`
//! ([`scaled_timeout`], D-722), for slow machines, emulators and
//! sanitizer builds.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Multiplies the harnesses' timeouts (D-722).
pub const HARNESS_SCALE_ENV: &str = "CHEBY_HARNESS_SCALE";

/// The largest `CHEBY_HARNESS_SCALE` (D-748).
pub const MAX_HARNESS_SCALE: u32 = 100;

/// The factor `CHEBY_HARNESS_SCALE` sets ([`parse_harness_scale`]).
///
/// # Errors
///
/// Returns a message if the variable is set to anything but a whole number
/// from 1 to 100. The harness then stops with status 2, as for the other
/// harness variables (D-567).
pub fn harness_scale() -> Result<u32, String> {
    parse_harness_scale(std::env::var_os(HARNESS_SCALE_ENV))
}

/// [`harness_scale`] with the variable's value passed in: a whole number
/// from 1 to 100, written in decimal digits, with surrounding whitespace
/// ignored. Unset or empty is 1, as an empty value of the other harness
/// variables is no setting (D-567). Anything else is an error
/// (D-748).
///
/// # Errors
///
/// As [`harness_scale`].
pub fn parse_harness_scale(value: Option<OsString>) -> Result<u32, String> {
    let Some(value) = value else {
        return Ok(1);
    };
    let text = value
        .into_string()
        .map_err(|v| format!("{HARNESS_SCALE_ENV}: not valid UTF-8: {}", v.display()))?;
    let text = text.trim();
    if text.is_empty() {
        return Ok(1);
    }
    let invalid = || {
        format!("{HARNESS_SCALE_ENV}: `{text}` is not a whole number from 1 to {MAX_HARNESS_SCALE}")
    };
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    match text.parse::<u32>() {
        Ok(n) if (1..=MAX_HARNESS_SCALE).contains(&n) => Ok(n),
        _ => Err(invalid()),
    }
}

/// `base` multiplied by `CHEBY_HARNESS_SCALE` ([`harness_scale`]).
///
/// # Errors
///
/// As [`harness_scale`].
pub fn scaled_timeout(base: Duration) -> Result<Duration, String> {
    harness_scale().map(|scale| scale_timeout(base, scale))
}

/// `base` multiplied by `scale`, saturating.
#[must_use]
pub fn scale_timeout(base: Duration, scale: u32) -> Duration {
    base.saturating_mul(scale)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    /// The exit code, or `None` if the process was killed by a signal or
    /// by the timeout.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl RunOutput {
    /// Describes how the process ended, for messages.
    #[must_use]
    pub fn describe_status(&self) -> String {
        match (self.timed_out, self.status) {
            (true, _) => "timed out".to_owned(),
            (false, Some(code)) => format!("exit code {code}"),
            (false, None) => "killed by a signal".to_owned(),
        }
    }
}

/// Runs `cmd`, feeding it `stdin` (or nothing), and kills it after
/// `timeout`. Output is decoded lossily as UTF-8 with `\r\n` normalized to
/// `\n`, so goldens are the same on every platform.
///
/// # Errors
///
/// Returns a message if the process cannot be started or waited for.
pub fn run(mut cmd: Command, stdin: Option<&[u8]>, timeout: Duration) -> Result<RunOutput, String> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {cmd:?}: {e}"))?;

    let writer = match (stdin, child.stdin.take()) {
        (Some(input), Some(mut pipe)) => {
            let input = input.to_vec();
            // A program may exit without reading its input, so a broken
            // pipe is not an error.
            Some(thread::spawn(move || {
                let _ = pipe.write_all(&input);
            }))
        }
        (Some(_), None) => return Err("stdin of the child is not piped".to_owned()),
        (None, _) => None,
    };
    let stdout = read_in_background(child.stdout.take().ok_or("stdout is not piped")?);
    let stderr = read_in_background(child.stderr.take().ok_or("stderr is not piped")?);

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if start.elapsed() > timeout => {
                timed_out = true;
                let _ = child.kill();
                break child.wait().map_err(|e| e.to_string())?;
            }
            None => thread::sleep(Duration::from_millis(5)),
        }
    };

    if let Some(writer) = writer {
        let _ = writer.join();
    }
    Ok(RunOutput {
        status: if timed_out { None } else { status.code() },
        stdout: normalize(&stdout.join().unwrap_or_default()),
        stderr: normalize(&stderr.join().unwrap_or_default()),
        timed_out,
    })
}

fn read_in_background(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// Decodes output and normalizes line endings.
#[must_use]
pub fn normalize(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scale(value: &str) -> Result<u32, String> {
        parse_harness_scale(Some(value.into()))
    }

    #[test]
    fn the_scale_defaults_to_one() {
        assert_eq!(parse_harness_scale(None), Ok(1));
        assert_eq!(scale(""), Ok(1));
        assert_eq!(scale("  "), Ok(1));
    }

    #[test]
    fn the_scale_is_a_whole_number_from_1_to_100() {
        assert_eq!(scale("1"), Ok(1));
        assert_eq!(scale("4"), Ok(4));
        assert_eq!(scale(" 10\n"), Ok(10));
        assert_eq!(scale("100"), Ok(100));
        assert_eq!(scale("007"), Ok(7));
        for bad in ["0", "101", "-1", "+2", "1.5", "2x", "four", "99999999999"] {
            let err = scale(bad).unwrap_err();
            assert!(
                err.starts_with("CHEBY_HARNESS_SCALE: ") && err.contains("1 to 100"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn timeouts_are_multiplied() {
        let base = Duration::from_secs(60);
        assert_eq!(scale_timeout(base, 1), base);
        assert_eq!(scale_timeout(base, 3), Duration::from_secs(180));
        assert_eq!(
            scale_timeout(Duration::from_secs(180), 100),
            Duration::from_secs(18_000)
        );
        assert_eq!(scale_timeout(Duration::MAX, 2), Duration::MAX);
    }
}
