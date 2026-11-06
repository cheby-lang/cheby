//! The byte encoding of the driver's cache payloads: front records,
//! read-set hints and stored diagnostics ([`super::front`],
//! [`super::replay`]).
//!
//! Integers are little-endian; a string or a byte string is its length as
//! a `u64` and its bytes; a list is its length as a `u64` and its
//! elements. The encoding is internal: the store's magics and the build ID
//! keep a payload from being read by another compiler (D-957, D-960).
//! [`Reader`] never panics and never preallocates from a length it read:
//! every malformed payload is `None`, which the caller treats as a miss.

/// Appends values to a payload.
#[derive(Debug, Default)]
pub(crate) struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.bytes.push(v);
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    /// A list's or a string's length.
    pub(crate) fn len(&mut self, n: usize) {
        self.u64(u64::try_from(n).unwrap_or(u64::MAX));
    }

    pub(crate) fn raw(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    pub(crate) fn str(&mut self, s: &str) {
        self.len(s.len());
        self.raw(s.as_bytes());
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Reads values back from a payload; `None` for anything malformed.
#[derive(Debug)]
pub(crate) struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) const fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    pub(crate) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.rest.split_at_checked(n)?;
        self.rest = tail;
        Some(head)
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    pub(crate) fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    pub(crate) fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    /// A length, which must not exceed the bytes left (every element
    /// takes at least one byte), so a corrupt length never allocates.
    pub(crate) fn len(&mut self) -> Option<usize> {
        let n = usize::try_from(self.u64()?).ok()?;
        (n <= self.rest.len()).then_some(n)
    }

    pub(crate) fn array16(&mut self) -> Option<[u8; 16]> {
        self.take(16)?.try_into().ok()
    }

    pub(crate) fn str(&mut self) -> Option<&'a str> {
        let n = self.len()?;
        std::str::from_utf8(self.take(n)?).ok()
    }

    /// Whether every byte was read.
    pub(crate) const fn is_done(&self) -> bool {
        self.rest.is_empty()
    }
}
