// Foreign helper for random.cheby on the JS target.
//
// `Math.random` is a global, not the export of an ES module, so this
// module re-exports it under a name that `@external` can refer to.

export function random() {
  return Math.random();
}
