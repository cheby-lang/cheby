//! The help of E0042 for `m::x` where `m` has no visible `x`: a close
//! name in `m`, or, when `m` is a std module, the std item that another
//! language's name means, or else the std items called `x` in other std
//! modules, imported or not (D-1658, D-1662).
//!
//! The names come from the toolchain's catalog of std
//! ([`crate::StdCatalog::items`]), as the help of "`list` is not imported"
//! does from [`crate::StdCatalog::known`]: names only, never a body, so resolution stays
//! local (D-246). The catalog is called only when this help is needed.
//!
//! [`MissingItem::help`] is the one lookup: the resolver writes the help
//! from it, and the language server's fix for the same diagnostic reads it
//! again, so the fix offers exactly what the help names (D-1694).

use cheby_iface::{
    DefIndex, Interner, ModuleId, ModuleInterface, Name, Namespace, PackageId, builtin,
};
use cheby_syntax::SyntaxTree;
use cheby_syntax::ast::{
    AstNode, AstToken, Item, SourceFile, TypeBody, Variant, Visibility, VisibilityKind,
};

/// A public item of a shipped std module, for [`crate::StdCatalog::items`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StdItemName {
    /// The module's path under `std`, such as `io` or `json::decode`.
    pub module: Box<str>,
    /// The item's name.
    pub name: Box<str>,
    /// The namespace it is named in.
    pub namespace: Namespace,
}

/// The list of [`crate::StdCatalog::items`]: std's public items, in module
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
    // The names of Rust, Python, JavaScript and Go for what the tier-1
    // modules have (D-1937).
    (("list", "for_each"), ("list", "each")),
    (("list", "foreach"), ("list", "each")),
    (("list", "reduce"), ("list", "fold")),
    (("list", "fold_left"), ("list", "fold")),
    (("list", "foldl"), ("list", "fold")),
    (("list", "foldr"), ("list", "fold_right")),
    (("list", "concat"), ("list", "append")),
    (("list", "extend"), ("list", "append")),
    (("list", "includes"), ("list", "contains")),
    (("list", "at"), ("list", "get")),
    (("list", "nth"), ("list", "get")),
    (("list", "index"), ("list", "position")),
    (("list", "head"), ("list", "first")),
    (("list", "push_back"), ("list", "push")),
    (("list", "unshift"), ("list", "prepend")),
    (("list", "push_front"), ("list", "prepend")),
    (("list", "sorted"), ("list", "sort")),
    (("list", "sort_by"), ("list", "sort_with")),
    (("list", "reversed"), ("list", "reverse")),
    (("list", "rev"), ("list", "reverse")),
    (("list", "every"), ("list", "all")),
    (("list", "some"), ("list", "any")),
    (("list", "dedup"), ("list", "unique")),
    (("list", "distinct"), ("list", "unique")),
    (("list", "uniq"), ("list", "unique")),
    (("list", "concat_map"), ("list", "flat_map")),
    (("list", "max_by"), ("list", "max_with")),
    (("list", "min_by"), ("list", "min_with")),
    (("list", "join"), ("string", "join")),
    (("string", "upper"), ("string", "uppercase")),
    (("string", "to_upper"), ("string", "uppercase")),
    (("string", "to_uppercase"), ("string", "uppercase")),
    (("string", "to_upper_case"), ("string", "uppercase")),
    (("string", "lower"), ("string", "lowercase")),
    (("string", "to_lower"), ("string", "lowercase")),
    (("string", "to_lowercase"), ("string", "lowercase")),
    (("string", "to_lower_case"), ("string", "lowercase")),
    (("string", "strip"), ("string", "trim")),
    (("string", "lstrip"), ("string", "trim_start")),
    (("string", "rstrip"), ("string", "trim_end")),
    (("string", "trim_left"), ("string", "trim_start")),
    (("string", "trim_right"), ("string", "trim_end")),
    (("string", "startswith"), ("string", "starts_with")),
    (("string", "has_prefix"), ("string", "starts_with")),
    (("string", "endswith"), ("string", "ends_with")),
    (("string", "has_suffix"), ("string", "ends_with")),
    (("string", "includes"), ("string", "contains")),
    (("string", "concat"), ("string", "add")),
    (("string", "substr"), ("string", "slice")),
    (("string", "trim_prefix"), ("string", "remove_prefix")),
    (("string", "strip_prefix"), ("string", "remove_prefix")),
    (("string", "removeprefix"), ("string", "remove_prefix")),
    (("string", "trim_suffix"), ("string", "remove_suffix")),
    (("string", "strip_suffix"), ("string", "remove_suffix")),
    (("string", "removesuffix"), ("string", "remove_suffix")),
    (("string", "splitlines"), ("string", "lines")),
    (("string", "split_lines"), ("string", "lines")),
    (("string", "chars"), ("string", "to_code_points")),
    (("string", "pad_left"), ("string", "pad_start")),
    (("string", "pad_right"), ("string", "pad_end")),
    (("string", "rjust"), ("string", "pad_start")),
    (("string", "ljust"), ("string", "pad_end")),
    (("string", "replace_all"), ("string", "replace")),
    (("string", "size"), ("string", "length")),
    (("string", "to_float"), ("float", "parse")),
    (("string", "parse_float"), ("float", "parse")),
    (("string", "from_int"), ("int", "show")),
    (("int", "to_string"), ("int", "show")),
    (("int", "to_str"), ("int", "show")),
    (("int", "itoa"), ("int", "show")),
    (("int", "atoi"), ("int", "parse")),
    (("int", "from_str"), ("int", "parse")),
    (("int", "parse_int"), ("int", "parse")),
    (("int", "to_float"), ("float", "from_int")),
    (("int", "power"), ("int", "pow")),
    (("int", "remainder"), ("int", "rem")),
    (("float", "to_string"), ("float", "show")),
    (("float", "to_str"), ("float", "show")),
    (("float", "from_str"), ("float", "parse")),
    (("float", "parse_float"), ("float", "parse")),
    (("float", "power"), ("float", "pow")),
    (("float", "powf"), ("float", "pow")),
    (("float", "log"), ("float", "ln")),
    (("option", "and_then"), ("option", "then")),
    (("option", "flat_map"), ("option", "then")),
    (("option", "bind"), ("option", "then")),
    (("option", "unwrap_or_else"), ("option", "lazy_unwrap_or")),
    (("option", "get_or_else"), ("option", "unwrap_or")),
    (("option", "with_default"), ("option", "unwrap_or")),
    (("option", "unwrap"), ("option", "expect")),
    (("option", "ok_or"), ("option", "to_result")),
    (("result", "and_then"), ("result", "try")),
    (("result", "then"), ("result", "try")),
    (("result", "flat_map"), ("result", "try")),
    (("result", "map_err"), ("result", "map_error")),
    (("result", "is_err"), ("result", "is_error")),
    (("result", "unwrap_or_else"), ("result", "lazy_unwrap_or")),
    (("result", "with_default"), ("result", "unwrap_or")),
    (("result", "unwrap"), ("result", "expect")),
    (("result", "ok"), ("result", "to_option")),
    (("map", "len"), ("map", "size")),
    (("map", "length"), ("map", "size")),
    (("map", "contains_key"), ("map", "has_key")),
    (("map", "contains"), ("map", "has_key")),
    (("map", "has"), ("map", "has_key")),
    (("map", "set"), ("map", "insert")),
    (("map", "put"), ("map", "insert")),
    (("map", "delete"), ("map", "remove")),
    (("map", "items"), ("map", "to_list")),
    (("map", "entries"), ("map", "to_list")),
    (("map", "for_each"), ("map", "each")),
    (("map", "lookup"), ("map", "get")),
    (("set", "len"), ("set", "size")),
    (("set", "length"), ("set", "size")),
    (("set", "add"), ("set", "insert")),
    (("set", "has"), ("set", "contains")),
    (("set", "delete"), ("set", "remove")),
    (("set", "discard"), ("set", "remove")),
    (("set", "for_each"), ("set", "each")),
    (("set", "issubset"), ("set", "is_subset")),
    (("io", "puts"), ("io", "println")),
    (("io", "write_line"), ("io", "println")),
    (("io", "writeln"), ("io", "println")),
    (("io", "input"), ("io", "read_line")),
    (("io", "readline"), ("io", "read_line")),
];

/// The std item that another language's name `module::name` means, by the
/// table of D-1658, as `(module, name)` under `std`: `("list", "length")`
/// for `list::len`. The type checker reads it for the help of a method
/// call (D-1938).
#[must_use]
pub fn std_alias(module: &str, name: &str) -> Option<(&'static str, &'static str)> {
    ALIASES
        .iter()
        .find(|((m, n), _)| *m == module && *n == name)
        .map(|&(_, to)| to)
}

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

/// A lookup of E0042's help for `m::x`, where module `m` has no item `x`
/// that the naming package may see.
#[derive(Clone, Copy, Debug)]
pub struct MissingItem<'a> {
    /// The interface of `m`.
    pub target: &'a ModuleInterface,
    /// `m`'s path under `std`, such as `io` or `json::decode`, when it is a
    /// module of package std; `None` for any other module.
    pub std_module: Option<&'a str>,
    /// The builtin module's interface, which [`builtin_homes`] reads. Only
    /// read when `std_module` is set.
    pub builtins: Option<&'a ModuleInterface>,
    /// The run's names.
    pub names: &'a Interner,
    /// The package of the module that names `m::x`, for visibility.
    pub from: PackageId,
    /// Std's public items, for the help of D-1658.
    pub items: StdItems<'a>,
}

/// What E0042's help for `m::x` names ([`MissingItem::help`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemHelp {
    /// A visible item of `m` whose name is close: "did you mean".
    Close(&'static str),
    /// What std has instead, when `m` is a std module (D-1658).
    Std(StdSuggestion),
}

/// What std has for `m::x` when std module `m` has no `x` (D-1658,
/// D-1662).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StdSuggestion {
    /// An alias whose target is in `m` itself, under this name.
    Rename(&'static str),
    /// The std modules that have the item, by their paths under `std`, at
    /// most three in module order, and the item's name in them: an alias's
    /// target, or else the items called `x` in other modules and the homes
    /// of a builtin item of that name.
    Elsewhere {
        /// The modules, such as `io`.
        modules: Vec<String>,
        /// The item's name in each of them.
        name: String,
    },
}

impl StdSuggestion {
    /// The help line of [`Self::Elsewhere`]: "`std::io` has `read_file`".
    /// `None` for [`Self::Rename`], which is a "did you mean".
    #[must_use]
    pub fn elsewhere_text(&self) -> Option<String> {
        match self {
            Self::Rename(_) => None,
            Self::Elsewhere { modules, name } => Some(have(modules, name)),
        }
    }
}

impl MissingItem<'_> {
    /// The help for `name` in namespace `ns`: a close name among the
    /// visible items of `m` in that namespace, or else, for a std module,
    /// what [`std_suggestion`] finds, with the homes of [`builtin_homes`].
    /// `None` when there is neither.
    #[must_use]
    pub fn help(&self, ns: Namespace, name: Name) -> Option<ItemHelp> {
        let text = self.names.get(name);
        let mut candidates: Vec<&'static str> = self
            .target
            .items
            .iter_enumerated()
            .filter(|(i, item)| {
                item.kind.namespace() == Some(ns) && self.target.visible_from(*i, self.from)
            })
            .map(|(_, item)| self.names.get(item.name))
            .collect();
        candidates.sort_unstable();
        if let Some(close) = crate::closest_name(text, candidates) {
            return Some(ItemHelp::Close(close));
        }
        let module = self.std_module?;
        let homes = self
            .builtins
            .map_or_else(Vec::new, |builtins| builtin_homes(builtins, ns, name));
        std_suggestion(self.items.list(), &homes, module, text, ns).map(ItemHelp::Std)
    }
}

/// The std modules, by their paths under `std`, where a builtin item
/// called `name` in namespace `ns` is homed, as `Map` is in `std::map`
/// (D-447), in [`builtin::HOMES`] order. `builtins` is the builtin
/// module's interface.
#[must_use]
pub fn builtin_homes(builtins: &ModuleInterface, ns: Namespace, name: Name) -> Vec<&'static str> {
    let is_it = |index: DefIndex| {
        builtins
            .item(index)
            .is_some_and(|item| item.name == name && item.kind.namespace() == Some(ns))
    };
    builtin::HOMES
        .iter()
        .filter(|(def, _)| def.module == ModuleId::BUILTIN && is_it(def.index))
        .map(|&(_, home)| home)
        .collect()
}

/// What std has for `module::name` in namespace `ns`, where `module` is
/// the path under `std` of a std module that has no such item: the target
/// of an alias, which is a deliberate mapping, or else the other std
/// modules with an item of that name among `items`, std's public items,
/// up to three in module order (D-1662). `homes` are the std modules
/// where a builtin item of that name is homed ([`builtin_homes`]).
/// `None` when there is neither.
#[must_use]
pub fn std_suggestion(
    items: &[StdItemName],
    homes: &[&str],
    module: &str,
    name: &str,
    ns: Namespace,
) -> Option<StdSuggestion> {
    let alias = ALIASES
        .iter()
        .find(|((m, n), _)| *m == module && *n == name && ns == Namespace::Value);
    if let Some(&(_, (to_module, to_name))) = alias {
        if to_module == module {
            return Some(StdSuggestion::Rename(to_name));
        }
        return Some(StdSuggestion::Elsewhere {
            modules: vec![to_module.to_owned()],
            name: to_name.to_owned(),
        });
    }
    let mut modules: Vec<&str> = items
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
    Some(StdSuggestion::Elsewhere {
        modules: modules.into_iter().map(str::to_owned).collect(),
        name: name.to_owned(),
    })
}

/// "`std::a` has `x`", "`std::a` and `std::b` have `x`", or with three
/// modules "`std::a`, `std::b` and `std::c` have `x`".
fn have(modules: &[String], name: &str) -> String {
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
    use crate::StdCatalog;

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

    /// Every `.stderr` file under `dir`, sorted.
    fn goldens(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                goldens(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "stderr") {
                out.push(path);
            }
        }
    }

    /// The paths `a::b`, `a::b::c` written in the code `code`, each as its
    /// segments: the runs of name characters and `::` that hold a `::` with a
    /// name on each side.
    fn paths_in(code: &str) -> Vec<Vec<&str>> {
        code.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
            .filter(|run| run.contains("::"))
            .map(|run| run.split("::").collect::<Vec<&str>>())
            .filter(|segments| segments.iter().all(|s| !s.is_empty()))
            .collect()
    }

    /// Items that a UI golden's help line names on purpose although std
    /// lacks them, as `(golden, item, reason)`. Each must still occur.
    const NAMED_ON_PURPOSE: &[(&str, &str, &str)] = &[(
        "tests/ui/07-modules-and-packages/module_as_value.stderr",
        "io::name",
        "the help of a module used as a value writes `name` for any item of the module",
    )];

    /// Every std item and module that a help line of a UI golden names in
    /// backticks, as in "call `list::get(xs, i)`" or "add `import std::map`",
    /// exists in std: a module's item in the value or the type namespace,
    /// a builtin item homed there (D-447), or a module `std::…`. A qualifier
    /// that is no std module, such as a test's own module or an import
    /// alias, is not std's. So a help that names std cannot outlive a
    /// rename in std unseen, as the alias table cannot (D-1658, D-1930).
    #[test]
    fn std_items_named_in_ui_helps_exist() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut sources = Vec::new();
        std_sources(&root.join("std"), "", &mut sources);
        let modules: std::collections::BTreeSet<String> =
            sources.iter().map(|(path, _)| path.clone()).collect();
        let mut items: std::collections::BTreeSet<(String, String)> = sources
            .iter()
            .flat_map(|(path, text)| {
                names_of(text)
                    .into_iter()
                    .map(|(name, _)| (path.clone(), name))
            })
            .collect();
        let globals = cheby_iface::Globals::new();
        let builtins = builtin::interface(&globals);
        for (index, item) in builtins.items.iter_enumerated() {
            let owner = item.parent.unwrap_or(index);
            let owner = cheby_iface::DefId::new(ModuleId::BUILTIN, owner);
            if let Some(&(_, home)) = builtin::HOMES.iter().find(|(def, _)| *def == owner) {
                items.insert((home.to_owned(), globals.names.get(item.name).to_owned()));
            }
        }
        let mut files = Vec::new();
        goldens(&root.join("tests/ui"), &mut files);
        let mut used = vec![false; NAMED_ON_PURPOSE.len()];
        let mut missing = Vec::new();
        for file in &files {
            let shown = file
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(file).unwrap();
            let helps = text
                .lines()
                .filter_map(|line| line.trim_start().strip_prefix("= help: "));
            for help in helps {
                // The odd pieces between backticks are code.
                for code in help.split('`').skip(1).step_by(2) {
                    for segments in paths_in(code) {
                        let named = segments.join("::");
                        let exists = if let Some((&"std", rest)) = segments.split_first() {
                            modules.contains(&rest.join("::"))
                                || rest.split_last().is_some_and(|(last, module)| {
                                    items.contains(&(module.join("::"), (*last).to_owned()))
                                })
                        } else {
                            let Some((last, module)) = segments.split_last() else {
                                continue;
                            };
                            let module = module.join("::");
                            if !modules.contains(&module) {
                                continue;
                            }
                            items.contains(&(module, (*last).to_owned()))
                        };
                        if exists {
                            continue;
                        }
                        let excused = NAMED_ON_PURPOSE
                            .iter()
                            .position(|&(golden, item, _)| golden == shown && item == named);
                        match excused {
                            Some(index) => {
                                if let Some(flag) = used.get_mut(index) {
                                    *flag = true;
                                }
                            }
                            None => missing.push(format!("{shown}: `{named}`")),
                        }
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "help lines name std items that std does not have:\n{}",
            missing.join("\n")
        );
        let stale: Vec<String> = NAMED_ON_PURPOSE
            .iter()
            .zip(&used)
            .filter(|&(_, &used)| !used)
            .map(|(&(golden, item, _), _)| format!("{golden}: `{item}`"))
            .collect();
        assert!(
            stale.is_empty(),
            "exceptions that no golden names any more:\n{}",
            stale.join("\n")
        );
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
        std_suggestion(std.items.list(), homes, module, name, ns).map(|help| match help {
            StdSuggestion::Rename(to) => format!("rename to {to}"),
            elsewhere @ StdSuggestion::Elsewhere { .. } => elsewhere.elsewhere_text().unwrap(),
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
        assert!(std_suggestion(bare.items.list(), &[], "string", "read_file", value).is_none());
        assert_eq!(
            std_suggestion(bare.items.list(), &[], "int", "from_string", value),
            Some(StdSuggestion::Rename("parse"))
        );
    }

    /// A case of the test below: the module and the name asked, the
    /// namespace, the homes, then the modules and the name found, and the
    /// help's text.
    type Case<'a> = (
        &'a str,
        &'a str,
        Namespace,
        &'a [&'static str],
        &'a [&'a str],
        &'a str,
        &'a str,
    );

    /// The structured form names what the help's text names: the same
    /// modules, in the same order, and the item's name in them, so a fix
    /// read from it offers what the help says (D-1694, W11-Q3).
    #[test]
    fn the_structured_form_names_what_the_help_names() {
        let items = fake_items();
        let value = Namespace::Value;
        let cases: [Case<'_>; 5] = [
            (
                "string",
                "read_file",
                value,
                &[],
                &["io"],
                "read_file",
                "`std::io` has `read_file`",
            ),
            (
                "io",
                "length",
                value,
                &[],
                &["bytes", "list", "map"],
                "length",
                "`std::bytes`, `std::list` and `std::map` have `length`",
            ),
            (
                "file",
                "read_text",
                value,
                &[],
                &["io"],
                "read_file",
                "`std::io` has `read_file`",
            ),
            (
                "list",
                "Map",
                Namespace::Type,
                &["map"],
                &["map"],
                "Map",
                "`std::map` has `Map`",
            ),
            (
                "io",
                "size",
                value,
                &[],
                &["file", "heap", "map"],
                "size",
                "`std::file`, `std::heap` and `std::map` have `size`",
            ),
        ];
        for (module, name, ns, homes, modules, to, text) in cases {
            let found = std_suggestion(items, homes, module, name, ns).unwrap();
            assert_eq!(
                found,
                StdSuggestion::Elsewhere {
                    modules: modules.iter().map(|m| (*m).to_owned()).collect(),
                    name: to.to_owned(),
                },
                "{module}::{name}"
            );
            assert_eq!(
                found.elsewhere_text().as_deref(),
                Some(text),
                "{module}::{name}"
            );
        }
        assert_eq!(StdSuggestion::Rename("length").elsewhere_text(), None);
    }

    /// A close name in the module comes before what std has elsewhere, as
    /// in the help, and only visible items of the namespace count.
    #[test]
    fn a_close_name_comes_first() {
        let names = Interner::new();
        let target = ModuleInterface::empty(ModuleId::BUILTIN, PackageId::STD);
        let lookup = MissingItem {
            target: &target,
            std_module: Some("string"),
            builtins: None,
            names: &names,
            from: PackageId::LOCAL,
            items: StdItems::Shared(fake_items),
        };
        let read_file = names.intern("read_file");
        assert_eq!(
            lookup.help(Namespace::Value, read_file),
            Some(ItemHelp::Std(StdSuggestion::Elsewhere {
                modules: vec!["io".to_owned()],
                name: "read_file".to_owned(),
            }))
        );
        // Not a std module: nothing but a close name.
        let local = MissingItem {
            std_module: None,
            ..lookup
        };
        assert_eq!(local.help(Namespace::Value, read_file), None);
    }
}
