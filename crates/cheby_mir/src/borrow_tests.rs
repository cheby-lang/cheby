//! Borrowing inference on lowered MIR of small packages (D-715, D-779):
//! eligibility, the handle-free predicate, profitability and the
//! tail-call fixed point ([`crate::borrow`]).

use std::collections::BTreeMap;

use crate::borrow::{BorrowMask, BuildFacts, collect_facts};
use crate::dump;
use crate::test_support::{
    lower_package_inferred, lower_source, lower_source_inferred, lower_source_uncounted,
    lower_source_with_tests_uncounted,
};

/// The masks of a one-module package `app` built for `cheby run`.
fn masks(source: &str) -> BTreeMap<String, BorrowMask> {
    lower_source_inferred(&[("", source)], false).masks
}

/// The masks of a one-module package `app` built for `cheby test`.
fn test_masks(source: &str) -> BTreeMap<String, BorrowMask> {
    lower_source_inferred(&[("", source)], true).masks
}

/// The mask of parameters `params` borrowed.
fn mask(params: &[usize]) -> BorrowMask {
    params.iter().fold(BorrowMask::OWNED, |m, &i| m.with(i))
}

/// The mask `masks` gives `path`; all-owned when it is absent.
fn of(masks: &BTreeMap<String, BorrowMask>, path: &str) -> BorrowMask {
    masks.get(path).copied().unwrap_or(BorrowMask::OWNED)
}

const TREE: &str = "
type Tree {
  Leaf,
  Node(Tree, Int, Tree),
}

fn build(n: Int) -> Tree {
  case n {
    0 => Leaf
    _ => Node(build(n - 1), n, Leaf)
  }
}
";

#[test]
fn borrow_mask_bits() {
    let m = BorrowMask::OWNED.with(0).with(3).with(64).with(usize::MAX);
    assert_eq!(m, BorrowMask(0b1001));
    assert!(m.is_borrowed(0) && m.is_borrowed(3));
    assert!(!m.is_borrowed(1) && !m.is_borrowed(64) && !m.is_borrowed(usize::MAX));
    assert_eq!(m.without(0), BorrowMask(0b1000));
    assert_eq!(m.indices().collect::<Vec<_>>(), [0, 3]);
    assert!(BorrowMask::default().is_owned());
    assert!(BorrowMask::OWNED.with(63).is_borrowed(63));
}

#[test]
fn borrow_tree_in_sum_and_depth_is_borrowed() {
    let source = format!(
        "import std::int
import std::io
{TREE}
fn main() {{
  let tree = build(10)
  let total = sum(tree)
  let deepest = depth(tree)
  io::println(\"{{total}} {{deepest}}\")
}}

priv fn sum(tree: Tree) -> Int {{
  case tree {{
    Leaf => 0
    Node(left, key, right) => sum(left) + key + sum(right)
  }}
}}

priv fn depth(tree: Tree) -> Int {{
  case tree {{
    Leaf => 0
    Node(left, _, right) => 1 + int::max(depth(left), depth(right))
  }}
}}
"
    );
    let m = masks(&source);
    assert_eq!(of(&m, "app::sum"), mask(&[0]), "{m:?}");
    assert_eq!(of(&m, "app::depth"), mask(&[0]), "{m:?}");
    // `build`'s only parameter is a raw `Int`, which is not counted.
    assert_eq!(of(&m, "app::build"), BorrowMask::OWNED);
    // Counted as a build counts it: the subtrees are borrowed projections
    // (D-807) passed on borrowed, so `sum` has no
    // `dup` at all and drops only its calls' boxed results.
    let built = lower_source(&[("", source.as_str())]);
    let sum = built.function("app", "app::sum");
    assert!(sum.starts_with("fn app::sum(borrow _0: value)"), "{sum}");
    assert!(!sum.contains("dup "), "{sum}");
    for subtree in ["_0", "_3", "_5"] {
        assert!(!sum.contains(&format!("drop {subtree}\n")), "{sum}");
    }
    assert!(sum.contains("call app::sum(borrow _3)"), "{sum}");
    assert!(sum.contains("call app::sum(borrow _5)"), "{sum}");
    // `main` keeps the tree across both calls and releases it once.
    let main = built.function("app", "app::main");
    assert!(main.contains("call app::sum(borrow _"), "{main}");
    assert!(main.contains("call app::depth(borrow _"), "{main}");
}

#[test]
fn borrow_pub_function_is_never_borrowed() {
    let m = masks(
        "import std::io

fn main() {
  let a = check(\"a\")
  let b = inner(\"b\")
  io::println(\"{a} {b}\")
}

pub fn check(s: String) -> Bool {
  s == \"x\"
}

priv fn inner(s: String) -> Bool {
  s == \"x\"
}
",
    );
    assert_eq!(of(&m, "app::check"), BorrowMask::OWNED, "{m:?}");
    assert_eq!(of(&m, "app::inner"), mask(&[0]), "{m:?}");
}

#[test]
fn borrow_package_visible_function_is_borrowed() {
    // D-787: not `pub` is enough.
    let files = [(
        "",
        "import std::io

fn main() {
  let a = check(\"a\")
  io::println(\"{a}\")
}

fn check(s: String) -> Bool {
  s == \"x\"
}
",
    )];
    let m = lower_source_inferred(&files, false).masks;
    assert_eq!(of(&m, "app::check"), mask(&[0]), "{m:?}");
}

#[test]
fn borrow_package_visible_function_named_by_another_module_borrows() {
    // D-787 refines D-250: a package-visible function that another module
    // of the package calls may borrow, and that module's code calls it
    // with its mask, in its callee declaration (the ABI signature and the
    // slot key, D-715) and at the call site.
    let files = [
        (
            "",
            "import std::io
import app::util

fn main() {
  let a = util::check(\"a\")
  let b = util::local(\"b\")
  io::println(\"{a} {b}\")
}
",
        ),
        (
            "util",
            "pub fn local(s: String) -> Bool {
  let checked = check(s)
  let helped = helper(s)
  checked && helped
}

fn check(s: String) -> Bool {
  s == \"x\"
}

fn helper(s: String) -> Bool {
  s == \"y\"
}
",
        ),
    ];
    let m = lower_package_inferred("app", &files, false).unwrap().masks;
    assert_eq!(of(&m, "app::util::check"), mask(&[0]), "{m:?}");
    assert_eq!(of(&m, "app::util::helper"), mask(&[0]), "{m:?}");
    // `local` is `pub`: owned.
    assert_eq!(of(&m, "app::util::local"), BorrowMask::OWNED, "{m:?}");
    let built = crate::test_support::lower_package("app", &files).unwrap();
    let app = dump(built.unit("app")).unwrap();
    assert!(
        app.contains("callee app::util::check(borrow value) -> value"),
        "{app}"
    );
    // The literal is passed as the constant itself: nothing to release.
    assert!(
        app.contains("call app::util::check(borrow const str#"),
        "{app}"
    );
    let util = dump(built.unit("app::util")).unwrap();
    assert!(
        util.contains("fn app::util::check(borrow _0: value)"),
        "{util}"
    );
}

#[test]
fn borrow_function_used_as_value_in_its_code_is_owned() {
    let m = masks(
        "import std::io
import std::list

fn main() {
  let lengths = list::map([\"a\", \"bb\"], size)
  let c = other(\"c\")
  io::println(\"{lengths} {c}\")
}

priv fn size(s: String) -> Int {
  case s == \"a\" {
    True => 1
    False => 2
  }
}

priv fn other(s: String) -> Int {
  case s == \"a\" {
    True => 1
    False => 2
  }
}
",
    );
    assert_eq!(of(&m, "app::size"), BorrowMask::OWNED, "{m:?}");
    assert_eq!(of(&m, "app::other"), mask(&[0]), "{m:?}");
}

#[test]
fn borrow_function_used_as_value_in_tests_unit_is_owned() {
    let source = "import std::io
import std::list

fn main() {
  let a = size(\"a\")
  io::println(\"{a}\")
}

priv fn size(s: String) -> Int {
  case s == \"a\" {
    True => 1
    False => 2
  }
}

test \"sizes\" {
  assert list::map([\"a\"], size) == [1]
}
";
    assert_eq!(of(&masks(source), "app::size"), mask(&[0]));
    let with_tests = test_masks(source);
    assert_eq!(
        of(&with_tests, "app::size"),
        BorrowMask::OWNED,
        "{with_tests:?}"
    );
}

#[test]
fn borrow_show_of_main_error_in_entries_unit_is_owned() {
    // `main`'s error type is a list, so the entries unit holds a static
    // dictionary with `show`'s value shim (D-523): `show` is used as a
    // value there, and nowhere else.
    let source = "type Oops {
  Oops(String),
}

fn main() -> Result<Nil, List<Oops>> {
  case describe(Oops(\"x\")) == \"x\" {
    True => Ok(Nil)
    False => Err([Oops(\"no\")])
  }
}

fn show(error: Oops) -> String {
  let text = describe(error)
  text
}

priv fn describe(error: Oops) -> String {
  case error {
    Oops(text) => \"oops: {text}\"
  }
}
";
    let units = lower_source_uncounted(&[("", source)]).units;
    let refs: Vec<_> = units.iter().collect();
    let facts: BuildFacts = collect_facts(&refs);
    let entries = units.last().unwrap();
    assert_eq!(entries.unit, crate::UnitKind::Entries);
    let show = entries
        .functions
        .iter()
        .find_map(|f| match f.key {
            crate::FnKey::ValueShim(def) => Some(def),
            _ => None,
        })
        .expect("the entries unit holds `show`'s value shim");
    assert!(facts.used_as_value(show));
    let m = lower_package_inferred("app", &[("", source)], false)
        .unwrap()
        .masks;
    assert_eq!(of(&m, "app::show"), BorrowMask::OWNED, "{m:?}");
    assert_eq!(of(&m, "app::describe"), mask(&[0]), "{m:?}");
}

#[test]
fn borrow_external_wrapper_is_never_borrowed() {
    let m = masks(
        "import std::io

fn main() {
  let a = measure(\"a\")
  let b = inner(\"b\")
  io::println(\"{a} {b}\")
}

@external(native, \"app_measure\")
priv fn measure(s: String) -> Int {
  0
}

priv fn inner(s: String) -> Bool {
  s == \"x\"
}
",
    );
    assert_eq!(of(&m, "app::measure"), BorrowMask::OWNED, "{m:?}");
    assert_eq!(of(&m, "app::inner"), mask(&[0]), "{m:?}");
}

#[test]
fn borrow_sender_parameter_is_never_borrowed() {
    let m = masks(
        "import std::channel
import std::io

type Job {
  Job(String, channel::Sender<Int>),
}

fn main() {
  let (tx, _rx) = channel::new(1)
  let a = same(tx, tx)
  let b = named(Job(\"a\", tx))
  io::println(\"{a} {b}\")
}

priv fn same(a: channel::Sender<Int>, b: channel::Sender<Int>) -> Bool {
  a == b
}

priv fn named(job: Job) -> Bool {
  case job {
    Job(name, _) => name == \"a\"
  }
}
",
    );
    assert_eq!(of(&m, "app::same"), BorrowMask::OWNED, "{m:?}");
    // A type with a handle inside is not handle-free either.
    assert_eq!(of(&m, "app::named"), BorrowMask::OWNED, "{m:?}");
}

#[test]
fn borrow_fn_and_dyn_parameters_are_never_borrowed() {
    let m = masks(
        "import std::io

type Op {
  Op(fn(Int) -> Int),
}

fn main() {
  let f = fn(x) { x + 1 }
  let a = apply(f, 1)
  let b = run(Op(f))
  let c = text(\"a\", 1)
  io::println(\"{a} {b} {c}\")
}

priv fn apply(f: fn(Int) -> Int, x: Int) -> Int {
  f(x) + 1
}

priv fn run(op: Op) -> Bool {
  case op {
    Op(_) => True
  }
}

priv fn text(s: String, n: Int) -> String {
  \"{s}{n}\"
}
",
    );
    assert_eq!(of(&m, "app::apply"), BorrowMask::OWNED, "{m:?}");
    assert_eq!(of(&m, "app::run"), BorrowMask::OWNED, "{m:?}");
    // A raw `Int` is never counted, so never in a mask.
    assert_eq!(of(&m, "app::text"), mask(&[0]), "{m:?}");
}

#[test]
fn borrow_type_variable_and_dictionary_parameters_are_never_borrowed() {
    let m = masks(
        "import std::io

fn main() {
  let a = count([1, 2])
  let b = label(1, \"n\")
  io::println(\"{a} {b}\")
}

priv fn count<T>(xs: List<T>) -> Int {
  case xs {
    [] => 0
    [_] => 1
    _ => 2
  }
}

priv fn label<T: Show>(x: T, name: String) -> String {
  \"{name}: {x}\"
}
",
    );
    assert_eq!(of(&m, "app::count"), BorrowMask::OWNED, "{m:?}");
    // The dictionary is parameter 0, `x` is 1 and `name` is 2.
    assert_eq!(of(&m, "app::label"), mask(&[2]), "{m:?}");
}

#[test]
fn borrow_handle_free_adts_and_generic_instances() {
    let m = masks(
        "import std::channel
import std::io

type Pair<A> {
  Pair(A, A),
}

type Chain {
  End,
  Link(String, Chain),
}

fn main() {
  let (tx, _rx) = channel::new(1)
  let a = ints(Pair(1, 2))
  let b = senders(Pair(tx, tx))
  let c = chain(Link(\"a\", End))
  io::println(\"{a} {b} {c}\")
}

priv fn ints(p: Pair<Int>) -> Bool {
  case p {
    Pair(a, b) => a == b
  }
}

priv fn senders(p: Pair<channel::Sender<Int>>) -> Bool {
  case p {
    Pair(a, b) => a == b
  }
}

priv fn chain(c: Chain) -> Bool {
  case c {
    End => True
    Link(s, _) => s == \"a\"
  }
}
",
    );
    assert_eq!(of(&m, "app::ints"), mask(&[0]), "{m:?}");
    assert_eq!(of(&m, "app::senders"), BorrowMask::OWNED, "{m:?}");
    assert_eq!(of(&m, "app::chain"), mask(&[0]), "{m:?}");
}

#[test]
fn borrow_self_tail_call_keeps_or_drops_borrowing() {
    let m = masks(
        "import std::io

fn main() {
  let a = keep(\"a\", 3)
  let b = constant(\"a\", 3)
  let c = fresh(\"a\", 3)
  io::println(\"{a} {b} {c}\")
}

priv fn keep(s: String, n: Int) -> Int {
  case {
    n == 0 => 0
    s == \"stop\" => 1
    _ => keep(s, n - 1)
  }
}

priv fn constant(s: String, n: Int) -> Int {
  case {
    n == 0 => 0
    s == \"stop\" => 1
    _ => constant(\"stop\", n - 1)
  }
}

priv fn fresh(s: String, n: Int) -> Int {
  case {
    n == 0 => 0
    s == \"stop\" => 1
    _ => fresh(\"{n}\", n - 1)
  }
}
",
    );
    assert_eq!(of(&m, "app::keep"), mask(&[0]), "{m:?}");
    assert_eq!(of(&m, "app::constant"), mask(&[0]), "{m:?}");
    // An owned argument in a tail call: the position becomes owned.
    assert_eq!(of(&m, "app::fresh"), BorrowMask::OWNED, "{m:?}");
}

#[test]
fn borrow_mutual_recursion_stays_sound() {
    let m = masks(
        "import std::io

fn main() {
  let a = even(\"a\", 4)
  let b = ping(\"a\", 4)
  io::println(\"{a} {b}\")
}

priv fn even(s: String, n: Int) -> Bool {
  case n {
    0 => s == \"a\"
    _ => odd(s, n - 1)
  }
}

priv fn odd(s: String, n: Int) -> Bool {
  case n {
    0 => s == \"b\"
    _ => even(\"{n}\", n - 1)
  }
}

priv fn ping(s: String, n: Int) -> Bool {
  case n {
    0 => s == \"a\"
    _ => pong(s, n - 1)
  }
}

priv fn pong(s: String, n: Int) -> Bool {
  case n {
    0 => s == \"b\"
    _ => ping(s, n - 1)
  }
}
",
    );
    // `odd` passes an owned string to `even`, so `even` owns `s`, and
    // then `even`'s owned `s` cannot fill `odd`'s borrowed position.
    assert_eq!(of(&m, "app::even"), BorrowMask::OWNED, "{m:?}");
    assert_eq!(of(&m, "app::odd"), BorrowMask::OWNED, "{m:?}");
    assert_eq!(of(&m, "app::ping"), mask(&[0]), "{m:?}");
    assert_eq!(of(&m, "app::pong"), mask(&[0]), "{m:?}");
}

#[test]
fn borrow_pub_caller_tail_call_makes_position_owned() {
    let m = masks(
        "import std::io

fn main() {
  let a = entry(\"a\")
  let b = inner(\"b\")
  io::println(\"{a} {b}\")
}

pub fn entry(s: String) -> Bool {
  helper(s)
}

priv fn helper(s: String) -> Bool {
  s == \"x\"
}

priv fn inner(s: String) -> Bool {
  s == \"x\"
}
",
    );
    assert_eq!(of(&m, "app::helper"), BorrowMask::OWNED, "{m:?}");
    assert_eq!(of(&m, "app::inner"), mask(&[0]), "{m:?}");
}

#[test]
fn borrow_test_tail_call_makes_position_owned() {
    let source = "import std::io

fn main() {
  check(\"a\")
  io::println(\"checked\")
}

priv fn check(s: String) -> Nil {
  assert s != \"\"
}

test \"checks\" {
  let one = 1
  check(\"{one}\")
}
";
    let units = lower_source_with_tests_uncounted(&[("", source)]).units;
    let refs: Vec<_> = units.iter().collect();
    let facts = collect_facts(&refs);
    let check = units
        .iter()
        .find(|u| u.unit == crate::UnitKind::Tests(u.module().unwrap()))
        .and_then(|u| {
            u.callees
                .iter()
                .map(|c| c.def)
                .find(|&d| u.name(d).is_ok_and(|n| n.path() == "app::check"))
        })
        .expect("the tests unit calls `check`");
    assert_eq!(facts.tail_owned(check), mask(&[0]));
    assert_eq!(of(&masks(source), "app::check"), mask(&[0]));
    assert_eq!(of(&test_masks(source), "app::check"), BorrowMask::OWNED);
}

#[test]
fn borrow_list_slice_walk_stays_owned() {
    let m = masks(
        "import std::io

fn main() {
  let a = total([1, 2], 0)
  let b = first([1, 2])
  io::println(\"{a} {b}\")
}

priv fn total(xs: List<Int>, acc: Int) -> Int {
  case xs {
    [] => acc
    [x, ..rest] => total(rest, acc + x)
  }
}

priv fn first(xs: List<Int>) -> Int {
  case xs {
    [] => 0
    [x, ..] => x
  }
}
",
    );
    assert_eq!(of(&m, "app::total"), BorrowMask::OWNED, "{m:?}");
    assert_eq!(of(&m, "app::first"), mask(&[0]), "{m:?}");
}

#[test]
fn borrow_bench_gen_sum_stays_owned() {
    // bench/gen's shape (`bench/gen/src/lib.rs`, `sum_{unit}`), with the
    // helper made `priv` so that only rules 6 and 7 decide.
    let m = masks(
        "import std::io
import std::list

fn main() {
  let a = total_0()
  io::println(\"{a}\")
}

/// Sums a fixed list of numbers and adds its length.
pub fn total_0() -> Int {
  let items = [1, 2, 3]
  sum_0(items, list::length(items))
}

priv fn sum_0(items: List<Int>, acc: Int) -> Int {
  case items {
    [] => acc
    [first, ..rest] => sum_0(rest, acc + first)
  }
}
",
    );
    assert_eq!(of(&m, "app::sum_0"), BorrowMask::OWNED, "{m:?}");
}

#[test]
fn borrow_consuming_uses_make_the_parameter_owned() {
    let m = masks(
        "import std::io
import std::list

fn main() {
  let a = same(\"a\")
  let b = wrap(\"b\")
  let c = size([1])
  let d = outer(\"c\")
  let e = pass(\"d\")
  io::println(\"{a} {b} {c} {d} {e}\")
}

priv fn same(s: String) -> String {
  s
}

priv fn wrap(s: String) -> Option<String> {
  Some(s)
}

priv fn size(xs: List<Int>) -> Int {
  list::length(xs)
}

priv fn outer(s: String) -> Bool {
  inner(s) && s == \"c\"
}

priv fn inner(s: String) -> Bool {
  s == \"x\"
}

priv fn pass(s: String) -> Bool {
  keeps(s) == \"d\"
}

priv fn keeps(s: String) -> String {
  s
}
",
    );
    assert_eq!(of(&m, "app::same"), BorrowMask::OWNED, "{m:?}");
    assert_eq!(of(&m, "app::wrap"), BorrowMask::OWNED, "{m:?}");
    assert_eq!(of(&m, "app::size"), BorrowMask::OWNED, "{m:?}");
    // A non-tail call into a borrowed position does not consume.
    assert_eq!(of(&m, "app::outer"), mask(&[0]), "{m:?}");
    assert_eq!(of(&m, "app::inner"), mask(&[0]), "{m:?}");
    // An owned position does.
    assert_eq!(of(&m, "app::keeps"), BorrowMask::OWNED, "{m:?}");
    assert_eq!(of(&m, "app::pass"), BorrowMask::OWNED, "{m:?}");
}

#[test]
fn borrow_projection_fills_a_borrowed_tail_position() {
    let source = format!(
        "import std::io
{TREE}
fn main() {{
  let a = rightmost(build(3))
  let b = copy(build(3))
  io::println(\"{{a}} {{b}}\")
}}

priv fn rightmost(tree: Tree) -> Int {{
  case tree {{
    Leaf => 0
    Node(_, key, Leaf) => key
    Node(_, _, right) => rightmost(right)
  }}
}}

priv fn copy(tree: Tree) -> Int {{
  case tree {{
    Leaf => 0
    Node(left, _, _) => copy(Node(left, 0, Leaf))
  }}
}}
"
    );
    let m = masks(&source);
    assert_eq!(of(&m, "app::rightmost"), mask(&[0]), "{m:?}");
    assert_eq!(of(&m, "app::copy"), BorrowMask::OWNED, "{m:?}");
}

#[test]
fn borrow_inference_is_deterministic_and_changes_no_mir() {
    let source = format!(
        "import std::io
{TREE}
fn main() {{
  let total = sum(build(4))
  io::println(\"{{total}}\")
}}

priv fn sum(tree: Tree) -> Int {{
  case tree {{
    Leaf => 0
    Node(left, key, right) => sum(left) + key + sum(right)
  }}
}}
"
    );
    let files = [("", source.as_str())];
    let first = lower_source_inferred(&files, false);
    let second = lower_source_inferred(&files, false);
    assert_eq!(first.masks, second.masks);
    assert!(!first.masks.is_empty());
    // The analysis is pure: the units are what lowering gave.
    assert_eq!(first.units, lower_source_uncounted(&files).units);
    // And counting is untouched: `lower_source` still counts every unit.
    let counted = lower_source(&files);
    assert!(counted.dump().contains("drop "));
}

// The informative report of the masks on the examples, the RC programs
// and std (`$S3/P1/p1a-masks.md`), for the wiring's starting data.

/// A program of the report: its package name and its `(module path under
/// the package, source)` files, `""` being the root module.
fn program(dir: &std::path::Path) -> Option<(String, Vec<(String, String)>)> {
    let manifest = std::fs::read_to_string(dir.join("cheby.toml")).ok()?;
    let name = manifest.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("name")?.trim().strip_prefix('=')?;
        Some(rest.trim().trim_matches('"').to_owned())
    })?;
    let mut files = Vec::new();
    walk_sources(&dir.join("src"), "", &mut files);
    Some((name, files))
}

/// The `.cheby` files under `dir`, by module path under `prefix`.
fn walk_sources(dir: &std::path::Path, prefix: &str, out: &mut Vec<(String, String)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let full = if prefix.is_empty() {
            stem.clone()
        } else {
            format!("{prefix}::{stem}")
        };
        if path.is_dir() {
            walk_sources(&path, &full, out);
        } else if path.extension().is_some_and(|e| e == "cheby") {
            let module = if prefix.is_empty() && stem == "main" {
                String::new()
            } else {
                full
            };
            out.push((module, std::fs::read_to_string(&path).unwrap()));
        }
    }
}

/// One more for `path`.
fn bump(out: &mut BTreeMap<String, u32>, path: &str, by: u32) {
    let entry = out.entry(path.to_owned()).or_default();
    *entry = entry.saturating_add(by);
}

/// The mask `masks` gives the callee `def` of `unit`, with its path.
fn callee_mask(
    unit: &crate::MirModule,
    def: cheby_iface::DefId,
    masks: &BTreeMap<String, BorrowMask>,
) -> Option<(String, BorrowMask)> {
    let path = unit.name(def).ok()?.path();
    masks.get(&path).map(|&m| (path, m))
}

/// Records the owned uses among `uses`.
fn note_owned(
    owned_use: &mut std::collections::BTreeSet<crate::Local>,
    uses: Vec<(crate::Local, crate::UseKind)>,
) {
    for (l, kind) in uses {
        if kind == crate::UseKind::Owned {
            owned_use.insert(l);
        }
    }
}

/// Records the arguments of a direct call that the callee does not
/// borrow as owned uses.
fn note_call(
    owned_use: &mut std::collections::BTreeSet<crate::Local>,
    callee: BorrowMask,
    args: &[crate::Operand],
) {
    for (j, arg) in args.iter().enumerate() {
        if let crate::Operand::Local(l) = arg
            && !callee.is_borrowed(j)
        {
            owned_use.insert(*l);
        }
    }
}

/// Static `dup`/`drop` pairs the masks would save, per borrowing
/// function, estimated from today's counted MIR: a `dup` of an argument
/// right before a call that borrows its position (the callee's `drop` of
/// it goes too), and a projection of a borrowed parameter whose uses are
/// all borrowed (its `dup` and `drop`).
fn saved_pairs(
    counted: &[crate::MirModule],
    uncounted: &[crate::MirModule],
    masks: &BTreeMap<String, BorrowMask>,
) -> BTreeMap<String, u32> {
    let mut out = caller_dups_saved(counted, masks);
    for (path, n) in projections_saved(uncounted, masks) {
        bump(&mut out, &path, n);
    }
    out
}

/// The `dup`s of arguments right before calls that borrow their
/// positions, by callee.
fn caller_dups_saved(
    counted: &[crate::MirModule],
    masks: &BTreeMap<String, BorrowMask>,
) -> BTreeMap<String, u32> {
    use crate::{Callee, Operand, Rvalue, Stmt};
    let mut out: BTreeMap<String, u32> = BTreeMap::new();
    for unit in counted {
        for f in &unit.functions {
            for block in &f.blocks {
                for (k, stmt) in block.stmts.iter().enumerate() {
                    let (Stmt::Assign { value, .. } | Stmt::Eval(value)) = stmt else {
                        continue;
                    };
                    let Rvalue::Call {
                        callee: Callee::Direct(g),
                        args,
                    } = value
                    else {
                        continue;
                    };
                    let Some((path, m)) = callee_mask(unit, *g, masks) else {
                        continue;
                    };
                    let mut dups: Vec<crate::Local> = block.stmts[..k]
                        .iter()
                        .rev()
                        .map_while(|s| match s {
                            Stmt::Dup(x) => Some(*x),
                            _ => None,
                        })
                        .collect();
                    for j in m.indices() {
                        if let Some(Operand::Local(x)) = args.get(j)
                            && let Some(at) = dups.iter().position(|d| d == x)
                        {
                            dups.remove(at);
                            bump(&mut out, &path, 1);
                        }
                    }
                }
            }
        }
    }
    out
}

/// The projections of borrowed parameters with only borrowed uses, by
/// function.
fn projections_saved(
    uncounted: &[crate::MirModule],
    masks: &BTreeMap<String, BorrowMask>,
) -> BTreeMap<String, u32> {
    use crate::{Callee, FnKey, Rvalue, Stmt, Terminator};
    let mut out: BTreeMap<String, u32> = BTreeMap::new();
    for unit in uncounted {
        for f in &unit.functions {
            let FnKey::Item(def) = f.key else { continue };
            let Some((path, m)) = callee_mask(unit, def, masks) else {
                continue;
            };
            let mut roots: BTreeMap<crate::Local, usize> = f
                .params
                .iter()
                .enumerate()
                .filter(|&(i, _)| m.is_borrowed(i))
                .map(|(i, &p)| (p, i))
                .collect();
            let fields: Vec<(crate::Local, crate::Local)> = f
                .blocks
                .iter()
                .flat_map(|b| &b.stmts)
                .filter_map(|s| match s {
                    Stmt::Assign {
                        dst,
                        value: Rvalue::Field { base, .. },
                    } => Some((*dst, *base)),
                    _ => None,
                })
                .collect();
            let mut projections = Vec::new();
            for _ in 0..=fields.len() {
                for &(dst, base) in &fields {
                    if !roots.contains_key(&dst)
                        && let Some(&i) = roots.get(&base)
                    {
                        roots.insert(dst, i);
                        projections.push(dst);
                    }
                }
            }
            let mut owned_use = std::collections::BTreeSet::new();
            for block in &f.blocks {
                for stmt in &block.stmts {
                    match stmt {
                        Stmt::Assign {
                            value:
                                Rvalue::Call {
                                    callee: Callee::Direct(g),
                                    args,
                                },
                            ..
                        }
                        | Stmt::Eval(Rvalue::Call {
                            callee: Callee::Direct(g),
                            args,
                        }) => {
                            let callee =
                                callee_mask(unit, *g, masks).map_or(BorrowMask::OWNED, |(_, m)| m);
                            note_call(&mut owned_use, callee, args);
                        }
                        Stmt::Assign { value, .. } | Stmt::Eval(value) => {
                            note_owned(&mut owned_use, value.uses());
                        }
                        _ => {}
                    }
                }
                match &block.term {
                    Terminator::TailCall {
                        callee: Callee::Direct(g),
                        args,
                    } => {
                        let callee =
                            callee_mask(unit, *g, masks).map_or(BorrowMask::OWNED, |(_, m)| m);
                        note_call(&mut owned_use, callee, args);
                    }
                    term => note_owned(&mut owned_use, term.uses()),
                }
            }
            let only_borrowed = projections
                .iter()
                .filter(|p| {
                    !owned_use.contains(p)
                        && f.local(**p).is_ok_and(|d| d.repr == crate::Repr::Value)
                })
                .count();
            if only_borrowed > 0 {
                bump(&mut out, &path, u32::try_from(only_borrowed).unwrap());
            }
        }
    }
    out
}

/// One build of the report: the masks (D-787's reading) and the savings.
struct Report {
    masks: BTreeMap<String, BorrowMask>,
    saved: BTreeMap<String, u32>,
    test_masks: Option<BTreeMap<String, BorrowMask>>,
}

/// `units` as lowered, each counted on its own: all-owned, as before
/// borrowing.
fn counted_owned(units: &[crate::MirModule]) -> Vec<crate::MirModule> {
    let mut counted = units.to_vec();
    for unit in &mut counted {
        crate::count_and_check(unit).unwrap();
    }
    counted
}

fn report_one(name: &str, files: &[(String, String)]) -> Result<Report, String> {
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(m, s)| (m.as_str(), s.as_str()))
        .collect();
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let inferred = lower_package_inferred(name, &refs, false)?;
        let counted = counted_owned(&inferred.units);
        let saved = saved_pairs(&counted, &inferred.units, &inferred.masks);
        let tests = lower_package_inferred(name, &refs, true).ok();
        Ok::<_, String>(Report {
            masks: inferred.masks,
            saved,
            test_masks: tests.map(|t| t.masks),
        })
    }));
    match run {
        Ok(r) => r,
        Err(_) => Err("the build panicked".to_owned()),
    }
}

/// The report's section on every function of std, lowered in full.
fn std_section() -> String {
    use std::fmt::Write as _;
    let lowered = crate::test_support::lower_std_inferred().unwrap();
    let counted = counted_owned(&lowered.units);
    let saved = saved_pairs(&counted, &lowered.units, &lowered.masks);
    let mut text = String::new();
    let _ = writeln!(
        text,
        "### std, every function (`cheby run` build, no tests units)\n"
    );
    let _ = writeln!(
        text,
        "{} borrowing functions, about {} pairs.\n",
        lowered.masks.len(),
        saved.values().sum::<u32>(),
    );
    let _ = writeln!(text, "| Function | Mask | Pairs |");
    let _ = writeln!(text, "| --- | --- | --- |");
    for (p, m) in &lowered.masks {
        let _ = writeln!(
            text,
            "| `{p}` | {} | {} |",
            mask_text(*m),
            saved.get(p).copied().unwrap_or(0),
        );
    }
    text.push('\n');
    text
}

fn mask_text(m: BorrowMask) -> String {
    let params: Vec<String> = m.indices().map(|i| i.to_string()).collect();
    format!("{:#b} (params {})", m.0, params.join(", "))
}

#[test]
#[ignore = "writes the data of $S3/P1/p1a-masks.md to the file CHEBY_BORROW_REPORT names"]
#[expect(clippy::too_many_lines, reason = "one report, written in order")]
fn borrow_report_masks() {
    use std::fmt::Write as _;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    for base in ["docs/examples", "bench/runtime/rc"] {
        let mut found: Vec<_> = std::fs::read_dir(root.join(base))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.join("cheby.toml").exists())
            .collect();
        found.sort();
        dirs.extend(found);
    }
    let mut text = String::new();
    let mut std_masks: BTreeMap<String, BorrowMask> = BTreeMap::new();
    let mut std_saved: BTreeMap<String, u32> = BTreeMap::new();
    let mut totals = (0_usize, 0_u32);
    for dir in &dirs {
        let label = dir.strip_prefix(&root).unwrap().display().to_string();
        let Some((name, files)) = program(dir) else {
            let _ = writeln!(text, "### `{label}`\n\nNo manifest name.\n");
            continue;
        };
        let _ = writeln!(text, "### `{label}`\n");
        let report = match report_one(&name, &files) {
            Ok(r) => r,
            Err(e) => {
                let first = e.lines().next().unwrap_or_default().to_owned();
                let _ = writeln!(text, "Not built by the test front end: {first}\n");
                continue;
            }
        };
        let user = |m: &BTreeMap<String, BorrowMask>| -> Vec<(String, BorrowMask)> {
            m.iter()
                .filter(|(p, _)| !p.starts_with("std::"))
                .map(|(p, &m)| (p.clone(), m))
                .collect()
        };
        for (p, &m) in report.masks.iter().filter(|(p, _)| p.starts_with("std::")) {
            std_masks.insert(p.clone(), m);
            let s = report.saved.get(p).copied().unwrap_or(0);
            let e = std_saved.entry(p.clone()).or_default();
            *e = (*e).max(s);
        }
        let own = user(&report.masks);
        let own_saved: u32 = own
            .iter()
            .map(|(p, _)| report.saved.get(p).copied().unwrap_or(0))
            .sum();
        totals = (
            totals.0.saturating_add(own.len()),
            totals.1.saturating_add(own_saved),
        );
        let _ = writeln!(
            text,
            "{} borrowing functions, about {own_saved} pairs saved.\n",
            own.len(),
        );
        if !own.is_empty() {
            let _ = writeln!(text, "| Function | Mask | Pairs |");
            let _ = writeln!(text, "| --- | --- | --- |");
            for (p, m) in &own {
                let _ = writeln!(
                    text,
                    "| `{p}` | {} | {} |",
                    mask_text(*m),
                    report.saved.get(p).copied().unwrap_or(0),
                );
            }
            text.push('\n');
        }
        match &report.test_masks {
            Some(t) => {
                let tm = user(t);
                if tm != own {
                    let _ = writeln!(text, "`cheby test` build differs: {tm:?}\n");
                }
            }
            None => {
                let _ = writeln!(
                    text,
                    "The `cheby test` build was not made by the test front end.\n"
                );
            }
        }
    }
    let mut head = String::new();
    let _ = writeln!(
        head,
        "Totals over the examples and the RC programs (user code, D-787): {} functions, about {} pairs.\n",
        totals.0, totals.1
    );
    let _ = writeln!(head, "### std (`cheby run` builds of the programs above)\n");
    let _ = writeln!(
        head,
        "{} borrowing functions, about {} pairs.\n",
        std_masks.len(),
        std_saved.values().sum::<u32>(),
    );
    let _ = writeln!(head, "| Function | Mask | Pairs |");
    let _ = writeln!(head, "| --- | --- | --- |");
    for (p, m) in &std_masks {
        let _ = writeln!(
            head,
            "| `{p}` | {} | {} |",
            mask_text(*m),
            std_saved.get(p).copied().unwrap_or(0),
        );
    }
    head.push('\n');
    head.push_str(&std_section());
    head.push_str(&text);
    match std::env::var("CHEBY_BORROW_REPORT") {
        Ok(path) => std::fs::write(path, head).unwrap(),
        Err(_) => println!("{head}"),
    }
}
