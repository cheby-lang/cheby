//! The reference evaluator of the generator's subset (D-719, D-1028): the
//! third oracle of the program differential, beside naive counting and
//! Perceus.
//!
//! [`evaluate`] runs a generated package in process, without `cheby`, and
//! returns what the spec says the program prints and how it ends: its
//! standard output, its exit status (0, or 101 when a panic ends `main`,
//! §11.7), and its standard error when the evaluator can predict it
//! exactly ([`Expected::stderr`]). It is written from the spec and the
//! standard-library pages, not from the compiler: where the two disagree,
//! the spec decides, and the disagreement is a finding about the compiler.
//!
//! The program is read through `cheby_syntax`'s tree (the parser that the
//! cross-check pins against `tree-sitter-cheby`, D-272) and lowered into
//! the evaluator's own form (`eval/ir.rs`), which resolves every name and
//! refuses, before anything runs, every construct and standard-library
//! function outside the generator's subset ([`EvalError::Unsupported`]).
//! The subset is what `generator.rs` emits: `Int` arithmetic with its
//! overflow and division-by-zero panics, strings and interpolation with
//! `{x}` and `{x:?}`, lists and `[x, ..rest]`, `std::map`, records,
//! record updates and sum types, `case` with guards, alternatives and
//! several subjects or none, local functions, closures, recursion, `let
//! assert` and `assert` with the messages of §11.4 and §11.5,
//! `fiber::scope` and `fiber::message`, and the planted `panic`. A step
//! budget turns a runaway program into [`EvalError::Budget`], never a
//! hang, and the evaluator never panics.

mod interp;
mod ir;
mod lower;
mod stdlib;
mod value;

#[cfg(test)]
mod tests;

use std::fmt;

use super::Program;

/// The stack of the evaluator's thread: evaluation recurses on the Rust
/// stack, once per nested expression and call.
const STACK_BYTES: usize = 256 << 20;

/// What a generated program prints and how it ends, as the spec defines
/// it (D-719, D-1028).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    pub stdout: String,
    pub status: i32,
    /// Standard error when the evaluator predicts it exactly: empty for a
    /// program that ends with 0, and the two-line panic report of §11.6
    /// (D-449) for a panic that ends `main` at a location the spec fixes.
    /// `None` when it does not predict it, as for a panic raised inside a
    /// standard-library function, whose location the spec leaves open
    /// (GAP(progen-eval-compared-output)).
    pub stderr: Option<String>,
}

/// Why the evaluator gives no [`Expected`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    /// A construct or a standard-library function outside the subset.
    Unsupported(String),
    /// The program ran past the step budget or the call depth.
    Budget,
    /// The program is not a well-formed, well-typed package, which the
    /// generator never emits.
    Malformed(String),
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(what) => write!(f, "outside the evaluator's subset: {what}"),
            Self::Budget => write!(
                f,
                "past the evaluator's budget of {} steps or its call depth",
                interp::MAX_STEPS
            ),
            Self::Malformed(what) => write!(f, "not a well-formed program: {what}"),
        }
    }
}

impl std::error::Error for EvalError {}

/// What the spec says `program` prints and how it ends. Runs on a thread
/// of its own, with a large stack.
///
/// # Errors
///
/// [`EvalError::Unsupported`] for a construct outside the subset,
/// [`EvalError::Budget`] for a runaway program and
/// [`EvalError::Malformed`] for a package the generator would never
/// write.
pub fn evaluate(program: &Program) -> Result<Expected, EvalError> {
    evaluate_counting(program).map(|(expected, _)| expected)
}

/// [`evaluate`], with the number of steps the run took.
pub(crate) fn evaluate_counting(program: &Program) -> Result<(Expected, u64), EvalError> {
    let program = program.clone();
    let worker = std::thread::Builder::new()
        .name("progen-eval".to_owned())
        .stack_size(STACK_BYTES)
        .spawn(move || run(&program))
        .map_err(|e| EvalError::Malformed(format!("cannot start the evaluator's thread: {e}")))?;
    worker
        .join()
        .map_err(|_| EvalError::Malformed("the evaluator's thread panicked".to_owned()))?
}

fn run(program: &Program) -> Result<(Expected, u64), EvalError> {
    let lowered = lower::lower(program)?;
    let mut interp = interp::Interp::new(&lowered);
    let outcome = interp.run_main();
    let steps = interp.steps();
    let stdout = std::mem::take(&mut interp.stdout);
    match outcome {
        Ok(_) => Ok((
            Expected {
                stdout,
                status: 0,
                stderr: Some(String::new()),
            },
            steps,
        )),
        // A panic that ends `main`: standard output, then the report on
        // standard error, and status 101 (§11.6, §11.7, D-449).
        Err(interp::Flow::Panic { message, loc }) => {
            let stderr = loc.and_then(|loc| {
                let (path, text) = lowered.files.get(loc.file)?;
                let (line, column) = lower::line_col(text, loc.offset);
                Some(format!("panicked at {path}:{line}:{column}:\n{message}\n"))
            });
            Ok((
                Expected {
                    stdout,
                    status: 101,
                    stderr,
                },
                steps,
            ))
        }
        Err(interp::Flow::Error(error)) => Err(error),
    }
}
