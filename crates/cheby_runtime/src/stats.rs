//! Runtime counters per accounting domain: allocations, and which path the
//! in-place operations of F2-F4 took (D-274, D-316, D-525).
//!
//! Every [`crate::alloc::Domain`] holds a [`Stats`]. The operations that
//! may update a value in place record an [`Event`] in the current
//! domain (the running fiber's, or a host thread's): whether a list push, a list update, a string append,
//! a string or bytes kernel, or a map or set update changed its unique
//! argument in place, grew it (unique, but out of room) or copied it
//! (shared). Tests read the counters of their own domain, so they may run
//! in parallel, and `cheby run` prints them after `main` when
//! `CHEBY_RT_STATS=1` is set ([`report`]), which is how the in-place paths
//! are shown to be hit in the examples. The counters are a developer
//! tool, not program behavior: nothing a program observes depends on them.

use std::fmt::Write as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crossbeam_utils::CachePadded;

/// An event the runtime counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A heap object was allocated.
    Alloc,
    /// `ListPush` or a push kernel added to a unique list in place.
    ListPushInPlace,
    /// A push copied the root of a shared list.
    ListPushCopied,
    /// Any list root update (push, prepend, set, slice) was in place.
    ListUpdateInPlace,
    /// A list root update made a new root for a shared list.
    ListUpdateCopied,
    /// `cheby_rt_str_append` (interpolation) appended in place.
    StrAppendInPlace,
    /// It grew a unique accumulator that was out of room (doubling).
    StrAppendGrown,
    /// It copied a shared accumulator.
    StrAppendCopied,
    /// A `String` or `Bytes` kernel extended its unique argument in place.
    TextKernelInPlace,
    /// It grew a unique argument that was out of room.
    TextKernelGrown,
    /// It copied a shared (or aliased) argument.
    TextKernelCopied,
    /// A map or set insert, upsert or remove changed a unique root in place.
    MapUpdateInPlace,
    /// It made a new root for a shared map or set.
    MapUpdateCopied,
}

impl Event {
    /// Every event, in report order.
    pub const ALL: [Self; 13] = [
        Self::Alloc,
        Self::ListPushInPlace,
        Self::ListPushCopied,
        Self::ListUpdateInPlace,
        Self::ListUpdateCopied,
        Self::StrAppendInPlace,
        Self::StrAppendGrown,
        Self::StrAppendCopied,
        Self::TextKernelInPlace,
        Self::TextKernelGrown,
        Self::TextKernelCopied,
        Self::MapUpdateInPlace,
        Self::MapUpdateCopied,
    ];

    /// The event's name in reports.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Alloc => "allocations",
            Self::ListPushInPlace => "list push in place",
            Self::ListPushCopied => "list push copied",
            Self::ListUpdateInPlace => "list update in place",
            Self::ListUpdateCopied => "list update copied",
            Self::StrAppendInPlace => "string append in place",
            Self::StrAppendGrown => "string append grown",
            Self::StrAppendCopied => "string append copied",
            Self::TextKernelInPlace => "string/bytes kernel in place",
            Self::TextKernelGrown => "string/bytes kernel grown",
            Self::TextKernelCopied => "string/bytes kernel copied",
            Self::MapUpdateInPlace => "map/set update in place",
            Self::MapUpdateCopied => "map/set update copied",
        }
    }

    /// The event's index in [`Event::ALL`].
    #[expect(
        clippy::as_conversions,
        reason = "the discriminant of a fieldless enum is its index"
    )]
    const fn index(self) -> usize {
        self as usize
    }
}

/// The counters of one accounting domain, sharded like the domain's own
/// (D-691): an event is counted in the shard of the thread that records
/// it, and reading sums the shards.
#[derive(Debug)]
pub struct Stats {
    shards: Box<[CachePadded<[AtomicU64; Event::ALL.len()]>]>,
    entries: AtomicU64,
    entry_nanos: AtomicU64,
    /// Debug leak checks skipped because the system allocator refused a
    /// request of the domain (GAP(system-refusal-leak)).
    leak_checks_skipped: AtomicU64,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            shards: (0..crate::alloc::shard_count())
                .map(|_| CachePadded::new(std::array::from_fn(|_| AtomicU64::new(0))))
                .collect(),
            entries: AtomicU64::new(0),
            entry_nanos: AtomicU64::new(0),
            leak_checks_skipped: AtomicU64::new(0),
        }
    }
}

impl Stats {
    /// Adds one to `event`'s counter in shard `shard` (reduced modulo the
    /// number of shards, a power of two).
    pub(crate) fn record(&self, shard: usize, event: Event) {
        let mask = self.shards.len().saturating_sub(1);
        if let Some(count) = self
            .shards
            .get(shard & mask)
            .and_then(|counts| counts.get(event.index()))
        {
            count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Counts one entry (a constant, `main`, a test, a REPL entry) that ran
    /// for `time`, from its start to its outcome.
    pub(crate) fn add_entry(&self, time: Duration) {
        self.entries.fetch_add(1, Ordering::Relaxed);
        let nanos = u64::try_from(time.as_nanos()).unwrap_or(u64::MAX);
        self.entry_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    /// The number of entries run in the domain, and their total time.
    #[must_use]
    pub fn entries(&self) -> (u64, Duration) {
        (
            self.entries.load(Ordering::Relaxed),
            Duration::from_nanos(self.entry_nanos.load(Ordering::Relaxed)),
        )
    }

    /// Counts a debug leak check that was skipped (GAP(system-refusal-leak)).
    pub(crate) fn skip_leak_check(&self) {
        self.leak_checks_skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// How many debug leak checks of the domain were skipped.
    #[must_use]
    pub fn leak_checks_skipped(&self) -> u64 {
        self.leak_checks_skipped.load(Ordering::Relaxed)
    }

    /// How often `event` happened.
    #[must_use]
    pub fn count(&self, event: Event) -> u64 {
        self.shards
            .iter()
            .filter_map(|counts| counts.get(event.index()))
            .map(|count| count.load(Ordering::Relaxed))
            .fold(0, u64::wrapping_add)
    }
}

/// Records `event` in the current accounting domain: the running fiber's,
/// or a host thread's.
pub(crate) fn record(event: Event) {
    crate::alloc::record_event(event);
}

/// Whether `CHEBY_RT_STATS=1` asks `cheby run` for the counters (read once
/// per process).
#[must_use]
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CHEBY_RT_STATS").is_some_and(|value| value == "1"))
}

/// A duration in milliseconds with three decimals.
fn millis(time: Duration) -> String {
    let micros = time.as_micros();
    format!("{}.{:03} ms", micros / 1000, micros % 1000)
}

/// The report `cheby run` prints to standard error after `main` when
/// [`enabled`]: `main`'s wall time, the number and total wall time of the
/// build's entries (its constants and `main`), the peak of live bytes,
/// every counter of `stats`, one per line, and the leak checks skipped.
#[must_use]
pub fn report(main_time: Duration, peak_bytes: i64, stats: &Stats) -> String {
    let mut out = String::new();
    let (entries, entry_time) = stats.entries();
    let _ = writeln!(
        out,
        "runtime stats: main {}, {entries} entries {}, peak {peak_bytes} bytes",
        millis(main_time),
        millis(entry_time)
    );
    for event in Event::ALL {
        let _ = writeln!(out, "  {}: {}", event.name(), stats.count(event));
    }
    let _ = writeln!(
        out,
        "  leak checks skipped: {}",
        stats.leak_checks_skipped()
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_listed_in_index_order() {
        for (i, event) in Event::ALL.into_iter().enumerate() {
            assert_eq!(event.index(), i, "{event:?}");
        }
    }

    #[test]
    fn records_go_to_the_current_domain() {
        let domain = crate::alloc::Domain::new();
        crate::alloc::with_domain(std::sync::Arc::clone(&domain), || {
            record(Event::ListPushInPlace);
            record(Event::ListPushInPlace);
            record(Event::MapUpdateCopied);
        });
        let stats = domain.stats();
        assert_eq!(stats.count(Event::ListPushInPlace), 2);
        assert_eq!(stats.count(Event::MapUpdateCopied), 1);
        assert_eq!(stats.count(Event::Alloc), 0);
    }

    /// The in-place paths of string append (interpolation), the string
    /// kernels and map insert are counted as such, and their shared paths
    /// as copies (the list paths are counted in `list::tests`).
    #[test]
    fn string_and_map_updates_are_counted_by_path() {
        use crate::alloc::{current_domain, live};
        use crate::helpers::{cheby_rt_str_append, cheby_rt_str_with_capacity};
        use crate::{rc, values};
        let domain = current_domain();
        let count = |event| domain.stats().count(event);
        let before = live();
        let start: Vec<u64> = Event::ALL.iter().map(|event| count(*event)).collect();
        let delta = |event: Event| {
            let index = Event::ALL.iter().position(|e| *e == event).unwrap();
            count(event) - start[index]
        };
        let null = std::ptr::null_mut();
        let piece = values::string("ab");
        // SAFETY (every block below): live strings with the ownership each
        // helper or kernel documents.
        unsafe {
            let acc = cheby_rt_str_with_capacity(null, 4);
            let acc = cheby_rt_str_append(null, acc, piece);
            let acc = cheby_rt_str_append(null, acc, piece);
            assert_eq!(delta(Event::StrAppendInPlace), 2);
            let acc = cheby_rt_str_append(null, acc, piece);
            assert_eq!(delta(Event::StrAppendGrown), 1);
            rc::dup(acc);
            let other = cheby_rt_str_append(null, acc, piece);
            assert_eq!(delta(Event::StrAppendCopied), 1);
            assert_eq!(values::str_of(other), "abababab");
            assert_eq!(values::str_of(acc), "ababab");
            rc::drop(other);
            // A kernel on a unique argument with room, then on a shared one.
            let text = cheby_rt_str_with_capacity(null, 16);
            let text = cheby_rt_str_append(null, text, piece);
            let longer = crate::text::kernels::cheby_string_add(text, piece);
            assert_eq!(longer, text, "the unique argument is reused");
            rc::drop(text);
            assert_eq!(delta(Event::TextKernelInPlace), 1);
            rc::dup(acc);
            let copy = crate::text::kernels::cheby_string_add(acc, piece);
            assert_eq!(delta(Event::TextKernelCopied), 1);
            assert_ne!(copy, acc);
            rc::drop(copy);
            rc::drop(longer);
            rc::drop(acc);
            rc::drop(acc);
            rc::drop(piece);
            // Map insert: the empty map, then a unique one, then a shared one.
            let key = |n| values::int_word(n);
            let m = crate::hamt::kernels::cheby_map_insert(
                crate::hamt::kernels::cheby_map_new(),
                key(1),
                key(10),
            );
            let m2 = crate::hamt::kernels::cheby_map_insert(m, key(2), key(20));
            assert_eq!(m2, m, "the unique map is reused");
            rc::drop(m);
            assert_eq!(delta(Event::MapUpdateInPlace), 1);
            rc::dup(m2);
            let m3 = crate::hamt::kernels::cheby_map_insert(m2, key(3), key(30));
            assert_ne!(m3, m2);
            assert_eq!(delta(Event::MapUpdateCopied), 1);
            rc::drop(m3);
            rc::drop(m2);
            rc::drop(m2);
        }
        assert_eq!(live(), before);
        assert!(delta(Event::Alloc) > 0);
    }

    #[test]
    fn the_report_lists_every_counter() {
        let stats = Stats::default();
        stats.record(0, Event::StrAppendGrown);
        stats.add_entry(Duration::from_micros(2_000));
        stats.add_entry(Duration::from_micros(12_345));
        let text = report(Duration::from_micros(12_345), 640, &stats);
        assert!(
            text.starts_with(
                "runtime stats: main 12.345 ms, 2 entries 14.345 ms, peak 640 bytes\n"
            ),
            "{text}"
        );
        assert!(text.contains("  string append grown: 1\n"), "{text}");
        assert!(text.ends_with("  leak checks skipped: 0\n"), "{text}");
        stats.skip_leak_check();
        let text = report(Duration::ZERO, 0, &stats);
        assert!(text.ends_with("  leak checks skipped: 1\n"), "{text}");
        assert_eq!(text.lines().count(), 2 + Event::ALL.len());
    }
}
