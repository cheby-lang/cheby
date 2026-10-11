//! Lowering tests for patterns (D3): decision trees through
//! `test_support::lower_source`, goldens under `tests/golden/pat/`, and
//! the check that no path of any tree in the examples and UI tests tests
//! one position twice.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::golden_tests::check_golden;
use crate::lower::pat::tree::{Decision, Key, Occ, OccId, RECORDED, RECORDING, Step, Test, Tree};
use crate::test_support::{Lowered, lower_package, lower_source};

/// Every pattern form, each in a function of its own.
const FORMS: &str = r#"//! Every pattern form.

import std::io

type Shape {
  Circle { radius: Int },
  Rect { width: Int, height: Int },
  Dot,
}

type Pair<T> {
  Pair(T, Int),
}

fn ints(n: Int) -> String {
  case n {
    1 => "one"
    -2 | 3 => "few"
    _ => "many"
  }
}

fn small(n: I8) -> String {
  case n {
    -1 => "minus one"
    _ => "other"
  }
}

fn strings(s: String) -> Int {
  case s {
    "a" => 1
    "b" => 2
    _ => 3
  }
}

fn bools(b: Bool, c: Bool) -> Int {
  case b, c {
    True, True => 1
    False, _ => 2
    _, False => 3
  }
}

fn options(o: Option<Int>) -> Int {
  case o {
    Some(0) => 0
    Some(n) when n > 10 => n
    Some(n) => n + 1
    None => -1
  }
}

fn results(r: Result<Option<Int>, String>) -> String {
  case r {
    Ok(Some(_)) => "some"
    Ok(None) => "none"
    Err(e) => e
  }
}

fn orders(o: Order) -> Int {
  case o {
    Less => 1
    Equal => 2
    Greater => 3
  }
}

fn shapes(s: Shape) -> Int {
  case s {
    Circle { radius } => radius
    Rect { width: w, height: 0 } => w
    Rect { width, .. } => width
    Dot => 0
  }
}

fn pairs(p: Pair<String>) -> String {
  let Pair(s, _) = p
  s
}

fn tuples(t: (Int, String)) -> String {
  case t {
    (0, s) | (1, s) => s
    (_, s) as whole => s
  }
}

fn lists(xs: List<Int>) -> Int {
  case xs {
    [] => 0
    [x] => x
    [first, .., last] => first + last
  }
}

fn rests(xs: List<Int>) -> List<Int> {
  case xs {
    [_, ..rest] => rest
    [] => []
  }
}

fn suffix(xs: List<Int>) -> Int {
  case xs {
    [.., a, b] => a + b
    _ => 0
  }
}

fn sign(n: Int) -> String {
  case {
    n < 0 => "negative"
    n == 0 => "zero"
    _ => "positive"
  }
}

fn destructure(t: (Int, Pair<String>)) -> String {
  let (_, Pair(s, _)) = t
  s
}

fn asserting(o: Option<String>) -> String {
  let assert Some(s) = o
  s
}

fn asserting_with(o: Option<String>) -> String {
  let assert Some(s) = o as "need a value"
  s
}

fn main() {
  io::println(ints(2))
  io::println(small(-1))
  let _ = strings("a")
  let _ = bools(True, False)
  let _ = options(Some(3))
  io::println(results(Ok(None)))
  let _ = orders(Less)
  let _ = shapes(Dot)
  io::println(pairs(Pair("x", 1)))
  io::println(tuples((1, "y")))
  let _ = lists([1, 2])
  let _ = rests([1])
  let _ = suffix([1, 2, 3])
  io::println(sign(1))
  io::println(destructure((1, Pair("z", 2))))
  io::println(asserting(Some("a")))
  io::println(asserting_with(Some("b")))
}
"#;

/// Lowers `src` as the root module and checks that nothing is E0038.
fn lower(src: &str) -> Lowered {
    let lowered = lower_source(&[("", src)]);
    assert_eq!(lowered.unsupported(), Vec::<String>::new());
    lowered
}

/// The dumps of `app::<name>` for each name, separated by blank lines.
fn functions(lowered: &Lowered, names: &[&str]) -> String {
    names
        .iter()
        .map(|n| lowered.function("app", &format!("app::{n}")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The number of lines of `text` containing `needle`.
fn count(text: &str, needle: &str) -> usize {
    text.lines().filter(|l| l.contains(needle)).count()
}

#[test]
fn literals_switch_on_the_value_and_strings_compare() {
    let lowered = lower(FORMS);
    let ints = lowered.function("app", "app::ints");
    // One switch for every literal of the column, `-2` as its bits.
    assert!(
        ints.contains("switch _0 [1 => bb1, 18446744073709551614 => bb2, 3 => bb3, _ => bb4]"),
        "{ints}"
    );
    assert_eq!(count(&ints, "switch"), 1, "{ints}");
    let small = lowered.function("app", "app::small");
    assert!(
        small.contains("switch _0 [255 => bb1, _ => bb2]"),
        "{small}"
    );
    let strings = lowered.function("app", "app::strings");
    assert_eq!(count(&strings, "call_runtime str_eq"), 2, "{strings}");
    check_golden(
        "pat/literals",
        &functions(&lowered, &["ints", "small", "strings"]),
    );
}

#[test]
fn bools_branch_unboxed() {
    let lowered = lower(FORMS);
    let bools = lowered.function("app", "app::bools");
    assert!(bools.contains("branch _0, "), "{bools}");
    assert!(bools.contains("branch _1, "), "{bools}");
    assert!(!bools.contains("read_tag"), "{bools}");
    // `b` is tested once on every path, `c` only where `b` is `True`.
    assert_eq!(count(&bools, "branch"), 2, "{bools}");
}

#[test]
fn constructors_read_their_tag_and_fields() {
    let lowered = lower(FORMS);
    let options = lowered.function("app", "app::options");
    assert!(options.contains("read_tag _0 of option"), "{options}");
    // The payload is a uniform field, unboxed once into an `Int`.
    assert!(options.contains("field _0.0 of ctor Some"), "{options}");
    assert_eq!(count(&options, "unbox int"), 1, "{options}");
    let results = lowered.function("app", "app::results");
    assert!(results.contains("read_tag _0 of result"), "{results}");
    assert!(results.contains("of option"), "{results}");
    let orders = lowered.function("app", "app::orders");
    // Three constructors: two cases, the last one is the default.
    assert!(
        orders.contains("switch _1 [0 => bb1, 1 => bb2, _ => bb3]"),
        "{orders}"
    );
    let shapes = lowered.function("app", "app::shapes");
    assert!(shapes.contains("read_tag _0 of app::Shape"), "{shapes}");
    // Raw `Int` fields are read unboxed (plan §5.2).
    assert!(
        shapes.contains(": int = field _0.0 of ctor app::Circle"),
        "{shapes}"
    );
    assert!(
        shapes.contains(": int = field _0.1 of ctor app::Rect"),
        "{shapes}"
    );
    assert!(!shapes.contains("unbox"), "{shapes}");
    check_golden(
        "pat/constructors",
        &functions(&lowered, &["options", "results", "orders", "shapes"]),
    );
}

#[test]
fn tuples_and_single_constructors_need_no_test() {
    let lowered = lower(FORMS);
    let pairs = lowered.function("app", "app::pairs");
    // A type parameter's field is uniform and stays so for `String`.
    assert!(pairs.contains("field _0.0 of ctor app::Pair"), "{pairs}");
    assert!(
        !pairs.contains("read_tag") && !pairs.contains("switch"),
        "{pairs}"
    );
    let destructure = lowered.function("app", "app::destructure");
    assert!(
        destructure.contains("field _0.1 of tuple/2"),
        "{destructure}"
    );
    assert!(
        destructure.contains("field _1.0 of ctor app::Pair"),
        "{destructure}"
    );
    assert!(
        !destructure.contains("branch") && !destructure.contains("switch"),
        "{destructure}"
    );
    let tuples = lowered.function("app", "app::tuples");
    assert!(tuples.contains("field _0.0 of tuple/2"), "{tuples}");
    assert!(tuples.contains("unbox int"), "{tuples}");
    check_golden(
        "pat/tuples",
        &functions(&lowered, &["pairs", "destructure", "tuples"]),
    );
}

#[test]
fn list_patterns_use_length_get_and_slice() {
    let lowered = lower(FORMS);
    let lists = lowered.function("app", "app::lists");
    assert!(
        lists.contains("call_runtime list_length(borrow _0)"),
        "{lists}"
    );
    assert!(
        lists.contains("switch _1 [0 => bb1, 1 => bb2, _ => bb3]"),
        "{lists}"
    );
    // `last` is counted from the end in the default (two or more).
    assert!(lists.contains("checked sub int _1, const int 1"), "{lists}");
    let rests = lowered.function("app", "app::rests");
    // An O(1) slice from 1 to the length (D-053), which consumes the list,
    // used for the last time there (rc.rs).
    assert!(
        rests.contains("call_runtime list_slice_owned(_0, borrow const int 1, borrow _1)"),
        "{rests}"
    );
    let suffix = lowered.function("app", "app::suffix");
    assert!(
        suffix.contains("checked sub int _1, const int 2"),
        "{suffix}"
    );
    assert_eq!(count(&suffix, "list_length"), 1, "{suffix}");
    check_golden(
        "pat/lists",
        &functions(&lowered, &["lists", "rests", "suffix"]),
    );
}

#[test]
fn alternatives_join_through_block_parameters_and_as_binds_the_whole() {
    let lowered = lower(FORMS);
    let tuples = lowered.function("app", "app::tuples");
    // `(0, s) | (1, s)`: two leaves pass `s` to one block.
    let shared = tuples
        .lines()
        .filter(|l| l.starts_with("bb") && l.contains("(_") && l.contains(": value)"))
        .count();
    assert!(shared >= 1, "{tuples}");
    assert_eq!(count(&tuples, "goto bb4("), 2, "{tuples}");
    // `as whole` binds the subject itself.
    assert!(tuples.contains(": value = _0"), "{tuples}");
}

#[test]
fn guards_fall_through_to_later_arms() {
    let lowered = lower(FORMS);
    let options = lowered.function("app", "app::options");
    // The guard compares `n`; when false, the next arm adds one.
    assert!(
        options.contains("compare gt int _3, const int 10"),
        "{options}"
    );
    assert!(options.contains("checked add int"), "{options}");
}

#[test]
fn a_guard_runs_once_per_arm() {
    RECORDED.with(|r| r.borrow_mut().clear());
    let lowered = lower(
        r"//! A guard after alternatives.

fn pick(pair: (Int, Int)) -> Int {
  case pair {
    (0, n) | (n, _) when n < 5 => n
    _ => -1
  }
}

fn main() {
  let _ = pick((0, 9))
}
",
    );
    let pick = lowered.function("app", "app::pick");
    // One guard per alternative's path, each tested once.
    assert_eq!(count(&pick, "compare lt int"), 2, "{pick}");
    check_golden("pat/guards", &pick);
    let recorded = RECORDED.with(|r| std::mem::take(&mut *r.borrow_mut()));
    let decision = recorded
        .iter()
        .find(|d| d.tree.leaf_counts(2) == vec![2, 2])
        .unwrap_or_else(|| panic!("{recorded:#?}"));
    assert_eq!(tested_twice(decision), Vec::<String>::new());
}

#[test]
fn multiple_subjects_and_subjectless_case() {
    let lowered = lower(FORMS);
    let sign = lowered.function("app", "app::sign");
    assert_eq!(count(&sign, "branch"), 2, "{sign}");
    assert!(!sign.contains("switch"), "{sign}");
    check_golden("pat/conditions", &functions(&lowered, &["bools", "sign"]));
}

#[test]
fn let_assert_matches_or_panics_with_the_value() {
    let lowered = lower(FORMS);
    let plain = lowered.function("app", "app::asserting");
    assert!(
        plain.contains(r#""`let assert` pattern did not match\n  value: ""#),
        "{plain}"
    );
    assert!(plain.contains("call_runtime debug(borrow _0)"), "{plain}");
    assert!(plain.contains("panic let_assert"), "{plain}");
    // The location is the pattern's (D-570).
    assert!(plain.contains(r#""src/main.cheby:128:14""#), "{plain}");
    let with = lowered.function("app", "app::asserting_with");
    assert!(
        with.contains(r#""`let assert` pattern did not match: ""#),
        "{with}"
    );
    assert!(with.contains(r#""\n  value: ""#), "{with}");
    check_golden(
        "pat/let_assert",
        &functions(&lowered, &["asserting", "asserting_with"]),
    );
}

#[test]
fn exhaustive_matches_never_panic() {
    let lowered = lower(FORMS);
    for name in [
        "ints",
        "small",
        "strings",
        "bools",
        "options",
        "results",
        "orders",
        "shapes",
        "pairs",
        "tuples",
        "lists",
        "rests",
        "suffix",
        "sign",
        "destructure",
    ] {
        let f = lowered.function("app", &format!("app::{name}"));
        assert!(!f.contains("panic"), "{f}");
    }
}

#[test]
fn irrefutable_alternatives_test_without_a_panic() {
    // The first branch can fail, so the tree has a test, but some branch
    // always matches.
    let lowered = lower(
        r"//! Alternatives in `let`.

fn pick(pair: (Int, Int)) -> Int {
  let (0, n) | (n, _) = pair
  n
}

fn main() {
  let _ = pick((0, 1))
}
",
    );
    let pick = lowered.function("app", "app::pick");
    assert!(pick.contains("switch"), "{pick}");
    assert!(!pick.contains("panic"), "{pick}");
}

#[test]
fn tail_position_reaches_every_arm() {
    let lowered = lower(
        r"//! Calls in arms.

fn twice(n: Int) -> Int {
  n * 2
}

fn pick(o: Option<Int>) -> Int {
  case o {
    Some(n) => twice(n)
    None => twice(0)
  }
}

fn main() {
  let _ = pick(None)
}
",
    );
    let pick = lowered.function("app", "app::pick");
    assert_eq!(count(&pick, "tail_call app::twice"), 2, "{pick}");
    assert!(!pick.contains("return"), "{pick}");
}

#[test]
fn diverging_arms_end_the_block() {
    let lowered = lower(
        r#"//! Every arm panics.

fn never(b: Bool) -> Int {
  case b {
    True => panic as "yes"
    False => todo
  }
}

fn main() {
  let _ = never(True)
}
"#,
    );
    let never = lowered.function("app", "app::never");
    assert_eq!(count(&never, "  panic "), 2, "{never}");
    assert!(
        !never.contains("return") && !never.contains("goto"),
        "{never}"
    );
}

#[test]
fn nested_patterns_test_inner_positions() {
    let lowered = lower(
        r"//! Nested constructors.

type Tree {
  Leaf,
  Node(Tree, Int, Tree),
}

fn deep(t: Tree) -> Bool {
  case t {
    Node(Node(_, _, _), _, _) | Node(_, _, Node(_, _, _)) => True
    _ => False
  }
}

fn main() {
  let _ = deep(Leaf)
}
",
    );
    let f = lowered.function("app", "app::deep");
    assert_eq!(count(&f, "read_tag"), 3, "{f}");
    check_golden("pat/nested", &f);
}

#[test]
fn parameter_patterns_bind_through_bind_param() {
    let lowered = lower(
        r"//! Pattern parameters (D-242).

type Point {
  Point { x: Int, y: Int },
}

fn main() {
  let sum = fn((a, b)) { a + b }
  let x_of = fn(Point { x, .. }) { x }
  let _ = sum((1, 2))
  let _ = x_of(Point { x: 1, y: 2 })
}
",
    );
    let dump = lowered.dump();
    assert!(dump.contains("of tuple/2"), "{dump}");
    assert!(dump.contains(": int = field"), "{dump}");
    assert!(dump.contains("of ctor app::Point"), "{dump}");
}

// --- No position tested twice -------------------------------------------

/// What a path through `decision` does twice, as messages: a position
/// tested twice, or a guard of one arm run twice (D-571). A list matched
/// one element at a time tests its length once per element, against a
/// different length each time (one `Len` case and a default); only the
/// same length twice, or another test of a position tested that way, is
/// a repeat.
fn tested_twice(decision: &Decision) -> Vec<String> {
    /// The positions tested on a path, with the length of an element-wise
    /// list test.
    type Seen = BTreeSet<(OccId, Option<u64>)>;
    let mut out = Vec::new();
    let mut work: Vec<(&Tree, Seen, BTreeSet<usize>)> =
        vec![(&decision.tree, BTreeSet::new(), BTreeSet::new())];
    while let Some((tree, seen, guards)) = work.pop() {
        match tree {
            Tree::Fail => {}
            Tree::Leaf { arm, otherwise, .. } => {
                if let Some(t) = otherwise {
                    let mut guards = guards;
                    if !guards.insert(*arm) {
                        out.push(format!("the guard of arm {arm}"));
                    }
                    work.push((t, seen, guards));
                }
            }
            Tree::Test {
                occ,
                test,
                cases,
                default,
            } => {
                let mut seen = seen;
                let length = match (test, cases.as_slice()) {
                    (Test::Len, [(Key::Len(n), _)]) if default.is_some() => Some(*n),
                    _ => None,
                };
                let repeated = match length {
                    Some(_) => seen.contains(&(*occ, None)),
                    None => seen.iter().any(|(o, _)| o == occ),
                };
                if repeated || !seen.insert((*occ, length)) {
                    out.push(format!("{occ:?} ({:?})", decision.occs[occ.index()]));
                }
                for (_, t) in cases {
                    work.push((t, seen.clone(), guards.clone()));
                }
                if let Some(t) = default {
                    work.push((t, seen.clone(), guards.clone()));
                }
            }
        }
    }
    out
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The `.cheby` files of a package directory as `(module path, text)`,
/// and the package name from its manifest.
fn package_sources(dir: &Path) -> (String, Vec<(String, String)>) {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            let module = match (prefix, stem.as_str()) {
                ("", "main") => String::new(),
                ("", _) => stem.clone(),
                _ => format!("{prefix}::{stem}"),
            };
            if path.is_dir() {
                walk(&path, &module, out);
            } else if path.extension().is_some_and(|e| e == "cheby") {
                out.push((module, std::fs::read_to_string(&path).unwrap()));
            }
        }
    }
    let manifest = std::fs::read_to_string(dir.join("cheby.toml")).unwrap();
    let name = manifest
        .lines()
        .find_map(|l| l.strip_prefix("name = \""))
        .and_then(|l| l.strip_suffix('"'))
        .unwrap()
        .to_owned();
    let mut files = Vec::new();
    walk(&dir.join("src"), "", &mut files);
    (name, files)
}

/// Every single-file UI test with a `main` that parses without any
/// diagnostic, as `(path, text)`.
fn ui_programs() -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                if !path.join("cheby.toml").exists() {
                    walk(&path, out);
                }
            } else if path.extension().is_some_and(|e| e == "cheby") {
                let text = std::fs::read_to_string(&path).unwrap();
                if text.contains("\nfn main(") && !text.contains("@mode: parse-fail") {
                    out.push((path, text));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(&repo().join("tests/ui"), &mut out);
    out.retain(|(_, text)| {
        let file = cheby_diag::SourceMap::new()
            .add_file("t.cheby".to_owned(), text.as_str())
            .unwrap();
        let (tree, errors) =
            cheby_syntax::parse(file, std::sync::Arc::from(text.as_str())).into_parts();
        errors.is_empty() && cheby_syntax::validate(&tree).is_empty()
    });
    out
}

/// A program to lower: where it comes from, its package name, and its
/// modules as `(module path, text)`.
type Package = (String, String, Vec<(String, String)>);

/// Every decision compiled while lowering what the examples and UI tests
/// reach (those the front end accepts) tests each position at most once
/// on every path.
#[test]
fn no_path_tests_a_position_twice() {
    let mut packages: Vec<Package> = Vec::new();
    let mut dirs: Vec<_> = std::fs::read_dir(repo().join("docs/examples"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("cheby.toml").exists())
        .collect();
    dirs.sort();
    assert!(dirs.len() >= 32);
    for dir in dirs {
        let (name, files) = package_sources(&dir);
        packages.push((dir.display().to_string(), name, files));
    }
    for (path, text) in ui_programs() {
        packages.push((
            path.display().to_string(),
            "ui_test".to_owned(),
            vec![(String::new(), text)],
        ));
    }
    let (mut programs, mut decisions, mut tests) = (0_usize, 0_usize, 0_usize);
    let mut failures = Vec::new();
    for (shown, name, files) in &packages {
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        RECORDED.with(|r| r.borrow_mut().clear());
        if lower_package(name, &refs).is_err() {
            // Front-end errors: unshipped modules (D-477), check-fail tests.
            continue;
        }
        programs += 1;
        let recorded = RECORDED.with(|r| std::mem::take(&mut *r.borrow_mut()));
        for d in &recorded {
            decisions += 1;
            tests += count_tests(&d.tree);
            for twice in tested_twice(d) {
                failures.push(format!("{shown}: {twice}"));
            }
        }
    }
    println!("{programs} programs, {decisions} decisions, {tests} tests");
    assert!(programs >= 30, "only {programs} programs lowered");
    assert!(decisions >= 100 && tests >= 100, "{decisions} {tests}");
    assert_eq!(failures, Vec::<String>::new());
}

fn count_tests(tree: &Tree) -> usize {
    match tree {
        Tree::Fail => 0,
        Tree::Leaf { otherwise, .. } => otherwise.as_deref().map_or(0, count_tests),
        Tree::Test { cases, default, .. } => {
            let inner: usize = cases.iter().map(|(_, t)| count_tests(t)).sum();
            inner
                .saturating_add(default.as_deref().map_or(0, count_tests))
                .saturating_add(1)
        }
    }
}

/// The check above finds a position tested twice on a path.
#[test]
fn the_check_finds_a_repeated_test() {
    let subject = OccId::from_index(0);
    let leaf = || Tree::Leaf {
        arm: 0,
        bindings: Vec::new(),
        otherwise: None,
    };
    let inner = Tree::Test {
        occ: subject,
        test: Test::Bool,
        cases: vec![(Key::Bool(true), leaf())],
        default: Some(Box::new(leaf())),
    };
    let decision = Decision {
        occs: vec![Occ {
            parent: None,
            step: Step::Subject(0),
            ty: cheby_iface::Ty::BOOL,
        }],
        tree: Tree::Test {
            occ: subject,
            test: Test::Bool,
            cases: vec![(Key::Bool(true), inner)],
            default: Some(Box::new(leaf())),
        },
    };
    assert_eq!(tested_twice(&decision).len(), 1);
}

/// A column of spreads with growing prefixes is matched one element at a
/// time, so its tree grows linearly with the arms (the gate-2 review's
/// R2-2); one switch over every length
/// would read N(N+1)/2 elements.
#[test]
fn growing_list_prefixes_compile_in_linear_size() {
    // The compiler recurses once per tested position; `cheby` lowers on
    // its main thread, whose stack is 8 MiB, larger than a test thread's.
    std::thread::Builder::new()
        .stack_size(8 << 20)
        .spawn(growing_list_prefixes)
        .unwrap()
        .join()
        .unwrap();
}

fn growing_list_prefixes() {
    const ARMS: usize = 120;
    let mut src =
        String::from("//! Growing prefixes.\n\nfn depth(xs: List<Int>) -> Int {\n  case xs {\n");
    for k in (1..=ARMS).rev() {
        let prefix: Vec<String> = (0..k).map(|i| i.to_string()).collect();
        writeln!(src, "    [{}, ..] => {k}", prefix.join(", ")).unwrap();
    }
    src.push_str("    _ => 0\n  }\n}\n\nfn main() {\n  let _ = depth([0, 1])\n}\n");
    RECORDED.with(|r| r.borrow_mut().clear());
    let lowered = lower(&src);
    let decisions = RECORDED.with(|r| std::mem::take(&mut *r.borrow_mut()));
    let [decision] = decisions.as_slice() else {
        panic!("expected one decision, got {}", decisions.len());
    };
    assert_eq!(tested_twice(decision), Vec::<String>::new());
    // One length test and one element test per prefix element.
    let tests = count_tests(&decision.tree);
    assert!(tests <= 2 * ARMS + 2, "{tests} tests for {ARMS} arms");
    let depth = lowered.function("app", "app::depth");
    let reads = count(&depth, "call_runtime list_get");
    assert!(reads <= ARMS + 1, "{reads} element reads for {ARMS} arms");
}

/// A list pattern of 3,000 elements gives a tree about 6,000 tests deep;
/// compiling, lowering and freeing it work on a 1 MiB stack, since none
/// of them recurses per tested position (D-601). The front end checks the
/// program first, on the test's own thread and rayon's default workers,
/// whose exhaustiveness search uses an explicit stack too.
#[test]
fn long_list_patterns_lower_on_a_small_stack() {
    const ELEMENTS: usize = 3_000;
    let zeros = vec!["0"; ELEMENTS].join(", ");
    let src = format!(
        "//! A long list pattern.\n\nfn f(xs: List<Int>) -> Int {{\n  case xs {{\n    \
         [{zeros}] => 1\n    [{zeros}, ..] => 2\n    _ => 0\n  }}\n}}\n\n\
         fn main() {{\n  let _ = f([0])\n}}\n"
    );
    let lowered =
        crate::test_support::lower_source_on_stack(&[("", src.as_str())], 1 << 20, || {
            RECORDING.with(|r| r.set(false));
        });
    let f = lowered.function("app", "app::f");
    // One length test and one element read per element.
    let reads = count(&f, "call_runtime list_get");
    assert!(
        (ELEMENTS..=ELEMENTS + 2).contains(&reads),
        "{reads} element reads"
    );
}

/// Few lengths keep one switch over the lengths.
#[test]
fn few_list_lengths_keep_one_switch() {
    let src = "//! Short lists.\n\nfn f(xs: List<Int>) -> Int {\n  case xs {\n    [] => 0\n    \
               [a] => a\n    [a, b, ..] => a + b\n  }\n}\n\nfn main() {\n  let _ = f([1])\n}\n";
    RECORDED.with(|r| r.borrow_mut().clear());
    let _ = lower(src);
    let decisions = RECORDED.with(|r| std::mem::take(&mut *r.borrow_mut()));
    let [decision] = decisions.as_slice() else {
        panic!("expected one decision, got {}", decisions.len());
    };
    assert_eq!(count_tests(&decision.tree), 1);
}
