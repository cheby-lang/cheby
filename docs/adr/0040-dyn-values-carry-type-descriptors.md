---
status: accepted
date: 2026-09-29
log: D-236
---

# `dyn` values carry type descriptors

Two `dyn I` values are equal when their underlying values have the same type and are structurally equal (D-033). Generic code is compiled once over a uniform representation (ADR-0011), so type arguments are not kept at run time: `[]: List<Int>` and `[]: List<String>` converted to `dyn Show` look the same, and so do `None: Option<Int>` and `None: Option<String>`. The packaged functions do not tell them apart either, because for generic types they are built at run time (D-064). Hashing has the same problem, so `dyn` values could not be `Map` keys or `Set` elements.

We give every `dyn` value a runtime type descriptor of its underlying value's full type, including all type arguments, next to its packaged functions. Descriptors identify nominal types exactly: two declarations with the same shape, or the same name in different modules or packages, have different descriptors, an alias has the descriptor of the aliased type, and a type redefined in the REPL gets a new descriptor (D-043). Two `dyn` values are equal when their descriptors are equal and their values are structurally equal, and hashing a `dyn` value hashes its descriptor together with its value. This keeps the same-type rule exactly, as Go does for interface values.

A concrete type's descriptor is supplied statically at the conversion. When the converted type mentions a type parameter `T` of the enclosing top-level function, `T`'s descriptor comes from one of its bounds: every function package passed for a bound (D-046) carries the descriptor of the type it was built for. Converting to `dyn` a value whose type mentions a type parameter without a bound is a compile error, and the diagnostic suggests adding one. An empty interface (D-166) always works as that bound, because every type satisfies it, so adding it never breaks a caller. The requirement is visible in the signature (D-011) and checked locally.

This refines ADR-0011 without superseding it: generic code is still compiled once over the uniform representation, and descriptors are the only runtime type information.

## Considered options

- Make `==` and hashing panic on `dyn` values: simple, but `List<dyn Shape>` could never be compared or used as a key.
- Compare only the type name and ignore type arguments: `[]: List<Int>` would equal `[]: List<String>`, which breaks the same-type rule and could never be changed after 1.0.
- Pass a descriptor for every type parameter: no bound needed, but every generic call pays, even in code that never uses `dyn`.
- Let the compiler infer which type parameters need a descriptor: no bound needed, but non-local, since a function's calling convention and cost would depend on the bodies of its callees, and it complicates REPL redefinition (D-043).

## Consequences

- `dyn` values can be compared with `==` and used as `Map` keys and `Set` elements. The D-233 rule still applies inside: if the comparison or hash reaches a function value or a handle, it panics.
- Different types with the same printed form are never equal, so `[]: List<Int>` as `dyn Show` is not equal to `[]: List<String>` as `dyn Show`.
- Generic code that converts to `dyn` needs a bound on the type parameters involved, for example `fn wrap<T: Any>(xs: List<T>) -> dyn Any` with an empty `interface Any`.
- Descriptors of generic types such as `List<T>` are built at run time from their arguments' descriptors. Implementations may cache or intern them.
- Only `dyn` values and function packages for bounds carry descriptors. Generic code without bounds, and code that never converts to `dyn`, pays nothing.
- Descriptors are observable only through `==` and hashing. There is still no conversion back from `dyn`, no downcasting and no reflection (D-168), and debug printing of a `dyn` value still prints the value inside it (D-233). Adding such features later stays additive.
- Relaxing the bound requirement later, to either rejected source of descriptors, breaks no program.
