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

A transcript may start with a header of `// @decisions: D-NNN, …` lines, before its first input
line, naming the decisions the session pins, as a UI test's header does. The header is not part
of the session: it is neither fed nor expected, blessing keeps it, and `cargo xtask coverage`
counts its decisions (D-638). On Windows the harness ignores every transcript until M6 (D-545).

The session ends at the end of input, and `cheby repl` must then exit with status 0. A session
runs in an empty directory, unless a directory `<name>/` next to the transcript holds a
`cheby.toml`: then it runs inside a copy of that package in a scratch directory, whose
modules it can import. An entry may change the copy's files, as `reload.repl` does before
`:reload`.

`CHEBY_BLESS=1 cargo test -p cheby_cli --test repl` rewrites every transcript with the actual
output, keeping its `< ` lines; read each diff before keeping it. `cargo xtask cross-check`
parses every entry with both parsers (D-272).
