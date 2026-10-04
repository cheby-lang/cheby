# 9. Memory model

All Cheby values are immutable (D-004). Memory is managed by precise reference counting in the style of Perceus, not by a tracing garbage collector (D-005, ADR-0003). This chapter defines what programs can observe about memory management, and what implementations must guarantee.

## 9.1 Values and sharing

A value is never modified after it is created, so sharing a value is never observable. Two bindings that refer to the same value behave exactly as if each had its own copy. Persistent data structures (`List`, `Map`, `Set`) share structure between versions (D-004, D-039).

Because data is immutable, there is no way to build a reference cycle out of ordinary values: a value can only refer to values that existed before it. Cycles can only arise through runtime objects ([§9.6](#96-reference-cycles)).

Identity is not observable. There is no pointer equality, and `==` is always structural ([§3.13](03-types.md#313-equality-hashing-and-debug-printing)). For this reason functions and handles cannot be compared or hashed, and they debug-print as placeholders (D-233).

## 9.2 Reference counting

On native targets, every heap value carries a reference count (D-005):

- The compiler inserts increments and decrements precisely, so that a value is freed as soon as the last reference to it is gone, not at the end of a scope.
- Borrowing is inferred by the compiler, which removes increment/decrement pairs where it can prove they are unnecessary. There are no user annotations for ownership or borrowing (D-072). Borrowing is inferred only for calls within one module. Calls across modules use a fixed owned convention, so editing a function body never changes how other modules call it ([§13.12.3](13-tooling.md#13123-interface-artifacts-and-recompilation)) (D-250).
- Small integers and other immediate values are not heap-allocated and are not counted (D-021).

What is heap-allocated, and when counts change, is an implementation detail. The only observable guarantees are:

1. Memory held only by unreachable values is released promptly, without pauses for collection.
2. Handle drop functions run deterministically when the last reference goes away ([§9.5](#95-handles-and-drop-functions)).
3. A binding stops holding its value right after its last use, on every target (D-205, ADR-0036). A binding that one branch of a `case` uses and another does not is released at the start of the branch that does not use it. It is never held until the end of its scope. The same applies to parameters, pattern bindings, `use` binders and values captured by closures, which are released when the closure itself is.

Guarantee 3 is what makes channel closing predictable ([§10.5.3](10-concurrency.md#1053-closing)): a fiber that passes its last use of a `Sender` closes the channel at that point, even if it keeps running. An implementation must not extend a handle's lifetime past its last use, even to save a reference-count operation. A program that needs a handle kept alive must use it again later.

_Example:_ `rx` is dropped at the start of the `0` branch, which closes the channel before `found` is returned.

```cheby
fn take(rx: Receiver<Int>, remaining: Int, found: List<Int>) -> List<Int> {
  case remaining {
    0 => found   // rx is released here
    _ => {
      let assert Ok(n) = channel::receive(rx)
      take(rx, remaining - 1, [..found, n])
    }
  }
}
```

## 9.3 Reuse

When the compiler can prove that a value is uniquely referenced at the point where a new value of the same size is built from it, it may reuse the old memory in place ("functional but in-place") (D-005, D-017). Typical cases:

- a record update `T { ..base, field: v }` where `base` is not used afterwards,
- inserting into a `List`, `Map` or `Set` that is not used afterwards,
- appending to a `Bytes` value that is not used afterwards (D-316),
- pattern-matching a value and constructing a new value of the same shape in the same arm.

Reuse is an optimization. It never changes a program's result, because the old value is unreachable when it is reused. Implementations should reuse wherever they can, because Cheby has no mutable escape hatch and relies on reuse for performance (D-017, ADR-0007).

_Example:_ when `counts` is not used afterwards, this updates the map in place.

```cheby
let updated = map::insert(counts, word, n + 1)
```

Implementations provide a compiler flag that reports where reuse did or did not happen, for performance tuning (D-170). The flag and its output are tooling, outside this specification.

## 9.4 Sharing across threads

Fibers run in parallel on several OS threads (D-012). To keep reference counting cheap, a value is either **local** to one fiber or **shared** (D-072, ADR-0024):

- A value becomes shared, together with everything it reaches, when it is sent on a channel, captured by a spawned fiber, or stored in a global such as a constant.
- Local values use plain, non-atomic count operations. Shared values use atomic ones.
- A shared value is never reused in place, since other threads may still see it ([§9.3](#93-reuse)).

Marking is automatic and invisible to programs, except for its cost: marking a large structure as shared traverses it once (ADR-0024).

## 9.5 Handles and drop functions

Some runtime and FFI types are **handles**: channel `Sender` and `Receiver` values ([chapter 10](10-concurrency.md)), files, sockets, and external types declared with a drop function ([§12.5.3](12-targets-and-ffi.md#1253-external-types)) (D-101).

- A handle type may have a **drop function**. It runs exactly once, when the last reference to the handle goes away (D-101).
- Drop functions run deterministically on every target, including JS ([§9.8](#98-memory-on-the-js-target)) (D-100, ADR-0028).
- Drop functions also run when a fiber unwinds because of a panic or cancellation ([§11.3](11-errors-and-panics.md#113-unwinding)).
- User-defined ADTs never have drop functions (D-101). A user type that contains a handle releases it when the user value itself is released.
- Handles have no structural equality or hash: `==` and hashing panic when they reach one, and a handle debug-prints as a placeholder such as `<Sender>` ([§3.13](03-types.md#313-equality-hashing-and-debug-printing)) (D-233).

- A drop function runs on the fiber that released the last reference to the handle (D-171).
- A panic inside a drop function aborts the whole process, because there is no sensible place to report it and continuing would silently leak the resource (D-171).

Channels close when their last `Sender` or last `Receiver` is dropped ([§10.5](10-concurrency.md#105-channels)).

## 9.6 Reference cycles

Mutable runtime objects, such as channels and fibers, can form reference cycles, for example a channel whose buffered message contains a `Sender` for that same channel. Reference counting cannot free such cycles (D-029, ADR-0014).

- In v1, memory reachable only through such cycles leaks. Implementations must document this (D-029).
- In debug builds, the runtime should report leaked runtime objects at program exit (D-029).
- A cycle collector limited to runtime objects may be added later without changing the language (D-029).

Structured concurrency ([§10.3](10-concurrency.md#103-scopes)) prevents the most common source of leaks, fibers that are never joined (D-040).

## 9.7 Constants

The values of module constants are computed at compile time ([§4.5](04-declarations.md#45-constants)) and stored in the program image (D-090). They are shared by all fibers, are never reference counted, and are never freed. Reference count operations on a constant's value are no-ops.

## 9.8 Memory on the JS target

On the JS target, ordinary values are managed by the JS engine's garbage collector, and no reference counts are kept for them (D-100). This is not observable, because values are immutable and identity is not observable ([§9.1](#91-values-and-sharing)).

Handles are the exception. For handle types, and for any value that may contain a handle, the JS backend emits the same increments and decrements the native backend would, so drop functions and channel closing happen at exactly the same points as on native (D-100, D-101, ADR-0028).

A handle may be stored inside a generic container whose element type is not known statically. To find such handles, values that may contain handles carry a runtime flag in the uniform representation, and the JS runtime traverses flagged values when releasing them (D-172). Values that contain only pure data are not flagged and pay nothing.
