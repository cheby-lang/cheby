//! Typed bodies (THIR, C15): the type checker's output and the only input
//! MIR lowering needs besides interfaces.
//!
//! Every expression carries its closed type, every call its resolved
//! callee, and every interface use its resolved dictionary, so lowering
//! never infers types or searches interfaces. `use`, pipes and captures
//! are already desugared to calls and closures (C7). Tail position is not
//! marked here; MIR lowering marks it (§5.15.2).
//!
//! The shape is complete for M1. The vertical slice produces only the
//! variants marked _slice_; the others are produced by later phases, and
//! MIR lowering may treat them as an internal error until then.

use cheby_diag::Span;
use cheby_iface::{DefId, IndexVec, ModuleId, Name, ParamIdx, Ty};
use cheby_resolve::LocalId;

cheby_iface::newtype_index! {
    /// An expression of one body.
    pub struct ExprId;
}

/// The checked bodies of one module.
#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    pub module: ModuleId,
    /// Functions, in `DefIndex` order. _Slice._
    pub fns: Vec<FnDef>,
    /// Constants, in `DefIndex` order (C13).
    pub consts: Vec<ConstDef>,
    /// Tests, in `DefIndex` order (G4).
    pub tests: Vec<TestDef>,
}

/// The result of checking one item ([`crate::check_item`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Fn(FnDef),
    Const(ConstDef),
    Test(TestDef),
}

/// A top-level function. _Slice._
#[derive(Clone, Debug, PartialEq)]
pub struct FnDef {
    pub def: DefId,
    /// The function's name.
    pub span: Span,
    /// `@external(native, "symbol")`, which replaces the body on native
    /// (§12.5, D-207).
    pub native_external: Option<External>,
    /// The checked body. `None` for a function with only externals.
    pub body: Option<Body>,
    /// Set for every top-level `main` (any module's `main` can be the
    /// entry, D-137): what the entry thunk needs that only the checker can
    /// resolve.
    pub entry: Option<EntryInfo>,
}

/// A module constant (C13).
#[derive(Clone, Debug, PartialEq)]
pub struct ConstDef {
    pub def: DefId,
    /// The constant's name.
    pub span: Span,
    /// The initializer, with no parameters.
    pub body: Body,
    /// The constants the initializer reads, for evaluation order.
    pub deps: Box<[DefId]>,
}

/// A `test` block (G4).
#[derive(Clone, Debug, PartialEq)]
pub struct TestDef {
    pub def: DefId,
    /// The test's name, as written (escapes decoded).
    pub name: Box<str>,
    /// The test's name literal.
    pub span: Span,
    pub body: Body,
    pub entry: EntryInfo,
}

/// How an entry thunk reports the body's result (D-073, D-182, D-140).
#[derive(Clone, Debug, PartialEq)]
pub enum EntryInfo {
    /// The body returns `Nil`. _Slice._
    Nil,
    /// The body returns `Result<Nil, E>`: an `Err(e)` is printed with
    /// `Show` when `E` satisfies it (the resolved dictionary), else with
    /// debug printing (D-182).
    Result { err_ty: Ty, show: Option<Dict> },
}

/// A function's native implementation (§12.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct External {
    pub symbol: Box<str>,
    /// The attribute.
    pub span: Span,
}

/// A checked body: a function's block, a constant's initializer or a
/// test's block.
#[derive(Clone, Debug, PartialEq)]
pub struct Body {
    /// The parameters, in order. The ids are the resolver's.
    pub params: Vec<LocalId>,
    /// Every local of the item, nested closures and local functions
    /// included, with its type.
    pub locals: IndexVec<LocalId, Local>,
    pub exprs: IndexVec<ExprId, Expr>,
    /// The body's block or initializer.
    pub value: ExprId,
    /// The declared result type.
    pub ret: Ty,
}

impl Body {
    /// The expression `id`.
    #[must_use]
    pub fn expr(&self, id: ExprId) -> Option<&Expr> {
        self.exprs.get(id)
    }

    /// The local `id`.
    #[must_use]
    pub fn local(&self, id: LocalId) -> Option<&Local> {
        self.locals.get(id)
    }
}

/// A local binding with its type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Local {
    pub name: Name,
    pub ty: Ty,
    /// The binding's name.
    pub span: Span,
}

/// An expression with its closed type.
#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub ty: Ty,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    /// _Slice:_ [`Lit::Str`].
    Lit(Lit),
    /// A local variable. _Slice._
    Local(LocalId),
    /// A unit constructor as a value. _Slice:_ `Nil`.
    Ctor {
        ctor: DefId,
        ty_args: Box<[Ty]>,
    },
    /// A call. _Slice:_ [`Callee::Fn`].
    Call {
        callee: Callee,
        args: Box<[ExprId]>,
    },
    /// A block; a missing tail means the value `Nil` (§5.1). _Slice._
    Block {
        stmts: Box<[Stmt]>,
        tail: Option<ExprId>,
    },
    /// A top-level function as a value.
    FnRef {
        def: DefId,
        ty_args: Box<[Ty]>,
        dicts: Box<[Dict]>,
    },
    /// A module constant, read from its constant slot (C13, E6).
    ConstRef {
        def: DefId,
    },
    /// An anonymous function (C3, D4).
    Closure {
        params: Box<[Param]>,
        body: ExprId,
    },
    /// A local named function, as a statement-level declaration (§5.8).
    LocalFn {
        local: LocalId,
        params: Box<[Param]>,
        body: ExprId,
    },
    /// A local function's own name inside its body (D-110). MIR lowers it
    /// to the closure's `self`, never to a capture (D-453).
    SelfRef {
        local: LocalId,
    },
    /// `case` with subjects, or subjectless (C5, C6).
    Case {
        subjects: Box<[ExprId]>,
        arms: Box<[Arm]>,
    },
    /// `x.name`, with the field's index in each constructor that has it
    /// (C4, D-071).
    Field {
        base: ExprId,
        field: Name,
        index_per_ctor: Box<[(DefId, u32)]>,
    },
    /// A record construction or update, fields by declaration index.
    Record {
        ctor: DefId,
        ty_args: Box<[Ty]>,
        fields: Box<[(u32, ExprId)]>,
        spread: Option<ExprId>,
    },
    Tuple(Box<[ExprId]>),
    List {
        items: Box<[ListItem]>,
    },
    /// A binary operator; `dict` for operators resolved through an
    /// interface (§8.7).
    Binary {
        op: BinOpKind,
        lhs: ExprId,
        rhs: ExprId,
        dict: Option<Dict>,
    },
    Not(ExprId),
    Neg(ExprId),
    /// An interpolated string (C11).
    Interpolate {
        parts: Box<[InterpPart]>,
    },
    /// `panic` or `todo`, with an optional message (D9).
    Panic {
        kind: PanicKind,
        message: Option<ExprId>,
    },
    /// A conversion to `dyn I` (C10).
    DynCoerce {
        value: ExprId,
        iface: DefId,
        dict: Dict,
    },
    /// An expression with a reported error. Never reaches MIR: a body with
    /// an error is not returned.
    Error,
}

impl ExprKind {
    /// The expressions this one contains directly, in evaluation order
    /// (§5.15.1), statements' expressions included.
    #[must_use]
    pub fn children(&self) -> Vec<ExprId> {
        let mut out = Vec::new();
        match self {
            Self::Lit(_)
            | Self::Local(_)
            | Self::Ctor { .. }
            | Self::FnRef { .. }
            | Self::ConstRef { .. }
            | Self::SelfRef { .. }
            | Self::Error => {}
            Self::Call { callee, args } => {
                if let Callee::Value(callee) = callee {
                    out.push(*callee);
                }
                out.extend(args.iter().copied());
            }
            Self::Block { stmts, tail } => {
                for stmt in stmts {
                    match stmt {
                        Stmt::Let { init, .. } | Stmt::Expr(init) => out.push(*init),
                        Stmt::Assert { cond, message } => {
                            out.push(*cond);
                            out.extend(*message);
                        }
                        Stmt::LetAssert { init, message, .. } => {
                            out.push(*init);
                            out.extend(*message);
                        }
                    }
                }
                out.extend(*tail);
            }
            Self::Closure { body, .. } | Self::LocalFn { body, .. } => out.push(*body),
            Self::Case { subjects, arms } => {
                out.extend(subjects.iter().copied());
                for arm in arms {
                    out.extend(arm.guard);
                    out.push(arm.body);
                }
            }
            Self::Field { base, .. } => out.push(*base),
            Self::Record { fields, spread, .. } => {
                out.extend(*spread);
                out.extend(fields.iter().map(|&(_, e)| e));
            }
            Self::Tuple(elems) => out.extend(elems.iter().copied()),
            Self::List { items } => out.extend(items.iter().map(|item| match item {
                ListItem::Elem(e) | ListItem::Spread(e) => *e,
            })),
            Self::Binary { lhs, rhs, .. } => {
                out.push(*lhs);
                out.push(*rhs);
            }
            Self::Not(e) | Self::Neg(e) => out.push(*e),
            Self::Interpolate { parts } => {
                out.extend(parts.iter().filter_map(|part| match part {
                    InterpPart::Text(_) => None,
                    InterpPart::Value { expr, .. } => Some(*expr),
                }));
            }
            Self::Panic { message, .. } => out.extend(*message),
            Self::DynCoerce { value, .. } => out.push(*value),
        }
        out
    }
}

/// A literal.
#[derive(Clone, Debug, PartialEq)]
pub enum Lit {
    Int(i128),
    Float(f64),
    /// _Slice._ Escapes decoded, line breaks as LF (D-114).
    Str(Box<str>),
}

/// What a call calls.
#[derive(Clone, Debug, PartialEq)]
pub enum Callee {
    /// A known top-level function, called directly. _Slice._
    Fn {
        def: DefId,
        ty_args: Box<[Ty]>,
        dicts: Box<[Dict]>,
    },
    /// A positional constructor applied to its fields.
    Ctor { ctor: DefId, ty_args: Box<[Ty]> },
    /// Any other expression of function type, called indirectly.
    Value(ExprId),
}

/// A statement of a block.
#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    /// `let pat = init`. _Slice:_ [`Pat::Bind`] and [`Pat::Wild`].
    Let { pat: Pat, init: ExprId },
    /// An expression whose value is not used. _Slice._
    Expr(ExprId),
    /// `assert cond as message` (D-160).
    Assert {
        cond: ExprId,
        message: Option<ExprId>,
    },
    /// `let assert pat = init as message` (§11.4).
    LetAssert {
        pat: Pat,
        init: ExprId,
        message: Option<ExprId>,
    },
}

/// A pattern.
#[derive(Clone, Debug, PartialEq)]
pub enum Pat {
    /// A name. _Slice._
    Bind(LocalId),
    /// `_`. _Slice._
    Wild,
    /// A constructor with its fields' patterns, by declaration index.
    Ctor {
        ctor: DefId,
        ty_args: Box<[Ty]>,
        fields: Box<[(u32, Pat)]>,
    },
    Tuple(Box<[Pat]>),
    Lit(Lit),
    /// `[a, b, ..rest]`: the leading elements, then the rest pattern if
    /// written (`..` alone is `Some(Wild)`).
    List {
        prefix: Box<[Pat]>,
        rest: Option<Box<Pat>>,
    },
    Or(Box<[Pat]>),
    /// `pat as name`.
    As {
        pat: Box<Pat>,
        local: LocalId,
    },
}

/// A closure or local-function parameter.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub pat: Pat,
    pub ty: Ty,
    pub span: Span,
}

/// An arm of a `case`. A subjectless `case` has arms with no patterns and
/// its condition as the guard; its `_` arm has neither.
#[derive(Clone, Debug, PartialEq)]
pub struct Arm {
    pub pats: Box<[Pat]>,
    pub guard: Option<ExprId>,
    pub body: ExprId,
}

/// An element of a list literal.
#[derive(Clone, Debug, PartialEq)]
pub enum ListItem {
    Elem(ExprId),
    Spread(ExprId),
}

/// A part of an interpolated string.
#[derive(Clone, Debug, PartialEq)]
pub enum InterpPart {
    Text(Box<str>),
    /// `{x}` with `Show`, or `{x:?}` with debug printing (D-086).
    Value {
        expr: ExprId,
        debug: bool,
        dict: Option<Dict>,
    },
}

/// Why a `panic` expression panics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PanicKind {
    Panic,
    Todo,
}

/// A binary operator after desugaring (pipes are calls).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinOpKind {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    BitOr,
    BitXor,
    BitAnd,
    Shl,
    Shr,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// A resolved implementation of an interface for a bound (C9). Always
/// empty in the slice. Fully resolved by the checker, so MIR lowering
/// never searches interfaces (C15).
#[derive(Clone, Debug, PartialEq)]
pub enum Dict {
    /// `fns` are the satisfying module functions in the interface's
    /// function order (embedded interfaces' functions first, in embedding
    /// order), and `args` are the dictionaries those functions' own bounds
    /// need (conditional conformance, D-064).
    Impl {
        iface: DefId,
        ty: Ty,
        fns: Box<[DefId]>,
        args: Box<[Dict]>,
    },
    /// The dictionary passed to the enclosing top-level function for
    /// bound number `bound` of its type parameter `param`.
    Param { param: ParamIdx, bound: u32 },
    /// An embedded interface's dictionary taken out of another one (C8).
    Embedded { of: Box<Dict>, index: u32 },
    /// Runtime-provided implementations.
    Builtin { iface: DefId, ty: Ty },
}
