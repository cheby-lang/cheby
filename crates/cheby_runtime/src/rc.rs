//! `dup` and `drop` (D-273): the reference-count operations generated code
//! calls. Both skip tagged integers with one bit test (D-278) and static
//! objects (count 0, §9.7) without writing them.
//!
//! A positive count is local to its fiber and updated with plain-cost
//! relaxed accesses. A negative count `-n` is shared (M2, D-072): `n`
//! references, updated atomically. In M1 no object is ever shared, but the
//! paths exist so that the counting rules are fixed from the start.

use std::sync::atomic::Ordering;
#[cfg(not(cheby_sanitize = "thread"))]
use std::sync::atomic::fence;

use crate::abi::{self, Word};
use crate::alloc;
use crate::ctx::ExecCtx;
use crate::object::{load_rc, ptr_to_word, shared_rc, store_rc, word_to_ptr};

/// Adds a reference to `value`.
///
/// # Safety
///
/// `value` is a tagged integer or a word pointing to a live object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_dup(value: Word) {
    // SAFETY: forwarded from the caller.
    unsafe { dup(value) };
}

/// Removes a reference to `value`, freeing it when none remain.
///
/// `ctx` is the running fiber's control block, which generated code always
/// passes (D-772): a free then reads the block cache through
/// the control block's record and charges `ctx`'s domain, with no
/// thread-local lookup (D-739). It may be null, and then the free looks
/// the running fiber up, as [`drop`] does.
///
/// # Safety
///
/// `ctx` is null or the control block of the fiber running on this
/// thread; `value` is a tagged integer or a word pointing to a live
/// object, and the caller owns the reference it gives up.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_drop(ctx: *mut ExecCtx, value: Word) {
    if abi::is_tagged_int(value) || value == 0 {
        return;
    }
    // SAFETY: forwarded from the caller.
    unsafe { alloc::check_live(value, "`drop`") };
    let obj = word_to_ptr(value);
    // SAFETY: `obj` is a live object and the caller gives up a reference.
    if unsafe { release(obj) } {
        // SAFETY: that was the last reference; `ctx` is null or the
        // running fiber's control block (forwarded from the caller).
        unsafe { alloc::free_in(ctx.as_ref(), obj) };
    }
}

/// Adds a reference to `value`. See [`cheby_rt_dup`]. The common case, a
/// local count below the maximum, is inlined; the rest is `dup_slow`.
///
/// # Safety
///
/// As for [`cheby_rt_dup`].
#[inline]
pub unsafe fn dup(value: Word) {
    if abi::is_tagged_int(value) || value == 0 {
        return;
    }
    // SAFETY: forwarded from the caller.
    unsafe { alloc::check_live(value, "`dup`") };
    let obj = word_to_ptr(value);
    // SAFETY: `obj` is a live object.
    let rc = unsafe { load_rc(obj) };
    if let Some(next) = rc.checked_add(1).filter(|_| rc > 0) {
        // SAFETY: a positive count is local to this thread.
        unsafe { store_rc(obj, next) };
        return;
    }
    // SAFETY: as above.
    unsafe { dup_slow(obj, rc) };
}

/// [`dup`] of an object whose count `rc` is static, shared or at its
/// maximum.
///
/// # Safety
///
/// `obj` points to a live object whose count is `rc`.
#[inline(never)]
unsafe fn dup_slow(obj: *mut u8, rc: i32) {
    if rc > 0 {
        // The count cannot grow further: the object becomes static and is
        // never freed (D-454). `mark_shared` skips a static object and
        // everything it reaches, so what this one reaches is made shared
        // first: a static object reaches only static or shared objects,
        // as a shared one does (I-2), and a value reaching it may cross
        // to another fiber (D-753).
        //
        // `cheby_rt_dup` has no unwind edge, so this walk must not raise:
        // its worklist is not charged to the memory budget, and a system
        // refusal while it grows ends the process (D-765).
        let list = alloc::scratch::ScratchVec::unbudgeted(OVERFLOW_WALK_REFUSED);
        // SAFETY: a live object whose count is positive, so it and every
        // object it reaches with a positive count are confined to this
        // fiber (I-1); marking stores `-rc` into it, which the next store
        // replaces.
        unsafe { mark_with(ptr_to_word(obj), list) };
        // SAFETY: a live object, now shared, that only this fiber has
        // touched since it was local; no other fiber can reach it before
        // this dup returns.
        unsafe {
            store_rc(obj, abi::RC_STATIC);
            alloc::leave_domain(obj);
        }
    } else if rc < 0 {
        // SAFETY: a negative count marks a live shared heap object.
        let cell = unsafe { shared_rc(obj) };
        let update = cell.try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |current| match current {
                abi::RC_STATIC => None,
                // Overflow: the object becomes static.
                i32::MIN => Some(abi::RC_STATIC),
                _ => current.checked_sub(1),
            },
        );
        if update == Ok(i32::MIN) {
            // SAFETY: the object just became static.
            unsafe { alloc::leave_domain(obj) };
        }
    }
}

/// Removes a reference to `value`, freeing it when none remain: the Rust
/// callers' [`cheby_rt_drop`], which looks the running fiber up.
///
/// # Safety
///
/// `value` is a tagged integer or a word pointing to a live object, and the
/// caller owns the reference it gives up.
#[inline]
pub unsafe fn drop(value: Word) {
    if abi::is_tagged_int(value) || value == 0 {
        return;
    }
    // SAFETY: forwarded from the caller.
    unsafe { alloc::check_live(value, "`drop`") };
    let obj = word_to_ptr(value);
    // SAFETY: `obj` is a live object and the caller gives up a reference.
    if unsafe { release(obj) } {
        // SAFETY: that was the last reference.
        unsafe { alloc::free(obj) };
    }
}

/// Whether `value` is a heap object with exactly one reference (a local
/// count of 1), so its owner may update it in place (D-316, D-525). Never
/// true for tagged integers, 0, static objects or shared objects.
///
/// # Safety
///
/// `value` is a tagged integer, 0, or a word pointing to a live object.
#[must_use]
pub unsafe fn is_unique(value: Word) -> bool {
    if abi::is_tagged_int(value) || value == 0 {
        return false;
    }
    // SAFETY: forwarded from the caller.
    unsafe { alloc::check_live(value, "a uniqueness check") };
    // SAFETY: as above.
    unsafe { load_rc(word_to_ptr(value)) == abi::RC_UNIQUE }
}

/// The count in `value`'s header: positive and local to one fiber, 0 for
/// a static object, negative once shared (D-072, D-705); `None` for a
/// tagged integer or 0. A diagnostic for tests, never part of a program's
/// behavior: the REPL's check that a binding is marked shared (D-712).
///
/// # Safety
///
/// `value` is a tagged integer, 0, or a word pointing to a live object.
#[doc(hidden)]
#[must_use]
pub unsafe fn count_of(value: Word) -> Option<i32> {
    if abi::is_tagged_int(value) || value == 0 {
        return None;
    }
    // SAFETY: forwarded from the caller.
    unsafe { alloc::check_live(value, "reading a count") };
    // SAFETY: as above.
    Some(unsafe { load_rc(word_to_ptr(value)) })
}

/// Marks `word` and everything it reaches shared (D-072, D-705): each heap
/// object with a positive, fiber-local count `c` gets the count `-c`, so
/// from then on its references are counted atomically and it is never
/// updated in place. Static objects (count 0) and objects that are
/// already shared are skipped, and so is what they reach: a shared
/// object only reaches shared or static ones (I-2), and so does a static
/// one. Frozen constants are static all the way down, and a heap object
/// that becomes static by count overflow (D-454) is marked shared first
/// (`dup_slow`).
///
/// It runs where a value reaches another fiber: a value sent on a
/// channel, the function a fiber is spawned with, a fiber's result, and a
/// REPL binding. Sharing is sticky in M2. The walk is a worklist of
/// scratch memory, so a million-deep value is marked without deep Rust
/// recursion (plan §0), and the scratch counts towards constant
/// evaluation's memory budget like every walk's. The one exception is the
/// walk of a count overflow (`dup_slow`), which runs the same steps on an
/// unbudgeted worklist (D-765).
///
/// The walk marks an object only after everything it reaches is marked
/// (post-order), so the invariant it relies on, "a count `<= 0` means
/// static, or shared together with everything it reaches", holds at every
/// step. Growing the worklist can raise `out of memory` (D-642), which an
/// unwinding backend lets a program catch and retry (D-697): the raise
/// then leaves some objects shared, each with all it reaches, and the
/// value itself unmarked, so a retry enters it again and completes the
/// walk (review finding MEM-1: marking before pushing the children left a
/// shared root above local counts, which a retry skipped). Values are
/// immutable and so acyclic: an object is entered once, since a second
/// entry still on the list sits below the first one's mark and finds it
/// marked.
///
/// # Safety
///
/// `word` is a tagged integer, 0, or a live object, and every object it
/// reaches with a positive count is confined to the calling fiber (no
/// other thread reads or writes its count meanwhile).
pub unsafe fn mark_shared(word: Word) {
    // SAFETY: forwarded from the caller.
    unsafe { mark_with(word, alloc::scratch::ScratchVec::new()) };
}

/// What ends the process when the system refuses the worklist of a count
/// overflow's walk (D-765), after `fatal runtime error: `.
const OVERFLOW_WALK_REFUSED: &str =
    "out of memory while marking a value shared after its count overflowed";

/// One step of [`mark_shared`]'s walk.
enum Step {
    /// Enter the value: push its mark, then its children.
    Enter(Word),
    /// Everything the object reaches is marked: mark it.
    Mark(Word),
}

/// [`mark_shared`] with `list`, an empty worklist, whose growth is
/// accounted for as the caller chose: charged to the budget, or not
/// (`dup_slow`, D-765).
///
/// # Safety
///
/// As for [`mark_shared`].
unsafe fn mark_with(word: Word, mut list: alloc::scratch::ScratchVec<Step>) {
    list.push(Step::Enter(word));
    while let Some(step) = list.pop() {
        match step {
            Step::Enter(word) => {
                if abi::is_tagged_int(word) || word == 0 {
                    continue;
                }
                // SAFETY: every word on the list is reached from the
                // caller's live value; in poisoning mode a freed one ends
                // the process here.
                unsafe { alloc::check_live(word, "marking a value shared") };
                // SAFETY: a live object.
                if unsafe { load_rc(word_to_ptr(word)) } <= 0 {
                    // Static, or shared together with everything it
                    // reaches.
                    continue;
                }
                list.push(Step::Mark(word));
                // SAFETY: a live object.
                unsafe {
                    alloc::for_each_child(word, &mut |child| list.push(Step::Enter(child)));
                }
            }
            Step::Mark(word) => {
                let obj = word_to_ptr(word);
                // SAFETY: a live object, entered with a positive count
                // that only this fiber touches (the caller's guarantee),
                // so a plain load and store are enough; marking its
                // children left its own count as it was, and `-rc` cannot
                // overflow for a positive `rc`.
                unsafe {
                    let rc = load_rc(obj);
                    store_rc(obj, rc.saturating_neg());
                }
            }
        }
    }
}

/// Removes one reference from an object, and returns whether that was the
/// last one, in which case the caller must free it.
///
/// # Safety
///
/// `obj` points to a live object and the caller gives up a reference it
/// owns.
#[inline]
pub(crate) unsafe fn release(obj: *mut u8) -> bool {
    // SAFETY: forwarded from the caller.
    let rc = unsafe { load_rc(obj) };
    match rc {
        abi::RC_STATIC => false,
        1 => true,
        2.. => {
            // SAFETY: a positive count is local to this thread.
            unsafe { store_rc(obj, rc.saturating_sub(1)) };
            false
        }
        _ => {
            // SAFETY: a negative count marks a live shared heap object.
            let previous = unsafe { shared_rc(obj) }.fetch_add(1, Ordering::Release);
            if previous == -1 {
                // Every other thread's last use happens before the free.
                #[cfg(not(cheby_sanitize = "thread"))]
                fence(Ordering::Acquire);
                // D-879: ThreadSanitizer does not model
                // a standalone fence, so its build acquires with a load of
                // the count instead, as std's `Arc` does.
                #[cfg(cheby_sanitize = "thread")]
                // SAFETY: as above; the object is not freed yet.
                let _ = unsafe { shared_rc(obj) }.load(Ordering::Acquire);
                true
            } else {
                false
            }
        }
    }
}

/// `mark_shared` (D-705): its own module, so that
/// `cargo test -p cheby_runtime rc::mark_shared` selects these tests.
#[cfg(test)]
mod mark_shared_tests {
    use std::collections::{BTreeSet, HashSet};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use super::*;
    use crate::abi::layout as ids;
    use crate::alloc::{Domain, alloc_object, for_each_child, poison_enabled, with_domain};
    use crate::handles::tests::{KINDS, counted, selector};
    use crate::handles::{HandleKind, SELECTOR_WORDS};
    use crate::layout::{self, FieldKind, LayoutInfo};
    use crate::object::{self, scan_and_layout, write_field};
    use crate::values::{bytes, f32_word, f64_word, int_word, string, tuple, u64_word};

    fn release(words: impl IntoIterator<Item = Word>) {
        for word in words {
            // SAFETY: tests release the references they own.
            unsafe { drop(word) };
        }
    }

    fn count(word: Word) -> i32 {
        // SAFETY: tests pass live objects.
        unsafe { load_rc(word_to_ptr(word)) }
    }

    /// Every distinct object reachable from `word`: its count and its
    /// layout's name. Iterative, like the code under test.
    fn reachable(word: Word) -> Vec<(i32, String)> {
        let mut seen = HashSet::new();
        let mut list = vec![word];
        let mut out = Vec::new();
        while let Some(word) = list.pop() {
            if abi::is_tagged_int(word) || word == 0 || !seen.insert(word) {
                continue;
            }
            let obj = word_to_ptr(word);
            // SAFETY: a live object reached from the test's value.
            let (rc, (_, id)) = unsafe { (load_rc(obj), scan_and_layout(obj)) };
            out.push((rc, layout::get(id).unwrap().name.to_string()));
            // SAFETY: as above.
            unsafe { for_each_child(word, &mut |child| list.push(child)) };
        }
        out
    }

    /// A list of owned items.
    fn list(items: Vec<Word>) -> Word {
        crate::list::from_iter(items)
    }

    /// A user constructor `t.Pair(value, Int)` holding `value` (owned).
    fn user_ctor(value: Word) -> Word {
        let id = layout::register(LayoutInfo::ctor(
            "t.Pair",
            0,
            &[(FieldKind::Value, None), (FieldKind::Int, None)],
        ))
        .unwrap();
        let obj = alloc_object(24, abi::header(1, 1, id));
        // SAFETY: a fresh object: the value field, then a raw `Int` that
        // looks like a pointer and must not be followed.
        unsafe {
            write_field(obj, 8, value);
            write_field(obj, 16, 0x1000);
        }
        object::ptr_to_word(obj)
    }

    /// An object of a uniform layout `id` holding `fields` (owned).
    fn uniform(id: abi::LayoutId, fields: &[Word]) -> Word {
        let count = fields.len();
        let obj = alloc_object(
            object::to_u64(count.saturating_add(1).saturating_mul(8)),
            abi::header(1, u8::try_from(count).unwrap(), id),
        );
        for (index, field) in fields.iter().enumerate() {
            // SAFETY: initializing a fresh object's uniform fields.
            unsafe { write_field(obj, index.saturating_add(1).saturating_mul(8), *field) };
        }
        object::ptr_to_word(obj)
    }

    /// `map::from_list` of owned pairs.
    fn map_of(pairs: Vec<(Word, Word)>) -> Word {
        let pairs = list(pairs.into_iter().map(|(k, v)| tuple(&[k, v])).collect());
        // SAFETY: the kernel borrows its list.
        let map = unsafe { crate::hamt::cheby_map_from_list(pairs) };
        release([pairs]);
        map
    }

    /// `set::from_list` of owned items.
    fn set_of(items: Vec<Word>) -> Word {
        let items = list(items);
        // SAFETY: the kernel borrows its list.
        let set = unsafe { crate::hamt::cheby_set_from_list(items) };
        release([items]);
        set
    }

    /// One value holding an object of every layout kind, with the handle
    /// payloads counting into `finalized`.
    fn every_kind(finalized: &Arc<AtomicUsize>) -> Word {
        let desc_of = |name: &str| crate::desc::builtin(name).unwrap();
        let desc = uniform(ids::DESC, &[desc_of("list"), desc_of("int")]);
        let dict = uniform(ids::DICT, &[desc]);
        let dyn_value = uniform(ids::DYN, &[dict, string("inside a dyn")]);
        let closure = crate::generic::tests::closure(&[string("captured")]);
        let numbers = tuple(&[
            int_word(i64::MAX),
            u64_word(u64::MAX),
            f64_word(1.5),
            f32_word(2.5),
        ]);
        let long_list = list((0..2_000).map(int_word).collect());
        let map = map_of((0..100).map(|n| (int_word(n), string("v"))).collect());
        let set = set_of((0..100).map(int_word).collect());
        // Every key collides under a zero mask: collision nodes.
        let previous = crate::tls::set_hash_mask(0);
        let colliding_map = map_of((0..4).map(|n| (int_word(n), int_word(n))).collect());
        let colliding_set = set_of((0..4).map(int_word).collect());
        crate::tls::set_hash_mask(previous);
        let mut handles: Vec<Word> = KINDS
            .iter()
            .map(|kind| counted(*kind, finalized, vec![string("payload")]))
            .collect();
        handles.push(selector([
            counted(HandleKind::Receiver, finalized, Vec::new()),
            string("apply"),
            int_word(0),
            int_word(0),
            object::nil(),
        ]));
        let handles = list(handles);
        tuple(&[
            string("text"),
            bytes(&[1, 2, 3]),
            numbers,
            closure,
            dyn_value,
            user_ctor(string("field")),
            long_list,
            map,
            set,
            colliding_map,
            colliding_set,
            handles,
            object::nil(),
            int_word(7),
        ])
    }

    #[test]
    fn every_layout_kind_is_marked_and_freed_through_the_atomic_path() {
        let domain = Domain::new();
        with_domain(Arc::clone(&domain), || {
            let finalized = Arc::new(AtomicUsize::new(0));
            let value = every_kind(&finalized);
            // SAFETY: a live value only this thread uses.
            unsafe { mark_shared(value) };
            let objects = reachable(value);
            let names: BTreeSet<&str> = objects
                .iter()
                .filter(|(rc, _)| *rc < 0)
                .map(|(_, name)| name.as_str())
                .collect();
            for name in [
                "String",
                "Bytes",
                "Int",
                "U64",
                "Float",
                "F32",
                "closure",
                "tuple",
                "dictionary",
                "dyn",
                "descriptor",
                "t.Pair",
                "List",
                "List.Leaf",
                "List.Branch",
                "Map",
                "Map.Node",
                "Map.Collision",
                "Set",
                "Set.Node",
                "Set.Collision",
                "Sender",
                "Receiver",
                "Fiber",
                "Scope",
                "Selector",
            ] {
                assert!(names.contains(name), "{name} is not marked: {names:?}");
            }
            // Every heap object is shared; only static ones keep count 0.
            for (rc, name) in &objects {
                assert!(*rc < 0 || *rc == abi::RC_STATIC, "{name}: {rc}");
            }
            let statics = objects.iter().filter(|(rc, _)| *rc == 0).count();
            assert!(statics > 0, "the nominal descriptors and `Nil` are static");
            // Marking again changes nothing.
            // SAFETY: as above.
            unsafe { mark_shared(value) };
            assert_eq!(reachable(value), objects);
            // A second reference, then both dropped: the first drop only
            // decrements atomically, the second frees everything through
            // the shared path, finalizing each handle once.
            // SAFETY: a live shared value.
            unsafe { dup(value) };
            assert_eq!(count(value), -2);
            release([value]);
            assert_eq!(finalized.load(AtomicOrdering::Relaxed), 0);
            release([value]);
            assert_eq!(finalized.load(AtomicOrdering::Relaxed), 5);
        });
        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    }

    /// Static objects are never written, and an object that is already
    /// shared is not entered again: its subgraph is shared by I-2, so a
    /// positive count planted under it here stays as it is.
    #[test]
    fn static_and_shared_objects_are_skipped() {
        let domain = Domain::new();
        with_domain(Arc::clone(&domain), || {
            let hidden = tuple(&[]);
            let inner = tuple(&[hidden]);
            // SAFETY: a live object only this thread uses.
            unsafe { store_rc(word_to_ptr(inner), -1) };
            let static_true = object::cheby_rt_static_true.word();
            let header = object::cheby_rt_static_true.header();
            let outer = tuple(&[inner, static_true, int_word(1)]);
            // SAFETY: as above.
            unsafe { mark_shared(outer) };
            assert_eq!(count(outer), -1);
            assert_eq!(count(inner), -1);
            assert_eq!(count(hidden), 1);
            assert_eq!(object::cheby_rt_static_true.header(), header);
            // Tagged integers, 0 and statics on their own.
            // SAFETY: never dereferenced, or static.
            unsafe {
                mark_shared(int_word(5));
                mark_shared(0);
                mark_shared(static_true);
            }
            assert_eq!(object::cheby_rt_static_true.header(), header);
            // A count above one becomes the same count, negative.
            let twice = tuple(&[]);
            // SAFETY: a live object only this thread uses.
            unsafe {
                dup(twice);
                mark_shared(twice);
            }
            assert_eq!(count(twice), -2);
            release([outer, twice, twice]);
        });
        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    }

    /// A heap object that becomes static by count overflow (D-454) reaches
    /// only shared or static objects, so marking a value that reaches it,
    /// which skips it, leaves no local count below it (review findings
    /// MEM-1 and CONC2-3: before the fix its children kept count 1).
    #[test]
    fn an_object_made_static_by_overflow_reaches_only_shared_objects() {
        let domain = Domain::new();
        with_domain(Arc::clone(&domain), || {
            let child = tuple(&[string("grandchild")]);
            let parent = tuple(&[child, string("sibling")]);
            // SAFETY: a live object only this thread uses, one reference
            // short of the most a local count holds.
            unsafe {
                store_rc(word_to_ptr(parent), i32::MAX);
                dup(parent);
            }
            assert_eq!(count(parent), abi::RC_STATIC);
            assert_eq!(count(child), -1);
            let outer = tuple(&[parent]);
            // SAFETY: as above.
            unsafe { mark_shared(outer) };
            let reached = reachable(outer);
            assert_eq!(reached.len(), 5);
            for (rc, name) in &reached {
                assert!(*rc <= 0, "{name} kept the local count {rc}");
            }
            // SAFETY: a live object.
            assert!(!unsafe { is_unique(child) });
            release([outer]);
        });
        // The static parent is never freed, so neither is what it holds:
        // the child, the grandchild and the sibling, which stay charged to
        // the domain as D-454's overflow leaves them.
        assert_eq!(domain.live_objects(), 3);
    }

    /// `[[…[0]…]]`, lists nested a million deep, marked on a small stack:
    /// the walk is a worklist, not recursion.
    #[test]
    #[cfg_attr(miri, ignore = "a million-deep value")]
    fn a_million_deep_list_is_marked_without_recursion() {
        let domain = Domain::new();
        let handle = std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn({
                let domain = Arc::clone(&domain);
                move || {
                    with_domain(domain, || {
                        let mut value = int_word(0);
                        for _ in 0..1_000_000 {
                            value = list(vec![value]);
                        }
                        // SAFETY: a live value only this thread uses.
                        unsafe { mark_shared(value) };
                        // Down the spine: each level is a root and a leaf.
                        let objects = reachable(value);
                        assert_eq!(objects.len(), 2_000_000);
                        assert!(objects.iter().all(|(rc, _)| *rc == -1));
                        release([value]);
                    });
                }
            })
            .unwrap();
        handle.join().unwrap();
        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    }

    /// A shared object freed by another thread, through the atomic path,
    /// with poisoning on (this crate's tests poison by default): the last
    /// release frees it and its shared children once, and the blocks go
    /// to that thread's quarantine.
    #[test]
    fn a_shared_object_is_freed_through_the_atomic_path_under_poisoning() {
        let domain = Domain::new();
        let finalized = Arc::new(AtomicUsize::new(0));
        let value = with_domain(Arc::clone(&domain), || {
            let handle = counted(HandleKind::Scope, &finalized, Vec::new());
            let value = tuple(&[string("shared"), list(vec![int_word(1)]), handle]);
            // SAFETY: a live value only this thread uses so far.
            unsafe { mark_shared(value) };
            value
        });
        let threads: Vec<_> = (0..8)
            .map(|_| {
                // SAFETY: a live shared value: one reference per thread.
                unsafe { dup(value) };
                let domain = Arc::clone(&domain);
                std::thread::spawn(move || {
                    with_domain(domain, || {
                        // SAFETY: this thread's reference.
                        unsafe { drop(value) };
                    });
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(count(value), -1);
        let freeing = std::thread::spawn({
            let domain = Arc::clone(&domain);
            move || {
                with_domain(domain, || {
                    let before = crate::alloc::quarantine_len();
                    // SAFETY: the last reference.
                    unsafe { drop(value) };
                    crate::alloc::quarantine_len().saturating_sub(before)
                })
            }
        });
        let quarantined = freeing.join().unwrap();
        assert_eq!(finalized.load(AtomicOrdering::Relaxed), 1);
        if poison_enabled() {
            // The tuple, the string, the list's root and leaf, the handle.
            assert_eq!(quarantined, 5);
        }
        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    }

    #[test]
    fn selector_words_are_marked() {
        let domain = Domain::new();
        with_domain(Arc::clone(&domain), || {
            let words: [Word; SELECTOR_WORDS] = [
                string("a"),
                tuple(&[string("b")]),
                int_word(3),
                0,
                object::nil(),
            ];
            let node = selector(words);
            // SAFETY: a live value only this thread uses.
            unsafe { mark_shared(node) };
            let objects = reachable(node);
            assert_eq!(objects.iter().filter(|(rc, _)| *rc == -1).count(), 4);
            release([node]);
        });
        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    }
}

/// D-765: the walk that `dup_slow` runs when a local count overflows is not
/// charged to the memory budget, so it never raises, and a refusal of the
/// system allocator during it ends the process. Its own module, so that
/// `cargo test -p cheby_runtime rc::overflow_oom` selects these tests.
#[cfg(test)]
mod overflow_oom_tests {
    use std::sync::{Arc, Mutex, PoisonError};

    use super::*;
    use crate::alloc::{Domain, scratch, with_domain};
    use crate::entry::{EntryConfig, EntryMode, EntryOutcome, current_fiber, run_task};
    use crate::values::{string, tuple};

    /// The line D-765 ends the process with.
    const FATAL_LINE: &str = "fatal runtime error: out of memory while marking a value shared after its count overflowed\n";

    /// `(parent, child)`: a tuple holding a list of `items` strings and a
    /// string, one reference short of the most a local count holds, so the
    /// next `dup` overflows it and its walk grows its worklist many times.
    fn overflowing(items: i64) -> (Word, Word) {
        let child = crate::list::from_iter((0..items).map(|n| string(&format!("item {n}"))));
        let parent = tuple(&[child, string("sibling")]);
        // SAFETY: a live object only this thread uses.
        unsafe { store_rc(word_to_ptr(parent), i32::MAX) };
        (parent, child)
    }

    /// Every distinct heap object reachable from `word` (the root included)
    /// and whether each one's count is `<= 0`. Iterative.
    fn all_shared_or_static(word: Word) -> (usize, bool) {
        let mut seen = std::collections::HashSet::new();
        let mut list = vec![word];
        let mut all = true;
        while let Some(word) = list.pop() {
            if abi::is_tagged_int(word) || word == 0 || !seen.insert(word) {
                continue;
            }
            // SAFETY: a live object reached from the test's value.
            all &= unsafe { load_rc(word_to_ptr(word)) } <= 0;
            // SAFETY: as above.
            unsafe { alloc::for_each_child(word, &mut |child| list.push(child)) };
        }
        (seen.len(), all)
    }

    /// What the entry of the budget test saw.
    #[derive(Debug, Default)]
    struct Seen {
        budget_set: u64,
        budget_after: u64,
        flag: u32,
        scratch_before: u64,
        scratch_after: u64,
        peak_after: u64,
        domain_before: i64,
        domain_after: i64,
        parent_count: i32,
        reached: usize,
        all_shared: bool,
    }

    /// An overflow inside a fiber whose budget is used up: before D-765 the
    /// walk's first scratch growth raised the memory limit inside
    /// `cheby_rt_dup`, which has no unwind edge. Now the subgraph is marked
    /// shared, nothing raises, and the budget's use is unchanged: no
    /// scratch is charged (live and peak stay where they were) and the
    /// domain's live bytes only fall by the object that left it (D-454).
    #[test]
    #[cfg_attr(miri, ignore = "runs an entry thread")]
    fn an_overflow_inside_an_exhausted_budget_marks_the_subgraph_and_raises_nothing() {
        let domain = Domain::new();
        let config = EntryConfig {
            domain: Arc::clone(&domain),
            ..EntryConfig::new(EntryMode::Test)
        };
        let slot: Arc<Mutex<Option<Seen>>> = Arc::default();
        let out = Arc::clone(&slot);
        let task = move || {
            let Some(ctx) = current_fiber() else {
                return 1;
            };
            let (parent, _child) = overflowing(2_000);
            scratch::reset_peak();
            let scratch_before = scratch::live();
            let domain_before = ctx.domain().live_bytes();
            // Nothing more may be allocated, scratch included (D-536).
            let budget_set = u64::try_from(domain_before)
                .unwrap_or(u64::MAX)
                .saturating_add(scratch_before);
            ctx.set_budget(budget_set);
            // SAFETY: a live object only this fiber uses.
            unsafe { cheby_rt_dup(parent) };
            let seen = Seen {
                budget_set,
                budget_after: ctx.budget(),
                flag: ctx.unwind.load(Ordering::Acquire),
                scratch_before,
                scratch_after: scratch::live(),
                peak_after: scratch::peak(),
                domain_before,
                domain_after: ctx.domain().live_bytes(),
                // SAFETY: a live object.
                parent_count: unsafe { load_rc(word_to_ptr(parent)) },
                reached: 0,
                all_shared: false,
            };
            ctx.set_budget(u64::MAX);
            let (reached, all_shared) = all_shared_or_static(parent);
            // SAFETY: the test's reference to a static object: a no-op.
            unsafe { cheby_rt_drop(std::ptr::null_mut(), parent) };
            *out.lock().unwrap_or_else(PoisonError::into_inner) = Some(Seen {
                reached,
                all_shared,
                ..seen
            });
            0
        };
        // SAFETY: the task builds and reads only its own runtime objects.
        let outcome = unsafe { run_task(&config, task) }.unwrap();
        assert!(matches!(outcome, EntryOutcome::Returned(0)), "{outcome:?}");
        let seen = slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap();
        assert_eq!(seen.flag, 0, "nothing raised: {seen:?}");
        assert_eq!(seen.budget_after, seen.budget_set, "{seen:?}");
        assert_eq!(seen.scratch_after, seen.scratch_before, "{seen:?}");
        assert_eq!(seen.peak_after, seen.scratch_before, "{seen:?}");
        assert!(seen.domain_after < seen.domain_before, "{seen:?}");
        assert_eq!(seen.parent_count, abi::RC_STATIC);
        // The tuple, the sibling, the list's nodes and its 2,000 strings.
        assert!(seen.reached > 2_002, "{seen:?}");
        assert!(seen.all_shared, "a local count is left below: {seen:?}");
    }

    /// The same walk on a thread that runs no fiber charges nothing either.
    #[test]
    fn the_overflow_walk_charges_no_scratch() {
        let domain = Domain::new();
        with_domain(Arc::clone(&domain), || {
            let (parent, child) = overflowing(100);
            scratch::reset_peak();
            let before = scratch::live();
            // SAFETY: a live object only this thread uses.
            unsafe { cheby_rt_dup(parent) };
            assert_eq!((scratch::live(), scratch::peak()), (before, before));
            assert!(all_shared_or_static(parent).1);
            // SAFETY: a live object.
            assert!(unsafe { load_rc(word_to_ptr(child)) } < 0);
        });
    }

    /// D-765's fatal line: the child test below overflows a count with the
    /// next scratch growth refused, as a system that has no memory left
    /// would refuse it.
    #[test]
    #[cfg_attr(miri, ignore = "starts a process")]
    fn a_refused_growth_of_the_overflow_walk_is_fatal() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "rc::overflow_oom_tests::refused_overflow_walk_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("CHEBY_RT_TEST_DUP_REFUSAL", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("running 1 test"), "{stdout}{stderr}");
        assert!(!output.status.success(), "{stdout}{stderr}");
        assert!(stderr.contains(FATAL_LINE), "{stdout}{stderr}");
        assert!(!stdout.contains("the dup returned"), "{stdout}{stderr}");
    }

    /// The child side of `a_refused_growth_of_the_overflow_walk_is_fatal`:
    /// does nothing unless `CHEBY_RT_TEST_DUP_REFUSAL` is set.
    #[test]
    fn refused_overflow_walk_child() {
        if std::env::var_os("CHEBY_RT_TEST_DUP_REFUSAL").is_none() {
            return;
        }
        with_domain(Domain::new(), || {
            let (parent, _child) = overflowing(100);
            scratch::refuse_next_scratch_growth();
            // SAFETY: a live object only this thread uses.
            unsafe { cheby_rt_dup(parent) };
            println!("the dup returned");
        });
    }
}

#[cfg(test)]
mod tests {
    use std::ptr;
    use std::sync::Arc;

    use super::*;
    use crate::alloc::tests::{serial, tuple};
    use crate::alloc::{Domain, LEAK_CHECK_ENABLED, live_objects};
    use crate::entry::{EntryConfig, EntryMode, EntryOutcome, current_fiber, run_task};
    use crate::object::cheby_rt_static_true;
    use crate::string::tests::StaticImage;

    fn count(word: Word) -> i32 {
        // SAFETY: tests only pass live objects.
        unsafe { load_rc(word_to_ptr(word)) }
    }

    /// D-772: generated code's drop passes its fiber's control
    /// block. A free through it, of a leaf or of a value with fields,
    /// charges that fiber's domain, and its block cache is read through
    /// the control block's record (D-739; debug builds check that the
    /// fiber is current).
    #[test]
    #[cfg_attr(miri, ignore = "runs an entry thread")]
    fn a_drop_given_its_fibers_ctx_frees_into_its_domain() {
        let domain = Domain::new();
        let config = EntryConfig {
            domain: Arc::clone(&domain),
            ..EntryConfig::new(EntryMode::Test)
        };
        let task = || {
            let Some(ctx) = current_fiber() else {
                return 1;
            };
            let me = ptr::from_ref(ctx).cast_mut();
            let pair = tuple(&[crate::values::string("a"), crate::values::string("b")]);
            let leaf = crate::values::string("leaf");
            if ctx.domain().live_objects() != 4 {
                return 2;
            }
            // SAFETY: live objects owned here; `me` is the running fiber.
            unsafe {
                cheby_rt_dup(pair);
                cheby_rt_drop(me, pair);
            }
            if (ctx.domain().live_objects(), count(pair)) != (4, 1) {
                return 3;
            }
            // SAFETY: the last references: the tuple and its fields, then
            // the leaf, are freed.
            unsafe {
                cheby_rt_drop(me, pair);
                cheby_rt_drop(me, leaf);
                cheby_rt_drop(me, abi::tag_int(7).unwrap_or(0));
            }
            if (ctx.domain().live_objects(), ctx.domain().live_bytes()) != (0, 0) {
                return 4;
            }
            0
        };
        // SAFETY: the task frees everything it allocates.
        let outcome = unsafe { run_task(&config, task) }.unwrap();
        assert_eq!(outcome, EntryOutcome::Returned(0));
        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    }

    #[test]
    fn tagged_ints_are_skipped() {
        for n in [0, -1, abi::INT63_MAX, abi::INT63_MIN] {
            let word = abi::tag_int(n).unwrap();
            // SAFETY: tagged integers are never dereferenced.
            unsafe {
                cheby_rt_dup(word);
                cheby_rt_drop(ptr::null_mut(), word);
            }
        }
    }

    #[test]
    fn static_objects_are_never_written() {
        let word = cheby_rt_static_true.word();
        let before = cheby_rt_static_true.header();
        // SAFETY: a static object; count operations are no-ops.
        unsafe {
            cheby_rt_dup(word);
            cheby_rt_drop(ptr::null_mut(), word);
            cheby_rt_drop(ptr::null_mut(), word);
        }
        assert_eq!(cheby_rt_static_true.header(), before);
        let image = StaticImage::new("static");
        // SAFETY: a static string object in a live buffer.
        unsafe {
            cheby_rt_dup(image.word());
            cheby_rt_drop(ptr::null_mut(), image.word());
            assert_eq!(load_rc(word_to_ptr(image.word())), abi::RC_STATIC);
        }
    }

    #[test]
    fn heap_counts_go_up_and_down() {
        let _guard = serial();
        let before = live_objects();
        let word = tuple(&[]);
        // SAFETY: a live tuple; two references added, three dropped.
        unsafe {
            cheby_rt_dup(word);
            cheby_rt_dup(word);
            assert_eq!(count(word), 3);
            cheby_rt_drop(ptr::null_mut(), word);
            cheby_rt_drop(ptr::null_mut(), word);
            assert_eq!(count(word), 1);
            cheby_rt_drop(ptr::null_mut(), word);
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn uniqueness_is_a_count_of_exactly_one() {
        let word = tuple(&[]);
        let image = StaticImage::new("static");
        // SAFETY: live objects and a tagged integer.
        unsafe {
            assert!(is_unique(word));
            dup(word);
            assert!(!is_unique(word));
            drop(word);
            assert!(is_unique(word));
            assert!(!is_unique(abi::tag_int(1).unwrap()));
            assert!(!is_unique(0));
            assert!(!is_unique(cheby_rt_static_true.word()));
            assert!(!is_unique(image.word()));
            store_rc(word_to_ptr(word), -1);
            assert!(!is_unique(word));
            store_rc(word_to_ptr(word), 1);
            drop(word);
        }
    }

    /// L2-1 (wave 3): the test-only reader the REPL's session test uses to
    /// check that a binding is marked shared (D-705, D-712): `None` for a
    /// tagged integer and 0, the local count, 0 for a static object, and
    /// the negative count once marked shared.
    #[test]
    fn count_of_reads_local_static_and_shared_counts() {
        let word = tuple(&[]);
        let image = StaticImage::new("static");
        // SAFETY: live objects, 0 and a tagged integer; `word` is used only
        // by this thread, and its last reference is dropped at the end.
        unsafe {
            assert_eq!(count_of(abi::tag_int(5).unwrap()), None);
            assert_eq!(count_of(0), None);
            assert_eq!(count_of(word), Some(1));
            dup(word);
            assert_eq!(count_of(word), Some(2));
            drop(word);
            assert_eq!(count_of(image.word()), Some(0));
            mark_shared(word);
            assert_eq!(count_of(word), Some(-1));
            drop(word);
        }
    }

    #[test]
    fn count_overflow_makes_the_object_static() {
        let _guard = serial();
        let before = live_objects();
        let word = tuple(&[]);
        // SAFETY: a live tuple whose count is set just below the limit.
        unsafe {
            store_rc(word_to_ptr(word), i32::MAX);
            cheby_rt_dup(word);
            assert_eq!(count(word), abi::RC_STATIC);
            cheby_rt_drop(ptr::null_mut(), word);
            assert_eq!(count(word), abi::RC_STATIC);
        }
        // Static now: never freed and no longer counted.
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn shared_counts_are_negative_and_free_at_zero() {
        let _guard = serial();
        let before = live_objects();
        let word = tuple(&[]);
        // SAFETY: a live tuple marked shared by hand (M2's marking).
        unsafe {
            store_rc(word_to_ptr(word), -1);
            cheby_rt_dup(word);
            assert_eq!(count(word), -2);
            cheby_rt_drop(ptr::null_mut(), word);
            assert_eq!(count(word), -1);
            cheby_rt_drop(ptr::null_mut(), word);
        }
        assert_eq!(live_objects(), before);
    }

    #[test]
    fn shared_count_overflow_makes_the_object_static() {
        let _guard = serial();
        let before = live_objects();
        let word = tuple(&[]);
        // SAFETY: a live tuple marked shared with the most references.
        unsafe {
            store_rc(word_to_ptr(word), i32::MIN);
            cheby_rt_dup(word);
            assert_eq!(count(word), abi::RC_STATIC);
        }
        assert_eq!(live_objects(), before);
        assert!(LEAK_CHECK_ENABLED || before == 0);
    }
}
