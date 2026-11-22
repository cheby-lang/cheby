//! Std's export record (design T15): what a release build needs from std
//! besides std's release code records, stored once per build ID and
//! settings, so a warm release command does not check, lower or optimize
//! std again.
//!
//! The record holds std's release masks by name (the masks a release
//! build writes into its package units, [`super::std_release_masks_in`])
//! and std's exports in their byte form ([`cheby_mir::export::encode_bodies`]),
//! both taken from [`super::ReleaseStd`]. It lies under [`std_export_key`]:
//! [`KeyKind::Export`] over the build ID, the tag `std-x1` and std's release
//! settings, the profile and the levers among them ([`super::std_settings`]).
//! The build ID fixes std's sources (D-957), so no source hash enters it.
//! A debug build never reads or writes it: only a build whose std code is
//! release code asks for it ([`super::is_release`]).
//!
//! The record is sealed with its hash, like std's code records
//! ([`crate::cache::seal`], D-960). One that is missing, damaged or does not
//! decode is a miss, logged as `export.std release miss` beside its key's
//! line `key export.std release <hex>` (D-973): the command
//! computes [`super::ReleaseStd`] (std's first use in the process, logged as
//! `parse.std <module>` lines) and writes the record again. A command
//! without a cache, or one that bypasses reads (`CHEBY_DUMP_DIR`, D-965),
//! computes it in the process and writes nothing.

use std::sync::{Arc, OnceLock};

use cheby_iface::Ice;
use cheby_mir::MirOptions;
use cheby_mir::borrow::BorrowMask;
use cheby_mir::export::{EncodedBodies, PortableBodies, PortableSource};

use super::StdStore;
use crate::cache::{BuildId, BuildKind, Key, KeyKind, Settings};
use crate::frontend::Frontend;

/// What a release build takes from std besides its code (module docs).
#[derive(Debug)]
pub(crate) struct StdExports {
    /// Std's release masks of the functions that borrow something, by
    /// module path and item, sorted.
    named: Vec<(String, String, BorrowMask)>,
    /// The bodies a package's units may inline, by module path and item.
    bodies: Bodies,
}

/// Std's exports: [`super::ReleaseStd`]'s, or the record's, each decoded
/// when the build's inliner asks for it, so a warm command decodes only
/// the bodies its package calls.
#[derive(Debug)]
enum Bodies {
    Made(PortableBodies),
    Read(EncodedBodies),
}

impl StdExports {
    /// The masks of std's release functions that borrow something, by
    /// module path and item, sorted.
    pub(crate) fn named(&self) -> &[(String, String, BorrowMask)] {
        &self.named
    }

    /// The bodies a package's units may inline, by module path and item.
    pub(crate) fn bodies(&self) -> &dyn PortableSource {
        match &self.bodies {
            Bodies::Made(bodies) => bodies,
            Bodies::Read(bodies) => bodies,
        }
    }

    /// Every body, decoded, for the checks that compare records.
    pub(crate) fn decoded(&self) -> Result<PortableBodies, Ice> {
        match &self.bodies {
            Bodies::Made(bodies) => Ok(bodies.clone()),
            Bodies::Read(bodies) => bodies.decode_all(),
        }
    }

    /// Whether `self` and `other` hold the same masks and bodies.
    pub(crate) fn same(&self, other: &Self) -> bool {
        self.named == other.named
            && matches!((self.decoded(), other.decoded()), (Ok(a), Ok(b)) if a == b)
    }

    /// The record's payload before its seal: a magic, a version, the
    /// masks and the bodies' bytes, every count and length a little-endian
    /// `u32`.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        put_len(&mut out, self.named.len());
        for (module, item, mask) in &self.named {
            put_str(&mut out, module);
            put_str(&mut out, item);
            out.extend_from_slice(&mask.0.to_le_bytes());
        }
        match &self.bodies {
            Bodies::Made(bodies) => {
                out.extend_from_slice(&cheby_mir::export::encode_bodies(bodies));
            }
            Bodies::Read(bodies) => out.extend_from_slice(bodies.as_bytes()),
        }
        out
    }

    /// What [`StdExports::encode`] wrote, its bodies not decoded yet; `None`
    /// for anything else.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = bytes.strip_prefix(MAGIC.as_slice())?;
        let (&version, rest) = r.split_first()?;
        if version != VERSION {
            return None;
        }
        r = rest;
        let count = take_len(&mut r)?;
        let mut named = Vec::new();
        for _ in 0..count {
            let module = take_str(&mut r)?;
            let item = take_str(&mut r)?;
            let (word, rest) = r.split_first_chunk::<8>()?;
            r = rest;
            named.push((module, item, BorrowMask(u64::from_le_bytes(*word))));
        }
        let bodies = EncodedBodies::read(r.to_vec()).ok()?;
        Some(Self {
            named,
            bodies: Bodies::Read(bodies),
        })
    }
}

/// The first bytes of the record's payload.
const MAGIC: &[u8; 4] = b"CBSX";
/// The version of the payload.
const VERSION: u8 = 1;

/// Appends a count as four little-endian bytes (saturated).
fn put_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&u32::try_from(len).unwrap_or(u32::MAX).to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_len(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

/// Reads what [`put_len`] wrote: a count that cannot exceed the bytes left.
fn take_len(r: &mut &[u8]) -> Option<usize> {
    let (word, rest) = r.split_first_chunk::<4>()?;
    *r = rest;
    let n = usize::try_from(u32::from_le_bytes(*word)).ok()?;
    (n <= r.len()).then_some(n)
}

fn take_str(r: &mut &[u8]) -> Option<String> {
    let len = take_len(r)?;
    let (text, rest) = r.split_at_checked(len)?;
    *r = rest;
    String::from_utf8(text.to_vec()).ok()
}

/// Std's export record key: [`KeyKind::Export`] over the build ID, the tag
/// `std-x1` and `settings`, std's release settings (module docs).
pub(crate) fn std_export_key(build: BuildId, settings: &Settings) -> Key {
    Key::builder(KeyKind::Export, build)
        .str("std-x1")
        .settings(settings)
        .finish()
}

/// What a release build whose std code is release code under `std_mir`
/// ([`super::is_release`]) takes from std besides that code: from std's
/// export record when the build's front end has a cache it reads, and
/// from [`super::ReleaseStd`] otherwise (module docs).
///
/// # Errors
///
/// [`super::release_std`]'s errors on a miss.
pub(crate) fn std_exports(front: &Frontend, std_mir: MirOptions) -> Result<Arc<StdExports>, Ice> {
    let _std = crate::phases::span(crate::phases::Phase::Std);
    let sink = crate::borrowing::sink(front);
    match sink.and_then(crate::borrowing::RecordSink::std_store) {
        Some(at) => {
            let settings =
                super::std_settings(&crate::borrowing::settings(BuildKind::Std, std_mir));
            from_store(&at, &settings)
        }
        None => in_process(),
    }
}

/// Std's export record under `settings` from `at`, or, on a miss, made in
/// the process and stored behind the command (D-1138); a write that fails
/// is skipped, and a later command misses (D-1129).
fn from_store(at: &StdStore<'_>, settings: &Settings) -> Result<Arc<StdExports>, Ice> {
    let key = std_export_key(at.store.build_id(), settings);
    at.log.key("export.std", "release", &key);
    let found = if at.reads {
        at.store
            .get_record(&key)
            .and_then(|bytes| StdExports::decode(crate::cache::front::unsealed(&bytes)?))
    } else {
        None
    };
    at.log.record("export.std", "release", found.is_some());
    if let Some(found) = found {
        return Ok(Arc::new(found));
    }
    let whole = super::whole_std()?;
    for module in &whole.modules {
        at.log.parsed_std(module);
    }
    let made = in_process()?;
    let payload = made.encode();
    // A debug toolchain checks that the record gives back what it holds,
    // so a form of MIR the bytes miss fails every test that writes one.
    if cfg!(debug_assertions) && !StdExports::decode(&payload).is_some_and(|back| back.same(&made))
    {
        return Err(Ice::new(
            "std's export record does not decode to what it holds (design T15)",
        ));
    }
    let _ = at
        .store
        .put_record(&key, &crate::cache::front::sealed(payload));
    Ok(made)
}

/// [`StdExports`] from [`super::ReleaseStd`], made once per process.
///
/// # Errors
///
/// [`super::release_std`]'s errors.
pub(crate) fn in_process() -> Result<Arc<StdExports>, Ice> {
    static MADE: OnceLock<Result<Arc<StdExports>, String>> = OnceLock::new();
    MADE.get_or_init(|| {
        super::release_std()
            .map(|release| {
                Arc::new(StdExports {
                    named: release.named.clone(),
                    bodies: Bodies::Made(release.exports().clone()),
                })
            })
            .map_err(|e| e.message().to_owned())
    })
    .clone()
    .map_err(Ice::new)
}
