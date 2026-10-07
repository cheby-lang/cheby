//! The support gate (D-451): E0038 for well-formed constructs that
//! the compiler cannot check and run yet, once per outermost construct.
//! A hit in an item's declaration makes it [`ItemStatus::Unsupported`]; a
//! hit inside a function body only makes it
//! [`ItemStatus::BodyUnsupported`], so the item's interface never depends
//! on its body (D-246, §13.12.3).
//!
//! This gate looks at syntax shapes only. Shapes that need resolution
//! (a function used as a value, an interface function path) are gated by
//! the body resolver, and types other than `String` and `Nil` by
//! `cheby_types`, so that `cheby check` and `cheby run` agree. Each later
//! M1 phase deletes its rows here when it lands.

use cheby_diag::{Diagnostic, Span, TextRange, codes};
use cheby_iface::{DefIndex, Ice, IndexVec};
use cheby_syntax::ast::{Arg, AstNode, Block, Expr, Item, LiteralKind, Pat, SourceFile, Stmt};

use cheby_package::Targets;

use crate::{ItemAttrs, ItemDef, ItemStatus, ResolveInput, items::Collected};

/// The E0038 diagnostic: "`<what>` are not supported yet".
pub(crate) fn not_supported(span: Span, what: &str) -> Diagnostic {
    Diagnostic::new(
        codes::NOT_SUPPORTED_YET,
        format!("{what} are not supported yet"),
    )
    .with_primary(span, "")
}

pub(crate) fn gate_module(
    input: &ResolveInput<'_>,
    collected: &mut Collected,
    diags: &mut Vec<Diagnostic>,
) -> Result<(), Ice> {
    for &(span, what) in &collected.inner_gates {
        diags.push(not_supported(span, what));
    }
    for request in input.imports {
        if let Some(span) = request.items_span {
            diags.push(not_supported(span, "item imports"));
        }
    }
    let items = &mut collected.items;
    for &(index, span, what) in &collected.attr_gates {
        diags.push(not_supported(span, what));
        mark(items, index)?;
    }
    let Some(file) = SourceFile::cast(input.tree.root()) else {
        return Err(Ice::new("the tree's root is not a source file"));
    };
    // What the module's code runs on: its package's targets, narrowed by
    // `@!target` (§12.6).
    let required = input.package_targets.intersection(collected.module_targets);
    for item in file.items() {
        let Some(index) = index_of(items, item) else {
            continue;
        };
        let found = gate_item(item, items, index, required);
        if !found.decl.is_empty() {
            mark(items, index)?;
        } else if !found.body.is_empty() {
            mark_body(items, index)?;
        }
        diags.extend(found.decl);
        diags.extend(found.body);
    }
    Ok(())
}

/// Marks a function whose body alone holds unsupported constructs, unless
/// its declaration is already unsupported.
pub(crate) fn mark_body(
    items: &mut IndexVec<DefIndex, ItemDef>,
    index: DefIndex,
) -> Result<(), Ice> {
    let item = items
        .get_mut(index)
        .ok_or_else(|| Ice::new("gated item is missing"))?;
    if item.status == ItemStatus::Ok {
        item.status = ItemStatus::BodyUnsupported;
    }
    Ok(())
}

/// Marks an item and its members unsupported.
fn mark(items: &mut IndexVec<DefIndex, ItemDef>, index: DefIndex) -> Result<(), Ice> {
    items
        .get_mut(index)
        .ok_or_else(|| Ice::new("gated item is missing"))?
        .status = ItemStatus::Unsupported;
    for item in items.iter_mut() {
        if item.parent == Some(index) {
            item.status = ItemStatus::Unsupported;
        }
    }
    Ok(())
}

fn index_of(items: &IndexVec<DefIndex, ItemDef>, item: Item<'_>) -> Option<DefIndex> {
    let node = item.syntax().id();
    items
        .iter_enumerated()
        .find(|(_, def)| def.node == node)
        .map(|(index, _)| index)
}

/// From the start of `node` to the end of its name: `pub type Shape`.
fn head_span(item: Item<'_>, name_end: Option<Span>) -> Span {
    let full = item.significant_span();
    let keyword_start = item.keyword().map(|t| t.span().range.start());
    let start = item
        .visibility()
        .map(|v| v.significant_span().range.start())
        .or(keyword_start)
        .unwrap_or(full.range.start());
    let end = name_end.map_or(full.range.end(), |s| s.range.end());
    Span::new(full.file, TextRange::new(start, end.max(start)))
}

/// The gate's findings for one item: in its declaration (kind, type
/// parameters, attributes, how it is implemented) and in its body.
#[derive(Default)]
struct Found {
    decl: Vec<Diagnostic>,
    body: Vec<Diagnostic>,
}

/// Whether an item with `attrs` exists on every target in `required`.
fn covers(required: Targets, attrs: Option<&ItemAttrs>) -> bool {
    attrs.is_some_and(|a| a.targets.intersection(required) == required)
}

fn gate_item(
    item: Item<'_>,
    items: &IndexVec<DefIndex, ItemDef>,
    index: DefIndex,
    required: Targets,
) -> Found {
    let mut found = Found::default();
    let out = &mut found.decl;
    let name_span = items.get(index).map(|def| def.name_span);
    match item {
        Item::Type(_) => out.push(not_supported(
            head_span(item, name_span),
            "type declarations",
        )),
        Item::Alias(_) => out.push(not_supported(head_span(item, name_span), "type aliases")),
        Item::Const(_) => out.push(not_supported(head_span(item, name_span), "constants")),
        Item::Interface(_) => out.push(not_supported(head_span(item, name_span), "interfaces")),
        Item::Test(_) => out.push(not_supported(head_span(item, name_span), "tests")),
        Item::Fn(f) => {
            if let Some(params) = f.type_params() {
                out.push(not_supported(params.significant_span(), "type parameters"));
            }
            let attrs = items.get(index).map(|def| &def.attrs);
            let has_externals = attrs.is_some_and(|a| !a.externals.is_empty());
            let native = attrs.and_then(|a| a.native_external()).is_some();
            // An attribute row already rejected the item.
            let already_gated = items
                .get(index)
                .is_some_and(|def| def.status != ItemStatus::Ok);
            match f.body() {
                Some(_) if has_externals => out.push(not_supported(
                    head_span(item, name_span),
                    "functions with both a body and an `@external` attribute",
                )),
                Some(body) => gate_block_contents(body, &mut found.body),
                // No external at all is E0046, reported by item collection.
                None if has_externals && !native => out.push(not_supported(
                    head_span(item, name_span),
                    "functions without an `@external(native, …)` implementation",
                )),
                // A bodiless function exists only on the targets of its
                // externals (§12.5), and code that uses it must be
                // restricted to them (§12.6, D-056, D-190). Until the
                // cross-target check (C12) lands, such a function is
                // rejected unless it exists on every target its module
                // is built for, so no unchecked use can get through.
                None if has_externals && !covers(required, attrs) && !already_gated => {
                    out.push(not_supported(
                        head_span(item, name_span),
                        "functions that exist only on some of the package's targets",
                    ));
                }
                None => {}
            }
        }
    }
    found
}

fn gate_block_contents(block: Block<'_>, out: &mut Vec<Diagnostic>) {
    for stmt in block.stmts() {
        match stmt {
            Stmt::Let(let_stmt) => {
                if let_stmt.is_assert() {
                    out.push(not_supported(
                        let_stmt.significant_span(),
                        "`let assert` statements",
                    ));
                    continue;
                }
                match let_stmt.pattern() {
                    Some(Pat::Ident(_) | Pat::Wildcard(_)) | None => {}
                    Some(pat) => out.push(not_supported(
                        pat.significant_span(),
                        "patterns other than a name or `_` in `let`",
                    )),
                }
                if let Some(value) = let_stmt.value() {
                    gate_expr(value, out);
                }
            }
            Stmt::Use(s) => out.push(not_supported(s.significant_span(), "`use` statements")),
            Stmt::Assert(s) => out.push(not_supported(s.significant_span(), "`assert` statements")),
            Stmt::LocalFn(s) => out.push(not_supported(s.significant_span(), "local functions")),
            Stmt::Expr(e) => gate_expr(e, out),
        }
    }
}

fn gate_expr(expr: Expr<'_>, out: &mut Vec<Diagnostic>) {
    let span = expr.significant_span();
    let what = match expr {
        Expr::String(s) if s.has_interpolation() => "interpolated strings",
        Expr::Literal(lit) => match lit.kind() {
            Some(LiteralKind::RawString(_)) => "raw strings",
            _ => "numeric literals",
        },
        Expr::Path(path) => {
            if path.type_args().is_some() {
                "type arguments on paths"
            } else {
                return;
            }
        }
        Expr::Call(call) => {
            match call.callee() {
                Some(Expr::Path(path)) if path.type_args().is_none() => {}
                Some(Expr::Path(path)) => {
                    out.push(not_supported(
                        path.significant_span(),
                        "type arguments on paths",
                    ));
                }
                Some(callee) => out.push(not_supported(
                    callee.significant_span(),
                    "calls of expressions other than a function's name",
                )),
                None => {}
            }
            for arg in call.args() {
                match arg {
                    Arg::Expr(e) => gate_expr(e, out),
                    Arg::Hole(hole) => {
                        out.push(not_supported(hole.significant_span(), "function captures"));
                    }
                }
            }
            return;
        }
        Expr::Record(r) if r.is_update() => "record updates",
        Expr::Record(_) => "record expressions",
        Expr::Tuple(_) => "tuples",
        Expr::Paren(_) => "parenthesized expressions",
        Expr::List(_) => "list expressions",
        Expr::Block(_) => "block expressions",
        Expr::Closure(_) => "anonymous functions",
        Expr::Case(_) => "`case` expressions",
        Expr::Panic(_) => "`panic` expressions",
        Expr::Todo(_) => "`todo` expressions",
        Expr::Binary(b) if b.op() == Some(cheby_syntax::ast::BinOp::Pipe) => "pipes",
        Expr::Binary(_) | Expr::Prefix(_) => "operators",
        // Holes outside argument lists are E0025 (phase A).
        Expr::String(_) | Expr::Hole(_) => return,
        Expr::Field(_) => "field access",
    };
    out.push(not_supported(span, what));
}
