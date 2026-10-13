//! Stack slots and the regions they are carved from (D-677, D-678).
//!
//! Stacks come in power-of-two classes from 512 KiB to 16 GiB. A class
//! carves its slots from regions of `max(1 GiB, 8 slots)`, each one
//! `mmap(PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON | MAP_NORESERVE)`
//! with one more page below the slots, which is `PROT_NONE`. Regions are
//! never unmapped while the process runs, so a stale pointer into a slot,
//! such as the monitor's pointer to a finished fiber's control block,
//! always points to mapped memory (I-8). Only pages are released.
//!
//! - **Reuse.** Free slots are kept per class and reused LIFO, so the
//!   warm pages of a recently used slot are reused first. A class keeps
//!   at most [`FREE_KEEP`] free slots warm; beyond that, a slot given back
//!   is released whole and kept apart, and is handed out only when no
//!   warm slot is free.
//! - **Release.** A fiber writes down to [`STACK_HEADROOM`] below its
//!   lowest soft limit, the watermark, without lowering it. A slot whose
//!   watermark minus that headroom is below its warm top ([`WARM_KEEP`]
//!   below the top) has every page below the warm top released, with
//!   `madvise(MADV_DONTNEED)` on Linux and a `MAP_FIXED` remap elsewhere
//!   (macOS's `MADV_FREE` is lazy and would make memory tests flaky). So
//!   a free slot never holds a written page below its warm top.
//! - **Debug guard pages** (D-678). In builds with debug assertions the
//!   lowest page of every slot is `PROT_NONE`, unless
//!   `CHEBY_RT_STACK_GUARD=0`. Release builds keep D-124's rule of no
//!   guard page per slot.
//! - **The fiber control block** lives at the top of its slot, placed
//!   with [`StackSlot::reserve_top`]; the stack grows down from below it.
//!
//! Strict overcommit (`vm.overcommit_memory=2`) is not supported: a
//! region that cannot be mapped makes [`StackPool::take`] fail with
//! [`StackError::OutOfMemory`], which `spawn` reports as D-642's
//! `out of memory` panic (D-677).

use std::fmt;
use std::ptr::NonNull;
use std::sync::OnceLock;

use parking_lot::Mutex;

use crate::entry::STACK_HEADROOM;
use crate::panic::fatal;

/// The smallest class: 512 KiB, for the runtime's internal entries.
pub const MIN_STACK: usize = 512 << 10;

/// The largest class: 16 GiB (D-677).
pub const MAX_STACK: usize = 16 << 30;

/// The smallest stack a user may request (D-660, D-677).
pub const MIN_USER_STACK: usize = 1 << 20;

/// The number of classes, 512 KiB to 16 GiB.
const CLASSES: usize = 16;

/// The `log2` of [`MIN_STACK`].
const MIN_SHIFT: u32 = MIN_STACK.trailing_zeros();

/// The least size of a region's slots.
const REGION_BYTES: usize = 1 << 30;

/// The least number of slots in a region.
const REGION_SLOTS: usize = 8;

/// How far below the top of its slot a fiber's first soft limit lies
/// (architecture §1.2). A fiber whose watermark stayed at it never went
/// deep.
pub const WARM_BYTES: usize = 128 << 10;

/// The part of a slot below its top that a watermark release keeps: the
/// first soft limit's [`WARM_BYTES`] plus D-457's [`STACK_HEADROOM`],
/// which a fiber may write below its soft limit without lowering it (a
/// checked frame of up to 64 KiB, D-679, and the runtime and foreign
/// calls made from it). A fiber that never lowered its first soft limit
/// wrote only here, so giving its slot back costs no system call.
pub const WARM_KEEP: usize = WARM_BYTES + STACK_HEADROOM;

/// The free slots a class keeps warm. A slot given back beyond them has
/// all its pages released.
pub const FREE_KEEP: usize = 64;

/// The most a slot reserves at its top with [`StackSlot::reserve_top`].
pub const MAX_RESERVE: usize = 64 << 10;

/// The alignment of a stack pointer on every supported platform.
const STACK_ALIGN: usize = 16;

/// Why a stack slot could not be handed out or reserved in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackError {
    /// The request is larger than the largest class, 16 GiB.
    TooLarge {
        /// The requested size in bytes.
        requested: usize,
    },
    /// A region could not be mapped, or a slot's guard page could not be
    /// set: the address space or the kernel's mapping table is exhausted.
    OutOfMemory {
        /// The size of the mapping that failed.
        bytes: usize,
    },
    /// [`StackSlot::reserve_top`] was asked for more than [`MAX_RESERVE`]
    /// in all, or with an alignment that is not a power of two.
    BadReserve {
        /// The requested size in bytes.
        bytes: usize,
        /// The requested alignment.
        align: usize,
    },
    /// This platform has no stack regions (Windows until M6, D-714).
    Unsupported,
}

impl fmt::Display for StackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { requested } => {
                write!(f, "a stack of {requested} bytes exceeds the largest class")
            }
            Self::OutOfMemory { bytes } => {
                write!(f, "a stack region of {bytes} bytes could not be mapped")
            }
            Self::BadReserve { bytes, align } => write!(
                f,
                "cannot reserve {bytes} bytes aligned to {align} at the top of a stack slot"
            ),
            Self::Unsupported => f.write_str("fiber stacks are not supported on this platform"),
        }
    }
}

impl std::error::Error for StackError {}

/// The class a request of `bytes` falls into: `bytes` rounded up to a
/// power of two, at least [`MIN_STACK`], or `None` above [`MAX_STACK`].
#[must_use]
pub fn class_size(bytes: usize) -> Option<usize> {
    if bytes > MAX_STACK {
        return None;
    }
    bytes.max(MIN_STACK).checked_next_power_of_two()
}

/// A user-requested stack size clamped to D-660's range, 1 MiB to 16 GiB
/// (D-659, D-677). The pool then rounds it up to its class.
#[must_use]
pub fn clamp_user_size(bytes: u64) -> usize {
    usize::try_from(bytes).map_or(MAX_STACK, |bytes| bytes.clamp(MIN_USER_STACK, MAX_STACK))
}

/// The index of a class size in the pool's table.
fn class_index(size: usize) -> Option<usize> {
    if !size.is_power_of_two() || !(MIN_STACK..=MAX_STACK).contains(&size) {
        return None;
    }
    usize::try_from(size.trailing_zeros().checked_sub(MIN_SHIFT)?).ok()
}

/// A stack slot handed out by a [`StackPool`]: `size` bytes from
/// `bottom` up, owned by whoever holds this value until it goes back
/// with [`StackPool::give_back`] or [`StackPool::quarantine`]. Dropping
/// it instead keeps the slot out of use for the rest of the process, like
/// a quarantine.
#[derive(Debug)]
pub struct StackSlot {
    /// The lowest address of the slot, including its guard page.
    bottom: usize,
    /// The class size.
    size: usize,
    /// The bytes protected at the bottom: one page or none.
    guard: usize,
    /// The bytes reserved at the top with [`StackSlot::reserve_top`].
    reserved: usize,
}

impl StackSlot {
    /// The lowest address of the slot. With a debug guard page, the page
    /// at this address is `PROT_NONE`.
    #[must_use]
    pub fn bottom(&self) -> usize {
        self.bottom
    }

    /// The address one past the highest byte of the slot.
    #[must_use]
    pub fn top(&self) -> usize {
        // The slot lies inside a mapping, so its end does not overflow.
        self.bottom.wrapping_add(self.size)
    }

    /// The size of the slot, its class.
    #[must_use]
    pub fn size(&self) -> usize {
        self.size
    }

    /// The lowest address the stack may use: above the guard page, if any.
    #[must_use]
    pub fn usable_bottom(&self) -> usize {
        self.bottom.wrapping_add(self.guard)
    }

    /// Whether the lowest page of the slot is a debug guard page (D-678).
    #[must_use]
    pub fn guarded(&self) -> bool {
        self.guard != 0
    }

    /// The lowest stack limit a fiber on this slot may have: the bottom
    /// plus D-457's headroom of 256 KiB, which leaves room below the last
    /// entry check for the slow path and runtime calls (D-677).
    #[must_use]
    pub fn hard_limit(&self) -> usize {
        self.bottom.wrapping_add(STACK_HEADROOM)
    }

    /// The highest stack pointer a fresh frame may start from: the top
    /// below everything reserved, aligned down to 16 bytes.
    #[must_use]
    pub fn stack_top(&self) -> usize {
        self.top().wrapping_sub(self.reserved) & !(STACK_ALIGN - 1)
    }

    /// Whether `address` lies inside the slot.
    #[must_use]
    pub fn contains(&self, address: usize) -> bool {
        (self.bottom..self.top()).contains(&address)
    }

    /// The lowest address of the part a watermark release keeps:
    /// [`WARM_KEEP`] below the top, rounded down to a page.
    fn warm_top(&self, page: usize) -> usize {
        page_floor(self.top().saturating_sub(WARM_KEEP), page)
    }

    /// Reserves `bytes` aligned to `align` at the top of the slot, below
    /// what is already reserved, and returns their start. The fiber
    /// control block lives there (D-677); the stack starts below it, at
    /// [`stack_top`](Self::stack_top). The memory is zeroed in a fresh
    /// slot and holds old data in a reused one. A reservation lasts until
    /// the slot goes back to its pool.
    ///
    /// # Errors
    ///
    /// [`StackError::BadReserve`] if `align` is not a power of two or the
    /// reservations would exceed [`MAX_RESERVE`] in all.
    pub fn reserve_top(&mut self, bytes: usize, align: usize) -> Result<NonNull<u8>, StackError> {
        let bad = StackError::BadReserve { bytes, align };
        if !align.is_power_of_two() {
            return Err(bad);
        }
        let start = self
            .top()
            .wrapping_sub(self.reserved)
            .checked_sub(bytes)
            .ok_or(bad)?
            & !(align.wrapping_sub(1));
        let reserved = self.top().wrapping_sub(start);
        if reserved > MAX_RESERVE {
            return Err(bad);
        }
        self.reserved = reserved;
        NonNull::new(std::ptr::with_exposed_provenance_mut(start)).ok_or(bad)
    }

    /// Releases the whole pages of `[low, high)` that lie inside the
    /// usable part of the slot: they read as zero afterwards and use no
    /// memory until touched again. For a quarantined slot, whose dead
    /// frames lie between a parked context and the top.
    ///
    /// # Safety
    ///
    /// Nothing live is stored in `[low, high)`: no frame that will be
    /// resumed or read, and no part of the fiber control block that will be
    /// read.
    pub unsafe fn release(&self, low: usize, high: usize) {
        let page = os::page_size();
        let Some(low) = low.max(self.usable_bottom()).checked_next_multiple_of(page) else {
            return;
        };
        let high = page_floor(high.min(self.top()), page);
        if high > low {
            os::release(low, high.wrapping_sub(low));
        }
    }
}

/// `address` rounded down to a multiple of `page`, a power of two.
fn page_floor(address: usize, page: usize) -> usize {
    address & !(page.wrapping_sub(1))
}

/// What one class of a pool has done, for tests and diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassCounts {
    /// Regions mapped for the class; they are never unmapped.
    pub regions: usize,
    /// Slots carved from the regions so far.
    pub carved: usize,
    /// Free slots, warm or released whole.
    pub free: usize,
    /// Slots quarantined: never handed out again.
    pub quarantined: usize,
}

/// One class's free lists and the unused rest of its newest region.
#[derive(Debug, Default)]
struct ClassState {
    /// The bottoms of the free slots whose top pages are kept, at most
    /// [`FREE_KEEP`], reused last in, first out.
    warm: Vec<usize>,
    /// The bottoms of the free slots released whole, reused last in,
    /// first out once no warm slot is free.
    cold: Vec<usize>,
    /// Slots given back as warm whose deep pages are being released
    /// outside the lock; they count against [`FREE_KEEP`].
    releasing: usize,
    /// The next uncarved slot of the newest region.
    next: usize,
    /// The end of the newest region.
    end: usize,
    /// See [`ClassCounts`].
    regions: usize,
    /// See [`ClassCounts`].
    carved: usize,
    /// See [`ClassCounts`].
    quarantined: usize,
}

/// The process-wide pool of stack slots, shared by every runtime
/// (D-677). Each class has its own lock, held only for list operations
/// and the mapping of a new region, never across a switch (I-5).
#[derive(Debug)]
pub struct StackPool {
    /// Whether every slot gets a debug guard page (D-678).
    guard_pages: bool,
    /// The page size.
    page: usize,
    /// One entry per class, smallest first.
    classes: [Mutex<ClassState>; CLASSES],
}

impl StackPool {
    /// The process-global pool. Its guard pages follow D-678: on in builds
    /// with debug assertions unless `CHEBY_RT_STACK_GUARD=0`, read once.
    #[must_use]
    pub fn global() -> &'static StackPool {
        static POOL: OnceLock<StackPool> = OnceLock::new();
        POOL.get_or_init(Self::from_env)
    }

    /// A pool configured as [`global`](Self::global) is.
    pub(crate) fn from_env() -> StackPool {
        let enabled = cfg!(debug_assertions)
            && std::env::var_os("CHEBY_RT_STACK_GUARD").is_none_or(|value| value != "0");
        Self::new(enabled)
    }

    /// A separate pool, with or without guard pages. Its regions are never
    /// unmapped either, so only tests make more than the global one.
    pub(crate) fn new(guard_pages: bool) -> StackPool {
        StackPool {
            guard_pages,
            page: os::page_size(),
            classes: std::array::from_fn(|_| Mutex::new(ClassState::default())),
        }
    }

    /// Whether the slots of this pool have debug guard pages.
    #[must_use]
    pub fn guard_pages(&self) -> bool {
        self.guard_pages
    }

    /// Hands out a slot of at least `bytes`, rounded up to its class: the
    /// most recently given back warm slot of the class, else the most
    /// recently given back one released whole, else a fresh one.
    ///
    /// # Errors
    ///
    /// [`StackError::TooLarge`] above 16 GiB, [`StackError::OutOfMemory`]
    /// if a new region or a guard page cannot be mapped, and
    /// [`StackError::Unsupported`] on Windows until M6.
    pub fn take(&self, bytes: usize) -> Result<StackSlot, StackError> {
        let too_large = StackError::TooLarge { requested: bytes };
        let size = class_size(bytes).ok_or(too_large)?;
        let class = class_index(size)
            .and_then(|index| self.classes.get(index))
            .ok_or(too_large)?;
        let mut state = class.lock();
        let bottom = match state.warm.pop().or_else(|| state.cold.pop()) {
            Some(bottom) => bottom,
            None => self.carve(&mut state, size)?,
        };
        drop(state);
        Ok(StackSlot {
            bottom,
            size,
            guard: if self.guard_pages { self.page } else { 0 },
            reserved: 0,
        })
    }

    /// Carves a fresh slot of `size` from the class's newest region,
    /// mapping a new region first if it is full.
    fn carve(&self, state: &mut ClassState, size: usize) -> Result<usize, StackError> {
        if state.end.wrapping_sub(state.next) < size {
            let slots = size
                .checked_mul(REGION_SLOTS)
                .map(|bytes| bytes.max(REGION_BYTES));
            let Some(len) = slots.and_then(|bytes| bytes.checked_add(self.page)) else {
                return Err(StackError::OutOfMemory { bytes: usize::MAX });
            };
            let base = os::map(len)?;
            if !os::protect_none(base, self.page) {
                // SAFETY: the region was just mapped and no slot of it was
                // handed out, so nothing points into it.
                unsafe { os::unmap(base, len) };
                return Err(StackError::OutOfMemory { bytes: len });
            }
            state.next = base.wrapping_add(self.page);
            state.end = base.wrapping_add(len);
            state.regions = state.regions.saturating_add(1);
        }
        let bottom = state.next;
        if self.guard_pages && !os::protect_none(bottom, self.page) {
            return Err(StackError::OutOfMemory { bytes: self.page });
        }
        state.next = bottom.wrapping_add(size);
        state.carved = state.carved.saturating_add(1);
        Ok(bottom)
    }

    /// Takes a slot back for reuse. `watermark` is the fiber's lowest soft
    /// limit while the slot was out (the lowest stack limit its entry
    /// checks used), [`StackSlot::bottom`] when unknown, or `usize::MAX` for
    /// a slot no fiber ran on. The fiber may have written down to D-457's
    /// [`STACK_HEADROOM`] below it. If that is below the warm top
    /// ([`WARM_KEEP`] below the top), every page below the warm top is
    /// released first. If the class already keeps [`FREE_KEEP`] warm free
    /// slots, the whole slot is released, and it is handed out again only
    /// when no warm slot is free (D-677, ADR-0032).
    ///
    /// (D-749) D-677 says only that "pages below a depth
    /// watermark are released on reuse". The released range is everything
    /// from the usable bottom up to the warm top, not just the pages the
    /// fiber touched, and the release happens here, when the slot joins the
    /// free list, so that a free slot never holds a written page below its
    /// warm top. The warm top lies [`STACK_HEADROOM`] below the first soft
    /// limit, so a fiber that never lowered it costs no system call.
    ///
    /// Nothing stored in the slot may be used after this call. The slot
    /// must come from this pool; a size that is no class is a broken
    /// invariant and ends the process.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the pool takes the slot back, so the caller cannot use it again"
    )]
    pub fn give_back(&self, slot: StackSlot, watermark: usize) {
        let Some(class) = class_index(slot.size).and_then(|index| self.classes.get(index)) else {
            fatal(format_args!(
                "a stack slot of {} bytes is not a class",
                slot.size
            ));
        };
        let usable = slot.usable_bottom();
        let warm_top = slot.warm_top(self.page);
        let deep = watermark.saturating_sub(STACK_HEADROOM) < warm_top;
        let mut state = class.lock();
        // Decided under the same lock as the push, counting the warm slots
        // still being released, so the class never keeps more than
        // `FREE_KEEP` warm.
        let keep_warm = state.warm.len().saturating_add(state.releasing) < FREE_KEEP;
        let end = match (keep_warm, deep) {
            (true, false) => {
                state.warm.push(slot.bottom);
                return;
            }
            (true, true) => warm_top,
            (false, _) => slot.top(),
        };
        if keep_warm {
            state.releasing = state.releasing.saturating_add(1);
        }
        // The system call runs outside the lock.
        drop(state);
        if end > usable {
            os::release(usable, end.wrapping_sub(usable));
        }
        let mut state = class.lock();
        if keep_warm {
            state.releasing = state.releasing.saturating_sub(1);
            state.warm.push(slot.bottom);
        } else {
            state.cold.push(slot.bottom);
        }
    }

    /// Takes a slot out of use for the rest of the process, for a slot
    /// that something may still point into, such as the frames of a
    /// context parked forever. Its pages stay as they are; the owner may
    /// release the dead part with [`StackSlot::release`] first.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the pool takes the slot, so the caller cannot use it again"
    )]
    pub fn quarantine(&self, slot: StackSlot) {
        if let Some(class) = class_index(slot.size).and_then(|index| self.classes.get(index)) {
            let mut state = class.lock();
            state.quarantined = state.quarantined.saturating_add(1);
        }
    }

    /// The counts of the class that a request of `bytes` falls into.
    #[must_use]
    pub fn counts(&self, bytes: usize) -> Option<ClassCounts> {
        let class = class_size(bytes)
            .and_then(class_index)
            .and_then(|index| self.classes.get(index))?;
        let state = class.lock();
        Some(ClassCounts {
            regions: state.regions,
            carved: state.carved,
            free: state.warm.len().saturating_add(state.cold.len()),
            quarantined: state.quarantined,
        })
    }
}

/// Mapping, protecting and releasing pages.
#[cfg(unix)]
mod os {
    use std::ptr::with_exposed_provenance_mut;

    use super::StackError;

    /// `MAP_NORESERVE` where the platform has it: Linux charges a writable
    /// private mapping against the commit limit without it.
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    const NORESERVE: libc::c_int = libc::MAP_NORESERVE;
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    const NORESERVE: libc::c_int = 0;

    /// The size of a page.
    pub(super) fn page_size() -> usize {
        // SAFETY: `sysconf` has no preconditions.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(size)
            .ok()
            .filter(|size| size.is_power_of_two())
            .unwrap_or(4096)
    }

    /// Maps a fresh region of `len` bytes, readable and writable, and
    /// returns its address with its provenance exposed.
    pub(super) fn map(len: usize) -> Result<usize, StackError> {
        // SAFETY: an anonymous mapping at an address of the kernel's
        // choice touches no existing memory.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON | NORESERVE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED || base.is_null() {
            return Err(StackError::OutOfMemory { bytes: len });
        }
        Ok(base.expose_provenance())
    }

    /// Unmaps a region that never handed out a slot.
    ///
    /// # Safety
    ///
    /// Nothing points into `[base, base + len)`.
    pub(super) unsafe fn unmap(base: usize, len: usize) {
        // SAFETY: forwarded from the caller; a failure leaves the region
        // mapped, which is harmless.
        unsafe { libc::munmap(with_exposed_provenance_mut(base), len) };
    }

    /// Makes `[address, address + len)` inaccessible; false on failure.
    pub(super) fn protect_none(address: usize, len: usize) -> bool {
        // SAFETY: the range is page-aligned inside a region of this pool
        // that nothing has used yet: a fresh region's lowest page or a
        // fresh slot's.
        unsafe { libc::mprotect(with_exposed_provenance_mut(address), len, libc::PROT_NONE) == 0 }
    }

    /// Gives the memory of `[address, address + len)`, page-aligned and
    /// inside a region, back to the operating system. The pages stay
    /// mapped and read as zero.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(super) fn release(address: usize, len: usize) {
        // SAFETY: the callers pass page-aligned ranges of slots that hold
        // nothing live. `MADV_DONTNEED` on a private anonymous mapping
        // frees the pages at once and zero-fills them on the next touch.
        // A failure leaves the pages resident, which is harmless.
        unsafe {
            libc::madvise(
                with_exposed_provenance_mut(address),
                len,
                libc::MADV_DONTNEED,
            )
        };
    }

    /// Gives the memory of `[address, address + len)`, page-aligned and
    /// inside a region, back to the operating system. The pages stay
    /// mapped and read as zero.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub(super) fn release(address: usize, len: usize) {
        let target = with_exposed_provenance_mut::<libc::c_void>(address);
        // SAFETY: the callers pass page-aligned ranges of slots that hold
        // nothing live. A fresh private anonymous mapping with the region's
        // protection replaces them, which frees their memory at once
        // (`MADV_FREE` is lazy on macOS; M1's trick, D-677).
        let result = unsafe {
            libc::mmap(
                target,
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED | NORESERVE,
                -1,
                0,
            )
        };
        if result != target {
            // A failed `MAP_FIXED` may leave a hole, and regions must stay
            // mapped (I-8).
            crate::panic::fatal(format_args!(
                "cannot release {len} bytes of a stack region: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
}

/// Windows until M6 (D-714): there are no regions, and
/// [`StackPool::take`] fails with [`StackError::Unsupported`].
#[cfg(not(unix))]
mod os {
    use super::StackError;

    /// The size of a page.
    pub(super) fn page_size() -> usize {
        4096
    }

    /// Always fails: there are no stack regions here.
    pub(super) fn map(_len: usize) -> Result<usize, StackError> {
        Err(StackError::Unsupported)
    }

    /// Never reached: no region is ever mapped.
    pub(super) unsafe fn unmap(_base: usize, _len: usize) {}

    /// Never reached: no region is ever mapped.
    pub(super) fn protect_none(_address: usize, _len: usize) -> bool {
        false
    }

    /// Never reached: no region is ever mapped.
    pub(super) fn release(_address: usize, _len: usize) {}
}

#[cfg(test)]
mod tests;
