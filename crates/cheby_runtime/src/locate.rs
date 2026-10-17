//! Where a blocked fiber waits (D-856): the process-wide map from JIT code
//! addresses to call locations, and the frame-record walk a deadlock report
//! uses to find the innermost call outside package `std`.
//!
//! # The code map
//!
//! GAP(loc-map-lifetime): one map serves the whole process. A JIT
//! registers each batch's functions with [`register_code`] after
//! `finalize_definitions`, under an owner id from [`new_code_owner`], and
//! removes them with [`unregister_code`] when its code memory is freed
//! (D-708's last token holder). A REPL redefinition keeps its old code
//! (D-712), so its old ranges stay and a fiber still in old code is
//! located. Each range has its start, its end, whether the function has the
//! Cheby ABI, and its call sites sorted by return offset as 16-byte
//! `(return offset, path index, line, column)` records; each path is
//! interned once for the process and kept (paths are few and short: one per
//! source file and one per REPL entry). Ranges never overlap: a new range
//! replaces any registered range it overlaps, since the code memory under
//! the old one has been reused, and within one batch the first of two
//! overlapping ranges wins. The map's lock is a leaf (plan §0 'Locks'):
//! written at registration, read only while a deadlock report is built.
//!
//! # The walk
//!
//! A waiting std function publishes its frame record (`abi::CTX_WAIT_FRAME`,
//! D-856); the walk starts there and reads only records of Cheby functions.
//! From a record at `fp` it reads the return address at `fp + 8`, stripped
//! of pointer-authentication bits, and the caller's frame pointer at `fp`
//! (both ISAs keep `[fp] = caller's fp, [fp + 8] = return address`). A
//! return address at a recorded call site is the answer. One elsewhere in
//! Cheby-ABI code (std, shims, an untagged call) moves on to the caller's
//! record, since the address proved the caller is Cheby code. One in a
//! C-ABI function (an entry thunk, a trampoline, a `@blocking` shim) or
//! outside JIT code ends the walk with `None`: Rust frames lie beyond, and
//! Rust may keep no frame records (x86-64 Linux). A frame pointer that is
//! 0, not 16-byte aligned, outside the fiber's stack or not above the
//! previous one ends it too, and so does the cap of [`MAX_FRAMES`] records.
//! The walk is a pure function over a [`FrameReader`], so its tests feed it
//! synthetic stacks and run under Miri.

use std::collections::BTreeMap;
use std::fmt;
use std::mem::{align_of, size_of};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

/// One call site of a JIT function: the call's return address as an offset
/// from the function's start, and the call's location, `path:line:column`
/// (D-856, D-449).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSiteDesc {
    /// The return address's offset from the start of the function.
    pub ret_offset: u32,
    /// The call's location, `path:line:column`; the path may itself hold a
    /// colon (`<repl:3>:1:16`), so the line and the column are split off
    /// from the right.
    pub location: String,
}

/// One function of a JIT batch, as the JIT registers it after
/// `finalize_definitions` (GAP(loc-map-lifetime)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeRangeDesc {
    /// The function's first byte.
    pub start: usize,
    /// The function's length in bytes.
    pub len: u32,
    /// Whether the function has the Cheby ABI. A C-ABI function (an entry
    /// thunk, a trampoline, a `@blocking` shim) ends a walk (D-856).
    pub cheby_abi: bool,
    /// The function's call sites that have a location: calls in code
    /// outside package `std`; empty for std, shims and thunks. Any order.
    pub sites: Vec<CallSiteDesc>,
}

/// A call's location in a deadlock report: `path:line:column` (D-858).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The file's path as diagnostics print it, or `<repl:N>`.
    pub path: Arc<str>,
    /// The line, from 1.
    pub line: u32,
    /// The column, from 1.
    pub column: u32,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.path, self.line, self.column)
    }
}

/// The most frame records one walk reads (plan §0 'Deep data').
pub(crate) const MAX_FRAMES: usize = 4096;

/// The alignment of every frame pointer: frame records are pushed on a
/// 16-byte aligned stack on both supported ISAs.
const FRAME_ALIGN: usize = 16;

/// A frame record: the caller's frame pointer, then the return address.
const RECORD: usize = size_of::<[usize; 2]>();

/// The path index of a call site whose location could not be read: the
/// walk stops there with `None`, since the innermost call outside std was
/// found but has no printable location.
const NO_PATH: u32 = u32::MAX;

/// A call site in the map: 16 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct CallSite {
    ret_offset: u32,
    /// An index into [`CodeMap::paths`], or [`NO_PATH`].
    path: u32,
    line: u32,
    column: u32,
}

const _: () = assert!(size_of::<CallSite>() == 16);

/// What a return address inside a function says.
#[derive(Debug)]
enum SiteAt {
    /// It is no recorded call site: the walk moves on.
    NoSite,
    /// A recorded site without a printable location: the walk ends with
    /// `None`.
    Unknown,
    /// A recorded site: the walk's answer.
    Known(Location),
}

/// A registered function.
#[derive(Debug)]
struct CodeRange {
    start: usize,
    /// One past the last byte.
    end: usize,
    owner: u64,
    cheby_abi: bool,
    /// Sorted by `ret_offset`, without duplicates.
    sites: Box<[CallSite]>,
}

/// The map from code addresses to functions and call sites (module
/// documentation).
#[derive(Debug)]
pub(crate) struct CodeMap {
    /// Sorted by `start`; no two overlap.
    ranges: Vec<CodeRange>,
    /// Every path a site names, each once.
    paths: Vec<Arc<str>>,
    /// Each path's index in `paths`.
    path_ids: BTreeMap<Arc<str>, u32>,
}

impl CodeMap {
    /// An empty map.
    pub(crate) const fn new() -> Self {
        Self {
            ranges: Vec::new(),
            paths: Vec::new(),
            path_ids: BTreeMap::new(),
        }
    }

    /// Adds a batch's functions under `owner` (module documentation): a
    /// range of length 0 or past the end of the address space is skipped,
    /// a site outside its range is dropped, and a registered range a new one
    /// overlaps is removed.
    pub(crate) fn register(&mut self, owner: u64, descs: Vec<CodeRangeDesc>) {
        let mut batch: Vec<CodeRange> = Vec::with_capacity(descs.len());
        for desc in descs {
            let Some(len) = usize::try_from(desc.len).ok().filter(|&len| len > 0) else {
                continue;
            };
            let Some(end) = desc.start.checked_add(len) else {
                continue;
            };
            let mut sites: Vec<CallSite> = desc
                .sites
                .iter()
                .filter(|site| site.ret_offset <= desc.len)
                .map(|site| self.site(site))
                .collect();
            sites.sort_by_key(|site| site.ret_offset);
            sites.dedup_by_key(|site| site.ret_offset);
            batch.push(CodeRange {
                start: desc.start,
                end,
                owner,
                cheby_abi: desc.cheby_abi,
                sites: sites.into_boxed_slice(),
            });
        }
        batch.sort_by_key(|range| range.start);
        let mut disjoint: Vec<CodeRange> = Vec::with_capacity(batch.len());
        for range in batch {
            if disjoint.last().is_some_and(|last| last.end > range.start) {
                continue;
            }
            disjoint.push(range);
        }
        // The batch is sorted and disjoint, so its ends are sorted too: the
        // first range ending after `range.start` is the only candidate.
        self.ranges.retain(|range| {
            let first = disjoint.partition_point(|new| new.end <= range.start);
            disjoint.get(first).is_none_or(|new| new.start >= range.end)
        });
        self.ranges.extend(disjoint);
        self.ranges.sort_by_key(|range| range.start);
    }

    /// Removes every range of `owner`.
    pub(crate) fn unregister(&mut self, owner: u64) {
        self.ranges.retain(|range| range.owner != owner);
    }

    /// A site's record, its path interned: [`NO_PATH`] when its location
    /// has no `path:line:column` form.
    fn site(&mut self, desc: &CallSiteDesc) -> CallSite {
        let parsed = split_location(&desc.location)
            .and_then(|(path, line, column)| Some((self.intern(path)?, line, column)));
        let (path, line, column) = parsed.unwrap_or((NO_PATH, 0, 0));
        CallSite {
            ret_offset: desc.ret_offset,
            path,
            line,
            column,
        }
    }

    /// The index of `path`, added the first time it is seen.
    fn intern(&mut self, path: &str) -> Option<u32> {
        if let Some(&id) = self.path_ids.get(path) {
            return Some(id);
        }
        let id = u32::try_from(self.paths.len())
            .ok()
            .filter(|&id| id != NO_PATH)?;
        let path: Arc<str> = Arc::from(path);
        self.paths.push(Arc::clone(&path));
        self.path_ids.insert(path, id);
        Some(id)
    }

    /// The range holding the byte at `address`.
    fn find(&self, address: usize) -> Option<&CodeRange> {
        let after = self.ranges.partition_point(|range| range.start <= address);
        let range = self.ranges.get(after.checked_sub(1)?)?;
        (address < range.end).then_some(range)
    }

    /// What the return address `ra` inside `range` says.
    fn site_at(&self, range: &CodeRange, ra: usize) -> SiteAt {
        let Some(site) = ra
            .checked_sub(range.start)
            .and_then(|offset| u32::try_from(offset).ok())
            .and_then(|offset| {
                let index = range
                    .sites
                    .binary_search_by_key(&offset, |site| site.ret_offset)
                    .ok()?;
                range.sites.get(index)
            })
        else {
            return SiteAt::NoSite;
        };
        let path = usize::try_from(site.path)
            .ok()
            .and_then(|path| self.paths.get(path));
        match path {
            Some(path) => SiteAt::Known(Location {
                path: Arc::clone(path),
                line: site.line,
                column: site.column,
            }),
            None => SiteAt::Unknown,
        }
    }

    /// The registered ranges as `(start, end, owner)`, in order.
    #[cfg(test)]
    pub(crate) fn spans(&self) -> Vec<(usize, usize, u64)> {
        self.ranges
            .iter()
            .map(|range| (range.start, range.end, range.owner))
            .collect()
    }

    /// How many distinct paths the map holds.
    #[cfg(test)]
    pub(crate) fn path_count(&self) -> usize {
        self.paths.len()
    }
}

/// Splits `path:line:column` from the right, so a path may hold colons
/// (`<repl:3>:1:16`).
fn split_location(location: &str) -> Option<(&str, u32, u32)> {
    let mut parts = location.rsplitn(3, ':');
    let column = parts.next()?.parse().ok()?;
    let line = parts.next()?.parse().ok()?;
    let path = parts.next().filter(|path| !path.is_empty())?;
    Some((path, line, column))
}

/// Reads the words of a stack for [`walk`].
pub(crate) trait FrameReader {
    /// The word at `address`, or `None` when it cannot be read.
    fn word(&self, address: usize) -> Option<usize>;
    /// The return address `ra` without its pointer-authentication bits.
    fn strip(&self, ra: usize) -> usize;
}

/// The innermost recorded call site reached from the frame record at
/// `frame` on the stack `[stack_lo, stack_hi)` (module documentation), or
/// `None`. It reads at most [`MAX_FRAMES`] records and allocates nothing.
pub(crate) fn walk(
    map: &CodeMap,
    reader: &impl FrameReader,
    frame: usize,
    stack_lo: usize,
    stack_hi: usize,
) -> Option<Location> {
    let mut fp = frame;
    for _ in 0..MAX_FRAMES {
        if fp == 0
            || !fp.is_multiple_of(FRAME_ALIGN)
            || fp < stack_lo
            || fp.checked_add(RECORD)? > stack_hi
        {
            return None;
        }
        let ra = reader.strip(reader.word(fp.checked_add(size_of::<usize>())?)?);
        let caller = reader.word(fp)?;
        // The call ends right before its return address.
        let range = map.find(ra.checked_sub(1)?)?;
        if !range.cheby_abi {
            return None;
        }
        match map.site_at(range, ra) {
            SiteAt::Known(location) => return Some(location),
            SiteAt::Unknown => return None,
            SiteAt::NoSite => {}
        }
        if caller <= fp {
            return None;
        }
        fp = caller;
    }
    None
}

/// The process's code map.
static CODE_MAP: RwLock<CodeMap> = RwLock::new(CodeMap::new());

/// The next owner id.
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// A fresh owner id per JIT: the key its ranges are registered and
/// unregistered under.
#[must_use]
pub fn new_code_owner() -> u64 {
    NEXT_OWNER.fetch_add(1, Ordering::Relaxed)
}

/// Adds a finalized batch's functions to the process's map under `owner`
/// (GAP(loc-map-lifetime)): called after `finalize_definitions`, never for
/// a batch that failed. A registered range a new one overlaps is removed.
pub fn register_code(owner: u64, ranges: Vec<CodeRangeDesc>) {
    CODE_MAP
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .register(owner, ranges);
}

/// Removes every range of `owner` from the process's map, when that JIT's
/// code memory is freed.
pub fn unregister_code(owner: u64) {
    CODE_MAP
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .unregister(owner);
}

/// The innermost recorded call site above `frame` (a published frame
/// record), or `None` (D-856).
///
/// Called only by the detector's collect step, after the deadlock is
/// confirmed, under the fiber's scope lock with its state and resumes
/// re-checked, with the fiber's [`crate::ExecCtx::stack_bounds`] as
/// `[stack_lo, stack_hi)`: the fiber cannot retire while that lock is
/// held, and its slot is never unmapped (D-677), so every word in those
/// bounds is readable. The fiber is parked, but D-857's stuck-group wake
/// can resume it during the walk; the words are then read with relaxed
/// atomic loads ([`LiveStack`]), and the caller re-checks the state and
/// resumes afterwards and drops the result if the fiber ran (D-860).
pub(crate) fn locate(frame: usize, stack_lo: usize, stack_hi: usize) -> Option<Location> {
    let map = CODE_MAP.read().unwrap_or_else(PoisonError::into_inner);
    walk(
        &map,
        &LiveStack {
            lo: stack_lo,
            hi: stack_hi,
        },
        frame,
        stack_lo,
        stack_hi,
    )
}

/// The words of a parked fiber's stack `[lo, hi)`, read through exposed
/// provenance with relaxed atomic loads: D-857's stuck-group wake can
/// resume the fiber during a walk ([`locate`]), and its writes then race
/// with these reads.
#[derive(Debug)]
pub(crate) struct LiveStack {
    /// The lowest readable address.
    pub(crate) lo: usize,
    /// One past the highest readable address.
    pub(crate) hi: usize,
}

impl FrameReader for LiveStack {
    fn word(&self, address: usize) -> Option<usize> {
        let end = address.checked_add(size_of::<usize>())?;
        if address < self.lo || end > self.hi || !address.is_multiple_of(align_of::<AtomicUsize>())
        {
            return None;
        }
        let word = std::ptr::with_exposed_provenance_mut::<usize>(address);
        // SAFETY: `address` is aligned to `AtomicUsize` and lies in
        // `[lo, hi)`,
        // which `locate`'s caller guarantees is the mapped, writable stack
        // of a fiber that cannot retire while the walk runs (D-677: slots
        // are never unmapped), so the word is valid for the load. The
        // reference is used for this one load only. The fiber is parked,
        // except after D-857's stuck-group wake: its own writes may then
        // race with this load, which reads a stale word at worst; the
        // caller discards the walk's result in that case (D-860).
        let atomic = unsafe { AtomicUsize::from_ptr(word) };
        Some(atomic.load(Ordering::Relaxed))
    }

    fn strip(&self, ra: usize) -> usize {
        strip_return_address(ra)
    }
}

/// `ra` without the pointer-authentication bits Apple arm64 signs return
/// addresses with (`cranelift-native` enables signing there): `xpaclri`, a
/// hint-space instruction that is a no-op on a core without pointer
/// authentication.
#[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
pub(crate) fn strip_return_address(ra: usize) -> usize {
    let mut lr = ra;
    // SAFETY: `xpaclri` only rewrites x30, which the operand names as both
    // input and output; it touches no memory, no stack and no flags, and
    // it executes as a NOP on a core without PAuth.
    unsafe {
        std::arch::asm!(
            "xpaclri",
            inout("x30") lr,
            options(pure, nomem, nostack, preserves_flags)
        );
    }
    lr
}

/// `ra` unchanged: no other supported target signs return addresses.
#[cfg(not(all(target_arch = "aarch64", not(miri), not(windows))))]
pub(crate) fn strip_return_address(ra: usize) -> usize {
    ra
}
