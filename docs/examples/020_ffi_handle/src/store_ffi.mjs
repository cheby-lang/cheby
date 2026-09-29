// Foreign helpers for store.cheby on the JS target.
//
// A store is a plain JS object. To Cheby it is a value of the external
// type `Store`, which it passes back to these functions unchanged and
// never looks inside. The Cheby runtime counts references to it like
// any other handle, and calls `close` exactly once, when the last one
// goes away. No `FinalizationRegistry` or other garbage-collector hook
// is involved, so `close` runs at the same point as on native.

export function open(name) {
  return { name, entries: [] };
}

// Returns nothing, which Cheby sees as `Nil`.
export function write(store, entry) {
  store.entries.push(entry);
}

export function contents(store) {
  return store.entries.join("; ");
}

// A JS number, which is how an `Int` is represented on JS.
export function count(store) {
  return store.entries.length;
}

// The drop function of `Store`. It must not throw: a failure in a drop
// function aborts the whole program.
export function close(store) {
  console.log(`closing ${store.name}`);
}
