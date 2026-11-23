//! D-913's code model and D-921's fixed ISA: text holds only PC-relative
//! relocations to generated symbols, runtime and foreign symbols are named
//! only by import words' `Abs8` data relocations, no libcall appears, and
//! the ISA's flags are a constant of the target, never of the machine.
#![expect(
    clippy::wildcard_enum_match_arm,
    reason = "a test matches only the variants it checks"
)]

use cranelift_codegen::binemit::Reloc;
use cranelift_codegen::ir::{self, LibCall};
use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_module::{
    DataDescription, DataId, FuncId, Linkage, ModuleDeclarations, ModuleRelocTarget, ModuleResult,
};

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, Ice, IndexVec, ModuleId};
use cheby_mir::{
    Abi, AbiSig, ArithOp, Block, BlockId, CType, Callee, CalleeDecl, CmpOp, Const, DefName,
    FieldDesc, FnKey, ForeignSig, Function, LayoutDesc, LayoutRef, Local, LocalDecl, Operand, Repr,
    Rvalue, Stmt, Terminator, UnitKind,
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
        Self::with_isa(isa::host_isa().unwrap())
    }

    fn with_isa(isa: OwnedTargetIsa) -> Self {
        Self {
            isa,
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
        externals: Vec::new(),
        source: None,
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
        // allocation's.
        "cb$imp$cheby_rt_stack_slow",
        "cb$imp$cheby_rt_alloc_slow",
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
        srclocs: Vec::new(),
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

/// macOS is aarch64 only (D-892): `x86_64-apple-darwin` is not one of
/// the fixed targets (D-921), so asking for it is the unsupported-host
/// error, never a panic, and the error names the five targets.
#[test]
fn isa_x86_64_apple_darwin_is_unsupported() {
    match isa::isa_for("x86_64-apple-darwin") {
        Err(CodegenError::UnsupportedHost(message)) => {
            assert!(message.starts_with("x86_64-apple-darwin: "), "{message}");
            assert!(
                message.contains(
                    "x86-64 on Linux and Windows, and aarch64 on Linux, macOS and Windows"
                ),
                "{message}"
            );
        }
        Err(other) => panic!("not the unsupported-host error: {other}"),
        Ok(_) => panic!("x86_64-apple-darwin has an ISA"),
    }
}

/// `app::spin(k: Int, cr, ci, zr, zi: Float)`, the shape of the
/// comparison's `escape_time`: while `k != 0`, tail-calls itself as
/// `spin(k - 1, cr, ci, zr * zr - zi * zi + cr, 2.0 * zr * zi + ci)`; at
/// 0 returns `Nil`. The self tail call is a jump to the loop header,
/// whose entry check's slow path is the loop's only call.
///
/// With `allocates`, `spin` also carries a uniform word `acc` and each
/// step makes `acc` the pair of the old `acc` and the new `zr` boxed, so
/// the loop allocates twice per step, a box and a tuple.
#[expect(clippy::too_many_lines, reason = "one hand-built MIR unit")]
fn float_loop_unit(allocates: bool) -> MirModule {
    let spin = def(0);
    let local = Local::from_u32;
    let float = |n: f64| Operand::Const(Const::Float(n.to_bits()));
    let arith = |dst: u32, op: ArithOp, lhs: Operand, rhs: Operand| Stmt::Assign {
        dst: local(dst),
        value: Rvalue::FloatArith {
            op,
            lhs,
            rhs,
            repr: Repr::Float,
        },
    };
    let l = |n: u32| Operand::Local(local(n));
    let mut params: Vec<Local> = (0..5).map(local).collect();
    if allocates {
        params.push(local(14));
    }
    let mut body = function(
        FnKey::Item(spin),
        params,
        &[
            // The parameters, then the test of `k`, the steps and `k - 1`.
            Repr::Int,
            Repr::Float,
            Repr::Float,
            Repr::Float,
            Repr::Float,
            Repr::Bool,
            Repr::Float,
            Repr::Float,
            Repr::Float,
            Repr::Float,
            Repr::Float,
            Repr::Float,
            Repr::Float,
            Repr::Int,
            // `acc`, the box and the pair.
            Repr::Value,
            Repr::Value,
            Repr::Value,
        ],
        Vec::new(),
        Terminator::Return(Operand::Const(Const::Nil)),
        &["a.cheby:1:4", "a.cheby:2:8"],
    );
    let mut step = vec![l(13), l(1), l(2), l(9), l(12)];
    let mut allocations = Vec::new();
    if allocates {
        allocations = vec![
            Stmt::Assign {
                dst: local(15),
                value: Rvalue::Box {
                    value: l(9),
                    from: Repr::Float,
                },
            },
            Stmt::Assign {
                dst: local(16),
                value: Rvalue::Alloc {
                    layout: LayoutRef::Tuple(2),
                    fields: vec![l(14), l(15)],
                },
            },
        ];
        step.push(l(16));
    }
    let blocks = vec![
        Block {
            pos: Vec::new(),
            params: Vec::new(),
            stmts: vec![Stmt::Assign {
                dst: local(5),
                value: Rvalue::Compare {
                    op: CmpOp::Eq,
                    lhs: l(0),
                    rhs: Operand::Const(Const::Int(0)),
                    repr: Repr::Int,
                },
            }],
            term: Terminator::Branch {
                cond: l(5),
                then_: BlockId::from_u32(1),
                else_: BlockId::from_u32(2),
            },
            unwind: Vec::new(),
        },
        Block {
            pos: Vec::new(),
            params: Vec::new(),
            stmts: Vec::new(),
            term: Terminator::Return(Operand::Const(Const::Nil)),
            unwind: Vec::new(),
        },
        Block {
            pos: Vec::new(),
            params: Vec::new(),
            stmts: vec![
                arith(6, ArithOp::Mul, l(3), l(3)),
                arith(7, ArithOp::Mul, l(4), l(4)),
                arith(8, ArithOp::Sub, l(6), l(7)),
                arith(9, ArithOp::Add, l(8), l(1)),
                arith(10, ArithOp::Mul, float(2.0), l(3)),
                arith(11, ArithOp::Mul, l(10), l(4)),
                arith(12, ArithOp::Add, l(11), l(2)),
                Stmt::Assign {
                    dst: local(13),
                    value: Rvalue::Checked {
                        op: ArithOp::Sub,
                        lhs: l(0),
                        rhs: Operand::Const(Const::Int(1)),
                        repr: Repr::Int,
                        loc: StrId::from_u32(1),
                    },
                },
            ]
            .into_iter()
            .chain(allocations)
            .collect(),
            term: Terminator::TailCall {
                callee: Callee::Direct(spin),
                args: step,
            },
            unwind: Vec::new(),
        },
    ];
    body.blocks = IndexVec::from_raw(blocks).unwrap();
    let float = Repr::Float;
    let mut sig = vec![Repr::Int, float, float, float, float];
    if allocates {
        sig.push(Repr::Value);
    }
    MirModule {
        externals: Vec::new(),
        source: None,
        unit: UnitKind::Code(ModuleId::from_u32(1)),
        path: "app".into(),
        functions: vec![body],
        callees: vec![CalleeDecl {
            def: spin,
            sig: AbiSig::owned(Abi::Cheby, sig, Repr::Value),
        }],
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: vec![DefName {
            def: spin,
            module: "app".into(),
            item: "spin".into(),
        }],
    }
}

/// `float_loop_unit`'s `spin` compiled for `isa` at `opt`, with the slow
/// paths called with `preserve_all` when `saving` (and the target has
/// the stubs): its CLIF text and the size of its frame below the frame
/// record, its spill slots and saved callee-saved registers.
fn float_loop(
    isa: OwnedTargetIsa,
    opt: OptLevel,
    saving: bool,
    allocates: bool,
) -> (String, u32, String) {
    let mut m = Recording::with_isa(isa);
    let decls = Decls::new(m.isa()).unwrap();
    let mut decls = if saving {
        decls
    } else {
        decls.without_saving_slow_paths()
    };
    let cx = CodegenCx {
        layouts: &PairLayout,
    };
    let opts = CodegenOptions {
        opt_level: opt,
        ..CodegenOptions::default()
    };
    let units = [float_loop_unit(allocates)];
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    let func = translate(&units[0], &units[0].functions[0], &decls, &cx, &opts).unwrap();
    let text = func.display().to_string();
    let mut ctx = cranelift_codegen::Context::for_function(func);
    ctx.set_disasm(true);
    ctx.compile(m.isa(), &mut ControlPlane::default()).unwrap();
    let code = ctx.compiled_code().unwrap();
    let frame = code.buffer.frame_layout().unwrap().frame_to_fp_offset;
    (text, frame, code.vcode.clone().unwrap_or_default())
}

/// The targets whose Cranelift backend this build has (D-936): the
/// host's architecture.
fn built_targets() -> Vec<&'static str> {
    [
        "aarch64-apple-darwin",
        "aarch64-unknown-linux-gnu",
        "aarch64-pc-windows-msvc",
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
    ]
    .into_iter()
    .filter(|triple| isa::isa_for(triple).is_ok())
    .collect()
}

/// The entry check's slow paths save every register (`cheby_runtime::panic`)
/// and are called with `preserve_all` on aarch64 and x86-64 outside
/// Windows, so the Floats a self tail call's loop carries stay in
/// registers: the loop's function has no frame below its frame record,
/// no spill slot and no saved register, in both profiles. Called with the
/// C convention, as on Windows, the same loop keeps them in spill slots,
/// since the call clobbers every vector register.
#[test]
fn a_float_loop_keeps_its_floats_in_registers() {
    let targets = built_targets();
    assert_ne!(targets.len(), 0, "no backend built");
    for triple in targets {
        let windows = triple.contains("windows");
        for opt in [OptLevel::None, OptLevel::Speed] {
            let isa = || isa::isa_with(triple, opt).unwrap();
            let (text, frame, _) = float_loop(isa(), opt, true, false);
            let context = format!("{triple} at {opt:?}");
            assert_eq!(
                text.contains("preserve_all"),
                !windows,
                "{context}:\n{text}"
            );
            if !windows {
                assert_eq!(frame, 0, "{context}: a frame of {frame} bytes\n{text}");
            }
            let (text, frame, _) = float_loop(isa(), opt, false, false);
            assert!(!text.contains("preserve_all"), "{context}:\n{text}");
            assert!(frame > 0, "{context}: with the C convention, no frame");
        }
    }
}

/// Whether a line of disassembly stores a vector register to the stack or
/// loads one from it: a spill or a reload.
fn float_spill(line: &str) -> bool {
    let line = line.trim();
    let vector = |word: &str| {
        let word = word.trim_start_matches('%').trim_end_matches(',');
        ["d", "q", "v", "s", "xmm"].iter().any(|prefix| {
            word.strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        })
    };
    let stack = line.contains("[sp") || line.contains("(%rsp)") || line.contains("(%rbp)");
    stack && line.split_whitespace().skip(1).any(vector)
}

/// An allocation of a cached size is inline (`crate::alloc_inline`), and
/// its slow path is a stub called with `preserve_all` on aarch64 and
/// x86-64 outside Windows, so a loop that boxes a `Float` and allocates a
/// tuple at every step makes no call on the hit path and keeps its Floats
/// in registers: no frame below its frame record, in both profiles. Every
/// call it has is to a stub. Where the stubs are called with the C
/// convention, as on Windows, the loop calls the allocation helpers and
/// keeps its Floats in spill slots.
#[test]
fn an_allocating_float_loop_keeps_its_floats_in_registers() {
    let targets = built_targets();
    assert_ne!(targets.len(), 0, "no backend built");
    for triple in targets {
        let windows = triple.contains("windows");
        for opt in [OptLevel::None, OptLevel::Speed] {
            let isa = || isa::isa_with(triple, opt).unwrap();
            let (text, frame, code) = float_loop(isa(), opt, true, true);
            let context = format!("{triple} at {opt:?}");
            // The allocation helpers return the object; the stubs, and
            // the overflow's panic, after which nothing is live, return
            // nothing.
            let helpers: Vec<&str> = text
                .lines()
                .filter(|line| line.trim_start().starts_with("sig") && line.contains("->"))
                .collect();
            if windows {
                assert!(!text.contains("preserve_all"), "{context}:\n{text}");
                assert!(!helpers.is_empty(), "{context}:\n{text}");
                continue;
            }
            assert!(
                helpers.is_empty() && text.contains("preserve_all"),
                "{context}: an allocation helper's call\n{text}"
            );
            // A callee-saved integer register may be saved once at the
            // entry; no vector register goes to or comes from the stack.
            let spills: Vec<&str> = code.lines().filter(|line| float_spill(line)).collect();
            assert!(
                spills.is_empty(),
                "{context}: {spills:?} in a frame of {frame} bytes\n{code}"
            );
            let (text, frame, code) = float_loop(isa(), opt, false, true);
            assert!(!text.contains("preserve_all"), "{context}:\n{text}");
            assert!(frame > 0, "{context}: with the C convention, no frame");
            assert!(
                code.lines().any(float_spill),
                "{context}: with the C convention, no spill\n{code}"
            );
        }
    }
}
