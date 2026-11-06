//! The JIT registers through the one routine (D-919, D-1163,
//! D-1169): each batch defines a module record for
//! each code and tests unit and registers them with
//! `cheby_runtime::image::register_records` once finalized, so layout
//! words and constructor statics hold their headers before any entry runs
//! (D-454, D-920), hooks sit on every constructor of a hooked type
//! (D-585), a REPL batch registers only what earlier batches did not, a
//! failed batch registers nothing, and the entries unit has no record, so
//! its thunks are in no registered range (D-1107, D-1111).

#![expect(
    clippy::arithmetic_side_effects,
    reason = "test helpers: offsets inside a record and its tables, and consecutive layout ids"
)]

use cheby_codegen_clif::SessionName;
use cheby_mir::{FieldDesc, LayoutDesc, LayoutRef};
use cheby_runtime::layout;
use cheby_runtime::locate;

use super::*;

/// The word at `at` in a JIT's memory.
fn word(at: usize) -> u64 {
    // SAFETY: the tests read only records, tables and words of a live
    // JIT's finalized memory, at offsets `image.rs`'s module documentation
    // gives.
    unsafe { std::ptr::with_exposed_provenance::<u64>(at).read_unaligned() }
}

fn half(at: usize) -> u32 {
    // SAFETY: as in `word`.
    unsafe { std::ptr::with_exposed_provenance::<u32>(at).read_unaligned() }
}

fn address(at: usize) -> usize {
    usize::try_from(word(at)).unwrap()
}

/// A record's table at `offset` of the header: its count and its first
/// entry's address.
fn table(record: &JitRecord, offset: usize) -> (usize, usize) {
    (
        usize::try_from(word(record.address + offset)).unwrap(),
        address(record.address + offset + 8),
    )
}

/// Per type of `record`, per constructor in tag order: the address of its
/// layout word and of its constructor static (0 for none).
pub(super) fn ctor_words(record: &JitRecord) -> Vec<Vec<(usize, usize)>> {
    let (types, at) = table(record, 16);
    (0..types)
        .map(|t| {
            let entry = at + 32 * t;
            let ctors = usize::try_from(word(entry + 16)).unwrap();
            let first = address(entry + 24);
            (0..ctors)
                .map(|c| {
                    let ctor = first + 48 * c;
                    (address(ctor + 32), address(ctor + 40))
                })
                .collect()
        })
        .collect()
}

/// The hooks of `record`: each hooked type's index and its three words.
fn hooks(record: &JitRecord) -> Vec<(u32, [u64; 3])> {
    let (count, at) = table(record, 48);
    (0..count)
        .map(|h| {
            let entry = at + 32 * h;
            (
                half(entry),
                [word(entry + 8), word(entry + 16), word(entry + 24)],
            )
        })
        .collect()
}

/// The free-shape words of `record`.
fn shapes(record: &JitRecord) -> Vec<u64> {
    let (count, at) = table(record, 64);
    (0..count).map(|s| word(at + 8 * s)).collect()
}

/// The layout id a layout word or constructor static at `at` holds.
pub(super) fn id_at(at: usize) -> u32 {
    abi::header_layout(word(at))
}

/// The layout of constructor `ctor` of type `ty` in module `module`.
fn ctor_layout(
    module: &str,
    ty: DefId,
    ctor: DefId,
    ctor_name: &str,
    tag: u32,
    fields: usize,
) -> LayoutDesc {
    let fields: Vec<FieldDesc> = (0..fields)
        .map(|k| FieldDesc {
            name: None,
            repr: Repr::Value,
            offset: i32::try_from(8 + 8 * k).unwrap(),
        })
        .collect();
    LayoutDesc {
        layout: LayoutRef::Ctor(ctor),
        name: format!("{module}::{ctor_name}").into(),
        tag,
        only_ctor: false,
        value_fields: u32::try_from(fields.len()).unwrap(),
        raw_fields: 0,
        fields,
        type_def: ty,
        type_name: "R".into(),
        ctor_name: ctor_name.into(),
        record: false,
        hooks: None,
        type_key: cheby_mir::type_key(module, "R"),
    }
}

/// `app::R` with `P(Value)` and the nullary `Q`; `make_p()` allocates
/// `P(Nil)` and `get_q()` returns the static `Q`, each run by a thunk.
fn r_program() -> (Prog, DefId, DefId, DefId, DefId) {
    let (ty, p_ctor, q_ctor) = (def(1, 800), def(1, 801), def(1, 802));
    let mut p = Prog {
        layouts: vec![
            ctor_layout("app", ty, p_ctor, "P", 0, 1),
            ctor_layout("app", ty, q_ctor, "Q", 1, 0),
        ],
        ..Prog::default()
    };
    for (d, item) in [(ty, "R"), (p_ctor, "P"), (q_ctor, "Q")] {
        p.names.push(name(d, "app", item));
    }
    let make_p = p.item(
        0,
        "make_p",
        &[],
        &[Repr::Value],
        straight(
            vec![assign(
                0,
                Rvalue::Alloc {
                    layout: LayoutRef::Ctor(p_ctor),
                    fields: vec![Operand::Const(Const::Nil)],
                },
            )],
            0,
        ),
        &[],
    );
    let get_q = p.item(
        1,
        "get_q",
        &[],
        &[],
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::StaticCtor(q_ctor))),
        )],
        &[],
    );
    let run_p = p.call(make_p, Vec::new());
    let run_q = p.call(get_q, Vec::new());
    (p, p_ctor, q_ctor, run_p, run_q)
}

/// The layout words and the constructor static hold the headers the one
/// routine writes once the batch is finalized, before any entry runs:
/// count 1, the scan count and consecutive ids in tag order in the words,
/// count 0 and the id in the static (D-454, D-522, D-920); the runtime's
/// layouts carry the record's facts and free-shape words (D-907); the
/// code then allocates and returns objects with those ids.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn jit_records_layout_words_and_statics_written_before_the_entry_runs() {
    let (p, _, _, run_p, run_q) = r_program();
    let jit = p.compile();
    let records = jit.module_records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].unit, UnitKind::Code(ModuleId::from_u32(1)));
    assert_eq!(&*records[0].path, "app");
    let words = ctor_words(&records[0]);
    let [ty] = words.as_slice() else {
        panic!("{words:?}");
    };
    let [(p_word, p_static), (q_word, q_static)] = ty.as_slice() else {
        panic!("{ty:?}");
    };
    assert_eq!(*p_static, 0);
    let p_id = id_at(*p_word);
    assert!(p_id >= abi::layout::FIRST_USER);
    assert_eq!(id_at(*q_word), p_id + 1);
    assert_eq!(abi::header_rc(word(*p_word)), 1);
    assert_eq!(abi::header_scan(word(*p_word)), 1);
    assert_eq!(word(*q_static), abi::header(abi::RC_STATIC, 0, p_id + 1));
    let info = layout::get(p_id).unwrap();
    assert_eq!((&*info.type_name, &*info.ctor_name), ("R", "P"));
    assert_eq!(&*layout::get(p_id + 1).unwrap().ctor_name, "Q");
    assert_eq!(
        shapes(&records[0]),
        [p_id, p_id + 1].map(|id| layout::free_shape_word(layout::get(id).unwrap()))
    );
    let q = returned(run_const(&jit, run_q));
    assert_eq!(usize::try_from(q).unwrap(), *q_static);
    assert_eq!(layout_of(q), p_id + 1);
    let object = returned(run_const(&jit, run_p));
    assert_eq!(layout_of(object), p_id);
    assert_eq!(rc_of(object), 1);
    release(&jit, object);
}

/// `fn <item>() { return Nil }` as item `index` of `app`, with its value
/// shim, so the unit defines its static function value `cb$fnval$…`.
fn hook_fn(p: &mut Prog, index: u32, item: &str) -> DefId {
    let f = p.item(
        index,
        item,
        &[],
        &[],
        vec![block(
            Vec::new(),
            Terminator::Return(Operand::Const(Const::Nil)),
        )],
        &[],
    );
    p.functions.push(function(
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
    ));
    f
}

/// A type with `@std_hooks` whose unit defines the three function values
/// gets one hook entry, and the routine sets its words on every one of
/// its constructors (D-482, D-585); the other type gets none.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn jit_records_hooks_on_every_constructor_of_a_hooked_type() {
    let (hooked, x, y) = (def(1, 700), def(1, 701), def(1, 702));
    let (plain, z) = (def(1, 710), def(1, 711));
    let mut p = Prog::default();
    let values = [
        hook_fn(&mut p, 20, "equal"),
        hook_fn(&mut p, 21, "hash"),
        hook_fn(&mut p, 22, "debug"),
    ];
    let with_hooks = |ctor, ctor_name, tag| LayoutDesc {
        hooks: Some(values),
        type_def: hooked,
        type_key: cheby_mir::type_key("app", "H"),
        type_name: "H".into(),
        ..ctor_layout("app", hooked, ctor, ctor_name, tag, 0)
    };
    p.layouts = vec![
        with_hooks(x, "X", 0),
        with_hooks(y, "Y", 1),
        ctor_layout("app", plain, z, "Z", 0, 1),
    ];
    for (d, item) in [(hooked, "H"), (x, "X"), (y, "Y"), (plain, "R"), (z, "Z")] {
        p.names.push(name(d, "app", item));
    }
    let jit = p.compile();
    let records = jit.module_records();
    let [record] = records else {
        panic!("{records:?}");
    };
    let hook_entries = hooks(record);
    let [(0, words)] = hook_entries.as_slice() else {
        panic!("{hook_entries:?}");
    };
    assert!(words.iter().all(|&w| w != 0), "{words:?}");
    let ctors = ctor_words(record);
    for &(layout_word, _) in &ctors[0] {
        let info = layout::get(id_at(layout_word)).unwrap();
        assert_eq!(info.hooks(), Some(*words), "{}", info.name);
    }
    let z_info = layout::get(id_at(ctors[1][0].0)).unwrap();
    assert_eq!(z_info.hooks(), None);
}

/// The session name of an item of entry module `<repl:{entry}>`.
fn session(entry: u32, item: &str, generation: u32) -> SessionName {
    SessionName {
        module: format!("<repl:{entry}>").into(),
        item: item.into(),
        generation,
        reuse: None,
    }
}

/// A code unit of module `<repl:{module}>` declaring the type `R` with
/// the nullary constructor `ctor` and a function `f`, named `item`,
/// returning it.
fn repl_unit(module: u32, ty: DefId, ctor: DefId, f: DefId, item: &str) -> MirModule {
    let path = format!("<repl:{module}>");
    let mut unit = code_unit(
        module,
        &path,
        vec![function(
            FnKey::Item(f),
            Abi::Cheby,
            &[],
            &[],
            Repr::Value,
            vec![block(
                Vec::new(),
                Terminator::Return(Operand::Const(Const::StaticCtor(ctor))),
            )],
            &[],
        )],
        vec![CalleeDecl {
            def: f,
            sig: cheby_sig(&[]),
        }],
        vec![
            name(ty, &path, "R"),
            name(ctor, &path, "A"),
            name(f, &path, item),
        ],
    );
    unit.layouts = vec![ctor_layout(&path, ty, ctor, "A", 0, 0)];
    unit
}

/// A REPL session (D-712): a batch that lists a type an earlier batch
/// registered registers it again nowhere (its record lists no type, and
/// the static keeps its header); a redefinition registers the new type
/// with new ids, and the old static keeps the old id, so values of the
/// old definition stay what they were.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn jit_records_repl_redefinition() {
    let (ty, a, f) = (def(1, 0), def(1, 1), def(1, 2));
    let mut jit = Jit::new().unwrap();
    jit.set_session_names([
        (ty, session(1, "R", 0)),
        (a, session(1, "A", 0)),
        (f, session(1, "f", 0)),
    ]);
    jit.compile(&[repl_unit(1, ty, a, f, "f")]).unwrap();
    let first = ctor_words(&jit.module_records()[0]);
    let old_static = first[0][0].1;
    let old_id = id_at(old_static);
    // The same module again, as constant evaluation's batches and the
    // run's list the same layouts: nothing is registered again.
    let g = def(1, 3);
    jit.set_session_names([(g, session(1, "g", 0))]);
    jit.compile(&[repl_unit(1, ty, a, g, "g")]).unwrap();
    assert_eq!(jit.module_records().len(), 2);
    assert_eq!(ctor_words(&jit.module_records()[1]), Vec::<Vec<_>>::new());
    assert_eq!(id_at(old_static), old_id);
    // A redefinition of `R` in a later entry: new layouts, new ids.
    let (ty2, a2, f2) = (def(2, 0), def(2, 1), def(2, 2));
    jit.set_session_names([
        (ty2, session(2, "R", 1)),
        (a2, session(2, "A", 1)),
        (f2, session(2, "f", 1)),
    ]);
    jit.compile(&[repl_unit(2, ty2, a2, f2, "f")]).unwrap();
    let redefined = ctor_words(&jit.module_records()[2]);
    let new_id = id_at(redefined[0][0].1);
    assert_ne!(new_id, old_id);
    assert_ne!(redefined[0][0].1, old_static);
    assert_eq!(id_at(old_static), old_id);
    assert_eq!(&*layout::get(new_id).unwrap().ctor_name, "A");
}

/// The first block of the unit's first function (`make_p`).
fn first_block(code: &mut MirModule) -> &mut Block {
    code.functions[0].blocks.get_mut(bb(0)).unwrap()
}

/// A batch that fails, at its native symbols or in code generation,
/// defines and registers no record and no range (D-867); the corrected
/// batch registers its type.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn jit_records_failing_batch_registers_nothing() {
    let mut jit = Jit::new().unwrap();
    let owner = jit.keep_alive.code_owner;
    let (p, ..) = r_program();
    let [mut code, entries] = p.units();
    first_block(&mut code)
        .stmts
        .push(Stmt::Eval(Rvalue::CallForeign {
            symbol: "cheby_test_records_missing".into(),
            sig: cheby_mir::ForeignSig {
                params: Vec::new(),
                ret: cheby_mir::CType::Void,
            },
            args: Vec::new(),
            blocking: false,
        }));
    let err = jit.compile(&[code, entries]).unwrap_err();
    assert!(matches!(err, JitError::UnresolvedSymbols(_)), "{err}");
    assert_eq!(jit.module_records(), &[]);
    assert_eq!(locate::owner_ranges(owner), Vec::new());
    // Code generation fails: a call of a function no unit declares.
    let (p, ..) = r_program();
    let [mut code, entries] = p.units();
    first_block(&mut code).stmts.push(assign(
        0,
        Rvalue::Call {
            callee: Callee::Direct(def(1, 999)),
            args: Vec::new(),
        },
    ));
    assert!(jit.compile(&[code, entries]).is_err());
    assert_eq!(jit.module_records(), &[]);
    assert_eq!(locate::owner_ranges(owner), Vec::new());
    // The corrected batch.
    let (p, _, _, _, run_q) = r_program();
    jit.compile(&p.units()).unwrap();
    let records = jit.module_records();
    assert_eq!(records.len(), 1);
    let q_static = ctor_words(&records[0])[0][1].1;
    assert!(id_at(q_static) >= abi::layout::FIRST_USER);
    assert_ne!(locate::owner_ranges(owner), Vec::new());
    let q = returned(run_const(&jit, run_q));
    assert_eq!(usize::try_from(q).unwrap(), q_static);
}

/// The entries unit has no module record (D-1107), so its entry thunks
/// are outside every registered range, as in an executable (D-1111):
/// a return address there maps to no range, while the code unit's
/// functions are registered under the JIT's owner, which is unregistered
/// when the `Jit` drops (D-712, D-867).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn jit_records_entry_thunks_are_in_no_range() {
    let (p, _, _, run_p, run_q) = r_program();
    let jit = p.compile();
    let owner = jit.keep_alive.code_owner;
    for target in [run_p, run_q] {
        let thunk = jit.entry(EntryKind::Const(target)).unwrap().thunk();
        #[expect(clippy::as_conversions, reason = "a function's address")]
        let thunk = thunk as usize;
        // A return address inside the thunk: past its first instruction.
        assert_eq!(locate::range_at(thunk + 4), None);
        assert_eq!(locate::range_at(thunk), None);
    }
    let make_p = jit.func_id(FnKey::Item(def(1, 0))).unwrap();
    let start = jit
        .module
        .get_finalized_function(make_p)
        .expose_provenance();
    let (found, range) = locate::range_at(start).unwrap();
    assert_eq!(found, owner);
    assert!(range.cheby_abi);
    drop(jit);
    assert_eq!(locate::owner_ranges(owner), Vec::new());
}
