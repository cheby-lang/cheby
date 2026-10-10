//! `cargo xtask check-examples` (G6, plan §12): runs `cheby check` in every
//! example directory, as a user would, and compares the outcome with the
//! one the milestone expects.
//!
//! - Every example checks cleanly: exit status 0 and no diagnostic at all,
//!   warnings included (D-477).
//! - The later-tier examples, which import standard-library modules that
//!   are planned but not shipped yet, fail with E0038 for those imports
//!   only (D-451, D-480); warnings are allowed.
//! - `030_shop_service` fails with E0036 only: its git dependency resolves
//!   once fetching exists (D-446).

use std::path::Path;
use std::process::Command;

use cheby_test_support::example;

/// The examples that import unshipped standard-library modules.
const LATER_TIER: [&str; 8] = [
    "005_prime_sieve",
    "009_counter_actor",
    "013_dining_philosophers",
    "018_worker_pool",
    "019_supervisor",
    "022_bank_accounts",
    "028_money_and_rationals",
    "031_web_scraper",
];

/// The example whose dependency cannot resolve yet.
const DEPENDENCY: &str = "030_shop_service";

/// What `cheby check` printed: the code and message of each diagnostic.
struct Outcome {
    success: bool,
    errors: Vec<(String, String)>,
    warnings: Vec<(String, String)>,
    text: String,
}

pub fn run(root: &Path) -> Result<(), String> {
    let cheby = crate::build_cheby(root)?;
    let mut failures = Vec::new();
    let examples = example::all(root)?;
    for ex in &examples {
        let outcome = check(&cheby, &ex.dir)?;
        let verdict = judge(&ex.name, &outcome);
        match &verdict {
            Ok(note) => println!("ok    {}{note}", ex.name),
            Err(why) => {
                println!("FAIL  {}: {why}", ex.name);
                for line in outcome.text.lines().take(20) {
                    println!("        {line}");
                }
                failures.push(ex.name.clone());
            }
        }
    }
    println!(
        "check-examples: {} examples, {} as expected",
        examples.len(),
        examples.len().saturating_sub(failures.len())
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "unexpected `cheby check` results: {}",
            failures.join(", ")
        ))
    }
}

fn check(cheby: &Path, dir: &Path) -> Result<Outcome, String> {
    let out = Command::new(cheby)
        .args(["--color=never", "check"])
        .current_dir(dir)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", cheby.display()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for line in text.lines() {
        if let Some(found) = header(line, "error[") {
            errors.push(found);
        } else if let Some(found) = header(line, "warning[") {
            warnings.push(found);
        }
    }
    Ok(Outcome {
        success: out.status.success(),
        errors,
        warnings,
        text,
    })
}

/// The code and message of a diagnostic's first line, such as
/// `error[E0038]: the module ...`.
fn header(line: &str, prefix: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix(prefix)?;
    let (code, message) = rest.split_once("]: ")?;
    Some((code.to_owned(), message.to_owned()))
}

/// Whether `outcome` is what `name` should give, with a note for the
/// expected failures.
fn judge(name: &str, outcome: &Outcome) -> Result<String, String> {
    if name == DEPENDENCY {
        let codes: Vec<&str> = outcome.errors.iter().map(|(c, _)| c.as_str()).collect();
        return if !outcome.success && !codes.is_empty() && codes.iter().all(|&c| c == "E0036") {
            Ok(" (E0036: its dependency cannot be fetched yet)".to_owned())
        } else {
            Err(format!("expected only E0036, got {codes:?}"))
        };
    }
    if LATER_TIER.contains(&name) {
        if outcome.success || outcome.errors.is_empty() {
            return Err("a later-tier example checks cleanly: remove it from the list".to_owned());
        }
        let unexpected: Vec<String> = outcome
            .errors
            .iter()
            .filter(|(code, message)| {
                code != "E0038"
                    || !(message.starts_with("the module `std::")
                        && message.ends_with("` is not supported yet"))
            })
            .map(|(code, message)| format!("{code}: {message}"))
            .collect();
        return if unexpected.is_empty() {
            Ok(format!(
                " ({} unshipped std imports, E0038)",
                outcome.errors.len()
            ))
        } else {
            Err(format!(
                "errors other than unshipped imports: {unexpected:?}"
            ))
        };
    }
    if !outcome.success {
        return Err("`cheby check` failed".to_owned());
    }
    if !outcome.errors.is_empty() || !outcome.warnings.is_empty() {
        let all: Vec<String> = outcome
            .errors
            .iter()
            .chain(&outcome.warnings)
            .map(|(code, message)| format!("{code}: {message}"))
            .collect();
        return Err(format!("expected no diagnostics (D-477), got {all:?}"));
    }
    Ok(String::new())
}
