//! Tests of the phase timer (D-974, GAP(phases-format)): the file's
//! format, exclusive spans, `add` moving time, restarts and the unset
//! variable.

use std::time::{Duration, Instant};

use crate::phases::{Phase, Recorder, target_from};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn nanos(n: u64) -> i128 {
    i128::from(n).saturating_mul(1_000_000)
}

fn recorder(start: Instant) -> Recorder {
    Recorder::new(std::thread::current().id(), start)
}

/// The phase lines of `text` as `(name, nanoseconds)`, and `command`'s.
fn read_file(text: &str) -> (Vec<(String, u128)>, u128) {
    let mut phases = Vec::new();
    let mut command = 0;
    for line in text.lines().skip(1) {
        let mut fields = line.split(' ');
        let (Some(name), Some(value)) = (fields.next(), fields.next()) else {
            panic!("a malformed line `{line}`");
        };
        let value: u128 = value.parse().unwrap_or(u128::MAX);
        if name == "command" {
            command = value;
        } else {
            phases.push((name.to_owned(), value));
        }
    }
    (phases, command)
}

#[test]
fn phases_unset_or_empty_names_no_file() {
    assert_eq!(target_from(None), None);
    assert_eq!(target_from(Some("".into())), None);
    assert_eq!(
        target_from(Some("/tmp/p.txt".into())),
        Some(std::path::PathBuf::from("/tmp/p.txt"))
    );
}

#[test]
fn phases_format_lists_the_phases_that_ran_in_order() {
    let t0 = Instant::now();
    let mut r = recorder(t0);
    let check = r.open(Phase::Check, t0 + ms(1));
    r.close(check, t0 + ms(3));
    let parse = r.open(Phase::Parse, t0 + ms(3));
    r.close(parse, t0 + ms(4));
    let parse = r.open(Phase::Parse, t0 + ms(5));
    r.close(parse, t0 + ms(6));
    let text = r.render("check", t0 + ms(10));
    assert_eq!(
        text,
        "cheby-phases 1 check\nparse 2000000 2\ncheck 2000000 1\ncommand 10000000\n"
    );
    assert!(text.is_ascii());
}

#[test]
fn phases_are_exclusive_and_add_up_to_at_most_command() {
    let t0 = Instant::now();
    let mut r = recorder(t0);
    // check [1, 9) with parse [2, 5) inside, itself with std [3, 4).
    let check = r.open(Phase::Check, t0 + ms(1));
    let parse = r.open(Phase::Parse, t0 + ms(2));
    let std = r.open(Phase::Std, t0 + ms(3));
    r.close(std, t0 + ms(4));
    r.close(parse, t0 + ms(5));
    r.close(check, t0 + ms(9));
    assert_eq!(r.total(Phase::Std).nanos, nanos(1));
    assert_eq!(r.total(Phase::Parse).nanos, nanos(2));
    assert_eq!(r.total(Phase::Check).nanos, nanos(5));
    let (phases, command) = read_file(&r.render("run", t0 + ms(10)));
    let sum: u128 = phases.iter().map(|(_, n)| n).sum();
    assert_eq!(sum, 8_000_000);
    assert!(sum <= command);
}

#[test]
fn phases_add_moves_time_from_the_enclosing_span() {
    let t0 = Instant::now();
    let with = {
        let mut r = recorder(t0);
        let register = r.open(Phase::Register, t0);
        r.add(Phase::Codegen, ms(3));
        r.add(Phase::Declare, ms(2));
        r.close(register, t0 + ms(10));
        r
    };
    let without = {
        let mut r = recorder(t0);
        let register = r.open(Phase::Register, t0);
        r.close(register, t0 + ms(10));
        r
    };
    // The enclosing phase shrinks by exactly the time added.
    assert_eq!(
        without
            .total(Phase::Register)
            .nanos
            .saturating_sub(with.total(Phase::Register).nanos),
        nanos(5)
    );
    assert_eq!(with.total(Phase::Codegen).nanos, nanos(3));
    assert_eq!(with.total(Phase::Declare).nanos, nanos(2));
    // Nothing counts twice: the three together are the span's time.
    let all: i128 = [Phase::Register, Phase::Codegen, Phase::Declare]
        .into_iter()
        .map(|phase| with.total(phase).nanos)
        .sum();
    assert_eq!(all, nanos(10));
}

#[test]
fn phases_add_without_a_span_only_adds() {
    let t0 = Instant::now();
    let mut r = recorder(t0);
    r.add(Phase::Finalize, ms(4));
    assert_eq!(r.total(Phase::Finalize).nanos, nanos(4));
    assert_eq!(r.total(Phase::Finalize).spans, 1);
}

#[test]
fn phases_restart_is_the_first_attempts_elapsed_time() {
    let t0 = Instant::now();
    let mut r = recorder(t0);
    let hash = r.open(Phase::Hash, t0);
    r.close(hash, t0 + ms(1));
    // The first attempt [2, 9): parse 3 ms and check 2 ms inside it.
    let mark = r.mark(t0 + ms(2));
    let parse = r.open(Phase::Parse, t0 + ms(3));
    r.close(parse, t0 + ms(6));
    let check = r.open(Phase::Check, t0 + ms(6));
    r.close(check, t0 + ms(8));
    r.restarted(&mark, t0 + ms(9));
    // The second attempt: parse 1 ms.
    let parse = r.open(Phase::Parse, t0 + ms(9));
    r.close(parse, t0 + ms(10));
    assert_eq!(r.total(Phase::Restart).nanos, nanos(7));
    assert_eq!(r.total(Phase::Restart).spans, 1);
    assert_eq!(r.total(Phase::Parse).nanos, nanos(1));
    assert_eq!(r.total(Phase::Parse).spans, 1);
    assert_eq!(r.total(Phase::Check).nanos, 0);
    assert_eq!(r.total(Phase::Hash).nanos, nanos(1));
}

#[test]
fn phases_nested_restarts_count_the_outer_attempt_once() {
    let t0 = Instant::now();
    let mut r = recorder(t0);
    let outer = r.mark(t0);
    let inner = r.mark(t0 + ms(1));
    let parse = r.open(Phase::Parse, t0 + ms(1));
    r.close(parse, t0 + ms(3));
    r.restarted(&inner, t0 + ms(4));
    let check = r.open(Phase::Check, t0 + ms(4));
    r.close(check, t0 + ms(6));
    r.restarted(&outer, t0 + ms(7));
    assert_eq!(r.total(Phase::Restart).nanos, nanos(7));
    assert_eq!(r.total(Phase::Parse).nanos, 0);
    assert_eq!(r.total(Phase::Check).nanos, 0);
}

#[test]
fn phases_restart_inside_an_open_span_moves_its_share() {
    let t0 = Instant::now();
    let mut r = recorder(t0);
    let outer = r.open(Phase::Lower, t0);
    let mark = r.mark(t0 + ms(2));
    let parse = r.open(Phase::Parse, t0 + ms(3));
    r.close(parse, t0 + ms(4));
    r.restarted(&mark, t0 + ms(6));
    r.close(outer, t0 + ms(10));
    assert_eq!(r.total(Phase::Restart).nanos, nanos(4));
    assert_eq!(r.total(Phase::Lower).nanos, nanos(6));
    assert_eq!(r.total(Phase::Parse).nanos, 0);
}

#[test]
fn phases_teardown_is_the_rest_after_main() {
    let t0 = Instant::now();
    let mut r = recorder(t0);
    let main = r.open(Phase::Main, t0 + ms(1));
    r.close(main, t0 + ms(5));
    let text = r.render("run", t0 + ms(8));
    assert_eq!(
        text,
        "cheby-phases 1 run\nmain 4000000 1\nteardown 3000000 1\ncommand 8000000\n"
    );
}

#[test]
fn phases_nothing_is_recorded_or_written_when_unset() {
    // The test binary never sets `CHEBY_PHASES`: the spans and `finish`
    // record nothing and write nothing.
    if std::env::var_os("CHEBY_PHASES").is_some() {
        return;
    }
    let _span = crate::phases::span(Phase::Check);
    crate::phases::add(Phase::Codegen, ms(1));
    crate::phases::begin_attempt();
    crate::phases::attempt_restarted();
    crate::phases::finish("check");
    drop(crate::phases::command("check"));
    assert!(crate::phases::recorded_nothing());
}
