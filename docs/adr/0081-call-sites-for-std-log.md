---
status: accepted
date: 2026-11-17
log: D-1628
---

# Call sites for `std::log`

A log record says where it was logged: the path, line and column of the log call, the three parts of a panic report's location. The compiler passes them. A function of `std` that carries `@std_call_site(constructor)` is never called itself: the compiler calls its constructor with the call's arguments and the place of the call.

```cheby
@std_call_site(info_at)
pub fn info(message: String) {
  let _ = message
  panic as "the compiler did not replace this call of `log::info` (D-1630)"
}

fn info_at(message: String, path: String, line: Int, column: Int) {
  emit(Info, Location { path, line, column }, fn() { (message, []) })
}
```

A function cannot see its caller, so a location has to be passed in, and only the compiler knows it. `@std_initializer` already replaces one call by a constructor's and adds arguments that the compiler computes (D-1599). This attribute is the same shape with a location.

Three choices shape it.

**Only package `std` may write it.** The sixteen log functions are a case with none of the hard questions. None takes a type of its module as its first parameter, so none can satisfy an interface or sit in a dictionary, which has no place to carry. None is called through another function that wants the caller's caller. The location arrives as three ordinary arguments, so no location type is needed in the language. A user-facing attribute would have to answer all of these, and it stays open (D-1632).

**The call site is where the function is named.** In `message |> log::info`, and in a pipeline of several stages, the record names the line of `log::info`. A call's location in a deadlock report is the start of the call expression, which for a pipe is the start of the chain (D-856). That rule suits a report of where a fiber waits. A log line should point at the log statement (D-1629).

**A function named as a value keeps working.** `list::each(lines, log::info)` becomes a function that calls the constructor with the place of that mention. Every record logged through it carries that one place.

## Considered options

- No location in the first version. It needs no compiler change, but `Record` is an exposed type, so adding a field later breaks every program that builds or matches one.
- A user-facing `@track_caller` first, with `std::log` as its first user. It is the general answer, and a large language change with its own design questions, before 1.0 and before any package asks for it.
- An error for a log function named as a value, as E0080 is for `context::key`. It is less compiler work, but `list::each(lines, log::info)` is ordinary Cheby, and a new diagnostic would exist only to forbid it.
- `pub` constructors, so that a wrapper could pass a location on. User code has no location to pass, so they would be sixteen public functions with no honest use.

## Consequences

- A function of a package that wraps a log call reports the place inside the wrapper, until the language has a way to pass a caller's location.
- A backtrace and a deadlock report name the constructor's frame, as in `std::log::info_at`, a name no program writes.
- The compiler never infers a borrowed parameter for a function that such an attribute names. A package calls the constructor without seeing its body, so the two sides must agree on who owns the arguments (D-1630).
- The location is produced in MIR lowering, where a panic's is. The type checker has no source map, and a REPL entry's spans move after it is checked (D-1631).
- The panics that std raises keep `<unknown location>` (D-732). The mechanism could serve them, and nothing here decides that it will.
