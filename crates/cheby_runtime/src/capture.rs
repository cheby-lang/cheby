//! Captured output (D-723, ADR-0062): the standard-stream writes of one
//! test, held in memory so that `cheby test` can replay each test's output
//! in source order while tests run at once.
//!
//! A [`Capture`] hangs on a test's [`DetachedGroup`]
//! ([`DetachedGroup::with_capture`]). Every fiber of the test, scoped,
//! detached or nested, inherits its spawner's group (D-799), so it reaches
//! the capture through its group with no field of its own. `main`'s group
//! and a REPL session's group never carry one.
//!
//! `io`'s funnel offers each write to [`route`] first (D-684): a write of a
//! fiber whose group has a capture becomes one chunk, in write order, and
//! never fails; any other write (a host thread, an isolated entry, a fiber
//! of a group without a capture) goes to the real stream. A captured
//! fiber's flushes of standard output are recorded in order ([`flush`],
//! [`Chunk::Flush`]) instead of made, so the host's replay flushes where
//! the program did, and the bytes and their buffering are those of a run
//! without the capture (D-723).
//!
//! # Locks
//!
//! The capture's lock is a leaf: taken with no runtime lock held, before
//! (never under) a stream lock, and never held across a fiber switch
//! (I-5). Nothing under it allocates a Cheby object or raises (D-767): a
//! panic report of an unwinding fiber reaches it.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::DetachedGroup;

/// A standard stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// One event of a test's output, in the order the test made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chunk {
    /// One write call's bytes, the parts joined.
    Write(Stream, Vec<u8>),
    /// A flush of standard output that the program made: `io::write_stdout`
    /// (D-365), the flush before a read (D-346) or before a panic report is
    /// taken (D-595). The host flushes there too when it replays the
    /// chunks, so the bytes reach the stream when they would have without
    /// the capture, also relative to standard error (D-723).
    Flush,
}

/// One test's captured output: chunks in write order, one per write call
/// (D-723, ADR-0062). Its lock is a leaf, never held across a switch.
#[derive(Debug, Default)]
pub struct Capture {
    /// The chunks written so far and not taken.
    chunks: Mutex<Vec<Chunk>>,
    /// The temporary files of children's inherited output that no `wait`
    /// or `finish` joined, because an unwinding body left the child to the
    /// reaper (D-1922): their bytes follow the chunks when the host takes
    /// them at the test's end. A leaf lock.
    #[cfg(unix)]
    pending: Mutex<Vec<(Stream, Arc<std::fs::File>)>>,
}

impl Capture {
    /// An empty capture, to hang on a test's group.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Takes the chunks written so far, in order; the host replays them.
    /// The output of children that the reaper took follows them, each
    /// file's bytes as one chunk (D-1922).
    #[must_use]
    pub fn take(&self) -> Vec<Chunk> {
        #[cfg_attr(not(unix), expect(unused_mut, reason = "only Unix keeps files"))]
        let mut chunks = std::mem::take(&mut *self.chunks.lock());
        #[cfg(unix)]
        {
            let pending = std::mem::take(&mut *self.pending.lock());
            for (stream, file) in pending {
                let bytes = file_contents(&file);
                if !bytes.is_empty() {
                    chunks.push(Chunk::Write(stream, bytes));
                }
            }
        }
        chunks
    }

    /// Keeps the temporary file of a child's inherited output on `stream`
    /// until the host takes the chunks (D-1922).
    #[cfg(unix)]
    pub(crate) fn keep_output(&self, stream: Stream, file: Arc<std::fs::File>) {
        self.pending.lock().push((stream, file));
    }

    /// Appends one write's bytes, the parts joined, as one chunk. The bytes
    /// are copied before the lock is taken, so the lock covers only a push.
    fn append(&self, stream: Stream, parts: &[&[u8]]) {
        let bytes = parts.concat();
        if bytes.is_empty() {
            return;
        }
        self.chunks.lock().push(Chunk::Write(stream, bytes));
    }

    /// Appends a child's captured output on `stream`, read from its
    /// temporary file once the child was reaped, as one chunk (D-1918);
    /// nothing when it is empty.
    #[cfg(unix)]
    pub(crate) fn append_output(&self, stream: Stream, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        self.chunks.lock().push(Chunk::Write(stream, bytes));
    }

    /// Appends a flush of standard output, unless the last chunk is one:
    /// a second flush in a row writes nothing.
    fn append_flush(&self) {
        let mut chunks = self.chunks.lock();
        if chunks.last() != Some(&Chunk::Flush) {
            chunks.push(Chunk::Flush);
        }
    }
}

/// Every byte of `file`, from its start, read without moving its
/// position; what a read error leaves out is lost (D-1918).
#[cfg(unix)]
pub(crate) fn file_contents(file: &std::fs::File) -> Vec<u8> {
    use std::os::unix::fs::FileExt;
    let mut data = Vec::new();
    let mut block = vec![0_u8; 64 << 10];
    loop {
        let offset = u64::try_from(data.len()).unwrap_or(u64::MAX);
        match file.read_at(&mut block, offset) {
            Ok(0) => return data,
            Ok(count) => data.extend_from_slice(block.get(..count).unwrap_or_default()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return data,
        }
    }
}

/// The capture of the running fiber's group: `None` on a host thread, on an
/// isolated entry (which has no group) and on a fiber whose group has no
/// capture. `std::process` keeps it for a child whose inherited output it
/// captures (D-1918, `process.rs`).
pub(crate) fn current() -> Option<Arc<Capture>> {
    let me = crate::entry::current_fiber()?;
    let group: Arc<DetachedGroup> = me.conc.group()?;
    group.capture().map(Arc::clone)
}

/// Offers one write of the running fiber to its group's capture:
/// `Some(Ok(()))` when taken, `None` when the write goes to the real stream
/// (a host thread, an isolated entry, an uncaptured group). Never raises
/// (the print kernels are in D-767's table), never allocates a Cheby
/// object; called with no runtime lock held.
pub(crate) fn route(stream: Stream, parts: &[&[u8]]) -> Option<std::io::Result<()>> {
    let capture = current()?;
    capture.append(stream, parts);
    Some(Ok(()))
}

/// Whether the running fiber's writes are captured, for this crate's
/// tests; the io funnel asks [`flush`] instead.
#[cfg(test)]
pub(crate) fn captured() -> bool {
    current().is_some()
}

/// Offers a flush of standard output by the running fiber to its group's
/// capture: true when its writes are captured, and the flush is then
/// recorded as a [`Chunk::Flush`] instead of made; false when the caller
/// flushes the real stream. Never raises, never allocates a Cheby object.
#[must_use = "a caller whose flush was not taken flushes the real stream"]
pub fn flush() -> bool {
    match current() {
        Some(capture) => {
            capture.append_flush();
            true
        }
        None => false,
    }
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
