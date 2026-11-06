//! Sockets on the poller (D-984, D-680, D-681, ADR-0059): the seam that
//! `std::net`'s kernels (S2, M3 wave 5) build on.
//!
//! - **Registration.** The poll driver keeps a `mio::Registry` cloned from
//!   its `Poll` ([`IoShared`]), outside the poller's lock, so a fiber
//!   registers a socket while a worker blocks in `poll`. A socket is
//!   registered once ([`IoHandle::register`]), when it is created, for
//!   reading and writing, edge-triggered (`mio` is edge-triggered on every
//!   platform), and its finalizer deregisters it
//!   ([`IoHandle::deregister`]), which never waits (D-703).
//! - **Entries.** Each socket has a reference-counted heap entry
//!   ([`IoEntry`]) with its token, readiness bits per direction and one
//!   parked fiber per direction. Nothing of a wait lives on a fiber's
//!   stack: the waiter's header is on the heap (`Arc<WaitHeader>`), so a
//!   frozen fiber (D-708) leaves only heap memory behind. The table maps a
//!   token to its entry; a token carries a generation in its upper half, so
//!   a late event for a socket that was deregistered, and whose slot a new
//!   socket took, finds nothing.
//! - **The wait** ([`IoHandle::wait`]): a kernel tries its system call
//!   first, after clearing its direction's readiness bit. On `WouldBlock`
//!   it arms its direction under the entry's lock (stores its waiter, and
//!   its domain's IO-wait count rises, `DomainCounts::io_armed`), checks the
//!   readiness bit again, publishes its header to the cancellation walk and
//!   parks, handing the entry's lock to its worker (released after the
//!   switch). A bit set during the call makes it disarm and try again.
//! - **Dispatch** ([`IoShared::dispatch`]): whoever polls (the poll holder,
//!   or the monitor on its tick, D-789) sets the bits of each event's
//!   entry and takes the waiting fibers under the entry's lock, then, with
//!   no entry lock held, settles each waiter's header `Waiting -> Claimed`
//!   and wakes its fiber; the count falls only after that wake (D-857), as
//!   a timer's does.
//! - **Cancellation** (D-670, D-983): the walk settles a published header
//!   `Waiting -> Cancelled` and wakes the fiber, which, running again,
//!   disarms its direction if its waiter is still there, lowers the count
//!   and returns [`Cancelled`]. The count of a wait falls exactly once
//!   ([`IoPending::release`]): by the dispatcher after its wake, or by the
//!   fiber once it runs again, whichever comes first; both come after the
//!   wake the wait stands for.
//! - **Closing** (D-1063, [`IoHandle::close`]): `net::close` and
//!   `net::close_listener` mark the entry closed and take both directions'
//!   waiters under the entry's lock, then wake each with the dispatcher's
//!   wake; a wait that tries, arms or re-checks after that finds the entry
//!   closed and returns the error [`is_closed_error`] recognizes. A
//!   cancellation is checked before every try, so a fiber that a
//!   dispatcher woke after the walk targeted it tries nothing more.
//!   `std::net`'s kernels (`net.rs`) serialize their waits per socket and
//!   direction with turns (`sched/turn.rs`), since the entry parks one
//!   fiber per direction.
//! - **The deadlock count** (D-724, D-857, D-860, D-984): only an armed
//!   wait counts, never a registered socket, so an idle listener does not
//!   hide a deadlock. A fiber parked here records no wait site (D-858): it
//!   never appears in a report, and a survey reads it as busy.
//! - **Locks.** An entry's lock and the table's lock are leaves, taken
//!   under the poller's lock by the dispatch and never held while a fiber
//!   is woken or runs; the fiber lock is taken under an entry's lock when a
//!   waiter publishes its header, as under a channel's lock.
//! - **Halts** (D-073, D-708): once a halt starts, a poll dispatches
//!   nothing, as no timer fires (C-4).
//!
//! Windows (D-714): the code builds; a runtime never starts there before
//! M6, so nothing registers.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use parking_lot::Mutex;

use super::deadlock::{self, IoPending};
use super::fiber::{AfterSwitch, Unlock, begin_park, cancel_park, park};
use super::{Shared, WaitHeader, WaitState, wake};
use crate::ctx::ExecCtx;
use crate::fiber_ops::cancel::{self, Cancelled};

/// The direction a fiber waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    /// The socket can be read, accepted on, or its peer closed it.
    Read,
    /// The socket can be written, its connection completed, or it failed.
    Write,
}

impl Direction {
    /// Both directions, in the order a dispatch wakes them.
    const BOTH: [Self; 2] = [Self::Read, Self::Write];

    /// The direction's readiness bit.
    fn bit(self) -> u8 {
        match self {
            Self::Read => READ,
            Self::Write => WRITE,
        }
    }
}

/// The readiness bit of [`Direction::Read`].
const READ: u8 = 1;
/// The readiness bit of [`Direction::Write`].
const WRITE: u8 = 2;

/// The bits of a token below its generation: the slot's index plus one.
const INDEX_BITS: u32 = usize::BITS / 2;
/// The mask of a token's index part.
/// No socket's token has it as its index part: the signal pipe's token
/// (`sched/signal.rs`) does.
pub(super) const INDEX_MASK: usize = (1 << INDEX_BITS) - 1;

/// The token of slot `index` at `generation`: the index plus one (token 0
/// is the waker's) below, the generation's low bits above. `None` when the
/// index does not fit, or would make the token `usize::MAX`, which `mio`
/// may reserve.
fn token_of(index: usize, generation: usize) -> Option<mio::Token> {
    let low = index.checked_add(1).filter(|low| *low < INDEX_MASK)?;
    Some(mio::Token(generation.wrapping_shl(INDEX_BITS) | low))
}

/// The slot index and generation bits a token names, or `None` for the
/// waker's token.
fn parts_of(token: mio::Token) -> Option<(usize, usize)> {
    let index = (token.0 & INDEX_MASK).checked_sub(1)?;
    Some((index, token.0.wrapping_shr(INDEX_BITS)))
}

/// The generation bits a token carries.
fn generation_bits(generation: usize) -> usize {
    generation & (usize::MAX >> INDEX_BITS)
}

/// A fiber parked in one direction of an entry: its header, which the
/// dispatcher or the walk settles once, and its count's token.
struct Waiter {
    /// On the heap, never on the fiber's stack (D-708).
    header: Arc<WaitHeader>,
    /// Its domain's IO-wait count (`None` off every domain).
    pending: Option<Arc<IoPending>>,
}

impl Waiter {
    /// The dispatcher's wake (module documentation): settles the header
    /// `Waiting -> Claimed` and wakes the fiber, then lowers the count. A
    /// header the walk won first is left alone: the walk woke the fiber,
    /// which lowers the count itself once it runs.
    fn wake(self) {
        if !self.header.try_settle_arm(WaitState::Claimed, 0) {
            return;
        }
        // SAFETY: the swap won the header, so its fiber is parked or
        // parking on it and nothing else wakes it: it cannot run, and so
        // cannot retire, before this wake.
        unsafe { wake(self.header.fiber(), false) };
        // D-857: only after the wake the count stands for.
        if let Some(pending) = &self.pending {
            pending.release();
        }
    }
}

/// What an entry's lock guards.
#[derive(Default)]
struct EntryState {
    /// [`READ`] and [`WRITE`]: an event came for the direction since its
    /// last try cleared the bit.
    ready: u8,
    /// Set once by [`IoHandle::deregister`].
    closed: bool,
    /// The fiber waiting to read.
    read: Option<Waiter>,
    /// The fiber waiting to write.
    write: Option<Waiter>,
}

impl EntryState {
    /// The waiter slot of `direction`.
    fn slot(&mut self, direction: Direction) -> &mut Option<Waiter> {
        match direction {
            Direction::Read => &mut self.read,
            Direction::Write => &mut self.write,
        }
    }

    /// Takes `direction`'s waiter if it is the one with `header`.
    fn disarm(&mut self, direction: Direction, header: &Arc<WaitHeader>) -> Option<Waiter> {
        let slot = self.slot(direction);
        if slot
            .as_ref()
            .is_some_and(|waiter| Arc::ptr_eq(&waiter.header, header))
        {
            slot.take()
        } else {
            None
        }
    }
}

/// One registered socket (module documentation).
pub(crate) struct IoEntry {
    /// Its token in the poller, generation included.
    token: mio::Token,
    /// Its readiness and waiters; a leaf lock.
    state: Mutex<EntryState>,
}

impl fmt::Debug for IoEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoEntry")
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl IoEntry {
    /// An event set `bits`: records them, takes the waiters of those
    /// directions under the lock, and wakes them after it.
    fn ready(&self, bits: u8) {
        let mut woken: [Option<Waiter>; 2] = [None, None];
        {
            let mut state = self.state.lock();
            state.ready |= bits;
            for (direction, waiter) in Direction::BOTH.into_iter().zip(woken.iter_mut()) {
                if bits & direction.bit() != 0 {
                    *waiter = state.slot(direction).take();
                }
            }
        }
        for waiter in woken.into_iter().flatten() {
            waiter.wake();
        }
    }
}

/// A slot of the token table.
#[derive(Default)]
struct TableSlot {
    /// Raised each time the slot is freed: a token of an earlier socket
    /// carries an older one.
    generation: usize,
    /// The socket registered there, if any.
    entry: Option<Arc<IoEntry>>,
}

/// The token table: entries by slot, and the free slots.
#[derive(Default)]
struct Table {
    slots: Vec<TableSlot>,
    free: Vec<usize>,
}

/// What the poll driver shares with every [`IoHandle`] (module
/// documentation): the registry clone, the token table and the halt flag.
pub(crate) struct IoShared {
    /// The `mio::Registry` cloned from the driver's `Poll`, used without
    /// the poller's lock.
    registry: mio::Registry,
    /// Tokens to entries; a leaf lock.
    table: Mutex<Table>,
    /// Sockets registered and not deregistered.
    registered: AtomicUsize,
    /// Set once a halt starts: nothing is dispatched any more.
    halted: AtomicBool,
    /// This runtime's descriptor of the signal pipe's reading end, once a
    /// fiber of the runtime armed a scope (D-1073, `sched/signal.rs`).
    #[cfg(unix)]
    signals: std::sync::OnceLock<mio::net::UnixStream>,
}

impl fmt::Debug for IoShared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoShared")
            .field("registered", &self.registered.load(Ordering::Relaxed))
            .field("halted", &self.halted.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl IoShared {
    /// The shared part over `registry`, a clone of the driver's.
    pub(super) fn new(registry: mio::Registry) -> Self {
        Self {
            registry,
            table: Mutex::new(Table::default()),
            registered: AtomicUsize::new(0),
            halted: AtomicBool::new(false),
            #[cfg(unix)]
            signals: std::sync::OnceLock::new(),
        }
    }

    /// Whether any socket, or the signal pipe, is registered: the monitor
    /// polls only then.
    pub(super) fn has_sockets(&self) -> bool {
        // D-1073: the signal pipe, once a fiber of the runtime armed a scope.
        #[cfg(unix)]
        let signals = self.signals.get().is_some();
        #[cfg(not(unix))]
        let signals = false;
        self.registered.load(Ordering::SeqCst) != 0 || signals
    }

    /// Registers the signal pipe's reading end that `make` duplicates
    /// with this poller under `SIGNAL_TOKEN`, readable, unless it is
    /// registered already (D-1073, `sched/signal.rs`). Called under the
    /// signal registry's lock, so two fibers never both register.
    ///
    /// # Errors
    ///
    /// The system's error if the descriptor cannot be duplicated or
    /// registered; nothing is registered then.
    #[cfg(unix)]
    pub(super) fn register_signals(
        &self,
        make: impl FnOnce() -> io::Result<mio::net::UnixStream>,
    ) -> io::Result<()> {
        if self.signals.get().is_some() {
            return Ok(());
        }
        let mut read = make()?;
        self.registry.register(
            &mut read,
            super::signal::SIGNAL_TOKEN,
            mio::Interest::READABLE,
        )?;
        // The registry's lock is held, so nobody set it meanwhile.
        let _ = self.signals.set(read);
        Ok(())
    }

    /// A halt started: from now on nothing is dispatched (D-708, C-4).
    pub(super) fn halt(&self) {
        self.halted.store(true, Ordering::SeqCst);
    }

    /// Dispatches the events of one poll (module documentation): for each
    /// event of a live token, sets its entry's bits and wakes its waiters.
    /// The waker's event and a late event of a deregistered socket do
    /// nothing. Called by the poller's holder, under the poller's lock.
    pub(super) fn dispatch(&self, events: &mio::Events) {
        for event in events {
            #[cfg(unix)]
            if event.token() == super::signal::SIGNAL_TOKEN {
                // D-1073: the handler wrote to the signal pipe; whoever
                // polled acts on it after releasing the poller.
                if let Some(read) = self.signals.get() {
                    super::signal::drain(read);
                }
                continue;
            }
            let mut bits = 0;
            if event.is_readable() || event.is_read_closed() || event.is_error() {
                bits |= READ;
            }
            if event.is_writable() || event.is_write_closed() || event.is_error() {
                bits |= WRITE;
            }
            self.deliver(event.token(), bits);
        }
    }

    /// Delivers readiness `bits` for `token`: nothing for the waker's
    /// token, a token whose generation is stale, during a halt, or for no
    /// bits.
    pub(super) fn deliver(&self, token: mio::Token, bits: u8) {
        if bits == 0 || self.halted.load(Ordering::SeqCst) {
            return;
        }
        if let Some(entry) = self.lookup(token) {
            entry.ready(bits);
        }
    }

    /// The live entry `token` names, if its generation is current.
    fn lookup(&self, token: mio::Token) -> Option<Arc<IoEntry>> {
        let (index, generation) = parts_of(token)?;
        let table = self.table.lock();
        let slot = table.slots.get(index)?;
        if generation_bits(slot.generation) != generation {
            return None;
        }
        slot.entry.clone()
    }

    /// Takes a free slot for a new entry and returns the entry.
    fn insert(&self) -> io::Result<Arc<IoEntry>> {
        let mut table = self.table.lock();
        let index = if let Some(index) = table.free.pop() {
            index
        } else {
            let index = table.slots.len();
            table.slots.push(TableSlot::default());
            index
        };
        let Some(slot) = table.slots.get_mut(index) else {
            return Err(io::Error::other("the socket table lost a slot"));
        };
        let Some(token) = token_of(index, generation_bits(slot.generation)) else {
            table.free.push(index);
            return Err(io::Error::other("too many sockets are registered"));
        };
        let entry = Arc::new(IoEntry {
            token,
            state: Mutex::new(EntryState::default()),
        });
        slot.entry = Some(Arc::clone(&entry));
        self.registered.fetch_add(1, Ordering::SeqCst);
        Ok(entry)
    }

    /// Frees the slot of `token` if it still holds `entry`: its
    /// generation rises, so a later event for `token` finds nothing.
    fn remove(&self, entry: &Arc<IoEntry>) {
        let Some((index, generation)) = parts_of(entry.token) else {
            return;
        };
        let mut table = self.table.lock();
        let Some(slot) = table.slots.get_mut(index) else {
            return;
        };
        let current = generation_bits(slot.generation) == generation
            && slot
                .entry
                .as_ref()
                .is_some_and(|known| Arc::ptr_eq(known, entry));
        if !current {
            return;
        }
        slot.entry = None;
        slot.generation = slot.generation.wrapping_add(1);
        table.free.push(index);
        self.registered.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A socket registered on its runtime's poller (D-984): a
/// reference-counted heap entry with a generation-checked token, readiness
/// bits per direction and one parked fiber per direction.
pub(crate) struct IoHandle {
    /// The socket's entry.
    entry: Arc<IoEntry>,
    /// The registry and the table it is registered in.
    io: Arc<IoShared>,
}

impl fmt::Debug for IoHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoHandle")
            .field("token", &self.entry.token)
            .finish_non_exhaustive()
    }
}

/// What [`deregistered`]'s error carries, so that a kernel can tell a
/// closed handle from the system's own `ENOTCONN` ([`is_closed_error`]).
#[derive(Debug)]
struct ClosedEntry;

impl fmt::Display for ClosedEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the socket was closed or deregistered from its runtime's poller")
    }
}

impl std::error::Error for ClosedEntry {}

/// The error a wait on a closed or deregistered handle returns.
fn deregistered() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, ClosedEntry)
}

/// Whether `error` is the one a wait on a closed or deregistered handle
/// returns ([`IoHandle::close`], D-1063).
pub(crate) fn is_closed_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<ClosedEntry>)
}

/// The error a second wait in a direction that a fiber already waits in
/// returns: D-984 keeps one parked fiber per direction, so the kernels
/// serialize their waits per socket and direction.
fn busy() -> io::Error {
    io::Error::new(
        io::ErrorKind::ResourceBusy,
        "another fiber already waits on this socket in this direction",
    )
}

impl IoHandle {
    /// Registers `source` once, for reading and writing, edge-triggered,
    /// through the registry clone the driver keeps outside the poller's
    /// lock.
    ///
    /// # Errors
    ///
    /// The system's error, or an error when the runtime has no system
    /// poller (a test driver) or the token table is full.
    pub(crate) fn register(
        runtime: &Shared,
        source: &mut dyn mio::event::Source,
    ) -> io::Result<IoHandle> {
        let Some(io) = runtime.driver.io() else {
            return Err(io::Error::other("the runtime has no system poller"));
        };
        Self::register_on(io, source)
    }

    /// [`IoHandle::register`] on the shared part `io`.
    fn register_on(io: &Arc<IoShared>, source: &mut dyn mio::event::Source) -> io::Result<Self> {
        let entry = io.insert()?;
        if let Err(error) = io.registry.register(
            source,
            entry.token,
            mio::Interest::READABLE | mio::Interest::WRITABLE,
        ) {
            io.remove(&entry);
            return Err(error);
        }
        Ok(Self {
            entry,
            io: Arc::clone(io),
        })
    }

    /// Runs `op` on the calling fiber until it does not return
    /// `WouldBlock`: try, arm (the domain's IO-wait count rises), re-check,
    /// park; a cancellation that wins the wait disarms it and is returned
    /// as `Err(Cancelled)` (D-670, D-984). A wait on a deregistered handle,
    /// or in a direction another fiber waits in, returns an error instead
    /// of parking. `me` is the running fiber; one of a runtime, since only
    /// those can park (D-983: the kernel names its operation in that fatal
    /// error).
    ///
    /// # Errors
    ///
    /// [`Cancelled`] when a cancellation reached the fiber before or while
    /// it waited.
    pub(crate) fn wait<T>(
        &self,
        me: &ExecCtx,
        dir: Direction,
        op: &mut dyn FnMut() -> io::Result<T>,
    ) -> Result<io::Result<T>, Cancelled> {
        loop {
            // A fiber targeted by a cancellation tries nothing more, also
            // after a dispatcher's wake that won the header first: a
            // cancelled `accept` or read takes nothing (D-670, D-816,
            // D-1000).
            cancel::check(me)?;
            // Try: the bit is cleared first, so an event that comes during
            // the call is kept for the re-check.
            {
                let mut state = self.entry.state.lock();
                if state.closed {
                    return Ok(Err(deregistered()));
                }
                state.ready &= !dir.bit();
            }
            match op() {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                done => return Ok(done),
            }
            if me.sched.runtime().is_none() {
                crate::fiber_ops::needs_scheduler("a socket wait");
            }
            let counts = deadlock::counts_of(me);
            let header = Arc::new(WaitHeader::new(me));
            begin_park(me);
            let mut state = self.entry.state.lock();
            if state.slot(dir).is_some() {
                drop(state);
                cancel_park(me);
                return Ok(Err(busy()));
            }
            // Arm: the waiter, and its domain's count.
            let pending = counts.map(|counts| counts.io_armed());
            *state.slot(dir) = Some(Waiter {
                header: Arc::clone(&header),
                pending: pending.clone(),
            });
            // Re-check: an event during the call, or a deregistration.
            if state.closed || state.ready & dir.bit() != 0 {
                let waiter = state.disarm(dir, &header);
                drop(state);
                cancel_park(me);
                drop(waiter);
                if let Some(pending) = &pending {
                    pending.release();
                }
                continue;
            }
            // SAFETY: `header` is held by this frame until after
            // `unpublish` below.
            if unsafe { cancel::publish(me, &header) }.is_err() {
                // Cancelled before it waits: no dispatcher has seen the
                // waiter, which this lock still hides.
                let waiter = state.disarm(dir, &header);
                drop(state);
                cancel_park(me);
                drop(waiter);
                if let Some(pending) = &pending {
                    pending.release();
                }
                return Err(Cancelled);
            }
            park(me, AfterSwitch::Park(Some(Unlock::new(state))));
            cancel::unpublish(me);
            // Woken: by the dispatcher, which took the waiter, or by the
            // walk, which left it armed.
            let waiter = self.entry.state.lock().disarm(dir, &header);
            drop(waiter);
            // D-857: this fiber runs, so the wake has happened.
            if let Some(pending) = &pending {
                pending.release();
            }
            match header.state() {
                WaitState::Cancelled => return Err(Cancelled),
                WaitState::Claimed | WaitState::Closed | WaitState::Waiting => {}
            }
        }
    }

    /// Deregisters `source`; never waits, so a finalizer may call it
    /// (D-703). The token's slot is freed with a new generation, so a late
    /// event finds nothing; a fiber still armed on the handle, which no
    /// kernel leaves behind, is woken and finds it deregistered.
    pub(crate) fn deregister(&self, source: &mut dyn mio::event::Source) {
        self.io.remove(&self.entry);
        self.close();
        // The socket is closed right after; an error leaves nothing to do.
        let _ = self.io.registry.deregister(source);
    }

    /// Marks the entry closed and wakes the fibers parked on it in either
    /// direction, each with the dispatcher's wake (one wake per wait, the
    /// count after it, D-857); a later wait returns at once with the error
    /// [`is_closed_error`] recognizes (D-1063). Never waits: it takes only
    /// the entry's leaf lock. The caller closes the descriptor once no
    /// system call on it is running; the registration stays until
    /// [`IoHandle::deregister`].
    pub(crate) fn close(&self) {
        let woken = {
            let mut state = self.entry.state.lock();
            state.closed = true;
            [state.read.take(), state.write.take()]
        };
        for waiter in woken.into_iter().flatten() {
            waiter.wake();
        }
    }

    /// Another handle on the same entry and poller, for the owner of the
    /// socket's descriptor, which deregisters it when the last system call
    /// on it has returned (`net.rs`).
    pub(crate) fn share(&self) -> Self {
        Self {
            entry: Arc::clone(&self.entry),
            io: Arc::clone(&self.io),
        }
    }

    /// The handle's token (for tests).
    #[cfg(test)]
    pub(crate) fn token(&self) -> mio::Token {
        self.entry.token
    }

    /// The runtime's shared part (for tests).
    #[cfg(test)]
    pub(crate) fn shared(&self) -> &Arc<IoShared> {
        &self.io
    }

    /// Whether a fiber waits in `dir` (for tests).
    #[cfg(test)]
    pub(crate) fn armed(&self, dir: Direction) -> bool {
        self.entry.state.lock().slot(dir).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_tokens_carry_the_index_and_the_generation() {
        for (index, generation) in [(0, 0), (1, 0), (0, 7), (41, usize::MAX)] {
            let token = token_of(index, generation_bits(generation)).unwrap();
            assert_ne!(token, mio::Token(0), "the waker's token");
            assert_ne!(token, mio::Token(usize::MAX));
            assert_eq!(parts_of(token), Some((index, generation_bits(generation))));
        }
        assert_eq!(parts_of(mio::Token(0)), None);
        assert_eq!(token_of(INDEX_MASK, 0), None);
        // A slot's next generation gives another token.
        assert_ne!(
            token_of(3, generation_bits(5)),
            token_of(3, generation_bits(6))
        );
    }
}
