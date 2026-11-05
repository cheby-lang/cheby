//! The REPL on code records (K4c, D-1225): every entry's
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

/// §8.6.1's example, D-1312: two entries convert one value, each
/// in its own unit with its own copy of the descriptor, and the values are
/// equal.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_repl_dyn_values_of_two_entries_are_equal() {
    let printed = session_output(&[
        "interface Any",
        "type P {\n  x: Int,\n}",
        "let a: dyn Any = P { x: 1 }",
        "let b: dyn Any = P { x: 1 }",
        "a == b",
    ]);
    assert_eq!(printed, "True : Bool\n");
}

/// §8.6.1, §13.5.2, D-1313: a redefined type is another type,
/// even with the shape it had two generations ago, and within a generation
/// values from two entries are equal. Its printed name and module path are
/// its entry's, so this pins that each entry's module path stays its own.
/// The empty lists read the descriptors alone: a value's constructor would
/// tell the generations apart by itself (D-559).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_repl_redefined_type_stays_another_type() {
    let printed = session_output(&[
        "interface Any",
        "type Box<T> {\n  value: T,\n}",
        "type P {\n  x: Int,\n}",
        "let a: dyn Any = P { x: 1 }",
        "let a2: dyn Any = P { x: 1 }",
        "a == a2",
        "let boxed: dyn Any = Box { value: P { x: 1 } }",
        "let none: List<P> = []",
        "let n: dyn Any = none",
        "type P {\n  x: Int,\n  y: Int,\n}",
        "let b: dyn Any = P { x: 1, y: 2 }",
        "a == b",
        "type P {\n  x: Int,\n}",
        "let c: dyn Any = P { x: 1 }",
        "a == c",
        "let d: dyn Any = P { x: 1 }",
        "c == d",
        "let boxed2: dyn Any = Box { value: P { x: 1 } }",
        "boxed == boxed2",
        "let none2: List<P> = []",
        "let n2: dyn Any = none2",
        "n == n2",
        "let n3: dyn Any = none2",
        "n2 == n3",
    ]);
    assert_eq!(
        printed,
        "True : Bool\nFalse : Bool\nFalse : Bool\nTrue : Bool\nFalse : Bool\nFalse : Bool\nTrue : Bool\n"
    );
}

/// §8.6.1, §13.5.2, D-1313: a definition checked again, in the
/// module `<repl:N:M>`, keeps naming the descriptor of the type it named,
/// whose module path is the type's entry's, not its own.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_repl_definition_checked_again_keeps_its_type() {
    let printed = session_output(&[
        "interface Any",
        "type P {\n  x: Int,\n}",
        "fn k() -> Int {\n  1\n}",
        "fn mk() -> dyn Any {\n  let _v = k()\n  P { x: 1 }\n}",
        "let a = mk()",
        "fn k() -> String {\n  \"s\"\n}",
        "let b = mk()",
        "a == b",
        "let c: dyn Any = P { x: 1 }",
        "b == c",
        "type P {\n  x: Int,\n  y: Int,\n}",
        "let d = mk()",
        "a == d",
        "let e: dyn Any = P { x: 1, y: 2 }",
        "d == e",
    ]);
    assert_eq!(
        printed,
        "True : Bool\nTrue : Bool\nTrue : Bool\nFalse : Bool\n"
    );
}

/// §8.6.1, D-547, D-1313: a redefined interface is another
/// interface, so `List<dyn I>` of each generation is another type.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_repl_redefined_interface_stays_another_dyn_type() {
    let printed = session_output(&[
        "interface Any",
        "interface I",
        "let e: List<dyn I> = []",
        "let a: dyn Any = e",
        "let a2: dyn Any = e",
        "a == a2",
        "interface I",
        "let f: List<dyn I> = []",
        "let b: dyn Any = f",
        "a == b",
        "let b2: dyn Any = f",
        "b == b2",
    ]);
    assert_eq!(printed, "True : Bool\nFalse : Bool\nTrue : Bool\n");
}

/// §8.6.1, D-625, D-1313: `:reload` makes a new version of an
/// edited module, whose types are other types even when their text did not
/// change: the new version's module path carries the reload's generation.
/// A type of a module the reload left alone stays one type. A session type
/// that holds a reloaded type is checked again, in `<repl:N:M>`, so it is
/// another type too.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_record_repl_reloaded_type_stays_another_type() {
    const PRICES: &str = "//! Edited.\n\nimport rel::stay\n\n/// Not edited.\ntype T {\n  x: Int,\n}\n\n/// Boxed.\nfn make(x: Int) -> dyn stay::Any {\n  let b: dyn stay::Any = T { x }\n  b\n}\n\n/// An empty list: only the descriptor differs.\nfn none() -> dyn stay::Any {\n  let xs: List<T> = []\n  let b: dyn stay::Any = xs\n  b\n}\n";
    const STAY: &str = "//! Not edited.\n\n/// Everything.\ninterface Any\n\n/// Kept.\ntype K {\n  x: Int,\n}\n\n/// Boxed.\nfn make(x: Int) -> dyn Any {\n  let b: dyn Any = K { x }\n  b\n}\n";
    let dir = std::env::temp_dir().join(format!("cheby-repl-reload-{}", std::process::id()));
    let root = dir.join("rel");
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        root.join("cheby.toml"),
        "name = \"rel\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(src.join("prices.cheby"), PRICES).unwrap();
    std::fs::write(src.join("stay.cheby"), STAY).unwrap();
    std::fs::write(src.join("main.cheby"), "//! Root.\n\nfn main() {\n}\n").unwrap();
    let printed = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            let options = SessionOptions {
                threads: Some(2),
                ..SessionOptions::default()
            };
            let Start::Ready(mut session, _) = Session::start(&root, options).unwrap() else {
                panic!("the session did not start");
            };
            let mut out = Printed::default();
            for entry in [
                "import rel::prices",
                "import rel::stay",
                "let a = prices::make(1)",
                "a == prices::make(1)",
                "let k = stay::make(1)",
                "let n = prices::none()",
                "type W {\n  t: prices::T,\n}",
                "let ws: List<W> = []",
                "let wn: dyn stay::Any = ws",
            ] {
                session.eval(entry, &mut out).unwrap();
            }
            let edited = format!("{PRICES}\n/// Added.\nfn added() -> Int {{\n  1\n}}\n");
            std::fs::write(src.join("prices.cheby"), edited).unwrap();
            session.reload(&mut out).unwrap();
            for entry in [
                "let c = prices::make(1)",
                "c == a",
                "c == prices::make(1)",
                "k == stay::make(1)",
                "n == prices::none()",
                "prices::none() == prices::none()",
                "let ws2: List<W> = []",
                "let wn2: dyn stay::Any = ws2",
                "wn == wn2",
                "let wn3: dyn stay::Any = ws2",
                "wn2 == wn3",
            ] {
                session.eval(entry, &mut out).unwrap();
            }
            out.0
        })
        .unwrap()
        .join()
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(
        printed,
        "True : Bool\nreloaded `rel::prices`\nFalse : Bool\nTrue : Bool\nTrue : Bool\nFalse : Bool\nTrue : Bool\nFalse : Bool\nTrue : Bool\n"
    );
}
