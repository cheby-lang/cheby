//! `cheby repl` (§13.5, G5, plan §8.3, D-546, D-548).
//!
//! [`run`] starts a [`Session`] in the current directory (inside a
//! package, the package's modules can be imported) and reads entries until
//! the end of input:
//!
//! - The prompt is `> `, and `. ` while a bracket, a block or a string is
//!   open ([`is_open`]); the lines read so far form one entry.
//! - On a terminal, `rustyline` edits the lines, with an in-memory history
//!   and no history file (M1). Otherwise there is no line editing: lines
//!   come from the runtime's one buffered reader of standard input
//!   (`cheby_runtime::io::read_input_line`, D-575), the same one
//!   `io::read_line` uses, so an entry that reads standard input consumes
//!   the lines that follow it, which are not echoed (D-548); every line
//!   read as input is echoed after its prompt, so the output reads as a
//!   transcript.
//! - Standard output is flushed before anything is written to standard
//!   error (D-548), and a failed write ends the REPL with status 1 (D-306).
//!
//! An entry that starts with `:` is a command (§13.5.3, [`HELP`]): `:type
//! expr`, `:module path` (`:module` alone leaves the module), `:reload`,
//! `:help` and `:quit`. An unknown command is a tool error (D-464).

use std::io::{IsTerminal as _, Write as _};
use std::process::ExitCode;

use cheby_diag::Colors;
use cheby_driver::session::{Output, Session, SessionOptions, Start};

/// The prompt of a new entry.
pub const PROMPT: &str = "> ";

/// The prompt of a line that continues an entry.
pub const CONTINUATION: &str = ". ";

/// What `:help` prints (§13.5.3, D-624).
pub const HELP: &str = "\
Enter an expression to see its value and type, `let` or `let assert` to bind names \
for later entries, a declaration (`fn`, `type`, a type alias, `interface`, `const`), \
or an `import`. A name may be defined again (§13.5.2).

Commands:
  :type <expr>     show the type of an expression without evaluating it
  :module <path>   enter a module, so its `priv` items and its imports are in scope
  :module          leave the module entered with `:module`
  :reload          check and compile again the modules of the package that changed
  :help            show this text
  :quit            end the session, as the end of input does
";

/// How `cheby repl` runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplOptions {
    /// `--deny-warnings`: every warning is an error, and an entry with one
    /// is refused (plan §8.4, D-549).
    pub deny_warnings: bool,
}

/// Runs a session until the end of input and returns the process's exit
/// status: 0, or 1 when the package cannot be loaded or output fails.
#[must_use]
#[expect(
    clippy::needless_pass_by_value,
    reason = "plan §4.5 fixes the signature"
)]
pub fn run(options: ReplOptions) -> ExitCode {
    run_colored(&options, Colors::Off)
}

/// [`run`], with diagnostics and tool errors colored as `colors` says: the
/// global `--color` option and the environment decide, as for the other
/// commands (D-467).
#[must_use]
pub fn run_colored(options: &ReplOptions, colors: Colors) -> ExitCode {
    let mut streams = Streams;
    let dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            streams.stderr(&cheby_diag::render_tool_error(
                &format!("cannot read the current directory: {e}"),
                colors,
            ));
            return ExitCode::FAILURE;
        }
    };
    let session_options = SessionOptions {
        deny_warnings: options.deny_warnings,
        colors,
    };
    let (mut session, warnings) = match Session::start(&dir, session_options) {
        Ok(Start::Ready(session, warnings)) => (session, warnings),
        Ok(Start::Failed(diagnostics)) => {
            streams.stderr(&diagnostics);
            return ExitCode::FAILURE;
        }
        Err(e) => {
            streams.stderr(&cheby_diag::render_tool_error(&e.to_string(), colors));
            return ExitCode::FAILURE;
        }
    };
    if !warnings.is_empty() {
        streams.stderr(&warnings);
    }
    if std::io::stdin().is_terminal() {
        interactive(&mut session, &mut streams);
    } else {
        piped(&mut session, &mut streams);
    }
    if cheby_runtime::io::flush_stdout().is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Reads entries from a terminal with line editing.
fn interactive(session: &mut Session, streams: &mut Streams) {
    let Ok(mut editor) = rustyline::DefaultEditor::new() else {
        // No usable terminal after all: read plain lines.
        piped(session, streams);
        return;
    };
    let mut entry = String::new();
    loop {
        let prompt = if entry.is_empty() {
            PROMPT
        } else {
            CONTINUATION
        };
        let line = match editor.readline(prompt) {
            Ok(line) => line,
            // Ctrl-C drops the entry being typed.
            Err(rustyline::error::ReadlineError::Interrupted) => {
                entry.clear();
                continue;
            }
            Err(_) => {
                finish(session, &entry, streams);
                break;
            }
        };
        if !entry.is_empty() {
            entry.push('\n');
        }
        entry.push_str(&line);
        if is_open(&entry) {
            continue;
        }
        let _ = editor.add_history_entry(entry.as_str());
        let text = std::mem::take(&mut entry);
        if !evaluate(session, &text, streams) {
            break;
        }
    }
}

/// Reads entries from standard input that is not a terminal, echoing each
/// line after its prompt.
fn piped(session: &mut Session, streams: &mut Streams) {
    let mut entry = String::new();
    loop {
        let prompt = if entry.is_empty() {
            PROMPT
        } else {
            CONTINUATION
        };
        streams.stdout(prompt);
        let Some(line) = cheby_runtime::io::read_input_line() else {
            streams.stdout("\n");
            finish(session, &entry, streams);
            break;
        };
        streams.stdout(&line);
        streams.stdout("\n");
        if !entry.is_empty() {
            entry.push('\n');
        }
        entry.push_str(&line);
        if is_open(&entry) {
            continue;
        }
        let text = std::mem::take(&mut entry);
        if !evaluate(session, &text, streams) {
            break;
        }
    }
}

/// Runs an entry that the end of input left incomplete, so the checker
/// reports what is missing rather than the entry vanishing silently.
fn finish(session: &mut Session, entry: &str, streams: &mut Streams) {
    if !entry.trim().is_empty() {
        evaluate(session, entry, streams);
    }
}

/// Runs one entry or command; `false` when the session ends.
fn evaluate(session: &mut Session, text: &str, streams: &mut Streams) -> bool {
    if let Some(rest) = text.trim_start().strip_prefix(':') {
        return command(session, rest, streams);
    }
    if let Err(e) = session.eval(text, streams) {
        streams.stderr(&cheby_diag::render_tool_error(
            &e.to_string(),
            session.colors(),
        ));
    }
    true
}

/// A REPL command (§13.5.3), the text after its `:`; `false` when the
/// session ends.
fn command(session: &mut Session, text: &str, streams: &mut Streams) -> bool {
    let text = text.trim();
    let (name, rest) = text
        .split_once(char::is_whitespace)
        .map_or((text, ""), |(name, rest)| (name, rest.trim()));
    let colors = session.colors();
    let result = match name {
        "quit" | "help" | "reload" if !rest.is_empty() => {
            streams.stderr(&cheby_diag::render_tool_error(
                &format!("`:{name}` takes no argument"),
                colors,
            ));
            Ok(())
        }
        "quit" => return false,
        "help" => {
            streams.stdout(HELP);
            Ok(())
        }
        "type" => session.type_of(rest, streams),
        "module" => session.enter_module(rest, streams),
        "reload" => session.reload(streams),
        _ => {
            streams.stderr(&cheby_diag::render_tool_error(
                &format!("there is no command `:{name}`; `:help` lists the commands"),
                colors,
            ));
            Ok(())
        }
    };
    if let Err(e) = result {
        streams.stderr(&cheby_diag::render_tool_error(&e.to_string(), colors));
    }
    true
}

/// Whether `text` leaves a bracket, a block, a string or an interpolation
/// open, or ends with an attribute or a doc comment, which an item must
/// follow, so the entry continues on the next line. `//` comments and the
/// contents of strings do not count, except the code of an interpolation
/// `{…}` inside a string. A closing bracket that matches nothing ends the
/// entry: the checker reports it.
#[must_use]
pub fn is_open(text: &str) -> bool {
    let last = text.lines().rev().map(str::trim).find(|l| !l.is_empty());
    if last.is_some_and(|l| l.starts_with('@') || l.starts_with("///")) && !brackets_open(text) {
        return true;
    }
    brackets_open(text)
}

/// Whether `text` leaves a bracket, a block, a string or an interpolation
/// open. A raw string `r"…"` or `r#"…"#` has no escapes and no
/// interpolation (§2, D-114, D-130).
fn brackets_open(text: &str) -> bool {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Frame {
        Bracket,
        Str,
        Interpolation,
    }
    let chars: Vec<char> = text.chars().collect();
    let mut stack: Vec<Frame> = Vec::new();
    let mut at = 0;
    let mut previous: Option<char> = None;
    while let Some(&c) = chars.get(at) {
        at = at.saturating_add(1);
        if stack.last() == Some(&Frame::Str) {
            match c {
                '\\' => at = at.saturating_add(1),
                '"' => {
                    stack.pop();
                }
                '{' => stack.push(Frame::Interpolation),
                _ => {}
            }
            previous = Some(c);
            continue;
        }
        match c {
            '/' if chars.get(at) == Some(&'/') => {
                while let Some(&next) = chars.get(at) {
                    at = at.saturating_add(1);
                    if next == '\n' {
                        break;
                    }
                }
            }
            'r' if !previous.is_some_and(is_ident_char) => {
                let hashes = chars
                    .get(at..)
                    .map_or(0, |rest| rest.iter().take_while(|&&h| h == '#').count());
                let quote = at.saturating_add(hashes);
                if chars.get(quote) == Some(&'"') {
                    match raw_string_end(&chars, quote.saturating_add(1), hashes) {
                        Some(end) => at = end,
                        None => return true,
                    }
                }
            }
            '"' => stack.push(Frame::Str),
            '(' | '[' | '{' => stack.push(Frame::Bracket),
            ')' | ']' | '}' if stack.pop().is_none() => return false,
            _ => {}
        }
        previous = chars.get(at.wrapping_sub(1)).copied();
    }
    !stack.is_empty()
}

/// The position after the end of a raw string with `hashes` hashes whose
/// contents start at `from`: the first `"` followed by as many `#`.
/// `None` if it does not end in `chars`.
fn raw_string_end(chars: &[char], from: usize, hashes: usize) -> Option<usize> {
    let mut at = from;
    while let Some(&c) = chars.get(at) {
        at = at.saturating_add(1);
        if c == '"'
            && chars
                .get(at..at.saturating_add(hashes))
                .is_some_and(|tail| tail.iter().all(|&h| h == '#'))
        {
            return Some(at.saturating_add(hashes));
        }
    }
    None
}

/// Whether `c` can be part of an identifier, so an `r` after it is not
/// the start of a raw string.
const fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The process's standard streams.
struct Streams;

impl Output for Streams {
    fn stdout(&mut self, text: &str) {
        let mut out = std::io::stdout().lock();
        if out.write_all(text.as_bytes()).is_err() {
            exit_after_write_failure();
        }
    }

    fn stderr(&mut self, text: &str) {
        if cheby_runtime::io::flush_stdout().is_err() {
            exit_after_write_failure();
        }
        if std::io::stderr().lock().write_all(text.as_bytes()).is_err() {
            exit_after_write_failure();
        }
    }
}

/// Ends the REPL with status 1 when it cannot write its output (D-306).
#[expect(
    clippy::exit,
    reason = "D-306: a failed write ends the program at once with status 1"
)]
fn exit_after_write_failure() -> ! {
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::is_open;

    #[test]
    fn entries_continue_while_something_is_open() {
        assert!(!is_open("1 + 2"));
        assert!(is_open("fn f() {"));
        assert!(is_open("list::map([1,"));
        assert!(!is_open("fn f() {\n  1\n}"));
        assert!(is_open("\"text"));
        assert!(!is_open("\"{x}\""));
        assert!(is_open("\"a {f("));
        assert!(!is_open("\"a \\\" b\""));
        assert!(!is_open("f(1) // (unclosed in a comment"));
        assert!(!is_open("1)"));
        assert!(is_open("@external(native, \"f\")"));
        assert!(is_open("/// Docs.\n@target(native)"));
        assert!(!is_open("@external(native, \"f\")\nfn f()"));
    }

    /// Raw strings have no escapes and no interpolation (§2, D-114, D-130).
    #[test]
    fn raw_strings_are_taken_literally() {
        assert!(!is_open(r#"r"C:\""#));
        assert!(!is_open(r#"r"{""#));
        assert!(!is_open("r#\"say \"hi\"\"#"));
        assert!(!is_open(r###"f(r##"a "# b"##)"###));
        assert!(is_open("r#\"open \"quote"));
        assert!(is_open(r#"r"line one"#));
        assert!(is_open(r#"f(r"x")"#.trim_end_matches(')')));
        // An identifier ending in `r` does not start a raw string.
        assert!(!is_open(r#"f(bar, "{x}")"#));
        assert!(is_open(r#"for_r"{"#));
    }
}
