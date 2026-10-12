//! Finalizers run from the free worklist (D-703): once per object, before
//! its block is freed, with a release function that feeds the worklist, so
//! a chain of a million handles whose finalizers each release the next
//! one frees on a small stack.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{Domain, current_domain, live_objects, with_domain};
use crate::abi::Word;
use crate::handles::tests::KINDS;
use crate::handles::{HandleKind, HandlePayload, new_handle};
use crate::rc;
use crate::values::{int_word, string, tuple};

/// A payload owning the next link of a chain (or a tagged integer at the
/// end), counting its finalizations.
#[derive(Debug)]
struct Link {
    next: Word,
    finalized: Arc<AtomicUsize>,
}

impl HandlePayload for Link {
    fn finalize(self: Box<Self>, release: &mut dyn FnMut(Word)) {
        self.finalized.fetch_add(1, Ordering::Relaxed);
        release(self.next);
    }
}

/// A handle of `kind` owning `next`.
fn link(kind: HandleKind, next: Word, finalized: &Arc<AtomicUsize>) -> Word {
    let payload = Box::new(Link {
        next,
        finalized: Arc::clone(finalized),
    });
    // SAFETY: a null context charges the host thread's domain.
    unsafe { new_handle(std::ptr::null_mut(), kind, payload) }
}

#[test]
#[cfg_attr(miri, ignore = "a million handles")]
fn finalizers_free_a_million_handle_chain_without_recursion() {
    let length: usize = 1_000_000;
    let finalized = Arc::new(AtomicUsize::new(0));
    // A small stack: a finalizer that freed the next handle itself, or a
    // free that recursed into children, would overflow it quickly.
    let handle = std::thread::Builder::new()
        .stack_size(64 << 10)
        .spawn({
            let finalized = Arc::clone(&finalized);
            move || {
                let mut head = int_word(0);
                for (n, kind) in KINDS.iter().cycle().take(length).enumerate() {
                    // Plain objects in the chain too, freed by the same
                    // walk.
                    let next = if n.is_multiple_of(1000) {
                        tuple(&[head, string("between handles")])
                    } else {
                        head
                    };
                    head = link(*kind, next, &finalized);
                }
                let before = live_objects();
                // SAFETY: the only reference to the head; each payload owns
                // the next link.
                unsafe { rc::drop(head) };
                (before, live_objects(), current_domain().live_bytes())
            }
        })
        .unwrap();
    let (before, after, bytes) = handle.join().unwrap();
    assert_eq!(before, 1_002_000);
    assert_eq!((after, bytes), (0, 0));
    assert_eq!(finalized.load(Ordering::Relaxed), length);
}

/// The finalizer runs once, and a word the payload releases that still
/// has other references loses one and stays.
#[test]
fn finalizers_run_once_and_release_only_what_the_payload_owns() {
    let domain = Domain::new();
    with_domain(Arc::clone(&domain), || {
        let finalized = Arc::new(AtomicUsize::new(0));
        let shared = string("kept");
        // SAFETY: a live string: one reference for the payload, one kept.
        unsafe { rc::dup(shared) };
        let word = link(HandleKind::Receiver, shared, &finalized);
        // SAFETY: the handle's only reference.
        unsafe { rc::drop(word) };
        assert_eq!(finalized.load(Ordering::Relaxed), 1);
        assert_eq!(domain.live_objects(), 1);
        // SAFETY: a live string with one reference left.
        unsafe {
            assert_eq!(
                crate::object::load_rc(crate::object::word_to_ptr(shared)),
                1
            );
            rc::drop(shared);
        }
    });
    assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
}
