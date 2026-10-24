//! Replay tests `link_replay_*` (D-915): code records of hand-built units
//! replayed into ELF and Mach-O objects for the host architecture (D-936),
//! read back with the `object` crate. Mach-O is replayed on aarch64 hosts
//! only: macOS is aarch64 only (D-892), so on an x86-64 host the tests
//! cover ELF alone.
//!
//! The MIR builders are copied from `cheby_codegen_clif`'s tests, which
//! another crate cannot reach (plan §2.4).

#![expect(
    clippy::arithmetic_side_effects,
    reason = "test counters on small, known values"
)]

use ::object::read::{Object as _, ObjectSection as _, ObjectSymbol as _};
use ::object::{RelocationFlags, SectionKind, SymbolScope, elf, macho};

use cheby_codegen_clif::record::{
    CodeRecord, RecordError, RecordReloc, SymbolLinkage, record_unit,
};
use cheby_codegen_clif::{CodegenOptions, UnitCx, isa};
use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, IndexVec, ModuleId};
use cheby_mir::{
    Abi, AbiSig, Block, CType, Callee, CalleeDecl, Const, DefName, FieldDesc, FnKey, ForeignSig,
    Function, LayoutDesc, LayoutRef, Local, LocalDecl, MirModule, Operand, Repr, Rvalue, Stmt,
    StrId, Terminator, UnitKind,
};

use crate::LinkError;
use crate::object::replay;

mod two_units;

/// The object formats replay writes, by the triple that picks them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Elf,
    MachO,
}

impl Format {
    /// The formats replay writes for the host architecture: ELF, and
    /// Mach-O on aarch64 hosts only, since macOS is aarch64 only (D-892):
    /// on an x86-64 host these tests cover ELF alone.
    const ALL: &[Self] = if cfg!(target_arch = "aarch64") {
        &[Self::Elf, Self::MachO]
    } else {
        &[Self::Elf]
    };

    /// The host architecture's triple of this format (D-936: only the
    /// host's backend is compiled in).
    fn triple(self) -> String {
        let arch = std::env::consts::ARCH;
        match self {
            Self::Elf => format!("{arch}-unknown-linux-gnu"),
            Self::MachO => format!("{arch}-apple-darwin"),
        }
    }

    fn isa(self) -> cranelift_codegen::isa::OwnedTargetIsa {
        isa::isa_for(&self.triple()).unwrap()
    }

    /// A symbol's name as the object spells it: Mach-O adds a leading
    /// underscore.
    fn object_name(self, name: &str) -> String {
        match self {
            Self::Elf => name.to_owned(),
            Self::MachO => format!("_{name}"),
        }
    }
}

fn def(module: u32, index: u32) -> DefId {
    DefId::new(ModuleId::from_u32(module), DefIndex::from_u32(index))
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

/// `app::f(x)` calls the foreign `cheby_test_kernel(x)` and the
/// `@blocking` foreign `cheby_test_blocking(1)`, calls `app::g()` through
/// its slot, and returns `Pair("hi", Nil)`; `app::g()` returns `Nil`
/// (`cheby_codegen_clif`'s code-model unit).
#[expect(clippy::too_many_lines, reason = "one hand-built MIR unit")]
fn code_model_unit() -> MirModule {
    let (f, g, pair, pair_type) = (def(1, 0), def(1, 1), def(1, 2), def(1, 3));
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
        vec![(local(1), StrId::from_u32(0))],
        &["src/app.cheby:3:5"],
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

/// `app` is named by other units in `f` only (D-1110).
fn named(def: DefId) -> bool {
    def.index == DefIndex::from_u32(0)
}

fn record_for(unit: &MirModule, format: Format) -> CodeRecord {
    record_unit(
        format.isa(),
        unit,
        &UnitCx {
            named_outside: &named,
        },
        &CodegenOptions::default(),
    )
    .unwrap()
}

/// The record of `unit` for `format`, replayed.
fn replayed(unit: &MirModule, format: Format) -> (CodeRecord, Vec<u8>) {
    let record = record_for(unit, format);
    let bytes = replay(&record, format.isa(), &unit.path).unwrap();
    (record, bytes)
}

/// Every symbol of `record` in the object `bytes` with its recorded
/// linkage: `Local` a local symbol, `Hidden` a global hidden one (ELF
/// `STV_HIDDEN`, Mach-O private extern), `Import` undefined; nothing is
/// exported.
fn check_linkage(record: &CodeRecord, bytes: &[u8], format: Format) {
    let file = ::object::File::parse(bytes).unwrap();
    assert_eq!(
        file.format(),
        match format {
            Format::Elf => ::object::BinaryFormat::Elf,
            Format::MachO => ::object::BinaryFormat::MachO,
        }
    );
    let mut seen = 0;
    for symbol in &record.symbols {
        let wanted = format.object_name(&symbol.name);
        let found = file
            .symbols()
            .find(|s| s.name() == Ok(wanted.as_str()))
            .unwrap_or_else(|| panic!("{wanted} is not in the {format:?} object"));
        seen += 1;
        match symbol.linkage {
            SymbolLinkage::Local => {
                assert!(found.is_local() && found.is_definition(), "{wanted}");
                assert_eq!(found.scope(), SymbolScope::Compilation, "{wanted}");
            }
            SymbolLinkage::Hidden => {
                assert!(found.is_global() && found.is_definition(), "{wanted}");
                assert_eq!(found.scope(), SymbolScope::Linkage, "{wanted}");
                match found.flags() {
                    ::object::SymbolFlags::Elf { st_other, .. } => {
                        assert_eq!(st_other & 3, elf::STV_HIDDEN, "{wanted}");
                    }
                    ::object::SymbolFlags::MachO { .. } => {}
                    other => panic!("{other:?}"),
                }
            }
            SymbolLinkage::Import => {
                assert!(found.is_undefined(), "{wanted}");
            }
            SymbolLinkage::Export => panic!("{wanted} is exported in a unit's record"),
        }
    }
    assert_eq!(seen, record.symbols.len());
    // Nothing is exported: no defined symbol of dynamic scope.
    for symbol in file.symbols() {
        assert!(
            symbol.is_undefined() || symbol.scope() != SymbolScope::Dynamic,
            "{:?} is exported",
            symbol.name()
        );
    }
}

/// The units the tests replay: the code-model unit and the same functions
/// as a tests unit.
fn units() -> Vec<MirModule> {
    let mut tests = code_model_unit();
    tests.unit = UnitKind::Tests(ModuleId::from_u32(1));
    vec![code_model_unit(), tests]
}

/// A record replays into ELF for the host architecture's Linux triple,
/// every symbol with its recorded linkage; the module record is hidden
/// data.
#[test]
fn link_replay_elf_keeps_names_and_linkage() {
    for unit in units() {
        let (record, bytes) = replayed(&unit, Format::Elf);
        check_linkage(&record, &bytes, Format::Elf);
        assert!(
            record
                .symbols
                .iter()
                .any(|s| s.linkage == SymbolLinkage::Hidden)
        );
    }
}

/// A record replays into Mach-O for the host architecture's macOS triple,
/// every symbol with its recorded linkage. aarch64 hosts only: macOS is
/// aarch64 only (D-892).
#[test]
#[cfg(target_arch = "aarch64")]
fn link_replay_macho_keeps_names_and_linkage() {
    for unit in units() {
        let (record, bytes) = replayed(&unit, Format::MachO);
        check_linkage(&record, &bytes, Format::MachO);
    }
}

/// Whether a relocation in text is PC-relative, as D-913 allows there:
/// calls, pages and page offsets on aarch64, `rel32` on x86-64.
fn pc_relative(flags: RelocationFlags) -> bool {
    match flags {
        RelocationFlags::Elf { r_type } => matches!(
            r_type,
            elf::R_AARCH64_ADR_PREL_PG_HI21
                | elf::R_AARCH64_ADD_ABS_LO12_NC
                | elf::R_AARCH64_CALL26
                | elf::R_X86_64_PC32
                | elf::R_X86_64_PLT32
        ),
        // Mach-O is aarch64 only (D-892).
        RelocationFlags::MachO { r_type, .. } => matches!(
            r_type,
            macho::ARM64_RELOC_PAGE21 | macho::ARM64_RELOC_PAGEOFF12 | macho::ARM64_RELOC_BRANCH26
        ),
        _ => false,
    }
}

/// Text holds only PC-relative relocations (D-913); ELF objects carry
/// `.note.GNU-stack`, so linkers keep the stack non-executable; a data
/// object with relocations is never zero-fill, which no format relocates.
#[test]
fn link_replay_text_is_pc_relative_and_stack_is_noexec() {
    for &format in Format::ALL {
        let (_, bytes) = replayed(&code_model_unit(), format);
        let file = ::object::File::parse(&*bytes).unwrap();
        let mut text_relocs = 0;
        for section in file.sections() {
            let relocs: Vec<_> = section.relocations().collect();
            if section.kind() == SectionKind::Text {
                for (offset, reloc) in &relocs {
                    assert!(
                        pc_relative(reloc.flags()),
                        "{format:?}: {:?} at {offset}",
                        reloc.flags()
                    );
                    text_relocs += 1;
                }
            }
            if !relocs.is_empty() {
                assert!(
                    !matches!(
                        section.kind(),
                        SectionKind::UninitializedData | SectionKind::UninitializedTls
                    ),
                    "{format:?}: relocations in {:?}",
                    section.name()
                );
            }
        }
        assert!(text_relocs > 0, "{format:?}");
        let note = file.section_by_name(".note.GNU-stack");
        assert_eq!(note.is_some(), format == Format::Elf, "{format:?}");
    }
}

/// Two replays of one record give identical bytes, and so do the replays
/// of two records of one unit compiled twice (D-264); the object names its
/// module path, never a file path.
#[test]
fn link_replay_is_byte_stable() {
    for &format in Format::ALL {
        let unit = code_model_unit();
        let (record, first) = replayed(&unit, format);
        let again = replay(&record, format.isa(), "app").unwrap();
        assert_eq!(first, again, "{format:?}");
        let (_, recompiled) = replayed(&unit.clone(), format);
        assert_eq!(first, recompiled, "{format:?}");
        let file = ::object::File::parse(&*first).unwrap();
        if format == Format::Elf {
            let files: Vec<_> = file
                .symbols()
                .filter(|s| s.kind() == ::object::SymbolKind::File)
                .map(|s| s.name().unwrap().to_owned())
                .collect();
            assert_eq!(files, ["app"]);
        }
        let cwd = std::env::current_dir().unwrap();
        let cwd = cwd.to_string_lossy();
        assert!(
            !first.windows(cwd.len()).any(|w| w == cwd.as_bytes()),
            "{format:?} holds {cwd}"
        );
    }
}

/// Replay refuses, without panicking, a malformed record, a relocation of
/// another architecture, and a target whose objects M3 does not write.
#[test]
fn link_replay_refuses_what_it_cannot_write() {
    let format = Format::Elf;
    let record = record_for(&code_model_unit(), format);
    let with_reloc = |change: &dyn Fn(&mut RecordReloc)| {
        let mut bad = record.clone();
        let function = bad
            .functions
            .iter_mut()
            .find(|f| !f.relocs.is_empty())
            .unwrap();
        change(&mut function.relocs[0]);
        bad
    };
    let out_of_range = with_reloc(&|r| r.symbol = u32::MAX);
    assert!(matches!(
        replay(&out_of_range, format.isa(), "app"),
        Err(LinkError::Record(RecordError::Malformed(_)))
    ));
    let past_end = with_reloc(&|r| r.offset = u32::MAX - 2);
    assert!(matches!(
        replay(&past_end, format.isa(), "app"),
        Err(LinkError::Record(RecordError::Malformed(_)))
    ));
    let other_arch = with_reloc(&|r| {
        r.kind = if std::env::consts::ARCH == "aarch64" {
            cranelift_codegen::binemit::Reloc::X86PCRel4
        } else {
            cranelift_codegen::binemit::Reloc::Arm64Call
        };
    });
    assert!(matches!(
        replay(&other_arch, format.isa(), "app"),
        Err(LinkError::Record(RecordError::Malformed(_)))
    ));
    // An absolute relocation in text (D-913), within the code.
    let absolute = with_reloc(&|r| {
        r.kind = cranelift_codegen::binemit::Reloc::Abs8;
        r.offset = 0;
    });
    assert!(matches!(
        replay(&absolute, format.isa(), "app"),
        Err(LinkError::Record(RecordError::Malformed(_)))
    ));
    let windows = format!("{}-pc-windows-msvc", std::env::consts::ARCH);
    let result = replay(&record, isa::isa_for(&windows).unwrap(), "app");
    assert!(matches!(result, Err(LinkError::Object(_))), "{result:?}");
    assert!(result.unwrap_err().to_string().contains("windows"));
}

/// A huge alignment is refused as malformed, never passed to the object
/// writer, which pads its section up to it: 2^63 panicked there with
/// "capacity overflow" and 2^55 aborted the process on the allocation
/// (wave 2 review, SND-2).
#[test]
fn link_replay_refuses_huge_alignment() {
    for &format in Format::ALL {
        let record = record_for(&code_model_unit(), format);
        for align in [1 << 55, 1 << 63] {
            let mut function = record.clone();
            let last = function.functions.len() - 1;
            function.functions[last].align = align;
            let mut data = record.clone();
            data.data[0].align = Some(align);
            for bad in [function, data] {
                let result = replay(&bad, format.isa(), "app");
                assert!(
                    matches!(
                        result,
                        Err(LinkError::Record(RecordError::Malformed(
                            "an alignment too large"
                        )))
                    ),
                    "{format:?} {align}: {result:?}"
                );
            }
        }
    }
}

/// A small xorshift generator, so the mutations are the same every run.
struct Mutator(u64);

impl Mutator {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n.max(1)).unwrap()).unwrap()
    }

    fn byte(&mut self) -> u8 {
        self.next().to_le_bytes()[0]
    }

    /// One to four random edits of `bytes`: a flipped bit, a random byte,
    /// a planted 1-, 4- or 8-byte value chosen to hit counts, lengths and
    /// alignments, a truncation, an insertion or a removal.
    fn mutate(&mut self, bytes: &mut Vec<u8>) {
        const VALUES: [u64; 13] = [
            0,
            1,
            2,
            3,
            4,
            8,
            15,
            16,
            1 << 31,
            1 << 32,
            1 << 55,
            1 << 63,
            u64::MAX,
        ];
        for _ in 0..=self.below(4) {
            if bytes.is_empty() {
                return;
            }
            let at = self.below(bytes.len());
            match self.below(6) {
                0 => bytes[at] ^= 1 << self.below(8),
                1 => bytes[at] = self.byte(),
                2 => {
                    let value = VALUES[self.below(VALUES.len())].to_le_bytes();
                    let width = [1, 4, 8][self.below(3)];
                    for (slot, b) in bytes.iter_mut().skip(at).zip(&value[..width]) {
                        *slot = *b;
                    }
                }
                3 => bytes.truncate(at),
                4 => bytes.insert(at, self.byte()),
                _ => {
                    bytes.remove(at);
                }
            }
        }
    }
}

/// Neither decode nor replay panics on a corrupt record (D-422): random
/// mutations of real records' encodings either fail to decode or decode
/// to a record whose replay returns bytes or an error. Every replay also
/// gets a planted huge alignment's chance through the mutator's values.
#[test]
fn link_replay_mutated_records_never_panic() {
    let format = if cfg!(target_os = "macos") {
        Format::MachO
    } else {
        Format::Elf
    };
    let seeds: Vec<Vec<u8>> = units()
        .iter()
        .map(|unit| record_for(unit, format).encode())
        .collect();
    let mut mutator = Mutator(0x9e37_79b9_7f4a_7c15);
    let (mut decoded, mut replayed) = (0_u32, 0_u32);
    for i in 0..20_000 {
        let mut bytes = seeds[mutator.below(seeds.len())].clone();
        mutator.mutate(&mut bytes);
        let outcome = std::panic::catch_unwind(|| {
            let record = CodeRecord::decode(&bytes).ok()?;
            Some(replay(&record, format.isa(), "app").is_ok())
        });
        let Ok(outcome) = outcome else {
            panic!("mutation {i} panicked: {bytes:?}");
        };
        if let Some(ok) = outcome {
            decoded += 1;
            replayed += u32::from(ok);
        }
    }
    // Code bytes and addends are free: many mutations still decode, and
    // most of those replay.
    assert!(decoded > 100 && replayed > 100, "{decoded} {replayed}");
}
