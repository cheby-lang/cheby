//! The JIT's memory for code and data: one address-space reservation,
//! committed page by page as segments are allocated.
//!
//! Every relocation between JIT code and data is PC-relative and must
//! reach within ±2 GiB: x86-64 uses 32-bit displacements (`X86PCRel4`,
//! `X86CallPCRel4`) because code is not PIC, and aarch64 uses `ADRP`. One
//! reservation smaller than 2 GiB keeps every reference in range on both.
//!
//! cranelift-jit's own `ArenaMemoryProvider` reserves through
//! `region::alloc`, which on Windows calls
//! `VirtualAlloc(MEM_COMMIT | MEM_RESERVE)` and so charges the whole
//! reservation against the system commit limit for every `Jit`. This
//! provider only reserves address space and commits each segment when it is
//! allocated, on every platform:
//!
//! - Unix: `mmap(PROT_NONE, MAP_PRIVATE | MAP_ANON | MAP_NORESERVE)`, then
//!   `mprotect` to read-write per segment. Untouched pages use no memory.
//! - Windows: `VirtualAlloc(MEM_RESERVE, PAGE_NOACCESS)`, then
//!   `VirtualAlloc(MEM_COMMIT, PAGE_READWRITE)` per segment.
//!
//! Segments are writable while the JIT fills them and are never writable
//! and executable at once: `finalize` clears the instruction cache over each
//! code segment, makes it read-execute (with BTI on Linux aarch64 when
//! asked), makes data read-only or keeps it read-write, and flushes the
//! pipelines of every thread, as cranelift-jit's provider does. A finalized
//! segment is never written again; later batches get new segments.
//!
//! The reservation is released by `free_memory` and on drop.

use std::io;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cranelift_jit::{BranchProtection, JITMemoryKind, JITMemoryProvider};
use cranelift_module::{ModuleError, ModuleResult};

/// The address range reserved per `Jit`. Below 2 GiB, so that every
/// PC-relative relocation on x86-64 and aarch64 reaches (see the module
/// documentation).
pub(crate) const ARENA_SIZE: usize = 1 << 30;

/// The final protection of a segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protection {
    ReadExecute,
    ReadWrite,
    Read,
}

impl Protection {
    const fn of(kind: &JITMemoryKind) -> Self {
        match kind {
            JITMemoryKind::Executable => Self::ReadExecute,
            JITMemoryKind::Writable => Self::ReadWrite,
            JITMemoryKind::ReadOnly => Self::Read,
        }
    }
}

/// A run of committed pages of one final protection. Offsets are from the
/// start of the reservation.
#[derive(Debug)]
struct Segment {
    start: usize,
    len: usize,
    /// The first free byte, relative to `start`.
    position: usize,
    protection: Protection,
    finalized: bool,
}

impl Segment {
    /// Where an allocation of `size` bytes aligned to `align` would start,
    /// relative to `start`, if it fits.
    fn fit(&self, size: usize, align: usize) -> Option<usize> {
        let offset = align_up(self.position, align)?;
        let end = offset.checked_add(size)?;
        (!self.finalized && end <= self.len).then_some(offset)
    }
}

/// How much of a reservation is in use, readable after the provider was
/// moved into the `JITModule`.
#[derive(Debug, Default)]
pub(crate) struct Usage {
    reserved: AtomicUsize,
    committed: AtomicUsize,
}

impl Usage {
    /// Bytes of address space reserved: [`ARENA_SIZE`] until the memory is
    /// freed, then 0.
    pub(crate) fn reserved(&self) -> usize {
        self.reserved.load(Ordering::Relaxed)
    }

    /// Bytes committed for segments so far, 0 after the memory is freed.
    pub(crate) fn committed(&self) -> usize {
        self.committed.load(Ordering::Relaxed)
    }
}

/// A [`JITMemoryProvider`] over one reservation of `size` bytes.
pub(crate) struct ReservedArena {
    /// Null once the reservation is released.
    base: *mut u8,
    size: usize,
    page: usize,
    /// Bytes of the reservation handed to segments, a multiple of `page`.
    used: usize,
    segments: Vec<Segment>,
    usage: Arc<Usage>,
}

// SAFETY: the arena owns its reservation exclusively; the raw pointer is
// only an address into it, and every access goes through `&mut self`.
unsafe impl Send for ReservedArena {}

impl std::fmt::Debug for ReservedArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReservedArena")
            .field("size", &self.size)
            .field("used", &self.used)
            .field("segments", &self.segments.len())
            .finish_non_exhaustive()
    }
}

/// `n` rounded up to a multiple of `align`, a power of two.
fn align_up(n: usize, align: usize) -> Option<usize> {
    let mask = align.checked_sub(1)?;
    Some(n.checked_add(mask)? & !mask)
}

impl ReservedArena {
    /// Reserves `size` bytes, rounded up to whole pages, without committing
    /// any.
    ///
    /// # Errors
    ///
    /// The OS error if the address space cannot be reserved.
    pub(crate) fn new(size: usize) -> io::Result<Self> {
        let page = os::page_size()?;
        let size = align_up(size, page)
            .filter(|&s| s > 0)
            .ok_or_else(|| io::Error::other("invalid JIT arena size"))?;
        let base = os::reserve(size)?;
        let usage = Arc::new(Usage::default());
        usage.reserved.store(size, Ordering::Relaxed);
        Ok(Self {
            base,
            size,
            page,
            used: 0,
            segments: Vec::new(),
            usage,
        })
    }

    /// The shared usage counters of this arena.
    pub(crate) fn usage(&self) -> Arc<Usage> {
        Arc::clone(&self.usage)
    }

    fn address(&self, offset: usize) -> *mut u8 {
        self.base.wrapping_add(offset)
    }

    /// Commits the next `len` bytes (whole pages) of the reservation as
    /// read-write and returns their offset.
    fn commit(&mut self, len: usize) -> io::Result<usize> {
        let start = self.used;
        let end = start
            .checked_add(len)
            .filter(|&end| end <= self.size)
            .ok_or_else(|| io::Error::other("the JIT memory arena is exhausted"))?;
        // SAFETY: `[start, end)` lies inside the live reservation and is
        // not part of any segment yet, so no reference points into it.
        unsafe { os::commit(self.address(start), len) }?;
        self.used = end;
        self.usage.committed.fetch_add(len, Ordering::Relaxed);
        Ok(start)
    }

    fn allocate_inner(
        &mut self,
        size: usize,
        align: u64,
        protection: Protection,
    ) -> io::Result<*mut u8> {
        if self.base.is_null() {
            return Err(io::Error::other("the JIT memory arena was freed"));
        }
        let align = usize::try_from(align)
            .ok()
            .filter(|&a| a.is_power_of_two() && a <= self.page)
            .ok_or_else(|| io::Error::other(format!("unsupported JIT alignment {align}")))?;
        // An open segment of the same protection with room.
        if let Some(segment) = self
            .segments
            .iter_mut()
            .find(|s| s.protection == protection && s.fit(size, align).is_some())
            && let Some(offset) = segment.fit(size, align)
        {
            segment.position = offset.saturating_add(size);
            let at = segment.start.saturating_add(offset);
            return Ok(self.address(at));
        }
        let pages = align_up(size.max(1), self.page)
            .ok_or_else(|| io::Error::other("JIT allocation too large"))?;
        // Grow the last segment if it is open, of the same protection, and
        // ends where the committed part of the reservation ends.
        let grow = self.segments.last().is_some_and(|s| {
            s.protection == protection && !s.finalized && s.start.saturating_add(s.len) == self.used
        });
        if grow {
            let added = self.commit(pages)?;
            if let Some(segment) = self.segments.last_mut() {
                segment.len = added.saturating_add(pages).saturating_sub(segment.start);
                if let Some(offset) = segment.fit(size, align) {
                    segment.position = offset.saturating_add(size);
                    let at = segment.start.saturating_add(offset);
                    return Ok(self.address(at));
                }
            }
        }
        // A new segment, page aligned, so any `align <= page` fits at 0.
        let start = self.commit(pages)?;
        self.segments.push(Segment {
            start,
            len: pages,
            position: size,
            protection,
            finalized: false,
        });
        Ok(self.address(start))
    }

    fn finalize_inner(&mut self, branch_protection: BranchProtection) -> io::Result<()> {
        if self.base.is_null() {
            return Ok(());
        }
        for segment in &mut self.segments {
            if segment.finalized {
                continue;
            }
            let address = self.base.wrapping_add(segment.start);
            match segment.protection {
                Protection::ReadExecute => {
                    // Clear the instruction cache before the memory becomes
                    // read-only, as cranelift-jit does (some CPUs have
                    // errata about doing it after).
                    // SAFETY: the segment is committed memory of this
                    // arena, `len` bytes long.
                    unsafe {
                        wasmtime_internal_jit_icache_coherence::clear_cache(
                            address.cast_const().cast(),
                            segment.len,
                        )
                    }
                    .map_err(|e| {
                        io::Error::other(format!("cannot clear the instruction cache: {e}"))
                    })?;
                    // SAFETY: as above; nothing writes code after this.
                    unsafe { os::protect_executable(address, segment.len, branch_protection) }?;
                }
                // SAFETY: the segment is committed memory of this arena.
                Protection::Read => unsafe { os::protect_read_only(address, segment.len) }?,
                Protection::ReadWrite => {}
            }
            segment.finalized = true;
        }
        // Make every thread see the new code before any runs it.
        wasmtime_internal_jit_icache_coherence::pipeline_flush_mt()
            .map_err(|e| io::Error::other(format!("cannot flush the pipelines: {e}")))?;
        Ok(())
    }

    /// Releases the reservation. Idempotent.
    fn release(&mut self) {
        if self.base.is_null() {
            return;
        }
        // SAFETY: `base` and `size` are exactly the live reservation. The
        // caller guarantees nothing uses its memory any more.
        if unsafe { os::release(self.base, self.size) }.is_ok() {
            self.usage.reserved.store(0, Ordering::Relaxed);
            self.usage.committed.store(0, Ordering::Relaxed);
        }
        // On failure the range is leaked, never reused.
        self.base = ptr::null_mut();
        self.segments.clear();
        self.used = 0;
    }
}

impl JITMemoryProvider for ReservedArena {
    fn allocate(&mut self, size: usize, align: u64, kind: JITMemoryKind) -> io::Result<*mut u8> {
        self.allocate_inner(size, align, Protection::of(&kind))
    }

    unsafe fn free_memory(&mut self) {
        self.release();
    }

    fn finalize(&mut self, branch_protection: BranchProtection) -> ModuleResult<()> {
        self.finalize_inner(branch_protection)
            .map_err(|err| ModuleError::Allocation { err })
    }
}

impl Drop for ReservedArena {
    /// Frees the reservation. The `Jit` that owns the arena guarantees that
    /// no code or data of it is used after the `Jit` is dropped.
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(unix)]
mod os {
    use std::io;
    use std::ptr;

    use cranelift_jit::BranchProtection;

    /// Linux charges private writable mappings against the commit limit
    /// unless they are `MAP_NORESERVE`; Apple accepts and ignores it.
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    const NORESERVE: libc::c_int = libc::MAP_NORESERVE;
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    const NORESERVE: libc::c_int = 0;

    fn check(result: libc::c_int) -> io::Result<()> {
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub(super) fn page_size() -> io::Result<usize> {
        // SAFETY: `sysconf` only reads a configuration value.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(page)
            .ok()
            .filter(|p| p.is_power_of_two())
            .ok_or_else(|| io::Error::other("cannot read the page size"))
    }

    pub(super) fn reserve(size: usize) -> io::Result<*mut u8> {
        // SAFETY: a fresh anonymous mapping at an address of the kernel's
        // choice touches no existing memory.
        let address = unsafe {
            libc::mmap(
                ptr::null_mut(),
                size,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON | NORESERVE,
                -1,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            Err(io::Error::last_os_error())
        } else {
            Ok(address.cast())
        }
    }

    /// # Safety
    ///
    /// `[address, address + len)` is reserved, page aligned, and not in use.
    pub(super) unsafe fn commit(address: *mut u8, len: usize) -> io::Result<()> {
        // SAFETY: forwarded from the caller.
        check(unsafe { libc::mprotect(address.cast(), len, libc::PROT_READ | libc::PROT_WRITE) })
    }

    /// # Safety
    ///
    /// `[address, address + len)` is committed memory nothing writes again.
    pub(super) unsafe fn protect_executable(
        address: *mut u8,
        len: usize,
        branch_protection: BranchProtection,
    ) -> io::Result<()> {
        let prot = libc::PROT_READ | libc::PROT_EXEC;
        #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
        let prot = if branch_protection == BranchProtection::BTI
            && std::arch::is_aarch64_feature_detected!("bti")
        {
            prot | libc::PROT_BTI
        } else {
            prot
        };
        #[cfg(not(all(target_arch = "aarch64", target_os = "linux")))]
        let _ = branch_protection;
        // SAFETY: forwarded from the caller.
        check(unsafe { libc::mprotect(address.cast(), len, prot) })
    }

    /// # Safety
    ///
    /// `[address, address + len)` is committed memory nothing writes again.
    pub(super) unsafe fn protect_read_only(address: *mut u8, len: usize) -> io::Result<()> {
        // SAFETY: forwarded from the caller.
        check(unsafe { libc::mprotect(address.cast(), len, libc::PROT_READ) })
    }

    /// # Safety
    ///
    /// `address` and `size` are a whole reservation that nothing uses.
    pub(super) unsafe fn release(address: *mut u8, size: usize) -> io::Result<()> {
        // SAFETY: forwarded from the caller.
        check(unsafe { libc::munmap(address.cast(), size) })
    }
}

#[cfg(windows)]
mod os {
    use std::io;
    use std::ptr;

    use cranelift_jit::BranchProtection;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_NOACCESS,
        PAGE_PROTECTION_FLAGS, PAGE_READONLY, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        VirtualProtect,
    };
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

    pub(super) fn page_size() -> io::Result<usize> {
        // SAFETY: `SYSTEM_INFO` is plain data; `GetSystemInfo` fills it.
        let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid, writable `SYSTEM_INFO`.
        unsafe { GetSystemInfo(&raw mut info) };
        usize::try_from(info.dwPageSize)
            .ok()
            .filter(|p| p.is_power_of_two())
            .ok_or_else(|| io::Error::other("cannot read the page size"))
    }

    pub(super) fn reserve(size: usize) -> io::Result<*mut u8> {
        // SAFETY: reserving fresh address space touches no existing memory.
        let address = unsafe { VirtualAlloc(ptr::null(), size, MEM_RESERVE, PAGE_NOACCESS) };
        if address.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(address.cast())
        }
    }

    /// # Safety
    ///
    /// `[address, address + len)` is reserved, page aligned, and not in use.
    pub(super) unsafe fn commit(address: *mut u8, len: usize) -> io::Result<()> {
        // SAFETY: forwarded from the caller.
        let committed =
            unsafe { VirtualAlloc(address.cast_const().cast(), len, MEM_COMMIT, PAGE_READWRITE) };
        if committed.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    unsafe fn protect(address: *mut u8, len: usize, prot: PAGE_PROTECTION_FLAGS) -> io::Result<()> {
        let mut old: PAGE_PROTECTION_FLAGS = 0;
        // SAFETY: forwarded from the caller; `old` is writable.
        let ok = unsafe { VirtualProtect(address.cast_const().cast(), len, prot, &raw mut old) };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// # Safety
    ///
    /// `[address, address + len)` is committed memory nothing writes again.
    pub(super) unsafe fn protect_executable(
        address: *mut u8,
        len: usize,
        _branch_protection: BranchProtection,
    ) -> io::Result<()> {
        // SAFETY: forwarded from the caller.
        unsafe { protect(address, len, PAGE_EXECUTE_READ) }
    }

    /// # Safety
    ///
    /// `[address, address + len)` is committed memory nothing writes again.
    pub(super) unsafe fn protect_read_only(address: *mut u8, len: usize) -> io::Result<()> {
        // SAFETY: forwarded from the caller.
        unsafe { protect(address, len, PAGE_READONLY) }
    }

    /// # Safety
    ///
    /// `address` is the base of a whole reservation that nothing uses.
    pub(super) unsafe fn release(address: *mut u8, _size: usize) -> io::Result<()> {
        // SAFETY: forwarded from the caller; `MEM_RELEASE` takes size 0.
        if unsafe { VirtualFree(address.cast(), 0, MEM_RELEASE) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

// The reservation path of Windows has never run, and Windows support is
// M6 (D-545), so these tests are not built there.
#[cfg(test)]
#[cfg(not(windows))]
mod tests {
    use super::*;

    #[test]
    fn reserving_commits_nothing() {
        let arena = ReservedArena::new(ARENA_SIZE).unwrap();
        let usage = arena.usage();
        assert_eq!(usage.reserved(), ARENA_SIZE);
        assert_eq!(usage.committed(), 0);
        drop(arena);
        assert_eq!(usage.reserved(), 0);
    }

    /// Allocations are aligned, share segments per protection, commit only
    /// whole pages as needed, are writable before finalizing, and new
    /// segments open after a finalize.
    #[test]
    fn segments_commit_on_demand() {
        let mut arena = ReservedArena::new(ARENA_SIZE).unwrap();
        let usage = arena.usage();
        let page = arena.page;
        let mut code = Vec::new();
        // The most the code allocations can take, padding included.
        let mut code_bytes = 0;
        for align_log2 in 0..8_u32 {
            let align = 1_u64 << align_log2;
            for size in 1..64 {
                let p = arena
                    .allocate(size, align, JITMemoryKind::Executable)
                    .unwrap();
                assert_eq!(p.addr() % usize::try_from(align).unwrap(), 0);
                code.push(p);
                code_bytes += size + usize::try_from(align).unwrap() - 1;
            }
        }
        let data = arena.allocate(100, 8, JITMemoryKind::Writable).unwrap();
        let rodata = arena
            .allocate(3 * page, 16, JITMemoryKind::ReadOnly)
            .unwrap();
        // SAFETY: allocated, committed and writable until finalized.
        unsafe {
            code[0].write(0xc3);
            data.write_bytes(7, 100);
            rodata.write_bytes(9, 3 * page);
        }
        // Whole pages: the code's, one for the data and three for the
        // read-only bytes. The page is 16 KiB on macOS arm64 and 4 KiB on
        // most Linux systems, so the bound is computed, not fixed.
        let committed = usage.committed();
        let bound = align_up(code_bytes, page).unwrap() + 4 * page;
        assert!(
            committed.is_multiple_of(page) && committed <= bound,
            "{committed} bytes committed, at most {bound} expected"
        );
        arena.finalize(BranchProtection::None).unwrap();
        // SAFETY: still mapped: data stays read-write, read-only is readable.
        unsafe {
            assert_eq!(rodata.add(3 * page - 1).read(), 9);
            data.write(1);
        }
        // Finalized segments are closed: the next allocation opens a new one.
        let more = arena.allocate(8, 8, JITMemoryKind::Writable).unwrap();
        assert!(more.addr() >= rodata.addr() + 3 * page);
        // SAFETY: nothing uses the arena's memory after this.
        unsafe { arena.free_memory() };
        assert_eq!(usage.reserved(), 0);
        assert_eq!(usage.committed(), 0);
        assert!(arena.allocate(8, 8, JITMemoryKind::Writable).is_err());
    }

    #[test]
    fn the_arena_has_a_limit() {
        let mut arena = ReservedArena::new(1 << 20).unwrap();
        arena.allocate(900_000, 1, JITMemoryKind::Writable).unwrap();
        assert!(arena.allocate(200_000, 1, JITMemoryKind::Writable).is_err());
        assert!(arena.allocate(8, 1 << 20, JITMemoryKind::Writable).is_err());
    }
}
