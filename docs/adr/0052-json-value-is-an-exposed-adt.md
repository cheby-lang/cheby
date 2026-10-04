---
status: accepted
date: 2026-10-04
log: D-321
---

# `json::Value` is an exposed ADT

`json::Value` is an `exposed` type with the variants `Null`, `Bool`, `Int`, `Float`, `String`, `Array` and `Object`, so any program can build a value with its constructors and take one apart with `case`. Decoders (D-320) stay the main way to read typed data, but generic tools such as pretty printers, diffs and path lookups no longer need them. Exposed types are frozen after 1.0 (D-075), and that costs nothing here, because the JSON specification fixes the set of shapes. This follows D-304's rule for error types whose cases an outside specification fixes.

## Considered options

- An opaque value with builder functions (`json::int`, `json::object`) and a `decode::value()` decoder, as in Gleam and as example 011 first assumed: the representation could change later, but walking an unknown document needs decoders, and there would be two ways to build every value.

## Consequences

- The builder functions disappear, and examples 011 and 030 use the constructors, such as `json::Int(1965)` and `json::Object([...])`.
- `json::Int` can hold an integer outside the JS safe-integer range. It is written as is and reads back as a `Float` (D-222).
- `json::Object` is a list of pairs in written order (D-218), so equal documents with keys in a different order are not `==`.
