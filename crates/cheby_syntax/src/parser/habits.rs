//! Help lines for habits from other languages that end in a grammar error
//! (D-1930, D-1931, D-1932, D-1933).
//!
//! People who write Cheby bring forms from Rust, Python, TypeScript, Go
//! and others, and the first grammar error is where they learn the Cheby
//! form. [`explain_habits`] adds a help line that names that form to an
//! error (E0013, E0014, E0015) whose tokens show the habit, as
//! [`super::explain_reserved_words`] does for reserved words. The grammar
//! sets the helps that only it can place, such as `->` in a `case` arm, with
//! [`super::Parser::suggest_at_current`].
//!
//! No word becomes reserved: `var`, `not`, `and`, `def` or `switch` stay
//! valid names, and a help is added only where the parser already reports
//! an error. A help carries an edit only when the fix rewrites one token
//! (D-1931).
//!
//! An error at the next token of the line after one that a habit explains
//! is dropped when it follows only from that habit, such as the error at
//! `x` after `&` in `&x`, or when it repeats the same help (D-1933).

use cheby_diag::{Code, Diagnostic, Edit, Help, Span, TextRange, TextSize, codes};

use crate::SyntaxKind::{
    self, AMP, AS_KW, ASSERT_KW, BANG, CARET, COLON, COLON2, COMMA, COMMENT, CONST_KW, DOT, DOT2,
    EOF, EQ, EQ2, FAT_ARROW, FLOAT_NUMBER, FN_KW, GT, IMPORT_KW, INT_NUMBER, L_BRACE, L_BRACK,
    L_PAREN, LOWER_IDENT, LT, MINUS, NEQ, NEWLINE, PANIC_KW, PERCENT, PIPE, PIPE2, PLUS, R_BRACE,
    R_BRACK, R_PAREN, RAW_STRING, SLASH, STAR, STRING_END, STRING_START, THIN_ARROW, TODO_KW,
    UPPER_IDENT, USE_KW, WHITESPACE,
};
use crate::lexer::Lexed;
use crate::names::{to_pascal_case, to_snake_case};

/// The help of a closure written in another language's form.
pub(super) const CLOSURE_HELP: &str = "a closure is written `fn(x) { x + 1 }`";

/// The help of `->` or `:` where a `case` arm needs `=>`.
pub(super) const ARM_ARROW_HELP: &str = "a `case` arm uses `=>`; write `=>`";

/// The help of `:` before a function's return type.
pub(super) const RETURN_TYPE_HELP: &str = "a return type follows `->`; write `->`";

/// The help of `try`.
const TRY_HELP: &str = "Cheby has no `try`; a failure is an `Err` value, handled with `case` or \
                        `use x <- result::try(…)`";

/// The help of a cast with `as`.
pub(crate) const CAST_HELP: &str =
    "Cheby has no casts; convert with a function, such as `float::from_int(x)`";

/// Whether the tokens after `as`, without trivia, name a type, as in
/// `x as Float`, `x as i32` or `x as m::T`: a cast of another language.
#[must_use]
pub(crate) fn is_cast_target(tokens: &[(SyntaxKind, &str)]) -> bool {
    match tokens {
        [(UPPER_IDENT, _)] | [(LOWER_IDENT, _), (COLON2, _), (UPPER_IDENT, _)] => true,
        [(LOWER_IDENT, word)] => type_word(word).1.is_some() || matches!(*word, "int" | "float"),
        _ => false,
    }
}

/// The help of `x = …`, of `x += …` and of `x++`, for the name `name`.
fn immutable_help(name: &str) -> String {
    let fresh = to_snake_case(name);
    format!("values are immutable; bind a new name, such as `let {fresh}_2 = …`")
}

/// The help for a type written in lower case, as `int` or `i32`, with the
/// name its edit writes (D-1934). The edit is `None` where the word names
/// no type of the prelude. The language server offers the edit as the fix
/// unless the module hides that prelude type (D-1678, D-1931).
#[must_use]
pub(super) fn type_word(word: &str) -> (String, Option<&'static str>) {
    let to = match word {
        "int" | "integer" | "long" | "isize" | "usize" | "i128" | "u128" => "Int",
        "float" | "double" | "f64" | "number" => "Float",
        "string" | "str" | "char" | "rune" => "String",
        "bool" | "boolean" => "Bool",
        "nil" | "void" | "unit" => "Nil",
        "list" | "vec" | "array" => "List",
        "option" => "Option",
        "result" => "Result",
        "i8" => "I8",
        "i16" => "I16",
        "i32" => "I32",
        "i64" => "I64",
        "u8" | "byte" => "U8",
        "u16" => "U16",
        "u32" => "U32",
        "u64" => "U64",
        "f32" => "F32",
        _ => {
            let shown = to_pascal_case(word);
            return (
                format!("type names start with an upper-case letter, such as `{shown}`"),
                None,
            );
        }
    };
    (
        format!("type names start with an upper-case letter; write `{to}`"),
        Some(to),
    )
}

/// What a habit's help says, and the edit that applies it, if any.
struct Habit {
    help: String,
    /// The range to replace and its replacement.
    edit: Option<(TextRange, String)>,
    /// Whether an error at the next token of the line follows only from
    /// this habit (D-1933).
    cascade: bool,
}

impl Habit {
    fn text(help: impl Into<String>) -> Self {
        Self {
            help: help.into(),
            edit: None,
            cascade: false,
        }
    }

    fn edit(help: impl Into<String>, range: TextRange, replacement: impl Into<String>) -> Self {
        Self {
            help: help.into(),
            edit: Some((range, replacement.into())),
            cascade: false,
        }
    }

    fn cascading(self) -> Self {
        Self {
            cascade: true,
            ..self
        }
    }
}

/// The tokens of the file, read around an error.
#[derive(Clone, Copy)]
struct Cx<'a> {
    lexed: &'a Lexed,
    text: &'a str,
}

impl<'a> Cx<'a> {
    fn kind(self, i: usize) -> SyntaxKind {
        self.lexed.kind(i)
    }

    fn range(self, i: usize) -> TextRange {
        self.lexed.range(i)
    }

    fn word(self, i: usize) -> &'a str {
        self.lexed.text(self.text, i)
    }

    /// Whether token `i` is the name `word`.
    fn is_word(self, i: usize, word: &str) -> bool {
        self.kind(i) == LOWER_IDENT && self.word(i) == word
    }

    /// The last significant token before `i` on its line.
    fn prev(self, i: usize) -> Option<usize> {
        i.checked_sub(1)
            .and_then(|j| super::previous_on_line(self.lexed, j))
    }

    /// The next significant token after `i` on its line.
    fn next(self, i: usize) -> Option<usize> {
        let from = i.checked_add(1)?;
        (from..self.lexed.len())
            .find(|&j| !matches!(self.kind(j), WHITESPACE | COMMENT))
            .filter(|&j| !matches!(self.kind(j), NEWLINE | EOF))
    }

    /// The token right before `i`, if no space lies between.
    fn touching_prev(self, i: usize) -> Option<usize> {
        i.checked_sub(1)
            .filter(|&j| !matches!(self.kind(j), WHITESPACE | COMMENT | NEWLINE))
    }

    /// The token right after `i`, if no space lies between.
    fn touching_next(self, i: usize) -> Option<usize> {
        i.checked_add(1)
            .filter(|&j| !matches!(self.kind(j), WHITESPACE | COMMENT | NEWLINE | EOF))
    }

    /// Whether `i` is the first significant token of its line.
    fn starts_line(self, i: usize) -> bool {
        self.prev(i).is_none()
    }

    /// Whether `i` starts its line in column 1, where items are.
    fn in_column_one(self, i: usize) -> bool {
        i.checked_sub(1).is_none_or(|j| self.kind(j) == NEWLINE)
    }

    /// Whether token `i` can end an operand: a name, a literal or a closer.
    fn ends_operand(self, i: usize) -> bool {
        matches!(
            self.kind(i),
            LOWER_IDENT
                | UPPER_IDENT
                | INT_NUMBER
                | FLOAT_NUMBER
                | RAW_STRING
                | STRING_END
                | R_PAREN
                | R_BRACK
        )
    }

    /// Whether token `i` can start an operand.
    fn starts_operand(self, i: usize) -> bool {
        matches!(
            self.kind(i),
            LOWER_IDENT
                | UPPER_IDENT
                | INT_NUMBER
                | FLOAT_NUMBER
                | RAW_STRING
                | STRING_START
                | L_PAREN
                | L_BRACK
                | BANG
                | MINUS
                | FN_KW
        )
    }

    /// Whether the token before `i` on its line leaves room for an operand
    /// at `i`: there is none, or no operand ends with it.
    fn at_operand_start(self, i: usize) -> bool {
        self.prev(i).is_none_or(|j| !self.ends_operand(j))
    }

    /// Whether `i` is a name that starts a statement, as the target of an
    /// assignment does: it starts its line, and the line before does not
    /// end inside a list, as an argument of a call broken over lines does.
    fn target(self, i: usize) -> bool {
        let before = (0..i)
            .rev()
            .find(|&j| !matches!(self.kind(j), WHITESPACE | COMMENT | NEWLINE));
        self.kind(i) == LOWER_IDENT
            && self.starts_line(i)
            && before.is_none_or(|j| !matches!(self.kind(j), COMMA | L_PAREN | L_BRACK))
    }

    /// The kind of the nearest delimiter before `i` that is still open at
    /// `i`, if any.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "a match on syntax kinds names only the kinds it handles"
    )]
    fn enclosing(self, i: usize) -> Option<SyntaxKind> {
        let mut depth = 0_usize;
        for j in (0..i).rev() {
            match self.kind(j) {
                R_PAREN | R_BRACK | R_BRACE => depth = depth.saturating_add(1),
                L_PAREN | L_BRACK | L_BRACE => {
                    if depth == 0 {
                        return Some(self.kind(j));
                    }
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
        }
        None
    }
}

/// Adds the help of a habit (D-1930) to each grammar error of `errors`,
/// which are in source order, that has no help yet and whose tokens show
/// one, then drops the errors that only follow from a habit or repeat its
/// help (D-1933).
pub(super) fn explain_habits(lexed: &Lexed, text: &str, errors: &mut Vec<Diagnostic>) {
    let cx = Cx { lexed, text };
    let mut cascades: Vec<bool> = Vec::with_capacity(errors.len());
    for diag in errors.iter_mut() {
        let mut cascade = false;
        if diag.helps.is_empty()
            && let Some(span) = diag.primary_span()
            && let Some(i) = token_at(lexed, span.range.start())
            && let Some(habit) = habit(cx, diag.code, i)
        {
            cascade = habit.cascade;
            diag.helps.push(Help {
                message: habit.help,
                edits: habit
                    .edit
                    .map(|(range, replacement)| Edit {
                        span: Span::new(span.file, range),
                        replacement,
                    })
                    .into_iter()
                    .collect(),
            });
        }
        cascades.push(cascade);
    }
    drop_followers(cx, errors, &cascades);
}

/// The token that starts at `at`, or else the first one after it.
fn token_at(lexed: &Lexed, at: TextSize) -> Option<usize> {
    let i = lexed.tokens().partition_point(|t| t.start < at);
    (i < lexed.len()).then_some(i)
}

/// Drops each E0013 at the next token of the line after an error with a
/// help, when it has no help and that error's habit makes it follow
/// (`cascades`), or when it has the same help (D-1933).
fn drop_followers(cx: Cx<'_>, errors: &mut Vec<Diagnostic>, cascades: &[bool]) {
    let mut keep = Vec::with_capacity(errors.len());
    // The token, the help and the cascade of the last error kept.
    let mut last: Option<(usize, Option<String>, bool)> = None;
    for (diag, &cascade) in errors.iter().zip(cascades) {
        let token = diag
            .primary_span()
            .and_then(|span| token_at(cx.lexed, span.range.start()));
        let help = diag.helps.first().map(|help| help.message.clone());
        let follows = diag.code == codes::SYNTAX_ERROR
            && last.as_ref().zip(token).is_some_and(
                |((before, before_help, before_cascade), token)| {
                    cx.next(*before) == Some(token)
                        && before_help.is_some()
                        && ((*before_cascade && help.is_none()) || help == *before_help)
                },
            );
        keep.push(!follows);
        if !follows {
            last = token.map(|token| (token, help, cascade));
        }
    }
    let mut flags = keep.into_iter();
    errors.retain(|_| flags.next().unwrap_or(true));
}

/// The habit that the error of `code` at token `i` shows, if any.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "a match on syntax kinds names only the kinds it handles"
)]
fn habit(cx: Cx<'_>, code: Code, i: usize) -> Option<Habit> {
    if code == codes::UNCLOSED_DELIMITER {
        return local_const(cx, i);
    }
    if code != codes::SYNTAX_ERROR && code != codes::UNEXPECTED_LINE_BREAK {
        return None;
    }
    let here = match cx.kind(i) {
        EQ => equals(cx, i),
        COLON => typed_binding(cx, i),
        PLUS | MINUS => increment(cx, i),
        NEWLINE | EOF | WHITESPACE | COMMENT => cx.prev(i).and_then(|j| increment(cx, j)),
        STAR => star(cx, i),
        AMP => prefix_operator(
            cx,
            i,
            "Cheby has no references; write the value without `&`",
        ),
        GT => not_equal(cx, i),
        L_BRACK => bracket(cx, i),
        DOT2 => cx
            .prev(i)
            .filter(|&j| cx.ends_operand(j))
            .map(|_| range(cx, i)),
        DOT => dot(cx, i),
        BANG => bang(cx, i),
        PIPE | PIPE2 => closure_bars(cx, i),
        AS_KW => cx
            .prev(i)
            .filter(|&j| cx.ends_operand(j))
            .and_then(|_| cx.next(i))
            .filter(|&j| cx.kind(j) == UPPER_IDENT)
            .map(|_| Habit::text(CAST_HELP)),
        STRING_START => f_string(cx, i),
        USE_KW => use_import(cx, i),
        LOWER_IDENT => word_here(cx, i),
        _ => None,
    };
    here.or_else(|| word_before(cx, i))
}

/// `x: Int = 1` as a statement, as Python and TypeScript bind a name with
/// its type, at the `:` at `i` (D-1932): a name that starts a statement
/// as an assignment's target does, a type, then `=` on the same line.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "a match on syntax kinds names only the kinds it handles"
)]
fn typed_binding(cx: Cx<'_>, i: usize) -> Option<Habit> {
    let target = cx.touching_prev(i).filter(|&j| cx.target(j))?;
    let first = cx
        .next(i)
        .filter(|&j| matches!(cx.kind(j), UPPER_IDENT | LOWER_IDENT | L_PAREN | FN_KW))?;
    // The type runs to the first `=` outside its brackets.
    let mut depth = 0_usize;
    let mut last = first;
    let mut j = first;
    loop {
        match cx.kind(j) {
            LT | L_PAREN => depth = depth.saturating_add(1),
            GT | R_PAREN => depth = depth.checked_sub(1)?,
            EQ if depth == 0 => break,
            UPPER_IDENT | LOWER_IDENT | COLON2 | COMMA | FN_KW | THIN_ARROW => {}
            _ => return None,
        }
        if cx.kind(j) != EQ {
            last = j;
        }
        j = cx.next(j)?;
    }
    let ty = TextRange::new(cx.range(first).start(), cx.range(last).end()).slice(cx.text)?;
    let name = cx.word(target);
    Some(Habit::text(format!(
        "a name is bound with `let`; write `let {name}: {ty} = …`"
    )))
}

/// `0..10` in an expression, or `1..5 =>` in a `case` arm, at the `..`
/// at `i`.
fn range(cx: Cx<'_>, i: usize) -> Habit {
    let mut j = i;
    let mut arm = false;
    while let Some(next) = cx.next(j) {
        if cx.kind(next) == FAT_ARROW {
            arm = true;
            break;
        }
        j = next;
    }
    if arm {
        Habit::text(
            "Cheby has no range patterns; match with a guard, such as \
             `k when k >= 1 && k < 5 =>`",
        )
        .cascading()
    } else {
        Habit::text(
            "Cheby has no range operator; call `list::range(start, end)`, which stops before \
             `end`",
        )
        .cascading()
    }
}

/// `===`, `!==`, an assignment `x = …` or a compound one `x += …`, at the
/// `=` at `i`.
fn equals(cx: Cx<'_>, i: usize) -> Option<Habit> {
    let before = cx.touching_prev(i);
    match before.map(|j| cx.kind(j)) {
        Some(EQ2) => Some(Habit::edit(
            "Cheby has no `===`; write `==`",
            cx.range(i),
            "",
        )),
        Some(NEQ) => Some(Habit::edit(
            "Cheby has no `!==`; write `!=`",
            cx.range(i),
            "",
        )),
        Some(PLUS | MINUS | STAR | SLASH | PERCENT | CARET) => {
            let target = before
                .and_then(|op| cx.prev(op))
                .filter(|&j| cx.target(j))?;
            Some(Habit::text(immutable_help(cx.word(target))))
        }
        _ => {
            let target = cx.prev(i).filter(|&j| cx.target(j))?;
            let name = cx.word(target);
            // `x = 1` without an earlier `x` binds a name, as Python and Go
            // do.
            let bound = (0..target).any(|j| cx.kind(j) == LOWER_IDENT && cx.word(j) == name);
            Some(Habit::text(if bound {
                immutable_help(name)
            } else {
                format!("a name is bound with `let`; write `let {name} = …`")
            }))
        }
    }
}

/// `x++` or `x--`, with the second `+` or `-` at `i`.
fn increment(cx: Cx<'_>, i: usize) -> Option<Habit> {
    let kind = cx.kind(i);
    if !matches!(kind, PLUS | MINUS) {
        return None;
    }
    let first = cx.touching_prev(i).filter(|&j| cx.kind(j) == kind)?;
    let target = cx.touching_prev(first).filter(|&j| cx.target(j))?;
    Some(Habit::text(immutable_help(cx.word(target))))
}

/// `**`, or `*x` written to read through a pointer, at the `*` at `i`.
fn star(cx: Cx<'_>, i: usize) -> Option<Habit> {
    if cx.touching_prev(i).is_some_and(|j| cx.kind(j) == STAR) {
        return Some(
            Habit::text("Cheby has no `**`; call `int::pow(a, b)` or `float::pow(a, b)`")
                .cascading(),
        );
    }
    prefix_operator(cx, i, "Cheby has no pointers; write the value without `*`")
}

/// `&x` or `*x`, with the operator at `i` where an operand starts: the edit
/// deletes the operator.
fn prefix_operator(cx: Cx<'_>, i: usize, help: &str) -> Option<Habit> {
    let operand = cx.touching_next(i)?;
    let plain = matches!(cx.kind(operand), LOWER_IDENT | UPPER_IDENT | L_PAREN)
        && !cx.is_word(operand, "mut");
    (plain && cx.at_operand_start(i)).then(|| Habit::edit(help, cx.range(i), "").cascading())
}

/// `a <> b`, with the `>` at `i`.
fn not_equal(cx: Cx<'_>, i: usize) -> Option<Habit> {
    let lt = cx.touching_prev(i).filter(|&j| cx.kind(j) == LT)?;
    // Between operands, with a space before: `List<>` is an empty list of
    // type arguments.
    cx.prev(lt).filter(|&j| cx.ends_operand(j))?;
    cx.touching_prev(lt).is_none().then_some(())?;
    cx.next(i).filter(|&j| cx.starts_operand(j))?;
    Some(
        Habit::text("Cheby has no `<>`; write `!=` to compare, or `+` to join strings").cascading(),
    )
}

/// `xs[0]`, `Int[]` or `[Int]`, at the `[` at `i`.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "a match on syntax kinds names only the kinds it handles"
)]
fn bracket(cx: Cx<'_>, i: usize) -> Option<Habit> {
    if let Some(before) = cx.touching_prev(i) {
        return match cx.kind(before) {
            UPPER_IDENT if cx.touching_next(i).is_some_and(|j| cx.kind(j) == R_BRACK) => {
                let name = cx.word(before);
                Some(Habit::text(format!(
                    "a list type is written `List<T>`, such as `List<{name}>`"
                )))
            }
            LOWER_IDENT | R_PAREN | R_BRACK | STRING_END => Some(Habit::text(
                "Cheby has no indexing; call `list::get(xs, i)` or `map::get(m, key)`, which \
                 return an `Option`",
            )),
            _ => None,
        };
    }
    // `[Int]` where a type belongs, after `:` or `->`.
    cx.prev(i)
        .filter(|&j| matches!(cx.kind(j), COLON | THIN_ARROW))?;
    let shown = cx
        .next(i)
        .filter(|&j| cx.kind(j) == UPPER_IDENT)
        .filter(|&j| cx.next(j).is_some_and(|k| cx.kind(k) == R_BRACK))
        .map_or("T", |j| cx.word(j));
    Some(
        Habit::text(format!(
            "a list type is written `List<T>`, such as `List<{shown}>`"
        ))
        .cascading(),
    )
}

/// `[...xs]` or `import std.io`, at the `.` at `i`.
fn dot(cx: Cx<'_>, i: usize) -> Option<Habit> {
    if let Some(dots) = cx.touching_prev(i).filter(|&j| cx.kind(j) == DOT2) {
        // A spread exists only in a list literal (§5.13).
        if cx.enclosing(dots) != Some(L_BRACK) {
            return None;
        }
        return Some(
            Habit::edit("a spread is written `..xs`; write `..`", cx.range(i), "").cascading(),
        );
    }
    let module = cx.touching_prev(i).filter(|&j| cx.kind(j) == LOWER_IDENT)?;
    let import = (0..module)
        .rev()
        .find(|&j| !matches!(cx.kind(j), LOWER_IDENT | COLON2 | DOT | WHITESPACE))
        .is_some_and(|j| cx.kind(j) == IMPORT_KW);
    if !import {
        return None;
    }
    // Only a path with one `.` is fixed by one edit.
    let more = (i.saturating_add(1)..cx.lexed.len())
        .take_while(|&j| matches!(cx.kind(j), LOWER_IDENT | DOT | COLON2))
        .any(|j| cx.kind(j) == DOT);
    Some(if more {
        Habit::text("module paths use `::`, such as `import std::json::decode`")
    } else {
        Habit::edit("module paths use `::`; write `::`", cx.range(i), "::")
    })
}

/// `println!(…)` or a postfix `!` such as `x!`, at the `!` at `i`.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "a match on syntax kinds names only the kinds it handles"
)]
fn bang(cx: Cx<'_>, i: usize) -> Option<Habit> {
    let before = cx.touching_prev(i)?;
    let called = cx
        .touching_next(i)
        .is_some_and(|j| matches!(cx.kind(j), L_PAREN | L_BRACK | L_BRACE));
    let name = match cx.kind(before) {
        LOWER_IDENT | PANIC_KW | TODO_KW | ASSERT_KW if called => cx.word(before),
        _ if cx.ends_operand(before) => {
            return Some(Habit::text(
                "Cheby has no postfix `!`; take the value apart with `case`, or call \
                 `option::expect` or `result::expect`",
            ));
        }
        _ => return None,
    };
    let help = match name {
        "println" | "print" | "eprintln" | "eprint" => {
            format!("Cheby has no macros; call `io::{name}(…)`")
        }
        "panic" | "unreachable" => "Cheby has no macros; write `panic as \"message\"`".to_owned(),
        "todo" | "unimplemented" => "Cheby has no macros; write `todo as \"message\"`".to_owned(),
        "assert" | "assert_eq" | "assert_ne" | "debug_assert" | "debug_assert_eq" => {
            "Cheby has no macros; write `assert a == b`".to_owned()
        }
        "vec" => "Cheby has no macros; a list is written `[1, 2]`".to_owned(),
        "format" => "Cheby has no macros; strings interpolate, such as `\"{x}\"`".to_owned(),
        _ => "Cheby has no macros; call a function".to_owned(),
    };
    Some(Habit::text(help))
}

/// `|x| x + 1` or `|| 1`, at the bar at `i` where an operand starts.
fn closure_bars(cx: Cx<'_>, i: usize) -> Option<Habit> {
    if !cx.at_operand_start(i) {
        return None;
    }
    let closed = cx.kind(i) == PIPE2 || {
        let mut j = i;
        let mut found = false;
        while let Some(next) = cx.next(j) {
            if cx.kind(next) == PIPE {
                found = true;
                break;
            }
            j = next;
        }
        found
    };
    closed.then(|| Habit::text(CLOSURE_HELP).cascading())
}

/// `f"{x}"`, at the string at `i`: the edit deletes the `f`.
fn f_string(cx: Cx<'_>, i: usize) -> Option<Habit> {
    let prefix = cx.touching_prev(i).filter(|&j| cx.is_word(j, "f"))?;
    Some(Habit::edit(
        "Cheby strings interpolate without a prefix; remove the `f`",
        cx.range(prefix),
        "",
    ))
}

/// `use std::io` at the top level, at the `use` at `i`.
fn use_import(cx: Cx<'_>, i: usize) -> Option<Habit> {
    let module = cx.next(i).filter(|&j| cx.kind(j) == LOWER_IDENT)?;
    let colons = cx.next(module).is_some_and(|j| cx.kind(j) == COLON2);
    (cx.in_column_one(i) && colons).then(|| {
        Habit::edit(
            "Cheby imports a module with `import`; write `import`",
            cx.range(i),
            "import",
        )
    })
}

/// A `const` inside a body (E0015), at the `const` at `i`.
fn local_const(cx: Cx<'_>, i: usize) -> Option<Habit> {
    let name = cx.next(i).filter(|&j| cx.kind(j) == LOWER_IDENT)?;
    let binds = cx
        .next(name)
        .is_some_and(|j| matches!(cx.kind(j), EQ | COLON));
    (cx.kind(i) == CONST_KW && !cx.in_column_one(i) && binds).then(|| {
        Habit::edit(
            "a `const` belongs at the top level; inside a body write `let`",
            cx.range(i),
            "let",
        )
    })
}

/// A word of another language where the error is: `and` or `or` between
/// operands, or an item keyword in column 1, such as `class` or `def`.
fn word_here(cx: Cx<'_>, i: usize) -> Option<Habit> {
    let word = cx.word(i);
    let symbol = match word {
        "and" => Some("&&"),
        "or" => Some("||"),
        _ => None,
    };
    if let Some(symbol) = symbol {
        let between = cx.prev(i).is_some_and(|j| cx.ends_operand(j))
            && cx.next(i).is_some_and(|j| cx.starts_operand(j));
        return between.then(|| {
            Habit::edit(
                format!("Cheby has no `{word}`; write `{symbol}`"),
                cx.range(i),
                symbol,
            )
        });
    }
    if !cx.in_column_one(i) {
        return None;
    }
    let next = cx.next(i);
    match word {
        "def" | "function" | "func" | "fun" => fn_word(cx, i),
        "class" => next
            .filter(|&j| cx.kind(j) == UPPER_IDENT)
            .map(|_| Habit::text("Cheby has no `class`; use `type`")),
        "from" => next
            .filter(|&j| cx.kind(j) == LOWER_IDENT)
            .map(|_| Habit::text("Cheby imports a module with `import`, such as `import std::io`")),
        "package" => next.filter(|&j| cx.kind(j) == LOWER_IDENT).map(|_| {
            Habit::text("Cheby has no `package` line; each file is a module, so remove it")
        }),
        _ => None,
    }
}

/// `def f(…)`, `function f(…)`, `func f(…)` or `fun f(…)`, with the word
/// at `word`: the edit writes `fn`.
fn fn_word(cx: Cx<'_>, word: usize) -> Option<Habit> {
    let name = cx.next(word).filter(|&j| cx.kind(j) == LOWER_IDENT)?;
    cx.next(name)
        .filter(|&j| matches!(cx.kind(j), L_PAREN | LT))?;
    let text = cx.word(word);
    Some(Habit::edit(
        format!("Cheby has no `{text}`; write `fn`"),
        cx.range(word),
        "fn",
    ))
}

/// A word of another language right before the error at `i` on its line,
/// or right before a parenthesized group there, as in `switch (x) {`.
fn word_before(cx: Cx<'_>, i: usize) -> Option<Habit> {
    let before = cx.prev(i)?;
    let group = (cx.kind(before) == R_PAREN)
        .then(|| super::matching_open_paren(cx.lexed, before))
        .flatten()
        .and_then(|open| cx.prev(open));
    [Some(before), group]
        .into_iter()
        .flatten()
        .filter(|&w| cx.kind(w) == LOWER_IDENT)
        .find_map(|w| statement_word(cx, w, i))
}

/// The help of the word at `w` before the error at `i`.
fn statement_word(cx: Cx<'_>, w: usize, i: usize) -> Option<Habit> {
    let word = cx.word(w);
    let first = cx.starts_line(w);
    let binds = || cx.next(i).is_some_and(|j| matches!(cx.kind(j), EQ | COLON));
    match word {
        "var" | "val" if first && cx.next(w) == Some(i) && binds() => Some(Habit::edit(
            format!("Cheby has no `{word}`; write `let`"),
            cx.range(w),
            "let",
        )),
        "not" if cx.at_operand_start(w) && cx.next(w) == Some(i) && cx.starts_operand(i) => {
            // `not x` becomes `!x`: the word and the space after it.
            let range = TextRange::new(cx.range(w).start(), cx.range(i).start());
            Some(Habit::edit("Cheby has no `not`; write `!`", range, "!"))
        }
        "def" | "function" | "func" | "fun" if first => fn_word(cx, w),
        "lambda" if matches!(cx.kind(i), LOWER_IDENT | COLON) => {
            Some(Habit::text(CLOSURE_HELP).cascading())
        }
        "switch" if first => Some(Habit::text("Cheby has no `switch`; use `case`")),
        "elif" | "elsif" | "unless" if first => {
            Some(Habit::text(format!("Cheby has no `{word}`; use `case`")))
        }
        "do" if first && cx.next(w) == Some(i) && cx.kind(i) == L_BRACE => Some(Habit::text(
            "Cheby has no `do`; use recursion, often through a local `fn`",
        )),
        "until" | "foreach" if first => Some(Habit::text(format!(
            "Cheby has no `{word}`; use recursion, often through a local `fn`"
        ))),
        // `try` alone, since `try(r, f)` may call an imported `result::try`.
        "try" if first && cx.next(w) == Some(i) && matches!(cx.kind(i), L_BRACE | COLON) => {
            Some(Habit::text(TRY_HELP))
        }
        "catch" if first => Some(Habit::text(format!(
            "Cheby has no `{word}`; a failure is an `Err` value, handled with `case` or \
             `use x <- result::try(…)`"
        ))),
        "throw" | "raise" if first => Some(Habit::text(format!(
            "Cheby has no `{word}`; return an `Err`, or stop with `panic as \"message\"`"
        ))),
        _ => None,
    }
}
