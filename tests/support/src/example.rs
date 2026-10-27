//! Examples in `docs/examples/` and their golden files (D-275).
//!
//! ```text
//! docs/examples/003_fizzbuzz/
//!   cheby.toml, src/
//!   expected.stdout   required unless test.toml has skip-run
//!   expected.stderr   optional, empty if absent
//!   expected.exit     optional, 0 if absent
//!   stdin             optional input
//!   args              optional, one argument per line; may hold {http_fixture}
//!   env               optional, KEY=VALUE per line; may hold {http_fixture}
//!   site/             optional, the files the HTTP fixture serves
//!   test.toml         milestone, skip-run, reason, timing
//! ```
//!
//! `{http_fixture}` in an `args` line or an `env` value stands for the URL
//! of the HTTP fixture's server for the example's `site/` (D-1027), and
//! any other `{…}` word there is an error. [`Example::run_args`] expands
//! it, and every runner of examples calls it.
//!
//! `timing` is an optional boolean, false by default. An example that sets
//! it runs every one of its trials serially after the parallel batch of
//! the conformance harness (D-725). Another type, or `timing = true` with
//! `skip-run = true`, is an error, since a check-only example waits for
//! nothing (D-886).

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
    /// The example waits for real time, so its trials run serially after
    /// the parallel batch (D-725).
    pub timing: bool,
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

/// The placeholder `args` and `env` may hold (D-1027).
pub const HTTP_FIXTURE: &str = "{http_fixture}";

/// [`HTTP_FIXTURE`] without its braces.
const HTTP_FIXTURE_WORD: &str = "http_fixture";

/// An example run's arguments and environment with the placeholder
/// expanded ([`Example::run_args`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunArgs {
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
    /// Returns a message for invalid TOML, an unknown or ill-typed key, a
    /// missing `milestone`, `skip-run` without a `reason`, or `timing`
    /// with `skip-run`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
        let mut milestone = None;
        let mut skip_run = false;
        let mut reason = None;
        let mut timing = false;
        for (key, value) in &table {
            match (key.as_str(), value) {
                ("milestone", toml::Value::String(s)) => milestone = Some(s.parse()?),
                ("skip-run", toml::Value::Boolean(b)) => skip_run = *b,
                ("reason", toml::Value::String(s)) => reason = Some(s.clone()),
                ("timing", toml::Value::Boolean(b)) => timing = *b,
                _ => return Err(format!("unknown or ill-typed key `{key}`")),
            }
        }
        let milestone = milestone.ok_or("missing `milestone`")?;
        if skip_run && reason.is_none() {
            return Err("`skip-run` needs a `reason`".to_owned());
        }
        // D-886: a `skip-run` example is only checked, so
        // it has no timing to protect.
        if skip_run && timing {
            return Err("`timing` has no effect on a `skip-run` example".to_owned());
        }
        Ok(Self {
            milestone,
            skip_run,
            reason,
            timing,
        })
    }
}

/// The names of the examples whose trials run serially after the parallel
/// batch, in the order of `examples` (D-725).
#[must_use]
pub fn serial_names(examples: &[Example]) -> Vec<&str> {
    examples
        .iter()
        .filter(|ex| ex.config.timing)
        .map(|ex| ex.name.as_str())
        .collect()
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

    /// `goldens`' `args` and `env` with each `{http_fixture}` replaced by
    /// the URL of this process's HTTP fixture for `<dir>/site` (D-1027):
    /// `run_args_with(goldens, &http_fixture::url_for)`.
    ///
    /// # Errors
    ///
    /// As [`Example::run_args_with`].
    pub fn run_args(&self, goldens: &Goldens) -> Result<RunArgs, String> {
        self.run_args_with(goldens, &crate::http_fixture::url_for)
    }

    /// [`Example::run_args`] with another resolver of a site directory's
    /// URL, so a test can drive the rules with servers of its own
    /// (`http_fixture::Server::start`). `url_for` is called at most once,
    /// and only when a placeholder needs it, so an example without one
    /// starts no server.
    ///
    /// D-1249: a placeholder is a `{`, one or more
    /// characters that are neither white space nor a brace, and a `}`, in
    /// an `args` line or an `env` value; only `{http_fixture}` is known. A
    /// brace that starts no such run is kept as it is, and `env` keys are
    /// never expanded.
    ///
    /// # Errors
    ///
    /// Returns a message naming the file (`args` or `env`) for any other
    /// placeholder, for `{http_fixture}` without a `site/` directory next to
    /// `cheby.toml`, and for a resolver's error.
    pub fn run_args_with(
        &self,
        goldens: &Goldens,
        url_for: &dyn Fn(&Path) -> Result<String, String>,
    ) -> Result<RunArgs, String> {
        let mut url: Option<String> = None;
        let mut resolve = |file: &str| -> Result<String, String> {
            if let Some(url) = &url {
                return Ok(url.clone());
            }
            let site = self.dir.join("site");
            if !site.is_dir() {
                return Err(format!(
                    "{}/{file}: `{HTTP_FIXTURE}` needs a `site/` directory next to \
                     `cheby.toml` (D-1027)",
                    self.name
                ));
            }
            let found = url_for(&site).map_err(|e| format!("{}/{file}: {e}", self.name))?;
            url = Some(found.clone());
            Ok(found)
        };
        let args = goldens
            .args
            .iter()
            .map(|arg| expand(arg, "args", &self.name, &mut resolve))
            .collect::<Result<_, _>>()?;
        let env = goldens
            .env
            .iter()
            .map(|(key, value)| Ok((key.clone(), expand(value, "env", &self.name, &mut resolve)?)))
            .collect::<Result<_, String>>()?;
        Ok(RunArgs { args, env })
    }
}

/// `text`, a line of the example's `file`, with each `{http_fixture}`
/// replaced by `resolve(file)`; any other placeholder is an error
/// (D-1249, D-1027).
fn expand(
    text: &str,
    file: &str,
    example: &str,
    resolve: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once('{') {
        out.push_str(before);
        match placeholder(after) {
            Some((word, tail)) if word == HTTP_FIXTURE_WORD => {
                out.push_str(&resolve(file)?);
                rest = tail;
            }
            Some((word, _)) => {
                return Err(format!(
                    "{example}/{file}: unknown placeholder `{{{word}}}`; only `{HTTP_FIXTURE}` \
                     is known (D-1027)"
                ));
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// The word of a placeholder whose `{` came just before `after`, and the
/// text after its `}`, or `None` if `after` does not continue one.
fn placeholder(after: &str) -> Option<(&str, &str)> {
    let (word, tail) = after.split_once('}')?;
    let is_word = !word.is_empty() && !word.contains(|c: char| c.is_whitespace() || c == '{');
    is_word.then_some((word, tail))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;

    use super::{Example, Goldens, HTTP_FIXTURE, RunArgs, TestConfig, serial_names};
    use crate::cache::FreshDir;
    use crate::milestone::Milestone;

    #[test]
    fn parses_test_toml() {
        let c = TestConfig::parse("milestone = \"M2\"\n").unwrap();
        assert_eq!(c.milestone, Milestone::M2);
        assert!(!c.skip_run);
        assert!(!c.timing);
        assert!(TestConfig::parse("milestone = \"M3\"\nskip-run = true\n").is_err());
        assert!(TestConfig::parse("milestone = \"M1\"\ncolour = 1\n").is_err());
        // D-725, D-886.
        assert!(
            TestConfig::parse("milestone = \"M2\"\ntiming = true\n")
                .unwrap()
                .timing
        );
        assert!(
            !TestConfig::parse("milestone = \"M2\"\ntiming = false\n")
                .unwrap()
                .timing
        );
        for bad in ["timing = \"yes\"\n", "timing = 1\n"] {
            let err = TestConfig::parse(&format!("milestone = \"M2\"\n{bad}")).unwrap_err();
            assert_eq!(err, "unknown or ill-typed key `timing`");
        }
        let err = TestConfig::parse(
            "milestone = \"M3\"\nskip-run = true\nreason = \"a fixture\"\ntiming = true\n",
        )
        .unwrap_err();
        assert_eq!(err, "`timing` has no effect on a `skip-run` example");
        assert!(
            !TestConfig::parse(
                "milestone = \"M3\"\nskip-run = true\nreason = \"a fixture\"\ntiming = false\n"
            )
            .unwrap()
            .timing
        );
    }

    /// An example in a fresh directory, with a `site/` if `with_site`.
    fn scratch_example(prefix: &str, with_site: bool) -> (FreshDir, Example) {
        let dir = FreshDir::create(prefix).unwrap();
        if with_site {
            std::fs::create_dir(dir.path().join("site")).unwrap();
            std::fs::write(dir.path().join("site/index.html"), "home\n").unwrap();
        }
        let example = Example {
            name: "999_fixture".to_owned(),
            dir: dir.path().to_path_buf(),
            config: TestConfig::parse("milestone = \"M3\"\n").unwrap(),
        };
        (dir, example)
    }

    fn goldens(args: &[&str], env: &[(&str, &str)]) -> Goldens {
        Goldens {
            args: args.iter().map(|&a| a.to_owned()).collect(),
            env: env
                .iter()
                .map(|&(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
            ..Goldens::default()
        }
    }

    #[test]
    fn run_args_replaces_the_placeholder_and_resolves_once() {
        let (dir, ex) = scratch_example("run-args-replace", true);
        let calls = RefCell::new(Vec::new());
        let resolver = |site: &Path| {
            calls.borrow_mut().push(site.to_path_buf());
            Ok("http://127.0.0.1:4711".to_owned())
        };
        let g = goldens(
            &["{http_fixture}", "--base={http_fixture}/about", "plain"],
            &[("SITE", "{http_fixture}"), ("KEEP", "x")],
        );
        let run = ex.run_args_with(&g, &resolver).unwrap();
        assert_eq!(
            run,
            RunArgs {
                args: vec![
                    "http://127.0.0.1:4711".to_owned(),
                    "--base=http://127.0.0.1:4711/about".to_owned(),
                    "plain".to_owned(),
                ],
                env: vec![
                    ("SITE".to_owned(), "http://127.0.0.1:4711".to_owned()),
                    ("KEEP".to_owned(), "x".to_owned()),
                ],
            }
        );
        assert_eq!(*calls.borrow(), [dir.path().join("site")]);
    }

    #[test]
    fn run_args_without_the_placeholder_starts_no_server() {
        let (_dir, ex) = scratch_example("run-args-none", false);
        let resolver = |_: &Path| -> Result<String, String> { panic!("no server is needed") };
        let g = goldens(
            &["a", "{", "}", "{}", "{\"a\": 1}", "{ http_fixture }", "x{y"],
            &[("JSON", "{\"k\": \"v\"}"), ("{K}", "v")],
        );
        let run = ex.run_args_with(&g, &resolver).unwrap();
        assert_eq!(run.args, g.args);
        assert_eq!(run.env, g.env);
    }

    #[test]
    fn run_args_rejects_any_other_placeholder() {
        let (_dir, ex) = scratch_example("run-args-other", true);
        let resolver = |_: &Path| Ok("http://127.0.0.1:1".to_owned());
        let err = ex
            .run_args_with(&goldens(&["{http_fixtur}"], &[]), &resolver)
            .unwrap_err();
        assert_eq!(
            err,
            "999_fixture/args: unknown placeholder `{http_fixtur}`; only `{http_fixture}` is \
             known (D-1027)"
        );
        let err = ex
            .run_args_with(&goldens(&[], &[("PORT", "{port}")]), &resolver)
            .unwrap_err();
        assert!(
            err.starts_with("999_fixture/env: unknown placeholder `{port}`"),
            "{err}"
        );
        // A known one first does not excuse an unknown one after it.
        let err = ex
            .run_args_with(&goldens(&["{http_fixture}/{path}"], &[]), &resolver)
            .unwrap_err();
        assert!(err.contains("`{path}`"), "{err}");
    }

    #[test]
    fn run_args_needs_a_site_directory() {
        let (_dir, ex) = scratch_example("run-args-no-site", false);
        let resolver = |_: &Path| Ok("http://127.0.0.1:1".to_owned());
        let err = ex
            .run_args_with(&goldens(&[], &[("URL", "{http_fixture}")]), &resolver)
            .unwrap_err();
        assert_eq!(
            err,
            "999_fixture/env: `{http_fixture}` needs a `site/` directory next to `cheby.toml` \
             (D-1027)"
        );
        // A resolver's error names the file too.
        let (_dir, ex) = scratch_example("run-args-failing", true);
        let failing = |_: &Path| Err("no port".to_owned());
        let err = ex
            .run_args_with(&goldens(&["{http_fixture}"], &[]), &failing)
            .unwrap_err();
        assert_eq!(err, "999_fixture/args: no port");
    }

    #[test]
    fn run_args_uses_the_process_fixture() {
        let (dir, ex) = scratch_example("run-args-fixture", true);
        let g = goldens(&["{http_fixture}"], &[("URL", "{http_fixture}/")]);
        let first = ex.run_args(&g).unwrap();
        let url = crate::http_fixture::url_for(&dir.path().join("site")).unwrap();
        assert_eq!(first.args, std::slice::from_ref(&url));
        assert_eq!(first.env, [("URL".to_owned(), format!("{url}/"))]);
        // A second expansion shares the server.
        assert_eq!(ex.run_args(&g).unwrap(), first);
    }

    /// No example holds a placeholder yet (031 gets one in wave 9), so
    /// every example's arguments and environment are its goldens' as they
    /// are, and none starts a server.
    #[test]
    fn run_args_of_the_examples_without_a_placeholder_are_their_goldens() {
        let resolver = |_: &Path| -> Result<String, String> { panic!("no server is needed") };
        for ex in super::all(&crate::repo_root()).unwrap() {
            let g = ex.goldens().unwrap();
            let holds = |t: &str| t.contains(HTTP_FIXTURE);
            if g.args.iter().any(|a| holds(a)) || g.env.iter().any(|(_, v)| holds(v)) {
                continue;
            }
            let run = ex.run_args_with(&g, &resolver).unwrap();
            assert_eq!((run.args, run.env), (g.args, g.env), "{}", ex.name);
        }
    }

    #[test]
    fn the_timing_examples_are_the_three_of_d725() {
        let examples = super::all(&crate::repo_root()).unwrap();
        assert_eq!(
            serial_names(&examples),
            [
                "009_counter_actor",
                "013_dining_philosophers",
                "019_supervisor"
            ]
        );
    }
}
