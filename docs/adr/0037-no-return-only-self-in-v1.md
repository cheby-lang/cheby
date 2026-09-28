---
status: accepted
date: 2026-09-29
log: D-209
---

# No interface functions with `Self` only in the return type in v1

Interfaces such as `FromJson { fn from_json(Value) -> Result<Self, E> }` or `Default { fn default() -> Self }` would need the caller to name the type, as in `FromJson::from_json::<User>(v)`, because no argument carries it. That is type-directed resolution, which D-173 deliberately avoids. In v1 every interface function must still mention `Self` in a parameter. Decoding is done with ordinary decoder values instead, such as `decode::Decoder<User>`, which compose with `use`. Allowing return-only `Self` later would break no code.
