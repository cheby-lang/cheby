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
use cheby_iface::{DefId, Ice, IndexVec, ModuleId, Name, Ty};

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
    /// In order: items by `DefIndex`, each followed by its shims and
    /// closures in creation order; entry thunks in the order given.
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
    /// The path of every def this unit mentions (function keys, callees,
    /// constructors, constants, layouts), sorted by `DefId`.
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
    /// A call whose result is `void` (a foreign function returning `Nil`).
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
    /// The static object of a nullary constructor.
    StaticCtor(DefId),
    /// The static closure of a non-generic top-level function (D-453).
    FnValue(DefId),
    /// The code word `addr | 1` of a function, only as the first field of
    /// a closure [`Rvalue::Alloc`].
    Code(FnKey),
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
            Self::Str(_) | Self::Nil | Self::StaticCtor(_) | Self::FnValue(_) | Self::Code(_) => {
                Repr::Value
            }
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
    /// A raw value in `from` boxed into a uniform word (canonical boxing,
    /// D-278).
    Box { value: Operand, from: Repr },
    /// A uniform word (borrowed) unboxed into `to`.
    Unbox { value: Operand, to: Repr },
    /// Arithmetic that panics on overflow or a zero divisor (D-025,
    /// D-069) at `loc`.
    Checked {
        op: ArithOp,
        lhs: Operand,
        rhs: Operand,
        repr: Repr,
        loc: StrId,
    },
    /// A comparison of two raw values of `repr`, giving a [`Repr::Bool`].
    Compare {
        op: CmpOp,
        lhs: Operand,
        rhs: Operand,
        repr: Repr,
    },
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
            Self::CallRuntime { func, .. } => match *func {},
            Self::Alloc { fields, .. } => operand_uses(fields, UseKind::Owned, &mut out),
            Self::Field { base, .. } | Self::ReadLayout { base } => {
                out.push((*base, UseKind::Borrowed));
            }
            Self::LoadConst(_) => {}
            Self::Box { value, .. } | Self::Unbox { value, .. } => {
                operand_uses(std::slice::from_ref(value), UseKind::Borrowed, &mut out);
            }
            Self::Checked { lhs, rhs, .. } | Self::Compare { lhs, rhs, .. } => {
                operand_uses(&[lhs.clone(), rhs.clone()], UseKind::Borrowed, &mut out);
            }
        }
        out
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

/// A runtime helper called from generated code (F2-F7). Each variant
/// documents the ownership of its arguments and result. None exist in the
/// slice; the enum grows with the runtime.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RuntimeFn {}

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
}

impl PanicKind {
    /// Every kind, in numbering order.
    pub const ALL: [Self; 9] = [
        Self::Explicit,
        Self::Todo,
        Self::Overflow,
        Self::DivisionByZero,
        Self::Assert,
        Self::LetAssert,
        Self::StackOverflow,
        Self::EqualityOnFunction,
        Self::ShiftOutOfRange,
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
        }
    }
}
