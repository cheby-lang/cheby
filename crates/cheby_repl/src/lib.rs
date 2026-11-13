//! `cheby repl` (§13.5, G5, plan §8.3, D-546, D-548).
//!
//! [`run`] starts a [`Session`] in the current directory (inside a
//! package, the package's modules can be imported) and reads entries until
//! the end of input:
//!
//! - The prompt is `> `, and `. ` while a bracket, a block or a string is
//!   open ([`is_open`]); the lines read so far form one entry (D-605). A
//!   closing bracket closes the innermost open bracket of its own kind, as
//!   the parser matches them, and one that matches nothing ends the entry
//!   (D-1532). Brackets the parser skips, in a `/*` error token or an
//!   interpolation, do not count (D-1541), and a `/*` with no `*/` on its
//!   line skips only the rest of that line (D-1543).
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
//! - Entries run on the session's runtime, and the detached fibers they
//!   start keep running between entries (D-194, D-712). A termination
//!   while no entry runs, such as a detached fiber's failed write, ends the
//!   REPL before the next prompt with its status, printing nothing more
//!   (D-709, D-828). At the end of input
//!   or `:quit`, the session ends: its detached fibers are torn down and
//!   its runtime stops ([`Session::end`]).
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
    run_with(options, colors, None)
}

/// [`run_colored`] with the build cache of the compiler whose build ID is
/// `build` (D-957, D-959): std's code is loaded from std's records, which
/// std's first use writes, and nothing of the session is stored (K6,
/// D-971, D-980, D-964). The cache is `CHEBY_CACHE` or the OS default, its
/// lookups logged to `CHEBY_CACHE_LOG` (D-973); a fallback to a temporary
/// cache is noted first, and a relative `CHEBY_CACHE` or an unsafe root is
/// a tool error, as for the other commands.
#[must_use]
pub fn run_cached(
    options: &ReplOptions,
    colors: Colors,
    build: cheby_driver::cache::BuildId,
) -> ExitCode {
    use cheby_driver::cache::{CacheEnv, CacheLog};
    let log = CacheLog::from_var(std::env::var_os("CHEBY_CACHE_LOG"));
    match cheby_driver::Cache::open(&CacheEnv::from_process(), build, log) {
        Ok((cache, fallback)) => {
            // The note `cheby_cli` prints for its other commands (D-959).
            if let Some(fallback) = fallback {
                Streams.stderr(&format!(
                    "note: the build cache cannot be used ({fallback}), so this command uses a \
                     temporary cache that is deleted when it ends; set CHEBY_CACHE to a writable \
                     directory to keep the cache between commands\n"
                ));
            }
            run_with(options, colors, Some(cache))
        }
        Err(e) => {
            Streams.stderr(&cheby_diag::render_tool_error(&e.to_string(), colors));
            ExitCode::FAILURE
        }
    }
}

/// [`run_colored`], with std's code from `cache` when one is given.
fn run_with(options: &ReplOptions, colors: Colors, cache: Option<cheby_driver::Cache>) -> ExitCode {
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
        threads: None,
    };
    let (mut session, warnings) = match Session::start_cached(&dir, session_options, cache) {
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
    // The session's end tears its detached fibers down and stops its
    // runtime (§13.5.4, D-712); a termination before or during it ends
    // the process with its status, printing nothing more (D-709, D-797).
    if let Some(status) = session.end() {
        exit_with(status);
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
        session.exit_if_terminated();
        let prompt = if entry.is_empty() {
            PROMPT
        } else {
            CONTINUATION
        };
        let read = editor.readline(prompt);
        session.exit_if_terminated();
        let line = match read {
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
        // A termination while no entry ran (a detached fiber's failed
        // write, D-306, D-709) ends the session before the next prompt
        // (D-828).
        session.exit_if_terminated();
        let prompt = if entry.is_empty() {
            PROMPT
        } else {
            CONTINUATION
        };
        streams.stdout(prompt);
        // A flush that fails before the read ends the REPL with status 1,
        // printing nothing (D-306, spec §13.5): this host thread runs no
        // entry, so the runtime hands the failure back
        // (D-801, D-709). The read shares standard input with the
        // session's detached fibers that read it, which take lines in the
        // order they ask (D-548, D-829).
        let Ok(read) = cheby_runtime::io::read_input_line() else {
            exit_after_write_failure();
        };
        // And one that happened while the REPL waited for this line ends
        // it before the line is echoed or run.
        session.exit_if_terminated();
        let Some(line) = read else {
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
/// follow, so the entry continues on the next line (D-605). `//` comments
/// and the contents of strings do not count, except the code of an
/// interpolation `{…}` inside a string. What the parser skips does not
/// count either: the brackets inside an interpolation and the error token
/// of a `/*` (D-1541), which is an error, since block comments do not exist
/// (D-078). A `/*` with no `*/` on its line skips only the rest of that
/// line, and counting goes on on the next one, so the session never waits
/// for a `*/` (D-1543). Brackets are matched as the parser
/// matches them (D-1532): a closing bracket closes the innermost open
/// bracket of its own kind and so ends any bracket opened inside that one,
/// and one with no open bracket of its kind matches nothing and ends the
/// entry, even after an attribute: the checker reports it.
#[must_use]
pub fn is_open(text: &str) -> bool {
    match scan(text) {
        Scan::Open => true,
        Scan::Unmatched => false,
        Scan::Closed => {
            let last = text.lines().rev().map(str::trim).find(|l| !l.is_empty());
            last.is_some_and(|l| l.starts_with('@') || l.starts_with("///"))
        }
    }
}

/// What [`scan`] finds at the end of an entry's text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scan {
    /// Every bracket, string and interpolation is closed.
    Closed,
    /// A bracket, a string, a raw string or an interpolation is open.
    Open,
    /// A closing bracket matches no open bracket of its kind (D-1532).
    Unmatched,
}

/// Where the scan is inside a string.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Quoted {
    /// The text of a string.
    Text,
    /// The code of an interpolation `{…}` in a string.
    Interpolation,
}

/// Scans `text` as the lexer and the parser read it (§2.5, D-605, D-1532,
/// D-1541).
///
/// - A raw string `r"…"` or `r#"…"#` has no escapes and no interpolation
///   (§2, D-114, D-130).
/// - An interpolation ends at the next `}` (§2.5.4). The lexer also ends it
///   at a `"`, which then ends the string, and at a line break, after which
///   the string goes on. The parser skips the brackets inside it, so they
///   are not counted (D-1541), and there `//` starts no comment, `/*` no
///   error token and `r"` no raw string.
/// - `/*` starts an error token, which the parser skips, so the brackets
///   inside it are not counted (D-078, D-1541). The scan ends it after a
///   `*/` on the same line, as the lexer does, or else at the end of the
///   line, where the lexer reads on to the next `*/` or the end of the
///   text (D-1543). A `*/` on a later line is then plain text.
/// - A `)`, `]` or `}` closes the innermost open bracket of its own kind
///   and ends the brackets opened inside that one, as the parser's
///   recovery does. One that matches no open bracket ends the scan.
fn scan(text: &str) -> Scan {
    let chars: Vec<char> = text.chars().collect();
    let mut brackets: Vec<char> = Vec::new();
    let mut quoted: Option<Quoted> = None;
    let mut at = 0;
    let mut previous: Option<char> = None;
    while let Some(&c) = chars.get(at) {
        at = at.saturating_add(1);
        match quoted {
            Some(Quoted::Text) => match c {
                '\\' => at = escape_end(&chars, at),
                '"' => quoted = None,
                '{' => quoted = Some(Quoted::Interpolation),
                _ => {}
            },
            Some(Quoted::Interpolation) => match c {
                '}' | '\n' => quoted = Some(Quoted::Text),
                '"' => quoted = None,
                _ => {}
            },
            None => match c {
                '/' if chars.get(at) == Some(&'/') => {
                    while let Some(&next) = chars.get(at) {
                        at = at.saturating_add(1);
                        if next == '\n' {
                            break;
                        }
                    }
                }
                // The lexer makes `/*` through the next `*/` one error token
                // (D-078), which the parser skips, so its brackets do not
                // count (D-1541). With no `*/` on its line, only the rest of
                // the line is skipped (D-1543).
                '/' if chars.get(at) == Some(&'*') => {
                    at = block_comment_end(&chars, at.saturating_add(1));
                }
                'r' if !previous.is_some_and(is_ident_char) => {
                    let hashes = chars
                        .get(at..)
                        .map_or(0, |rest| rest.iter().take_while(|&&h| h == '#').count());
                    let quote = at.saturating_add(hashes);
                    if chars.get(quote) == Some(&'"') {
                        match raw_string_end(&chars, quote.saturating_add(1), hashes) {
                            Some(end) => at = end,
                            None => return Scan::Open,
                        }
                    }
                }
                '"' => quoted = Some(Quoted::Text),
                '(' | '[' | '{' => brackets.push(c),
                ')' | ']' | '}' => {
                    let opener = match c {
                        ')' => '(',
                        ']' => '[',
                        _ => '{',
                    };
                    match brackets.iter().rposition(|&open| open == opener) {
                        Some(i) => brackets.truncate(i),
                        None => return Scan::Unmatched,
                    }
                }
                _ => {}
            },
        }
        previous = chars.get(at.wrapping_sub(1)).copied();
    }
    if quoted.is_some() || !brackets.is_empty() {
        Scan::Open
    } else {
        Scan::Closed
    }
}

/// The position after an escape in a string whose `\` ends before `at`:
/// `\u{` with hex digits and a `}`, or else `\` and one character, as the
/// lexer reads it (§2.5.3).
fn escape_end(chars: &[char], at: usize) -> usize {
    if chars.get(at) == Some(&'u') && chars.get(at.saturating_add(1)) == Some(&'{') {
        let digits = at.saturating_add(2);
        let end = chars.get(digits..).map_or(digits, |rest| {
            digits.saturating_add(rest.iter().take_while(|h| h.is_ascii_hexdigit()).count())
        });
        if chars.get(end) == Some(&'}') {
            return end.saturating_add(1);
        }
    }
    at.saturating_add(1)
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

/// Where the reader ends the error token of a `/*` whose `*` ends before
/// `from`: after a `*/` on the same line, as the lexer reads it (D-078,
/// D-1541), or else at the line break, which is scanned next, or the end of
/// `chars` (D-1543). Block comments do not nest.
fn block_comment_end(chars: &[char], from: usize) -> usize {
    let mut at = from;
    while let Some(&c) = chars.get(at) {
        if c == '\n' {
            return at;
        }
        at = at.saturating_add(1);
        if c == '*' && chars.get(at) == Some(&'/') {
            return at.saturating_add(1);
        }
    }
    chars.len()
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

/// Ends the REPL with the status a termination fixed (D-709), printing
/// nothing more: the session has completed its runtime's halt (D-797).
#[expect(
    clippy::exit,
    reason = "D-709: the session ends with the termination's status"
)]
fn exit_with(status: i32) -> ! {
    std::process::exit(status)
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

    /// A closing bracket closes the innermost open bracket of its own kind
    /// and ends those opened inside it, as the parser's recovery does, and
    /// one with no open bracket of its kind ends the entry (D-1532, D-605).
    #[test]
    fn brackets_match_by_kind() {
        assert!(!is_open("{ ( }"));
        assert!(!is_open("let x = {\n  let y = {\n    1 ]"));
        assert!(!is_open("[ { ] }"));
        assert!(!is_open("[ { ]"));
        assert!(!is_open("f({ ) }"));
        assert!(!is_open("f({ )"));
        assert!(!is_open("let x = [\n  {\n    1\n  ]"));
        // Nothing of its kind is open.
        assert!(!is_open("{ )"));
        assert!(!is_open("( ]"));
        assert!(!is_open("[1, (2, ]"));
        assert!(!is_open("{ ( ]"));
        // Still open: the closer ends only what it closes.
        assert!(is_open("{ ( )"));
        assert!(is_open("{ [ ]"));
        assert!(is_open("{ [ ( ]"));
        assert!(is_open("( { ( }"));
        assert!(is_open("let x = {\n  let y = {\n    (1 }"));
        // A closer that matches nothing ends the entry after an attribute
        // or a doc comment too.
        assert!(!is_open("@external(native, \"f\"))"));
        assert!(!is_open("/// Docs.\n@inline)"));
    }

    /// An interpolation ends at its `}` whatever it holds, since the
    /// parser skips the brackets inside it (§2.5.4, D-1532, D-1541), and at
    /// a `"` or a line break, as the lexer ends it.
    #[test]
    fn interpolations_end_as_the_lexer_ends_them() {
        assert!(!is_open("\"{ ( }\""));
        assert!(!is_open("\"{ [ }\""));
        assert!(!is_open("\"{ { }\""));
        assert!(is_open("f(\"{ ( }\""));
        assert!(!is_open("f(\"{ ( }\")"));
        assert!(!is_open("{ \"{ ( }\" }"));
        // A closer inside an interpolation closes nothing outside it.
        assert!(is_open("f(\"{ ) }\","));
        assert!(is_open("{ \"{ } }\""));
        // A `"` ends the interpolation and the string.
        assert!(!is_open("\"{\""));
        assert!(!is_open("\"{ \\\""));
        // A line break ends the interpolation; the string goes on.
        assert!(is_open("\"{a\n"));
        assert!(!is_open("\"{a\n)\""));
        assert!(!is_open("\"{a\n}\""));
        // No comment and no raw string inside an interpolation.
        assert!(!is_open("\"{ // }\""));
        assert!(!is_open("\"{r}\""));
    }

    /// The error token of a `/*` runs through the next `*/` on its line,
    /// as the lexer makes it, and the parser skips it, so its brackets do
    /// not count (D-1541, D-078).
    #[test]
    fn block_comment_tokens_are_skipped() {
        assert!(!is_open("/* ( */ 3"));
        assert!(!is_open("/* ( */"));
        assert!(!is_open("( /* ) */ 5 )"));
        assert!(!is_open("{ /* } */ }"));
        assert!(is_open("{ /* } */"));
        assert!(!is_open("/* a ( */ 3"));
        // An unclosed `/*` skips the rest of its line.
        assert!(!is_open("1 /* x"));
        assert!(!is_open("/* ("));
        assert!(!is_open("/* ( {"));
        assert!(is_open("( /* x"));
        // `/*/` does not close it; block comments do not nest.
        assert!(!is_open("/*/ ( */"));
        assert!(!is_open("/* /* ( */ 1"));
        // A closer after it still matches nothing and ends the entry.
        assert!(!is_open("/* ( */ )"));
        // `//*` is a `//` comment, and `/*` in a string or an
        // interpolation is no error token.
        assert!(is_open("{ //* }"));
        assert!(is_open("( \"/*\""));
        assert!(is_open("\"{ /* }\" ("));
    }

    /// A `/*` with no `*/` on its line skips only the rest of that line, and
    /// counting goes on on the next one, so the session never waits for a
    /// `*/` (D-1543). The lexer still reads it to the next `*/` or the end of
    /// the entry, so the parser reports the bracket it hid.
    #[test]
    fn an_unclosed_block_comment_ends_at_its_line() {
        assert!(!is_open("{\n/* note\n}"));
        assert!(!is_open("(\n/* x\n)"));
        assert!(!is_open("( /* x\n)"));
        assert!(!is_open("[1,\n/* two\n2]"));
        assert!(is_open("{\n/* note"));
        assert!(is_open("{\n/* note\n1"));
        assert!(is_open("/* x\n("));
        // Its brackets still do not count.
        assert!(is_open("( /* ) x"));
        assert!(!is_open("( /* ) x\n)"));
        assert!(!is_open("/* { (\n1"));
        // A `*/` on a later line is plain text: the reader has ended the
        // token at the first line's end, so the brackets on the later line
        // count, though the lexer skips them through the `*/`.
        assert!(!is_open("( /* x\n */ )"));
        assert!(!is_open("/* (\n */ 1"));
        assert!(!is_open("{\n/* a\n} */"));
        assert!(is_open("/* a\n( */ 1"));
        assert!(!is_open("/* a\n) */ 1"));
        // A `/*` in that text starts another one, which hides the `)`.
        assert!(is_open("( /* a\n*/* )"));
    }

    /// Brackets in strings, raw strings and `//` comments do not count.
    #[test]
    fn brackets_in_strings_and_comments_do_not_count() {
        assert!(!is_open("\"} ) ]\""));
        assert!(is_open("{ \"}\""));
        assert!(is_open("{ // }"));
        assert!(is_open("{ r\"}\""));
        assert!(is_open("{ r#\"}\"\"#"));
        assert!(!is_open("f(\"(\", r\"[\") // ] }"));
        assert!(is_open("{ \"\\{ }\""));
        assert!(!is_open("\"\\u{7d}\""));
        assert!(is_open("{ \"\\u{7d}\""));
        assert!(!is_open("\"\\\\\" }"));
    }
}
