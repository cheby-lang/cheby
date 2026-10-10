//! W0003: unused imports (§13.8, D-080, D-253).
//!
//! A module import is used when a path starts with the name it binds. An
//! item import is used when one of the names it binds is used, in either
//! namespace (`import m::{Shape}` binds the type and its constructor).
//! Imports that failed (already reported) and names that collided (E0041)
//! are not reported again.
//!
//! Only written names count. Satisfaction never needs an import: a home
//! module's functions count without one (D-447, §7.6), and a type's
//! satisfying functions count wherever the type is visible (§8.2). So an
//! import that brings in a module whose `show` an interpolation calls,
//! but that nothing names, is unused like any other (D-515).

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{ModuleInterface, Name, Namespace};
use cheby_resolve::{ImportRequest, Res, ScopeSource, collect_imports};
use rustc_hash::FxHashSet;

use super::uses::PathUse;
use crate::TypesInput;

/// One import with what it binds.
struct Import {
    request: ImportRequest,
    /// What it binds, per written name: the module name of a module
    /// import (one entry), or each item of an item import. Names that
    /// failed or collided bind nothing here.
    bindings: Vec<Binding>,
}

struct Binding {
    /// The module-scope names it introduced.
    keys: Vec<(Namespace, Name)>,
    /// The written item (`x`, `Y as Z`), for an item import.
    span: Span,
    /// As written, for the message.
    text: String,
    used: bool,
}

/// The W0003 warnings of the module, in import order.
pub(super) fn unused_imports(
    input: &TypesInput<'_>,
    own: &ModuleInterface,
    paths: &[PathUse],
) -> Vec<Diagnostic> {
    let mut imports = collect(input);
    // Uses by name.
    let mut used: FxHashSet<(Namespace, Name)> = FxHashSet::default();
    for path in paths {
        used.extend(path.scope_keys(input, own));
    }
    for import in &mut imports {
        for binding in &mut import.bindings {
            binding.used = binding.keys.iter().any(|key| used.contains(key));
        }
    }
    let mut out = Vec::new();
    for import in &imports {
        report(input, import, &mut out);
    }
    out
}

fn collect(input: &TypesInput<'_>) -> Vec<Import> {
    let names = &input.globals.names;
    let scope = &input.resolved.scope;
    let entry_is = |ns: Namespace, name: Name, n: usize| {
        scope
            .get(ns, name)
            .is_some_and(|e| e.source == ScopeSource::Import(n) && e.res != Res::Err)
    };
    collect_imports(input.tree, names)
        .into_iter()
        .enumerate()
        .map(|(n, request)| {
            let bindings = match &request.items {
                None => request
                    .module_binding()
                    .filter(|&(name, _)| entry_is(Namespace::Value, name, n))
                    .map(|(name, _)| Binding {
                        keys: vec![(Namespace::Value, name)],
                        span: request.span,
                        text: request.path_text(names),
                        used: false,
                    })
                    .into_iter()
                    .collect(),
                Some(items) => items
                    .iter()
                    .filter_map(|item| {
                        let (bound, bound_span) = item.alias.unwrap_or((item.name, item.span));
                        let keys: Vec<(Namespace, Name)> = [Namespace::Type, Namespace::Value]
                            .into_iter()
                            .filter(|&ns| entry_is(ns, bound, n))
                            .map(|ns| (ns, bound))
                            .collect();
                        let text = match item.alias {
                            Some((alias, _)) => {
                                format!("{} as {}", names.get(item.name), names.get(alias))
                            }
                            None => names.get(item.name).to_owned(),
                        };
                        (!keys.is_empty()).then(|| Binding {
                            keys,
                            span: Span::new(
                                item.span.file,
                                item.span.range.cover(bound_span.range),
                            ),
                            text,
                            used: false,
                        })
                    })
                    .collect(),
            };
            Import { request, bindings }
        })
        .collect()
}

fn report(input: &TypesInput<'_>, import: &Import, out: &mut Vec<Diagnostic>) {
    let unused: Vec<&Binding> = import.bindings.iter().filter(|b| !b.used).collect();
    if unused.is_empty() {
        return;
    }
    let names = &input.globals.names;
    let request = &import.request;
    let whole = match &request.items {
        None => true,
        // Every written item is unused (failed ones bind nothing and are
        // already reported, so they do not keep the import alive).
        Some(items) => unused.len() == items.len(),
    };
    if whole {
        let text = match (&request.items, request.alias) {
            (None, Some((alias, _))) => {
                format!("{} as {}", request.path_text(names), names.get(alias))
            }
            (None, None) => request.path_text(names),
            (Some(_), _) => {
                let list: Vec<&str> = unused.iter().map(|b| b.text.as_str()).collect();
                format!("{}::{{{}}}", request.path_text(names), list.join(", "))
            }
        };
        out.push(
            Diagnostic::new(codes::UNUSED_IMPORT, format!("unused import `{text}`"))
                .with_primary(request.span, "nothing in this module uses it")
                .with_help("remove the import"),
        );
        return;
    }
    for binding in unused {
        out.push(
            Diagnostic::new(
                codes::UNUSED_IMPORT,
                format!(
                    "unused import `{}` from `{}`",
                    binding.text,
                    request.path_text(names)
                ),
            )
            .with_primary(binding.span, "nothing in this module uses it")
            .with_help("remove it from the list"),
        );
    }
}
