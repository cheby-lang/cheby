---
status: accepted
date: 2026-10-24
log: D-1160
---

# Comments keep their place

`cheby fmt` keeps every comment in its place between the same two tokens. A comment on the line of the token before it stays at the end of that line. A comment on a line of its own stays on a line of its own before the token after it. Since a `//` comment ends its line, the line breaks after it, and the parts of the node after the break go one level deeper. The rule holds between the parts of every node, from statements and `case` arms to binary chains, pipelines, calls, patterns, types and item headers.

There is one exception. A comment between a list element and its comma is printed after the comma, so no line starts with a comma.

```cheby
fn backoff() -> List<Int> {
  [
    100, // the first retry waits briefly
    400,
    1600,
  ]
}

fn ready(queued: Int, limit: Int) -> Bool {
  queued > 0 // nothing to send yet
    && queued <= limit
}
```

Every comment ends its line (D-078), so placing one adds a line break, and §2.8 allows a break only at continuation points. The source already broke the line in the same gap between the same two tokens, and continuation depends only on those tokens and their brackets (D-107, D-231). Keeping the comment in its gap is therefore always legal. The tree builder puts every comment between two parts of a node, never at its edge, so the rule covers every comment.

The comma is the list's own separator, and the style already decides whether it is there (D-1152). Keeping a comment before it would start the next line with a comma, a form the formatter otherwise never produces.

## Considered options

- The comment's exact place in every case, so the next line starts with `, b`. There is no exception to the rule, but canonical output gains a leading-comma line that nothing else produces.
- Keeping a node that holds a comment verbatim, as waves 1 and 2 did. It is simple, but the output is not canonical (D-079).

## Consequences

- `cheby fmt` has no options (D-079), so this rule is the canonical place of every comment. Changing it later reformats user code.
- A comment between a list element and its comma is the one place where a comment moves past a token.
- A comment forces a line break, so a node that holds one never fits on one line. In a binary chain the other operators then break too, and a pipeline breaks one stage per line.
- A corpus test checks the rule with 79,905 comments injected into 130 example and std files and 815 UI sources.
