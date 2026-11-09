# Language server tests

`cargo test -p cheby_cli --test lsp` runs `cheby lsp` the way an editor does, over stdio, one process per trial (D-1352). The harness is `tests/lsp_harness/main.rs`, and its client is `cheby_test_support::lsp`.

## Trial groups

| Group                     | What it checks                                                                                                                                                             | Golden                        |
| ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------- |
| `protocol/<name>`         | Handshake, `shutdown` and `exit`, end of input, a frame that is not JSON-RPC, error answers, `$/cancelRequest`, encodings, publishing, standard output holding frames only | two, in `tests/lsp/protocol/` |
| `format/<name>`           | Formatting of a text no file here can hold, such as CRLF line ends                                                                                                         | none, `cheby fmt` runs        |
| `examples/<name>`         | Every request the server advertises, at every identifier of every example, is answered with no JSON-RPC error. Formatting returns no edit                                  | none                          |
| `symbols/examples/<name>` | The outline of every source file of an example equals its item list                                                                                                        | none, the parser's items      |
| `parity/examples/<name>`  | The published diagnostics equal `cheby check`'s (D-1348)                                                                                                                   | none, `cheby check` runs      |
| `parity/ui/<name>`        | The published diagnostics equal the `.stderr` golden of a UI check test                                                                                                    | the UI test's own             |
| `golden/<name>`           | The exact answers of a test in this directory                                                                                                                              | `<name>.expected`             |

The examples group sends only what the server's `capabilities` advertise, so it grows stricter as features land (D-1379).

A frame whose body is not a JSON-RPC message ends the server with status 1 and one line on standard error, and the server answers nothing (D-1430). `protocol/malformed_frame_ends_with_1` sends three such bodies after `initialize` and one before it, each to a fresh server, and waits for the end with the input still open.

The outline of `symbols/examples/*` is the parser's item list, since the harness cannot reach the checker's: each item, with a type's variants or fields, a named variant's fields and an interface's functions as children, by name, kind and nesting. Local functions and imports are not part of it. Every `selectionRange` must lie inside its `range`, and every child inside its parent. The request is sent as soon as the document is open, since a text request never waits for analysis (D-1367).

### Waiting for the server

The client waits for a `publishDiagnostics` of one URI and one version. The server sends one for each new version of an open document, and again after `didOpen` and `didClose`, even with an unchanged list. A run that changes neither the version nor the list sends nothing (D-1442). So no trial waits for a publish after `didSave`, a settings change or a sweep. A request does not wait for such a run either, since it waits only for the text it names (D-1367, D-1443). A trial that needs the server to have caught up sends an edit and waits for that version's publish, since a package's jobs run in order.

Parity opens only `src/main.cheby` and waits for its publish. The other files are published as closed files. A diagnostic is compared by severity, code, the first line of its message, file, and start position. The harness converts the compiler's 1-based code-point columns (D-421) to the protocol's positions itself. A diagnostic with no location, such as E0037, is expected on `cheby.toml` at any position. When a file has a phase-A error, only the files with one and `cheby.toml` are compared, since the other files are checked against last good text (D-1347). Parity leaves out UI tests with `@flags`, run tests, and goldens with an error that has no code. Goldens with a back-end code are ignored until the constants child lands (D-1349).

## Golden tests

A test is one `.cheby` file, which the harness copies into a package named `lsp_test` as `src/main.cheby`. A test may also be a directory with its own `cheby.toml`, copied as it is. A file with `@package: none` is copied alone, under its own name, into a directory with no `cheby.toml` above it, and the server is started as Zed starts one for a file outside its worktree: rooted at that directory, with no workspace folders. The server then serves it in single-file mode (D-1371), as `single_file/type_error` checks. Its expected answers are in `<name>.expected` beside the file or the directory.

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
| `@package`   | `none`: the file is opened outside a package  | `lsp_test` |

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

`foldingRange` answers are sorted by start, then end, since their order means nothing. Whatever the golden says, a `formatting` answer applied to the file must give `cheby fmt -`'s output byte for byte, `[]` when the file is already in its form, and `null` when `cheby fmt` refuses it. Every edit of a `rangeFormatting` answer must touch the line of its marker (D-1399), except one that only removes and inserts `import` lines: the formatter sorts imports, and the other half of a moved import is kept with the half the range touches.

### Text features

The goldens under `format/`, `symbols/` and `folding/` are written by hand from `features.md` §5.1, §5.3 and §5.4. Where that text leaves the rendering open, they take the simplest JSON:

- `formatting` and `rangeFormatting`: one `TextEdit` per line-diff hunk, replacing whole lines, from column 0 of the hunk's first line to column 0 of the line after it, with the new lines and their line ends as `newText`. The edits of `formatting` use LF, so a CRLF file is converted, as `cheby fmt` converts it (`features.md` §5.4). `format/crlf_is_converted` checks it. `rangeFormatting` compares lines with a CR before the LF ignored, and writes a kept hunk with CRLF where the lines it replaces have it.
- `documentSymbol`, hierarchical: `range` is the item's node, from its first doc comment or attribute to its last token, and `selectionRange` its name, or a test's string literal with its quotes. A function's or an interface function's `detail` is its parameter list and, after one space, its return type, printed on one line, with comments, line breaks and trailing commas dropped. A positional variant's `detail` is its field list. A field's and a constant's `detail` is the type as written, and an alias's the aliased type. An alias is a `TypeParameter`. `children` and `detail` are left out when there are none. `@deprecated` gives `"tags": [1]` to a client whose `tagSupport` lists it, as the `standard` client's does, and `"deprecated": true` to any other.
- `documentSymbol`, flat, for a client without `hierarchicalDocumentSymbolSupport`: one `SymbolInformation` per symbol in the same order, a child with its parent's name as `containerName`.
- `foldingRange`, for a client without `lineFoldingOnly`: the node's range, from the opener to the end of the closer, in the client's encoding. A block or a body of variants, fields, interface functions or case arms has no `kind`. A multi-line list, argument list, parameter list or pipeline has `"region"`, a run of imports `"imports"`, and a run of comments of any kind on adjacent lines `"comment"`, so a test's header and module doc fold together.

Paths are relative to the package, and materialized std files show as `<std>/…`, so goldens hold no absolute path (D-264). The server's version in `serverInfo` shows as `<version>`. Locations of `references` and the edits of `rename` are sorted by path, line and column.

`CHEBY_BLESS=1 cargo test -p cheby_cli --test lsp` writes the goldens, and so does `cargo xtask bless`. Review a blessed change like code (D-292). A test of a later milestone is never blessed, and trials without a golden are ignored under blessing. A missing golden fails with the whole answer in the message, so it can also be written by hand.

## Milestones and platforms

Protocol trials and golden tests use the header default, M1, so they fail CI from the day they land while `tests/milestone` names a later milestone (D-1352). Example trials use the example's milestone, and a `skip-run` example of a later milestone, such as 030, is tolerated. UI parity trials use the UI test's milestone. The strict runs of D-544 apply:

```sh
CHEBY_PENDING=require cargo test -p cheby_cli --test lsp
```

On Windows, until M6, the `parity/examples/*` trials are ignored, since they run `cheby check`, which builds a JIT for a package that passes the front end, and so are the UI parity trials whose golden has no front-end error, as their check tests are (D-545, D-641). The server never runs generated code (D-1349), so every other trial runs there.

Every `cheby` gets the harness's fresh `CHEBY_CACHE` (D-959). `CHEBY_HARNESS_SCALE` multiplies the timeout of each wait (D-722). `cargo xtask coverage` reads the `@decisions` of the tests here.
