//! `cargo xtask check-examples` (G6, plan §12): runs `cheby check` in every
//! example directory, as a user would, and compares the outcome with the
//! one the milestone expects.
//!
//! - Every example checks cleanly: exit status 0 and no diagnostic at all,
//!   warnings included (D-477, D-663), except `022_bank_accounts`, which
//!   has exactly one W0009 at `src/money.cheby`, for its deliberate `todo`
//!   (D-068, D-663).
//! - The later-tier examples, which import standard-library modules that
//!   are planned but not shipped yet, fail with E0038 for those imports
//!   only (D-451, D-480); warnings are allowed.
//! - `030_shop_service` fails with E0036 only: its git dependency resolves
//!   once fetching exists (D-446).

use std::path::Path;
use std::process::Command;

use cheby_test_support::example;

/// The examples that import unshipped standard-library modules. M2's first
/// slice shipped `std::fiber`, `std::channel` and `std::duration`, so 005,
/// 018, 022 and 028 left the list (D-729), and with `std::selector`,
/// `std::random` and `std::runtime` 009, 013 and 019 left it too.
const LATER_TIER: [&str; 1] = ["031_web_scraper"];

/// The example whose dependency cannot resolve yet.
const DEPENDENCY: &str = "030_shop_service";

/// The example whose clean check allows one W0009, and the file that has
/// it (D-663).
const TODO_EXAMPLE: (&str, &str) = ("022_bank_accounts", "src/money.cheby");

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
    let mut cmd = Command::new(cheby);
    cmd.args(["--color=never", "check"]).current_dir(dir);
    crate::cache::apply(&mut cmd)?;
    let out = cmd
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
    if name == TODO_EXAMPLE.0 {
        return judge_todo_example(outcome);
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

/// D-663's exception to the clean check: `022_bank_accounts` keeps the
/// `todo` of `money::apply_interest`, so its check has exactly one
/// diagnostic, a W0009 at `src/money.cheby`, and nothing else.
fn judge_todo_example(outcome: &Outcome) -> Result<String, String> {
    let codes: Vec<&str> = outcome
        .errors
        .iter()
        .chain(&outcome.warnings)
        .map(|(code, _)| code.as_str())
        .collect();
    let at_money = outcome.text.lines().any(|line| {
        line.trim_start()
            .strip_prefix("--> ")
            .is_some_and(|place| place.starts_with(&format!("{}:", TODO_EXAMPLE.1)))
    });
    if outcome.errors.is_empty() && codes == ["W0009"] && at_money {
        Ok(format!(" (its one W0009 at {}, D-663)", TODO_EXAMPLE.1))
    } else {
        Err(format!(
            "expected exactly one W0009 at {} (D-663), got {codes:?}",
            TODO_EXAMPLE.1
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(errors: &[&str], warnings: &[&str], text: &str) -> Outcome {
        let pairs = |codes: &[&str]| {
            codes
                .iter()
                .map(|c| ((*c).to_owned(), "message".to_owned()))
                .collect()
        };
        Outcome {
            success: errors.is_empty(),
            errors: pairs(errors),
            warnings: pairs(warnings),
            text: text.to_owned(),
        }
    }

    const MONEY: &str = "warning[W0009]: `todo` left in the code\n --> src/money.cheby:40:3\n";

    /// D-663: 022 may have one W0009 at `src/money.cheby`, and nothing
    /// else; every other example has no diagnostic at all.
    #[test]
    fn only_022_may_warn_and_only_once_in_money() {
        assert!(judge("022_bank_accounts", &outcome(&[], &["W0009"], MONEY)).is_ok());
        let elsewhere = "warning[W0009]: `todo`\n --> src/bank.cheby:4:3\n";
        assert!(judge("022_bank_accounts", &outcome(&[], &["W0009"], elsewhere)).is_err());
        let twice = format!("{MONEY}{MONEY}");
        let two = outcome(&[], &["W0009", "W0009"], &twice);
        assert!(judge("022_bank_accounts", &two).is_err());
        let other = outcome(&[], &["W0002", "W0009"], MONEY);
        assert!(judge("022_bank_accounts", &other).is_err());
        assert!(judge("022_bank_accounts", &outcome(&[], &[], "")).is_err());
        assert!(judge("021_other", &outcome(&[], &["W0009"], MONEY)).is_err());
        assert!(judge("021_other", &outcome(&[], &[], "")).is_ok());
    }

    /// The examples of M2's slices are judged by the clean rule, and the
    /// rest of the later tier still expects only E0038 (D-729).
    #[test]
    fn the_slice_examples_left_the_later_tier() {
        for name in [
            "005_prime_sieve",
            "009_counter_actor",
            "013_dining_philosophers",
            "018_worker_pool",
            "019_supervisor",
            "022_bank_accounts",
            "028_money_and_rationals",
        ] {
            assert!(!LATER_TIER.contains(&name), "{name}");
        }
        let unshipped = "error[E0038]: the module `std::http` is not supported yet\n";
        let e0038 = Outcome {
            success: false,
            errors: vec![(
                "E0038".to_owned(),
                "the module `std::http` is not supported yet".to_owned(),
            )],
            warnings: Vec::new(),
            text: unshipped.to_owned(),
        };
        assert!(judge("031_web_scraper", &e0038).is_ok());
        let e0042 = outcome(&["E0042"], &[], "error[E0042]: ...\n");
        assert!(judge("031_web_scraper", &e0042).is_err());
        // A former later-tier example that checks cleanly is judged clean.
        assert!(judge("009_counter_actor", &outcome(&[], &[], "")).is_ok());
        assert!(judge("009_counter_actor", &e0038).is_err());
    }
}
