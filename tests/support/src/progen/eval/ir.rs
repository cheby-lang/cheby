//! The evaluator's own form of a program: what [`super::lower`] makes of
//! the syntax tree. Every name is resolved here, so evaluation never
//! looks at the tree again, and a construct outside the generator's
//! subset is refused before anything runs.

use std::rc::Rc;

/// A name, shared between the program and the values that hold it.
pub(super) type Name = Rc<str>;

/// Where a panic starts: a file of the program and a byte offset in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Loc {
    /// The index in [`Program::files`].
    pub(super) file: usize,
    pub(super) offset: u32,
}

/// A constructor: a variant of a sum type, the one constructor of a
/// record type (named after the type, §4.3.3), or one of the prelude's
/// `Some`, `None`, `Ok` and `Err`.
#[derive(Debug)]
pub(super) struct CtorDef {
    pub(super) name: Name,
    pub(super) shape: CtorShape,
}

/// The fields a constructor takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CtorShape {
    Unit,
    /// That many positional fields.
    Positional(usize),
    /// Named fields, in declaration order.
    Named(Vec<Name>),
}

/// A standard-library function the generator calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StdFn {
    ListLength,
    ListSum,
    ListIsEmpty,
    ListContains,
    ListFilter,
    ListReverse,
    ListDrop,
    ListTake,
    ListRange,
    ListSort,
    ListUnique,
    ListMap,
    ListFold,
    ListFirst,
    ListGet,
    StringLength,
    StringSlice,
    StringUppercase,
    StringLowercase,
    StringReverse,
    StringTrim,
    StringJoin,
    StringRepeat,
    StringSplit,
    StringStartsWith,
    StringContains,
    StringEndsWith,
    MapNew,
    MapFromList,
    MapInsert,
    MapRemove,
    MapGet,
    MapSize,
    MapToSortedList,
    IntMin,
    IntMax,
    IoPrintln,
    FiberScope,
    FiberMessage,
}

impl StdFn {
    /// The function `module::name` of the standard library, if the
    /// evaluator knows it.
    pub(super) fn lookup(module: &str, name: &str) -> Option<Self> {
        let f = match (module, name) {
            ("list", "length") => Self::ListLength,
            ("list", "sum") => Self::ListSum,
            ("list", "is_empty") => Self::ListIsEmpty,
            ("list", "contains") => Self::ListContains,
            ("list", "filter") => Self::ListFilter,
            ("list", "reverse") => Self::ListReverse,
            ("list", "drop") => Self::ListDrop,
            ("list", "take") => Self::ListTake,
            ("list", "range") => Self::ListRange,
            ("list", "sort") => Self::ListSort,
            ("list", "unique") => Self::ListUnique,
            ("list", "map") => Self::ListMap,
            ("list", "fold") => Self::ListFold,
            ("list", "first") => Self::ListFirst,
            ("list", "get") => Self::ListGet,
            ("string", "length") => Self::StringLength,
            ("string", "slice") => Self::StringSlice,
            ("string", "uppercase") => Self::StringUppercase,
            ("string", "lowercase") => Self::StringLowercase,
            ("string", "reverse") => Self::StringReverse,
            ("string", "trim") => Self::StringTrim,
            ("string", "join") => Self::StringJoin,
            ("string", "repeat") => Self::StringRepeat,
            ("string", "split") => Self::StringSplit,
            ("string", "starts_with") => Self::StringStartsWith,
            ("string", "contains") => Self::StringContains,
            ("string", "ends_with") => Self::StringEndsWith,
            ("map", "new") => Self::MapNew,
            ("map", "from_list") => Self::MapFromList,
            ("map", "insert") => Self::MapInsert,
            ("map", "remove") => Self::MapRemove,
            ("map", "get") => Self::MapGet,
            ("map", "size") => Self::MapSize,
            ("map", "to_sorted_list") => Self::MapToSortedList,
            ("int", "min") => Self::IntMin,
            ("int", "max") => Self::IntMax,
            ("io", "println") => Self::IoPrintln,
            ("fiber", "scope") => Self::FiberScope,
            ("fiber", "message") => Self::FiberMessage,
            _ => return None,
        };
        Some(f)
    }
}

/// A name that is not a local: a top-level function, a standard-library
/// function, a constructor or `True` and `False`.
#[derive(Debug, Clone)]
pub(super) enum Global {
    /// The index in [`Program::fns`].
    Fn(usize),
    Std(StdFn),
    Ctor(Rc<CtorDef>),
    Bool(bool),
}

/// A top-level function.
#[derive(Debug)]
pub(super) struct FnDef {
    /// `module::name`, for messages.
    pub(super) name: String,
    pub(super) params: Vec<Pat>,
    pub(super) body: Block,
}

/// An anonymous function or a local named function (§5.7, §5.8).
#[derive(Debug)]
pub(super) struct ClosureDef {
    pub(super) params: Vec<Pat>,
    pub(super) body: Block,
}

/// A block: its value is that of its last statement when that is an
/// expression, else `Nil` (§5.1).
#[derive(Debug, Default)]
pub(super) struct Block {
    pub(super) stmts: Vec<Stmt>,
}

#[derive(Debug)]
pub(super) enum Stmt {
    Let {
        pat: Pat,
        value: Expr,
    },
    /// `let assert` (§11.4), located at the start of its pattern.
    LetAssert {
        pat: Pat,
        value: Expr,
        message: Option<Expr>,
        loc: Loc,
    },
    /// `assert` (§11.5): the condition's source text, and its location.
    Assert {
        cond: Expr,
        message: Option<Expr>,
        text: String,
        loc: Loc,
    },
    /// A local named function, visible in its own body and after it.
    LocalFn {
        name: Name,
        def: Rc<ClosureDef>,
    },
    Expr(Expr),
}

/// A binary operator of the subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl Op {
    /// Whether the operator is a comparison, whose failing `assert` shows
    /// both operands (§11.5).
    pub(super) fn is_comparison(self) -> bool {
        matches!(
            self,
            Self::Eq | Self::Ne | Self::Lt | Self::Le | Self::Gt | Self::Ge
        )
    }
}

/// A part of a string literal.
#[derive(Debug)]
pub(super) enum Part {
    Text(String),
    /// `{root.field…}` or `{root.field…:?}` (§5.3.2).
    Value {
        root: Name,
        fields: Vec<Name>,
        debug: bool,
    },
}

/// An item of a list literal (§5.13).
#[derive(Debug)]
pub(super) enum Item {
    One(Expr),
    Spread(Expr),
}

/// An arm of a `case` with subjects.
#[derive(Debug)]
pub(super) struct Arm {
    pub(super) pats: Vec<Pat>,
    pub(super) guard: Option<Expr>,
    pub(super) body: Expr,
}

#[derive(Debug)]
pub(super) enum Expr {
    Int(i64),
    Str(Name),
    Interp(Vec<Part>),
    Local(Name),
    Global(Global),
    /// `C { f: e, … }`: the fields as written, in the order written.
    Construct {
        ctor: Rc<CtorDef>,
        fields: Vec<(Name, Expr)>,
    },
    /// `T { ..base, f: e, … }` (§5.11.2).
    Update {
        base: Box<Expr>,
        fields: Vec<(Name, Expr)>,
    },
    Tuple(Vec<Expr>),
    List(Vec<Item>),
    Block(Block),
    Closure(Rc<ClosureDef>),
    Case {
        subjects: Vec<Expr>,
        arms: Vec<Arm>,
    },
    /// A `case` without a subject (§5.9.5).
    Cond {
        arms: Vec<(Expr, Expr)>,
        default: Option<Box<Expr>>,
    },
    /// `panic` (`todo: false`) or `todo` (`todo: true`), with its message.
    Panic {
        todo: bool,
        message: Option<Box<Expr>>,
        loc: Loc,
    },
    Binary {
        op: Op,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        loc: Loc,
    },
    Not(Box<Expr>),
    Neg {
        operand: Box<Expr>,
        loc: Loc,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    Field {
        receiver: Box<Expr>,
        field: Name,
    },
}

/// The fields of a constructor pattern.
#[derive(Debug)]
pub(super) enum PatFields {
    /// `C`.
    None,
    /// `C(p, …)`.
    Positional(Vec<Pat>),
    /// `C { f: p, …, .. }`: the fields listed.
    Named(Vec<(Name, Pat)>),
}

#[derive(Debug)]
pub(super) enum Pat {
    Wild,
    Bind(Name),
    Int(i64),
    Str(Name),
    Bool(bool),
    Ctor {
        ctor: Rc<CtorDef>,
        fields: PatFields,
    },
    Tuple(Vec<Pat>),
    /// `[p, …, ..rest, q, …]`: the patterns before the rest, the rest, and
    /// the patterns after it.
    List {
        before: Vec<Pat>,
        rest: ListRest,
        after: Vec<Pat>,
    },
    Or(Vec<Pat>),
    As(Box<Pat>, Name),
}

/// The rest of a list pattern (§6.6).
#[derive(Debug)]
pub(super) enum ListRest {
    /// No rest: the list has exactly as many elements as patterns.
    No,
    /// `..`.
    Anonymous,
    /// `..name`, bound to the elements between the patterns.
    Bind(Name),
}

/// A program ready to run.
#[derive(Debug)]
pub(super) struct Program {
    /// Each module's path relative to the package (`src/main.cheby`) and
    /// its text, for locations.
    pub(super) files: Vec<(String, String)>,
    pub(super) fns: Vec<FnDef>,
    /// The index of `main` in [`Self::fns`].
    pub(super) main: usize,
}
