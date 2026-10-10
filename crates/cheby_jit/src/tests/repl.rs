//! E4: REPL redefinition through slots, generations and session names
//! (D-043, D-456, D-547).

use cheby_codegen_clif::SessionName;
use cheby_mir::{FieldDesc, LayoutDesc, LayoutRef, StaticId, StaticKind, StaticObject};

use super::*;

/// The session name of an item of entry module `<repl:{entry}>`.
fn session(entry: u32, item: &str, generation: u32, reuse: Option<DefId>) -> SessionName {
    SessionName {
        module: format!("<repl:{entry}>").into(),
        item: item.into(),
        generation,
        reuse,
    }
}

/// `fn <item>(<params>) { return <ctor> }` as item `index` of module
/// `module`: a function that returns the static constructor `ctor`.
fn returns(f: DefId, params: &[Repr], ctor: DefId) -> Function {
    function(
        FnKey::Item(f),
        Abi::Cheby,
        params,
        &[],
        Repr::Value,
        vec![block(
            params
                .iter()
                .enumerate()
                .map(|(n, _)| Stmt::Drop(local(u32::try_from(n).unwrap())))
                .collect(),
            Terminator::Return(Operand::Const(Const::StaticCtor(ctor))),
        )],
        &[],
    )
}

/// `fn <from>() { return <to>(<args>) }`.
fn calls(from: DefId, to: DefId, args: Vec<Operand>) -> Function {
    function(
        FnKey::Item(from),
        Abi::Cheby,
        &[],
        &[],
        Repr::Value,
        vec![block(
            Vec::new(),
            Terminator::TailCall {
                callee: Callee::Direct(to),
                args,
            },
        )],
        &[],
    )
}

/// A code unit of entry module `<repl:{module}>`.
fn entry_unit(
    module: u32,
    functions: Vec<Function>,
    callees: Vec<(DefId, AbiSig)>,
    names: Vec<(DefId, &str, &str)>,
) -> MirModule {
    let mut callees: Vec<CalleeDecl> = callees
        .into_iter()
        .map(|(def, sig)| CalleeDecl { def, sig })
        .collect();
    callees.sort_by_key(|c| c.def);
    let mut all: Vec<DefName> = names
        .into_iter()
        .map(|(def, module, item)| name(def, module, item))
        .collect();
    all.push(name(builtin::TRUE, "", "True"));
    all.push(name(builtin::FALSE, "", "False"));
    code_unit(module, &format!("<repl:{module}>"), functions, callees, all)
}

fn thunk_unit(target: DefId, callee: DefId) -> MirModule {
    entries_unit(
        vec![const_thunk(target, callee, Vec::new())],
        vec![CalleeDecl {
            def: callee,
            sig: cheby_sig(&[]),
        }],
        vec![
            name(callee, "<repl:2>", "g"),
            name(target, "<repl:2>", "RUN"),
        ],
    )
}

fn true_word() -> u64 {
    static_word(abi::layout::TRUE)
}

fn false_word() -> u64 {
    static_word(abi::layout::FALSE)
}

/// Entry 1 defines `f() -> True`, entry 2 `g() { f() }` and a thunk that
/// runs `g`.
fn first_two_entries() -> (Jit, DefId, DefId, DefId) {
    let f = def(1, 0);
    let g = def(2, 0);
    let run = def(2, 1);
    let mut jit = Jit::new().unwrap();
    jit.set_session_names([(f, session(1, "f", 0, None)), (g, session(2, "g", 0, None))]);
    jit.compile(&[entry_unit(
        1,
        vec![returns(f, &[], builtin::TRUE)],
        vec![(f, cheby_sig(&[]))],
        vec![(f, "<repl:1>", "f")],
    )])
    .unwrap();
    jit.compile(&[
        entry_unit(
            2,
            vec![calls(g, f, Vec::new())],
            vec![(f, cheby_sig(&[])), (g, cheby_sig(&[]))],
            vec![(f, "<repl:1>", "f"), (g, "<repl:2>", "g")],
        ),
        thunk_unit(run, g),
    ])
    .unwrap();
    assert_eq!(run_const(&jit, run), EntryOutcome::Returned(true_word()));
    (jit, f, g, run)
}

/// A redefinition with the same signature takes over the slot: the
/// caller compiled before it reaches the new code (D-043); reverting the
/// batch's repoints puts the old code back.
#[test]
fn a_same_signature_redefinition_repoints_the_slot() {
    let (mut jit, f, _, run) = first_two_entries();
    let f2 = def(3, 0);
    let before = jit.slot(f, &cheby_sig(&[])).unwrap();
    jit.set_session_names([(f2, session(3, "f", 1, Some(f)))]);
    jit.compile(&[entry_unit(
        3,
        vec![returns(f2, &[], builtin::FALSE)],
        vec![(f2, cheby_sig(&[]))],
        vec![(f2, "<repl:3>", "f")],
    )])
    .unwrap();
    assert_eq!(jit.slot(f2, &cheby_sig(&[])), Some(before));
    assert_eq!(run_const(&jit, run), EntryOutcome::Returned(false_word()));
    let symbols = jit.decls.symbols();
    assert!(symbols.contains(&"cb$fn$<repl>$f$r1"), "{symbols:?}");
    assert!(!symbols.contains(&"cb$slot$<repl>$f$r1"), "{symbols:?}");
    jit.revert_repoints().unwrap();
    assert_eq!(run_const(&jit, run), EntryOutcome::Returned(true_word()));
    // The explicit API does the same.
    jit.repoint(before, f2).unwrap();
    assert_eq!(run_const(&jit, run), EntryOutcome::Returned(false_word()));
}

/// A redefinition with another signature gets its own slot `$r1`; the old
/// caller keeps the old code, and a new caller reaches the new one.
#[test]
fn another_signature_gets_a_new_slot() {
    let (mut jit, f, _, run) = first_two_entries();
    let f2 = def(3, 0);
    let h = def(4, 0);
    let run_h = def(4, 1);
    jit.set_session_names([
        (f2, session(3, "f", 1, None)),
        (h, session(4, "h", 0, None)),
    ]);
    let one = cheby_sig(&[Repr::Value]);
    jit.compile(&[entry_unit(
        3,
        vec![returns(f2, &[Repr::Value], builtin::FALSE)],
        vec![(f2, one.clone())],
        vec![(f2, "<repl:3>", "f")],
    )])
    .unwrap();
    let old = jit.slot(f, &cheby_sig(&[])).unwrap();
    let new = jit.slot(f2, &one).unwrap();
    assert_ne!(old, new);
    assert_eq!(jit.decls.slot_of(f2).unwrap().symbol, "cb$slot$<repl>$f$r1");
    assert_eq!(jit.decls.slot_of(f).unwrap().symbol, "cb$slot$<repl>$f");
    // `g` keeps calling the old `f`.
    assert_eq!(run_const(&jit, run), EntryOutcome::Returned(true_word()));
    // A slot never points at code of another signature.
    assert!(jit.repoint(old, f2).is_err());
    // A caller compiled now reaches the new `f`.
    jit.compile(&[
        entry_unit(
            4,
            vec![calls(h, f2, vec![Operand::Const(Const::Nil)])],
            vec![(f2, one), (h, cheby_sig(&[]))],
            vec![(f2, "<repl:3>", "f"), (h, "<repl:4>", "h")],
        ),
        entries_unit(
            vec![const_thunk(run_h, h, Vec::new())],
            vec![CalleeDecl {
                def: h,
                sig: cheby_sig(&[]),
            }],
            vec![name(h, "<repl:4>", "h"), name(run_h, "<repl:4>", "RUN")],
        ),
    ])
    .unwrap();
    assert_eq!(run_const(&jit, run_h), EntryOutcome::Returned(false_word()));
}

/// A batch that would define one symbol twice fails before it defines
/// anything, so nothing of it is left in the module: the next batch
/// finalizes without relocations to code that was never defined.
#[test]
fn a_failed_batch_leaves_nothing_behind() {
    let (mut jit, _, g, _) = first_two_entries();
    let a = def(3, 0);
    let b = def(4, 0);
    jit.set_session_names([
        (a, session(3, "dup", 0, None)),
        (b, session(4, "dup", 0, None)),
    ]);
    let err = jit
        .compile(&[
            entry_unit(
                3,
                vec![returns(a, &[], builtin::TRUE)],
                vec![(a, cheby_sig(&[]))],
                vec![(a, "<repl:3>", "dup")],
            ),
            entry_unit(
                4,
                vec![returns(b, &[], builtin::FALSE)],
                vec![(b, cheby_sig(&[]))],
                vec![(b, "<repl:4>", "dup")],
            ),
        ])
        .unwrap_err();
    assert!(err.to_string().contains("defined twice"), "{err}");
    let h = def(5, 0);
    let run_h = def(5, 1);
    jit.set_session_names([(h, session(5, "h", 0, None))]);
    jit.compile(&[
        entry_unit(
            5,
            vec![calls(h, g, Vec::new())],
            vec![(g, cheby_sig(&[])), (h, cheby_sig(&[]))],
            vec![(g, "<repl:2>", "g"), (h, "<repl:5>", "h")],
        ),
        entries_unit(
            vec![const_thunk(run_h, h, Vec::new())],
            vec![CalleeDecl {
                def: h,
                sig: cheby_sig(&[]),
            }],
            vec![name(h, "<repl:5>", "h"), name(run_h, "<repl:5>", "RUN")],
        ),
    ])
    .unwrap();
    assert_eq!(run_const(&jit, run_h), EntryOutcome::Returned(true_word()));
}

/// The same item compiled again with a new generation (a module compiled
/// again, `:reload`): its function key is declared anew, with the new
/// generation's symbol, and its slot is repointed.
#[test]
fn an_item_is_declared_again_for_its_redefinition() {
    let (mut jit, f, _, run) = first_two_entries();
    jit.set_session_names([(f, session(1, "f", 1, Some(f)))]);
    jit.compile(&[entry_unit(
        1,
        vec![returns(f, &[], builtin::FALSE)],
        vec![(f, cheby_sig(&[]))],
        vec![(f, "<repl:1>", "f")],
    )])
    .unwrap();
    assert_eq!(run_const(&jit, run), EntryOutcome::Returned(false_word()));
    assert_eq!(
        jit.decls.func(FnKey::Item(f)).unwrap().symbol,
        "cb$fn$<repl>$f$r1"
    );
    // Without a new generation, a second declaration is still a bug.
    let again = jit.compile(&[entry_unit(
        1,
        vec![returns(f, &[], builtin::TRUE)],
        vec![(f, cheby_sig(&[]))],
        vec![(f, "<repl:1>", "f")],
    )]);
    assert!(again.is_err());
    assert_eq!(run_const(&jit, run), EntryOutcome::Returned(false_word()));
}

/// Every symbol of a redefined item carries its generation
/// (D-547, D-611): function, slot, shim, function
/// value, layout words, a nullary constructor's static, a constant's slot,
/// and the descriptor and dictionary statics of entry modules.
#[test]
#[expect(clippy::too_many_lines, reason = "one hand-built MIR unit")]
fn every_symbol_of_a_redefined_item_carries_its_generation() {
    let ty = def(5, 0);
    let a = def(5, 1);
    let b = def(5, 2);
    let f = def(5, 3);
    let c = def(5, 4);
    let reader = def(5, 5);
    let mut jit = Jit::new().unwrap();
    jit.set_session_names([
        (ty, session(5, "T", 2, None)),
        (a, session(5, "A", 2, None)),
        (b, session(5, "B", 2, None)),
        (f, session(5, "f", 2, None)),
        (c, session(5, "c", 2, None)),
        (reader, session(5, "reader", 0, None)),
    ]);
    let layout = |ctor: DefId, ctor_name: &str, tag: u32, fields: Vec<FieldDesc>| LayoutDesc {
        layout: LayoutRef::Ctor(ctor),
        name: format!("<repl:5>::{ctor_name}").into(),
        tag,
        value_fields: u32::try_from(fields.len()).unwrap(),
        raw_fields: 0,
        fields,
        type_def: ty,
        type_name: "T".into(),
        ctor_name: ctor_name.into(),
        record: false,
        hooks: None,
        type_key: cheby_mir::type_key("<repl:5>", "T"),
    };
    let shim = function(
        FnKey::ValueShim(f),
        Abi::Cheby,
        &[Repr::Value],
        &[],
        Repr::Value,
        vec![block(
            vec![Stmt::Drop(local(0))],
            Terminator::TailCall {
                callee: Callee::Direct(f),
                args: Vec::new(),
            },
        )],
        &[],
    );
    let read = function(
        FnKey::Item(reader),
        Abi::Cheby,
        &[],
        &[Repr::Value, Repr::Value],
        Repr::Value,
        vec![block(
            vec![
                assign(0, Rvalue::LoadConst(c)),
                assign(1, Rvalue::Use(Operand::Const(Const::FnValue(f)))),
                Stmt::Drop(local(1)),
                Stmt::Dup(local(0)),
            ],
            ret(0),
        )],
        &[],
    );
    let mut unit = entry_unit(
        5,
        vec![returns(f, &[], a), shim, read],
        vec![(f, cheby_sig(&[])), (reader, cheby_sig(&[]))],
        vec![
            (ty, "<repl:5>", "T"),
            (a, "<repl:5>", "A"),
            (b, "<repl:5>", "B"),
            (f, "<repl:5>", "f"),
            (c, "<repl:5>", "c"),
            (reader, "<repl:5>", "reader"),
        ],
    );
    unit.layouts = vec![
        layout(a, "A", 0, Vec::new()),
        layout(
            b,
            "B",
            1,
            vec![FieldDesc {
                name: None,
                repr: Repr::Value,
                offset: 8,
            }],
        ),
    ];
    unit.statics = vec![
        StaticObject {
            id: StaticId::from_u32(0),
            kind: StaticKind::Desc,
            module: "<repl:5>".into(),
            name: "T".into(),
            layout: LayoutRef::DescNominal,
            fields: Box::new([
                Const::Str(StrId::from_u32(0)),
                Const::Str(StrId::from_u32(1)),
            ]),
            strings: IndexVec::from_raw(vec!["T".into(), "<repl:5>".into()]).unwrap(),
        },
        StaticObject {
            id: StaticId::from_u32(1),
            kind: StaticKind::Dict,
            module: "<repl:5>".into(),
            name: "Show$<repl:5>::T".into(),
            layout: LayoutRef::Dict {
                fns: 1,
                embedded: 0,
            },
            fields: Box::new([Const::Static(StaticId::from_u32(0)), Const::FnValue(f)]),
            strings: IndexVec::from_raw(Vec::new()).unwrap(),
        },
    ];
    // The dictionary is used, so it is declared.
    unit.functions.push(function(
        FnKey::Item(def(5, 6)),
        Abi::Cheby,
        &[],
        &[],
        Repr::Value,
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::Static(StaticId::from_u32(1)))),
        )],
        &[],
    ));
    unit.callees.push(CalleeDecl {
        def: def(5, 6),
        sig: cheby_sig(&[]),
    });
    unit.names.push(name(def(5, 6), "<repl:5>", "dict"));
    unit.names.sort_by_key(|n| n.def);
    jit.compile(&[unit]).unwrap();
    let symbols = jit.decls.symbols();
    for expected in [
        "cb$fn$<repl>$f$r2",
        "cb$slot$<repl>$f$r2",
        "cb$shim$<repl>$f$r2",
        "cb$fnval$<repl>$f$r2",
        "cb$layout$<repl>$A$r2",
        "cb$ctor$<repl>$A$r2",
        "cb$layout$<repl>$B$r2",
        "cb$const$<repl>$c$r2",
        "cb$desc$<repl>$$T$r2",
        "cb$dict$<repl>$$Show$<repl>::T$r2",
        "cb$fn$<repl>$reader",
        // Not a session item: the entry's own path.
        "cb$fn$<repl:5>$dict",
    ] {
        assert!(
            symbols.contains(&expected),
            "{expected} is not in {symbols:#?}"
        );
    }
}

/// Session values (D-547): a value is stored in its slot after its entry
/// ran; one that takes over an earlier value shares its slot, so code
/// compiled before reads the new value; another type gets a new slot.
#[test]
fn session_values_live_in_constant_slots() {
    let x = def(6, 0);
    let x2 = def(7, 0);
    let x3 = def(8, 0);
    let reader = def(9, 0);
    let run = def(9, 1);
    let mut jit = Jit::new().unwrap();
    jit.set_session_names([(x, session(6, "x", 0, None))]);
    jit.bind_values(&[(x, true_word())]).unwrap();
    jit.set_session_names([(reader, session(9, "read_x", 0, None))]);
    let read = function(
        FnKey::Item(reader),
        Abi::Cheby,
        &[],
        &[Repr::Value],
        Repr::Value,
        vec![block(
            vec![assign(0, Rvalue::LoadConst(x)), Stmt::Dup(local(0))],
            ret(0),
        )],
        &[],
    );
    jit.compile(&[
        entry_unit(
            9,
            vec![read],
            vec![(reader, cheby_sig(&[]))],
            vec![(reader, "<repl:9>", "read_x"), (x, "<repl:6>", "x")],
        ),
        entries_unit(
            vec![const_thunk(run, reader, Vec::new())],
            vec![CalleeDecl {
                def: reader,
                sig: cheby_sig(&[]),
            }],
            vec![
                name(reader, "<repl:9>", "read_x"),
                name(run, "<repl:9>", "RUN"),
            ],
        ),
    ])
    .unwrap();
    assert_eq!(run_const(&jit, run), EntryOutcome::Returned(true_word()));
    // Rebinding with the same type: the reader sees the new value.
    jit.set_session_names([(x2, session(7, "x", 1, Some(x)))]);
    jit.bind_values(&[(x2, false_word())]).unwrap();
    assert_eq!(jit.const_slot(x2), jit.const_slot(x));
    assert_eq!(run_const(&jit, run), EntryOutcome::Returned(false_word()));
    // Another type: a new slot; the reader keeps the old one.
    jit.set_session_names([(x3, session(8, "x", 2, None))]);
    jit.bind_values(&[(x3, true_word())]).unwrap();
    assert_ne!(jit.const_slot(x3), jit.const_slot(x));
    assert_eq!(run_const(&jit, run), EntryOutcome::Returned(false_word()));
    let symbols = jit.decls.symbols();
    assert!(symbols.contains(&"cb$const$<repl>$x"), "{symbols:?}");
    assert!(symbols.contains(&"cb$const$<repl>$x$r2"), "{symbols:?}");
    assert!(!symbols.contains(&"cb$const$<repl>$x$r1"), "{symbols:?}");
}
