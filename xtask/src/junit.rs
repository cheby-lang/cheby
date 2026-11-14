//! `cargo xtask junit-report`: renders nextest's `JUnit` report as one HTML
//! page and as a Markdown summary (D-1559).
//!
//! The `ci` profile of `.config/nextest.toml` writes
//! `target/nextest/ci/junit.xml` (D-1558). This command reads it and
//! writes:
//!
//! - `--html <file>`: one self-contained page, with no script and nothing
//!   linked, so a browser shows it straight from a CI run's artifacts. It
//!   lists the tests that failed or timed out with their output, the flaky
//!   and the slow ones, and then every test per test binary.
//! - `--summary <file>`: Markdown appended to the file, such as
//!   `$GITHUB_STEP_SUMMARY`: the counts, the failures with the end of
//!   their output, the flaky tests and the slowest ones. It is capped well
//!   under GitHub's limit of 1 MiB per step, and the page holds the rest.
//!
//! nextest's report has no mark for a slow test, so a test is slow here
//! when it took [`SLOW_SECONDS`] or more, the `ci` profile's period. It
//! has no entry for an ignored test either, so none is counted.
//!
//! The command fails only when the report cannot be read or a file cannot
//! be written. Failed tests do not fail it: the test step already did.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Where the `ci` profile writes its report, from the repository's root.
const DEFAULT_JUNIT: &str = "target/nextest/ci/junit.xml";

/// A test that took this long is slow: the period of the `ci` profile's
/// `slow-timeout` (D-1558).
const SLOW_SECONDS: f64 = 60.0;

/// The page keeps this much of the end of each output stream.
const PAGE_OUTPUT_BYTES: usize = 64 * 1024;

/// The summary keeps this much of the end of each output stream.
const SUMMARY_OUTPUT_BYTES: usize = 3000;

/// The summary shows the output of this many failures and only names
/// [`SUMMARY_NAMED`] more.
const SUMMARY_DETAILED: usize = 30;

/// See [`SUMMARY_DETAILED`]; also the cap on the flaky tests named.
const SUMMARY_NAMED: usize = 200;

/// The summary lists this many slow tests, slowest first.
const SUMMARY_SLOW: usize = 20;

#[derive(Debug, clap::Args)]
pub struct Args {
    /// nextest's `JUnit` report (default: `target/nextest/ci/junit.xml` of
    /// the repository)
    #[arg(long)]
    junit: Option<PathBuf>,
    /// Write the HTML page to this file
    #[arg(long)]
    html: PathBuf,
    /// Append a Markdown summary to this file, such as
    /// `$GITHUB_STEP_SUMMARY`
    #[arg(long)]
    summary: Option<PathBuf>,
    /// The run's name in the headings, such as the CI platform
    #[arg(long, default_value = "local")]
    title: String,
}

pub(crate) fn run(root: &Path, args: &Args) -> Result<(), String> {
    let junit = args
        .junit
        .clone()
        .unwrap_or_else(|| root.join(DEFAULT_JUNIT));
    let xml = std::fs::read_to_string(&junit).map_err(|e| format!("{}: {e}", junit.display()))?;
    let report = Report::parse(&xml).map_err(|e| format!("{}: {e}", junit.display()))?;
    std::fs::write(&args.html, page(&report, &args.title))
        .map_err(|e| format!("{}: {e}", args.html.display()))?;
    if let Some(summary) = &args.summary {
        let page_name = args
            .html
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        super::append(
            summary,
            &markdown(&report, &args.title, page_name.as_deref()),
        )?;
    }
    println!("{} -> {}", report.totals().line(), args.html.display());
    Ok(())
}

/// One nextest run.
#[derive(Debug)]
struct Report {
    /// nextest's id of the run.
    id: String,
    /// When the run started, as nextest wrote it.
    started: String,
    /// The run's wall-clock time.
    seconds: f64,
    suites: Vec<Suite>,
}

/// The tests of one test binary.
#[derive(Debug)]
struct Suite {
    /// The binary's id, such as `cheby_cli::ui`.
    name: String,
    cases: Vec<Case>,
}

#[derive(Debug)]
struct Case {
    name: String,
    /// The time of the attempt that decided the outcome.
    seconds: f64,
    outcome: Outcome,
    /// The attempts that failed, in order. A flaky test passed after them.
    attempts: Vec<Attempt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Passed,
    /// Passed on a retry.
    Flaky,
    Failed,
    /// Stopped by the profile's `terminate-after`.
    TimedOut,
    Skipped,
}

/// One failed attempt of a test.
#[derive(Debug)]
struct Attempt {
    /// nextest's description, such as `test failure with exit code 101`.
    kind: String,
    /// The first line of the failure, when nextest found one.
    message: String,
    /// The failure's text, usually the panic message.
    text: String,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Totals {
    tests: usize,
    /// Flaky tests included.
    passed: usize,
    failed: usize,
    timed_out: usize,
    flaky: usize,
    slow: usize,
    skipped: usize,
}

impl Totals {
    fn line(&self) -> String {
        format!(
            "{} tests: {} passed, {} failed, {} timed out; {} flaky, {} slow",
            self.tests, self.passed, self.failed, self.timed_out, self.flaky, self.slow
        )
    }
}

impl Outcome {
    /// Failed or timed out.
    fn is_bad(self) -> bool {
        matches!(self, Self::Failed | Self::TimedOut)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Flaky => "flaky",
            Self::Failed => "failed",
            Self::TimedOut => "timed out",
            Self::Skipped => "skipped",
        }
    }

    /// The CSS class of the outcome's badge.
    fn class(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Flaky => "flaky",
            Self::Failed | Self::TimedOut => "bad",
            Self::Skipped => "skipped",
        }
    }
}

impl Case {
    fn is_slow(&self) -> bool {
        self.seconds >= SLOW_SECONDS
    }

    /// How many times the test ran.
    fn tries(&self) -> usize {
        let passing = usize::from(self.outcome == Outcome::Flaky);
        self.attempts.len().saturating_add(passing).max(1)
    }
}

impl Report {
    /// Reads nextest's `JUnit` XML.
    fn parse(xml: &str) -> Result<Self, String> {
        let document = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
        let root = document.root_element();
        if !root.has_tag_name("testsuites") {
            return Err(format!(
                "the root element is `{}`, not `testsuites`",
                root.tag_name().name()
            ));
        }
        let suites = elements(root, &["testsuite"])
            .map(|suite| Suite {
                name: attribute(suite, "name"),
                cases: elements(suite, &["testcase"]).map(case).collect(),
            })
            .collect();
        Ok(Self {
            id: attribute(root, "uuid"),
            started: attribute(root, "timestamp"),
            seconds: seconds(root),
            suites,
        })
    }

    fn cases(&self) -> impl Iterator<Item = (&Suite, &Case)> {
        self.suites
            .iter()
            .flat_map(|suite| suite.cases.iter().map(move |case| (suite, case)))
    }

    fn with_outcome(&self, outcome: Outcome) -> Vec<(&Suite, &Case)> {
        self.cases()
            .filter(|(_, case)| case.outcome == outcome)
            .collect()
    }

    /// The slow tests, slowest first.
    fn slow(&self) -> Vec<(&Suite, &Case)> {
        let mut slow: Vec<_> = self.cases().filter(|(_, case)| case.is_slow()).collect();
        slow.sort_by(|(_, a), (_, b)| b.seconds.total_cmp(&a.seconds));
        slow
    }

    fn totals(&self) -> Totals {
        let count = |outcome| self.cases().filter(|(_, c)| c.outcome == outcome).count();
        let flaky = count(Outcome::Flaky);
        Totals {
            tests: self.cases().count(),
            passed: count(Outcome::Passed).saturating_add(flaky),
            failed: count(Outcome::Failed),
            timed_out: count(Outcome::TimedOut),
            flaky,
            slow: self.cases().filter(|(_, c)| c.is_slow()).count(),
            skipped: count(Outcome::Skipped),
        }
    }
}

/// The child elements of `node` with one of these tag names.
fn elements<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    names: &[&str],
) -> impl Iterator<Item = roxmltree::Node<'a, 'input>> {
    node.children()
        .filter(move |child| child.is_element() && names.contains(&child.tag_name().name()))
}

fn attribute(node: roxmltree::Node<'_, '_>, name: &str) -> String {
    clean(node.attribute(name).unwrap_or_default())
}

/// The `time` attribute, in seconds.
fn seconds(node: roxmltree::Node<'_, '_>) -> f64 {
    node.attribute("time")
        .and_then(|time| time.parse().ok())
        .unwrap_or(0.0)
}

/// The text of the first child element named `name`.
fn child_text(node: roxmltree::Node<'_, '_>, name: &str) -> String {
    elements(node, &[name])
        .next()
        .and_then(|child| child.text())
        .map(clean)
        .unwrap_or_default()
}

/// The text directly inside `node`, without its child elements' text.
fn own_text(node: roxmltree::Node<'_, '_>) -> String {
    let text: String = node
        .children()
        .filter(roxmltree::Node::is_text)
        .filter_map(|child| child.text())
        .collect();
    clean(text.trim())
}

/// One `testcase`. Its `failure` is the first attempt, whose output is the
/// test case's own. A `rerunFailure` is a later failed attempt, and a
/// `flakyFailure` an attempt that failed before the test passed; both hold
/// their own output.
fn case(node: roxmltree::Node<'_, '_>) -> Case {
    let attempt = |failure: roxmltree::Node<'_, '_>, output: roxmltree::Node<'_, '_>| Attempt {
        kind: attribute(failure, "type"),
        message: attribute(failure, "message"),
        text: own_text(failure),
        stdout: child_text(output, "system-out"),
        stderr: child_text(output, "system-err"),
    };
    let first = elements(node, &["failure", "error"]).next();
    let flaky: Vec<_> = elements(node, &["flakyFailure", "flakyError"]).collect();
    let outcome = match first {
        Some(failure) if failure.attribute("type") == Some("test timeout") => Outcome::TimedOut,
        Some(_) => Outcome::Failed,
        None if elements(node, &["skipped"]).next().is_some() => Outcome::Skipped,
        None if flaky.is_empty() => Outcome::Passed,
        None => Outcome::Flaky,
    };
    let attempts = first
        .map(|failure| attempt(failure, node))
        .into_iter()
        .chain(
            elements(node, &["rerunFailure", "rerunError"])
                .chain(flaky.iter().copied())
                .map(|failure| attempt(failure, failure)),
        )
        .collect();
    Case {
        name: attribute(node, "name"),
        seconds: seconds(node),
        outcome,
        attempts,
    }
}

/// `text` without terminal escape sequences and control characters other
/// than the newline and the tab.
fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // A CSI sequence ends at its first character from `@` to `~`.
                if chars.next_if_eq(&'[').is_some() {
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
            }
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// `text` with HTML's special characters escaped.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// [`escape`], and the characters that Markdown reads as markup written as
/// character references, so a name such as `[pass] a_b` stays as it is in
/// a summary.
fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in escape(text).chars() {
        match c {
            '*' | '_' | '[' | ']' | '`' | '|' | '\\' | '~' | '$' => {
                let _ = write!(out, "&#{};", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

/// At most the last `max` bytes of `text`, from the start of a line where
/// there is one, and whether anything was cut.
fn tail(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let from = text.len().saturating_sub(max);
    let from = (from..=text.len())
        .find(|&at| text.is_char_boundary(at))
        .unwrap_or(text.len());
    let rest = text.get(from..).unwrap_or_default();
    let rest = match rest.split_once('\n') {
        Some((_, after)) if !after.is_empty() => after,
        _ => rest,
    };
    (rest, true)
}

/// A time as `0.012 s`, `12.3 s` or `6 min 53 s`.
fn duration(seconds: f64) -> String {
    if seconds < 1.0 {
        format!("{seconds:.3} s")
    } else if seconds < 59.95 {
        format!("{seconds:.1} s")
    } else {
        let whole = seconds.round();
        let minutes = (whole / 60.0).floor();
        let rest = whole % 60.0;
        format!("{minutes:.0} min {rest:.0} s")
    }
}

/// `1 test` or `n tests`, for any noun with a plain plural.
fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// The headline of a failed test: the first line nextest found, or its
/// description of the failure.
fn headline(case: &Case) -> &str {
    case.attempts.first().map_or("", |attempt| {
        if attempt.message.is_empty() {
            &attempt.kind
        } else {
            &attempt.message
        }
    })
}

const STYLE: &str = r"
:root {
  color-scheme: light dark;
  --text: #1f2328; --muted: #59636e; --line: #d1d9e0; --panel: #f6f8fa; --page: #fff;
  --good: #1a7f37; --bad: #cf222e; --warn: #9a6700; --slow: #0969da;
}
@media (prefers-color-scheme: dark) {
  :root {
    --text: #f0f6fc; --muted: #9198a1; --line: #3d444d; --panel: #151b23; --page: #0d1117;
    --good: #3fb950; --bad: #f85149; --warn: #d29922; --slow: #4493f8;
  }
}
* { box-sizing: border-box; }
body {
  margin: 0 auto; padding: 24px 16px 64px; max-width: 1100px; background: var(--page);
  color: var(--text); font: 14px/1.5 system-ui, -apple-system, 'Segoe UI', sans-serif;
}
h1 { font-size: 24px; margin: 0 0 4px; }
h2 { font-size: 18px; margin: 32px 0 12px; }
code, pre, .time { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
code { font-size: 13px; overflow-wrap: anywhere; }
.meta, .time, .note { color: var(--muted); }
.time { font-size: 12px; white-space: nowrap; }
.totals { display: flex; flex-wrap: wrap; gap: 8px; margin: 16px 0 0; padding: 0; list-style: none; }
.totals li {
  min-width: 112px; padding: 8px 12px; border: 1px solid var(--line); border-radius: 6px;
  background: var(--panel);
}
.totals b { display: block; font-size: 22px; font-variant-numeric: tabular-nums; }
.totals a { color: inherit; text-decoration: none; }
.badge {
  display: inline-block; min-width: 72px; padding: 0 8px; border: 1px solid; border-radius: 999px;
  font-size: 12px; text-align: center; white-space: nowrap;
}
.passed { color: var(--good); }
.bad { color: var(--bad); }
.flaky { color: var(--warn); }
.slow { color: var(--slow); }
.skipped { color: var(--muted); }
details { margin: 8px 0; border: 1px solid var(--line); border-radius: 6px; }
details > summary { padding: 8px 12px; cursor: pointer; background: var(--panel); border-radius: 6px; }
details[open] > summary { border-bottom: 1px solid var(--line); border-radius: 6px 6px 0 0; }
details > .body { padding: 4px 12px 12px; }
details details > summary { background: none; }
.stream { margin: 12px 0 4px; color: var(--muted); font-size: 12px; }
pre {
  margin: 0; padding: 12px; overflow: auto; max-height: 480px; border: 1px solid var(--line);
  border-radius: 6px; background: var(--panel); font-size: 12px; line-height: 1.45;
}
table { width: 100%; border-collapse: collapse; }
th, td { padding: 4px 12px; border-bottom: 1px solid var(--line); text-align: left; vertical-align: top; }
tbody tr:last-child td { border-bottom: 0; }
th { color: var(--muted); font-weight: 600; }
td:first-child, th:first-child { width: 1%; }
td:last-child, th:last-child { width: 1%; text-align: right; }
.wrap { overflow-x: auto; }
.show > input { margin: 0 4px 0 0; vertical-align: -1px; }
.show > label { margin-right: 16px; }
.suites { margin-top: 12px; }
#show-bad:checked ~ .suites tbody tr:not(.is-bad),
#show-bad:checked ~ .suites details:not(.has-bad),
#show-flaky:checked ~ .suites tbody tr:not(.is-flaky),
#show-flaky:checked ~ .suites details:not(.has-flaky),
#show-slow:checked ~ .suites tbody tr:not(.is-slow),
#show-slow:checked ~ .suites details:not(.has-slow) { display: none; }
";

/// The HTML page.
fn page(report: &Report, title: &str) -> String {
    let totals = report.totals();
    let mut out = String::new();
    page_head(&mut out, report, &totals, title);
    let sections = [
        ("failed", "Failed", report.with_outcome(Outcome::Failed)),
        (
            "timed-out",
            "Timed out",
            report.with_outcome(Outcome::TimedOut),
        ),
        ("flaky", "Flaky", report.with_outcome(Outcome::Flaky)),
    ];
    for (id, heading, cases) in sections {
        if cases.is_empty() {
            continue;
        }
        let _ = writeln!(out, "<h2 id=\"{id}\">{heading} ({})</h2>", cases.len());
        if id == "flaky" {
            out.push_str(
                "<p class=\"note\">Passed on a retry. A flaky test is fixed, never hidden.</p>\n",
            );
        }
        for (suite, case) in cases {
            page_case(&mut out, suite, case);
        }
    }
    page_slow(&mut out, &report.slow());
    page_all(&mut out, report, &totals);
    out.push_str("</body>\n</html>\n");
    out
}

/// The document's head, the heading and the counts. A count links to its
/// section.
fn page_head(out: &mut String, report: &Report, totals: &Totals, title: &str) {
    let title = escape(&clean(title));
    let _ = write!(
        out,
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>Tests: {title}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n\
         <h1>Tests: {title}</h1>\n"
    );
    let _ = writeln!(
        out,
        "<p class=\"meta\">Started {}, took {}. nextest run <code>{}</code>.</p>",
        escape(&report.started),
        duration(report.seconds),
        escape(&report.id),
    );
    out.push_str("<ul class=\"totals\">\n");
    let tiles = [
        (totals.tests, "tests", "", "all"),
        (totals.passed, "passed", "passed", "all"),
        (totals.failed, "failed", "bad", "failed"),
        (totals.timed_out, "timed out", "bad", "timed-out"),
        (totals.flaky, "flaky", "flaky", "flaky"),
        (totals.slow, "slow", "slow", "slow"),
    ];
    for (count, label, class, target) in tiles {
        // A count of zero stays in the text's color and links nowhere.
        if count == 0 {
            let _ = writeln!(out, "<li><b>0</b>{label}</li>");
        } else {
            let _ = writeln!(
                out,
                "<li><a href=\"#{target}\"><b class=\"{class}\">{count}</b>{label}</a></li>"
            );
        }
    }
    out.push_str("</ul>\n");
    if totals.skipped > 0 {
        let _ = writeln!(
            out,
            "<p class=\"note\">{} skipped.</p>",
            plural(totals.skipped, "test")
        );
    }
}

const TABLE_HEAD: &str = "<div class=\"wrap\"><table>\n<thead><tr><th>Outcome</th><th>Test</th><th>Time</th></tr></thead>\n<tbody>\n";

/// The slow tests, whatever their outcome.
fn page_slow(out: &mut String, slow: &[(&Suite, &Case)]) {
    if slow.is_empty() {
        return;
    }
    let _ = writeln!(out, "<h2 id=\"slow\">Slow ({})</h2>", slow.len());
    let _ = writeln!(
        out,
        "<p class=\"note\">Took {SLOW_SECONDS:.0} s or more. The CI profile stops a test \
         after 15 such periods.</p>"
    );
    out.push_str(TABLE_HEAD);
    for (suite, case) in slow {
        page_row(out, Some(suite), case);
    }
    out.push_str("</tbody>\n</table></div>\n");
}

/// Every test, per binary, under the radio buttons that filter the rows.
fn page_all(out: &mut String, report: &Report, totals: &Totals) {
    out.push_str("<h2 id=\"all\">All tests</h2>\n<div class=\"show\">\n");
    let shows = [
        ("all", "All", totals.tests),
        (
            "bad",
            "Failed or timed out",
            totals.failed.saturating_add(totals.timed_out),
        ),
        ("flaky", "Flaky", totals.flaky),
        ("slow", "Slow", totals.slow),
    ];
    // The radio buttons are siblings of the suites and come before them,
    // so the style sheet can filter the rows without a script.
    for (id, label, count) in shows {
        let checked = if id == "all" { " checked" } else { "" };
        let _ = writeln!(
            out,
            "<input type=\"radio\" name=\"show\" id=\"show-{id}\"{checked}>\
             <label for=\"show-{id}\">{label} ({count})</label>"
        );
    }
    out.push_str("<div class=\"suites\">\n");
    for suite in &report.suites {
        page_suite(out, suite);
    }
    out.push_str("</div>\n</div>\n");
}

/// A failed, timed-out or flaky test with the output of its attempts.
fn page_case(out: &mut String, suite: &Suite, case: &Case) {
    let open = if case.outcome.is_bad() { " open" } else { "" };
    let _ = writeln!(
        out,
        "<details{open}>\n<summary><span class=\"badge {}\">{}</span> <code>{}</code> \
         <code><b>{}</b></code> <span class=\"time\">{}, {}</span></summary>\n<div class=\"body\">",
        case.outcome.class(),
        case.outcome.label(),
        escape(&suite.name),
        escape(&case.name),
        duration(case.seconds),
        plural(case.tries(), "attempt"),
    );
    for (index, attempt) in case.attempts.iter().enumerate() {
        // Later attempts usually repeat the first one, so they start closed.
        if index == 0 {
            page_attempt(out, attempt);
        } else {
            let _ = writeln!(
                out,
                "<details>\n<summary>Attempt {}: {}</summary>\n<div class=\"body\">",
                index.saturating_add(1),
                escape(&attempt.kind),
            );
            page_attempt(out, attempt);
            out.push_str("</div>\n</details>\n");
        }
    }
    out.push_str("</div>\n</details>\n");
}

fn page_attempt(out: &mut String, attempt: &Attempt) {
    let headline = if attempt.message.is_empty() {
        &attempt.kind
    } else {
        &attempt.message
    };
    let _ = writeln!(out, "<p class=\"bad\">{}</p>", escape(headline));
    // The failure's text is the end of standard error when there is one.
    let streams = [
        ("failure", &attempt.text, attempt.stderr.is_empty()),
        ("stdout", &attempt.stdout, true),
        ("stderr", &attempt.stderr, true),
    ];
    for (name, text, shown) in streams {
        if !shown || text.trim().is_empty() {
            continue;
        }
        let (text, cut) = tail(text, PAGE_OUTPUT_BYTES);
        let note = if cut { ", the end only" } else { "" };
        let _ = writeln!(
            out,
            "<p class=\"stream\">{name}{note}</p>\n<pre>{}</pre>",
            escape(text.trim_matches('\n'))
        );
    }
}

/// A table row for `case`, with its binary in front when `suite` is given.
fn page_row(out: &mut String, suite: Option<&Suite>, case: &Case) {
    let mut classes = String::new();
    if case.outcome.is_bad() {
        classes.push_str("is-bad ");
    }
    if case.outcome == Outcome::Flaky {
        classes.push_str("is-flaky ");
    }
    if case.is_slow() {
        classes.push_str("is-slow ");
    }
    let suite = suite.map_or_else(String::new, |suite| {
        format!("<code>{}</code> ", escape(&suite.name))
    });
    let slow = if case.is_slow() { " slow" } else { "" };
    let _ = writeln!(
        out,
        "<tr class=\"{}\"><td><span class=\"badge {}\">{}</span></td><td>{suite}<code>{}</code></td>\
         <td class=\"time{slow}\">{}</td></tr>",
        classes.trim_end(),
        case.outcome.class(),
        case.outcome.label(),
        escape(&case.name),
        duration(case.seconds),
    );
}

/// Every test of one binary, open when one of them failed or timed out.
fn page_suite(out: &mut String, suite: &Suite) {
    let count = |test: fn(&Case) -> bool| suite.cases.iter().filter(|case| test(case)).count();
    let bad = count(|case| case.outcome.is_bad());
    let flaky = count(|case| case.outcome == Outcome::Flaky);
    let slow = count(Case::is_slow);
    let mut classes = String::new();
    let mut counts = plural(suite.cases.len(), "test");
    if bad > 0 {
        classes.push_str("has-bad ");
        let _ = write!(
            counts,
            ", <span class=\"bad\">{bad} failed or timed out</span>"
        );
    }
    if flaky > 0 {
        classes.push_str("has-flaky ");
        let _ = write!(counts, ", <span class=\"flaky\">{flaky} flaky</span>");
    }
    if slow > 0 {
        classes.push_str("has-slow ");
        let _ = write!(counts, ", <span class=\"slow\">{slow} slow</span>");
    }
    let open = if bad > 0 { " open" } else { "" };
    let total: f64 = suite.cases.iter().map(|case| case.seconds).sum();
    let _ = writeln!(
        out,
        "<details class=\"{}\"{open}>\n<summary><code><b>{}</b></code> {counts} \
         <span class=\"time\">{}</span></summary>\n<div class=\"wrap\"><table>\n\
         <thead><tr><th>Outcome</th><th>Test</th><th>Time</th></tr></thead>\n<tbody>",
        classes.trim_end(),
        escape(&suite.name),
        duration(total),
    );
    for case in &suite.cases {
        page_row(out, None, case);
    }
    out.push_str("</tbody>\n</table></div>\n</details>\n");
}

/// The Markdown summary. `page_name` is the HTML page's file name, which
/// is also its artifact's name in a CI run.
fn markdown(report: &Report, title: &str, page_name: Option<&str>) -> String {
    let totals = report.totals();
    let mut out = String::new();
    let _ = writeln!(out, "### Tests: {}\n", escape_markdown(&clean(title)));
    out.push_str("| Tests | Passed | Failed | Timed out | Flaky | Slow | Time |\n");
    out.push_str("| ----: | -----: | -----: | --------: | ----: | ---: | ---: |\n");
    let _ = writeln!(
        out,
        "| {} | {} | {} | {} | {} | {} | {} |\n",
        totals.tests,
        totals.passed,
        totals.failed,
        totals.timed_out,
        totals.flaky,
        totals.slow,
        duration(report.seconds),
    );
    let mut bad = report.with_outcome(Outcome::Failed);
    bad.extend(report.with_outcome(Outcome::TimedOut));
    markdown_bad(&mut out, &bad);
    markdown_flaky(&mut out, &report.with_outcome(Outcome::Flaky));
    markdown_slow(&mut out, &report.slow());
    if let Some(page_name) = page_name {
        let _ = writeln!(
            out,
            "Every test is in the artifact `{page_name}`, which opens in the browser.\n"
        );
    }
    out
}

/// A test's binary and name, as they are written in a summary.
fn markdown_name(suite: &Suite, case: &Case) -> String {
    format!(
        "<code>{}</code> <code>{}</code>",
        escape_markdown(&suite.name),
        escape_markdown(&case.name)
    )
}

/// The tests that failed or timed out: the first [`SUMMARY_DETAILED`] with
/// the end of their first attempt's output, then [`SUMMARY_NAMED`] names.
fn markdown_bad(out: &mut String, bad: &[(&Suite, &Case)]) {
    if bad.is_empty() {
        return;
    }
    let _ = writeln!(out, "#### Failed or timed out ({})\n", bad.len());
    for (suite, case) in bad.iter().take(SUMMARY_DETAILED) {
        let _ = writeln!(
            out,
            "<details><summary>{} ({}, {}): {}</summary>\n",
            markdown_name(suite, case),
            duration(case.seconds),
            plural(case.tries(), "attempt"),
            escape_markdown(headline(case)),
        );
        if let Some(attempt) = case.attempts.first() {
            for (stream, text) in [("stdout", &attempt.stdout), ("stderr", &attempt.stderr)] {
                let (text, _) = tail(text, SUMMARY_OUTPUT_BYTES);
                if !text.trim().is_empty() {
                    let _ = writeln!(
                        out,
                        "The end of {stream}:\n\n<pre>{}</pre>\n",
                        escape(text.trim_matches('\n'))
                    );
                }
            }
        }
        out.push_str("</details>\n\n");
    }
    for (suite, case) in bad.iter().skip(SUMMARY_DETAILED).take(SUMMARY_NAMED) {
        let _ = writeln!(
            out,
            "- {} ({})",
            markdown_name(suite, case),
            case.outcome.label()
        );
    }
    let listed = SUMMARY_DETAILED.saturating_add(SUMMARY_NAMED);
    if bad.len() > listed {
        let _ = writeln!(out, "- and {} more", bad.len().saturating_sub(listed));
    }
    out.push('\n');
}

fn markdown_flaky(out: &mut String, flaky: &[(&Suite, &Case)]) {
    if flaky.is_empty() {
        return;
    }
    let _ = writeln!(out, "#### Flaky ({})\n", flaky.len());
    for (suite, case) in flaky.iter().take(SUMMARY_NAMED) {
        let _ = writeln!(
            out,
            "- {} passed on attempt {}",
            markdown_name(suite, case),
            case.tries()
        );
    }
    if flaky.len() > SUMMARY_NAMED {
        let _ = writeln!(
            out,
            "- and {} more",
            flaky.len().saturating_sub(SUMMARY_NAMED)
        );
    }
    out.push('\n');
}

/// The [`SUMMARY_SLOW`] slowest of the slow tests.
fn markdown_slow(out: &mut String, slow: &[(&Suite, &Case)]) {
    if slow.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "#### Slow ({})\n\n| Test | Time |\n| ---- | ---: |",
        slow.len()
    );
    for (suite, case) in slow.iter().take(SUMMARY_SLOW) {
        let _ = writeln!(
            out,
            "| {} | {} |",
            markdown_name(suite, case),
            duration(case.seconds)
        );
    }
    out.push('\n');
    if slow.len() > SUMMARY_SLOW {
        let _ = writeln!(
            out,
            "The {SUMMARY_SLOW} slowest of {} are shown.\n",
            slow.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run of nextest 0.9.151 under a `ci`-like profile, shortened: one
    /// test of each outcome, with retries.
    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="6" skipped="0" failures="2" errors="0" uuid="b641e4d4-17a9-4501-a6d5-e952e4db4201" timestamp="2026-11-14T05:36:05.217+03:00" time="912.033">
    <testsuite name="cheby_cli::ui" tests="3" skipped="0" errors="0" failures="1">
        <testcase name="[pass] 05-expressions/a_b" classname="cheby_cli::ui" timestamp="2026-11-14T05:36:05.217+03:00" time="0.006"/>
        <testcase name="tests::flaky" classname="cheby_cli::ui" timestamp="2026-11-14T05:36:05.227+03:00" time="0.006">
            <flakyFailure timestamp="2026-11-14T05:36:05.217+03:00" time="0.007" message="thread &apos;tests::flaky&apos; panicked at src/lib.rs:10:101" type="test failure with exit code 101">thread &apos;tests::flaky&apos; panicked at src/lib.rs:10:101:
first try fails
                <system-out>
running 1 test
test tests::flaky ... FAILED
</system-out>
                <system-err>
first try fails
</system-err>
            </flakyFailure>
        </testcase>
        <testcase name="tests::fails" classname="cheby_cli::ui" timestamp="2026-11-14T05:36:05.217+03:00" time="0.009">
            <failure message="thread &apos;tests::fails&apos; panicked at src/lib.rs:6:69" type="test failure with exit code 101">assertion `left == right` failed: one is not &lt;two&gt; &amp; &quot;so&quot;</failure>
            <rerunFailure timestamp="2026-11-14T05:36:05.227+03:00" time="0.006" message="second" type="test failure with exit code 101">second text
                <system-out>second stdout</system-out>
                <system-err>second stderr</system-err>
            </rerunFailure>
            <rerunFailure timestamp="2026-11-14T05:36:05.235+03:00" time="0.006" message="third" type="test failure with exit code 101">third text
                <system-out>third stdout</system-out>
                <system-err>third stderr</system-err>
            </rerunFailure>
            <system-out>first stdout &lt;b&gt;</system-out>
            <system-err>first stderr</system-err>
        </testcase>
    </testsuite>
    <testsuite name="cheby_driver" tests="3" skipped="0" errors="0" failures="1">
        <testcase name="live_matches_check" classname="cheby_driver" timestamp="2026-11-14T05:36:05.217+03:00" time="413.2"/>
        <testcase name="sweep_sample" classname="cheby_driver" timestamp="2026-11-14T05:36:05.217+03:00" time="60"/>
        <testcase name="hangs" classname="cheby_driver" timestamp="2026-11-14T05:36:05.217+03:00" time="900.009">
            <failure type="test timeout"/>
            <rerunFailure timestamp="2026-11-14T05:36:09.228+03:00" time="900.010" type="test timeout">
                <system-out>
running 1 test
</system-out>
                <system-err></system-err>
            </rerunFailure>
            <system-out>
running 1 test
</system-out>
            <system-err></system-err>
        </testcase>
    </testsuite>
</testsuites>
"#;

    fn sample() -> Report {
        Report::parse(SAMPLE).unwrap()
    }

    fn find<'a>(report: &'a Report, name: &str) -> &'a Case {
        report
            .cases()
            .map(|(_, case)| case)
            .find(|case| case.name == name)
            .unwrap()
    }

    #[test]
    fn counts_each_outcome_once() {
        let totals = sample().totals();
        assert_eq!(
            totals,
            Totals {
                tests: 6,
                passed: 4,
                failed: 1,
                timed_out: 1,
                flaky: 1,
                slow: 3,
                skipped: 0,
            }
        );
        assert_eq!(
            totals.line(),
            "6 tests: 4 passed, 1 failed, 1 timed out; 1 flaky, 3 slow"
        );
    }

    /// The first attempt's output is the test case's own, and each retry
    /// holds its own.
    #[test]
    fn a_failed_test_keeps_every_attempt_in_order() {
        let report = sample();
        let case = find(&report, "tests::fails");
        assert_eq!(case.outcome, Outcome::Failed);
        assert_eq!(case.tries(), 3);
        let stdout: Vec<_> = case.attempts.iter().map(|a| a.stdout.as_str()).collect();
        assert_eq!(
            stdout,
            ["first stdout <b>", "second stdout", "third stdout"]
        );
        assert_eq!(
            case.attempts[0].text,
            "assertion `left == right` failed: one is not <two> & \"so\""
        );
        assert_eq!(case.attempts[1].text, "second text");
        assert_eq!(
            headline(case),
            "thread 'tests::fails' panicked at src/lib.rs:6:69"
        );
    }

    #[test]
    fn a_flaky_test_passed_after_its_failed_attempts() {
        let report = sample();
        let case = find(&report, "tests::flaky");
        assert_eq!(case.outcome, Outcome::Flaky);
        assert_eq!(case.tries(), 2);
        assert_eq!(case.attempts.len(), 1);
        assert_eq!(case.attempts[0].stderr.trim(), "first try fails");
    }

    #[test]
    fn a_timeout_is_not_a_plain_failure() {
        let report = sample();
        let case = find(&report, "hangs");
        assert_eq!(case.outcome, Outcome::TimedOut);
        assert_eq!(case.tries(), 2);
        assert_eq!(headline(case), "test timeout");
    }

    /// The slow tests come slowest first, whatever their outcome, and the
    /// threshold itself counts.
    #[test]
    fn slow_tests_are_ordered_by_time() {
        let report = sample();
        let names: Vec<_> = report
            .slow()
            .iter()
            .map(|(_, case)| case.name.as_str())
            .collect();
        assert_eq!(names, ["hangs", "live_matches_check", "sweep_sample"]);
    }

    /// D-1558: the threshold here is the `ci` profile's period, which
    /// nextest's report does not carry.
    #[test]
    fn the_slow_threshold_is_the_ci_profiles_period() {
        let config =
            std::fs::read_to_string(cheby_test_support::repo_root().join(".config/nextest.toml"))
                .unwrap();
        let period = format!("slow-timeout = {{ period = \"{SLOW_SECONDS:.0}s\"");
        assert!(config.contains(&period), "no `{period}` in nextest.toml");
    }

    #[test]
    fn the_page_escapes_names_and_output() {
        let html = page(&sample(), "linux <x86_64>");
        assert!(html.contains("<title>Tests: linux &lt;x86_64&gt;</title>"));
        assert!(html.contains("<pre>first stdout &lt;b&gt;</pre>"));
        assert!(!html.contains("one is not &lt;two&gt;"));
        assert!(html.contains("<code>[pass] 05-expressions/a_b</code>"));
        assert!(!html.contains("<script"));
        // Failures are open, flaky tests and later attempts are closed.
        assert_eq!(html.matches("<details open>").count(), 2);
        assert!(html.contains("<summary>Attempt 3: test failure with exit code 101</summary>"));
        assert!(html.contains("<details class=\"has-bad has-flaky\" open>"));
        assert!(html.contains("<details class=\"has-bad has-slow\" open>"));
        assert!(html.contains("<tr class=\"is-bad is-slow\">"));
    }

    /// The failure's own text is shown only when standard error is empty,
    /// since it is the end of standard error otherwise.
    #[test]
    fn the_failure_text_is_shown_without_stderr() {
        let xml = SAMPLE.replace("<system-err>first stderr</system-err>", "");
        let html = page(&Report::parse(&xml).unwrap(), "t");
        assert!(html.contains("one is not &lt;two&gt; &amp; &quot;so&quot;"));
    }

    #[test]
    fn the_summary_keeps_names_literal() {
        let text = markdown(&sample(), "linux-x86_64", Some("junit-linux-x86_64.html"));
        assert!(text.starts_with("### Tests: linux-x86&#95;64\n\n| Tests |"));
        assert!(text.contains("| 6 | 4 | 1 | 1 | 1 | 3 | 15 min 12 s |"));
        assert!(text.contains("#### Failed or timed out (2)"));
        assert!(text.contains(
            "<code>cheby&#95;cli::ui</code> <code>tests::fails</code> (0.009 s, 3 attempts)"
        ));
        assert!(text.contains("<pre>first stdout &lt;b&gt;</pre>"));
        assert!(text.contains(
            "- <code>cheby&#95;cli::ui</code> <code>tests::flaky</code> passed on attempt 2"
        ));
        assert!(text.contains(
            "| <code>cheby&#95;driver</code> <code>live&#95;matches&#95;check</code> | 6 min 53 s |"
        ));
        assert!(text.contains("the artifact `junit-linux-x86_64.html`"));
    }

    /// A summary stays far under GitHub's limit of 1 MiB per step, however
    /// many tests fail and however much they print.
    #[test]
    fn the_summary_is_capped() {
        let output = "x".repeat(10_000);
        let case = format!(
            "<testcase name=\"t\" time=\"70\"><failure type=\"f\"/>\
             <system-out>{output}</system-out><system-err>{output}</system-err></testcase>"
        );
        let xml = format!(
            "<testsuites><testsuite name=\"s\">{}</testsuite></testsuites>",
            case.repeat(500)
        );
        let text = markdown(&Report::parse(&xml).unwrap(), "t", None);
        assert!(text.len() < 400 * 1024, "{} bytes", text.len());
        assert!(text.contains("- and 270 more"));
        assert!(text.contains("The 20 slowest of 500 are shown."));
    }

    #[test]
    fn clean_drops_escape_sequences_and_control_characters() {
        assert_eq!(clean("\u{1b}[1;31mred\u{1b}[0m\r\n\tx\u{7}"), "red\n\tx");
    }

    #[test]
    fn tail_cuts_at_a_line_and_at_a_character() {
        assert_eq!(tail("one\ntwo\nthree", 100), ("one\ntwo\nthree", false));
        assert_eq!(tail("one\ntwo\nthree", 7), ("three", true));
        assert_eq!(tail("ααα", 3), ("α", true));
        assert_eq!(tail("abcdef\n", 3), ("ef\n", true));
    }

    #[test]
    fn durations_read_as_text() {
        assert_eq!(duration(0.0123), "0.012 s");
        assert_eq!(duration(12.34), "12.3 s");
        assert_eq!(duration(59.96), "1 min 0 s");
        assert_eq!(duration(413.2), "6 min 53 s");
        assert_eq!(duration(3599.6), "60 min 0 s");
    }

    #[test]
    fn a_report_that_is_not_junit_is_an_error() {
        assert!(Report::parse("<html/>").unwrap_err().contains("testsuites"));
        assert!(Report::parse("not xml").is_err());
    }
}
