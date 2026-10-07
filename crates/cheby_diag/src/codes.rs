//! The registry of diagnostic codes (D-419) and their explanations (D-420).
//!
//! Every code the compiler can emit is listed here once, in order, with a
//! descriptive constant name and a one-line summary. Its long explanation
//! is `docs/diagnostics/<CODE>.md`, embedded at build time, so a code
//! without a file does not compile, and a test checks that there is no file
//! without a code.
//!
//! To add a code: append a line to the table below with the next number,
//! and add its explanation file in the same change. Codes are never reused
//! or renumbered. When a check is removed, its line and file stay, and the
//! file says that the code is no longer emitted.

use std::fmt;

use crate::diagnostic::Severity;

/// A diagnostic code: `E` and four digits for an error, `W` and four digits
/// for a warning (D-419).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Code {
    severity: Severity,
    number: u16,
}

impl Code {
    const fn new(severity: Severity, number: u16) -> Self {
        Self { severity, number }
    }

    #[must_use]
    pub const fn severity(self) -> Severity {
        self.severity
    }

    #[must_use]
    pub const fn number(self) -> u16 {
        self.number
    }

    /// Reads a code written like `E0012` or `W0003`. The code need not be
    /// registered; see [`lookup`].
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (severity, digits) = if let Some(digits) = text.strip_prefix('E') {
            (Severity::Error, digits)
        } else {
            (Severity::Warning, text.strip_prefix('W')?)
        };
        if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let number = digits.parse().ok().filter(|&n| n != 0)?;
        Some(Self::new(severity, number))
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let letter = match self.severity {
            Severity::Error => 'E',
            Severity::Warning => 'W',
        };
        write!(f, "{letter}{:04}", self.number)
    }
}

/// A registered code with its explanation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeInfo {
    pub code: Code,
    /// The constant's name in this module, such as `SYNTAX_ERROR`.
    pub name: &'static str,
    /// One line saying what the code is about.
    pub summary: &'static str,
    /// The text of `docs/diagnostics/<CODE>.md`.
    pub explanation: &'static str,
}

/// The registered code `code`, if any.
#[must_use]
pub fn lookup(code: Code) -> Option<&'static CodeInfo> {
    ALL.iter().find(|info| info.code == code)
}

/// The long explanation of `code`, printed by `cheby explain` (D-420).
#[must_use]
pub fn explanation(code: Code) -> Option<&'static str> {
    lookup(code).map(|info| info.explanation)
}

macro_rules! codes {
    ($( $code:ident $severity:ident $number:literal $name:ident $summary:literal; )*) => {
        $(
            #[doc = concat!("`", stringify!($code), "`: ", $summary, ".")]
            pub const $name: Code = Code::new(Severity::$severity, $number);
        )*

        /// Every registered code, in code order (errors, then warnings).
        pub static ALL: &[CodeInfo] = &[
            $(
                CodeInfo {
                    code: $name,
                    name: stringify!($name),
                    summary: $summary,
                    explanation: include_str!(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/../../docs/diagnostics/",
                        stringify!($code),
                        ".md"
                    )),
                },
            )*
        ];

        #[cfg(test)]
        const SPELLINGS: &[(&str, Code)] = &[$( (stringify!($code), $name), )*];
    };
}

codes! {
    // Lexical structure (chapter 2).
    E0001 Error 1 INVALID_UTF8 "source file is not valid UTF-8";
    E0002 Error 2 LONE_CARRIAGE_RETURN "carriage return not followed by a line feed";
    E0003 Error 3 UNEXPECTED_CHARACTER "character that cannot start a token";
    E0004 Error 4 BLOCK_COMMENT "block comment";
    E0005 Error 5 NON_ASCII_IDENTIFIER "identifier with a non-ASCII character";
    E0006 Error 6 NAMING_CONVENTION "identifier that breaks the naming convention";
    E0007 Error 7 RESERVED_WORD "reserved word used as a name";
    E0008 Error 8 MALFORMED_NUMBER "malformed numeric literal";
    E0009 Error 9 NUMBER_SUFFIX "numeric literal with a suffix";
    E0010 Error 10 UNTERMINATED_STRING "string literal without its closing quote";
    E0011 Error 11 INVALID_ESCAPE "invalid escape sequence";
    E0012 Error 12 INVALID_INTERPOLATION "invalid string interpolation";
    // Grammar.
    E0013 Error 13 SYNTAX_ERROR "syntax error";
    E0014 Error 14 UNEXPECTED_LINE_BREAK "line break where the line does not continue";
    E0015 Error 15 UNCLOSED_DELIMITER "unclosed delimiter";
    E0016 Error 16 NESTING_TOO_DEEP "code nested too deeply";
    E0017 Error 17 MISPLACED_DOC_COMMENT "doc comment that documents nothing";
    E0018 Error 18 MODULE_ORDER "module header out of order";
    E0019 Error 19 DANGLING_ATTRIBUTE "attribute not attached to an item";
    // Declarations (chapter 4).
    E0020 Error 20 MISSING_TYPE_ANNOTATION "missing type annotation in a signature";
    E0021 Error 21 PATTERN_IN_TOP_LEVEL_PARAM "pattern as a top-level function parameter";
    E0022 Error 22 INVALID_EXPOSED "`exposed` without `pub` or on something other than a type";
    E0023 Error 23 INVALID_BOUND "bound or type argument where none is allowed";
    // Expressions (chapter 5).
    E0024 Error 24 TUPLE_INDEX "positional tuple access";
    E0025 Error 25 MISPLACED_HOLE "`_` that is not a whole call argument";
    E0026 Error 26 MULTIPLE_HOLES "more than one `_` in one argument list";
    E0027 Error 27 SOLE_HOLE "call whose only argument is `_`";
    E0028 Error 28 PIPE_WITHOUT_HOLE "pipe into a call without a `_`";
    E0029 Error 29 CHAINED_COMPARISON "chained comparison";
    E0030 Error 30 SHORT_TUPLE "tuple with fewer than two elements";
    E0031 Error 31 INVALID_RECORD_UPDATE "malformed record update";
    // Patterns (chapter 6).
    E0032 Error 32 MISPLACED_REST "misplaced rest pattern";
    E0033 Error 33 FLOAT_PATTERN "float literal pattern";
    E0034 Error 34 INTERPOLATED_PATTERN "interpolation in a string pattern";
    E0035 Error 35 SUBJECTLESS_CASE_DEFAULT "subjectless `case` without a final `_` arm";
    // Packages (chapters 7 and 13).
    E0036 Error 36 INVALID_MANIFEST "invalid `cheby.toml`";
    E0037 Error 37 INVALID_MODULE_NAME "module file or directory name that is not a valid name";
    // Warnings.
    W0001 Warning 1 TUPLE_SHAPE "tuple type that hides meaning";
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::*;

    #[test]
    fn spellings_match_the_numbers() {
        for &(spelling, code) in SPELLINGS {
            assert_eq!(code.to_string(), spelling);
            assert_eq!(Code::parse(spelling), Some(code));
        }
    }

    #[test]
    fn codes_are_consecutive_from_one() {
        for severity in [Severity::Error, Severity::Warning] {
            let numbers: Vec<u16> = ALL
                .iter()
                .filter(|info| info.code.severity() == severity)
                .map(|info| info.code.number())
                .collect();
            let expected: Vec<u16> = (1..).take(numbers.len()).collect();
            assert_eq!(numbers, expected, "{severity:?} codes");
        }
        let mut sorted = ALL.to_vec();
        sorted.sort_by_key(|info| info.code);
        assert_eq!(sorted, ALL, "ALL is in code order");
    }

    #[test]
    fn parses_only_well_formed_codes() {
        for bad in [
            "", "E", "E1", "E00001", "e0001", "X0001", "E0000", "E00a1", "E 001",
        ] {
            assert_eq!(Code::parse(bad), None, "{bad:?}");
        }
        assert_eq!(
            Code::parse("W0042").map(|c| c.to_string()).as_deref(),
            Some("W0042")
        );
    }

    #[test]
    fn explanations_name_their_code() {
        for info in ALL {
            let heading = format!("# {}: ", info.code);
            assert!(
                info.explanation.starts_with(&heading),
                "docs/diagnostics/{}.md must start with `{heading}`",
                info.code
            );
            assert_eq!(explanation(info.code), Some(info.explanation));
        }
        assert_eq!(lookup(Code::new(Severity::Error, 9999)), None);
    }

    /// The files in `docs/diagnostics/` and the registered codes are the
    /// same set (D-420).
    #[test]
    fn every_explanation_file_has_a_code() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/diagnostics");
        let files: BTreeSet<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "README.md")
            .collect();
        let codes: BTreeSet<String> = ALL.iter().map(|info| format!("{}.md", info.code)).collect();
        assert_eq!(files, codes);
    }
}
