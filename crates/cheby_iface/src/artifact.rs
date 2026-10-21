//! The interface artifact (D-953, `cache.md` §3.1): one blob per module.
//!
//! ```text
//! "CHBA" 0x01
//! build ID                 16 bytes
//! interface hash           16 bytes, SipHash-1-3 128 of section H
//! section H                u64 length, then D-460's encoding (CHBI) byte for byte
//! section S                u64 length, then per item of H in encoded order
//!                          (crate::encoded_order): its name's span in the
//!                          module's own file as u32 start and u32 length,
//!                          or 0xFFFFFFFF 0xFFFFFFFF for an item with no span
//! ```
//!
//! Integers are little-endian. GAP(iface-artifact-layout): the section
//! lengths are `u64` and an item without a span takes a whole entry of
//! two `0xFFFFFFFF` words, so S is fixed-size per item. Section H is what the interface hash
//! covers, so a reader checks it ([`read_artifact`]). Section S is not
//! hashed: moving an item changes S and never the hash, so it never
//! reaches a dependent's key (D-953). The format has no compatibility
//! promise across compiler builds; the magic and the build ID catch
//! misuse. The artifact takes the build ID as bytes, so this crate does
//! not depend on the driver.

use crate::{DecodeError, InterfaceHash, hash_encoding};

/// The artifact's magic and version.
const MAGIC: &[u8] = b"CHBA\x01";

/// The span entry of an item without a span.
const NO_SPAN: u32 = u32::MAX;

/// A read artifact. `h` borrows the bytes it was read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact<'a> {
    /// The build ID of the compiler that wrote it (D-957).
    pub build_id: [u8; 16],
    /// The interface hash, checked against `h`.
    pub hash: InterfaceHash,
    /// Section H: the canonical encoding, for [`crate::decode_interface`].
    pub h: &'a [u8],
    /// Section S: each item's span as start and length, in encoded order.
    pub spans: Vec<Option<(u32, u32)>>,
}

/// Writes the artifact of an interface whose canonical encoding is `h`,
/// with its items' `spans` in encoded order ([`crate::encoded_order`]). A
/// span whose start is `0xFFFFFFFF` is written as no span.
#[must_use]
pub fn write_artifact(build_id: [u8; 16], h: &[u8], spans: &[Option<(u32, u32)>]) -> Vec<u8> {
    let s_len = spans.len().saturating_mul(8);
    let mut out = Vec::with_capacity(
        MAGIC
            .len()
            .saturating_add(48)
            .saturating_add(h.len())
            .saturating_add(s_len),
    );
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&build_id);
    out.extend_from_slice(&hash_encoding(h).0);
    out.extend_from_slice(&u64_len(h.len()).to_le_bytes());
    out.extend_from_slice(h);
    out.extend_from_slice(&u64_len(s_len).to_le_bytes());
    for span in spans {
        let (start, len) = span.unwrap_or((NO_SPAN, NO_SPAN));
        out.extend_from_slice(&start.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
    }
    out
}

fn u64_len(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Reads an artifact and checks section H against its recorded hash.
///
/// # Errors
///
/// [`DecodeError::Malformed`] for a wrong magic, a truncated blob, a
/// span section that is not whole entries, or trailing bytes;
/// [`DecodeError::Version`] for another artifact version; and
/// [`DecodeError::HashMismatch`] if H does not hash to the recorded hash.
pub fn read_artifact(bytes: &[u8]) -> Result<Artifact<'_>, DecodeError> {
    let mut r = Cursor { bytes, at: 0 };
    let magic = r.take(MAGIC.len())?;
    if magic != MAGIC {
        return match (magic.split_last(), MAGIC.split_last()) {
            (Some((&version, tag)), Some((_, want))) if tag == want => {
                Err(DecodeError::Version(version))
            }
            _ => Err(DecodeError::Malformed("not an interface artifact")),
        };
    }
    let build_id = r.array16()?;
    let hash = InterfaceHash(r.array16()?);
    let h_len = r.len()?;
    let h = r.take(h_len)?;
    let s_len = r.len()?;
    if !s_len.is_multiple_of(8) {
        return Err(DecodeError::Malformed(
            "the span section is not whole entries",
        ));
    }
    let s = r.take(s_len)?;
    if r.at != bytes.len() {
        return Err(DecodeError::Malformed("trailing bytes"));
    }
    if hash_encoding(h) != hash {
        return Err(DecodeError::HashMismatch);
    }
    let spans = s
        .as_chunks::<8>()
        .0
        .iter()
        .map(|&[s0, s1, s2, s3, l0, l1, l2, l3]| {
            let start = u32::from_le_bytes([s0, s1, s2, s3]);
            (start != NO_SPAN).then(|| (start, u32::from_le_bytes([l0, l1, l2, l3])))
        })
        .collect();
    Ok(Artifact {
        build_id,
        hash,
        h,
        spans,
    })
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .at
            .checked_add(n)
            .ok_or(DecodeError::Malformed("length overflow"))?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(DecodeError::Malformed("truncated artifact"))?;
        self.at = end;
        Ok(slice)
    }

    fn array16(&mut self) -> Result<[u8; 16], DecodeError> {
        <[u8; 16]>::try_from(self.take(16)?)
            .map_err(|_| DecodeError::Malformed("truncated artifact"))
    }

    fn len(&mut self) -> Result<usize, DecodeError> {
        let bytes = <[u8; 8]>::try_from(self.take(8)?)
            .map_err(|_| DecodeError::Malformed("truncated artifact"))?;
        usize::try_from(u64::from_le_bytes(bytes))
            .map_err(|_| DecodeError::Malformed("length overflow"))
    }
}
