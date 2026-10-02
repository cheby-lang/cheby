---
status: accepted
date: 2026-10-02
log: D-268
---

# Per-command processes, a build server only as an accelerator

Every `cheby` command runs as its own process, and the content-addressed disk cache (D-255) is the only state shared between commands. The language server keeps its own state in memory. By estimate, a one-function edit in a 1M-line project costs about 150-350 ms without a server (stat checks, mapping interface artifacts, compiling one module, loading cached code), which fits the 500 ms budget of D-247. A persistent build server may be added later, but only as a pure accelerator: its results must be bit-identical to a fresh process (D-264), and nothing may depend on it for correctness.

## Considered options

- A persistent build server from the start (Bazel, Buck2, Gradle): lowest latency and shared state between the CLI and the language server, but stale state, client-server version skew, zombie processes, resident memory and unreliable file watchers, and hard to remove once tooling relies on it.
- Per-command processes with no rule about a later server: the same today, but a future server could quietly become a source of truth.

## Consequences

- Every command pays change detection and artifact loading. Keeping interface artifacts cheap to map is part of meeting D-247.
- A future server can always be bypassed, for example with a `--no-server` flag, and CI can compare both paths.
