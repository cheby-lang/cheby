//! Lexical checks that are not grammar errors (class V in the phase-A
//! design).
//!
//! `tree-sitter-cheby` accepts these inputs, so the parser does too, and
//! validation reports them: a lone CR (E0002), names that break the case
//! conventions (E0006), reserved words used as names (E0007), leading zeros
//! and `_` after a base prefix (E0008), invalid escapes (E0011) and
//! whitespace inside interpolation braces (E0012).

use cheby_diag::{Diagnostic, Span, TextRange, TextSize, codes};

use super::literal::{EscapeError, escape_value};
use super::number::{Base, scan_number};
use crate::{SyntaxKind, SyntaxToken, SyntaxTree};

/// Reports the V-class lexical diagnostics of `tree`, in source order
/// except that E0002 comes first.
pub(crate) fn check_tokens(tree: &SyntaxTree, out: &mut Vec<Diagnostic>) {
    lone_carriage_returns(tree, out);
    let mut in_interpolation = false;
    for token in tree.root().tokens() {
        match token.kind() {
            SyntaxKind::LOWER_IDENT => lower_ident(token, out),
            SyntaxKind::UPPER_IDENT => upper_ident(token, out),
            SyntaxKind::INT_NUMBER | SyntaxKind::FLOAT_NUMBER => number(token, out),
            SyntaxKind::STRING_ESCAPE => escape(token, out),
            SyntaxKind::INTERP_START => in_interpolation = true,
            SyntaxKind::INTERP_END | SyntaxKind::STRING_TEXT | SyntaxKind::STRING_END => {
                in_interpolation = false;
            }
            // In an interpolation the parser rejected, the grammar error
            // already says what is wrong.
            SyntaxKind::WHITESPACE if in_interpolation && !in_broken_interpolation(token) => {
                out.push(
                    Diagnostic::new(
                        codes::INVALID_INTERPOLATION,
                        "whitespace inside an interpolation",
                    )
                    .with_primary(token.span(), "")
                    .with_suggestion(
                        "remove the whitespace (D-115)",
                        token.span(),
                        "",
                    ),
                );
            }
            _ => {}
        }
    }
}

/// Whether `token` lies in an interpolation with a grammar error: one
/// with an `ERROR` node or without its closing `}`.
fn in_broken_interpolation(token: SyntaxToken<'_>) -> bool {
    let Some(interpolation) = token.parent().and_then(|parent| {
        parent
            .ancestors()
            .find(|node| node.kind() == SyntaxKind::INTERPOLATION)
    }) else {
        return false;
    };
    interpolation
        .children()
        .any(|child| child.kind() == SyntaxKind::ERROR)
        || interpolation
            .first_token_of_kind(SyntaxKind::INTERP_END)
            .is_none()
}

/// E0002: a `\r` not followed by `\n` (§2.1), wherever it is, including
/// inside comments and strings.
fn lone_carriage_returns(tree: &SyntaxTree, out: &mut Vec<Diagnostic>) {
    let bytes = tree.text().as_bytes();
    let mut next = bytes.iter().skip(1).map(Some).chain([None]);
    for (i, &b) in bytes.iter().enumerate() {
        let following = next.next().flatten();
        if b != b'\r' || following == Some(&b'\n') {
            continue;
        }
        let start = TextSize::try_from(i).unwrap_or(TextSize::MAX);
        let span = Span::new(tree.file(), TextRange::at(start, 1));
        out.push(
            Diagnostic::new(
                codes::LONE_CARRIAGE_RETURN,
                "carriage return without a line feed",
            )
            .with_primary(span, "")
            .with_help("save the file with LF or CRLF line endings"),
        );
    }
}

/// E0007 for reserved words, E0006 for `LOWER` names with upper-case
/// letters (§2.3, §2.4, D-059, D-128, D-240).
fn lower_ident(token: SyntaxToken<'_>, out: &mut Vec<Diagnostic>) {
    let text = token.text();
    if !text.is_ascii() {
        return; // E0005 was reported by the lexer.
    }
    // An attribute's name is not a binding: `@async` is an attribute
    // (§4.8, §12.5.2) although `async` is reserved.
    if token
        .parent()
        .is_some_and(|p| matches!(p.kind(), SyntaxKind::ATTR | SyntaxKind::INNER_ATTR))
    {
        return;
    }
    if let Some(help) = reserved_word_help(text) {
        out.push(
            Diagnostic::new(codes::RESERVED_WORD, format!("`{text}` is a reserved word"))
                .with_primary(token.span(), "reserved, cannot be used as a name")
                .with_help(help),
        );
    } else if text.bytes().any(|b| b.is_ascii_uppercase()) {
        let suggestion = to_snake_case(text);
        out.push(
            Diagnostic::new(
                codes::NAMING_CONVENTION,
                format!("name `{text}` is not in snake_case"),
            )
            .with_primary(token.span(), "")
            .with_suggestion(format!("write `{suggestion}`"), token.span(), suggestion),
        );
    }
}

/// E0006 for `UPPER` names with `_` (§2.3, D-059).
fn upper_ident(token: SyntaxToken<'_>, out: &mut Vec<Diagnostic>) {
    let text = token.text();
    if !text.is_ascii() || !text.contains('_') {
        return;
    }
    let suggestion = to_pascal_case(text);
    out.push(
        Diagnostic::new(
            codes::NAMING_CONVENTION,
            format!("type name `{text}` is not in PascalCase"),
        )
        .with_primary(token.span(), "")
        .with_suggestion(format!("write `{suggestion}`"), token.span(), suggestion),
    );
}

/// The targeted help for a reserved word (§2.4, D-128, D-240), or `None`
/// if `word` is not reserved.
fn reserved_word_help(word: &str) -> Option<String> {
    let help = match word {
        "if" | "else" | "match" => format!("Cheby has no `{word}`; use `case`"),
        "for" | "while" | "loop" | "break" | "continue" => {
            format!("Cheby has no `{word}`; use recursion, often through a local `fn`")
        }
        "return" => "Cheby has no `return`; a function returns the value of its body".to_owned(),
        "mut" => "values are immutable; bind a new name".to_owned(),
        "impl" | "trait" => {
            format!("Cheby has no `{word}`; use module functions and `interface`")
        }
        "struct" | "enum" => format!("Cheby has no `{word}`; use `type`"),
        "go" => "Cheby has no `go`; spawn a fiber in a scope".to_owned(),
        "async" | "await" | "macro" | "self" | "super" | "where" | "yield" => {
            "this word is reserved for future use".to_owned()
        }
        _ => return None,
    };
    Some(help)
}

/// `myValue` → `my_value`, `parseHTTPRequest` → `parse_http_request`.
fn to_snake_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len().saturating_add(4));
    let mut chars = text.chars().peekable();
    let mut prev: Option<char> = None;
    while let Some(c) = chars.next() {
        if c.is_ascii_uppercase() {
            let next_lower = chars.peek().is_some_and(char::is_ascii_lowercase);
            let boundary = prev.is_some_and(|p| {
                p.is_ascii_lowercase()
                    || p.is_ascii_digit()
                    || (p.is_ascii_uppercase() && next_lower)
            });
            if boundary && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
        prev = Some(c);
    }
    out
}

/// `My_Type` → `MyType`, `HTTP_server` → `HTTPServer`.
fn to_pascal_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for part in text.split('_') {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.extend(chars);
        }
    }
    out
}

/// E0008, validation cases: a decimal literal with a leading zero (§2.5.1)
/// and `_` right after a base prefix, which tree-sitter accepts.
fn number(token: SyntaxToken<'_>, out: &mut Vec<Diagnostic>) {
    let text = token.text();
    let scan = scan_number(text.as_bytes());
    if scan.error.is_some() || scan.len != text.len() {
        return; // A grammar error, reported by the lexer.
    }
    let prefix = scan.base.prefix();
    if scan.base != Base::Dec {
        let digits = text.get(prefix.len()..).unwrap_or_default();
        if digits.bytes().all(|b| b == b'_') {
            out.push(
                Diagnostic::new(
                    codes::MALFORMED_NUMBER,
                    format!(
                        "`{prefix}` is not followed by any {} digit",
                        scan.base.name()
                    ),
                )
                .with_primary(token.span(), format!("expected digits after `{prefix}`")),
            );
        } else if digits.starts_with('_') {
            let fixed = format!("{prefix}{}", digits.trim_start_matches('_'));
            out.push(
                Diagnostic::new(
                    codes::MALFORMED_NUMBER,
                    format!("`_` right after `{prefix}`"),
                )
                .with_primary(token.span(), "")
                .with_suggestion(format!("write `{fixed}`"), token.span(), fixed),
            );
        }
        return;
    }
    let int_end = text.find(['.', 'e', 'E']).unwrap_or(text.len());
    let int_part = text.get(..int_end).unwrap_or_default();
    let rest = text.get(int_end..).unwrap_or_default();
    if int_part.len() > 1 && int_part.starts_with('0') {
        let trimmed = int_part.trim_start_matches(['0', '_']);
        let trimmed = if trimmed.is_empty() { "0" } else { trimmed };
        let fixed = format!("{trimmed}{rest}");
        out.push(
            Diagnostic::new(
                codes::MALFORMED_NUMBER,
                format!("decimal literal `{text}` has a leading zero"),
            )
            .with_primary(token.span(), "")
            .with_note("octal literals start with `0o` (D-129)")
            .with_suggestion(format!("write `{fixed}`"), token.span(), fixed),
        );
    }
}

/// E0011: escapes other than those of D-114 (§2.5.3).
fn escape(token: SyntaxToken<'_>, out: &mut Vec<Diagnostic>) {
    let Err(error) = escape_value(token.text()) else {
        return;
    };
    let help = "the escapes are `\\n`, `\\t`, `\\r`, `\\\\`, `\\\"`, `\\{` and `\\u{…}` (D-114)";
    let message = match error {
        EscapeError::Unknown('\n' | '\r') => {
            // A `\` before a line break, as in Rust's line continuation.
            // Point at the backslash only, and do not print the line break
            // as if it were the escape `\n`.
            let backslash = Span::new(
                token.span().file,
                TextRange::at(token.text_range().start(), 1),
            );
            out.push(
                Diagnostic::new(
                    codes::INVALID_ESCAPE,
                    "`\\` at the end of a line is not an escape",
                )
                .with_primary(backslash, "")
                .with_note("strings may span lines (D-114)")
                .with_suggestion("remove the `\\`", backslash, "")
                .with_help("write `\\\\` for a backslash"),
            );
            return;
        }
        EscapeError::Unknown(c) if c.is_control() || c.is_whitespace() => {
            format!("unknown escape: `\\` followed by {}", describe_char(c))
        }
        EscapeError::Unknown(c) => format!("unknown escape `\\{c}`"),
        EscapeError::Incomplete => "incomplete escape `\\`".to_owned(),
        EscapeError::UnicodeWithoutBraces => {
            "`\\u` must be followed by hex digits in braces, such as `\\u{e9}`".to_owned()
        }
        EscapeError::UnicodeDigitCount => "`\\u{…}` needs 1 to 6 hex digits".to_owned(),
        EscapeError::NotScalarValue(value) => {
            format!("`\\u{{{value:X}}}` is not a Unicode scalar value")
        }
    };
    out.push(
        Diagnostic::new(codes::INVALID_ESCAPE, message)
            .with_primary(token.span(), "")
            .with_help(help),
    );
}

/// A whitespace or control character in words, for messages: escaping it
/// with `\` would look like a valid escape.
fn describe_char(c: char) -> String {
    match c {
        '\t' => "a tab".to_owned(),
        ' ' => "a space".to_owned(),
        _ => format!("the character U+{:04X}", u32::from(c)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cheby_diag::{Diagnostic, FileId};

    use super::{check_tokens, to_pascal_case, to_snake_case};
    use crate::lexer::lex;
    use crate::{event::Event, tree};

    fn check(text: &str) -> Vec<Diagnostic> {
        let lexed = lex(FileId::new(0), text);
        let mut events = vec![Event::Start {
            kind: crate::SyntaxKind::SOURCE_FILE,
            forward_parent: None,
        }];
        events.push(Event::Finish);
        let tree = tree::build(FileId::new(0), Arc::from(text), &lexed, events);
        let mut out = Vec::new();
        check_tokens(&tree, &mut out);
        out
    }

    fn codes(text: &str) -> Vec<String> {
        check(text).iter().map(|d| d.code.to_string()).collect()
    }

    #[test]
    fn case_conversions() {
        assert_eq!(to_snake_case("myValue"), "my_value");
        assert_eq!(to_snake_case("parseHTTPRequest"), "parse_http_request");
        assert_eq!(to_snake_case("x1Y"), "x1_y");
        assert_eq!(to_snake_case("_privateThing"), "_private_thing");
        assert_eq!(to_snake_case("a_B"), "a_b");
        assert_eq!(to_pascal_case("My_Type"), "MyType");
        assert_eq!(to_pascal_case("HTTP_server"), "HTTPServer");
        assert_eq!(to_pascal_case("T_"), "T");
    }

    #[test]
    fn clean_code_has_no_diagnostics() {
        let text = "fn main() {\r\n  let _x1 = \"a {b.c:?} \\{ \\u{1F600}\"\r\n  0x1F + 0 + 0.5 + 10e3\r\n}\n";
        assert_eq!(codes(text), Vec::<String>::new());
    }

    #[test]
    fn naming_and_reserved() {
        let diags = check("myValue My_Type if go self Self HTTPServer");
        let shown: Vec<_> = diags
            .iter()
            .map(|d| (d.code.to_string(), d.message.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("E0006".to_owned(), "name `myValue` is not in snake_case"),
                (
                    "E0006".to_owned(),
                    "type name `My_Type` is not in PascalCase"
                ),
                ("E0007".to_owned(), "`if` is a reserved word"),
                ("E0007".to_owned(), "`go` is a reserved word"),
                ("E0007".to_owned(), "`self` is a reserved word"),
            ]
        );
        assert_eq!(diags[0].helps[0].edits[0].replacement, "my_value");
        assert_eq!(diags[1].helps[0].edits[0].replacement, "MyType");
        assert_eq!(diags[2].helps[0].message, "Cheby has no `if`; use `case`");
        assert_eq!(
            diags[3].helps[0].message,
            "Cheby has no `go`; spawn a fiber in a scope"
        );
    }

    #[test]
    fn non_ascii_names_are_left_to_the_lexer() {
        assert_eq!(codes("caféBar"), Vec::<String>::new());
    }

    #[test]
    fn numbers() {
        let diags = check("0x_ 0o__");
        let messages: Vec<_> = diags.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(
            messages,
            [
                "`0x` is not followed by any hexadecimal digit",
                "`0o` is not followed by any octal digit"
            ]
        );
        let diags = check("007 0_1 00 01.5 0x_1 0b__1 0 0.5 0e1 1u8");
        let fixes: Vec<_> = diags
            .iter()
            .map(|d| (d.code.to_string(), d.helps[0].edits[0].replacement.clone()))
            .collect();
        let expected = [
            ("E0008", "7"),
            ("E0008", "1"),
            ("E0008", "0"),
            ("E0008", "1.5"),
            ("E0008", "0x1"),
            ("E0008", "0b1"),
        ];
        let expected: Vec<_> = expected
            .iter()
            .map(|(c, f)| ((*c).to_owned(), (*f).to_owned()))
            .collect();
        assert_eq!(fixes, expected);
    }

    #[test]
    fn escapes() {
        let diags = check(r#""\q \u{} \u{D800} \u \n \u{1234567}""#);
        let messages: Vec<_> = diags.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(
            messages,
            [
                "unknown escape `\\q`",
                "`\\u{…}` needs 1 to 6 hex digits",
                "`\\u{D800}` is not a Unicode scalar value",
                "`\\u` must be followed by hex digits in braces, such as `\\u{e9}`",
                "`\\u{…}` needs 1 to 6 hex digits",
            ]
        );
        assert!(diags.iter().all(|d| d.code.to_string() == "E0011"));
    }

    #[test]
    fn backslash_before_line_break_or_control_character() {
        for text in ["\"a \\\nb\"", "\"a \\\r\nb\""] {
            let diags = check(text);
            assert_eq!(diags.len(), 1, "{text:?}");
            let diag = diags.first().unwrap();
            assert_eq!(diag.message, "`\\` at the end of a line is not an escape");
            // The span is the backslash alone, not the line break.
            assert_eq!(diag.primary_span().unwrap().range.len(), 1);
        }
        let messages: Vec<_> = check("\"\\\t \\\u{7}\"")
            .into_iter()
            .map(|d| d.message)
            .collect();
        assert_eq!(
            messages,
            [
                "unknown escape: `\\` followed by a tab",
                "unknown escape: `\\` followed by the character U+0007",
            ]
        );
    }

    #[test]
    fn whitespace_in_interpolation() {
        assert_eq!(codes(r#""{ a }" "{a}""#), ["E0012", "E0012"]);
        assert_eq!(codes(r#""a { b""#), ["E0012"]);
    }

    #[test]
    fn lone_carriage_return() {
        let diags = check("a\rb\r\nc // x\r\"s\rt\"\r");
        let starts: Vec<_> = diags
            .iter()
            .map(|d| (d.code.to_string(), d.primary_span().unwrap().range.start()))
            .collect();
        assert_eq!(
            starts,
            [
                ("E0002".to_owned(), 1),
                ("E0002".to_owned(), 11),
                ("E0002".to_owned(), 14),
                ("E0002".to_owned(), 17),
            ]
        );
    }
}
