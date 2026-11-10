# Language server tests

`cargo test -p cheby_cli --test lsp` runs `cheby lsp` the way an editor does, over stdio, one process per trial (D-1352). The harness is `tests/lsp_harness/main.rs`, and its client is `cheby_test_support::lsp`.

## Trial groups

| Group                      | What it checks                                                                                                                                                             | Golden                        |
| -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------- |
| `protocol/<name>`          | Handshake, `shutdown` and `exit`, end of input, a frame that is not JSON-RPC, error answers, `$/cancelRequest`, encodings, publishing, standard output holding frames only | two, in `tests/lsp/protocol/` |
| `format/<name>`            | Formatting of a text no file here can hold, such as CRLF line ends                                                                                                         | none, `cheby fmt` runs        |
| `examples/<name>`          | Every request the server advertises, at every identifier, call and `.` and `\|>` of every example, is answered with no JSON-RPC error. Formatting returns no edit          | none                          |
| `navigation/<name>`        | An answer followed into another file: `std_round_trip` goes from `io::println` into std's sources, opens that file and hovers in it (R-17)                                 | none                          |
| `quality/hover_on_mutants` | The quality gate of plan W7: hover on seeded body mutants of the examples, graded against the clean text. Same at or above 90%, key probe at or above 95%                  | none, the clean text's hovers |
| `symbols/examples/<name>`  | The outline of every source file of an example equals its item list                                                                                                        | none, the parser's items      |
| `parity/examples/<name>`   | The published diagnostics equal `cheby check`'s (D-1348)                                                                                                                   | none, `cheby check` runs      |
| `parity/ui/<name>`         | The published diagnostics equal the `.stderr` golden of a UI check test                                                                                                    | the UI test's own             |
| `golden/<name>`            | The exact answers of a test in this directory                                                                                                                              | `<name>.expected`             |

The examples group sends only what the server's `capabilities` advertise, so it grows stricter as features land (D-1379). Position requests go to the start of every identifier: hover, definition, declaration, typeDefinition, implementation, references, documentHighlight, codeAction, prepareCallHierarchy, selectionRange, rangeFormatting and prepareRename, then rename where prepareRename accepted. Completion goes to the end of every identifier and after every `.` and `|>`, with the context a typed trigger character sends, and every item of its answer must have an edit on one line that holds the position (D-1394). The first item of each answer is then sent to `completionItem/resolve`, if the server advertises it. Signature help goes inside every argument list: after its `(`, after each of its own commas, and at its `)`. Each of the two has a budget per example, 600 points: beyond it the trial keeps every n-th point, the same on every run, and says on standard error how many it dropped. Each item a prepareCallHierarchy answer holds gets `callHierarchy/incomingCalls` and `callHierarchy/outgoingCalls`. Each file gets the document requests: documentSymbol, formatting, foldingRange, documentLink, codeLens, inlayHint, and `semanticTokens/full` and `semanticTokens/range` over the whole text, as the provider's `full` and `range` say. `workspace/symbol` is asked once with an empty query and once with every distinct name of the example. A `…Provider` the harness does not know fails the trial, so a new capability cannot go unswept.

### The quality gate

`quality/hover_on_mutants` measures what `facts.md` §0.1 measured in process, through the real server. Its mutants are §0.1's ten body edits: `dot`, `new_dot`, `open_call`, `half_let`, `open_case`, `pipe_end`, `drop_brace`, `typo`, `drop_arg` and `mismatch`. They sit at the sites §0.1's probe chose, found from the syntax tree: a local is a one-segment path whose name a pattern or a local function of the same item binds, since no name is shadowed (D-051). Every site of every example is shuffled with §0.1's seed, and the first 50 of each kind are kept, so the run is deterministic and a drop is a real change. An example whose clean text has an error, such as 030, which needs a dependency, is left out before the sample is drawn, so every kind gets its full 50.

Each example gets one server, which keeps it warm as an editor would. Every source is opened, and the hover at each identifier of an edited item is asked on the clean text. Each mutant is then sent as an edit of its file, and the hovers are asked again at the same identifiers, moved by the edit. Identifiers in the replaced text are skipped. What is graded is every code block of the answer, joined, so an item's instantiated form counts with its header. An answer whose only block is a header, such as a non-generic function's signature, a module's import or a type parameter's bounds, is not graded: it comes from interfaces, and no body edit can change it. `facts.md` §0.1 graded the type from the facts. An identifier whose clean blocks hold a `_` standing alone is not graded either.

- **Same**: the block is unchanged. The floor is 90%.
- **Key probe**: hover on what an editing feature needs at the cursor. For `dot` it is the receiver, for `new_dot` the name just typed, for `pipe_end` the left side's last name or its callee, and for `open_call` the callee. It passes when the answer still shows a type that is not wholly unknown: not `_`, and not the bare name that a local of unknown type shows when its clean block had `: T`. For `open_call` it needs more, as §0.1 asks: the callee's call signature, the instantiated `fn(…) -> …` block of a generic item or the `fn(…)` type of a local, with no parameter whose type is `_` at the head. An open call whose callee shows no call signature on the clean text, such as a non-generic function, has no key probe. A `pipe_end` whose left side ends in no name, such as a literal, has no key probe. The floor is 95%.

The trial prints both numbers and §0.1's table, by kind: mutants, those with a grammar error, graded identifiers, same, partial, other and missing, same before and after the edit, and key probes.

A frame whose body is not a JSON-RPC message ends the server with status 1 and one line on standard error, and the server answers nothing (D-1430). `protocol/malformed_frame_ends_with_1` sends three such bodies after `initialize` and one before it, each to a fresh server, and waits for the end with the input still open.

The outline of `symbols/examples/*` is the parser's item list, since the harness cannot reach the checker's: each item, with a type's variants or fields, a named variant's fields and an interface's functions as children, by name, kind and nesting. Local functions and imports are not part of it. Every `selectionRange` must lie inside its `range`, and every child inside its parent. The request is sent as soon as the document is open, since a text request never waits for analysis (D-1367).

### Waiting for the server

The client waits for a `publishDiagnostics` of one URI and one version. The server sends one for each new version of an open document, and again after `didOpen` and `didClose`, even with an unchanged list. A run that changes neither the version nor the list sends nothing (D-1442). So no trial waits for a publish after `didSave`, a settings change or a sweep. A request does not wait for such a run either, since it waits only for the text it names (D-1367, D-1443). A trial that needs the server to have caught up sends an edit and waits for that version's publish, since a package's jobs run in order.

Parity opens only `src/main.cheby` and waits for its publish. The other files are published as closed files. A diagnostic is compared by severity, code, the first line of its message, file, and start position. The harness converts the compiler's 1-based code-point columns (D-421) to the protocol's positions itself. A diagnostic with no location, such as E0037, is expected on `cheby.toml` at any position. When a file has a phase-A error, only the files with one and `cheby.toml` are compared, since the other files are checked against last good text (D-1347). Parity leaves out UI tests with `@flags`, run tests, and goldens with an error that has no code. Goldens with a back-end code are ignored until the constants child lands (D-1349).

## Golden tests

A test is one `.cheby` file, which the harness copies into a package named `lsp_test` as `src/main.cheby`. A test may also be a directory with its own `cheby.toml`, copied as it is. A file with `@package: none` is copied alone, under its own name, into a directory with no `cheby.toml` above it, and the server is started as Zed starts one for a file outside its worktree: rooted at that directory, with no workspace folders. The server then serves it in single-file mode (D-1371), as `single_file/type_error` checks. `single_file/names_no_package` checks that hover, the call hierarchy and `workspace/symbol` name no package for it (D-1477). Its expected answers are in `<name>.expected` beside the file or the directory.

The files here are test input, not examples. They need not be valid Cheby, need not be in `cheby fmt`'s form, and are outside `cargo xtask cross-check`.

### Header

The file, or a package's `src/main.cheby`, starts with `// @key: value` lines:

| Key           | Value                                         | Default             |
| ------------- | --------------------------------------------- | ------------------- |
| `@decisions`  | the decisions the test pins, `D-NNN, …`       | none                |
| `@milestone`  | the milestone in which the test must pass     | `M1`                |
| `@client`     | `empty`, `standard` or `full` capabilities    | `standard`          |
| `@encoding`   | the only position encoding the client offers  | none                |
| `@settings`   | the `cheby` settings object, as JSON (D-1376) | none                |
| `@requests`   | document requests sent for every source file  | none                |
| `@package`    | `none`: the file is opened outside a package  | `lsp_test`          |
| `@clientInfo` | the `clientInfo.name` of `initialize`         | `cheby-lsp-harness` |

The `standard` client announces every capability the server reads but no position encodings, so it runs on UTF-16 (D-1368). `full` adds all three encodings. `@clientInfo: Zed` makes the client name itself as Zed does, for the one answer that depends on the name, `filterText` after `xs.` (`features.md` §4.2). `@settings` is sent as `initializationOptions` and as the answer to `workspace/configuration`. `@requests` takes `documentSymbol`, `formatting`, `foldingRange`, `documentLink`, `codeLens`, `semanticTokens` (`full`), `semanticTokensRange` (the whole text) and `inlayHint`.

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
- `//> L:C name args` names the compiler's 1-based line and code-point column instead, for a column a caret cannot reach, such as column 1. `//> L:C-L:C name args` names a range, its end exclusive, which may span lines.
- Ordinary comments between a marker and its line are skipped.

The names are `hover`, `definition`, `declaration`, `typeDefinition`, `implementation`, `references`, `documentHighlight`, `completion [LABEL…]`, `apply LABEL`, `signatureHelp`, `inlayHint`, `prepareRename`, `rename NEW_NAME`, `codeAction [KIND]`, `prepareCallHierarchy`, `incomingCalls`, `outgoingCalls`, `workspaceSymbol [QUERY]`, `selectionRange` and `rangeFormatting`. `references noDeclaration` leaves out the declaration. A code action gets the published diagnostics that touch its range. `incomingCalls` and `outgoingCalls` send `prepareCallHierarchy` at the marker, then the follow-up with each item it prepared: the block is a list of one answer per item, or the prepare answer itself when it prepared none. `workspaceSymbol` sends only its words, joined by spaces, as the query; its position means nothing.

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

### Name features

The goldens under `hover/`, `definition/`, `references/`, `highlight/`, `hierarchy/`, `links/` and `tokens/` each come in three states: `clean`, `type_error` and `grammar_error`, so every answer is pinned on broken text too. `definition/declaration_of_show` comes in the same three states, with `_type_error` and `_grammar_error` after its name (D-1478). `references/clean` and the `links/` tests are packages of more than one module. `tokens/non_ascii` checks columns after text that is not ASCII. Their answers were written by hand and checked position by position against the sources.

- `hover`: the first code block is the declaration (`facts.md` §1.8), printed as `facts.md` §6.1 says. An instantiated form, where it is from, notes and docs follow.
- `semanticTokens`: the raw `data` of the answer, five numbers per token, in the legend the server advertises.

Paths are relative to the package, and materialized std files show as `<std>/…`, so goldens hold no absolute path (D-264). The server's version in `serverInfo` shows as `<version>`. Locations of `references` and the edits of `rename` are sorted by path, line and column.

### Editing features

The goldens under `completion/`, `signature/` and `inlay/` pin plan W8. Their answers are printed in a readable form, not as the raw JSON, and were written by hand.

- `completion/`: one test for each broken form of `features.md` §4.1, such as `call_open`, `case_open` and `let_no_value`, plus `clean` and `type_error`. `dot/`, `pipe/` and `auto_import/` cover `xs.`, `|>` and auto-import, on clean text and with type and grammar errors. `dot/multi_line_receiver` and `dot/dollar_in_receiver` pin plan §1.11's edit rule.
- `signature/`: `clean`, `type_error` and `grammar_error`. `clean` has markers for every kind of callee, nesting, a pipe's hole, a capture and `use`. `type_error` covers a wrong argument and wrong arity, and `grammar_error` recovery from calls that do not parse.
- `inlay/`: one test for each row of `features.md` §7.1, such as `let_types` and `pipe_stages`, plus `type_error`, `settings`, `disabled` and `range_limit`.

```cheby
fn main() {
  let xs = [1, 2]
  let n = xs.le
  //           ^ completion
  //           ^ completion length `case`
  //           ^ apply length
  let m = list::fold(xs, 0, )
  //                        ^ signatureHelp
}
//> 2:1-4:1 inlayHint
```

**The request.** `completion` and `apply` send `textDocument/completion` at the marker. When the character before the position is one of the trigger characters the server advertises, the context is `triggerKind` 2 with that character, as typing it would send. Otherwise it is `triggerKind` 1. `signatureHelp` sends no context. `inlayHint` sends the marker's range: a caret with tildes for a range on one line, `//> L:C-L:C` for one across lines, and a caret alone for an empty range. `@requests: inlayHint` asks over the whole file.

**Completion items.** A `CompletionList` and a bare list of items print alike, as `isIncomplete` and `items`. The list's `itemDefaults` are merged into each item first, as a client merges them. Items are in the client's order: by `sortText`, or the label where there is none, and then as the server sent them.

- `completion` with no words prints every item as one line. The parts are joined by ` | `: the label with `labelDetails.detail` after it, the kind's name (`-` for none), then, where present, `detail`, `labelDetails.description` in parentheses, the text the main edit writes in backticks when it differs from the label, `+N edits` for `additionalTextEdits`, and `deprecated`. So `` "length(xs) | Function | pub fn length<T>(xs: List<T>) -> Int | (list) | ` |> list::length`" ``.
- `completion` with words prints `count`, the number of items, and the whole JSON of each item a word names, in the client's order. A word names the items whose label it is, or one item as `label@description`, with `labelDetails.description` after the `@`. A label with a space is written in backticks: `` `let guard` ``. When the server advertises `resolveProvider`, each named item is sent to `completionItem/resolve` and the resolved item is printed. `data` is left out, since it is the server's own. A word that names no item is listed under `absent`, which is how a test says an item must not be offered.
- `apply LABEL` prints the item that `LABEL` names, resolved as above, and `text`: the file after its edits, one string per line. The main edit is the `textEdit`, or for an `InsertReplaceEdit` its `insert` range. A snippet is expanded as a client would expand it: `${1:f}` gives `f`, `${1|a,b|}` gives `a`, `$1` and `$0` give nothing, and `\$`, `\}` and `\\` give the character. Then `additionalTextEdits` apply to the same text. The result must parse with no syntax error, or the trial fails and names the errors. So a test of an item that completes its construct puts one broken construct in its file, and an item that does not complete one is tested with `completion LABEL`. A `LABEL` that names no item, or several, fails the trial. `cheby fmt`'s form is not required (plan W8, S-24).

Whatever the golden says, every item of every completion answer must have an edit on one line that holds the position (D-1394, and the protocol's rule for `textEdit`). For an `InsertReplaceEdit` both ranges must, and `insert` must be a prefix of `replace`. Its `additionalTextEdits` must not overlap the main edit.

**Signature help** prints the answer with each parameter's label as text: a label given as offsets prints as `"[start, end] text"`, the offsets in the negotiated encoding, and a label given as a string prints as it is. Offsets outside the signature's label fail the trial.

**Inlay hints** print as a list of lines, sorted by position: the compiler's 1-based line and code-point column, the kind (`type`, `parameter` or `-`), and the label in backticks. A label of parts prints each part in backticks, joined by `+`, with a part's location after it as `@path:L:C`. Then, where set, `padding-left`, `padding-right`, `tooltip` and each of `textEdits` as `edit L:C-L:C` with its text in backticks. So `` "3:8 type `: List<Int>` edit 3:8-3:8 `: List<Int>`" ``.

`CHEBY_BLESS=1 cargo test -p cheby_cli --test lsp` writes the goldens, and so does `cargo xtask bless`. Review a blessed change like code (D-292). A test of a later milestone is never blessed, and trials without a golden are ignored under blessing. A missing golden fails with the whole answer in the message, so it can also be written by hand.

## Milestones and platforms

Protocol trials and golden tests use the header default, M1, so they fail CI from the day they land while `tests/milestone` names a later milestone (D-1352). Example trials use the example's milestone, and a `skip-run` example of a later milestone, such as 030, is tolerated. UI parity trials use the UI test's milestone. The strict runs of D-544 apply:

```sh
CHEBY_PENDING=require cargo test -p cheby_cli --test lsp
```

On Windows, until M6, the `parity/examples/*` trials are ignored, since they run `cheby check`, which builds a JIT for a package that passes the front end, and so are the UI parity trials whose golden has no front-end error, as their check tests are (D-545, D-641). The server never runs generated code (D-1349), so every other trial runs there.

Every `cheby` gets the harness's fresh `CHEBY_CACHE` (D-959). `CHEBY_HARNESS_SCALE` multiplies the timeout of each wait (D-722). `cargo xtask coverage` reads the `@decisions` of the tests here.
