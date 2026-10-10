//! The MIR data model (D1).
//!
//! MIR is close to SSA: every local is defined exactly once (a function
//! parameter, a block parameter or an [`Stmt::Assign`]), and joins pass
//! values through block parameters. Layouts are symbolic ([`LayoutRef`]);
//! code generation maps them to runtime layout ids, so MIR never holds a
//! per-process number. Every def a unit mentions has its path in
//! [`MirModule::names`], so a unit, its dump and its symbols need no
//! interfaces.

use cheby_diag::Span;
use cheby_iface::{DefId, Ice, Idx, IndexVec, ModuleId, Name, Ty};

use crate::abi::AbiSig;
use crate::layout::LayoutDesc;
use crate::repr::{ForeignSig, Repr};

cheby_iface::newtype_index! {
    /// A string of one function's string table. Dumps as `str#N`.
    pub struct StrId;
}

cheby_iface::newtype_index! {
    /// A local of one function. Dumps as `_N`.
    pub struct Local;
}

cheby_iface::newtype_index! {
    /// A basic block of one function. Dumps as `bbN`; `bb0` is the entry.
    pub struct BlockId;
}

cheby_iface::newtype_index! {
    /// A static object of one unit ([`MirModule::statics`]). Dumps as
    /// `static#N`.
    pub struct StaticId;
}

/// What a unit of MIR holds. A unit never mixes kinds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum UnitKind {
    /// One module's code: its functions, external wrappers, shims,
    /// closures and constant initializers.
    Code(ModuleId),
    /// One module's `test` blocks (D-265).
    Tests(ModuleId),
    /// The C-ABI entry thunks of one build or REPL entry. They belong to
    /// no module, so a module's code does not depend on which entry runs.
    Entries,
}

/// A unit of MIR: one module's code, one module's tests, or entry thunks.
#[derive(Clone, PartialEq, Debug)]
pub struct MirModule {
    pub unit: UnitKind,
    /// The module's path (`std::io`) for `Code` and `Tests`; empty for
    /// `Entries`.
    pub path: Box<str>,
    /// In order: items by `DefIndex`, each followed by its closures in
    /// creation order, then the value shims the unit needs by `DefId`;
    /// entry thunks in the order given.
    pub functions: Vec<Function>,
    /// Every [`Callee::Direct`] target, own module or not, sorted by
    /// `DefId`, with its signature from [`crate::abi::abi_sig`]. The
    /// validator and codegen take callee signatures from here, so neither
    /// needs the callee's MIR.
    pub callees: Vec<CalleeDecl>,
    /// Layouts of the constructors this module's types declare, in
    /// `DefIndex` order (D-252, D-263). Registered before any of the
    /// unit's code is defined (D-454).
    pub layouts: Vec<LayoutDesc>,
    /// Layouts of constructors that other modules declare, builtin ones
    /// included, which this unit's code allocates, reads fields of, uses
    /// as static objects or reads tags of: for a [`TagType::Adt`], every
    /// constructor of the type. Sorted by [`LayoutRef`], without
    /// duplicates. Computed from interfaces only (D-246), so the
    /// validator and codegen find field offsets without the other unit.
    /// Never registered from here: their own unit registers them, and
    /// builtin ones have fixed ids (D-454).
    pub foreign_layouts: Vec<LayoutDesc>,
    /// The unit's static objects (D-523), in [`StaticId`] order: element
    /// `n` has id `n`.
    pub statics: Vec<StaticObject>,
    /// The path of every def this unit mentions (function keys, callees,
    /// constructors, constants, layouts, tag types), sorted by `DefId`.
    pub names: Vec<DefName>,
}

impl MirModule {
    /// The module the unit belongs to; `None` for entry thunks.
    #[must_use]
    pub const fn module(&self) -> Option<ModuleId> {
        match self.unit {
            UnitKind::Code(m) | UnitKind::Tests(m) => Some(m),
            UnitKind::Entries => None,
        }
    }

    /// The declared signature of the direct callee `def`.
    #[must_use]
    pub fn callee(&self, def: DefId) -> Option<&CalleeDecl> {
        self.callees
            .binary_search_by(|c| c.def.cmp(&def))
            .ok()
            .and_then(|n| self.callees.get(n))
    }

    /// The path of `def`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the unit does not record `def`.
    pub fn name(&self, def: DefId) -> Result<&DefName, Ice> {
        self.names
            .binary_search_by(|n| n.def.cmp(&def))
            .ok()
            .and_then(|n| self.names.get(n))
            .ok_or_else(|| Ice::new(format!("MIR unit has no name for {def:?}")))
    }

    /// The function with key `key`, if this unit defines it.
    #[must_use]
    pub fn function(&self, key: FnKey) -> Option<&Function> {
        self.functions.iter().find(|f| f.key == key)
    }

    /// The layout of a constructor `layout`, from [`MirModule::layouts`]
    /// or [`MirModule::foreign_layouts`].
    #[must_use]
    pub fn layout(&self, layout: LayoutRef) -> Option<&LayoutDesc> {
        self.layouts
            .iter()
            .chain(&self.foreign_layouts)
            .find(|d| d.layout == layout)
    }

    /// The layouts this unit knows of the constructors of type `ty`, in
    /// tag order.
    #[must_use]
    pub fn type_layouts(&self, ty: DefId) -> Vec<&LayoutDesc> {
        let mut out: Vec<&LayoutDesc> = self
            .layouts
            .iter()
            .chain(&self.foreign_layouts)
            .filter(|d| d.type_def == ty)
            .collect();
        out.sort_by_key(|d| d.tag);
        out
    }

    /// The static object `id`.
    #[must_use]
    pub fn static_object(&self, id: StaticId) -> Option<&StaticObject> {
        self.statics.get(id.index())
    }
}

/// A static object that code generation emits (D-523): count 0, never
/// written, defined once per process (a later unit that names the same
/// symbol imports it). Its symbol is `cb$<kind>$<module path>$<name>`
/// (plan §4.4), where the module path is the unit's.
#[derive(Clone, PartialEq, Debug)]
pub struct StaticObject {
    /// Its position in [`MirModule::statics`].
    pub id: StaticId,
    pub kind: StaticKind,
    /// The module path of its symbol, `cb$<kind>$<module>$<name>`: the
    /// unit's own for a dictionary or a function value, the type's for a
    /// nominal descriptor (`cb$desc$<module path>$<Type>`), and the
    /// interface's for the type `dyn I` (`cb$desc$<module path of
    /// I>$dyn$<I>`, D-523). A unit may define a static of another module;
    /// the JIT defines each symbol once per process.
    pub module: Box<str>,
    /// Unique with `module` and `kind` within the unit, and stable: built
    /// from paths and names, never from ids (D-264, D-523).
    pub name: Box<str>,
    /// [`LayoutRef::Dict`] for a dictionary, [`LayoutRef::Desc`] or
    /// [`LayoutRef::DescNominal`] for a descriptor, [`LayoutRef::Closure`]
    /// of no captures for a function value.
    pub layout: LayoutRef,
    /// The fields in declaration order (plan §5.4): constants only, so a
    /// static never points at anything that is not static. A function
    /// value's first field is its [`Const::Code`]. A [`Const::Str`] here
    /// indexes [`StaticObject::strings`].
    pub fields: Box<[Const]>,
    /// The static's own string table, such as a nominal descriptor's
    /// printed name and module path (D-524). Codegen emits them as
    /// `cb$str$<the static's symbol without cb$>$<n>` (D-456).
    pub strings: IndexVec<StrId, Box<str>>,
}

/// What a [`StaticObject`] is, which picks its symbol kind (D-523).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum StaticKind {
    /// A dictionary that needs no dictionary of its caller:
    /// `cb$dict$…`.
    Dict,
    /// A type descriptor: `cb$desc$…`.
    Desc,
    /// A closure with no captures: `cb$fnval$…`.
    FnValue,
}

impl StaticKind {
    /// The name used in dumps and in the symbol kind.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dict => "dict",
            Self::Desc => "desc",
            Self::FnValue => "fnval",
        }
    }
}

/// The signature of a direct callee.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct CalleeDecl {
    pub def: DefId,
    pub sig: AbiSig,
}

/// The path of a def, for dumps and symbol names.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct DefName {
    pub def: DefId,
    /// The module path, `std::io`; empty for the builtin module.
    pub module: Box<str>,
    /// The item within the module: `println`, `Shape::area` for an
    /// interface function, the name of a constructor.
    pub item: Box<str>,
}

impl DefName {
    /// `module::item`, or `item` for a builtin def (as `def_path` writes it).
    #[must_use]
    pub fn path(&self) -> String {
        if self.module.is_empty() {
            self.item.to_string()
        } else {
            format!("{}::{}", self.module, self.item)
        }
    }
}

/// Identifies a function of MIR: what codegen declares and names.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum FnKey {
    /// A top-level function, including the wrapper of an `@external`.
    Item(DefId),
    /// The uniform-convention adapter of a top-level function used as a
    /// value or a dictionary entry (D-453).
    ValueShim(DefId),
    /// A closure or local function of `owner`, numbered in lowering order.
    Closure { owner: DefId, index: u32 },
    /// A constant's initializer: Cheby ABI, no parameters (E6).
    ConstInit(DefId),
    /// A `test` block's body: Cheby ABI, no parameters (G4).
    Test(DefId),
    /// A C-ABI entry thunk.
    Entry(EntryKind),
    /// A C-ABI trampoline into the uniform convention, by arity: how the
    /// runtime calls a closure (D-453).
    Trampoline(u32),
    /// The code of the built-in `Show` or `Compare` function `iface_fn`
    /// for tuples of `arity` elements (D-167), the entry of a tuple
    /// dictionary. Uniform signature ([`crate::abi::uniform_sig`]).
    TupleDictFn { iface_fn: DefId, arity: u32 },
    /// The code of a dictionary entry for `dyn I` that forwards the call
    /// of `iface_fn` to the dictionary the `dyn` value carries (§8.6).
    /// Uniform signature.
    DynForward { iface_fn: DefId },
}

impl FnKey {
    /// Whether functions with this key use the uniform convention for
    /// unknown callees: the owned `self`, then uniform words (D-453).
    #[must_use]
    pub const fn is_uniform(self) -> bool {
        matches!(
            self,
            Self::ValueShim(_)
                | Self::Closure { .. }
                | Self::TupleDictFn { .. }
                | Self::DynForward { .. }
        )
    }
}

/// What an entry thunk runs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum EntryKind {
    /// A module's `main` (D-137).
    Main(DefId),
    /// A `test` block (G4).
    Test(DefId),
    /// A constant's initializer (E6).
    Const(DefId),
    /// A REPL entry (G5).
    Repl(u32),
}

/// A function's calling convention.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Abi {
    /// Cranelift's `tail` convention with the hidden `ctx` parameter first.
    Cheby,
    /// The platform's C convention. Entry thunks and trampolines also take
    /// `ctx` first.
    C,
}

/// One function of MIR.
#[derive(Clone, PartialEq, Debug)]
pub struct Function {
    pub key: FnKey,
    pub abi: Abi,
    /// The parameters, without the hidden `ctx` (D-453).
    pub params: Vec<Local>,
    /// [`Repr::Value`] for [`Abi::Cheby`] (uniform return, D-453);
    /// [`Repr::I64`] for `Main` and `Test` thunks (the exit status);
    /// [`Repr::Value`] for `Const` and `Repl` thunks.
    pub ret: Repr,
    pub locals: IndexVec<Local, LocalDecl>,
    /// `bb0` is the entry block.
    pub blocks: IndexVec<BlockId, Block>,
    /// The function's name in the source, or the entry's item.
    pub span: Span,
    /// Literals and locations, deduplicated, in first-use order. Per
    /// function, so functions lower in parallel with no shared counter
    /// (D-263, D-264).
    pub strings: IndexVec<StrId, Box<str>>,
    /// The location the function-entry stack check reports. Set for every
    /// [`Abi::Cheby`] function, `None` for [`Abi::C`].
    pub entry_loc: Option<StrId>,
}

impl Function {
    /// The local `local`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if there is no such local.
    pub fn local(&self, local: Local) -> Result<&LocalDecl, Ice> {
        self.locals.try_get(local, "MIR local")
    }

    /// The block `block`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if there is no such block.
    pub fn block(&self, block: BlockId) -> Result<&Block, Ice> {
        self.blocks.try_get(block, "MIR block")
    }

    /// The representation of `op`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] for an unknown local.
    pub fn operand_repr(&self, op: &Operand) -> Result<Repr, Ice> {
        match op {
            Operand::Local(l) => Ok(self.local(*l)?.repr),
            Operand::Const(c) => Ok(c.repr()),
        }
    }
}

/// A local's declaration.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LocalDecl {
    pub repr: Repr,
    /// The source name, for debugging.
    pub name: Option<Name>,
    /// The Cheby type, when the local holds a source value.
    pub ty: Option<Ty>,
}

/// A basic block.
#[derive(Clone, PartialEq, Debug)]
pub struct Block {
    pub params: Vec<Local>,
    pub stmts: Vec<Stmt>,
    pub term: Terminator,
}

/// A statement.
#[derive(Clone, PartialEq, Debug)]
pub enum Stmt {
    Assign {
        dst: Local,
        value: Rvalue,
    },
    /// A call whose result is `void`: a foreign function returning `Nil`,
    /// or a runtime helper without a result ([`RuntimeFn::ret`]).
    Eval(Rvalue),
    /// One more reference. A no-op at run time for tagged integers and
    /// static objects.
    Dup(Local),
    /// One reference less; frees the object at zero.
    Drop(Local),
}

/// An operand: a local or a constant.
#[derive(Clone, PartialEq, Debug)]
pub enum Operand {
    Local(Local),
    Const(Const),
}

impl Operand {
    /// The local, if the operand is one.
    #[must_use]
    pub const fn local(&self) -> Option<Local> {
        match self {
            Self::Local(l) => Some(*l),
            Self::Const(_) => None,
        }
    }
}

/// A constant operand. Value constants are static objects, so they need no
/// reference counting.
#[derive(Clone, PartialEq, Debug)]
pub enum Const {
    /// [`Repr::Int`].
    Int(i64),
    /// [`Repr::Float`], as bits.
    Float(u64),
    /// [`Repr::Bool`].
    Bool(bool),
    /// A sized number: `repr` is one of `I8` … `U64`, `F32`, and `bits`
    /// its value's bits (sign-extended for signed types). Main and test
    /// thunks return `Sized { I64, status }`.
    Sized { repr: Repr, bits: u64 },
    /// A static `String` object holding the function's string `StrId`.
    Str(StrId),
    /// The static `Nil` object.
    Nil,
    /// The static object of a nullary constructor: the runtime's for a
    /// builtin one, `cb$ctor$…` for a user one (D-454).
    StaticCtor(DefId),
    /// The static closure `cb$fnval$…` of a top-level function without
    /// dictionaries, whose code is its [`FnKey::ValueShim`] (D-453,
    /// D-523).
    FnValue(DefId),
    /// The code word `addr | 1` of a function, only as the first field of
    /// a closure [`Rvalue::Alloc`] or of a function-value static.
    Code(FnKey),
    /// The runtime's static empty list (`cheby_rt_static_empty_list`).
    EmptyList,
    /// A static object of this unit ([`MirModule::statics`]).
    Static(StaticId),
    /// The runtime's nominal descriptor of a builtin type,
    /// `cheby_rt_desc_<name>` (D-523).
    BuiltinDesc(BuiltinDesc),
}

/// The builtin types with a runtime nominal descriptor
/// `cheby_rt_desc_<name>` (D-523, plan §4.3.2).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum BuiltinDesc {
    Int,
    Float,
    F32,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    Bool,
    Nil,
    String,
    Bytes,
    List,
    Map,
    Set,
    Option,
    Result,
    Order,
    /// Every tuple; the arity comes from the descriptor's arguments.
    Tuple,
    /// Every function type; the arguments are the parameter types, then
    /// the result.
    Fn,
}

impl BuiltinDesc {
    /// The `<name>` of `cheby_rt_desc_<name>`, also used in dumps.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Float => "float",
            Self::F32 => "f32",
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::Bool => "bool",
            Self::Nil => "nil",
            Self::String => "string",
            Self::Bytes => "bytes",
            Self::List => "list",
            Self::Map => "map",
            Self::Set => "set",
            Self::Option => "option",
            Self::Result => "result",
            Self::Order => "order",
            Self::Tuple => "tuple",
            Self::Fn => "fn",
        }
    }
}

impl Const {
    /// The representation of the constant.
    #[must_use]
    pub const fn repr(&self) -> Repr {
        match self {
            Self::Int(_) => Repr::Int,
            Self::Float(_) => Repr::Float,
            Self::Bool(_) => Repr::Bool,
            Self::Sized { repr, .. } => *repr,
            Self::Str(_)
            | Self::Nil
            | Self::StaticCtor(_)
            | Self::FnValue(_)
            | Self::Code(_)
            | Self::EmptyList
            | Self::Static(_)
            | Self::BuiltinDesc(_) => Repr::Value,
        }
    }
}

/// A value computation.
#[derive(Clone, PartialEq, Debug)]
pub enum Rvalue {
    /// A copy of a constant, or a move of a local (an owned use).
    Use(Operand),
    /// A Cheby-ABI call: owned arguments (dictionaries first), an owned
    /// [`Repr::Value`] result.
    Call { callee: Callee, args: Vec<Operand> },
    /// A C-ABI call: borrowed arguments, an owned result (D-187).
    CallForeign {
        symbol: Box<str>,
        sig: ForeignSig,
        args: Vec<Operand>,
    },
    /// A runtime helper, with the ownership [`RuntimeFn`] documents.
    CallRuntime { func: RuntimeFn, args: Vec<Operand> },
    /// A new object: owned fields in declaration order (a closure's code
    /// word first).
    Alloc {
        layout: LayoutRef,
        fields: Vec<Operand>,
    },
    /// Field `field` (declaration order) of `base`, which is borrowed. A
    /// [`Repr::Value`] result is borrowed from the base, so the RC pass
    /// dups it right after the definition.
    Field {
        base: Local,
        layout: LayoutRef,
        field: u32,
    },
    /// The layout id ([`Repr::U32`]) of a pointer `base` (borrowed).
    ReadLayout { base: Local },
    /// The value in a constant's slot, borrowed from the slot (the RC pass
    /// dups it).
    LoadConst(DefId),
    /// A raw value in `from` boxed into a uniform word, canonically
    /// (D-278, D-521): tagged when it fits, else a boxed number; a `Bool`
    /// as the static `True` or `False`. The result is owned.
    Box { value: Operand, from: Repr },
    /// A uniform word (borrowed) unboxed into `to` (D-521).
    Unbox { value: Operand, to: Repr },
    /// Integer arithmetic on `Int` or a sized integer (`I8` … `U64`) that
    /// panics at `loc`: `Overflow` on overflow (`<<` included, §5.4.6),
    /// `DivisionByZero` on a zero divisor, `ShiftOutOfRange` on a shift
    /// count outside the width (§3.3, D-025, D-069). `MIN % -1` is 0.
    Checked {
        op: ArithOp,
        lhs: Operand,
        rhs: Operand,
        repr: Repr,
        loc: StrId,
    },
    /// A comparison of two raw values of `repr`, giving a [`Repr::Bool`]:
    /// integers, `Bool` (`Eq` and `Ne` only) and floats. On `Float` and
    /// `F32`, `Eq`/`Ne` follow §3.3.2 (NaN equals NaN, `-0.0 == 0.0`) and
    /// `Lt Le Gt Ge` the total order of `compare` (§3.14, D-527). Never on
    /// uniform values: structural `==` is [`RuntimeFn::Equal`].
    Compare {
        op: CmpOp,
        lhs: Operand,
        rhs: Operand,
        repr: Repr,
    },
    /// IEEE arithmetic on `Float`/`F32`: `Add`, `Sub`, `Mul`, `Div`,
    /// `Rem`. Never panics (§3.3.2). `Rem` has the sign of the dividend
    /// (§3.3.1, D-527, Rust's `%` on floats).
    FloatArith {
        op: ArithOp,
        lhs: Operand,
        rhs: Operand,
        repr: Repr,
    },
    /// `&`, `|`, `^` on `Int` and the sized integers (§5.4.6). Never
    /// panics.
    Bitwise {
        op: BitOp,
        lhs: Operand,
        rhs: Operand,
        repr: Repr,
    },
    /// Sign flip of a `Float`/`F32` (built-in unary `-`, D-118, D-527):
    /// `-0.0` for `0.0`.
    FloatNeg { value: Operand, repr: Repr },
    /// `!` on an unboxed `Bool`.
    Not(Operand),
    /// The constructor tag (declaration order) of `base` (borrowed), a
    /// value of an ADT with more than one constructor, as an
    /// [`Repr::Int`], without embedding a user layout id (D-454, D-522).
    ReadTag { base: Operand, ty: TagType },
}

/// Bitwise operators ([`Rvalue::Bitwise`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BitOp {
    And,
    Or,
    Xor,
}

impl BitOp {
    /// The name used in dumps.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::And => "and",
            Self::Or => "or",
            Self::Xor => "xor",
        }
    }
}

/// The type whose tag [`Rvalue::ReadTag`] reads (D-522).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum TagType {
    /// `Option`: the layout id minus 22.
    Option,
    /// `Result`: the layout id minus 24.
    Result,
    /// `Order`: the layout id minus 19.
    Order,
    /// A user ADT with more than one constructor, by the type's `DefId`:
    /// the layout id minus the id in its first constructor's
    /// `cb$layout$` word. Its constructors' layouts are in the unit
    /// ([`MirModule::type_layouts`]).
    Adt(DefId),
}

/// How an instruction uses a local (D7).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum UseKind {
    /// Consumes one reference.
    Owned,
    /// Reads the value; the reference stays with the local.
    Borrowed,
}

fn operand_uses(ops: &[Operand], kind: UseKind, out: &mut Vec<(Local, UseKind)>) {
    out.extend(ops.iter().filter_map(Operand::local).map(|l| (l, kind)));
}

impl Rvalue {
    /// The locals the rvalue reads, in operand order, with how it uses
    /// them. A local may appear more than once.
    #[must_use]
    pub fn uses(&self) -> Vec<(Local, UseKind)> {
        let mut out = Vec::new();
        match self {
            Self::Use(op) => operand_uses(std::slice::from_ref(op), UseKind::Owned, &mut out),
            Self::Call { callee, args } => {
                if let Callee::Indirect(f) = callee {
                    out.push((*f, UseKind::Owned));
                }
                operand_uses(args, UseKind::Owned, &mut out);
            }
            Self::CallForeign { args, .. } => operand_uses(args, UseKind::Borrowed, &mut out),
            Self::CallRuntime { func, args } => {
                let params = func.params();
                for (n, arg) in args.iter().enumerate() {
                    // An argument beyond the helper's parameters is a
                    // validator error; it counts as owned meanwhile.
                    let kind = params.get(n).map_or(UseKind::Owned, |&(_, k)| k);
                    operand_uses(std::slice::from_ref(arg), kind, &mut out);
                }
            }
            Self::Alloc { fields, .. } => operand_uses(fields, UseKind::Owned, &mut out),
            Self::Field { base, .. } | Self::ReadLayout { base } => {
                out.push((*base, UseKind::Borrowed));
            }
            Self::LoadConst(_) => {}
            Self::Box { value, .. }
            | Self::Unbox { value, .. }
            | Self::FloatNeg { value, .. }
            | Self::Not(value)
            | Self::ReadTag { base: value, .. } => {
                operand_uses(std::slice::from_ref(value), UseKind::Borrowed, &mut out);
            }
            Self::Checked { lhs, rhs, .. }
            | Self::Compare { lhs, rhs, .. }
            | Self::FloatArith { lhs, rhs, .. }
            | Self::Bitwise { lhs, rhs, .. } => {
                operand_uses(&[lhs.clone(), rhs.clone()], UseKind::Borrowed, &mut out);
            }
        }
        out
    }

    /// The operands the rvalue reads, in order (the base of `field` and
    /// `read_layout` included).
    #[must_use]
    pub fn operands(&self) -> Vec<Operand> {
        match self {
            Self::Use(op)
            | Self::Box { value: op, .. }
            | Self::Unbox { value: op, .. }
            | Self::FloatNeg { value: op, .. }
            | Self::Not(op)
            | Self::ReadTag { base: op, .. } => vec![op.clone()],
            Self::Call { callee, args } => {
                let mut out = Vec::with_capacity(args.len().saturating_add(1));
                if let Callee::Indirect(f) = callee {
                    out.push(Operand::Local(*f));
                }
                out.extend(args.iter().cloned());
                out
            }
            Self::CallForeign { args, .. } | Self::CallRuntime { args, .. } => args.clone(),
            Self::Alloc { fields, .. } => fields.clone(),
            Self::Field { base, .. } | Self::ReadLayout { base } => vec![Operand::Local(*base)],
            Self::LoadConst(_) => Vec::new(),
            Self::Checked { lhs, rhs, .. }
            | Self::Compare { lhs, rhs, .. }
            | Self::FloatArith { lhs, rhs, .. }
            | Self::Bitwise { lhs, rhs, .. } => vec![lhs.clone(), rhs.clone()],
        }
    }

    /// Whether a [`Repr::Value`] result is borrowed (from a base object or
    /// a constant slot) rather than owned.
    #[must_use]
    pub const fn result_is_borrowed(&self) -> bool {
        matches!(self, Self::Field { .. } | Self::LoadConst(_))
    }
}

/// What a Cheby-ABI call calls.
#[derive(Clone, PartialEq, Debug)]
pub enum Callee {
    /// A top-level Cheby function, always through its indirection slot
    /// (D-043), even in the same module. Its signature is in
    /// [`MirModule::callees`].
    Direct(DefId),
    /// A function by key that is not called through a slot: a shim or
    /// closure body of this unit, or a constant initializer or test body
    /// (from an entry thunk).
    Code(FnKey),
    /// A function value: a closure object, passed as the owned `self`.
    Indirect(Local),
}

/// How a block ends.
#[derive(Clone, PartialEq, Debug)]
pub enum Terminator {
    /// Returns an owned value.
    Return(Operand),
    /// A Cheby-to-Cheby tail call (D-015): Cranelift `return_call` or
    /// `return_call_indirect`. Owned arguments.
    TailCall {
        callee: Callee,
        args: Vec<Operand>,
    },
    /// A jump passing owned values into the target's parameters.
    Goto {
        target: BlockId,
        args: Vec<Operand>,
    },
    /// On a [`Repr::Bool`] condition. Targets take no parameters.
    Branch {
        cond: Operand,
        then_: BlockId,
        else_: BlockId,
    },
    /// On a layout id ([`Repr::U32`]), an `Int` or a sized integer; each
    /// case is the bits of a value in `on`'s representation. Targets take
    /// no parameters.
    Switch {
        on: Operand,
        cases: Vec<(u64, BlockId)>,
        default: Option<BlockId>,
    },
    /// Panics at `loc`, with an optional borrowed `String` message. Live
    /// locals leak in M1 (no unwinding, D-077 in M2).
    Panic {
        kind: PanicKind,
        message: Option<Operand>,
        loc: StrId,
    },
    Unreachable,
}

impl Terminator {
    /// The locals the terminator reads, with how it uses them.
    #[must_use]
    pub fn uses(&self) -> Vec<(Local, UseKind)> {
        let mut out = Vec::new();
        match self {
            Self::Return(op) => operand_uses(std::slice::from_ref(op), UseKind::Owned, &mut out),
            Self::TailCall { callee, args } => {
                if let Callee::Indirect(f) = callee {
                    out.push((*f, UseKind::Owned));
                }
                operand_uses(args, UseKind::Owned, &mut out);
            }
            Self::Goto { args, .. } => operand_uses(args, UseKind::Owned, &mut out),
            Self::Branch { cond, .. } => {
                operand_uses(std::slice::from_ref(cond), UseKind::Borrowed, &mut out);
            }
            Self::Switch { on, .. } => {
                operand_uses(std::slice::from_ref(on), UseKind::Borrowed, &mut out);
            }
            Self::Panic { message, .. } => {
                if let Some(m) = message {
                    operand_uses(std::slice::from_ref(m), UseKind::Borrowed, &mut out);
                }
            }
            Self::Unreachable => {}
        }
        out
    }

    /// The successor blocks, in order, duplicates included.
    #[must_use]
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Self::Return(_) | Self::TailCall { .. } | Self::Panic { .. } | Self::Unreachable => {
                Vec::new()
            }
            Self::Goto { target, .. } => vec![*target],
            Self::Branch { then_, else_, .. } => vec![*then_, *else_],
            Self::Switch { cases, default, .. } => cases
                .iter()
                .map(|&(_, b)| b)
                .chain(default.iter().copied())
                .collect(),
        }
    }

    /// Replaces every successor `from` with `to`.
    pub fn retarget(&mut self, from: BlockId, to: BlockId) {
        let swap = |b: &mut BlockId| {
            if *b == from {
                *b = to;
            }
        };
        match self {
            Self::Return(_) | Self::TailCall { .. } | Self::Panic { .. } | Self::Unreachable => {}
            Self::Goto { target, .. } => swap(target),
            Self::Branch { then_, else_, .. } => {
                swap(then_);
                swap(else_);
            }
            Self::Switch { cases, default, .. } => {
                for (_, b) in cases {
                    swap(b);
                }
                if let Some(b) = default {
                    swap(b);
                }
            }
        }
    }
}

/// A symbolic object layout. Codegen maps it to a runtime layout id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum LayoutRef {
    /// A runtime-defined object kind.
    Builtin(BuiltinLayout),
    /// A constructor, builtin (`Some`, `Nil`) or user-declared.
    Ctor(DefId),
    /// A tuple of the given arity; every field is a uniform value.
    Tuple(u32),
    /// A closure with the given number of captures, after the code word.
    Closure(u32),
    /// A dictionary (layout id 9, D-524): the type descriptor, `fns`
    /// closures in interface-function order, then `embedded` dictionaries
    /// of the directly embedded interfaces in written order. Every field
    /// is a uniform value.
    Dict { fns: u32, embedded: u32 },
    /// A `dyn` value (layout id 10, D-524): the dictionary of its
    /// interface for the underlying type, then the underlying value.
    Dyn,
    /// A type descriptor (layout id 11, D-524): the nominal descriptor,
    /// then `args` descriptors of the type arguments.
    Desc { args: u32 },
    /// A nominal descriptor (layout id 12, D-524): the type's printed
    /// name and its module path, both static `String`s. Only static.
    DescNominal,
}

impl LayoutRef {
    /// The number of fields of a layout whose fields are all uniform
    /// values, the closure's code word included; `None` for constructors
    /// and runtime-defined kinds, whose fields come from elsewhere.
    #[must_use]
    pub fn uniform_fields(self) -> Option<u64> {
        match self {
            Self::Tuple(n) => Some(u64::from(n)),
            Self::Closure(n) | Self::Desc { args: n } => Some(u64::from(n).saturating_add(1)),
            Self::Dict { fns, embedded } => Some(
                u64::from(fns)
                    .saturating_add(u64::from(embedded))
                    .saturating_add(1),
            ),
            Self::Dyn | Self::DescNominal => Some(2),
            Self::Builtin(_) | Self::Ctor(_) => None,
        }
    }
}

/// The runtime-defined object kinds that are not constructors or tuples.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum BuiltinLayout {
    String,
    Bytes,
    BoxedInt,
    BoxedFloat,
    BoxedF32,
    BoxedU64,
}

/// A runtime helper called from generated code (D-520, plan §4.1). Each
/// variant documents the ownership of its arguments and result, which
/// [`RuntimeFn::params`] states for the RC pass and the validator. Every
/// [`Repr::Value`] result is owned. The symbol and C signature are the
/// runtime's (`cheby_runtime::abi::sym`); codegen passes `ctx` where the
/// helper takes it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum RuntimeFn {
    /// `(capacity: Int) -> String`: an empty unique `String` with that
    /// capacity. Result owned.
    StrWithCapacity,
    /// `(acc: String owned, piece: String borrowed) -> String`: appends
    /// in place when `acc` is unique (count 1), copies otherwise. Result
    /// owned.
    StrAppend,
    /// `(a: String borrowed, b: String borrowed) -> Bool`: byte equality
    /// (string literal patterns).
    StrEq,
    /// `(list owned, elem owned) -> List`: appends `elem`, in place when
    /// the list is unique (F3). Result owned.
    ListPush,
    /// `(a owned, b borrowed) -> List`: `a` followed by `b`'s elements
    /// (spreads, `[..a, ..b]`). Result owned.
    ListAppend,
    /// `(list borrowed) -> Int`: the length (list patterns).
    ListLength,
    /// `(list borrowed, index: Int) -> T`: the element at `index`, which
    /// lowering has checked is in range; the element is dup'd, so the
    /// result is owned.
    ListGet,
    /// `(list borrowed, start: Int, end: Int) -> List`: an O(1) view of
    /// `start..end` (D-053), for `..rest`. Result owned.
    ListSlice,
    /// `(a borrowed, b borrowed, location: String borrowed) -> Bool`:
    /// structural `==` (§3.13). Panics `EqualityOnFunction` (D-233) at
    /// `location`, the static `Const::Str` of the `==`'s source location
    /// (§11.6, D-449, D-556); may call `@std_hooks` functions (D-482).
    Equal,
    /// `(value borrowed) -> String`: debug text (§3.13, D-531). Result
    /// owned.
    Debug,
    /// `(symbol: String borrowed) -> void`: panics `ForbiddenEffect` when
    /// the running entry evaluates a constant (plan §7.4); a no-op
    /// otherwise. Evaluated for effect ([`Stmt::Eval`]).
    EffectCheck,
}

impl RuntimeFn {
    /// Every helper, in declaration order.
    pub const ALL: [Self; 11] = [
        Self::StrWithCapacity,
        Self::StrAppend,
        Self::StrEq,
        Self::ListPush,
        Self::ListAppend,
        Self::ListLength,
        Self::ListGet,
        Self::ListSlice,
        Self::Equal,
        Self::Debug,
        Self::EffectCheck,
    ];

    /// The parameters: each one's representation and whether the helper
    /// consumes it ([`UseKind::Owned`]) or only reads it.
    #[must_use]
    pub const fn params(self) -> &'static [(Repr, UseKind)] {
        use Repr::{Int, Value};
        use UseKind::{Borrowed, Owned};
        match self {
            Self::StrWithCapacity => &[(Int, Borrowed)],
            Self::StrAppend | Self::ListAppend => &[(Value, Owned), (Value, Borrowed)],
            Self::StrEq => &[(Value, Borrowed), (Value, Borrowed)],
            Self::Equal => &[(Value, Borrowed), (Value, Borrowed), (Value, Borrowed)],
            Self::ListPush => &[(Value, Owned), (Value, Owned)],
            Self::ListLength | Self::Debug | Self::EffectCheck => &[(Value, Borrowed)],
            Self::ListGet => &[(Value, Borrowed), (Int, Borrowed)],
            Self::ListSlice => &[(Value, Borrowed), (Int, Borrowed), (Int, Borrowed)],
        }
    }

    /// The result's representation; `None` for a helper returning
    /// nothing, which is evaluated for effect.
    #[must_use]
    pub const fn ret(self) -> Option<Repr> {
        match self {
            Self::StrWithCapacity
            | Self::StrAppend
            | Self::ListPush
            | Self::ListAppend
            | Self::ListGet
            | Self::ListSlice
            | Self::Debug => Some(Repr::Value),
            Self::StrEq | Self::Equal => Some(Repr::Bool),
            Self::ListLength => Some(Repr::Int),
            Self::EffectCheck => None,
        }
    }

    /// The name used in dumps.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::StrWithCapacity => "str_with_capacity",
            Self::StrAppend => "str_append",
            Self::StrEq => "str_eq",
            Self::ListPush => "list_push",
            Self::ListAppend => "list_append",
            Self::ListLength => "list_length",
            Self::ListGet => "list_get",
            Self::ListSlice => "list_slice",
            Self::Equal => "equal",
            Self::Debug => "debug",
            Self::EffectCheck => "effect_check",
        }
    }
}

/// Checked arithmetic operators.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Shl,
    Shr,
}

impl ArithOp {
    /// The name used in dumps.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Sub => "sub",
            Self::Mul => "mul",
            Self::Div => "div",
            Self::Rem => "rem",
            Self::Shl => "shl",
            Self::Shr => "shr",
        }
    }
}

/// Comparison operators.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    /// The name used in dumps.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Lt => "lt",
            Self::Le => "le",
            Self::Gt => "gt",
            Self::Ge => "ge",
        }
    }
}

/// Why a panic happens. Mirrors `cheby_runtime::abi::PanicKind`, number
/// for number (a test in `cheby_jit` checks it); `cheby_mir` does not
/// depend on the runtime. Append-only.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PanicKind {
    Explicit,
    Todo,
    Overflow,
    DivisionByZero,
    Assert,
    LetAssert,
    StackOverflow,
    EqualityOnFunction,
    ShiftOutOfRange,
    /// A constant's evaluation needed its own value (D-511); constant
    /// evaluation reports it as E0069, never to users directly.
    ConstCycle,
    /// Constant evaluation reached an effect it may not use (§4.5,
    /// D-360); reported as E0077, never to users directly.
    ForbiddenEffect,
}

impl PanicKind {
    /// Every kind, in numbering order.
    pub const ALL: [Self; 11] = [
        Self::Explicit,
        Self::Todo,
        Self::Overflow,
        Self::DivisionByZero,
        Self::Assert,
        Self::LetAssert,
        Self::StackOverflow,
        Self::EqualityOnFunction,
        Self::ShiftOutOfRange,
        Self::ConstCycle,
        Self::ForbiddenEffect,
    ];

    /// The number passed to `cheby_rt_panic`.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::Explicit => 0,
            Self::Todo => 1,
            Self::Overflow => 2,
            Self::DivisionByZero => 3,
            Self::Assert => 4,
            Self::LetAssert => 5,
            Self::StackOverflow => 6,
            Self::EqualityOnFunction => 7,
            Self::ShiftOutOfRange => 8,
            Self::ConstCycle => 9,
            Self::ForbiddenEffect => 10,
        }
    }

    /// The name used in dumps.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Todo => "todo",
            Self::Overflow => "overflow",
            Self::DivisionByZero => "division_by_zero",
            Self::Assert => "assert",
            Self::LetAssert => "let_assert",
            Self::StackOverflow => "stack_overflow",
            Self::EqualityOnFunction => "equality_on_function",
            Self::ShiftOutOfRange => "shift_out_of_range",
            Self::ConstCycle => "const_cycle",
            Self::ForbiddenEffect => "forbidden_effect",
        }
    }
}
