//! Code records sealed with their hash (D-960, D-1260, D-1264; the review's
//! RB-5).
//!
//! A package code or tests record and a std code record are stored as
//! `CodeRecord::encode()`'s bytes followed by their SipHash-1-3 128, the way
//! [`super::front::sealed`] seals front records and constant entries. A record
//! whose seal does not match, or that then does not decode, is a miss, which
//! the command writes again (D-960): in release builds as in debug builds,
//! since a record, unlike a blob, is not named by its hash.
//! D-1282: the seal is checked in every build and is a second
//! hash of the record's bytes beside `Shared::get`'s digest (`store.rs`).
//!
//! `CodeRecord`'s own bytes and `RECORD_VERSION` do not change (D-1253): the
//! seal wraps them. The build ID (D-957) changes with any file under
//! `crates/`, so a `cheby` with seals never reads an unsealed record.

use cheby_codegen_clif::record::CodeRecord;

use super::Hash128;
use super::front::{sealed, unsealed};

/// A code record's stored bytes: `CodeRecord::encode()` followed by its
/// SipHash-1-3 128, as [`sealed`] seals front records and constant entries
/// (D-960, D-1260; RB-5).
pub(crate) fn seal_code(record: &CodeRecord) -> Vec<u8> {
    sealed(record.encode())
}

/// The record of `bytes` when the seal matches and `CodeRecord::decode`'s
/// checks pass; `None` otherwise (a miss, D-960).
pub(crate) fn unseal_code(bytes: &[u8]) -> Option<CodeRecord> {
    CodeRecord::decode(unsealed(bytes)?).ok()
}

/// The identity N9's same-code test compares: `Hash128::of(&record.encode())`.
pub(crate) fn record_hash(record: &CodeRecord) -> Hash128 {
    Hash128::of(&record.encode())
}
