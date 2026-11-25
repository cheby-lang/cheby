//! Minimal version selection (§13.4, D-192, D-238): of each package
//! identity, the highest version that any manifest of the build requires.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use cheby_package::{GitReference, GitUrl, Version};

use crate::PkgError;

/// What a manifest requires.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Requirement {
    pub url: GitUrl,
    pub reference: GitReference,
}

impl Requirement {
    /// What makes two requirements one, however the URL is written.
    fn key(&self) -> (String, String) {
        (self.url.canonical().to_owned(), self.reference.to_string())
    }
}

/// A package at a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub commit: String,
    pub version: Version,
    pub requirements: Vec<Requirement>,
}

/// Where packages come from: git, or a table in tests.
pub trait Source {
    /// The package that `requirement` names.
    ///
    /// # Errors
    ///
    /// When there is none, or it is no package.
    fn candidate(&mut self, requirement: &Requirement) -> Result<Candidate, PkgError>;
}

/// The version of one package identity that a build uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selected {
    pub url: GitUrl,
    pub version: Version,
    pub commit: String,
    /// Every reference the manifests require of it, sorted.
    pub required: Vec<String>,
}

/// One commit of a package identity, and the references that name it.
struct Node {
    candidate: Candidate,
    references: BTreeSet<GitReference>,
    urls: BTreeSet<GitUrl>,
}

type Identity = (String, String);

/// Selects the packages of a build from the requirements of its local
/// manifests, the first `of_root` of which are the root manifest's. The
/// result is sorted by URL and major.
///
/// # Errors
///
/// From `source`, and for requirements that conflict (D-238).
pub fn resolve(
    roots: &[Requirement],
    of_root: usize,
    source: &mut dyn Source,
) -> Result<Vec<Selected>, PkgError> {
    let mut queue: VecDeque<Requirement> = roots.iter().cloned().collect();
    let mut identities: BTreeMap<(String, String), Identity> = BTreeMap::new();
    let mut nodes: BTreeMap<Identity, BTreeMap<String, Node>> = BTreeMap::new();
    while let Some(requirement) = queue.pop_front() {
        if let Some(identity) = identities.get(&requirement.key()) {
            // The same requirement through another spelling of the URL.
            if let Some(commits) = nodes.get_mut(identity) {
                for node in commits.values_mut() {
                    if node.references.contains(&requirement.reference) {
                        node.urls.insert(requirement.url.clone());
                    }
                }
            }
            continue;
        }
        let candidate = source.candidate(&requirement)?;
        let identity = (
            requirement.url.canonical().to_owned(),
            candidate.version.major_part(),
        );
        identities.insert(requirement.key(), identity.clone());
        queue.extend(candidate.requirements.iter().cloned());
        let node = nodes
            .entry(identity)
            .or_default()
            .entry(candidate.commit.clone())
            .or_insert_with(|| Node {
                candidate,
                references: BTreeSet::new(),
                urls: BTreeSet::new(),
            });
        node.references.insert(requirement.reference);
        node.urls.insert(requirement.url);
    }
    let mut chosen: BTreeMap<Identity, &Node> = BTreeMap::new();
    for (identity, commits) in &nodes {
        if let Some(node) = choose(identity, commits)? {
            chosen.insert(identity.clone(), node);
        }
    }
    // What the chosen versions do not reach is not in the build.
    let mut reached: BTreeSet<Identity> = BTreeSet::new();
    let mut queue: VecDeque<Requirement> = roots.iter().cloned().collect();
    while let Some(requirement) = queue.pop_front() {
        let Some(identity) = identities.get(&requirement.key()) else {
            continue;
        };
        if reached.insert(identity.clone())
            && let Some(node) = chosen.get(identity)
        {
            queue.extend(node.candidate.requirements.iter().cloned());
        }
    }
    let mut out = Vec::new();
    for (identity, node) in chosen {
        if !reached.contains(&identity) {
            continue;
        }
        let commits = nodes.get(&identity);
        let of_all = || commits.into_iter().flat_map(BTreeMap::values);
        let required: BTreeSet<String> = of_all()
            .flat_map(|n| n.references.iter().map(GitReference::to_string))
            .collect();
        // D-1886: the root manifest's spelling, then `https`.
        let root_urls: BTreeSet<&GitUrl> = roots
            .iter()
            .take(of_root)
            .filter(|r| identities.get(&r.key()) == Some(&identity))
            .map(|r| &r.url)
            .collect();
        let rank = |url: &&GitUrl| {
            let is_https = url.source().starts_with("https://");
            (
                !root_urls.contains(*url),
                !is_https,
                url.source().to_owned(),
            )
        };
        let Some(url) = of_all().flat_map(|n| n.urls.iter()).min_by_key(rank) else {
            continue;
        };
        out.push(Selected {
            url: url.clone(),
            version: node.candidate.version.clone(),
            commit: node.candidate.commit.clone(),
            required: required.into_iter().collect(),
        });
    }
    Ok(out)
}

/// The commit of `identity` with the highest version.
fn choose<'a>(
    identity: &Identity,
    commits: &'a BTreeMap<String, Node>,
) -> Result<Option<&'a Node>, PkgError> {
    let names = |node: &Node| {
        let names: Vec<String> = node
            .references
            .iter()
            .map(GitReference::to_string)
            .collect();
        names.join(", ")
    };
    let mut sorted: Vec<&Node> = commits.values().collect();
    sorted.sort_by(|a, b| b.candidate.version.cmp_precedence(&a.candidate.version));
    for pair in sorted.windows(2) {
        if let [a, b] = pair
            && a.candidate
                .version
                .cmp_precedence(&b.candidate.version)
                .is_eq()
        {
            return Err(PkgError::new(format!(
                "two commits of `{}` claim version {}: {} and {}",
                identity.0,
                a.candidate.version,
                names(a),
                names(b)
            )));
        }
    }
    let Some((first, others)) = sorted.split_first() else {
        return Ok(None);
    };
    let is_rev = |node: &Node| {
        node.references
            .iter()
            .any(|r| matches!(r, GitReference::Rev(_)))
    };
    if is_rev(first)
        && let Some(other) = others.iter().find(|n| is_rev(n))
    {
        return Err(PkgError::new(format!(
            "two commits of `{}` are required, and a build has one: {} and {}",
            identity.0,
            names(first),
            names(other)
        )));
    }
    Ok(Some(first))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cheby_package::{GitReference, GitUrl, Version};

    use super::{Candidate, Requirement, Selected, Source, resolve};
    use crate::PkgError;

    /// `url@reference`, where a reference of hex digits is a `rev`.
    fn requirement(text: &str) -> Requirement {
        let (url, reference) = text.split_once('@').unwrap();
        let reference = reference.to_owned();
        Requirement {
            url: GitUrl::parse(&format!("https://h/{url}")).unwrap(),
            reference: if reference.starts_with('v') {
                GitReference::Tag(reference)
            } else {
                GitReference::Rev(reference)
            },
        }
    }

    /// Each package at a reference: its commit, version and requirements.
    struct Table(BTreeMap<&'static str, (&'static str, &'static str, Vec<&'static str>)>);

    impl Source for Table {
        fn candidate(&mut self, r: &Requirement) -> Result<Candidate, PkgError> {
            let name = r.url.canonical().trim_start_matches("h/");
            let (_, reference) = r
                .reference
                .to_string()
                .split_once(':')
                .map(|(a, b)| (a.to_owned(), b.to_owned()))
                .unwrap();
            let (commit, version, requirements) = self
                .0
                .get(format!("{name}@{reference}").as_str())
                .ok_or_else(|| PkgError::new(format!("no {name}@{reference}")))?;
            Ok(Candidate {
                commit: (*commit).to_owned(),
                version: Version::parse(version).unwrap(),
                requirements: requirements.iter().map(|r| requirement(r)).collect(),
            })
        }
    }

    fn select(
        roots: &[&str],
        table: &[(&'static str, &'static str, &'static str, &[&'static str])],
    ) -> Result<Vec<String>, String> {
        let table = table
            .iter()
            .map(|&(at, c, v, r)| (at, (c, v, r.to_vec())))
            .collect();
        let roots: Vec<Requirement> = roots.iter().map(|r| requirement(r)).collect();
        resolve(&roots, roots.len(), &mut Table(table))
            .map(|found| {
                found
                    .iter()
                    .map(|s: &Selected| {
                        format!(
                            "{} {} {} {}",
                            s.url.canonical(),
                            s.version,
                            s.commit,
                            s.required.join("+")
                        )
                    })
                    .collect()
            })
            .map_err(|e| e.message().to_owned())
    }

    /// The highest required version, not the newest there is, and what it
    /// requires.
    #[test]
    fn the_highest_required_version_is_chosen() {
        let found = select(
            &["a@v1.0.0", "b@v1.0.0"],
            &[
                ("a@v1.0.0", "a1", "1.0.0", &["c@v1.1.0"]),
                ("b@v1.0.0", "b1", "1.0.0", &["c@v1.3.0"]),
                ("c@v1.1.0", "c1", "1.1.0", &["d@v1.0.0"]),
                ("c@v1.3.0", "c3", "1.3.0", &[]),
                ("d@v1.0.0", "d1", "1.0.0", &[]),
            ],
        );
        // Only c 1.1.0, which is not chosen, requires d.
        assert_eq!(
            found.unwrap(),
            [
                "h/a 1.0.0 a1 tag:v1.0.0",
                "h/b 1.0.0 b1 tag:v1.0.0",
                "h/c 1.3.0 c3 tag:v1.1.0+tag:v1.3.0"
            ]
        );
    }

    /// Majors are packages of their own, below 1.0 too (D-239), and packages
    /// may require each other.
    #[test]
    fn majors_and_cycles() {
        let found = select(
            &["a@v1.0.0", "a@v2.0.0", "z@v0.1.3", "z@v0.1.8", "z@v0.2.0"],
            &[
                ("a@v1.0.0", "a1", "1.0.0", &["a@v2.0.0"]),
                ("a@v2.0.0", "a2", "2.0.0", &["a@v1.0.0"]),
                ("z@v0.1.3", "z3", "0.1.3", &[]),
                ("z@v0.1.8", "z8", "0.1.8", &[]),
                ("z@v0.2.0", "z2", "0.2.0", &[]),
            ],
        );
        assert_eq!(
            found.unwrap(),
            [
                "h/a 1.0.0 a1 tag:v1.0.0",
                "h/a 2.0.0 a2 tag:v2.0.0",
                "h/z 0.1.8 z8 tag:v0.1.3+tag:v0.1.8",
                "h/z 0.2.0 z2 tag:v0.2.0"
            ]
        );
    }

    /// The URL is one the root manifest writes, `https` first, then `https` of
    /// any manifest (D-1886).
    #[test]
    fn the_url_that_is_locked() {
        let at = |url: &str| Requirement {
            url: GitUrl::parse(url).unwrap(),
            reference: GitReference::Tag("v1.0.0".to_owned()),
        };
        let (ssh, https) = ("git@h:a.git", "https://h/a");
        let locked = |urls: &[&str], of_root: usize| {
            let table = [("a@v1.0.0", ("a1", "1.0.0", Vec::new()))].into();
            let roots: Vec<Requirement> = urls.iter().map(|url| at(url)).collect();
            let found = resolve(&roots, of_root, &mut Table(table)).unwrap();
            found.first().unwrap().url.source().to_owned()
        };
        assert_eq!(locked(&[ssh, https], 2), https);
        assert_eq!(locked(&[ssh, https], 1), ssh);
        assert_eq!(locked(&[https, ssh], 1), https);
        assert_eq!(locked(&[ssh, https], 0), https);
        assert_eq!(locked(&[ssh, "ssh://h/a"], 0), ssh);
    }

    /// The conflicts of D-238, and a pre-release below its release.
    #[test]
    fn conflicts() {
        let table: &[(&str, &str, &str, &[&str])] = &[
            ("a@v1.0.0", "a1", "1.0.0", &[]),
            ("a@v1.0.0-rc.1", "a0", "1.0.0-rc.1", &[]),
            ("a@aaaaaaa", "a2", "1.0.0", &[]),
            ("a@bbbbbbb", "a3", "1.1.0", &[]),
            ("a@ccccccc", "a4", "1.2.0", &[]),
            ("a@ddddddd", "a1", "1.0.0", &[]),
        ];
        assert_eq!(
            select(&["a@v1.0.0", "a@aaaaaaa"], table).unwrap_err(),
            "two commits of `h/a` claim version 1.0.0: tag:v1.0.0 and rev:aaaaaaa"
        );
        assert_eq!(
            select(&["a@bbbbbbb", "a@ccccccc"], table).unwrap_err(),
            "two commits of `h/a` are required, and a build has one: rev:ccccccc and rev:bbbbbbb"
        );
        assert_eq!(
            select(&["a@v1.0.0", "a@bbbbbbb"], table).unwrap(),
            ["h/a 1.1.0 a3 rev:bbbbbbb+tag:v1.0.0"]
        );
        assert_eq!(
            select(&["a@v1.0.0", "a@ddddddd"], table).unwrap(),
            ["h/a 1.0.0 a1 rev:ddddddd+tag:v1.0.0"]
        );
        assert_eq!(
            select(&["a@v1.0.0", "a@v1.0.0-rc.1"], table).unwrap(),
            ["h/a 1.0.0 a1 tag:v1.0.0+tag:v1.0.0-rc.1"]
        );
        assert_eq!(select(&["a@v9.0.0"], table).unwrap_err(), "no a@v9.0.0");
    }
}
