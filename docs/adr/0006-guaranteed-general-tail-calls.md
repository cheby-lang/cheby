---
status: accepted
date: 2026-09-28
log: D-015
---

# Guaranteed general tail calls

Recursion is the only way to loop, so every tail call is guaranteed not to grow the stack. This includes mutual and indirect calls, not just self-recursion. On native, this uses Cranelift's `tail` calling convention and `return_call`. On JS, where only Safari implements proper tail calls, self-recursion compiles to loops and all other tail calls go through trampolines, which costs some performance on those paths.
