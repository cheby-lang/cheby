---
status: accepted
date: 2026-10-04
log: D-361
---

# JSON decoders do not name `json::Value`

`std::json` imports `std::json::decode`, because `json::Error` holds `decode::Error` and `json::parse` takes a `decode::Decoder`. Imports may not form cycles (D-145) and Cheby has no re-exports, so `decode` cannot name `json::Value`. Decoders therefore run over a raw tree in an internal standard-library module that both modules import, and `json::decode_value(value, decoder)` decodes an already-built `Value`. This keeps every name of D-320, D-321 and examples 011 and 030.

## Considered options

- Moving the decoders into `std::json` (`json::Decoder`, `json::field`, …): no cycle, and decoders could return raw values, but D-320's names change and examples 011 and 030 are rewritten.
- Moving `Value` into its own submodule: decoders could name it, but its constructors lose the `json::` prefix (D-321).

## Consequences

- No decoder can keep part of a document as a raw `json::Value`, so there is no `decode::value()` and no `decode::run`.
- `json::decode_value` converts the `Value` back to the raw tree, which costs O(n) on top of decoding.
