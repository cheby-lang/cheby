//! Loading code records (K4c, D-914, D-918, D-922, D-1169): records
//! compiled alone load side by side, copies of one shim included; a record
//! from the store loads as a fresh one does; a record whose import names a
//! symbol nothing defines fails before anything is declared.

use super::*;

/// `b::get`.
fn get() -> DefId {
    def(2, 0)
}

/// `b::Empty`, `b::T`'s only constructor, nullary.
fn empty() -> DefId {
    def(2, 1)
}

/// `b::T`.
fn t_type() -> DefId {
    def(2, 2)
}

/// `a::run`.
fn run() -> DefId {
    def(1, 0)
}

/// The constant whose thunk calls `a::run`.
fn target() -> DefId {
    def(1, 10_000)
}

fn get_sig() -> AbiSig {
    cheby_sig(&[])
}

/// `b::get`'s value shim, which calls `b::get` through its slot: a copy
/// in every unit that uses `b::get` as a value (D-918).
fn get_shim() -> Function {
    function(
        FnKey::ValueShim(get()),
        Abi::Cheby,
        &[Repr::Value],
        &[Repr::Value],
        Repr::Value,
        vec![block(
            vec![assign(
                1,
                Rvalue::Call {
                    callee: Callee::Direct(get()),
                    args: Vec::new(),
                },
            )],
            ret(1),
        )],
        &[],
    )
}

/// The dictionary both units hold, `cb$dict$b$$b::Show$b::T` (D-579).
fn dictionary() -> StaticObject {
    StaticObject {
        id: StaticId::from_u32(0),
        kind: StaticKind::Dict,
        module: "b".into(),
        name: "b::Show$b::T".into(),
        layout: LayoutRef::Dict {
            fns: 1,
            embedded: 0,
        },
        fields: vec![
            Const::Code(FnKey::ValueShim(get())),
            Const::StaticCtor(empty()),
        ]
        .into_boxed_slice(),
        strings: IndexVec::new(),
    }
}

fn empty_layout() -> LayoutDesc {
    LayoutDesc {
        layout: LayoutRef::Ctor(empty()),
        name: "b::Empty".into(),
        tag: 0,
        only_ctor: true,
        fields: Vec::new(),
        value_fields: 0,
        raw_fields: 0,
        type_def: t_type(),
        type_name: "T".into(),
        ctor_name: "Empty".into(),
        record: false,
        hooks: None,
        type_key: cheby_runtime::layout::type_key("b", "T"),
    }
}

fn names_of_b() -> Vec<DefName> {
    vec![
        name(get(), "b", "get"),
        name(empty(), "b", "Empty"),
        name(t_type(), "b", "T"),
    ]
}

/// Unit `b`: `get()` uses itself as a value and the dictionary, and
/// returns `Empty`'s static.
fn unit_b() -> MirModule {
    let body = function(
        FnKey::Item(get()),
        Abi::Cheby,
        &[],
        &[Repr::Value, Repr::Value],
        Repr::Value,
        vec![block(
            vec![
                assign(0, Rvalue::Use(Operand::Const(Const::FnValue(get())))),
                assign(
                    1,
                    Rvalue::Use(Operand::Const(Const::Static(StaticId::from_u32(0)))),
                ),
            ],
            Terminator::Return(Operand::Const(Const::StaticCtor(empty()))),
        )],
        &[],
    );
    let mut unit = code_unit(
        2,
        "b",
        vec![body, get_shim()],
        vec![CalleeDecl {
            def: get(),
            sig: get_sig(),
        }],
        names_of_b(),
    );
    unit.layouts = vec![empty_layout()];
    unit.statics = vec![dictionary()];
    unit
}

/// Unit `a`: `run()` calls `b::get` through its slot and holds its own
/// copies of `b::get`'s shim, function value and dictionary.
fn unit_a() -> MirModule {
    let body = function(
        FnKey::Item(run()),
        Abi::Cheby,
        &[],
        &[Repr::Value, Repr::Value, Repr::Value],
        Repr::Value,
        vec![block(
            vec![
                assign(
                    0,
                    Rvalue::Call {
                        callee: Callee::Direct(get()),
                        args: Vec::new(),
                    },
                ),
                assign(1, Rvalue::Use(Operand::Const(Const::FnValue(get())))),
                assign(
                    2,
                    Rvalue::Use(Operand::Const(Const::Static(StaticId::from_u32(0)))),
                ),
            ],
            ret(0),
        )],
        &[],
    );
    let mut names = names_of_b();
    names.push(name(run(), "a", "run"));
    let mut unit = code_unit(
        1,
        "a",
        vec![body, get_shim()],
        vec![
            CalleeDecl {
                def: run(),
                sig: cheby_sig(&[]),
            },
            CalleeDecl {
                def: get(),
                sig: get_sig(),
            },
        ],
        names,
    );
    unit.foreign_layouts = vec![empty_layout()];
    unit.statics = vec![dictionary()];
    unit
}

/// The entries unit: a constant thunk that calls `a::run`.
fn unit_entries() -> MirModule {
    entries_unit(
        vec![const_thunk(target(), run(), Vec::new())],
        vec![CalleeDecl {
            def: run(),
            sig: cheby_sig(&[]),
        }],
        vec![name(run(), "a", "run"), name(target(), "a", "VALUE")],
    )
}

/// Whether `record` defines `symbol` locally.
fn defines_locally(record: &CodeRecord, symbol: &str) -> bool {
    record.symbol(symbol).is_some_and(|s| {
        s.linkage == cheby_codegen_clif::record::SymbolLinkage::Local && record.defines(symbol)
    })
}

/// Two records that each hold a copy of `b::get`'s shim, its function
/// value and the dictionary load side by side (D-918), fresh and again
/// from their bytes in a second JIT, as a warm run loads them; `b::Empty`'s
/// layout is registered from `b`'s module record (D-1169), so its static
/// has a user layout id when `a::run` returns it.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn record_load_copies_of_one_shim_side_by_side() {
    let (b, a, entries) = (unit_b(), unit_a(), unit_entries());
    let mut jit = Jit::new().unwrap();
    let records = jit
        .compile_units(
            &[
                BatchUnit::Mir(&b),
                BatchUnit::Mir(&a),
                BatchUnit::Mir(&entries),
            ],
            &BatchCx::default(),
        )
        .unwrap();
    let [Some(b_record), Some(a_record), None] = records.as_slice() else {
        panic!("not one record per code unit: {records:?}");
    };
    for record in [b_record, a_record] {
        assert!(defines_locally(record, "cb$shim$b$get"), "{record:?}");
        assert!(
            defines_locally(record, "cb$dict$b$$b::Show$b::T"),
            "{record:?}"
        );
    }
    let fresh = returned(run_const(&jit, target()));
    assert_eq!(rc_of(fresh), abi::RC_STATIC);
    assert!(builtin_layout_id(&LayoutRef::Ctor(empty())).is_none());
    assert!(
        layout_of(fresh) >= abi::layout::FIRST_USER,
        "{}",
        layout_of(fresh)
    );

    // The records' bytes, as the store keeps them, loaded with a fresh
    // entries unit into a JIT of their own.
    let b_bytes = b_record.encode();
    let a_bytes = a_record.encode();
    let b_back = CodeRecord::decode(&b_bytes).unwrap();
    let a_back = CodeRecord::decode(&a_bytes).unwrap();
    let mut warm = Jit::new().unwrap();
    let again = warm
        .compile_units(
            &[
                BatchUnit::Record {
                    record: &b_back,
                    unit: b.unit,
                    path: "b",
                },
                BatchUnit::Record {
                    record: &a_back,
                    unit: a.unit,
                    path: "a",
                },
                BatchUnit::Mir(&entries),
            ],
            &BatchCx::default(),
        )
        .unwrap();
    assert!(again.iter().all(Option::is_none), "{again:?}");
    let word = returned(run_const(&warm, target()));
    assert_eq!(rc_of(word), abi::RC_STATIC);
    assert!(
        layout_of(word) >= abi::layout::FIRST_USER,
        "{}",
        layout_of(word)
    );
    assert_eq!(warm.module_records().len(), 2);
}

/// `app::ident(x) = x` and `app::caller(x)`, which calls `ident` at two
/// located sites; a thunk calls `caller`.
fn located_program() -> (Prog, DefId) {
    let mut p = Prog::default();
    p.item(
        0,
        "ident",
        &[Repr::Value],
        &[],
        vec![block(Vec::new(), ret(0))],
        &[],
    );
    let call = |arg: u32| Rvalue::Call {
        callee: Callee::Direct(def(1, 0)),
        args: vec![op(arg)],
    };
    p.item(
        1,
        "caller",
        &[Repr::Value],
        &[Repr::Value, Repr::Value],
        vec![block(vec![assign(1, call(0)), assign(2, call(1))], ret(2))],
        &[],
    );
    let caller = p
        .functions
        .iter_mut()
        .find(|f| f.key == FnKey::Item(def(1, 1)))
        .unwrap();
    for (l, text) in [(1, "src/app.cheby:3:5"), (2, "src/app.cheby:4:7")] {
        let id = caller.call_loc_strings.push(text.into()).unwrap();
        caller.call_locs.push((local(l), id));
    }
    let target = p.call(def(1, 1), vec![Operand::Const(Const::Nil)]);
    (p, target)
}

/// What a registered range says, its address aside: its display name,
/// length, ABI and call sites (offsets and locations).
type RangeFacts = (Option<String>, u32, bool, Vec<(u32, String)>);

/// The ranges `jit` registered, each read back through
/// `cheby_runtime::locate::range_at` at its first byte, as a backtrace or a
/// deadlock report finds them, without addresses, sorted.
fn located_ranges(jit: &Jit) -> Vec<RangeFacts> {
    let mut out: Vec<RangeFacts> = cheby_runtime::locate::owner_ranges(jit.keep_alive.code_owner)
        .iter()
        .map(|range| {
            let (owner, found) = cheby_runtime::locate::range_at(range.start).unwrap();
            assert_eq!(owner, jit.keep_alive.code_owner);
            assert_eq!(found.start, range.start);
            (
                found.name.map(|n| n.as_str().to_owned()),
                found.len,
                found.cheby_abi,
                found
                    .sites
                    .iter()
                    .map(|s| (s.ret_offset, s.location.clone()))
                    .collect(),
            )
        })
        .collect();
    out.sort();
    out
}

/// D-1041, plan §2.1's round trip: a record loaded from its bytes, as a
/// warm run loads it, registers every function with the display name, the
/// length and the call sites that the fresh compile registered, found
/// through `range_at`; and its code still runs.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn record_load_keeps_names() {
    let (p, target) = located_program();
    let [code, entries] = p.units();
    let mut fresh = Jit::new().unwrap();
    let records = fresh
        .compile_units(
            &[BatchUnit::Mir(&code), BatchUnit::Mir(&entries)],
            &BatchCx::default(),
        )
        .unwrap();
    let Some(record) = records.first().cloned().flatten() else {
        panic!("no record: {records:?}");
    };
    let cold = located_ranges(&fresh);
    let names: Vec<Option<&str>> = cold.iter().map(|r| r.0.as_deref()).collect();
    assert!(names.contains(&Some("app::caller")), "{cold:#?}");
    assert!(names.contains(&Some("app::ident")), "{cold:#?}");
    let caller = cold
        .iter()
        .find(|r| r.0.as_deref() == Some("app::caller"))
        .unwrap();
    let locations: Vec<&str> = caller.3.iter().map(|(_, l)| l.as_str()).collect();
    assert_eq!(locations, ["src/app.cheby:3:5", "src/app.cheby:4:7"]);

    let back = CodeRecord::decode(&record.encode()).unwrap();
    let mut warm = Jit::new().unwrap();
    warm.compile_units(
        &[
            BatchUnit::Record {
                record: &back,
                unit: code.unit,
                path: "app",
            },
            BatchUnit::Mir(&entries),
        ],
        &BatchCx::default(),
    )
    .unwrap();
    assert_eq!(located_ranges(&warm), cold);
    assert_eq!(returned(run_const(&warm, target)), static_nil());
}

/// A code unit whose wrapper calls `cheby_no_such_symbol_anywhere`.
fn missing_batch() -> [MirModule; 2] {
    labs_batch(7, labs_wrapper(7, &[cheby_mir::CType::I64], &[-3], true))
}

/// A record whose import word names a symbol that neither the runtime,
/// the package libraries nor the process defines fails its batch with
/// `UnresolvedSymbols`, which the driver reports as E0053 (D-943), before
/// anything of it is declared; with stubs it loads, bound to
/// `cheby_rt_unresolved`. The record from the store fails the same way.
#[cfg(unix)]
#[test]
fn record_load_missing_import_is_unresolved() {
    let [code, entries] = missing_batch();
    let no_stubs = JitOptions {
        stub_unresolved: false,
        ..JitOptions::from_env()
    };
    let mut jit = Jit::configured(CodegenOptions::default(), no_stubs).unwrap();
    let err = jit
        .compile_units(
            &[BatchUnit::Mir(&code), BatchUnit::Mir(&entries)],
            &BatchCx::default(),
        )
        .unwrap_err();
    let JitError::UnresolvedSymbols(symbols) = err else {
        panic!("{err}");
    };
    assert_eq!(symbols, vec!["cheby_no_such_symbol_anywhere".to_owned()]);
    assert_eq!(jit.module_records(), []);
    // Stubbed, the batch loads and the record comes back.
    let stubs = JitOptions {
        stub_unresolved: true,
        ..JitOptions::from_env()
    };
    let mut stubbed = Jit::configured(CodegenOptions::default(), stubs).unwrap();
    let records = stubbed
        .compile_units(
            &[BatchUnit::Mir(&code), BatchUnit::Mir(&entries)],
            &BatchCx::default(),
        )
        .unwrap();
    let Some(record) = records.first().cloned().flatten() else {
        panic!("no record: {records:?}");
    };
    let mut cold = Jit::configured(CodegenOptions::default(), no_stubs).unwrap();
    let err = cold.load_records(&[&record]).unwrap_err();
    assert!(
        matches!(&err, JitError::UnresolvedSymbols(s) if s == &["cheby_no_such_symbol_anywhere"]),
        "{err}"
    );
}
