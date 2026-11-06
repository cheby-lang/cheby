//! Call locations (D-856) and tail-called waits (D-861): lowering records
//! the location of every call in user code and none in std, the dumps do
//! not show them, the validator checks them, and a tail call from user
//! code to `channel::send`, `channel::receive` or `fiber::join` becomes a
//! call and a return.

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, IndexVec, ModuleId};

use crate::abi::AbiSig;
use crate::builder::FunctionBuilder;
use crate::ir::{
    Abi, Callee, CalleeDecl, DefName, FnKey, Function, MirModule, Operand, Rvalue, StrId,
    Terminator, UnitKind,
};
use crate::repr::Repr;
use crate::test_support::{lower_source, lower_source_with_tests};
use crate::validate::{Stage, validate};

const PROGRAM: &str = "import std::channel
import std::fiber
import std::io

fn wait(rx: channel::Receiver<Int>) -> Result<Int, Nil> {
  channel::receive(rx)
}

fn piped(rx: channel::Receiver<Int>) -> Result<Int, Nil> {
  rx |> channel::receive
}

fn give(tx: channel::Sender<Int>, n: Int) -> Result<Nil, Int> {
  channel::send(tx, n)
}

fn finish(f: fiber::Fiber<Int>) -> Int {
  fiber::join(f)
}

fn show(n: Int) -> Nil {
  io::println(\"{n}\")
}

fn main() {
  let (tx, rx) = channel::new::<Int>(2)
  let assert Ok(Nil) = give(tx, 1)
  let assert Ok(n) = wait(rx)
  let (tx2, rx2) = channel::new::<Int>(1)
  let assert Ok(Nil) = channel::send(tx2, 2)
  let assert Ok(m) = piped(rx2)
  let assert Ok(total) = {
    use owner <- fiber::scope()
    let (tx3, rx3) = channel::new::<Int>(1)
    let assert Ok(Nil) = channel::send(tx3, 3)
    let reader = fiber::spawn(owner, fn() { channel::receive(rx3) })
    let assert Ok(k) = fiber::join(reader)
    finish(fiber::spawn(owner, fn() { n + m + k }))
  }
  show(total)
}
";

/// The function `key` of `unit`.
fn function<'a>(unit: &'a MirModule, key: &str) -> &'a Function {
    unit.functions
        .iter()
        .find(|f| {
            crate::dump_function(unit, f)
                .unwrap()
                .strip_prefix("fn ")
                .is_some_and(|rest| rest.starts_with(&format!("{key}(")))
        })
        .unwrap_or_else(|| panic!("no function `{key}`"))
}

/// The location texts of `f`'s calls, in recording order.
fn locations(f: &Function) -> Vec<&str> {
    f.call_locs
        .iter()
        .map(|&(_, loc)| &**f.call_loc_strings.get(loc).unwrap())
        .collect()
}

#[test]
fn call_locs_name_each_call_of_user_code_and_none_of_std() {
    let lowered = lower_source(&[("", PROGRAM)]);
    let app = lowered.unit("app");
    let main = function(app, "app::main");
    let locs = locations(main);
    // `give(tx, 1)` on line 27 at column 24, `wait(rx)` on line 28, and
    // `channel::send(tx2, 2)` on line 30: the start of each call.
    for expected in [
        "src/main.cheby:27:24",
        "src/main.cheby:28:22",
        "src/main.cheby:30:24",
    ] {
        assert!(locs.contains(&expected), "{expected} not in {locs:?}");
    }
    // `show(total)`, the last expression, is a tail call: no location.
    assert!(
        !locs.iter().any(|l| l.starts_with("src/main.cheby:40:")),
        "{locs:?}"
    );
    // Every recorded local is defined by a call, and each text appears
    // once in the table.
    let mut texts: Vec<&str> = main.call_loc_strings.iter().map(|s| &**s).collect();
    texts.sort_unstable();
    texts.dedup();
    assert_eq!(texts.len(), main.call_loc_strings.len());
    for unit in &lowered.units {
        if unit.path.starts_with("std") {
            for f in &unit.functions {
                assert!(
                    f.call_locs.is_empty() && f.call_loc_strings.is_empty(),
                    "{} records call locations",
                    unit.path
                );
            }
        }
    }
    // The dump shows no location of a call: the strings it prints are
    // the function's own (D-856, plan §0 'Goldens that may change').
    let text = lowered.function("app", "app::main");
    assert!(!text.contains("src/main.cheby:28:22"), "{text}");
}

#[test]
fn call_locs_follow_closures_and_tests() {
    let source = "import std::channel

fn main() {
  let (tx, rx) = channel::new::<Int>(1)
  let assert Ok(Nil) = channel::send(tx, 1)
  let assert Ok(_) = channel::receive(rx)
  Nil
}

test \"a channel gives back what it got\" {
  let (tx, rx) = channel::new::<Int>(1)
  let assert Ok(Nil) = channel::send(tx, 1)
  assert channel::receive(rx) == Ok(1)
}
";
    let lowered = lower_source_with_tests(&[("", source)]);
    let tests = lowered
        .units
        .iter()
        .find(|u| matches!(u.unit, UnitKind::Tests(_)))
        .unwrap();
    let locs: Vec<&str> = tests.functions.iter().flat_map(locations).collect();
    assert!(locs.contains(&"src/main.cheby:12:24"), "{locs:?}");
    assert!(locs.contains(&"src/main.cheby:13:10"), "{locs:?}");
}

/// The body of `f` as dumped: its blocks only.
fn body(text: &str) -> String {
    text.lines()
        .skip_while(|l| !l.starts_with("bb"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn tail_called_waits_keep_their_frame_outside_std() {
    let lowered = lower_source(&[("", PROGRAM)]);
    for key in ["app::wait", "app::piped", "app::give", "app::finish"] {
        let text = body(&lowered.function("app", key));
        assert!(!text.contains("tail_call"), "{key}:\n{text}");
        assert!(text.contains("return _"), "{key}:\n{text}");
        let f = function(lowered.unit("app"), key);
        assert_eq!(f.call_locs.len(), 1, "{key}: {:?}", locations(f));
    }
    // A closure's one-liner keeps its frame too.
    let closures: Vec<String> = lowered
        .unit("app")
        .functions
        .iter()
        .filter(|f| matches!(f.key, FnKey::Closure { .. }))
        .map(|f| body(&crate::dump_function(lowered.unit("app"), f).unwrap()))
        .collect();
    assert!(
        closures
            .iter()
            .any(|c| c.contains("std::channel::receive(") && !c.contains("tail_call")),
        "{closures:#?}"
    );
    // Other tail calls stay tail calls (D-015).
    let show = body(&lowered.function("app", "app::show"));
    assert!(show.contains("tail_call std::io::println"), "{show}");
    let main = body(&lowered.function("app", "app::main"));
    assert!(main.contains("tail_call app::show"), "{main}");
    // In std, `fiber::join` still tail-calls `sys_join` (D-861 is about
    // calls from outside std).
    let join = body(&lowered.function("std::fiber", "std::fiber::join"));
    assert!(join.contains("tail_call std::fiber::sys_join"), "{join}");
}

#[test]
fn tail_called_select_stays_a_tail_call() {
    let source = "import std::channel
import std::selector

fn pick(rx: channel::Receiver<Int>) -> Int {
  selector::new()
  |> selector::receive(_, rx, fn(r) {
    case r {
      Ok(n) => n
      Err(Nil) => 0
    }
  })
  |> selector::select
}

fn main() {
  let (tx, rx) = channel::new::<Int>(1)
  let assert Ok(Nil) = channel::send(tx, 1)
  let _ = pick(rx)
  Nil
}
";
    let lowered = lower_source(&[("", source)]);
    let pick = body(&lowered.function("app", "app::pick"));
    assert!(pick.contains("tail_call std::selector::select"), "{pick}");
}

fn span() -> Span {
    Span::new(FileId::new(0), TextRange::empty(0))
}

const MODULE: ModuleId = ModuleId::from_u32(1);

fn def(n: u32) -> DefId {
    DefId::new(MODULE, DefIndex::from_u32(n))
}

/// `app::f0`, which calls `app::f1` into `_0` and returns it, with
/// `call_locs` set by `locs` to `_0` or `_1` (a `use` of `_0`).
fn located(locs: impl FnOnce(&mut Function, crate::ir::Local, crate::ir::Local)) -> MirModule {
    let mut b = FunctionBuilder::new(FnKey::Item(def(0)), Abi::Cheby, Repr::Value, span()).unwrap();
    b.set_entry_loc("src/main.cheby:1:4").unwrap();
    let called = b
        .assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Direct(def(1)),
                args: vec![Operand::Const(crate::ir::Const::Nil)],
            },
        )
        .unwrap();
    let copied = b
        .assign(Repr::Value, Rvalue::Use(Operand::Local(called)))
        .unwrap();
    b.terminate(Terminator::Return(Operand::Local(copied)))
        .unwrap();
    let mut f = b.finish().unwrap();
    locs(&mut f, called, copied);
    MirModule {
        externals: Vec::new(),
        source: None,
        unit: UnitKind::Code(MODULE),
        path: "app".into(),
        functions: vec![f],
        callees: vec![CalleeDecl {
            def: def(1),
            sig: AbiSig::owned(Abi::Cheby, vec![Repr::Value], Repr::Value),
        }],
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: (0..2)
            .map(|n| DefName {
                def: def(n),
                module: "app".into(),
                item: format!("f{n}").into(),
            })
            .collect(),
    }
}

#[test]
fn the_validator_checks_call_locations() {
    let ok = located(|f, called, _| {
        f.call_loc_strings = IndexVec::from_raw(vec!["src/main.cheby:2:3".into()]).unwrap();
        f.call_locs = vec![(called, StrId::from_u32(0))];
    });
    validate(&ok, Stage::Lowered).unwrap();
    let not_a_call = located(|f, _, copied| {
        f.call_loc_strings = IndexVec::from_raw(vec!["src/main.cheby:2:3".into()]).unwrap();
        f.call_locs = vec![(copied, StrId::from_u32(0))];
    });
    let errors = validate(&not_a_call, Stage::Lowered).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.contains("names a local no call defines")),
        "{errors:?}"
    );
    let unknown = located(|f, called, _| {
        f.call_locs = vec![(called, StrId::from_u32(0))];
    });
    let errors = validate(&unknown, Stage::Lowered).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.contains("unknown call location str#0")),
        "{errors:?}"
    );
    let twice = located(|f, called, _| {
        f.call_loc_strings = IndexVec::from_raw(vec!["src/main.cheby:2:3".into()]).unwrap();
        f.call_locs = vec![(called, StrId::from_u32(0)), (called, StrId::from_u32(0))];
    });
    let errors = validate(&twice, Stage::Lowered).unwrap_err();
    assert!(
        errors.iter().any(|e| e.contains("not sorted by local")),
        "{errors:?}"
    );
    // The dump is the same with and without the table.
    let plain = located(|_, _, _| {});
    assert_eq!(crate::dump(&ok).unwrap(), crate::dump(&plain).unwrap());
}
