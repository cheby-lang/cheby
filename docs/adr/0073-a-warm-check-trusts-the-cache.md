---
status: accepted
date: 2026-10-24
log: D-1132, D-1133
---

# A warm check trusts the cache

A warm `cheby check` checks no module twice. It reads one package index per package root and command family first. A module whose source hash matches its index entry is not parsed, a hit is not checked unless constant evaluation needs it (D-1134), and a hit's interface is decoded only when a checked module needs it. A miss is checked against the decoded interfaces of its dependencies and written from that check. A command writes its index only when the bytes change, so a warm check with no edit opens the index and the non-empty diagnostics blobs, and writes nothing.

The output still equals a cold check's (D-268), through two paths off the fast one. A module whose check against a decoded interface reports an error is checked again from source on fresh interfaces (D-1136). If anything the index vouched for turns out wrong, such as an import that no longer resolves to the same path or a grammar error in a module parsed later, the front end starts again once with every module parsed and each hit's interface decoded at its lookup.

The trade is speed against a second check. Wave 2 checked every hit and every reader again from source, and a warm check cost +4% to +52% against wave 1. With this decision a warm `cheby check` is 24% to 84% faster than wave 1's in every cell of wave 3's gate, and on the 1M-line `wide` project it parses no module and writes no file.

## Considered options

- Check every hit and every reader again from source (wave 2's text). Output equals a cold check's by construction, but every warm check pays for the modules it did not change.
- No package index: read each module's hint and record files. It needs no restart, but costs several opens per module where D-962 asks for one.
- Write the index on every command. It is simpler, but a warm command on the 1M-line `wide` project then rewrites about 16.9 MB, and the warm path is no longer write-free.
- Trust the index without a restart. In wave 3's review a lost diagnostics blob made a warm check an internal error ("interface published twice"), and a phase-A error elsewhere dropped an unparsed module's W0001 from the output.

## Consequences

- K4a and K4c build on this warm path. Until they land, `cheby run`, `cheby test` and `cheby build`'s front end check each module once from source, because their lowering needs every checked body, and warm edit-to-run stays +13% to +36% on the `wide` projects.
- Correctness rests on the error path and the restart, not on a second check. A warm check with an error rechecks that module and the hits it reads, and a wrong index costs one restart, never different output.
- The index is sealed, so a damaged record or index is a miss. A bad or stale index is ignored.
- Every change to the warm path must keep D-268's checks passing: the UI suite twice on one cache, `check-determinism` and the cache-hit tests.
- A cold `cheby check` still costs +32% to +102% against wave 1 at CP1, most of it in store writes (D-1138). The user accepted that cost until CP3, which reads the cache's layout and cold cost again.
