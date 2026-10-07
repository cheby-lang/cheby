//! The `cheby` binary: every command of spec §13.1 (D-031).
//!
//! Commands that are not implemented yet report the milestone that brings
//! them and exit with status 1. Flag spellings are informative (§13).
//!
//! Diagnostics go to stderr in the layout of D-418. Errors that are not
//! about the program, such as a missing `cheby.toml`, are plain `error:`
//! lines without a code (D-442). Exit status: 0 on success, 1 on any
//! error. `cheby run` exits with the program's status (§10.10, D-182): 0,
//! 1 when `main` returns `Err`, 101 after a panic, and in debug builds of
//! the toolchain 102 when objects leak (D-458).

use std::io::Write as _;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "cheby", version, about = "The Cheby toolchain")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a package (D-097)
    New {
        /// Package name, a lower-case identifier
        name: String,
    },
    /// Type-check the package without producing artifacts (D-282)
    Check,
    /// Compile the package for `native` (AOT) or `js` (D-006, D-007)
    Build {
        /// Target to build for
        #[arg(long, value_enum)]
        target: Option<Target>,
    },
    /// Compile with the JIT and run a module's `main` (D-137)
    Run {
        /// Module path, such as `my_app::tools::migrate`; defaults to the root module
        module_path: Option<String>,
        /// Arguments passed to the program, after `--`
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Start an interactive session (§13.5)
    Repl,
    /// Run `test` blocks (§13.6)
    Test,
    /// Format source files (§13.7)
    Fmt {
        /// Exit non-zero if any file would change, without writing
        #[arg(long)]
        check: bool,
    },
    /// Run the language server over stdio (§13.11)
    Lsp,
    /// Generate HTML documentation (§13.10)
    Doc,
    /// Add a dependency to the manifest and lockfile (§13.4)
    Add,
    /// Re-resolve tags and refresh the lockfile (§13.4)
    Update,
    /// Download what the lockfile names (§13.4)
    Fetch,
    /// Explain a diagnostic code such as `E0012` (D-420)
    Explain {
        /// Diagnostic code, such as `E0012` or `W0003`
        code: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Target {
    Native,
    Js,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::New { .. } => not_yet("new", "M5"),
        Command::Check => check(),
        Command::Build { target } => build(target),
        Command::Run { module_path, args } => run(module_path, args),
        Command::Repl => not_yet("repl", "M1"),
        Command::Test => not_yet("test", "M1"),
        Command::Fmt { .. } => not_yet("fmt", "M5"),
        Command::Lsp => not_yet("lsp", "M5"),
        Command::Doc => not_yet("doc", "M5"),
        Command::Add => not_yet("add", "M5"),
        Command::Update => not_yet("update", "M5"),
        Command::Fetch => not_yet("fetch", "M5"),
        Command::Explain { code } => explain(&code),
    }
}

/// The root of the package that holds the current directory.
fn package_root() -> Result<std::path::PathBuf, ExitCode> {
    let cwd = std::env::current_dir()
        .map_err(|e| tool_error(&format!("cannot read the current directory: {e}")))?;
    cheby_driver::find_package_root(&cwd).ok_or_else(|| {
        tool_error("could not find `cheby.toml` in the current directory or any parent")
    })
}

/// Prints diagnostics to stderr. A failed write to stderr cannot be
/// reported anywhere else.
fn print_diagnostics(out: &cheby_driver::CheckOutput) {
    let _ = std::io::stderr().lock().write_all(out.render().as_bytes());
}

/// `cheby check` (D-282): checks the package that holds the current
/// directory with every implemented phase and prints its diagnostics.
fn check() -> ExitCode {
    let root = match package_root() {
        Ok(root) => root,
        Err(code) => return code,
    };
    match cheby_driver::check(&root) {
        Ok(out) => {
            print_diagnostics(&out);
            if out.has_errors() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => tool_error(e.message()),
    }
}

/// `cheby run [module_path] [-- args]` (D-137, D-286): checks and compiles
/// the package with the JIT, then runs the module's `main`. Compile errors
/// are printed and exit with 1 before anything runs; warnings are printed
/// before the program starts.
fn run(module_path: Option<String>, args: Vec<String>) -> ExitCode {
    let root = match package_root() {
        Ok(root) => root,
        Err(code) => return code,
    };
    let options = cheby_driver::RunOptions { module_path, args };
    let program = match cheby_driver::prepare_run(&root, options) {
        Ok(cheby_driver::Prepared::Ready(program)) => program,
        Ok(cheby_driver::Prepared::CompileFailed(out)) => {
            print_diagnostics(&out);
            return ExitCode::FAILURE;
        }
        Err(e) => return tool_error(e.message()),
    };
    print_diagnostics(program.output());
    match program.run() {
        Ok(status) => ExitCode::from(u8::try_from(status).unwrap_or(1)),
        Err(e) => tool_error(e.message()),
    }
}

/// `cheby explain <code>` (D-420): prints the code's explanation, the
/// text of `docs/diagnostics/<code>.md`.
fn explain(code: &str) -> ExitCode {
    let text = cheby_diag::Code::parse(code).and_then(cheby_diag::codes::explanation);
    match text {
        Some(text) => {
            let mut stdout = std::io::stdout().lock();
            let written = stdout.write_all(text.as_bytes());
            let newline = if text.ends_with('\n') {
                Ok(())
            } else {
                stdout.write_all(b"\n")
            };
            if written.and(newline).is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        None => tool_error(&format!("no diagnostic has the code `{code}`")),
    }
}

fn tool_error(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::FAILURE
}

/// Until a backend exists, building for it is an error that points to
/// `cheby check` and `cheby run` (D-282).
fn build(target: Option<Target>) -> ExitCode {
    let (name, milestone) = match target {
        None | Some(Target::Native) => ("native", "M3"),
        Some(Target::Js) => ("js", "M4"),
    };
    eprintln!(
        "error: `cheby build --target {name}` is not available yet (planned for {milestone})\n\
         help: use `cheby check` to type-check the package, or `cheby run` to run it"
    );
    ExitCode::FAILURE
}

fn not_yet(command: &str, milestone: &str) -> ExitCode {
    eprintln!("error: `cheby {command}` is not implemented yet (planned for {milestone})");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    /// Every command of spec §13.1 is present.
    #[test]
    fn lists_all_spec_commands() {
        let cmd = Cli::command();
        let names: Vec<_> = cmd.get_subcommands().map(clap::Command::get_name).collect();
        for expected in [
            "new", "check", "build", "run", "repl", "test", "fmt", "lsp", "doc", "add", "update",
            "fetch", "explain",
        ] {
            assert!(names.contains(&expected), "missing command `{expected}`");
        }
    }
}
