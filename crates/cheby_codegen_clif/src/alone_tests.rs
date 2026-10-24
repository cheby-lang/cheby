//! Units compiled alone (D-917, D-918, D-920): each unit's own definitions
//! are local or global hidden, what other units define is imported, code
//! and statics fixed by their key are local copies with the same bytes in
//! every unit, layout words hold no layout id, and slots of borrowing
//! functions carry their mask.

use std::collections::{BTreeMap, BTreeSet};

use cranelift_codegen::binemit::Reloc;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir;
use cranelift_codegen::isa::{OwnedTargetIsa, TargetIsa};
use cranelift_module::{
    DataDescription, DataId, FuncId, Init, Linkage, Module, ModuleDeclarations, ModuleReloc,
    ModuleRelocTarget, ModuleResult,
};

use cheby_diag::{FileId, Span, TextRange};
use cheby_iface::{DefId, DefIndex, Ice, IndexVec, ModuleId};
use cheby_mir::borrow::BorrowMask;
use cheby_mir::{
    Abi, AbiSig, Block, Callee, CalleeDecl, Const, DefName, EntryKind, FnKey, Function, LayoutDesc,
    LayoutRef, Local, LocalDecl, MirModule, Operand, Repr, Rvalue, StaticId, StaticKind,
    StaticObject, Stmt, StrId, Terminator, UnitKind,
};
use cheby_runtime::abi;

use crate::*;

/// A relocation with its target named: `(kind, offset, target, addend)`.
type NamedReloc = (Reloc, u32, String, i64);

/// One definition a recording module captured.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Captured {
    /// The machine code, or the data's bytes (zero-filled data as zeros).
    bytes: Vec<u8>,
    relocs: Vec<NamedReloc>,
}

/// A module that records every declaration and definition, as a code
/// record would hold them (D-914), with relocation targets by name.
struct Recording {
    isa: OwnedTargetIsa,
    declarations: ModuleDeclarations,
    /// `(symbol, is a function, definition)` in definition order.
    defined: Vec<(String, bool, Captured)>,
}

impl Recording {
    fn new() -> Self {
        Self {
            isa: isa::host_isa().unwrap(),
            declarations: ModuleDeclarations::default(),
            defined: Vec::new(),
        }
    }

    fn name_of(&self, target: &ModuleRelocTarget) -> String {
        match *target {
            ModuleRelocTarget::User {
                namespace: 0,
                index,
            } => self
                .declarations
                .get_function_decl(FuncId::from_u32(index))
                .name
                .clone()
                .unwrap(),
            ModuleRelocTarget::User {
                namespace: 1,
                index,
            } => self
                .declarations
                .get_data_decl(DataId::from_u32(index))
                .name
                .clone()
                .unwrap(),
            ref other => panic!("an unexpected relocation target {other}"),
        }
    }

    fn named(&self, relocs: impl Iterator<Item = ModuleReloc>) -> Vec<NamedReloc> {
        relocs
            .map(|r| (r.kind, r.offset, self.name_of(&r.name), r.addend))
            .collect()
    }

    /// Every declared symbol with its linkage, in declaration order:
    /// functions, then data objects.
    fn symbols(&self) -> Vec<(String, Linkage)> {
        let functions = self
            .declarations
            .get_functions()
            .map(|(_, d)| (d.name.clone().unwrap(), d.linkage));
        let data = self
            .declarations
            .get_data_objects()
            .map(|(_, d)| (d.name.clone().unwrap(), d.linkage));
        functions.chain(data).collect()
    }

    fn linkage(&self, symbol: &str) -> Linkage {
        self.symbols()
            .into_iter()
            .find(|(name, _)| name == symbol)
            .unwrap_or_else(|| panic!("`{symbol}` is not declared"))
            .1
    }

    fn definition(&self, symbol: &str) -> &Captured {
        &self
            .defined
            .iter()
            .find(|(name, _, _)| name == symbol)
            .unwrap_or_else(|| panic!("`{symbol}` is not defined"))
            .2
    }

    fn is_defined(&self, symbol: &str) -> bool {
        self.defined.iter().any(|(name, _, _)| name == symbol)
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
            self.name_of(&ModuleRelocTarget::from(func)),
        ))
    }

    fn define_function_bytes(
        &mut self,
        func: FuncId,
        _alignment: u64,
        bytes: &[u8],
        relocs: &[ModuleReloc],
    ) -> ModuleResult<()> {
        let name = self.name_of(&ModuleRelocTarget::from(func));
        let relocs = self.named(relocs.iter().cloned());
        self.defined.push((
            name,
            true,
            Captured {
                bytes: bytes.to_vec(),
                relocs,
            },
        ));
        Ok(())
    }

    fn define_data(&mut self, data: DataId, desc: &DataDescription) -> ModuleResult<()> {
        let name = self.name_of(&ModuleRelocTarget::from(data));
        let bytes = match &desc.init {
            Init::Bytes { contents } => contents.to_vec(),
            Init::Zeros { size } => vec![0; *size],
            Init::Uninitialized => {
                return Err(cranelift_module::ModuleError::InvalidImportDefinition(name));
            }
        };
        let relocs = self.named(desc.all_relocs(Reloc::Abs8));
        self.defined.push((name, false, Captured { bytes, relocs }));
        Ok(())
    }
}

const A: ModuleId = ModuleId::from_u32(1);
const B: ModuleId = ModuleId::from_u32(2);
const M: ModuleId = ModuleId::from_u32(3);

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

/// The dictionary both units use, `cb$dict$b$$b::Show$b::T` (D-579): the
/// shim's code word and `b::Empty`'s static.
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

fn compile_alone(unit: &MirModule) -> (Recording, UnitCode) {
    let mut m = Recording::new();
    let ucx = UnitCx {
        named_outside: &named_outside,
    };
    let code = compile_unit(&mut m, unit, &ucx, &CodegenOptions::default()).unwrap();
    (m, code)
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
        &[Reloc::X86CallPCRel4, Reloc::X86PCRel4]
    }
}

/// Checks the rules every unit alone follows, whatever its contents:
/// import words are local, read-only and defined; runtime symbols are
/// imports named only from import words' data; every local or hidden
/// symbol is defined and no imported one is; text has only PC-relative
/// relocations.
fn check_unit_rules(m: &Recording) {
    for (symbol, linkage) in m.symbols() {
        let defined = m.is_defined(&symbol);
        match linkage {
            Linkage::Import => assert!(!defined, "`{symbol}` is imported and defined"),
            Linkage::Local | Linkage::Hidden => {
                assert!(defined, "`{symbol}` ({linkage:?}) is never defined");
            }
            other => panic!("`{symbol}` has the linkage {other:?}"),
        }
        if symbol.starts_with("cb$imp$") {
            assert_eq!(linkage, Linkage::Local, "{symbol}");
        }
        if !symbol.starts_with(mangle::PREFIX) {
            assert_eq!(linkage, Linkage::Import, "{symbol}");
        }
    }
    for (symbol, is_function, captured) in &m.defined {
        for (kind, _, target, _) in &captured.relocs {
            if *is_function {
                assert!(
                    text_kinds().contains(kind),
                    "{kind:?} to `{target}` in `{symbol}`"
                );
                assert!(
                    target.starts_with(mangle::PREFIX) && !target.starts_with("cb$libcall$"),
                    "`{symbol}` names `{target}` from its text"
                );
            } else {
                assert_eq!(*kind, Reloc::Abs8, "`{symbol}` to `{target}`");
                if !target.starts_with(mangle::PREFIX) {
                    // A runtime symbol, named by an import word, or by a
                    // static object's field (D-1106).
                    assert_eq!(m.linkage(target), Linkage::Import, "{target}");
                }
            }
        }
    }
}

/// The linkage of every symbol each unit declares (D-917): own items that
/// another unit may name, their slots, layout words and `cb$ctor$`
/// statics are global hidden; a `priv` item, strings and the copies of
/// D-918 are local; another unit's slot, layout word and `cb$ctor$` static
/// are imports; import words are local in each unit.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn alone_linkage_of_every_symbol() {
    let (a, _) = compile_alone(&unit_a());
    let (b, _) = compile_alone(&unit_b());
    check_unit_rules(&a);
    check_unit_rules(&b);
    let expect = |m: &Recording, unit: &str, symbols: &[(&str, Linkage)]| {
        for &(symbol, linkage) in symbols {
            assert_eq!(m.linkage(symbol), linkage, "`{symbol}` in `{unit}`");
        }
        // Every other generated symbol is local.
        for (symbol, linkage) in m.symbols() {
            if symbol.starts_with(mangle::PREFIX) && !symbols.iter().any(|&(s, _)| s == symbol) {
                assert_eq!(linkage, Linkage::Local, "`{symbol}` in `{unit}`");
            }
        }
    };
    let copies = [
        ("cb$shim$b$get", Linkage::Local),
        ("cb$fnval$b$get", Linkage::Local),
        ("cb$dict$b$$b::Show$b::T", Linkage::Local),
    ];
    let mut in_b = vec![
        ("cb$fn$b$get", Linkage::Hidden),
        ("cb$slot$b$get", Linkage::Hidden),
        ("cb$fn$b$helper", Linkage::Local),
        ("cb$slot$b$helper", Linkage::Local),
        ("cb$layout$b$Empty", Linkage::Hidden),
        ("cb$ctor$b$Empty", Linkage::Hidden),
        ("cb$str$fn$b$get$1", Linkage::Local),
        ("cb$imp$cheby_rt_stack_slow", Linkage::Local),
    ];
    in_b.extend(copies);
    expect(&b, "b", &in_b);
    let mut in_a = vec![
        ("cb$fn$a$run", Linkage::Hidden),
        ("cb$slot$a$run", Linkage::Hidden),
        ("cb$slot$b$get", Linkage::Import),
        ("cb$layout$b$Empty", Linkage::Import),
        ("cb$ctor$b$Empty", Linkage::Import),
        ("cb$imp$cheby_rt_stack_slow", Linkage::Local),
    ];
    in_a.extend(copies);
    expect(&a, "a", &in_a);
    // `a` defines nothing of `b` but the copies and their own strings.
    let copy_strings = ["cb$str$shim$b$get$", "cb$str$dict$b$$"];
    for (symbol, _, _) in &a.defined {
        assert!(
            !symbol.contains("$b$")
                || copies.iter().any(|&(s, _)| s == symbol)
                || copy_strings.iter().any(|p| symbol.starts_with(p)),
            "`a` defines `{symbol}`"
        );
    }
    // A layout word and a `cb$ctor$` static are writable: an executable's
    // start patches them (D-920).
    for m in [&a, &b] {
        for symbol in ["cb$layout$b$Empty", "cb$ctor$b$Empty"] {
            let Some(cranelift_module::FuncOrDataId::Data(id)) = m.get_name(symbol) else {
                panic!("`{symbol}` is not data");
            };
            assert!(m.declarations.get_data_decl(id).writable, "{symbol}");
        }
    }
}

/// `a`'s text reaches `b`'s slot, and only PC-relative (D-913, D-917);
/// nothing reaches `b`'s function directly (D-916).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn alone_text_reaches_other_units_slots_pc_relative() {
    let (a, _) = compile_alone(&unit_a());
    let mut reached = Vec::new();
    for (symbol, is_function, captured) in &a.defined {
        for (kind, _, target, _) in &captured.relocs {
            if target == "cb$slot$b$get" {
                assert!(*is_function, "`{symbol}` holds `b`'s slot");
                assert!(text_kinds().contains(kind), "{kind:?}");
                reached.push(symbol.clone());
            }
            assert_ne!(target, "cb$fn$b$get", "`{symbol}` names `b`'s function");
        }
    }
    reached.sort();
    reached.dedup();
    // `run` and the shim's copy both load the slot.
    assert_eq!(reached, ["cb$fn$a$run", "cb$shim$b$get"]);
}

/// No data object of a unit alone holds a user layout id (D-920): the
/// layout word and the `cb$ctor$` static hold 0 where the id goes, and
/// every other header is a runtime kind's fixed id.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn alone_data_holds_no_layout_id() {
    let (a, _) = compile_alone(&unit_a());
    let (b, _) = compile_alone(&unit_b());
    let header = |c: &Captured| u64::from_le_bytes(c.bytes[..8].try_into().unwrap());
    let layout_word = header(b.definition("cb$layout$b$Empty"));
    assert_eq!(abi::header_layout(layout_word), 0);
    assert_eq!(layout_word, crate::decls::ctor_header(abi::RC_UNIQUE, 0, 0));
    let ctor = header(b.definition("cb$ctor$b$Empty"));
    assert_eq!(ctor, abi::header(abi::RC_STATIC, 0, 0));
    for m in [&a, &b] {
        for (symbol, is_function, captured) in &m.defined {
            let objects = ["cb$dict$", "cb$desc$", "cb$fnval$", "cb$str$"];
            if *is_function || !objects.iter().any(|p| symbol.starts_with(p)) {
                continue;
            }
            let layout = abi::header_layout(header(captured));
            assert!(
                layout != 0 && layout <= abi::layout::RUNTIME_RESERVED_END,
                "`{symbol}` has layout {layout}"
            );
        }
    }
}

/// The local copies of the shim, the function value and the dictionary
/// in `a` and `b` have the same bytes and relocations (D-918): a load set
/// or a link that meets both sees one content for one symbol.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn alone_copies_identical() {
    let (a, _) = compile_alone(&unit_a());
    let (b, _) = compile_alone(&unit_b());
    for symbol in ["cb$shim$b$get", "cb$fnval$b$get", "cb$dict$b$$b::Show$b::T"] {
        assert_eq!(a.definition(symbol), b.definition(symbol), "{symbol}");
        assert!(!a.definition(symbol).relocs.is_empty(), "{symbol}");
    }
    // The shim's own location string is a copy too.
    assert_eq!(
        a.definition("cb$str$shim$b$get$0"),
        b.definition("cb$str$shim$b$get$0")
    );
}

/// Compiling a unit twice gives the same declarations, bytes and
/// relocations (D-264), and the same code list.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn alone_compiles_the_same_twice() {
    for unit in [unit_a(), unit_b()] {
        let (first, first_code) = compile_alone(&unit);
        let (second, second_code) = compile_alone(&unit);
        assert_eq!(first.symbols(), second.symbols());
        assert_eq!(first.defined, second.defined);
        assert_eq!(first_code.defined, second_code.defined);
        assert_eq!(first_code.stats, second_code.stats);
    }
}

/// A unit alone asks for no layout id: its `LayoutIds` refuses. Since the
/// JIT registers through module records (D-919, D-920,
/// D-1169), the JIT's `define` of the same unit
/// asks for none either: with a table that refuses every id it defines
/// the unit, its layout word holding 0 where registration writes the id.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn alone_consults_no_layout_ids() {
    let (b, code) = compile_alone(&unit_b());
    assert!(b.is_defined("cb$layout$b$Empty"));
    assert_eq!(code.stats.functions, 3);
    let mut m = Recording::new();
    let mut decls = Decls::new(m.isa()).unwrap();
    let cx = CodegenCx { layouts: &Refuse };
    let units = [unit_b()];
    let opts = CodegenOptions::default();
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    define_with_stats(&mut m, &units, &mut decls, &cx, &opts).unwrap();
    let word = m.definition("cb$layout$b$Empty");
    let header = u64::from_le_bytes(word.bytes[..8].try_into().unwrap());
    assert_eq!(abi::header_layout(header), 0);
}

/// The JIT's mode keeps its linkage: one module holds both units, every
/// symbol is local, and `a` calls `b`'s slot, which `b` defines. Its
/// layout words hold 0 where the id goes, whatever the layout table says,
/// until registration writes them (D-920, D-1169).
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn alone_jit_mode_keeps_every_symbol_local() {
    let mut m = Recording::new();
    let mut decls = Decls::new(m.isa()).unwrap();
    let cx = CodegenCx {
        layouts: &BuiltinLayoutsAnd(77),
    };
    let units = [unit_b(), unit_a()];
    let opts = CodegenOptions::default();
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    define_with_stats(&mut m, &units, &mut decls, &cx, &opts).unwrap();
    for (symbol, linkage) in m.symbols() {
        let expected = if symbol.starts_with(mangle::PREFIX) {
            Linkage::Local
        } else {
            Linkage::Import
        };
        assert_eq!(linkage, expected, "{symbol}");
    }
    let word = m.definition("cb$layout$b$Empty");
    let header = u64::from_le_bytes(word.bytes[..8].try_into().unwrap());
    assert_eq!(abi::header_layout(header), 0);
}

/// Layout ids that are never given.
struct Refuse;

impl LayoutIds for Refuse {
    fn id(&self, _: &LayoutRef) -> Result<u32, Ice> {
        Err(Ice::new("asked"))
    }
}

/// Builtin layout ids, and `id` for every user constructor.
struct BuiltinLayoutsAnd(u32);

impl LayoutIds for BuiltinLayoutsAnd {
    fn id(&self, layout: &LayoutRef) -> Result<u32, Ice> {
        Ok(builtin_layout_id(layout).unwrap_or(self.0))
    }
}

/// `m::h(x)`, `priv`, borrows its parameter (D-715); `m::f(x)` calls it.
fn h() -> DefId {
    def(M, 0)
}
fn f() -> DefId {
    def(M, 1)
}
fn the_test() -> DefId {
    def(M, 2)
}

fn h_sig() -> AbiSig {
    AbiSig {
        borrow: BorrowMask::OWNED.with(0),
        ..AbiSig::owned(Abi::Cheby, vec![Repr::Value], Repr::Value)
    }
}

fn mask_names() -> Vec<DefName> {
    vec![
        name(h(), "m", "h"),
        name(f(), "m", "f"),
        name(the_test(), "m", "a test"),
    ]
}

fn mask_callees() -> Vec<CalleeDecl> {
    vec![
        CalleeDecl {
            def: h(),
            sig: h_sig(),
        },
        CalleeDecl {
            def: f(),
            sig: helper_sig(),
        },
    ]
}

/// Module `m`'s code unit: `h` borrows, `f(x)` calls `h(x)` borrowing.
fn mask_code_unit() -> MirModule {
    let mut h_body = function(
        FnKey::Item(h()),
        vec![local(0)],
        &[Repr::Value],
        Vec::new(),
        Terminator::Return(Operand::Const(Const::Nil)),
        &["m.cheby:1:1"],
    );
    h_body.borrow = h_sig().borrow;
    let f_body = function(
        FnKey::Item(f()),
        vec![local(0)],
        &[Repr::Value, Repr::Value],
        vec![call(
            1,
            Callee::Borrowing(h(), h_sig().borrow),
            vec![Operand::Local(local(0))],
        )],
        Terminator::Return(Operand::Local(local(1))),
        &["m.cheby:2:1"],
    );
    MirModule {
        externals: Vec::new(),
        source: None,
        unit: UnitKind::Code(M),
        path: "m".into(),
        functions: vec![h_body, f_body],
        callees: mask_callees(),
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: mask_names(),
    }
}

/// Module `m`'s tests unit: one test that calls `h(Nil)` borrowing.
fn mask_tests_unit() -> MirModule {
    let body = function(
        FnKey::Test(the_test()),
        Vec::new(),
        &[Repr::Value],
        vec![call(
            0,
            Callee::Borrowing(h(), h_sig().borrow),
            vec![Operand::Const(Const::Nil)],
        )],
        Terminator::Return(Operand::Local(local(0))),
        &["m.cheby:3:1"],
    );
    MirModule {
        externals: Vec::new(),
        source: None,
        unit: UnitKind::Tests(M),
        path: "m".into(),
        functions: vec![body],
        callees: vec![CalleeDecl {
            def: h(),
            sig: h_sig(),
        }],
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: mask_names(),
    }
}

/// The entries unit of a `cheby test` of `m`: the test's thunk, which
/// calls the test body directly.
fn mask_entries_unit() -> MirModule {
    let kind = EntryKind::Test(the_test());
    let mut thunk = function(
        FnKey::Entry(kind),
        Vec::new(),
        &[Repr::Value],
        vec![call(0, Callee::Code(FnKey::Test(the_test())), Vec::new())],
        Terminator::Return(Operand::Local(local(0))),
        &["m.cheby:3:1"],
    );
    thunk.abi = Abi::C;
    thunk.ret = cheby_mir::entry_sig(kind).ret;
    MirModule {
        externals: Vec::new(),
        source: None,
        unit: UnitKind::Entries,
        path: "".into(),
        functions: vec![thunk],
        callees: Vec::new(),
        layouts: Vec::new(),
        foreign_layouts: Vec::new(),
        statics: Vec::new(),
        names: vec![name(the_test(), "m", "a test")],
    }
}

/// D-917's suffix: `$$b` and the mask in lowercase hex without leading
/// zeros (D-1109); an all-owned slot keeps D-456's name.
#[test]
fn alone_slot_mask_names() {
    let n = name(h(), "m", "h");
    assert_eq!(mangle::slot(&n, BorrowMask::OWNED), "cb$slot$m$h");
    assert_eq!(mangle::slot(&n, BorrowMask(1)), "cb$slot$m$h$$b1");
    assert_eq!(mangle::slot(&n, BorrowMask(0x1a)), "cb$slot$m$h$$b1a");
    assert_eq!(
        mangle::slot(&n, BorrowMask(u64::MAX)),
        "cb$slot$m$h$$bffffffffffffffff"
    );
    // The masked slot of `m::h` is never the plain slot of an item `b1`
    // in the module `m::h` (wave 2 review, SND-1), whatever the masks.
    let nested = name(f(), "m::h", "b1");
    assert_eq!(mangle::slot(&nested, BorrowMask::OWNED), "cb$slot$m$h$b1");
    for mask in [0, 1, 0x1a, u64::MAX] {
        for other in [1, 0x1a, u64::MAX] {
            assert_ne!(
                mangle::slot(&nested, BorrowMask(mask)),
                mangle::slot(&n, BorrowMask(other)),
                "{mask:x} {other:x}"
            );
        }
    }
    // The suffix follows a REPL item's generation.
    let session = SessionName {
        module: "<repl:4>".into(),
        item: "h".into(),
        generation: 2,
        reuse: None,
    };
    assert_eq!(
        mangle::slot(&session.def_name(h()), BorrowMask(1)),
        "cb$slot$<repl>$h$r2$$b1"
    );
}

/// The JIT and units alone name a borrowing slot with its mask, and an
/// owned one without; the tests unit imports the borrowing slot under the
/// masked name, which the code unit defines hidden.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn alone_slot_mask_in_the_jit_and_in_units_alone() {
    let mut m = Recording::new();
    let mut decls = Decls::new(m.isa()).unwrap();
    let cx = CodegenCx {
        layouts: &BuiltinLayouts,
    };
    let units = [mask_code_unit(), mask_tests_unit()];
    let opts = CodegenOptions::default();
    declare(&mut m, &mut decls, &units, &cx, &opts).unwrap();
    define_with_stats(&mut m, &units, &mut decls, &cx, &opts).unwrap();
    let symbols = decls.symbols();
    assert!(symbols.contains(&"cb$slot$m$h$$b1"), "{symbols:?}");
    assert!(symbols.contains(&"cb$slot$m$f"), "{symbols:?}");
    assert!(!symbols.contains(&"cb$slot$m$h"), "{symbols:?}");
    // In the JIT, a session's borrowing slot keeps its generation first.
    let mut session = Decls::new(m.isa()).unwrap();
    session.set_session_name(
        h(),
        SessionName {
            module: "m".into(),
            item: "h".into(),
            generation: 3,
            reuse: None,
        },
    );
    let mut sm = Recording::new();
    declare(&mut sm, &mut session, &[mask_code_unit()], &cx, &opts).unwrap();
    assert!(session.symbols().contains(&"cb$slot$<repl>$h$r3$$b1"));

    // `h` is named by its module's tests unit, so it is hidden.
    let ucx = UnitCx {
        named_outside: &|d| d == h() || d == f() || d == the_test(),
    };
    let mut code = Recording::new();
    compile_unit(&mut code, &mask_code_unit(), &ucx, &opts).unwrap();
    assert_eq!(code.linkage("cb$slot$m$h$$b1"), Linkage::Hidden);
    assert_eq!(code.linkage("cb$slot$m$f"), Linkage::Hidden);
    let mut tests = Recording::new();
    compile_unit(&mut tests, &mask_tests_unit(), &ucx, &opts).unwrap();
    assert_eq!(tests.linkage("cb$slot$m$h$$b1"), Linkage::Import);
    assert_eq!(tests.linkage("cb$test$m$0"), Linkage::Hidden);
    check_unit_rules(&code);
    check_unit_rules(&tests);
}

/// The entries unit imports the test bodies it calls directly, the only
/// direct calls across units (D-916), named by the test's ordinal in its
/// tests unit; its thunks are local.
#[test]
#[cfg_attr(
    windows,
    ignore = "runs JIT-compiled code; Windows support is M6 (D-545)"
)]
fn alone_entries_import_test_bodies() {
    let ucx = UnitCx {
        named_outside: &|_| true,
    };
    let opts = CodegenOptions::default();
    let mut entries = Recording::new();
    compile_unit(&mut entries, &mask_entries_unit(), &ucx, &opts).unwrap();
    check_unit_rules(&entries);
    assert_eq!(entries.linkage("cb$test$m$0"), Linkage::Import);
    assert_eq!(entries.linkage("cb$entry$test$m$0"), Linkage::Local);
    let thunk = entries.definition("cb$entry$test$m$0");
    let calls: BTreeSet<&str> = thunk
        .relocs
        .iter()
        .filter(|(_, _, target, _)| target == "cb$test$m$0")
        .map(|(kind, _, _, _)| {
            assert!(text_kinds().contains(kind), "{kind:?}");
            "cb$test$m$0"
        })
        .collect();
    assert_eq!(calls.len(), 1);
    // Every function an entries unit defines is local; it imports only
    // what the tests unit defines hidden.
    let mut tests = Recording::new();
    compile_unit(&mut tests, &mask_tests_unit(), &ucx, &opts).unwrap();
    let imported: BTreeMap<String, Linkage> = entries
        .symbols()
        .into_iter()
        .filter(|(s, l)| s.starts_with(mangle::PREFIX) && *l == Linkage::Import)
        .collect();
    for symbol in imported.keys() {
        assert_eq!(tests.linkage(symbol), Linkage::Hidden, "{symbol}");
    }
}

/// In a unit alone, D-913's check accepts a PC-relative relocation in
/// text to another unit's generated symbol, an import, and still rejects
/// an absolute one, a libcall and a runtime symbol, which only an import
/// word may name; in the JIT an imported target stays an error.
#[test]
fn alone_check_accepts_only_generated_imports() {
    let unit = unit_a();
    let mut m = Recording::new();
    let mut decls = Decls::new_alone(m.isa(), [run()].into_iter().collect()).unwrap();
    let cx = CodegenCx { layouts: &Refuse };
    let units = std::slice::from_ref(&unit);
    declare(&mut m, &mut decls, units, &cx, &CodegenOptions::default()).unwrap();
    let f = decls.func(FnKey::Item(run())).unwrap().id;
    let slot = decls.slot_data("b::get", &get_sig()).unwrap();
    assert_eq!(m.declarations.get_data_decl(slot).linkage, Linkage::Import);
    let runtime = match m.get_name("cheby_rt_alloc") {
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
    let imported = ModuleRelocTarget::user(1, slot.as_u32());
    check_text_relocs(&m, &decls, f, &code(pc, imported.clone())).unwrap();
    for (kind, name, what) in [
        (
            Reloc::Abs8,
            imported.clone(),
            "`Abs8` relocation to `cb$slot$b$get`",
        ),
        (
            pc,
            ModuleRelocTarget::LibCall(cranelift_codegen::ir::LibCall::Memcpy),
            "libcall",
        ),
        (
            pc,
            ModuleRelocTarget::user(0, runtime.as_u32()),
            "`cheby_rt_alloc`, which only an import word may name",
        ),
    ] {
        let text = check_text_relocs(&m, &decls, f, &code(kind, name))
            .unwrap_err()
            .to_string();
        assert!(
            text.contains("`cb$fn$a$run`") && text.contains(what),
            "{what}: {text}"
        );
    }
    // The JIT's table never accepts an imported target in text.
    let jit = Decls::new(m.isa()).unwrap();
    assert!(!jit.is_generated_import("cb$slot$b$get"));
    assert!(decls.is_generated_import("cb$slot$b$get"));
    assert!(!decls.is_generated_import("cheby_rt_alloc"));
    assert!(!decls.is_generated_import("cb$libcall$Memcpy"));
}
