//! Tests of the `std::string` kernels: one unit test per kernel through
//! its C entry point, following `docs/stdlib/string.md` (code points
//! D-302, graphemes and case mapping D-310, clamping D-298, order by code
//! point D-136), property tests of `split`/`join`, `slice` and the padding
//! against reference implementations, the in-place paths at count one
//! (D-525), the multi-version state machine of plan §6, and the Unicode
//! version check of D-552.
//!
//! Every test runs with poisoning on (every `cfg(test)` build) and checks
//! that its thread's accounting domain returns to where it started.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::string_slice,
    clippy::unreadable_literal,
    reason = "test arithmetic and slicing on small, known values"
)]

use std::cmp::Ordering;

use proptest::prelude::*;

use super::kernels::*;
use super::ops;
use crate::abi::Word;
use crate::alloc::live;
use crate::string::tests::StaticImage;
use crate::string::{self, new_bytes, new_string};
use crate::{list, rc, values};

// ---------------------------------------------------------------------
// Helpers

/// Runs `f` and checks that it leaves the domain as it found it.
/// A property's cases: `normal`, or 4 under Miri, as the Miri job's
/// `PROPTEST_CASES`, which an explicit count would override (D-721,
/// D-884).
const fn miri_cases(normal: u32) -> u32 {
    if cfg!(miri) { 4 } else { normal }
}

fn balanced(f: impl FnOnce()) {
    let before = live();
    f();
    assert_eq!(live(), before, "objects leaked or freed twice");
}

/// The text of a borrowed string word.
fn text(word: Word) -> String {
    // SAFETY: tests pass live strings.
    unsafe { string::as_str(word) }.unwrap().to_owned()
}

/// Releases an owned word.
fn release(word: Word) {
    // SAFETY: tests own the words they release.
    unsafe { rc::drop(word) };
}

/// The text of an owned string word, which is released.
fn take(word: Word) -> String {
    let out = text(word);
    release(word);
    out
}

/// The strings of an owned list word, which is released.
fn take_list(word: Word) -> Vec<String> {
    // SAFETY: a live list of live strings.
    let out = unsafe { list::iter(word) }.map(text).collect();
    release(word);
    out
}

/// The ints of an owned list word, which is released.
fn take_ints(word: Word) -> Vec<i64> {
    // SAFETY: a live list of live `Int` words.
    let out = unsafe { list::iter(word) }
        .map(|w| unsafe { values::word_to_i64(w) })
        .collect();
    release(word);
    out
}

/// Calls a kernel as its `@external` wrapper does (D-525): the arguments
/// are new strings that the wrapper owns, passed borrowed, and dropped
/// after the call. Returns the owned result.
fn call(args: &[&str], kernel: impl FnOnce(&[Word]) -> Word) -> Word {
    let words: Vec<Word> = args.iter().map(|a| new_string(a)).collect();
    let result = kernel(&words);
    words.into_iter().for_each(release);
    result
}

/// [`call`] for a kernel of one string returning a string.
fn s1(kernel: unsafe extern "C" fn(Word) -> Word, a: &str) -> String {
    // SAFETY: a live string argument.
    take(call(&[a], |w| unsafe { kernel(w[0]) }))
}

/// [`call`] for a kernel of two strings returning a string.
fn s2(kernel: unsafe extern "C" fn(Word, Word) -> Word, a: &str, b: &str) -> String {
    // SAFETY: live string arguments.
    take(call(&[a, b], |w| unsafe { kernel(w[0], w[1]) }))
}

/// [`call`] for a kernel of two strings returning a `Bool`.
fn b2(kernel: unsafe extern "C" fn(Word, Word) -> bool, a: &str, b: &str) -> bool {
    let words = [new_string(a), new_string(b)];
    // SAFETY: live string arguments.
    let out = unsafe { kernel(words[0], words[1]) };
    words.into_iter().for_each(release);
    out
}

fn length(a: &str) -> i64 {
    let word = new_string(a);
    // SAFETY: a live string.
    let n = unsafe { cheby_string_length(word) };
    release(word);
    n
}

fn grapheme_length(a: &str) -> i64 {
    let word = new_string(a);
    // SAFETY: a live string.
    let n = unsafe { cheby_string_grapheme_length(word) };
    release(word);
    n
}

fn compare(a: &str, b: &str) -> Ordering {
    let result = call(&[a, b], |w| unsafe { cheby_string_compare(w[0], w[1]) });
    // SAFETY: a live `Order`.
    let out = unsafe { values::word_to_order(result) };
    release(result);
    out
}

fn split(a: &str, sep: &str) -> Vec<String> {
    // SAFETY: live string arguments.
    take_list(call(&[a, sep], |w| unsafe {
        cheby_string_split(w[0], w[1])
    }))
}

fn split_once(a: &str, sep: &str) -> Option<(String, String)> {
    // SAFETY: live string arguments.
    let result = call(&[a, sep], |w| unsafe {
        cheby_string_split_once(w[0], w[1])
    });
    // SAFETY: a live `Option<(String, String)>`.
    let out = unsafe { values::read_option(result) }.map(|pair| {
        // SAFETY: a live pair of strings.
        let fields = unsafe { values::read_tuple(pair) };
        (text(fields[0]), text(fields[1]))
    });
    release(result);
    out
}

/// A list of new strings, owned.
fn string_list(parts: &[&str]) -> Word {
    list::from_iter(parts.iter().map(|p| new_string(p)))
}

fn join(parts: &[&str], sep: &str) -> String {
    let list = string_list(parts);
    let sep = new_string(sep);
    // SAFETY: a live list of strings and a live string.
    let out = take(unsafe { cheby_string_join(list, sep) });
    release(list);
    release(sep);
    out
}

fn repeat(a: &str, count: i64) -> String {
    // SAFETY: a live string.
    take(call(&[a], |w| unsafe { cheby_string_repeat(w[0], count) }))
}

fn pad_start(a: &str, width: i64, pad: &str) -> String {
    // SAFETY: live strings.
    take(call(&[a, pad], |w| unsafe {
        cheby_string_pad_start(w[0], width, w[1])
    }))
}

fn pad_end(a: &str, width: i64, pad: &str) -> String {
    // SAFETY: live strings.
    take(call(&[a, pad], |w| unsafe {
        cheby_string_pad_end(w[0], width, w[1])
    }))
}

fn slice(a: &str, start: i64, end: i64) -> String {
    // SAFETY: a live string.
    take(call(&[a], |w| unsafe {
        cheby_string_slice(w[0], start, end)
    }))
}

fn from_bytes(data: &[u8]) -> Option<String> {
    let bytes = new_bytes(data);
    // SAFETY: a live `Bytes`.
    let result = unsafe { cheby_string_from_bytes(bytes) };
    release(bytes);
    // SAFETY: a live `Result<String, Nil>`.
    let out = unsafe { values::read_result(result) }.ok().map(text);
    release(result);
    out
}

fn from_values(points: &[i64]) -> Option<String> {
    let list = list::from_iter(points.iter().map(|&v| values::int_word(v)));
    // SAFETY: a live list of ints.
    let result = unsafe { cheby_string_from_code_point_values(list) };
    release(list);
    // SAFETY: a live `Result<String, Nil>`.
    let out = unsafe { values::read_result(result) }.ok().map(text);
    release(result);
    out
}

// ---------------------------------------------------------------------
// Unit tests, one per kernel, with the examples of string.md

#[test]
fn length_counts_code_points() {
    balanced(|| {
        assert_eq!(length(""), 0);
        assert_eq!(length("café"), 4);
        assert_eq!(length("e\u{301}"), 2);
        assert_eq!(length("🇳🇱"), 2);
        assert_eq!(length("𝄞x"), 2);
    });
}

#[test]
fn grapheme_length_counts_clusters() {
    balanced(|| {
        assert_eq!(grapheme_length("café"), 4);
        assert_eq!(grapheme_length("e\u{301}"), 1);
        assert_eq!(grapheme_length("🇳🇱"), 1);
        assert_eq!(grapheme_length("\r\n"), 1);
        assert_eq!(grapheme_length("👩\u{200D}👩\u{200D}👧"), 1);
        assert_eq!(grapheme_length(""), 0);
    });
}

#[test]
fn compare_orders_by_code_point() {
    balanced(|| {
        assert_eq!(compare("apple", "banana"), Ordering::Less);
        assert_eq!(compare("app", "apple"), Ordering::Less);
        assert_eq!(compare("Z", "a"), Ordering::Less);
        assert_eq!(compare("é", "z"), Ordering::Greater);
        assert_eq!(compare("same", "same"), Ordering::Equal);
        assert_eq!(compare("", ""), Ordering::Equal);
        // U+FFFF against U+10000: code-point order, not UTF-16 order.
        assert_eq!(compare("\u{FFFF}", "\u{10000}"), Ordering::Less);
    });
}

#[test]
fn starts_with_ends_with_and_contains() {
    balanced(|| {
        assert!(b2(
            cheby_string_starts_with,
            "https://example.com",
            "https://"
        ));
        assert!(!b2(cheby_string_starts_with, "http", "https"));
        assert!(b2(cheby_string_starts_with, "text", ""));
        assert!(b2(cheby_string_ends_with, "report.csv", ".csv"));
        assert!(!b2(cheby_string_ends_with, "report.csv", ".json"));
        assert!(b2(cheby_string_ends_with, "text", ""));
        assert!(b2(cheby_string_contains, "hello world", "o w"));
        assert!(!b2(cheby_string_contains, "hello", "world"));
        assert!(b2(cheby_string_contains, "", ""));
    });
}

#[test]
fn split_keeps_empty_parts() {
    balanced(|| {
        assert_eq!(split("a,b,,c", ","), ["a", "b", "", "c"]);
        assert_eq!(split("a,b,", ","), ["a", "b", ""]);
        assert_eq!(split("", ","), [""]);
        assert_eq!(split("abc", "-"), ["abc"]);
        assert_eq!(split("abc", ""), ["a", "b", "c"]);
        assert_eq!(split("", ""), Vec::<String>::new());
        assert_eq!(split("a::b", "::"), ["a", "b"]);
        assert_eq!(split("aaa", "aa"), ["", "a"]);
    });
}

#[test]
fn split_without_a_separator_shares_the_text() {
    balanced(|| {
        let text_word = new_string("no separator");
        let sep = new_string(",");
        // SAFETY: live strings, borrowed.
        let parts = unsafe { cheby_string_split(text_word, sep) };
        // SAFETY: a live list.
        let first = unsafe { list::iter(parts) }.next();
        assert_eq!(first, Some(text_word));
        release(parts);
        // SAFETY: still alive: the test's reference remains.
        assert!(unsafe { rc::is_unique(text_word) });
        release(text_word);
        release(sep);
    });
}

#[test]
fn split_once_splits_at_the_first_separator() {
    balanced(|| {
        let pair = |a: &str, b: &str| Some((a.to_owned(), b.to_owned()));
        assert_eq!(split_once("key=value=1", "="), pair("key", "value=1"));
        assert_eq!(split_once("key", "="), None);
        assert_eq!(split_once("abc", ""), pair("a", "bc"));
        assert_eq!(split_once("éa", ""), pair("é", "a"));
        assert_eq!(split_once("", ""), None);
        assert_eq!(split_once("", "="), None);
        assert_eq!(split_once("=", "="), pair("", ""));
    });
}

#[test]
fn to_code_points_and_graphemes() {
    balanced(|| {
        // SAFETY: live string arguments.
        let points =
            |a: &str| take_list(call(&[a], |w| unsafe { cheby_string_to_code_points(w[0]) }));
        // SAFETY: live string arguments.
        let clusters =
            |a: &str| take_list(call(&[a], |w| unsafe { cheby_string_to_graphemes(w[0]) }));
        assert_eq!(points("abc"), ["a", "b", "c"]);
        assert_eq!(points("e\u{301}"), ["e", "\u{301}"]);
        assert_eq!(points(""), Vec::<String>::new());
        assert_eq!(clusters("e\u{301}a"), ["e\u{301}", "a"]);
        assert_eq!(clusters("🇳🇱!"), ["🇳🇱", "!"]);
        assert_eq!(clusters(""), Vec::<String>::new());
    });
}

#[test]
fn add_concatenates() {
    balanced(|| {
        assert_eq!(s2(cheby_string_add, "con", "cat"), "concat");
        assert_eq!(s2(cheby_string_add, "", ""), "");
        assert_eq!(s2(cheby_string_add, "Ada", ""), "Ada");
        assert_eq!(s2(cheby_string_add, "", "é"), "é");
    });
}

/// D-903 (escapes of D-326, D-398): every byte below 0x20, `"` and `\`
/// get an escape and every other byte is copied.
#[test]
fn add_json_quoted_quotes_and_escapes() {
    balanced(|| {
        let quoted = |out: &str, t: &str| s2(cheby_string_add_json_quoted, out, t);
        assert_eq!(quoted("", ""), "\"\"");
        assert_eq!(quoted("[", ""), "[\"\"");
        assert_eq!(quoted("", "plain"), "\"plain\"");
        assert_eq!(quoted("", "a\"b\\c/"), r#""a\"b\\c/""#);
        let short = [(8, 'b'), (9, 't'), (10, 'n'), (12, 'f'), (13, 'r')];
        for byte in 0_u8..0x20 {
            let escape = short
                .iter()
                .find(|&&(b, _)| b == byte)
                .map_or_else(|| format!("\\u{byte:04x}"), |&(_, c)| format!("\\{c}"));
            let text = format!("x{}y", char::from(byte));
            assert_eq!(quoted("o", &text), format!("o\"x{escape}y\""), "{byte:#x}");
        }
        let controls: String = (0_u8..0x20).map(char::from).collect();
        let all = quoted("", &controls);
        assert_eq!(all.len(), 2 + 27 * 6 + 5 * 2, "{all}");
        let kept = "\u{7F}\u{80}\u{9F}é\u{2028}\u{2029}\u{FEFF}😀'<>&";
        assert_eq!(quoted("", kept), format!("\"{kept}\""));
        assert_eq!(quoted("", "é\n😀\""), "\"é\\n😀\\\"\"");
    });
}

#[test]
fn add_json_quoted_extends_a_unique_text_and_copies_others() {
    balanced(|| {
        let piece = new_string("d\"f");
        let acc = roomy("abc", 16);
        // SAFETY: live strings, borrowed by the kernel.
        let same = as_wrapper(acc, |acc| unsafe {
            cheby_string_add_json_quoted(acc, piece)
        });
        assert_eq!(same, acc, "a unique text with room is reused");
        assert_eq!(text(same), r#"abc"d\"f""#);
        // Without room: a copy with doubled capacity.
        let small = new_string("xy");
        // SAFETY: as above.
        let grown = as_wrapper(small, |s| unsafe { cheby_string_add_json_quoted(s, piece) });
        assert_ne!(grown, small);
        // SAFETY: a live string.
        assert_eq!(unsafe { string::capacity(grown) }, 8);
        assert_eq!(take(grown), r#"xy"d\"f""#);
        // A shared text is never changed.
        let shared = roomy("abc", 16);
        // SAFETY: a live string; the test keeps a second reference.
        unsafe { rc::dup(shared) };
        // SAFETY: live strings.
        let out = as_wrapper(shared, |s| unsafe {
            cheby_string_add_json_quoted(s, piece)
        });
        assert_ne!(out, shared);
        assert_eq!(text(shared), "abc");
        assert_eq!(take(out), r#"abc"d\"f""#);
        // The same object twice, with one reference: no in-place write,
        // with and without escapes.
        let aliased = roomy("x\"y", 16);
        // SAFETY: live strings.
        let doubled = unsafe { cheby_string_add_json_quoted(aliased, aliased) };
        assert_ne!(doubled, aliased);
        assert_eq!(take(doubled), r#"x"y"x\"y""#);
        assert_eq!(text(aliased), "x\"y");
        let plain = roomy("ab", 16);
        // SAFETY: live strings.
        let doubled = unsafe { cheby_string_add_json_quoted(plain, plain) };
        assert_ne!(doubled, plain);
        assert_eq!(take(doubled), r#"ab"ab""#);
        // A static text gets a new object.
        let image = StaticImage::new("lit");
        // SAFETY: a static string, borrowed; the result owned.
        let from_static = unsafe { cheby_string_add_json_quoted(image.word(), image.word()) };
        assert_eq!(take(from_static), r#"lit"lit""#);
        release(same);
        release(shared);
        release(aliased);
        release(plain);
        release(piece);
    });
}

#[test]
fn join_puts_the_separator_between_parts() {
    balanced(|| {
        assert_eq!(join(&["a", "b", "c"], ", "), "a, b, c");
        assert_eq!(join(&["only"], ", "), "only");
        assert_eq!(join(&[], ", "), "");
        assert_eq!(join(&["x", "y"], ""), "xy");
        assert_eq!(join(&["", ""], "-"), "-");
    });
}

#[test]
fn repeat_clamps_its_count() {
    balanced(|| {
        assert_eq!(repeat("ab", 3), "ababab");
        assert_eq!(repeat("ab", 0), "");
        assert_eq!(repeat("ab", -2), "");
        assert_eq!(repeat("ab", 1), "ab");
        assert_eq!(repeat("", i64::MAX), "");
        assert_eq!(repeat("é", 2), "éé");
    });
}

#[test]
fn pad_start_and_pad_end() {
    balanced(|| {
        assert_eq!(pad_start("7", 3, "0"), "007");
        assert_eq!(pad_start("121", 6, "ab"), "aba121");
        assert_eq!(pad_start("", 3, "-"), "---");
        assert_eq!(pad_start("é", 3, " "), "  é");
        assert_eq!(pad_start("long", 2, " "), "long");
        assert_eq!(pad_start("text", -3, " "), "text");
        assert_eq!(pad_start("x", 5, ""), "x");
        assert_eq!(pad_start("x", 4, "éü"), "éüéx");
        assert_eq!(pad_end("200", 8, " "), "200     ");
        assert_eq!(pad_end("121", 6, "ab"), "121aba");
        assert_eq!(pad_end("long", 2, " "), "long");
        assert_eq!(pad_end("x", i64::MIN, "-"), "x");
        assert_eq!(pad_end("x", 4, "éü"), "xéüé");
    });
}

#[test]
fn trimming_removes_white_space() {
    balanced(|| {
        assert_eq!(s1(cheby_string_trim, "  hi there \n"), "hi there");
        assert_eq!(s1(cheby_string_trim, "\t\r\n"), "");
        assert_eq!(s1(cheby_string_trim, "\u{3000}x"), "x");
        assert_eq!(s1(cheby_string_trim, "\u{85}x\u{A0}"), "x");
        // U+FEFF is not `White_Space`, though ECMAScript's `trim` removes it.
        assert_eq!(
            s1(cheby_string_trim, "\u{FEFF}x\u{FEFF}"),
            "\u{FEFF}x\u{FEFF}"
        );
        assert_eq!(s1(cheby_string_trim, ""), "");
        assert_eq!(s1(cheby_string_trim_start, "  hi "), "hi ");
        assert_eq!(s1(cheby_string_trim_start, "\t\nx"), "x");
        assert_eq!(s1(cheby_string_trim_end, " hi  "), " hi");
        assert_eq!(s1(cheby_string_trim_end, "line\r\n"), "line");
        // U+200B ZERO WIDTH SPACE is not `White_Space`.
        assert_eq!(s1(cheby_string_trim, "\u{200B}x"), "\u{200B}x");
    });
}

/// `words` through its entry point.
fn words(a: &str) -> Vec<String> {
    // SAFETY: a live string argument.
    take_list(call(&[a], |w| unsafe { cheby_string_words(w[0]) }))
}

/// The 25 code points with Unicode 17.0.0's `White_Space` property, the
/// table that `trim` and `words` share on JS in M4 (D-310, D-1092).
const WHITE_SPACE: [u32; 25] = [
    0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x20, 0x85, 0xA0, 0x1680, 0x2000, 0x2001, 0x2002, 0x2003, 0x2004,
    0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200A, 0x2028, 0x2029, 0x202F, 0x205F, 0x3000,
];

/// `words` splits at runs of `White_Space` and gives no empty parts; U+0085
/// separates words and U+FEFF does not, unlike ECMAScript's `\s` (D-1092).
#[test]
fn words_split_at_runs_of_white_space() {
    balanced(|| {
        let none: Vec<String> = Vec::new();
        assert_eq!(
            words("  one\ttwo\u{3000}three \r\n"),
            ["one", "two", "three"]
        );
        assert_eq!(words("a\u{85}b"), ["a", "b"]);
        assert_eq!(words("\u{FEFF}x"), ["\u{FEFF}x"]);
        assert_eq!(words("a\u{200B}b"), ["a\u{200B}b"]);
        assert_eq!(words("solo"), ["solo"]);
        assert_eq!(words(" \t "), none);
        assert_eq!(words(""), none);
        for point in WHITE_SPACE {
            let space = char::from_u32(point).unwrap();
            assert_eq!(
                words(&format!("é{space}{space}𝄞")),
                ["é", "𝄞"],
                "{point:#x}"
            );
        }
    });
}

/// The shared `White_Space` test holds for exactly the 25 code points of
/// the table, so `trim`, `words` and M4's JS table agree (D-310, D-1092).
#[test]
#[cfg_attr(miri, ignore = "scans every code point")]
fn white_space_is_the_table_of_25_code_points() {
    let found: Vec<u32> = (0..=char::MAX as u32)
        .filter_map(char::from_u32)
        .filter(|&c| super::ops::is_white_space(c))
        .map(u32::from)
        .collect();
    assert_eq!(found, WHITE_SPACE);
}

#[test]
fn removing_a_prefix_or_a_suffix() {
    balanced(|| {
        assert_eq!(
            s2(
                cheby_string_remove_prefix,
                "https://example.com",
                "https://"
            ),
            "example.com"
        );
        assert_eq!(s2(cheby_string_remove_prefix, "0042", "0"), "042");
        assert_eq!(s2(cheby_string_remove_prefix, "text", "x"), "text");
        assert_eq!(s2(cheby_string_remove_prefix, "text", ""), "text");
        assert_eq!(
            s2(cheby_string_remove_suffix, "https://example.com/", "/"),
            "https://example.com"
        );
        assert_eq!(s2(cheby_string_remove_suffix, "a//", "/"), "a/");
        assert_eq!(s2(cheby_string_remove_suffix, "text", ""), "text");
        assert_eq!(s2(cheby_string_remove_suffix, "same", "same"), "");
    });
}

#[test]
fn case_mapping_is_full_and_locale_free() {
    balanced(|| {
        assert_eq!(s1(cheby_string_lowercase, "Hello, World"), "hello, world");
        assert_eq!(s1(cheby_string_lowercase, "ÉCOLE"), "école");
        assert_eq!(s1(cheby_string_lowercase, "I"), "i");
        // Final sigma, language-independent (SpecialCasing.txt).
        assert_eq!(
            s1(cheby_string_lowercase, "ΟΔΟΣ"),
            "\u{3bf}\u{3b4}\u{3bf}\u{3c2}"
        );
        assert_eq!(s1(cheby_string_lowercase, "İ"), "i\u{307}");
        assert_eq!(s1(cheby_string_uppercase, "a1b2"), "A1B2");
        assert_eq!(s1(cheby_string_uppercase, "straße"), "STRASSE");
        assert_eq!(s1(cheby_string_uppercase, "ß"), "SS");
        assert_eq!(s1(cheby_string_uppercase, "i"), "I");
        assert_eq!(s1(cheby_string_uppercase, "ﬁ"), "FI");
        assert_eq!(s1(cheby_string_uppercase, "ŉ"), "ʼN");
    });
}

#[test]
fn final_sigma_lowercases_at_the_end_of_a_word() {
    balanced(|| {
        assert_eq!(s1(cheby_string_lowercase, "ΣΑΣ"), "\u{3c3}\u{3b1}\u{3c2}");
        assert_eq!(s1(cheby_string_lowercase, "Σ"), "σ");
    });
}

/// The kernels write case mappings code point by code point, with their
/// own `Final_Sigma` rule; it must agree with `str::to_lowercase`.
#[test]
fn case_mapping_agrees_with_the_standard_library() {
    let samples = [
        "ΟΔΟΣ ΟΔΟΣ. Σ.Σ ΑΣ' Σ'Α Α\u{301}Σ ΑΣ\u{301} ΑΣ\u{AD}Β A:Σ:B 1Σ ΣΣ",
        "ὈΔΥΣΣΕΎΣ ΣΑΣ-ΑΣ ΑΣ_ ΑΣ\u{300}\u{300}\u{300}x ΑΣ\u{2019} Σ\u{37A}Α",
        "İSTANBUL ǅ ǈ ß ŉ ﬁ Ω K Å",
    ];
    for text in samples {
        assert_eq!(
            ops::lowercase(text).collect::<String>(),
            text.to_lowercase()
        );
        assert_eq!(
            ops::uppercase(text).collect::<String>(),
            text.to_uppercase()
        );
    }
    // Every ASCII code point on either side of a sigma, after a cased
    // letter and after a space: the classes the rule reads off ASCII
    // directly.
    for c in (0_u8..128).map(char::from) {
        for text in [
            format!("A{c}Σ"),
            format!(" {c}Σ"),
            format!("AΣ{c}"),
            format!("AΣ{c}b"),
            format!("A{c}{c}Σ "),
        ] {
            assert_eq!(
                ops::lowercase(&text).collect::<String>(),
                text.to_lowercase(),
                "{text:?}"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: miri_cases(512), ..ProptestConfig::default() })]

    /// Texts dense in sigmas, cased and uncased letters and
    /// `Case_Ignorable` code points (`'`, `.`, `:`, combining marks, a
    /// soft hyphen, a modifier letter).
    #[test]
    fn lowercase_matches_the_standard_library(
        text in "[ΣσΑαAa ,.':^`\u{301}\u{AD}\u{2B0}\u{37A}1ǅ]{0,16}",
    ) {
        prop_assert_eq!(ops::lowercase(&text).collect::<String>(), text.to_lowercase());
    }
}

#[test]
fn reverse_reverses_code_points() {
    balanced(|| {
        assert_eq!(s1(cheby_string_reverse, "stressed"), "desserts");
        assert_eq!(s1(cheby_string_reverse, "e\u{301}x"), "x\u{301}e");
        assert_eq!(s1(cheby_string_reverse, ""), "");
    });
}

#[test]
fn slice_takes_clamped_code_point_positions() {
    balanced(|| {
        assert_eq!(slice("héllo", 1, 3), "él");
        assert_eq!(slice("abc", 1, 10), "bc");
        assert_eq!(slice("abc", -5, 1), "a");
        assert_eq!(slice("abc", 2, 1), "");
        assert_eq!(slice("abc", 0, 3), "abc");
        assert_eq!(slice("abc", i64::MIN, i64::MAX), "abc");
        assert_eq!(slice("abc", 3, 3), "");
        assert_eq!(slice("", 0, 1), "");
    });
}

#[test]
fn from_bytes_decodes_utf8() {
    balanced(|| {
        assert_eq!(
            from_bytes(&[0x63, 0x61, 0x66, 0xC3, 0xA9]).as_deref(),
            Some("café")
        );
        assert_eq!(from_bytes(b"").as_deref(), Some(""));
        assert_eq!(from_bytes(b"\xEF\xBB\xBFx").as_deref(), Some("\u{FEFF}x"));
        assert_eq!(from_bytes(&[0xFF]), None);
        assert_eq!(from_bytes(&[0xC3]), None);
        assert_eq!(from_bytes(&[0xED, 0xA0, 0x80]), None);
        assert_eq!(from_bytes(&[0xC0, 0x80]), None);
    });
}

/// D-905: `from_byte_range` clamps as `bytes::slice` does (D-298, D-334),
/// replaces each invalid UTF-8 sequence with U+FFFD, and copies even the
/// whole of `data`.
#[test]
fn from_byte_range_clamps_and_decodes_lossily() {
    let range = |data: &[u8], start: i64, end: i64| {
        let word = new_bytes(data);
        // SAFETY: a live `Bytes`, borrowed.
        let out = take(unsafe { cheby_string_from_byte_range(word, start, end) });
        release(word);
        out
    };
    balanced(|| {
        let text = "abcdefgé".as_bytes();
        for (start, end, want) in [
            (0, 2, "ab"),
            (-1, 2, "ab"),
            (2, 2, ""),
            (3, 1, ""),
            (0, 0, ""),
            (5, 9, "fgé"),
            (7, 9, "é"),
            (7, 8, "\u{FFFD}"),
            (8, 9, "\u{FFFD}"),
            (6, 99, "gé"),
            (99, 100, ""),
            (i64::MIN, i64::MAX, "abcdefgé"),
            (i64::MAX, i64::MIN, ""),
        ] {
            assert_eq!(range(text, start, end), want, "{start}..{end}");
        }
        assert_eq!(range(&[], 0, 0), "");
        assert_eq!(range(&[], -3, 3), "");
        assert_eq!(range(b"a\xFFb", 0, 3), "a\u{FFFD}b");
        let surrogate = [0xED, 0xA0, 0x80];
        assert_eq!(range(&surrogate, 0, 3), "\u{FFFD}".repeat(3));
        assert_eq!(range(&[0xF0, 0x9F, 0x98], 0, 3), "\u{FFFD}");
        assert_eq!(range(b"\xEF\xBB\xBFx", 0, 4), "\u{FEFF}x");
        // The whole of `data` is copied into a new `String`, never shared.
        let word = new_bytes(b"whole");
        // SAFETY: a live `Bytes`, borrowed.
        let out = unsafe { cheby_string_from_byte_range(word, 0, 5) };
        assert_ne!(out, word);
        assert_eq!(take(out), "whole");
        // SAFETY: a live `Bytes`.
        assert_eq!(unsafe { values::bytes_of(word) }, b"whole");
        release(word);
    });
}

#[test]
fn code_point_values_round_trip() {
    balanced(|| {
        // SAFETY: live string arguments.
        let values_of = |a: &str| {
            take_ints(call(&[a], |w| unsafe {
                cheby_string_to_code_point_values(w[0])
            }))
        };
        assert_eq!(values_of("aé"), [97, 233]);
        assert_eq!(values_of(""), Vec::<i64>::new());
        assert_eq!(from_values(&[104, 105]).as_deref(), Some("hi"));
        assert_eq!(from_values(&[0x1F600]).as_deref(), Some("\u{1F600}"));
        assert_eq!(from_values(&[]).as_deref(), Some(""));
        assert_eq!(from_values(&[0xD800]), None);
        assert_eq!(from_values(&[0xDFFF]), None);
        assert_eq!(from_values(&[0x110000]), None);
        assert_eq!(from_values(&[-1]), None);
        assert_eq!(from_values(&[i64::MAX]), None);
    });
}

#[test]
fn kernels_accept_static_strings() {
    balanced(|| {
        let image = StaticImage::new("  lit  ");
        let word = image.word();
        // SAFETY: a static string, borrowed; results owned.
        unsafe {
            assert_eq!(take(cheby_string_trim(word)), "lit");
            let same = cheby_string_slice(word, 0, 100);
            assert_eq!(same, word);
            release(same);
            assert_eq!(take(cheby_string_add(word, word)), "  lit    lit  ");
        }
    });
}

/// D-552: case mapping (the standard library) and grapheme segmentation
/// (`unicode-segmentation`) follow one Unicode version, which
/// `docs/stdlib/string.md` records. The shipped build is the pinned stable
/// toolchain's (D-277); the nightly of the sanitizer and Miri jobs (D-721)
/// may bring a newer Unicode in its std, so those builds leave the check to
/// the stable one (D-884).
#[test]
#[cfg_attr(
    any(cheby_sanitize = "address", cheby_sanitize = "thread", miri),
    ignore = "the nightly of the sanitizer and Miri jobs (D-721) may follow a newer Unicode \
              than unicode-segmentation; the stable build checks it (D-277, D-552)"
)]
fn unicode_versions_agree() {
    let (major, minor, update) = char::UNICODE_VERSION;
    assert_eq!(
        (u64::from(major), u64::from(minor), u64::from(update)),
        unicode_segmentation::UNICODE_VERSION,
        "the standard library and unicode-segmentation follow different Unicode versions"
    );
    assert_eq!(
        char::UNICODE_VERSION,
        (17, 0, 0),
        "update string.md's recorded version"
    );
}

// ---------------------------------------------------------------------
// In place at count one (D-525)

/// A unique string with room for `extra` more bytes.
fn roomy(text: &str, extra: usize) -> Word {
    let word = crate::object::ptr_to_word(string::alloc_string((text.len() + extra) as u64));
    // SAFETY: the unique string built here.
    unsafe { string::append(word, text.as_bytes()) }
}

/// Calls `kernel` on `text` as its wrapper does (borrowed, then dropped)
/// and returns the result.
fn as_wrapper(text: Word, kernel: impl FnOnce(Word) -> Word) -> Word {
    let result = kernel(text);
    release(text);
    result
}

#[test]
fn add_appends_in_place_to_a_unique_text_with_room() {
    balanced(|| {
        let piece = new_string("def");
        let acc = roomy("abc", 16);
        // SAFETY: live strings, borrowed by the kernel.
        let same = as_wrapper(acc, |acc| unsafe { cheby_string_add(acc, piece) });
        assert_eq!(same, acc, "a unique text with room is reused");
        // SAFETY: a live string.
        assert!(unsafe { rc::is_unique(same) });
        assert_eq!(text(same), "abcdef");
        // Without room: a copy with doubled capacity, then in place again.
        let small = new_string("xy");
        // SAFETY: as above.
        let grown = as_wrapper(small, |s| unsafe { cheby_string_add(s, piece) });
        assert_ne!(grown, small);
        // SAFETY: a live string.
        assert_eq!(unsafe { string::capacity(grown) }, 5);
        // SAFETY: as above.
        let grown2 = as_wrapper(grown, |s| unsafe { cheby_string_add(s, piece) });
        assert_ne!(grown2, grown);
        // SAFETY: a live string.
        assert_eq!(unsafe { string::capacity(grown2) }, 10);
        // Length 8 of 10: one more byte fits in place.
        let bang = new_string("!");
        // SAFETY: as above.
        let same2 = as_wrapper(grown2, |s| unsafe { cheby_string_add(s, bang) });
        assert_eq!(same2, grown2);
        assert_eq!(text(same2), "xydefdef!");
        release(same);
        release(same2);
        release(piece);
        release(bang);
    });
}

#[test]
fn shared_and_aliased_texts_are_never_changed() {
    balanced(|| {
        let shared = roomy("abc", 16);
        let piece = new_string("!");
        // SAFETY: a live string; the test keeps a second reference.
        unsafe { rc::dup(shared) };
        // SAFETY: live strings.
        let out = as_wrapper(shared, |s| unsafe { cheby_string_add(s, piece) });
        assert_ne!(out, shared);
        assert_eq!(text(shared), "abc");
        assert_eq!(take(out), "abc!");
        // `add(s, s)` with one reference: no in-place write.
        // SAFETY: live strings.
        let doubled = unsafe { cheby_string_add(shared, shared) };
        assert_ne!(doubled, shared);
        assert_eq!(take(doubled), "abcabc");
        assert_eq!(text(shared), "abc");
        // Trimming a shared text copies.
        // SAFETY: as above.
        unsafe { rc::dup(shared) };
        // SAFETY: as above.
        let sliced = as_wrapper(shared, |s| unsafe { cheby_string_slice(s, 1, 2) });
        assert_ne!(sliced, shared);
        assert_eq!(take(sliced), "b");
        assert_eq!(text(shared), "abc");
        release(shared);
        release(piece);
    });
}

#[test]
fn shrinking_kernels_work_in_place_at_count_one() {
    balanced(|| {
        type Kernel = fn(Word, Word) -> Word;
        let cases: [(&str, Kernel, &str); 7] = [
            // SAFETY (each): live strings, borrowed.
            ("  pad  ", |t, _| unsafe { cheby_string_trim(t) }, "pad"),
            (
                "  pad  ",
                |t, _| unsafe { cheby_string_trim_start(t) },
                "pad  ",
            ),
            (
                "  pad  ",
                |t, _| unsafe { cheby_string_trim_end(t) },
                "  pad",
            ),
            (
                "prefix-rest",
                |t, p| unsafe { cheby_string_remove_prefix(t, p) },
                "rest",
            ),
            (
                "rest-prefix-",
                |t, p| unsafe { cheby_string_remove_suffix(t, p) },
                "rest-",
            ),
            ("héllo", |t, _| unsafe { cheby_string_slice(t, 1, 3) }, "él"),
            ("héllo", |t, _| unsafe { cheby_string_slice(t, 0, 2) }, "hé"),
        ];
        let affix = new_string("prefix-");
        for (input, kernel, expected) in cases {
            let word = new_string(input);
            let out = as_wrapper(word, |w| kernel(w, affix));
            assert_eq!(out, word, "{input} -> {expected} in place");
            assert_eq!(take(out), expected);
        }
        release(affix);
    });
}

#[test]
fn pad_end_appends_in_place() {
    balanced(|| {
        let pad = new_string("ab");
        let word = roomy("1", 8);
        // SAFETY: live strings, borrowed.
        let out = as_wrapper(word, |w| unsafe { cheby_string_pad_end(w, 6, pad) });
        assert_eq!(out, word);
        assert_eq!(take(out), "1ababa");
        release(pad);
    });
}

// ---------------------------------------------------------------------
// Property tests against reference implementations

/// Text drawn from a small alphabet that mixes ASCII, two- to four-byte
/// code points, a combining mark and whitespace, so separators occur.
fn small_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec![
            'a', 'b', ',', ' ', 'é', '\u{301}', '€', '𝄞', '\n', '\u{3000}',
        ]),
        0..24,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

/// The reference split: scan for the separator from the start, by bytes.
fn reference_split(text: &str, sep: &str) -> Vec<String> {
    if sep.is_empty() {
        return text.chars().map(String::from).collect();
    }
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix(sep) {
            parts.push(std::mem::take(&mut current));
            rest = after;
        } else {
            let c = rest.chars().next().unwrap();
            current.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    parts.push(current);
    parts
}

/// The reference padding: one code point at a time from a cycled pad.
fn reference_pad(text: &str, width: i64, pad: &str, start: bool) -> String {
    let n = text.chars().count() as i64;
    if width <= n || pad.is_empty() {
        return text.to_owned();
    }
    let fill: String = pad.chars().cycle().take((width - n) as usize).collect();
    if start {
        fill + text
    } else {
        text.to_owned() + &fill
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: miri_cases(256), ..ProptestConfig::default() })]

    #[test]
    fn split_matches_the_reference_and_join_inverts_it(
        text in small_text(),
        sep in prop::sample::select(vec!["", ",", " ", "a,", "é\u{301}", "𝄞", "ab"]),
    ) {
        balanced(|| {
            let parts = split(&text, sep);
            assert_eq!(parts, reference_split(&text, sep));
            let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
            assert_eq!(join(&parts, sep), text);
            assert_eq!(
                split_once(&text, sep),
                if sep.is_empty() {
                    text.chars().next().map(|c| {
                        (c.to_string(), text[c.len_utf8()..].to_owned())
                    })
                } else {
                    parts.first().filter(|_| parts.len() > 1).map(|first| {
                        ((*first).to_owned(), text[first.len() + sep.len()..].to_owned())
                    })
                },
            );
        });
    }

    #[test]
    fn words_match_split_whitespace(text in small_text()) {
        balanced(|| {
            let want: Vec<&str> = text.split_whitespace().collect();
            assert_eq!(words(&text), want);
        });
    }

    #[test]
    fn join_matches_the_reference(
        parts in prop::collection::vec(small_text(), 0..8),
        sep in small_text(),
    ) {
        balanced(|| {
            let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
            assert_eq!(join(&parts, &sep), parts.join(&sep));
        });
    }

    #[test]
    fn slice_matches_the_reference(text in small_text(), start in -4_i64..30, end in -4_i64..30) {
        balanced(|| {
            let n = text.chars().count() as i64;
            let (a, b) = (start.clamp(0, n), end.clamp(0, n));
            let expected: String = if b > a {
                text.chars().skip(a as usize).take((b - a) as usize).collect()
            } else {
                String::new()
            };
            assert_eq!(slice(&text, start, end), expected);
        });
    }

    #[test]
    fn padding_matches_the_reference(
        text in small_text(),
        width in -4_i64..40,
        pad in small_text(),
    ) {
        balanced(|| {
            assert_eq!(pad_start(&text, width, &pad), reference_pad(&text, width, &pad, true));
            assert_eq!(pad_end(&text, width, &pad), reference_pad(&text, width, &pad, false));
            let length = text.chars().count();
            assert_eq!(ops::length(&text), length);
        });
    }
}

// ---------------------------------------------------------------------
// The multi-version state machine (plan §6)

/// One step over live string versions.
#[derive(Debug, Clone)]
enum Step {
    /// Another reference to version `i` becomes a version of its own.
    Share(usize),
    /// Version `i` is dropped.
    Drop(usize),
    /// `string::add(v, piece)`.
    Add(usize, String),
    /// `string::pad_end(v, length + k, pad)`.
    PadEnd(usize, i64, String),
    /// `string::slice(v, a, b)`.
    Slice(usize, i64, i64),
    /// `string::trim_end(v)`.
    TrimEnd(usize),
    /// `string::remove_prefix(v, the first `k` bytes' code points)`.
    RemovePrefix(usize, usize),
    /// `string::add(v, v)`, the same object twice.
    AddSelf(usize),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        (0..8_usize).prop_map(Step::Share),
        (0..8_usize).prop_map(Step::Drop),
        (0..8_usize, small_text()).prop_map(|(i, s)| Step::Add(i, s)),
        (0..8_usize, 0..6_i64, small_text()).prop_map(|(i, k, s)| Step::PadEnd(i, k, s)),
        (0..8_usize, -2..12_i64, -2..12_i64).prop_map(|(i, a, b)| Step::Slice(i, a, b)),
        (0..8_usize).prop_map(Step::TrimEnd),
        (0..8_usize, 0..4_usize).prop_map(|(i, k)| Step::RemovePrefix(i, k)),
        (0..8_usize).prop_map(Step::AddSelf),
    ]
}

/// The most live versions the machine keeps.
const VERSIONS: usize = 8;

/// Applies an updating kernel to version `i` as its wrapper does: the
/// version's reference is given to the wrapper, which drops it after the
/// call, and the result replaces it.
fn update(
    versions: &mut [(Word, String)],
    i: usize,
    model: String,
    kernel: impl FnOnce(Word) -> Word,
) {
    let (word, text_model) = &mut versions[i];
    *word = as_wrapper(*word, kernel);
    *text_model = model;
}

proptest! {
    #![proptest_config(ProptestConfig { cases: miri_cases(128), ..ProptestConfig::default() })]

    /// N live versions under random sharing, dropping and in-place-capable
    /// updates, each checked against its own model after every step; the
    /// domain returns to its start.
    #[test]
    fn versions_stay_independent(
        start in small_text(),
        steps in prop::collection::vec(step(), 1..60),
    ) {
        balanced(|| {
            let mut versions: Vec<(Word, String)> = vec![(new_string(&start), start.clone())];
            for step in &steps {
                let n = versions.len();
                match step.clone() {
                    Step::Share(i) if n < VERSIONS => {
                        let (word, model) = versions[i % n].clone();
                        // SAFETY: a live version; the copy owns the new reference.
                        unsafe { rc::dup(word) };
                        versions.push((word, model));
                    }
                    Step::Drop(i) if n > 1 => {
                        let (word, _) = versions.swap_remove(i % n);
                        release(word);
                    }
                    Step::Add(i, piece) => {
                        let i = i % n;
                        let model = versions[i].1.clone() + &piece;
                        let piece = new_string(&piece);
                        // SAFETY: live strings, borrowed.
                        update(&mut versions, i, model, |w| unsafe { cheby_string_add(w, piece) });
                        release(piece);
                    }
                    Step::PadEnd(i, k, pad) => {
                        let i = i % n;
                        let current = versions[i].1.clone();
                        let width = current.chars().count() as i64 + k;
                        let model = reference_pad(&current, width, &pad, false);
                        let pad = new_string(&pad);
                        // SAFETY: live strings, borrowed.
                        update(&mut versions, i, model, |w| unsafe { cheby_string_pad_end(w, width, pad) });
                        release(pad);
                    }
                    Step::Slice(i, a, b) => {
                        let i = i % n;
                        let current = &versions[i].1;
                        let len = current.chars().count() as i64;
                        let (x, y) = (a.clamp(0, len), b.clamp(0, len));
                        let model: String = if y > x {
                            current.chars().skip(x as usize).take((y - x) as usize).collect()
                        } else {
                            String::new()
                        };
                        // SAFETY: a live string, borrowed.
                        update(&mut versions, i, model, |w| unsafe { cheby_string_slice(w, a, b) });
                    }
                    Step::TrimEnd(i) => {
                        let i = i % n;
                        let model = versions[i].1.trim_end().to_owned();
                        // SAFETY: a live string, borrowed.
                        update(&mut versions, i, model, |w| unsafe { cheby_string_trim_end(w) });
                    }
                    Step::RemovePrefix(i, k) => {
                        let i = i % n;
                        let current = versions[i].1.clone();
                        let prefix: String = current.chars().take(k).collect();
                        let model = current[prefix.len()..].to_owned();
                        let prefix = new_string(&prefix);
                        // SAFETY: live strings, borrowed.
                        update(&mut versions, i, model, |w| unsafe { cheby_string_remove_prefix(w, prefix) });
                        release(prefix);
                    }
                    Step::AddSelf(i) => {
                        let i = i % n;
                        let model = versions[i].1.repeat(2);
                        // SAFETY: a live string passed twice, borrowed; the
                        // wrapper owns one reference per argument.
                        update(&mut versions, i, model, |w| unsafe {
                            rc::dup(w);
                            let out = cheby_string_add(w, w);
                            rc::drop(w);
                            out
                        });
                    }
                    _ => {}
                }
                for (word, model) in &versions {
                    assert_eq!(&text(*word), model);
                }
            }
            for (word, _) in versions {
                release(word);
            }
        });
    }
}
