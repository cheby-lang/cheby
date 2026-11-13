---
status: accepted
date: 2026-11-07
log: D-1347
---

# Two tracks while a file does not parse

While a file has a phase-A error, the language server works from two texts. It publishes that file's phase-A errors from the live text. It checks the other files against the broken file's last good text, so their diagnostics stay real. It answers hover, completion and every other query from a tolerant run on the live text, and it discards that run's diagnostics. `cheby check` does not change: D-452 still stops it at phase A.

The server spends much of its time on text that does not parse. A user who types `xs.` has a grammar error at that keystroke, and that is when completion is wanted. No single run can serve both needs. A strict run gives true diagnostics but no answers about the new text, and a tolerant run gives answers but false diagnostics. So the strict run on last good text, the D run, makes diagnostics, and the tolerant run on live text, the Q run, answers queries.

It costs little. A D run happens only when the hash of its inputs changes, so typing inside a broken file costs one Q run per keystroke, and that is about 8 ms on an example. When no file is broken, one run serves both tracks (D-1359).

The trade is that a published list can describe text that is not on the screen. Another file can report E0042 for a function the user just added to the broken file, because the D run sees that file's last good text. W0005 can flag an item whose only new use is in the broken file. Both clear when the file parses.

## Considered options

- Strict D-452 alone. It is one run with one meaning, but the package's diagnostics blank at each unfinished token, and queries have nothing to read.
- The phases on the broken tree, with their diagnostics published. It needs no second text, but it adds false errors in 6.6% of multi-file runs.
- The broken file left out of the D run. Its importers then show false E0042 for everything they use from it. This form remains only for a file that never parsed, and those importers keep their last list (D-1357).
- Queries answered from last good text, with positions mapped. Mapping cannot answer about text that is new, which is what the user just typed (D-1367).

## Consequences

- The server's diagnostics equal `cheby check`'s only when no file has a phase-A error (D-1348). The exit criterion says so.
- A warning from substituted text may be false, so W0003, W0004 and W0005 offer no removal in that state, and organize imports removes nothing (D-1360).
- A tolerant run hides internal errors, so one may serve as the D run only when nothing was lost (D-1359). Containment must never turn a compiler bug into a clean report.
- Answers on broken code are best effort. D-1380, D-1381 and D-1382 are what make them good: on 1,000 mutants of the examples, 91.8% of hovers equal the clean run's.
- Import cycles differ between the tracks: a Q run cuts one import per cycle, and a D run keeps E0040 (D-1358).
