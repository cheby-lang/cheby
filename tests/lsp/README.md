# Language server tests

`cargo test -p cheby_cli --test lsp` runs `cheby lsp` the way an editor does, over stdio, one process per trial (D-1352). The harness is `tests/lsp_harness/main.rs`, and its client is `cheby_test_support::lsp`.

## Trial groups

| Group                    | What it checks                                                                                                                               | Golden                        |
| ------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------- |
| `protocol/<name>`        | Handshake, `shutdown` and `exit`, end of input, error answers, `$/cancelRequest`, encodings, publishing, standard output holding frames only | two, in `tests/lsp/protocol/` |
| `examples/<name>`        | Every request the server advertises, at every identifier of every example, is answered with no JSON-RPC error. Formatting returns no edit    | none                          |
| `parity/examples/<name>` | The published diagnostics equal `cheby check`'s (D-1348)                                                                                     | none, `cheby check` runs      |
| `parity/ui/<name>`       | The published diagnostics equal the `.stderr` golden of a UI check test                                                                      | the UI test's own             |
| `golden/<name>`          | The exact answers of a test in this directory                                                                                                | `<name>.expected`             |

The examples group sends only what the server's `capabilities` advertise, so it grows stricter as features land (D-1379).

Parity opens only `src/main.cheby` and waits for its publish. The other files are published as closed files. A diagnostic is compared by severity, code, the first line of its message, file, and start position. The harness converts the compiler's 1-based code-point columns (D-421) to the protocol's positions itself. A diagnostic with no location, such as E0037, is expected on `cheby.toml` at any position. When a file has a phase-A error, only the files with one and `cheby.toml` are compared, since the other files are checked against last good text (D-1347). Parity leaves out UI tests with `@flags`, run tests, and goldens with an error that has no code. Goldens with a back-end code are ignored until the constants child lands (D-1349).

## Golden tests

A test is one `.cheby` file, which the harness copies into a package named `lsp_test` as `src/main.cheby`. A test may also be a directory with its own `cheby.toml`, copied as it is. Its expected answers are in `<name>.expected` beside the file or the directory.

The files here are test input, not examples. They need not be valid Cheby, need not be in `cheby fmt`'s form, and are outside `cargo xtask cross-check`.

### Header

The file, or a package's `src/main.cheby`, starts with `// @key: value` lines:

| Key          | Value                                         | Default    |
| ------------ | --------------------------------------------- | ---------- |
| `@decisions` | the decisions the test pins, `D-NNN, …`       | none       |
| `@milestone` | the milestone in which the test must pass     | `M1`       |
| `@client`    | `empty`, `standard` or `full` capabilities    | `standard` |
| `@encoding`  | the only position encoding the client offers  | none       |
| `@settings`  | the `cheby` settings object, as JSON (D-1376) | none       |
| `@requests`  | document requests sent for every source file  | none       |

The `standard` client announces every capability the server reads but no position encodings, so it runs on UTF-16 (D-1368). `full` adds all three encodings. `@settings` is sent as `initializationOptions` and as the answer to `workspace/configuration`. `@requests` takes `documentSymbol`, `formatting`, `foldingRange`, `documentLink`, `codeLens`, `semanticTokens` and `inlayHint`.

### Markers

A marker is a comment line that points at the nearest line above it that is not a comment:

```cheby
fn main() {
  let total = add(1, 2)
  //          ^ hover
  //      ^~~~~ codeAction quickfix
}
```

- `^ name args` puts the position at the caret's column. `^~~~` makes a range from the caret to the end of the tildes.
- `//> L:C name args` names the compiler's 1-based line and code-point column instead, for a column a caret cannot reach, such as column 1.
- Ordinary comments between a marker and its line are skipped.

The names are `hover`, `definition`, `declaration`, `typeDefinition`, `implementation`, `references`, `documentHighlight`, `completion`, `signatureHelp`, `prepareRename`, `rename NEW_NAME`, `codeAction [KIND]`, `prepareCallHierarchy`, `selectionRange` and `rangeFormatting`. `references noDeclaration` leaves out the declaration. A code action gets the published diagnostics that touch its range.

### The `.expected` file

The file is a list of blocks. Each block is a line `== <label>` followed by pretty-printed JSON, with keys sorted:

- `== diagnostics`: the last published list of every file with a non-empty one.
- `== <file>:<line>:<col> <name>`: one block per marker, in file and line order. A request that fails shows `{"error": {"code", "message"}}`.
- `== <file> <name>`: one block per `@requests` entry.

Paths are relative to the package, and materialized std files show as `<std>/…`, so goldens hold no absolute path (D-264). The server's version in `serverInfo` shows as `<version>`. Locations of `references` and the edits of `rename` are sorted by path, line and column.

`CHEBY_BLESS=1 cargo test -p cheby_cli --test lsp` writes the goldens, and so does `cargo xtask bless`. Review a blessed change like code (D-292). A test of a later milestone is never blessed, and trials without a golden are ignored under blessing. A missing golden fails with the whole answer in the message, so it can also be written by hand.

## Milestones and platforms

Protocol trials and golden tests use the header default, M1, so they fail CI from the day they land while `tests/milestone` names a later milestone (D-1352). Example trials use the example's milestone, and a `skip-run` example of a later milestone, such as 030, is tolerated. UI parity trials use the UI test's milestone. The strict runs of D-544 apply:

```sh
CHEBY_PENDING=require cargo test -p cheby_cli --test lsp
```

On Windows, until M6, the `parity/examples/*` trials are ignored, since they run `cheby check`, which builds a JIT for a package that passes the front end, and so are the UI parity trials whose golden has no front-end error, as their check tests are (D-545, D-641). The server never runs generated code (D-1349), so every other trial runs there.

Every `cheby` gets the harness's fresh `CHEBY_CACHE` (D-959). `CHEBY_HARNESS_SCALE` multiplies the timeout of each wait (D-722). `cargo xtask coverage` reads the `@decisions` of the tests here.
