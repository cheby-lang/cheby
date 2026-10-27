//! The opt-in panic backtrace (D-939, D-1038 to D-1043).
//!
//! `CHEBY_BACKTRACE=1` turns it on; unset, empty or `0` leaves it off, and
//! any other value stops the program before `main` runs (D-1038). The
//! runtime reads the variable once per process, in `Runtime::tuned`, the
//! one place every host starts its runtime ([`setting_from_env`]), and
//! keeps the setting in a static (never a thread-local, D-686).
//!
//! # Where the walk starts (D-1040)
//!
//! A backtrace lists the frames of generated code only, from a frame record
//! that generated code made ([`Start`]): generated code's own raises
//! (`cheby_rt_panic` and the entry check's slow paths, whose entries are
//! naked stubs that pass their caller's frame pointer and return address,
//! `crate::panic`, `crate::unwind::entry_slow`) start at the caller's
//! frame; `joined fiber failed` and `spawn into a scope that has ended`
//! start at the control block's wait-frame word (D-868,
//! [`wait_start`]); every other raise in Rust code has no start and
//! prints the no-frames line. Rust frames are never walked.
//!
//! # What it shows (D-1039)
//!
//! The walk ([`render`]) reads at most [`MAX_FRAMES`] frame records, each
//! checked against the fiber's stack as D-856's walk checks them, and
//! resolves each return address against the call-site map under its read
//! lock (D-867, D-1042): the function's display name (D-1041) and, for a
//! frame above the first, the location of its call. The first line's
//! position is the report's location. Unnamed code is not shown, nor a
//! large function's outer frame when its inner body is the next frame
//! inward. More than [`FOLD`] equal lines in a row print [`FOLD`] and a
//! `(repeated <n> more times)` line, and at most [`MAX_LINES`] frame lines
//! print. The walk's end gives the last line: none at an entry thunk (the
//! root of `main`, a test or a REPL entry, with no kernel callback on its
//! stack), `<called from outside Cheby code>` at a trampoline or any other
//! code, `(more frames not walked)` at the cap.
//!
//! GAP(backtrace-format-details): the log leaves open how the caps and
//! folding meet, so a run of equal lines counts its printed lines toward
//! [`MAX_LINES`] and its `(repeated …)` line does not; frames past the cap
//! are counted in `(<n> more frames)`, folded ones included; a walk that
//! reaches both caps prints `(<n> more frames)` and then `(more frames not
//! walked)`; a frame record outside the fiber's stack ends the walk as
//! code outside Cheby code does.
//!
//! # Capture (D-1042)
//!
//! A raise captures the backtrace right before it records the panic in the
//! fiber (D-699), while every frame is on the stack ([`capture`]), into the
//! report ([`crate::panic::PanicReport`]), whose text prints it after the
//! report's two lines in the same write. Nothing is captured with the
//! variable off, for a cancellation or in constant evaluation. The walk is
//! iterative and writes one buffer, so a stack overflow captures in the
//! slow path's reserved room (D-679).

use std::ffi::{OsStr, OsString};
use std::fmt::{self, Write as _};
use std::sync::OnceLock;

use crate::ctx::ExecCtx;
use crate::entry::{EntryMode, RuntimeError, STACK_HEADROOM};
use crate::locate::{self, CodeMap, FRAME_ALIGN, FrameReader, LiveStack, MAX_FRAMES, RECORD};
use crate::panic::UNKNOWN_LOCATION;

/// The variable that turns the backtrace on (D-1038).
pub(crate) const BACKTRACE_ENV: &str = "CHEBY_BACKTRACE";

/// The setting, read once per process: on, off, or the invalid value as
/// written.
static SETTING: OnceLock<Result<bool, String>> = OnceLock::new();

/// Whether `value` of `CHEBY_BACKTRACE` turns the backtrace on (D-1038):
/// `1` does; unset, empty and `0` do not.
///
/// # Errors
///
/// [`RuntimeError::InvalidBacktrace`] with the value as written for any
/// other value.
pub(crate) fn parse_setting(value: Option<&OsStr>) -> Result<bool, RuntimeError> {
    match value.map(OsStr::to_str) {
        None | Some(Some("" | "0")) => Ok(false),
        Some(Some("1")) => Ok(true),
        Some(_) => Err(RuntimeError::InvalidBacktrace(
            value
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )),
    }
}

/// The variable's value in this process. The runtime's own unit tests
/// never read it, so a developer's setting never changes them; the
/// variable is tested through child processes (`cheby_cli`'s tests and
/// the UI test `bad_value`).
fn env_value() -> Option<OsString> {
    if cfg!(test) {
        None
    } else {
        std::env::var_os(BACKTRACE_ENV)
    }
}

/// D-1038: reads `CHEBY_BACKTRACE` once per process (a static, never a
/// thread-local, D-686) and keeps the setting; called by `Runtime::tuned`
/// (N8b's `sched/mod.rs`, N7b's shared line) right after
/// `threads_from_env()`, before the `Unsupported` return, so every host
/// checks it before its first entry runs.
///
/// # Errors
///
/// `RuntimeError::InvalidBacktrace` with the value as written, for any
/// value but unset, empty, `0` and `1`.
pub(crate) fn setting_from_env() -> Result<(), RuntimeError> {
    let setting = SETTING.get_or_init(|| {
        parse_setting(env_value().as_deref()).map_err(|error| match error {
            RuntimeError::InvalidBacktrace(value) => value,
            other => other.to_string(),
        })
    });
    match setting {
        Ok(_) => Ok(()),
        Err(value) => Err(RuntimeError::InvalidBacktrace(value.clone())),
    }
}

/// Whether the backtrace is on: the variable was read (a runtime started)
/// and is `1`. One load of a static, as a raise tests it (D-1042's cost
/// with the variable off).
pub(crate) fn enabled() -> bool {
    matches!(SETTING.get(), Some(Ok(true)))
}

/// Where a backtrace's walk starts (D-1040).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Start {
    /// No frame record of generated code is known: a panic raised inside
    /// Rust code. The backtrace is the no-frames line.
    Unknown,
    /// Generated code called the raising entry: `fp` is the caller's frame
    /// record and `ra` the return address into the caller, frame 0, whose
    /// position is the report's location.
    Frame {
        /// The caller's frame pointer (`x29`, `rbp`).
        fp: usize,
        /// The return address into the caller (`x30`, the word at `[rsp]`).
        ra: usize,
    },
    /// A waiting kernel's raise: the frame record a std function published
    /// right before it called the kernel (D-868). That function is not
    /// shown; the first line is its caller, at the call that reached it.
    Wait {
        /// The published frame pointer.
        fp: usize,
    },
}

/// The start a stub passes: [`Start::Frame`], or [`Start::Unknown`] when
/// the entry was called as a plain function (Miri, Windows until M6,
/// D-1040), which passes 0 for both words.
pub(crate) const fn start_at(fp: usize, ra: usize) -> Start {
    if fp == 0 || ra == 0 {
        Start::Unknown
    } else {
        Start::Frame { fp, ra }
    }
}

/// D-1040's second start: the wait-frame word `word` when it lies in the
/// fiber's stack above the raising code's stack pointer `sp`, below its
/// top `stack_hi`; a word of a frame that has returned lies below `sp`,
/// and 0 is no word. Otherwise no start.
pub(crate) fn wait_start(word: usize, sp: usize, stack_hi: usize) -> Start {
    if word > sp && word < stack_hi && word.is_multiple_of(FRAME_ALIGN) {
        Start::Wait { fp: word }
    } else {
        Start::Unknown
    }
}

/// The start of `fiber`'s waiting kernel's raise ([`wait_start`]): its
/// wait-frame word against the current stack position and its stack's
/// top.
pub(crate) fn wait_start_of(fiber: &ExecCtx) -> Start {
    let (_, top) = fiber.stack_bounds();
    wait_start(fiber.wait_frame(), crate::panic::stack_position(), top)
}

/// The most equal lines in a row printed before the rest fold (D-1039).
pub(crate) const FOLD: usize = 3;

/// The most frame lines printed, counted after folding (D-1039).
pub(crate) const MAX_LINES: usize = 100;

/// The line of a backtrace without a start (D-1039, D-1040).
pub(crate) const NO_FRAMES: &str = "  <no frames: the panic was raised in runtime code>";

/// The last line of a walk that ended outside Cheby code (D-1039).
pub(crate) const OUTSIDE: &str = "  <called from outside Cheby code>";

/// The last line of a walk that read [`MAX_FRAMES`] records (D-1039).
pub(crate) const NOT_WALKED: &str = "  (more frames not walked)";

/// The bytes reserved for a backtrace's text up front: a hundred lines of
/// about eighty bytes.
const RESERVE: usize = 8 << 10;

/// How a walk ended, which decides its last line (D-1039).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// At an entry thunk: no line.
    Entry,
    /// At a trampoline, other code or a frame outside the stack.
    Outside,
    /// At the cap of [`MAX_FRAMES`] records.
    NotWalked,
}

/// Compares what is written to it with a text, allocating nothing: whether
/// a frame's line equals the previous one.
struct Matches<'a> {
    /// What is still to match.
    rest: &'a str,
    /// Whether everything written so far matched.
    ok: bool,
}

impl fmt::Write for Matches<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        match self.rest.strip_prefix(text) {
            Some(rest) if self.ok => self.rest = rest,
            _ => self.ok = false,
        }
        Ok(())
    }
}

/// Writes a frame's line, with its newline.
fn write_line(out: &mut impl fmt::Write, name: &str, at: Option<&dyn fmt::Display>) -> fmt::Result {
    write!(out, "  in {name}")?;
    if let Some(at) = at {
        write!(out, " at {at}")?;
    }
    out.write_char('\n')
}

/// The backtrace's text as it is written, one buffer: frame lines folded
/// and capped as they come (D-1039).
struct Printer {
    /// The text so far.
    out: String,
    /// The last frame's line in `out`, and whether it is printed; one not
    /// printed is kept only to compare the next line with, then removed.
    last: Option<(usize, bool)>,
    /// Equal lines in the current run, the last frame's included.
    run: usize,
    /// How many of them printed.
    run_printed: usize,
    /// Frame lines printed.
    lines: usize,
    /// Frames not printed because of [`MAX_LINES`].
    hidden: usize,
}

impl Printer {
    /// A text with its first line, `backtrace:`.
    fn new() -> Self {
        let mut out = String::with_capacity(RESERVE);
        out.push_str("backtrace:\n");
        Self {
            out,
            last: None,
            run: 0,
            run_printed: 0,
            lines: 0,
            hidden: 0,
        }
    }

    /// The text of a backtrace without a start.
    fn no_frames(mut self) -> String {
        self.out.push_str(NO_FRAMES);
        self.out.push('\n');
        self.out
    }

    /// Adds a frame `name`, at `at` when it has a position.
    fn frame(&mut self, name: &str, at: Option<&dyn fmt::Display>) {
        let same = self.last.is_some_and(|(start, _)| {
            let mut matches = Matches {
                rest: self.out.get(start..).unwrap_or_default(),
                ok: true,
            };
            write_line(&mut matches, name, at).is_ok() && matches.ok && matches.rest.is_empty()
        });
        if same {
            // The last line stays the run's text to compare with.
            self.run = self.run.saturating_add(1);
            if self.run <= FOLD && self.lines < MAX_LINES {
                self.last = Some((self.out.len(), true));
                // Writing to a `String` never fails.
                let _ = write_line(&mut self.out, name, at);
                self.lines = self.lines.saturating_add(1);
                self.run_printed = self.run_printed.saturating_add(1);
            } else if self.run_printed < FOLD {
                // Cut by the cap rather than folded.
                self.hidden = self.hidden.saturating_add(1);
            }
            return;
        }
        self.drop_pending();
        self.close_run();
        self.run = 1;
        let start = self.out.len();
        // Writing to a `String` never fails.
        let _ = write_line(&mut self.out, name, at);
        let print = self.lines < MAX_LINES;
        if print {
            self.lines = self.lines.saturating_add(1);
            self.run_printed = 1;
        } else {
            self.hidden = self.hidden.saturating_add(1);
        }
        self.last = Some((start, print));
    }

    /// Removes the last frame's line if it was written only to compare.
    fn drop_pending(&mut self) {
        if let Some((start, false)) = self.last {
            self.out.truncate(start);
            self.last = None;
        }
    }

    /// Ends the current run: more than [`FOLD`] equal lines print the
    /// `(repeated <n> more times)` line when all of the [`FOLD`] printed.
    fn close_run(&mut self) {
        let extra = self.run.saturating_sub(FOLD);
        if extra > 0 && self.run_printed == FOLD {
            // Writing to a `String` never fails.
            let _ = writeln!(self.out, "  (repeated {extra} more times)");
        }
        self.run = 0;
        self.run_printed = 0;
    }

    /// The finished text, with the line of how the walk ended.
    fn finish(mut self, end: End) -> String {
        self.drop_pending();
        self.close_run();
        if self.hidden > 0 {
            // Writing to a `String` never fails.
            let _ = writeln!(self.out, "  ({} more frames)", self.hidden);
        }
        match end {
            End::Entry => {}
            End::Outside => {
                self.out.push_str(OUTSIDE);
                self.out.push('\n');
            }
            End::NotWalked => {
                self.out.push_str(NOT_WALKED);
                self.out.push('\n');
            }
        }
        self.out
    }
}

/// The frame record at `fp` on the stack `[lo, hi)`: its return address,
/// stripped, and its caller's frame pointer; `None` for a frame pointer
/// that is 0, not aligned, or whose record does not fit in the stack, as
/// D-856's walk checks them.
fn record(reader: &impl FrameReader, fp: usize, lo: usize, hi: usize) -> Option<(usize, usize)> {
    if fp == 0 || !fp.is_multiple_of(FRAME_ALIGN) || fp < lo || fp.checked_add(RECORD)? > hi {
        return None;
    }
    let ra = reader.strip(reader.word(fp.checked_add(size_of::<usize>())?)?);
    let caller = reader.word(fp)?;
    Some((ra, caller))
}

/// The backtrace's text (module documentation): `backtrace:` and its lines,
/// each ending in a newline, walked from `start` over the stack `[lo, hi)`
/// that `reader` reads, resolved against `map`. `location` is the report's,
/// frame 0's position. `entry_base` is whether code outside Cheby code
/// that the walk reaches is the entry thunk of `main`, a test or a REPL
/// entry: the fiber is an entry's root with no kernel callback on its
/// stack. Reads at most [`MAX_FRAMES`] records; allocates only its text.
pub(crate) fn render(
    map: &CodeMap,
    reader: &impl FrameReader,
    start: Start,
    (lo, hi): (usize, usize),
    location: &str,
    entry_base: bool,
) -> String {
    let mut out = Printer::new();
    let (mut fp, mut inner_large) = match start {
        Start::Unknown => return out.no_frames(),
        Start::Wait { fp } => (fp, false),
        Start::Frame { fp, ra } => match map.frame_at(reader.strip(ra)) {
            Some(frame) if frame.cheby_abi => {
                if let Some(name) = frame.name {
                    let known = location != UNKNOWN_LOCATION;
                    let at: Option<&dyn fmt::Display> = if known { Some(&location) } else { None };
                    out.frame(name.as_str(), at);
                }
                (fp, frame.large_body)
            }
            _ => return out.no_frames(),
        },
    };
    let mut end = End::Outside;
    let mut walked = 0_usize;
    loop {
        if walked == MAX_FRAMES {
            end = End::NotWalked;
            break;
        }
        walked = walked.saturating_add(1);
        let Some((ra, caller)) = record(reader, fp, lo, hi) else {
            break;
        };
        // Entry thunks are in no range (D-1111); a registered C-ABI
        // function, such as a `@blocking` shim, is outside Cheby code.
        let Some(frame) = map.frame_at(ra) else {
            if entry_base {
                end = End::Entry;
            }
            break;
        };
        if !frame.cheby_abi {
            break;
        }
        // A large function's outer frame is not shown when its inner body
        // is the next frame inward (D-1039).
        if !inner_large && let Some(name) = frame.name {
            match &frame.site {
                Some(site) => out.frame(name.as_str(), Some(site)),
                None => out.frame(name.as_str(), None),
            }
        }
        inner_large = frame.large_body;
        if caller <= fp {
            break;
        }
        fp = caller;
    }
    out.finish(end)
}

/// Whether code outside Cheby code that `fiber`'s walk reaches is an entry
/// thunk: the fiber is an entry's root (a host waits for its outcome) and
/// no kernel callback is on its stack (D-687), whose trampoline would be
/// reached first. A spawned fiber starts in a trampoline (D-526).
fn entry_base(fiber: &ExecCtx) -> bool {
    fiber.outcome().is_some() && !fiber.sched.pinned()
}

/// The backtrace of a panic raised on `fiber` from `start`, whose report's
/// location is `location`, or `None` when nothing is captured: the
/// variable is off, or the fiber evaluates a constant (D-1042). Walks
/// `fiber`'s own running stack under the map's read lock.
pub(crate) fn capture(fiber: &ExecCtx, start: Start, location: &str) -> Option<Box<str>> {
    capture_if(enabled(), fiber, start, location)
}

/// [`capture`] with the setting given: no walk runs when `on` is false.
pub(crate) fn capture_if(
    on: bool,
    fiber: &ExecCtx,
    start: Start,
    location: &str,
) -> Option<Box<str>> {
    if !on || matches!(fiber.entry_mode(), EntryMode::ConstEval(_)) {
        return None;
    }
    let (hard_limit, top) = fiber.stack_bounds();
    // The frame whose entry check found an overflow may lie below the
    // hard limit, in the slot's headroom (D-457, D-677); half of it stays
    // clear of a debug guard page (D-678).
    let lo = hard_limit.saturating_sub(STACK_HEADROOM / 2);
    let reader = LiveStack { lo, hi: top };
    let text =
        locate::with_map(|map| render(map, &reader, start, (lo, top), location, entry_base(fiber)));
    Some(text.into_boxed_str())
}
