//! The mutation sweep (A §2.8, plan W4): realistic edits of every example
//! and of a sample of the UI tests and std, each run through a session as
//! the server runs it, [`Session::analyze_live`] and then
//! [`Session::analyze_strict`] when the live run plans one.
//!
//! The edits are the ten body edits of F §0.1 (`dot`, `typo`, `open_call`,
//! `drop_arg`, `half_let`, `pipe_end`, `new_dot`, `open_case`, `mismatch`,
//! `drop_brace`) and the declaration edits of review-code's `mutprobe`:
//! return type, parameter type, parameter list, generics, bound, body and
//! name of a function; name, body and generics of a type; a broken
//! variant; a field type; an open import; a broken interface, constant or
//! attribute.
//!
//! Most edits run on one warm session per package, which keeps last good
//! texts and a strict key as an editor session does. Some run with the
//! strict key forgotten, so that a D run reads last good text. Some run on
//! a cold session, where an open broken document takes its clean disk text
//! as its last good text (D-1356). The same ones also run on a copy of the
//! package whose disk holds the edit, closed, so that the broken file is a
//! stub and its importers are held (D-1357).
//!
//! A run must give no `Err`, no failed run and no panic, and every
//! published diagnostic must lie in its own file: its range in the
//! report's text, which is the live text of its path, and each label and
//! edit in the live text of a package file (A §2.3). A label or an edit of
//! a D run must not point into a broken file, whose text in the run is not
//! its live text. A diagnostic with no range belongs to `cheby.toml`.
//!
//! `sweep_sample` takes twelve edits per package and runs by default.
//! `sweep_full` takes up to six of each kind per package and is ignored,
//! for a manual run with `--ignored --nocapture`, which prints its numbers.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cheby_diag::TextRange;
use cheby_iface::Globals;
use cheby_package::MANIFEST_FILE;
use cheby_syntax::{SyntaxKind as K, SyntaxNode, SyntaxTree};

use super::{
    FileLocation, FileReport, Overlay, OverlayFile, PackageKind, Res, ResolvedModule, Session,
    SessionConfig,
};

const UI_MANIFEST: &str = "name = \"ui_test\"\nversion = \"0.1.0\"\n";
const STD_MANIFEST: &str = "name = \"app\"\nversion = \"0.1.0\"\n";
const STD_MAIN: &str = "//! Root.\n\nfn main() {\n  Nil\n}\n";

/// The analysis thread's stack in the server (A §3.1). It only reserves
/// address space.
const STACK: usize = 256 << 20;

/// A package in a fresh temporary directory, removed on drop.
struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A package to sweep.
struct Swept {
    /// A short name for messages.
    name: String,
    root: PathBuf,
    _temp: Option<Temp>,
}

/// One edit of one file.
struct Mutant {
    kind: &'static str,
    path: PathBuf,
    text: String,
}

/// How much a sweep covers.
#[derive(Clone, Copy)]
struct Size {
    /// Edits of each kind per package, at most.
    per_kind: usize,
    /// Edits per package, at most.
    per_package: usize,
    /// Every `ui_step`-th UI test.
    ui_step: usize,
    /// Every `std_step`-th std file.
    std_step: usize,
    /// Every `cold_step`-th edit also runs on a cold session.
    cold_step: usize,
    /// Every `forget_step`-th edit forgets the strict key first.
    forget_step: usize,
    /// Packages swept at once.
    threads: usize,
}

/// What a sweep saw.
#[derive(Default)]
struct Tally {
    packages: usize,
    live_runs: usize,
    strict_runs: usize,
    broken_runs: usize,
    /// D runs with a stub.
    stub_runs: usize,
    /// D runs that held a file.
    held_runs: usize,
    diagnostics: usize,
    ranges: usize,
    /// Edits per kind.
    by_kind: BTreeMap<&'static str, usize>,
    failures: Vec<String>,
}

fn repo() -> PathBuf {
    std::path::absolute(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

/// The number of the next temporary package, so that sweeps that run at
/// once in one process never share a directory.
static TEMPS: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = TEMPS.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("cheby_sweep_{}_{n}_{tag}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::path::absolute(dir).unwrap()
}

/// Every example, every `ui_step`-th UI test and every `std_step`-th std
/// file, each as a package. A std file becomes a module of a package of
/// its own, beside a trivial root.
fn packages(size: Size) -> Vec<Swept> {
    let repo = repo();
    let mut out = Vec::new();
    let mut examples: Vec<PathBuf> = std::fs::read_dir(repo.join("docs/examples"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join(MANIFEST_FILE).is_file())
        .collect();
    examples.sort();
    for root in examples {
        out.push(Swept {
            name: root.file_name().unwrap().to_string_lossy().into_owned(),
            root: std::path::absolute(root).unwrap(),
            _temp: None,
        });
    }
    let mut ui = Vec::new();
    walk(&repo.join("tests/ui"), &mut ui, true);
    for (n, case) in ui.iter().enumerate().step_by(size.ui_step.max(1)) {
        let name = case
            .strip_prefix(repo.join("tests/ui"))
            .unwrap()
            .display()
            .to_string();
        if case.is_dir() {
            out.push(Swept {
                name,
                root: std::path::absolute(case).unwrap(),
                _temp: None,
            });
        } else {
            let dir = temp_dir(&format!("ui_{n}"));
            std::fs::write(dir.join(MANIFEST_FILE), UI_MANIFEST).unwrap();
            std::fs::copy(case, dir.join("src/main.cheby")).unwrap();
            out.push(Swept {
                name,
                root: dir.clone(),
                _temp: Some(Temp(dir)),
            });
        }
    }
    let mut std_files = Vec::new();
    walk(&repo.join("std"), &mut std_files, false);
    for (n, file) in std_files.iter().enumerate().step_by(size.std_step.max(1)) {
        let dir = temp_dir(&format!("std_{n}"));
        std::fs::write(dir.join(MANIFEST_FILE), STD_MANIFEST).unwrap();
        std::fs::write(dir.join("src/main.cheby"), STD_MAIN).unwrap();
        let stem = file.file_stem().unwrap().to_string_lossy().into_owned();
        std::fs::copy(file, dir.join(format!("src/{stem}.cheby"))).unwrap();
        out.push(Swept {
            name: file.strip_prefix(&repo).unwrap().display().to_string(),
            root: dir.clone(),
            _temp: Some(Temp(dir)),
        });
    }
    out
}

/// The `.cheby` files under `dir`, sorted; with `packages`, a directory
/// with a manifest stands for itself.
fn walk(dir: &Path, out: &mut Vec<PathBuf>, packages: bool) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if packages && path.join(MANIFEST_FILE).is_file() {
                out.push(path);
            } else {
                walk(&path, out, packages);
            }
        } else if path.extension().is_some_and(|e| e == "cheby") {
            out.push(path);
        }
    }
}

/// `text` with `start..end` replaced by `new`.
fn splice(text: &str, start: usize, end: usize, new: &str) -> Option<String> {
    Some(format!("{}{new}{}", text.get(..start)?, text.get(end..)?))
}

fn start_of(node: SyntaxNode<'_>) -> usize {
    usize::try_from(node.text_range().start()).unwrap_or(0)
}

fn end_of(node: SyntaxNode<'_>) -> usize {
    usize::try_from(node.text_range().end()).unwrap_or(0)
}

/// F §0.1's ten body edits of every body of `resolved`, at the sites the
/// prototype's probe chose.
fn body_mutants(tree: &SyntaxTree, resolved: &ResolvedModule) -> Vec<(&'static str, String)> {
    let text = tree.text();
    let mut out = Vec::new();
    let mut put = |kind, start, end, new: &str| {
        if let Some(m) = splice(text, start, end, new) {
            out.push((kind, m));
        }
    };
    let mut bodies: Vec<_> = resolved.bodies.iter().collect();
    bodies.sort_by_key(|(index, _)| **index);
    for (&index, res) in bodies {
        let Some(item) = resolved.item(index) else {
            continue;
        };
        let Some(node) = tree.node(item.node) else {
            continue;
        };
        for n in node.descendants() {
            let (ns, ne) = (start_of(n), end_of(n));
            match n.kind() {
                K::PATH_EXPR => {
                    let res = n
                        .first_child_of_kind(K::PATH)
                        .and_then(|p| res.paths.get(&p.id()).copied());
                    let is_callee = n.parent().is_some_and(|p| {
                        p.kind() == K::CALL_EXPR
                            && p.first_child().map(SyntaxNode::id) == Some(n.id())
                    });
                    if matches!(res, Some(Res::Local(_))) && !is_callee {
                        put("dot", ne, ne, ".");
                    }
                    if matches!(res, Some(Res::Local(_) | Res::Def(_))) {
                        put("typo", ne, ne, "z");
                    }
                }
                K::CALL_EXPR => {
                    let args: Vec<_> = n
                        .first_child_of_kind(K::ARG_LIST)
                        .map(|list| list.children().collect())
                        .unwrap_or_default();
                    if let Some(&first) = args.first() {
                        put("open_call", end_of(first), ne, ", ");
                    }
                    if let [.., prev, last] = args.as_slice() {
                        put("drop_arg", end_of(*prev), end_of(*last), "");
                    }
                }
                K::LET_STMT => {
                    put("half_let", ns, ns, "let probe_y = \n");
                    if let Some(value) = n.children().filter(|c| c.kind() != K::IDENT_PAT).last() {
                        let ve = end_of(value);
                        put("pipe_end", ve, ve, " |> ");
                    }
                    if let Some(pat) = n.first_child_of_kind(K::IDENT_PAT) {
                        let name = pat.text().trim();
                        put("new_dot", ne, ne, &format!("\n  {name}."));
                        put("open_case", ne, ne, &format!("\n  case {name} {{"));
                    }
                }
                K::LITERAL => {
                    let digits = n.text().trim();
                    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                        put("mismatch", ns, ne, "\"s\"");
                    }
                }
                K::BLOCK => {
                    let is_body = n.parent().map(SyntaxNode::id) == Some(node.id());
                    if !is_body && n.text().ends_with('}') {
                        put("drop_brace", ne.saturating_sub(1), ne, "");
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// `mutprobe`'s declaration edits of `text`, line by line (review-code,
/// "Declaration-level mutants").
fn decl_mutants(text: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let mut off = 0_usize;
    for line in text.split_inclusive('\n') {
        let start = off;
        off = off.saturating_add(line.len());
        for (kind, new) in line_edits(line) {
            if let Some(m) = new.and_then(|new| splice(text, start, off, &new)) {
                out.push((kind, m));
            }
        }
    }
    out
}

/// `mutprobe`'s replacements of one line, by kind; `None` where the line
/// does not split as the edit needs.
fn line_edits(line: &str) -> Vec<(&'static str, Option<String>)> {
    let mut out = Vec::new();
    let mut put = |kind: &'static str, new: Option<String>| out.push((kind, new));
    let t = line.trim_end();
    let col0 = !line.starts_with(' ');
    if col0 && (t.starts_with("fn ") || t.starts_with("pub fn ")) {
        fn_edits(t, &mut put);
    }
    if col0 && t.contains("type ") && t.ends_with('{') {
        put("type_name_gone", Some("pub type {\n".to_owned()));
        put(
            "type_open",
            Some(format!("{}\n", t.trim_end_matches('{').trim_end())),
        );
        if let Some(i) = t.find('<') {
            put("type_generics_open", t.get(..=i).map(|h| format!("{h}\n")));
        }
    }
    let first = t.trim_start().chars().next();
    if !col0
        && first.is_some_and(char::is_uppercase)
        && (t.ends_with('{') || t.ends_with("),") || t.ends_with(','))
    {
        let name = t.split(['(', '{']).next().unwrap_or_default().trim_end();
        put("variant_broken", Some(format!("{name}(\n")));
    }
    if !col0 && t.contains(": ") && t.ends_with(',') && !t.contains("=>") && !t.contains('(') {
        let name = t.find(':').and_then(|i| t.get(..i));
        put("field_ty_gone", name.map(|n| format!("{n}:\n")));
    }
    if col0 && t.starts_with("import ") {
        put("import_open", Some(format!("{t}::\n")));
        if let Some(i) = t.find('{') {
            put("import_items_open", t.get(..=i).map(|h| format!("{h}\n")));
        }
    }
    if col0 && (t.starts_with("interface ") || t.starts_with("pub interface ")) {
        put(
            "iface_open",
            Some(format!("{}\n", t.trim_end_matches('{').trim_end())),
        );
        put("iface_name_gone", Some("pub interface {\n".to_owned()));
    }
    if col0
        && (t.starts_with("const ") || t.starts_with("pub const "))
        && let Some(j) = t.find(" = ")
    {
        if let Some(i) = t.find(": ") {
            let new = t.get(..i).zip(t.get(j.saturating_add(1)..));
            put("const_ty_gone", new.map(|(h, r)| format!("{h}: {r}\n")));
        }
        put("const_value_gone", t.get(..j).map(|h| format!("{h} =\n")));
        let head = t.split(':').next().unwrap_or_default();
        put("const_no_ty", t.get(j..).map(|r| format!("{head}{r}\n")));
    }
    if col0 && t.starts_with('@') {
        let head = t.split('(').next().unwrap_or_default();
        put("attr_open", Some(format!("{head}(\n")));
        put("attr_gone", Some("@\n".to_owned()));
    }
    out
}

/// `mutprobe`'s edits of a function's first line `t`.
fn fn_edits(t: &str, put: &mut impl FnMut(&'static str, Option<String>)) {
    let open = t.find('(');
    if let Some(i) = t.rfind("-> ") {
        put("ret_gone", t.get(..i).map(|h| format!("{h}-> {{\n")));
    }
    if let Some(i) = t.find(": ") {
        let rest = t.get(i.saturating_add(2)..).unwrap_or_default();
        if let Some(j) = rest.find([',', ')']) {
            let new = t.get(..i).zip(rest.get(j..));
            put("param_ty_gone", new.map(|(h, r)| format!("{h}: {r}\n")));
        }
        if t.contains('<') && i < open.unwrap_or(0) {
            let after = t.find('>').unwrap_or(i).saturating_add(1);
            let new = t.get(..i).zip(t.get(after..));
            put("bound_gone", new.map(|(h, r)| format!("{h}: >{r}\n")));
        }
    }
    if let Some(i) = open {
        put("params_open", t.get(..=i).map(|h| format!("{h}\n")));
    }
    if t.ends_with('{') {
        put(
            "no_body",
            Some(format!("{}\n", t.trim_end_matches('{').trim_end())),
        );
    }
    if let Some(i) = t.find('<') {
        put("generics_open", t.get(..=i).map(|h| format!("{h}\n")));
    }
    let head = if t.starts_with("pub") {
        "pub fn "
    } else {
        "fn "
    };
    let rest = t.get(open.unwrap_or(0)..);
    put("name_gone", rest.map(|r| format!("{head}{r}\n")));
}

/// A seeded xorshift, so that a sweep picks the same edits every time.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        let n = u64::try_from(n).unwrap_or(u64::MAX).max(1);
        usize::try_from(self.0.checked_rem(n).unwrap_or(0)).unwrap_or(0)
    }
}

/// Up to `per_kind` edits of each kind, picked by `rng`, then at most
/// `per_package` of them: kinds in name order, turned by `turn` so that
/// packages in a row try different kinds.
fn pick(all: Vec<Mutant>, size: Size, turn: usize, rng: &mut Rng) -> Vec<Mutant> {
    let mut by_kind: BTreeMap<&'static str, Vec<Mutant>> = BTreeMap::new();
    for m in all {
        by_kind.entry(m.kind).or_default().push(m);
    }
    let mut kinds: Vec<Vec<Mutant>> = by_kind.into_values().collect();
    if let Some(n) = turn.checked_rem(kinds.len()) {
        kinds.rotate_left(n);
    }
    let mut out = Vec::new();
    for mut ms in kinds {
        for i in (1..ms.len()).rev() {
            ms.swap(i, rng.below(i.saturating_add(1)));
        }
        ms.truncate(size.per_kind);
        out.extend(ms);
    }
    out.truncate(size.per_package);
    out
}

/// Every edit of every module file of the package at `root`, from its
/// clean run, or a failure.
fn mutants(session: &mut Session, root: &Path) -> Result<Vec<Mutant>, String> {
    let live = session
        .analyze_live(&Overlay::default())
        .map_err(|e| format!("clean run: Err {}", e.message()))?;
    if let Some(failed) = live.failed {
        return Err(format!("clean run failed: {failed}"));
    }
    if let Some(plan) = live.plan {
        session
            .analyze_strict(plan)
            .map_err(|e| format!("clean D run: Err {}", e.message()))?;
    }
    let snapshot = live.snapshot.ok_or("clean run: no snapshot")?;
    let mut out = Vec::new();
    for module in snapshot.local_modules() {
        let Some(file) = module.file() else { continue };
        let Some(FileLocation::Package(path)) = snapshot.location(file) else {
            continue;
        };
        if !path.starts_with(root) {
            continue;
        }
        let Some(source) = snapshot.source(file) else {
            continue;
        };
        let mut edits = decl_mutants(source.text());
        if let (Some(tree), Some(resolved)) = (module.tree(), module.resolved()) {
            edits.extend(body_mutants(tree, resolved));
        }
        out.extend(edits.into_iter().map(|(kind, text)| Mutant {
            kind,
            path: path.clone(),
            text,
        }));
    }
    Ok(out)
}

/// The live text of a path: the overlay's, else disk's.
struct Texts<'a> {
    overlay: &'a Overlay,
    disk: BTreeMap<PathBuf, Option<String>>,
}

impl Texts<'_> {
    fn of(&mut self, path: &Path) -> Option<&str> {
        if let Some(open) = self.overlay.files.get(path) {
            return Some(&open.text);
        }
        self.disk
            .entry(path.to_path_buf())
            .or_insert_with(|| std::fs::read_to_string(path).ok())
            .as_deref()
    }
}

/// Checks that every diagnostic of `files` lies in its own file (module
/// doc), and records a failure for each that does not.
fn check_published(
    root: &Path,
    files: &BTreeMap<PathBuf, Arc<FileReport>>,
    texts: &mut Texts<'_>,
    substituted: &BTreeSet<PathBuf>,
    what: &str,
    tally: &mut Tally,
) {
    let mut wrong = Vec::new();
    for (path, report) in files {
        if !path.starts_with(root) {
            wrong.push(format!("{what}: a report for {}", path.display()));
            continue;
        }
        if texts.of(path) != Some(&*report.text) {
            wrong.push(format!("{what}: {} holds no live text", path.display()));
            continue;
        }
        let is_manifest = path.file_name().is_some_and(|n| n == MANIFEST_FILE);
        for diag in &report.diagnostics {
            tally.diagnostics = tally.diagnostics.saturating_add(1);
            let label = format!(
                "{what}: {} {} in {}",
                diag.code,
                diag.message,
                path.display()
            );
            match diag.range {
                Some(range) if range.slice(&report.text).is_none() => {
                    wrong.push(format!("{label}: {range:?} out of its text"));
                }
                Some(_) => tally.ranges = tally.ranges.saturating_add(1),
                None if !is_manifest => wrong.push(format!("{label}: no range")),
                None => {}
            }
            let placed: Vec<(&Path, TextRange)> = diag
                .related
                .iter()
                .map(|l| (l.path.as_path(), l.range))
                .chain(
                    diag.helps
                        .iter()
                        .flat_map(|h| &h.edits)
                        .map(|e| (e.path.as_path(), e.range)),
                )
                .collect();
            for (at, range) in placed {
                if substituted.contains(at) {
                    wrong.push(format!(
                        "{label}: {range:?} in {}, which the run read substituted",
                        at.display()
                    ));
                    continue;
                }
                let inside = at.starts_with(root)
                    && texts.of(at).is_some_and(|text| range.slice(text).is_some());
                if inside {
                    tally.ranges = tally.ranges.saturating_add(1);
                } else {
                    wrong.push(format!("{label}: {range:?} out of {}", at.display()));
                }
            }
        }
    }
    tally.failures.extend(wrong);
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default()
}

/// One edit through `session`: the live run, then the D run it plans.
fn step(session: &mut Session, root: &Path, overlay: &Overlay, what: &str, tally: &mut Tally) {
    let mut texts = Texts {
        overlay,
        disk: BTreeMap::new(),
    };
    tally.live_runs = tally.live_runs.saturating_add(1);
    let live = match catch_unwind(AssertUnwindSafe(|| session.analyze_live(overlay))) {
        Ok(Ok(live)) => live,
        Ok(Err(e)) => {
            tally.failures.push(format!("{what}: Err {}", e.message()));
            return;
        }
        Err(payload) => {
            let message = panic_text(payload.as_ref());
            tally.failures.push(format!("{what}: panic {message}"));
            return;
        }
    };
    if let Some(failed) = &live.failed {
        tally.failures.push(format!("{what}: failed {failed}"));
    }
    if live.report.is_none() {
        tally.broken_runs = tally.broken_runs.saturating_add(1);
    }
    let none = BTreeSet::new();
    check_published(root, &live.phase_a, &mut texts, &none, what, tally);
    if let Some(report) = &live.report {
        check_published(root, &report.files, &mut texts, &none, what, tally);
    }
    let Some(plan) = live.plan else { return };
    tally.strict_runs = tally.strict_runs.saturating_add(1);
    if plan.stubs() > 0 {
        tally.stub_runs = tally.stub_runs.saturating_add(1);
    }
    // The broken files: the D run reads their last good text or a stub.
    let broken: BTreeSet<PathBuf> = live
        .phase_a
        .keys()
        .filter(|path| path.file_name().is_none_or(|n| n != MANIFEST_FILE))
        .cloned()
        .collect();
    let what = format!("{what} (D run)");
    match catch_unwind(AssertUnwindSafe(|| session.analyze_strict(plan))) {
        Ok(Ok(report)) => {
            if !report.held.is_empty() {
                tally.held_runs = tally.held_runs.saturating_add(1);
            }
            check_published(root, &report.files, &mut texts, &broken, &what, tally);
        }
        Ok(Err(e)) => tally.failures.push(format!("{what}: Err {}", e.message())),
        Err(payload) => {
            let message = panic_text(payload.as_ref());
            tally.failures.push(format!("{what}: panic {message}"));
        }
    }
}

fn new_session(root: &Path, globals: &Arc<Globals>) -> Session {
    Session::new(SessionConfig {
        root: root.to_path_buf(),
        kind: PackageKind::Package,
        target: None,
        globals: Arc::clone(globals),
    })
}

/// Sweeps one package: its clean run, then each picked edit.
fn sweep_package(
    swept: &Swept,
    size: Size,
    globals: &Arc<Globals>,
    turn: usize,
    tally: &mut Tally,
) {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ u64::try_from(turn).unwrap_or(0));
    tally.packages = tally.packages.saturating_add(1);
    let root = &swept.root;
    let mut warm = new_session(root, globals);
    let all = match mutants(&mut warm, root) {
        Ok(all) => all,
        Err(message) => {
            tally.failures.push(format!("{}: {message}", swept.name));
            return;
        }
    };
    for (n, m) in pick(all, size, turn.saturating_mul(size.per_package), &mut rng)
        .into_iter()
        .enumerate()
    {
        let count = tally.by_kind.entry(m.kind).or_default();
        *count = count.saturating_add(1);
        let stamp = u64::try_from(n).unwrap_or(u64::MAX).saturating_add(1);
        let overlay = Overlay {
            files: BTreeMap::from([(
                m.path.clone(),
                OverlayFile {
                    text: Arc::from(m.text.as_str()),
                    version: i32::try_from(stamp).unwrap_or(i32::MAX),
                    stamp,
                },
            )]),
        };
        let file = m
            .path
            .strip_prefix(root)
            .unwrap_or(&m.path)
            .display()
            .to_string();
        let what = format!("{} {file} {}#{n}", swept.name, m.kind);
        if n.checked_rem(size.forget_step) == Some(0) {
            warm.forget_strict_key();
        }
        step(&mut warm, root, &overlay, &what, tally);
        if n.checked_rem(size.cold_step) == Some(0) {
            let mut cold = new_session(root, globals);
            step(&mut cold, root, &overlay, &format!("{what} (cold)"), tally);
            let (copy, temp) = copy_with(root, &m);
            let mut stub = new_session(&copy, globals);
            let empty = Overlay::default();
            step(
                &mut stub,
                &copy,
                &empty,
                &format!("{what} (on disk)"),
                tally,
            );
            drop(temp);
        }
    }
}

/// A copy of the package at `root`, its manifest and `src/`, with the
/// edit `m` on disk in place of its file.
fn copy_with(root: &Path, m: &Mutant) -> (PathBuf, Temp) {
    let dir = temp_dir("on_disk");
    std::fs::copy(root.join(MANIFEST_FILE), dir.join(MANIFEST_FILE)).unwrap();
    copy_tree(
        &root.join(cheby_package::SOURCE_DIR),
        &dir.join(cheby_package::SOURCE_DIR),
    );
    let at = dir.join(m.path.strip_prefix(root).unwrap());
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    std::fs::write(at, &m.text).unwrap();
    (dir.clone(), Temp(dir))
}

/// Copies every file under `from` to the same place under `to`.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_tree(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

/// Runs a sweep, `size.threads` packages at a time on threads with the
/// server's stack and one shared `Globals`, as the server shares it (A
/// §1.6), and returns its numbers, merged in package order.
fn sweep(size: Size) -> Tally {
    let packages = packages(size);
    let globals = Arc::new(Globals::new());
    let next = AtomicUsize::new(0);
    let mut parts: Vec<(usize, Tally)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..size.threads.max(1))
            .map(|_| {
                std::thread::Builder::new()
                    .stack_size(STACK)
                    .spawn_scoped(scope, || {
                        let mut done = Vec::new();
                        loop {
                            let n = next.fetch_add(1, Ordering::Relaxed);
                            let Some(swept) = packages.get(n) else { break };
                            let mut tally = Tally::default();
                            sweep_package(swept, size, &globals, n, &mut tally);
                            done.push((n, tally));
                        }
                        done
                    })
                    .unwrap()
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect()
    });
    parts.sort_by_key(|(n, _)| *n);
    let mut all = Tally::default();
    for (_, part) in parts {
        all.packages = all.packages.saturating_add(part.packages);
        all.live_runs = all.live_runs.saturating_add(part.live_runs);
        all.strict_runs = all.strict_runs.saturating_add(part.strict_runs);
        all.broken_runs = all.broken_runs.saturating_add(part.broken_runs);
        all.stub_runs = all.stub_runs.saturating_add(part.stub_runs);
        all.held_runs = all.held_runs.saturating_add(part.held_runs);
        all.diagnostics = all.diagnostics.saturating_add(part.diagnostics);
        all.ranges = all.ranges.saturating_add(part.ranges);
        for (kind, n) in part.by_kind {
            let count = all.by_kind.entry(kind).or_default();
            *count = count.saturating_add(n);
        }
        all.failures.extend(part.failures);
    }
    all
}

/// The numbers of a sweep, and its failures, for the test output.
fn summary(tally: &Tally) -> String {
    let mut out = format!(
        "{} packages, {} live runs ({} not the check), {} D runs ({} with a stub, {} holding), \
         {} diagnostics, {} ranges, {} failures\n",
        tally.packages,
        tally.live_runs,
        tally.broken_runs,
        tally.strict_runs,
        tally.stub_runs,
        tally.held_runs,
        tally.diagnostics,
        tally.ranges,
        tally.failures.len()
    );
    for (kind, n) in &tally.by_kind {
        let _ = writeln!(out, "  {kind:<20} {n:>6}");
    }
    for failure in tally.failures.iter().take(40) {
        let _ = writeln!(out, "FAIL {failure}");
    }
    out
}

/// Checks a sweep's result: no failure, D runs and broken runs, and every
/// edit kind tried, or with `!every` most of them.
fn assert_swept(tally: &Tally, every: bool) {
    let text = summary(tally);
    eprintln!("{text}");
    assert!(tally.failures.is_empty(), "{text}");
    assert!(tally.strict_runs > 0, "{text}");
    assert!(tally.broken_runs > 0, "{text}");
    assert!(tally.stub_runs > 0, "{text}");
    assert!(tally.held_runs > 0, "{text}");
    if !every {
        assert!(tally.by_kind.len() >= 25, "{text}");
        return;
    }
    for kind in [
        "dot",
        "typo",
        "open_call",
        "drop_arg",
        "half_let",
        "pipe_end",
        "new_dot",
        "open_case",
        "mismatch",
        "drop_brace",
        "ret_gone",
        "param_ty_gone",
        "params_open",
        "no_body",
        "generics_open",
        "name_gone",
        "type_name_gone",
        "type_open",
        "variant_broken",
        "field_ty_gone",
        "import_open",
        "iface_open",
        "const_value_gone",
        "attr_open",
    ] {
        assert!(tally.by_kind.contains_key(kind), "no `{kind}` edit\n{text}");
    }
}

/// A §2.8: twelve edits per package, of kinds that turn from package to
/// package, over every example and a sample of the UI tests and std.
#[test]
fn sweep_sample() {
    let tally = sweep(Size {
        per_kind: 1,
        per_package: 12,
        ui_step: 40,
        std_step: 12,
        cold_step: 6,
        forget_step: 4,
        threads: 3,
    });
    assert_swept(&tally, false);
}

/// The full sweep: up to six edits of each kind per package, over every
/// example, every fourth UI test and every std file.
#[test]
#[ignore = "the full mutation sweep takes long in a debug build: run it with --ignored"]
fn sweep_full() {
    let tally = sweep(Size {
        per_kind: 6,
        per_package: usize::MAX,
        ui_step: 4,
        std_step: 1,
        cold_step: 5,
        forget_step: 3,
        threads: 4,
    });
    assert_swept(&tally, true);
}
