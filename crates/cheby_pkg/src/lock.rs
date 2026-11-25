//! `cheby.lock` (§13.4, D-1880).

use std::fmt::Write as _;
use std::path::Path;

use cheby_package::{GitReference, GitUrl, Version, is_valid_rev};
use toml::de::{DeTable, DeValue};

use crate::{PkgError, toml_string};

/// The file name of the lockfile, beside the root package's manifest.
pub const LOCK_FILE: &str = "cheby.lock";

const HEADER: &str = "# Written by `cheby update`. Do not edit it.\n\nversion = 1\n";

/// One package identity of a build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockedPackage {
    /// The URL as git gets it; its canonical form identifies the package.
    pub source: GitUrl,
    pub version: Version,
    /// The full hash of the chosen commit.
    pub commit: String,
    /// The checksum of the commit's files.
    pub checksum: String,
    /// What the manifests of the build require of the package, such as
    /// `tag:v1.2.0`, sorted.
    pub required: Vec<String>,
}

impl LockedPackage {
    /// The major version of the identity (D-239).
    #[must_use]
    pub fn major(&self) -> String {
        self.version.major_part()
    }

    /// The package as messages show it.
    #[must_use]
    pub fn shown(&self) -> String {
        format!("{}@{}", self.source.canonical(), self.version)
    }
}

/// The lockfile of a root package, sorted by URL and major.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lockfile {
    pub packages: Vec<LockedPackage>,
}

impl Lockfile {
    /// Reads the lockfile of the package at `root`; `None` without one.
    ///
    /// # Errors
    ///
    /// When it cannot be read or is not a lockfile.
    pub fn load(root: &Path) -> Result<Option<Self>, PkgError> {
        let path = root.join(LOCK_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).map(Some).map_err(|problem| {
                PkgError::new(format!("`{LOCK_FILE}` is not valid: {problem}"))
                    .with_help("run `cheby update` to write it again")
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(PkgError::new(format!("{LOCK_FILE}: {e}"))),
        }
    }

    /// The package that the requirement `reference` of `url` chose.
    #[must_use]
    pub fn find(&self, url: &GitUrl, reference: &GitReference) -> Option<&LockedPackage> {
        let wanted = reference.to_string();
        self.packages
            .iter()
            .find(|p| p.source.canonical() == url.canonical() && p.required.contains(&wanted))
    }

    /// Parses the text of a lockfile.
    ///
    /// # Errors
    ///
    /// What is wrong, as a phrase. Every value is checked, because a commit
    /// and a URL become paths and arguments.
    pub fn parse(text: &str) -> Result<Self, String> {
        let table = DeTable::parse(text)
            .map_err(|e| e.message().lines().next().unwrap_or("").to_owned())?;
        let table = table.get_ref();
        match table.get("version").map(toml::Spanned::get_ref) {
            Some(DeValue::Integer(n)) if n.as_str() == "1" => {}
            _ => return Err("its `version` is not 1".to_owned()),
        }
        let mut packages: Vec<LockedPackage> = Vec::new();
        for (key, value) in table {
            match (key.get_ref().as_ref(), value.get_ref()) {
                ("version", _) => {}
                ("package", DeValue::Array(items)) => {
                    for item in items {
                        let package = item
                            .get_ref()
                            .as_table()
                            .ok_or_else(|| "a `package` is no table".to_owned())
                            .and_then(|table| package(table))?;
                        packages.push(package);
                    }
                }
                (other, _) => return Err(format!("unknown key `{other}`")),
            }
        }
        sort(&mut packages);
        let twice = packages
            .windows(2)
            .find(|pair| matches!(pair, [a, b] if key_of(a) == key_of(b)));
        if let Some([package, ..]) = twice {
            return Err(format!(
                "`{}` is listed twice for major {}",
                package.source.canonical(),
                package.major()
            ));
        }
        Ok(Self { packages })
    }

    /// The text of the lockfile.
    #[must_use]
    pub fn render(&self) -> String {
        let mut packages = self.packages.clone();
        sort(&mut packages);
        let mut out = HEADER.to_owned();
        for package in &packages {
            let required: Vec<String> = package.required.iter().map(|r| toml_string(r)).collect();
            // Writing to a string cannot fail. Only a tag may hold a
            // character that TOML escapes.
            let _ = write!(
                out,
                "\n[[package]]\nurl = \"{}\"\nmajor = \"{}\"\nversion = \"{}\"\nsource = \"{}\"\ncommit = \"{}\"\nchecksum = \"{}\"\nrequired = [{}]\n",
                package.source.canonical(),
                package.major(),
                package.version,
                package.source.source(),
                package.commit,
                package.checksum,
                required.join(", "),
            );
        }
        out
    }
}

fn key_of(package: &LockedPackage) -> (String, String) {
    (package.source.canonical().to_owned(), package.major())
}

fn sort(packages: &mut [LockedPackage]) {
    packages.sort_by_key(key_of);
}

/// Whether `commit` is the full hash of a commit.
pub(crate) fn is_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64)
        && is_valid_rev(commit)
        && !commit.bytes().any(|b| b.is_ascii_uppercase())
}

fn package(table: &DeTable<'_>) -> Result<LockedPackage, String> {
    const KEYS: [&str; 7] = [
        "url", "major", "version", "source", "commit", "checksum", "required",
    ];
    if let Some((key, _)) = table
        .iter()
        .find(|(key, _)| !KEYS.contains(&key.get_ref().as_ref()))
    {
        return Err(format!("unknown key `{}` in a package", key.get_ref()));
    }
    let text = |key: &str| {
        table
            .get(key)
            .and_then(|v| v.get_ref().as_str())
            .ok_or_else(|| format!("a package has no `{key}`"))
    };
    let source = text("source")?;
    let source = GitUrl::parse(source).map_err(|e| format!("a `source` {e}"))?;
    if text("url")? != source.canonical() {
        return Err(format!("`url` and `source` differ for `{}`", text("url")?));
    }
    let version = text("version")?;
    let version = Version::parse(version).ok_or_else(|| format!("`{version}` is not a version"))?;
    if text("major")? != version.major_part() {
        return Err(format!(
            "`major` and `version` differ for `{}`",
            text("url")?
        ));
    }
    let commit = text("commit")?.to_owned();
    if !is_commit(&commit) {
        return Err(format!("`{commit}` is not a commit"));
    }
    let checksum = text("checksum")?.to_owned();
    let digits = checksum.strip_prefix("sha256:").unwrap_or("");
    if digits.len() != 64 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("`{checksum}` is not a checksum"));
    }
    let mut required = Vec::new();
    let Some(DeValue::Array(items)) = table.get("required").map(toml::Spanned::get_ref) else {
        return Err("a package has no `required`".to_owned());
    };
    for item in items {
        let item = item
            .get_ref()
            .as_str()
            .filter(|r| r.starts_with("tag:") || r.starts_with("rev:"))
            .ok_or_else(|| "`required` lists `tag:…` and `rev:…`".to_owned())?;
        required.push(item.to_owned());
    }
    required.sort();
    required.dedup();
    Ok(LockedPackage {
        source,
        version,
        commit,
        checksum,
        required,
    })
}

#[cfg(test)]
mod tests {
    use cheby_package::{GitReference, GitUrl, Version};

    use super::{LockedPackage, Lockfile};

    fn locked(url: &str, version: &str) -> LockedPackage {
        LockedPackage {
            source: GitUrl::parse(url).unwrap(),
            version: Version::parse(version).unwrap(),
            commit: "a".repeat(40),
            checksum: format!("sha256:{}", "0".repeat(64)),
            required: vec![format!("tag:v{version}")],
        }
    }

    /// What is written is read back, sorted.
    #[test]
    fn a_lockfile_round_trips() {
        let lock = Lockfile {
            packages: vec![
                locked("https://h/b", "2.0.0"),
                locked("git@H:b.git", "1.0.0"),
                locked("https://h/a", "0.1.0"),
            ],
        };
        let text = lock.render();
        let read = Lockfile::parse(&text).unwrap();
        assert_eq!(read.render(), text);
        let shown: Vec<String> = read.packages.iter().map(LockedPackage::shown).collect();
        assert_eq!(shown, ["h/a@0.1.0", "h/b@1.0.0", "h/b@2.0.0"]);
        let url = GitUrl::parse("ssh://h/b").unwrap();
        let found = read.find(&url, &GitReference::Tag("v2.0.0".to_owned()));
        assert_eq!(
            found.map(LockedPackage::shown).as_deref(),
            Some("h/b@2.0.0")
        );
        assert_eq!(
            read.find(&url, &GitReference::Tag("v3.0.0".to_owned())),
            None
        );
    }

    /// A tag may hold a quotation mark.
    #[test]
    fn a_quotation_mark_round_trips() {
        let mut package = locked("https://h/a", "0.1.0");
        package.required = vec!["tag:v1\"x".to_owned()];
        let lock = Lockfile {
            packages: vec![package],
        };
        assert_eq!(Lockfile::parse(&lock.render()).unwrap(), lock);
    }

    /// Nothing unchecked comes out of a lockfile.
    #[test]
    fn bad_lockfiles() {
        let good = Lockfile {
            packages: vec![locked("https://h/b", "1.0.0")],
        }
        .render();
        assert!(Lockfile::parse(&good).is_ok());
        let commit = "a".repeat(40);
        for (from, to) in [
            ("version = 1\n", "version = 2\n"),
            ("https://h/b", "file:///b"),
            ("https://h/b", "https://h/c"),
            ("major = \"1\"", "major = \"2\""),
            (commit.as_str(), "../../x"),
            (commit.as_str(), "main"),
            ("sha256:0", "sha256:"),
            ("tag:v1.0.0", "v1.0.0"),
            ("commit", "kommit"),
            ("[[package]]", "<<<<<<< HEAD\n[[package]]"),
        ] {
            assert!(Lockfile::parse(&good.replace(from, to)).is_err(), "{to}");
        }
        let body = good.split_once("[[package]]").unwrap().1;
        assert!(Lockfile::parse(&format!("{good}\n[[package]]{body}")).is_err());
    }
}
