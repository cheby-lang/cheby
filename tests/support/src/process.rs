//! Running a command with optional input, captured output and a timeout.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
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
