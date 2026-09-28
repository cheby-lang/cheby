---
status: accepted
date: 2026-09-29
log: D-210
---

# JSON in the standard library

ADR-0022 left JSON encoding to "libraries or external code generators". We ship `std::json` in the bundled standard library (D-058), with a JSON value type, encoding functions and decoder values, but still no derive. JSON is needed by almost every program, and a standard module lets the toolchain generate encoding and decoding code for a type without a plugin system, as Go does with `encoding/json`.

## Considered options

- A third-party package only: keeps the standard library small, but the toolchain could not generate code against an API it does not own, and every program would pick among competing packages.

## Consequences

- `std::json` falls under the compatibility promise (D-075), so its API must be designed carefully before 1.0.
