//! `cargo xtask bench-compile --oqp11` and `--restarts` (K7): OQ-P11's
//! edit-to-run rounds (D-974) and the three causes that start a warm
//! command again (D-1256, the review's RB-3). Reported, never enforced: the
//! budget (D-247's 500 ms) and the three fallback gates are CP3's inputs,
//! which the user decides on (D-974); `.github/scripts/bench_oqp11.py`
//! renders them. On x86-64 they run in `.github/workflows/bench.yml`'s two
//! steps that only `-f compile=true` turns on, each with
//! `continue-on-error` and its own timeout, so they never fail the stop
//! line's job (D-1035, D-1255, D-1281).
//!
//! - `--oqp11`: for `bench/gen`'s `wide` and `deep` projects of `--lines`
//!   lines (1M by default) with seed 1, generated or copied from `--from
//!   DIR/<shape>-<lines>`, each on a fresh `CHEBY_CACHE`: one cold `cheby
//!   run`, timed and reported; then `--rounds` rounds (5) of an edit to one
//!   function body in a module `main` imports (`bench.rs`'s
//!   `edit_one_body`) followed by `cheby run`, and as many rounds of an edit
//!   to `main`'s own body ([`main_edit`], D-1278). Each reading
//!   is the median of its rounds' whole-command times. Every run writes
//!   `CHEBY_PHASES`, and each phase's median sits beside the reading. Every
//!   run's output must equal the cold run's.
//! - `--restarts`: a warm `cheby run` after each of RB-3's causes, on `wide`
//!   and `deep` with seed 1, at 100k lines (`--rounds` rounds) and at
//!   `--lines` when it is larger (at most 3 rounds at 1M): an edit only a
//!   reader's lowering reads (`reader_lowering`, cache-hit test 6's shape),
//!   a body edit that changes a package-visible function's borrow mask
//!   (`mask`, D-812, cache-hit test 2's shape), and a change of settings
//!   (`settings`: `CHEBY_POSITIONS=off` after a run with positions on,
//!   D-1173, D-958); beside an unedited warm run (`unedited`) and a
//!   one-body edit (`one_body`) as references; and one sample row of `mask`
//!   under `cheby test`. The edit sites live in a fixture module the tool
//!   adds to its own copy of each project ([`add_fixture`],
//!   D-1279); `bench/gen`'s output never changes. Every
//!   measured round reaches a state never seen before (a fresh field name
//!   or a further mask each round; for `settings` a fresh cache per round,
//!   filled by one untimed run with positions on), and each cause starts
//!   from its own fresh cache filled by an untimed run. Every measured run
//!   of a cause must write `restart` in its phases file, and the cause's
//!   untimed logged run must parse every package module again, or the tool
//!   fails: no median mixes runs that started again with runs that did not.
//!   `CHEBY_CACHE_LOG` is never set on a timed run; the `parse` and
//!   `parse.std` counts (D-1135, D-1263) come from one more untimed run per
//!   cause, on a fresh state of its own.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use cheby_bench_gen::{Config, Shape};
use serde_json::{Value, json};

/// D-247's edit-to-run budget, which D-974 applies to both shapes.
pub(crate) const BUDGET: Duration = Duration::from_millis(500);

/// The shapes D-974 reads.
const SHAPES: [Shape; 2] = [Shape::Wide, Shape::Deep];

/// The size every restart cause is read at besides `--lines` (D-1256's
/// reading is at 100k).
const RESTART_LINES: usize = 100_000;

/// The rounds of a restart cause at 1M lines or more.
const RESTART_ROUNDS_1M: usize = 3;

/// The rounds of the `cheby test` sample row.
const TEST_ROUNDS: usize = 3;

/// The parameters of the fixture's `check`: each `mask` round owns one
/// more, so a cause has at most this many rounds, its logged run included.
const MASK_PARAMS: usize = 16;

/// What every run of the rounds shares.
#[derive(Debug)]
pub(crate) struct Cx<'a> {
    /// The `cheby` timed.
    pub cheby: &'a Path,
    /// `--env`: set for every child.
    pub env: &'a [(String, String)],
    /// Where projects, phases files and logs go.
    pub work: PathBuf,
    /// `--lines`.
    pub lines: usize,
    /// `--rounds`.
    pub rounds: usize,
    /// `--from`: pre-generated projects, `<dir>/<shape>-<lines>`.
    pub from: Option<&'a Path>,
}

/// An environment change of one child: a value, or `None` to remove it.
type Extra<'a> = (&'a str, Option<&'a str>);

/// One `cheby` command's wall time, standard output and phases.
#[derive(Clone, Debug, Default)]
pub(crate) struct Ran {
    pub time: Duration,
    pub stdout: String,
    /// `CHEBY_PHASES`'s phase lines by name (`command` left out); empty
    /// for a run without the variable.
    pub phases: BTreeMap<String, u64>,
    /// `CHEBY_CACHE_LOG`'s text; empty for a run without the variable.
    pub log: String,
}

impl Ran {
    /// Whether the run started again (its phases file has `restart`).
    pub(crate) fn restarted(&self) -> bool {
        self.phases.get("restart").is_some_and(|&n| n > 0)
    }
}

/// Runs `cheby <args>` in `dir` on the cache `cache`, with `--env` and
/// `extra` applied, timed. With `phases`, `CHEBY_PHASES` names that file
/// and its phases are read; with `log`, `CHEBY_CACHE_LOG` names that file
/// (never on a timed reading) and its text is read. The command must
/// succeed.
pub(crate) fn run_cheby(
    cx: &Cx<'_>,
    dir: &Path,
    cache: &Path,
    args: &[&str],
    extra: &[Extra<'_>],
    files: (Option<&Path>, Option<&Path>),
) -> Result<Ran, String> {
    let (phases, log) = files;
    let mut cmd = Command::new(cx.cheby);
    for (key, value) in cx.env {
        cmd.env(key, value);
    }
    cmd.env("CHEBY_CACHE", cache)
        .env_remove("CHEBY_CACHE_LOG")
        .env_remove("CHEBY_PHASES");
    for &(key, value) in extra {
        match value {
            Some(value) => cmd.env(key, value),
            None => cmd.env_remove(key),
        };
    }
    for (var, file) in [("CHEBY_PHASES", phases), ("CHEBY_CACHE_LOG", log)] {
        if let Some(file) = file {
            let _ = std::fs::remove_file(file);
            cmd.env(var, file);
        }
    }
    cmd.arg("--color=never").args(args).current_dir(dir);
    let start = Instant::now();
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run {}: {e}", cx.cheby.display()))?;
    let time = start.elapsed();
    if !out.status.success() {
        return Err(format!(
            "`cheby {}` failed in {} ({})\n{}",
            args.join(" "),
            dir.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let read =
        |file: &Path| std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()));
    Ok(Ran {
        time,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        phases: match phases {
            Some(file) => parse_phases(&read(file)?)?,
            None => BTreeMap::new(),
        },
        log: match log {
            Some(file) => read(file)?,
            None => String::new(),
        },
    })
}

/// The phase lines of a `CHEBY_PHASES` file (plan §2.1: `cheby-phases 1
/// <command>`, `<phase> <ns> <spans>` lines, `command <ns>`), `command`
/// left out.
pub(crate) fn parse_phases(text: &str) -> Result<BTreeMap<String, u64>, String> {
    let mut lines = text.lines();
    if !lines
        .next()
        .is_some_and(|head| head.starts_with("cheby-phases 1 "))
    {
        return Err(format!("not a phases file: `{text}`"));
    }
    let mut out = BTreeMap::new();
    let mut command = false;
    for line in lines {
        let mut fields = line.split(' ');
        let (Some(name), Some(nanos)) = (fields.next(), fields.next()) else {
            return Err(format!("a malformed phases line `{line}`"));
        };
        let nanos: u64 = nanos
            .parse()
            .map_err(|_| format!("a malformed phases line `{line}`"))?;
        if name == "command" {
            command = true;
        } else {
            out.insert(name.to_owned(), nanos);
        }
    }
    if command {
        Ok(out)
    } else {
        Err(format!("a phases file without `command`: `{text}`"))
    }
}

/// The middle of `values` once sorted (the upper middle of an even
/// count), 0 when there is none.
pub(crate) fn median(values: &[u64]) -> u64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied().unwrap_or(0)
}

/// Each phase's median over `runs`, a phase a run lacks counting 0.
pub(crate) fn phases_median(runs: &[Ran]) -> BTreeMap<String, u64> {
    let mut names: Vec<&String> = runs.iter().flat_map(|run| run.phases.keys()).collect();
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .map(|name| {
            let values: Vec<u64> = runs
                .iter()
                .map(|run| run.phases.get(name).copied().unwrap_or(0))
                .collect();
            (name.clone(), median(&values))
        })
        .collect()
}

fn nanos(time: Duration) -> u64 {
    u64::try_from(time.as_nanos()).unwrap_or(u64::MAX)
}

/// An edit kind's rounds as plan §2.1's JSON.
pub(crate) fn rounds_json(runs: &[Ran]) -> Value {
    let times: Vec<u64> = runs.iter().map(|run| nanos(run.time)).collect();
    json!({
        "runs_ns": times,
        "median_ns": median(&times),
        "phases_median": phases_median(runs),
        "restarted": runs.iter().filter(|run| run.restarted()).count(),
    })
}

/// The edit to `main`'s own body (D-1278): a statement `let _k7 =
/// <n>` first in `main`, inserted on the first edit and given the value
/// `n` on each later one. `main` binds the value to nothing, so the
/// program prints what it printed; the module's source, and so its front
/// and code keys, change with every new `n`.
pub(crate) fn main_edit(dir: &Path, n: usize) -> Result<(), String> {
    let file = dir.join("src/main.cheby");
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let edited = main_edited(&text, n)
        .ok_or_else(|| format!("{}: no `fn main() {{` to edit", file.display()))?;
    std::fs::write(&file, edited).map_err(|e| format!("{}: {e}", file.display()))
}

/// The marker line [`main_edit`] keeps first in `main`.
const MAIN_EDIT: &str = "  let _k7 = ";

/// `text` with [`main_edit`]'s statement set to `n`; `None` without
/// `fn main() {`.
pub(crate) fn main_edited(text: &str, n: usize) -> Option<String> {
    let line = format!("{MAIN_EDIT}{n}");
    if text.lines().any(|l| l.starts_with(MAIN_EDIT)) {
        let mut out = String::with_capacity(text.len());
        for l in text.split_inclusive('\n') {
            if l.starts_with(MAIN_EDIT) {
                out.push_str(&line);
                out.push('\n');
            } else {
                out.push_str(l);
            }
        }
        return Some(out);
    }
    let head = "fn main() {\n";
    let at = text.find(head)?.saturating_add(head.len());
    Some(format!(
        "{}{line}\n{}",
        text.get(..at)?,
        text.get(at..).unwrap_or_default()
    ))
}

/// Copies the directory `from` into `to`, which it creates.
pub(crate) fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let entries = std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", from.display()))?;
        let target = to.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", entry.path().display()))?;
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("{}: {e}", target.display()))?;
        }
    }
    Ok(())
}

/// The package's module files under `dir/src`.
pub(crate) fn count_modules(dir: &Path) -> Result<usize, String> {
    fn walk(dir: &Path, n: &mut usize) -> Result<(), String> {
        let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
            if path.is_dir() {
                walk(&path, n)?;
            } else if path.extension().is_some_and(|ext| ext == "cheby") {
                *n = n.saturating_add(1);
            }
        }
        Ok(())
    }
    let mut n = 0;
    walk(&dir.join("src"), &mut n)?;
    Ok(n)
}

/// Writes `bench/gen`'s project `shape` of `lines` lines with seed 1 to
/// `dir` (empty or missing), or copies `from/<shape>-<lines>` there.
fn project(from: Option<&Path>, shape: Shape, lines: usize, dir: &Path) -> Result<(), String> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    match from {
        Some(from) => {
            let source = from.join(format!("{}-{lines}", shape.name()));
            if !source.join("cheby.toml").is_file() {
                return Err(format!(
                    "--from: no project `{}` (expected `<dir>/<shape>-<lines>`)",
                    source.display()
                ));
            }
            copy_dir(&source, dir)
        }
        None => cheby_bench_gen::generate(&Config {
            lines,
            shape,
            seed: 1,
        })
        .write(dir)
        .map(|_| ())
        .map_err(|e| format!("{}: {e}", dir.display())),
    }
}

/// `--oqp11`: plan §2.1's `oqp11` object.
pub(crate) fn oqp11(cx: &Cx<'_>) -> Result<Value, String> {
    let mut shapes = serde_json::Map::new();
    for shape in SHAPES {
        let name = shape.name();
        eprintln!(
            "bench-compile --oqp11: `{name}`, {} lines, {} rounds",
            cx.lines, cx.rounds
        );
        let dir = cx.work.join(format!("oqp11/{name}-{}", cx.lines));
        project(cx.from, shape, cx.lines, &dir)?;
        let cache = crate::cache::fresh(&format!("oqp11-{name}"))?;
        let phases = cx.work.join(format!("oqp11/{name}.phases"));
        let run = |files| run_cheby(cx, &dir, cache.path(), &["run"], &[], files);
        let cold = run((Some(&phases), None))?;
        let mut body: Vec<Ran> = Vec::with_capacity(cx.rounds);
        for round in 1..=cx.rounds {
            crate::bench::edit_one_body(&dir)?;
            let ran = run((Some(&phases), None))?;
            let first = body.first().map(|first| first.stdout.as_str());
            body_edit_output(&cold.stdout, first, &ran.stdout, round)?;
            body.push(ran);
        }
        // `main`'s edits print what the last body edit's run printed.
        let reference = body.last().unwrap_or(&cold).clone();
        let mut main = Vec::with_capacity(cx.rounds);
        for n in 1..=cx.rounds {
            main_edit(&dir, n)?;
            main.push(same_output(
                &reference,
                run((Some(&phases), None))?,
                "a main edit",
            )?);
        }
        shapes.insert(
            name.to_owned(),
            json!({
                "modules": count_modules(&dir)?,
                "cold_ns": nanos(cold.time),
                "cold_phases": cold.phases,
                "body_edit": rounds_json(&body),
                "main_edit": rounds_json(&main),
            }),
        );
    }
    Ok(json!({
        "lines": cx.lines,
        "rounds": cx.rounds,
        "shapes": shapes,
        "budget_ns": nanos(BUDGET),
    }))
}

/// Checks the output of round `round` (from 1) of `edit_one_body`, which
/// adds 1 to one counter per round, so the printed total grows by the same
/// step each round (D-1278): `run` must print an integer, `cold`
/// plus `round` times the first round's growth (`first`, `None` in round
/// 1).
pub(crate) fn body_edit_output(
    cold: &str,
    first: Option<&str>,
    run: &str,
    round: usize,
) -> Result<(), String> {
    let number = |text: &str| -> Result<i128, String> {
        text.trim().parse().map_err(|_| {
            format!(
                "a body edit's run printed `{}`, not a total",
                text.trim_end()
            )
        })
    };
    let (cold, run) = (number(cold)?, number(run)?);
    let Some(first) = first else {
        return Ok(());
    };
    let step = number(first)?.saturating_sub(cold);
    let round = i128::try_from(round).unwrap_or(i128::MAX);
    let expected = cold.saturating_add(step.saturating_mul(round));
    if run == expected {
        Ok(())
    } else {
        Err(format!(
            "round {round} of a body edit printed {run}, not {expected} (the cold run's {cold} \
             plus {round} steps of {step})"
        ))
    }
}

/// `run`, failing unless it printed what `cold` printed.
fn same_output(cold: &Ran, run: Ran, what: &str) -> Result<Ran, String> {
    if run.stdout == cold.stdout {
        Ok(run)
    } else {
        Err(format!(
            "after {what} `cheby` printed `{}`, not the cold run's `{}`",
            run.stdout.trim_end(),
            cold.stdout.trim_end()
        ))
    }
}

/// One of RB-3's restart causes, or a reference (D-1256).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    /// A warm run with no edit.
    Unedited,
    /// `edit_one_body`: one body of a module `main` imports.
    OneBody,
    /// A field added to a record type that an unedited module passes along
    /// (cache-hit test 6's shape).
    ReaderLowering,
    /// A body edit that changes a package-visible function's borrow mask
    /// (D-812, cache-hit test 2's shape).
    Mask,
    /// `CHEBY_POSITIONS=off` after a run with positions on (D-1173, D-958).
    Settings,
}

impl Cause {
    /// Every row of a shape and size, in order.
    pub(crate) const ALL: [Self; 5] = [
        Self::Unedited,
        Self::OneBody,
        Self::ReaderLowering,
        Self::Mask,
        Self::Settings,
    ];

    /// The JSON's name.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Unedited => "unedited",
            Self::OneBody => "one_body",
            Self::ReaderLowering => "reader_lowering",
            Self::Mask => "mask",
            Self::Settings => "settings",
        }
    }

    /// Whether every measured run must start again.
    pub(crate) const fn restarts(self) -> bool {
        matches!(self, Self::ReaderLowering | Self::Mask | Self::Settings)
    }

    /// Brings `dir` to the cause's state of round `round` (from 1), never
    /// seen before by the cause's cache.
    fn apply(self, dir: &Path, round: usize) -> Result<(), String> {
        let write = |file: &str, text: String| {
            let path = dir.join(FIXTURE_DIR).join(file);
            std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
        };
        match self {
            Self::Unedited | Self::Settings => Ok(()),
            Self::OneBody => crate::bench::edit_one_body(dir),
            Self::ReaderLowering => write("thing.cheby", thing_source(Some(round))),
            Self::Mask => {
                if round > MASK_PARAMS {
                    return Err(format!(
                        "--restarts: at most {} rounds",
                        MASK_PARAMS.saturating_sub(1)
                    ));
                }
                write("masks.cheby", masks_source(round))
            }
        }
    }
}

/// Where the fixture's modules go in a project (`bench::k7::…`).
const FIXTURE_DIR: &str = "src/k7";

/// The fixture's record type and its maker, with the field `f<n>` added
/// for `Some(n)`: an interface edit only a reader's lowering reads
/// (`reader.cheby` passes a `wrap::Wrapper` along, whose field is a
/// `Thing`).
pub(crate) fn thing_source(field: Option<usize>) -> String {
    let (decl, init) = match field {
        Some(n) => (format!("  f{n}: Int,\n"), format!(", f{n}: {n}")),
        None => (String::new(), String::new()),
    };
    format!(
        "//! K7's restart fixture: a record type whose fields only a reader's lowering reads.\n\n\
         /// A thing with a size.\npub type Thing {{\n  size: Int,\n{decl}}}\n\n\
         /// A thing of size 1.\npub fn make() -> Thing {{\n  Thing {{ size: 1{init} }}\n}}\n"
    )
}

/// The fixture's package-visible `check`, its first `owned` parameters
/// put in a list (owned, D-812) and the rest only compared (borrowed):
/// each further `owned` is a mask no earlier round had.
pub(crate) fn masks_source(owned: usize) -> String {
    let params: Vec<String> = (0..MASK_PARAMS).map(|i| format!("a{i}: String")).collect();
    let terms: Vec<String> = (0..MASK_PARAMS)
        .map(|i| {
            if i < owned {
                format!("[a{i}] == [\"x\"]")
            } else {
                format!("a{i} == \"x\"")
            }
        })
        .collect();
    format!(
        "//! K7's restart fixture: a package-visible function whose borrow mask each round changes.\n\n\
         /// Whether every argument is `x`.\nfn check({}) -> Bool {{\n  {}\n}}\n",
        params.join(", "),
        terms.join(" && ")
    )
}

/// The fixture's modules besides `thing` and `masks`: `wrap` (a record
/// whose field is a `Thing`), `reader` (passes a `Wrapper` along and never
/// reads `thing` in its check) and `caller` (calls `masks::check`, with a
/// test for the `cheby test` sample).
fn fixture_sources() -> [(&'static str, String); 5] {
    let args = vec!["\"x\""; MASK_PARAMS].join(", ");
    [
        ("thing.cheby", thing_source(None)),
        ("masks.cheby", masks_source(0)),
        (
            "wrap.cheby",
            "//! K7's restart fixture: a record whose field is a `thing::Thing`.\n\n\
             import bench::k7::thing\n\n/// A thing, wrapped.\npub type Wrapper {\n  inner: thing::Thing,\n}\n\n\
             /// A wrapped thing.\npub fn wrap() -> Wrapper {\n  Wrapper { inner: thing::make() }\n}\n"
                .to_owned(),
        ),
        (
            "reader.cheby",
            "//! K7's restart fixture: passes a `wrap::Wrapper` along; its check never reads `thing`.\n\n\
             import bench::k7::wrap\n\nfn keep(w: wrap::Wrapper) -> wrap::Wrapper {\n  w\n}\n\n\
             /// Passes a wrapper along and returns 0.\npub fn run() -> Int {\n  let _ = keep(wrap::wrap())\n  0\n}\n"
                .to_owned(),
        ),
        (
            "caller.cheby",
            format!(
                "//! K7's restart fixture: calls `masks::check`, never edited.\n\n\
                 import bench::k7::masks\n\n/// Whether the fixture's arguments check.\npub fn run() -> Bool {{\n  \
                 masks::check({args})\n}}\n\ntest \"the arguments check\" {{\n  assert run()\n}}\n"
            ),
        ),
    ]
}

/// Adds the restart fixture (D-1279) to the project in
/// `dir`, a copy of `bench/gen`'s output: its modules under `src/k7/`, and
/// in `main` the imports of `caller` and `reader` after the others and a
/// call of each first in its body, whose values it drops, so the program
/// prints what it printed and `main` reaches every edit site.
pub(crate) fn add_fixture(dir: &Path) -> Result<(), String> {
    let fixture = dir.join(FIXTURE_DIR);
    std::fs::create_dir_all(&fixture).map_err(|e| format!("{}: {e}", fixture.display()))?;
    for (file, text) in fixture_sources() {
        let path = fixture.join(file);
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let file = dir.join("src/main.cheby");
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let edited = main_with_fixture(&text)
        .ok_or_else(|| format!("{}: no imports and `fn main() {{`", file.display()))?;
    std::fs::write(&file, edited).map_err(|e| format!("{}: {e}", file.display()))
}

/// `main.cheby`'s text with the fixture's imports after its last import
/// and its calls first in `main`; `None` without both.
pub(crate) fn main_with_fixture(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len().saturating_add(160));
    let last_import = text
        .split_inclusive('\n')
        .enumerate()
        .filter(|(_, line)| line.starts_with("import "))
        .map(|(i, _)| i)
        .last()?;
    let mut called = false;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        out.push_str(line);
        if i == last_import {
            out.push_str("import bench::k7::caller\nimport bench::k7::reader\n");
        }
        if line == "fn main() {\n" && !called {
            out.push_str("  let _ = caller::run()\n  let _ = reader::run()\n");
            called = true;
        }
    }
    called.then_some(out)
}

/// The sizes the restart causes are read at: 100k, and `lines` when it is
/// another size; `lines` alone below 100k (the gate's smoke).
pub(crate) fn restart_sizes(lines: usize) -> Vec<usize> {
    match lines.cmp(&RESTART_LINES) {
        std::cmp::Ordering::Less | std::cmp::Ordering::Equal => vec![lines],
        std::cmp::Ordering::Greater => vec![RESTART_LINES, lines],
    }
}

/// The rounds of a restart cause at `lines`: `rounds`, at most 3 at 1M.
pub(crate) fn restart_rounds(rounds: usize, lines: usize) -> usize {
    if lines >= 1_000_000 {
        rounds.min(RESTART_ROUNDS_1M)
    } else {
        rounds
    }
}

/// `--restarts`: plan §2.1's `restarts` object.
pub(crate) fn restarts(cx: &Cx<'_>) -> Result<Value, String> {
    if cx.rounds >= MASK_PARAMS {
        return Err(format!(
            "--restarts: at most {} rounds",
            MASK_PARAMS.saturating_sub(1)
        ));
    }
    let mut rows = Vec::new();
    for lines in restart_sizes(cx.lines) {
        for shape in SHAPES {
            let pristine = cx
                .work
                .join(format!("restarts/{}-{lines}/pristine", shape.name()));
            project(cx.from, shape, lines, &pristine)?;
            add_fixture(&pristine)?;
            let rounds = restart_rounds(cx.rounds, lines);
            for cause in Cause::ALL {
                rows.push(restart_row(
                    cx, &pristine, shape, lines, cause, "run", rounds,
                )?);
            }
        }
    }
    // The `cheby test` sample: `mask` on `wide` at 100k (the smoke's size
    // below that), over the fixture's test.
    let lines = restart_sizes(cx.lines).first().copied().unwrap_or(cx.lines);
    let pristine = cx.work.join(format!("restarts/wide-{lines}/pristine"));
    rows.push(restart_row(
        cx,
        &pristine,
        Shape::Wide,
        lines,
        Cause::Mask,
        "test",
        cx.rounds.min(TEST_ROUNDS),
    )?);
    Ok(json!({ "rounds": cx.rounds, "rows": rows }))
}

/// One row of `--restarts`: `cause`'s rounds of `command` on a copy of
/// `pristine` (the project with the fixture), and its untimed logged run.
fn restart_row(
    cx: &Cx<'_>,
    pristine: &Path,
    shape: Shape,
    lines: usize,
    cause: Cause,
    command: &str,
    rounds: usize,
) -> Result<Value, String> {
    let label = format!(
        "{} {lines} {} `cheby {command}`",
        shape.name(),
        cause.name()
    );
    eprintln!("bench-compile --restarts: {label}, {rounds} rounds");
    let base = cx.work.join(format!(
        "restarts/{}-{lines}/{}-{command}",
        shape.name(),
        cause.name()
    ));
    if base.exists() {
        std::fs::remove_dir_all(&base).map_err(|e| format!("{}: {e}", base.display()))?;
    }
    let dir = base.join("project");
    copy_dir(pristine, &dir)?;
    let args: Vec<&str> = match command {
        "test" => vec!["test", "--module", "bench::k7::caller"],
        _ => vec![command],
    };
    let phases = base.join("phases.txt");
    let log = base.join("cache.log");
    // Positions on for every fill; `settings` turns them off for the runs.
    let fill_env: [Extra<'_>; 1] = [("CHEBY_POSITIONS", None)];
    let run_env: &[Extra<'_>] = if cause == Cause::Settings {
        &[("CHEBY_POSITIONS", Some("off"))]
    } else {
        &fill_env
    };
    let fill = |cache: &Path| run_cheby(cx, &dir, cache, &args, &fill_env, (None, None));
    let shared = crate::cache::fresh("restarts")?;
    let reference = fill(shared.path())?;
    let mut runs = Vec::with_capacity(rounds);
    // Round `rounds + 1` is the logged run's own fresh state.
    for round in 1..=rounds.saturating_add(1) {
        let own;
        let cache = if cause == Cause::Settings {
            own = crate::cache::fresh("restarts-settings")?;
            fill(own.path())?;
            own.path()
        } else {
            shared.path()
        };
        cause.apply(&dir, round)?;
        let files = if round > rounds {
            (None, Some(log.as_path()))
        } else {
            (Some(phases.as_path()), None)
        };
        let run = run_cheby(cx, &dir, cache, &args, run_env, files)?;
        if command == "run" && cause == Cause::OneBody {
            let first = runs.first().map(|first: &Ran| first.stdout.as_str());
            body_edit_output(&reference.stdout, first, &run.stdout, round)
                .map_err(|e| format!("{label}: {e}"))?;
        } else if command == "run" && run.stdout != reference.stdout {
            return Err(format!(
                "{label}: round {round} printed `{}`, not `{}`",
                run.stdout.trim_end(),
                reference.stdout.trim_end()
            ));
        }
        if round <= rounds {
            if cause.restarts() && !run.restarted() {
                return Err(format!(
                    "{label}: round {round} did not start again (no `restart` in its phases)"
                ));
            }
            runs.push(run);
        } else {
            return restart_json(&dir, (shape, lines), cause, command, &runs, &run.log);
        }
    }
    Err(format!("{label}: no logged run"))
}

/// A restart row (plan §2.1) from its measured `runs` and the logged run's
/// `log`, failing when a cause's logged run did not parse every package
/// module again.
pub(crate) fn restart_json(
    dir: &Path,
    (shape, lines): (Shape, usize),
    cause: Cause,
    command: &str,
    runs: &[Ran],
    log: &str,
) -> Result<Value, String> {
    let modules = count_modules(dir)?;
    let parses = log.lines().filter(|l| l.starts_with("parse ")).count();
    let std_parses = log.lines().filter(|l| l.starts_with("parse.std ")).count();
    if cause.restarts() && parses != modules {
        return Err(format!(
            "{} {lines} {} `cheby {command}`: the logged run parsed {parses} of {modules} \
             package modules; a restart checks every module again (D-1256)",
            shape.name(),
            cause.name()
        ));
    }
    let times: Vec<u64> = runs.iter().map(|run| nanos(run.time)).collect();
    let first: Vec<u64> = runs
        .iter()
        .map(|run| run.phases.get("restart").copied().unwrap_or(0))
        .collect();
    Ok(json!({
        "shape": shape.name(),
        "lines": lines,
        "command": command,
        "cause": cause.name(),
        "modules": modules,
        "runs_ns": times,
        "median_ns": median(&times),
        "restarted": runs.iter().filter(|run| run.restarted()).count(),
        "first_attempt_ns": median(&first),
        "rechecked_modules": if runs.iter().any(Ran::restarted) { parses } else { 0 },
        "parses": parses,
        "std_parses": std_parses,
        "phases_median": phases_median(runs),
    }))
}

/// A short Markdown summary of `oqp11` and `restarts` for the terminal and
/// `--summary` (`bench_oqp11.py` renders the full tables).
pub(crate) fn render(oqp11: Option<&Value>, restarts: Option<&Value>) -> String {
    let secs = |v: &Value, at: &str| {
        let ns = v.pointer(at).and_then(Value::as_f64).unwrap_or(0.0);
        format!("{:.3} s", ns / 1e9)
    };
    let mut out = String::new();
    if let Some(o) = oqp11 {
        let _ = writeln!(
            out,
            "\n| OQ-P11 ({} lines) | Cold run | Body edit (median) | Main edit (median) |\n\
             | ----------------- | -------- | ------------------ | ------------------ |",
            o.get("lines").unwrap_or(&Value::Null)
        );
        if let Some(shapes) = o.get("shapes").and_then(Value::as_object) {
            for (name, s) in shapes {
                let _ = writeln!(
                    out,
                    "| `{name}` | {} | {} | {} |",
                    secs(s, "/cold_ns"),
                    secs(s, "/body_edit/median_ns"),
                    secs(s, "/main_edit/median_ns")
                );
            }
        }
    }
    if let Some(r) = restarts {
        out.push_str(
            "\n| Restart cause | Command | Median | Restarted | First attempt | Parses |\n\
             | ------------- | ------- | ------ | --------- | ------------- | ------ |\n",
        );
        for row in r
            .get("rows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let field = |key: &str| row.get(key).unwrap_or(&Value::Null);
            let text = |key: &str| field(key).as_str().unwrap_or_default().to_owned();
            let _ = writeln!(
                out,
                "| `{}` {} {} | `{}` | {} | {} of {} | {} | {} |",
                text("shape"),
                field("lines"),
                text("cause"),
                text("command"),
                secs(row, "/median_ns"),
                field("restarted"),
                field("runs_ns").as_array().map_or(0, Vec::len),
                secs(row, "/first_attempt_ns"),
                field("parses"),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use cheby_test_support::cache::FreshDir;

    const MAIN: &str = "//! Entry point.\n\nimport std::io\nimport bench::g000::m00039\n\n\
                        fn main() {\n  let total = m00039::entry(10)\n  io::println(\"{total}\")\n}\n";

    fn ran(ms: u64, phases: &[(&str, u64)]) -> Ran {
        Ran {
            time: Duration::from_millis(ms),
            phases: phases.iter().map(|&(k, v)| (k.to_owned(), v)).collect(),
            ..Ran::default()
        }
    }

    /// D-1278: the edit inserts `let _k7 = <n>` first in `main`,
    /// then changes only its value; the rest of `main` stays.
    #[test]
    fn oqp11_main_edit_sets_one_statement() {
        let first = main_edited(MAIN, 1).unwrap_or_default();
        assert!(
            first.contains("fn main() {\n  let _k7 = 1\n  let total"),
            "{first}"
        );
        let second = main_edited(&first, 2).unwrap_or_default();
        assert_eq!(second, first.replace("let _k7 = 1", "let _k7 = 2"));
        assert_eq!(second.matches("let _k7").count(), 1);
        assert_eq!(second.replace("  let _k7 = 2\n", ""), MAIN);
        assert_eq!(main_edited("fn other() {\n}\n", 1), None);
    }

    /// Each reading is the median of its rounds; a phase a run lacks
    /// counts 0.
    #[test]
    fn oqp11_median_and_phases_median() {
        assert_eq!(median(&[5, 1, 3]), 3);
        assert_eq!(median(&[4, 1, 3, 2]), 3);
        assert_eq!(median(&[]), 0);
        let runs = [
            ran(10, &[("codegen", 4), ("hash", 1)]),
            ran(30, &[("codegen", 6)]),
            ran(20, &[("codegen", 5), ("hash", 3)]),
        ];
        let medians = phases_median(&runs);
        assert_eq!(medians.get("codegen"), Some(&5));
        assert_eq!(medians.get("hash"), Some(&1));
    }

    /// A body edit's round prints the cold total plus its rounds' steps.
    #[test]
    fn oqp11_body_edit_output_grows_by_one_step_a_round() {
        assert_eq!(body_edit_output("8690\n", None, "8691\n", 1), Ok(()));
        assert_eq!(
            body_edit_output("8690\n", Some("8693\n"), "8696\n", 2),
            Ok(())
        );
        assert!(body_edit_output("8690\n", Some("8693\n"), "8695\n", 2).is_err());
        assert!(body_edit_output("8690\n", None, "oops\n", 1).is_err());
    }

    #[test]
    fn oqp11_phases_file_parses() {
        let text = "cheby-phases 1 run\nhash 10 1\nrestart 30 1\ncommand 100\n";
        let phases = parse_phases(text).unwrap_or_default();
        assert_eq!(phases.get("hash"), Some(&10));
        assert_eq!(phases.get("restart"), Some(&30));
        assert!(!phases.contains_key("command"));
        assert!(parse_phases("hash 1 1\ncommand 2\n").is_err());
        assert!(parse_phases("cheby-phases 1 run\nhash 1 1\n").is_err());
        assert!(parse_phases("cheby-phases 1 run\nhash x 1\ncommand 2\n").is_err());
    }

    /// Plan §2.1's shape of an edit kind's object.
    #[test]
    fn oqp11_json_shape() {
        let runs = [
            ran(300, &[("codegen", 100), ("restart", 0)]),
            ran(100, &[("codegen", 50)]),
            ran(200, &[("codegen", 70), ("restart", 9)]),
        ];
        let value = rounds_json(&runs);
        assert_eq!(
            value.get("runs_ns"),
            Some(&json!([300_000_000_u64, 100_000_000_u64, 200_000_000_u64]))
        );
        assert_eq!(value.get("median_ns"), Some(&json!(200_000_000_u64)));
        assert_eq!(value.pointer("/phases_median/codegen"), Some(&json!(70)));
        assert_eq!(value.get("restarted"), Some(&json!(1)));
    }

    /// The fixture's edit sites (D-1279): `main` imports the
    /// fixture after its own imports, so `edit_one_body` still finds the
    /// generated module first, and calls it first in its body.
    #[test]
    fn restarts_fixture_joins_main() {
        let text = main_with_fixture(MAIN).unwrap_or_default();
        let imports: Vec<&str> = text.lines().filter(|l| l.starts_with("import ")).collect();
        assert_eq!(
            imports,
            [
                "import std::io",
                "import bench::g000::m00039",
                "import bench::k7::caller",
                "import bench::k7::reader",
            ]
        );
        assert!(text.contains(
            "fn main() {\n  let _ = caller::run()\n  let _ = reader::run()\n  let total"
        ));
        assert_eq!(main_with_fixture("fn main() {\n}\n"), None);
    }

    /// Every round reaches a state never seen before: a fresh field name,
    /// a mask with one more owned parameter.
    #[test]
    fn restarts_rounds_are_fresh_states() {
        let things: Vec<String> = (1..=5).map(|n| thing_source(Some(n))).collect();
        let masks: Vec<String> = (1..=MASK_PARAMS).map(masks_source).collect();
        for (sources, base) in [(&things, thing_source(None)), (&masks, masks_source(0))] {
            let mut all: Vec<&String> = sources.iter().chain([&base]).collect();
            all.sort_unstable();
            all.dedup();
            assert_eq!(all.len(), sources.len().saturating_add(1));
        }
        assert!(thing_source(Some(3)).contains("  f3: Int,\n"));
        assert!(thing_source(Some(3)).contains("Thing { size: 1, f3: 3 }"));
        assert_eq!(masks_source(2).matches("] == [\"x\"]").count(), 2);
        assert!(
            Cause::Mask
                .apply(Path::new("."), MASK_PARAMS.saturating_add(1))
                .is_err()
        );
    }

    #[test]
    fn restarts_sizes_and_rounds() {
        assert_eq!(restart_sizes(10_000), [10_000]);
        assert_eq!(restart_sizes(100_000), [100_000]);
        assert_eq!(restart_sizes(1_000_000), [100_000, 1_000_000]);
        assert_eq!(restart_rounds(5, 100_000), 5);
        assert_eq!(restart_rounds(5, 1_000_000), 3);
        assert_eq!(restart_rounds(1, 1_000_000), 1);
        let causes: Vec<&str> = Cause::ALL.iter().map(|c| c.name()).collect();
        assert_eq!(
            causes,
            [
                "unedited",
                "one_body",
                "reader_lowering",
                "mask",
                "settings"
            ]
        );
        assert_eq!(Cause::ALL.iter().filter(|c| c.restarts()).count(), 3);
    }

    /// A cause's row: its runs all started again and its logged run parsed
    /// every package module, or the tool fails.
    #[test]
    fn restarts_row_needs_every_module_parsed_again() {
        let tmp = FreshDir::create("xtask-restarts").unwrap_or_else(|e| panic!("{e}"));
        let src = tmp.path().join("src/k7");
        std::fs::create_dir_all(&src).unwrap_or_else(|e| panic!("{e}"));
        for file in ["main.cheby", "k7/a.cheby"] {
            std::fs::write(tmp.path().join("src").join(file), "").unwrap_or_else(|e| panic!("{e}"));
        }
        let runs = [
            ran(100, &[("restart", 40), ("check", 20)]),
            ran(120, &[("restart", 50)]),
        ];
        let log = "parse bench\nparse bench::k7::a\nparse.std std::io\n";
        let row = restart_json(
            tmp.path(),
            (Shape::Wide, 10_000),
            Cause::Mask,
            "run",
            &runs,
            log,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        for (key, value) in [
            ("cause", json!("mask")),
            ("command", json!("run")),
            ("restarted", json!(2)),
            ("first_attempt_ns", json!(50)),
            ("rechecked_modules", json!(2)),
            ("parses", json!(2)),
            ("std_parses", json!(1)),
            ("median_ns", json!(120_000_000_u64)),
        ] {
            assert_eq!(row.get(key), Some(&value), "{key}");
        }
        let partial = "parse bench\n";
        assert!(
            restart_json(
                tmp.path(),
                (Shape::Wide, 10_000),
                Cause::Mask,
                "run",
                &runs,
                partial
            )
            .is_err()
        );
        // A reference row parses what it parses.
        let reference = restart_json(
            tmp.path(),
            (Shape::Deep, 10_000),
            Cause::OneBody,
            "run",
            &[ran(10, &[])],
            partial,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(reference.get("rechecked_modules"), Some(&json!(0)));
        assert_eq!(reference.get("restarted"), Some(&json!(0)));
    }

    /// `.github/scripts/bench_oqp11.py` on fixture files: the OQ-P11 and
    /// restart tables and the cold columns render (0), a file without any
    /// is 1, a usage error 2, and a cause row whose runs did not all start
    /// again is never rendered as a reading.
    #[test]
    fn oqp11_renderer_on_fixture_files() {
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../.github/scripts/bench_oqp11.py");
        let tmp = FreshDir::create("xtask-oqp11-render").unwrap_or_else(|e| panic!("{e}"));
        let render = |value: &Value| -> Option<(i32, String)> {
            let file = tmp.path().join("in.json");
            std::fs::write(&file, value.to_string()).unwrap_or_else(|e| panic!("{e}"));
            let out = Command::new("python3")
                .arg("-I")
                .arg(&script)
                .args(["--title", "fixture"])
                .arg(&file)
                .output()
                .ok()?;
            Some((
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout).into_owned(),
            ))
        };
        let edit = |median: u64| {
            json!({"runs_ns": [median], "median_ns": median, "restarted": 0,
                   "phases_median": {"codegen": median / 2, "declare": median / 8,
                                     "define": median / 8, "finalize": 1, "hash": 3}})
        };
        let full = json!({
            "rows": [{"lines": 10000, "shape": "wide", "cold_check_ns": 300_000_000,
                      "cold_build_ns": 900_000_000, "cold_check_std_warm_ns": 100_000_000,
                      "cold_build_std_warm_ns": 700_000_000}],
            "oqp11": {"lines": 1_000_000, "rounds": 1, "budget_ns": 500_000_000,
                      "shapes": {"wide": {"modules": 40, "cold_ns": 9_000_000_000_u64,
                                          "body_edit": edit(800_000_000), "main_edit": edit(200_000_000)},
                                 "deep": {"modules": 4000, "cold_ns": 9_000_000_000_u64,
                                          "body_edit": edit(800_000_000), "main_edit": edit(200_000_000)}}},
            "restarts": {"rounds": 1, "rows": [
                {"shape": "wide", "lines": 100_000, "command": "run", "cause": "mask",
                 "modules": 403, "runs_ns": [141_000_000], "median_ns": 141_000_000, "restarted": 1,
                 "first_attempt_ns": 60_000_000, "rechecked_modules": 403, "parses": 403,
                 "std_parses": 23},
                {"shape": "deep", "lines": 100_000, "command": "run", "cause": "settings",
                 "modules": 403, "runs_ns": [1, 2], "median_ns": 2, "restarted": 1,
                 "first_attempt_ns": 1, "rechecked_modules": 403, "parses": 403, "std_parses": 0}]},
        });
        let Some((status, text)) = render(&full) else {
            eprintln!("no python3: the renderer is not exercised");
            return;
        };
        assert_eq!(status, 0, "{text}");
        for wanted in [
            "## fixture",
            "### OQ-P11: edit to run at 1000000 lines",
            "misses by 0.300 s",
            "| `codegen` | 0.400 s | 50.0% | 1 | yes |",
            "lazy compilation (D-923)",
            "`declare` + `define` + `finalize`",
            "GAP(oqp11-gates)",
            "| `wide` | 100000 | `run` | `mask` | 0.141 s | 1 of 1 | 0.060 s | 403 of 403 | 403 | 23 |",
            "not a reading: 1 of 2 runs started again",
            "| 10000 lines, `wide` | 0.300 s | 0.100 s | 0.200 s | 0.900 s | 0.700 s | 0.200 s |",
        ] {
            assert!(
                text.to_lowercase().contains(&wanted.to_lowercase()),
                "{wanted}\n{text}"
            );
        }
        let cold_only = json!({"rows": full.get("rows").cloned().unwrap_or_default()});
        assert_eq!(render(&cold_only).map(|r| r.0), Some(0));
        let nothing = json!({"rows": [{"lines": 10000, "shape": "wide"}]});
        assert_eq!(render(&nothing).map(|r| r.0), Some(1));
        let usage = Command::new("python3").arg("-I").arg(&script).output().ok();
        assert_eq!(usage.and_then(|o| o.status.code()), Some(2));
        // The review's RB10-2: an edit's null median or phase is a file that
        // is not such JSON (2), and a cause row with no runs is no reading.
        let null_median = json!({"oqp11": {"shapes": {"wide": {"body_edit":
            {"runs_ns": [], "median_ns": null, "phases_median": {}}}}}});
        assert_eq!(render(&null_median).map(|r| r.0), Some(2));
        let null_phase = json!({"oqp11": {"shapes": {"wide": {"body_edit":
            {"runs_ns": [1], "median_ns": 1, "phases_median": {"codegen": null}}}}}});
        assert_eq!(render(&null_phase).map(|r| r.0), Some(2));
        let no_runs =
            json!({"restarts": {"rows": [{"shape": "wide", "cause": "mask", "runs_ns": []}]}});
        let rendered = render(&no_runs);
        assert_eq!(rendered.as_ref().map(|r| r.0), Some(0));
        let text = rendered.map(|r| r.1).unwrap_or_default();
        assert!(text.contains("not a reading: no runs"), "{text}");
        assert!(!text.contains("0 of 0"), "{text}");
    }
}
