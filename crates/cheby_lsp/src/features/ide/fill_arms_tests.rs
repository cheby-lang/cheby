//! Tests of fill arms (X §6.4, F §8, D-1387, D-1692), in process over
//! real snapshots and reports. Each runs only the front end, so it runs on
//! Windows too (D-1516). [`Fixture`] serves the tests of `stubs` and
//! `near_miss` too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cheby_driver::analysis::{
    FileReport, Globals, Overlay, OverlayFile, PackageKind, PlainDiagnostic, PlainEdit, Session,
    SessionConfig, TextRange, TextSize,
};

use super::{ADD_ARMS, MISSING_ARMS, provide, resolve};
use crate::features::ide::actions::{Action, ActionId, ActionKind, DiagRef, FixCx};
use crate::features::ide::checkout_tests::Checkout;
use crate::features::ide::hover::tests::Temp;
use crate::features::stdlib::StdService;
use crate::features::{Analyzed, CancelToken};

/// A run of a package, or of a std checkout, with its reports, as a
/// provider reads them.
pub(crate) struct Fixture {
    /// The package's directory, removed on drop, or the checkout.
    _dir: Dir,
    root: PathBuf,
    pub(crate) analyzed: Analyzed,
    reports: BTreeMap<PathBuf, Arc<FileReport>>,
    /// The phase-A reports of the run, which [`Self::with_phase_a`] adds.
    phase_a: BTreeMap<PathBuf, Arc<FileReport>>,
    substituted: bool,
    std: StdService,
    cancel: CancelToken,
}

/// What a [`Fixture`] removes on drop.
enum Dir {
    Package(#[expect(dead_code, reason = "held for its drop")] Temp),
    Checkout(#[expect(dead_code, reason = "held for its drop")] Checkout),
}

impl Fixture {
    /// The package `app` with `files` under `src/`, those named in `open`
    /// open with their text.
    pub(crate) fn package(name: &str, files: &[(&str, &str)], open: &[&str]) -> Self {
        let temp = Temp::package(name, files);
        let root = temp.0.clone();
        let opened: Vec<(PathBuf, &str)> = files
            .iter()
            .filter(|(file, _)| open.contains(file))
            .map(|(file, text)| (temp.src(file), *text))
            .collect();
        Self::run(Dir::Package(temp), root, PackageKind::Package, &opened)
    }

    /// A std checkout with `probes` written as `std/<name>` and opened.
    pub(crate) fn checkout(name: &str, probes: &[(&str, &str)]) -> Self {
        let checkout = Checkout::new(name);
        let root = checkout.at("std").parent().unwrap().to_path_buf();
        let mut opened = Vec::new();
        for (file, text) in probes {
            let path = checkout.at(&format!("std/{file}"));
            std::fs::write(&path, text).unwrap();
            opened.push((path, *text));
        }
        let kind = PackageKind::StdCheckout {
            checkout: root.clone(),
        };
        Self::run(Dir::Checkout(checkout), root, kind, &opened)
    }

    fn run(dir: Dir, root: PathBuf, kind: PackageKind, open: &[(PathBuf, &str)]) -> Self {
        let mut session = Session::new(SessionConfig {
            root: root.clone(),
            kind,
            target: None,
            globals: Arc::new(Globals::new()),
        });
        let overlay = Overlay {
            files: open
                .iter()
                .zip(1_u64..)
                .map(|((path, text), stamp)| {
                    let file = OverlayFile {
                        text: Arc::from(*text),
                        version: 1,
                        stamp,
                    };
                    (path.clone(), file)
                })
                .collect(),
        };
        let live = session.analyze_live(&overlay).expect("a run");
        let (reports, substituted) = live.report.map_or_else(Default::default, |report| {
            (report.files.clone(), report.substituted)
        });
        Self {
            _dir: dir,
            root,
            analyzed: Analyzed::new(live.snapshot.expect("a snapshot")),
            reports,
            phase_a: live.phase_a,
            substituted,
            std: StdService::none(),
            cancel: CancelToken::new(),
        }
    }

    /// The fixture with the phase-A report of each file that the check's
    /// report does not hold, as the server publishes a file with a grammar
    /// error.
    pub(crate) fn with_phase_a(mut self) -> Self {
        for (path, report) in std::mem::take(&mut self.phase_a) {
            self.reports.entry(path).or_insert(report);
        }
        self
    }

    /// The path of `file` under `src/`, or under `std/` in a checkout.
    pub(crate) fn path(&self, file: &str) -> PathBuf {
        let dir = if self.analyzed.snapshot.is_std_checkout() {
            "std"
        } else {
            "src"
        };
        file.split('/')
            .fold(self.root.join(dir), |path, part| path.join(part))
    }

    /// The text of `file` in the snapshot.
    pub(crate) fn text(&self, file: &str) -> &str {
        let snapshot = &self.analyzed.snapshot;
        let id = snapshot
            .file_of(&self.path(file))
            .expect("a file of the run");
        snapshot.source(id).unwrap().text()
    }

    /// The published report's diagnostics of `file`.
    pub(crate) fn diagnostics(&self, file: &str) -> &[PlainDiagnostic] {
        self.reports
            .get(&self.path(file))
            .map_or(&[], |report| report.diagnostics.as_slice())
    }

    /// What a provider reads for `file`.
    pub(crate) fn cx(&self, file: &str) -> FixCx<'_> {
        let snapshot = &self.analyzed.snapshot;
        let id = snapshot
            .file_of(&self.path(file))
            .expect("a file of the run");
        FixCx {
            analyzed: &self.analyzed,
            view: snapshot.module_at(id).expect("a module"),
            report: self.reports.get(&self.path(file)).map(|r| &**r),
            substituted: self.substituted,
            context: &[],
            text: self.text(file),
            std: &self.std,
            resolve_edits: true,
            disabled_support: true,
            cancel: &self.cancel,
        }
    }

    /// The range of `needle` in `file`, which must occur once; a `|` in it
    /// marks an empty range there instead.
    pub(crate) fn range(&self, file: &str, needle: &str) -> TextRange {
        let text = self.text(file);
        let plain = needle.replace('|', "");
        assert_eq!(text.matches(&plain).count(), 1, "`{plain}` occurs once");
        let at = text.find(&plain).unwrap();
        let size = |n: usize| TextSize::try_from(n).unwrap();
        match needle.find('|') {
            Some(bar) => TextRange::empty(size(at.checked_add(bar).unwrap())),
            None => TextRange::new(size(at), size(at.checked_add(plain.len()).unwrap())),
        }
    }

    /// What `provide` offers at `needle` of `file`, given the report's
    /// diagnostics that touch it, as the frame gives them.
    pub(crate) fn offered(
        &self,
        file: &str,
        needle: &str,
        provide: crate::features::ide::actions::Provide,
    ) -> Vec<Action> {
        let range = self.range(file, needle);
        let touching: Vec<(usize, &PlainDiagnostic)> = self
            .diagnostics(file)
            .iter()
            .enumerate()
            .filter(|(_, d)| {
                d.range
                    .is_some_and(|at| at.start() <= range.end() && range.start() <= at.end())
            })
            .collect();
        provide(&self.cx(file), range, &touching)
    }

    /// The text of `file` after the edits of it among `edits`.
    pub(crate) fn applied(&self, file: &str, edits: &[PlainEdit]) -> String {
        apply(self.text(file), &self.path(file), edits)
    }
}

/// `text`, the text of `path`, after the edits of it among `edits`.
pub(crate) fn apply(text: &str, path: &Path, edits: &[PlainEdit]) -> String {
    let mut mine: Vec<&PlainEdit> = edits.iter().filter(|e| e.path == path).collect();
    mine.sort_by_key(|e| std::cmp::Reverse(e.range.start()));
    let mut out = text.to_owned();
    for edit in mine {
        let start = usize::try_from(edit.range.start()).unwrap();
        let end = usize::try_from(edit.range.end()).unwrap();
        out.replace_range(start..end, &edit.replacement);
    }
    out
}

/// The titles of `actions`.
pub(crate) fn titles(actions: &[Action]) -> Vec<&str> {
    actions.iter().map(|a| a.title.as_str()).collect()
}

/// The one action titled `title` among `actions`.
pub(crate) fn titled<'a>(actions: &'a [Action], title: &str) -> &'a Action {
    let found: Vec<&Action> = actions.iter().filter(|a| a.title == title).collect();
    assert_eq!(found.len(), 1, "one `{title}` among {:?}", titles(actions));
    found[0]
}

/// The fix's edits: those of the listing, or a resolve's.
fn edits_of(fixture: &Fixture, file: &str, needle: &str, action: &Action) -> Vec<PlainEdit> {
    if let Some(edits) = &action.edit {
        return edits.clone();
    }
    let id = action.data.as_ref().expect("an edit or data");
    resolve(&fixture.cx(file), fixture.range(file, needle), id).expect("the resolve's edits")
}

const SHAPE: &str = "//! Shapes.

pub exposed type Shape {
  Circle { radius: Float },
  Square { side: Float },
  Dot,
}
";

const MAIN: &str = "//! Fill arms.

import app::shape

type Color {
  Red,
  Green,
  Blue,
}

fn name(color: Color) -> String {
  case color {
    Red => \"red\"
  }
}

fn maybe(x: Option<Int>) -> Int {
  case x {
    Some(1) => 1
  }
}

fn nested(x: Option<Option<Color>>) -> Int {
  case x {
    Some(Some(Red)) => 1
    None => 0
  }
}

fn pair(a: Color, b: Bool) -> Int {
  case a, b {
    Red, True => 1
  }
}

fn area(s: shape::Shape) -> Int {
  case s {
    shape::Dot => 0
  }
}

fn main() {
  assert name(Red) == \"red\"
  assert maybe(None) == 0 && nested(None) == 0 && pair(Red, True) == 1
  assert area(shape::Dot) == 0
}
";

const MORE: &str = "//! Many missing values.

import app::make
import app::shape as geo

type Color {
  Red,
  Green,
  Blue,
}

type Mine {
  Some(Int),
}

fn four(a: Color, b: Color, c: Color, d: Color) -> Int {
  case a, b, c, d {
    Red, Red, Red, Red => 1
  }
}

fn aliased(s: geo::Shape) -> Int {
  case s {
    geo::Dot => 0
  }
}

fn through() -> Int {
  case make::make_shape() {
    _ when False => 0
  }
}

fn hidden(x: Option<Int>) -> Int {
  case x { None => 0 }
}

fn main() {
  assert four(Red, Red, Red, Red) == 1 && aliased(geo::Dot) == 0
  assert through() == 0 && hidden(None) == 0 && Some(1) == Some(1)
}
";

const MAKE: &str = "//! Makes a shape.

import app::shape

pub fn make_shape() -> shape::Shape {
  shape::Dot
}
";

/// The quick fix on E0061 writes one arm per missing value after the last
/// arm, at its indentation, for an enum, `Option`, a nested pattern, two
/// subjects and another module's constructors.
#[test]
fn e0061_offers_the_missing_arms() {
    let fixture = Fixture::package(
        "e0061",
        &[("main.cheby", MAIN), ("shape.cheby", SHAPE)],
        &["main.cheby"],
    );
    let cases = [
        (
            "case |color",
            "    Red => \"red\"\n    Green => todo\n    Blue => todo\n",
        ),
        (
            "case |x {\n    Some(1)",
            "    Some(1) => 1\n    Some(_) => todo\n    None => todo\n",
        ),
        (
            "case |x {\n    Some(Some",
            "    None => 0\n    Some(Some(Green)) => todo\n    Some(Some(Blue)) => todo\n    Some(None) => todo\n",
        ),
        (
            "case |a, b",
            "    Red, True => 1\n    Red, False => todo\n    Green, _ => todo\n",
        ),
        (
            "case |s {",
            "    shape::Dot => 0\n    shape::Circle { .. } => todo\n    shape::Square { .. } => todo\n",
        ),
    ];
    let mut wrong = Vec::new();
    for (at, expected) in cases {
        let actions = fixture.offered("main.cheby", at, provide);
        let fix = titled(&actions, MISSING_ARMS);
        assert_eq!(fix.kind, ActionKind::QuickFix);
        assert!(matches!(fix.diagnostic, Some(DiagRef::Report(_))), "{at}");
        assert!(fix.preferred);
        let edits = edits_of(&fixture, "main.cheby", at, fix);
        let after = fixture.applied("main.cheby", &edits);
        if !after.contains(expected) {
            wrong.push(format!("{at}:\n{after}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n---\n"));
}

/// Beyond the rows a fact holds, one `_` arm; a constructor the module
/// cannot name, through an alias it can, one it does not import and one
/// whose prelude name it hides, becomes the `_` arm (D-1678). A `case` on
/// one line gets its rows and its `}` on lines of their own.
#[test]
fn rows_the_module_cannot_name_become_one_wildcard() {
    let fixture = Fixture::package(
        "wild",
        &[
            ("main.cheby", MORE),
            ("shape.cheby", SHAPE),
            ("make.cheby", MAKE),
        ],
        &["main.cheby"],
    );
    let cases = [
        (
            "case |a, b",
            "    Red, Red, Red, Red => 1\n    Red, Red, Red, Green => todo\n    Red, Red, Red, Blue => todo\n    Red, Red, Green, _ => todo\n",
        ),
        (
            "case |s {",
            "    geo::Dot => 0\n    geo::Circle { .. } => todo\n    geo::Square { .. } => todo\n",
        ),
        (
            "case |make::make_shape",
            "    _ when False => 0\n    _ => todo\n  }",
        ),
        ("case |x {", "  case x { None => 0\n    _ => todo\n  }"),
    ];
    let mut wrong = Vec::new();
    for (at, expected) in cases {
        let actions = fixture.offered("main.cheby", at, provide);
        let Some(fix) = actions.iter().find(|a| a.title == MISSING_ARMS) else {
            let codes: Vec<String> = fixture
                .diagnostics("main.cheby")
                .iter()
                .map(|d| format!("{} {:?}", d.code, d.range))
                .collect();
            wrong.push(format!("{at}: nothing offered, {codes:?}"));
            continue;
        };
        let edits = edits_of(&fixture, "main.cheby", at, fix);
        let after = fixture.applied("main.cheby", &edits);
        if !after.contains(expected) {
            wrong.push(format!("{at}:\n{after}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n---\n"));
}

const EMPTY: &str = "//! An empty case.

type Color {
  Red,
  Green,
}

fn name(color: Color) -> Int {
  case color { }
}

fn main() {
  assert name(Red) == 1
}
";

/// An empty `case`, E0013, gets "Add arms", one arm per constructor,
/// from the fact of the tolerant run (F §8), and its edit is resolved.
#[test]
fn an_empty_case_gets_add_arms() {
    let fixture = Fixture::package("empty", &[("main.cheby", EMPTY)], &["main.cheby"]);
    let actions = fixture.offered("main.cheby", "case |color", provide);
    let add = titled(&actions, ADD_ARMS);
    assert_eq!(add.kind, ActionKind::RefactorRewrite);
    assert_eq!(add.data, Some(ActionId::FillArms));
    assert_eq!(add.edit, None);
    let edits = edits_of(&fixture, "main.cheby", "case |color", add);
    let after = fixture.applied("main.cheby", &edits);
    assert!(
        after.contains("  case color {\n    Red => todo\n    Green => todo\n  }\n"),
        "{after}"
    );
}

const BESIDE: &str = "//! Another error in the body.

type Color {
  Red,
  Green,
}

fn name(color: Color) -> Int {
  let wrong: Int = \"no\"
  case color {
    Red => 1
  }
}

fn other(color: Color) -> Int {
  case color {
    Red => 1
  }
}

fn main() {
  assert name(Red) == other(Red)
}
";

/// Beside an E0047 in the body no E0061 is published (D-494), so the fill
/// is the refactor on the `case`, resolved from the fact. Where E0061 is
/// published, the cursor on it gets the quick fix alone, and elsewhere in
/// the `case` the refactor.
#[test]
fn a_case_beside_another_error_gets_the_refactor() {
    let fixture = Fixture::package("beside", &[("main.cheby", BESIDE)], &["main.cheby"]);
    let codes: Vec<String> = fixture
        .diagnostics("main.cheby")
        .iter()
        .filter(|d| d.severity == cheby_driver::analysis::Severity::Error)
        .map(|d| d.code.to_string())
        .collect();
    assert_eq!(codes, ["E0047", "E0061"], "only the other function's E0061");
    let at = "    Red => |1\n  }\n}\n\nfn other";
    let actions = fixture.offered("main.cheby", at, provide);
    let refactor = titled(&actions, MISSING_ARMS);
    assert_eq!(refactor.kind, ActionKind::RefactorRewrite);
    assert_eq!(refactor.diagnostic, None);
    let edits = edits_of(&fixture, "main.cheby", at, refactor);
    let after = fixture.applied("main.cheby", &edits);
    assert!(
        after.contains("    Red => 1\n    Green => todo\n  }\n}\n\nfn other"),
        "{after}"
    );
    // Where E0061 is published: on it the quick fix only, elsewhere the
    // refactor.
    let on = fixture.offered(
        "main.cheby",
        "case |color {\n    Red => 1\n  }\n}\n\nfn main",
        provide,
    );
    assert_eq!(titles(&on), [MISSING_ARMS]);
    assert_eq!(on[0].kind, ActionKind::QuickFix);
    let off = fixture.offered("main.cheby", "Red => |1\n  }\n}\n\nfn main", provide);
    assert_eq!(titles(&off), [MISSING_ARMS]);
    assert_eq!(off[0].kind, ActionKind::RefactorRewrite);
    // A complete `case` offers nothing.
    let clean = Fixture::package(
        "complete",
        &[(
            "main.cheby",
            "//! Complete.\n\nfn f(x: Bool) -> Int {\n  case x {\n    True => 1\n    False => 0\n  }\n}\n\nfn main() {\n  assert f(True) == 1\n}\n",
        )],
        &["main.cheby"],
    );
    assert_eq!(clean.offered("main.cheby", "case |x", provide), []);
}

/// More missing values than a fact holds, 64, give the `_` arm alone
/// (F §8).
#[test]
fn more_rows_than_a_fact_holds_give_one_wildcard() {
    let mut variants = String::new();
    for n in 0..70 {
        variants.push_str("  V");
        variants.push_str(&n.to_string());
        variants.push_str(",\n");
    }
    let text = format!(
        "//! Many variants.\n\ntype Many {{\n{variants}}}\n\nfn pick(m: Many) -> Int {{\n  case m {{\n    V0 => 0\n  }}\n}}\n\nfn main() {{\n  assert pick(V0) == 0\n}}\n"
    );
    let fixture = Fixture::package("many", &[("main.cheby", &text)], &["main.cheby"]);
    let actions = fixture.offered("main.cheby", "case |m", provide);
    let fix = titled(&actions, MISSING_ARMS);
    let after = fixture.applied(
        "main.cheby",
        &edits_of(&fixture, "main.cheby", "case |m", fix),
    );
    assert!(after.contains("    V0 => 0\n    _ => todo\n  }"), "{after}");
}
