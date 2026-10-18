//! Typed bodies (THIR v2, C15): the type checker's output and the only
//! input MIR lowering needs besides interfaces.
//!
//! Every expression carries its closed type, every call its resolved
//! callee, and every interface use its resolved dictionary, so lowering
//! never infers types or searches interfaces. `use`, pipes, captures,
//! holes and parenthesized expressions are already desugared (C7); a
//! positional constructor or an interface function used as a value is a
//! [`ExprKind::Closure`] (D-500). Tail position is not
//! marked here; MIR lowering marks it (§5.15.2).
//!
//! Rules that go with the shape:
//!
//! - A body with any error is never returned, so [`ExprKind::Error`] never
//!   appears in output.
//! - Every [`Pat`] has a closed `ty`. While a body is checked, `ty` is
//!   `Ty::ERROR` and the checker's side table holds the inference type
//!   until `finish` fills it.
//! - `ty_args` of [`ExprKind::Ctor`], [`ExprKind::Record`] and
//!   [`Callee::Ctor`] equal the arguments of the expression's type;
//!   [`Callee::Fn`] and [`ExprKind::FnRef`] `ty_args` follow the callee's
//!   generics order.
//! - A type parameter's type descriptor, for `dyn` conversions and
//!   dictionaries, comes from its first bound: `Dict::Param { param,
//!   bound: 0 }` (D-236, D-499).
//! - Operators on built-in numeric types use [`OpImpl::Builtin`] (§8.7).
//!   `==` and `!=` are always [`OpImpl::Builtin`].
//! - The shape is additive: later phases may add variants and fields, so
//!   code that matches THIR keeps a fallback arm. The enums that MIR
//!   lowering matches ([`ExprKind`], [`Stmt`], [`PatKind`], [`Callee`],
//!   [`Lit`]) are `#[non_exhaustive]`, so other crates must have one.

use cheby_diag::Span;
use cheby_iface::{DefId, IndexVec, ModuleId, Name, ParamIdx, Ty};
use cheby_resolve::LocalId;

cheby_iface::newtype_index! {
    /// An expression of one body.
    pub struct ExprId;
}

cheby_iface::newtype_index! {
    /// A pattern of one body, numbered in the order the checker creates
    /// them.
    pub struct PatId;
}

/// The checked bodies of one module.
#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    pub module: ModuleId,
    /// Functions, in `DefIndex` order.
    pub fns: Vec<FnDef>,
    /// Constants, in `DefIndex` order (C13).
    pub consts: Vec<ConstDef>,
    /// Tests, in `DefIndex` order (G4).
    pub tests: Vec<TestDef>,
}

/// The result of checking one item ([`crate::check_item`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// A top-level function.
    Fn(FnDef),
    /// A module constant.
    Const(ConstDef),
    /// A `test` block.
    Test(TestDef),
}

/// A top-level function.
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
    /// entry, D-137, D-448): what the entry thunk needs that only the
    /// checker can resolve.
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
    /// The constants of any module that the initializer reads directly,
    /// sorted by (module path, name), for evaluation order and cycles.
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
    /// How the test's result is reported (D-140).
    pub entry: EntryInfo,
}

/// How an entry thunk reports the body's result (D-073, D-182, D-140).
#[derive(Clone, Debug, PartialEq)]
pub enum EntryInfo {
    /// The body returns `Nil`.
    Nil,
    /// The body returns `Result<Nil, E>`: an `Err(e)` is printed with
    /// `Show` when `err_ty` satisfies it (`show` is that dictionary), else
    /// with debug printing (D-182).
    Result { err_ty: Ty, show: Option<Dict> },
}

/// A function's native implementation (§12.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct External {
    /// The native symbol.
    pub symbol: Box<str>,
    /// The attribute.
    pub span: Span,
    /// The function carries the standard library's `@std_context`: the
    /// kernel receives the running fiber's context as its first C argument
    /// (D-692).
    pub context: bool,
    /// The function carries `@blocking`: each call runs the foreign
    /// function on the blocking pool while the calling fiber is parked
    /// (D-042, D-683). The resolver never sets it together with `context`
    /// (D-819).
    pub blocking: bool,
}

/// A checked body: a function's block, a constant's initializer or a
/// test's block.
#[derive(Clone, Debug, PartialEq)]
pub struct Body {
    /// The parameters, in order. The ids are the resolver's.
    pub params: Vec<LocalId>,
    /// Every local of the item, nested closures and local functions
    /// included, with its type: the resolver's locals first, with the
    /// same ids, then the checker's synthetic locals.
    pub locals: IndexVec<LocalId, Local>,
    /// Every expression of the item.
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

    /// Every root pattern of the body with where it is written: `let` and
    /// `let assert` statements, `case` arms, closure and local-function
    /// parameters. Ordered by the expression that holds them (by
    /// [`ExprId`]), then in written order. Sub-patterns are reached with
    /// [`Pat::walk`].
    #[must_use]
    pub fn pats(&self) -> Vec<(PatOrigin, &Pat)> {
        self.exprs
            .iter()
            .flat_map(|expr| expr.kind.pats())
            .collect()
    }

    /// Calls `f` on every pattern of the body, sub-patterns included, in
    /// the order of [`Body::pats`] and pre-order within each.
    pub fn for_each_pat_mut(&mut self, f: &mut dyn FnMut(&mut Pat)) {
        for expr in &mut self.exprs {
            for (_, pat) in expr.kind.pats_mut() {
                pat.walk_mut(f);
            }
        }
    }
}

/// A local binding with its type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Local {
    pub name: Name,
    pub ty: Ty,
    /// The binding's name, or for a synthetic local the expression it
    /// holds.
    pub span: Span,
    /// Introduced by desugaring (a capture's argument, a pipe's left side),
    /// not written by the user.
    pub synthetic: bool,
}

/// An expression with its closed type.
#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    /// The expression's type; `Ty::NEVER` for a diverging expression.
    pub ty: Ty,
    pub span: Span,
}

/// What an expression is.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ExprKind {
    /// A literal.
    Lit(Lit),
    /// A local variable.
    Local(LocalId),
    /// A unit constructor as a value: `Nil`, `True`, `None`, a user's unit
    /// variant.
    Ctor { ctor: DefId, ty_args: Box<[Ty]> },
    /// A call. `args` are in evaluation order, which is the written order.
    Call { callee: Callee, args: Box<[ExprId]> },
    /// A block; a missing tail means the value `Nil` (§5.1).
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
    ConstRef { def: DefId },
    /// An anonymous function (C3). Its return type is the body's type in
    /// context.
    Closure { params: Box<[Param]>, body: ExprId },
    /// A local named function (§5.8). Only as a [`Stmt::Expr`], of type
    /// `Nil`; the local's type is the function's type.
    LocalFn {
        local: LocalId,
        params: Box<[Param]>,
        body: ExprId,
    },
    /// A local function's own name inside its body (D-110). MIR lowers it
    /// to the closure's `self`, never to a capture (D-453).
    SelfRef { local: LocalId },
    /// `case` (C5). A subjectless `case` has no subjects and arms with a
    /// guard only, except its last `_` arm, which has neither.
    Case {
        subjects: Box<[ExprId]>,
        arms: Box<[Arm]>,
    },
    /// `x.name`, with the field's index in every constructor of the base
    /// type (C4, D-071).
    Field {
        base: ExprId,
        field: Name,
        index_per_ctor: Box<[(DefId, u32)]>,
    },
    /// A named-field construction. `fields` are in written order, each with
    /// its declaration index.
    Record {
        ctor: DefId,
        ty_args: Box<[Ty]>,
        fields: Box<[(u32, ExprId)]>,
    },
    /// `T { ..base, … }`: a copy of `base` in the same variant, with the
    /// listed fields replaced, in written order (D-156).
    RecordUpdate {
        base: ExprId,
        fields: Box<[UpdateField]>,
    },
    /// A tuple, elements in order.
    Tuple(Box<[ExprId]>),
    /// A list literal, items in order.
    List { items: Box<[ListItem]> },
    /// A binary operator, with how it is implemented.
    Binary {
        op: BinOpKind,
        lhs: ExprId,
        rhs: ExprId,
        imp: OpImpl,
    },
    /// `!x`.
    Not(ExprId),
    /// `-x`, with how it is implemented.
    Neg { operand: ExprId, imp: OpImpl },
    /// An interpolated string (C11).
    Interpolate { parts: Box<[InterpPart]> },
    /// `panic` or `todo`, with an optional message (§5.14).
    Panic {
        kind: PanicKind,
        message: Option<ExprId>,
    },
    /// A conversion to `dyn iface` (C10). `value`'s type satisfies
    /// `iface`, and the expression's type is `dyn iface`.
    DynCoerce {
        value: ExprId,
        iface: DefId,
        dict: Dict,
    },
    /// An expression with a reported error. Never in a returned body.
    Error,
}

impl ExprKind {
    /// The expressions this one contains directly, in evaluation order
    /// (§5.15.1), statements' expressions included. Expressions inside a
    /// closure or local function are its body, evaluated when it is
    /// called; they come in its place in this order.
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
                callee.push_value(&mut out);
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
            Self::Record { fields, .. } => out.extend(fields.iter().map(|&(_, e)| e)),
            Self::RecordUpdate { base, fields } => {
                out.push(*base);
                out.extend(fields.iter().map(|field| field.value));
            }
            Self::Tuple(elems) => out.extend(elems.iter().copied()),
            Self::List { items } => out.extend(items.iter().map(|item| match item {
                ListItem::Elem(e) | ListItem::Spread(e) => *e,
            })),
            Self::Binary { lhs, rhs, .. } => {
                out.push(*lhs);
                out.push(*rhs);
            }
            Self::Not(e) | Self::Neg { operand: e, .. } => out.push(*e),
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

    /// The root patterns this expression holds directly, in written order:
    /// its statements' patterns for a block, its arms' patterns for a
    /// `case`, its parameters' patterns for a closure or local function.
    #[must_use]
    pub fn pats(&self) -> Vec<(PatOrigin, &Pat)> {
        match self {
            Self::Block { stmts, .. } => stmts
                .iter()
                .filter_map(|stmt| match stmt {
                    Stmt::Let { pat, .. } => Some((PatOrigin::Let, pat)),
                    Stmt::LetAssert { pat, .. } => Some((PatOrigin::LetAssert, pat)),
                    Stmt::Expr(_) | Stmt::Assert { .. } => None,
                })
                .collect(),
            Self::Case { arms, .. } => arms
                .iter()
                .flat_map(|arm| arm.pats.iter().map(|pat| (PatOrigin::Arm, pat)))
                .collect(),
            Self::Closure { params, .. } | Self::LocalFn { params, .. } => params
                .iter()
                .map(|param| (PatOrigin::Param(param.origin), &param.pat))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// [`ExprKind::pats`], mutably.
    pub fn pats_mut(&mut self) -> Vec<(PatOrigin, &mut Pat)> {
        match self {
            Self::Block { stmts, .. } => stmts
                .iter_mut()
                .filter_map(|stmt| match stmt {
                    Stmt::Let { pat, .. } => Some((PatOrigin::Let, pat)),
                    Stmt::LetAssert { pat, .. } => Some((PatOrigin::LetAssert, pat)),
                    Stmt::Expr(_) | Stmt::Assert { .. } => None,
                })
                .collect(),
            Self::Case { arms, .. } => arms
                .iter_mut()
                .flat_map(|arm| arm.pats.iter_mut().map(|pat| (PatOrigin::Arm, pat)))
                .collect(),
            Self::Closure { params, .. } | Self::LocalFn { params, .. } => params
                .iter_mut()
                .map(|param| (PatOrigin::Param(param.origin), &mut param.pat))
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Where a root pattern is written ([`Body::pats`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PatOrigin {
    /// `let pat = …`: must be irrefutable.
    Let,
    /// `let assert pat = …`: may be refutable.
    LetAssert,
    /// A `case` arm: may be refutable.
    Arm,
    /// A closure, local-function, `use`, capture or pipe parameter: must be
    /// irrefutable (D-242).
    Param(ParamOrigin),
}

/// A replaced field of a record update.
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateField {
    /// The new value.
    pub value: ExprId,
    /// The field's index in every constructor of the base type (D-071).
    pub index_per_ctor: Box<[(DefId, u32)]>,
}

/// A literal.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Lit {
    /// An integer of any integer type, possibly negative (D-470).
    Int(i128),
    /// A `Float` or `F32`.
    Float(FloatLit),
    /// A string. Escapes decoded, line breaks as LF (D-114).
    Str(Box<str>),
}

/// A float literal's value, rounded once from its decimal text to each
/// float type (§3.3.4, D-496). The `F32` value is not the `Float` value
/// narrowed: that would round twice and can be off by one ulp
/// (D-569).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloatLit {
    /// The nearest `Float`.
    pub wide: f64,
    /// The nearest `F32`.
    pub narrow: f32,
}

/// What a call calls.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Callee {
    /// A top-level function called directly, a type's satisfying function
    /// included.
    Fn {
        def: DefId,
        ty_args: Box<[Ty]>,
        dicts: Box<[Dict]>,
    },
    /// A positional constructor applied to its fields.
    Ctor { ctor: DefId, ty_args: Box<[Ty]> },
    /// An interface function called through a dictionary: a bounded type
    /// parameter, the tuple rule, or `dyn` forwarding.
    Iface { iface_fn: DefId, dict: Dict },
    /// An interface function called on a `dyn I` value, which is `args[0]`
    /// (object safety, D-117).
    Dyn { iface_fn: DefId },
    /// Any other expression of function type, called indirectly. Evaluated
    /// before the arguments (§5.15.1).
    Value(ExprId),
}

impl Callee {
    fn push_value(&self, out: &mut Vec<ExprId>) {
        if let Self::Value(e) = self {
            out.push(*e);
        }
    }
}

/// How an interface is satisfied for one type. The `dicts` of a call or
/// [`ExprKind::FnRef`] hold, for each type parameter of the callee in
/// order, for each of its bounds in written order, one dictionary.
#[derive(Clone, Debug, PartialEq)]
pub enum Dict {
    /// `ty`'s satisfying functions, in `cheby_iface::interface_fns(iface)`
    /// order. Empty for an interface without functions, which every type
    /// satisfies (D-166).
    Impl {
        iface: DefId,
        ty: Ty,
        fns: Box<[DictFn]>,
    },
    /// The enclosing item's own dictionary for bound `bound` of its type
    /// parameter `param`.
    Param { param: ParamIdx, bound: u32 },
    /// The dictionary of `iface`, embedded (transitively) in `of`'s
    /// interface.
    Embedded { of: Box<Dict>, iface: DefId },
    /// `Show` or `Compare` of a tuple by the built-in rule (D-167), from
    /// the elements' dictionaries.
    Tuple {
        iface: DefId,
        ty: Ty,
        elems: Box<[Dict]>,
    },
    /// The dictionary a `dyn J` value carries, where `J` is `iface` or
    /// embeds it (§8.6). `ty` is `dyn J`.
    Dyn { iface: DefId, ty: Ty },
}

/// A satisfying function of a [`Dict::Impl`], instantiated for the
/// dictionary's type (conditional conformance, D-064).
#[derive(Clone, Debug, PartialEq)]
pub struct DictFn {
    pub def: DefId,
    pub ty_args: Box<[Ty]>,
    pub dicts: Box<[Dict]>,
}

/// How an operator is implemented.
#[derive(Clone, Debug, PartialEq)]
pub enum OpImpl {
    /// Built in: arithmetic and comparison on the built-in numeric types,
    /// `&&` and `||` on `Bool`, structural `==` and `!=`, bitwise
    /// operators and shifts on integers.
    Builtin,
    /// A call of the operand type's `add`, `sub`, `mul`, `div`, `neg` or
    /// `compare`; comparisons test the `Order` result (§5.4.4). Never a
    /// [`Callee::Value`].
    Call(Callee),
}

/// A statement of a block.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Stmt {
    /// `let pat = init`.
    Let { pat: Pat, init: ExprId },
    /// An expression whose value is not used.
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

/// A pattern with its closed type.
#[derive(Clone, Debug, PartialEq)]
pub struct Pat {
    pub id: PatId,
    pub kind: PatKind,
    /// The type of the values it matches. `Ty::ERROR` while checking.
    pub ty: Ty,
    pub span: Span,
}

impl Pat {
    /// The sub-patterns this one holds directly, in written order.
    #[must_use]
    pub fn children(&self) -> Vec<&Self> {
        match &self.kind {
            PatKind::Wild | PatKind::Bind(_) | PatKind::Lit(_) => Vec::new(),
            PatKind::Ctor { fields, .. } => fields.iter().map(|(_, p)| p).collect(),
            PatKind::Tuple(pats) | PatKind::Or(pats) => pats.iter().collect(),
            PatKind::List {
                prefix,
                rest,
                suffix,
            } => prefix
                .iter()
                .chain(rest.as_deref())
                .chain(suffix.iter())
                .collect(),
            PatKind::As { pat, .. } => vec![pat],
        }
    }

    /// Calls `f` on this pattern and every sub-pattern, in pre-order.
    pub fn walk(&self, f: &mut dyn FnMut(&Self)) {
        f(self);
        for child in self.children() {
            child.walk(f);
        }
    }

    /// Calls `f` on this pattern and every sub-pattern, in pre-order.
    pub fn walk_mut(&mut self, f: &mut dyn FnMut(&mut Self)) {
        f(self);
        match &mut self.kind {
            PatKind::Wild | PatKind::Bind(_) | PatKind::Lit(_) => {}
            PatKind::Ctor { fields, .. } => {
                for (_, p) in fields.iter_mut() {
                    p.walk_mut(f);
                }
            }
            PatKind::Tuple(pats) | PatKind::Or(pats) => {
                for p in pats.iter_mut() {
                    p.walk_mut(f);
                }
            }
            PatKind::List {
                prefix,
                rest,
                suffix,
            } => {
                for p in prefix.iter_mut() {
                    p.walk_mut(f);
                }
                if let Some(p) = rest {
                    p.walk_mut(f);
                }
                for p in suffix.iter_mut() {
                    p.walk_mut(f);
                }
            }
            PatKind::As { pat, .. } => pat.walk_mut(f),
        }
    }
}

/// What a pattern is.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PatKind {
    /// `_`.
    Wild,
    /// A name.
    Bind(LocalId),
    /// An `Int` or `String` literal; there are no float patterns (D-113).
    Lit(Lit),
    /// A constructor with its fields' patterns, by declaration index:
    /// every field for a positional constructor, the listed fields for a
    /// named one.
    Ctor {
        ctor: DefId,
        fields: Box<[(u32, Pat)]>,
    },
    /// A tuple pattern.
    Tuple(Box<[Pat]>),
    /// `[a, ..rest, z]`: the elements before the spread, the spread if
    /// written (`None` for no spread; `Wild` or `Bind` of the list type
    /// otherwise, D-158), and the elements after it.
    List {
        prefix: Box<[Pat]>,
        rest: Option<Box<Pat>>,
        suffix: Box<[Pat]>,
    },
    /// Alternatives `a | b` (§6.7).
    Or(Box<[Pat]>),
    /// `pat as name` (§6.8).
    As { pat: Box<Pat>, local: LocalId },
}

/// A closure or local-function parameter; its type is `pat.ty`.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub pat: Pat,
    pub span: Span,
    /// What the parameter was written as.
    pub origin: ParamOrigin,
}

/// Where a parameter comes from: written, or made by desugaring (C7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParamOrigin {
    /// A closure's parameter.
    Closure,
    /// A local function's parameter.
    LocalFn,
    /// A `use` binder (§5.10).
    Use,
    /// The parameter of a function capture `f(a, _)` (D-155).
    Capture,
    /// A parameter made for a pipe.
    Pipe,
}

/// An arm of a `case`. A subjectless `case` has arms with no patterns and
/// its condition as the guard; its last `_` arm has neither.
#[derive(Clone, Debug, PartialEq)]
pub struct Arm {
    /// One pattern per subject.
    pub pats: Box<[Pat]>,
    pub guard: Option<ExprId>,
    pub body: ExprId,
    /// The whole arm.
    pub span: Span,
}

/// An element of a list literal.
#[derive(Clone, Debug, PartialEq)]
pub enum ListItem {
    /// One element.
    Elem(ExprId),
    /// `..xs`: every element of a list (D-157).
    Spread(ExprId),
}

/// A part of an interpolated string.
#[derive(Clone, Debug, PartialEq)]
pub enum InterpPart {
    /// Literal text, escapes decoded.
    Text(Box<str>),
    /// `{x}`, printed with `show` (D-086), or `{x:?}` with debug printing,
    /// where `show` is `None`.
    Value {
        expr: ExprId,
        debug: bool,
        show: Option<Callee>,
    },
}

/// Why a [`ExprKind::Panic`] panics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PanicKind {
    /// `panic`.
    Panic,
    /// `todo`.
    Todo,
}

/// A binary operator after desugaring (pipes are calls).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinOpKind {
    /// `||`.
    Or,
    /// `&&`.
    And,
    /// `==`.
    Eq,
    /// `!=`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// `|`.
    BitOr,
    /// `^`.
    BitXor,
    /// `&`.
    BitAnd,
    /// `<<`.
    Shl,
    /// `>>`.
    Shr,
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
    /// `%`.
    Rem,
}

#[cfg(test)]
mod tests {
    use cheby_diag::{FileId, TextRange, TextSize};
    use cheby_iface::{Idx, sym};

    use super::*;

    fn span(at: u32) -> Span {
        Span::new(FileId::new(0), TextRange::empty(TextSize::from(at)))
    }

    fn pat(id: u32, kind: PatKind) -> Pat {
        Pat {
            id: PatId::from_u32(id),
            kind,
            ty: Ty::ERROR,
            span: span(id),
        }
    }

    fn expr(kind: ExprKind) -> Expr {
        Expr {
            kind,
            ty: Ty::NIL,
            span: span(0),
        }
    }

    fn e(n: u32) -> ExprId {
        ExprId::from_u32(n)
    }

    fn l(n: u32) -> LocalId {
        LocalId::from_u32(n)
    }

    /// `fn(p, (a, _)) { let x = s; let assert [h, ..t] = s; case s { y | y
    /// => f(y) } }` built by hand.
    fn body() -> Body {
        let exprs = vec![
            expr(ExprKind::Lit(Lit::Str("s".into()))), // 0
            expr(ExprKind::Local(l(0))),               // 1
            expr(ExprKind::Local(l(4))),               // 2
            expr(ExprKind::Call {
                callee: Callee::Value(e(1)),
                args: Box::new([e(2), e(0)]),
            }), // 3
            expr(ExprKind::Case {
                subjects: Box::new([e(0)]),
                arms: Box::new([Arm {
                    pats: Box::new([pat(
                        6,
                        PatKind::Or(Box::new([
                            pat(7, PatKind::Bind(l(4))),
                            pat(8, PatKind::Bind(l(4))),
                        ])),
                    )]),
                    guard: Some(e(1)),
                    body: e(3),
                    span: span(4),
                }]),
            }), // 4
            expr(ExprKind::Block {
                stmts: Box::new([
                    Stmt::Let {
                        pat: pat(2, PatKind::Bind(l(1))),
                        init: e(0),
                    },
                    Stmt::Assert {
                        cond: e(1),
                        message: Some(e(0)),
                    },
                    Stmt::LetAssert {
                        pat: pat(
                            3,
                            PatKind::List {
                                prefix: Box::new([pat(4, PatKind::Bind(l(2)))]),
                                rest: Some(Box::new(pat(5, PatKind::Bind(l(3))))),
                                suffix: Box::new([]),
                            },
                        ),
                        init: e(2),
                        message: None,
                    },
                ]),
                tail: Some(e(4)),
            }), // 5
            expr(ExprKind::Closure {
                params: Box::new([
                    Param {
                        pat: pat(0, PatKind::Bind(l(5))),
                        span: span(0),
                        origin: ParamOrigin::Closure,
                    },
                    Param {
                        pat: pat(
                            1,
                            PatKind::Tuple(Box::new([
                                pat(9, PatKind::Bind(l(6))),
                                pat(10, PatKind::Wild),
                            ])),
                        ),
                        span: span(1),
                        origin: ParamOrigin::Use,
                    },
                ]),
                body: e(5),
            }), // 6
        ];
        let locals = (0..7)
            .map(|n| Local {
                name: sym::T,
                ty: Ty::STRING,
                span: span(n),
                synthetic: n == 6,
            })
            .collect();
        Body {
            params: Vec::new(),
            locals: IndexVec::from_raw(locals).unwrap(),
            exprs: IndexVec::from_raw(exprs).unwrap(),
            value: e(6),
            ret: Ty::NIL,
        }
    }

    #[test]
    fn children_follow_evaluation_order() {
        let body = body();
        let kids = |n: u32| body.expr(e(n)).unwrap().kind.children();
        assert_eq!(kids(0), []);
        // The callee value first, then the arguments.
        assert_eq!(kids(3), [e(1), e(2), e(0)]);
        // Subjects, then each arm's guard and body.
        assert_eq!(kids(4), [e(0), e(1), e(3)]);
        // Statements' expressions in order, then the tail.
        assert_eq!(kids(5), [e(0), e(1), e(0), e(2), e(4)]);
        assert_eq!(kids(6), [e(5)]);
        let update = ExprKind::RecordUpdate {
            base: e(9),
            fields: Box::new([
                UpdateField {
                    value: e(7),
                    index_per_ctor: Box::new([]),
                },
                UpdateField {
                    value: e(8),
                    index_per_ctor: Box::new([]),
                },
            ]),
        };
        assert_eq!(update.children(), [e(9), e(7), e(8)]);
        let neg = ExprKind::Neg {
            operand: e(2),
            imp: OpImpl::Builtin,
        };
        assert_eq!(neg.children(), [e(2)]);
    }

    #[test]
    fn pats_cover_every_site() {
        let mut body = body();
        let roots: Vec<(PatOrigin, u32)> = body
            .pats()
            .into_iter()
            .map(|(origin, p)| (origin, p.id.as_u32()))
            .collect();
        assert_eq!(
            roots,
            [
                (PatOrigin::Arm, 6),
                (PatOrigin::Let, 2),
                (PatOrigin::LetAssert, 3),
                (PatOrigin::Param(ParamOrigin::Closure), 0),
                (PatOrigin::Param(ParamOrigin::Use), 1),
            ]
        );
        let mut all = Vec::new();
        for (_, root) in body.pats() {
            root.walk(&mut |p| all.push(p.id.as_u32()));
        }
        assert_eq!(all, [6, 7, 8, 2, 3, 4, 5, 0, 1, 9, 10]);
        // The mutable walk reaches the same patterns in the same order.
        let mut seen = Vec::new();
        body.for_each_pat_mut(&mut |p| {
            seen.push(p.id.as_u32());
            p.ty = Ty::STRING;
        });
        assert_eq!(seen, all);
        for (_, root) in body.pats() {
            root.walk(&mut |p| assert_eq!(p.ty, Ty::STRING));
        }
        assert_eq!(body.local(l(6)).map(|local| local.synthetic), Some(true));
        let idx: usize = PatId::from_u32(3).index();
        assert_eq!(idx, 3);
    }
}
