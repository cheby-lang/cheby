//! Calling a type's `@std_hooks(equal, hash, debug)` closures (D-342,
//! D-482): the generic routines call them instead of walking the fields,
//! through [`callback::call`] on the running entry (kernel-reentry, D-526).
//!
//! The JIT registers the three function values of each layout with
//! [`crate::layout::set_hooks`]. Each call gives the hook its own
//! references to its arguments, so the hook may keep them, and a hook
//! never sees a value it could update in place. A panic inside a hook
//! parks the entry's thread like any other panic (D-457), abandoning the
//! walk that called it.

use crate::abi::Word;
use crate::callback;
use crate::rc;
use crate::string;
use crate::values;

/// `equal(a, b)` of the hooks: whether two borrowed values of the type are
/// `==`.
///
/// # Safety
///
/// On an entry's thread; `hooks` are a type's registered hooks and `a`,
/// `b` live values of that type.
pub(super) unsafe fn equal(hooks: [Word; 3], a: Word, b: Word) -> bool {
    let [equal, _, _] = hooks;
    // SAFETY: forwarded from the caller; the hook receives owned
    // references, and its result is an owned `Bool`.
    unsafe {
        rc::dup(a);
        rc::dup(b);
        let result = callback::call(equal, &[a, b]);
        let same = values::word_to_bool(result);
        rc::drop(result);
        same
    }
}

/// `hash(value)` of the hooks: the canonical value `K` to hash in place of
/// `value`, owned by the caller.
///
/// # Safety
///
/// As for [`equal`].
pub(super) unsafe fn hash_key(hooks: [Word; 3], value: Word) -> Word {
    let [_, hash, _] = hooks;
    // SAFETY: forwarded from the caller.
    unsafe {
        rc::dup(value);
        callback::call(hash, &[value])
    }
}

/// `debug(value)` of the hooks: the value's debug text, printed as it is.
///
/// # Safety
///
/// As for [`equal`].
pub(super) unsafe fn debug_text(hooks: [Word; 3], value: Word) -> String {
    let [_, _, debug] = hooks;
    // SAFETY: forwarded from the caller; the result is an owned `String`.
    unsafe {
        rc::dup(value);
        let text = callback::call(debug, &[value]);
        let out = string::text_bytes(text)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default();
        rc::drop(text);
        out
    }
}
