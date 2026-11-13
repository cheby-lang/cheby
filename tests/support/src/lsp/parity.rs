//! Diagnostics as `cheby check` prints them and as the server publishes
//! them, reduced to one key: severity, code, the first line of the
//! message, file, and the protocol's 0-based line and column (D-1348).
//!
//! `cheby check` prints a diagnostic as a headline, `error[E0047]:
//! mismatched types`, followed by ` --> path:line:col` in the compiler's
//! 1-based lines and code-point columns (D-421). The harness turns that
//! into a byte offset of the file's text by the compiler's rules, then into
//! a position by the protocol's ([`super::text`]), so a lone `\r` or a
//! character outside the BMP is compared exactly. A diagnostic with no
//! location, such as E0037, is the server's on `cheby.toml` at any
//! position (A §2.7). Labels, notes and helps are not part of the key.

use std::fmt;

use serde_json::Value;

use super::text::{Bom, Encoding, Lines, Position, compiler_offset};

/// The manifest, which holds the diagnostics with no location.
pub const MANIFEST: &str = "cheby.toml";

/// One diagnostic as `cheby check` printed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Printed {
    /// `error` or `warning`.
    pub severity: String,
    /// The code, `None` for a tool error such as `error: …` (D-442).
    pub code: Option<String>,
    /// The headline's message.
    pub message: String,
    /// The file as printed, with its 1-based line and code-point column.
    pub at: Option<(String, u32, u32)>,
}

/// The diagnostics in `cheby check`'s standard error, in order.
#[must_use]
pub fn parse_check_output(text: &str) -> Vec<Printed> {
    let mut out: Vec<Printed> = Vec::new();
    let mut located = true;
    for line in text.lines() {
        if let Some(printed) = headline(line) {
            out.push(printed);
            located = false;
            continue;
        }
        if located {
            continue;
        }
        let Some(location) = line.trim_start().strip_prefix("--> ") else {
            continue;
        };
        located = true;
        let mut parts = location.rsplitn(3, ':');
        let (Some(col), Some(row), Some(file)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if let (Ok(row), Ok(col), Some(last)) = (row.parse(), col.parse(), out.last_mut()) {
            last.at = Some((file.to_owned(), row, col));
        }
    }
    out
}

/// `error[E0047]: msg`, `warning[W0001]: msg` or `error: msg`.
fn headline(line: &str) -> Option<Printed> {
    let (severity, rest) = ["error", "warning"]
        .into_iter()
        .find_map(|s| line.strip_prefix(s).map(|rest| (s, rest)))?;
    let (code, message) = if let Some(rest) = rest.strip_prefix('[') {
        let (code, message) = rest.split_once("]: ")?;
        (Some(code.to_owned()), message)
    } else {
        (None, rest.strip_prefix(": ")?)
    };
    Some(Printed {
        severity: severity.to_owned(),
        code,
        message: message.to_owned(),
        at: None,
    })
}

/// The comparable form of one diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    /// The file, relative to the package, with `/` separators.
    pub file: String,
    /// The protocol's start position, `None` for any position.
    pub at: Option<Position>,
    /// The protocol's end position of a published key, `None` for a
    /// printed one, since the compiler prints only a span's start.
    pub end: Option<Position>,
    /// `error`, `warning`, `information` or `hint`.
    pub severity: String,
    /// The code, empty if none.
    pub code: String,
    /// The first line of the message.
    pub message: String,
    /// Where else a printed position may be published: the previous
    /// character's start, since the server widens an empty range to the
    /// previous character on publish (A §5), and the compiler prints only
    /// a span's start. It matches only a published range of exactly that
    /// character, which ends at the printed position. `None` for a
    /// published key.
    pub widened: Option<Position>,
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.at {
            Some(at) => write!(f, "{}:{}:{}", self.file, at.line, at.character)?,
            None => write!(f, "{}:*", self.file)?,
        }
        write!(f, " {}[{}]: {}", self.severity, self.code, self.message)
    }
}

/// The key of a published diagnostic of `file`.
#[must_use]
pub fn published_key(file: &str, diagnostic: &Value) -> Key {
    let severity = match diagnostic.get("severity").and_then(Value::as_u64) {
        Some(1) | None => "error",
        Some(2) => "warning",
        Some(3) => "information",
        Some(_) => "hint",
    };
    let code = match diagnostic.get("code") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let message = diagnostic
        .get("message")
        .and_then(Value::as_str)
        .and_then(|m| m.lines().next())
        .unwrap_or_default()
        .to_owned();
    let range = diagnostic.get("range");
    let position = |edge: &str| {
        range
            .and_then(|r| r.get(edge))
            .and_then(|p| Position::from_json(p).ok())
    };
    Key {
        file: file.to_owned(),
        at: position("start"),
        end: position("end"),
        severity: severity.to_owned(),
        code,
        message,
        widened: None,
    }
}

/// The key of a printed diagnostic, whose file `file` already maps to the
/// package's own path. `text` is that file's text, `bom` whether the
/// server counts its BOM, which it does for a file the client opened.
#[must_use]
pub fn printed_key(printed: &Printed, file: Option<(&str, &str, Bom)>, encoding: Encoding) -> Key {
    let (file, at, widened) = match (&printed.at, file) {
        (Some((_, row, col)), Some((file, text, bom))) => {
            let offset = compiler_offset(text, *row, *col);
            let lines = Lines::new(text, bom);
            let at = lines.position(offset, encoding);
            let previous = text
                .get(..offset)
                .and_then(|before| before.char_indices().next_back())
                .map(|(start, _)| lines.position(start, encoding))
                .filter(|&p| p != at);
            (file.to_owned(), Some(at), previous)
        }
        _ => (MANIFEST.to_owned(), None, None),
    };
    Key {
        file,
        at,
        end: None,
        widened,
        severity: printed.severity.clone(),
        code: printed.code.clone().unwrap_or_default(),
        message: printed
            .message
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
    }
}

/// Compares the two lists as multisets. An expected key with no position
/// matches an actual key of the same file at any position, and one with a
/// [`Key::widened`] position also matches a range from there to its own
/// position when no key matches its own.
///
/// # Errors
///
/// Returns both lists and what is missing or extra.
pub fn compare(expected: &[Key], actual: &[Key]) -> Result<(), String> {
    let mut left: Vec<&Key> = actual.iter().collect();
    left.sort();
    let mut missing = Vec::new();
    let mut sorted: Vec<&Key> = expected.iter().collect();
    // Exact keys first, so a wildcard never takes a key an exact one needs.
    sorted.sort_by_key(|k| (k.at.is_none(), (*k).clone()));
    for want in sorted {
        let same = |got: &&Key| {
            got.file == want.file
                && got.severity == want.severity
                && got.code == want.code
                && got.message == want.message
        };
        let found = left
            .iter()
            .position(|got| same(got) && (want.at.is_none() || got.at == want.at))
            .or_else(|| {
                let widened = want.widened?;
                left.iter()
                    .position(|got| same(got) && got.at == Some(widened) && got.end == want.at)
            });
        match found {
            Some(at) => {
                left.remove(at);
            }
            None => missing.push(want),
        }
    }
    if missing.is_empty() && left.is_empty() {
        return Ok(());
    }
    let list = |keys: &mut Vec<&Key>| {
        use std::fmt::Write as _;
        keys.sort();
        keys.iter().fold(String::new(), |mut out, k| {
            let _ = writeln!(out, "  {k}");
            out
        })
    };
    let mut expected_all: Vec<&Key> = expected.iter().collect();
    let mut actual_all: Vec<&Key> = actual.iter().collect();
    Err(format!(
        "the published diagnostics differ from `cheby check`'s\nmissing:\n{}extra:\n{}\
         expected (cheby check):\n{}published:\n{}",
        list(&mut missing),
        list(&mut left),
        list(&mut expected_all),
        list(&mut actual_all),
    ))
}

/// Whether `bytes` would give a phase-A error of the driver's
/// `parse_file`: text that is not UTF-8, an error of the lexer or the
/// parser, or of validation, which runs in full only on a tree with no
/// grammar error (D-425). The server then shows that file's phase-A
/// diagnostics and checks the others on last good text (D-1347), so parity
/// holds only for such files (D-1348).
#[must_use]
pub fn is_broken(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return true;
    };
    let parse = cheby_syntax::parse(cheby_diag::FileId::new(0), std::sync::Arc::from(text));
    if parse.errors().iter().any(cheby_diag::Diagnostic::is_error) {
        return true;
    }
    cheby_syntax::validate(parse.tree())
        .iter()
        .any(cheby_diag::Diagnostic::is_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OUTPUT: &str = "error[E0047]: mismatched types\n --> src/main.cheby:2:5\n  |\n2 |   \"x\"\n  |   ^^^ expected `Int`\n\nwarning[W0001]: tuple type with 4 elements\n  --> src/a.cheby:10:18\n   = help: …\n\nerror[E0037]: `src/Helpers.cheby` is not a valid module name\n  = note: …\nerror: CHEBY_BACKTRACE must be 0 or 1\n";

    #[test]
    fn check_output() {
        let printed = parse_check_output(OUTPUT);
        assert_eq!(printed.len(), 4);
        assert_eq!(printed[0].code.as_deref(), Some("E0047"));
        assert_eq!(printed[0].at, Some(("src/main.cheby".to_owned(), 2, 5)));
        assert_eq!(printed[1].severity, "warning");
        assert_eq!(printed[1].at, Some(("src/a.cheby".to_owned(), 10, 18)));
        assert_eq!(printed[2].at, None);
        assert_eq!(printed[3].code, None);
        assert_eq!(printed[3].message, "CHEBY_BACKTRACE must be 0 or 1");
    }

    #[test]
    fn keys_match() {
        let printed = parse_check_output(OUTPUT);
        let text = "fn f() -> Int {\n😀  \"x\"\n}\n";
        let key = printed_key(
            &printed[0],
            Some(("src/main.cheby", text, Bom::Counted)),
            Encoding::Utf16,
        );
        // Column 5 in code points is after the emoji and three spaces:
        // 2 + 3 = 5 UTF-16 units.
        assert_eq!(
            key.at,
            Some(Position {
                line: 1,
                character: 5
            })
        );
        let published = json!({
            "range": {"start": {"line": 1, "character": 5}, "end": {"line": 1, "character": 8}},
            "severity": 1, "code": "E0047", "message": "mismatched types\nmore",
        });
        let actual = published_key("src/main.cheby", &published);
        assert_eq!(
            compare(std::slice::from_ref(&key), std::slice::from_ref(&actual)),
            Ok(())
        );
        assert!(compare(&[key], &[]).is_err());
        assert!(compare(&[], &[actual]).is_err());
        // No location: any position on the manifest.
        let manifest = printed_key(&printed[2], None, Encoding::Utf16);
        let on_manifest = published_key(
            MANIFEST,
            &json!({"range": {"start": {"line": 0, "character": 0}}, "severity": 1,
                "code": "E0037", "message": "`src/Helpers.cheby` is not a valid module name"}),
        );
        assert_eq!(compare(&[manifest], &[on_manifest]), Ok(()));
    }

    /// A.5: an empty range at a line break is published one character
    /// earlier.
    #[test]
    fn widened_empty_ranges() {
        let printed =
            parse_check_output("error[E0014]: unexpected line break\n --> src/main.cheby:1:7\n");
        let text = "let ab\n";
        let key = printed_key(
            &printed[0],
            Some(("src/main.cheby", text, Bom::Counted)),
            Encoding::Utf16,
        );
        assert_eq!(
            key.widened,
            Some(Position {
                line: 0,
                character: 5
            })
        );
        let published = |start: u32, end: u32| {
            published_key(
                "src/main.cheby",
                &json!({"range": {"start": {"line": 0, "character": start},
                    "end": {"line": 0, "character": end}}, "severity": 1,
                    "code": "E0014", "message": "unexpected line break"}),
            )
        };
        let one = std::slice::from_ref(&key);
        assert_eq!(compare(one, &[published(5, 6)]), Ok(()));
        assert_eq!(compare(one, &[published(6, 6)]), Ok(()));
        assert_eq!(compare(one, &[published(6, 7)]), Ok(()));
        assert!(compare(one, &[published(4, 6)]).is_err());
        // The previous character's start, but not that one character: a
        // server that starts every range one character early.
        assert!(compare(one, &[published(5, 7)]).is_err());
        assert!(compare(one, &[published(5, 5)]).is_err());
    }

    #[test]
    fn broken_files() {
        assert!(!is_broken(b"fn main() {}\n"));
        assert!(is_broken(b"fn main( {\n"));
        assert!(is_broken(b"/* block */\nfn main() {}\n"));
        assert!(is_broken(b"\xff"));
    }
}
