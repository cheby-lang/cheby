//! Spelling rules for names: the case conventions (§2.3, E0006) and the
//! reserved words (§2.4, E0007).
//!
//! These are not checked token by token. Name resolution reports them
//! once, where a name is bound or declared, and a use of a badly spelled
//! name adds nothing (D-059, D-128). The parser uses [`reserved_word_help`]
//! to explain a grammar error that a reserved word causes, such as `if`.

use cheby_diag::{Diagnostic, Span, codes};

use crate::{SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};

/// The targeted help for a reserved word (§2.4, D-128, D-240), or `None`
/// if `word` is not reserved.
#[must_use]
pub fn reserved_word_help(word: &str) -> Option<String> {
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

/// E0007 for the reserved word `word` at `span`, or `None` if `word` is
/// not reserved.
#[must_use]
pub fn reserved_word_error(word: &str, span: Span) -> Option<Diagnostic> {
    let help = reserved_word_help(word)?;
    Some(
        Diagnostic::new(codes::RESERVED_WORD, format!("`{word}` is a reserved word"))
            .with_primary(span, "reserved, cannot be used as a name")
            .with_help(help),
    )
}

/// E0007 or E0006 for the name `token` where it is bound or declared, or
/// `None` if it is spelled correctly. A `LOWER` name may not be a reserved
/// word and has no upper-case letter; an `UPPER` name has no `_` (§2.3,
/// §2.4, D-059, D-128, D-240). Non-ASCII names are left to the lexer
/// (E0005).
#[must_use]
pub fn check_name(token: SyntaxToken<'_>) -> Option<Diagnostic> {
    let text = token.text();
    if !text.is_ascii() {
        return None;
    }
    match token.kind() {
        SyntaxKind::LOWER_IDENT => {
            if let Some(diag) = reserved_word_error(text, token.span()) {
                return Some(diag);
            }
            if !text.bytes().any(|b| b.is_ascii_uppercase()) {
                return None;
            }
            let suggestion = to_snake_case(text);
            Some(
                Diagnostic::new(
                    codes::NAMING_CONVENTION,
                    format!("name `{text}` is not in snake_case"),
                )
                .with_primary(token.span(), "")
                .with_suggestion(
                    format!("write `{suggestion}`"),
                    token.span(),
                    suggestion,
                ),
            )
        }
        SyntaxKind::UPPER_IDENT if text.contains('_') => {
            let suggestion = to_pascal_case(text);
            Some(
                Diagnostic::new(
                    codes::NAMING_CONVENTION,
                    format!("type name `{text}` is not in PascalCase"),
                )
                .with_primary(token.span(), "")
                .with_suggestion(
                    format!("write `{suggestion}`"),
                    token.span(),
                    suggestion,
                ),
            )
        }
        _ => None,
    }
}

/// The name that `node` binds or declares, if `node` is a binding or a
/// declaration: an item, a variant, a record field, an interface function,
/// a type parameter, a local function, an import's `as` name, or a name in
/// a pattern (`x`, `p as x`, `..rest`). Parameters bind through their
/// pattern.
///
/// A punned field pattern such as `Point { x }` is not included: its name
/// is the field's, checked where the field is declared. Module names are
/// file names, checked when modules are discovered (E0037). Attribute
/// names bind nothing (D-435).
#[must_use]
pub fn bound_name(node: SyntaxNode<'_>) -> Option<SyntaxToken<'_>> {
    match node.kind() {
        SyntaxKind::FN_DECL
        | SyntaxKind::TYPE_DECL
        | SyntaxKind::ALIAS_DECL
        | SyntaxKind::CONST_DECL
        | SyntaxKind::INTERFACE_DECL
        | SyntaxKind::INTERFACE_FN
        | SyntaxKind::VARIANT
        | SyntaxKind::RECORD_FIELD
        | SyntaxKind::TYPE_PARAM
        | SyntaxKind::LOCAL_FN
        | SyntaxKind::RENAME
        | SyntaxKind::IDENT_PAT
        | SyntaxKind::AS_PAT
        | SyntaxKind::REST_PAT => node
            .children_with_tokens()
            .filter_map(|child| match child {
                SyntaxElement::Token(token) => Some(token),
                SyntaxElement::Node(_) => None,
            })
            .find(|t| matches!(t.kind(), SyntaxKind::LOWER_IDENT | SyntaxKind::UPPER_IDENT)),
        _ => None,
    }
}

/// `myValue` → `my_value`, `parseHTTPRequest` → `parse_http_request`.
#[must_use]
pub fn to_snake_case(text: &str) -> String {
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
#[must_use]
pub fn to_pascal_case(text: &str) -> String {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cheby_diag::FileId;

    use super::{bound_name, check_name, to_pascal_case, to_snake_case};

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

    /// The bound names of `text` with their diagnostics, in source order.
    fn checked(text: &str) -> Vec<(String, Option<String>)> {
        let parse = crate::parse(FileId::new(0), Arc::from(text));
        assert!(parse.errors().is_empty(), "{:?}", parse.errors());
        parse
            .tree()
            .root()
            .descendants()
            .filter_map(bound_name)
            .map(|t| {
                let diag = check_name(t).map(|d| format!("{}: {}", d.code, d.message));
                (t.text().to_owned(), diag)
            })
            .collect()
    }

    #[test]
    fn bindings_and_declarations() {
        let text = "import std::io as myIo\n\n@async\nfn f<T_x>(aB: Int, (c, dE): P) -> Int {\n  let go = case x {\n    Some(y) as zZ => 1\n    [_, ..restOf] => 2\n    P { field } => 3\n  }\n  fn loop() { 1 }\n  myValue + go\n}\n\ntype My_Type {\n  Var_A,\n  V { someField: Int },\n}\n";
        assert_eq!(
            checked(text),
            [
                (
                    "myIo".to_owned(),
                    Some("E0006: name `myIo` is not in snake_case".to_owned())
                ),
                ("f".to_owned(), None),
                (
                    "T_x".to_owned(),
                    Some("E0006: type name `T_x` is not in PascalCase".to_owned())
                ),
                (
                    "aB".to_owned(),
                    Some("E0006: name `aB` is not in snake_case".to_owned())
                ),
                ("c".to_owned(), None),
                (
                    "dE".to_owned(),
                    Some("E0006: name `dE` is not in snake_case".to_owned())
                ),
                (
                    "go".to_owned(),
                    Some("E0007: `go` is a reserved word".to_owned())
                ),
                (
                    "zZ".to_owned(),
                    Some("E0006: name `zZ` is not in snake_case".to_owned())
                ),
                ("y".to_owned(), None),
                (
                    "restOf".to_owned(),
                    Some("E0006: name `restOf` is not in snake_case".to_owned())
                ),
                (
                    "loop".to_owned(),
                    Some("E0007: `loop` is a reserved word".to_owned())
                ),
                (
                    "My_Type".to_owned(),
                    Some("E0006: type name `My_Type` is not in PascalCase".to_owned())
                ),
                (
                    "Var_A".to_owned(),
                    Some("E0006: type name `Var_A` is not in PascalCase".to_owned())
                ),
                ("V".to_owned(), None),
                (
                    "someField".to_owned(),
                    Some("E0006: name `someField` is not in snake_case".to_owned())
                ),
            ]
        );
    }

    #[test]
    fn non_ascii_names_are_left_to_the_lexer() {
        let parse = crate::parse(FileId::new(0), Arc::from("caféBar"));
        let token = parse.tree().root().tokens().next().unwrap();
        assert!(check_name(token).is_none());
    }
}
