---
status: accepted
date: 2026-10-21
log: D-994
---

# Unbuffered file writes

Writes to a `file::File` are not buffered. Each `write`, `write_line` and `write_bytes` is one job on the blocking pool, which returns once the operating system has the data. A write that returned `Ok` is in the file, and a file's drop function only closes the descriptor. There is no `flush`. Reads are buffered inside the handle, as standard input's are.

A drop function may not block or fail (D-171, D-703), so it cannot flush a buffer. Any buffered design must then either lose data at drop, or flush where nobody sees the error.

## Considered options

- Buffered writes flushed by the drop function: the flush would block and could fail, where neither is allowed.
- Buffered writes whose drop function hands the flush to the pool as a job nobody waits for: the error is lost, and a program that reads the file right after the handle's last use races the flush.
- Buffered writes with an explicit `flush`, and silent loss of what is left at drop.
- A separate buffered-writer type now: more API than v1 needs.

## Consequences

- Every target behaves the same, and a file is complete once its last write returned.
- Each write call costs one pool round trip. A program that writes many small pieces builds its text first.
- A buffered writer can be added later as a new type without breaking a program.
