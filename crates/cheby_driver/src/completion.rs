//! What shell completion of `cheby` reads from the working directory
//! (D-1958): a package's module paths, from a walk of `src/`, and the names
//! of its tests, from a parse of each module.
//!
//! Nothing is checked, so an answer takes little time and a package that
//! does not compile still completes. A completion has nowhere to report an
//! error, so every failure is an empty answer.

use std::path::Path;

use cheby_diag::SourceMap;
use cheby_syntax::SyntaxTree;
use cheby_syntax::ast::{AstNode as _, Item, SourceFile, StringLit};

/// A test by the two parts of its full name, `<module path> "<test name>"`
/// (§13.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestName {
    /// The path of the module that holds the test.
    pub module: String,
    /// The test's name, the value of its string literal.
    pub name: String,
}

/// The paths of the modules of the package at `root`, sorted: what
/// `cheby run`, `cheby build` and `cheby test --module` take. A file or
/// directory with an invalid name is no module (E0037) and is left out.
#[must_use]
pub fn module_paths(root: &Path) -> Vec<String> {
    let Some(package) = crate::package_name(root) else {
        return Vec::new();
    };
    let mut paths: Vec<String> = cheby_package::discover(root, &package, &mut Vec::new())
        .unwrap_or_default()
        .into_iter()
        .map(|module| module.path.to_string())
        .collect();
    paths.sort();
    paths
}

/// The tests of the package at `root`, by module in file order and then in
/// source order. A module with syntax errors gives the tests its tree still
/// has.
#[must_use]
pub fn package_tests(root: &Path) -> Vec<TestName> {
    let mut sources = SourceMap::new();
    let Ok(Some(package)) = crate::load_package(root, &mut sources, &mut Vec::new()) else {
        return Vec::new();
    };
    package
        .modules
        .iter()
        .flat_map(|module| tests_of(module.source.syntax.as_ref(), &module.path.to_string()))
        .collect()
}

/// The tests of the single file `file` (D-1949), whose module is
/// [`crate::SINGLE_PACKAGE`].
#[must_use]
pub fn file_tests(file: &Path) -> Vec<TestName> {
    let Ok(bytes) = std::fs::read(file) else {
        return Vec::new();
    };
    let mut sources = SourceMap::new();
    let display = file.to_string_lossy();
    let Ok(parsed) = crate::parse_file(&mut sources, &display, bytes, &mut Vec::new()) else {
        return Vec::new();
    };
    tests_of(parsed.syntax.as_ref(), crate::SINGLE_PACKAGE)
}

/// The tests of the module `module` with the tree `tree`. A test whose
/// name has an interpolation, which is an error, is left out.
fn tests_of(tree: Option<&SyntaxTree>, module: &str) -> Vec<TestName> {
    let Some(file) = tree.and_then(|tree| SourceFile::cast(tree.root())) else {
        return Vec::new();
    };
    file.items()
        .filter_map(|item| match item {
            Item::Test(test) => test.name().and_then(StringLit::plain_value),
            Item::Fn(_) | Item::Type(_) | Item::Alias(_) | Item::Const(_) | Item::Interface(_) => {
                None
            }
        })
        .map(|name| TestName {
            module: module.to_owned(),
            name,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A fresh directory with `files`, each a path and its text.
    fn directory(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cheby-completion-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (file, text) in files {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn named(tests: &[TestName]) -> Vec<(&str, &str)> {
        tests
            .iter()
            .map(|test| (test.module.as_str(), test.name.as_str()))
            .collect()
    }

    /// The modules are the valid files under `src/`, and the tests are
    /// read from the trees, also from one with a syntax error and from a
    /// package that would not check (D-1958).
    #[test]
    fn a_package_gives_its_modules_and_tests() {
        let root = directory(
            "package",
            &[
                ("cheby.toml", "name = \"app\"\nversion = \"0.1.0\"\n"),
                (
                    "src/main.cheby",
                    "//! The program.\n\nfn main() {\n  missing()\n}\n\ntest \"adds\" {\n  assert 1 + 1 == 2\n}\n",
                ),
                (
                    "src/util.cheby",
                    "//! Helpers.\n\ntest \"splits a \\\"b\\\"\" {\n  assert True\n}\n\ntest \"adds\" {\n  assert True\n}\n",
                ),
                (
                    "src/web/router.cheby",
                    "//! Routes.\n\ntest \"routes\" {\n  assert True\n}\n\nfn broken( {\n",
                ),
                ("src/Bad.cheby", "test \"never\" {\n}\n"),
                ("src/notes.txt", "test \"no module\" {}\n"),
            ],
        );
        assert_eq!(
            module_paths(&root),
            ["app", "app::util", "app::web::router"]
        );
        assert_eq!(
            named(&package_tests(&root)),
            [
                ("app", "adds"),
                ("app::util", "splits a \"b\""),
                ("app::util", "adds"),
                ("app::web::router", "routes"),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A single file's tests are those of the module `script`, and a
    /// directory without a manifest, or a file that is not there, gives
    /// nothing.
    #[test]
    fn a_file_gives_its_tests_and_nothing_else_does() {
        let dir = directory(
            "file",
            &[(
                "probe.cheby",
                "test \"doubles\" {\n  assert 2 * 2 == 4\n}\n\ntest \"is {1}\" {\n}\n",
            )],
        );
        assert_eq!(
            named(&file_tests(&dir.join("probe.cheby"))),
            [(crate::SINGLE_PACKAGE, "doubles")]
        );
        assert_eq!(file_tests(&dir.join("absent.cheby")), Vec::new());
        assert_eq!(module_paths(&dir), Vec::<String>::new());
        assert_eq!(package_tests(&dir), Vec::new());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
