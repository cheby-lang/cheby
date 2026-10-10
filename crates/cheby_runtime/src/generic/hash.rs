//! Structural hashing (§3.13), consistent with [`super::equal`]: equal
//! values hash equally.
//!
//! The hasher is SipHash-1-3 keyed by a seed: the running entry's (the
//! constant's seed during constant evaluation, the per-process seed
//! otherwise, D-538). Constructors hash their type's key and their tag,
//! never a layout id, so a value hashes the same whatever order layouts
//! were registered in (D-264).

use std::hash::Hasher as _;

use siphasher::sip::SipHasher13;

use super::Reached;
use super::raw::{Raw, View, f32_hash_bits, float_hash_bits, view};
use crate::abi::Word;
use crate::entry;
use crate::layout::FieldKind;
use crate::object::{read_field, to_u64, to_usize, word_to_ptr};
use crate::panic::fatal;

/// Bytes that start each kind of value, so the encoding is prefix-free.
mod mark {
    pub(super) const INT: u8 = 1;
    pub(super) const TEXT: u8 = 2;
    pub(super) const TUPLE: u8 = 3;
    pub(super) const CTOR: u8 = 4;
    pub(super) const FLOAT: u8 = 5;
    pub(super) const F32: u8 = 6;
    pub(super) const BYTES: u8 = 7;
    pub(super) const BOOL: u8 = 8;
}

/// The state of one hashing walk: the SipHash-1-3 state and the children
/// still to hash. A runtime-defined kind's `hash` function writes its own
/// data and schedules its children through it.
#[derive(Debug)]
pub struct Hasher {
    sip: SipHasher13,
    pending: Vec<Word>,
}

impl Hasher {
    /// A hasher keyed by `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            sip: SipHasher13::new_with_keys(seed, 0),
            pending: Vec::new(),
        }
    }

    /// Writes one byte.
    pub fn write_u8(&mut self, value: u8) {
        self.sip.write_u8(value);
    }

    /// Writes a 64-bit number.
    pub fn write_u64(&mut self, value: u64) {
        self.sip.write_u64(value);
    }

    /// Writes a length-prefixed byte string.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.sip.write_u64(to_u64(bytes.len()));
        self.sip.write(bytes);
    }

    /// Schedules a borrowed child value, hashed after the current one, in
    /// push order.
    pub fn push(&mut self, child: Word) {
        self.pending.push(child);
    }

    /// The hash of everything written so far.
    #[must_use]
    pub fn finish(&self) -> u64 {
        self.sip.finish()
    }
}

/// The hash of a borrowed uniform word under the running entry's seed
/// ([`entry::current_seed`]). `Err(Reached::Function)` when the value
/// contains a function value (D-233).
///
/// # Errors
///
/// [`Reached::Function`] when the walk reaches a function value.
///
/// # Safety
///
/// `word` is a tagged integer or a live object, not changed during the
/// call.
pub unsafe fn hash(word: Word) -> Result<u64, Reached> {
    // SAFETY: forwarded from the caller.
    unsafe { hash_with_seed(word, entry::current_seed()) }
}

/// The hash of a borrowed uniform word under `seed`.
///
/// # Errors
///
/// As for [`hash`].
///
/// # Safety
///
/// As for [`hash`].
pub unsafe fn hash_with_seed(word: Word, seed: u64) -> Result<u64, Reached> {
    let mut hasher = Hasher::new(seed);
    let mut stack = vec![word];
    while let Some(word) = stack.pop() {
        // SAFETY: every word on the stack is borrowed from `word`.
        unsafe { step(word, &mut hasher) }?;
        stack.extend(hasher.pending.drain(..).rev());
    }
    Ok(hasher.finish())
}

/// Hashes one level of `word`, scheduling its parts.
///
/// # Safety
///
/// As for [`hash`].
unsafe fn step(word: Word, h: &mut Hasher) -> Result<(), Reached> {
    // SAFETY: forwarded from the caller.
    match unsafe { view(word, "hashing") } {
        View::Closure => return Err(Reached::Function),
        View::Tagged(value) | View::Int(value) => {
            h.write_u8(mark::INT);
            h.write_u64(value.cast_unsigned());
        }
        View::U64(value) => {
            h.write_u8(mark::INT);
            h.write_u64(value);
        }
        View::Float(value) => {
            h.write_u8(mark::FLOAT);
            h.write_u64(float_hash_bits(value));
        }
        View::F32(value) => {
            h.write_u8(mark::F32);
            h.write_u64(f32_hash_bits(value));
        }
        View::String(bytes) => {
            h.write_u8(mark::TEXT);
            h.write_bytes(bytes);
        }
        View::Bytes(bytes) => {
            h.write_u8(mark::BYTES);
            h.write_bytes(bytes);
        }
        View::Tuple(fields) => {
            h.write_u8(mark::TUPLE);
            h.write_u64(to_u64(fields.len()));
            h.pending.extend(fields);
        }
        View::Ctor(info) => {
            // W3-B: a type with `@std_hooks` hashes through its `hash` hook.
            if info.hooks().is_some() {
                fatal(format_args!(
                    "not implemented yet: hashing through `@std_hooks` of `{}`",
                    info.type_name
                ));
            }
            h.write_u8(mark::CTOR);
            h.write_u64(info.type_key);
            h.write_u64(u64::from(info.tag));
            let obj = word_to_ptr(word);
            for field in &info.fields {
                // SAFETY: a live constructor with this field.
                let bits = unsafe { read_field(obj, to_usize(u64::from(field.offset))) };
                if field.kind == FieldKind::Value {
                    h.pending.push(bits);
                    continue;
                }
                match Raw::decode(field.kind, bits) {
                    Raw::Int(value) => {
                        h.write_u8(mark::INT);
                        h.write_u64(value.cast_unsigned());
                    }
                    Raw::Unsigned(value) => {
                        h.write_u8(mark::INT);
                        h.write_u64(value);
                    }
                    Raw::Float(value) => {
                        h.write_u8(mark::FLOAT);
                        h.write_u64(float_hash_bits(value));
                    }
                    Raw::F32(value) => {
                        h.write_u8(mark::F32);
                        h.write_u64(f32_hash_bits(value));
                    }
                    Raw::Bool(value) => {
                        h.write_u8(mark::BOOL);
                        h.write_u8(u8::from(value));
                    }
                }
            }
        }
        View::Custom(kind) => {
            let Some(hash) = kind.hash else {
                fatal(format_args!(
                    "not implemented yet: hashing `{}` values",
                    kind.name
                ));
            };
            // SAFETY: a live object of this kind.
            unsafe { hash(word, h) };
        }
        // W4-A: a `dyn` value's hash combines its descriptor's and its
        // value's (D-530).
        View::Dyn(_) => fatal(format_args!("not implemented yet: hashing `dyn` values")),
        View::Internal(info) => fatal(format_args!(
            "hashing reached a `{}`, which is not a value",
            info.name
        )),
    }
    Ok(())
}
