//! `link_replay_two_units`: `cheby_codegen_clif`'s two hand-built units of
//! `alone_tests.rs` (copied, since another crate cannot reach them; plan
//! §2.4), each recorded alone and replayed: what `a` imports from `b` is a
//! global hidden definition in `b`'s object with the same name (D-917).

use std::collections::BTreeSet;

use ::object::read::{Object as _, ObjectSymbol as _};
use ::object::{SymbolKind, SymbolScope};

use cheby_codegen_clif::record::{CodeRecord, SymbolLinkage, record_unit};
use cheby_codegen_clif::{CodegenOptions, UnitCx};
use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, IndexVec, ModuleId};
use cheby_mir::borrow::BorrowMask;
use cheby_mir::{
    Abi, AbiSig, Block, Callee, CalleeDecl, Const, DefName, FnKey, Function, LayoutDesc, LayoutRef,
    Local, LocalDecl, MirModule, Operand, Repr, Rvalue, StaticId, StaticKind, StaticObject, Stmt,
    StrId, Terminator, UnitKind,
};

use super::{Format, check_linkage};
use crate::object::replay;

const A: ModuleId = ModuleId::from_u32(1);
const B: ModuleId = ModuleId::from_u32(2);

fn def(module: ModuleId, index: u32) -> DefId {
    DefId::new(module, DefIndex::from_u32(index))
}

/// `b::get`, package-visible; `b::helper`, `priv`; the nullary
/// constructor `b::Empty` of type `b::T`.
fn get() -> DefId {
    def(B, 0)
}
fn helper() -> DefId {
    def(B, 1)
}
fn empty() -> DefId {
    def(B, 2)
}
fn t_type() -> DefId {
    def(B, 3)
}
/// `a::run`.
fn run() -> DefId {
    def(A, 0)
}

fn local(n: u32) -> Local {
    Local::from_u32(n)
}

fn locals(reprs: &[Repr]) -> IndexVec<Local, LocalDecl> {
    IndexVec::from_raw(
        reprs
            .iter()
            .map(|&repr| LocalDecl {
                repr,
                name: None,
                ty: None,
            })
            .collect(),
    )
    .unwrap()
}

fn function(
    key: FnKey,
    params: Vec<Local>,
    reprs: &[Repr],
    stmts: Vec<Stmt>,
    term: Terminator,
    strings: &[&str],
) -> Function {
    Function {
        positions: cheby_mir::Positions::default(),
        key,
        abi: Abi::Cheby,
        params,
        ret: Repr::Value,
        locals: locals(reprs),
        blocks: IndexVec::from_raw(vec![Block {
            pos: Vec::new(),
            params: Vec::new(),
            stmts,
            term,
            unwind: Vec::new(),
        }])
        .unwrap(),
        span: Span::new(FileId::new(0), TextRange::new(0, 0)),
        strings: IndexVec::from_raw(strings.iter().map(|&s| s.into()).collect()).unwrap(),
        entry_loc: Some(StrId::from_u32(0)),
        entry_unwind: None,
        precounted: false,
        borrow: BorrowMask::OWNED,
        sites: Vec::new(),
        call_locs: Vec::new(),
        call_loc_strings: IndexVec::new(),
    }
}

fn call(dst: u32, callee: Callee, args: Vec<Operand>) -> Stmt {
    Stmt::Assign {
        dst: local(dst),
        value: Rvalue::Call { callee, args },
    }
}

fn use_const(dst: u32, c: Const) -> Stmt {
    Stmt::Assign {
        dst: local(dst),
        value: Rvalue::Use(Operand::Const(c)),
    }
}

fn name(def: DefId, module: &str, item: &str) -> DefName {
    DefName {
        def,
        module: module.into(),
        item: item.into(),
    }
}

/// `b::get`'s value shim, which calls `b::get` through its slot: a copy
/// in every unit that uses `b::get` as a value (D-918).
fn get_shim() -> Function {
    function(
        FnKey::ValueShim(get()),
        vec![local(0)],
        &[Repr::Value, Repr::Value],
        vec![call(1, Callee::Direct(get()), Vec::new())],
        Terminator::Return(Operand::Local(local(1))),
        &["b.cheby:1:1"],
    )
}

/// The dictionary both units use: the shim's code word and `b::Empty`'s
/// static.
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
        type_key: 0,
    }
}

fn get_sig() -> AbiSig {
    AbiSig::owned(Abi::Cheby, Vec::new(), Repr::Value)
}

fn helper_sig() -> AbiSig {
    AbiSig::owned(Abi::Cheby, vec![Repr::Value], Repr::Value)
}

/// Unit `b`: `get()` calls the `priv` `helper(s)` with a string through
/// its slot, uses itself as a value and the dictionary, and returns
/// `Empty`, its nullary constructor's static.
fn unit_b() -> MirModule {
    let get_body = function(
        FnKey::Item(get()),
        Vec::new(),
        &[Repr::Value, Repr::Value, Repr::Value],
        vec![
            call(
                0,
                Callee::Direct(helper()),
                vec![Operand::Const(Const::Str(StrId::from_u32(1)))],
            ),
            use_const(1, Const::FnValue(get())),
            use_const(2, Const::Static(StaticId::from_u32(0))),
        ],
        Terminator::Return(Operand::Const(Const::StaticCtor(empty()))),
        &["b.cheby:2:1", "a string of b"],
    );
    let helper_body = function(
        FnKey::Item(helper()),
        vec![local(0)],
        &[Repr::Value],
        Vec::new(),
        Terminator::Return(Operand::Local(local(0))),
        &["b.cheby:3:1"],
    );
    MirModule {
        externals: Vec::new(),
        source: None,
        unit: UnitKind::Code(B),
        path: "b".into(),
        functions: vec![get_body, helper_body, get_shim()],
        callees: vec![
            CalleeDecl {
                def: get(),
                sig: get_sig(),
            },
            CalleeDecl {
                def: helper(),
                sig: helper_sig(),
            },
        ],
        layouts: vec![empty_layout()],
        foreign_layouts: Vec::new(),
        statics: vec![dictionary()],
        names: vec![
            name(get(), "b", "get"),
            name(helper(), "b", "helper"),
            name(empty(), "b", "Empty"),
            name(t_type(), "b", "T"),
        ],
    }
}

/// Unit `a`: `run()` calls `b::get` through its slot, and uses `b::get`
/// as a value and the dictionary, so it holds its own copies of the
/// shim, the function value and the dictionary.
fn unit_a() -> MirModule {
    let run_body = function(
        FnKey::Item(run()),
        Vec::new(),
        &[Repr::Value, Repr::Value, Repr::Value],
        vec![
            call(0, Callee::Direct(get()), Vec::new()),
            use_const(1, Const::FnValue(get())),
            use_const(2, Const::Static(StaticId::from_u32(0))),
        ],
        Terminator::Return(Operand::Local(local(0))),
        &["a.cheby:1:1"],
    );
    MirModule {
        externals: Vec::new(),
        source: None,
        unit: UnitKind::Code(A),
        path: "a".into(),
        functions: vec![run_body, get_shim()],
        callees: vec![CalleeDecl {
            def: get(),
            sig: get_sig(),
        }],
        layouts: Vec::new(),
        foreign_layouts: vec![empty_layout()],
        statics: vec![dictionary()],
        names: vec![
            name(run(), "a", "run"),
            name(get(), "b", "get"),
            name(empty(), "b", "Empty"),
            name(t_type(), "b", "T"),
        ],
    }
}

/// What the front end would answer (D-1110): every
/// item but `b::helper`, which is `priv` and named by no test.
fn named_outside(def: DefId) -> bool {
    def != helper()
}

fn record(unit: &MirModule, format: Format) -> CodeRecord {
    record_unit(
        format.isa(),
        unit,
        &UnitCx {
            named_outside: &named_outside,
        },
        &CodegenOptions::default(),
    )
    .unwrap()
}

/// The two units compiled alone with `record_unit` and replayed: every
/// generated symbol `a` imports is a global hidden definition in `b`'s
/// object with the same name (`b`'s slot, layout word and `cb$ctor$`
/// static among them), no global symbol is defined by both, and both
/// module records are hidden data.
#[test]
fn link_replay_two_units() {
    for &format in Format::ALL {
        let (a, b) = (record(&unit_a(), format), record(&unit_b(), format));
        let a_bytes = replay(&a, format.isa(), "a").unwrap();
        let b_bytes = replay(&b, format.isa(), "b").unwrap();
        // `CHEBY_LINK_TEST_OBJECTS=<directory>` keeps the objects, for a
        // look with `nm` or a trial link by hand.
        if let Some(dir) = std::env::var_os("CHEBY_LINK_TEST_OBJECTS") {
            let dir = std::path::Path::new(&dir);
            std::fs::write(dir.join(format!("a-{format:?}.o")), &a_bytes).unwrap();
            std::fs::write(dir.join(format!("b-{format:?}.o")), &b_bytes).unwrap();
        }
        check_linkage(&a, &a_bytes, format);
        check_linkage(&b, &b_bytes, format);
        let a_file = ::object::File::parse(&*a_bytes).unwrap();
        let b_file = ::object::File::parse(&*b_bytes).unwrap();
        let generated: BTreeSet<&str> = a
            .symbols
            .iter()
            .filter(|s| s.linkage == SymbolLinkage::Import && s.name.starts_with("cb$"))
            .map(|s| s.name.as_str())
            .collect();
        for wanted in ["cb$slot$b$get", "cb$layout$b$Empty", "cb$ctor$b$Empty"] {
            assert!(generated.contains(wanted), "{format:?}: {generated:?}");
        }
        for name in &generated {
            let object_name = format.object_name(name);
            let in_a = a_file
                .symbols()
                .find(|s| s.name() == Ok(object_name.as_str()))
                .unwrap();
            assert!(in_a.is_undefined(), "{format:?}: {name} in a");
            let in_b = b_file
                .symbols()
                .find(|s| s.name() == Ok(object_name.as_str()))
                .unwrap_or_else(|| panic!("{format:?}: {name} is not in b's object"));
            assert!(in_b.is_definition(), "{format:?}: {name} in b");
            assert_eq!(in_b.scope(), SymbolScope::Linkage, "{format:?}: {name}");
            assert_eq!(
                b.symbol(name).map(|s| s.linkage),
                Some(SymbolLinkage::Hidden),
                "{format:?}: {name}"
            );
        }
        // One definition per global symbol across the two objects.
        let globals = |file: &::object::File<'_>| -> BTreeSet<String> {
            file.symbols()
                .filter(|s| s.is_definition() && s.is_global())
                .map(|s| s.name().unwrap().to_owned())
                .collect()
        };
        let both: Vec<_> = globals(&a_file)
            .intersection(&globals(&b_file))
            .cloned()
            .collect();
        assert!(both.is_empty(), "{format:?}: {both:?}");
        for (file, record_name) in [(&a_file, "cb$mod$a"), (&b_file, "cb$mod$b")] {
            let object_name = format.object_name(record_name);
            let symbol = file
                .symbols()
                .find(|s| s.name() == Ok(object_name.as_str()))
                .unwrap();
            assert_eq!(symbol.kind(), SymbolKind::Data, "{format:?}: {record_name}");
            assert_eq!(
                symbol.scope(),
                SymbolScope::Linkage,
                "{format:?}: {record_name}"
            );
            assert!(symbol.is_definition());
        }
    }
}
