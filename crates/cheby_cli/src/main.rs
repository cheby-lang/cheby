//! The `cheby` binary: every command of spec §13.1 (D-031).
//!
//! Commands that are not implemented yet report the milestone that brings
//! them and exit with status 1. Flag spellings are informative (§13).
//!
//! - `--deny-warnings` on `check`, `run`, `test` and `repl` renders every
//!   warning as an error with its own code and fails the command with
//!   status 1; `run` and `test` then run nothing (D-549).
//! - `cheby check --target native|js` checks the code as it runs on that
//!   target only (D-550).
//! - `-j N`/`--jobs N`, global, sets the number of worker threads of the
//!   build, by default the available parallelism (D-551).
//!
//! Diagnostics go to stderr in the layout of D-418. Errors that are not
//! about the program, such as a missing `cheby.toml`, are `error:` lines
//! without a code (D-442). Exit status: 0 on success, 1 on any
//! error. `cheby run` exits with the program's status (§10.10, D-182): 0,
//! 1 when `main` returns `Err`, 101 after a panic, and in debug builds of
//! the toolchain 102 when objects leak (D-458).
//!
//! Diagnostics, tool errors and clap's help and usage errors are colored
//! as the global `--color` flag and the environment decide ([`color`]).
//! Color only adds escapes: the text is the same. Panic reports of a
//! running program stay plain (D-449).

mod color;

use std::io::{IsTerminal as _, Write as _};
use std::num::NonZeroUsize;
use std::process::ExitCode;

use cheby_diag::Colors;
use cheby_runtime::entry::AtExit;
use clap::{CommandFactory as _, FromArgMatches as _, Parser, Subcommand, ValueEnum};

use crate::color::ColorWhen;

/// The global allocator of the toolchain and of the programs it runs:
/// runtime objects are allocated through it (`cheby_runtime::alloc`).
/// Sanitizer builds keep the system allocator, which the sanitizers
/// intercept (D-721).
#[cfg(not(any(cheby_sanitize = "address", cheby_sanitize = "thread")))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Parser)]
#[command(name = "cheby", version, about = "The Cheby toolchain")]
struct Cli {
    /// When to color diagnostics, errors and help
    #[arg(
        long,
        global = true,
        value_enum,
        value_name = "WHEN",
        default_value_t = ColorWhen::Auto
    )]
    color: ColorWhen,
    /// Number of worker threads for the build; defaults to the available parallelism
    #[arg(short = 'j', long, global = true, value_name = "N")]
    jobs: Option<NonZeroUsize>,
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
    Check {
        /// Check the code as it runs on this target only (D-550)
        #[arg(long, value_enum)]
        target: Option<Target>,
        /// Treat every warning as an error (D-549)
        #[arg(long)]
        deny_warnings: bool,
    },
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
        /// Treat every warning as an error and run nothing after one (D-549)
        #[arg(long)]
        deny_warnings: bool,
        /// Print to stderr, before the program runs, where reuse in place happens or not and why (D-717)
        #[arg(long)]
        reuse_report: bool,
        /// Arguments passed to the program, after `--`
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Start an interactive session (§13.5)
    Repl {
        /// Treat every warning as an error (D-549)
        #[arg(long)]
        deny_warnings: bool,
    },
    /// Run `test` blocks (§13.6, D-541)
    Test {
        /// Only run tests whose full name, `<module path> "<test name>"`, contains this text
        filter: Option<String>,
        /// Only run the tests of this module and its submodules, such as `my_app::util`
        #[arg(long, value_name = "PATH")]
        module: Option<String>,
        /// Treat every warning as an error and run no test after one (D-549)
        #[arg(long)]
        deny_warnings: bool,
        /// Print to stderr, before the tests run, where reuse in place happens or not and why
        /// (D-717); a test build may infer other borrowing than `cheby run`, so its report can
        /// differ (D-779, D-813)
        #[arg(long)]
        reuse_report: bool,
    },
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

impl From<Target> for cheby_driver::Target {
    fn from(target: Target) -> Self {
        match target {
            Target::Native => Self::Native,
            Target::Js => Self::Js,
        }
    }
}

/// The stack of the thread that runs the command. Parts of the compiler
/// recurse over the source's structure, such as the decision trees of long
/// list patterns (about six frames per tested element), so the 8 MiB main
/// thread limits valid programs; this moves the limit far beyond realistic
/// sources. Only address space is reserved up front (D-601).
const COMMAND_STACK_SIZE: usize = 256 << 20;

fn main() -> ExitCode {
    let command = std::thread::Builder::new()
        .name("cheby".to_owned())
        .stack_size(COMMAND_STACK_SIZE)
        .spawn(run_command);
    match command.map(std::thread::JoinHandle::join) {
        Ok(Ok(code)) => code,
        // The command thread panicked; the panic hook printed the report.
        Ok(Err(_)) => ExitCode::from(101),
        Err(e) => {
            eprintln!("error: cannot start the compiler's thread: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_command() -> ExitCode {
    let env = |name: &str| std::env::var_os(name);
    // clap prints help and usage errors while it parses, so its color is
    // set from a scan of the raw arguments first.
    let scanned = color::scan_flag(std::env::args_os()).unwrap_or_default();
    let matches = Cli::command()
        .color(color::clap_choice(color::forced(scanned, env)))
        .get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let colors = color::diagnostics(
        color::forced(cli.color, env),
        std::io::stderr().is_terminal(),
    );
    let out = Out { colors };
    // The commands that build start the worker pool first (D-551). `-j`
    // is global, and the commands that build nothing ignore it (D-622).
    let builds = matches!(
        cli.command,
        Command::Check { .. } | Command::Run { .. } | Command::Repl { .. } | Command::Test { .. }
    );
    if builds && let Err(e) = cheby_driver::configure_jobs(cli.jobs) {
        return out.error(e.message());
    }
    match cli.command {
        Command::New { .. } => out.not_yet("new", "M5"),
        Command::Check {
            target,
            deny_warnings,
        } => check(
            out,
            cheby_driver::CheckOptions {
                target: target.map(Into::into),
                deny_warnings,
            },
        ),
        Command::Build { target } => build(out, target),
        Command::Run {
            module_path,
            deny_warnings,
            reuse_report,
            args,
        } => run(
            out,
            cheby_driver::RunOptions {
                module_path,
                args,
                deny_warnings,
                reuse_report,
            },
        ),
        Command::Repl { deny_warnings } => {
            cheby_repl::run_colored(&cheby_repl::ReplOptions { deny_warnings }, colors)
        }
        Command::Test {
            filter,
            module,
            deny_warnings,
            reuse_report,
        } => test(
            out,
            &cheby_driver::TestOptions {
                filter,
                module,
                deny_warnings,
                reuse_report,
            },
        ),
        Command::Fmt { .. } => out.not_yet("fmt", "M5"),
        Command::Lsp => out.not_yet("lsp", "M5"),
        Command::Doc => out.not_yet("doc", "M5"),
        Command::Add => out.not_yet("add", "M5"),
        Command::Update => out.not_yet("update", "M5"),
        Command::Fetch => out.not_yet("fetch", "M5"),
        Command::Explain { code } => explain(out, &code),
    }
}

/// Writes to stderr. A failed write to stderr cannot be reported anywhere
/// else, so it is ignored.
fn write_stderr(text: &str) {
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}

/// How the toolchain's own messages are written to stderr: colored or not.
#[derive(Clone, Copy)]
struct Out {
    colors: Colors,
}

impl Out {
    /// Prints diagnostics in the layout of D-418, warnings as errors
    /// under `--deny-warnings` (D-549).
    fn diagnostics(self, out: &cheby_driver::CheckOutput) {
        write_stderr(&out.render_with(self.colors));
    }

    /// Prints an `error:` line without a code (D-442) and returns status 1.
    fn error(self, message: &str) -> ExitCode {
        write_stderr(&cheby_diag::render_tool_error(message, self.colors));
        ExitCode::FAILURE
    }

    /// Prints a `help:` line that follows a tool error.
    fn help(self, message: &str) {
        write_stderr(&cheby_diag::render_tool_help(message, self.colors));
    }

    fn not_yet(self, command: &str, milestone: &str) -> ExitCode {
        self.error(&format!(
            "`cheby {command}` is not implemented yet (planned for {milestone})"
        ))
    }
}

/// The root of the package that holds the current directory.
fn package_root(out: Out) -> Result<std::path::PathBuf, ExitCode> {
    let cwd = std::env::current_dir()
        .map_err(|e| out.error(&format!("cannot read the current directory: {e}")))?;
    cheby_driver::find_package_root(&cwd).ok_or_else(|| {
        out.error("could not find `cheby.toml` in the current directory or any parent")
    })
}

/// `cheby check [--target T] [--deny-warnings]` (D-282, D-549, D-550):
/// checks the package that holds the current directory with every
/// implemented phase and prints its diagnostics.
fn check(out: Out, options: cheby_driver::CheckOptions) -> ExitCode {
    let root = match package_root(out) {
        Ok(root) => root,
        Err(code) => return code,
    };
    match cheby_driver::check_with(&root, &options) {
        Ok(result) => {
            out.diagnostics(&result);
            if result.has_errors() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => out.error(e.message()),
    }
}

/// `cheby run [module_path] [-- args]` (D-137, D-286): checks and compiles
/// the package with the JIT, then runs the module's `main`. Compile errors
/// are printed and exit with 1 before anything runs; warnings are printed
/// before the program starts, or, with `--deny-warnings`, stop it like
/// errors (D-549).
fn run(out: Out, options: cheby_driver::RunOptions) -> ExitCode {
    let root = match package_root(out) {
        Ok(root) => root,
        Err(code) => return code,
    };
    let mut program = match cheby_driver::prepare_run(&root, options) {
        Ok(cheby_driver::Prepared::Ready(program)) => program,
        Ok(cheby_driver::Prepared::CompileFailed(result)) => {
            out.diagnostics(&result);
            return ExitCode::FAILURE;
        }
        Err(e) => return out.error(e.message()),
    };
    out.diagnostics(program.output());
    // `--reuse-report` (D-717): after the warnings, before `main` runs.
    write_stderr(program.reuse_report());
    // D-797: the process exits right after `main`, so a
    // build without the leak check freezes the fibers left instead of
    // tearing them down (D-073, D-708); a debug build tears them down so
    // that the leak check sees what they held (D-710).
    program.set_at_exit(if cheby_runtime::LEAK_CHECK_ENABLED {
        AtExit::Teardown
    } else {
        AtExit::Freeze
    });
    match program.run() {
        Ok(status) => ExitCode::from(u8::try_from(status).unwrap_or(1)),
        Err(e) => out.error(e.message()),
    }
}

/// `cheby test [FILTER] [--module PATH]` (§13.6, D-541): checks and
/// compiles the package's selected tests, then runs them, several at once
/// (D-723), and prints each test's own output and a line per test in
/// source order, a block per failure and a summary to stdout. Compile
/// errors are printed and exit with 1 before any test runs; warnings are
/// printed first, or, with `--deny-warnings`, stop it like errors (D-549).
/// Exits with 0 when every selected test passes, 1 otherwise.
fn test(out: Out, options: &cheby_driver::TestOptions) -> ExitCode {
    let root = match package_root(out) {
        Ok(root) => root,
        Err(code) => return code,
    };
    let program = match cheby_driver::prepare_test(&root, options) {
        Ok(cheby_driver::PreparedTests::Ready(program)) => program,
        Ok(cheby_driver::PreparedTests::CompileFailed(result)) => {
            out.diagnostics(&result);
            return ExitCode::FAILURE;
        }
        Err(e) => return out.error(e.message()),
    };
    out.diagnostics(program.output());
    // `--reuse-report` (D-717): after the warnings, before any test runs.
    write_stderr(program.reuse_report());
    // The tests' captured output is replayed through the process's own
    // stdout and stderr, as the tests would have written it, each test's
    // before its line (D-723, ADR-0062). A failed write of the report or of
    // that output ends the run with 1 and prints nothing more
    // (`terminated`, D-306, D-783); a tool error here is only a runtime
    // that cannot start or run a test.
    match program.run(&mut std::io::stdout(), &mut std::io::stderr()) {
        Ok(report) if report.success() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => out.error(e.message()),
    }
}

/// `cheby explain <code>` (D-420): prints the code's explanation, the
/// text of `docs/diagnostics/<code>.md`, never colored.
fn explain(out: Out, code: &str) -> ExitCode {
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
        None => out.error(&format!("no diagnostic has the code `{code}`")),
    }
}

/// Until a backend exists, building for it is an error that points to
/// `cheby check` and `cheby run` (D-282).
fn build(out: Out, target: Option<Target>) -> ExitCode {
    let (name, milestone) = match target {
        None | Some(Target::Native) => ("native", "M3"),
        Some(Target::Js) => ("js", "M4"),
    };
    let code = out.error(&format!(
        "`cheby build --target {name}` is not available yet (planned for {milestone})"
    ));
    out.help("use `cheby check` to type-check the package, or `cheby run` to run it");
    code
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser as _};

    use super::{Cli, Command, Target};

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

    /// `--deny-warnings` on `check`, `run`, `test` and `repl` (D-549),
    /// `check --target` (D-550) and the global `-j`/`--jobs` (D-551).
    #[test]
    fn parses_the_build_flags() {
        let cli =
            Cli::try_parse_from(["cheby", "check", "--target", "js", "--deny-warnings"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Check {
                target: Some(Target::Js),
                deny_warnings: true
            }
        ));
        assert_eq!(cli.jobs, None);
        let cli = Cli::try_parse_from([
            "cheby",
            "-j",
            "3",
            "run",
            "--deny-warnings",
            "--",
            "-j",
            "x",
        ])
        .unwrap();
        assert_eq!(cli.jobs.map(std::num::NonZeroUsize::get), Some(3));
        let Command::Run {
            deny_warnings,
            args,
            ..
        } = cli.command
        else {
            panic!("not run");
        };
        assert!(deny_warnings);
        assert_eq!(args, ["-j", "x"]);
        let cli = Cli::try_parse_from(["cheby", "test", "--jobs", "1", "--deny-warnings", "adds"])
            .unwrap();
        assert_eq!(cli.jobs.map(std::num::NonZeroUsize::get), Some(1));
        assert!(matches!(
            cli.command,
            Command::Test {
                deny_warnings: true,
                ..
            }
        ));
        let cli = Cli::try_parse_from(["cheby", "repl", "--deny-warnings"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Repl {
                deny_warnings: true
            }
        ));
        assert!(Cli::try_parse_from(["cheby", "-j", "0", "check"]).is_err());
        assert!(Cli::try_parse_from(["cheby", "check", "--target", "wasm"]).is_err());
        assert!(Cli::try_parse_from(["cheby", "fmt", "--deny-warnings"]).is_err());
    }

    /// `--reuse-report` on `run` and `test` only (D-717), and before `--`
    /// only: after it, it is the program's argument.
    #[test]
    fn parses_the_reuse_report_flag() {
        let cli = Cli::try_parse_from(["cheby", "run", "--reuse-report"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Run {
                reuse_report: true,
                ..
            }
        ));
        let cli = Cli::try_parse_from(["cheby", "run", "--", "--reuse-report"]).unwrap();
        let Command::Run {
            reuse_report, args, ..
        } = cli.command
        else {
            panic!("not run");
        };
        assert!(!reuse_report);
        assert_eq!(args, ["--reuse-report"]);
        let cli = Cli::try_parse_from(["cheby", "test", "--reuse-report", "adds"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Test {
                reuse_report: true,
                ..
            }
        ));
        assert!(Cli::try_parse_from(["cheby", "check", "--reuse-report"]).is_err());
        assert!(Cli::try_parse_from(["cheby", "repl", "--reuse-report"]).is_err());
    }
}
