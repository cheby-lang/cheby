//! Robustness: the parser never panics, never loops, and always produces a
//! lossless tree (A8). Inputs are truncated and mutated examples, token
//! soups from a fixed-seed generator, and deeply nested code.

use std::sync::Arc;

use cheby_diag::FileId;

use super::{check_tree, example_files, parse_text};
use crate::lexer::lex;
use crate::parser::{MAX_DEPTH, parse_with_fuel};

/// Parses `text`, checking losslessness and that the fuel never ran out.
fn robust(text: &str) -> usize {
    let lexed = lex(FileId::new(0), text);
    let (events, errors, out_of_fuel) = parse_with_fuel(FileId::new(0), &lexed, text);
    assert!(!out_of_fuel, "the parser ran out of fuel on:\n{text}");
    let tree = crate::tree::build(FileId::new(0), Arc::from(text), &lexed, events);
    check_tree(&tree, text);
    errors.len()
}

/// A small deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next().checked_rem(u64::try_from(n).unwrap()).unwrap()).unwrap()
    }
}

#[test]
fn truncated_examples() {
    for path in example_files() {
        let text = std::fs::read_to_string(&path).unwrap();
        let ends: Vec<usize> = text.match_indices('\n').map(|(i, _)| i).collect();
        for &end in ends.iter().step_by(5) {
            robust(text.get(..end).unwrap());
            // Cut in the middle of the line too, on a char boundary.
            let mut mid = end.saturating_sub(7);
            while !text.is_char_boundary(mid) {
                mid -= 1;
            }
            robust(text.get(..mid).unwrap());
        }
    }
}

#[test]
fn mutated_examples() {
    let mut rng = Rng(0x00C0_FFEE);
    for path in example_files() {
        let text = std::fs::read_to_string(&path).unwrap();
        let lexed = lex(FileId::new(0), &text);
        let count = lexed.len();
        for _ in 0..40 {
            // Delete, duplicate or swap a few tokens.
            let mut pieces: Vec<&str> = (0..count).map(|i| lexed.text(&text, i)).collect();
            for _ in 0..3 {
                let i = rng.below(pieces.len());
                match rng.below(3) {
                    0 => {
                        pieces.remove(i);
                    }
                    1 => pieces.insert(i, pieces[i]),
                    _ => {
                        let j = rng.below(pieces.len());
                        pieces.swap(i, j);
                    }
                }
            }
            robust(&pieces.concat());
        }
    }
}

const FRAGMENTS: &[&str] = &[
    "fn",
    "type",
    "const",
    "interface",
    "test",
    "import",
    "pub",
    "priv",
    "exposed",
    "let",
    "assert",
    "use",
    "case",
    "when",
    "as",
    "dyn",
    "panic",
    "todo",
    "x",
    "foo",
    "Foo",
    "Some",
    "_",
    "1",
    "2.5",
    "0x1f",
    "\"s\"",
    "\"a{b}\"",
    "\"{\"",
    "\"{a.\"",
    "r#\"raw\"#",
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
    "<",
    ">",
    ">>",
    ">=",
    "<-",
    "->",
    "=>",
    "=",
    "==",
    "+",
    "-",
    "*",
    "|",
    "|>",
    "||",
    "&&",
    "!",
    "::",
    ":",
    ",",
    ".",
    "..",
    "@",
    "@!",
    "\n",
    "\n",
    "\n",
    " ",
    "  ",
    "// c\n",
    "/// d\n",
    "$",
    ";",
    "\"open",
    "/* x",
    "é",
];

#[test]
fn token_soups() {
    let mut rng = Rng(0x5EED_CAFE);
    for _ in 0..3000 {
        let len = rng.below(120);
        let mut text = String::new();
        for _ in 0..len {
            text.push_str(FRAGMENTS[rng.below(FRAGMENTS.len())]);
            if rng.below(3) == 0 {
                text.push(' ');
            }
        }
        robust(&text);
    }
}

#[test]
fn arbitrary_characters() {
    let mut rng = Rng(42);
    let alphabet: Vec<char> = "abcXYZ_09 \t\n\r(){}[]<>=-+*/%!&|^,.:;@\"\\#'`$~?é→\u{0}\u{FEFF}"
        .chars()
        .collect();
    for _ in 0..3000 {
        let len = rng.below(80);
        let text: String = (0..len)
            .map(|_| alphabet[rng.below(alphabet.len())])
            .collect();
        robust(&text);
    }
}

/// Code nested `depth` times with `open` and `close` around `inner`,
/// inside a function body.
fn nested(open: &str, inner: &str, close: &str, depth: usize) -> String {
    format!(
        "fn f() {{\n  let x = {}{inner}{}\n}}\n",
        open.repeat(depth),
        close.repeat(depth)
    )
}

#[test]
fn nesting_below_the_limit_parses() {
    // Each level of these uses one level of depth or a little more; the
    // limit is far above what they need, and the parser must not
    // overflow the stack of a test thread.
    let depth = usize::try_from(MAX_DEPTH).unwrap() / 2;
    let cases = [
        nested("(", "1", ")", depth),
        nested("[", "1", "]", depth),
        nested("{ ", "1", " }", depth),
        nested("-", "1", "", depth),
        nested("f(", "1", ")", depth),
        nested("fn() { ", "1", " }", depth / 2),
        nested("case x { _ => ", "1", " }", depth / 2),
        nested("Some(", "1", ")", depth),
        nested("1 + (", "1", ")", depth / 2),
        nested("", "1", " + 1", depth),
        nested("", "x", ".f", depth),
        nested("", "f", "()", depth),
        nested("", "1", " |> f(_, 1)", depth),
        format!("fn f() {{\n{}  x\n}}\n", "  use x <- f()\n".repeat(depth)),
        format!(
            "fn f(x: {}Int{}) {{}}\n",
            "List<".repeat(depth),
            ">".repeat(depth)
        ),
        format!("fn f({}x{}) {{}}\n", "(".repeat(depth), ")".repeat(depth)),
        format!(
            "fn f({}x{}) {{}}\n",
            "Some(".repeat(depth),
            ")".repeat(depth)
        ),
    ];
    for text in cases {
        let (_, _, errors) = parse_text("deep.cheby", &text);
        assert!(errors.is_empty(), "{text}: {errors:?}");
    }
}

#[test]
fn nesting_above_the_limit_is_e0016() {
    let depth = usize::try_from(MAX_DEPTH).unwrap() + 50;
    let cases = [
        nested("(", "1", ")", depth),
        nested("[", "1", "]", depth),
        nested("{ ", "1", " }", depth),
        nested("-", "1", "", depth),
        nested("f(", "1", ")", depth),
        nested("(", "1", "", depth),
        nested("fn() { ", "1", " }", depth),
        nested("case x { _ => ", "1", " }", depth),
        nested("Point { x: ", "1", " }", depth),
        // Chains nest their left operand (GAP(chain-depth)).
        nested("", "1", " + 1", depth),
        nested("", "1", " && True", depth),
        nested("", "1", " |> f(_, 1)", depth),
        nested("", "x", ".f", depth),
        nested("", "f", "()", depth),
        nested("", "f", "(1).g", depth),
        format!("fn f() {{\n{}  x\n}}\n", "  use x <- f()\n".repeat(depth)),
        // Indented: in column 1, `fn g` with no `}` to close the body
        // ends it (an item boundary).
        format!("fn f() {{\n{}\n}}\n", "  fn g() {\n".repeat(depth)),
        format!(
            "fn f({}x{}) {{}}\n",
            "Some(".repeat(depth),
            ")".repeat(depth)
        ),
        format!(
            "fn f(x: {}Int{}) {{}}\n",
            "List<".repeat(depth),
            ">".repeat(depth)
        ),
        format!(
            "fn f({}x{}) {{}}\nfn g() {{}}\n",
            "(".repeat(depth),
            ")".repeat(depth)
        ),
    ];
    for text in cases {
        let (_, tree, errors) = parse_text("deep.cheby", &text);
        let codes: Vec<String> = errors.iter().map(|d| d.code.to_string()).collect();
        assert_eq!(
            codes.iter().filter(|c| *c == "E0016").count(),
            1,
            "{codes:?}"
        );
        assert_eq!(tree.root().kind(), crate::SyntaxKind::SOURCE_FILE);
    }
}

#[test]
fn deeply_nested_beyond_any_stack() {
    // Far beyond the limit: still one error and a lossless tree, with the
    // rest skipped without recursion.
    let text = format!("fn f() {{ {} }}", "([{".repeat(20_000));
    assert!(robust(&text) >= 1);
    let text = "-".repeat(100_000);
    robust(&text);
}

#[test]
fn long_flat_input() {
    // Long chains are loops, not recursion, in the parser. Later passes
    // recurse along them, so past the limit they are one E0016
    // (GAP(chain-depth)).
    let text = format!("const x: Int = {}1\n", "a + ".repeat(50_000));
    assert_eq!(robust(&text), 1);
    let text = format!("const x: Int = {}a\n", "f(1).".repeat(20_000));
    assert_eq!(robust(&text), 1);
    let text = format!("const x: Int = {}1\n", "a + ".repeat(200));
    assert_eq!(robust(&text), 0);
    let text = "fn f() {}\n".repeat(20_000);
    assert_eq!(robust(&text), 0);
}

#[test]
fn long_runs_of_blank_and_comment_lines() {
    // Deciding each line break of a run used to rescan the rest of the
    // run, which took seconds for some ten thousand lines. The bound is
    // far above the linear cost, even in a debug build.
    let lines = 100_000;
    let texts = [
        format!("fn f() -> Int {{\n  1\n{}}}\n", "  // c\n".repeat(lines)),
        format!("fn f() -> Int {{\n  1\n}}\n{}", "\n".repeat(lines)),
        format!(
            "fn f() -> Int {{\n  x\n{}  |> g(_)\n}}\n",
            "\n".repeat(lines)
        ),
        format!(
            "fn f() -> Bool {{\n  a\n{}  && b\n}}\n",
            "  // c\n\n".repeat(lines)
        ),
    ];
    for text in texts {
        let start = std::time::Instant::now();
        assert_eq!(robust(&text), 0);
        let elapsed = start.elapsed();
        assert!(elapsed.as_secs() < 5, "took {elapsed:?}");
    }
}

#[test]
#[ignore = "timing aid: cargo test --release -p cheby_syntax -- --ignored parse_speed --nocapture"]
fn parse_speed() {
    let mut text = String::new();
    for path in example_files() {
        text.push_str(&std::fs::read_to_string(&path).unwrap());
        text.push('\n');
    }
    let text = text.repeat(25);
    let start = std::time::Instant::now();
    let lexed = lex(FileId::new(0), &text);
    let lexed_at = start.elapsed();
    let (events, errors, _) = parse_with_fuel(FileId::new(0), &lexed, &text);
    let parsed_at = start.elapsed();
    let tree = crate::tree::build(FileId::new(0), Arc::from(text.as_str()), &lexed, events);
    let built_at = start.elapsed();
    println!(
        "{} bytes, {} lines, {} errors: lex {lexed_at:?}, +parse {parsed_at:?}, +build {built_at:?}, {:?}",
        text.len(),
        text.lines().count(),
        errors.len(),
        tree.stats()
    );
}
