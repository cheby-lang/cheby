//! The evaluator on hand-written programs, one rule of the spec each, and
//! on every program of the generator's self-check seeds.

use std::time::Instant;

use super::{EvalError, Expected, evaluate, evaluate_counting, interp};
use crate::progen::{CHECK_SEEDS, DIFF_SEEDS, GenConfig, Program, generate};

const MANIFEST: &str = "name = \"progen\"\nversion = \"0.1.0\"\n";

const RUN: &str = "fn run(name: String, body: fn() -> String) {
  case fiber::scope(fn(_) { body() }) {
    Ok(text) => io::println(\"{name}: {text}\")
    Err(failure) => {
      let reason = fiber::message(failure)
      io::println(\"{name} panicked: {reason}\")
    }
  }
}
";

fn package(main: &str, helpers: Option<&str>) -> Program {
    let mut files = vec![
        ("cheby.toml".to_owned(), MANIFEST.to_owned()),
        ("src/main.cheby".to_owned(), main.to_owned()),
    ];
    if let Some(h) = helpers {
        files.push(("src/helpers.cheby".to_owned(), h.to_owned()));
    }
    Program { seed: 0, files }
}

/// A program whose `main` is `body`, with `run` and the usual imports.
fn with_main(body: &str) -> Program {
    package(
        &format!(
            "//! A test.\n\nimport std::fiber\nimport std::int\nimport std::io\nimport std::list\nimport std::map\nimport std::string\n\n{RUN}\nfn main() {{\n{body}\n}}\n"
        ),
        None,
    )
}

fn stdout_of(body: &str) -> String {
    let expected = evaluate(&with_main(body)).unwrap();
    assert_eq!(expected.status, 0, "{expected:?}");
    assert_eq!(expected.stderr.as_deref(), Some(""));
    expected.stdout
}

#[test]
fn progen_eval_int_panics_are_caught_with_their_messages() {
    let out = stdout_of(
        "  run(\"mul\", fn() {
    let x = 9_007_199_254_740_991 * 3_037_000_500
    \"{x}\"
  })
  run(\"div\", fn() {
    let z = 0
    let x = 5 / z
    \"{x}\"
  })
  run(\"rem\", fn() {
    let z = 0
    let x = -7 % z
    \"{x}\"
  })
  run(\"signs\", fn() {
    let a = -7 / 2
    let b = -7 % 2
    let c = 7 % -2
    \"{a} {b} {c}\"
  })
  run(\"sum\", fn() {
    let s = list::sum([9_007_199_254_740_991 * 100, 9_007_199_254_740_991 * 1_000], 0)
    \"{s}\"
  })",
    );
    assert_eq!(
        out,
        "mul panicked: integer overflow\ndiv panicked: division by zero\n\
         rem panicked: division by zero\nsigns: -3 -1 1\nsum panicked: integer overflow\n"
    );
}

/// [`with_main`] with a record type and a sum type before `run`.
fn with_types(body: &str) -> Program {
    let mut program = with_main(body);
    if let Some((_, text)) = program.files.get_mut(1) {
        *text = text.replacen(
            "fn run(",
            "type Rec0 {\n  k0: Int,\n  k1: List<String>,\n}\n\ntype Sum0 {\n  S0A,\n  S0B(Int, String),\n  S0C { k0: Rec0, k1: Bool },\n}\n\nfn run(",
            1,
        );
    }
    program
}

#[test]
fn progen_eval_records_variants_and_updates() {
    let program = with_types(
        "  let r = Rec0 { k0: -3, k1: [\"a\", \"x y\"] }
  let v = S0C { k0: r, k1: True }
  let xs = [S0A, S0B(1, \"q\\{\"), v]
  let m = map::to_sorted_list(map::from_list([(\"b\", 1), (\"a\", 2), (\"b\", 3)]))
  let o = list::get([1], 3)
  let words = [\"a\", \"b\"]
  let u = Rec0 { ..r, k0: r.k0 + 1 }
  let Rec0 { k1: w, .. } = u
  let shown = case v {
    S0C { k0: Rec0 { k0: n, .. }, .. } when n < 0 => \"neg {n}\"
    _ => \"other\"
  }
  io::println(\"{xs:?} {m:?} {o:?} {words} {words:?} {r.k1} {u:?} {w:?} {shown}\")",
    );
    let expected = evaluate(&program).unwrap();
    assert_eq!(
        expected.stdout,
        "[S0A, S0B(1, \"q\\{\"), S0C { k0: Rec0 { k0: -3, k1: [\"a\", \"x y\"] }, k1: True }] \
         [(\"a\", 2), (\"b\", 3)] None [a, b] [\"a\", \"b\"] [a, x y] \
         Rec0 { k0: -2, k1: [\"a\", \"x y\"] } [\"a\", \"x y\"] neg -3\n"
    );
}

#[test]
fn progen_eval_assertion_messages_follow_chapter_11() {
    let out = stdout_of(
        "  run(\"cmp\", fn() {
    let x = 1
    assert x + 1 > list::length([1, 2, 3])
    \"x\"
  })
  run(\"plain\", fn() {
    assert list::is_empty([1])
    \"x\"
  })
  run(\"let\", fn() {
    let assert [h, ..t] = list::range(0, 0)
    \"{h} {t}\"
  })
  run(\"as\", fn() {
    let n = 7
    assert n == 8 as \"n is {n}\"
    \"x\"
  })",
    );
    assert_eq!(
        out,
        "cmp panicked: assertion failed: x + 1 > list::length([1, 2, 3])\n  left: 2\n right: 3\n\
         plain panicked: assertion failed: list::is_empty([1])\n\
         let panicked: `let assert` pattern did not match\n  value: []\n\
         as panicked: assertion failed: n is 7\n  left: 7\n right: 8\n"
    );
}

#[test]
fn progen_eval_std_functions() {
    let out = stdout_of(
        "  let a = string::split(\"abc\", \"\")
  let b = string::slice(\"hello\", 2, 40)
  let c = string::repeat(\"ab\", 3)
  let d = string::split(\" a  b \", \" \")
  let e = string::split(\"tree\", \"e\")
  let f = list::unique([3, 1, 3, 2, 1])
  let g = list::take([1, 2, 3], -1)
  let h = list::drop([1, 2], 4)
  let i = list::range(2, 5)
  let j = list::sort([\"b\", \"Zed\", \"a\"])
  let k = string::trim(\"  x y \")
  let l = list::fold([\"a\", \"b\"], \"\", fn(acc, s) { acc + s })
  let m = list::filter(list::map([1, 2, 3], fn(x) { x * 2 }), fn(x) { x > 2 })
  let n = map::size(map::remove(map::insert(map::new::<String, Int>(), \"a\", 1), \"b\"))
  let o = map::get(map::from_list([(\"a\", 1)]), \"a\")
  let p = int::min(3, -4)
  let q = string::uppercase(\"x y\") + string::reverse(\"abc\")
  io::println(\"{a:?} {b} {c} {d:?} {e:?} {f} {g} {h} {i} {j:?} {k} {l} {m} {n} {o:?} {p} {q}\")",
    );
    assert_eq!(
        out,
        "[\"a\", \"b\", \"c\"] llo ababab [\"\", \"a\", \"\", \"b\", \"\"] [\"tr\", \"\", \"\"] \
         [3, 1, 2] [] [] [2, 3, 4] [\"Zed\", \"a\", \"b\"] x y ab [4, 6] 1 Some(1) -4 X Ycba\n"
    );
}

#[test]
fn progen_eval_order_guards_and_local_functions() {
    let out = stdout_of(
        "  let say = fn(s: String) -> Int {
    io::println(s)
    1
  }
  let _t = (say(\"a\"), say(\"b\"))
  let r = case say(\"subject\"), True {
    1, False => 0
    k, _ when say(\"guard\") > k => 1
    k, True | False => k + say(\"body\")
  }
  fn count(n: Int, acc: Int) -> Int {
    case {
      n <= 0 => acc
      _ => count(n - 1, acc + n)
    }
  }
  let total = count(4, 0)
  let skipped = (False) && (say(\"never\") == 1)
  io::println(\"{r} {total} {skipped}\")",
    );
    assert_eq!(out, "a\nb\nsubject\nguard\nbody\n2 10 False\n");
}

#[test]
fn progen_eval_helpers_module_and_function_values() {
    let program = package(
        "//! Main.\n\nimport std::io\nimport std::list\nimport progen::helpers\n\nfn f0(x: Int) -> Int {\n  x + helpers::h0(x)\n}\n\nfn main() {\n  let ys = list::map([1, 2], f0)\n  let zs = list::map(ys, helpers::h0)\n  io::println(\"{ys} {zs}\")\n}\n",
        Some(
            "//! Helpers.\n\npub fn h0(x: Int) -> Int {\n  h1(x) * 10\n}\n\nfn h1(x: Int) -> Int {\n  x + 1\n}\n",
        ),
    );
    let expected = evaluate(&program).unwrap();
    assert_eq!(expected.stdout, "[21, 32] [220, 330]\n");
}

#[test]
fn progen_eval_planted_panic_reports_on_stderr() {
    let program = package(
        "//! Main.\n\nimport std::io\n\nfn finale(n: Int, p1: Int) -> Int {\n  case {\n    n <= 0 => panic as \"planted panic\"\n    _ => {\n      let r = finale(n - 1, p1)\n      r + p1\n    }\n  }\n}\n\nfn main() {\n  io::println(\"before\")\n  let last = finale(2, 1)\n  io::println(\"{last}\")\n}\n",
        None,
    );
    assert_eq!(
        evaluate(&program).unwrap(),
        Expected {
            stdout: "before\n".to_owned(),
            status: 101,
            stderr: Some("panicked at src/main.cheby:7:15:\nplanted panic\n".to_owned()),
        }
    );
}

#[test]
fn progen_eval_refuses_what_it_does_not_know() {
    for (body, what) in [
        ("  let x = 1.5\n  io::println(\"{x}\")", "float"),
        (
            "  let x = list::last([1])\n  io::println(\"{x:?}\")",
            "list::last",
        ),
        (
            "  let x = [1] |> list::length\n  io::println(\"{x}\")",
            "|>",
        ),
        ("  let x = 1 << 2\n  io::println(\"{x}\")", "<<"),
    ] {
        match evaluate(&with_main(body)) {
            Err(EvalError::Unsupported(message)) => assert!(message.contains(what), "{message}"),
            other => panic!("{body}: {other:?}"),
        }
    }
    let unparsed = package("fn main( {", None);
    assert!(matches!(evaluate(&unparsed), Err(EvalError::Malformed(_))));
}

#[test]
fn progen_eval_budget_stops_a_runaway_program() {
    let program = with_main(
        "  fn forever(n: Int) -> Int {\n    forever(n + 1)\n  }\n  let x = forever(0)\n  io::println(\"{x}\")",
    );
    assert_eq!(evaluate(&program), Err(EvalError::Budget));
    let deep = with_main(
        "  fn deep(n: Int) -> Int {\n    1 + deep(n)\n  }\n  let x = deep(0)\n  io::println(\"{x}\")",
    );
    assert_eq!(evaluate(&deep), Err(EvalError::Budget));
}

/// D-719, D-1028: every program of the generator's self-check seeds (and
/// of the differential's) is inside the subset and finishes within the
/// budget, in process and without `cheby`; a planted panic ends it with
/// 101 and a report on standard error.
#[test]
fn progen_eval_covers_generator() {
    let config = GenConfig::default();
    let start = Instant::now();
    let mut most_steps = 0u64;
    let mut total_steps = 0u64;
    let mut planted = 0usize;
    let mut lines = 0usize;
    let mut caught = [0usize; PANIC_KINDS.len() + 1];
    let seeds: Vec<u64> = CHECK_SEEDS.chain(DIFF_SEEDS).collect();
    for seed in &seeds {
        let program = generate(*seed, &config);
        let (expected, steps) = match evaluate_counting(&program) {
            Ok(out) => out,
            Err(e) => panic!(
                "seed {seed}: {e}\n--- src/main.cheby\n{}\n--- src/helpers.cheby\n{}",
                program.file("src/main.cheby").unwrap_or(""),
                program.file("src/helpers.cheby").unwrap_or("")
            ),
        };
        let want = if program.planted_panic() { 101 } else { 0 };
        assert_eq!(expected.status, want, "seed {seed}: {expected:?}");
        if want == 101 {
            planted += 1;
            let stderr = expected.stderr.as_deref().unwrap_or("");
            assert!(
                stderr.starts_with("panicked at src/main.cheby:")
                    && stderr.ends_with("\nplanted panic\n"),
                "seed {seed}: {stderr:?}"
            );
        }
        lines += expected.stdout.lines().count();
        for line in expected.stdout.lines() {
            if let Some((_, message)) = line.split_once(" panicked: ") {
                let kind = PANIC_KINDS
                    .iter()
                    .position(|k| message.starts_with(k))
                    .unwrap_or(PANIC_KINDS.len());
                caught[kind] += 1;
            }
        }
        most_steps = most_steps.max(steps);
        total_steps += steps;
    }
    let elapsed = start.elapsed();
    println!(
        "{} programs evaluated in {elapsed:?}: {planted} with a planted panic, {lines} lines \
         of output, {total_steps} steps, at most {most_steps} in one program (budget {})",
        seeds.len(),
        interp::MAX_STEPS
    );
    let kinds: Vec<String> = PANIC_KINDS
        .iter()
        .chain(&["other"])
        .zip(caught)
        .map(|(k, n)| format!("{n} `{k}`"))
        .collect();
    println!("panics caught by `run`: {}", kinds.join(", "));
    assert!(most_steps < interp::MAX_STEPS);
}

/// The panic messages generated programs reach, for the coverage line.
const PANIC_KINDS: [&str; 4] = [
    "integer overflow",
    "division by zero",
    "assertion failed",
    "`let assert` pattern did not match",
];
