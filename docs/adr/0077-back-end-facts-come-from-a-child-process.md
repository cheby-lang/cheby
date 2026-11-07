---
status: accepted
date: 2026-11-07
log: D-1349
---

# Back-end facts come from a child process

The language server process runs the front end and nothing after it. Whatever needs the back end comes from a child `cheby` process, started by the hidden `cheby __backend`, off the request path. Constants are evaluated there, and memory facts are computed there (D-1410, D-1416). The server process never lowers MIR and never runs generated code.

Evaluating constants in the server would put generated code inside a process the user depends on for every keystroke:

- Kernels cannot be interrupted, so a slow constant could not be stopped. The only sure way to stop generated code is to kill its process.
- A crash in generated code would end the server.
- `Runtime::new` is `Unsupported` on Windows until M6 (D-714), so the server would need a path of its own there. With a child, it simply starts none.
- Evaluation in process leaks about 10 KB per check, which adds up over a long session.

The trade is freshness. A child reads the disk, so every back-end fact describes the last save, never the live text. E0069 and E0075 to E0078 arrive when the child finishes, and so can be missing for a moment after a save, and memory marks do not follow typing. One child pays about 200 ms to start evaluating. Results are matched to text by hash, so a stale one is hidden and never shown at a wrong position (D-1412, D-1422).

Memory facts run no generated code, but they need the back end's lowering. They run in a child too, which keeps MIR out of the server and costs 20 to 60 ms per child on example 015.

## Considered options

- A thread in the server process. It has no start-up cost and could read live text, but it has the problems above.
- One child for both jobs. It computes the front end once, but constants can take seconds and memory facts take milliseconds. A 60 s constant would starve the memory facts, and each save would kill both.
- `Mode::Build` in the child. It compiles the package's C on every save. `Mode::Check` compiles no C and loads no library.
- Children only on save. Opening a repository then runs nothing, but diagnostics differ from `cheby check` until a save, and hover says "not evaluated yet" on every constant. The user chose a first run at the first clean report (D-1411).

## Consequences

- The exit criterion names the states in which constant diagnostics are absent (D-1348).
- Windows has no constant evaluation in the server until M6. Memory facts run there, since they run no generated code (D-1414).
- Opening a clean package runs generated code in a child. §13.9 bounds it, it links no C, and the worst case is CPU time.
- A run that is cut short must show, so child runs use `$/progress`, and one `window/showMessage` per session reports the cut (D-1415).
- A feature that wants a back-end fact on live text cannot have it. The tail marker reads compiled MIR on save for that reason, and a live marker would need a second copy of §5.15.2 (D-1423).
