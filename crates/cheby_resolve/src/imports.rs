//! Import requests, their targets and import cycles (B3, §7.5, D-145).

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{Ice, Interner, ModuleId, ModuleTable, Name, PackageId};
use cheby_syntax::SyntaxTree;
use cheby_syntax::ast::{AstNode, AstToken, ImportDecl, SourceFile};

/// One `import` as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportRequest {
    pub ast: cheby_syntax::ast::AstId<ImportDecl<'static>>,
    /// The module path's segments as written.
    pub path: Vec<(Name, Span)>,
    /// The span of the module path.
    pub path_span: Span,
    /// `as d`.
    pub alias: Option<(Name, Span)>,
    /// `::{x, Y as Z}`.
    pub items: Option<Vec<ImportItemRequest>>,
    /// The `{…}` list, if written.
    pub items_span: Option<Span>,
    /// The whole import.
    pub span: Span,
}

impl ImportRequest {
    /// The name the import binds for the module: the alias, or the last
    /// segment. `None` for an item import, which binds no module (D-144).
    #[must_use]
    pub fn module_binding(&self) -> Option<(Name, Span)> {
        if self.items.is_some() {
            return None;
        }
        self.alias.or_else(|| self.path.last().copied())
    }

    /// The path as text, such as `std::io`.
    #[must_use]
    pub fn path_text(&self, names: &Interner) -> String {
        self.path
            .iter()
            .map(|&(name, _)| names.get(name))
            .collect::<Vec<_>>()
            .join("::")
    }
}

/// One name of an item import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportItemRequest {
    pub name: Name,
    pub span: Span,
    pub alias: Option<(Name, Span)>,
}

/// The imports of a module, in source order. Imports with a missing path
/// (only possible in trees with syntax errors) are skipped.
#[must_use]
pub fn collect_imports(tree: &SyntaxTree, names: &Interner) -> Vec<ImportRequest> {
    let Some(file) = SourceFile::cast(tree.root()) else {
        return Vec::new();
    };
    file.imports()
        .filter_map(|import| {
            let module_path = import.module_path()?;
            let path: Vec<(Name, Span)> = module_path
                .segments()
                .map(|segment| (names.intern(segment.text()), segment.span()))
                .collect();
            if path.is_empty() {
                return None;
            }
            let alias = import
                .rename()
                .and_then(cheby_syntax::ast::Rename::name)
                .map(|name| (names.intern(name.text()), name.span()));
            let list = import.list();
            let items = list.map(|list| {
                list.items()
                    .filter_map(|item| {
                        let name = item.name()?;
                        Some(ImportItemRequest {
                            name: names.intern(name.text()),
                            span: name.span(),
                            alias: item
                                .rename()
                                .and_then(cheby_syntax::ast::Rename::name)
                                .map(|alias| (names.intern(alias.text()), alias.span())),
                        })
                    })
                    .collect()
            });
            Some(ImportRequest {
                ast: import.id(),
                path,
                path_span: module_path.significant_span(),
                alias,
                items,
                items_span: list.map(AstNode::significant_span),
                span: import.significant_span(),
            })
        })
        .collect()
}

/// Where a written module path leads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    Found(ModuleId),
    /// A standard-library module the spec defines but the toolchain does
    /// not ship yet (D-451).
    Planned,
    /// The package exists but has no such module. `closest` is a similar
    /// module path for the help line, if any.
    NoModule {
        package: PackageId,
        closest: Option<String>,
    },
    /// The first segment is neither the package's own name, nor `std`, nor
    /// a dependency key.
    NoPackage,
    /// `my_app::main`: the root module is imported by the package name
    /// alone (D-142).
    RootByMainSegment,
}

/// Maps written module paths to modules. The driver implements it, loading
/// and parsing a standard-library module on first use.
///
/// First-segment rules (§7.5, D-238): the importing package's own name
/// leads to its modules, `std` to the standard library, and any other name
/// to [`Lookup::NoPackage`] (dependencies arrive in M5). `pkg::main` is
/// [`Lookup::RootByMainSegment`] when `pkg` is a package (D-142).
pub trait ModuleLookup {
    /// Maps `path` (segments as written) imported from a module of package
    /// `from`. Called in a deterministic order.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the lookup breaks an invariant.
    fn lookup(&mut self, from: PackageId, path: &[&str]) -> Result<Lookup, Ice>;
}

/// The module an import refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportTarget {
    Module(ModuleId),
    /// The diagnostic was emitted; names through this import resolve to
    /// [`Res::Err`](crate::Res::Err).
    Failed,
}

impl ImportTarget {
    #[must_use]
    pub const fn module(self) -> Option<ModuleId> {
        match self {
            Self::Module(module) => Some(module),
            Self::Failed => None,
        }
    }
}

/// Maps each import of a module of package `from` to its target, in order,
/// reporting E0038 for planned standard-library modules and E0039 for
/// paths that lead nowhere.
///
/// # Errors
///
/// An [`Ice`] from the lookup.
pub fn resolve_import_targets(
    requests: &[ImportRequest],
    from: PackageId,
    lookup: &mut dyn ModuleLookup,
    names: &Interner,
) -> Result<(Vec<ImportTarget>, Vec<Diagnostic>), Ice> {
    let mut targets = Vec::with_capacity(requests.len());
    let mut diags = Vec::new();
    for request in requests {
        let segments: Vec<&str> = request.path.iter().map(|&(n, _)| names.get(n)).collect();
        let text = segments.join("::");
        let span = request.path_span;
        let target = match lookup.lookup(from, &segments)? {
            Lookup::Found(module) => ImportTarget::Module(module),
            Lookup::Planned => {
                diags.push(
                    Diagnostic::new(
                        codes::NOT_SUPPORTED_YET,
                        format!("the module `{text}` is not supported yet"),
                    )
                    .with_primary(span, ""),
                );
                ImportTarget::Failed
            }
            Lookup::NoModule { closest, .. } => {
                let mut diag = Diagnostic::new(
                    codes::UNRESOLVED_IMPORT,
                    format!("there is no module `{text}`"),
                )
                .with_primary(span, "no such module");
                if let Some(closest) = closest {
                    diag = diag.with_suggestion(
                        format!("a module with a similar name exists: `{closest}`"),
                        span,
                        closest,
                    );
                }
                diags.push(diag);
                ImportTarget::Failed
            }
            Lookup::NoPackage => {
                let package = segments.first().copied().unwrap_or("");
                let first = request.path.first().map_or(span, |&(_, s)| s);
                diags.push(
                    Diagnostic::new(
                        codes::UNRESOLVED_IMPORT,
                        format!("there is no package named `{package}`"),
                    )
                    .with_primary(first, "unknown package")
                    .with_note(
                        "an import starts with this package's name, `std`, or a dependency \
                         declared in `cheby.toml`",
                    ),
                );
                ImportTarget::Failed
            }
            Lookup::RootByMainSegment => {
                let package = segments.first().copied().unwrap_or("");
                diags.push(
                    Diagnostic::new(
                        codes::UNRESOLVED_IMPORT,
                        format!("the root module is imported as `{package}`, not `{text}`"),
                    )
                    .with_primary(span, "the root module has no `main` segment")
                    .with_suggestion(
                        format!("import it as `{package}`"),
                        span,
                        package.to_owned(),
                    ),
                );
                ImportTarget::Failed
            }
        };
        targets.push(target);
    }
    Ok((targets, diags))
}

/// The E0040 diagnostic of an import cycle (D-145). `cycle` lists the
/// modules in import order, each with its import of the next one (the
/// last imports the first). The primary label is on the first module's
/// import; the others are secondary.
///
/// # Errors
///
/// An [`Ice`] if the cycle is empty or names an unknown module.
pub fn cycle_diagnostic(
    cycle: &[(ModuleId, Span)],
    modules: &ModuleTable,
) -> Result<Diagnostic, Ice> {
    let paths = cycle
        .iter()
        .map(|&(module, _)| modules.try_get(module).map(|info| info.path.to_string()))
        .collect::<Result<Vec<_>, Ice>>()?;
    let (&(_, first_span), rest) = cycle
        .split_first()
        .ok_or_else(|| Ice::new("empty import cycle"))?;
    let first_path = paths.first().cloned().unwrap_or_default();
    if rest.is_empty() {
        return Ok(Diagnostic::new(
            codes::IMPORT_CYCLE,
            format!("the module `{first_path}` imports itself"),
        )
        .with_primary(first_span, "imports its own module"));
    }
    let next = paths.get(1).cloned().unwrap_or_default();
    let mut diag = Diagnostic::new(codes::IMPORT_CYCLE, "modules import each other in a cycle")
        .with_primary(first_span, format!("`{first_path}` imports `{next}`"));
    for (n, &(_, span)) in rest.iter().enumerate() {
        let from = paths.get(n.saturating_add(1)).cloned().unwrap_or_default();
        let to = paths
            .get(n.saturating_add(2))
            .cloned()
            .unwrap_or_else(|| first_path.clone());
        diag = diag.with_secondary(span, format!("`{from}` imports `{to}`"));
    }
    let mut chain = paths.join(" → ");
    chain.push_str(" → ");
    chain.push_str(&first_path);
    Ok(diag
        .with_note(format!("the cycle is {chain}"))
        .with_help("move what the modules share into a module that neither imports"))
}

/// The candidate closest to `target` by edit distance, if it is close
/// enough to be a likely typo (at most a third of the length, and at least
/// one edit allowed). Ties go to the earliest candidate.
pub fn closest_name<'a>(
    target: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let limit = (target.chars().count() / 3).max(1);
    let mut best: Option<(usize, &'a str)> = None;
    for candidate in candidates {
        if candidate == target {
            continue;
        }
        let distance = edit_distance(target, candidate);
        if distance <= limit && best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, name)| name)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len().saturating_add(1)];
    for (i, ca) in a.chars().enumerate() {
        if let Some(first) = cur.first_mut() {
            *first = i.saturating_add(1);
        }
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let delete = prev
                .get(j.saturating_add(1))
                .map_or(usize::MAX, |d| d.saturating_add(1));
            let insert = cur.get(j).map_or(usize::MAX, |d| d.saturating_add(1));
            let replace = prev.get(j).map_or(usize::MAX, |d| d.saturating_add(cost));
            if let Some(slot) = cur.get_mut(j.saturating_add(1)) {
                *slot = delete.min(insert).min(replace);
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev.last().copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closest_names() {
        assert_eq!(closest_name("lst", ["list", "io", "map"]), Some("list"));
        assert_eq!(closest_name("io", ["list", "map"]), None);
        assert_eq!(closest_name("strng", ["string", "set"]), Some("string"));
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
    }
}
