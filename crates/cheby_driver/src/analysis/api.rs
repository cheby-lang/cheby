//! The index of `cheby doc <path>` (§13.10, D-1947): every module a
//! package can import, with each item's signature and doc comment, as plain
//! data.
//!
//! - Std comes from [`StdIndex`], the index the language server reads: every
//!   shipped module, checked once, with what a package may name in it.
//! - The package's own modules and its dependencies' come from one live
//!   run of the package ([`Session::analyze_live`]). A module of the package
//!   shows every item, since any of its modules may use what is
//!   package-visible and its own code reads the private ones; a
//!   dependency's module shows what another package may name.
//!
//! Signatures are [`item_signature`]'s, as hover shows them, and docs come
//! from the retained trees (D-1385).

use std::path::Path;
use std::sync::Arc;

use cheby_iface::{
    DefId, DefKind, DepView, DisplayCx, Globals, IfaceItem, ModuleId, PackageId, SigOpts,
    item_signature, structure_visible,
};
use cheby_package::Targets;
use cheby_resolve::item_docs;
use cheby_syntax::SyntaxTree;
use cheby_syntax::ast::{AstNode, AstToken as _, Item, SourceFile, doc_text, module_doc_text};

use super::{
    ModuleView, Overlay, PackageKind, Session, SessionConfig, Snapshot, StdBase, StdIndex,
};
use crate::Error;

/// How many variants a type's signature shows before `…`.
const MAX_VARIANTS: usize = 64;

/// Whose module it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiOrigin {
    /// A module of the package the command runs in.
    Package,
    /// A module of one of its dependencies.
    Dependency,
    /// A module of the standard library.
    Std,
}

impl ApiOrigin {
    /// The word for it in output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Package => "package",
            Self::Dependency => "dependency",
            Self::Std => "std",
        }
    }
}

/// One item of an [`ApiModule`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiItem {
    /// Its name, such as `map` or `Some`.
    pub name: String,
    /// What it is.
    pub kind: DefKind,
    /// Its declaration without a body ([`item_signature`]). A constructor
    /// and an interface function have a second line that names the parent.
    pub signature: String,
    /// Its doc comment, as Markdown.
    pub doc: Option<String>,
    /// The message of its `@deprecated`.
    pub deprecated: Option<String>,
    /// The targets it exists on.
    pub targets: Targets,
}

/// One module with what the package may name in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiModule {
    /// Its path, such as `std::list` or `my_app::util`.
    pub path: String,
    pub origin: ApiOrigin,
    /// Its `//!` comment, as Markdown.
    pub doc: Option<String>,
    /// Its items in declaration order; for a std module, the builtin items
    /// homed there follow (D-447).
    pub items: Vec<ApiItem>,
}

/// The modules `cheby doc` looks in: the package's, then its dependencies',
/// then std's, each group sorted by path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApiIndex {
    pub modules: Vec<ApiModule>,
}

/// The index for a lookup: all of std, and with `root` the modules of the
/// package there and of its dependencies. When the package cannot be read
/// or its run fails, the index has std alone and the second value says
/// why.
///
/// # Errors
///
/// An [`Error`] when the standard library does not check, which is an
/// internal error.
pub fn api_index(root: Option<&Path>) -> Result<(ApiIndex, Option<String>), Error> {
    let globals = Arc::new(Globals::new());
    let base = StdBase::build(Arc::clone(&globals))?;
    let mut index = ApiIndex::default();
    let mut unread = None;
    if let Some(root) = root {
        let mut session = Session::new(SessionConfig {
            root: root.to_path_buf(),
            kind: PackageKind::Package,
            target: None,
            globals,
        });
        match session.analyze_live(&Overlay::default()) {
            Ok(live) => match live.snapshot {
                Some(snapshot) => index.add_package(&snapshot),
                None => {
                    unread = Some(
                        live.failed
                            .unwrap_or_else(|| "its `cheby.toml` cannot be used".to_owned()),
                    );
                }
            },
            Err(error) => unread = Some(error.message().to_owned()),
        }
    }
    index.add_std(base.index());
    Ok((index, unread))
}

impl ApiIndex {
    /// Adds every module of std's index.
    pub fn add_std(&mut self, std: &StdIndex) {
        for module in &std.modules {
            self.modules.push(ApiModule {
                path: module.path.clone(),
                origin: ApiOrigin::Std,
                doc: module.doc.clone(),
                items: std
                    .items
                    .iter()
                    .filter(|item| item.module == module.path)
                    .map(|item| ApiItem {
                        name: item.key.text().to_owned(),
                        kind: item.kind,
                        signature: item.signature.clone(),
                        doc: item.doc.clone(),
                        deprecated: item.deprecated.clone(),
                        targets: item.targets,
                    })
                    .collect(),
            });
        }
    }

    /// Adds the modules of `snapshot`'s package, then those of its
    /// dependencies, each group sorted by path. Std's modules of the run
    /// are left to [`ApiIndex::add_std`], which has all of them.
    pub fn add_package(&mut self, snapshot: &Snapshot) {
        let deps = DepView::new(snapshot.interfaces(), snapshot.all_ids());
        // A module of the package, as the one that looks at a dependency.
        let viewer = snapshot.local_modules().next().map(ModuleView::id);
        let mut found: Vec<ApiModule> = snapshot
            .all_modules()
            .filter_map(|module| read_module(snapshot, &deps, viewer, module))
            .collect();
        found.sort_by(|a, b| {
            (a.origin == ApiOrigin::Dependency, &a.path)
                .cmp(&(b.origin == ApiOrigin::Dependency, &b.path))
        });
        self.modules.extend(found);
    }
}

/// The entry of `module`, a module of the package or of a dependency.
/// `None` for a module of std, the builtin module, and one with no
/// interface.
fn read_module(
    snapshot: &Snapshot,
    deps: &DepView<'_>,
    viewer: Option<ModuleId>,
    module: ModuleView<'_>,
) -> Option<ApiModule> {
    let info = snapshot.table().get(module.id())?;
    if module.id() == ModuleId::BUILTIN || info.package == PackageId::STD {
        return None;
    }
    let own = info.package == PackageId::LOCAL;
    let path = shown_path(snapshot, module, own)?;
    let iface = module.interface()?;
    let tree = module.tree();
    let resolved = module.resolved();
    // A live run keeps the trees of the package only, so a dependency's
    // file is parsed again for its doc comments.
    let parsed = match tree {
        Some(_) => None,
        None => module
            .file()
            .and_then(|file| Some((file, snapshot.source(file)?)))
            .map(|(file, source)| cheby_syntax::parse(file, source.text_arc())),
    };
    let reparsed = parsed.as_ref().map(cheby_syntax::Parse::tree);
    // Whether the package may see the constructors and fields of a type.
    let structure = |ty: &IfaceItem| {
        own || viewer.is_some_and(|viewer| {
            structure_visible(ty, iface.module, iface.package, viewer, PackageId::LOCAL)
        })
    };
    let cx = DisplayCx {
        globals: snapshot.globals(),
        modules: snapshot.table(),
        own: Some(iface),
        deps,
        generics: None,
        imports: None,
    };
    let mut items = Vec::new();
    for (at, item) in iface.items.iter_enumerated() {
        let nameable = own || iface.visible_from(at, PackageId::LOCAL);
        if iface.is_hidden(at) || !iface.is_current(at) || item.kind == DefKind::Test || !nameable {
            continue;
        }
        // A member shows only with what it is a member of.
        let parent = item.parent.and_then(|parent| iface.item(parent));
        let shown = match (item.kind, item.parent, parent) {
            (DefKind::Ctor, _, Some(ty)) => structure(ty),
            (DefKind::InterfaceFn, Some(parent), _) => {
                own || iface.visible_from(parent, PackageId::LOCAL)
            }
            _ => true,
        };
        if !shown {
            continue;
        }
        let opts = SigOpts {
            structure: structure(item),
            max_variants: MAX_VARIANTS,
        };
        items.push(ApiItem {
            name: snapshot.globals().names.get(item.name).to_owned(),
            kind: item.kind,
            signature: item_signature(DefId::new(module.id(), at), item, &cx, opts),
            doc: tree
                .zip(resolved.and_then(|resolved| resolved.item(at)))
                .and_then(|(tree, def)| item_docs(tree, def))
                .or_else(|| {
                    docs_by_name(
                        reparsed?,
                        item.kind,
                        snapshot.globals().names.get(item.name),
                    )
                }),
            deprecated: item.deprecated.as_deref().map(str::to_owned),
            targets: item.targets,
        });
    }
    let doc = tree
        .or(reparsed)
        .and_then(|tree| SourceFile::cast(tree.root()))
        .map(|root| module_doc_text(root.module_docs()))
        .filter(|text| !text.trim().is_empty());
    Some(ApiModule {
        path,
        origin: if own {
            ApiOrigin::Package
        } else {
            ApiOrigin::Dependency
        },
        doc,
        items,
    })
}

/// The path a module of the package writes for `module` in an import: its
/// own path for a module of the package, and for a dependency's module the
/// path with the key of `[dependencies]` in place of the package's identity
/// (D-238). `None` for a module of a package that the manifest does not
/// name, which no module of the package can import.
fn shown_path(snapshot: &Snapshot, module: ModuleView<'_>, own: bool) -> Option<String> {
    let path = module.path()?;
    if own {
        return Some(path.to_string());
    }
    let (identity, rest) = path.segments().split_first()?;
    let key = snapshot.dependency_key(identity)?;
    Some(
        std::iter::once(key)
            .chain(rest.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("::"),
    )
}

/// The doc comment of the top-level item of `tree` with this kind and name,
/// for a module whose resolution was not kept. A constructor of a
/// single-variant type has its type's, and other members have none here.
fn docs_by_name(tree: &SyntaxTree, kind: DefKind, name: &str) -> Option<String> {
    let root = SourceFile::cast(tree.root())?;
    let item = root.items().find(|item| {
        let same_kind = match (item, kind) {
            (Item::Fn(_), DefKind::Fn)
            | (Item::Type(_), DefKind::Type | DefKind::Ctor)
            | (Item::Alias(_), DefKind::Alias)
            | (Item::Const(_), DefKind::Const)
            | (Item::Interface(_), DefKind::Interface) => true,
            (
                Item::Fn(_)
                | Item::Type(_)
                | Item::Alias(_)
                | Item::Const(_)
                | Item::Interface(_)
                | Item::Test(_),
                _,
            ) => false,
        };
        same_kind && item.name().is_some_and(|found| found.text() == name)
    })?;
    Some(doc_text(item.doc_comments())).filter(|text| !text.trim().is_empty())
}
