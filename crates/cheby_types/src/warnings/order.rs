//! W0014: imports that are not in `cheby fmt`'s blocks and order (§13.7,
//! §13.8, D-1953).
//!
//! The rule and the section are `cheby_syntax::imports`'s, which the
//! formatter reads too, so a file that `cheby fmt` wrote is never reported
//! and the fix leaves the imports as the formatter would. A file gets one
//! warning, at the first import the formatter would move or space
//! differently, and its fix rewrites the whole section: every import and
//! comment as written, in order.
//!
//! Only the modules of the local package and of the standard library are
//! checked. A dependency's module is not the user's to reorder, and its
//! own name is not known here.

use cheby_diag::{Diagnostic, Edit, Span, codes};
use cheby_iface::PackageId;
use cheby_syntax::imports::{ImportChunk, Misplaced, import_section};

use crate::TypesInput;

/// The W0014 warning of the module, if its imports are out of order.
pub(super) fn import_order(input: &TypesInput<'_>) -> Option<Diagnostic> {
    if input.package != PackageId::LOCAL && input.package != PackageId::STD {
        return None;
    }
    let package = input.modules.get(input.module)?.path.package();
    let tree = input.tree;
    let section = import_section(tree.root(), Some(package))?;
    let misplaced = section.misplaced()?;
    let chunk = |index: usize| section.chunks().get(index);
    let path = |c: &ImportChunk<'_>| c.key.0.clone();
    let (at, label) = match misplaced {
        Misplaced::Order { at, wanted } => {
            let wanted = chunk(wanted)?;
            (
                chunk(at)?,
                format!("`{}` comes before this import", path(wanted)),
            )
        }
        Misplaced::NoBlank(index) => {
            let at = chunk(index)?;
            (
                at,
                format!("a blank line comes before {} imports", at.class.describe()),
            )
        }
        Misplaced::Blank(index) => (
            chunk(index)?,
            "no blank line comes before this import".to_owned(),
        ),
    };
    let range = section.range();
    let eol = if range.slice(tree.text()).is_some_and(|t| t.contains("\r\n")) {
        "\r\n"
    } else {
        "\n"
    };
    let file = at.import.span().file;
    Some(
        Diagnostic::new(codes::IMPORT_ORDER, "the imports are not in order")
            .with_primary(at.import.span(), label)
            .with_note(
                "imports come in up to three blocks, each sorted by path: the standard \
                 library's, other packages', then the package's own",
            )
            .with_fix_edits(
                "put the imports in order, as `cheby fmt` does",
                vec![Edit {
                    span: Span::new(file, range),
                    replacement: section.rewritten(eol),
                }],
            ),
    )
}
