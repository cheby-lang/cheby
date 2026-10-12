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
//!   thread charges (D-691);
//! - the host slot: what a thread that runs no fiber charges and counts
//!   (its current accounting domain and scratch counts), M1's per-thread
//!   behavior for host code;
//! - the allocator's free worklist, block cache and quarantine, which stay
//!   per thread.
//!
//! A source test checks the whole crate for thread-locals declared or
//! accessed elsewhere.

use std::cell::{Cell, RefCell};
use std::hint::black_box;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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

thread_local! {
    /// The control block of the fiber running on this thread, or null.
    static CURRENT_FIBER: Cell<*mut ExecCtx> = const { Cell::new(ptr::null_mut()) };
    /// The shard of accounting domains this thread charges (D-691).
    static SHARD: Cell<usize> = const { Cell::new(NO_SHARD) };
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
/// accounting domains (D-691).
///
/// # Safety
///
/// `fiber` is null or a live control block that stays live, and is used
/// only by this thread's code, until another call replaces it.
#[inline(never)]
pub(crate) unsafe fn set_current_fiber(fiber: *mut ExecCtx) -> *mut ExecCtx {
    let previous = CURRENT_FIBER
        .try_with(|cell| cell.replace(fiber))
        .unwrap_or(ptr::null_mut());
    // SAFETY: forwarded from the caller.
    if let Some(ctx) = unsafe { black_box(fiber).as_ref() } {
        ctx.set_shard(shard_index());
    }
    black_box(previous)
}

/// The shard of accounting domains this thread charges (D-691). Each
/// thread gets the next index the first time it asks; a domain reduces it
/// modulo its number of shards.
#[inline(never)]
pub(crate) fn shard_index() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    SHARD
        .try_with(|cell| {
            let index = cell.get();
            if index != NO_SHARD {
                return index;
            }
            // Below `NO_SHARD`, whatever the count.
            let index = NEXT.fetch_add(1, Ordering::Relaxed) & (usize::MAX >> 1);
            cell.set(index);
            index
        })
        .unwrap_or(0)
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
/// `None` during thread teardown, when the cache is gone. `f` never
/// suspends.
#[inline(never)]
pub(crate) fn with_fiber_and_cache<R>(
    f: impl FnOnce(*mut ExecCtx, &RefCell<BlockCache>) -> R,
) -> Option<R> {
    CACHE
        .try_with(|cache| {
            let fiber = black_box(CURRENT_FIBER.try_with(Cell::get).unwrap_or(ptr::null_mut()));
            f(fiber, cache)
        })
        .ok()
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
}
