//! The backtrace (D-1038 to D-1043): the variable's values, and walks on
//! synthetic stacks through a frame reader, which run under Miri too.

use std::collections::BTreeMap;
use std::ffi::OsStr;

use crate::abi::PanicKind;
use crate::backtrace::{
    FOLD, MAX_LINES, NO_FRAMES, NOT_WALKED, OUTSIDE, Start, capture_if, parse_setting, render,
    start_at, wait_start,
};
use crate::ctx::{ExecCtx, FiberInit};
use crate::entry::{ConstEvalLimits, EntryMode, RuntimeError};
use crate::locate::{CallSiteDesc, CodeMap, CodeRangeDesc, FrameReader, FunctionName, MAX_FRAMES};
use crate::panic::{PanicReport, UNKNOWN_LOCATION};

/// D-1038: `1` is on; unset, empty and `0` are off; anything else is the
/// error with the value as written, whose text is D-1038's line without
/// `error: `.
#[test]
fn backtrace_walk_setting_takes_0_and_1_only() {
    assert!(!parse_setting(None).unwrap());
    assert!(!parse_setting(Some(OsStr::new(""))).unwrap());
    assert!(!parse_setting(Some(OsStr::new("0"))).unwrap());
    assert!(parse_setting(Some(OsStr::new("1"))).unwrap());
    for bad in ["yes", "true", "01", " 1", "1 ", "2", "full", "on"] {
        match parse_setting(Some(OsStr::new(bad))) {
            Err(RuntimeError::InvalidBacktrace(value)) => {
                assert_eq!(value, bad);
                assert_eq!(
                    RuntimeError::InvalidBacktrace(value).to_string(),
                    format!("CHEBY_BACKTRACE must be 0 or 1, not \"{bad}\"")
                );
            }
            other => panic!("{bad:?}: {other:?}"),
        }
    }
}

/// The words of a synthetic stack.
#[derive(Default)]
struct Words(BTreeMap<usize, usize>);

impl FrameReader for Words {
    fn word(&self, address: usize) -> Option<usize> {
        self.0.get(&address).copied()
    }

    fn strip(&self, ra: usize) -> usize {
        ra
    }
}

/// The synthetic functions: each at `0x1_0000 * (index + 1)`, 0x100 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum F {
    Check,
    Closure,
    Fold,
    DoubleAll,
    Main,
    Shim,
    Blocking,
    BigInner,
    BigOuter,
    Rec,
    A,
    B,
}

/// Every function, in address order.
const ALL: [F; 12] = [
    F::Check,
    F::Closure,
    F::Fold,
    F::DoubleAll,
    F::Main,
    F::Shim,
    F::Blocking,
    F::BigInner,
    F::BigOuter,
    F::Rec,
    F::A,
    F::B,
];

/// The return offset of the one call site a function may have.
const SITE: u32 = 0x10;

/// A return offset that is no call site.
const NO_SITE: u32 = 0x20;

impl F {
    fn start(self) -> usize {
        let index = ALL.iter().position(|&f| f == self).unwrap();
        0x1_0000_usize.wrapping_mul(index.wrapping_add(1))
    }

    /// The return address at the function's call site, or past its other
    /// call (std code, shims: no site).
    fn ra(self) -> usize {
        let offset = match self.site() {
            Some(_) => SITE,
            None => NO_SITE,
        };
        self.start().wrapping_add(usize::try_from(offset).unwrap())
    }

    fn name(self) -> Option<&'static str> {
        Some(match self {
            F::Check => "demo::check",
            F::Closure => "demo::double_all::{fn#0}",
            F::Fold => "std::list::fold",
            F::DoubleAll => "demo::double_all",
            F::Main => "demo::main",
            F::Shim | F::Blocking => return None,
            F::BigInner | F::BigOuter => "demo::big",
            F::Rec => "demo::rec",
            F::A => "demo::a",
            F::B => "demo::b",
        })
    }

    fn site(self) -> Option<&'static str> {
        match self {
            F::Closure => Some("src/main.cheby:14:43"),
            F::DoubleAll => Some("src/main.cheby:14:17"),
            F::Main => Some("src/main.cheby:19:17"),
            F::BigInner => Some("src/main.cheby:30:5"),
            F::Rec => Some("src/main.cheby:40:3"),
            F::A => Some("src/main.cheby:50:3"),
            F::B => Some("src/main.cheby:60:3"),
            F::Check | F::Fold | F::Shim | F::Blocking | F::BigOuter => None,
        }
    }

    fn desc(self) -> CodeRangeDesc {
        CodeRangeDesc {
            start: self.start(),
            len: 0x100,
            cheby_abi: self != F::Blocking,
            sites: self
                .site()
                .map(|location| CallSiteDesc {
                    ret_offset: SITE,
                    location: location.to_owned(),
                })
                .into_iter()
                .collect(),
            // SAFETY: a static text, valid for the whole test.
            name: self
                .name()
                .map(|name| unsafe { FunctionName::in_record(name) }),
            large_body: self == F::BigInner,
        }
    }
}

/// The map of every synthetic function.
fn map() -> CodeMap {
    let mut map = CodeMap::new();
    map.register(1, ALL.iter().map(|f| f.desc()).collect());
    map
}

/// The lowest address of the synthetic stack.
const LO: usize = 0x100_0000;

/// One past its highest address.
const HI: usize = 0x200_0000;

/// An address outside every range: the entry thunk or a trampoline.
const OUTSIDE_CODE: usize = 0x90_0000;

/// A stack of `chain`'s frames, innermost first: frame 0 is in
/// `chain[0]`, which panicked; the record of frame k at `LO + 0x100 * (k +
/// 1)` holds frame k + 1's frame pointer and the return address into
/// `chain[k + 1]`, and the last one returns to `end`. The start is frame
/// 0's record and an address in `chain[0]`.
fn stack(chain: &[F], end: usize) -> (Words, Start) {
    let fp = |k: usize| LO.wrapping_add(0x100_usize.wrapping_mul(k.wrapping_add(1)));
    let mut words = Words::default();
    for k in 0..chain.len() {
        let next = k.wrapping_add(1);
        let ra = chain.get(next).map_or(end, |f| f.ra());
        words.0.insert(fp(k), fp(next));
        words.0.insert(fp(k).wrapping_add(8), ra);
    }
    let first = chain
        .first()
        .map_or(OUTSIDE_CODE, |f| f.start().wrapping_add(0x40));
    (
        words,
        Start::Frame {
            fp: fp(0),
            ra: first,
        },
    )
}

/// The backtrace of `chain` at the report's `location`.
fn text(chain: &[F], end: usize, location: &str, entry_base: bool) -> String {
    let (words, start) = stack(chain, end);
    render(&map(), &words, start, (LO, HI), location, entry_base)
}

/// D-1039's example: positions from the report and the call sites, a std
/// frame without one, a closure, `map`'s frame replaced by its tail call;
/// at an entry thunk the walk ends without a line.
#[test]
fn backtrace_walk_nested_calls_end_at_the_entry() {
    let chain = [F::Check, F::Closure, F::Fold, F::DoubleAll, F::Main];
    assert_eq!(
        text(&chain, OUTSIDE_CODE, "src/main.cheby:8:14", true),
        "backtrace:\n\
         \x20 in demo::check at src/main.cheby:8:14\n\
         \x20 in demo::double_all::{fn#0} at src/main.cheby:14:43\n\
         \x20 in std::list::fold\n\
         \x20 in demo::double_all at src/main.cheby:14:17\n\
         \x20 in demo::main at src/main.cheby:19:17\n"
    );
}

/// A spawned fiber or a kernel's callback: the walk reaches a trampoline,
/// and the last line says so (D-1039, D-526); so does a C-ABI function
/// such as a `@blocking` shim, and a frame record outside the stack.
#[test]
fn backtrace_walk_ends_outside_cheby_code() {
    let chain = [F::Check, F::Main];
    let expected = format!(
        "backtrace:\n  in demo::check at a.cheby:1:2\n  in demo::main at src/main.cheby:19:17\n{OUTSIDE}\n"
    );
    assert_eq!(text(&chain, OUTSIDE_CODE, "a.cheby:1:2", false), expected);
    assert_eq!(
        text(&chain, F::Blocking.ra(), "a.cheby:1:2", true),
        expected
    );
    // The last record's caller lies outside the stack.
    let (mut words, start) = stack(&chain, F::Main.ra());
    let last = LO + 0x100 * 2;
    words.0.insert(last, HI + 0x100);
    let walked = render(&map(), &words, start, (LO, HI), "a.cheby:1:2", true);
    assert_eq!(
        walked,
        format!(
            "backtrace:\n  in demo::check at a.cheby:1:2\n  in demo::main at src/main.cheby:19:17\n  in demo::main at src/main.cheby:19:17\n{OUTSIDE}\n"
        )
    );
}

/// No start, or a start outside generated code: the no-frames line alone
/// (D-1039, D-1040). A std frame 0 with the report's unknown location has
/// no position.
#[test]
fn backtrace_walk_without_a_start() {
    let words = Words::default();
    let lone = format!("backtrace:\n{NO_FRAMES}\n");
    assert_eq!(
        render(&map(), &words, Start::Unknown, (LO, HI), "a:1:1", true),
        lone
    );
    let outside = Start::Frame {
        fp: LO + 0x100,
        ra: OUTSIDE_CODE,
    };
    assert_eq!(
        render(&map(), &words, outside, (LO, HI), "a:1:1", true),
        lone
    );
    assert_eq!(start_at(0, 0), Start::Unknown);
    assert_eq!(start_at(LO, 0), Start::Unknown);
    assert_eq!(start_at(LO, 1), Start::Frame { fp: LO, ra: 1 });
    assert_eq!(
        text(&[F::Fold, F::Main], OUTSIDE_CODE, UNKNOWN_LOCATION, true),
        "backtrace:\n  in std::list::fold\n  in demo::main at src/main.cheby:19:17\n"
    );
}

/// Unnamed code is not shown, but the walk goes on past a Cheby-ABI shim
/// (D-1039); a large function's outer frame is not shown when its inner
/// body is the next frame inward, which is shown under its name (D-679).
#[test]
fn backtrace_walk_skips_shims_and_a_large_functions_outer_frame() {
    let chain = [F::Check, F::Shim, F::BigInner, F::BigOuter, F::Main];
    assert_eq!(
        text(&chain, OUTSIDE_CODE, "a.cheby:1:2", true),
        "backtrace:\n\
         \x20 in demo::check at a.cheby:1:2\n\
         \x20 in demo::big at src/main.cheby:30:5\n\
         \x20 in demo::main at src/main.cheby:19:17\n"
    );
    // An outer frame whose inner body is not the next frame is shown.
    let chain = [F::Check, F::BigOuter, F::Main];
    assert_eq!(
        text(&chain, OUTSIDE_CODE, "a.cheby:1:2", true),
        "backtrace:\n\
         \x20 in demo::check at a.cheby:1:2\n\
         \x20 in demo::big\n\
         \x20 in demo::main at src/main.cheby:19:17\n"
    );
}

/// More than three equal lines print three and the count of the rest;
/// three print as they are (D-1039).
#[test]
fn backtrace_walk_folds_equal_lines() {
    let mut chain = vec![F::Check];
    chain.extend([F::Rec; 10]);
    chain.push(F::Main);
    let rec = "  in demo::rec at src/main.cheby:40:3\n";
    assert_eq!(
        text(&chain, OUTSIDE_CODE, "a.cheby:1:2", true),
        format!(
            "backtrace:\n  in demo::check at a.cheby:1:2\n{}  (repeated 7 more times)\n  in demo::main at src/main.cheby:19:17\n",
            rec.repeat(FOLD)
        )
    );
    let mut chain = vec![F::Check];
    chain.extend([F::Rec; 3]);
    chain.push(F::Main);
    assert_eq!(
        text(&chain, OUTSIDE_CODE, "a.cheby:1:2", true),
        format!(
            "backtrace:\n  in demo::check at a.cheby:1:2\n{}  in demo::main at src/main.cheby:19:17\n",
            rec.repeat(3)
        )
    );
}

/// At most 100 frame lines print, counted after folding, and the rest is
/// one line (D-1039).
#[test]
fn backtrace_walk_caps_the_lines() {
    let mut chain = vec![F::Check];
    for _ in 0..80 {
        chain.extend([F::A, F::B]);
    }
    chain.push(F::Main);
    let walked = text(&chain, OUTSIDE_CODE, "a.cheby:1:2", true);
    let lines: Vec<&str> = walked.lines().collect();
    assert_eq!(lines.len(), 1 + MAX_LINES + 1, "{walked}");
    assert_eq!(lines.first(), Some(&"backtrace:"));
    assert_eq!(lines.get(1), Some(&"  in demo::check at a.cheby:1:2"));
    assert_eq!(lines.get(2), Some(&"  in demo::a at src/main.cheby:50:3"));
    // 1 + 160 + 1 frames, 100 printed.
    assert_eq!(lines.last(), Some(&"  (62 more frames)"));
}

/// The walk reads at most 4096 frame records: a deeper stack, such as a
/// stack overflow's, ends with `(more frames not walked)`, after its
/// folded run (D-1039, D-1042).
#[test]
fn backtrace_walk_caps_the_records() {
    let mut chain = vec![F::Check];
    chain.extend(std::iter::repeat_n(F::Rec, MAX_FRAMES + 10));
    chain.push(F::Main);
    let (words, start) = stack(&chain, OUTSIDE_CODE);
    let walked = render(&map(), &words, start, (LO, HI), "a.cheby:1:2", true);
    assert_eq!(
        walked,
        format!(
            "backtrace:\n  in demo::check at a.cheby:1:2\n{}  (repeated {} more times)\n{NOT_WALKED}\n",
            "  in demo::rec at src/main.cheby:40:3\n".repeat(FOLD),
            MAX_FRAMES - FOLD
        )
    );
}

/// D-1235: a run of equal lines that the line cap cuts adds the frames it
/// does not print to `(<n> more frames)` and prints no `(repeated …)`
/// line, whether the cap falls after its first line or its second; a run
/// whose three lines fill the cap folds as usual. A run that folds below
/// the cap leaves its `(repeated …)` line out of the count (D-1039: lines
/// are counted after folding).
#[test]
fn backtrace_walk_line_cap_cuts_a_run() {
    let head = "backtrace:\n  in demo::check at a.cheby:1:2\n";
    let a_line = "  in demo::a at src/main.cheby:50:3\n";
    let pair = format!("{a_line}  in demo::b at src/main.cheby:60:3\n");
    let rec = "  in demo::rec at src/main.cheby:40:3\n";
    let walk = |pairs: usize, lone_a: bool| {
        let mut chain = vec![F::Check];
        for _ in 0..pairs {
            chain.extend([F::A, F::B]);
        }
        if lone_a {
            chain.push(F::A);
        }
        chain.extend([F::Rec; 10]);
        chain.push(F::Main);
        let walked = text(&chain, OUTSIDE_CODE, "a.cheby:1:2", true);
        assert_eq!(
            walked.lines().filter(|l| l.starts_with("  in ")).count(),
            MAX_LINES,
            "{walked}"
        );
        walked
    };
    // The cap falls after the run's first line: its other 9 frames and
    // `main` are the 10 more frames.
    assert_eq!(
        walk(49, false),
        format!("{head}{}{rec}  (10 more frames)\n", pair.repeat(49))
    );
    // After its second line: 8 of the run and `main`.
    assert_eq!(
        walk(48, true),
        format!(
            "{head}{}{a_line}{}  (9 more frames)\n",
            pair.repeat(48),
            rec.repeat(2)
        )
    );
    // The run's three lines are lines 98 to 100: the run folds, and only
    // `main` is past the cap.
    assert_eq!(
        walk(48, false),
        format!(
            "{head}{}{}  (repeated 7 more times)\n  (1 more frames)\n",
            pair.repeat(48),
            rec.repeat(FOLD)
        )
    );
    // A run that folds below the cap: its `(repeated …)` line does not
    // count, so 1 + 3 + 96 lines fill the cap exactly.
    let mut chain = vec![F::Check];
    chain.extend([F::Rec; 10]);
    for _ in 0..48 {
        chain.extend([F::A, F::B]);
    }
    chain.push(F::Main);
    assert_eq!(
        text(&chain, OUTSIDE_CODE, "a.cheby:1:2", true),
        format!(
            "{head}{}  (repeated 7 more times)\n{}  (1 more frames)\n",
            rec.repeat(FOLD),
            pair.repeat(48)
        )
    );
}

/// D-1235: a walk that reaches both caps prints `(<n> more frames)`, then
/// `(more frames not walked)`. The count holds every frame the walk read
/// past the line cap, a run of equal lines that would have folded
/// included: 4096 records give 4097 frames with the start's, and 100 of
/// them print.
#[test]
fn backtrace_walk_reaches_both_caps() {
    let mut chain = vec![F::Check];
    for _ in 0..60 {
        chain.extend([F::A, F::B]);
    }
    chain.extend(std::iter::repeat_n(F::Rec, MAX_FRAMES));
    chain.push(F::Main);
    let walked = text(&chain, OUTSIDE_CODE, "a.cheby:1:2", true);
    let a_line = "  in demo::a at src/main.cheby:50:3\n";
    let pair = format!("{a_line}  in demo::b at src/main.cheby:60:3\n");
    let hidden = MAX_FRAMES + 1 - MAX_LINES;
    assert_eq!(hidden, 3997);
    assert_eq!(
        walked,
        format!(
            "backtrace:\n  in demo::check at a.cheby:1:2\n{}{a_line}  ({hidden} more frames)\n{NOT_WALKED}\n",
            pair.repeat(49)
        )
    );
}

/// D-1040's second start: the published record of the std function that
/// called the kernel, which is not shown; the first line is its caller at
/// the call that reached it.
#[test]
fn backtrace_walk_starts_at_the_wait_frame() {
    let (words, _) = stack(&[F::Fold, F::Main], OUTSIDE_CODE);
    let start = Start::Wait { fp: LO + 0x100 };
    assert_eq!(
        render(&map(), &words, start, (LO, HI), UNKNOWN_LOCATION, true),
        "backtrace:\n  in demo::main at src/main.cheby:19:17\n"
    );
}

/// A wait-frame word is used only when it lies in the stack above the
/// raising code's stack pointer: a stale word of a frame that returned
/// lies below it, and 0 is no word (D-1040).
#[test]
fn backtrace_walk_refuses_a_stale_wait_frame() {
    let sp = LO + 0x8000;
    assert_eq!(
        wait_start(sp + 0x100, sp, HI),
        Start::Wait { fp: sp + 0x100 }
    );
    assert_eq!(wait_start(sp - 0x100, sp, HI), Start::Unknown);
    assert_eq!(wait_start(sp, sp, HI), Start::Unknown);
    assert_eq!(wait_start(0, sp, HI), Start::Unknown);
    assert_eq!(wait_start(HI, sp, HI), Start::Unknown);
    assert_eq!(wait_start(sp + 0x108, sp, HI), Start::Unknown);
}

/// With the variable off nothing walks: a start that names no readable
/// memory is never read (D-1042). In constant evaluation nothing is
/// captured either; with it on, a panic in Rust code gets the no-frames
/// line.
#[test]
fn backtrace_walk_runs_nowhere_with_the_variable_off() {
    let fiber = ExecCtx::new(FiberInit::placeholder());
    let bogus = Start::Frame { fp: 0x10, ra: 0x20 };
    assert_eq!(capture_if(false, &fiber, bogus, "a:1:1"), None);
    let constants = ExecCtx::new(FiberInit {
        entry_mode: EntryMode::ConstEval(ConstEvalLimits::default()),
        ..FiberInit::placeholder()
    });
    assert_eq!(capture_if(true, &constants, bogus, "a:1:1"), None);
    assert_eq!(
        capture_if(true, &fiber, Start::Unknown, "a:1:1").as_deref(),
        Some(format!("backtrace:\n{NO_FRAMES}\n").as_str())
    );
}

/// The report's text holds its backtrace after its two lines, so a host
/// prints both in one write (D-1042); without one it is as before.
#[test]
fn backtrace_walk_prints_with_the_report() {
    let report = PanicReport::new(PanicKind::Explicit, Some("boom"), "a.cheby:1:2");
    assert_eq!(report.to_string(), "panicked at a.cheby:1:2:\nboom\n");
    assert_eq!(report.backtrace(), None);
    let text = format!("backtrace:\n{NO_FRAMES}\n");
    let report = report.with_backtrace(Some(text.clone().into_boxed_str()));
    assert_eq!(report.backtrace(), Some(text.as_str()));
    assert_eq!(
        report.to_string(),
        format!("panicked at a.cheby:1:2:\nboom\n{text}")
    );
}
