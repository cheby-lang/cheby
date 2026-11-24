//! The stepped intrinsics (`intrinsic.rs`): `list::sort_with` and
//! `std::heap`'s updates run their algorithm in steps and call the
//! comparison from generated code, never from a kernel; in release the
//! comparison is inlined into a clone of the loop.

use crate::golden_tests::check_golden;
use crate::test_support::{lower_source, lower_source_release};

const SOURCE: &str = "import std::heap
import std::int
import std::io
import std::list

type Job {
  rank: Int,
  name: String,
}

fn compare(a: Job, b: Job) -> Order {
  int::compare(a.rank, b.rank)
}

fn sorted(xs: List<Int>) -> List<Int> {
  list::sort_with(xs, fn(a, b) { int::compare(b, a) })
}

fn queued(h: heap::Heap<Job>, job: Job) -> heap::Heap<Job> {
  heap::push(h, job)
}

fn main() {
  let xs = sorted([3, 1, 2])
  let h = queued(queued(heap::new_min(), Job { rank: 2, name: \"b\" }), Job { rank: 1, name: \"a\" })
  let names = list::map(heap::to_list(h), fn(job) { job.name })
  let assert Some((first, _)) = heap::pop(heap::from_list([Job { rank: 3, name: \"c\" }]))
  let same = compare(Job { rank: 1, name: \"x\" }, Job { rank: 1, name: \"y\" })
  io::println(\"{xs:?} {names:?} {same:?} {first.name}\")
}
";

/// In debug the caller calls `sort_with`, and its body is the loop: the
/// start, a step per comparison, the entry check, the two elements read
/// from the pair, the comparison called through the uniform convention and
/// the finish. No kernel is called.
#[test]
fn sort_with_calls_its_comparison_from_its_loop() {
    let lowered = lower_source(&[("", SOURCE)]);
    let caller = lowered.function("app", "app::sorted");
    assert!(caller.contains("std::list::sort_with("), "{caller}");
    let body = lowered.function("std::list", "std::list::sort_with");
    check_golden("steps/sort_with_debug", &body);
    assert!(!body.contains("call_foreign"), "{body}");
    for step in [
        "call_runtime sort_start(",
        "call_runtime buffer_step(",
        "call_runtime loop_check()",
        "call_runtime pair_left(",
        "call_runtime pair_right(",
        "call_runtime buffer_finish(",
        "read_tag",
    ] {
        assert!(body.contains(step), "{step}:\n{body}");
    }
    assert_eq!(body.matches("call _").count(), 1, "{body}");
}

/// `heap::push`'s update keeps its steps in a stack slot and returns the
/// list its start returned; the first pair is read from the slot.
#[test]
fn a_heap_update_steps_in_a_stack_slot() {
    let lowered = lower_source(&[("", SOURCE)]);
    let body = lowered.function("std::heap", "std::heap::sys_push");
    check_golden("steps/heap_push_debug", &body);
    assert!(!body.contains("call_foreign"), "{body}");
    for step in [
        "call_runtime heap_slot()",
        "call_runtime heap_push(",
        "call_runtime heap_first(",
        "call_runtime heap_step(",
        "call_runtime loop_check()",
    ] {
        assert!(body.contains(step), "{step}:\n{body}");
    }
    for name in ["sys_pop", "sys_from_list", "sys_to_list"] {
        let other = lowered.function("std::heap", &format!("std::heap::{name}"));
        assert!(!other.contains("call_foreign"), "{name}:\n{other}");
        assert!(
            other.contains("call_runtime loop_check()"),
            "{name}:\n{other}"
        );
    }
}

/// In release a function with a loop is cloned, never inlined, so the
/// clone of `sort_with` for the caller's closure calls nothing in its
/// loop: the comparison is inlined. A heap's update is reached through
/// `std::heap`'s generic functions, which name `priv` helpers that make
/// closures, so no other unit clones them, and the update calls `compare`
/// through the uniform convention.
#[test]
fn release_inlines_the_comparison_into_a_clone_of_the_loop() {
    let lowered = lower_source_release(&[("", SOURCE)], false);
    let unit = lowered.unit("app");
    let clones: Vec<String> = unit
        .functions
        .iter()
        .map(|f| crate::dump_function(unit, f).unwrap())
        .filter(|text| text.contains("call_runtime loop_check()"))
        .collect();
    let sort = clones
        .iter()
        .find(|text| text.contains("of std::list::sort_with("))
        .unwrap_or_else(|| panic!("no clone of sort_with:\n{}", clones.join("\n")));
    check_golden("steps/sort_with_release", sort);
    assert!(!sort.contains("call _"), "{sort}");
    assert!(!sort.contains("call <closure"), "{sort}");
    assert!(!sort.contains("call app::"), "{sort}");
    let push = lowered.function("std::heap", "std::heap::sys_push");
    assert!(!push.contains("call_foreign"), "{push}");
    assert_eq!(push.matches("call _").count(), 1, "{push}");
}
