//! `String` objects (D-454): header (layout 1, scan 0), `len`, `cap`, then
//! `cap` bytes of which the first `len` are UTF-8 text. Static strings
//! (literals) have count 0 and `cap == len`; code generation builds them
//! with [`abi::string::static_image`]. F2's kernels grow here and update in
//! place when the count is 1 and `cap` suffices.

use crate::abi::{self, Word};
use crate::alloc;
use crate::object::{self, abi_offset, read_field, scan_and_layout, to_u64, to_usize, word_to_ptr};
use crate::panic::fatal;

/// Allocates an empty heap string (count 1) with room for `cap` bytes.
/// Ends the process if the size overflows or allocation fails.
#[must_use]
pub fn alloc_string(cap: u64) -> *mut u8 {
    let Some(size) = abi::string::alloc_size(cap) else {
        fatal(format_args!("cannot allocate a string of {cap} bytes"));
    };
    let obj = alloc::alloc_object(size, abi::header(abi::RC_UNIQUE, 0, abi::layout::STRING));
    // SAFETY: a fresh string object: `len` and `cap` are within it.
    unsafe {
        object::write_field(obj, abi_offset(abi::string::LEN_OFFSET), 0);
        object::write_field(obj, abi_offset(abi::string::CAP_OFFSET), cap);
    }
    obj
}

/// A new heap string (count 1) holding `text`, with `cap == len`.
#[must_use]
pub fn new_string(text: &str) -> Word {
    let len = to_u64(text.len());
    let obj = alloc_string(len);
    // SAFETY: the object has room for `len` bytes after `BYTES_OFFSET`, and
    // `text` does not overlap a fresh allocation.
    unsafe {
        std::ptr::copy_nonoverlapping(
            text.as_ptr(),
            obj.add(abi_offset(abi::string::BYTES_OFFSET)),
            text.len(),
        );
        object::write_field(obj, abi_offset(abi::string::LEN_OFFSET), len);
    }
    object::ptr_to_word(obj)
}

/// The text of a string object, or `None` if `word` is not a string.
///
/// # Safety
///
/// `word` is a tagged integer or points to a live object that outlives
/// `'a` and is not changed during `'a`.
#[must_use]
pub unsafe fn bytes<'a>(word: Word) -> Option<&'a [u8]> {
    if abi::is_tagged_int(word) || word == 0 {
        return None;
    }
    let obj = word_to_ptr(word);
    // SAFETY: `obj` is a live object.
    let (_, id) = unsafe { scan_and_layout(obj) };
    if id != abi::layout::STRING {
        return None;
    }
    // SAFETY: a string object has `len` at this offset, and `len` bytes of
    // text after `BYTES_OFFSET`, live for `'a`.
    unsafe {
        let len = to_usize(read_field(obj, abi_offset(abi::string::LEN_OFFSET)));
        Some(std::slice::from_raw_parts(
            obj.add(abi_offset(abi::string::BYTES_OFFSET)),
            len,
        ))
    }
}

/// The text of a string object as a Rust string, with any invalid UTF-8
/// replaced, or `None` if `word` is not a string.
///
/// # Safety
///
/// As for [`bytes`].
#[must_use]
pub unsafe fn to_rust_string(word: Word) -> Option<String> {
    // SAFETY: forwarded from the caller.
    unsafe { bytes(word) }.map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::alloc::live_objects;
    use crate::alloc::tests::serial;
    use crate::rc;

    /// A static string object in an 8-aligned buffer, as code generation
    /// emits it.
    pub(crate) struct StaticImage(Vec<u64>);

    impl StaticImage {
        pub(crate) fn new(text: &str) -> Self {
            let words = abi::string::static_image(text)
                .as_chunks::<8>()
                .0
                .iter()
                .copied()
                .map(u64::from_le_bytes)
                .collect();
            Self(words)
        }

        pub(crate) fn word(&self) -> Word {
            object::ptr_to_word(self.0.as_ptr().cast::<u8>())
        }
    }

    #[test]
    fn heap_strings_hold_their_text() {
        let _guard = serial();
        let before = live_objects();
        for text in ["", "Hello, world!", "héllo wörld ✓", &"x".repeat(1000)] {
            let word = new_string(text);
            // SAFETY: a live string.
            unsafe {
                assert_eq!(bytes(word), Some(text.as_bytes()));
                assert_eq!(to_rust_string(word).as_deref(), Some(text));
                assert_eq!(object::load_rc(word_to_ptr(word)), 1);
                rc::drop(word);
            }
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn strings_with_spare_capacity_free_by_cap() {
        let _guard = serial();
        let before = live_objects();
        let obj = alloc_string(100);
        let word = object::ptr_to_word(obj);
        // SAFETY: a live empty string; then the only reference is dropped.
        unsafe {
            assert_eq!(bytes(word), Some(&b""[..]));
            rc::drop(word);
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn static_images_read_back_as_strings() {
        let image = StaticImage::new("Hello, world!");
        // SAFETY: a static string in a live buffer.
        unsafe {
            assert_eq!(bytes(image.word()), Some(&b"Hello, world!"[..]));
            assert_eq!(object::load_rc(word_to_ptr(image.word())), abi::RC_STATIC);
        }
    }

    #[test]
    fn non_strings_are_rejected() {
        // SAFETY: a tagged integer and a static object.
        unsafe {
            assert_eq!(bytes(abi::tag_int(5).unwrap()), None);
            assert_eq!(bytes(object::nil()), None);
            assert_eq!(bytes(0), None);
        }
    }
}
