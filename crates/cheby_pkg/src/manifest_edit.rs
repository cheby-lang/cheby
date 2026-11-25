//! Adding a dependency to the text of `cheby.toml`, which keeps everything
//! else as it is written (D-1882).

use cheby_package::{GitReference, GitUrl};
use toml::de::DeTable;

use crate::{PkgError, toml_string};

/// The entry of a git dependency.
fn entry(url: &GitUrl, reference: &GitReference) -> String {
    let (field, text) = match reference {
        GitReference::Tag(tag) => ("tag", tag),
        GitReference::Rev(rev) => ("rev", rev),
    };
    format!(
        "{{ git = {}, {field} = {} }}",
        toml_string(url.source()),
        toml_string(text)
    )
}

/// `text` with the dependency `key`. An entry of that key must be one of
/// the same repository, and is replaced.
pub(crate) fn add(
    text: &str,
    key: &str,
    url: &GitUrl,
    reference: &GitReference,
) -> Result<String, PkgError> {
    let by_hand = || {
        PkgError::new("cannot add to the `dependencies` of this `cheby.toml`").with_help(format!(
            "add the line `{key} = {}` yourself, then run `cheby update`",
            entry(url, reference)
        ))
    };
    let parsed =
        DeTable::parse(text).map_err(|_| PkgError::new("`cheby.toml` is not valid TOML"))?;
    let Some(table) = parsed.get_ref().get("dependencies") else {
        let gap = if text.is_empty() || text.ends_with("\n\n") {
            ""
        } else if text.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        return Ok(format!(
            "{text}{gap}[dependencies]\n{key} = {}\n",
            entry(url, reference)
        ));
    };
    let table = table.get_ref().as_table().ok_or_else(by_hand)?;
    if let Some(old) = table.get(key) {
        let same = old
            .get_ref()
            .as_table()
            .and_then(|t| t.get("git"))
            .and_then(|v| v.get_ref().as_str())
            .and_then(|v| GitUrl::parse(v).ok())
            .is_some_and(|old| old.canonical() == url.canonical());
        if !same {
            return Err(
                PkgError::new(format!("`cheby.toml` has another dependency `{key}`"))
                    .with_help("choose another key with `--name`"),
            );
        }
        let span = old.span();
        let is_inline = text.get(span.clone()).is_some_and(|t| t.starts_with('{'));
        let (before, after) = (text.get(..span.start), text.get(span.end..));
        return match (before, after) {
            (Some(before), Some(after)) if is_inline => {
                Ok(format!("{before}{}{after}", entry(url, reference)))
            }
            _ => Err(by_hand()),
        };
    }
    // The line after the header, or after the last entry of the table. A
    // comment after these may be about the next table.
    let mut at = None;
    let mut offset = 0_usize;
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        let end = offset.saturating_add(line.len());
        let trimmed = line.trim();
        if trimmed.starts_with('[') && !inside {
            let name = trimmed.split('#').next().unwrap_or("").trim();
            inside = name == "[dependencies]";
        } else if trimmed.starts_with('[') {
            break;
        }
        if inside && !trimmed.is_empty() && !trimmed.starts_with('#') {
            at = Some(end);
        }
        offset = end;
    }
    let at = at.ok_or_else(by_hand)?;
    let (Some(before), Some(after)) = (text.get(..at), text.get(at..)) else {
        return Err(by_hand());
    };
    let gap = if before.ends_with('\n') { "" } else { "\n" };
    Ok(format!(
        "{before}{gap}{key} = {}\n{after}",
        entry(url, reference)
    ))
}

#[cfg(test)]
mod tests {
    use cheby_package::{GitReference, GitUrl};

    use super::add;

    const HEAD: &str = "name = \"app\"\nversion = \"0.1.0\"\n";
    const ENTRY: &str = "json = { git = \"https://h/json\", tag = \"v1.0.0\" }\n";

    fn added(text: &str) -> Result<String, String> {
        let url = GitUrl::parse("https://h/json").unwrap();
        add(text, "json", &url, &GitReference::Tag("v1.0.0".to_owned()))
            .map_err(|e| e.message().to_owned())
    }

    #[test]
    fn a_table_is_made_or_extended() {
        assert_eq!(
            added(HEAD).unwrap(),
            format!("{HEAD}\n[dependencies]\n{ENTRY}")
        );
        assert_eq!(
            added(HEAD.trim_end()).unwrap(),
            format!("{HEAD}\n[dependencies]\n{ENTRY}")
        );
        let with =
            format!("{HEAD}\n[dependencies] # ours\n# a comment\na = {{ path = \"../a\" }}\n");
        assert_eq!(added(&with).unwrap(), format!("{with}{ENTRY}"));
        let tail = "\n[runtime]\nmain-stack = \"64MiB\"\n";
        assert_eq!(
            added(&format!("{with}{tail}")).unwrap(),
            format!("{with}{ENTRY}{tail}")
        );
        assert_eq!(
            added(&format!("{HEAD}\n[dependencies]\n{tail}")).unwrap(),
            format!("{HEAD}\n[dependencies]\n{ENTRY}{tail}")
        );
        // A comment above the next table stays there.
        let commented = format!("\n# The program's stacks.\n{}", tail.trim_start());
        for table in [
            "[dependencies]\n",
            "[dependencies]\na = { path = \"../a\" }\n",
        ] {
            assert_eq!(
                added(&format!("{HEAD}\n{table}{commented}")).unwrap(),
                format!("{HEAD}\n{table}{ENTRY}{commented}")
            );
        }
    }

    #[test]
    fn an_entry_is_replaced_only_by_its_repository() {
        let old = |entry: &str| format!("{HEAD}\n[dependencies]\njson = {entry} # pinned\n");
        assert_eq!(
            added(&old("{ git = \"git@h:json.git\", rev = \"aaaaaaa\" }")).unwrap(),
            old("{ git = \"https://h/json\", tag = \"v1.0.0\" }")
        );
        assert_eq!(
            added(&old("{ path = \"../json\" }")).unwrap_err(),
            "`cheby.toml` has another dependency `json`"
        );
        assert!(
            added(&format!(
                "{HEAD}\n[dependencies.json]\ngit = \"https://h/json\"\ntag = \"v0.1.0\"\n"
            ))
            .is_err()
        );
        assert!(
            added(&format!(
                "dependencies = {{ a = {{ path = \"../a\" }} }}\n{HEAD}"
            ))
            .is_err()
        );
    }
}
