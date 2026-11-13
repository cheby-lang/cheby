//! `textDocument/documentLink` (X §2.9): each import's module path links
//! to the module's file.
//!
//! Class `Doc` (plan §1.10): it reads the document's tree in the snapshot,
//! the snapshot's module table for the files, and std's place on disk
//! (D-1369). It needs no resolution, so it works on a file with grammar
//! errors as far as its imports parse (X §1.3, L0). A path the run has no
//! module for gets no link, and nothing else in the file gets one: not
//! doc comments, not `cheby.toml`. There is no `documentLink/resolve`.

use std::path::Path;

use cheby_syntax::{SyntaxKind, SyntaxNode};
use lsp_types::{DocumentLink, DocumentLinkParams};
use serde_json::Value;

use super::hierarchy::{Locator, module_path};
use crate::features::{Analyzed, HandlerOutput, RequestCx, RequestError, params};

/// `textDocument/documentLink` (class `Doc`). `std_dir` is where std's
/// sources are on disk (D-1369): without it, std imports get no link.
///
/// # Errors
///
/// `InvalidParams` for parameters of another shape, `Cancelled` when the
/// client cancels.
pub(crate) fn document_link(
    cx: &RequestCx<'_>,
    analyzed: &Analyzed,
    params_value: Value,
    std_dir: Option<&Path>,
) -> Result<HandlerOutput, RequestError> {
    let p: DocumentLinkParams = params(params_value)?;
    let snapshot = &analyzed.snapshot;
    let mut locator = Locator::new(snapshot, std_dir, cx.encoding);
    let tree = locator
        .open(&p.text_document.uri)
        .and_then(|file| Some((file, snapshot.module_at(file)?.tree()?)));
    let Some((file, tree)) = tree else {
        return Ok(HandlerOutput::value(Value::Null));
    };
    let mut links = Vec::new();
    for path in import_paths(tree.root()) {
        cx.cancel.check()?;
        let text = segments(path).join("::");
        let target = module_path(&text)
            .and_then(|module| snapshot.table().find(&module))
            .and_then(|id| snapshot.module(id)?.file())
            .and_then(|target| locator.uri(target));
        let (Some(target), Some(range)) = (target, locator.range(file, path.text_range())) else {
            continue;
        };
        links.push(DocumentLink {
            range,
            target: Some(target),
            tooltip: Some(text),
            data: None,
        });
    }
    serde_json::to_value(links)
        .map(HandlerOutput::value)
        .map_err(|e| RequestError::Failed(e.to_string()))
}

/// The module path of every import under `root`, in text order.
fn import_paths(root: SyntaxNode<'_>) -> impl Iterator<Item = SyntaxNode<'_>> {
    root.descendants().filter(|node| {
        node.kind() == SyntaxKind::MODULE_PATH
            && node
                .parent()
                .is_some_and(|parent| parent.kind() == SyntaxKind::IMPORT_DECL)
    })
}

/// The names of a module path, as written.
fn segments(path: SyntaxNode<'_>) -> Vec<&str> {
    path.tokens()
        .filter(|t| t.kind() == SyntaxKind::LOWER_IDENT)
        .map(cheby_syntax::SyntaxToken::text)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::{Value, json};

    use super::document_link;
    use crate::features::Analyzed;
    use crate::features::handlers::symbols::tests::with_cx;
    use crate::features::ide::hierarchy::tests::{Temp, analyze};
    use crate::text::uri::path_to_uri;

    const OTHER: &str = "pub fn g() -> Int {\n  1\n}\n";

    fn uri(path: &Path) -> String {
        path_to_uri(path).unwrap().as_str().to_owned()
    }

    /// The links of `file` as `(range, target, tooltip)`.
    fn links(
        analyzed: &Analyzed,
        file: &Path,
        std_dir: Option<&Path>,
    ) -> Vec<(Value, Value, Value)> {
        let p = json!({ "textDocument": { "uri": uri(file) } });
        let got = with_cx(|cx| document_link(cx, analyzed, p, std_dir))
            .unwrap()
            .value;
        got.as_array()
            .expect("a list")
            .iter()
            .map(|l| {
                (
                    l["range"].clone(),
                    l["target"].clone(),
                    l["tooltip"].clone(),
                )
            })
            .collect()
    }

    fn range(line: u32, start: u32, end: u32) -> Value {
        json!({
            "start": { "line": line, "character": start },
            "end": { "line": line, "character": end },
        })
    }

    fn check(name: &str, main_text: &str) {
        let temp = Temp::package(name, &[("main.cheby", main_text), ("other.cheby", OTHER)]);
        let main = temp.src("main.cheby");
        let other = temp.src("other.cheby");
        let analyzed = analyze(&temp.0, &[(main.clone(), main_text)]);
        let std_dir = temp.0.join("std-src");
        let got = links(&analyzed, &main, Some(&std_dir));
        assert_eq!(
            got,
            [
                (
                    range(0, 7, 14),
                    json!(uri(&std_dir.join("std/io.cheby"))),
                    json!("std::io")
                ),
                (range(1, 7, 17), json!(uri(&other)), json!("app::other")),
            ],
            "{name}"
        );
        // Without std's place, std gets no link.
        assert_eq!(links(&analyzed, &main, None).len(), 1, "{name}");
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn imports_link_to_their_files() {
        check(
            "links_clean",
            "import std::io\nimport app::other::{g}\n\nfn main() {\n  io::println(\"{g()}\")\n}\n",
        );
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn a_type_error_keeps_the_links() {
        check(
            "links_type_error",
            "import std::io\nimport app::other::{g}\n\nfn main() {\n  io::println(g())\n}\n",
        );
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn a_grammar_error_keeps_the_links() {
        check(
            "links_grammar_error",
            "import std::io\nimport app::other::{g}\nimport app::missing\n\nfn main() {\n  io::println(\"{g()}\"\n}\n",
        );
    }
}
