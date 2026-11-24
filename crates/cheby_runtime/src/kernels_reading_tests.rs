//! The proof of the MIR's table of kernels that only read their
//! arguments (`crates/cheby_mir/src/kernels_reading.rs`): a direct call
//! of one passes its counted arguments borrowed, without the owned
//! temporary that lets an in-place kernel see the wrapper's counts
//! (D-525). Each listed kernel is called here with unique arguments and
//! with arguments held twice, and must leave every count as it found it,
//! keep no reference and write nothing.

use crate::abi::{self, Word};
use crate::alloc::live;
use crate::bytes::kernels::{
    cheby_bytes_from_string, cheby_bytes_get, cheby_bytes_index_of, cheby_bytes_length,
};
use crate::hamt::{
    cheby_map_get, cheby_map_has_key, cheby_map_insert, cheby_map_is_empty, cheby_map_new,
    cheby_map_size, cheby_set_contains, cheby_set_insert, cheby_set_is_empty, cheby_set_new,
    cheby_set_size,
};
use crate::list::{cheby_list_get, cheby_list_length};
use crate::string::{new_string, to_rust_string};
use crate::text::kernels::{
    cheby_string_compare, cheby_string_contains, cheby_string_ends_with,
    cheby_string_grapheme_length, cheby_string_length, cheby_string_starts_with,
};

/// The symbols of the MIR's table, read from its source: the third string
/// of each `("…", "…", "…")` between the table's opening line and its
/// `];`, however the entry is broken over lines.
fn reading_kernels() -> Vec<String> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../cheby_mir/src/kernels_reading.rs"
    );
    let source = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read the MIR's table {path} ({error})"));
    let open = "pub const READING_KERNELS: &[(&str, &str, &str)] = &[";
    let (_, body) = source
        .split_once(open)
        .unwrap_or_else(|| panic!("{path} has no `{open}`"));
    let (list, _) = body
        .split_once("];")
        .unwrap_or_else(|| panic!("{path}: the table has no closing `];`"));
    list.split("),")
        .filter(|entry| entry.contains('"'))
        .map(|entry| {
            let strings: Vec<&str> = entry.split('"').skip(1).step_by(2).collect();
            let [_, _, symbol] = strings.as_slice() else {
                panic!("{path}: `{entry}` is not `(\"…\", \"…\", \"…\")`");
            };
            (*symbol).to_owned()
        })
        .collect()
}

/// A string, owned.
fn text(s: &str) -> Word {
    new_string(s)
}

/// The list of `items`, strings, owned.
fn list_of(items: &[&str]) -> Word {
    crate::list::from_iter(items.iter().map(|s| text(s)))
}

/// The map of `pairs`, strings to strings, owned.
fn map_of(pairs: &[(&str, &str)]) -> Word {
    let mut m = cheby_map_new();
    for &(k, v) in pairs {
        let (k, v) = (text(k), text(v));
        // SAFETY: live borrowed arguments.
        let next = unsafe { cheby_map_insert(m, k, v) };
        for word in [m, k, v] {
            release(word);
        }
        m = next;
    }
    m
}

/// The set of `items`, strings, owned.
fn set_of(items: &[&str]) -> Word {
    let mut s = cheby_set_new();
    for item in items {
        let item = text(item);
        // SAFETY: live borrowed arguments.
        let next = unsafe { cheby_set_insert(s, item) };
        release(s);
        release(item);
        s = next;
    }
    s
}

/// Bytes, owned.
fn bytes_of(s: &str) -> Word {
    let t = text(s);
    // SAFETY: a live string.
    let b = unsafe { cheby_bytes_from_string(t) };
    release(t);
    b
}

fn release(word: Word) {
    // SAFETY: a live word the test owns.
    unsafe { crate::rc::drop(word) };
}

fn hold(word: Word) {
    // SAFETY: a live word.
    unsafe { crate::rc::dup(word) };
}

/// The count of a counted word.
fn count(word: Word) -> Option<i32> {
    // SAFETY: a live word.
    unsafe { crate::rc::count_of(word) }
}

/// The debug text of a value: what it holds.
fn shown(word: Word) -> String {
    // SAFETY: a live word.
    let text = unsafe { crate::generic::debug_object(word) };
    // SAFETY: a live string.
    let out = unsafe { to_rust_string(text) }.unwrap();
    release(text);
    out
}

/// A counted result, or a scalar one.
enum Out {
    Word(Word),
    Scalar,
}

/// A kernel of the table, with the arguments it is called with: the
/// counted ones built fresh by `args`, the call made by `call`.
struct Probe {
    symbol: &'static str,
    args: fn() -> Vec<Word>,
    call: fn(&[Word]) -> Out,
}

/// The counted argument `n` of a probe.
fn arg(args: &[Word], n: usize) -> Word {
    args[n]
}

#[expect(clippy::too_many_lines, reason = "one entry per kernel of the table")]
fn probes() -> Vec<Probe> {
    vec![
        Probe {
            symbol: "cheby_bytes_get",
            args: || vec![bytes_of("abc")],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                Out::Word(unsafe { cheby_bytes_get(arg(a, 0), 1) })
            },
        },
        Probe {
            symbol: "cheby_bytes_index_of",
            args: || vec![bytes_of("abcabc"), bytes_of("ca")],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                Out::Word(unsafe { cheby_bytes_index_of(arg(a, 0), arg(a, 1), 0) })
            },
        },
        Probe {
            symbol: "cheby_bytes_length",
            args: || vec![bytes_of("abc")],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                assert_eq!(unsafe { cheby_bytes_length(arg(a, 0)) }, 3);
                Out::Scalar
            },
        },
        Probe {
            symbol: "cheby_list_get",
            args: || vec![list_of(&["a", "bb", "ccc"])],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                Out::Word(unsafe { cheby_list_get(arg(a, 0), 1) })
            },
        },
        Probe {
            symbol: "cheby_list_length",
            args: || vec![list_of(&["a", "bb"])],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                assert_eq!(unsafe { cheby_list_length(arg(a, 0)) }, 2);
                Out::Scalar
            },
        },
        Probe {
            symbol: "cheby_map_get",
            args: || vec![map_of(&[("k", "v"), ("l", "w")]), text("k")],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                Out::Word(unsafe { cheby_map_get(arg(a, 0), arg(a, 1)) })
            },
        },
        Probe {
            symbol: "cheby_map_has_key",
            args: || vec![map_of(&[("k", "v")]), text("k")],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                assert!(unsafe { cheby_map_has_key(arg(a, 0), arg(a, 1)) });
                Out::Scalar
            },
        },
        Probe {
            symbol: "cheby_map_is_empty",
            args: || vec![map_of(&[("k", "v")])],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                assert!(!unsafe { cheby_map_is_empty(arg(a, 0)) });
                Out::Scalar
            },
        },
        Probe {
            symbol: "cheby_map_size",
            args: || vec![map_of(&[("k", "v")])],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                assert_eq!(unsafe { cheby_map_size(arg(a, 0)) }, 1);
                Out::Scalar
            },
        },
        Probe {
            symbol: "cheby_set_contains",
            args: || vec![set_of(&["x", "y"]), text("y")],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                assert!(unsafe { cheby_set_contains(arg(a, 0), arg(a, 1)) });
                Out::Scalar
            },
        },
        Probe {
            symbol: "cheby_set_is_empty",
            args: || vec![set_of(&["x"])],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                assert!(!unsafe { cheby_set_is_empty(arg(a, 0)) });
                Out::Scalar
            },
        },
        Probe {
            symbol: "cheby_set_size",
            args: || vec![set_of(&["x", "y"])],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                assert_eq!(unsafe { cheby_set_size(arg(a, 0)) }, 2);
                Out::Scalar
            },
        },
        Probe {
            symbol: "cheby_string_compare",
            args: || vec![text("apple"), text("pear")],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                Out::Word(unsafe { cheby_string_compare(arg(a, 0), arg(a, 1)) })
            },
        },
        Probe {
            symbol: "cheby_string_contains",
            args: || vec![text("hello world"), text("lo w")],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                assert!(unsafe { cheby_string_contains(arg(a, 0), arg(a, 1)) });
                Out::Scalar
            },
        },
        Probe {
            symbol: "cheby_string_ends_with",
            args: || vec![text("hello world"), text("world")],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                assert!(unsafe { cheby_string_ends_with(arg(a, 0), arg(a, 1)) });
                Out::Scalar
            },
        },
        Probe {
            symbol: "cheby_string_grapheme_length",
            args: || vec![text("héllo")],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                assert_eq!(unsafe { cheby_string_grapheme_length(arg(a, 0)) }, 5);
                Out::Scalar
            },
        },
        Probe {
            symbol: "cheby_string_length",
            args: || vec![text("héllo")],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                assert_eq!(unsafe { cheby_string_length(arg(a, 0)) }, 5);
                Out::Scalar
            },
        },
        Probe {
            symbol: "cheby_string_starts_with",
            args: || vec![text("hello world"), text("hello")],
            call: |a| {
                // SAFETY: the probe's own live arguments, borrowed.
                assert!(unsafe { cheby_string_starts_with(arg(a, 0), arg(a, 1)) });
                Out::Scalar
            },
        },
    ]
}

/// Calls `probe` with its arguments held `holds` times, and checks that
/// the counts and the contents of every argument are the same after the
/// call, with its result alive and once it is released, and that the
/// result is no argument.
fn check(probe: &Probe, holds: usize) {
    let before = live();
    let args = (probe.args)();
    for &a in &args {
        for _ in 1..holds {
            hold(a);
        }
    }
    let counts: Vec<Option<i32>> = args.iter().map(|&a| count(a)).collect();
    let contents: Vec<String> = args.iter().map(|&a| shown(a)).collect();
    let same = |when: &str| {
        let now: Vec<Option<i32>> = args.iter().map(|&a| count(a)).collect();
        assert_eq!(now, counts, "{}: a count changed {when}", probe.symbol);
        let now: Vec<String> = args.iter().map(|&a| shown(a)).collect();
        assert_eq!(
            now, contents,
            "{}: an argument changed {when}",
            probe.symbol
        );
    };
    let out = (probe.call)(&args);
    same("while its result is alive");
    if let Out::Word(result) = out {
        assert!(
            !args.contains(&result),
            "{}: returned an argument",
            probe.symbol
        );
        assert!(
            abi::is_tagged_int(result) || count(result).is_some(),
            "{}",
            probe.symbol
        );
        release(result);
    }
    same("after its result is released");
    for &a in &args {
        for _ in 0..holds {
            release(a);
        }
    }
    assert_eq!(
        live(),
        before,
        "{}: objects leaked or freed twice",
        probe.symbol
    );
}

/// Every kernel of the MIR's table only reads: called with unique
/// arguments and with arguments held twice, it changes no count, keeps no
/// reference and writes nothing. Each has a probe here, and every probe is
/// in the table.
#[test]
fn reading_kernels_only_read() {
    let table = reading_kernels();
    let all = probes();
    let probed: Vec<&str> = all.iter().map(|p| p.symbol).collect();
    let mut sorted = table;
    sorted.sort();
    assert_eq!(
        probed, sorted,
        "the probes and crates/cheby_mir/src/kernels_reading.rs differ"
    );
    let registered: Vec<&str> = crate::symbols().into_iter().map(|(name, _)| name).collect();
    for probe in &all {
        assert!(registered.contains(&probe.symbol), "{}", probe.symbol);
        for holds in [1, 2] {
            check(probe, holds);
        }
    }
}
