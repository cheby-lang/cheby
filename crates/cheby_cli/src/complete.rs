//! Shell completion of `cheby` (D-1958, D-1959).
//!
//! `cheby completions <shell>` prints a script, [`registration`], that the
//! shell sources at its start. On each Tab the script runs `cheby` again with
//! the words of the command line after `--` and [`VAR`] set to the shell's
//! name, and [`answer`] prints the candidates and exits. `clap_complete`
//! finds the subcommands, the flags and the values of an enumeration from
//! the command's definition. The functions below add what only a run can
//! know: the diagnostic codes, the module paths and the test names of the
//! package in the working directory, `.cheby` files, and the paths
//! `cheby doc` finds.
//!
//! A completion has nowhere to report an error, so a function that cannot
//! read what it needs offers nothing, and none writes to standard error.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use clap::ValueEnum;
use clap_complete::engine::{CompletionCandidate, PathCompleter, ValueCompleter as _};
use clap_complete::env::{CompleteEnv, Elvish, EnvCompleter, Fish, Powershell, Shells, Zsh};

use crate::single;

mod bash;

/// The environment variable that makes a run of `cheby` answer a
/// completion request. It is the script's own and no stable interface.
pub(crate) const VAR: &str = "CHEBY_COMPLETE";

/// The name the script completes, and the name of its functions.
const BIN: &str = "cheby";

/// The shell `cheby completions` writes a script for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Shell {
    Bash,
    Elvish,
    Fish,
    Powershell,
    Zsh,
}

impl Shell {
    /// What writes the shell's script and its answers: `clap_complete`'s
    /// adapter, except for bash, which has its own ([`bash`]).
    fn adapter(self) -> &'static dyn EnvCompleter {
        match self {
            Self::Bash => &bash::Bash,
            Self::Elvish => &Elvish,
            Self::Fish => &Fish,
            Self::Powershell => &Powershell,
            Self::Zsh => &Zsh,
        }
    }
}

/// The adapters of every [`Shell`], which [`VAR`] selects by name.
const ADAPTERS: Shells<'static> = Shells(&[&bash::Bash, &Elvish, &Fish, &Powershell, &Zsh]);

/// Answers a completion request and exits, when [`VAR`] names a shell;
/// returns at once in every other run. `command` builds the command to
/// complete. Nothing may be written to standard output before this.
pub(crate) fn answer(command: fn() -> clap::Command) {
    CompleteEnv::with_factory(command)
        .var(VAR)
        .shells(ADAPTERS)
        .complete();
}

/// The script that turns completion on in `shell`: it calls this `cheby`
/// as it was called, by its name when that was found on `PATH`, and else by
/// its absolute path.
///
/// # Errors
///
/// A failed write, which a buffer does not have.
pub(crate) fn registration(shell: Shell) -> std::io::Result<Vec<u8>> {
    let mut script = Vec::new();
    shell
        .adapter()
        .write_registration(VAR, BIN, BIN, &called_as(), &mut script)?;
    Ok(script)
}

/// How the script calls `cheby`: the first argument of this run, made
/// absolute when it is a path, since the shell completes from other
/// directories.
fn called_as() -> String {
    let Some(first) = std::env::args_os().next() else {
        return BIN.to_owned();
    };
    let mut path = PathBuf::from(first);
    if path.components().count() > 1
        && let Ok(absolute) = std::path::absolute(&path)
    {
        path = absolute;
    }
    path.to_string_lossy().into_owned()
}

/// The root of the package that holds the working directory.
fn package_root() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    cheby_driver::find_package_root(&cwd)
}

/// A candidate with one line about it.
fn candidate(value: &str, about: Option<&str>) -> CompletionCandidate {
    CompletionCandidate::new(value).help(about.map(|about| about.to_owned().into()))
}

/// Every diagnostic code with its summary, for `cheby explain`.
pub(crate) fn codes() -> Vec<CompletionCandidate> {
    cheby_diag::codes::ALL
        .iter()
        .map(|info| candidate(&info.code.to_string(), Some(info.summary)))
        .collect()
}

/// The module paths of the package, for `cheby build` and
/// `cheby test --module`.
pub(crate) fn modules() -> Vec<CompletionCandidate> {
    package_root()
        .map_or_default(|root| cheby_driver::completion::module_paths(&root))
        .iter()
        .map(|path| candidate(path, None))
        .collect()
}

/// The module paths of the package, then the files of [`files`], for
/// `cheby run`, which takes either (D-1949).
pub(crate) fn modules_or_files(typed: &OsStr) -> Vec<CompletionCandidate> {
    let mut found = modules();
    found.retain(|module| starts_with(module.get_value(), typed));
    found.extend(files(typed));
    found
}

/// The `.cheby` files and the directories at `typed`, and `-` for standard
/// input, for `cheby check`, `cheby run` and `cheby fmt`.
pub(crate) fn files(typed: &OsStr) -> Vec<CompletionCandidate> {
    let mut found = cheby_files(typed);
    if typed.is_empty() || typed == "-" {
        found.push(candidate("-", Some("standard input")));
    }
    found
}

/// The `.cheby` files and the directories at `typed`.
fn cheby_files(typed: &OsStr) -> Vec<CompletionCandidate> {
    PathCompleter::any()
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == cheby_driver::EXTENSION)
        })
        .complete(typed)
}

/// The paths `cheby doc` finds a module or an item at
/// ([`cheby_doc::complete`]): in std alone once `typed` starts with
/// `std::`, which needs no run of the package, and else in the package and
/// its dependencies too.
pub(crate) fn doc_paths(typed: &OsStr) -> Vec<CompletionCandidate> {
    let Some(typed) = typed.to_str() else {
        return Vec::new();
    };
    let root = if typed.starts_with("std::") {
        None
    } else {
        package_root()
    };
    // The package's run is on the worker pool, as `cheby doc`'s is.
    if cheby_driver::configure_jobs(None).is_err() {
        return Vec::new();
    }
    let Ok((index, _unread)) = cheby_driver::analysis::api_index(root.as_deref()) else {
        return Vec::new();
    };
    cheby_doc::complete(&index, typed)
        .iter()
        .map(|found| candidate(&found.path, found.about.as_deref()))
        .collect()
}

/// The names of the package's tests, then the `.cheby` files at `typed`,
/// for the first argument of `cheby test`: a filter selects by a part of
/// the full name, so a test's own name selects it.
pub(crate) fn tests_or_files(typed: &OsStr) -> Vec<CompletionCandidate> {
    let tests =
        package_root().map_or_default(|root| cheby_driver::completion::package_tests(&root));
    let mut found = test_names(&tests, typed);
    found.extend(cheby_files(typed));
    found
}

/// The names of the tests of the file that the command line names, for the
/// filter that follows a file in `cheby test FILE [FILTER]`.
pub(crate) fn file_tests(typed: &OsStr) -> Vec<CompletionCandidate> {
    let words: Vec<OsString> = std::env::args_os().collect();
    let Some(file) = tested_file(&words) else {
        return Vec::new();
    };
    test_names(&cheby_driver::completion::file_tests(&file), typed)
}

/// The file of `cheby test FILE` in the arguments of a completion request,
/// which has the words of the command line after `--`: the first word
/// after `test` that names a `.cheby` file. A completer is given only the
/// word it completes, so the file is read from there.
fn tested_file(arguments: &[OsString]) -> Option<PathBuf> {
    let words = arguments.iter().skip_while(|word| *word != "--").skip(1);
    words
        .skip_while(|word| *word != "test")
        .skip(1)
        .find(|word| word.to_str().is_some_and(single::names_file) && *word != "-")
        .map(PathBuf::from)
}

/// One candidate per test name of `tests` that starts with `typed`, in
/// their order, with the module that holds the test, or how many do. A name
/// that could not be a filter is left out: an empty one, one the command
/// would read as a flag or a file, and one with a control character, which
/// a line of the answer cannot hold.
fn test_names(
    tests: &[cheby_driver::completion::TestName],
    typed: &OsStr,
) -> Vec<CompletionCandidate> {
    let mut names: Vec<(&str, Vec<&str>)> = Vec::new();
    for test in tests {
        let name = test.name.as_str();
        let usable = !name.is_empty()
            && !name.starts_with('-')
            && !single::names_file(name)
            && !name.contains(char::is_control);
        if !usable || !starts_with(OsStr::new(name), typed) {
            continue;
        }
        match names.iter_mut().find(|(known, _)| *known == name) {
            Some((_, modules)) => modules.push(&test.module),
            None => names.push((name, vec![&test.module])),
        }
    }
    names
        .iter()
        .map(|(name, modules)| match modules.as_slice() {
            [module] => candidate(name, Some(&format!("test of {module}"))),
            several => candidate(name, Some(&format!("test of {} modules", several.len()))),
        })
        .collect()
}

/// Whether `value` starts with `typed`.
fn starts_with(value: &OsStr, typed: &OsStr) -> bool {
    value
        .as_encoded_bytes()
        .starts_with(typed.as_encoded_bytes())
}
