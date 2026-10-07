//! Names of packages, modules and dependency keys: `LOWER` identifiers
//! that are neither keywords nor reserved words (§2.3, §2.4, §7.1, §7.2).

/// The 18 keywords of §2.4. `cheby_syntax` lexes exactly these as keyword
/// tokens; a unit test checks that the two lists agree.
pub(crate) const KEYWORDS: [&str; 18] = [
    "as",
    "assert",
    "case",
    "const",
    "dyn",
    "exposed",
    "fn",
    "import",
    "interface",
    "let",
    "panic",
    "priv",
    "pub",
    "test",
    "todo",
    "type",
    "use",
    "when",
];

/// The reserved words of §2.4 that have no meaning (D-128, D-240).
pub(crate) const RESERVED: [&str; 22] = [
    "if", "else", "for", "while", "loop", "break", "continue", "return", "mut", "impl", "trait",
    "struct", "enum", "match", "async", "await", "macro", "self", "super", "where", "yield", "go",
];

/// Whether `name` is a `LOWER` identifier (§2.3) that is not `_`, a
/// keyword or a reserved word (§2.4), and so can name a package, a module
/// or a dependency key.
#[must_use]
pub fn is_valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let starts_well = bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b == b'_');
    starts_well
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && name != "_"
        && !is_keyword_or_reserved(name)
}

/// Whether `name` is one of the words of §2.4.
#[must_use]
pub fn is_keyword_or_reserved(name: &str) -> bool {
    KEYWORDS.contains(&name) || RESERVED.contains(&name)
}

/// A valid name close to `name`, for help lines: `HttpClient` →
/// `http_client`, `web-server` → `web_server`, `type` → `type_`.
/// `None` if no ASCII letter or digit is left to build one from.
#[must_use]
pub fn suggest_name(name: &str) -> Option<String> {
    let mut out = String::new();
    let mut prev_lower_or_digit = false;
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            if prev_lower_or_digit && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower_or_digit = false;
        } else if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            prev_lower_or_digit = true;
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
            prev_lower_or_digit = false;
        }
    }
    let trimmed = out.trim_end_matches('_');
    let mut out = trimmed.to_owned();
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    if is_keyword_or_reserved(&out) {
        out.push('_');
    }
    (is_valid_name(&out) && out != name).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        for name in ["my_app", "a", "_private", "x1", "aoc_2024_day_1", "main"] {
            assert!(is_valid_name(name), "{name}");
        }
        for name in [
            "", "_", "MyApp", "my-app", "1app", "my.app", "été", "type", "go", "self", "fn",
        ] {
            assert!(!is_valid_name(name), "{name}");
        }
    }

    #[test]
    fn suggestions() {
        assert_eq!(suggest_name("HttpClient").as_deref(), Some("http_client"));
        assert_eq!(suggest_name("web-server").as_deref(), Some("web_server"));
        assert_eq!(suggest_name("myValue").as_deref(), Some("my_value"));
        assert_eq!(suggest_name("type").as_deref(), Some("type_"));
        assert_eq!(suggest_name("2d").as_deref(), Some("_2d"));
        assert_eq!(suggest_name("---"), None);
        assert_eq!(suggest_name("ok"), None);
    }

    /// The keyword list agrees with the lexer's (§2.4).
    #[test]
    fn keywords_match_the_lexer() {
        for word in KEYWORDS {
            assert!(
                cheby_syntax::SyntaxKind::from_keyword(word).is_some(),
                "{word}"
            );
        }
        for word in RESERVED {
            assert!(
                cheby_syntax::SyntaxKind::from_keyword(word).is_none(),
                "{word}"
            );
        }
        let lexer_keywords = cheby_syntax::SyntaxKind::ALL
            .iter()
            .filter(|k| k.is_keyword())
            .count();
        assert_eq!(lexer_keywords, KEYWORDS.len());
    }
}
