//! Documentation (§13.10).
//!
//! `cheby doc <path>` looks an API up and prints it as text (D-1947): a
//! module with each item's signature and the first sentence of its doc
//! comment, an item with its whole doc comment, or, for a query that is
//! neither, the items whose names match. [`lookup`] finds what a query
//! names in an [`ApiIndex`], which the driver builds from std, the package
//! and its dependencies, and [`render_text`] and [`render_json`] write it.
//!
//! The HTML generator of `cheby doc` without a path is planned for M5. See
//! `docs/plan/architecture.md`.

use std::fmt::Write as _;

use cheby_diag::{json_string, render_json_object};
use cheby_driver::analysis::{ApiIndex, ApiItem, ApiModule, ApiOrigin, DefKind, Target, std_alias};

/// An item with the module it is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit<'a> {
    pub module: &'a ApiModule,
    pub item: &'a ApiItem,
}

/// What a query names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found<'a> {
    /// A module: its doc comment and its items in short.
    Module(&'a ApiModule),
    /// The modules below a path that is no module itself, such as `std`.
    Modules(Vec<&'a ApiModule>),
    /// Items in full. `note` says how the query led to them when it did
    /// not name them, as for another language's name of a std function
    /// (D-1658).
    Items {
        note: Option<String>,
        items: Vec<Hit<'a>>,
    },
    /// Items in short, found by name: every item named exactly as the
    /// query, or, with none, every item whose name has the query in it.
    Names(Vec<Hit<'a>>),
}

/// What `query` names in `index`, by the first of these that finds
/// something:
///
/// 1. a module, by its path, or a std module without its `std::`;
/// 2. an item `module::name` of such a module, or the std item that
///    another language's name of it means;
/// 3. the modules below the path;
/// 4. the items named as the query's last segment: one is shown in full,
///    several in short;
/// 5. the items whose names contain it, ignoring case.
#[must_use]
pub fn lookup<'a>(index: &'a ApiIndex, query: &str) -> Option<Found<'a>> {
    let query = query.trim().trim_end_matches("::");
    if let Some(module) = module(index, query) {
        return Some(Found::Module(module));
    }
    let (path, name) = query.rsplit_once("::").unwrap_or(("", query));
    if let Some(module) = module(index, path) {
        let items = named(module, name);
        if !items.is_empty() {
            return Some(Found::Items { note: None, items });
        }
        if let Some(found) = alias(index, module, name) {
            return Some(found);
        }
    }
    let below: Vec<&ApiModule> = index
        .modules
        .iter()
        .filter(|module| {
            module
                .path
                .strip_prefix(query)
                .is_some_and(|rest| rest.starts_with("::"))
        })
        .collect();
    if !below.is_empty() {
        return Some(Found::Modules(below));
    }
    let all = || {
        index
            .modules
            .iter()
            .flat_map(|module| module.items.iter().map(move |item| Hit { module, item }))
    };
    let exact = without_twins(
        &all()
            .filter(|hit| hit.item.name == name)
            .collect::<Vec<_>>(),
    );
    match exact.len() {
        0 => {}
        1 => {
            return Some(Found::Items {
                note: None,
                items: exact,
            });
        }
        _ => return Some(Found::Names(exact)),
    }
    let wanted = name.to_lowercase();
    let close = without_twins(
        &all()
            .filter(|hit| hit.item.name.to_lowercase().contains(&wanted))
            .collect::<Vec<_>>(),
    );
    (!close.is_empty() && !wanted.is_empty()).then_some(Found::Names(close))
}

/// One path that [`complete`] offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// What to pass to `cheby doc`, such as `list::map`.
    pub path: String,
    /// One line about it: the first sentence of its doc comment, or for an
    /// item without one the first line of its signature.
    pub about: Option<String>,
}

/// The paths that start with `typed` and that [`lookup`] finds a module or
/// an item at, sorted, for a shell's completion (D-1958):
///
/// 1. the modules of the package and of its dependencies, by path;
/// 2. the std modules, by their path once `typed` starts with `std::`, and
///    before that without `std::`, as `list`, with `std` itself, which
///    lists them, so that no module is offered twice;
/// 3. when the text before the last `::` of `typed` names a module, its
///    items, written after that text as it was typed.
#[must_use]
pub fn complete(index: &ApiIndex, typed: &str) -> Vec<Candidate> {
    const STD: &str = "std";
    let mut found = Vec::new();
    let full_std = typed.starts_with("std::");
    if STD.starts_with(typed) {
        found.push(Candidate {
            path: STD.to_owned(),
            about: Some("The modules of the standard library.".to_owned()),
        });
    }
    for module in &index.modules {
        let short = module
            .path
            .strip_prefix("std::")
            .filter(|_| module.origin == ApiOrigin::Std);
        let path = match short {
            // A module of the package with the short path is the one a
            // lookup of it finds.
            Some(short) if !full_std => {
                if index.modules.iter().any(|other| other.path == short) {
                    continue;
                }
                short
            }
            Some(_) | None => &module.path,
        };
        if path.starts_with(typed) {
            found.push(Candidate {
                path: path.to_owned(),
                about: module.doc.as_deref().and_then(first_sentence),
            });
        }
    }
    if let Some((before, name)) = typed.rsplit_once("::")
        && let Some(module) = module(index, before)
    {
        for item in &module.items {
            if item.name.starts_with(name) {
                found.push(Candidate {
                    path: format!("{before}::{}", item.name),
                    about: item
                        .doc
                        .as_deref()
                        .and_then(first_sentence)
                        .or_else(|| item.signature.lines().next().map(str::to_owned)),
                });
            }
        }
    }
    // A module comes before an item at the same path, as a lookup finds it,
    // and a constructor named as its type is the type's twin.
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found.dedup_by(|later, first| later.path == first.path);
    found
}

/// `hits` without each constructor that has its type's name when the type
/// is a hit too, as the constructor of a single-variant type has (§4.3.3):
/// the type's signature shows it.
fn without_twins<'a>(hits: &[Hit<'a>]) -> Vec<Hit<'a>> {
    let is_twin = |hit: &Hit<'_>| {
        hit.item.kind == DefKind::Ctor
            && hits.iter().any(|other| {
                other.item.kind == DefKind::Type
                    && other.item.name == hit.item.name
                    && std::ptr::eq(other.module, hit.module)
            })
    };
    hits.iter().copied().filter(|hit| !is_twin(hit)).collect()
}

/// The module at `path`, or the std module at `std::` and `path`. A module
/// of the package or of a dependency comes first, as an import of that
/// path would find it.
fn module<'a>(index: &'a ApiIndex, path: &str) -> Option<&'a ApiModule> {
    let at = |path: &str| index.modules.iter().find(|module| module.path == path);
    at(path).or_else(|| at(&format!("std::{path}")))
}

/// The items of `module` named `name`, a twin constructor left out
/// ([`without_twins`]).
fn named<'a>(module: &'a ApiModule, name: &str) -> Vec<Hit<'a>> {
    let hits: Vec<Hit<'a>> = module
        .items
        .iter()
        .filter(|item| item.name == name)
        .map(|item| Hit { module, item })
        .collect();
    without_twins(&hits)
}

/// The std item that `name`, another language's name for something of the
/// std module `module`, means (D-1658).
fn alias<'a>(index: &'a ApiIndex, module: &'a ApiModule, name: &str) -> Option<Found<'a>> {
    if module.origin != ApiOrigin::Std {
        return None;
    }
    let short = module.path.strip_prefix("std::")?;
    let (to_module, to_name) = std_alias(short, name)?;
    let target = index
        .modules
        .iter()
        .find(|m| m.origin == ApiOrigin::Std && m.path.strip_prefix("std::") == Some(to_module))?;
    let items = named(target, to_name);
    (!items.is_empty()).then(|| Found::Items {
        note: Some(format!(
            "std has no `{short}::{name}`; Cheby's name for it is `{to_module}::{to_name}`"
        )),
        items,
    })
}

/// Whether a module's short view lists `item`: a constructor and an
/// interface function are part of their type's and their interface's
/// signature there.
fn listed(item: &ApiItem) -> bool {
    !matches!(item.kind, DefKind::Ctor | DefKind::InterfaceFn)
}

/// `found` as text for a terminal, ending with a line break.
#[must_use]
pub fn render_text(found: &Found<'_>) -> String {
    let mut out = String::new();
    match found {
        Found::Module(module) => {
            let _ = writeln!(out, "{}", module.path);
            if let Some(doc) = &module.doc {
                out.push('\n');
                indented(&mut out, doc, "  ");
            }
            for item in module.items.iter().filter(|item| listed(item)) {
                out.push('\n');
                // A builtin item homed here has the module's own doc
                // comment (D-447), which is printed above.
                if item.doc == module.doc {
                    let _ = writeln!(out, "{}", item.signature);
                } else {
                    short(&mut out, item, &item.signature);
                }
            }
        }
        Found::Modules(modules) => {
            for module in modules {
                let _ = writeln!(out, "{}", module.path);
                if let Some(sentence) = module.doc.as_deref().and_then(first_sentence) {
                    let _ = writeln!(out, "    {sentence}");
                }
            }
        }
        Found::Items { note, items } => {
            if let Some(note) = note {
                let _ = writeln!(out, "{note}\n");
            }
            for (index, hit) in items.iter().enumerate() {
                if index > 0 {
                    out.push('\n');
                }
                full(&mut out, *hit);
            }
        }
        Found::Names(hits) => {
            for (index, hit) in hits.iter().enumerate() {
                if index > 0 {
                    out.push('\n');
                }
                let _ = writeln!(out, "{}::{}", hit.module.path, hit.item.name);
                // A type's variants are one query away.
                let mut lines = hit.item.signature.lines();
                let first = lines.next().unwrap_or_default();
                let signature = if lines.next().is_some() {
                    format!("{first} …")
                } else {
                    first.to_owned()
                };
                let mut entry = String::new();
                short(&mut entry, hit.item, &signature);
                indented(&mut out, &entry, "    ");
            }
        }
    }
    out
}

/// An item in short: its signature, the first sentence of its doc comment,
/// and that it is deprecated.
fn short(out: &mut String, item: &ApiItem, signature: &str) {
    let _ = writeln!(out, "{signature}");
    if let Some(sentence) = item.doc.as_deref().and_then(first_sentence) {
        let _ = writeln!(out, "    {sentence}");
    }
    if let Some(message) = &item.deprecated {
        let _ = writeln!(out, "    deprecated: {message}");
    }
}

/// An item in full: its path, its signature, its whole doc comment, and
/// below it that it is deprecated and the targets it is limited to.
fn full(out: &mut String, hit: Hit<'_>) {
    let item = hit.item;
    let _ = writeln!(
        out,
        "{}::{}\n\n{}",
        hit.module.path, item.name, item.signature
    );
    if let Some(doc) = &item.doc {
        out.push('\n');
        indented(out, doc, "  ");
    }
    let targets = targets(item);
    if item.deprecated.is_some() || targets.len() < 2 {
        out.push('\n');
    }
    if let Some(message) = &item.deprecated {
        let _ = writeln!(out, "  deprecated: {message}");
    }
    if targets.len() < 2 {
        let _ = writeln!(out, "  targets: {}", targets.join(", "));
    }
}

/// The names of the targets `item` exists on.
fn targets(item: &ApiItem) -> Vec<&'static str> {
    [(Target::Native, "native"), (Target::Js, "js")]
        .into_iter()
        .filter(|&(target, _)| item.targets.contains(target))
        .map(|(_, name)| name)
        .collect()
}

/// Appends the lines of `text`, each after `indent`, and an empty line as
/// it is.
fn indented(out: &mut String, text: &str, indent: &str) {
    for line in text.lines() {
        if line.trim().is_empty() {
            out.push('\n');
        } else {
            let _ = writeln!(out, "{indent}{line}");
        }
    }
}

/// The first sentence of a doc comment: its first paragraph on one line, up
/// to the first `.` that ends a word outside a code span, or the whole
/// paragraph when it has none. `None` for a comment without text.
fn first_sentence(doc: &str) -> Option<String> {
    let paragraph: Vec<&str> = doc
        .lines()
        .map(str::trim)
        .skip_while(|line| line.is_empty())
        .take_while(|line| !line.is_empty())
        .collect();
    let paragraph = paragraph.join(" ");
    let mut in_code = false;
    let mut chars = paragraph.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '`' => in_code = !in_code,
            '.' if !in_code && chars.peek().is_none_or(|&(_, next)| next == ' ') => {
                return paragraph.get(..=at).map(str::to_owned);
            }
            _ => {}
        }
    }
    (!paragraph.is_empty()).then_some(paragraph)
}

/// The word for an item's kind in the JSON form.
const fn kind_name(kind: DefKind) -> &'static str {
    match kind {
        DefKind::Fn => "fn",
        DefKind::Type => "type",
        DefKind::Alias => "alias",
        DefKind::Const => "const",
        DefKind::Interface => "interface",
        DefKind::InterfaceFn => "interface_fn",
        DefKind::Ctor => "constructor",
        DefKind::Test => "test",
    }
}

/// `found` as JSON Lines, for `--format json` (D-1944, D-1947): a module is
/// `{"type":"module","path":…,"origin":…,"doc":…}` and an item
/// `{"type":"item","module":…,"name":…,"kind":…,"signature":…,"doc":…,
/// "deprecated":…,"targets":[…]}`, with `doc` and `deprecated` left out when
/// the item has none. A module is followed by the items its text form
/// lists, each with its whole doc comment.
#[must_use]
pub fn render_json(found: &Found<'_>) -> String {
    let mut out = String::new();
    let module_line = |out: &mut String, module: &ApiModule| {
        let mut fields = vec![
            ("path", json_string(&module.path)),
            ("origin", json_string(module.origin.as_str())),
        ];
        if let Some(doc) = &module.doc {
            fields.push(("doc", json_string(doc)));
        }
        out.push_str(&render_json_object("module", &fields));
    };
    let item_line = |out: &mut String, module: &ApiModule, item: &ApiItem| {
        let mut fields = vec![
            ("module", json_string(&module.path)),
            ("name", json_string(&item.name)),
            ("kind", json_string(kind_name(item.kind))),
            ("signature", json_string(&item.signature)),
        ];
        if let Some(doc) = &item.doc {
            fields.push(("doc", json_string(doc)));
        }
        if let Some(message) = &item.deprecated {
            fields.push(("deprecated", json_string(message)));
        }
        let targets: Vec<String> = targets(item).into_iter().map(json_string).collect();
        fields.push(("targets", format!("[{}]", targets.join(","))));
        out.push_str(&render_json_object("item", &fields));
    };
    match found {
        Found::Module(module) => {
            module_line(&mut out, module);
            for item in module.items.iter().filter(|item| listed(item)) {
                item_line(&mut out, module, item);
            }
        }
        Found::Modules(modules) => {
            for module in modules {
                module_line(&mut out, module);
            }
        }
        Found::Items { note, items } => {
            if let Some(note) = note {
                out.push_str(&cheby_diag::render_json_message("note", note));
            }
            for hit in items {
                item_line(&mut out, hit.module, hit.item);
            }
        }
        Found::Names(hits) => {
            for hit in hits {
                item_line(&mut out, hit.module, hit.item);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use cheby_driver::analysis::Targets;

    use super::*;

    fn item(name: &str, kind: DefKind, signature: &str, doc: &str) -> ApiItem {
        ApiItem {
            name: name.to_owned(),
            kind,
            signature: signature.to_owned(),
            doc: (!doc.is_empty()).then(|| doc.to_owned()),
            deprecated: None,
            targets: Targets::ALL,
        }
    }

    fn module(path: &str, origin: ApiOrigin, doc: &str, items: Vec<ApiItem>) -> ApiModule {
        ApiModule {
            path: path.to_owned(),
            origin,
            doc: (!doc.is_empty()).then(|| doc.to_owned()),
            items,
        }
    }

    fn index() -> ApiIndex {
        let length = item(
            "length",
            DefKind::Fn,
            "pub fn length<T>(xs: List<T>) -> Int",
            "The number of elements. It is O(1).\n\nA second paragraph.",
        );
        let mut native_only = item("fold", DefKind::Fn, "pub fn fold<T, A>(…) -> A", "");
        native_only.targets = Targets::only(Target::Native);
        native_only.deprecated = Some("use `fold_left`".to_owned());
        ApiIndex {
            modules: vec![
                module(
                    "app::list",
                    ApiOrigin::Package,
                    "",
                    vec![item("helper", DefKind::Fn, "fn helper() -> Int", "Helps.")],
                ),
                module(
                    "std::list",
                    ApiOrigin::Std,
                    "Lists.\n\nMore about lists.",
                    vec![length, native_only],
                ),
                module(
                    "std::option",
                    ApiOrigin::Std,
                    "Optional values, as `a.b` shows. And more.",
                    vec![
                        item(
                            "Option",
                            DefKind::Type,
                            "pub exposed type Option<T> {\n  Some(T),\n  None,\n}",
                            "A value or none.",
                        ),
                        item("Some", DefKind::Ctor, "Some(T)\nof type Option<T>", ""),
                        item(
                            "length",
                            DefKind::Fn,
                            "pub fn length<T>(o: Option<T>) -> Int",
                            "0 or 1.",
                        ),
                    ],
                ),
            ],
        }
    }

    fn names(found: &Found<'_>) -> Vec<String> {
        match found {
            Found::Module(module) => vec![format!("module {}", module.path)],
            Found::Modules(modules) => modules
                .iter()
                .map(|m| format!("module {}", m.path))
                .collect(),
            Found::Items { items, .. } | Found::Names(items) => items
                .iter()
                .map(|hit| format!("{}::{}", hit.module.path, hit.item.name))
                .collect(),
        }
    }

    /// The paths a shell offers for `cheby doc` (D-1958): modules, std's
    /// by the short path until `std::` is typed, and the items of the
    /// module that the text before the last `::` names.
    #[test]
    fn completion_offers_modules_and_their_items() {
        let index = index();
        let paths = |typed: &str| -> Vec<String> {
            complete(&index, typed)
                .into_iter()
                .map(|found| found.path)
                .collect()
        };
        assert_eq!(paths(""), ["app::list", "list", "option", "std"]);
        assert_eq!(paths("st"), ["std"]);
        assert_eq!(paths("std"), ["std"]);
        assert_eq!(paths("std::"), ["std::list", "std::option"]);
        assert_eq!(paths("std::o"), ["std::option"]);
        assert_eq!(paths("app"), ["app::list"]);
        assert_eq!(paths("list::"), ["list::fold", "list::length"]);
        assert_eq!(paths("std::list::l"), ["std::list::length"]);
        assert_eq!(paths("app::list::"), ["app::list::helper"]);
        // Types sort before functions, and a constructor is an item.
        assert_eq!(
            paths("option::"),
            ["option::Option", "option::Some", "option::length"]
        );
        assert_eq!(paths("nothing::"), Vec::<String>::new());
        assert_eq!(paths("list::x"), Vec::<String>::new());
        // Every path is one a lookup finds.
        for typed in ["", "std::", "list::", "option::", "app::list::"] {
            for path in paths(typed) {
                assert!(lookup(&index, &path).is_some(), "{path}");
            }
        }
    }

    /// A candidate's line is the first sentence of the doc comment, and
    /// for an item without one the first line of its signature.
    #[test]
    fn completion_says_what_a_candidate_is() {
        let index = index();
        let about = |typed: &str| -> Vec<(String, Option<String>)> {
            complete(&index, typed)
                .into_iter()
                .map(|found| (found.path, found.about))
                .collect()
        };
        assert_eq!(
            about("list"),
            [("list".to_owned(), Some("Lists.".to_owned()))]
        );
        assert_eq!(
            about("list::"),
            [
                (
                    "list::fold".to_owned(),
                    Some("pub fn fold<T, A>(…) -> A".to_owned())
                ),
                (
                    "list::length".to_owned(),
                    Some("The number of elements.".to_owned())
                ),
            ]
        );
        assert_eq!(about("app"), [("app::list".to_owned(), None)]);
    }

    /// A module of the package at a std module's short path is the one a
    /// lookup of that path finds, so it is the one offered.
    #[test]
    fn completion_offers_a_shadowing_module_once() {
        let index = ApiIndex {
            modules: vec![
                module(
                    "option",
                    ApiOrigin::Package,
                    "Command-line options.",
                    vec![item("parse", DefKind::Fn, "fn parse() -> Int", "")],
                ),
                module(
                    "std::option",
                    ApiOrigin::Std,
                    "Optional values.",
                    Vec::new(),
                ),
            ],
        };
        let found = complete(&index, "op");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, "option");
        assert_eq!(found[0].about.as_deref(), Some("Command-line options."));
        let items: Vec<String> = complete(&index, "option::")
            .into_iter()
            .map(|found| found.path)
            .collect();
        assert_eq!(items, ["option::parse"]);
        let std: Vec<String> = complete(&index, "std::")
            .into_iter()
            .map(|found| found.path)
            .collect();
        assert_eq!(std, ["std::option"]);
    }

    #[test]
    fn a_query_names_a_module_an_item_or_is_searched() {
        let index = index();
        let found = |query: &str| lookup(&index, query).map(|found| names(&found));
        assert_eq!(
            found("std::list"),
            Some(vec!["module std::list".to_owned()])
        );
        assert_eq!(found("option"), Some(vec!["module std::option".to_owned()]));
        assert_eq!(
            found("std::list::length"),
            Some(vec!["std::list::length".to_owned()])
        );
        // The package's module comes before std's of the same last name
        // only under its own path.
        assert_eq!(
            found("app::list::helper"),
            Some(vec!["app::list::helper".to_owned()])
        );
        assert_eq!(
            found("list::length"),
            Some(vec!["std::list::length".to_owned()])
        );
        // A path that is no module lists the modules below it.
        assert_eq!(
            found("std"),
            Some(vec![
                "module std::list".to_owned(),
                "module std::option".to_owned()
            ])
        );
        // A name alone: one item in full, several in short.
        assert_eq!(found("Some"), Some(vec!["std::option::Some".to_owned()]));
        assert!(matches!(lookup(&index, "Some"), Some(Found::Items { .. })));
        assert_eq!(
            found("length"),
            Some(vec![
                "std::list::length".to_owned(),
                "std::option::length".to_owned()
            ])
        );
        assert!(matches!(lookup(&index, "length"), Some(Found::Names(_))));
        // No such name: the names that contain it, ignoring case.
        assert_eq!(found("OPT"), Some(vec!["std::option::Option".to_owned()]));
        assert_eq!(
            found("list::elp"),
            Some(vec!["app::list::helper".to_owned()])
        );
        assert_eq!(found("nothing_like_it"), None);
        assert_eq!(found(""), None);
    }

    /// A constructor named as its type is left to the type, whose signature
    /// shows it.
    #[test]
    fn a_twin_constructor_is_left_to_its_type() {
        let mut twins = index();
        twins.modules[0].items.extend([
            item("Point", DefKind::Type, "type Point {\n  x: Int,\n}", ""),
            item(
                "Point",
                DefKind::Ctor,
                "Point { x: Int }\nof type Point",
                "",
            ),
        ]);
        let exact = lookup(&twins, "Point").unwrap();
        assert!(matches!(exact, Found::Items { .. }));
        assert_eq!(names(&exact), ["app::list::Point"]);
        assert_eq!(names(&lookup(&twins, "poi").unwrap()), ["app::list::Point"]);
    }

    /// Another language's name for a std function leads to Cheby's, with
    /// a note (D-1658).
    #[test]
    fn a_foreign_name_leads_to_the_std_item() {
        let index = index();
        let Some(Found::Items { note, items }) = lookup(&index, "list::len") else {
            panic!("no items");
        };
        assert_eq!(
            note.as_deref(),
            Some("std has no `list::len`; Cheby's name for it is `list::length`")
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.name, "length");
    }

    #[test]
    fn a_module_lists_signatures_and_first_sentences() {
        let index = index();
        let text = render_text(&lookup(&index, "std::option").unwrap());
        assert_eq!(
            text,
            "std::option

  Optional values, as `a.b` shows. And more.

pub exposed type Option<T> {
  Some(T),
  None,
}
    A value or none.

pub fn length<T>(o: Option<T>) -> Int
    0 or 1.
"
        );
        let list = render_text(&lookup(&index, "list").unwrap());
        assert!(
            list.contains("\npub fn length<T>(xs: List<T>) -> Int\n    The number of elements.\n")
        );
        assert!(list.contains("\npub fn fold<T, A>(…) -> A\n    deprecated: use `fold_left`\n"));
    }

    #[test]
    fn an_item_has_its_whole_doc_and_what_limits_it() {
        let index = index();
        assert_eq!(
            render_text(&lookup(&index, "list::length").unwrap()),
            "std::list::length

pub fn length<T>(xs: List<T>) -> Int

  The number of elements. It is O(1).

  A second paragraph.
"
        );
        assert_eq!(
            render_text(&lookup(&index, "list::fold").unwrap()),
            "std::list::fold

pub fn fold<T, A>(…) -> A

  deprecated: use `fold_left`
  targets: native
"
        );
    }

    #[test]
    fn names_and_modules_in_short() {
        let index = index();
        assert_eq!(
            render_text(&lookup(&index, "opt").unwrap()),
            "std::option::Option
    pub exposed type Option<T> { …
        A value or none.
"
        );
        assert_eq!(
            render_text(&lookup(&index, "std").unwrap()),
            "std::list
    Lists.
std::option
    Optional values, as `a.b` shows.
"
        );
    }

    #[test]
    fn first_sentences() {
        assert_eq!(first_sentence("One. Two."), Some("One.".to_owned()));
        assert_eq!(
            first_sentence("Spans\ntwo lines. Two."),
            Some("Spans two lines.".to_owned())
        );
        assert_eq!(
            first_sentence("No full stop"),
            Some("No full stop".to_owned())
        );
        assert_eq!(
            first_sentence("Reads `a.b. c` and 1.5 too. Next."),
            Some("Reads `a.b. c` and 1.5 too.".to_owned())
        );
        assert_eq!(
            first_sentence("\n\nAfter blank lines.\n\nLater."),
            Some("After blank lines.".to_owned())
        );
        assert_eq!(first_sentence("  \n"), None);
    }

    #[test]
    fn json_lines_for_modules_and_items() {
        let index = index();
        assert_eq!(
            render_json(&lookup(&index, "list::fold").unwrap()),
            "{\"type\":\"item\",\"module\":\"std::list\",\"name\":\"fold\",\"kind\":\"fn\",\
             \"signature\":\"pub fn fold<T, A>(…) -> A\",\"deprecated\":\"use `fold_left`\",\
             \"targets\":[\"native\"]}\n"
        );
        let module = render_json(&lookup(&index, "std::option").unwrap());
        let lines: Vec<&str> = module.lines().collect();
        assert_eq!(lines.len(), 3, "{module}");
        assert!(lines[0].starts_with(
            "{\"type\":\"module\",\"path\":\"std::option\",\"origin\":\"std\",\"doc\":"
        ));
        assert!(lines[1].contains("\"name\":\"Option\",\"kind\":\"type\""));
        assert!(lines[2].contains("\"targets\":[\"native\",\"js\"]"));
        let aliased = render_json(&lookup(&index, "list::len").unwrap());
        assert!(
            aliased.starts_with("{\"type\":\"note\",\"message\":\"std has no"),
            "{aliased}"
        );
    }
}
