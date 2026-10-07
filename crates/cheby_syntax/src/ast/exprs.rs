//! Views of expressions (App. A.5, ch. 5) and string literals (§2.5.3,
//! §2.5.4).

use cheby_diag::TextRange;

use super::{
    AstChildren, AstNode, AstToken, AstTokens, Block, FloatLit, Ident, IntLit, Message, Param,
    ParamList, Pat, PathNode, RawStringLit, RetType, StringEscape, StringText, TypeArgList, child,
    child_after, child_before, child_token, children, token,
};
use crate::lexer::literal;
use crate::{SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};

ast_nodes! {
    /// A number or raw string literal (§5.3).
    Literal = LITERAL;
    /// A string literal with its text, escapes and interpolations (§2.5.3).
    StringLit = STRING_LIT;
    /// `{name.field:?}` in a string (§2.5.4, D-115).
    Interpolation = INTERPOLATION;
    /// A path used as a value, with an optional turbofish (§5.17).
    PathExpr = PATH_EXPR;
    /// A record construction `P { x: 1 }` or update `P { ..p, x: 1 }`
    /// (§5.11).
    RecordExpr = RECORD_EXPR;
    /// `name: expr`, or `name` punned, in a record expression.
    FieldInit = FIELD_INIT;
    /// `..expr` in a list or a record update.
    Spread = SPREAD;
    /// A tuple `(a, b)` (§5.12).
    TupleExpr = TUPLE_EXPR;
    /// A parenthesized expression `(e)`.
    ParenExpr = PAREN_EXPR;
    /// A list literal `[a, ..xs]` (§5.13).
    ListExpr = LIST_EXPR;
    /// An anonymous function `fn(x) { … }` (§5.7).
    Closure = CLOSURE;
    /// A `case`, with or without subjects (§5.9).
    CaseExpr = CASE_EXPR;
    /// The `{ arm … }` body of a `case`.
    CaseBody = CASE_BODY;
    /// An arm of a `case` with subjects: `p, q when g => e`.
    CaseArm = CASE_ARM;
    /// `when expr`, the guard of a case arm (§5.9.3).
    Guard = GUARD;
    /// An arm of a subjectless `case`: `cond => e` or `_ => e` (§5.9.5).
    CondArm = COND_ARM;
    /// `panic` or `panic as msg` (§5.14).
    PanicExpr = PANIC_EXPR;
    /// `todo` or `todo as msg` (§5.14).
    TodoExpr = TODO_EXPR;
    /// A binary operation `a op b` (§5.4).
    BinExpr = BIN_EXPR;
    /// `-e` or `!e`.
    PrefixExpr = PREFIX_EXPR;
    /// A call `f(a, b)` (§5.5).
    CallExpr = CALL_EXPR;
    /// The `(a, _, c)` arguments of a call.
    ArgList = ARG_LIST;
    /// `_`, a capture hole when it is a whole call argument (§5.6).
    Hole = HOLE;
    /// Field access `x.name` (§5.11.3).
    FieldExpr = FIELD_EXPR;
}

ast_enum! {
    /// An expression (App. A.5).
    Expr {
        Literal(Literal),
        String(StringLit),
        Path(PathExpr),
        Record(RecordExpr),
        Tuple(TupleExpr),
        Paren(ParenExpr),
        List(ListExpr),
        Block(Block),
        Closure(Closure),
        Case(CaseExpr),
        Panic(PanicExpr),
        Todo(TodoExpr),
        Binary(BinExpr),
        Prefix(PrefixExpr),
        Call(CallExpr),
        /// A `_` that is not a whole call argument, which is an error
        /// (E0025). Holes in argument lists are [`Arg::Hole`].
        Hole(Hole),
        Field(FieldExpr),
    }
}

ast_enum! {
    /// An arm of a `case`.
    CaseArmKind {
        Case(CaseArm),
        Cond(CondArm),
    }
}

/// The value of a literal expression.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LiteralKind<'t> {
    Int(IntLit<'t>),
    Float(FloatLit<'t>),
    RawString(RawStringLit<'t>),
}

impl<'t> Literal<'t> {
    /// The literal token.
    #[must_use]
    pub fn kind(self) -> Option<LiteralKind<'t>> {
        self.0.children_with_tokens().find_map(|child| {
            let token = child.as_token()?;
            IntLit::cast(token)
                .map(LiteralKind::Int)
                .or_else(|| FloatLit::cast(token).map(LiteralKind::Float))
                .or_else(|| RawStringLit::cast(token).map(LiteralKind::RawString))
        })
    }
}

/// A part of a string literal, in source order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StringPart<'t> {
    Text(StringText<'t>),
    Escape(StringEscape<'t>),
    Interpolation(Interpolation<'t>),
}

impl<'t> StringLit<'t> {
    /// The text, escapes and interpolations, in order.
    pub fn parts(self) -> impl Iterator<Item = StringPart<'t>> + 't {
        self.0
            .children_with_tokens()
            .filter_map(|child| match child {
                SyntaxElement::Token(token) => StringText::cast(token)
                    .map(StringPart::Text)
                    .or_else(|| StringEscape::cast(token).map(StringPart::Escape)),
                SyntaxElement::Node(node) => {
                    Interpolation::cast(node).map(StringPart::Interpolation)
                }
            })
    }

    /// The interpolations, in order.
    #[must_use]
    pub fn interpolations(self) -> AstChildren<'t, Interpolation<'t>> {
        children(self.0)
    }

    /// Whether the string has an interpolation.
    #[must_use]
    pub fn has_interpolation(self) -> bool {
        self.interpolations().next().is_some()
    }

    /// Whether the closing `"` is there (E0010 otherwise).
    #[must_use]
    pub fn is_terminated(self) -> bool {
        token(self.0, SyntaxKind::STRING_END).is_some()
    }

    /// The value of a string without interpolations, with escapes resolved
    /// and line breaks as LF (D-114). `None` if it has an interpolation.
    #[must_use]
    pub fn plain_value(self) -> Option<String> {
        if self.has_interpolation() {
            return None;
        }
        Some(literal::string_value(self.parts().filter_map(
            |part| match part {
                StringPart::Text(text) => Some((SyntaxKind::STRING_TEXT, text.text())),
                StringPart::Escape(escape) => Some((SyntaxKind::STRING_ESCAPE, escape.text())),
                StringPart::Interpolation(_) => None,
            },
        )))
    }
}

impl<'t> Interpolation<'t> {
    /// The name the path starts with, such as `user` in `{user.name}`.
    #[must_use]
    pub fn root(self) -> Option<Ident<'t>> {
        child_token(self.0)
    }

    /// The field names after the root, such as `name` in `{user.name}`.
    pub fn fields(self) -> impl Iterator<Item = Ident<'t>> + 't {
        AstTokens::<Ident<'t>>::new(self.0).skip(1)
    }

    /// Whether the interpolation ends with `:?`, for debug printing
    /// (D-086).
    #[must_use]
    pub fn is_debug(self) -> bool {
        token(self.0, SyntaxKind::COLON_QUESTION).is_some()
    }
}

impl<'t> PathExpr<'t> {
    /// The path.
    #[must_use]
    pub fn path(self) -> Option<PathNode<'t>> {
        child(self.0)
    }

    /// The turbofish type arguments `::<T>`, if written (§3.12.6).
    #[must_use]
    pub fn type_args(self) -> Option<TypeArgList<'t>> {
        child(self.0)
    }
}

/// An entry of a record expression.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RecordEntry<'t> {
    Field(FieldInit<'t>),
    Spread(Spread<'t>),
}

impl<'t> RecordEntry<'t> {
    /// The entry's node.
    #[must_use]
    pub fn syntax(self) -> SyntaxNode<'t> {
        match self {
            Self::Field(field) => field.syntax(),
            Self::Spread(spread) => spread.syntax(),
        }
    }
}

impl<'t> RecordExpr<'t> {
    /// The type or constructor path, which may be module-qualified
    /// (D-429).
    #[must_use]
    pub fn path(self) -> Option<PathNode<'t>> {
        child(self.0)
    }

    /// The fields and spreads, in order.
    pub fn entries(self) -> impl Iterator<Item = RecordEntry<'t>> + 't {
        self.0.children().filter_map(|node| {
            FieldInit::cast(node)
                .map(RecordEntry::Field)
                .or_else(|| Spread::cast(node).map(RecordEntry::Spread))
        })
    }

    /// The field initializers, in order.
    #[must_use]
    pub fn fields(self) -> AstChildren<'t, FieldInit<'t>> {
        children(self.0)
    }

    /// The `..base` of a record update. Well-formed updates have exactly
    /// one, first (E0031).
    #[must_use]
    pub fn spread(self) -> Option<Spread<'t>> {
        child(self.0)
    }

    /// Whether this is a record update `T { ..base, … }` (§5.11.2).
    #[must_use]
    pub fn is_update(self) -> bool {
        self.spread().is_some()
    }
}

impl<'t> FieldInit<'t> {
    /// The field's name.
    #[must_use]
    pub fn name(self) -> Option<Ident<'t>> {
        child_token(self.0)
    }

    /// The value. `None` when punned: `Point { x }` means `Point { x: x }`
    /// (D-113).
    #[must_use]
    pub fn value(self) -> Option<Expr<'t>> {
        child(self.0)
    }

    /// Whether the field is punned.
    #[must_use]
    pub fn is_punned(self) -> bool {
        token(self.0, SyntaxKind::COLON).is_none()
    }
}

impl<'t> Spread<'t> {
    /// The spread expression.
    #[must_use]
    pub fn value(self) -> Option<Expr<'t>> {
        child(self.0)
    }
}

impl<'t> TupleExpr<'t> {
    /// The elements, in order.
    #[must_use]
    pub fn elements(self) -> AstChildren<'t, Expr<'t>> {
        children(self.0)
    }
}

impl<'t> ParenExpr<'t> {
    /// The expression inside the parentheses.
    #[must_use]
    pub fn inner(self) -> Option<Expr<'t>> {
        child(self.0)
    }
}

/// An item of a list literal.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ListItem<'t> {
    Expr(Expr<'t>),
    /// `..xs`, which inserts all elements of `xs` (D-157).
    Spread(Spread<'t>),
}

impl<'t> ListExpr<'t> {
    /// The items, in order.
    pub fn items(self) -> impl Iterator<Item = ListItem<'t>> + 't {
        self.0.children().filter_map(|node| {
            Spread::cast(node)
                .map(ListItem::Spread)
                .or_else(|| Expr::cast(node).map(ListItem::Expr))
        })
    }
}

impl<'t> Closure<'t> {
    /// The `(…)` parameter list.
    #[must_use]
    pub fn param_list(self) -> Option<ParamList<'t>> {
        child(self.0)
    }

    /// The parameters, in order. Empty if the list is missing.
    pub fn params(self) -> impl Iterator<Item = Param<'t>> + 't {
        self.param_list().into_iter().flat_map(ParamList::params)
    }

    /// The `-> T` return type, if written.
    #[must_use]
    pub fn ret_type(self) -> Option<RetType<'t>> {
        child(self.0)
    }

    /// The body.
    #[must_use]
    pub fn body(self) -> Option<Block<'t>> {
        child(self.0)
    }
}

impl<'t> CaseExpr<'t> {
    /// The subjects, in order. Empty for a subjectless `case`.
    #[must_use]
    pub fn subjects(self) -> AstChildren<'t, Expr<'t>> {
        children(self.0)
    }

    /// Whether this is a subjectless `case { cond => … }`: the body follows
    /// `case` directly (§5.9.5).
    #[must_use]
    pub fn is_subjectless(self) -> bool {
        self.0
            .children_with_tokens()
            .filter(|child| !child.kind().is_trivia() && child.kind() != SyntaxKind::NEWLINE)
            .nth(1)
            .is_some_and(|child| child.kind() == SyntaxKind::CASE_BODY)
    }

    /// The `{ … }` body.
    #[must_use]
    pub fn body(self) -> Option<CaseBody<'t>> {
        child(self.0)
    }

    /// The arms, in order. Empty if the body is missing.
    pub fn arms(self) -> impl Iterator<Item = CaseArmKind<'t>> + 't {
        self.body().into_iter().flat_map(CaseBody::arms)
    }
}

impl<'t> CaseBody<'t> {
    /// The arms, in order.
    #[must_use]
    pub fn arms(self) -> AstChildren<'t, CaseArmKind<'t>> {
        children(self.0)
    }
}

impl<'t> CaseArm<'t> {
    /// The patterns, one per subject.
    #[must_use]
    pub fn patterns(self) -> AstChildren<'t, Pat<'t>> {
        children(self.0)
    }

    /// The `when` guard, if written.
    #[must_use]
    pub fn guard(self) -> Option<Guard<'t>> {
        child(self.0)
    }

    /// The expression after `=>`.
    #[must_use]
    pub fn body(self) -> Option<Expr<'t>> {
        child_after(self.0, SyntaxKind::FAT_ARROW)
    }
}

impl<'t> Guard<'t> {
    /// The condition after `when`.
    #[must_use]
    pub fn condition(self) -> Option<Expr<'t>> {
        child(self.0)
    }
}

impl<'t> CondArm<'t> {
    /// The `_` of the final default arm, if this is that arm.
    #[must_use]
    pub fn default_token(self) -> Option<SyntaxToken<'t>> {
        token(self.0, SyntaxKind::UNDERSCORE)
    }

    /// Whether this is the `_ => …` arm (D-109).
    #[must_use]
    pub fn is_default(self) -> bool {
        self.default_token().is_some()
    }

    /// The condition before `=>`. `None` for the `_` arm.
    #[must_use]
    pub fn condition(self) -> Option<Expr<'t>> {
        child_before(self.0, SyntaxKind::FAT_ARROW)
    }

    /// The expression after `=>`.
    #[must_use]
    pub fn body(self) -> Option<Expr<'t>> {
        child_after(self.0, SyntaxKind::FAT_ARROW)
    }
}

impl<'t> PanicExpr<'t> {
    /// The `as` message, if written.
    #[must_use]
    pub fn message(self) -> Option<Message<'t>> {
        child(self.0)
    }
}

impl<'t> TodoExpr<'t> {
    /// The `as` message, if written.
    #[must_use]
    pub fn message(self) -> Option<Message<'t>> {
        child(self.0)
    }
}

/// A binary operator (§5.4.1, D-108).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BinOp {
    /// `||`
    Or,
    /// `&&`
    And,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `|>`
    Pipe,
    /// `|`
    BitOr,
    /// `^`
    BitXor,
    /// `&`
    BitAnd,
    /// `<<`
    Shl,
    /// `>>`
    Shr,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Rem,
}

impl BinOp {
    /// The operator of a token kind, if it is a binary operator.
    #[must_use]
    pub const fn from_kind(kind: SyntaxKind) -> Option<Self> {
        let op = match kind {
            SyntaxKind::PIPE2 => Self::Or,
            SyntaxKind::AMP2 => Self::And,
            SyntaxKind::EQ2 => Self::Eq,
            SyntaxKind::NEQ => Self::Ne,
            SyntaxKind::LT => Self::Lt,
            SyntaxKind::LE => Self::Le,
            SyntaxKind::GT => Self::Gt,
            SyntaxKind::GE => Self::Ge,
            SyntaxKind::PIPE_GT => Self::Pipe,
            SyntaxKind::PIPE => Self::BitOr,
            SyntaxKind::CARET => Self::BitXor,
            SyntaxKind::AMP => Self::BitAnd,
            SyntaxKind::SHL => Self::Shl,
            SyntaxKind::SHR => Self::Shr,
            SyntaxKind::PLUS => Self::Add,
            SyntaxKind::MINUS => Self::Sub,
            SyntaxKind::STAR => Self::Mul,
            SyntaxKind::SLASH => Self::Div,
            SyntaxKind::PERCENT => Self::Rem,
            _ => return None,
        };
        Some(op)
    }

    /// The precedence level, from 1 (`||`) to 10 (`*`, `/`, `%`) (§5.4.1).
    #[must_use]
    pub const fn precedence(self) -> u8 {
        match self {
            Self::Or => 1,
            Self::And => 2,
            Self::Eq | Self::Ne | Self::Lt | Self::Le | Self::Gt | Self::Ge => 3,
            Self::Pipe => 4,
            Self::BitOr => 5,
            Self::BitXor => 6,
            Self::BitAnd => 7,
            Self::Shl | Self::Shr => 8,
            Self::Add | Self::Sub => 9,
            Self::Mul | Self::Div | Self::Rem => 10,
        }
    }

    /// Whether this is a comparison, which does not chain (D-108).
    #[must_use]
    pub const fn is_comparison(self) -> bool {
        self.precedence() == 3
    }

    /// The operator's text, such as `"|>"`.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::Or => "||",
            Self::And => "&&",
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Pipe => "|>",
            Self::BitOr => "|",
            Self::BitXor => "^",
            Self::BitAnd => "&",
            Self::Shl => "<<",
            Self::Shr => ">>",
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Rem => "%",
        }
    }
}

impl<'t> BinExpr<'t> {
    /// The operator token.
    #[must_use]
    pub fn op_token(self) -> Option<SyntaxToken<'t>> {
        self.0.children_with_tokens().find_map(|child| {
            child
                .as_token()
                .filter(|token| BinOp::from_kind(token.kind()).is_some())
        })
    }

    /// The operator.
    #[must_use]
    pub fn op(self) -> Option<BinOp> {
        BinOp::from_kind(self.op_token()?.kind())
    }

    /// The left operand.
    #[must_use]
    pub fn lhs(self) -> Option<Expr<'t>> {
        let op = self.op_token();
        self.0
            .children_with_tokens()
            .take_while(|child| op.is_none_or(|op| child.as_token() != Some(op)))
            .find_map(|child| child.as_node().and_then(Expr::cast))
    }

    /// The right operand.
    #[must_use]
    pub fn rhs(self) -> Option<Expr<'t>> {
        let op = self.op_token()?;
        self.0
            .children_with_tokens()
            .skip_while(|child| child.as_token() != Some(op))
            .find_map(|child| child.as_node().and_then(Expr::cast))
    }
}

/// A prefix operator.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PrefixOp {
    /// `-`
    Neg,
    /// `!`
    Not,
}

impl<'t> PrefixExpr<'t> {
    /// The operator token.
    #[must_use]
    pub fn op_token(self) -> Option<SyntaxToken<'t>> {
        token(self.0, SyntaxKind::MINUS).or_else(|| token(self.0, SyntaxKind::BANG))
    }

    /// The operator.
    #[must_use]
    pub fn op(self) -> Option<PrefixOp> {
        match self.op_token()?.kind() {
            SyntaxKind::MINUS => Some(PrefixOp::Neg),
            SyntaxKind::BANG => Some(PrefixOp::Not),
            _ => None,
        }
    }

    /// The operand.
    #[must_use]
    pub fn operand(self) -> Option<Expr<'t>> {
        child(self.0)
    }

    /// The number literal, if this is `-` directly before an integer or
    /// float literal, with nothing between them: one negative literal for
    /// typing (§5.3.1). `- 5` is not one; validation rejects it (E0056).
    /// `-(5)` is not one either: it negates a parenthesized expression.
    #[must_use]
    pub fn negative_literal(self) -> Option<Literal<'t>> {
        let literal = self.negated_number()?;
        (self.minus_gap()?.is_empty()).then_some(literal)
    }

    /// The number literal, if this is `-` applied to an integer or float
    /// literal operand, whatever lies between them.
    #[must_use]
    pub fn negated_number(self) -> Option<Literal<'t>> {
        if self.op()? != PrefixOp::Neg {
            return None;
        }
        match self.operand()? {
            Expr::Literal(literal)
                if matches!(
                    literal.kind(),
                    Some(LiteralKind::Int(_) | LiteralKind::Float(_))
                ) =>
            {
                Some(literal)
            }
            _ => None,
        }
    }

    /// For `-` applied to a number literal, the range between the `-` and
    /// the number: empty for `-5`, the space for `- 5`.
    #[must_use]
    pub fn minus_gap(self) -> Option<TextRange> {
        let literal = self.negated_number()?;
        let minus = self.op_token()?.text_range();
        let number = literal.syntax().first_significant_token()?.text_range();
        (minus.end() <= number.start()).then(|| TextRange::new(minus.end(), number.start()))
    }
}

impl<'t> CallExpr<'t> {
    /// The function called.
    #[must_use]
    pub fn callee(self) -> Option<Expr<'t>> {
        child(self.0)
    }

    /// The `(…)` argument list.
    #[must_use]
    pub fn arg_list(self) -> Option<ArgList<'t>> {
        child(self.0)
    }

    /// The arguments, in order. Empty if the list is missing.
    pub fn args(self) -> impl Iterator<Item = Arg<'t>> + 't {
        self.arg_list().into_iter().flat_map(ArgList::args)
    }
}

/// An argument of a call: an expression or a capture hole (§5.5, §5.6).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Arg<'t> {
    Expr(Expr<'t>),
    Hole(Hole<'t>),
}

impl<'t> Arg<'t> {
    /// The argument's node.
    #[must_use]
    pub fn syntax(self) -> SyntaxNode<'t> {
        match self {
            Self::Expr(expr) => expr.syntax(),
            Self::Hole(hole) => hole.syntax(),
        }
    }
}

impl<'t> ArgList<'t> {
    /// The arguments, in order.
    pub fn args(self) -> impl Iterator<Item = Arg<'t>> + 't {
        self.0.children().filter_map(|node| {
            Hole::cast(node)
                .map(Arg::Hole)
                .or_else(|| Expr::cast(node).map(Arg::Expr))
        })
    }

    /// The holes, in order. A capture has exactly one (§5.6, D-202).
    #[must_use]
    pub fn holes(self) -> AstChildren<'t, Hole<'t>> {
        children(self.0)
    }
}

impl<'t> FieldExpr<'t> {
    /// The expression whose field is read.
    #[must_use]
    pub fn receiver(self) -> Option<Expr<'t>> {
        child(self.0)
    }

    /// The field's name. `None` for positional access such as `x.0`, which
    /// is an error (D-241, E0024).
    #[must_use]
    pub fn field(self) -> Option<Ident<'t>> {
        child_token(self.0)
    }
}
