# REPL transcripts

Each `<name>.repl` file is the output of a piped `cheby repl` session (plan §8.3, D-546, D-548),
which the harness `tests/repl_harness/main.rs` (`cargo test -p cheby_cli --test repl`)
replays and compares:

- A line that starts with `> ` is the first line of an entry, after its prompt, and a line that
  starts with `. ` continues it while a bracket, a block or a string is open. The REPL echoes
  both, so they are input and output at once.
- A line that starts with `< ` is a line of standard input that an entry reads with
  `io::read_line`. It is fed right after the input line before it and is not echoed, so it is
  not part of the output.
- Every other line is expected output: standard output and standard error, merged in the order
  of writes, since the harness gives `cheby repl` one pipe for both.

A transcript may start with a header of `// @key: value` lines, before its first input line, as
a UI test's header does. The header is not part of the session: it is neither fed nor expected,
and blessing keeps it. It has two keys:

| Key          | Value                                                                                         |
| ------------ | --------------------------------------------------------------------------------------------- |
| `@decisions` | the decisions the session pins, `D-NNN, …`, which `cargo xtask coverage` counts (D-638)       |
| `@milestone` | the milestone in which the transcript must pass, `M1` by default, given at most once (D-726) |

The milestone is judged against `tests/milestone` as for a UI test (D-288): a transcript of an
earlier milestone must pass, one of the milestone in progress is pending and only reported, and
one of a later milestone must fail. `CHEBY_MILESTONE`, `CHEBY_PENDING=require` and
`CHEBY_PENDING_ALLOW` make a strict run, as described in [`tests/ui/README.md`](../ui/README.md)
(D-544), and the harness prints the pending transcripts at the end. Blessing skips a transcript
of a later milestone. A header that does not parse fails the transcript. On Windows the harness
ignores every transcript until M6 (D-545).

The session ends at the end of input, and `cheby repl` must then exit with status 0. A session
runs in an empty directory, unless a directory `<name>/` next to the transcript holds a
`cheby.toml`: then it runs inside a copy of that package in a scratch directory, whose
modules it can import. An entry may change the copy's files, as `reload.repl` does before
`:reload`.

`CHEBY_BLESS=1 cargo test -p cheby_cli --test repl` rewrites every transcript with the actual
output, keeping its `< ` lines; read each diff before keeping it. `cargo xtask cross-check`
parses every entry with both parsers (D-272).
