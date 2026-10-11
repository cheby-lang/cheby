//! The kinds of tokens and nodes in the concrete syntax tree.
//!
//! Token kinds come first and all have a discriminant below 128, so a
//! [`crate::TokenSet`] fits in a `u128`. Node kinds follow, covering every
//! rule of Appendix A that becomes a node. The variant names are the names
//! printed by [`crate::SyntaxTree::debug_dump`].

macro_rules! syntax_kinds {
    (
        tokens { $($tok:ident,)* }
        nodes { $($node:ident,)* }
    ) => {
        /// The kind of a token or node.
        ///
        /// Trivia, names, literals, the 18 keywords of §2.4, and the operators
        /// and punctuation of §2.6 are tokens. Reserved words (D-128, D-240)
        /// are lexed as [`SyntaxKind::LOWER_IDENT`] and reported by
        /// validation.
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        #[repr(u16)]
        #[expect(
            non_camel_case_types,
            reason = "kind names are spelled like the tree dump, rust-analyzer style"
        )]
        pub enum SyntaxKind {
            $($tok,)*
            $($node,)*
        }

        impl SyntaxKind {
            /// Every kind, in discriminant order.
            pub const ALL: &'static [Self] = &[$(Self::$tok,)* $(Self::$node,)*];

            /// The variant name, such as `"FN_DECL"`, used in tree dumps.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$tok => stringify!($tok),)*
                    $(Self::$node => stringify!($node),)*
                }
            }
        }
    };
}

syntax_kinds! {
    tokens {
        // Trivia.
        WHITESPACE,
        NEWLINE,
        COMMENT,
        DOC_COMMENT,
        MODULE_DOC,
        // Lexical error.
        ERROR_TOKEN,
        // Names.
        LOWER_IDENT,
        UPPER_IDENT,
        UNDERSCORE,
        // Literals.
        INT_NUMBER,
        FLOAT_NUMBER,
        RAW_STRING,
        STRING_START,
        STRING_TEXT,
        STRING_ESCAPE,
        STRING_END,
        INTERP_START,
        INTERP_END,
        // Keywords (§2.4).
        AS_KW,
        ASSERT_KW,
        CASE_KW,
        CONST_KW,
        DYN_KW,
        EXPOSED_KW,
        FN_KW,
        IMPORT_KW,
        INTERFACE_KW,
        LET_KW,
        PANIC_KW,
        PRIV_KW,
        PUB_KW,
        TEST_KW,
        TODO_KW,
        TYPE_KW,
        USE_KW,
        WHEN_KW,
        // Operators and punctuation (§2.6).
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
        // End of input. Never stored in a tree.
        EOF,
    }
    nodes {
        SOURCE_FILE,
        // Module header and attributes.
        INNER_ATTR,
        ATTR,
        ATTR_ARG_LIST,
        IMPORT_DECL,
        MODULE_PATH,
        IMPORT_LIST,
        IMPORT_ITEM,
        RENAME,
        // Items.
        FN_DECL,
        TYPE_DECL,
        ALIAS_DECL,
        CONST_DECL,
        INTERFACE_DECL,
        TEST_DECL,
        VISIBILITY,
        TYPE_PARAM_LIST,
        TYPE_PARAM,
        BOUND_LIST,
        INTERFACE_REF,
        PARAM_LIST,
        PARAM,
        RET_TYPE,
        VARIANT_LIST,
        VARIANT,
        POS_FIELD_LIST,
        RECORD_FIELD_LIST,
        RECORD_FIELD,
        INTERFACE_BODY,
        INTERFACE_FN,
        TYPE_LIST,
        // Paths.
        PATH,
        // Types.
        PATH_TYPE,
        TYPE_ARG_LIST,
        TUPLE_TYPE,
        FN_TYPE,
        DYN_TYPE,
        // Statements.
        BLOCK,
        LET_STMT,
        USE_STMT,
        USE_BINDER,
        ASSERT_STMT,
        LOCAL_FN,
        MESSAGE,
        // Expressions.
        LITERAL,
        STRING_LIT,
        INTERPOLATION,
        PATH_EXPR,
        RECORD_EXPR,
        FIELD_INIT,
        SPREAD,
        TUPLE_EXPR,
        PAREN_EXPR,
        LIST_EXPR,
        CLOSURE,
        CASE_EXPR,
        CASE_BODY,
        CASE_ARM,
        GUARD,
        COND_ARM,
        PANIC_EXPR,
        TODO_EXPR,
        BIN_EXPR,
        PREFIX_EXPR,
        CALL_EXPR,
        ARG_LIST,
        HOLE,
        FIELD_EXPR,
        // Patterns.
        WILDCARD_PAT,
        IDENT_PAT,
        LITERAL_PAT,
        CTOR_PAT,
        POS_PAT_LIST,
        FIELD_PAT_LIST,
        FIELD_PAT,
        REST_PAT,
        TUPLE_PAT,
        LIST_PAT,
        PAREN_PAT,
        OR_PAT,
        AS_PAT,
        // Recovery.
        ERROR,
    }
}

// `TokenSet` is a `u128` bit set over token kinds.
const _: () = assert!(SyntaxKind::EOF.to_u16() < 128);

impl SyntaxKind {
    /// The discriminant.
    #[must_use]
    #[expect(
        clippy::as_conversions,
        reason = "a fieldless `repr(u16)` enum always converts to its discriminant"
    )]
    pub const fn to_u16(self) -> u16 {
        self as u16
    }

    /// The kind with discriminant `raw`, if there is one.
    #[must_use]
    pub fn from_u16(raw: u16) -> Option<Self> {
        Self::ALL.get(usize::from(raw)).copied()
    }

    /// Whether this kind is a token (a leaf of the tree).
    #[must_use]
    pub const fn is_token(self) -> bool {
        self.to_u16() < Self::SOURCE_FILE.to_u16()
    }

    /// Whether this kind is a node.
    #[must_use]
    pub const fn is_node(self) -> bool {
        !self.is_token()
    }

    /// Whether this kind is trivia: whitespace or a comment. A `NEWLINE` is
    /// not trivia, because the parser decides which line breaks matter
    /// (§2.8, D-231).
    #[must_use]
    pub const fn is_trivia(self) -> bool {
        matches!(
            self,
            Self::WHITESPACE | Self::COMMENT | Self::DOC_COMMENT | Self::MODULE_DOC
        )
    }

    /// Whether this kind is one of the 18 keywords of §2.4.
    #[must_use]
    pub const fn is_keyword(self) -> bool {
        let raw = self.to_u16();
        Self::AS_KW.to_u16() <= raw && raw <= Self::WHEN_KW.to_u16()
    }

    /// The keyword spelled `text`, if `text` is one of the 18 keywords.
    /// Reserved words (D-128, D-240) are not keywords and give `None`.
    #[must_use]
    pub fn from_keyword(text: &str) -> Option<Self> {
        let kind = match text {
            "as" => Self::AS_KW,
            "assert" => Self::ASSERT_KW,
            "case" => Self::CASE_KW,
            "const" => Self::CONST_KW,
            "dyn" => Self::DYN_KW,
            "exposed" => Self::EXPOSED_KW,
            "fn" => Self::FN_KW,
            "import" => Self::IMPORT_KW,
            "interface" => Self::INTERFACE_KW,
            "let" => Self::LET_KW,
            "panic" => Self::PANIC_KW,
            "priv" => Self::PRIV_KW,
            "pub" => Self::PUB_KW,
            "test" => Self::TEST_KW,
            "todo" => Self::TODO_KW,
            "type" => Self::TYPE_KW,
            "use" => Self::USE_KW,
            "when" => Self::WHEN_KW,
            _ => return None,
        };
        Some(kind)
    }

    /// The fixed text of a keyword or punctuation token, such as `"fn"` or
    /// `"=>"`. Other kinds have no fixed text.
    #[must_use]
    pub const fn fixed_text(self) -> Option<&'static str> {
        let text = match self {
            Self::UNDERSCORE => "_",
            Self::AS_KW => "as",
            Self::ASSERT_KW => "assert",
            Self::CASE_KW => "case",
            Self::CONST_KW => "const",
            Self::DYN_KW => "dyn",
            Self::EXPOSED_KW => "exposed",
            Self::FN_KW => "fn",
            Self::IMPORT_KW => "import",
            Self::INTERFACE_KW => "interface",
            Self::LET_KW => "let",
            Self::PANIC_KW => "panic",
            Self::PRIV_KW => "priv",
            Self::PUB_KW => "pub",
            Self::TEST_KW => "test",
            Self::TODO_KW => "todo",
            Self::TYPE_KW => "type",
            Self::USE_KW => "use",
            Self::WHEN_KW => "when",
            Self::PLUS => "+",
            Self::MINUS => "-",
            Self::STAR => "*",
            Self::SLASH => "/",
            Self::PERCENT => "%",
            Self::BANG => "!",
            Self::AMP => "&",
            Self::PIPE => "|",
            Self::CARET => "^",
            Self::SHL => "<<",
            Self::SHR => ">>",
            Self::EQ2 => "==",
            Self::NEQ => "!=",
            Self::LT => "<",
            Self::LE => "<=",
            Self::GT => ">",
            Self::GE => ">=",
            Self::AMP2 => "&&",
            Self::PIPE2 => "||",
            Self::PIPE_GT => "|>",
            Self::EQ => "=",
            Self::FAT_ARROW => "=>",
            Self::THIN_ARROW => "->",
            Self::LEFT_ARROW => "<-",
            Self::COLON2 => "::",
            Self::DOT => ".",
            Self::DOT2 => "..",
            Self::COMMA => ",",
            Self::COLON => ":",
            Self::COLON_QUESTION => ":?",
            Self::AT => "@",
            Self::AT_BANG => "@!",
            Self::L_PAREN => "(",
            Self::R_PAREN => ")",
            Self::L_BRACK => "[",
            Self::R_BRACK => "]",
            Self::L_BRACE => "{",
            Self::R_BRACE => "}",
            _ => return None,
        };
        Some(text)
    }

    /// How a diagnostic names this kind, such as `` "`fn`" ``,
    /// `"identifier"` or `"line break"`.
    #[must_use]
    #[expect(clippy::too_many_lines, reason = "one arm per kind")]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::WHITESPACE => "whitespace",
            Self::NEWLINE => "line break",
            Self::COMMENT => "comment",
            Self::DOC_COMMENT => "doc comment",
            Self::MODULE_DOC => "module doc comment",
            Self::ERROR_TOKEN => "invalid token",
            Self::LOWER_IDENT => "identifier",
            Self::UPPER_IDENT => "type name",
            Self::UNDERSCORE => "`_`",
            Self::INT_NUMBER => "integer",
            Self::FLOAT_NUMBER => "float",
            Self::RAW_STRING => "raw string",
            Self::STRING_START => "string",
            Self::STRING_TEXT => "string text",
            Self::STRING_ESCAPE => "escape sequence",
            Self::STRING_END => "`\"`",
            Self::INTERP_START => "interpolation",
            Self::INTERP_END => "end of interpolation",
            Self::AS_KW => "`as`",
            Self::ASSERT_KW => "`assert`",
            Self::CASE_KW => "`case`",
            Self::CONST_KW => "`const`",
            Self::DYN_KW => "`dyn`",
            Self::EXPOSED_KW => "`exposed`",
            Self::FN_KW => "`fn`",
            Self::IMPORT_KW => "`import`",
            Self::INTERFACE_KW => "`interface`",
            Self::LET_KW => "`let`",
            Self::PANIC_KW => "`panic`",
            Self::PRIV_KW => "`priv`",
            Self::PUB_KW => "`pub`",
            Self::TEST_KW => "`test`",
            Self::TODO_KW => "`todo`",
            Self::TYPE_KW => "`type`",
            Self::USE_KW => "`use`",
            Self::WHEN_KW => "`when`",
            Self::PLUS => "`+`",
            Self::MINUS => "`-`",
            Self::STAR => "`*`",
            Self::SLASH => "`/`",
            Self::PERCENT => "`%`",
            Self::BANG => "`!`",
            Self::AMP => "`&`",
            Self::PIPE => "`|`",
            Self::CARET => "`^`",
            Self::SHL => "`<<`",
            Self::SHR => "`>>`",
            Self::EQ2 => "`==`",
            Self::NEQ => "`!=`",
            Self::LT => "`<`",
            Self::LE => "`<=`",
            Self::GT => "`>`",
            Self::GE => "`>=`",
            Self::AMP2 => "`&&`",
            Self::PIPE2 => "`||`",
            Self::PIPE_GT => "`|>`",
            Self::EQ => "`=`",
            Self::FAT_ARROW => "`=>`",
            Self::THIN_ARROW => "`->`",
            Self::LEFT_ARROW => "`<-`",
            Self::COLON2 => "`::`",
            Self::DOT => "`.`",
            Self::DOT2 => "`..`",
            Self::COMMA => "`,`",
            Self::COLON => "`:`",
            Self::COLON_QUESTION => "`:?`",
            Self::AT => "`@`",
            Self::AT_BANG => "`@!`",
            Self::L_PAREN => "`(`",
            Self::R_PAREN => "`)`",
            Self::L_BRACK => "`[`",
            Self::R_BRACK => "`]`",
            Self::L_BRACE => "`{`",
            Self::R_BRACE => "`}`",
            Self::EOF => "end of file",
            Self::SOURCE_FILE => "source file",
            Self::INNER_ATTR => "inner attribute",
            Self::ATTR => "attribute",
            Self::ATTR_ARG_LIST => "attribute arguments",
            Self::IMPORT_DECL => "import",
            Self::MODULE_PATH => "module path",
            Self::IMPORT_LIST => "import list",
            Self::IMPORT_ITEM => "imported name",
            Self::RENAME => "rename",
            Self::FN_DECL => "function",
            Self::TYPE_DECL => "type declaration",
            Self::ALIAS_DECL => "type alias",
            Self::CONST_DECL => "constant",
            Self::INTERFACE_DECL => "interface",
            Self::TEST_DECL => "test",
            Self::VISIBILITY => "visibility",
            Self::TYPE_PARAM_LIST => "type parameters",
            Self::TYPE_PARAM => "type parameter",
            Self::BOUND_LIST => "bounds",
            Self::INTERFACE_REF => "interface name",
            Self::PARAM_LIST => "parameters",
            Self::PARAM => "parameter",
            Self::RET_TYPE => "return type",
            Self::VARIANT_LIST => "variants",
            Self::VARIANT => "variant",
            Self::POS_FIELD_LIST => "positional fields",
            Self::RECORD_FIELD_LIST => "record fields",
            Self::RECORD_FIELD => "record field",
            Self::INTERFACE_BODY => "interface body",
            Self::INTERFACE_FN => "interface function",
            Self::TYPE_LIST => "parameter types",
            Self::PATH => "path",
            Self::PATH_TYPE => "named type",
            Self::TYPE_ARG_LIST => "type arguments",
            Self::TUPLE_TYPE => "tuple type",
            Self::FN_TYPE => "function type",
            Self::DYN_TYPE => "`dyn` type",
            Self::BLOCK => "block",
            Self::LET_STMT => "`let` statement",
            Self::USE_STMT => "`use` statement",
            Self::USE_BINDER => "`use` binder",
            Self::ASSERT_STMT => "`assert` statement",
            Self::LOCAL_FN => "local function",
            Self::MESSAGE => "message",
            Self::LITERAL => "literal",
            Self::STRING_LIT => "string literal",
            Self::INTERPOLATION => "string interpolation",
            Self::PATH_EXPR => "name",
            Self::RECORD_EXPR => "record expression",
            Self::FIELD_INIT => "field initializer",
            Self::SPREAD => "spread",
            Self::TUPLE_EXPR => "tuple",
            Self::PAREN_EXPR => "parenthesized expression",
            Self::LIST_EXPR => "list",
            Self::CLOSURE => "closure",
            Self::CASE_EXPR => "`case` expression",
            Self::CASE_BODY => "`case` body",
            Self::CASE_ARM => "`case` arm",
            Self::GUARD => "guard",
            Self::COND_ARM => "condition arm",
            Self::PANIC_EXPR => "`panic` expression",
            Self::TODO_EXPR => "`todo` expression",
            Self::BIN_EXPR => "binary expression",
            Self::PREFIX_EXPR => "prefix expression",
            Self::CALL_EXPR => "call",
            Self::ARG_LIST => "arguments",
            Self::HOLE => "hole",
            Self::FIELD_EXPR => "field access",
            Self::WILDCARD_PAT => "wildcard pattern",
            Self::IDENT_PAT => "binding pattern",
            Self::LITERAL_PAT => "literal pattern",
            Self::CTOR_PAT => "constructor pattern",
            Self::POS_PAT_LIST => "positional patterns",
            Self::FIELD_PAT_LIST => "field patterns",
            Self::FIELD_PAT => "field pattern",
            Self::REST_PAT => "rest pattern",
            Self::TUPLE_PAT => "tuple pattern",
            Self::LIST_PAT => "list pattern",
            Self::PAREN_PAT => "parenthesized pattern",
            Self::OR_PAT => "or-pattern",
            Self::AS_PAT => "`as` pattern",
            Self::ERROR => "invalid syntax",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SyntaxKind;

    #[test]
    fn discriminants_follow_all() {
        for (i, kind) in SyntaxKind::ALL.iter().enumerate() {
            assert_eq!(usize::from(kind.to_u16()), i);
            assert_eq!(SyntaxKind::from_u16(kind.to_u16()), Some(*kind));
        }
        assert_eq!(SyntaxKind::from_u16(u16::MAX), None);
    }

    #[test]
    fn tokens_before_nodes() {
        assert_eq!(SyntaxKind::EOF.to_u16(), 74);
        assert!(SyntaxKind::EOF.is_token());
        assert!(SyntaxKind::R_BRACE.is_token());
        assert!(SyntaxKind::SOURCE_FILE.is_node());
        assert!(SyntaxKind::ERROR.is_node());
        let tokens = SyntaxKind::ALL.iter().filter(|k| k.is_token()).count();
        assert_eq!(tokens, 75);
    }

    #[test]
    fn keywords() {
        let keywords: Vec<_> = SyntaxKind::ALL.iter().filter(|k| k.is_keyword()).collect();
        assert_eq!(keywords.len(), 18);
        for kind in keywords {
            let text = kind.fixed_text().unwrap();
            assert_eq!(SyntaxKind::from_keyword(text), Some(*kind));
            assert_eq!(kind.describe(), format!("`{text}`"));
        }
        for reserved in ["if", "go", "match", "self", "Self", "True"] {
            assert_eq!(SyntaxKind::from_keyword(reserved), None);
        }
    }

    #[test]
    fn trivia() {
        assert!(SyntaxKind::WHITESPACE.is_trivia());
        assert!(SyntaxKind::DOC_COMMENT.is_trivia());
        assert!(!SyntaxKind::NEWLINE.is_trivia());
        assert!(!SyntaxKind::ERROR_TOKEN.is_trivia());
    }

    #[test]
    fn names() {
        assert_eq!(SyntaxKind::FN_DECL.name(), "FN_DECL");
        assert_eq!(SyntaxKind::PIPE_GT.name(), "PIPE_GT");
        assert_eq!(SyntaxKind::PIPE_GT.describe(), "`|>`");
    }
}
