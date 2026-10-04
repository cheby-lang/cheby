---
status: accepted
date: 2026-10-04
log: D-302
---

# Strings count code points

`std::string` measures and pads strings in code points, the Unicode scalar values a `String` is made of (D-038, §3.5). Grapheme clusters, what a reader sees as one character, are available only through separate functions such as `string::to_graphemes`. So `string::length("🇳🇱")` is 2 and the family emoji `"👨‍👩‍👧"` has length 5, while `string::grapheme_length` gives 1 for both. Code-point counts need no Unicode tables, are the same on every target (D-232) and never change when Unicode is updated, so they fit the compatibility promise (D-075).

## Considered options

- Grapheme clusters by default, as in Gleam and Swift: closer to what users see, but every program pays for the segmentation tables on both targets (the JS host's `Intl.Segmenter` differs between hosts), counting is slower, and each Unicode update changes the result of `length` for existing programs.
- Bytes, as in Rust's `str::len`: depends on the encoding, which differs between native (UTF-8) and JS (UTF-16) (D-038).

## Consequences

- Padding text with combining marks or emoji gives the wrong visual width. Neither option gives terminal column width, since a CJK character is one code point and one grapheme but two columns wide.
- `string::length` is O(n) on both targets: UTF-8 on native and UTF-16 on JS both need a scan.
- Grapheme functions need Unicode segmentation tables shipped with the runtime on both targets.
