//! Every thread-local of the runtime (D-686).
//!
//! This is the only module of the crate that declares `thread_local!`, and
//! every access goes through one of its `#[inline(never)]` accessors. The
//! reason is fiber migration: LLVM may compute a thread-local's address
//! once and reuse it across an opaque call, `llvm.threadlocal.address`
//! having no memory effects. If that call is a fiber switch and the fiber
//! resumes on another thread, inlined code would go on using the old
//! thread's variable, such as its block cache. An accessor that is never
//! inlined recomputes the address at every call, and pointers read from
//! thread-locals pass through [`std::hint::black_box`].
//!
//! The rules for callers (D-686, D-687):
//!
//! - nothing derived from a thread-local (a pointer, a `RefCell` borrow)
//!   lives across a fiber switch;
//! - the closures given to `with_*` never suspend the fiber.
//!
//! The thread-locals:
//!
//! - the current fiber: the control block ([`ExecCtx`]) of the fiber
//!   running on this thread, or null;
//! - the shard index: which shard of an accounting domain's counters this
//!   thread charges (D-691), exclusive to it when it runs fibers and one
//!   is free (GAP(single-writer-shards));
//! - the host slot: what a thread that runs no fiber charges and counts
//!   (its current accounting domain and scratch counts), M1's per-thread
//!   behavior for host code;
//! - the allocator's free worklist, block cache and quarantine, which stay
//!   per thread. The fiber current on a thread also holds the address of
//!   that thread's block cache in its control block, recorded by
//!   [`set_current_fiber`] at every switch-in and cleared at switch-out,
//!   so the allocation paths that know their fiber skip an accessor call
//!   ([`with_cache_of`], [`with_fiber_and_cache`]; GATE1-1). The address
//!   is used only while that fiber is current here, so it never names
//!   another thread's cache.
//!
//! A source test checks the whole crate for thread-locals declared or
//! accessed elsewhere.

use std::cell::{Cell, RefCell};
use std::hint::black_box;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::abi::Word;
use crate::alloc::{BlockCache, Domain, Quarantine};
use crate::ctx::ExecCtx;

/// What a thread that runs no fiber charges and counts: host code, such as
/// a driver freeing an entry's result or a unit test.
#[derive(Debug)]
pub(crate) struct Host {
    /// The thread's current accounting domain, created on first use.
    pub(crate) domain: RefCell<Option<Arc<Domain>>>,
    /// The bytes of scratch capacity live on the thread.
    pub(crate) scratch_live: Cell<u64>,
    /// The most bytes of scratch live on the thread since the last reset.
    pub(crate) scratch_peak: Cell<u64>,
}

/// A shard index not assigned yet.
const NO_SHARD: usize = usize::MAX;

/// The shard of accounting domains a thread charges (D-691), and whether it
/// tried to claim an exclusive one. An index below `alloc::shard_count()`
/// is exclusive: this thread holds that bit of [`EXCLUSIVE`] and is the
/// only thread that charges that shard of any domain, so it charges it
/// with plain relaxed stores (GAP(single-writer-shards)). Any other index
/// is shared and charged with atomic adds. The exclusive bit is given
/// back when the thread ends.
#[derive(Debug)]
struct ShardSlot {
    index: Cell<usize>,
    tried_exclusive: Cell<bool>,
}

impl Drop for ShardSlot {
    fn drop(&mut self) {
        let index = self.index.get();
        if index < crate::alloc::shard_count() {
            // Release: this thread's last charges to the shard happen
            // before the next holder's first (`claim_exclusive`).
            EXCLUSIVE.fetch_and(!exclusive_bit(index), Ordering::Release);
        }
    }
}

/// The exclusive shards held by live threads, one bit per index below
/// `alloc::shard_count()` (at most 64).
static EXCLUSIVE: AtomicU64 = AtomicU64::new(0);

/// The bit of [`EXCLUSIVE`] for the exclusive shard `index` (below 64).
fn exclusive_bit(index: usize) -> u64 {
    u32::try_from(index)
        .ok()
        .and_then(|shift| 1_u64.checked_shl(shift))
        .unwrap_or(0)
}

/// Claims a free exclusive shard for this thread, or `None` when every one
/// is held. Acquire: the previous holder's charges to it happen before
/// this thread's (its `ShardSlot::drop`).
fn claim_exclusive() -> Option<usize> {
    let count = crate::alloc::shard_count();
    let all = u64::MAX >> 64_usize.saturating_sub(count).min(63);
    let mut held = EXCLUSIVE.load(Ordering::Relaxed);
    loop {
        let free = !held & all;
        if free == 0 {
            return None;
        }
        let bit = free.isolate_lowest_one();
        match EXCLUSIVE.compare_exchange_weak(
            held,
            held | bit,
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => return usize::try_from(bit.trailing_zeros()).ok(),
            Err(now) => held = now,
        }
    }
}

/// A new shared shard index: past the exclusive ones, distinct per call;
/// a domain reduces it to one of its shared shards.
fn next_shared_shard() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    // Below `NO_SHARD`, whatever the count.
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed) & (usize::MAX >> 2);
    crate::alloc::shard_count().saturating_add(sequence)
}

thread_local! {
    /// The control block of the fiber running on this thread, or null.
    static CURRENT_FIBER: Cell<*mut ExecCtx> = const { Cell::new(ptr::null_mut()) };
    /// The shard of accounting domains this thread charges (D-691).
    static SHARD: ShardSlot = const {
        ShardSlot {
            index: Cell::new(NO_SHARD),
            tried_exclusive: Cell::new(false),
        }
    };
    /// The host slot.
    static HOST: Host = const {
        Host {
            domain: RefCell::new(None),
            scratch_live: Cell::new(0),
            scratch_peak: Cell::new(0),
        }
    };
    /// The worklist of objects whose count reached zero, kept per thread
    /// so that its capacity is reused across frees.
    static WORKLIST: Cell<Vec<Word>> = const { Cell::new(Vec::new()) };
    /// Freed blocks of small objects kept for the next allocation.
    static CACHE: RefCell<BlockCache> = const { RefCell::new(BlockCache::new()) };
    /// Poisoned blocks kept out of the allocator.
    static QUARANTINE: RefCell<Quarantine> = const { RefCell::new(Quarantine::new()) };
    /// Tests force hash collisions in maps and sets by masking key hashes.
    #[cfg(test)]
    static HASH_MASK: Cell<u64> = const { Cell::new(u64::MAX) };
}

/// The control block of the fiber running on this thread, or null when the
/// thread runs no fiber (or is being torn down).
#[inline(never)]
pub(crate) fn current_fiber() -> *mut ExecCtx {
    black_box(CURRENT_FIBER.try_with(Cell::get).unwrap_or(ptr::null_mut()))
}

/// Makes `fiber` the fiber running on this thread and returns the previous
/// one. A fiber that becomes current charges this thread's shard of the
/// accounting domains (D-691) and records the address of this thread's
/// block cache, which the allocator then reads from the control block
/// instead of the thread-local ([`with_cache_of`]); the previous fiber's
/// record is cleared, so a control block holds the address only while it
/// is current here. The address is recomputed at every call, so a fiber
/// that migrates holds its new thread's cache from its first instruction
/// there (D-686). Reading this recorded address from the control block is
/// allowed (D-739): the hazard D-686 names (an address reused after a
/// switch to another thread) cannot arise, because the record is
/// rewritten at every switch-in and the readers never suspend.
///
/// # Safety
///
/// `fiber` is null or a live control block that stays live, and is used
/// only by this thread's code, until another call replaces it; the
/// previous fiber is still live when this call replaces it.
#[inline(never)]
pub(crate) unsafe fn set_current_fiber(fiber: *mut ExecCtx) -> *mut ExecCtx {
    let previous = CURRENT_FIBER
        .try_with(|cell| cell.replace(fiber))
        .unwrap_or(ptr::null_mut());
    // SAFETY: forwarded from the caller: the previous fiber is live.
    if let Some(ctx) = unsafe { black_box(previous).as_ref() }
        && previous != fiber
    {
        ctx.set_block_cache(ptr::null());
    }
    // SAFETY: forwarded from the caller.
    if let Some(ctx) = unsafe { black_box(fiber).as_ref() } {
        ctx.set_shard(fiber_shard_index());
        let cache = CACHE.try_with(ptr::from_ref).unwrap_or(ptr::null());
        ctx.set_block_cache(black_box(cache));
    }
    black_box(previous)
}

/// The shard of accounting domains this thread charges (D-691). A thread
/// that runs fibers claims an exclusive shard the first time one becomes
/// current on it ([`set_current_fiber`]), while one is free
/// (GAP(single-writer-shards)); any other thread, and a fiber thread
/// beyond the number of shards, gets a shared index the first time it
/// asks, distinct per thread, which a domain reduces to one of its shared
/// shards. During thread teardown, a shared index.
#[inline(never)]
pub(crate) fn shard_index() -> usize {
    SHARD
        .try_with(|slot| {
            let index = slot.index.get();
            if index != NO_SHARD {
                return index;
            }
            let index = next_shared_shard();
            slot.index.set(index);
            index
        })
        .unwrap_or_else(|_| crate::alloc::shard_count())
}

/// [`shard_index`] for a thread a fiber becomes current on: the first time,
/// it claims an exclusive shard if one is free (GAP(single-writer-shards)).
fn fiber_shard_index() -> usize {
    SHARD
        .try_with(|slot| {
            if !slot.tried_exclusive.replace(true)
                && let Some(index) = claim_exclusive()
            {
                slot.index.set(index);
                return index;
            }
            let index = slot.index.get();
            if index != NO_SHARD {
                return index;
            }
            let index = next_shared_shard();
            slot.index.set(index);
            index
        })
        .unwrap_or_else(|_| crate::alloc::shard_count())
}

/// Whether this thread holds the exclusive shard `index`: the condition
/// under which it may charge that shard with plain stores (debug checks).
#[inline(never)]
pub(crate) fn holds_exclusive_shard(index: usize) -> bool {
    index < crate::alloc::shard_count()
        && SHARD
            .try_with(|slot| slot.index.get() == index)
            .unwrap_or(false)
}

/// Calls `f` with this thread's host slot; `None` during thread teardown.
#[inline(never)]
pub(crate) fn with_host<R>(f: impl FnOnce(&Host) -> R) -> Option<R> {
    HOST.try_with(f).ok()
}

/// Calls `f` with this thread's free worklist; `None` during thread
/// teardown. `f` never suspends.
#[inline(never)]
pub(crate) fn with_worklist<R>(f: impl FnOnce(&Cell<Vec<Word>>) -> R) -> Option<R> {
    WORKLIST.try_with(f).ok()
}

/// Calls `f` with this thread's block cache; `None` during thread
/// teardown. `f` never suspends.
#[inline(never)]
pub(crate) fn with_cache<R>(f: impl FnOnce(&RefCell<BlockCache>) -> R) -> Option<R> {
    CACHE.try_with(f).ok()
}

/// Calls `f` with the control block of the fiber running on this thread
/// (null when none) and this thread's block cache, in one accessor call
/// (D-686): the allocator's free and kernel-allocation paths need both.
/// When a fiber runs, the cache's address comes from its control block
/// ([`set_current_fiber`]), so only one thread-local is read. `None`
/// during thread teardown, when the cache is gone. `f` never suspends.
#[inline(never)]
pub(crate) fn with_fiber_and_cache<R>(
    f: impl FnOnce(*mut ExecCtx, &RefCell<BlockCache>) -> R,
) -> Option<R> {
    let fiber = black_box(CURRENT_FIBER.try_with(Cell::get).unwrap_or(ptr::null_mut()));
    // SAFETY: the current fiber is null or a live control block current on
    // this thread (`set_current_fiber`'s contract).
    if let Some(ctx) = unsafe { fiber.as_ref() }
        // SAFETY: a current fiber's block cache is null or this thread's,
        // a thread-local that lives as long as the thread (see
        // `set_current_fiber`).
        && let Some(cache) = unsafe { ctx.block_cache().as_ref() }
    {
        return Some(f(fiber, cache));
    }
    CACHE.try_with(|cache| f(fiber, cache)).ok()
}

/// Calls `f` with this thread's block cache, given `ctx`, the control
/// block of the fiber running on this thread, or `None` for host code.
/// With a fiber the address comes from its control block and no
/// thread-local is read: the allocation path of generated code, which
/// passes its `ctx`, takes a cached block without a `tls.rs` accessor call
/// (GATE1-1). Without one, or when the fiber has no cache recorded, it is
/// [`with_cache`]. `None` during thread teardown. `f` never suspends.
#[inline]
pub(crate) fn with_cache_of<R>(
    ctx: Option<&ExecCtx>,
    f: impl FnOnce(&RefCell<BlockCache>) -> R,
) -> Option<R> {
    if let Some(ctx) = ctx {
        debug_assert!(
            ptr::eq(current_fiber(), ctx),
            "a block cache taken through a fiber that is not current on this thread"
        );
        // SAFETY: `ctx` is the fiber current on this thread (the caller's
        // contract, checked in debug builds), whose block cache is null or
        // this thread's, a thread-local that lives as long as the thread.
        if let Some(cache) = unsafe { ctx.block_cache().as_ref() } {
            return Some(f(cache));
        }
    }
    with_cache(f)
}

/// Calls `f` with this thread's quarantine; `None` during thread teardown.
/// `f` never suspends.
#[inline(never)]
pub(crate) fn with_quarantine<R>(f: impl FnOnce(&RefCell<Quarantine>) -> R) -> Option<R> {
    QUARANTINE.try_with(f).ok()
}

/// The mask applied to key hashes on this thread (tests only).
#[cfg(test)]
#[inline(never)]
pub(crate) fn hash_mask() -> u64 {
    HASH_MASK.try_with(Cell::get).unwrap_or(u64::MAX)
}

/// Replaces the mask applied to key hashes on this thread and returns the
/// previous one (tests only).
#[cfg(test)]
#[inline(never)]
pub(crate) fn set_hash_mask(mask: u64) -> u64 {
    HASH_MASK
        .try_with(|cell| cell.replace(mask))
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// Every `.rs` file under `dir`, sorted.
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// Whether a whole file is test-only code: a file named `tests.rs` or
    /// `*_tests.rs`, or any file under a `tests/` directory. Such files are
    /// compiled only into this crate's unit tests (their `mod` lines are
    /// `#[cfg(test)]`), which run each test on a plain thread and never
    /// switch fibers, so their thread-locals cannot be migrated.
    fn is_test_file(relative: &Path) -> bool {
        let name = relative
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        name == "tests.rs"
            || name.ends_with("_tests.rs")
            || relative
                .parent()
                .is_some_and(|dir| dir.components().any(|part| part.as_os_str() == "tests"))
    }

    /// The part of `source` outside its trailing `#[cfg(test)] mod tests`
    /// block, for the same reason as [`is_test_file`]: the block must start
    /// at column 0 and run to the end of the file, its closing `}` being
    /// the last non-empty line. A file without such a block is returned
    /// whole.
    fn non_test_part(source: &str) -> &str {
        let start = [
            "#[cfg(test)]\nmod tests {\n",
            "#[cfg(test)]\npub(crate) mod tests {\n",
        ]
        .iter()
        .filter_map(|marker| {
            if source.starts_with(marker) {
                return Some(0);
            }
            source
                .rfind(&format!("\n{marker}"))
                .map(|at| at.saturating_add(1))
        })
        .max();
        let Some((before, block)) =
            start.and_then(|at| Some((source.get(..at)?, source.get(at..)?)))
        else {
            return source;
        };
        // The block's first line at column 0 that closes it must be the
        // last non-empty line of the file.
        let lines: Vec<&str> = block.lines().collect();
        let closing = lines.iter().position(|line| *line == "}");
        let last = lines.iter().rposition(|line| !line.trim().is_empty());
        if closing.is_none() || closing != last {
            return source;
        }
        before
    }

    /// D-686: no module but this one declares or accesses a thread-local.
    /// Test-only code is exempt (see [`is_test_file`] and
    /// [`non_test_part`]): today `hamt/tests.rs`,
    /// `alloc/temporaries_tests.rs` and the test modules of `consteval.rs`
    /// and `entry.rs` declare thread-locals.
    #[test]
    fn no_thread_local_outside_tls() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&root, &mut files);
        assert!(
            files.len() > 50,
            "{} files under {}",
            files.len(),
            root.display()
        );
        let mut found = Vec::new();
        let mut exempt = Vec::new();
        for file in &files {
            let relative = file.strip_prefix(&root).unwrap();
            if relative == Path::new("tls.rs") {
                continue;
            }
            let source = std::fs::read_to_string(file).unwrap();
            let checked = if is_test_file(relative) {
                ""
            } else {
                non_test_part(&source)
            };
            let has = |text: &str| {
                ["thread_local!", ".with(|", ".try_with("]
                    .iter()
                    .any(|pattern| text.contains(pattern))
            };
            if has(&source) && !has(checked) {
                exempt.push(relative.display().to_string());
            }
            for (number, line) in checked.lines().enumerate() {
                if has(line) {
                    found.push(format!(
                        "{}:{}: {}",
                        relative.display(),
                        number + 1,
                        line.trim()
                    ));
                }
            }
        }
        assert!(
            found.is_empty(),
            "thread-locals outside tls.rs (D-686):\n{}",
            found.join("\n")
        );
        // The exemptions are the test-only code named above, and the scan
        // does see them.
        exempt.sort();
        assert_eq!(
            exempt,
            [
                "alloc/temporaries_tests.rs",
                "consteval.rs",
                "entry.rs",
                "hamt/tests.rs"
            ]
        );
    }

    #[test]
    fn the_trailing_test_module_is_cut_off() {
        let source = "fn a() {}\n\n#[cfg(test)]\nmod tests {\n    thread_local! {}\n}\n";
        assert_eq!(non_test_part(source), "fn a() {}\n\n");
        // Not trailing: something follows the block.
        let source = "#[cfg(test)]\nmod tests {\n}\nfn b() {}\n";
        assert_eq!(non_test_part(source), source);
        // A test-only item that is not the test module is checked.
        let source = "#[cfg(test)]\nthread_local! {}\n";
        assert_eq!(non_test_part(source), source);
        assert!(is_test_file(Path::new("hamt/tests.rs")));
        assert!(is_test_file(Path::new("alloc/temporaries_tests.rs")));
        assert!(is_test_file(Path::new("generic/tests/kinds.rs")));
        assert!(!is_test_file(Path::new("alloc/scratch.rs")));
    }

    #[test]
    fn threads_get_distinct_shard_indexes() {
        let here = shard_index();
        assert_eq!(shard_index(), here);
        let there = std::thread::spawn(shard_index).join().unwrap();
        assert_ne!(here, there);
    }

    #[test]
    fn the_current_fiber_is_per_thread() {
        assert!(current_fiber().is_null());
        let fiber = std::thread::spawn(|| {
            let ctx = Box::into_raw(Box::new(crate::ctx::ExecCtx::new(crate::ctx::FiberInit {
                entry_mode: crate::entry::EntryMode::Test,
                ..crate::ctx::FiberInit::placeholder()
            })));
            // SAFETY: a live control block, current until replaced below.
            let previous = unsafe { set_current_fiber(ctx) };
            assert!(previous.is_null());
            assert_eq!(current_fiber(), ctx);
            // SAFETY: as above.
            assert_eq!(unsafe { &*ctx }.shard(), shard_index());
            // SAFETY: null is always allowed.
            assert_eq!(unsafe { set_current_fiber(ptr::null_mut()) }, ctx);
            // SAFETY: allocated above and no longer current.
            drop(unsafe { Box::from_raw(ctx) });
            current_fiber().is_null()
        });
        assert!(fiber.join().unwrap());
        assert!(current_fiber().is_null());
    }

    /// GATE1-1: a fiber that becomes current records its thread's block
    /// cache, which `with_cache_of` and `with_fiber_and_cache` then use,
    /// and the record is cleared when it stops being current. On another
    /// thread it records that thread's cache (D-686: a migrated fiber
    /// never uses its old thread's cache).
    #[test]
    fn a_current_fiber_holds_its_threads_block_cache() {
        let new_ctx = || {
            Box::into_raw(Box::new(crate::ctx::ExecCtx::new(crate::ctx::FiberInit {
                entry_mode: crate::entry::EntryMode::Test,
                ..crate::ctx::FiberInit::placeholder()
            })))
        };
        let ctx = new_ctx();
        let other = new_ctx();
        let address = |cache: &RefCell<BlockCache>| ptr::from_ref(cache).addr();
        let here = with_cache(address).unwrap();
        // SAFETY: live control blocks; each is current until replaced, and
        // both are freed at the end, after they stop being current.
        unsafe {
            assert!((*ctx).block_cache().is_null());
            set_current_fiber(ctx);
            assert_eq!((*ctx).block_cache().addr(), here);
            assert_eq!(with_cache_of(Some(&*ctx), address), Some(here));
            assert_eq!(
                with_fiber_and_cache(|fiber, cache| (fiber, address(cache))),
                Some((ctx, here))
            );
            // Replacing it clears its record.
            set_current_fiber(other);
            assert!((*ctx).block_cache().is_null());
            assert_eq!((*other).block_cache().addr(), here);
            set_current_fiber(ptr::null_mut());
            assert!((*other).block_cache().is_null());
            // Host code takes the thread-local.
            assert_eq!(with_cache_of(None, address), Some(here));
        }
        let ctx_word = ctx.expose_provenance();
        let there = std::thread::spawn(move || {
            let ctx = ptr::with_exposed_provenance_mut::<crate::ctx::ExecCtx>(ctx_word);
            let there = with_cache(address).unwrap();
            // SAFETY: as above; current on this thread until cleared.
            unsafe {
                set_current_fiber(ctx);
                let recorded = (*ctx).block_cache().addr();
                set_current_fiber(ptr::null_mut());
                (there, recorded)
            }
        })
        .join()
        .unwrap();
        assert_eq!(there.0, there.1);
        assert_ne!(there.0, here);
        // SAFETY: allocated above and current nowhere.
        unsafe {
            drop(Box::from_raw(ctx));
            drop(Box::from_raw(other));
        }
    }
}
