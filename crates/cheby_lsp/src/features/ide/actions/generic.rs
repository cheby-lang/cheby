//! The fixes that a help's edits make (X §6.2's H rows), and the "Use a
//! field path" rewrite behind the provider interface (D-1645, D-1682).
//!
//! Every help whose edits all lie in the request's file becomes a quick
//! fix with those edits, titled with the help's text (X §6.2), and marked
//! preferred, which the frame keeps when the diagnostic has no other fix
//! with an edit. Two helps take titles of their own, since neither text
//! reads as the one thing its edit does (D-1694): D-1657's `.._` help
//! (E0013), whose edit deletes the `_`, is "Write `..`", and D-1661's
//! brace help (E0012), whose edit writes a `\` before the `{`, is "Escape
//! the brace as `\{`". E0073's help, whose edit writes `assert `, is "Add
//! `assert`" and not preferred, as D-1736's table and X §6.2 give its row.
//! Each is picked by code and by the edit's shape, the text it replaces
//! and its replacement, never by the help's text (D-1388).
//!
//! E0006's help on a name that is used is offered as a rename of every
//! use, computed on resolve through rename, with its checks (D-1668,
//! D-1709), where only the E0006 of the declarations it renames may go
//! away; the help of an unused name keeps its edit of the declaration
//! (D-1741). A function and an interface's function count as used, since
//! a satisfaction uses one by its name with no occurrence, and so does a
//! record type written through its constructor. A client that cannot
//! resolve `edit` gets no fix from E0006's help, and the help keeps its
//! line in the message (D-1754).
//!
//! A help offers nothing here, and keeps its line in the message
//! (`server::publish`), when an edit lies in another file (D-1752), when
//! it writes `pub` in a std checkout (E0022's two helps, D-1706), when it
//! writes a prelude name that the module hides, `Nil` as E0030's can or
//! `Int` and `True` as the habit helps can (D-1678, D-1931), or
//! when it is D-1661's escape of a brace in a shape it does not fix, such
//! as `"{"x"}"`, where the text it leaves has a grammar error (D-1742), as
//! `actions::offers_fix` says.

use cheby_driver::analysis::codes::{self, Code};
use cheby_driver::analysis::{DefKind, PlainDiagnostic, PlainEdit, PlainHelp, TextRange};

use super::offers_fix;
use super::{Action, ActionId, ActionKind, DiagRef, FixCx};
use crate::features::ide::rename::{self, RenameError};
use crate::features::ide::{Refusal, field_path, nav};
use crate::features::index::Meaning;

/// D-1694's title for D-1657's `.._` help.
pub(crate) const WRITE_REST: &str = "Write `..`";

/// D-1694's title for D-1661's brace help.
pub(crate) const ESCAPE_BRACE: &str = "Escape the brace as `\\{`";

/// D-1736's title for E0073's help, which writes `assert ` before the
/// test's last expression.
pub(crate) const ADD_ASSERT: &str = "Add `assert`";

/// The H rows' fixes among `diagnostics`, the report's that overlap the
/// range.
pub(crate) fn provide(
    cx: &FixCx<'_>,
    _range: TextRange,
    diagnostics: &[(usize, &PlainDiagnostic)],
) -> Vec<Action> {
    let Some(path) = cx.path() else {
        return Vec::new();
    };
    let snapshot = &*cx.analyzed.snapshot;
    diagnostics
        .iter()
        .flat_map(|&(index, diagnostic)| {
            diagnostic
                .helps
                .iter()
                .filter(|help| {
                    offers_fix(
                        snapshot,
                        cx.view,
                        diagnostic.code,
                        help,
                        path,
                        cx.resolve_edits,
                    )
                })
                .map(move |help| {
                    let own = own_title(diagnostic.code, help, cx.text);
                    (help, own)
                })
                .map(move |(help, own)| {
                    // E0006's help on a name that is used renames every
                    // use, on resolve (D-1741).
                    let renamed = renames_uses(cx, diagnostic, help);
                    Action {
                        title: own
                            .map_or_else(|| help.message.clone(), |(title, _)| title.to_owned()),
                        kind: ActionKind::QuickFix,
                        diagnostic: Some(DiagRef::Report(index)),
                        preferred: own.is_none_or(|(_, preferred)| preferred),
                        removes: false,
                        edit: if renamed.is_some() {
                            None
                        } else {
                            Some(help.edits.clone())
                        },
                        data: renamed,
                        disabled: None,
                        command: None,
                    }
                })
        })
        .collect()
}

/// The rename that E0006's `help` becomes when it renames a name that is
/// used (D-1741): its one edit replaces the diagnostic's span, the name of
/// a declaration, and the index places some other occurrence of that
/// name's symbol or of a symbol that rename writes with it, such as a
/// record type's constructor, or the name is a function's or an interface
/// function's, which a satisfaction uses with no occurrence. `None` for any other
/// help, and for an unused name, whose fix keeps the help's edit of the
/// declaration alone.
fn renames_uses(
    cx: &FixCx<'_>,
    diagnostic: &PlainDiagnostic,
    help: &PlainHelp,
) -> Option<ActionId> {
    if diagnostic.code != codes::NAMING_CONVENTION {
        return None;
    }
    let ([edit], Some(at)) = (help.edits.as_slice(), diagnostic.range) else {
        return None;
    };
    if edit.range != at {
        return None;
    }
    let index = cx.analyzed.index();
    let picked = nav::pick(index, cx.view, at.start())?;
    if picked.origin != at {
        return None;
    }
    let entry = picked.entries.first()?;
    let satisfiable = match entry.meaning {
        Meaning::Item(def) => cx
            .analyzed
            .snapshot
            .interfaces()
            .get(def.module)
            .and_then(|iface| iface.item(def.index))
            .is_some_and(|item| matches!(item.kind, DefKind::Fn | DefKind::InterfaceFn)),
        Meaning::Module(_)
        | Meaning::Field { .. }
        | Meaning::TypeParam { .. }
        | Meaning::Local { .. } => false,
    };
    // A record type is used through its constructor too, which rename
    // writes with it (X §3.4).
    let used = satisfiable || {
        let mut used = false;
        for symbol in rename::coupled_symbols(&cx.analyzed.snapshot, cx.view, entry) {
            let hits = index.references(&symbol, cx.cancel).ok()?.hits;
            if hits
                .iter()
                .any(|hit| hit.module.module != cx.view.id() || hit.occurrence.range != at)
            {
                used = true;
                break;
            }
        }
        used
    };
    used.then(|| ActionId::Rename {
        at: at.start(),
        name: edit.replacement.clone(),
    })
}

/// The edits of E0006's fix of a used name (D-1741): rename's, with its
/// checks, where only the declaration's E0006 may go away. The frame
/// answers a resolve of it itself, so that a stale file sweeps the
/// package (D-1731), and a client that takes the edits in the listing is
/// offered no E0006 fix (D-1754); this is the registry's resolver of the
/// id, which the tests call.
///
/// # Errors
///
/// Rename's refusal, a stale file's included, and a refusal when the name
/// can no longer be renamed there.
pub(crate) fn resolve_rename(
    cx: &FixCx<'_>,
    _range: TextRange,
    id: &ActionId,
) -> Result<Vec<PlainEdit>, Refusal> {
    let gone = || Refusal("the name can no longer be renamed here".to_owned());
    let ActionId::Rename { at, name } = id else {
        return Err(gone());
    };
    let file = cx.view.file().ok_or_else(gone)?;
    match rename::rename_fixing_case(cx.analyzed, file, *at, name, cx.cancel) {
        Ok(Some(plan)) => Ok(plan.edits),
        Ok(None) => Err(gone()),
        Err(RenameError::Refused(refusal) | RenameError::Stale { refusal, .. }) => Err(refusal),
        Err(RenameError::Cancelled) => Err(Refusal("the request was cancelled".to_owned())),
    }
}

/// The title of its own that `help`, a help of a diagnostic of `code` in
/// `text`, takes, from the code and the edit's shape alone, with whether
/// its row is preferred: D-1694's two rows, and E0073's, whose title and
/// "not preferred" D-1736 and X §6.2 give.
fn own_title(code: Code, help: &PlainHelp, text: &str) -> Option<(&'static str, bool)> {
    let [edit] = help.edits.as_slice() else {
        return None;
    };
    let replaced = edit.range.slice(text)?;
    match code {
        codes::SYNTAX_ERROR if replaced == "_" && edit.replacement.is_empty() => {
            Some((WRITE_REST, true))
        }
        codes::INVALID_INTERPOLATION if replaced.is_empty() && edit.replacement == "\\" => {
            Some((ESCAPE_BRACE, true))
        }
        codes::INVALID_TEST_RESULT if replaced.is_empty() && edit.replacement == "assert " => {
            Some((ADD_ASSERT, false))
        }
        _ => None,
    }
}

/// "Use a field path" for `range` (`ide::field_path`), with its edit in
/// the listing (D-1682).
pub(crate) fn use_field_path(
    cx: &FixCx<'_>,
    range: TextRange,
    diagnostics: &[(usize, &PlainDiagnostic)],
) -> Vec<Action> {
    field_path::provide(cx, range, diagnostics)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use cheby_driver::analysis::codes;
    use cheby_driver::analysis::{PlainEdit, PlainHelp, TextRange};

    use super::{ADD_ASSERT, ESCAPE_BRACE, WRITE_REST, own_title};
    use crate::features::ide::actions::ActionId;
    use crate::features::ide::fill_arms::tests::Fixture;

    fn help(message: &str, start: u32, end: u32, replacement: &str) -> PlainHelp {
        PlainHelp {
            message: message.to_owned(),
            edits: vec![PlainEdit {
                path: PathBuf::from("/p/src/main.cheby"),
                range: TextRange::new(start, end),
                replacement: replacement.to_owned(),
            }],
        }
    }

    /// P-1, D-1706: in a std checkout, E0022's help, which writes `pub`, is
    /// no quick fix, while in a package it is one.
    #[test]
    fn a_std_checkout_offers_no_fix_that_writes_pub() {
        const PROBE: &str = "//! A probe of E0022.\n\nexposed type Point {\n  x: Int,\n}\n";
        let at = "|exposed type Point";
        let package = Fixture::package("e22", &[("probe_e22.cheby", PROBE)], &["probe_e22.cheby"])
            .with_phase_a();
        let titles_in = |fixture: &Fixture| -> Vec<String> {
            fixture
                .offered("probe_e22.cheby", at, super::provide)
                .into_iter()
                .map(|a| a.title)
                .collect()
        };
        assert_eq!(
            titles_in(&package),
            ["make the type `pub` as well"],
            "{:?}",
            package.diagnostics("probe_e22.cheby")
        );
        let checkout = Fixture::checkout("e22", &[("probe_e22.cheby", PROBE)]).with_phase_a();
        assert!(
            checkout
                .diagnostics("probe_e22.cheby")
                .iter()
                .any(|d| d.code == codes::INVALID_EXPOSED),
            "{:?}",
            checkout.diagnostics("probe_e22.cheby")
        );
        assert_eq!(titles_in(&checkout), Vec::<String>::new());
    }

    /// C-4, D-1678: E0030's help writes `Nil`, which a module that declares
    /// a constructor `Nil` hides, so it is no quick fix there.
    #[test]
    fn no_fix_writes_a_hidden_nil() {
        let titles_in = |name: &str, types: &str| -> Vec<String> {
            let text = format!(
                "//! Units.\n\n{types}\nfn h() -> Unit {{\n  ()\n}}\n\nfn main() {{\n  let _ = h()\n}}\n"
            );
            let fixture =
                Fixture::package(name, &[("main.cheby", &text)], &["main.cheby"]).with_phase_a();
            fixture
                .offered("main.cheby", "{\n  |()", super::provide)
                .into_iter()
                .map(|a| a.title)
                .collect()
        };
        let plain = titles_in("nil_plain", "type Unit {\n  Other,\n}\n");
        assert!(
            plain.iter().any(|t| t.contains("`Nil`")),
            "the prelude's `Nil`: {plain:?}"
        );
        let hidden = titles_in("nil_hidden", "type Unit {\n  Nil,\n  Other,\n}\n");
        assert!(
            !hidden.iter().any(|t| t.contains("`Nil`")),
            "a hidden `Nil`: {hidden:?}"
        );
    }

    /// D-1741: E0006's help on a name that is used becomes a rename of every
    /// use, resolved through rename, and the declaration's own E0006 does
    /// not refuse it; on an unused name it keeps the help's one edit.
    #[test]
    fn a_used_name_is_renamed_everywhere() {
        const RECORD: &str = "//! Points.\n\ntype My_Point {\n  x: Int,\n}\n\nfn main() {\n  let p = My_Point { x: 1 }\n  assert p.x == 1\n}\n";
        const HIDES: &str = "//! Names.\n\nfn maxValue() -> Int {\n  3\n}\n\nfn main() {\n  assert maxValue() == max_value()\n}\n";
        const TEXT: &str = "//! Names.\n\nfn main() {\n  let maxValue = 3\n  assert maxValue == 3\n  let otherValue = 1\n}\n";
        let fixture =
            Fixture::package("e6_used", &[("main.cheby", TEXT)], &["main.cheby"]).with_phase_a();
        let used = fixture.offered("main.cheby", "|maxValue = 3", super::provide);
        let fix = used
            .iter()
            .find(|a| a.title == "write `max_value`")
            .unwrap_or_else(|| panic!("{used:?}"));
        assert_eq!(fix.edit, None);
        assert_eq!(
            fix.data,
            Some(ActionId::Rename {
                at: u32::try_from(TEXT.find("maxValue").unwrap()).unwrap(),
                name: "max_value".to_owned(),
            })
        );
        let cx = fixture.cx("main.cheby");
        let range = fixture.range("main.cheby", "|maxValue = 3");
        let edits = super::resolve_rename(&cx, range, fix.data.as_ref().unwrap()).unwrap();
        assert_eq!(
            fixture.applied("main.cheby", &edits),
            TEXT.replace("maxValue", "max_value")
        );
        let unused = fixture.offered("main.cheby", "|otherValue", super::provide);
        let fix = unused
            .iter()
            .find(|a| a.title == "write `other_value`")
            .unwrap_or_else(|| panic!("{unused:?}"));
        assert_eq!(fix.data, None);
        assert_eq!(fix.edit.as_ref().map(Vec::len), Some(1));
        // A record type written only through its constructor is used: the
        // fix renames the type and the constructor together.
        let fixture = Fixture::package("e6_record", &[("main.cheby", RECORD)], &["main.cheby"])
            .with_phase_a();
        let offered = fixture.offered("main.cheby", "type |My_Point", super::provide);
        let fix = offered
            .iter()
            .find(|a| a.title == "write `MyPoint`")
            .unwrap_or_else(|| panic!("{offered:?}"));
        assert_eq!(fix.edit, None);
        let cx = fixture.cx("main.cheby");
        let range = fixture.range("main.cheby", "type |My_Point");
        let edits = super::resolve_rename(&cx, range, fix.data.as_ref().unwrap()).unwrap();
        assert_eq!(
            fixture.applied("main.cheby", &edits),
            RECORD.replace("My_Point", "MyPoint")
        );
        // Another error that the rename would hide still refuses it
        // (D-1668): here the call of `max_value` would bind.
        let fixture =
            Fixture::package("e6_hides", &[("main.cheby", HIDES)], &["main.cheby"]).with_phase_a();
        let offered = fixture.offered("main.cheby", "fn |maxValue", super::provide);
        let fix = offered
            .iter()
            .find(|a| a.title == "write `max_value`")
            .unwrap_or_else(|| panic!("{offered:?}"));
        let cx = fixture.cx("main.cheby");
        let range = fixture.range("main.cheby", "fn |maxValue");
        let refusal = super::resolve_rename(&cx, range, fix.data.as_ref().unwrap()).unwrap_err();
        assert!(
            refusal
                .0
                .starts_with("Renaming would hide an error: error[E0042]"),
            "{}",
            refusal.0
        );
    }

    /// D-1741: E0006's fix on a name declared at several places, the
    /// alternatives of an or-pattern, or an interface's function and the
    /// function that satisfies it: the E0006 of each declaration it renames
    /// may go away, and the rename refuses what else would break (W11's
    /// review, A1, S3, S4).
    #[test]
    fn a_name_declared_twice_is_renamed_at_both() {
        const OR: &str = "//! Shapes.\n\ntype Shape {\n  Circle(Int),\n  Square(Int),\n}\n\nfn area(s: Shape) -> Int {\n  case s {\n    Circle(sideLen) | Square(sideLen) => sideLen * sideLen\n  }\n}\n\nfn main() {\n  assert area(Circle(2)) == 4\n}\n";
        const IFACE: &str = "//! Shapes.\n\ninterface Shape {\n  fn areaOf(Self) -> Int\n}\n\ntype Sq {\n  Sq(Int),\n}\n\nfn areaOf(s: Sq) -> Int {\n  case s {\n    Sq(n) => n * n\n  }\n}\n\nfn total<S: Shape>(s: S) -> Int {\n  Shape::areaOf(s)\n}\n\nfn main() {\n  assert total(Sq(2)) == 4\n}\n";
        let resolved = |fixture: &Fixture, needle: &str, new: &str| {
            let offered = fixture.offered("main.cheby", needle, super::provide);
            let title = format!("write `{new}`");
            let fix = offered
                .iter()
                .find(|a| a.title == title)
                .unwrap_or_else(|| panic!("{needle}: {offered:?}"));
            assert_eq!(fix.edit, None, "{needle}");
            let cx = fixture.cx("main.cheby");
            let range = fixture.range("main.cheby", needle);
            super::resolve_rename(&cx, range, fix.data.as_ref().unwrap())
        };
        let fixture =
            Fixture::package("e6_or", &[("main.cheby", OR)], &["main.cheby"]).with_phase_a();
        for needle in ["Circle(|sideLen)", "Square(|sideLen)"] {
            let edits = resolved(&fixture, needle, "side_len").unwrap();
            assert_eq!(
                fixture.applied("main.cheby", &edits),
                OR.replace("sideLen", "side_len"),
                "{needle}"
            );
        }
        let fixture =
            Fixture::package("e6_iface", &[("main.cheby", IFACE)], &["main.cheby"]).with_phase_a();
        // At the interface's function: the satisfying function follows.
        let edits = resolved(&fixture, "fn |areaOf(Self)", "area_of").unwrap();
        assert_eq!(
            fixture.applied("main.cheby", &edits),
            IFACE.replace("areaOf", "area_of")
        );
        // At the satisfying function, which no call names: a rename too,
        // which refuses to leave `Sq` without `Shape`.
        let refusal = resolved(&fixture, "fn |areaOf(s: Sq)", "area_of").unwrap_err();
        assert!(
            refusal.0.starts_with("Renaming would cause: error[E0063]"),
            "{}",
            refusal.0
        );
    }

    /// D-1742: D-1661's escape of the brace is a fix where it leaves no
    /// grammar error, as in `"price {" + amount + "}"`, and none in
    /// `"{"x"}"`, which it would turn into `"\{" x "}"`.
    #[test]
    fn the_brace_escape_is_offered_where_it_parses() {
        let titles_in = |name: &str, line: &str| -> Vec<String> {
            let text = format!(
                "//! Braces.\n\nfn main() {{\n  let amount = 1\n  let x = 2\n  let _ = {line}\n}}\n"
            );
            let fixture =
                Fixture::package(name, &[("main.cheby", &text)], &["main.cheby"]).with_phase_a();
            fixture
                .offered("main.cheby", &format!("let _ = {line}"), super::provide)
                .into_iter()
                .map(|a| a.title)
                .collect()
        };
        let price = titles_in("brace_price", r#""price {" + amount + "}""#);
        assert_eq!(price, [ESCAPE_BRACE], "{price:?}");
        let quoted = titles_in("brace_quoted", r#""{"x"}""#);
        assert_eq!(quoted, Vec::<String>::new(), "{quoted:?}");
        // Two such values on one line: each escape leaves the other brace's
        // error where it was, and adds none (W11's review, A3).
        let two = titles_in("brace_two", r#""a {" + amount + "} c {" + amount + "}""#);
        assert_eq!(two, [ESCAPE_BRACE, ESCAPE_BRACE], "{two:?}");
        // Beside a shape it does not fix, the one it fixes keeps its fix.
        let mixed = titles_in("brace_mixed", r#""a {" + amount + "}" + "{"x"}""#);
        assert_eq!(mixed, [ESCAPE_BRACE], "{mixed:?}");
        // A statement over 1 KiB is not checked, so its braces get no fix
        // and keep their help lines, which bounds a publish's cost (D-1742).
        let items: Vec<&str> = std::iter::repeat_n(r#""p {" + amount + "}""#, 60).collect();
        let long = format!("[{}]", items.join(", "));
        assert!(long.len() > 1024);
        let long = titles_in("brace_long", &long);
        assert_eq!(long, Vec::<String>::new(), "{long:?}");
    }

    /// D-1694's two rows take their own titles, picked by code and by the
    /// edit's shape; every other help of the same codes keeps its text,
    /// and the help's own words never decide (D-1388).
    #[test]
    fn two_helps_take_their_own_titles() {
        let rest = "fn f(xs: List<Int>) -> Int {\n  case xs { [x, .._] => x, _ => 0 }\n}\n";
        let at = u32::try_from(rest.find("_]").unwrap()).unwrap();
        let after = at.checked_add(1).unwrap();
        let words = "write `..` to ignore the rest of the list, or `..name` to bind it";
        assert_eq!(
            own_title(codes::SYNTAX_ERROR, &help(words, at, after, ""), rest),
            Some((WRITE_REST, true))
        );
        // Any text, the same shape: the same title.
        assert_eq!(
            own_title(
                codes::SYNTAX_ERROR,
                &help("other words", at, after, ""),
                rest
            ),
            Some((WRITE_REST, true))
        );
        // Another E0013 help: a deletion of another text keeps the help's.
        let comma = u32::try_from(rest.find(", _").unwrap()).unwrap();
        let shape = help("remove the comma", comma, comma.checked_add(1).unwrap(), "");
        assert_eq!(own_title(codes::SYNTAX_ERROR, &shape, rest), None);
        // The `_` deleted under another code.
        assert_eq!(
            own_title(
                codes::INVALID_INTERPOLATION,
                &help(words, at, after, ""),
                rest
            ),
            None
        );
        let brace = "fn main() {\n  let s = \"price {\" + \"1\"\n}\n";
        let open = u32::try_from(brace.find("{\"").unwrap()).unwrap();
        let insert = help("a `{` in a string …", open, open, "\\");
        assert_eq!(
            own_title(codes::INVALID_INTERPOLATION, &insert, brace),
            Some((ESCAPE_BRACE, true))
        );
        // E0012's other help with an edit, which removes whitespace.
        let space = help(
            "remove the whitespace (D-115)",
            open,
            open.checked_add(1).unwrap(),
            "",
        );
        assert_eq!(own_title(codes::INVALID_INTERPOLATION, &space, brace), None);
        assert_eq!(own_title(codes::SYNTAX_ERROR, &insert, brace), None);
        // E0073's `assert ` insertion: D-1736's title, not preferred.
        let test = "test \"t\" {\n  1 == 1\n}\n";
        let expr = u32::try_from(test.find("1 ==").unwrap()).unwrap();
        let assert = help(
            "to check the condition, write `assert` before it",
            expr,
            expr,
            "assert ",
        );
        assert_eq!(
            own_title(codes::INVALID_TEST_RESULT, &assert, test),
            Some((ADD_ASSERT, false))
        );
        assert_eq!(own_title(codes::SYNTAX_ERROR, &assert, test), None);
        // Two edits are never one of these rows.
        let mut two = help(words, at, after, "");
        two.edits.push(two.edits[0].clone());
        assert_eq!(own_title(codes::SYNTAX_ERROR, &two, rest), None);
    }
}
