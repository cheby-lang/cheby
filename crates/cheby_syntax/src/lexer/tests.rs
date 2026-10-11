//! Lexer tests: one group per token class, error codes, and losslessness
//! over every Cheby file in the repository.

use std::path::{Path, PathBuf};

use cheby_diag::FileId;

use super::{Lexed, lex};
use crate::SyntaxKind::{self, *};

fn lex_checked(text: &str) -> Lexed {
    let lexed = lex(FileId::new(0), text);
    assert_lossless(text, &lexed);
    lexed
}

/// The tokens of `text`, without whitespace, as `(kind, text)`.
fn tokens(text: &str) -> Vec<(SyntaxKind, &str)> {
    let lexed = lex_checked(text);
    (0..lexed.len())
        .filter(|&i| lexed.kind(i) != WHITESPACE)
        .map(|i| (lexed.kind(i), lexed.text(text, i)))
        .collect()
}

fn kinds(text: &str) -> Vec<SyntaxKind> {
    tokens(text).into_iter().map(|(kind, _)| kind).collect()
}

/// The codes of the lexer's errors.
fn error_codes(text: &str) -> Vec<String> {
    lex_checked(text)
        .errors()
        .iter()
        .map(|d| d.code.to_string())
        .collect()
}

fn assert_lossless(text: &str, lexed: &Lexed) {
    let mut joined = String::with_capacity(text.len());
    let mut expected_start = 0;
    for i in 0..lexed.len() {
        let range = lexed.range(i);
        assert_eq!(range.start(), expected_start, "token {i} is not contiguous");
        assert!(!range.is_empty(), "token {i} is empty");
        let piece = range.slice(text).expect("token on char boundaries");
        joined.push_str(piece);
        expected_start = range.end();
    }
    assert_eq!(usize::try_from(expected_start).unwrap(), text.len());
    assert_eq!(joined, text);
    assert_eq!(lexed.kind(lexed.len()), EOF);
}

#[test]
fn empty() {
    let lexed = lex_checked("");
    assert!(lexed.is_empty());
    assert_eq!(lexed.kind(0), EOF);
    assert_eq!(lexed.errors(), []);
}

#[test]
fn whitespace_and_newlines() {
    let lexed = lex_checked(" \t x\n\r\n\n\ry");
    let all: Vec<_> = (0..lexed.len()).map(|i| lexed.kind(i)).collect();
    assert_eq!(
        all,
        [
            WHITESPACE,
            LOWER_IDENT,
            NEWLINE,
            NEWLINE,
            NEWLINE,
            WHITESPACE,
            LOWER_IDENT
        ]
    );
    assert_eq!(lexed.text(" \t x\n\r\n\n\ry", 3), "\r\n");
    assert_eq!(lexed.errors(), []);
}

#[test]
fn byte_order_mark() {
    let text = "\u{feff}fn";
    let lexed = lex_checked(text);
    assert_eq!(lexed.kind(0), WHITESPACE);
    assert_eq!(lexed.text(text, 0), "\u{feff}");
    assert_eq!(lexed.kind(1), FN_KW);
    // Elsewhere, a BOM is an unexpected character.
    assert_eq!(error_codes("a \u{feff}"), ["E0003"]);
}

#[test]
fn comments() {
    let text = "// a\n/// b\n//! c\n//// d\n//!! e\n///\n//\n/// f\r\n//x";
    assert_eq!(
        tokens(text)
            .into_iter()
            .filter(|(k, _)| *k != NEWLINE)
            .collect::<Vec<_>>(),
        [
            (COMMENT, "// a"),
            (DOC_COMMENT, "/// b"),
            (MODULE_DOC, "//! c"),
            (COMMENT, "//// d"),
            (MODULE_DOC, "//!! e"),
            (DOC_COMMENT, "///"),
            (COMMENT, "//"),
            (DOC_COMMENT, "/// f"),
            (COMMENT, "//x"),
        ]
    );
    // A lone CR stays inside the comment; a CRLF ends it.
    assert_eq!(tokens("// a\r b"), [(COMMENT, "// a\r b")]);
    assert_eq!(tokens("// a\r"), [(COMMENT, "// a\r")]);
    assert_eq!(
        kinds("x // c\ny"),
        [LOWER_IDENT, COMMENT, NEWLINE, LOWER_IDENT]
    );
}

#[test]
fn block_comments_are_errors() {
    let text = "/* a\n b */ x /* open";
    assert_eq!(
        tokens(text),
        [
            (ERROR_TOKEN, "/* a\n b */"),
            (LOWER_IDENT, "x"),
            (ERROR_TOKEN, "/* open"),
        ]
    );
    let lexed = lex_checked(text);
    assert_eq!(lexed.errors().len(), 2);
    assert_eq!(lexed.errors()[0].code.to_string(), "E0004");
    assert_eq!(lexed.errors()[0].primary_span().unwrap().range.len(), 2);
    // Not nested.
    assert_eq!(
        tokens("/* /* */ */"),
        [(ERROR_TOKEN, "/* /* */"), (STAR, "*"), (SLASH, "/")]
    );
}

#[test]
fn identifiers_and_keywords() {
    assert_eq!(
        tokens("foo _bar _ __ x1 Foo Self HTTPServer myValue My_Type fnx fn"),
        [
            (LOWER_IDENT, "foo"),
            (LOWER_IDENT, "_bar"),
            (UNDERSCORE, "_"),
            (LOWER_IDENT, "__"),
            (LOWER_IDENT, "x1"),
            (UPPER_IDENT, "Foo"),
            (UPPER_IDENT, "Self"),
            (UPPER_IDENT, "HTTPServer"),
            (LOWER_IDENT, "myValue"),
            (UPPER_IDENT, "My_Type"),
            (LOWER_IDENT, "fnx"),
            (FN_KW, "fn"),
        ]
    );
    let keywords = "as assert case const dyn exposed fn import interface let panic priv pub \
                    test todo type use when";
    let all = kinds(keywords);
    assert_eq!(all.len(), 18);
    assert!(all.iter().all(|k| k.is_keyword()));
    // Reserved words are identifiers here; validation reports them.
    assert_eq!(kinds("if else match go self return"), [LOWER_IDENT; 6]);
    assert_eq!(error_codes("if go myValue My_Type"), Vec::<String>::new());
}

#[test]
fn non_ascii_identifiers() {
    assert_eq!(
        tokens("café x"),
        [(LOWER_IDENT, "café"), (LOWER_IDENT, "x")]
    );
    assert_eq!(tokens("Ünïcode"), [(UPPER_IDENT, "Ünïcode")]);
    assert_eq!(tokens("naïve_x2"), [(LOWER_IDENT, "naïve_x2")]);
    let lexed = lex_checked("café_crème");
    assert_eq!(lexed.errors().len(), 1, "one error per token");
    let diag = &lexed.errors()[0];
    assert_eq!(diag.code.to_string(), "E0005");
    assert_eq!(diag.primary_span().unwrap().range.start(), 3);
    assert_eq!(diag.primary_span().unwrap().range.len(), 2);
}

#[test]
fn numbers() {
    assert_eq!(
        tokens("0 1_000 0xFF 0o17 0b1010 1.5 2e10 3.0E-2 007 0x_1"),
        [
            (INT_NUMBER, "0"),
            (INT_NUMBER, "1_000"),
            (INT_NUMBER, "0xFF"),
            (INT_NUMBER, "0o17"),
            (INT_NUMBER, "0b1010"),
            (FLOAT_NUMBER, "1.5"),
            (FLOAT_NUMBER, "2e10"),
            (FLOAT_NUMBER, "3.0E-2"),
            (INT_NUMBER, "007"),
            (INT_NUMBER, "0x_1"),
        ]
    );
    assert_eq!(kinds("1..2"), [INT_NUMBER, DOT2, INT_NUMBER]);
    assert_eq!(kinds("x.0"), [LOWER_IDENT, DOT, INT_NUMBER]);
    assert_eq!(kinds("x.0.1"), [LOWER_IDENT, DOT, FLOAT_NUMBER]);
    assert_eq!(kinds("1.foo"), [INT_NUMBER, DOT, LOWER_IDENT]);
    assert_eq!(kinds("-5"), [MINUS, INT_NUMBER]);
    assert_eq!(
        error_codes("0 1_000 0xFF 1.5e3 007 0x_1"),
        Vec::<String>::new()
    );
}

#[test]
fn number_errors() {
    assert_eq!(
        tokens("1u8 2.5f"),
        [(INT_NUMBER, "1u8"), (FLOAT_NUMBER, "2.5f")]
    );
    assert_eq!(error_codes("1u8"), ["E0009"]);
    assert_eq!(error_codes("2.5f"), ["E0009"]);
    assert_eq!(error_codes("1e"), ["E0009"]);
    assert_eq!(error_codes("0x"), ["E0008"]);
    assert_eq!(error_codes("0b2"), ["E0008"]);
    assert_eq!(error_codes("0b_"), Vec::<String>::new(), "validation error");
    assert_eq!(error_codes("0b102"), ["E0008"]);
    assert_eq!(error_codes("0o78"), ["E0008"]);
    let lexed = lex_checked("0b102");
    let diag = &lexed.errors()[0];
    assert_eq!(diag.message, "invalid digits `2` in a binary literal");
    assert_eq!(diag.primary_span().unwrap().range.start(), 4);
    let lexed = lex_checked("1u8");
    assert_eq!(
        lexed.errors()[0].message,
        "unexpected suffix `u8` after a number"
    );
    // The help names a sized type that exists (D-105).
    let help = |text: &str| lex_checked(text).errors()[0].helps[0].message.clone();
    assert!(help("1u8").contains("`let x: U8 = 1`"), "{}", help("1u8"));
    assert!(help("2.5f32").contains("`let x: F32 = 2.5`"));
    assert!(help("7abc").contains("`let x: U8 = 1`"));
}

#[test]
fn raw_strings() {
    let text = "r\"a\\b{c}\" r#\"say \"hi\"\"# r\"\" r##\"x\"#\"## rx r#x";
    assert_eq!(
        tokens(text),
        [
            (RAW_STRING, r#"r"a\b{c}""#),
            (RAW_STRING, "r#\"say \"hi\"\"#"),
            (RAW_STRING, r#"r"""#),
            (RAW_STRING, "r##\"x\"#\"##"),
            (LOWER_IDENT, "rx"),
            (LOWER_IDENT, "r"),
            (ERROR_TOKEN, "#"),
            (LOWER_IDENT, "x"),
        ]
    );
    assert_eq!(tokens("r\"a\nb\""), [(RAW_STRING, "r\"a\nb\"")]);
    let text = "r#\"never \" closed";
    assert_eq!(tokens(text), [(RAW_STRING, text)]);
    let lexed = lex_checked(text);
    assert_eq!(lexed.errors().len(), 1);
    assert_eq!(lexed.errors()[0].code.to_string(), "E0010");
    assert_eq!(lexed.errors()[0].primary_span().unwrap().range.len(), 3);
}

#[test]
fn strings() {
    assert_eq!(
        tokens(r#""a {b:?} \{c}""#),
        [
            (STRING_START, "\""),
            (STRING_TEXT, "a "),
            (INTERP_START, "{"),
            (LOWER_IDENT, "b"),
            (COLON_QUESTION, ":?"),
            (INTERP_END, "}"),
            (STRING_TEXT, " "),
            (STRING_ESCAPE, r"\{"),
            (STRING_TEXT, "c}"),
            (STRING_END, "\""),
        ]
    );
    assert_eq!(
        kinds(r#""{user.name}""#),
        [
            STRING_START,
            INTERP_START,
            LOWER_IDENT,
            DOT,
            LOWER_IDENT,
            INTERP_END,
            STRING_END
        ]
    );
    assert_eq!(kinds(r#""""#), [STRING_START, STRING_END]);
    // Line breaks inside strings are text, never `NEWLINE`.
    assert_eq!(
        tokens("\"a\r\nb\"\n"),
        [
            (STRING_START, "\""),
            (STRING_TEXT, "a\r\nb"),
            (STRING_END, "\""),
            (NEWLINE, "\n"),
        ]
    );
    assert_eq!(
        error_codes(r#""a {b:?} \{c} \u{1F600}""#),
        Vec::<String>::new()
    );
}

#[test]
fn escapes() {
    let text = r#""\n\t\r\\\"\{\u{e9}\q\u{}\u{0000041}\é\u""#;
    assert_eq!(
        tokens(text),
        [
            (STRING_START, "\""),
            (STRING_ESCAPE, r"\n"),
            (STRING_ESCAPE, r"\t"),
            (STRING_ESCAPE, r"\r"),
            (STRING_ESCAPE, r"\\"),
            (STRING_ESCAPE, r#"\""#),
            (STRING_ESCAPE, r"\{"),
            (STRING_ESCAPE, r"\u{e9}"),
            (STRING_ESCAPE, r"\q"),
            (STRING_ESCAPE, r"\u{}"),
            (STRING_ESCAPE, r"\u{0000041}"),
            (STRING_ESCAPE, r"\é"),
            (STRING_ESCAPE, r"\u"),
            (STRING_END, "\""),
        ]
    );
    // `\u{xyz}` is `\u` followed by an interpolation.
    assert_eq!(
        kinds(r#""\u{xyz}""#),
        [
            STRING_START,
            STRING_ESCAPE,
            INTERP_START,
            LOWER_IDENT,
            INTERP_END,
            STRING_END
        ]
    );
    // `\u{12` lexes as `\u` and then an interpolation, as in tree-sitter.
    assert_eq!(
        kinds(r#""\u{12}""#),
        [STRING_START, STRING_ESCAPE, STRING_END]
    );
    assert_eq!(
        kinds(r#""\u{12""#),
        [
            STRING_START,
            STRING_ESCAPE,
            INTERP_START,
            INT_NUMBER,
            STRING_END
        ]
    );
    // A `\` at the end of the file.
    assert_eq!(
        tokens("\"\\"),
        [(STRING_START, "\""), (STRING_ESCAPE, "\\")]
    );
    // Escapes are validated later, not by the lexer (except the
    // unterminated string here).
    assert_eq!(error_codes(r#""\q""#), Vec::<String>::new());
}

#[test]
fn interpolation_mode() {
    // `}` ends the interpolation, `{` inside it is a brace.
    assert_eq!(
        kinds(r#""{a{}b}""#),
        [
            STRING_START,
            INTERP_START,
            LOWER_IDENT,
            L_BRACE,
            INTERP_END,
            STRING_TEXT,
            STRING_END
        ]
    );
    // A `"` ends the interpolation and the string.
    assert_eq!(
        kinds(r#""{a" x"#),
        [
            STRING_START,
            INTERP_START,
            LOWER_IDENT,
            STRING_END,
            LOWER_IDENT
        ]
    );
    // A line break ends the interpolation and becomes string text.
    assert_eq!(
        tokens("\"{a\nb\""),
        [
            (STRING_START, "\""),
            (INTERP_START, "{"),
            (LOWER_IDENT, "a"),
            (STRING_TEXT, "\nb"),
            (STRING_END, "\""),
        ]
    );
    // No comments or raw strings inside braces.
    assert_eq!(
        kinds(r#""{a // b} {r"x"}""#),
        [
            STRING_START,
            INTERP_START,
            LOWER_IDENT,
            SLASH,
            SLASH,
            LOWER_IDENT,
            INTERP_END,
            STRING_TEXT,
            INTERP_START,
            LOWER_IDENT,
            STRING_END,
            LOWER_IDENT,
            STRING_START,
            STRING_TEXT,
            STRING_END,
        ]
    );
    assert_eq!(
        kinds(r#""{a + 1}""#),
        [
            STRING_START,
            INTERP_START,
            LOWER_IDENT,
            PLUS,
            INT_NUMBER,
            INTERP_END,
            STRING_END
        ]
    );
}

#[test]
fn unterminated_strings() {
    for text in ["\"abc", "\"a {b", "\"a {b\n", "\"\\"] {
        let lexed = lex_checked(text);
        assert_eq!(lexed.errors().len(), 1, "{text:?}");
        let diag = &lexed.errors()[0];
        assert_eq!(diag.code.to_string(), "E0010");
        assert_eq!(
            diag.primary_span().unwrap().range,
            cheby_diag::TextRange::new(0, 1)
        );
    }
    let text = "x \"a\nb";
    assert_eq!(
        lex_checked(text).errors()[0]
            .primary_span()
            .unwrap()
            .range
            .start(),
        2
    );
}

#[test]
fn punctuation() {
    let text = "+ - * / % ! & | ^ << >> == != < <= > >= && || |> = => -> <- :: . .. , : :? @ @! ( ) [ ] { }";
    assert_eq!(
        kinds(text),
        [
            PLUS,
            MINUS,
            STAR,
            SLASH,
            PERCENT,
            BANG,
            AMP,
            PIPE,
            CARET,
            SHL,
            SHR,
            EQ2,
            NEQ,
            LT,
            LE,
            GT,
            GE,
            AMP2,
            PIPE2,
            PIPE_GT,
            EQ,
            FAT_ARROW,
            THIN_ARROW,
            LEFT_ARROW,
            COLON2,
            DOT,
            DOT2,
            COMMA,
            COLON,
            COLON_QUESTION,
            AT,
            AT_BANG,
            L_PAREN,
            R_PAREN,
            L_BRACK,
            R_BRACK,
            L_BRACE,
            R_BRACE,
        ]
    );
    for kind in kinds(text) {
        assert!(kind.fixed_text().is_some(), "{kind:?}");
    }
    // Longest match.
    assert_eq!(kinds("a<-1"), [LOWER_IDENT, LEFT_ARROW, INT_NUMBER]);
    assert_eq!(kinds(">>="), [SHR, EQ]);
    assert_eq!(kinds("..."), [DOT2, DOT]);
    assert_eq!(kinds("|||>"), [PIPE2, PIPE_GT]);
    assert_eq!(kinds("@inline"), [AT, LOWER_IDENT]);
    assert_eq!(kinds("@!allow"), [AT_BANG, LOWER_IDENT]);
    assert_eq!(kinds("x:?"), [LOWER_IDENT, COLON_QUESTION]);
}

#[test]
fn unexpected_characters() {
    assert_eq!(
        tokens("a $ ; → \u{0} \u{a0}"),
        [
            (LOWER_IDENT, "a"),
            (ERROR_TOKEN, "$"),
            (ERROR_TOKEN, ";"),
            (ERROR_TOKEN, "→"),
            (ERROR_TOKEN, "\u{0}"),
            (ERROR_TOKEN, "\u{a0}"),
        ]
    );
    let lexed = lex_checked("a ; \u{0}");
    let messages: Vec<_> = lexed.errors().iter().map(|d| d.message.as_str()).collect();
    assert_eq!(
        messages,
        ["unexpected character `;`", "unexpected character `\\u{0}`"]
    );
    assert!(lexed.errors().iter().all(|d| d.code.to_string() == "E0003"));
    assert_eq!(
        lexed.errors()[0].helps[0].message,
        "statements are separated by line breaks, not `;`"
    );
}

#[test]
fn errors_are_in_source_order() {
    let lexed = lex_checked("$ 1u8 /* x */ \"open");
    let codes: Vec<_> = lexed.errors().iter().map(|d| d.code.to_string()).collect();
    assert_eq!(codes, ["E0003", "E0009", "E0004", "E0010"]);
    let starts: Vec<_> = lexed
        .errors()
        .iter()
        .map(|d| d.primary_span().unwrap().range.start())
        .collect();
    let mut sorted = starts.clone();
    sorted.sort_unstable();
    assert_eq!(starts, sorted);
}

#[test]
fn arbitrary_bytes_are_lossless() {
    // A cheap deterministic fuzz: mixtures of tricky fragments.
    let fragments = [
        "\"", "{", "}", "\\", "r#", "#", "\"#", "/", "*", "//", "\n", "\r", "\r\n", "0x", "1.",
        "e", "_", "é", "→", " ", "\t", "u{", "a", "Z", "<", ">", "=", "!", "\u{feff}", "@",
    ];
    let mut state = 0x2545_f491_u32;
    for _ in 0..2000 {
        let mut text = String::new();
        for _ in 0..24 {
            state ^= state.wrapping_shl(13);
            state ^= state.wrapping_shr(17);
            state ^= state.wrapping_shl(5);
            let pick = usize::try_from(state)
                .unwrap()
                .checked_rem(fragments.len())
                .unwrap();
            text.push_str(fragments[pick]);
        }
        lex_checked(&text);
    }
}

/// Every `.cheby` file under `dir`, sorted.
fn cheby_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "cheby") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn examples_and_ui_tests_are_lossless() {
    let root = repo_root();
    let examples = cheby_files(&root.join("docs/examples"));
    assert!(examples.len() > 30, "examples not found");
    for path in &examples {
        let text = std::fs::read_to_string(path).unwrap();
        let lexed = lex_checked(&text);
        assert!(
            lexed.errors().is_empty(),
            "{}: {:?}",
            path.display(),
            lexed.errors()
        );
    }
    let ui = cheby_files(&root.join("tests/ui"));
    assert!(!ui.is_empty(), "UI tests not found");
    for path in &ui {
        // UI tests may be invalid; only losslessness is checked.
        if let Ok(text) = std::fs::read_to_string(path) {
            lex_checked(&text);
        }
    }
}
