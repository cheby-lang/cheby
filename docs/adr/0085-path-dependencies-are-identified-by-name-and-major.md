---
status: accepted
date: 2026-11-24
log: D-1857
---

# Path dependencies are identified by name and major

A package that depends on a local directory needs an identity for it. The identity decides which types are the same, and it is written into symbols, type descriptors, panic locations and cache keys (D-238, D-1313). It must hold no machine path, so that builds stay deterministic (D-264).

A path dependency is identified by the `name` in its own manifest and its major version. Two directories with one such identity in one build are an error. The root package keeps its plain module paths, and every dependency carries its identity (D-1863).

## Considered options

- **The path relative to the root package.** Two packages of one name could be in one build, but a package would have another identity in every project, and compiled code could not be shared.
- **A git URL beside the path.** One rule for every dependency (D-238), but a package has a URL only once it is published.

## Consequences

- Two local packages of one name and major cannot be in one build.
- A package reached both by path and by git is two packages. The root manifest's override gives a directory the git package's identity (D-1860).
- A package is compiled once as a root and once as a dependency.
