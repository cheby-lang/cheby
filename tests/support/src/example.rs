//! Examples in `docs/examples/` and their golden files (D-275).
//!
//! ```text
//! docs/examples/003_fizzbuzz/
//!   cheby.toml, src/
//!   expected.stdout   required unless test.toml has skip-run
//!   expected.stderr   optional, empty if absent
//!   expected.exit     optional, 0 if absent
//!   stdin             optional input
//!   args              optional, one argument per line
//!   env               optional, KEY=VALUE per line
//!   test.toml         milestone, skip-run, reason
//! ```

use std::path::{Path, PathBuf};

use crate::milestone::Milestone;

#[derive(Debug, Clone)]
pub struct Example {
    /// Directory name, such as `003_fizzbuzz`.
    pub name: String,
    pub dir: PathBuf,
    pub config: TestConfig,
}

/// The contents of an example's `test.toml`.
#[derive(Debug, Clone)]
pub struct TestConfig {
    /// The milestone in which the example must pass.
    pub milestone: Milestone,
    /// Only check the example, never run it.
    pub skip_run: bool,
    /// Why the example is not run.
    pub reason: Option<String>,
}

/// The inputs and goldens of one example run.
#[derive(Debug, Clone, Default)]
pub struct Goldens {
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub exit: Option<i32>,
    pub stdin: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Every example, sorted by name.
///
/// # Errors
///
/// Returns a message if the directory or a `test.toml` cannot be read.
pub fn all(root: &Path) -> Result<Vec<Example>, String> {
    let dir = root.join("docs/examples");
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut examples = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if !path.join("cheby.toml").is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let config = TestConfig::read(&path.join("test.toml"))?;
        examples.push(Example {
            name,
            dir: path,
            config,
        });
    }
    examples.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(examples)
}

impl TestConfig {
    /// Reads a `test.toml`.
    ///
    /// # Errors
    ///
    /// Returns a message if the file cannot be read or is invalid.
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Parses the contents of a `test.toml`.
    ///
    /// # Errors
    ///
    /// Returns a message for invalid TOML, an unknown key, a missing
    /// `milestone`, or `skip-run` without a `reason`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
        let mut milestone = None;
        let mut skip_run = false;
        let mut reason = None;
        for (key, value) in &table {
            match (key.as_str(), value) {
                ("milestone", toml::Value::String(s)) => milestone = Some(s.parse()?),
                ("skip-run", toml::Value::Boolean(b)) => skip_run = *b,
                ("reason", toml::Value::String(s)) => reason = Some(s.clone()),
                _ => return Err(format!("unknown or ill-typed key `{key}`")),
            }
        }
        let milestone = milestone.ok_or("missing `milestone`")?;
        if skip_run && reason.is_none() {
            return Err("`skip-run` needs a `reason`".to_owned());
        }
        Ok(Self {
            milestone,
            skip_run,
            reason,
        })
    }
}

impl Example {
    /// Reads the example's inputs and goldens.
    ///
    /// # Errors
    ///
    /// Returns a message if a file cannot be read or is malformed.
    pub fn goldens(&self) -> Result<Goldens, String> {
        let read = |name: &str| crate::golden::read(&self.dir.join(name));
        let exit = match read("expected.exit")? {
            Some(text) => Some(
                text.trim()
                    .parse()
                    .map_err(|e| format!("{}/expected.exit: {e}", self.name))?,
            ),
            None => None,
        };
        let args = read("args")?
            .map(|t| t.lines().map(str::to_owned).collect())
            .unwrap_or_default();
        let env = match read("env")? {
            Some(text) => text
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| {
                    l.split_once('=')
                        .map(|(k, v)| (k.to_owned(), v.to_owned()))
                        .ok_or_else(|| format!("{}/env: `{l}` is not KEY=VALUE", self.name))
                })
                .collect::<Result<_, _>>()?,
            None => Vec::new(),
        };
        Ok(Goldens {
            stdout: read("expected.stdout")?,
            stderr: read("expected.stderr")?,
            exit,
            stdin: read("stdin")?,
            args,
            env,
        })
    }

    /// Every `.cheby` file of the example.
    ///
    /// # Errors
    ///
    /// Returns a message if `src/` cannot be read.
    pub fn sources(&self) -> Result<Vec<PathBuf>, String> {
        crate::files_with_extension(&self.dir.join("src"), "cheby").map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::TestConfig;
    use crate::milestone::Milestone;

    #[test]
    fn parses_test_toml() {
        let c = TestConfig::parse("milestone = \"M2\"\n").unwrap();
        assert_eq!(c.milestone, Milestone::M2);
        assert!(!c.skip_run);
        assert!(TestConfig::parse("milestone = \"M3\"\nskip-run = true\n").is_err());
        assert!(TestConfig::parse("milestone = \"M1\"\ncolour = 1\n").is_err());
    }
}
