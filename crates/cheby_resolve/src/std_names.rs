//! The help of E0042 for `m::x` where `m` is a std module without `x`
//! (D-1658): the std item that another language's name means, or else
//! the std items called `x` in other std modules, imported or not
//! (D-1662).
//!
//! The names come from the toolchain's catalog of std ([`StdCatalog::items`]),
//! as the help of "`list` is not imported" does from
//! [`StdCatalog::known`]: names only, never a body, so resolution stays
//! local (D-246). The catalog is called only when this help is needed.

use cheby_iface::Namespace;
use cheby_syntax::SyntaxTree;
use cheby_syntax::ast::{
    AstNode, AstToken, Item, SourceFile, TypeBody, Variant, Visibility, VisibilityKind,
};

use crate::StdCatalog;

/// A public item of a shipped std module, for [`StdCatalog::items`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StdItemName {
    /// The module's path under `std`, such as `io` or `json::decode`.
    pub module: Box<str>,
    /// The item's name.
    pub name: Box<str>,
    /// The namespace it is named in.
    pub namespace: Namespace,
}

/// The list of [`StdCatalog::items`]: std's public items, in module
/// order.
#[derive(Clone, Copy, Debug, Default)]
pub enum StdItems<'a> {
    /// No list: the help names no other module.
    #[default]
    None,
    /// The binary's std, built once per process on the first call, which
    /// may parse all of std, so it is called only when the help is needed.
    Shared(fn() -> &'static [StdItemName]),
    /// A list the run owns, such as a std checkout's, built from its own
    /// texts (D-1700).
    Owned(&'a [StdItemName]),
}

impl<'a> StdItems<'a> {
    /// The items, built first if this is the binary's list and no run of
    /// the process has needed it yet.
    #[must_use]
    pub fn list(self) -> &'a [StdItemName] {
        match self {
            Self::None => &[],
            Self::Shared(items) => items(),
            Self::Owned(items) => items,
        }
    }
}

/// A std item, as `(module, name)` with the module's path under `std`.
type StdPath = (&'static str, &'static str);

/// The names that other languages give a std item, as `(module, name)`
/// written under `std`, and the std item they mean (D-1658). A test checks
/// that every target exists in std.
const ALIASES: &[(StdPath, StdPath)] = &[
    (("file", "read_text"), ("io", "read_file")),
    (("file", "read_to_string"), ("io", "read_file")),
    (("file", "read_all"), ("io", "read_file")),
    (("file", "write_text"), ("io", "write_file")),
    (("io", "read_text"), ("io", "read_file")),
    (("io", "read_to_string"), ("io", "read_file")),
    (("io", "read_all"), ("io", "read_file")),
    (("io", "write_text"), ("io", "write_file")),
    (("string", "substring"), ("string", "slice")),
    (("string", "len"), ("string", "length")),
    (("string", "to_int"), ("int", "parse")),
    (("string", "parse_int"), ("int", "parse")),
    (("int", "from_string"), ("int", "parse")),
    (("list", "len"), ("list", "length")),
    (("list", "size"), ("list", "length")),
    (("list", "find_index"), ("list", "position")),
    (("list", "index_of"), ("list", "position")),
    (("map", "get_or_default"), ("map", "get_or")),
    (("map", "get_with_default"), ("map", "get_or")),
];

/// How many modules the help names at most.
const MAX_MODULES: usize = 3;

/// The public items of a module's tree, in declaration order: functions,
/// constants and the constructors of `exposed` types in the value
/// namespace, types, aliases and interfaces in the type namespace.
#[must_use]
pub fn public_names(tree: &SyntaxTree) -> Vec<(String, Namespace)> {
    let Some(file) = SourceFile::cast(tree.root()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in file.items() {
        if item.visibility().and_then(Visibility::kind) != Some(VisibilityKind::Pub) {
            continue;
        }
        let Some(name) = item.name() else { continue };
        let name = name.text().to_owned();
        match item {
            Item::Fn(_) | Item::Const(_) => out.push((name, Namespace::Value)),
            Item::Alias(_) | Item::Interface(_) => out.push((name, Namespace::Type)),
            Item::Type(decl) => {
                out.push((name.clone(), Namespace::Type));
                if decl.is_exposed() {
                    match decl.body() {
                        Some(TypeBody::Variants(list)) => out.extend(
                            list.variants()
                                .filter_map(Variant::name)
                                .map(|v| (v.text().to_owned(), Namespace::Value)),
                        ),
                        Some(TypeBody::Record(_)) => out.push((name, Namespace::Value)),
                        None => {}
                    }
                }
            }
            Item::Test(_) => {}
        }
    }
    out
}

/// The help [`std_help`] gives.
pub(crate) enum StdHelp {
    /// The item is in the same module under this name.
    Rename(&'static str),
    /// A help line naming the modules that have the item.
    Elsewhere(String),
}

/// The help for `module::name` in namespace `ns`, where `module` is the
/// path under `std` of a std module that has no such item: the target of
/// an alias, which is a deliberate mapping, or else the other std modules
/// with an item of that name, up to three in module order (D-1662).
/// `homes` are the std modules where a builtin item of that name is
/// homed, as `Map` is in `std::map` (D-447). `None` when there is
/// neither.
pub(crate) fn std_help(
    std: StdCatalog<'_>,
    homes: &[&'static str],
    module: &str,
    name: &str,
    ns: Namespace,
) -> Option<StdHelp> {
    let alias = ALIASES
        .iter()
        .find(|((m, n), _)| *m == module && *n == name && ns == Namespace::Value);
    if let Some(&(_, (to_module, to_name))) = alias {
        if to_module == module {
            return Some(StdHelp::Rename(to_name));
        }
        return Some(StdHelp::Elsewhere(have(&[to_module], to_name)));
    }
    let mut modules: Vec<&str> = std
        .items
        .list()
        .iter()
        .filter(|item| item.namespace == ns && &*item.name == name)
        .map(|item| &*item.module)
        .chain(homes.iter().copied())
        .filter(|&m| m != module)
        .collect();
    modules.sort_unstable();
    modules.dedup();
    if modules.is_empty() {
        return None;
    }
    modules.truncate(MAX_MODULES);
    Some(StdHelp::Elsewhere(have(&modules, name)))
}

/// "`std::a` has `x`", "`std::a` and `std::b` have `x`", or with three
/// modules "`std::a`, `std::b` and `std::c` have `x`".
fn have(modules: &[&str], name: &str) -> String {
    let named: Vec<String> = modules.iter().map(|m| format!("`std::{m}`")).collect();
    match named.as_slice() {
        [one] => format!("{one} has `{name}`"),
        [rest @ .., last] => format!("{} and {last} have `{name}`", rest.join(", ")),
        [] => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use cheby_diag::FileId;

    use super::*;

    fn names_of(text: &str) -> Vec<(String, Namespace)> {
        let parse = cheby_syntax::parse(FileId::new(0), Arc::from(text));
        public_names(parse.tree())
    }

    /// Every `.cheby` file under `dir`, as `(module path under std,
    /// text)`.
    fn std_sources(dir: &Path, prefix: &str, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                std_sources(&path, &format!("{prefix}{name}::"), out);
            } else if let Some(stem) = name.strip_suffix(".cheby") {
                out.push((
                    format!("{prefix}{stem}"),
                    std::fs::read_to_string(&path).unwrap(),
                ));
            }
        }
    }

    #[test]
    fn public_names_lists_pub_items_in_order() {
        let text = "pub fn a() -> Int {\n  1\n}\n\nfn b() -> Int {\n  2\n}\n\n\
                    priv fn c() -> Int {\n  3\n}\n\npub const d: Int = 4\n\n\
                    pub exposed type E {\n  F,\n  G(Int),\n}\n\npub type H {\n  H(Int),\n}\n\n\
                    pub exposed type R {\n  x: Int,\n}\n\npub type A = Int\n\n\
                    pub interface I {\n  fn i(Self) -> Int\n}\n";
        let value = Namespace::Value;
        let ty = Namespace::Type;
        let expected = [
            ("a", value),
            ("d", value),
            ("E", ty),
            ("F", value),
            ("G", value),
            ("H", ty),
            ("R", ty),
            ("R", value),
            ("A", ty),
            ("I", ty),
        ];
        let expected: Vec<(String, Namespace)> =
            expected.iter().map(|&(n, ns)| (n.to_owned(), ns)).collect();
        assert_eq!(names_of(text), expected);
    }

    /// Every alias names an item that std's modules do not have, and
    /// means one that they do (D-1658).
    #[test]
    fn alias_targets_exist_in_std() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std");
        let mut sources = Vec::new();
        std_sources(&dir, "", &mut sources);
        let has = |module: &str, name: &str| {
            sources.iter().any(|(path, text)| {
                path == module
                    && names_of(text)
                        .iter()
                        .any(|(n, ns)| n == name && *ns == Namespace::Value)
            })
        };
        for &((module, name), (to_module, to_name)) in ALIASES {
            assert!(
                sources.iter().any(|(path, _)| path == module),
                "`std::{module}` is not a std module"
            );
            assert!(!has(module, name), "`std::{module}` has `{name}`");
            assert!(
                has(to_module, to_name),
                "{module}::{name}: `std::{to_module}` has no `{to_name}`"
            );
        }
    }

    fn fake_items() -> &'static [StdItemName] {
        static ITEMS: std::sync::OnceLock<Vec<StdItemName>> = std::sync::OnceLock::new();
        ITEMS.get_or_init(|| {
            [
                ("bytes", "index_of"),
                ("bytes", "length"),
                ("file", "size"),
                ("heap", "size"),
                ("io", "read_file"),
                ("list", "length"),
                ("list", "position"),
                ("map", "length"),
                ("map", "size"),
                ("string", "index_of"),
                ("string", "length"),
            ]
            .iter()
            .map(|&(module, name)| StdItemName {
                module: module.into(),
                name: name.into(),
                namespace: Namespace::Value,
            })
            .collect()
        })
    }

    fn help(module: &str, name: &str, ns: Namespace, homes: &[&'static str]) -> Option<String> {
        let std = StdCatalog {
            known: &[],
            items: StdItems::Shared(fake_items),
        };
        std_help(std, homes, module, name, ns).map(|help| match help {
            StdHelp::Rename(to) => format!("rename to {to}"),
            StdHelp::Elsewhere(text) => text,
        })
    }

    #[test]
    fn std_help_names_aliases_then_other_modules() {
        let value = Namespace::Value;
        assert_eq!(
            help("string", "read_file", value, &[]).as_deref(),
            Some("`std::io` has `read_file`")
        );
        // Up to three, in module order, never the module itself.
        assert_eq!(
            help("io", "length", value, &[]).as_deref(),
            Some("`std::bytes`, `std::list` and `std::map` have `length`")
        );
        assert_eq!(
            help("bytes", "length", value, &[]).as_deref(),
            Some("`std::list`, `std::map` and `std::string` have `length`")
        );
        // The type namespace finds no value, but a homed builtin type.
        assert_eq!(help("io", "read_file", Namespace::Type, &[]), None);
        assert_eq!(
            help("list", "Map", Namespace::Type, &["map"]).as_deref(),
            Some("`std::map` has `Map`")
        );
        // An alias gives the help.
        assert_eq!(
            help("file", "read_text", value, &[]).as_deref(),
            Some("`std::io` has `read_file`")
        );
        assert_eq!(
            help("list", "len", value, &[]).as_deref(),
            Some("rename to length")
        );
        // An alias comes before the same name in other modules (D-1662).
        assert_eq!(
            help("list", "size", value, &[]).as_deref(),
            Some("rename to length")
        );
        assert_eq!(
            help("list", "index_of", value, &[]).as_deref(),
            Some("rename to position")
        );
        assert_eq!(
            help("io", "size", value, &[]).as_deref(),
            Some("`std::file`, `std::heap` and `std::map` have `size`")
        );
        assert_eq!(help("list", "nothing_like_it", value, &[]), None);
        // Without the catalog only the aliases are left.
        let bare = StdCatalog::default();
        assert!(std_help(bare, &[], "string", "read_file", value).is_none());
        assert_eq!(
            std_help(bare, &[], "int", "from_string", value).map(|help| match help {
                StdHelp::Rename(to) => to,
                StdHelp::Elsewhere(_) => "elsewhere",
            }),
            Some("parse")
        );
    }
}
