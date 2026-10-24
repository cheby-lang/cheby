//! Tests of large literals (D-720, D-780): tuples filled one field at a
//! time (D-810) and literals split into chains of
//! continuations (D-811), from source through
//! `test_support`: the shapes by string checks, two goldens under
//! `tests/golden/data/` (`CHEBY_BLESS=1` rewrites them), the validator on
//! hand-built forms, and the structural guard of the statement budget.

use std::fmt::Write as _;

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, ModuleId};

use crate::builder::FunctionBuilder;
use crate::golden_tests::check_golden;
use crate::ir::{
    Abi, Callee, Const, DefName, FnKey, Function, LayoutRef, MirModule, Operand, Rvalue, Stmt,
    Terminator, UnitKind,
};
use crate::lower::{STATEMENT_BUDGET, TUPLE_INIT_THRESHOLD};
use crate::repr::Repr;
use crate::test_support::{Lowered, lower_source, lower_source_split, lower_source_with_tests};
use crate::validate::{Stage, validate};

/// `app::build(x: Int)` returning `"{t:?}"` of a tuple of `n` fields
/// `x + k`, and a `main` that prints it.
fn tuple_program(n: usize) -> String {
    let mut fields = String::new();
    for k in 0..n {
        let _ = writeln!(fields, "    x + {k},");
    }
    format!(
        "//! A large tuple.\n\nimport std::io\n\nfn build(x: Int) -> String {{\n  let t = (\n{fields}  )\n  \"{{t:?}}\"\n}}\n\nfn main() {{\n  io::println(build(1))\n}}\n"
    )
}

/// The functions of unit `app`, by their dump's first line.
fn headers(lowered: &Lowered) -> Vec<String> {
    let unit = lowered.unit("app");
    unit.functions
        .iter()
        .map(|f| {
            crate::dump_function(unit, f)
                .unwrap()
                .lines()
                .next()
                .unwrap()
                .to_owned()
        })
        .collect()
}

/// The number of statements of `f`, over every block.
fn statements(f: &Function) -> usize {
    f.blocks.iter().map(|b| b.stmts.len()).sum()
}

/// The continuations of unit `app`.
fn continuations(lowered: &Lowered) -> Vec<&Function> {
    lowered
        .unit("app")
        .functions
        .iter()
        .filter(|f| matches!(f.key, FnKey::Continuation { .. }))
        .collect()
}

// Tuples filled one field at a time.

#[test]
fn a_tuple_at_the_threshold_is_one_alloc() {
    let at = lower_source(&[("", &tuple_program(TUPLE_INIT_THRESHOLD))]);
    let build = at.function("app", "app::build");
    assert!(
        build.contains(&format!("alloc tuple/{TUPLE_INIT_THRESHOLD}(")),
        "{build}"
    );
    assert!(!build.contains("init_field"), "{build}");
    assert!(!build.contains("alloc_placeholder"), "{build}");

    let above = TUPLE_INIT_THRESHOLD + 1;
    let lowered = lower_source(&[("", &tuple_program(above))]);
    let build = lowered.function("app", "app::build");
    assert!(
        build.contains(&format!("= alloc_placeholder tuple/{above}")),
        "{build}"
    );
    assert!(!build.contains(&format!("alloc tuple/{above}(")), "{build}");
    for k in 0..above {
        assert!(
            build.contains(&format!(".{k} of tuple/{above} = ")),
            "field {k}: {build}"
        );
    }
    assert_eq!(continuations(&lowered).len(), 0);
}

/// Each field is computed, boxed and stored before the next one is
/// computed: between two stores there is one checked add and one box, and
/// an unwinding out of either releases only the tuple, whose placeholders
/// hold nothing (D-810).
#[test]
fn tuple_fields_are_stored_before_the_next_is_computed() {
    let n = 40;
    let lowered = lower_source(&[("", &tuple_program(n))]);
    let unit = lowered.unit("app");
    let f = build_fn(unit);
    let bb0 = f.blocks.get(crate::ir::BlockId::from_u32(0)).unwrap();
    let mut obj = None;
    let mut since_store = 0;
    let mut stores = 0;
    let mut last_store = 0;
    for (i, stmt) in bb0.stmts.iter().enumerate() {
        match stmt {
            Stmt::Assign {
                dst,
                value: Rvalue::AllocPlaceholder { .. },
            } => obj = Some(*dst),
            Stmt::Eval(Rvalue::InitField { obj: o, field, .. }) => {
                assert_eq!(Some(*o), obj);
                assert_eq!(*field, stores);
                assert!(
                    since_store <= 2,
                    "statement {i}: {since_store} since the last store"
                );
                since_store = 0;
                stores += 1;
                last_store = i;
            }
            Stmt::Assign { .. } if obj.is_some() => since_store += 1,
            _ => {}
        }
    }
    assert_eq!(stores, 40);
    // Every edge of the fields' computations goes to the pad that drops
    // the tuple alone.
    let obj = obj.unwrap();
    let pads: std::collections::BTreeSet<_> = bb0
        .unwind
        .iter()
        .filter(|&&(i, _)| usize::try_from(i).unwrap() < last_store && i > 0)
        .map(|&(_, pad)| pad)
        .collect();
    assert_eq!(pads.len(), 1, "{}", crate::dump_function(unit, f).unwrap());
    let pad = *pads.iter().next().unwrap();
    let pad_block = f.blocks.get(pad).unwrap();
    assert_eq!(pad_block.stmts, vec![Stmt::Drop(obj)]);
}

/// The function `app::build` of `unit`.
fn build_fn(unit: &MirModule) -> &Function {
    unit.functions
        .iter()
        .find(|f| matches!(f.key, FnKey::Item(d) if unit.name(d).unwrap().item.as_ref() == "build"))
        .unwrap()
}

/// A program whose tuple of 100 one-element lists divides by `x - 1` in
/// field 50, which is zero when `x` is 1.
fn panicking_tuple_program() -> String {
    let mut fields = String::new();
    for k in 0..100 {
        if k == 50 {
            fields.push_str("    [x / (x - 1)],\n");
        } else {
            let _ = writeln!(fields, "    [x + {k}],");
        }
    }
    format!(
        "//! A tuple that panics in its middle.\n\nimport std::io\n\nfn build(x: Int) -> String {{\n  let t = (\n{fields}  )\n  \"{{t:?}}\"\n}}\n\nfn main() {{\n  io::println(build(1))\n}}\n"
    )
}

/// A panic in the middle of a 100-field tuple (a division by zero in
/// field 50) unwinds to the pad that releases the partly filled tuple,
/// whose stored fields are lists and whose placeholders hold nothing; a
/// diverging field ends the literal there.
#[test]
fn a_panic_in_the_middle_of_a_tuple_releases_the_partial_tuple() {
    let lowered = lower_source(&[("", &panicking_tuple_program())]);
    let build = lowered.function("app", "app::build");
    let division = build
        .lines()
        .find(|l| l.contains("checked div int"))
        .unwrap_or_else(|| panic!("{build}"));
    let pad = division.rsplit("unwind ").next().unwrap();
    let pad_body: Vec<&str> = build
        .lines()
        .skip_while(|l| *l != format!("{pad}:"))
        .skip(1)
        .take(2)
        .map(str::trim)
        .collect();
    assert_eq!(pad_body, ["drop _1", "goto bb1()"], "{build}");
    assert!(build.contains("init_field _1.99 of tuple/100"), "{build}");

    // A field that diverges ends the literal: nothing after it is stored.
    let src = panicking_tuple_program().replace("[x / (x - 1)]", "panic as \"boom\"");
    let lowered = lower_source(&[("", &src)]);
    let build = lowered.function("app", "app::build");
    assert!(build.contains("init_field _1.49 of tuple/100"), "{build}");
    assert!(!build.contains(".50 of tuple/100"), "{build}");
    assert!(build.contains("panic explicit"), "{build}");
}

// Literals split into continuations (D-780).

/// A list of 24 computed elements cut every few statements: the owner
/// calls continuation 0 with the list and the locals the rest uses; each
/// continuation tail-calls the next with only what is still used; the
/// last returns the list. Numbering follows creation.
#[test]
fn a_list_is_split_into_a_chain_of_continuations() {
    let mut items = String::new();
    for k in 0..24 {
        let local = if k < 6 { "early" } else { "late" };
        let _ = writeln!(items, "    {local} + {k},");
    }
    let src = format!(
        "//! A split list.\n\nimport std::io\nimport std::list\n\nfn build(early: Int, late: Int) -> List<Int> {{\n  [\n{items}  ]\n}}\n\nfn main() {{\n  let n = list::length(build(1, 2))\n  io::println(\"{{n}}\")\n}}\n"
    );
    let lowered = lower_source_split(&[("", &src)], 10, false);
    let heads = headers(&lowered);
    let chain: Vec<&String> = heads
        .iter()
        .filter(|h| h.starts_with("fn continuation app::build#"))
        .collect();
    assert!(chain.len() >= 4, "{heads:#?}");
    for (n, h) in chain.iter().enumerate() {
        assert!(
            h.starts_with(&format!("fn continuation app::build#{n}(")),
            "{heads:#?}"
        );
    }
    // `early` goes along until its last item; `late` to the end.
    assert!(
        chain
            .first()
            .is_some_and(|h| h.contains("(_0: value, _1: int, _2: int)")),
        "{heads:#?}"
    );
    assert!(
        chain
            .iter()
            .skip(2)
            .all(|h| h.contains("(_0: value, _1: int)")),
        "{heads:#?}"
    );
    let build = lowered.function("app", "app::build");
    assert!(
        build.contains("call <continuation app::build#0>("),
        "{build}"
    );
    for n in 0..chain.len() {
        let text = lowered.function("app", &format!("continuation app::build#{n}"));
        if n + 1 < chain.len() {
            let next = format!("tail_call <continuation app::build#{}>(_", n + 1);
            assert!(text.contains(&next), "{text}");
        } else {
            assert!(text.contains("return _"), "{text}");
        }
    }
}

/// Splitting is a pure function of the MIR (D-264): the same source
/// lowers to the same text.
#[test]
fn splitting_is_deterministic() {
    let src = tuple_program(300);
    let a = lower_source_split(&[("", &src)], 40, false).dump();
    let b = lower_source_split(&[("", &src)], 40, false).dump();
    assert_eq!(a, b);
    assert!(a.contains("fn continuation app::build#5("), "{a}");
}

/// A large tuple continues in continuations that take the partly filled
/// tuple, each storing its own fields into its first parameter.
#[test]
fn a_tuple_continues_with_the_partly_filled_tuple() {
    let lowered = lower_source_split(&[("", &tuple_program(60))], 30, false);
    let conts = continuations(&lowered);
    assert!(conts.len() >= 2);
    let unit = lowered.unit("app");
    let mut stored = Vec::new();
    for f in std::iter::once(build_fn(unit)).chain(conts.iter().copied()) {
        for stmt in f.blocks.iter().flat_map(|b| &b.stmts) {
            if let Stmt::Eval(Rvalue::InitField { obj, field, .. }) = stmt {
                if matches!(f.key, FnKey::Continuation { .. }) {
                    assert_eq!(Some(obj), f.params.first());
                }
                stored.push(*field);
            }
        }
    }
    assert_eq!(stored, (0..60).collect::<Vec<u32>>());
}

/// The interpolation of a generic function: a continuation takes the
/// dictionary its `show` calls need (the part's, not the expression's).
#[test]
fn an_interpolation_continuation_takes_the_dictionaries_it_needs() {
    let mut parts = String::new();
    for _ in 0..30 {
        parts.push_str("{a},{b};");
    }
    let src = format!(
        "//! A generic interpolation.\n\nimport std::io\n\nfn both<T: Show>(a: T, b: Int) -> String {{\n  \"{parts}\"\n}}\n\nfn main() {{\n  io::println(both(\"x\", 2))\n}}\n"
    );
    let lowered = lower_source_split(&[("", &src)], 12, false);
    let heads = headers(&lowered);
    let conts: Vec<&String> = heads
        .iter()
        .filter(|h| h.starts_with("fn continuation app::both#"))
        .collect();
    assert!(conts.len() >= 2, "{heads:#?}");
    // The buffer, `a`, `b`, then the dictionary of `T: Show`; the last
    // may need less.
    for h in conts.iter().take(conts.len().saturating_sub(1)) {
        assert!(
            h.contains("(_0: value, _1: value, _2: int, _3: value)"),
            "{heads:#?}"
        );
    }
}

/// A closure in a continuation's items is a closure of the owner, and
/// captures what the continuation received; a nested literal splits on
/// its own, its continuations numbered after the ones made before it.
#[test]
fn closures_and_nested_literals_in_continuations() {
    let mut items = String::new();
    for k in 0..12 {
        let _ = writeln!(items, "    fn(a) {{ a + y + {k} }},");
    }
    let mut inner = String::new();
    for k in 0..12 {
        let _ = write!(inner, "y + {k}, ");
    }
    let src = format!(
        "//! Closures and nested lists.\n\nimport std::io\nimport std::list\n\nfn fns(y: Int) -> List<fn(Int) -> Int> {{\n  [\n{items}  ]\n}}\n\nfn nested(y: Int) -> List<List<Int>> {{\n  [[{inner}], [{inner}], [{inner}]]\n}}\n\nfn main() {{\n  let a = list::length(fns(1))\n  let b = list::length(nested(2))\n  io::println(\"{{a}} {{b}}\")\n}}\n"
    );
    let lowered = lower_source_split(&[("", &src)], 8, false);
    let heads = headers(&lowered);
    assert!(
        heads
            .iter()
            .any(|h| h.starts_with("fn continuation app::fns#0(")),
        "{heads:#?}"
    );
    assert!(
        heads
            .iter()
            .any(|h| h.starts_with("fn closure app::fns#11(")),
        "{heads:#?}"
    );
    let nested: Vec<&String> = heads
        .iter()
        .filter(|h| h.starts_with("fn continuation app::nested#"))
        .collect();
    assert!(nested.len() >= 4, "{heads:#?}");
}

/// An item that diverges ends the chain: the continuation that holds it
/// ends in the panic, and nothing after it is lowered.
#[test]
fn a_diverging_item_ends_the_chain() {
    let mut items = String::new();
    for k in 0..30 {
        if k == 20 {
            items.push_str("    panic as \"boom\",\n");
        } else {
            let _ = writeln!(items, "    x + {k},");
        }
    }
    let src = format!(
        "//! A list that panics.\n\nimport std::io\nimport std::list\n\nfn build(x: Int) -> List<Int> {{\n  [\n{items}  ]\n}}\n\nfn main() {{\n  let n = list::length(build(1))\n  io::println(\"{{n}}\")\n}}\n"
    );
    let lowered = lower_source_split(&[("", &src)], 10, false);
    let conts = continuations(&lowered);
    let last = conts.last().unwrap();
    let text = crate::dump_function(lowered.unit("app"), last).unwrap();
    assert!(text.contains("panic explicit"), "{text}");
    assert!(!text.contains("return"), "{text}");
    assert!(!text.contains("const int 29"), "{text}");
}

/// A parameter used by the items after a cut is passed to the
/// continuation, an owned use: it is never borrowed there, and counting
/// keeps the owner balanced (D-715).
#[test]
fn a_parameter_passed_to_a_continuation_is_owned() {
    let mut items = String::new();
    for k in 0..20 {
        let _ = writeln!(items, "    \"{{name}}{k}\",");
    }
    let src = format!(
        "//! Strings of a parameter.\n\nimport std::io\nimport std::list\n\nfn names(name: String) -> List<String> {{\n  [\n{items}  ]\n}}\n\nfn main() {{\n  let n = \"x\"\n  let count = list::length(names(n))\n  io::println(\"{{count}} {{n}}\")\n}}\n"
    );
    let lowered = lower_source_split(&[("", &src)], 10, false);
    assert!(
        !lowered.masks.contains_key("app::names"),
        "{:?}",
        lowered.masks
    );
    let conts = continuations(&lowered);
    assert_ne!(conts.len(), 0);
    for f in conts {
        assert!(f.borrow.is_owned());
    }
}

/// A test's literal continues in continuations of the test, and a
/// constant's in continuations of the constant.
#[test]
fn tests_and_constants_have_continuations() {
    let mut items = String::new();
    for k in 0..20 {
        let _ = write!(items, "{k} + 1, ");
    }
    let src = format!(
        "//! Literals in a test and a constant.\n\nimport std::io\nimport std::list\n\nconst xs: List<Int> = [{items}]\n\nfn main() {{\n  let n = list::length(xs)\n  io::println(\"{{n}}\")\n}}\n\ntest \"many\" {{\n  let ys = [{items}]\n  assert list::length(ys) == 20\n}}\n"
    );
    let lowered = lower_source_split(&[("", &src)], 6, true);
    let text = lowered.dump();
    assert!(text.contains("fn continuation app::xs#0("), "{text}");
    assert!(text.contains("fn continuation app::many#0("), "{text}");
}

/// The structural guard of D-780's statement budget: a 20k-element
/// literal of each kind (a list, an interpolation, a map from a list of
/// pairs and a tuple) leaves no function of its unit above
/// [`STATEMENT_BUDGET`] statements after counting and unwind edges.
#[test]
fn twenty_thousand_elements_stay_within_the_statement_budget() {
    let n = 20_000;
    let mut list = String::new();
    let mut interp = String::new();
    let mut pairs = String::new();
    let mut tuple = String::new();
    for k in 0..n {
        let _ = writeln!(list, "    x + {k},");
        let _ = write!(interp, "{{v{}}},", k % 10);
        let _ = writeln!(pairs, "    (x + {k}, x + {}),", k % 997);
        let _ = writeln!(tuple, "    x + {k},");
    }
    let locals: String = (0..10).fold(String::new(), |mut s, k| {
        let _ = writeln!(s, "  let v{k} = x + {k}");
        s
    });
    let src = format!(
        "//! Huge literals.\n\nimport std::io\nimport std::list\nimport std::map\nimport std::string\n\nfn a_list(x: Int) -> List<Int> {{\n  [\n{list}  ]\n}}\n\nfn a_string(x: Int) -> String {{\n{locals}  \"{interp}\"\n}}\n\nfn a_map(x: Int) -> map::Map<Int, Int> {{\n  map::from_list([\n{pairs}  ])\n}}\n\nfn a_tuple(x: Int) -> String {{\n  let t = (\n{tuple}  )\n  \"{{t:?}}\"\n}}\n\nfn main() {{\n  let a = list::length(a_list(1))\n  let b = string::length(a_string(1))\n  let c = map::size(a_map(1))\n  let d = string::length(a_tuple(1))\n  io::println(\"{{a}} {{b}} {{c}} {{d}}\")\n}}\n"
    );
    let lowered = lower_source(&[("", &src)]);
    let unit = lowered.unit("app");
    let mut per_owner = std::collections::BTreeMap::new();
    for f in &unit.functions {
        let size = statements(f);
        assert!(
            size <= STATEMENT_BUDGET,
            "{:?} has {size} statements",
            f.key
        );
        if let FnKey::Continuation { owner, .. } = f.key {
            *per_owner
                .entry(unit.name(owner).unwrap().item.to_string())
                .or_insert(0) += 1;
        }
    }
    // Every kind was split, into about one continuation per cut.
    for owner in ["a_list", "a_string", "a_map", "a_tuple"] {
        let count = per_owner.get(owner).copied().unwrap_or(0);
        assert!(count >= 20, "{owner}: {count} continuations: {per_owner:?}");
    }
}

/// The goldens of both forms (read by eye): a 34-field tuple filled one
/// field at a time, and a list and an interpolation split into
/// continuations at a small size.
#[test]
fn large_literal_goldens() {
    let lowered = lower_source(&[("", &tuple_program(TUPLE_INIT_THRESHOLD + 2))]);
    check_golden(
        "data/tuple_by_field",
        &lowered.function("app", "app::build"),
    );
    let src = r#"//! Split literals.

import std::io
import std::list

fn numbers(x: Int, y: Int) -> List<Int> {
  [x + 1, x + 2, x + 3, y + 4, y + 5, y + 6, y + 7]
}

fn text(name: String, n: Int) -> String {
  "{name}: {n}, {n}, {name}, {n}"
}

fn main() {
  let n = list::length(numbers(1, 2))
  io::println(text("n", n))
}
"#;
    let lowered = lower_source_split(&[("", src)], 6, false);
    check_golden(
        "data/split_literals",
        &crate::dump(lowered.unit("app")).unwrap(),
    );
}

/// `cheby test`'s units split as `cheby run`'s do.
#[test]
fn test_units_lower_with_the_default_size() {
    let src = "//! A test with a small literal.\n\nimport std::io\nimport std::list\n\nfn main() {\n  io::println(\"x\")\n}\n\ntest \"small\" {\n  assert list::length([1, 2, 3]) == 3\n}\n";
    let lowered = lower_source_with_tests(&[("", src)]);
    assert!(!lowered.dump().contains("continuation"));
}

// The validator on hand-built forms.

const MODULE: ModuleId = ModuleId::from_u32(1);

fn def(n: u32) -> DefId {
    DefId::new(MODULE, DefIndex::from_u32(n))
}

fn span() -> Span {
    Span::new(FileId::new(0), TextRange::empty(0))
}

fn builder(key: FnKey) -> FunctionBuilder {
    let mut b = FunctionBuilder::new(key, Abi::Cheby, Repr::Value, span()).unwrap();
    b.set_entry_loc("src/main.cheby:1:4").unwrap();
    b
}

fn unit(functions: Vec<Function>) -> MirModule {
    MirModule {
        externals: Vec::new(),
        source: None,
        unit: UnitKind::Code(MODULE),
        path: "app".into(),
        functions,
        callees: Vec::new(),
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: vec![DefName {
            def: def(0),
            module: "app".into(),
            item: "f".into(),
        }],
    }
}

const T3: LayoutRef = LayoutRef::Tuple(3);

fn init(obj: crate::ir::Local, layout: LayoutRef, field: u32) -> Stmt {
    Stmt::Eval(Rvalue::InitField {
        obj,
        layout,
        field,
        value: Operand::Const(Const::Nil),
    })
}

/// `f() { t = alloc_placeholder tuple/3; <stores>; return t }`.
fn filling(stores: impl FnOnce(crate::ir::Local) -> Vec<Stmt>) -> Function {
    let mut b = builder(FnKey::Item(def(0)));
    let t = b
        .assign(Repr::Value, Rvalue::AllocPlaceholder { layout: T3 })
        .unwrap();
    for s in stores(t) {
        b.push(s).unwrap();
    }
    b.terminate(Terminator::Return(Operand::Local(t))).unwrap();
    b.finish().unwrap()
}

fn errors(functions: Vec<Function>, stage: Stage) -> String {
    match validate(&unit(functions), stage) {
        Ok(()) => String::new(),
        Err(e) => e.join("\n"),
    }
}

#[test]
fn the_validator_checks_init_field() {
    let all = |t| (0..3).map(|k| init(t, T3, k)).collect();
    assert_eq!(errors(vec![filling(all)], Stage::Lowered), "");
    // A field out of range, another layout, a field stored twice.
    let e = errors(vec![filling(|t| vec![init(t, T3, 3)])], Stage::Lowered);
    assert!(e.contains("init_field 3 of Tuple(3)"), "{e}");
    let e = errors(
        vec![filling(|t| vec![init(t, LayoutRef::Tuple(4), 0)])],
        Stage::Lowered,
    );
    assert!(e.contains("not a fresh"), "{e}");
    let e = errors(
        vec![filling(|t| vec![init(t, T3, 1), init(t, T3, 1)])],
        Stage::Lowered,
    );
    assert!(e.contains("a second time"), "{e}");
    // An object that is not fresh: a parameter of an item.
    let mut b = builder(FnKey::Item(def(0)));
    let p = b.add_param(Repr::Value, None, None).unwrap();
    b.push(init(p, T3, 0)).unwrap();
    b.terminate(Terminator::Return(Operand::Local(p))).unwrap();
    let e = errors(vec![b.finish().unwrap()], Stage::Lowered);
    assert!(e.contains("not a fresh"), "{e}");
    // Only for effect.
    let mut b = builder(FnKey::Item(def(0)));
    let t = b
        .assign(Repr::Value, Rvalue::AllocPlaceholder { layout: T3 })
        .unwrap();
    b.assign(
        Repr::Value,
        Rvalue::InitField {
            obj: t,
            layout: T3,
            field: 0,
            value: Operand::Const(Const::Nil),
        },
    )
    .unwrap();
    b.terminate(Terminator::Return(Operand::Local(t))).unwrap();
    let e = errors(vec![b.finish().unwrap()], Stage::Lowered);
    assert!(e.contains("a void call cannot define a local"), "{e}");
    // A placeholder tuple only.
    let mut b = builder(FnKey::Item(def(0)));
    let t = b
        .assign(
            Repr::Value,
            Rvalue::AllocPlaceholder {
                layout: LayoutRef::Closure(1),
            },
        )
        .unwrap();
    b.terminate(Terminator::Return(Operand::Local(t))).unwrap();
    let e = errors(vec![b.finish().unwrap()], Stage::Lowered);
    assert!(e.contains("not a tuple"), "{e}");
}

/// A continuation's first parameter is the partly filled tuple; a
/// tuple being filled is never shared.
#[test]
fn the_validator_checks_continuations_and_sharing() {
    let key = FnKey::Continuation {
        owner: def(0),
        index: 0,
    };
    let mut rest = builder(key);
    let tuple = rest.add_param(Repr::Value, None, None).unwrap();
    rest.push(init(tuple, T3, 2)).unwrap();
    rest.terminate(Terminator::Return(Operand::Local(tuple)))
        .unwrap();
    let cont = rest.finish().unwrap();
    let mut owner = builder(FnKey::Item(def(0)));
    let made = owner
        .assign(Repr::Value, Rvalue::AllocPlaceholder { layout: T3 })
        .unwrap();
    owner.push(init(made, T3, 0)).unwrap();
    owner.push(init(made, T3, 1)).unwrap();
    let result = owner
        .assign(
            Repr::Value,
            Rvalue::Call {
                callee: Callee::Code(key),
                args: vec![Operand::Local(made)],
            },
        )
        .unwrap();
    owner
        .terminate(Terminator::Return(Operand::Local(result)))
        .unwrap();
    let owner = owner.finish().unwrap();
    assert_eq!(
        errors(vec![owner.clone(), cont.clone()], Stage::Lowered),
        ""
    );
    let mut counted = unit(vec![owner, cont]);
    crate::count_and_check(&mut counted).unwrap();
    // A continuation without the value first.
    let mut bad = builder(key);
    let number = bad.add_param(Repr::Int, None, None).unwrap();
    let boxed = bad
        .assign(
            Repr::Value,
            Rvalue::Box {
                value: Operand::Local(number),
                from: Repr::Int,
            },
        )
        .unwrap();
    bad.terminate(Terminator::Return(Operand::Local(boxed)))
        .unwrap();
    let found = errors(vec![bad.finish().unwrap()], Stage::Lowered);
    assert!(
        found.contains("a continuation takes the value being built first"),
        "{found}"
    );
    // A shared tuple being filled.
    let shared = filling(|t| vec![Stmt::Dup(t), init(t, T3, 0), Stmt::Drop(t)]);
    let e = errors(vec![shared], Stage::Counted);
    assert!(e.contains("a tuple being filled"), "{e}");
}
