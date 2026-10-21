# 9. Memory model

All Cheby values are immutable (D-004). Memory is managed by precise reference counting in the style of Perceus, not by a tracing garbage collector (D-005, ADR-0003). This chapter defines what programs can observe about memory management, and what implementations must guarantee.

## 9.1 Values and sharing

A value is never modified after it is created, so sharing a value is never observable. Two bindings that refer to the same value behave exactly as if each had its own copy. Persistent data structures (`List`, `Map`, `Set`) share structure between versions (D-004, D-039).

Because data is immutable, there is no way to build a reference cycle out of ordinary values: a value can only refer to values that existed before it. Cycles can only arise through runtime objects ([§9.6](#96-reference-cycles)).

Identity is not observable. There is no pointer equality, and `==` is always structural ([§3.13](03-types.md#313-equality-hashing-and-debug-printing)). For this reason functions and handles cannot be compared or hashed, and they debug-print as placeholders (D-233).

## 9.2 Reference counting

On native targets, every heap value carries a reference count (D-005):

- The compiler inserts increments and decrements precisely, so that a value is freed as soon as the last reference to it is gone, not at the end of a scope.
- Borrowing is inferred by the compiler, which removes increment/decrement pairs where it can prove they are unnecessary. There are no user annotations for ownership or borrowing (D-072). Borrowing is inferred over the units of a build that [§13.12.2](13-tooling.md#13122-units-of-compilation) names (D-779, D-968). A `pub` function, and so every call across a package boundary, uses a fixed owned convention, so editing a function body never changes how other packages call it (D-250, D-812). Inside a package, a package-visible function may be borrowed (D-787). Editing its body can then change how the package's other modules call it, and those modules are recompiled with it ([§13.12.3](13-tooling.md#13123-interface-artifacts-and-recompilation)) (D-812).
- Small integers and other immediate values are not heap-allocated and are not counted (D-021).

What is heap-allocated, and when counts change, is an implementation detail. The only observable guarantees are:

1. Memory held only by unreachable values is released promptly, without pauses for collection.
2. Handle drop functions run deterministically when the last reference goes away ([§9.5](#95-handles-and-drop-functions)).
3. A binding stops holding its value right after its last use, on every target (D-205, ADR-0036). A binding that one branch of a `case` uses and another does not is released at the start of the branch that does not use it. It is never held until the end of its scope. The same applies to parameters, pattern bindings, `use` binders and values captured by closures, which are released when the closure itself is.

Guarantee 3 covers the elements of a list in two cases. When a list that a pattern matches dies at the match, because it is the subject at its last use or is held only by such a value, the list that a spread binds, such as the `rest` of `[first, ..rest]` or of `[..rest, _, last]`, does not hold the elements the pattern cuts that no other list holds (D-893, D-908). A cut element that the pattern binds to a name is released at that name's last use, and any other at the match. The same holds for `list::slice`, `list::take` and `list::drop` when the call is the list's last use (D-894). An element that another list shares is not released by the cut, and lives until the last list holding it dies, which may be the slice itself. A spread of a list that is used again, and a slice made by one of those functions from a list that is used again, may keep the elements they cut alive for as long as the slice lives (D-893, D-894).

Guarantee 3 is what makes channel closing predictable ([§10.5.3](10-concurrency.md#1053-closing)): a fiber that passes its last use of a `Sender` closes the channel at that point, even if it keeps running. An implementation must not extend a handle's lifetime past its last use, even to save a reference-count operation, except for an element that a list slice keeps, as the previous paragraph allows (D-893, D-894). A program that needs a handle kept alive must use it again later. `let _ = x`, where `x` names a binding, is such a use: the binding is released at that statement if it is its last use (D-736).

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

A kernel, a standard-library function implemented in the runtime such as `list::push`, `map::insert` or `bytes::append`, may update an argument in place when nothing else refers to it at run time, and builds a new value otherwise (D-525).

_Example:_ when `counts` is not used afterwards, this updates the map in place.

```cheby
let updated = map::insert(counts, word, n + 1)
```

A shared value ([§9.4](#94-sharing-across-threads)) is never reused (D-716). Reuse, and the optimizations that go with it, such as fusing count operations and releasing a value whose constructor is known while keeping the fields still in use, must not move the release of a value that may hold a handle past the point that guarantee 3 of [§9.2](#92-reference-counting) fixes (D-205, D-716).

_Note:_ the reference implementation reuses the memory of a released value only for a new value of the same size, and only when a read of the old value's fields pins its constructor. A field that every variant of a type with several variants has, read with `x.name` ([§5.11.3](05-expressions.md#5113-field-access)) (D-071), pins no constructor, so it proves nothing about the layout. A static object, such as a constant's value, is never reused (D-838, D-839). A parameter whose memory would be reused is kept owned, never borrowed (D-841). Under Perceus counting it also cancels an increment that meets a decrement of the same value, releases a value whose constructor is known while keeping the fields still in use, and does not count a constant's value at all (D-850). Under every counting mode, it reads a record's remaining fields before a call that consumes one of its fields, when the record's type cannot hold a handle, so the record is released there and the field can be updated in place (D-911). Debug builds check that between a counted value's last use and each release of it, only count operations run, for every counted value, not only those that may hold a handle (D-851).

Implementations provide a compiler flag that reports where reuse did or did not happen, for performance tuning (D-170). The flag and its output are tooling, outside this specification. The reference toolchain's flag is `--reuse-report` ([§13.1](13-tooling.md#131-the-cheby-command)) (D-717).

## 9.4 Sharing across threads

Fibers run in parallel on several OS threads (D-012). To keep reference counting cheap, a value is either **local** to one fiber or **shared** (D-072, ADR-0024):

- A value becomes shared, together with everything it reaches, when it is sent on a channel, captured by a spawned fiber, returned by a fiber, whose result other fibers read with `join`, or stored in a global such as a constant (D-674). The body of a scope and the function of a timeout run on the calling fiber, so what they capture and return is not marked ([§10.3](10-concurrency.md#103-scopes)) (D-665, D-674).
- Local values use plain, non-atomic count operations. Shared values use atomic ones.
- A shared value is never reused in place, since other threads may still see it ([§9.3](#93-reuse)).

Marking is automatic and invisible to programs, except for its cost: marking a large structure as shared traverses it once (ADR-0024).

_Note:_ the reference implementation also marks the values bound at the REPL prompt, which detached fibers of later entries may reach ([§13.5.4](13-tooling.md#1354-fibers-in-the-repl)). A shared value stays shared, even when only one fiber can reach it again (D-705).

## 9.5 Handles and drop functions

Some runtime and FFI types are **handles**: channel `Sender` and `Receiver` values, `Fiber`, `Scope` and `Selector` values ([chapter 10](10-concurrency.md)), files (`file::File`), sockets (`net::Listener` and `net::Connection`), and external types declared with a drop function ([§12.5.3](12-targets-and-ffi.md#1253-external-types)) (D-101). The handles of the concurrency modules are external types of `std::channel`, `std::fiber` and `std::selector`, with drop functions (D-647). The handles of files and sockets are external types of `std::file` and `std::net` in the same way: a `File`'s drop function closes the file, and a socket's closes the socket without waiting (D-982).

- A handle type may have a **drop function**. It runs exactly once, when the last reference to the handle goes away (D-101).
- Drop functions run deterministically on every target, including JS ([§9.8](#98-memory-on-the-js-target)) (D-100, ADR-0028).
- Drop functions also run when a fiber unwinds because of a panic or cancellation ([§11.3](11-errors-and-panics.md#113-unwinding)).
- User-defined ADTs never have drop functions (D-101). A user type that contains a handle releases it when the user value itself is released.
- Handles have no structural equality or hash: `==` and hashing panic when they reach one, and a handle debug-prints as a placeholder such as `<Sender>` ([§3.13](03-types.md#313-equality-hashing-and-debug-printing)) (D-233).

- A drop function runs on the fiber that released the last reference to the handle (D-171).
- A panic inside a drop function aborts the whole process, because there is no sensible place to report it and continuing would silently leak the resource (D-171). For the same reason, a `cheby.h` function that cannot allocate while a drop function runs ends the process with the fatal runtime error `out of memory in a drop function`, instead of the `out of memory` panic it raises elsewhere ([§11.8.2](11-errors-and-panics.md#1182-out-of-memory), [§12.5.1](12-targets-and-ffi.md#1251-native-abi)) (D-946).

Channels close when their last `Sender` or last `Receiver` is dropped ([§10.5](10-concurrency.md#105-channels)).

The native drop function of a user external type is a C function `void f(void *handle)` ([§12.5.3](12-targets-and-ffi.md#1253-external-types)). Its contract is weaker than that of the standard library's drop functions, because C code cannot be checked (D-949):

- It is called once, on the fiber that released the last reference, while that fiber runs or unwinds, and never with a NULL handle.
- It may release Cheby values with `cheby_ffi_drop`, build values with `cheby.h`, and write with `cheby_ffi_write_stdout` and `cheby_ffi_write_stderr` ([§12.5.1](12-targets-and-ffi.md#1251-native-abi)).
- It should return quickly, like every foreign call without `@blocking` ([§12.7](12-targets-and-ffi.md#127-blocking-foreign-calls)). If it blocks, it blocks its thread. A drop function cannot be `@blocking`.
- It is not called for the handles that detached fibers still hold when the program ends ([§10.10](10-concurrency.md#1010-program-entry-and-exit)) (D-710).

_Note:_ the drop functions of the standard library's handles never block, suspend, panic or run Cheby code ([§10.5](10-concurrency.md#105-channels)) (D-672, D-703). The reference implementation runs them from M2, and the drop functions of user external types from M3 (D-703, D-949). While a drop function runs, a `cheby_ffi_drop` that releases another handle queues its release behind the running one instead of starting a nested release, so a chain of handles that each hold the next does not grow the stack (D-949).

## 9.6 Reference cycles

Mutable runtime objects, such as channels and fibers, can form reference cycles, for example a channel whose buffered message contains a `Sender` for that same channel. Reference counting cannot free such cycles (D-029, ADR-0014).

- In v1, memory reachable only through such cycles leaks. Implementations must document this (D-029).
- In debug builds, the runtime should report leaked runtime objects at program exit (D-029). _Note:_ debug builds of the reference implementation follow the leak check's line ([§10.10](10-concurrency.md#1010-program-entry-and-exit)) with one more line that names the kinds of the leaked runtime objects, such as `<Sender>` and `<Fiber>` (D-711). The line is printed only when a runtime object leaked, and names the counts in the order `<Sender>`, `<Receiver>`, `<Fiber>`, `<Scope>`, `<Selector>`, followed by the count of the other leaked objects if there are any, as in `note: leaked 1 <Receiver>, and 1 object of other kinds` (D-784).
- A cycle collector limited to runtime objects may be added later without changing the language (D-029).

Structured concurrency ([§10.3](10-concurrency.md#103-scopes)) prevents the most common source of leaks, fibers that are never joined (D-040).

## 9.7 Constants

The values of module constants are computed at compile time ([§4.5](04-declarations.md#45-constants)) and stored in the program image (D-090). They are shared by all fibers, are never reference counted, and are never freed. Reference count operations on a constant's value are no-ops.

_Note:_ the reference implementation stores each constant's result in the build cache as an image that describes values, not memory: no layout number, address or hash seed. Before `main` starts, every run installs the constants it reads from their images, fresh results included, as static objects with count 0 that leave the leak check, and executables install them the same way. A value that two constants share stays one value (D-539, D-979).

## 9.8 Memory on the JS target

On the JS target, ordinary values are managed by the JS engine's garbage collector, and no reference counts are kept for them (D-100). This is not observable, because values are immutable and identity is not observable ([§9.1](#91-values-and-sharing)).

Handles are the exception. For handle types, and for any value that may contain a handle, the JS backend emits the same increments and decrements the native backend would, so drop functions and channel closing happen at exactly the same points as on native (D-100, D-101, ADR-0028).

A handle may be stored inside a generic container whose element type is not known statically. To find such handles, values that may contain handles carry a runtime flag in the uniform representation, and the JS runtime traverses flagged values when releasing them (D-172). Values that contain only pure data are not flagged and pay nothing.
