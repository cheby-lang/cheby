//! What a REPL entry sees of its session (§13.5, D-098, D-193, D-547).
//!
//! Each entry is a module of its own (`<repl:N>`, plan §8.3). The session's
//! names are the items of earlier entries and the names their `import`s
//! bound, newest first. An entry sees them through synthetic item imports
//! that [`SessionScope::resolve_entry`] appends to the entry's own imports,
//! so name resolution itself is unchanged and reads only interfaces
//! (D-246):
//!
//! - one import per earlier entry module, listing the names whose newest
//!   definition is there;
//! - each earlier `import` again, narrowed to the names it still binds.
//!
//! The entry's own imports come first, and names the entry declares or
//! imports itself are left out, so a redefinition never collides with the
//! name it replaces. Synthetic imports point into a session file of their
//! own (`SessionScope::synthetic`): diagnostics whose primary span is not in
//! the entry's file are dropped, and so is E0044 (no shadowing) when the
//! name it shadows comes from the session, because a prompt-level `let` may
//! rebind a session name and the no-shadowing rule does not reach across
//! entries (D-193, §13.5.2).

use cheby_diag::{Diagnostic, FileId, LabelStyle, Span, codes};
use cheby_iface::{DefId, Ice, Interner, ModuleId, ModuleTable, Name, Namespace};
use cheby_syntax::ast::{AstId, ImportDecl};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{ImportItemRequest, ImportRequest, ImportTarget, ResolveInput, ResolvedModule};

/// What a session name refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionBinding {
    /// An item of an earlier entry module: a declaration, or a session
    /// value made by a prompt-level `let`.
    Item(DefId),
    /// A name bound by the earlier `import` with this number
    /// ([`SessionScope::import_request`]).
    Import(usize),
}

/// A session name with the entry that bound it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Bound {
    binding: SessionBinding,
    /// The number of the entry that bound it: newer entries win conflicts
    /// between synthetic imports.
    stamp: u32,
}

/// An `import` an earlier entry wrote, with its target.
#[derive(Clone, Debug)]
struct UserImport {
    request: ImportRequest,
    target: ImportTarget,
}

/// The names of a REPL session, per namespace, that later entries see.
#[derive(Clone, Debug)]
pub struct SessionScope {
    names: FxHashMap<(Namespace, Name), Bound>,
    imports: Vec<UserImport>,
    /// The span every synthetic import points at: an empty file of the
    /// session, so diagnostics about synthetic imports are recognized and
    /// dropped.
    synthetic: Span,
    /// The id of some parsed `import`, for the `ast` field of synthetic
    /// requests. Resolution never reads it.
    ast: AstId<ImportDecl<'static>>,
}

impl SessionScope {
    /// An empty session. `synthetic` is a span in a file of the session's
    /// own that no entry uses; `ast` is the id of any parsed `import`.
    #[must_use]
    pub fn new(synthetic: Span, ast: AstId<ImportDecl<'static>>) -> Self {
        Self {
            names: FxHashMap::default(),
            imports: Vec::new(),
            synthetic,
            ast,
        }
    }

    /// The file of the synthetic imports.
    #[must_use]
    pub const fn synthetic_file(&self) -> FileId {
        self.synthetic.file
    }

    /// What `name` means in namespace `ns` for the next entry.
    #[must_use]
    pub fn get(&self, ns: Namespace, name: Name) -> Option<SessionBinding> {
        self.names.get(&(ns, name)).map(|b| b.binding)
    }

    /// The earlier `import` number `index`, with its target.
    #[must_use]
    pub fn import_request(&self, index: usize) -> Option<(&ImportRequest, ImportTarget)> {
        self.imports
            .get(index)
            .map(|import| (&import.request, import.target))
    }

    /// Every session name with what it means, sorted by namespace and
    /// text: for listings and tests.
    #[must_use]
    pub fn names(&self, names: &Interner) -> Vec<(Namespace, Name, SessionBinding)> {
        let mut out: Vec<(Namespace, Name, SessionBinding)> = self
            .names
            .iter()
            .map(|(&(ns, name), bound)| (ns, name, bound.binding))
            .collect();
        out.sort_by(|a, b| (a.0, names.get(a.1)).cmp(&(b.0, names.get(b.1))));
        out
    }

    /// Makes `name` in `ns` mean the item `def`, bound by entry `stamp`: a
    /// declaration of that entry, or a session value.
    pub fn define(&mut self, ns: Namespace, name: Name, def: DefId, stamp: u32) {
        self.names.insert(
            (ns, name),
            Bound {
                binding: SessionBinding::Item(def),
                stamp,
            },
        );
    }

    /// Removes `name` from `ns` if it still means the item `def`: a
    /// constructor of a type that a redefinition replaced (§13.5.2).
    pub fn forget(&mut self, ns: Namespace, name: Name, def: DefId) {
        if self.get(ns, name) == Some(SessionBinding::Item(def)) {
            self.names.remove(&(ns, name));
        }
    }

    /// Records an `import` entry `stamp` wrote: the names it binds now mean
    /// what it imports. A failed import binds nothing.
    pub fn add_import(
        &mut self,
        request: &ImportRequest,
        target: ImportTarget,
        stamp: u32,
        names: &Interner,
    ) {
        if target.module().is_none() {
            return;
        }
        let index = self.imports.len();
        self.imports.push(UserImport {
            request: request.clone(),
            target,
        });
        let bound = Bound {
            binding: SessionBinding::Import(index),
            stamp,
        };
        for (ns, name) in import_bindings(request, names) {
            self.names.insert((ns, name), bound);
        }
    }

    /// The imports an entry is resolved with: its own (`own`, with their
    /// targets) first, then the synthetic imports of every session name
    /// the entry neither declares (`declared`) nor imports itself, newest
    /// entry first.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if a session item's module is not in `modules`.
    pub fn imports_for(
        &self,
        own: &[ImportRequest],
        own_targets: &[ImportTarget],
        declared: &[Name],
        modules: &ModuleTable,
        names: &Interner,
    ) -> Result<(Vec<ImportRequest>, Vec<ImportTarget>), Ice> {
        let mut excluded: FxHashSet<Name> = declared.iter().copied().collect();
        for request in own {
            excluded.extend(import_bindings(request, names).into_iter().map(|(_, n)| n));
        }
        // Per earlier entry module, and per earlier import, the names that
        // still mean what it defines or imports.
        let mut by_module: FxHashMap<ModuleId, (u32, Vec<Name>)> = FxHashMap::default();
        let mut by_import: FxHashMap<usize, (u32, Vec<Name>)> = FxHashMap::default();
        for (&(_, name), bound) in &self.names {
            if excluded.contains(&name) {
                continue;
            }
            let (stamp, list) = match bound.binding {
                SessionBinding::Item(def) => by_module.entry(def.module).or_default(),
                SessionBinding::Import(index) => by_import.entry(index).or_default(),
            };
            *stamp = (*stamp).max(bound.stamp);
            if !list.contains(&name) {
                list.push(name);
            }
        }
        // Newest first; then by module or import number, so the order never
        // depends on hashing (D-264).
        let mut groups: Vec<(u32, u8, usize, Vec<Name>)> = Vec::new();
        for (module, (stamp, list)) in by_module {
            groups.push((stamp, 0, cheby_iface::Idx::index(module), list));
        }
        for (index, (stamp, list)) in by_import {
            groups.push((stamp, 1, index, list));
        }
        groups.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        let mut requests: Vec<ImportRequest> = own.to_vec();
        let mut targets: Vec<ImportTarget> = own_targets.to_vec();
        for (_, kind, key, mut list) in groups {
            list.sort_by_key(|&n| names.get(n));
            if kind == 0 {
                let module = ModuleId::from_u32(
                    u32::try_from(key).map_err(|_| Ice::new("module id overflow"))?,
                );
                requests.push(self.item_import(module, &list, modules, names)?);
                targets.push(ImportTarget::Module(module));
            } else if let Some(import) = self.imports.get(key)
                && let Some(request) = narrowed(&import.request, &list)
            {
                requests.push(request);
                targets.push(import.target);
            }
        }
        Ok((requests, targets))
    }

    /// `import <module>::{names}`, pointing at the synthetic span.
    fn item_import(
        &self,
        module: ModuleId,
        list: &[Name],
        modules: &ModuleTable,
        names: &Interner,
    ) -> Result<ImportRequest, Ice> {
        let path = modules
            .try_get(module)?
            .path
            .segments()
            .iter()
            .map(|segment| (names.intern(segment), self.synthetic))
            .collect();
        Ok(ImportRequest {
            ast: self.ast,
            path,
            path_span: self.synthetic,
            alias: None,
            items: Some(
                list.iter()
                    .map(|&name| ImportItemRequest {
                        name,
                        span: self.synthetic,
                        alias: None,
                    })
                    .collect(),
            ),
            items_span: Some(self.synthetic),
            span: self.synthetic,
        })
    }

    /// Resolves an entry: `input` holds the entry's tree and its own
    /// imports; the session's names are added as synthetic imports. The
    /// diagnostics are the entry's own ([`keep_entry_diagnostic`]).
    ///
    /// # Errors
    ///
    /// An [`Ice`] if an invariant is broken.
    pub fn resolve_entry(
        &self,
        input: &ResolveInput<'_>,
    ) -> Result<(ResolvedModule, Vec<Diagnostic>), Ice> {
        let declared = declared_names(input)?;
        let (imports, targets) = self.imports_for(
            input.imports,
            input.targets,
            &declared,
            input.modules,
            &input.globals.names,
        )?;
        let with_session = ResolveInput {
            imports: &imports,
            targets: &targets,
            ..*input
        };
        let (resolved, mut diags) = crate::resolve_module(&with_session)?;
        let file = input.tree.file();
        diags.retain(|d| keep_entry_diagnostic(d, file));
        Ok((resolved, diags))
    }
}

/// The names an entry declares, in every namespace: the session names it
/// replaces.
///
/// # Errors
///
/// An [`Ice`] if the tree is not a source file.
pub fn declared_names(input: &ResolveInput<'_>) -> Result<Vec<Name>, Ice> {
    let mut scratch = Vec::new();
    let collected = crate::items::collect(input, &mut scratch)?;
    Ok(collected
        .items
        .iter()
        .filter(|item| item.kind.namespace().is_some())
        .map(|item| item.name)
        .collect())
}

/// Whether a diagnostic found while checking an entry in file `file` is
/// the entry's own: its primary span is in the entry (a diagnostic about a
/// synthetic import, or an earlier entry's import, is not), and it is not
/// E0044 against a name of the session, which a prompt-level binding may
/// rebind (D-193). Unused-import warnings (W0003) are never the entry's:
/// an `import` entry is used by later entries.
#[must_use]
pub fn keep_entry_diagnostic(diag: &Diagnostic, file: FileId) -> bool {
    if diag.code == codes::UNUSED_IMPORT {
        return false;
    }
    if let Some(span) = diag.primary_span()
        && span.file != file
    {
        return false;
    }
    if diag.code == codes::SHADOWED_NAME
        && diag
            .labels
            .iter()
            .any(|l| l.style == LabelStyle::Secondary && l.span.file != file)
    {
        return false;
    }
    true
}

/// The names `request` binds, with their namespaces: the module's name,
/// or each listed item's name or alias (an `UPPER` name in both
/// namespaces, D-111).
fn import_bindings(request: &ImportRequest, names: &Interner) -> Vec<(Namespace, Name)> {
    match &request.items {
        None => request
            .module_binding()
            .map(|(name, _)| vec![(Namespace::Value, name)])
            .unwrap_or_default(),
        Some(items) => {
            let mut out = Vec::new();
            for item in items {
                let bound = item.alias.map_or(item.name, |(alias, _)| alias);
                if is_upper(names.get(item.name)) {
                    out.push((Namespace::Type, bound));
                }
                out.push((Namespace::Value, bound));
            }
            out
        }
    }
}

/// `request` keeping only the bindings in `keep`; `None` if it keeps none.
fn narrowed(request: &ImportRequest, keep: &[Name]) -> Option<ImportRequest> {
    match &request.items {
        None => {
            let (name, _) = request.module_binding()?;
            keep.contains(&name).then(|| request.clone())
        }
        Some(items) => {
            let kept: Vec<ImportItemRequest> = items
                .iter()
                .filter(|item| keep.contains(&item.alias.map_or(item.name, |(alias, _)| alias)))
                .cloned()
                .collect();
            (!kept.is_empty()).then(|| ImportRequest {
                items: Some(kept),
                ..request.clone()
            })
        }
    }
}

fn is_upper(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cheby_diag::{Diagnostic, FileId, Span, TextRange, codes};
    use cheby_iface::{
        DefId, DefIndex, Globals, ModuleId, ModuleInfo, ModuleTable, Namespace, PackageId,
    };
    use cheby_package::ModulePath;
    use cheby_syntax::ast::{AstNode, SourceFile};

    use super::*;
    use crate::collect_imports;

    fn span(file: u32) -> Span {
        Span::new(FileId::new(file), TextRange::empty(0))
    }

    /// A scope whose synthetic file is 9, with the `ast` of a parsed
    /// import.
    fn scope(globals: &Globals) -> SessionScope {
        let tree = cheby_syntax::parse(FileId::new(8), Arc::from("import std::io\n")).into_parts();
        let file = SourceFile::cast(tree.0.root()).unwrap();
        let ast = file.imports().next().unwrap().id();
        let _ = globals;
        SessionScope::new(span(9), ast)
    }

    fn table(entries: u32) -> ModuleTable {
        let mut modules = ModuleTable::new();
        for n in 1..=entries {
            modules
                .add(ModuleInfo {
                    path: ModulePath::root(&format!("<repl:{n}>")),
                    package: PackageId::LOCAL,
                    file: None,
                })
                .unwrap();
        }
        modules
    }

    fn def(module: u32, index: u32) -> DefId {
        DefId::new(ModuleId::from_u32(module), DefIndex::from_u32(index))
    }

    fn listed(request: &ImportRequest, globals: &Globals) -> Vec<&'static str> {
        request
            .items
            .iter()
            .flatten()
            .map(|i| globals.names.get(i.name))
            .collect()
    }

    #[test]
    fn newest_definitions_come_first_and_declared_names_are_left_out() {
        let globals = Globals::new();
        let names = &globals.names;
        let mut s = scope(&globals);
        let (f, g, t) = (names.intern("f"), names.intern("g"), names.intern("T"));
        s.define(Namespace::Value, f, def(1, 0), 1);
        s.define(Namespace::Value, g, def(1, 1), 1);
        s.define(Namespace::Type, t, def(2, 0), 2);
        // `f` is redefined by entry 3.
        s.define(Namespace::Value, f, def(3, 0), 3);
        let (requests, targets) = s.imports_for(&[], &[], &[], &table(3), names).unwrap();
        let modules: Vec<_> = targets.iter().map(|t| t.module().unwrap()).collect();
        assert_eq!(
            modules,
            vec![
                ModuleId::from_u32(3),
                ModuleId::from_u32(2),
                ModuleId::from_u32(1)
            ]
        );
        assert_eq!(listed(&requests[0], &globals), vec!["f"]);
        assert_eq!(listed(&requests[2], &globals), vec!["g"]);
        assert_eq!(names.get(requests[0].path[0].0), "<repl:3>");
        // An entry that declares `g` does not import it.
        let (requests, _) = s.imports_for(&[], &[], &[g], &table(3), names).unwrap();
        assert!(requests.iter().all(|r| !listed(r, &globals).contains(&"g")));
    }

    #[test]
    fn earlier_imports_are_narrowed_to_what_they_still_bind() {
        let globals = Globals::new();
        let names = &globals.names;
        let mut s = scope(&globals);
        let (tree, _) = cheby_syntax::parse(
            FileId::new(1),
            Arc::from("import std::list::{map, filter}\n"),
        )
        .into_parts();
        let requests = collect_imports(&tree, names);
        s.add_import(
            &requests[0],
            ImportTarget::Module(ModuleId::from_u32(5)),
            1,
            names,
        );
        let map = names.intern("map");
        assert_eq!(
            s.get(Namespace::Value, map),
            Some(SessionBinding::Import(0))
        );
        // Entry 2 defines `map`.
        s.define(Namespace::Value, map, def(2, 0), 2);
        let (out, targets) = s.imports_for(&[], &[], &[], &table(2), names).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(listed(&out[0], &globals), vec!["map"]);
        assert_eq!(listed(&out[1], &globals), vec!["filter"]);
        assert_eq!(targets[1], ImportTarget::Module(ModuleId::from_u32(5)));
        // A failed import binds nothing.
        s.add_import(&requests[0], ImportTarget::Failed, 3, names);
        assert_eq!(
            s.get(Namespace::Value, names.intern("filter")),
            Some(SessionBinding::Import(0))
        );
    }

    #[test]
    fn entry_diagnostics_drop_session_noise() {
        let entry = FileId::new(1);
        let own = Diagnostic::new(codes::UNRESOLVED_NAME, "x").with_primary(span(1), "");
        let elsewhere = Diagnostic::new(codes::UNRESOLVED_NAME, "x").with_primary(span(9), "");
        let rebinding = Diagnostic::new(codes::SHADOWED_NAME, "x")
            .with_primary(span(1), "")
            .with_secondary(span(9), "imported here");
        let shadowing = Diagnostic::new(codes::SHADOWED_NAME, "x")
            .with_primary(span(1), "")
            .with_secondary(span(1), "first bound here");
        let unused = Diagnostic::new(codes::UNUSED_IMPORT, "x").with_primary(span(1), "");
        assert!(keep_entry_diagnostic(&own, entry));
        assert!(!keep_entry_diagnostic(&elsewhere, entry));
        assert!(!keep_entry_diagnostic(&rebinding, entry));
        assert!(keep_entry_diagnostic(&shadowing, entry));
        assert!(!keep_entry_diagnostic(&unused, entry));
    }
}
