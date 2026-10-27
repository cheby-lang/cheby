//! The REPL on code records (K4c, GAP(repl-on-records)): every entry's
//! units are compiled alone into records and loaded into the session's
//! JIT, and a redefinition takes over its slot as before (D-043, D-712).

use crate::session::{Output, Session, SessionOptions, Start};

/// What a session prints, both streams in order.
#[derive(Default)]
struct Printed(String);

impl Output for Printed {
    fn stdout(&mut self, text: &str) {
        self.0.push_str(text);
    }

    fn stderr(&mut self, text: &str) {
        self.0.push_str(text);
    }
}

/// Runs `entries` in a session outside any package and returns what it
/// printed.
fn session_output(entries: &'static [&'static str]) -> String {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            // Outside any package: the system's temporary directory.
            let dir = std::env::temp_dir();
            let options = SessionOptions {
                threads: Some(2),
                ..SessionOptions::default()
            };
            let Start::Ready(mut session, _) = Session::start(&dir, options).unwrap() else {
                panic!("the session did not start");
            };
            let mut out = Printed::default();
            for entry in entries {
                session.eval(entry, &mut out).unwrap();
            }
            out.0
        })
        .unwrap()
        .join()
        .unwrap()
}

/// D-043, D-456: a redefinition of a function with the same ABI signature
/// takes over its slot, so code compiled later and earlier reaches the new
/// code (`tests/repl/redefine_type.repl`'s case, on records).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_repl_redefinition_takes_over_the_slot() {
    let printed = session_output(&[
        "fn name(n: Int) -> String {\n  \"old {n}\"\n}",
        "fn greet(n: Int) -> String {\n  name(n)\n}",
        "greet(1)",
        "fn name(n: Int) -> String {\n  \"new {n}\"\n}",
        "name(2)",
        "greet(3)",
    ]);
    assert_eq!(
        printed,
        "\"old 1\" : String\n\"new 2\" : String\n\"new 3\" : String\n"
    );
}

/// `tests/repl/redefine_interface.repl`'s case (H1-1): after the interface
/// is redefined, a new `name` that satisfies it is what a generic function
/// compiled later reaches through the new dictionary.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_repl_redefined_interface() {
    let printed = session_output(&[
        "interface Named {\n  fn name(Self) -> String\n}",
        "type Cat {\n  Cat(Int),\n}",
        "fn name(_c: Cat) -> String {\n  \"cat\"\n}",
        "fn greet<T: Named>(x: T) -> String {\n  let n = Named::name(x)\n  \"hi {n}\"\n}",
        "greet(Cat(1))",
        "interface Named {\n  fn name(Self) -> Int\n}",
        "fn count<T: Named>(x: T) -> Int {\n  Named::name(x)\n}",
        "fn name(_c: Cat) -> Int {\n  9\n}",
        "count(Cat(6))",
    ]);
    assert_eq!(printed, "\"hi cat\" : String\n9 : Int\n");
}

/// `tests/repl/redefine_type.repl`'s case (H1-1): a type redefined, then
/// `show` for the new type, after an interpolation of the old type's
/// value compiled the old type's `Show` function. The entry that
/// interpolates the new value is one batch with the type's module's new
/// items, its `Show` function among them, whose new slot the entry's unit
/// must call, not the older generation's that the session's seed names.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_repl_show_of_a_redefined_type() {
    let printed = session_output(&[
        "type Cat {\n  Cat(Int),\n}",
        "fn show(c: Cat) -> String {\n  let Cat(n) = c\n  \"cat {n}\"\n}",
        "let tom = Cat(1)",
        "\"{tom}\"",
        "type Cat {\n  Cat(String),\n}",
        "let kit = Cat(\"k\")",
        "fn show(c: Cat) -> String {\n  let Cat(name) = c\n  \"cat named {name}\"\n}",
        "\"{kit}\"",
        "show(kit)",
    ]);
    assert_eq!(
        printed,
        "\"cat 1\" : String\n\"cat named k\" : String\n\"cat named k\" : String\n"
    );
}
