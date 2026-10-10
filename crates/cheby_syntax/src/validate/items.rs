//! Checks of declarations and statements: signatures (E0020, E0021),
//! `exposed` (E0022), bounds (E0023) and the shapes that App. A forbids
//! but `tree-sitter-cheby` accepts (E0013).

use cheby_diag::{Diagnostic, Span, TextRange, codes};

use super::{empty_at, has_error_child};
use crate::ast::{
    AliasDecl, AstNode, AstToken, ConstDecl, FnDecl, ImportDecl, ImportItem, InterfaceRef, LetStmt,
    LocalFn, Pat, Rename, TestDecl, TypeBody, TypeDecl, TypeParamList, UseStmt, Variant,
    VariantFields, Visibility,
};
use crate::{SyntaxElement, SyntaxKind};

/// E0012: a test's name is a string literal without interpolation (§4.7,
/// D-055). GAP(test-name-interpolation-code): the spec names no code, and
/// E0012 is the code of a string interpolation that is not allowed.
pub(super) fn test_decl(decl: TestDecl<'_>, out: &mut Vec<Diagnostic>) {
    let Some(name) = decl.name() else {
        return;
    };
    for interpolation in name.interpolations() {
        out.push(
            Diagnostic::new(codes::INVALID_INTERPOLATION, "interpolation in a test name")
                .with_primary(interpolation.significant_span(), "")
                .with_help("a test name is fixed text; write it out, or escape the brace as `\\{`"),
        );
    }
}

/// E0020 and E0021: a top-level function's parameters are plain names
/// with types (D-011, D-242).
pub(super) fn fn_decl(decl: FnDecl<'_>, out: &mut Vec<Diagnostic>) {
    for param in decl.params() {
        let Some(pattern) = param.pattern() else {
            continue;
        };
        if !matches!(pattern, Pat::Ident(_)) {
            out.push(
                Diagnostic::new(
                    codes::PATTERN_IN_TOP_LEVEL_PARAM,
                    "a top-level function parameter must be a name",
                )
                .with_primary(pattern.significant_span(), "")
                .with_help("name the parameter and take it apart in the body with `let` (D-242)"),
            );
        }
        if param.ty().is_none() {
            let message = match param.name() {
                Some(name) => format!("parameter `{}` has no type", name.text()),
                None => "parameter has no type".to_owned(),
            };
            out.push(
                Diagnostic::new(codes::MISSING_TYPE_ANNOTATION, message)
                    .with_primary(param.significant_span(), "")
                    .with_help(
                        "write the type after a `:`; top-level functions need full \
                         signatures (D-011)",
                    ),
            );
        }
    }
}

/// E0020: a constant needs a type (D-011).
pub(super) fn const_decl(decl: ConstDecl<'_>, out: &mut Vec<Diagnostic>) {
    if decl.ty().is_some() {
        return;
    }
    let Some(name) = decl.name() else {
        return;
    };
    out.push(
        Diagnostic::new(
            codes::MISSING_TYPE_ANNOTATION,
            format!("constant `{}` has no type", name.text()),
        )
        .with_primary(name.span(), "")
        .with_help(format!(
            "write the type after the name, such as `const {}: Int = …` (D-011)",
            name.text()
        )),
    );
}

/// E0022, E0023 and E0013 for type declarations.
pub(super) fn type_decl(decl: TypeDecl<'_>, out: &mut Vec<Diagnostic>) {
    exposed(decl, out);
    if let Some(params) = decl.type_params() {
        param_bounds(params, "a type declaration", out);
    }
    if let Some(TypeBody::Record(fields)) = decl.body()
        && fields.fields().next().is_none()
        && !has_error_child(fields.syntax())
    {
        let name = decl.name().map_or("T", |name| name.text());
        out.push(
            Diagnostic::new(
                codes::SYNTAX_ERROR,
                "a record type needs at least one field",
            )
            .with_primary(fields.significant_span(), "")
            .with_help(format!(
                "for a type without fields, declare a unit variant, such as \
                     `type {name} {{ {name} }}`"
            )),
        );
    }
}

/// E0022: `exposed` requires `pub` (D-150). `exposed` on anything but a
/// type declaration is a grammar error.
fn exposed(decl: TypeDecl<'_>, out: &mut Vec<Diagnostic>) {
    let Some(keyword) = decl.exposed_keyword() else {
        return;
    };
    let visibility = decl.visibility();
    if visibility.is_some_and(Visibility::is_pub) {
        return;
    }
    let mut diag = Diagnostic::new(codes::INVALID_EXPOSED, "`exposed` requires `pub`")
        .with_primary(keyword.span(), "");
    if let Some(private) = visibility.and_then(Visibility::keyword) {
        diag = diag
            .with_secondary(private.span(), "this keeps the type inside its module")
            .with_suggestion("make the type `pub`", private.span(), "pub");
    } else {
        let at = empty_at(decl.syntax(), keyword.text_range().start());
        diag = diag.with_suggestion("make the type `pub` as well", at, "pub ");
    }
    out.push(diag);
}

/// E0023: bounds on the parameters of a type declaration or alias (§4.3).
fn param_bounds(params: TypeParamList<'_>, what: &str, out: &mut Vec<Diagnostic>) {
    for param in params.params() {
        let Some(bounds) = param.bounds() else {
            continue;
        };
        out.push(
            Diagnostic::new(
                codes::INVALID_BOUND,
                format!("bounds are not allowed on the type parameters of {what}"),
            )
            .with_primary(bounds.significant_span(), "")
            .with_help("put the bound on the functions that need it"),
        );
    }
}

/// E0023 for type aliases.
pub(super) fn alias_decl(decl: AliasDecl<'_>, out: &mut Vec<Diagnostic>) {
    if let Some(params) = decl.type_params() {
        param_bounds(params, "a type alias", out);
    }
}

/// E0023: an interface in a bound or a `dyn` type has no type arguments
/// (App. A.2, `interface_ref`).
pub(super) fn interface_ref(iref: InterfaceRef<'_>, out: &mut Vec<Diagnostic>) {
    let Some(args) = iref.type_args() else {
        return;
    };
    out.push(
        Diagnostic::new(
            codes::INVALID_BOUND,
            "an interface is named without type arguments",
        )
        .with_primary(args.significant_span(), "")
        .with_suggestion("remove the type arguments", args.significant_span(), ""),
    );
}

/// E0013: a variant's field list is not empty (App. A.2, `variant_body`).
pub(super) fn variant(variant: Variant<'_>, out: &mut Vec<Diagnostic>) {
    let Some(fields) = variant.fields() else {
        return;
    };
    let empty = match fields {
        VariantFields::Positional(list) => list.types().next().is_none(),
        VariantFields::Record(list) => list.fields().next().is_none(),
    };
    if !empty || has_error_child(fields.syntax()) {
        return;
    }
    let span = fields.significant_span();
    out.push(
        Diagnostic::new(codes::SYNTAX_ERROR, "a variant's field list is empty")
            .with_primary(span, "")
            .with_suggestion(
                "for a variant without fields, remove the brackets",
                span,
                "",
            ),
    );
}

/// E0013: an import has a list or an alias, not both (App. A.1).
pub(super) fn import_decl(import: ImportDecl<'_>, out: &mut Vec<Diagnostic>) {
    let (Some(list), Some(rename)) = (import.list(), import.rename()) else {
        return;
    };
    let removal = Span::new(
        rename.span().file,
        TextRange::new(
            list.significant_span().range.end(),
            rename.significant_span().range.end(),
        ),
    );
    out.push(
        Diagnostic::new(
            codes::SYNTAX_ERROR,
            "an import has either a list of names or an alias, not both",
        )
        .with_primary(rename.significant_span(), "")
        .with_suggestion("remove the alias", removal, ""),
    );
}

/// E0013: an imported name keeps its case when renamed (App. A.1,
/// `import_item`).
pub(super) fn import_item(item: ImportItem<'_>, out: &mut Vec<Diagnostic>) {
    let (Some(name), Some(new)) = (item.name(), item.rename().and_then(Rename::name)) else {
        return;
    };
    if name.is_type_name() == new.is_type_name() {
        return;
    }
    out.push(
        Diagnostic::new(
            codes::SYNTAX_ERROR,
            format!(
                "`{} as {}` changes the case of the name",
                name.text(),
                new.text()
            ),
        )
        .with_primary(new.span(), "")
        .with_help(
            "a type or variant is renamed to an upper-case name, a value to a lower-case one",
        ),
    );
}

/// E0013: a local function has no visibility and no type parameters, and
/// has a body (App. A.4, `local_fn`; D-123).
pub(super) fn local_fn(function: LocalFn<'_>, out: &mut Vec<Diagnostic>) {
    if let Some(visibility) = function.visibility() {
        let fn_start = function
            .syntax()
            .first_token_of_kind(SyntaxKind::FN_KW)
            .map_or(visibility.significant_span().range.end(), |t| {
                t.text_range().start()
            });
        let removal = Span::new(
            visibility.span().file,
            TextRange::new(visibility.significant_span().range.start(), fn_start),
        );
        out.push(
            Diagnostic::new(codes::SYNTAX_ERROR, "a local function has no visibility")
                .with_primary(visibility.significant_span(), "")
                .with_suggestion("remove it", removal, ""),
        );
    }
    if let Some(params) = function.type_params() {
        out.push(
            Diagnostic::new(
                codes::SYNTAX_ERROR,
                "a local function cannot have type parameters",
            )
            .with_primary(params.significant_span(), "")
            .with_help(
                "local functions are monomorphic (D-123); declare a top-level function to \
                 make it generic",
            ),
        );
    }
    if function.body().is_none() && function.param_list().is_some() {
        let label_span = function
            .name()
            .map_or_else(|| function.significant_span(), AstToken::span);
        out.push(
            Diagnostic::new(codes::SYNTAX_ERROR, "a local function needs a body")
                .with_primary(label_span, "")
                .with_help("write the body in `{ … }` after the parameters"),
        );
    }
}

/// E0013: an `as` message needs `let assert` (App. A.4, D-159).
pub(super) fn let_stmt(stmt: LetStmt<'_>, out: &mut Vec<Diagnostic>) {
    if stmt.is_assert() {
        return;
    }
    let Some(message) = stmt.message() else {
        return;
    };
    let mut diag = Diagnostic::new(codes::SYNTAX_ERROR, "a `let` with a message needs `assert`")
        .with_primary(message.significant_span(), "");
    if let Some(let_kw) = stmt.syntax().first_token_of_kind(SyntaxKind::LET_KW) {
        let at = empty_at(stmt.syntax(), let_kw.text_range().end());
        diag = diag.with_suggestion(
            "write `let assert` for a refutable pattern (D-159)",
            at,
            " assert",
        );
    }
    out.push(diag);
}

/// E0013: no comma between the last `use` binder and `<-` (App. A.4).
pub(super) fn use_stmt(stmt: UseStmt<'_>, out: &mut Vec<Diagnostic>) {
    let mut last_comma = None;
    for child in stmt.syntax().children_with_tokens() {
        match child {
            SyntaxElement::Token(token) if token.kind() == SyntaxKind::COMMA => {
                last_comma = Some(token);
            }
            SyntaxElement::Token(token) if token.kind() == SyntaxKind::LEFT_ARROW => break,
            SyntaxElement::Token(token)
                if token.kind().is_trivia() || token.kind() == SyntaxKind::NEWLINE => {}
            _ => last_comma = None,
        }
    }
    let Some(comma) = last_comma else {
        return;
    };
    out.push(
        Diagnostic::new(
            codes::SYNTAX_ERROR,
            "trailing comma after the `use` binders",
        )
        .with_primary(comma.span(), "")
        .with_suggestion("remove the comma", comma.span(), ""),
    );
}
