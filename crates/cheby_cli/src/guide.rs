//! The guide for coding agents that the toolchain ships (D-1952):
//! `docs/guide/agents.md`, embedded in the binary as the explanations of
//! `cheby explain` are (D-420). `cheby guide` prints it, and `cheby new`
//! writes it as the new package's `AGENTS.md`.
//!
//! The guide cannot go stale unseen: a test here parses every `cheby …`
//! command it shows with the real argument parser, and `tests/guide.rs`
//! checks, formats and tests each of its code blocks.

/// The guide's text.
pub(crate) const GUIDE: &str = include_str!("../../../docs/guide/agents.md");

/// The file `cheby new` writes the guide to: the name coding agents read
/// their instructions from.
pub(crate) const FILE: &str = "AGENTS.md";

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::GUIDE;

    /// The `cheby …` commands the guide shows: the lines of its `sh`
    /// blocks, without their `#` comments, and the code spans of its text
    /// that start with `cheby `.
    fn commands() -> Vec<String> {
        let mut out = Vec::new();
        let mut in_shell = false;
        let mut in_block = false;
        for line in GUIDE.lines() {
            if let Some(info) = line.strip_prefix("```") {
                in_shell = !in_block && info.trim() == "sh";
                in_block = !in_block;
                continue;
            }
            if in_shell {
                let command = line.split(" #").next().unwrap_or_default().trim();
                if command.starts_with("cheby ") {
                    out.push(command.to_owned());
                }
            } else if !in_block {
                out.extend(
                    line.split('`')
                        .skip(1)
                        .step_by(2)
                        .filter(|span| span.starts_with("cheby "))
                        .map(str::to_owned),
                );
            }
        }
        out
    }

    /// Every command of the guide is one the argument parser accepts, so a
    /// renamed command or a removed flag fails here.
    #[test]
    fn every_command_of_the_guide_parses() {
        let commands = commands();
        assert!(commands.len() > 12, "{commands:?}");
        for command in commands {
            let parsed = crate::Cli::try_parse_from(command.split_whitespace());
            assert!(parsed.is_ok(), "{command}: {}", parsed.unwrap_err());
        }
    }

    /// The guide is for a user's package: it names nothing of this
    /// repository, and no decision number, which a user cannot look up.
    #[test]
    fn the_guide_stands_alone() {
        for word in ["cargo ", "xtask", "docs/", "D-0", "D-1", "AGENTS.md"] {
            assert!(!GUIDE.contains(word), "{word}");
        }
        assert!(GUIDE.starts_with("# "));
        assert!(GUIDE.ends_with('\n') && !GUIDE.ends_with("\n\n"));
    }
}
