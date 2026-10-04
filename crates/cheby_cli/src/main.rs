//! The `cheby` binary: every command of spec §13.1 (D-031).
//!
//! Commands that are not implemented yet report the milestone that brings
//! them and exit with status 1. Flag spellings are informative (§13).

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
        Command::Check => not_yet("check", "M1"),
        Command::Build { target } => build(target),
        Command::Run { .. } => not_yet("run", "M1"),
        Command::Repl => not_yet("repl", "M1"),
        Command::Test => not_yet("test", "M1"),
        Command::Fmt { .. } => not_yet("fmt", "M5"),
        Command::Lsp => not_yet("lsp", "M5"),
        Command::Doc => not_yet("doc", "M5"),
        Command::Add => not_yet("add", "M5"),
        Command::Update => not_yet("update", "M5"),
        Command::Fetch => not_yet("fetch", "M5"),
    }
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
            "fetch",
        ] {
            assert!(names.contains(&expected), "missing command `{expected}`");
        }
    }
}
