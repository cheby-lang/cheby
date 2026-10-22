//! The `cheby` binary: every command of spec §13.1 (D-031).
//!
//! Commands that are not implemented yet report the milestone that brings
//! them and exit with status 1. Flag spellings are informative (§13).
//!
//! - `--deny-warnings` on `check`, `build`, `run`, `test` and `repl`
//!   renders every warning as an error with its own code and fails the
//!   command with status 1; `build` then links nothing, and `run` and
//!   `test` run nothing (D-549).
//! - `cheby build [module_path] [--target native|js] [-o PATH]` builds an
//!   executable for the host (D-937, D-936), linked with `cc` against the
//!   runtime archive next to `cheby`, which `cargo build -p cheby_rt_exe`
//!   builds (D-929); `--target js` is planned for M4 (D-282), and Windows
//!   executables for M6 (D-935).
//! - `cheby check --target native|js` checks the code as it runs on that
//!   target only (D-550).
//! - `-j N`/`--jobs N`, global, sets the number of worker threads of the
//!   build, by default the available parallelism (D-551).
//! - `cheby fmt [--check] [PATH…]` formats the package's sources, the
//!   given files, or standard input for `-` (D-1031).
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

mod build_id;
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
    /// Compile a module's `main` into an executable for `native` (AOT) or `js` (D-006, D-937)
    Build {
        /// Module path, such as `my_app::tools::migrate`; defaults to the root module
        module_path: Option<String>,
        /// Target to build for; `native` when the manifest allows it
        #[arg(long, value_enum)]
        target: Option<Target>,
        /// Write the executable to PATH instead of `build/native/<name>`
        #[arg(short = 'o', value_name = "PATH")]
        output: Option<std::path::PathBuf>,
        /// Treat every warning as an error and link nothing after one (D-549)
        #[arg(long)]
        deny_warnings: bool,
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
    /// Format source files (§13.7, D-1031)
    Fmt {
        /// List the files that would change and exit with 1 if any would, writing nothing
        #[arg(long)]
        check: bool,
        /// Files to format, `-` for standard input to standard output; by default every
        /// `.cheby` file under the package's `src/`
        #[arg(value_name = "PATH")]
        paths: Vec<std::path::PathBuf>,
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
        Command::Check { .. }
            | Command::Build { .. }
            | Command::Run { .. }
            | Command::Repl { .. }
            | Command::Test { .. }
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
        Command::Build {
            module_path,
            target,
            output,
            deny_warnings,
        } => build(
            out,
            BuildArgs {
                module_path,
                target,
                output,
                deny_warnings,
            },
        ),
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
        Command::Fmt { check, paths } => fmt(out, check, &paths),
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

/// The note of D-959, printed once on standard error before anything
/// else the command prints, when the cache cannot be used and the command
/// runs on a temporary one (GAP(cache-fallback-note)).
fn fallback_note(fallback: &cheby_driver::cache::Fallback) -> String {
    format!(
        "note: the build cache cannot be used ({fallback}), so this command uses a temporary \
         cache that is deleted when it ends; set CHEBY_CACHE to a writable directory to keep \
         the cache between commands\n"
    )
}

/// Opens the build cache of `cheby check`, `cheby run` and `cheby test`
/// (D-959): `CHEBY_CACHE` or the OS default, with the build ID (D-957)
/// and the lookup log `CHEBY_CACHE_LOG` names (D-973). When it falls back
/// to a temporary cache, the note is printed first. A relative
/// `CHEBY_CACHE`, an unsafe root and a temporary cache that cannot be
/// created either are tool errors.
fn open_cache(out: Out) -> Result<cheby_driver::Cache, ExitCode> {
    use cheby_driver::cache::{CacheEnv, CacheLog};
    let log = CacheLog::from_var(std::env::var_os("CHEBY_CACHE_LOG"));
    match cheby_driver::Cache::open(&CacheEnv::from_process(), build_id::build_id(), log) {
        Ok((cache, fallback)) => {
            if let Some(fallback) = fallback {
                write_stderr(&fallback_note(&fallback));
            }
            Ok(cache)
        }
        Err(e) => Err(out.error(&e.to_string())),
    }
}

/// Ends the command's use of the cache (D-959, D-961, D-973): writes the
/// lookup log, trims when `trim`, and drops the store, which deletes a
/// temporary one, before the process exits. A log that cannot be written
/// is a tool error, and turns `code` into a failure.
fn finish_cache(out: Out, cache: cheby_driver::Cache, trim: bool, code: ExitCode) -> ExitCode {
    match cache.finish(trim) {
        Ok(()) => code,
        Err(e) => out.error(&format!("cannot write CHEBY_CACHE_LOG: {e}")),
    }
}

/// `cheby check [--target T] [--deny-warnings]` (D-282, D-549, D-550):
/// checks the package that holds the current directory with every
/// implemented phase and prints its diagnostics. It reads and writes the
/// build cache's front records (D-955) and trims the cache at most once a
/// day at its end (D-961).
fn check(out: Out, options: cheby_driver::CheckOptions) -> ExitCode {
    let root = match package_root(out) {
        Ok(root) => root,
        Err(code) => return code,
    };
    let cache = match open_cache(out) {
        Ok(cache) => cache,
        Err(code) => return code,
    };
    let code = match cheby_driver::check_cached(&root, &options, &cache) {
        Ok(result) => {
            out.diagnostics(&result);
            if result.has_errors() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => out.error(e.message()),
    };
    finish_cache(out, cache, true, code)
}

/// `cheby run [module_path] [-- args]` (D-137, D-286): checks and compiles
/// the package with the JIT, then runs the module's `main`. Compile errors
/// are printed and exit with 1 before anything runs; warnings are printed
/// before the program starts, or, with `--deny-warnings`, stop it like
/// errors (D-549). The build reads and writes the cache's front records
/// (D-955); the cache is closed before the program starts, which may end
/// the process itself, and `cheby run` never trims (D-961).
fn run(out: Out, options: cheby_driver::RunOptions) -> ExitCode {
    let root = match package_root(out) {
        Ok(root) => root,
        Err(code) => return code,
    };
    let cache = match open_cache(out) {
        Ok(cache) => cache,
        Err(code) => return code,
    };
    let prepared = cheby_driver::prepare_run_cached(&root, options, &cache);
    let mut program = match prepared {
        Ok(cheby_driver::Prepared::Ready(program)) => program,
        Ok(cheby_driver::Prepared::CompileFailed(result)) => {
            out.diagnostics(&result);
            return finish_cache(out, cache, false, ExitCode::FAILURE);
        }
        Err(e) => {
            let code = out.error(e.message());
            return finish_cache(out, cache, false, code);
        }
    };
    out.diagnostics(program.output());
    // `--reuse-report` (D-717): after the warnings, before `main` runs.
    write_stderr(program.reuse_report());
    let finished = finish_cache(out, cache, false, ExitCode::SUCCESS);
    if finished != ExitCode::SUCCESS {
        return finished;
    }
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
/// Exits with 0 when every selected test passes, 1 otherwise. The build
/// reads and writes the cache's front records (D-955); the cache is
/// trimmed at most once a day (D-961) and closed when the build ends,
/// before any test runs.
fn test(out: Out, options: &cheby_driver::TestOptions) -> ExitCode {
    let root = match package_root(out) {
        Ok(root) => root,
        Err(code) => return code,
    };
    let cache = match open_cache(out) {
        Ok(cache) => cache,
        Err(code) => return code,
    };
    let program = match cheby_driver::prepare_test_cached(&root, options, &cache) {
        Ok(cheby_driver::PreparedTests::Ready(program)) => program,
        Ok(cheby_driver::PreparedTests::CompileFailed(result)) => {
            out.diagnostics(&result);
            return finish_cache(out, cache, true, ExitCode::FAILURE);
        }
        Err(e) => {
            let code = out.error(e.message());
            return finish_cache(out, cache, true, code);
        }
    };
    out.diagnostics(program.output());
    // `--reuse-report` (D-717): after the warnings, before any test runs.
    write_stderr(program.reuse_report());
    let finished = finish_cache(out, cache, true, ExitCode::SUCCESS);
    if finished != ExitCode::SUCCESS {
        return finished;
    }
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

/// One source `cheby fmt` formats.
struct FmtTarget {
    /// The file, or `None` for standard input (`-`).
    path: Option<std::path::PathBuf>,
    /// How `--check` and errors name it: as given on the command line, or
    /// relative to the package root (GAP(fmt-check-listing)).
    display: String,
}

/// `cheby fmt [--check] [PATH…]` (§13.7, D-1031): formats every `.cheby`
/// file under the package's `src/`, or the given files, at the formatter's
/// width; `-` reads standard input and writes standard output. A file with
/// a syntax error is reported with its diagnostics and left alone. A file
/// is rewritten only when it changes, through a temporary file renamed in
/// the same directory. With `--check`, nothing is written: the files that
/// would change are listed on standard output, one per line. Exits with 1
/// after any error, or with `--check` when a file would change.
fn fmt(out: Out, check: bool, paths: &[std::path::PathBuf]) -> ExitCode {
    let targets: Vec<FmtTarget> = if paths.is_empty() {
        let root = match package_root(out) {
            Ok(root) => root,
            Err(code) => return code,
        };
        let src = root.join("src");
        match package_sources(&src) {
            Ok(files) => files
                .into_iter()
                .map(|path| FmtTarget {
                    display: cheby_diag::display_path(&path, &root),
                    path: Some(path),
                })
                .collect(),
            Err(e) => return out.error(&format!("cannot read `{}`: {e}", src.display())),
        }
    } else {
        paths
            .iter()
            .map(|path| FmtTarget {
                display: path.display().to_string(),
                path: (path.as_os_str() != "-").then(|| path.clone()),
            })
            .collect()
    };
    let mut failed = false;
    let mut would_change = false;
    for target in &targets {
        match fmt_one(out, target, check) {
            Ok(changed) if check && changed => {
                would_change = true;
                let mut stdout = std::io::stdout().lock();
                if writeln!(stdout, "{}", target.display).is_err() {
                    failed = true;
                }
            }
            Ok(_) => {}
            Err(()) => failed = true,
        }
    }
    if failed || would_change {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Formats one source. `Ok(true)` when formatting changes it; the change
/// is written unless `check` is set. Errors are reported here.
fn fmt_one(out: Out, target: &FmtTarget, check: bool) -> Result<bool, ()> {
    let fail = |message: String| {
        let _ = out.error(&message);
    };
    let text = if let Some(path) = &target.path {
        std::fs::read_to_string(path)
            .map_err(|e| fail(format!("cannot read `{}`: {e}", target.display)))?
    } else {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut text)
            .map_err(|e| fail(format!("cannot read standard input: {e}")))?;
        text
    };
    let name = if target.path.is_some() {
        target.display.as_str()
    } else {
        "<stdin>"
    };
    let mut sources = cheby_diag::SourceMap::new();
    let file = sources
        .add_file(name, text.as_str())
        .map_err(|e| fail(e.to_string()))?;
    let formatted = match cheby_fmt::format_file(file, &text, cheby_fmt::PROVISIONAL_WIDTH) {
        Ok(formatted) => formatted,
        Err(e) => {
            write_stderr(&cheby_diag::render_all_with(
                e.errors(),
                &sources,
                out.colors,
            ));
            return Err(());
        }
    };
    let changed = formatted != text;
    if check {
        return Ok(changed);
    }
    match &target.path {
        None => {
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all(formatted.as_bytes())
                .and_then(|()| stdout.flush())
                .map_err(|e| fail(format!("cannot write standard output: {e}")))?;
        }
        Some(path) if changed => replace_file(path, &formatted)
            .map_err(|e| fail(format!("cannot write `{}`: {e}", target.display)))?,
        Some(_) => {}
    }
    Ok(changed)
}

/// Replaces `path`'s contents with `text` through a temporary file in the
/// same directory, renamed over it, so a reader never sees half a file.
/// The temporary file takes the original's permissions. A symbolic link
/// is followed: its target is rewritten and the link stays a link. A
/// read-only file is an error and is left alone (GAP(fmt-write-target)).
fn replace_file(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    let path = std::fs::canonicalize(path)?;
    if std::fs::metadata(&path)?.permissions().readonly() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the file is read-only",
        ));
    }
    let path = path.as_path();
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the path names no file"))?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("the path names no file"))?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(".fmt-{}.tmp", std::process::id()));
    let temp = dir.join(temp_name);
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        let permissions = std::fs::metadata(path)?.permissions();
        std::fs::set_permissions(&temp, permissions)?;
        std::fs::rename(&temp, path)
    };
    let result = write();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Every `.cheby` file under `src`, sorted by path. Hidden entries are
/// skipped, as module discovery skips them (D-208). Symbolic links are
/// followed, but each directory and each file is visited once, under the
/// first path that reaches it in sorted order, so a link cycle ends the
/// walk instead of listing one file under many paths
/// (GAP(fmt-source-walk)).
fn package_sources(src: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    let mut seen_dirs = std::collections::BTreeSet::new();
    let mut seen_files = std::collections::BTreeSet::new();
    let mut dirs = vec![src.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        if !seen_dirs.insert(std::fs::canonicalize(&dir)?) {
            continue;
        }
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with('.') {
                entries.push(entry.path());
            }
        }
        entries.sort();
        let mut inner = Vec::new();
        for path in entries {
            // A dangling or looping link is neither a file nor a
            // directory, and is skipped.
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                inner.push(path);
            } else if meta.is_file()
                && path.extension().is_some_and(|e| e == "cheby")
                && seen_files.insert(std::fs::canonicalize(&path)?)
            {
                files.push(path);
            }
        }
        // Depth first, in sorted order.
        dirs.extend(inner.into_iter().rev());
    }
    files.sort();
    Ok(files)
}

/// What `cheby build` was asked for.
struct BuildArgs {
    module_path: Option<String>,
    target: Option<Target>,
    output: Option<std::path::PathBuf>,
    deny_warnings: bool,
}

/// `cheby build [module_path] [--target native|js] [-o PATH]
/// [--deny-warnings]` (D-937): builds the module's `main`, the root
/// module's by default, into an executable for the host (D-936), written
/// to `build/native/<package>` (or `<module path with :: replaced by ->`)
/// under the package root, or to `-o PATH`, a relative path being taken
/// from the current directory. The default target is `native` when the
/// manifest allows it; a target the manifest leaves out is `cheby check`'s
/// tool error (D-550); `--target js` keeps D-282's error until M4, and
/// `--target native` is "not available yet (M6)" on Windows (D-935).
/// Warnings go to standard error before the link, and `--deny-warnings`
/// stops the build after them (D-549); success prints nothing else. The
/// build reads and writes the cache's front records (D-955) and trims the
/// cache at most once a day at its end (D-961). It needs the runtime
/// archive next to `cheby` (`cargo build -p cheby_rt_exe`, D-929).
fn build(out: Out, args: BuildArgs) -> ExitCode {
    let js_not_yet = || {
        let code = out.error("`cheby build --target js` is not available yet (planned for M4)");
        out.help("use `cheby check` to type-check the package, or `cheby run` to run it");
        code
    };
    // D-282's error needs no package.
    if matches!(args.target, Some(Target::Js)) {
        return js_not_yet();
    }
    let root = match package_root(out) {
        Ok(root) => root,
        Err(code) => return code,
    };
    let target = match cheby_driver::build_target(&root, args.target.map(Into::into)) {
        Ok(target) => target,
        Err(e) => return out.error(e.message()),
    };
    if matches!(target, cheby_driver::Target::Js) {
        return js_not_yet();
    }
    if cfg!(windows) {
        return out.error(
            "`cheby build --target native` is not available yet on Windows (planned for M6)",
        );
    }
    let output = match args.output {
        Some(path) if path.is_relative() => match std::env::current_dir() {
            Ok(cwd) => Some(cwd.join(path)),
            Err(e) => return out.error(&format!("cannot read the current directory: {e}")),
        },
        other => other,
    };
    let cache = match open_cache(out) {
        Ok(cache) => cache,
        Err(code) => return code,
    };
    let options = cheby_driver::BuildOptions {
        module_path: args.module_path,
        output,
        deny_warnings: args.deny_warnings,
    };
    // The warnings are printed once the build has passed every check, before
    // the link, which may still fail.
    let built = cheby_driver::build_cached_with(&root, &options, &cache, &mut |warnings| {
        out.diagnostics(warnings);
    });
    let code = match built {
        Ok(cheby_driver::BuildOutcome::Linked { .. }) => ExitCode::SUCCESS,
        Ok(cheby_driver::BuildOutcome::CompileFailed(result)) => {
            out.diagnostics(&result);
            ExitCode::FAILURE
        }
        Err(e) => out.error(e.message()),
    };
    finish_cache(out, cache, true, code)
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

    /// `cheby build [module_path] [--target T] [-o PATH] [--deny-warnings]`
    /// (D-937), with the global `-j`.
    #[test]
    fn parses_the_build_arguments() {
        let cli = Cli::try_parse_from(["cheby", "build"]).unwrap();
        let Command::Build {
            module_path,
            target,
            output,
            deny_warnings,
        } = cli.command
        else {
            panic!("not build");
        };
        assert_eq!(module_path, None);
        assert!(target.is_none() && output.is_none() && !deny_warnings);
        let cli = Cli::try_parse_from([
            "cheby",
            "-j",
            "2",
            "build",
            "app::tools",
            "--target",
            "native",
            "-o",
            "out/app",
            "--deny-warnings",
        ])
        .unwrap();
        assert_eq!(cli.jobs.map(std::num::NonZeroUsize::get), Some(2));
        let Command::Build {
            module_path,
            target,
            output,
            deny_warnings,
        } = cli.command
        else {
            panic!("not build");
        };
        assert_eq!(module_path.as_deref(), Some("app::tools"));
        assert!(matches!(target, Some(Target::Native)));
        assert_eq!(output, Some(std::path::PathBuf::from("out/app")));
        assert!(deny_warnings);
        assert!(Cli::try_parse_from(["cheby", "build", "--target", "wasm"]).is_err());
    }

    /// `cheby fmt [--check] [PATH…]` with `-` for standard input
    /// (D-1031), the global flags, and no `--deny-warnings`.
    #[test]
    fn parses_the_fmt_arguments() {
        let cli = Cli::try_parse_from(["cheby", "fmt"]).unwrap();
        let Command::Fmt { check, paths } = cli.command else {
            panic!("not fmt");
        };
        assert!(!check);
        assert_eq!(paths, Vec::<std::path::PathBuf>::new());
        let cli = Cli::try_parse_from([
            "cheby",
            "--color",
            "never",
            "-j",
            "2",
            "fmt",
            "--check",
            "src/a.cheby",
            "-",
            "b.cheby",
        ])
        .unwrap();
        assert_eq!(cli.jobs.map(std::num::NonZeroUsize::get), Some(2));
        let Command::Fmt { check, paths } = cli.command else {
            panic!("not fmt");
        };
        assert!(check);
        assert_eq!(
            paths,
            ["src/a.cheby", "-", "b.cheby"].map(std::path::PathBuf::from)
        );
        let cli = Cli::try_parse_from(["cheby", "fmt", "-", "--color=always"]).unwrap();
        assert!(matches!(cli.command, Command::Fmt { check: false, .. }));
        assert!(Cli::try_parse_from(["cheby", "fmt", "--deny-warnings", "-"]).is_err());
        assert!(Cli::try_parse_from(["cheby", "fmt", "--width", "80"]).is_err());
    }
}
