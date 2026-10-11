//! Token views: names, literals, string parts and doc comments (ch. 2).
//!
//! Literal values come from [`crate::lexer::literal`], so a malformed
//! literal, already reported, gives `None` or a best-effort value.

use std::borrow::Cow;

use super::AstToken;
use crate::SyntaxToken;
use crate::lexer::literal::{self, EscapeError};

ast_tokens! {
    /// A `LOWER` name: a value, function, module, field or parameter name
    /// (§2.3).
    Ident = LOWER_IDENT;
    /// An `UPPER` name: a type, variant, interface or type parameter name
    /// (§2.3).
    TypeName = UPPER_IDENT;
    /// A name of either case, such as the last segment of a path or an
    /// imported name.
    Name = LOWER_IDENT | UPPER_IDENT;
    /// An integer literal (§2.5.1).
    IntLit = INT_NUMBER;
    /// A float literal (§2.5.2).
    FloatLit = FLOAT_NUMBER;
    /// A raw string literal `r#"…"#` (§2.5.3, D-130).
    RawStringLit = RAW_STRING;
    /// A run of literal text in a string (§2.5.3).
    StringText = STRING_TEXT;
    /// An escape sequence in a string, such as `\n` (§2.5.3, D-114).
    StringEscape = STRING_ESCAPE;
    /// A `///` doc comment (§2.2).
    DocComment = DOC_COMMENT;
    /// A `//!` module doc comment (§2.2).
    ModuleDoc = MODULE_DOC;
}

impl<'t> Name<'t> {
    /// The name as an [`Ident`], if it is a `LOWER` name.
    #[must_use]
    pub fn as_ident(self) -> Option<Ident<'t>> {
        Ident::cast(self.0)
    }

    /// The name as a [`TypeName`], if it is an `UPPER` name.
    #[must_use]
    pub fn as_type_name(self) -> Option<TypeName<'t>> {
        TypeName::cast(self.0)
    }

    /// Whether this is an `UPPER` name.
    #[must_use]
    pub fn is_type_name(self) -> bool {
        TypeName::can_cast(self.0.kind())
    }
}

impl<'t> From<Ident<'t>> for Name<'t> {
    fn from(ident: Ident<'t>) -> Self {
        Self(ident.0)
    }
}

impl<'t> From<TypeName<'t>> for Name<'t> {
    fn from(name: TypeName<'t>) -> Self {
        Self(name.0)
    }
}

impl IntLit<'_> {
    /// The value, `None` if the literal is malformed or above `u128::MAX`.
    #[must_use]
    pub fn value(self) -> Option<u128> {
        literal::int_value(self.text())
    }
}

impl FloatLit<'_> {
    /// The value, `None` if the literal is malformed.
    #[must_use]
    pub fn value(self) -> Option<f64> {
        literal::float_value(self.text())
    }

    /// The value rounded once to the nearest `F32`, `None` if the literal
    /// is malformed.
    #[must_use]
    pub fn value_f32(self) -> Option<f32> {
        literal::float_value_f32(self.text())
    }
}

impl<'t> RawStringLit<'t> {
    /// The contents without the delimiters, with line breaks as LF
    /// (D-431).
    #[must_use]
    pub fn value(self) -> Cow<'t, str> {
        literal::raw_string_value(self.text())
    }
}

impl StringEscape<'_> {
    /// The character the escape stands for (D-114).
    ///
    /// # Errors
    ///
    /// Returns why the escape is invalid (reported as E0011).
    pub fn value(self) -> Result<char, EscapeError> {
        literal::escape_value(self.text())
    }
}

impl<'t> DocComment<'t> {
    /// The text after `///`, without the line break.
    #[must_use]
    pub fn content(self) -> &'t str {
        self.text().get(3..).unwrap_or_default()
    }
}

impl<'t> ModuleDoc<'t> {
    /// The text after `//!`, without the line break.
    #[must_use]
    pub fn content(self) -> &'t str {
        self.text().get(3..).unwrap_or_default()
    }
}
