//! Code records (D-914) and module records (D-919): what a recording
//! module captures from a unit compiled alone, the record's encoding, and
//! the module record's bytes in `cheby_runtime::image`'s layout.
//!
//! The MIR builders are copied from `code_model_tests.rs` and
//! `locs_tests.rs`, whose helpers are private to those files.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "test arithmetic and offsets on small, known values"
)]

use std::mem::size_of;

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir;
use cranelift_module::{DataDescription, Linkage, Module};

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, IndexVec, ModuleId};
use cheby_mir::{
    Abi, AbiSig, Block, CType, Callee, CalleeDecl, Const, DefName, FieldDesc, FnKey, ForeignSig,
    Function, LayoutDesc, LayoutRef, Local, LocalDecl, MirModule, Operand, Repr, Rvalue, Stmt,
    StrId, Terminator, UnitKind,
};
use cheby_runtime::image::{FN_CHEBY_ABI, ModuleRecord, NO_STRING};

use crate::record::{
    CodeRecord, MAX_ALIGN, RECORD_VERSION, RecordContents, RecordError, RecordingModule,
    SymbolKind, SymbolLinkage, record_unit,
};
use crate::*;

fn def(index: u32) -> DefId {
    DefId::new(ModuleId::from_u32(1), DefIndex::from_u32(index))
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

#[expect(clippy::too_many_arguments, reason = "one hand-built MIR function")]
fn function(
    key: FnKey,
    params: Vec<Local>,
    reprs: &[Repr],
    stmts: Vec<Stmt>,
    term: Terminator,
    strings: &[&str],
    call_locs: Vec<(Local, StrId)>,
    call_loc_strings: &[&str],
) -> Function {
    Function {
        key,
        abi: Abi::Cheby,
        params,
        ret: Repr::Value,
        locals: locals(reprs),
        blocks: IndexVec::from_raw(vec![Block {
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
        borrow: cheby_mir::borrow::BorrowMask::OWNED,
        sites: Vec::new(),
        call_locs,
        call_loc_strings: IndexVec::from_raw(call_loc_strings.iter().map(|&s| s.into()).collect())
            .unwrap(),
    }
}

/// `code_model_tests.rs`'s unit: `app::f(x)` calls the runtime, the
/// foreign `cheby_test_kernel(x)` and the `@blocking` foreign
/// `cheby_test_blocking(1)`, calls `app::g()` through its slot, and
/// returns `Pair("hi", Nil)`; `app::g()` returns `Nil`. It holds every
/// kind of data object a unit without constants has: import words, a
/// string, slots, a layout word, a `@blocking` shim.
#[expect(clippy::too_many_lines, reason = "one hand-built MIR unit")]
fn code_model_unit() -> MirModule {
    let (f, g, pair, pair_type) = (def(0), def(1), def(2), def(3));
    let f_body = function(
        FnKey::Item(f),
        vec![local(0)],
        &[Repr::Value, Repr::Value, Repr::Value],
        vec![
            Stmt::Eval(Rvalue::CallForeign {
                symbol: "cheby_test_kernel".into(),
                sig: ForeignSig {
                    params: vec![CType::Ptr],
                    ret: CType::Void,
                },
                args: vec![Operand::Local(local(0))],
                blocking: false,
            }),
            Stmt::Eval(Rvalue::CallForeign {
                symbol: "cheby_test_blocking".into(),
                sig: ForeignSig {
                    params: vec![CType::I64],
                    ret: CType::Void,
                },
                args: vec![Operand::Const(Const::Int(1))],
                blocking: true,
            }),
            Stmt::Assign {
                dst: local(1),
                value: Rvalue::Call {
                    callee: Callee::Direct(g),
                    args: Vec::new(),
                },
            },
            Stmt::Assign {
                dst: local(2),
                value: Rvalue::Alloc {
                    layout: LayoutRef::Ctor(pair),
                    fields: vec![
                        Operand::Const(Const::Str(StrId::from_u32(1))),
                        Operand::Const(Const::Nil),
                    ],
                },
            },
        ],
        Terminator::Return(Operand::Local(local(2))),
        &["a.cheby:1:4", "hi"],
        Vec::new(),
        &[],
    );
    let g_body = function(
        FnKey::Item(g),
        Vec::new(),
        &[],
        Vec::new(),
        Terminator::Return(Operand::Const(Const::Nil)),
        &["a.cheby:2:4"],
        Vec::new(),
        &[],
    );
    let field = |offset| FieldDesc {
        name: None,
        repr: Repr::Value,
        offset,
    };
    let name = |def, item: &str| DefName {
        def,
        module: "app".into(),
        item: item.into(),
    };
    MirModule {
        unit: UnitKind::Code(ModuleId::from_u32(1)),
        path: "app".into(),
        functions: vec![f_body, g_body],
        callees: vec![
            CalleeDecl {
                def: f,
                sig: AbiSig::owned(Abi::Cheby, vec![Repr::Value], Repr::Value),
            },
            CalleeDecl {
                def: g,
                sig: AbiSig::owned(Abi::Cheby, Vec::new(), Repr::Value),
            },
        ],
        layouts: vec![LayoutDesc {
            layout: LayoutRef::Ctor(pair),
            name: "app::Pair".into(),
            tag: 0,
            only_ctor: true,
            fields: vec![field(8), field(16)],
            value_fields: 2,
            raw_fields: 0,
            type_def: pair_type,
            type_name: "Pair".into(),
            ctor_name: "Pair".into(),
            record: false,
            hooks: None,
            type_key: 0,
        }],
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: vec![
            name(f, "f"),
            name(g, "g"),
            name(pair, "Pair"),
            name(pair_type, "Pair"),
        ],
    }
}

fn call_f1(arg: u32) -> Rvalue {
    Rvalue::Call {
        callee: Callee::Direct(def(1)),
        args: vec![Operand::Local(local(arg))],
    }
}

/// `locs_tests.rs`'s calls with locations, in module `app::sub`:
/// `f0(x)` calls `f1` twice, at `src/main.cheby:3:5` and at
/// `<repl:2>:4:7` (a path with a colon), and tail-calls it; `f1(x)`
/// returns `x`.
fn located_unit(kind: UnitKind) -> MirModule {
    let f0 = function(
        FnKey::Item(def(0)),
        vec![local(0)],
        &[Repr::Value, Repr::Value, Repr::Value],
        vec![
            Stmt::Assign {
                dst: local(1),
                value: call_f1(0),
            },
            Stmt::Assign {
                dst: local(2),
                value: call_f1(1),
            },
        ],
        Terminator::TailCall {
            callee: Callee::Direct(def(1)),
            args: vec![Operand::Local(local(2))],
        },
        &["src/main.cheby:1:4"],
        vec![
            (local(1), StrId::from_u32(0)),
            (local(2), StrId::from_u32(1)),
        ],
        &["src/main.cheby:3:5", "<repl:2>:4:7"],
    );
    let f1 = function(
        FnKey::Item(def(1)),
        vec![local(0)],
        &[Repr::Value],
        Vec::new(),
        Terminator::Return(Operand::Local(local(0))),
        &["src/main.cheby:6:4"],
        Vec::new(),
        &[],
    );
    let sig = AbiSig::owned(Abi::Cheby, vec![Repr::Value], Repr::Value);
    MirModule {
        unit: kind,
        path: "app::sub".into(),
        functions: vec![f0, f1],
        callees: vec![
            CalleeDecl {
                def: def(0),
                sig: sig.clone(),
            },
            CalleeDecl { def: def(1), sig },
        ],
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: (0..2)
            .map(|n| DefName {
                def: def(n),
                module: "app::sub".into(),
                item: format!("f{n}").into(),
            })
            .collect(),
    }
}

fn code() -> UnitKind {
    UnitKind::Code(ModuleId::from_u32(1))
}

/// `f0` is named by other units, `f1` is not (D-1110).
fn named(def: DefId) -> bool {
    def.index == DefIndex::from_u32(0)
}

fn record(unit: &MirModule) -> CodeRecord {
    record_unit(
        isa::host_isa().unwrap(),
        unit,
        &UnitCx {
            named_outside: &named,
        },
        &CodegenOptions::default(),
    )
    .unwrap()
}

/// The units the encoding tests run on.
fn units() -> [MirModule; 3] {
    [
        code_model_unit(),
        located_unit(code()),
        located_unit(UnitKind::Tests(ModuleId::from_u32(1))),
    ]
}

/// A record survives its encoding both ways: `decode(encode(r)) == r`
/// and `encode(decode(b)) == b`.
#[test]
fn record_encoding_round_trips() {
    for unit in units() {
        let r = record(&unit);
        let bytes = r.encode();
        let decoded = CodeRecord::decode(&bytes).unwrap();
        assert_eq!(decoded, r, "{}", unit.path);
        assert_eq!(decoded.encode(), bytes, "{}", unit.path);
        assert!(!r.functions.is_empty() && !r.data.is_empty());
    }
}

/// The encoding starts with its magic and version; a record of another
/// version is refused, never read.
#[test]
fn record_other_version_is_an_error() {
    let bytes = record(&located_unit(code())).encode();
    assert_eq!(bytes.get(..4), Some(&b"cbcr"[..]));
    assert_eq!(bytes.get(4..8), Some(&RECORD_VERSION.to_le_bytes()[..]));
    for version in [0, RECORD_VERSION + 1, u32::MAX] {
        let mut other = bytes.clone();
        other[4..8].copy_from_slice(&version.to_le_bytes());
        assert_eq!(
            CodeRecord::decode(&other),
            Err(RecordError::Version(version))
        );
    }
    let mut magic = bytes;
    magic[0] = b'x';
    assert_eq!(
        CodeRecord::decode(&magic),
        Err(RecordError::Malformed("not a code record"))
    );
}

/// `decode` never panics (D-422): every truncation of a real record is an
/// error, and every single-byte change of it decodes to an error or to a
/// record that encodes back to the changed bytes.
#[test]
fn record_decode_never_panics() {
    let bytes = record(&located_unit(code())).encode();
    for len in 0..bytes.len() {
        assert!(CodeRecord::decode(&bytes[..len]).is_err(), "{len}");
    }
    // Every byte takes every other value: about a million decodes, spread
    // over threads by position.
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(16));
    let chunk = bytes.len().div_ceil(threads);
    let accepted: usize = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|t| {
                let bytes = &bytes;
                scope.spawn(move || {
                    let mut changed = bytes.clone();
                    let mut accepted = 0;
                    for at in (t * chunk)..((t + 1) * chunk).min(bytes.len()) {
                        let original = bytes[at];
                        for value in (0..=u8::MAX).filter(|&v| v != original) {
                            changed[at] = value;
                            if let Ok(record) = CodeRecord::decode(&changed) {
                                assert!(record.encode() == changed, "byte {at} = {value}");
                                accepted += 1;
                            }
                        }
                        changed[at] = original;
                    }
                    accepted
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).sum()
    });
    // Code bytes and addends are free: some changes are records too.
    assert!(accepted > 0);
}

/// An alignment above `MAX_ALIGN` is malformed, in `check` and in
/// `decode`, for functions and data: an object writer pads its section up
/// to it, and 2^63 panicked inside `object` while 2^55 aborted the process
/// (wave 2 review, SND-2). `MAX_ALIGN` itself is accepted.
#[test]
fn record_alignment_bounded() {
    let base = record(&code_model_unit());
    assert!(!base.functions.is_empty() && !base.data.is_empty());
    for align in [MAX_ALIGN * 2, 1 << 55, 1 << 63] {
        let mut function = base.clone();
        function.functions[0].align = align;
        let mut data = base.clone();
        data.data[0].align = Some(align);
        for bad in [function, data] {
            let expected = RecordError::Malformed("an alignment too large");
            assert_eq!(bad.check(), Err(expected.clone()), "{align}");
            assert_eq!(CodeRecord::decode(&bad.encode()), Err(expected), "{align}");
        }
    }
    let mut largest = base;
    largest.functions[0].align = MAX_ALIGN;
    largest.data[0].align = Some(MAX_ALIGN);
    assert_eq!(largest.check(), Ok(()));
    assert_eq!(CodeRecord::decode(&largest.encode()), Ok(largest));
}

/// Counts and lengths are checked against what remains: a huge count or
/// length in a short input is an error, without allocating for it.
#[test]
fn record_decode_checks_counts() {
    let mut bytes = b"cbcr".to_vec();
    bytes.extend_from_slice(&RECORD_VERSION.to_le_bytes());
    let mut huge = bytes.clone();
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(CodeRecord::decode(&huge).is_err());
    let mut name = bytes.clone();
    name.extend_from_slice(&1_u32.to_le_bytes());
    name.extend_from_slice(&[0, 0]);
    name.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        CodeRecord::decode(&name),
        Err(RecordError::Malformed("the record ends early"))
    );
    let mut empty = bytes;
    empty.extend_from_slice(&[0; 12]);
    assert_eq!(
        CodeRecord::decode(&empty),
        Ok(CodeRecord {
            symbols: Vec::new(),
            functions: Vec::new(),
            data: Vec::new(),
        })
    );
    let mut trailing = empty.clone();
    trailing.push(0);
    assert!(CodeRecord::decode(&trailing).is_err());
}

/// One unit compiled twice gives the same record and the same bytes
/// (D-264).
#[test]
fn record_same_unit_twice_same_bytes() {
    for unit in units() {
        let first = record(&unit);
        let second = record(&unit.clone());
        assert_eq!(first, second);
        assert_eq!(first.encode(), second.encode());
    }
}

/// The symbol table is in declaration order with each symbol's kind and
/// linkage; relocations name symbols by index; every symbol of linkage
/// `Local` or `Hidden` is defined once and every import is not; nothing
/// is a libcall (D-913).
#[test]
fn record_symbols_kinds_and_linkage() {
    let r = record(&code_model_unit());
    let kinds: Vec<_> = r
        .symbols
        .iter()
        .map(|s| (s.name.as_str(), s.kind, s.linkage))
        .collect();
    let find = |name: &str| {
        r.symbol(name)
            .unwrap_or_else(|| panic!("{name} in {kinds:?}"))
    };
    assert_eq!(find("cb$fn$app$f").kind, SymbolKind::Function);
    assert_eq!(
        find("cb$slot$app$g").kind,
        SymbolKind::Data { writable: true }
    );
    assert_eq!(
        find("cb$imp$cheby_rt_alloc").kind,
        SymbolKind::Data { writable: false }
    );
    assert_eq!(find("cb$imp$cheby_rt_alloc").linkage, SymbolLinkage::Local);
    assert_eq!(find("cheby_rt_alloc").linkage, SymbolLinkage::Import);
    assert!(r.defines("cb$fn$app$f") && r.defines("cb$imp$cheby_rt_alloc"));
    assert!(!r.defines("cheby_rt_alloc") && !r.defines("nothing"));
    assert!(r.symbol("nothing").is_none());
    // Every name once, no libcall, no anonymous symbol.
    let mut names: Vec<_> = r.symbols.iter().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), r.symbols.len());
    assert!(names.iter().all(|n| !n.contains("libcall")), "{names:?}");
    // Definitions name defined symbols of their kind; relocations name
    // symbols of the table.
    for function in &r.functions {
        let symbol = r.symbol_at(function.symbol).unwrap();
        assert_eq!(symbol.kind, SymbolKind::Function);
        assert_ne!(symbol.linkage, SymbolLinkage::Import);
        assert_eq!(function.align, FUNCTION_ALIGN);
        for reloc in &function.relocs {
            assert!(r.symbol_at(reloc.symbol).is_some());
        }
    }
    for object in &r.data {
        let symbol = r.symbol_at(object.symbol).unwrap();
        assert!(matches!(symbol.kind, SymbolKind::Data { .. }));
        // An object with relocations keeps its zeros as bytes.
        if !object.relocs.is_empty() {
            assert!(matches!(object.contents, RecordContents::Bytes(_)));
        }
    }
    // Declaration order: the symbols the recording module saw first come
    // first, so a second recording of the same declarations agrees.
    assert_eq!(r.symbols, record(&code_model_unit()).symbols);
}

/// The recording module refuses what a record cannot hold, and `finish`
/// refuses a defined-linkage symbol that was never defined.
#[test]
fn record_module_refuses_what_a_record_cannot_hold() {
    let mut m = RecordingModule::new(isa::host_isa().unwrap());
    let sig = m.make_signature();
    let f = m.declare_function("cb$fn$x", Linkage::Local, &sig).unwrap();
    assert!(m.declare_anonymous_function(&sig).is_err());
    assert!(m.declare_anonymous_data(false, false).is_err());
    assert!(
        m.declare_data("cb$tls", Linkage::Local, true, true)
            .is_err()
    );
    let mut ctx = cranelift_codegen::Context::for_function(ir::Function::new());
    assert!(
        m.define_function_with_control_plane(f, &mut ctx, &mut ControlPlane::default())
            .is_err()
    );
    assert!(matches!(m.finish(), Err(RecordError::Unsupported(_))));

    // No record has a libcall relocation (D-913), nor a relocation
    // kind it cannot encode.
    for (kind, name) in [
        (
            cranelift_codegen::binemit::Reloc::X86CallPCRel4,
            cranelift_module::ModuleRelocTarget::LibCall(ir::LibCall::Memcpy),
        ),
        (
            cranelift_codegen::binemit::Reloc::X86GOTPCRel4,
            cranelift_module::ModuleRelocTarget::user(0, 0),
        ),
    ] {
        let mut m = RecordingModule::new(isa::host_isa().unwrap());
        let sig = m.make_signature();
        let f = m.declare_function("cb$fn$x", Linkage::Local, &sig).unwrap();
        let reloc = cranelift_module::ModuleReloc {
            offset: 0,
            kind,
            name,
            addend: 0,
        };
        assert!(m.define_function_bytes(f, 16, &[0; 8], &[reloc]).is_err());
        assert!(matches!(m.finish(), Err(RecordError::Unsupported(_))));
    }

    let mut m = RecordingModule::new(isa::host_isa().unwrap());
    let sig = m.make_signature();
    m.declare_function("cb$fn$x", Linkage::Local, &sig).unwrap();
    m.declare_function("imported", Linkage::Import, &sig)
        .unwrap();
    assert_eq!(
        m.finish(),
        Err(RecordError::Undefined("cb$fn$x".to_owned()))
    );

    // A definition of an import, or a second definition, is refused.
    let mut m = RecordingModule::new(isa::host_isa().unwrap());
    let d = m
        .declare_data("imported", Linkage::Import, false, false)
        .unwrap();
    let mut desc = DataDescription::new();
    desc.define(vec![0; 8].into_boxed_slice());
    assert!(m.define_data(d, &desc).is_err());
    let local = m
        .declare_data("cb$x", Linkage::Local, false, false)
        .unwrap();
    m.define_data(local, &desc).unwrap();
    assert!(m.define_data(local, &desc).is_err());
    let r = m.finish().unwrap();
    assert_eq!(r.data.len(), 1);
    assert!(format!("{r:?}").contains("cb$x"));
}

/// A declaration's linkage is the merge of every declaration of the name,
/// as `ModuleDeclarations` merges it: an import declared again `Hidden` is
/// `Hidden`, and data declared writable once is writable.
#[test]
fn record_linkage_merges_as_declarations_do() {
    let mut m = RecordingModule::new(isa::host_isa().unwrap());
    let d = m
        .declare_data("cb$x", Linkage::Import, false, false)
        .unwrap();
    m.declare_data("cb$x", Linkage::Hidden, true, false)
        .unwrap();
    let mut desc = DataDescription::new();
    desc.define_zeroinit(8);
    m.define_data(d, &desc).unwrap();
    let r = m.finish().unwrap();
    assert_eq!(r.symbols.len(), 1);
    assert_eq!(r.symbols[0].linkage, SymbolLinkage::Hidden);
    assert_eq!(r.symbols[0].kind, SymbolKind::Data { writable: true });
    assert_eq!(r.data[0].contents, RecordContents::Zeros(8));
}

// ---- Module records (D-919) ----

/// `ModuleRecord`'s size, which the tables' offsets below rely on.
#[test]
fn record_modrec_size_of_module_record() {
    assert_eq!(size_of::<ModuleRecord>(), 112);
}

/// The bytes of `record`'s object `name` and the relocations at each
/// offset: `(offset, target name, addend)`.
fn object<'r>(record: &'r CodeRecord, name: &str) -> (&'r [u8], Vec<(u32, String, i64)>) {
    let symbol = record
        .symbols
        .iter()
        .position(|s| s.name == name)
        .unwrap_or_else(|| panic!("no {name}"));
    let object = record
        .data
        .iter()
        .find(|d| d.symbol as usize == symbol)
        .unwrap();
    let RecordContents::Bytes(bytes) = &object.contents else {
        panic!("{name} is zero-filled");
    };
    let relocs = object
        .relocs
        .iter()
        .map(|r| {
            (
                r.offset,
                record.symbols[r.symbol as usize].name.clone(),
                r.addend,
            )
        })
        .collect();
    (bytes, relocs)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// A string of a record's string table.
fn string_at(bytes: &[u8], table: usize, offset: u32) -> &str {
    let at = table + offset as usize;
    let len = u32_at(bytes, at) as usize;
    std::str::from_utf8(&bytes[at + 4..at + 4 + len]).unwrap()
}

/// The module record of a code unit, read back at `image.rs`'s offsets:
/// the function count and one entry per defined function in definition
/// order, each with a relocation to its code, its length, its ABI flag
/// and its call sites; one site's return offset, path, line and column;
/// every other table empty, with pointer 0 and no relocation.
#[test]
fn record_modrec_layout_matches_image() {
    let unit = located_unit(code());
    let r = record(&unit);
    // The call sites as code generation reports them.
    let mut m = RecordingModule::new(isa::host_isa().unwrap());
    let ucx = UnitCx {
        named_outside: &named,
    };
    let compiled = compile_unit(&mut m, &unit, &ucx, &CodegenOptions::default()).unwrap();
    let name = mangle::module_record("app::sub");
    assert_eq!(name, "cb$mod$app$sub");
    let (bytes, relocs) = object(&r, &name);
    let reloc_at = |at: usize| {
        relocs
            .iter()
            .find(|(offset, _, _)| *offset as usize == at)
            .map(|(_, target, addend)| (target.as_str(), *addend))
    };
    // The empty tables: types, externals, hooks, free shapes, constants.
    for table in [16, 32, 48, 64, 96] {
        assert_eq!(u64_at(bytes, table), 0, "table at {table}");
        assert_eq!(u64_at(bytes, table + 8), 0, "table at {table}");
        assert_eq!(reloc_at(table + 8), None, "table at {table}");
    }
    // The call-site table: one entry per function defined, in definition
    // order, right after the header.
    let count = u64_at(bytes, 80) as usize;
    assert_eq!(count, compiled.defined.len());
    assert_eq!(count, r.functions.len());
    assert_eq!(reloc_at(88), Some((name.as_str(), 112)));
    let strings = u64_at(bytes, 0) as usize;
    let strings_at = reloc_at(8).map(|(target, addend)| {
        assert_eq!(target, name);
        addend as usize
    });
    let strings_at = strings_at.unwrap();
    assert_eq!(strings_at + strings, bytes.len());
    let mut sites_seen = Vec::new();
    for (i, (code, function)) in compiled.defined.iter().zip(&r.functions).enumerate() {
        let entry = 112 + 32 * i;
        let symbol = &r.symbols[function.symbol as usize].name;
        assert_eq!(reloc_at(entry), Some((symbol.as_str(), 0)));
        assert_eq!(u32_at(bytes, entry + 8) as usize, function.bytes.len());
        assert_eq!(u32_at(bytes, entry + 8), code.len);
        let flags = u32_at(bytes, entry + 12);
        assert_eq!(flags, if code.cheby_abi { FN_CHEBY_ABI } else { 0 });
        let sites = u64_at(bytes, entry + 16) as usize;
        assert_eq!(sites, code.sites.len());
        if sites == 0 {
            assert_eq!(u64_at(bytes, entry + 24), 0);
            assert_eq!(reloc_at(entry + 24), None);
            continue;
        }
        let (target, at) = reloc_at(entry + 24).unwrap();
        assert_eq!(target, name);
        for (k, site) in code.sites.iter().enumerate() {
            let at = at as usize + 16 * k;
            assert_eq!(u32_at(bytes, at), site.ret_offset);
            let path = u32_at(bytes, at + 4);
            assert_ne!(path, NO_STRING);
            let location = format!(
                "{}:{}:{}",
                string_at(bytes, strings_at, path),
                u32_at(bytes, at + 8),
                u32_at(bytes, at + 12)
            );
            assert_eq!(location, site.location);
            sites_seen.push(location);
        }
    }
    // `f0`'s two located calls, the second with a colon in its path.
    assert_eq!(sites_seen, ["src/main.cheby:3:5", "<repl:2>:4:7"]);
}

/// The module record is read-only data with hidden linkage (D-917), and a
/// code unit's record has no tests record.
#[test]
fn record_modrec_is_hidden_read_only_data() {
    let r = record(&code_model_unit());
    let symbol = r.symbol("cb$mod$app").unwrap();
    assert_eq!(symbol.linkage, SymbolLinkage::Hidden);
    assert_eq!(symbol.kind, SymbolKind::Data { writable: false });
    assert!(r.defines("cb$mod$app"));
    assert!(r.symbol("cb$mod$test$app").is_none());
    // The code-model unit has functions without located calls: every
    // entry's site table is empty, and so is the string table.
    let (bytes, relocs) = object(&r, "cb$mod$app");
    assert_eq!(u64_at(bytes, 0), 0);
    assert_eq!(u64_at(bytes, 80) as usize, r.functions.len());
    assert!(relocs.iter().all(|(offset, _, _)| *offset != 8));
}

/// A tests unit's record is `cb$mod$test$<path>`
/// (D-1107).
#[test]
fn record_modrec_tests_unit_name() {
    let r = record(&located_unit(UnitKind::Tests(ModuleId::from_u32(1))));
    assert_eq!(
        mangle::tests_module_record("app::sub"),
        "cb$mod$test$app$sub"
    );
    let symbol = r.symbol("cb$mod$test$app$sub").unwrap();
    assert_eq!(symbol.linkage, SymbolLinkage::Hidden);
    assert_eq!(symbol.kind, SymbolKind::Data { writable: false });
    assert!(r.symbol("cb$mod$app$sub").is_none());
}

/// The entries unit gets no record: it is never cached (D-964).
#[test]
fn record_modrec_entries_unit_refused() {
    let mut unit = located_unit(code());
    unit.unit = UnitKind::Entries;
    unit.path = "".into();
    let result = record_unit(
        isa::host_isa().unwrap(),
        &unit,
        &UnitCx {
            named_outside: &named,
        },
        &CodegenOptions::default(),
    );
    assert!(
        matches!(&result, Err(RecordError::Unsupported(what)) if what.contains("entries")),
        "{result:?}"
    );
}
