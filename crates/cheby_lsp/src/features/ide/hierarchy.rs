//! Call hierarchy over the occurrence index (X §2.8, F §5).
//!
//! Only functions are items of the hierarchy: top-level functions and
//! interface functions. A local function is part of the item that holds
//! it, so its calls are that item's. A caller may also be a constant or a
//! `test`, whose bodies call functions too (X §2.8).
//!
//! A call is any occurrence that names the function other than its
//! declaration, an import or an attribute's argument: a call, a use as a
//! value such as `list::map(xs, f)`, a use through an import alias, and an
//! operator or `{x}` that calls it (F §5.3's implicit occurrences). The
//! index knows what it knows, so on broken text the lists hold what it
//! placed, and never an error (X §1.3).

use std::collections::BTreeMap;

use cheby_driver::analysis::{
    DefId, DefIndex, DefKind, FileId, ModulePath, ModuleView, Snapshot, Symbol, TextRange, TextSize,
};
use cheby_syntax::SyntaxKind;

use crate::features::ide::cursor::{pick_ident, token_at};
use crate::features::index::{Index, Meaning, ModuleIndex, Occurrence, Role};
use crate::features::{CancelToken, RequestError};

/// One item of the hierarchy: a function, or a constant or `test` that
/// calls one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallItem {
    /// The item in this snapshot.
    pub def: DefId,
    /// Its name across snapshots, for the item's `data` (D-1365).
    pub symbol: Symbol,
    /// Its name as written. A test's is its string.
    pub name: String,
    /// What it is.
    pub kind: DefKind,
    /// The module that declares it.
    pub module: ModulePath,
    /// The file of `range` and `selection`.
    pub file: FileId,
    /// The whole declaration, or its name when the module keeps no tree,
    /// as std in a package run.
    pub range: TextRange,
    /// The name, inside `range`.
    pub selection: TextRange,
    /// The item carries `@deprecated`.
    pub deprecated: bool,
}

/// One entry of an incoming or outgoing list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Call {
    /// The caller, for incoming calls; the callee, for outgoing ones.
    pub item: CallItem,
    /// Where the caller names the callee, in the caller's file, in text
    /// order.
    pub ranges: Vec<TextRange>,
}

/// The function at `offset` of `file`: a name that names one, else an
/// operator that calls one. `None` anywhere else, on a local function
/// included (X §2.8).
pub(crate) fn prepare(index: Index<'_>, file: FileId, offset: TextSize) -> Option<CallItem> {
    let module = index.of_file(file)?;
    let snapshot = index.snapshot();
    let tree = snapshot.module(module.module)?.tree()?;
    let token = pick_ident(token_at(tree.root(), offset))?;
    let def = if matches!(
        token.kind(),
        SyntaxKind::LOWER_IDENT | SyntaxKind::UPPER_IDENT
    ) {
        module
            .at(token.text_range())
            .and_then(|o| callee(snapshot, module, o))
    } else {
        module
            .implicit_at(offset)
            .into_iter()
            .find_map(|o| callee(snapshot, module, o))
    }?;
    item(snapshot, def)
}

/// `def` as an item of the hierarchy. `None` for an item the run does not
/// have, one with no name across snapshots, and a builtin, which has no
/// source to point at.
pub(crate) fn item(snapshot: &Snapshot, def: DefId) -> Option<CallItem> {
    let iface = snapshot.interfaces().get(def.module)?;
    let declared = iface.item(def.index)?;
    let symbol = snapshot.key_of(def)?;
    let module = snapshot.table().get(def.module)?.path.clone();
    let view = snapshot.module(def.module)?;
    let (file, range, selection) = if let Some(found) = declaration(view, def.index) {
        found
    } else {
        let span = declared.span?;
        (span.file, span.range, span.range)
    };
    Some(CallItem {
        def,
        symbol,
        name: snapshot.globals().names.get(declared.name).to_owned(),
        kind: declared.kind,
        module,
        file,
        range: if range.contains_range(selection) {
            range
        } else {
            range.cover(selection)
        },
        selection,
        deprecated: declared.deprecated.is_some(),
    })
}

/// The file, whole range and name range of item `index` of `view`, from
/// its tree. `None` when the module keeps no tree or resolution.
fn declaration(view: ModuleView<'_>, index: DefIndex) -> Option<(FileId, TextRange, TextRange)> {
    let (tree, resolved) = (view.tree()?, view.resolved()?);
    let item = resolved.item(index)?;
    let node = tree.node(item.node)?;
    Some((item.name_span.file, node.text_range(), item.name_span.range))
}

/// Every item that calls `target`, each with the ranges of its calls,
/// sorted by module path and position (X §1.4). A module the run did not
/// resolve gives none.
///
/// # Errors
///
/// [`RequestError::Cancelled`] when `cancel` is set, checked per module.
pub(crate) fn incoming(
    index: Index<'_>,
    target: DefId,
    cancel: &CancelToken,
) -> Result<Vec<Call>, RequestError> {
    let snapshot = index.snapshot();
    let mut found: BTreeMap<DefId, Vec<TextRange>> = BTreeMap::new();
    for module in index.modules() {
        cancel.check()?;
        let Some(view) = snapshot.module(module.module) else {
            continue;
        };
        let owners = owners(view);
        for occurrence in &module.occurrences {
            if !is_call(occurrence.role) || !names_def(module, occurrence, target) {
                continue;
            }
            if let Some(owner) = owner_of(&owners, occurrence.range) {
                found
                    .entry(DefId {
                        module: module.module,
                        index: owner,
                    })
                    .or_default()
                    .push(occurrence.range);
            }
        }
    }
    Ok(calls(snapshot, found))
}

/// Every function that `caller` calls, each with the ranges of its calls
/// in `caller`, sorted by module path and position (X §1.4). Empty when
/// `caller`'s module keeps no tree, as std in a package run.
///
/// # Errors
///
/// [`RequestError::Cancelled`] when `cancel` is set.
pub(crate) fn outgoing(
    index: Index<'_>,
    caller: DefId,
    cancel: &CancelToken,
) -> Result<Vec<Call>, RequestError> {
    cancel.check()?;
    let snapshot = index.snapshot();
    let (Some(module), Some(view)) = (index.module(caller.module), snapshot.module(caller.module))
    else {
        return Ok(Vec::new());
    };
    let Some((_, range, _)) = declaration(view, caller.index) else {
        return Ok(Vec::new());
    };
    let mut found: BTreeMap<DefId, Vec<TextRange>> = BTreeMap::new();
    for occurrence in &module.occurrences {
        if !is_call(occurrence.role) || !range.contains_range(occurrence.range) {
            continue;
        }
        if let Some(def) = callee(snapshot, module, occurrence) {
            found.entry(def).or_default().push(occurrence.range);
        }
    }
    Ok(calls(snapshot, found))
}

/// The entries of `found`, sorted, each with its ranges in text order and
/// once each. An item that [`item`] cannot place is left out.
fn calls(snapshot: &Snapshot, found: BTreeMap<DefId, Vec<TextRange>>) -> Vec<Call> {
    let mut calls: Vec<Call> = found
        .into_iter()
        .filter_map(|(def, mut ranges)| {
            ranges.sort_by_key(|r| (r.start(), r.end()));
            ranges.dedup();
            Some(Call {
                item: item(snapshot, def)?,
                ranges,
            })
        })
        .collect();
    calls.sort_by(|a, b| {
        let key = |c: &Call| (c.item.module.to_string(), c.item.selection.start());
        key(a)
            .cmp(&key(b))
            .then_with(|| a.item.name.cmp(&b.item.name))
    });
    calls
}

/// Whether an occurrence of `role` counts as a call: anything but a
/// declaration, a qualifier, an import or an attribute's argument.
const fn is_call(role: Role) -> bool {
    !matches!(
        role,
        Role::Decl | Role::Qualifier | Role::Import | Role::Attr
    )
}

/// Whether `occurrence` names `target`, as its symbol or as the value of a
/// punned field. A use through an import alias means what the alias
/// names, so it counts.
fn names_def(module: &ModuleIndex, occurrence: &Occurrence, target: DefId) -> bool {
    meanings(module, occurrence).any(|meaning| meaning == Meaning::Item(target))
}

/// The function `occurrence` names, as its symbol or as the value of a
/// punned field: a top-level function or an interface function.
fn callee(snapshot: &Snapshot, module: &ModuleIndex, occurrence: &Occurrence) -> Option<DefId> {
    meanings(module, occurrence).find_map(|meaning| match meaning {
        Meaning::Item(def) if is_function(snapshot, def) => Some(def),
        Meaning::Item(_)
        | Meaning::Module(_)
        | Meaning::Field { .. }
        | Meaning::TypeParam { .. }
        | Meaning::Local { .. } => None,
    })
}

/// What `occurrence` means: its symbol's meaning, then its `also`'s.
fn meanings<'m>(
    module: &'m ModuleIndex,
    occurrence: &Occurrence,
) -> impl Iterator<Item = Meaning> + 'm {
    std::iter::once(occurrence.symbol)
        .chain(occurrence.also)
        .filter_map(move |id| module.entry(id).map(|entry| entry.meaning))
}

/// Whether `def` is a top-level function or an interface function.
fn is_function(snapshot: &Snapshot, def: DefId) -> bool {
    snapshot
        .interfaces()
        .get(def.module)
        .and_then(|iface| iface.item(def.index))
        .is_some_and(|item| matches!(item.kind, DefKind::Fn | DefKind::InterfaceFn))
}

/// The items of `view` whose bodies can call: functions, constants and
/// tests, with their declarations' ranges, sorted by start.
fn owners(view: ModuleView<'_>) -> Vec<(TextRange, DefIndex)> {
    let (Some(tree), Some(resolved)) = (view.tree(), view.resolved()) else {
        return Vec::new();
    };
    let mut owners: Vec<(TextRange, DefIndex)> = resolved
        .items
        .iter_enumerated()
        .filter(|(_, item)| matches!(item.kind, DefKind::Fn | DefKind::Const | DefKind::Test))
        .filter_map(|(index, item)| Some((tree.node(item.node)?.text_range(), index)))
        .collect();
    owners.sort_by_key(|(range, _)| range.start());
    owners
}

/// The item of `owners` whose declaration holds `range`.
fn owner_of(owners: &[(TextRange, DefIndex)], range: TextRange) -> Option<DefIndex> {
    let after = owners.partition_point(|(r, _)| r.start() <= range.start());
    owners
        .get(..after)?
        .iter()
        .rev()
        .find(|(r, _)| r.contains_range(range))
        .map(|&(_, index)| index)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use cheby_driver::analysis::{
        DefKind, Globals, Overlay, OverlayFile, PackageKind, Session, SessionConfig, Snapshot,
        TextSize,
    };

    use super::{Call, CallItem, incoming, item, outgoing, prepare};
    use crate::features::{Analyzed, CancelToken};

    /// A package `app` in a fresh temporary directory, removed on drop.
    pub(crate) struct Temp(pub PathBuf);

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Temp {
        /// The package `app` with these files under `src/`.
        pub(crate) fn package(name: &str, files: &[(&str, &str)]) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("cheby_lsp_hierarchy_{}_{name}", std::process::id()));
            if dir.exists() {
                std::fs::remove_dir_all(&dir).unwrap();
            }
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(
                dir.join("cheby.toml"),
                "name = \"app\"\nversion = \"0.1.0\"\n",
            )
            .unwrap();
            for (path, text) in files {
                std::fs::write(dir.join("src").join(path), text).unwrap();
            }
            Self(std::path::absolute(dir).unwrap())
        }

        /// The absolute path of `src/<name>`.
        pub(crate) fn src(&self, name: &str) -> PathBuf {
            self.0.join("src").join(name)
        }
    }

    /// One Q run of `root`, with `open` as open documents at version 1.
    pub(crate) fn analyze(root: &Path, open: &[(PathBuf, &str)]) -> Analyzed {
        let mut session = Session::new(SessionConfig {
            root: root.to_path_buf(),
            kind: PackageKind::Package,
            target: None,
            globals: Arc::new(Globals::new()),
        });
        let overlay = Overlay {
            files: open
                .iter()
                .map(|(path, text)| {
                    let file = OverlayFile {
                        text: Arc::from(*text),
                        version: 1,
                        stamp: 1,
                    };
                    (path.clone(), file)
                })
                .collect(),
        };
        let live = session.analyze_live(&overlay).expect("a run");
        Analyzed::new(live.snapshot.expect("a snapshot"))
    }

    /// The offset of the `n`th `needle` in `text`, plus `delta`.
    pub(crate) fn offset_of(text: &str, needle: &str, n: usize, delta: usize) -> TextSize {
        let at = text
            .match_indices(needle)
            .nth(n)
            .map(|(at, _)| at)
            .expect("the needle");
        TextSize::try_from(at.checked_add(delta).unwrap()).unwrap()
    }

    /// The item prepared at `needle` in `src/<file>`.
    fn prepared(analyzed: &Analyzed, src: &Path, at: TextSize) -> Option<CallItem> {
        let snapshot: &Snapshot = &analyzed.snapshot;
        let file = snapshot.file_of(src).expect("the file");
        prepare(analyzed.index(), file, at)
    }

    /// Each call as `(name, ranges as text)`, `text` being the caller's.
    fn shown(calls: &[Call], text: &str) -> Vec<(String, Vec<String>)> {
        calls
            .iter()
            .map(|c| {
                let ranges = c
                    .ranges
                    .iter()
                    .map(|r| r.slice(text).expect("in the text").to_owned())
                    .collect();
                (c.item.name.clone(), ranges)
            })
            .collect()
    }

    const MAIN: &str = "import app::other::{twice as double}\n\nfn even(n: Int) -> Bool {\n  case n {\n    0 => True\n    _ => odd(n - 1)\n  }\n}\n\nfn odd(n: Int) -> Bool {\n  case n {\n    0 => False\n    _ => even(n - 1)\n  }\n}\n\nfn apply(f: fn(Int) -> Bool, n: Int) -> Bool {\n  f(n)\n}\n\nconst start: Bool = even(4)\n\nfn main() {\n  fn inner(x: Int) -> Bool {\n    odd(x)\n  }\n  assert apply(even, 2)\n  assert inner(3)\n  assert double(2) == 4\n}\n\ntest \"even\" {\n  assert even(2)\n}\n";

    const OTHER: &str = "pub fn twice(n: Int) -> Int {\n  n + n\n}\n";

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn mutual_recursion_value_uses_alias_and_tests() {
        let temp = Temp::package("clean", &[("main.cheby", MAIN), ("other.cheby", OTHER)]);
        let main = temp.src("main.cheby");
        let analyzed = analyze(&temp.0, &[(main.clone(), MAIN)]);
        let even = prepared(&analyzed, &main, offset_of(MAIN, "even", 0, 1)).expect("even");
        assert_eq!((even.name.as_str(), even.kind), ("even", DefKind::Fn));
        assert_eq!(even.selection.slice(MAIN), Some("even"));
        assert!(
            even.range
                .slice(MAIN)
                .is_some_and(|t| t.starts_with("fn even") && t.ends_with('}'))
        );
        let cancel = CancelToken::new();
        let callers = incoming(analyzed.index(), even.def, &cancel).unwrap();
        let s = |x: &str| x.to_owned();
        assert_eq!(
            shown(&callers, MAIN),
            [
                (s("odd"), vec![s("even")]),
                (s("start"), vec![s("even")]),
                (s("main"), vec![s("even")]),
                (s("even"), vec![s("even")]),
            ]
        );
        // `main` calls `odd` through its local function, which is part of
        // it, and `twice` through the alias.
        let main_fn = prepared(&analyzed, &main, offset_of(MAIN, "main", 0, 0));
        assert_eq!(main_fn.as_ref().map(|i| i.name.as_str()), Some("main"));
        let called = outgoing(analyzed.index(), main_fn.unwrap().def, &cancel).unwrap();
        assert_eq!(
            shown(&called, MAIN),
            [
                (s("even"), vec![s("even")]),
                (s("odd"), vec![s("odd")]),
                (s("apply"), vec![s("apply")]),
                (s("twice"), vec![s("double")]),
            ]
        );
        // A local function is no item of the hierarchy.
        assert_eq!(
            prepared(&analyzed, &main, offset_of(MAIN, "inner", 0, 0)),
            None
        );
        // Nor is a parameter, even one of function type.
        assert_eq!(
            prepared(&analyzed, &main, offset_of(MAIN, "f(n)", 0, 0)),
            None
        );
        // The alias names `twice`, in the other file.
        let twice = prepared(&analyzed, &main, offset_of(MAIN, "double(2)", 0, 0)).unwrap();
        assert_eq!(twice.name, "twice");
        assert_eq!(twice.module.to_string(), "app::other");
        // A test calls too, under its string.
        let test_calls = incoming(analyzed.index(), even.def, &cancel).unwrap();
        assert!(test_calls.iter().any(|c| c.item.kind == DefKind::Test));
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn operators_call_their_function() {
        const TEXT: &str = "type V {\n  n: Int,\n}\n\nfn add(a: V, b: V) -> V {\n  V { n: a.n + b.n }\n}\n\nfn sum(a: V, b: V) -> V {\n  a + b\n}\n\nfn main() {\n  assert sum(V { n: 1 }, V { n: 2 }).n == 3\n}\n";
        let temp = Temp::package("operators", &[("main.cheby", TEXT)]);
        let main = temp.src("main.cheby");
        let analyzed = analyze(&temp.0, &[(main.clone(), TEXT)]);
        let add = prepared(&analyzed, &main, offset_of(TEXT, "fn add", 0, 3)).expect("add");
        let cancel = CancelToken::new();
        let callers = incoming(analyzed.index(), add.def, &cancel).unwrap();
        let s = |x: &str| x.to_owned();
        assert_eq!(shown(&callers, TEXT), [(s("sum"), vec![s("+")])]);
        // The `+` itself prepares `add`.
        let plus = prepared(&analyzed, &main, offset_of(TEXT, "a + b", 0, 2)).expect("+");
        assert_eq!(plus.def, add.def);
        let sum = prepared(&analyzed, &main, offset_of(TEXT, "fn sum", 0, 3)).unwrap();
        let called = outgoing(analyzed.index(), sum.def, &cancel).unwrap();
        assert_eq!(shown(&called, TEXT), [(s("add"), vec![s("+")])]);
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn a_type_error_keeps_the_calls() {
        const TEXT: &str = "fn f(n: Int) -> Int {\n  n\n}\n\nfn g() -> Int {\n  f(\"one\") + f(2)\n}\n\nfn main() {\n  assert g() == 3\n}\n";
        let temp = Temp::package("type_error", &[("main.cheby", TEXT)]);
        let main = temp.src("main.cheby");
        let analyzed = analyze(&temp.0, &[(main.clone(), TEXT)]);
        let f = prepared(&analyzed, &main, offset_of(TEXT, "fn f", 0, 3)).expect("f");
        let cancel = CancelToken::new();
        let callers = incoming(analyzed.index(), f.def, &cancel).unwrap();
        let s = |x: &str| x.to_owned();
        assert_eq!(shown(&callers, TEXT), [(s("g"), vec![s("f"), s("f")])]);
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn a_grammar_error_keeps_what_parses() {
        const TEXT: &str = "fn f(n: Int) -> Int {\n  n\n}\n\nfn g() -> Int {\n  let x = f(1) +\n}\n\nfn h() -> Int {\n  f(2)\n}\n\nfn main() {\n  assert h() == 2\n}\n";
        let temp = Temp::package("grammar_error", &[("main.cheby", TEXT)]);
        let main = temp.src("main.cheby");
        let analyzed = analyze(&temp.0, &[(main.clone(), TEXT)]);
        let f = prepared(&analyzed, &main, offset_of(TEXT, "fn f", 0, 3)).expect("f");
        let cancel = CancelToken::new();
        let callers = incoming(analyzed.index(), f.def, &cancel).unwrap();
        let names: Vec<&str> = callers.iter().map(|c| c.item.name.as_str()).collect();
        assert!(names.contains(&"h"), "{names:?}");
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn std_items_point_at_their_names() {
        const TEXT: &str = "import std::io\n\nfn main() {\n  io::println(\"hi\")\n}\n";
        let temp = Temp::package("std", &[("main.cheby", TEXT)]);
        let main = temp.src("main.cheby");
        let analyzed = analyze(&temp.0, &[(main.clone(), TEXT)]);
        let println = prepared(&analyzed, &main, offset_of(TEXT, "println", 0, 0)).unwrap();
        assert_eq!(println.module.to_string(), "std::io");
        assert_eq!(println.range, println.selection);
        let cancel = CancelToken::new();
        // Std keeps no tree in a package run: no outgoing calls there.
        assert_eq!(
            outgoing(analyzed.index(), println.def, &cancel).unwrap(),
            []
        );
        let callers = incoming(analyzed.index(), println.def, &cancel).unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(item(&analyzed.snapshot, println.def), Some(println));
    }

    #[test]
    #[cfg_attr(windows, ignore = "builds a JIT until M6 (D-545, D-641)")]
    fn cancelled_walks_stop() {
        let temp = Temp::package("cancel", &[("main.cheby", MAIN), ("other.cheby", OTHER)]);
        let main = temp.src("main.cheby");
        let analyzed = analyze(&temp.0, &[(main.clone(), MAIN)]);
        let even = prepared(&analyzed, &main, offset_of(MAIN, "even", 0, 0)).unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(incoming(analyzed.index(), even.def, &cancel).is_err());
        assert!(outgoing(analyzed.index(), even.def, &cancel).is_err());
    }
}
