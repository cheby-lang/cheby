//! The import section of a file and the order `cheby fmt` gives it
//! (D-1953).
//!
//! A file's imports come in at most three blocks, with one blank line
//! between two blocks: the standard library's modules, then the modules of
//! other packages, then the modules of the file's own package. Inside a
//! block the imports are sorted by module path in byte order, and imports
//! of one path by their whole text.
//!
//! The formatter writes this order, W0014 reports a file that is not in
//! it, and the language server puts a new import where it belongs. All
//! three read the section and its order from here, so they cannot
//! disagree.
//!
//! A comment travels with its import: the comments on lines of their own
//! above it, and a comment at the end of its line. Above the first import,
//! only the comment lines that touch it travel, so a file's header stays
//! at the top. A comment after the last import is not part of the section.

use cheby_diag::TextRange;

use crate::{SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};

/// The block an import belongs to, in the order of the blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ImportClass {
    /// A module of the standard library: the path starts with `std`.
    Std,
    /// A module of another package: the path starts with a dependency key.
    Package,
    /// A module of the file's own package: the path starts with its name.
    Local,
}

impl ImportClass {
    /// The class of an import whose path starts with `first`, in a file
    /// of the package named `package`. A file of no package has no
    /// local imports, and in the standard library's own files every
    /// `std` import is the library's.
    #[must_use]
    pub fn of(first: &str, package: Option<&str>) -> Self {
        if first == "std" {
            Self::Std
        } else if package == Some(first) {
            Self::Local
        } else {
            Self::Package
        }
    }

    /// The block's name in a message.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Std => "the standard library's",
            Self::Package => "other packages'",
            Self::Local => "the package's own",
        }
    }
}

/// One import with the comments that travel with it.
#[derive(Clone, Debug)]
pub struct ImportChunk<'t> {
    /// The comments on lines of their own above the import.
    pub leading: Vec<SyntaxToken<'t>>,
    pub import: SyntaxNode<'t>,
    /// The comment at the end of the import's line.
    pub trailing: Option<SyntaxToken<'t>>,
    pub class: ImportClass,
    /// The sort key inside a block: the module path's text, then the
    /// import's whole text with its tokens joined by single spaces.
    pub key: (String, String),
    /// A blank line stands before the chunk.
    pub blank_before: bool,
    /// A blank line stands inside the chunk: between two of its comments,
    /// or between the last of them and the import.
    pub loose: bool,
}

impl ImportChunk<'_> {
    /// From the first comment above the import, or the import, to the end
    /// of the import or of the comment on its line.
    #[must_use]
    pub fn range(&self) -> TextRange {
        let start = self.leading.first().map_or_else(
            || self.import.text_range().start(),
            |c| c.text_range().start(),
        );
        let end = self
            .trailing
            .map_or_else(|| self.import.text_range().end(), |c| c.text_range().end());
        TextRange::new(start, end)
    }
}

/// Why a section is not in `cheby fmt`'s order: the first thing the
/// formatter would change, by chunk index in source order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Misplaced {
    /// The chunk `wanted` belongs where the chunk `at` stands.
    Order { at: usize, wanted: usize },
    /// The chunk starts a block and has no blank line before it.
    NoBlank(usize),
    /// The chunk continues a block and has a blank line before it, or
    /// between its comments and its import.
    Blank(usize),
}

/// The imports of a file that come before its first item, each with its
/// comments, in source order.
#[derive(Clone, Debug)]
pub struct ImportSection<'t> {
    chunks: Vec<ImportChunk<'t>>,
}

/// The sort key of `import` inside its block.
#[must_use]
pub fn import_key(import: SyntaxNode<'_>) -> (String, String) {
    let significant =
        |t: &SyntaxToken<'_>| !t.kind().is_trivia() && t.kind() != SyntaxKind::NEWLINE;
    let path = import
        .first_child_of_kind(SyntaxKind::MODULE_PATH)
        .map_or_default(|p| {
            p.tokens()
                .filter(significant)
                .map(SyntaxToken::text)
                .collect()
        });
    let whole: Vec<&str> = import
        .tokens()
        .filter(significant)
        .map(SyntaxToken::text)
        .collect();
    (path, whole.join(" "))
}

/// The first segment of `import`'s module path.
#[must_use]
pub fn first_segment(import: SyntaxNode<'_>) -> &str {
    import
        .first_child_of_kind(SyntaxKind::MODULE_PATH)
        .and_then(SyntaxNode::first_significant_token)
        .map_or("", SyntaxToken::text)
}

/// The import section of the file whose root is `root`, in a package
/// named `package`. `None` for a file without imports before its
/// first item.
#[must_use]
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "a match on syntax kinds names only the kinds it handles"
)]
pub fn import_section<'t>(
    root: SyntaxNode<'t>,
    package: Option<&str>,
) -> Option<ImportSection<'t>> {
    let mut chunks: Vec<ImportChunk<'t>> = Vec::new();
    // The comments on lines of their own since the last import, each with
    // the line breaks before it.
    let mut pending: Vec<(SyntaxToken<'t>, usize)> = Vec::new();
    let mut newlines = 0usize;
    let mut after_import = false;
    let mut at_start = true;
    for element in root.children_with_tokens() {
        match element {
            SyntaxElement::Token(token) => match token.kind() {
                SyntaxKind::WHITESPACE => continue,
                SyntaxKind::NEWLINE => {
                    newlines = newlines.saturating_add(1);
                    continue;
                }
                SyntaxKind::COMMENT | SyntaxKind::DOC_COMMENT => {
                    if newlines == 0 && !at_start {
                        // At the end of a line: the import's own comment,
                        // or the comment of whatever else the line holds.
                        match chunks.last_mut() {
                            Some(chunk) if after_import => chunk.trailing = Some(token),
                            Some(_) | None => pending.clear(),
                        }
                    } else {
                        pending.push((token, newlines));
                    }
                }
                // A module comment is the file's header, which nothing
                // above an import reaches past. After an import it is out
                // of place, and the section ends.
                _ if chunks.is_empty() => pending.clear(),
                _ => break,
            },
            SyntaxElement::Node(node) if node.kind() == SyntaxKind::IMPORT_DECL => {
                let leading = if chunks.is_empty() {
                    touching(&pending, newlines)
                } else {
                    pending.as_slice()
                };
                let blank_before = leading.first().map_or(newlines, |(_, n)| *n) > 1;
                let loose = !leading.is_empty()
                    && (newlines > 1 || leading.iter().skip(1).any(|(_, n)| *n > 1));
                chunks.push(ImportChunk {
                    leading: leading.iter().map(|(token, _)| *token).collect(),
                    import: node,
                    trailing: None,
                    class: ImportClass::of(first_segment(node), package),
                    key: import_key(node),
                    blank_before,
                    loose,
                });
                pending.clear();
            }
            // Inner attributes come before the imports.
            SyntaxElement::Node(node)
                if chunks.is_empty() && node.kind() == SyntaxKind::INNER_ATTR =>
            {
                pending.clear();
            }
            SyntaxElement::Node(_) => break,
        }
        after_import =
            matches!(element, SyntaxElement::Node(n) if n.kind() == SyntaxKind::IMPORT_DECL);
        at_start = false;
        newlines = 0;
    }
    (!chunks.is_empty()).then_some(ImportSection { chunks })
}

/// The comments at the end of `pending` that touch the first import,
/// which has `newlines` line breaks before it: no blank line between
/// them, or between the last of them and the import.
fn touching<'a, 't>(
    pending: &'a [(SyntaxToken<'t>, usize)],
    newlines: usize,
) -> &'a [(SyntaxToken<'t>, usize)] {
    if newlines != 1 {
        return &[];
    }
    let mut start = pending.len();
    while start > 0 {
        start = start.saturating_sub(1);
        // The comment at `start` is the first of the run when a blank
        // line stands before it.
        if pending.get(start).is_none_or(|(_, n)| *n != 1) {
            break;
        }
    }
    pending.get(start..).unwrap_or(&[])
}

impl<'t> ImportSection<'t> {
    /// The imports with their comments, in source order.
    #[must_use]
    pub fn chunks(&self) -> &[ImportChunk<'t>] {
        &self.chunks
    }

    /// From the first chunk's start to the last chunk's end.
    #[must_use]
    pub fn range(&self) -> TextRange {
        let start = self.chunks.first().map_or(0, |c| c.range().start());
        let end = self.chunks.last().map_or(start, |c| c.range().end());
        TextRange::new(start, end)
    }

    /// The chunks' indexes in `cheby fmt`'s order: by block, then by
    /// key. Two imports with one key keep their order.
    #[must_use]
    pub fn order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.chunks.len()).collect();
        order.sort_by(|a, b| {
            let key = |i: &usize| self.chunks.get(*i).map(|c| (c.class, &c.key));
            key(a).cmp(&key(b))
        });
        order
    }

    /// Whether the chunk at `position` of `order` starts a block: it is
    /// not the first, and the chunk before it is of another class.
    #[must_use]
    pub fn starts_block(&self, order: &[usize], position: usize) -> bool {
        let class = |p: usize| {
            order
                .get(p)
                .and_then(|i| self.chunks.get(*i))
                .map(|c| c.class)
        };
        position
            .checked_sub(1)
            .is_some_and(|before| class(before) != class(position))
    }

    /// The first thing `cheby fmt` would change in the section, or
    /// `None` when it is in order.
    #[must_use]
    pub fn misplaced(&self) -> Option<Misplaced> {
        let order = self.order();
        if let Some((at, wanted)) = order
            .iter()
            .enumerate()
            .find(|(position, index)| position != *index)
        {
            return Some(Misplaced::Order {
                at,
                wanted: *wanted,
            });
        }
        for (index, chunk) in self.chunks.iter().enumerate() {
            let starts = self.starts_block(&order, index);
            if index > 0 && starts && !chunk.blank_before {
                return Some(Misplaced::NoBlank(index));
            }
            if (index > 0 && !starts && chunk.blank_before) || chunk.loose {
                return Some(Misplaced::Blank(index));
            }
        }
        None
    }

    /// The section's text in `cheby fmt`'s order, with `eol` for a line
    /// break: every import and comment as written, and one blank line
    /// between two blocks. It replaces [`Self::range`].
    #[must_use]
    pub fn rewritten(&self, eol: &str) -> String {
        let order = self.order();
        let mut out = String::new();
        for (position, index) in order.iter().enumerate() {
            let Some(chunk) = self.chunks.get(*index) else {
                continue;
            };
            if position > 0 {
                out.push_str(eol);
                if self.starts_block(&order, position) {
                    out.push_str(eol);
                }
            }
            for comment in &chunk.leading {
                out.push_str(comment.text().trim_end());
                out.push_str(eol);
            }
            out.push_str(chunk.import.text().trim_end());
            if let Some(comment) = chunk.trailing {
                out.push(' ');
                out.push_str(comment.text().trim_end());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cheby_diag::FileId;

    use super::{ImportClass, Misplaced, import_section};

    /// The section of `text` in the package `app`: what is wrong with
    /// it, and its text in order.
    fn organized(text: &str) -> Option<(Option<Misplaced>, String)> {
        let parse = crate::parse(FileId::new(0), Arc::from(text));
        assert!(parse.errors().is_empty(), "{:?}", parse.errors());
        let section = import_section(parse.tree().root(), Some("app"))?;
        Some((section.misplaced(), section.rewritten("\n")))
    }

    #[test]
    fn classes_follow_the_first_segment() {
        assert_eq!(ImportClass::of("std", Some("app")), ImportClass::Std);
        assert_eq!(ImportClass::of("app", Some("app")), ImportClass::Local);
        assert_eq!(ImportClass::of("json", Some("app")), ImportClass::Package);
        assert_eq!(ImportClass::of("app", None), ImportClass::Package);
        // In the standard library's own files every import is the library's.
        assert_eq!(ImportClass::of("std", Some("std")), ImportClass::Std);
        assert!(ImportClass::Std < ImportClass::Package);
        assert!(ImportClass::Package < ImportClass::Local);
    }

    #[test]
    fn three_blocks_in_order() {
        let text = "import app::x\nimport std::list\nimport json\nimport std::io\nimport app\n\nfn f() {}\n";
        let (misplaced, out) = organized(text).unwrap();
        assert_eq!(misplaced, Some(Misplaced::Order { at: 0, wanted: 3 }));
        assert_eq!(
            out,
            "import std::io\nimport std::list\n\nimport json\n\nimport app\nimport app::x"
        );
        let (again, same) = organized(&format!("{out}\n\nfn f() {{}}\n")).unwrap();
        assert_eq!((again, same), (None, out));
    }

    #[test]
    fn blank_lines_are_the_blocks_only() {
        let split = "import std::io\n\nimport std::list\n";
        assert_eq!(organized(split).unwrap().0, Some(Misplaced::Blank(1)));
        let joined = "import std::io\nimport app::x\n";
        assert_eq!(organized(joined).unwrap().0, Some(Misplaced::NoBlank(1)));
        assert_eq!(
            organized(joined).unwrap().1,
            "import std::io\n\nimport app::x"
        );
    }

    #[test]
    fn comments_travel_with_their_import() {
        let text = "//! Doc.\n\n// header\n\n// about x\nimport app::x // last\n// lone\n\nimport std::io\n// after\n\nfn f() {}\n";
        let (misplaced, out) = organized(text).unwrap();
        assert_eq!(misplaced, Some(Misplaced::Order { at: 0, wanted: 1 }));
        assert_eq!(
            out,
            "// lone\nimport std::io\n\n// about x\nimport app::x // last"
        );
    }

    /// A header that a blank line separates from the first import stays,
    /// and one that touches it travels.
    #[test]
    fn the_header_stays_above_a_blank_line() {
        let parse = |text: &str| crate::parse(FileId::new(0), Arc::from(text));
        let apart = parse("// header\n\nimport std::io\n");
        let section = import_section(apart.tree().root(), None).unwrap();
        assert_eq!(section.chunks()[0].leading.len(), 0);
        let touching = parse("// one\n\n// two\n// three\nimport std::io\n");
        let section = import_section(touching.tree().root(), None).unwrap();
        assert_eq!(section.chunks()[0].leading.len(), 2);
        assert_eq!(section.misplaced(), None);
    }

    #[test]
    fn a_blank_line_under_a_comment_is_out_of_order() {
        let text = "import std::io\n\n// local\n\nimport app::x\n";
        let (misplaced, out) = organized(text).unwrap();
        assert_eq!(misplaced, Some(Misplaced::Blank(1)));
        assert_eq!(out, "import std::io\n\n// local\nimport app::x");
    }

    /// Imports after an item are not the section's, and a file whose
    /// first import follows an item has none.
    #[test]
    fn the_section_ends_at_the_first_item() {
        let text = "import std::list\n\nfn f() {}\n\nimport std::io\n";
        let (misplaced, out) = organized(text).unwrap();
        assert_eq!((misplaced, out.as_str()), (None, "import std::list"));
        assert!(organized("fn f() {}\n\nimport std::io\n").is_none());
        assert!(organized("fn f() {}\n").is_none());
    }

    #[test]
    fn an_inner_attribute_comes_before_the_section() {
        let text = "@!target(js)\n\nimport std::list\nimport std::io\n";
        let (misplaced, out) = organized(text).unwrap();
        assert_eq!(misplaced, Some(Misplaced::Order { at: 0, wanted: 1 }));
        assert_eq!(out, "import std::io\nimport std::list");
    }
}
