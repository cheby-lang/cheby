//! Structural hashing (§3.13), consistent with [`super::equal`]: equal
//! values hash equally.
//!
//! The hasher is SipHash-1-3 keyed by a seed: the running entry's (the
//! constant's seed during constant evaluation, the per-process seed
//! otherwise, D-538). Constructors hash their type's key and their tag,
//! never a layout id, so a value hashes the same whatever order layouts
//! were registered in (D-264). A type with `@std_hooks` hashes its key and
//! the value its `hash` hook returns (D-342, D-482). Descriptors hash
//! their nominal type's module path and name, never an address.

use std::hash::Hasher as _;

use siphasher::sip::SipHasher13;

use super::raw::{Raw, View, f32_hash_bits, float_hash_bits, view};
use super::{Reached, hooks};
use crate::abi::Word;
use crate::layout::FieldKind;
use crate::object::{read_field, to_u64, to_usize, word_to_ptr};
use crate::panic::fatal;
use crate::{desc, entry, rc};

/// Bytes that start each kind of value, so the encoding is prefix-free.
pub(crate) mod mark {
    pub(crate) const INT: u8 = 1;
    pub(crate) const TEXT: u8 = 2;
    pub(crate) const TUPLE: u8 = 3;
    pub(crate) const CTOR: u8 = 4;
    pub(crate) const FLOAT: u8 = 5;
    pub(crate) const F32: u8 = 6;
    pub(crate) const BYTES: u8 = 7;
    pub(crate) const BOOL: u8 = 8;
    /// A list (written by the list root's kind).
    pub(crate) const LIST: u8 = 9;
    pub(crate) const DESC: u8 = 10;
    pub(crate) const DYN: u8 = 11;
}

/// The second `SipHash` key that combines a map entry's key hash and value
/// hash, so an entry's hash differs from its key's.
const ENTRY_KEY: u64 = 0x6d61_7065_6e74_7279;

/// One step of a hashing walk, on the walk's explicit stack.
#[derive(Debug, Clone, Copy)]
enum Task {
    /// Hashes a borrowed value into the current state.
    Value(Word),
    /// Opens an unordered group ([`Hasher::push_unordered`]).
    OpenGroup,
    /// Starts a fresh state, keyed as a separate walk under the hasher's
    /// seed would be, for one key, value or element of the open group.
    Open,
    /// Finishes a map entry's key: its hash waits for the value's.
    CloseKey,
    /// Finishes a map entry's value and adds the entry's hash to the group.
    CloseValue,
    /// Finishes a set element and adds its hash to the group.
    CloseElement,
    /// Closes the group: the wrapping sum of its entry hashes is written
    /// into the enclosing state.
    CloseGroup,
}

/// A state set aside while a part of an unordered group is hashed.
#[derive(Debug)]
enum Saved {
    /// The enclosing `SipHash` state.
    State(SipHasher13),
    /// An open group: the sum of its finished entries, and the hash of the
    /// key whose value is being hashed.
    Group { sum: u64, key: u64 },
}

/// The state of one hashing walk: the SipHash-1-3 state and the children
/// still to hash. A runtime-defined kind's `hash` function writes its own
/// data and schedules its children through it.
///
/// Unordered groups (the entries of a map or set) are hashed on the same
/// explicit stack: each entry in a fresh state whose hash goes into the
/// group's sum, so values that nest maps and sets inside each other,
/// through keys or values, never deepen the Rust stack (plan §0, deep
/// data; GAP(nested-collection-walks)).
#[derive(Debug)]
pub struct Hasher {
    sip: SipHasher13,
    seed: u64,
    pending: Vec<Task>,
    /// The states and groups set aside, innermost last.
    saved: Vec<Saved>,
    /// Values the walk owns, such as the keys `hash` hooks returned,
    /// released when the hasher is dropped.
    owned: Vec<Word>,
}

impl Hasher {
    /// A hasher keyed by `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            sip: SipHasher13::new_with_keys(seed, 0),
            seed,
            pending: Vec::new(),
            saved: Vec::new(),
            owned: Vec::new(),
        }
    }

    /// The seed this hasher is keyed with ([`Hasher::new`]), so
    /// runtime-defined kinds can hash parts of themselves under the
    /// caller's key (maps and sets, plan §4.3.5, D-538).
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Schedules an unordered group of borrowed entries, hashed after the
    /// values already scheduled: a key with its value (a map entry) or an
    /// element alone (`None`, a set). Each key, value and element is hashed
    /// on its own under the hasher's seed, as [`hash_with_seed`] would; a
    /// map entry's hash combines its key's and its value's; the wrapping
    /// sum of the entry hashes, which does not depend on their order, is
    /// then written into this state.
    pub fn push_unordered(&mut self, entries: impl IntoIterator<Item = (Word, Option<Word>)>) {
        self.pending.push(Task::OpenGroup);
        for (key, value) in entries {
            self.pending.push(Task::Open);
            self.pending.push(Task::Value(key));
            match value {
                Some(value) => {
                    self.pending.push(Task::CloseKey);
                    self.pending.push(Task::Open);
                    self.pending.push(Task::Value(value));
                    self.pending.push(Task::CloseValue);
                }
                None => self.pending.push(Task::CloseElement),
            }
        }
        self.pending.push(Task::CloseGroup);
    }

    /// Ends the current part's state and returns its hash, restoring the
    /// enclosing state.
    fn close_part(&mut self) -> u64 {
        let hash = self.sip.finish();
        match self.saved.pop() {
            Some(Saved::State(sip)) => self.sip = sip,
            _ => fatal(format_args!("hashing closed a part it never opened")),
        }
        hash
    }

    /// The innermost open group.
    fn group(&mut self) -> (&mut u64, &mut u64) {
        match self.saved.last_mut() {
            Some(Saved::Group { sum, key }) => (sum, key),
            _ => fatal(format_args!(
                "hashing reached a group entry outside a group"
            )),
        }
    }

    /// Runs one control task of an unordered group.
    fn control(&mut self, task: Task) {
        match task {
            Task::Value(_) => {}
            Task::OpenGroup => self.saved.push(Saved::Group { sum: 0, key: 0 }),
            Task::Open => {
                let fresh = SipHasher13::new_with_keys(self.seed, 0);
                let outer = std::mem::replace(&mut self.sip, fresh);
                self.saved.push(Saved::State(outer));
            }
            Task::CloseKey => {
                let hash = self.close_part();
                *self.group().1 = hash;
            }
            Task::CloseValue => {
                let value = self.close_part();
                let seed = self.seed;
                let (sum, key) = self.group();
                let mut sip = SipHasher13::new_with_keys(seed, ENTRY_KEY);
                sip.write_u64(*key);
                sip.write_u64(value);
                *sum = sum.wrapping_add(sip.finish());
            }
            Task::CloseElement => {
                let element = self.close_part();
                let (sum, _) = self.group();
                *sum = sum.wrapping_add(element);
            }
            Task::CloseGroup => match self.saved.pop() {
                Some(Saved::Group { sum, .. }) => self.sip.write_u64(sum),
                _ => fatal(format_args!("hashing closed a group it never opened")),
            },
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
        self.pending.push(Task::Value(child));
    }

    /// The hash of everything written so far.
    #[must_use]
    pub fn finish(&self) -> u64 {
        self.sip.finish()
    }
}

impl Drop for Hasher {
    fn drop(&mut self) {
        for word in self.owned.drain(..) {
            // SAFETY: the walk owns these references.
            unsafe { rc::drop(word) };
        }
    }
}

/// The hash of a borrowed uniform word under the running entry's seed
/// ([`entry::current_seed`]). `Err(Reached::Function)` when the value
/// contains a function value or a handle (D-233).
///
/// # Errors
///
/// [`Reached::Function`] when the walk reaches a function value or a
/// handle.
///
/// # Safety
///
/// `word` is a tagged integer or a live object, not changed during the
/// call. On an entry's thread when the value may hold a type with hooks.
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
    let mut stack = vec![Task::Value(word)];
    while let Some(task) = stack.pop() {
        match task {
            // SAFETY: every word on the stack is borrowed from `word`, or
            // owned by the hasher until it is dropped.
            Task::Value(word) => unsafe { step(word, &mut hasher) }?,
            control => hasher.control(control),
        }
        stack.extend(hasher.pending.drain(..).rev());
    }
    Ok(hasher.finish())
}

/// Writes a decoded raw field.
fn raw(h: &mut Hasher, field: Raw) {
    match field {
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

/// Hashes one level of `word`, scheduling its parts.
///
/// # Safety
///
/// As for [`hash`].
unsafe fn step(word: Word, h: &mut Hasher) -> Result<(), Reached> {
    // SAFETY: forwarded from the caller.
    match unsafe { view(word, "hashing") } {
        View::Closure | View::External(_) => return Err(Reached::Function),
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
            for field in fields {
                h.push(field);
            }
        }
        View::Ctor(info) => {
            h.write_u8(mark::CTOR);
            h.write_u64(info.type_key);
            if let Some(hooks) = info.hooks() {
                // A type with hooks hashes its key and the `K` its `hash`
                // hook returns, never its tag (D-482,
                // GAP(std-hooks-dispatch)), so values its `equal` hook
                // equates, whatever their constructors, hash equally.
                // SAFETY: a live value of a type with these hooks.
                let key = unsafe { hooks::hash_key(hooks, word) };
                h.owned.push(key);
                h.push(key);
                return Ok(());
            }
            h.write_u64(u64::from(info.tag));
            let obj = word_to_ptr(word);
            for field in &info.fields {
                // SAFETY: a live constructor with this field.
                let bits = unsafe { read_field(obj, to_usize(u64::from(field.offset))) };
                if field.kind == FieldKind::Value {
                    h.push(bits);
                } else {
                    raw(h, Raw::decode(field.kind, bits));
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
        // A `dyn` value's hash combines its descriptor's and its value's
        // (D-530).
        View::Dyn { desc, value } => {
            h.write_u8(mark::DYN);
            h.push(desc);
            h.push(value);
        }
        View::Desc(parts) => {
            // SAFETY: a live descriptor's nominal descriptor.
            let (name, module) = unsafe { desc::names(parts.nominal) };
            h.write_u8(mark::DESC);
            h.write_bytes(module);
            h.write_bytes(name);
            h.write_u64(to_u64(parts.args.len()));
            for arg in parts.args {
                h.push(arg);
            }
        }
        View::Internal(info) => fatal(format_args!(
            "hashing reached a `{}`, which is not a value",
            info.name
        )),
    }
    Ok(())
}
