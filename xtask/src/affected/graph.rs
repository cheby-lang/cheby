//! The workspace's packages and what depends on what, from `cargo
//! metadata` (D-1948).

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// One package of the workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Package {
    pub(crate) name: String,
    /// Its directory from the repository root, with `/`, such as
    /// `crates/cheby_fmt`.
    pub(crate) dir: String,
    /// The workspace packages it depends on, in any way: a test binary is
    /// built from its package's dev-dependencies too.
    pub(crate) deps: BTreeSet<String>,
    /// Its integration-test targets, by name.
    pub(crate) tests: Vec<String>,
}

/// The packages of the workspace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Workspace {
    pub(crate) packages: Vec<Package>,
}

impl Workspace {
    /// Asks cargo for the workspace at `root`.
    pub(crate) fn load(root: &Path) -> Result<Self, String> {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let out = Command::new(cargo)
            .current_dir(root)
            .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
            .output()
            .map_err(|e| format!("cannot run cargo metadata: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "cargo metadata failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Self::from_json(&String::from_utf8_lossy(&out.stdout))
    }

    /// Reads the output of `cargo metadata --no-deps`.
    pub(crate) fn from_json(text: &str) -> Result<Self, String> {
        let broken = |what: &str| format!("cargo metadata has no {what}");
        let json: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("cargo metadata: {e}"))?;
        let root = json
            .get("workspace_root")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| broken("workspace_root"))?;
        let listed = json
            .get("packages")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| broken("packages"))?;
        let text_of = |value: &serde_json::Value, key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let names: BTreeSet<String> = listed.iter().filter_map(|p| text_of(p, "name")).collect();
        let mut packages = Vec::new();
        for package in listed {
            let name = text_of(package, "name").ok_or_else(|| broken("package name"))?;
            let manifest =
                text_of(package, "manifest_path").ok_or_else(|| broken("manifest_path"))?;
            let dir = Path::new(&manifest)
                .parent()
                .and_then(|dir| dir.strip_prefix(root).ok())
                .map(|dir| cheby_test_support::display_relative(dir, Path::new("")))
                .ok_or_else(|| format!("{manifest} is not under {root}"))?;
            let list = |key: &str| {
                package
                    .get(key)
                    .and_then(serde_json::Value::as_array)
                    .map_or(&[][..], Vec::as_slice)
            };
            let deps = list("dependencies")
                .iter()
                .filter_map(|dep| text_of(dep, "name"))
                .filter(|dep| names.contains(dep) && *dep != name)
                .collect();
            let tests = list("targets")
                .iter()
                .filter(|target| {
                    target
                        .get("kind")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "test"))
                })
                .filter_map(|target| text_of(target, "name"))
                .collect();
            packages.push(Package {
                name,
                dir,
                deps,
                tests,
            });
        }
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self { packages })
    }

    pub(crate) fn package(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|package| package.name == name)
    }

    /// The package whose directory holds `path`: the deepest one.
    pub(crate) fn owner(&self, path: &str) -> Option<&Package> {
        self.packages
            .iter()
            .filter(|package| {
                path.strip_prefix(package.dir.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|package| package.dir.len())
    }

    /// `name` and every package that depends on it, directly or through
    /// others.
    pub(crate) fn dependents(&self, name: &str) -> BTreeSet<String> {
        let mut found = BTreeSet::from([name.to_owned()]);
        loop {
            let more: Vec<String> = self
                .packages
                .iter()
                .filter(|package| !found.contains(&package.name))
                .filter(|package| package.deps.iter().any(|dep| found.contains(dep)))
                .map(|package| package.name.clone())
                .collect();
            if more.is_empty() {
                return found;
            }
            found.extend(more);
        }
    }

    /// `roots` and every package they depend on, directly or through others.
    pub(crate) fn closure(&self, roots: &[&str]) -> BTreeSet<String> {
        let mut found: BTreeSet<String> = roots.iter().map(|&root| root.to_owned()).collect();
        loop {
            let more: Vec<String> = found
                .iter()
                .filter_map(|name| self.package(name))
                .flat_map(|package| package.deps.iter())
                .filter(|dep| !found.contains(*dep))
                .cloned()
                .collect();
            if more.is_empty() {
                return found;
            }
            found.extend(more);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A workspace in the shape of this one: a syntax crate at the bottom,
    /// a formatter and a driver over it, a server over both, and a command
    /// line over all, with a runtime beside them.
    pub(crate) fn workspace() -> Workspace {
        let package = |name: &str, dir: &str, deps: &[&str], tests: &[&str]| Package {
            name: name.to_owned(),
            dir: dir.to_owned(),
            deps: deps.iter().map(|&dep| dep.to_owned()).collect(),
            tests: tests.iter().map(|&test| test.to_owned()).collect(),
        };
        let cli_tests: Vec<&str> = crate::affected::rules::CLI_TESTS
            .iter()
            .map(|&(name, _)| name)
            .collect();
        let mut packages = vec![
            package(
                "cheby_cli",
                "crates/cheby_cli",
                &[
                    "cheby_doc",
                    "cheby_driver",
                    "cheby_fmt",
                    "cheby_lsp",
                    "cheby_repl",
                    "cheby_runtime",
                    "cheby_test_support",
                ],
                &cli_tests,
            ),
            package("cheby_diag", "crates/cheby_diag", &[], &[]),
            package(
                "cheby_doc",
                "crates/cheby_doc",
                &["cheby_diag", "cheby_driver"],
                &[],
            ),
            package(
                "cheby_driver",
                "crates/cheby_driver",
                &["cheby_diag", "cheby_runtime", "cheby_syntax"],
                &[],
            ),
            package(
                "cheby_fmt",
                "crates/cheby_fmt",
                &["cheby_diag", "cheby_syntax", "cheby_test_support"],
                &["corpus"],
            ),
            package(
                "cheby_lsp",
                "crates/cheby_lsp",
                &["cheby_driver", "cheby_fmt", "cheby_syntax"],
                &[],
            ),
            package(
                "cheby_repl",
                "crates/cheby_repl",
                &["cheby_driver", "cheby_runtime"],
                &[],
            ),
            package(
                "cheby_rt_exe",
                "crates/cheby_rt_exe",
                &["cheby_runtime"],
                &[],
            ),
            package("cheby_runtime", "crates/cheby_runtime", &[], &[]),
            package("cheby_syntax", "crates/cheby_syntax", &["cheby_diag"], &[]),
            package(
                "cheby_test_support",
                "tests/support",
                &["cheby_diag", "cheby_syntax"],
                &[],
            ),
            package(
                "xtask",
                "xtask",
                &["cheby_driver", "cheby_test_support"],
                &[],
            ),
        ];
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        Workspace { packages }
    }

    #[test]
    fn reads_cargo_metadata() {
        let json = r#"{
          "workspace_root": "/repo",
          "packages": [
            {"name": "b", "manifest_path": "/repo/crates/b/Cargo.toml",
             "dependencies": [{"name": "a", "kind": null}, {"name": "serde", "kind": null},
                              {"name": "c", "kind": "dev"}],
             "targets": [{"kind": ["lib"], "name": "b"}, {"kind": ["test"], "name": "it"}]},
            {"name": "a", "manifest_path": "/repo/crates/a/Cargo.toml",
             "dependencies": [], "targets": [{"kind": ["lib"], "name": "a"}]},
            {"name": "c", "manifest_path": "/repo/tests/support/Cargo.toml",
             "dependencies": [], "targets": []}
          ]
        }"#;
        let workspace = Workspace::from_json(json).unwrap();
        let b = workspace.package("b").unwrap();
        assert_eq!(b.dir, "crates/b");
        assert_eq!(
            b.deps.iter().map(String::as_str).collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(b.tests, ["it"]);
        assert_eq!(workspace.package("c").unwrap().dir, "tests/support");
        assert!(Workspace::from_json("{}").is_err());
    }

    #[test]
    fn owners_dependents_and_closures() {
        let workspace = workspace();
        let owner = |path: &str| workspace.owner(path).map(|package| package.name.as_str());
        assert_eq!(owner("crates/cheby_fmt/src/lib.rs"), Some("cheby_fmt"));
        assert_eq!(owner("tests/support/src/ui.rs"), Some("cheby_test_support"));
        assert_eq!(owner("xtask/src/main.rs"), Some("xtask"));
        assert_eq!(owner("crates/cheby_fmt_other/src/lib.rs"), None);
        assert_eq!(owner("docs/spec/01-overview.md"), None);
        let names = |set: BTreeSet<String>| set.into_iter().collect::<Vec<_>>().join(" ");
        assert_eq!(
            names(workspace.dependents("cheby_fmt")),
            "cheby_cli cheby_fmt cheby_lsp"
        );
        assert_eq!(names(workspace.dependents("cheby_rt_exe")), "cheby_rt_exe");
        assert_eq!(
            names(workspace.closure(&["cheby_lsp"])),
            "cheby_diag cheby_driver cheby_fmt cheby_lsp cheby_runtime cheby_syntax \
             cheby_test_support"
        );
    }
}
