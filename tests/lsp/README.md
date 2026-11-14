# Language server tests

`cargo xtask test -E 'binary_id(cheby_cli::lsp)'` runs `cheby lsp` the way an editor does, over stdio, one process per trial (D-1352). The harness is `tests/lsp_harness/main.rs`, and its client is `cheby_test_support::lsp`.

## Trial groups

| Group                      | What it checks                                                                                                                                                             | Golden                        |
| -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------- |
| `protocol/<name>`          | Handshake, `shutdown` and `exit`, end of input, a frame that is not JSON-RPC, error answers, `$/cancelRequest`, encodings, publishing, standard output holding frames only | two, in `tests/lsp/protocol/` |
| `format/<name>`            | Formatting of a text no file here can hold, such as CRLF line ends                                                                                                         | none, `cheby fmt` runs        |
| `examples/<name>`          | Every request the server advertises, at every identifier, call and `.` and `\|>` of every example, is answered with no JSON-RPC error. Formatting returns no edit          | none                          |
| `navigation/<name>`        | An answer followed into another file: `std_round_trip` goes from `io::println` into std's sources, opens that file and hovers in it (R-17)                                 | none                          |
| `quality/hover_on_mutants` | The quality gate of plan W7: hover on seeded body mutants of the examples, graded against the clean text. Same at or above 90%, key probe at or above 95%                  | none, the clean text's hovers |
| `symbols/examples/<name>`  | The outline of every source file of an example equals its item list                                                                                                        | none, the parser's items      |
| `parity/examples/<name>`   | The published diagnostics, after `cheby/idle`, equal `cheby check`'s (D-1348)                                                                                              | none, `cheby check` runs      |
| `parity/ui/<name>`         | The published diagnostics, after `cheby/idle`, equal the `.stderr` golden of a UI check test                                                                               | the UI test's own             |
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

A trial that needs a save's jobs to have finished, the constants child included, waits for `cheby/idle`: the server sends it for a package when nothing is running or due for it (D-1415). Every client of the harness but `empty` asks for it with `experimental.chebyIdle`. The wait takes the first `cheby/idle` whose `root` is the package's, or the file's in single-file mode, and that comes after the last message the client read. Earlier ones, and every other notification not yet taken, are dropped, since they tell of a state before the edits and saves the client sent. So are the notifications read before it. Its payload must be `root` and a whole `generation`, 0 when the root has had no snapshot since the server started, as for a package whose manifest gives none (D-1492, D-1499), and that generation must be at least the one of every earlier `cheby/idle` of the same session, since it never goes back (D-1495). The server keeps the count of a root for its whole life, also across a session that ends and starts again (D-1499). No golden ends a session, so `single_file_tests.rs` checks that in process.

Parity opens only `src/main.cheby`, waits for its publish and then for `cheby/idle`, so the constants child's diagnostics are in: E0069 from evaluation and E0075 to E0078, of the codes of `ui::BACK_END_CODES` (D-1349). E0053 is in that list too, but `cheby check` never reports it (D-532). The other files are published as closed files. A diagnostic is compared by severity, code, the first line of its message, file, and start position. The lines after the first carry the rest of `cheby check`'s text (D-1509), which the goldens pin: `diagnostics/mismatched_types_shows_its_label` shows the primary label, and `diagnostics/division_by_zero_note`, which waits for `cheby/idle` and so is ignored on Windows, shows an E0075 from the child with the `note:` that gives the panic's reason. The harness converts the compiler's 1-based code-point columns (D-421) to the protocol's positions itself. A diagnostic with no location, such as E0037, is expected on `cheby.toml` at any position. When a file has a phase-A error, only the files with one and `cheby.toml` are compared, since the other files are checked against last good text (D-1347). Parity leaves out UI tests with `@flags`, run tests, and goldens with an error that has no code. It has no skip list.

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
| `@steps`      | what runs after the opens, in order           | none                |
| `@rewrite`    | `bom`, `crlf` or both: the form of the copy   | none                |

The `standard` client announces every capability the server reads but no position encodings, so it runs on UTF-16 (D-1368). It announces `insertTextModeSupport` with `asIs` and `adjustIndentation`, so a completion item whose text spans lines shows `insertTextMode: 2` (D-1506). `full` adds all three encodings. `@clientInfo: Zed` makes the client name itself as Zed does, for the one answer that depends on the name, `filterText` after `xs.` (`features.md` §4.2). `@settings` is sent as `initializationOptions` and as the answer to `workspace/configuration`.

`@steps` is a list of steps, run after every source file is opened and published, before the markers' requests:

- `idle` waits for `cheby/idle` of the package, as "Waiting for the server" says.
- `started` waits for the `$/progress` begin of a child run, whose token starts with `cheby/child/`.
- `quiet` waits for `cheby/idle` as `idle` does, and fails the trial if a child run began since the last `cheby/idle` the client read.
- `settings NAME` pushes the settings object in `NAME.json`, at the package's root, with `workspace/didChangeConfiguration`, and makes it the client's answer to `workspace/configuration`. It waits for nothing.
- `edit DIR` makes each file under the test's `DIR/` the text of the open document at the same path below the package, `DIR/src/other.cheby` for `src/other.cheby`, unsaved, and waits for its publish.
- `save DIR` does what `edit` does, and also writes the file to disk and sends `didSave`.
- `zed-save DIR` does what `save` does, in the order Zed 1.27 sends a save: it writes the file, sends `workspace/didChangeWatchedFiles` for it, then `didSave` 5 ms later and `didSave` again.

A test with `edit` or `save` is a package, and its markers are read from each document's text after the steps, so the files under `DIR/` hold the markers of the documents they replace. `@rewrite: bom` starts every source file of the copy with a BOM, and `@rewrite: crlf` ends its lines with `\r\n`, which no file here can hold (`.gitattributes`). Files under a `DIR/` take the same form on disk. The documents are opened and edited as Zed holds such files, with no BOM and `\n` line ends, so the open text differs from disk as it does in the editor. `@requests` takes `documentSymbol`, `formatting`, `foldingRange`, `documentLink`, `codeLens`, `semanticTokens` (`full`), `semanticTokensRange` (the whole text) and `inlayHint`.

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

### Constants

The goldens under `constants/` pin plan W9: constants are evaluated by a child process (D-1349, D-1410), and its diagnostics and values reach the client. Each waits for `cheby/idle` with `@steps`, except `std_constant_shows_initializer`, since std constants show their initializer as written and no child evaluates them (D-1413).

- `panic_gives_e0075`: E0075 at the constant's name, and hover says "Evaluation failed, see `E0075`."
- `value_reaches_hover`: an `Int`, a `String` and a `List` in their debug text, joined to the header, at a declaration and at a use.
- `killed_loop_constant`: a constant that never ends, under limits of 300 s and 600 s, far above the harness's wait. A save while its child runs (`started`) kills it, so `cheby/idle` comes in time, and the saved value reaches hover (D-1411).
- `e0077_prints_nothing`: E0077 for a constant that reaches `io::println`, with standard output holding frames only, which every golden trial checks.
- `one_failing_constant_keeps_other_values`: after a save that makes one constant panic, that one says it failed and the others show their last values, "`40` when last saved" (D-1413, S-20).
- `dirty_buffer_does_not_stop_evaluation`: a save evaluates with another file's buffer unsaved. The saved file's value is current, and the unsaved file's, read from disk, is stale (D-1411, D-1412, S-27).
- `bom_file_keeps_child_diagnostics` and `crlf_file_keeps_child_diagnostics`: `panic_gives_e0075` with `@rewrite`, whose E0075 lands at the same place, since the child's ranges and hashes are over normalized text (D-1412, R-15, S-30). The open document has no BOM and `\n` line ends while disk keeps them, so a hash of the exact text on either side would hide the diagnostic.
- `target_native_skips_e0078` and `no_target_gives_e0078`: a constant past the JS range in a package that targets `native` and `js`. With `analysis.target` set to `native` the child gets `--target native`, so no E0078 is published, as `cheby check --target native` prints none. With the setting unset, E0078 is published, as `cheby check` prints it (D-1489, D-540).
- `failed_run_leaves_no_value` and `cut_short_leaves_no_value`: a constant beside one that panics, and beside one that never ends under a limit of 1 s. Neither has a value, not even a stale one, and hover says "Not evaluated: the last evaluation failed or was cut short (see `cheby/status`)." (D-1490). `cheby/idle` after the cut comes once the server killed the child, so the second costs one second.
- `raising_the_limit_reruns`: a run cut short by a limit of 2 s. Lowering the limit to 1 s and saving unchanged text start no run (`quiet`), and raising it to 600 s starts one at once (`started`, D-1493). A save then gives a constant that ends, whose run must finish, so the limit is far above what a slow runner needs.
- `saving_an_edit_after_a_cut_reruns`: a run cut short by a limit of 1 s, an unsaved edit that fixes it, and a limit raised to 10 s, whose run reads the saved text and is cut short too. Saving the edit changes the text on disk, so it runs again (`started`) and its value reaches hover: a cut run is keyed by the text the child reads, not by the buffers (D-1412, D-1493, D-1494).
- `unchanged_save_keeps_the_run`: a constant that takes some seconds. Saving unchanged text while its child runs leaves the child running, so no second run begins (`quiet`), and its value reaches hover (D-1494).
- `target_change_reruns`: the package of `target_native_skips_e0078`, then a settings change to `js`. The change makes a constants run due at once, with no save, so E0078 is published after `cheby/idle` (D-1498). It rebuilds the session, whose snapshots number from 1 again, and the generation of `cheby/idle` does not go back (D-1495).
- `target_change_reruns_a_cut_run`: a run cut short by a limit of 1 s. Saving unchanged text starts no run (`quiet`, D-1493), and a change of `analysis.target` starts one at once (`started`), although the text on disk has not changed since the cut (D-1498, D-1500). That run is cut short too, so hover says so (D-1490).
- `skipped_for_errors_on_disk`: a saved file with a type error and a buffer that fixes it, unsaved. The buffer gives the session's first clean report, so the constants run (D-1411), but the child reads the saved file and skips evaluation. Hover says "Not evaluated yet: the saved files have errors; values are computed on save." (D-1497).
- `saved_errors_start_no_run`: a saved file with a type error, then a save that leaves another one on disk. No report is clean, so the server starts no run, and hover says the same as after a run the child skipped (D-1497, D-1501). The hover comes after the save, so this golden does not show what a session opened over such files reads before a save. `child_tests.rs` checks that (D-1504).
- `unsaved_edit_keeps_saved_errors`: a save that leaves a type error in `util` on disk, an unsaved edit of `util` that leaves it broken, and a save of `main`. The report is unclean only through the `util` buffer, so no run starts, and the saved text of `util` is still the broken one, so the mark stays (D-1504) and hover on a new constant there still says that the saved files have errors (D-1501).
- `saved_errors_make_values_stale`: a run that gives `util::base` its value, then a save that leaves a type error in `main` on disk, so no run starts. `util` has not changed, but hover shows its value as "`2` when last saved", as after a run the child skipped (D-1503).
- `zed_save_runs_once`: a save in Zed's order, a watched change, `didSave` and its echo, starts one run that takes some seconds. The same save sent again while that child runs leaves it running, so no second run begins (`quiet`), and the saved value reaches hover (D-1494).
- `being_evaluated`: a run gives `limit` its value, then a save adds a constant that never ends, so the run is still going when hover asks. `limit` says "`20` when last evaluated; the new save is being evaluated.", and the new constant says "Not evaluated yet: the constants are being evaluated." (D-1511).
- `reached_module_unsaved_is_stale`: after a run, an unsaved edit of `util`. `scaled` reaches it through a function, so it shows "`60` when last saved"; `plain` reaches only `main` and stays current (D-1510).
- `long_value_is_a_block`: a list of seven records, too long for the header line, shows in its own code block, one element per line, and a short list stays on the header line (D-1512). Its numbers show as the runtime prints them, `10000`, with no `_` groups (D-1523).
- `single_file_shows_no_value` and `evaluate_off_shows_no_value`: a constant in single-file mode and with `constants.evaluate` off shows no value line (D-1371, D-1491). The first also waits for `cheby/idle` of a single-file session, whose `root` is the file (D-1492).

A package constant's value line follows `features.md` §2.1's constant value rule as plan §1.8 amends it: a current value joins the header, `const limit: Int = 20`; a stale one adds "`20` when last saved"; a failed one names its code; one not evaluated yet says "Not evaluated yet: values are computed on save."; one with no value after a run that failed or was cut short says "Not evaluated: the last evaluation failed or was cut short (see `cheby/status`)." (D-1490); and one with no value after a run skipped because the saved files have front-end errors, or after a save that left such errors on disk so that the server started no run, says "Not evaluated yet: the saved files have errors; values are computed on save." (D-1497, D-1501). While a save has left such errors on disk and so started no run, every value is stale (D-1503), and `cheby/status` says `stale` (D-1505). A value is current only while each module file its initializer reaches, as the child reports it, has the text the run read (D-1510). While a run is due or running, a constant with no value says "Not evaluated yet: the constants are being evaluated.", and a stale value says "`v` when last evaluated; the new save is being evaluated." (D-1511). A value that would take the header past 80 columns is shown in its own block, broken by its brackets (D-1512). The rest of D-1504, when that mark is set and cleared, is checked in process by `child_tests.rs`, since no golden can hover before a step: a session opened over broken saved files reads as not yet, a fixing save keeps the mark until its run starts, and a run that ends clears it. A package that never evaluates, in single-file mode, on Windows or with `constants.evaluate` off, shows no line (D-1491). A dependency's constant shows its written initializer, as std's does (D-1491), but dependencies arrive in M5 (§13.4), so no golden here has one: `hover_tests.rs` checks the same path with a std constant that no index knows. An initializer is cut at 4,096 characters with `…`, as a value is (D-1496). No std constant is that long, so `hover_tests.rs` checks the cut on the header hover builds, and no golden names D-1496. `value_reaches_hover` names D-1414 for its side that runs: off Windows the child evaluates. `CHEBY_LSP_CHILD_FIXTURE` is read only in a debug build (S-24), and the harness runs the debug build, so no trial here can show that a release build ignores it.

### Editing features

The goldens under `completion/`, `signature/` and `inlay/` pin plan W8. Their answers are printed in a readable form, not as the raw JSON, and were written by hand.

- `completion/`: one test for each broken form of `features.md` §4.1, such as `call_open`, `case_open` and `let_no_value`, plus `clean` and `type_error`. `dot/`, `pipe/` and `auto_import/` cover `xs.`, `|>` and auto-import, on clean text and with type and grammar errors. `dot/multi_line_receiver` and `dot/dollar_in_receiver` pin plan §1.11's edit rule.
- `signature/`: `clean`, `type_error` and `grammar_error`. `clean` has markers for every kind of callee, nesting, a pipe's hole, a capture and `use`. `type_error` covers a wrong argument and wrong arity, and `grammar_error` recovery from calls that do not parse.
- `completion/dot/postfix_case` and `completion/guard_nested` pin D-1506: text on several lines is written relative to its first line and carries `insertTextMode: 2`. `completion/auto_import/imported_module` pins D-1507 and D-1508: an imported module's item is written through the import's binding with no import edit, at rank 30, labelled `list::reverse` and filtered by `reverse`. `completion/auto_import/before_path` pins D-1521, D-1525 and D-1530: before `::`, as at `st‸::length`, no item written through a module path is offered, only the module's name, written bare, and the interfaces, and a one-character partial leaves the list complete. `completion/before_path` pins D-1530 with D-173: before a path's `::`, an expression and `|>` offer the modules, an alias among them, and the interfaces, bare as `Show` or through their module as `ops::Compare`, and a type offers the modules only. Where two segments follow the `::`, as at `op‸::Compare::compare(a, b)`, only the modules are offered (D-1539). `completion/later_segment` pins D-1538, D-1539 and D-1548: after `io::` the module's interfaces `Source` and `Sink` come with its functions, written `Source::`, `io::So‸::read_bytes(src, 4)` offers only the interfaces, written `Source`, and `i‸::Source::read_bytes(src, 4)` only the modules. `completion/first_segment` pins D-1546: in an expression and after `|>`, the prelude's `Show` and the file's `Sized` are written `Show::` and `Sized::`, the imported `std::ops`'s `Compare` comes as `ops::Compare::` from two characters, and one character leaves the list incomplete. `completion/auto_import/interface` pins D-1546 with D-1479: an interface of a module the file does not import comes with its import edit, at rank 60 in an expression and 40 after `|>`. `completion/pipe/interface_function` pins D-1545: after `src |> io::Source::` and `1 |> ops::Compare::` the functions are written in the pipe form, `read_bytes(_, $1)$0`, `Show::show` is written bare, and nothing is offered before a turbofish. `completion/type_later_segment` pins D-1547: `json::Va‸::X` in a type and `json::In‸::X(n)` in a pattern offer nothing. `completion/before_turbofish_names` pins D-1544: before `::<`, a type offers no `dyn` and a pattern no `_`, and `js‸::Value::<Int>(1)` offers only the modules. `completion/dot/before_path` pins D-1530 and D-1537 after `xs.`, where nothing is offered before `::` or `::<`. `completion/auto_import/before_turbofish` pins D-1525, D-1529 and D-1536: before `::<`, `re‸::<Int>(xs)`, `xs |> re‸::<Int>` and `re‸::` with `<Int>(xs)` on the next line inside `(` offer `list::reverse` and no module name, and the first offers the local `xs` but no constant, keyword or snippet. `completion/pattern_before_path` pins D-1525 and D-1530 in a pattern: `js‸::Int(n)` offers only `json`, and `js‸` with no `::` offers `json::Int`. `completion/let_pattern_subject` pins D-1531: with a type annotation, the pattern of `let`, `let guard` and `let assert` ranks the value's constructors first. `completion/let_guard_value` pins it for the rest of a `let guard`: its value after `=` is an expression, ranked by the annotation and offering only modules and interfaces before a path's `::` (D-1530), and its annotation after `:` is a type. A marker word names an item written through a module path by its qualified label, `apply list::enumerate`.
- `inlay/`: one test for each row of `features.md` §7.1, such as `let_types`, `pipe_stages` and `parameter_names` (D-1515, D-1524), plus `type_error`, `settings`, `disabled` and `range_limit`.

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
- `apply LABEL` prints the item that `LABEL` names, resolved as above, and `text`: the file after its edits, one string per line. The main edit is the `textEdit`, or for an `InsertReplaceEdit` its `insert` range. A snippet is expanded as a client would expand it: `${1:f}` gives `f`, `${1|a,b|}` gives `a`, `$1` and `$0` give nothing, and `\$`, `\}` and `\\` give the character. Each later line of the main edit's text gets the indentation of the cursor's line, up to the cursor, as `insertTextMode` `adjustIndentation` and Zed do, unless the item says `asIs` (D-1506). Then `additionalTextEdits` apply to the same text. The result must parse with no syntax error, or the trial fails and names the errors. So a test of an item that completes its construct puts one broken construct in its file, and an item that does not complete one is tested with `completion LABEL`. A `LABEL` that names no item, or several, fails the trial. `cheby fmt`'s form is not required (plan W8, S-24).

Whatever the golden says, every item of every completion answer must have an edit on one line that holds the position (D-1394, and the protocol's rule for `textEdit`). For an `InsertReplaceEdit` both ranges must, and `insert` must be a prefix of `replace`. Its `additionalTextEdits` must not overlap the main edit.

**Signature help** prints the answer with each parameter's label as text: a label given as offsets prints as `"[start, end] text"`, the offsets in the negotiated encoding, and a label given as a string prints as it is. Offsets outside the signature's label fail the trial.

**Inlay hints** print as a list of lines, sorted by position: the compiler's 1-based line and code-point column, the kind (`type`, `parameter` or `-`), and the label in backticks. A label of parts prints each part in backticks, joined by `+`, with a part's location after it as `@path:L:C`. Then, where set, `padding-left`, `padding-right`, `tooltip` and each of `textEdits` as `edit L:C-L:C` with its text in backticks. So `` "3:8 type `: List<Int>` edit 3:8-3:8 `: List<Int>`" ``.

`CHEBY_BLESS=1 cargo xtask test -E 'binary_id(cheby_cli::lsp)'` writes the goldens, and so does `cargo xtask bless`. Review a blessed change like code (D-292). A test of a later milestone is never blessed, and trials without a golden are ignored under blessing. A missing golden fails with the whole answer in the message, so it can also be written by hand.

## Milestones and platforms

Protocol trials and golden tests use the header default, M1, so they fail CI from the day they land while `tests/milestone` names a later milestone (D-1352). Example trials use the example's milestone, and a `skip-run` example of a later milestone, such as 030, is tolerated. UI parity trials use the UI test's milestone. The strict runs of D-544 apply:

```sh
CHEBY_PENDING=require cargo xtask test -E 'binary_id(cheby_cli::lsp)'
```

On Windows, until M6, the `parity/examples/*` trials are ignored, since they run `cheby check`, which builds a JIT for a package that passes the front end, and so are the UI parity trials whose golden has no front-end error, as their check tests are (D-545, D-641). The server evaluates no constant there (D-1414), so a golden test whose `@steps` hold `idle` or `started` is ignored too. The server never runs generated code (D-1349), so every other trial runs there.

Every `cheby` gets the harness's fresh `CHEBY_CACHE` (D-959). `CHEBY_HARNESS_SCALE` multiplies the timeout of each wait (D-722). `cargo xtask coverage` reads the `@decisions` of the tests here.
