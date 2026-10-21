//! D-913's code model and D-921's fixed ISA: text holds only PC-relative
//! relocations to generated symbols, runtime and foreign symbols are named
//! only by import words' `Abs8` data relocations, no libcall appears, and
//! the ISA's flags are a constant of the target, never of the machine.

use cranelift_codegen::binemit::Reloc;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::{self, LibCall};
use cranelift_codegen::isa::{OwnedTargetIsa, TargetIsa};
use cranelift_module::{
    DataDescription, DataId, FuncId, Linkage, Module, ModuleDeclarations, ModuleReloc,
    ModuleRelocTarget, ModuleResult,
};

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, Ice, IndexVec, ModuleId};
use cheby_mir::{
    Abi, AbiSig, Block, CType, Callee, CalleeDecl, Const, DefName, FieldDesc, FnKey, ForeignSig,
    Function, LayoutDesc, LayoutRef, Local, LocalDecl, MirModule, Operand, Repr, Rvalue, Stmt,
    StrId, Terminator, UnitKind,
};

use crate::*;

/// A module that records what is defined in it: each function's text
/// relocations and each data object's relocations, in definition order.
struct Recording {
    isa: OwnedTargetIsa,
    declarations: ModuleDeclarations,
    text: Vec<(FuncId, Vec<ModuleReloc>)>,
    data: Vec<(DataId, Vec<ModuleReloc>)>,
}

impl Recording {
    fn new() -> Self {
        Self {
            isa: isa::host_isa().unwrap(),
            declarations: ModuleDeclarations::default(),
            text: Vec::new(),
            data: Vec::new(),
        }
    }

    /// The symbol a relocation names, and its linkage.
    fn target(&self, target: &ModuleRelocTarget) -> (String, Linkage) {
        match *target {
            ModuleRelocTarget::User {
                namespace: 0,
                index,
            } => {
                let decl = self.declarations.get_function_decl(FuncId::from_u32(index));
                (decl.name.clone().unwrap(), decl.linkage)
            }
            ModuleRelocTarget::User {
                namespace: 1,
                index,
            } => {
                let decl = self.declarations.get_data_decl(DataId::from_u32(index));
                (decl.name.clone().unwrap(), decl.linkage)
            }
            ref other => panic!("an unexpected relocation target {other}"),
        }
    }

    fn func_name(&self, id: FuncId) -> String {
        self.declarations
            .get_function_decl(id)
            .name
            .clone()
            .unwrap()
    }

    fn data_name(&self, id: DataId) -> String {
        self.declarations.get_data_decl(id).name.clone().unwrap()
    }
}

impl Module for Recording {
    fn isa(&self) -> &dyn TargetIsa {
        &*self.isa
    }

    fn declarations(&self) -> &ModuleDeclarations {
        &self.declarations
    }

    fn declare_function(
        &mut self,
        name: &str,
        linkage: Linkage,
        signature: &ir::Signature,
    ) -> ModuleResult<FuncId> {
        Ok(self
            .declarations
            .declare_function(name, linkage, signature)?
            .0)
    }

    fn declare_anonymous_function(&mut self, signature: &ir::Signature) -> ModuleResult<FuncId> {
        self.declarations.declare_anonymous_function(signature)
    }

    fn declare_data(
        &mut self,
        name: &str,
        linkage: Linkage,
        writable: bool,
        tls: bool,
    ) -> ModuleResult<DataId> {
        Ok(self
            .declarations
            .declare_data(name, linkage, writable, tls)?
            .0)
    }

    fn declare_anonymous_data(&mut self, writable: bool, tls: bool) -> ModuleResult<DataId> {
        self.declarations.declare_anonymous_data(writable, tls)
    }

    /// Code generation defines bytes; this is never called.
    fn define_function_with_control_plane(
        &mut self,
        func: FuncId,
        _ctx: &mut cranelift_codegen::Context,
        _ctrl_plane: &mut ControlPlane,
    ) -> ModuleResult<()> {
        Err(cranelift_module::ModuleError::InvalidImportDefinition(
            self.func_name(func),
        ))
    }

    fn define_function_bytes(
        &mut self,
        func: FuncId,
        _alignment: u64,
        _bytes: &[u8],
        relocs: &[ModuleReloc],
    ) -> ModuleResult<()> {
        self.text.push((func, relocs.to_vec()));
        Ok(())
    }

    fn define_data(&mut self, data: DataId, desc: &DataDescription) -> ModuleResult<()> {
        self.data
            .push((data, desc.all_relocs(Reloc::Abs8).collect()));
        Ok(())
    }
}

/// Layout ids for the code-model unit: the builtin ones and `app::Pair`.
struct PairLayout;

impl LayoutIds for PairLayout {
    fn id(&self, layout: &LayoutRef) -> Result<u32, Ice> {
        Ok(builtin_layout_id(layout).unwrap_or(1000))
    }
}

fn def(index: u32) -> DefId {
    DefId::new(ModuleId::from_u32(1), DefIndex::from_u32(index))
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
        call_locs: Vec::new(),
        call_loc_strings: IndexVec::new(),
    }
}

/// `app::f(x)`, which calls the runtime (its stack check's slow path and
/// the allocation), calls the foreign `cheby_test_kernel(x)` and the
/// `@blocking` foreign `cheby_test_blocking(1)`, calls `app::g()` through
/// its slot, and returns `Pair("hi", Nil)`: a string, a runtime static and
/// a user constructor's layout word. `app::g()` returns `Nil`.
#[expect(clippy::too_many_lines, reason = "one hand-built MIR unit")]
fn code_model_unit() -> MirModule {
    let (f, g, pair, pair_type) = (def(0), def(1), def(2), def(3));
    let local = Local::from_u32;
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
    );
    let g_body = function(
        FnKey::Item(g),
        Vec::new(),
        &[],
        Vec::new(),
        Terminator::Return(Operand::Const(Const::Nil)),
        &["a.cheby:2:4"],
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

/// The relocation kinds D-913 leaves in text on this build's target.
fn text_kinds() -> &'static [Reloc] {
    if cfg!(target_arch = "aarch64") {
        &[
            Reloc::Aarch64AdrPrelPgHi21,
            Reloc::Aarch64AddAbsLo12Nc,
            Reloc::Arm64Call,
        ]
    } else {
        // X0's caveat 3: `lea` of colocated data is `X86CallPCRel4` too.
        &[Reloc::X86CallPCRel4, Reloc::X86PCRel4]
    }
}

/// Compiles the code-model unit into a recording module, with every
/// function split into an outer function and its `cb$big$` body when
/// `split` (a frame threshold of 0, D-679).
fn compile(split: bool) -> Recording {
    let mut m = Recording::new();
    let mut decls = Decls::new(m.isa()).unwrap();
    let cx = CodegenCx {
        layouts: &PairLayout,
    };
    let opts = CodegenOptions {
        large_frame_threshold: if split { 0 } else { LARGE_FRAME_THRESHOLD },
        ..CodegenOptions::default()
    };
    let units = [code_model_unit()];
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    define_with_stats(&mut m, &units, &mut decls, &cx, &opts).unwrap();
    m
}

/// Every relocation in text is PC-relative, of the kinds of the
/// architecture, and names a generated symbol (`cb$…`, never imported);
/// the unit's text reaches the runtime, the foreign symbols and every
/// generated object it uses that way.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_model_text_is_pc_relative_to_generated_symbols() {
    let m = compile(false);
    let mut kinds = std::collections::BTreeSet::new();
    let mut named = std::collections::BTreeSet::new();
    for (func, relocs) in &m.text {
        for reloc in relocs {
            let (name, linkage) = m.target(&reloc.name);
            assert!(
                text_kinds().contains(&reloc.kind),
                "{:?} to `{name}` in `{}`",
                reloc.kind,
                m.func_name(*func)
            );
            assert!(
                name.starts_with(mangle::PREFIX) && linkage != Linkage::Import,
                "`{}` names `{name}` ({linkage:?}) from its text",
                m.func_name(*func)
            );
            kinds.insert(format!("{:?}", reloc.kind));
            named.insert(name);
        }
    }
    for symbol in [
        // A runtime function, twice: the entry check's slow path and the
        // allocation.
        "cb$imp$cheby_rt_stack_slow",
        "cb$imp$cheby_rt_alloc",
        // `@blocking`'s runtime function and the shim's address.
        "cb$imp$cheby_rt_blocking_call",
        "cb$blk$cheby_test_blocking",
        // A runtime static.
        "cb$imp$cheby_rt_static_nil",
        // Foreign symbols: called directly, and by the blocking shim.
        "cb$imp$cheby_test_kernel",
        "cb$imp$cheby_test_blocking",
        // A string and a layout word.
        "cb$str$fn$app$f$1",
        "cb$layout$app$Pair",
        // Another function, through its slot (D-043, D-916).
        "cb$slot$app$g",
    ] {
        assert!(named.contains(symbol), "{symbol} in {named:?}");
    }
    // No function is called directly: calls between Cheby functions go
    // through slots and runtime calls through import words.
    assert!(!kinds.contains("Arm64Call"), "{kinds:?}");
    if cfg!(target_arch = "aarch64") {
        assert_eq!(
            kinds.into_iter().collect::<Vec<_>>(),
            ["Aarch64AddAbsLo12Nc", "Aarch64AdrPrelPgHi21"]
        );
    } else {
        // Calls and `lea` of data alike (X0's caveat 3).
        assert_eq!(kinds.into_iter().collect::<Vec<_>>(), ["X86CallPCRel4"]);
    }
}

/// The only direct calls are within a unit's own code: an outer function
/// of a large frame `return_call`s its `cb$big$` body (D-679), a
/// colocated `bl` on aarch64 and a `call`/`jmp` rel32 on x86-64.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_model_direct_calls_reach_only_large_frame_bodies() {
    let m = compile(true);
    let mut calls = Vec::new();
    for (func, relocs) in &m.text {
        for reloc in relocs {
            let (name, _) = m.target(&reloc.name);
            assert!(text_kinds().contains(&reloc.kind), "{:?}", reloc.kind);
            let direct = if cfg!(target_arch = "aarch64") {
                reloc.kind == Reloc::Arm64Call
            } else {
                name.starts_with("cb$big$")
            };
            if direct {
                calls.push((m.func_name(*func), name));
            }
        }
    }
    calls.sort();
    // `g`'s frame is empty, so only `f` is split.
    assert_eq!(
        calls,
        [("cb$fn$app$f".to_owned(), "cb$big$fn$app$f".to_owned())]
    );
}

/// `Abs8` relocations are only in data: one in each import word, to its
/// runtime or foreign symbol, and one in each slot, to its function.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn code_model_absolute_relocations_are_in_import_words_and_slots() {
    let m = compile(false);
    let mut words = Vec::new();
    for (data, relocs) in &m.data {
        let name = m.data_name(*data);
        if relocs.is_empty() {
            continue;
        }
        let [reloc] = relocs.as_slice() else {
            panic!("`{name}` has {} relocations", relocs.len());
        };
        assert_eq!(
            (reloc.kind, reloc.offset, reloc.addend),
            (Reloc::Abs8, 0, 0)
        );
        let (target, linkage) = m.target(&reloc.name);
        if let Some(symbol) = name.strip_prefix("cb$imp$") {
            assert_eq!((target.as_str(), linkage), (symbol, Linkage::Import));
            let decl = m.declarations.get_data_decl(*data);
            assert!(!decl.writable && decl.linkage == Linkage::Local, "{name}");
            words.push(symbol.to_owned());
        } else {
            // A slot holds its own function.
            let item = name.strip_prefix("cb$slot$").unwrap_or_else(|| {
                panic!("`{name}` holds `{target}`, but is neither a word nor a slot")
            });
            assert_eq!(target, format!("cb$fn${item}"));
            assert_eq!(linkage, Linkage::Local);
        }
    }
    for symbol in [
        "cheby_rt_alloc",
        "cheby_rt_blocking_call",
        "cheby_rt_static_nil",
        "cheby_test_blocking",
        "cheby_test_kernel",
    ] {
        assert!(words.iter().any(|w| w == symbol), "{symbol} in {words:?}");
    }
}

/// The check of [`define_with_stats`] rejects, naming the function, an
/// absolute relocation in text, a libcall and a PC-relative reference to
/// an imported symbol, before anything is defined.
#[test]
fn code_model_check_rejects_absolute_libcall_and_imported_targets() {
    let mut m = Recording::new();
    let mut decls = Decls::new(m.isa()).unwrap();
    let cx = CodegenCx {
        layouts: &PairLayout,
    };
    let units = [code_model_unit()];
    declare(&mut m, &mut decls, &units, &cx, &CodegenOptions::default()).unwrap();
    let f = decls.func(FnKey::Item(def(0))).unwrap().id;
    let generated = decls.slot("app::g", &units[0].callees[1].sig).unwrap().id;
    let imported = match m.get_name("cheby_rt_alloc") {
        Some(cranelift_module::FuncOrDataId::Func(id)) => id,
        other => panic!("{other:?}"),
    };
    let code = |kind, name| CompiledFunction {
        bytes: vec![0; 16],
        align: FUNCTION_ALIGN,
        relocs: vec![ModuleReloc {
            offset: 4,
            kind,
            name,
            addend: 0,
        }],
        frame_size: 0,
        call_sites: Vec::new(),
        cheby_abi: true,
    };
    let pc = text_kinds()[0];
    let data = ModuleRelocTarget::user(1, generated.as_u32());
    check_text_relocs(&m, &decls, f, &code(pc, data.clone())).unwrap();
    for (kind, name, what) in [
        (Reloc::Abs8, data, "`Abs8` relocation to `cb$slot$app$g`"),
        (pc, ModuleRelocTarget::LibCall(LibCall::Memcpy), "libcall"),
        (
            pc,
            ModuleRelocTarget::user(0, imported.as_u32()),
            "`cheby_rt_alloc`, which only an import word may name",
        ),
    ] {
        let err = check_text_relocs(&m, &decls, f, &code(kind, name)).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("`cb$fn$app$f`") && text.contains(what),
            "{what}: {text}"
        );
    }
}

/// Libcalls are named so that nothing resolves them (D-913).
#[test]
fn code_model_libcalls_resolve_to_nothing() {
    let names = isa::libcall_names();
    assert_eq!(names(LibCall::Memcpy), "cb$libcall$Memcpy");
    assert_eq!(names(LibCall::CeilF64), "cb$libcall$CeilF64");
    assert_eq!(names(LibCall::Probestack), "cb$libcall$Probestack");
}

/// The ISA flags of this build's target (D-921), as Cranelift prints
/// them: the fixed feature set, every other flag off.
fn expected_isa_flags() -> &'static [&'static str] {
    if cfg!(target_arch = "x86_64") {
        &[
            "has_sse3=1",
            "has_ssse3=1",
            "has_cmpxchg16b=1",
            "has_sse41=1",
            "has_sse42=1",
            "has_avx=0",
            "has_avx2=0",
            "has_fma=0",
            "has_avx_vnni=0",
            "has_avx512bitalg=0",
            "has_avx512dq=0",
            "has_avx512vl=0",
            "has_avx512vbmi=0",
            "has_avx512f=0",
            "has_avx512vnni=0",
            "has_popcnt=1",
            "has_bmi1=0",
            "has_bmi2=0",
            "has_lzcnt=0",
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "has_lse=1",
            "has_pauth=1",
            "has_fp16=1",
            "has_dotprod=1",
            "has_i8mm=0",
            "sign_return_address_all=0",
            "sign_return_address=0",
            "sign_return_address_with_bkey=0",
            "use_bti=0",
            "use_csdb=0",
        ]
    } else {
        &[
            "has_lse=0",
            "has_pauth=0",
            "has_fp16=0",
            "has_dotprod=0",
            "has_i8mm=0",
            "sign_return_address_all=0",
            "sign_return_address=0",
            "sign_return_address_with_bkey=0",
            "use_bti=0",
            "use_csdb=0",
        ]
    }
}

fn isa_flags(isa: &dyn TargetIsa) -> Vec<String> {
    isa.isa_flags().iter().map(ToString::to_string).collect()
}

/// [`isa::host_isa`] gives the target's constant flags (D-921), the same
/// on every machine and every call, never the host's detected features;
/// return-address signing is off.
#[test]
fn isa_fixed_flags_are_a_constant_of_the_target() {
    let first = isa::host_isa().unwrap();
    assert_eq!(isa_flags(&*first), expected_isa_flags());
    let second = isa::host_isa().unwrap();
    assert_eq!(isa_flags(&*first), isa_flags(&*second));
    assert_eq!(first.flags().to_string(), second.flags().to_string());
}

/// The shared flags of D-913: no PIC, no colocated libcalls, frame
/// pointers, no unwind info, no optimization (D-259).
#[test]
fn isa_fixed_shared_flags_follow_the_code_model() {
    let isa = isa::host_isa().unwrap();
    let flags = isa.flags();
    assert!(!flags.is_pic());
    assert!(!flags.use_colocated_libcalls());
    assert!(flags.preserve_frame_pointers());
    assert!(!flags.unwind_info());
    assert!(!flags.enable_probestack());
    assert_eq!(
        flags.opt_level(),
        cranelift_codegen::settings::OptLevel::None
    );
}
