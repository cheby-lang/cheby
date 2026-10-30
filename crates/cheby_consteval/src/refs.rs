//! The earlier images an image references (D-1188's tag 3), read from its
//! bytes without building anything: what K5b's cache checks before it
//! installs a cached image beside others (D-1261).
//!
//! The reader follows the format `cheby_runtime::consteval::image`
//! documents (D-1188) and checks only what it needs to find every
//! reference: the header, each object's kind and parts, and each
//! reference's tag. Anything it cannot read, an in-process image's
//! address (tag 4) included, is `None`: the caller then treats the image
//! as not cacheable.

use cheby_runtime::image::FIELD_KINDS;
use cheby_runtime::layout::FieldKind;

/// The paths of the earlier images `bytes` references (tag 3), sorted and
/// without repeats; `None` when the bytes are not a stored image of
/// format 1 (an in-process one included).
#[must_use]
pub fn image_references(bytes: &[u8]) -> Option<Vec<Box<str>>> {
    let mut r = Reader { bytes, at: 0 };
    if r.take(4)? != b"CHBV" || r.byte()? != 1 {
        return None;
    }
    let _path = r.text()?;
    let count = r.number()?;
    let mut out: Vec<Box<str>> = Vec::new();
    r.reference(&mut out)?;
    for _ in 0..count {
        r.object(&mut out)?;
    }
    if r.at != bytes.len() {
        return None;
    }
    out.sort();
    out.dedup();
    Some(out)
}

/// A cursor over an image's bytes.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let out = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(out)
    }

    fn byte(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn number(&mut self) -> Option<u64> {
        let mut n: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            let b = self.byte()?;
            let low = u64::from(b & 0x7f);
            if shift >= 64 || (shift == 63 && low > 1) {
                return None;
            }
            n |= low << shift;
            if b & 0x80 == 0 {
                return Some(n);
            }
            shift = shift.saturating_add(7);
        }
    }

    /// A count of parts that take at least a byte each.
    fn count(&mut self) -> Option<usize> {
        let n = usize::try_from(self.number()?).ok()?;
        (n <= self.bytes.len().saturating_sub(self.at)).then_some(n)
    }

    fn text(&mut self) -> Option<&'a str> {
        let n = self.count()?;
        std::str::from_utf8(self.take(n)?).ok()
    }

    /// One reference; an earlier image's path goes into `out`.
    fn reference(&mut self, out: &mut Vec<Box<str>>) -> Option<()> {
        match self.byte()? {
            // An immediate word.
            0 => {
                self.take(8)?;
            }
            // An object of this image, by index.
            1 => {
                self.number()?;
            }
            // A runtime static by its code; `Some(n)` carries `n` as an
            // `i16`.
            2 => {
                if self.byte()? == 8 {
                    self.take(2)?;
                }
            }
            // An object of an earlier image.
            3 => {
                let path = self.text()?;
                self.number()?;
                out.push(path.into());
            }
            _ => return None,
        }
        Some(())
    }

    /// `n` references.
    fn references(&mut self, n: usize, out: &mut Vec<Box<str>>) -> Option<()> {
        for _ in 0..n {
            self.reference(out)?;
        }
        Some(())
    }

    /// One object.
    fn object(&mut self, out: &mut Vec<Box<str>>) -> Option<()> {
        match self.byte()? {
            // `String` and `Bytes`: their bytes.
            1 | 2 => {
                let n = self.count()?;
                self.take(n)?;
            }
            // A boxed number's bits.
            3..=6 => {
                self.take(8)?;
            }
            // A tuple, a list and a set: a count and the references.
            7 | 12 | 14 => {
                let n = self.count()?;
                self.references(n, out)?;
            }
            // `Some`, `Ok` and `Err`: one field.
            8..=10 => self.reference(out)?,
            // A user constructor: type key, index, then each field's kind
            // and its reference or bits.
            11 => {
                self.take(8)?;
                self.number()?;
                let n = self.count()?;
                for _ in 0..n {
                    let kind = FIELD_KINDS.get(usize::from(self.byte()?))?;
                    if *kind == FieldKind::Value {
                        self.reference(out)?;
                    } else {
                        self.take(8)?;
                    }
                }
            }
            // A map: an entry count, then each key and value.
            13 => {
                let n = self.count()?;
                self.references(n.checked_mul(2)?, out)?;
            }
            _ => return None,
        }
        Some(())
    }
}
