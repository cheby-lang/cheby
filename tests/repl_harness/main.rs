//! REPL transcript harness: runs every transcript under `tests/repl/`
//! through the real `cheby repl` (plan §8.3, D-546, D-548).
//!
//! A transcript `tests/repl/<name>.repl` is what a piped session prints:
//!
//! - a line that starts with `> ` or `. ` is an input line after its
//!   prompt, which the REPL echoes;
//! - a line that starts with `< ` is a line of standard input that an
//!   entry reads (`io::read_line`): it is fed after the input line before
//!   it and is not echoed, so it is not part of the output
//!   (GAP(repl-transcript-stdin));
//! - every other line is expected output, standard output and standard
//!   error merged in the order of writes: the harness gives `cheby repl`
//!   one pipe for both streams.
//!
//! The session ends at the end of input; the final prompt it prints then
//! is not part of the transcript. `cheby repl` must exit with 0. Without a
//! package, the session runs in an empty directory; with a directory
//! `tests/repl/<name>/` that holds a `cheby.toml`, it runs inside that
//! package.
//!
//! `CHEBY_BLESS=1` rewrites each transcript with the actual output, its
//! `< ` lines kept after the same input lines. Every `cheby` runs with
//! `CHEBY_RT_POISON=1`, like the other harnesses (plan §4.8).

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use cheby_test_support::milestone::POISON_ENV;
use cheby_test_support::{bless_enabled, files_with_extension, golden, process, repo_root};
use libtest_mimic::{Arguments, Failed, Trial};

const CHEBY: &str = env!("CARGO_BIN_EXE_cheby");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const TIMEOUT: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    let args = Arguments::from_args();
    let dir = repo_root().join("tests").join("repl");
    let transcripts = match files_with_extension(&dir, "repl") {
        Ok(files) => files,
        Err(e) => {
            eprintln!("REPL harness: {}: {e}", dir.display());
            return ExitCode::from(2);
        }
    };
    let bless = bless_enabled();
    let trials = transcripts
        .into_iter()
        .map(|path| {
            let name = path
                .strip_prefix(&dir)
                .unwrap_or(&path)
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");
            Trial::test(name, move || {
                run_transcript(&path, bless).map_err(Failed::from)
            })
        })
        .collect();
    libtest_mimic::run(&args, trials).exit_code()
}

/// One transcript, read.
#[derive(Debug, Default)]
struct Transcript {
    /// What is fed to standard input: the input lines and the `< ` lines,
    /// in order, each ending with a newline.
    input: String,
    /// The expected output: every line but the `< ` lines.
    expected: String,
    /// Each `< ` line with the number of input lines before it, for
    /// blessing.
    stdin_lines: Vec<(usize, String)>,
}

fn parse(text: &str) -> Transcript {
    let mut t = Transcript::default();
    let mut inputs = 0_usize;
    for line in text.split_inclusive('\n') {
        let bare = line.strip_suffix('\n').unwrap_or(line);
        if let Some(rest) = bare.strip_prefix("< ") {
            t.input.push_str(rest);
            t.input.push('\n');
            t.stdin_lines.push((inputs, rest.to_owned()));
            continue;
        }
        if let Some(rest) = bare.strip_prefix("> ").or_else(|| bare.strip_prefix(". ")) {
            t.input.push_str(rest);
            t.input.push('\n');
            inputs = inputs.saturating_add(1);
        }
        t.expected.push_str(bare);
        t.expected.push('\n');
    }
    t
}

fn run_transcript(path: &Path, bless: bool) -> Result<(), String> {
    let text = golden::read(path)?.ok_or_else(|| format!("{} is missing", path.display()))?;
    let transcript = parse(&text);
    let package = path.with_extension("");
    let (cwd, scratch) = if package.join("cheby.toml").is_file() {
        (package, None)
    } else {
        let scratch = scratch_dir(path)?;
        (scratch.clone(), Some(scratch))
    };
    let result = run_repl(&cwd, &transcript.input);
    if let Some(scratch) = scratch {
        let _ = std::fs::remove_dir_all(scratch);
    }
    let (status, output) = result?;
    // The prompt printed when the input ended.
    let output = output
        .strip_suffix("> \n")
        .map_or_else(|| output.clone(), str::to_owned);
    if bless {
        let blessed = with_stdin_lines(&output, &transcript.stdin_lines);
        golden::write(path, &blessed, true)?;
    } else {
        golden::compare("the transcript", &transcript.expected, &output)?;
    }
    if status != Some(0) {
        return Err(format!("`cheby repl` ended with status {status:?}"));
    }
    Ok(())
}

/// `output` with each `< ` line put back after the same number of input
/// lines.
fn with_stdin_lines(output: &str, stdin_lines: &[(usize, String)]) -> String {
    let mut out = String::new();
    let mut next = 0_usize;
    put_stdin_lines(&mut out, stdin_lines, &mut next, 0);
    let mut inputs = 0_usize;
    for line in output.split_inclusive('\n') {
        out.push_str(line);
        if line.starts_with("> ") || line.starts_with(". ") {
            inputs = inputs.saturating_add(1);
            put_stdin_lines(&mut out, stdin_lines, &mut next, inputs);
        }
    }
    put_stdin_lines(&mut out, stdin_lines, &mut next, usize::MAX);
    out
}

/// Writes the `< ` lines from `next` on that come at most `inputs` input
/// lines in.
fn put_stdin_lines(out: &mut String, lines: &[(usize, String)], next: &mut usize, inputs: usize) {
    while let Some((at, line)) = lines.get(*next)
        && *at <= inputs
    {
        out.push_str("< ");
        out.push_str(line);
        out.push('\n');
        *next = next.saturating_add(1);
    }
}

/// An empty directory outside any package for one transcript. The path
/// holds the process id, so concurrent runs of the harness never share
/// one, as in the UI harness.
fn scratch_dir(path: &Path) -> Result<PathBuf, String> {
    let name = path
        .file_stem()
        .map_or_else(|| "session".into(), |s| s.to_string_lossy().into_owned());
    let dir = Path::new(TMP)
        .join(format!("repl-{}", std::process::id()))
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// Runs `cheby repl` in `cwd` with `input` on standard input and one pipe
/// for standard output and standard error; returns its status and output.
fn run_repl(cwd: &Path, input: &str) -> Result<(Option<i32>, String), String> {
    let (mut reader, writer) = std::io::pipe().map_err(|e| format!("cannot make a pipe: {e}"))?;
    let writer_err = writer
        .try_clone()
        .map_err(|e| format!("cannot clone a pipe: {e}"))?;
    let mut cmd = Command::new(CHEBY);
    cmd.args(["--color=never", "repl"])
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .env(POISON_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(writer)
        .stderr(writer_err);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {cmd:?}: {e}"))?;
    // The command holds the pipe's write ends; the reader sees the end of
    // the output once the child closes them.
    drop(cmd);
    let mut stdin = child.stdin.take().ok_or("stdin is not piped")?;
    let input = input.as_bytes().to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let collector = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status.code(),
            None if start.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("`cheby repl` timed out".to_owned());
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let _ = feeder.join();
    let output = collector.join().unwrap_or_default();
    Ok((status, process::normalize(&output)))
}
