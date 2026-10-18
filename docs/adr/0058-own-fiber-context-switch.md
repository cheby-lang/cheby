---
status: accepted
date: 2026-10-11
log: D-676
---

# Our own fiber context switch

Fibers switch with our own `naked_asm!` switch, one per platform: x86-64 SysV, including Mach-O, and AAPCS64 on Linux and Apple. Win64 and Windows aarch64 follow in M6. This settles OQ-P7.

The switch is called only from Rust, so it saves exactly the platform's C callee-saved registers. It is used asymmetrically: a fiber always switches to its worker's own stack, and the worker switches to the next fiber, as Go's `g0` does. The switch carries a message word with a post-switch action, such as releasing a lock, which the worker runs after the fiber's registers are saved. This is Go's `gopark(unlockf)`. A new fiber's first frame has a null return address and frame pointer, which ends every backtrace.

## Considered options

- corosensei: it lacks Windows aarch64, which D-277's matrix needs in M6. It assumes a guard page per stack, which D-124 rejects. It forces a Rust unwinding through a dropped coroutine's stack, which aborts at Cheby's `extern "C"` frames. Its coroutines are `!Send`, and it has no hooks for TSan.
- wasmtime-fiber: it allocates its own stacks, so it cannot use the shared stack regions of ADR-0032, and it uses Win32 fibers on Windows.
- Cranelift's `stack_switch`: it exists only on x64.

## Consequences

- About 40 lines of assembly per architecture, plus `.cfi_*` directives that link a fiber's stack to its worker's for debuggers.
- The switch module also holds the TSan and ASan fiber annotations.
- Floating-point control state, such as MXCSR, is not saved, because Cheby never changes it.
- Until M6, Windows has no switch, and a program there runs without the fiber scheduler (D-692).
- If an assembly bug blocks the M2 slice, corosensei behind the same API is the fallback, on Linux and macOS only.
