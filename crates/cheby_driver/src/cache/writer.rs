//! The store's writes behind the command (K3c; D-960, D-551).
//!
//! D-1138: `Store::put_*` hand the file's bytes (header
//! and payload) to the [`Writer`] and return. Each queued entry gets one
//! job on rayon's global pool, the pool that `-j` sizes (D-551, D-622):
//! the store has no thread of its own, so `-j 1` stays one worker. A job
//! takes the oldest queued entry and writes it as D-960 says (into
//! `tmp/`, then a rename); `Store::flush` takes the rest on the calling
//! thread and then waits for the entries other threads are writing, so
//! it never waits for a job that sits in its own thread's queue. While an
//! entry is queued or being written, the process's own gets answer from
//! its bytes. More than [`MAX_IN_FLIGHT`] bytes in flight make a put
//! write on the calling thread. A write that fails is skipped
//! (D-1129): the entry leaves the queue and the
//! next lookup misses.
//!
//! D-1139: the writer remembers, per entry, the hash of the payload that
//! this process wrote, queued, or read and found valid,
//! and a put of the same payload writes nothing. Blobs are named by that
//! hash, and a record's key fixes its payload (D-264), so in a command
//! each blob and record is written at most once; a hint is written again
//! only with other bytes. A get that finds the file missing or bad forgets
//! it, so the next put writes it again (D-960). A blob read counts as
//! valid only where the read checks its hash (debug builds): a release
//! build's damaged blob, which its reader fails to decode, is written
//! again by the next put.
//!
//! An entry is never written by two threads of the process at once: a put
//! of other bytes while the entry is being written marks it dirty, and the
//! writer writes it again with the newest bytes, so the last put wins as
//! it did when writes were synchronous. No file name or content depends on
//! the order the jobs run in (D-264).

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use rustc_hash::FxHashMap;

use super::Hash128;
use super::store::FileKind;

/// The bytes of queued and running writes above which a put writes on
/// its own thread instead, so a command that produces artifacts faster
/// than the disk takes them does not hold them all in memory.
pub(crate) const MAX_IN_FLIGHT: usize = 64 << 20;

/// An entry of the store: its kind and its name.
pub(crate) type EntryId = (FileKind, Hash128);

/// A whole file to write: the 32-byte header and the payload.
pub(crate) type FileBytes = Arc<Vec<u8>>;

/// The queue of writes, the entries this process knows to be on disk, and
/// the condition `flush` waits on.
#[derive(Debug, Default)]
pub(crate) struct Writer {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct State {
    /// Entries queued or being written, with their newest bytes.
    pending: FxHashMap<EntryId, Pending>,
    /// Queued entries, oldest first; each appears once.
    queue: VecDeque<EntryId>,
    /// Entries being written now.
    writing: usize,
    /// The bytes of every pending entry.
    in_flight: usize,
    /// Per entry, the hash of the payload this process wrote or read and
    /// found valid.
    known: FxHashMap<EntryId, Hash128>,
}

#[derive(Debug)]
struct Pending {
    file: FileBytes,
    digest: Hash128,
    writing: bool,
    /// Put again with other bytes while being written.
    dirty: bool,
}

/// What a put must do after [`Writer::submit`].
#[derive(Debug)]
pub(crate) enum Submit {
    /// Nothing: the same payload is on disk or pending, or the newer
    /// bytes replaced a pending entry's.
    Skip,
    /// Queued: spawn one job that runs [`Writer::take`].
    Queued,
    /// Write it now on this thread, then call [`Writer::done`], as a job
    /// does: too many bytes are in flight.
    Now(FileBytes),
}

impl Writer {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while the lock is held leaves the maps consistent: every
        // update is a single insert or remove.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether this process already wrote or validly read `id` with the
    /// payload hashing to `digest`, or has it pending with that payload.
    pub(crate) fn has(&self, id: EntryId, digest: Hash128) -> bool {
        let state = self.lock();
        state.known.get(&id) == Some(&digest)
            || state.pending.get(&id).is_some_and(|p| p.digest == digest)
    }

    /// Hands the whole file of `id` to the writer.
    pub(crate) fn submit(&self, id: EntryId, digest: Hash128, file: FileBytes) -> Submit {
        let mut guard = self.lock();
        let state = &mut *guard;
        if state.known.get(&id) == Some(&digest) {
            return Submit::Skip;
        }
        let len = file.len();
        if let Some(pending) = state.pending.get_mut(&id) {
            if pending.digest != digest {
                state.in_flight = state
                    .in_flight
                    .saturating_sub(pending.file.len())
                    .saturating_add(len);
                pending.file = file;
                pending.digest = digest;
                pending.dirty |= pending.writing;
            }
            return Submit::Skip;
        }
        let now = state.in_flight > 0 && state.in_flight.saturating_add(len) > MAX_IN_FLIGHT;
        state.in_flight = state.in_flight.saturating_add(len);
        state.pending.insert(
            id,
            Pending {
                file: Arc::clone(&file),
                digest,
                writing: now,
                dirty: false,
            },
        );
        if now {
            state.writing = state.writing.saturating_add(1);
            Submit::Now(file)
        } else {
            state.queue.push_back(id);
            drop(guard);
            self.changed.notify_all();
            Submit::Queued
        }
    }

    /// The oldest queued entry, now marked as being written by the caller,
    /// who calls [`Writer::done`] after writing it.
    pub(crate) fn take(&self) -> Option<(EntryId, FileBytes)> {
        let mut guard = self.lock();
        let state = &mut *guard;
        while let Some(id) = state.queue.pop_front() {
            if let Some(pending) = state.pending.get_mut(&id) {
                pending.writing = true;
                state.writing = state.writing.saturating_add(1);
                return Some((id, Arc::clone(&pending.file)));
            }
        }
        None
    }

    /// Ends a write of `id` that `written` says succeeded or failed: the
    /// newest bytes when the entry was put again with other bytes
    /// meanwhile (write them and call this again), else `None`.
    pub(crate) fn done(&self, id: EntryId, written: bool) -> Option<FileBytes> {
        let mut guard = self.lock();
        let state = &mut *guard;
        if let Some(pending) = state.pending.get_mut(&id)
            && pending.dirty
        {
            pending.dirty = false;
            return Some(Arc::clone(&pending.file));
        }
        if let Some(pending) = state.pending.remove(&id) {
            state.in_flight = state.in_flight.saturating_sub(pending.file.len());
            if written {
                state.known.insert(id, pending.digest);
            } else {
                state.known.remove(&id);
            }
        }
        state.writing = state.writing.saturating_sub(1);
        drop(guard);
        self.changed.notify_all();
        None
    }

    /// The whole file of `id` when it is queued or being written.
    pub(crate) fn pending(&self, id: EntryId) -> Option<FileBytes> {
        self.lock().pending.get(&id).map(|p| Arc::clone(&p.file))
    }

    /// Records that `id` was read from disk and found valid, with a payload
    /// that hashes to `digest`.
    pub(crate) fn note_valid(&self, id: EntryId, digest: Hash128) {
        self.lock().known.insert(id, digest);
    }

    /// Records that `id` is missing or bad on disk.
    pub(crate) fn note_missing(&self, id: EntryId) {
        let mut state = self.lock();
        if !state.known.is_empty() {
            state.known.remove(&id);
        }
    }

    /// Waits until no entry is queued or being written, or until one is
    /// queued: `true` when the queue holds an entry for the caller to take.
    pub(crate) fn wait(&self) -> bool {
        let mut state = self.lock();
        loop {
            if !state.queue.is_empty() {
                return true;
            }
            if state.writing == 0 {
                return false;
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}
