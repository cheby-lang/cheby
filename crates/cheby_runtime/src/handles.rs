//! The handle kinds of `std::channel`, `std::fiber` and `std::selector`
//! (D-647, D-703): `Sender`, `Receiver`, `Fiber`, `Scope` and `Selector`
//! values are the runtime kinds 40 to 44 ([`abi::layout`]).
//!
//! They are handles: `==` and hashing stop on them with the catchable
//! panic of a function value, and their debug text is `<Sender>`,
//! `<Receiver>`, `<Fiber>`, `<Scope>` or `<Selector>`, exactly as for a
//! value of an external type (D-233, D-590), through the kinds' `handle`
//! flag ([`CustomKind::handle`]).
//!
//! (D-750) The objects of kinds 40 to 43 are 24 bytes:
//! the header, a pointer to the boxed Rust payload ([`HandlePayload`]), and
//! a pointer to the payload's vtable. The payload is shared by every copy
//! of the value (copies share the object), so it is `Send + Sync`. When the
//! last reference goes, the kind's finalizer hands the payload to
//! [`HandlePayload::finalize`], once, from the free worklist (D-703): it
//! gives the Cheby words the payload owns to `release`, which feeds the
//! worklist, and must never block, suspend, raise or run Cheby code. The
//! selector node (44) is the header and five uniform words, all visited as
//! children, so freeing, freezing and shared marking see them; it has no
//! finalizer (architecture.md §7.3: one arm and the previous node).
//!
//! # Selector nodes
//!
//! A `Selector` is a persistent list of nodes (D-695), newest arm first,
//! each node made by [`new_selector_node`] and owning its five words:
//!
//! | Word | Receive arm (tag 0)     | Timer arm (tag 1)       | Empty selector (tag 2) |
//! | ---- | ----------------------- | ----------------------- | ---------------------- |
//! | 0    | the previous node       | the previous node       | `Int` 0                |
//! | 1    | `Int` 0                 | `Int` 1                 | `Int` 2                |
//! | 2    | the `Receiver`          | the span's seconds      | `Int` 0                |
//! | 3    | `Int` 0                 | the span's nanoseconds  | `Int` 0                |
//! | 4    | the apply closure       | the apply closure       | `Int` 0                |
//!
//! Every list ends with an empty node (tag 2), which `selector::new`
//! returns: an empty selector is a real kind-44 object, so it prints as
//! `<Selector>` (selector.md). Integers are uniform `Int` words (tagged, or
//! boxed outside 63 bits). The kernels of `fiber_ops::select` read the
//! words.
//!
//! The kinds are registered once per process, from the layout table's
//! builtin initialization ([`register`]), before any object exists.

use std::any::TypeId;
use std::marker::PhantomData;
use std::sync::Once;

use crate::abi::{self, LayoutId, Word, layout as ids};
use crate::alloc;
use crate::ctx::ExecCtx;
use crate::layout::{self, CustomKind, LayoutInfo};
use crate::object::{self, scan_and_layout, word_to_ptr};
use crate::panic::fatal;

/// The Rust state behind a `Sender`, `Receiver`, `Fiber` or `Scope` handle
/// (kinds 40 to 43), owned by the handle object and shared by every copy
/// of the value.
pub(crate) trait HandlePayload: Send + Sync + 'static {
    /// Runs once, when the last reference to the handle goes away, on the
    /// fiber that released it (D-171, D-703). Gives every Cheby word the
    /// payload owns to `release`, which frees it through the free
    /// worklist when that was its last reference. Never blocks, suspends,
    /// raises or runs Cheby code; it may take a runtime lock for O(1) work
    /// and wake fibers. A Rust panic in it ends the process.
    fn finalize(self: Box<Self>, release: &mut dyn FnMut(Word));
}

/// The handle kinds with a payload: 40 to 43.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandleKind {
    /// A channel's sending handle, `std::channel.Sender` (40).
    Sender,
    /// A channel's receiving handle, `std::channel.Receiver` (41).
    Receiver,
    /// A fiber handle, `std::fiber.Fiber` (42).
    Fiber,
    /// A scope handle, `std::fiber.Scope` (43).
    Scope,
}

impl HandleKind {
    /// The kind's layout id.
    pub(crate) const fn id(self) -> LayoutId {
        match self {
            Self::Sender => ids::SENDER,
            Self::Receiver => ids::RECEIVER,
            Self::Fiber => ids::FIBER,
            Self::Scope => ids::SCOPE,
        }
    }
}

/// The size of a handle object of kinds 40 to 43: the header, the payload
/// pointer and the vtable pointer.
const HANDLE_SIZE: u64 = 24;

/// The offset of the payload pointer.
const PAYLOAD_OFFSET: usize = 8;

/// The offset of the vtable pointer.
const VTABLE_OFFSET: usize = 16;

/// The number of uniform words of a selector node (44).
pub(crate) const SELECTOR_WORDS: usize = 5;

/// The size of a selector node: the header and its words.
const SELECTOR_SIZE: u64 = 48;

/// The tag of a selector node's receive arm (module documentation).
pub(crate) const SELECTOR_RECEIVE: i64 = 0;

/// The tag of a selector node's timer arm.
pub(crate) const SELECTOR_AFTER: i64 = 1;

/// The tag of the empty selector, which ends every list.
pub(crate) const SELECTOR_EMPTY: i64 = 2;

/// Allocates a selector node (kind 44) with count 1 that owns `words`
/// (the layout of the module documentation), charged to the current
/// domain: the running fiber's, or a host thread's. The one constructor of
/// kind-44 objects. A request that cannot be served ends as
/// [`alloc::out_of_memory`] says, and the words are then not released.
#[must_use]
pub(crate) fn new_selector_node(words: [Word; SELECTOR_WORDS]) -> Word {
    register();
    let obj = alloc::alloc_object(SELECTOR_SIZE, abi::header(abi::RC_UNIQUE, 0, ids::SELECTOR));
    #[cfg(debug_assertions)]
    alloc::count_new_kind(None, ids::SELECTOR);
    let mut offset = object::to_usize(abi::HEADER_SIZE);
    for word in words {
        // SAFETY: a fresh node of `SELECTOR_SIZE` bytes: its five words
        // follow the header, each initialized here once.
        unsafe { object::write_field(obj, offset, word) };
        offset = offset.saturating_add(object::to_usize(abi::FIELD_SIZE));
    }
    object::ptr_to_word(obj)
}

/// The words of a live selector node, borrowed.
///
/// # Safety
///
/// `word` is a live selector node made by [`new_selector_node`].
pub(crate) unsafe fn selector_words(word: Word) -> [Word; SELECTOR_WORDS] {
    let obj = word_to_ptr(word);
    if cfg!(debug_assertions) {
        // SAFETY: forwarded from the caller; poisoning catches a freed
        // node here.
        unsafe { alloc::check_live(word, "reading a selector") };
        // SAFETY: a live object.
        let (_, id) = unsafe { scan_and_layout(obj) };
        if id != ids::SELECTOR {
            fatal(format_args!(
                "reading a selector reached an object of layout {id}"
            ));
        }
    }
    let mut words = [0; SELECTOR_WORDS];
    let mut offset = object::to_usize(abi::HEADER_SIZE);
    for slot in &mut words {
        // SAFETY: forwarded from the caller: the node's words follow its
        // header.
        *slot = unsafe { object::read_field(obj, offset) };
        offset = offset.saturating_add(object::to_usize(abi::FIELD_SIZE));
    }
    words
}

/// How a handle object reaches its payload's type: written once by
/// [`new_handle`], one static table per payload type.
#[derive(Debug)]
struct PayloadVtable {
    /// Finalizes the boxed payload at the given address and frees it.
    finalize: unsafe fn(*mut (), &mut dyn FnMut(Word)),
    /// The payload's type, which [`payload`] checks in debug builds.
    type_id: fn() -> TypeId,
}

/// The vtable of the payload type `P`.
struct VtableOf<P>(PhantomData<P>);

impl<P: HandlePayload> VtableOf<P> {
    /// A static table, one per payload type.
    const VTABLE: &'static PayloadVtable = &PayloadVtable {
        finalize: finalize_payload::<P>,
        type_id: TypeId::of::<P>,
    };
}

/// [`PayloadVtable::finalize`] of the payload type `P`.
///
/// # Safety
///
/// `data` is the pointer [`new_handle`] took from a `Box<P>`, given here
/// once.
unsafe fn finalize_payload<P: HandlePayload>(data: *mut (), release: &mut dyn FnMut(Word)) {
    // SAFETY: forwarded from the caller: the box's own pointer, released
    // once, so the box is rebuilt and dropped once.
    let payload = unsafe { Box::from_raw(data.cast::<P>()) };
    payload.finalize(release);
}

/// The payload pointer and the vtable of a handle object of kinds 40 to
/// 43.
///
/// # Safety
///
/// `obj` points to a live (or dying) handle object of kinds 40 to 43 made
/// by [`new_handle`].
#[expect(
    clippy::cast_ptr_alignment,
    reason = "objects are 8-aligned and the two pointers are at offsets 8 and 16"
)]
unsafe fn parts(obj: *const u8) -> (*mut (), &'static PayloadVtable) {
    // SAFETY: forwarded from the caller: `new_handle` wrote both pointers,
    // 8-aligned, and the vtable is a static table.
    unsafe {
        (
            obj.add(PAYLOAD_OFFSET).cast::<*mut ()>().read(),
            obj.add(VTABLE_OFFSET)
                .cast::<&'static PayloadVtable>()
                .read(),
        )
    }
}

/// Allocates a handle of `kind` that owns `payload`, with count 1, charged
/// to `ctx`'s domain (or, with a null `ctx`, the current fiber's or the
/// host's). A request that cannot be served ends as
/// [`alloc::out_of_memory`] says, and the payload is then dropped without
/// being finalized.
///
/// # Safety
///
/// `ctx` is null or the running fiber's control block.
#[expect(
    clippy::cast_ptr_alignment,
    reason = "objects are 8-aligned and the two pointers are at offsets 8 and 16"
)]
pub(crate) unsafe fn new_handle<P: HandlePayload>(
    ctx: *mut ExecCtx,
    kind: HandleKind,
    payload: Box<P>,
) -> Word {
    register();
    // SAFETY: forwarded from the caller.
    let obj = unsafe { alloc::alloc_for(ctx, HANDLE_SIZE, abi::header(1, 0, kind.id())) };
    // SAFETY: forwarded from the caller: null or the running fiber's
    // control block, live while it runs.
    #[cfg(debug_assertions)]
    alloc::count_new_kind(unsafe { ctx.as_ref() }, kind.id());
    let data = Box::into_raw(payload).cast::<()>();
    // SAFETY: a fresh 24-byte, 8-aligned object: the payload pointer goes
    // at offset 8, the vtable pointer at 16.
    unsafe {
        obj.add(PAYLOAD_OFFSET).cast::<*mut ()>().write(data);
        obj.add(VTABLE_OFFSET)
            .cast::<&'static PayloadVtable>()
            .write(VtableOf::<P>::VTABLE);
    }
    object::ptr_to_word(obj)
}

/// The payload of the handle `word`, borrowed for as long as the caller
/// keeps a reference to the handle. Debug builds check that the handle is
/// of `kind` and its payload of type `P`, and end the process otherwise.
///
/// # Safety
///
/// `word` is a live handle of `kind` made by [`new_handle`] with a payload
/// of type `P`, and stays live for `'a`.
pub(crate) unsafe fn payload<'a, P: HandlePayload>(word: Word, kind: HandleKind) -> &'a P {
    let obj = word_to_ptr(word);
    if cfg!(debug_assertions) {
        if abi::is_tagged_int(word) || word == 0 {
            fatal(format_args!(
                "reading a `{kind:?}` handle reached the word {word:#x}"
            ));
        }
        // SAFETY: forwarded from the caller; poisoning catches a freed
        // handle here.
        unsafe { alloc::check_live(word, "reading a handle") };
        // SAFETY: a live object.
        let (_, id) = unsafe { scan_and_layout(obj) };
        if id != kind.id() {
            fatal(format_args!(
                "reading a `{kind:?}` handle reached an object of layout {id}"
            ));
        }
    }
    // SAFETY: forwarded from the caller: a live handle of kinds 40 to 43.
    let (data, vtable) = unsafe { parts(obj) };
    if cfg!(debug_assertions) && (vtable.type_id)() != TypeId::of::<P>() {
        fatal(format_args!(
            "reading a `{kind:?}` handle with the wrong payload type"
        ));
    }
    // SAFETY: the payload is a live `P` (checked above in debug builds),
    // owned by the handle, which the caller keeps live for `'a`; it is
    // only ever shared.
    unsafe { &*data.cast::<P>() }
}

unsafe fn handle_size(_: Word) -> u64 {
    HANDLE_SIZE
}

/// Kinds 40 to 43 hold no Cheby word in the object itself; what their
/// payload owns it gives up when finalized.
unsafe fn handle_children(_: Word, _: &mut dyn FnMut(Word)) {}

/// The finalizer of kinds 40 to 43: finalizes and frees the payload.
///
/// Safety: `word` is a dying handle of kinds 40 to 43 made by
/// [`new_handle`], finalized once.
unsafe fn handle_finalize(word: Word, release: &mut dyn FnMut(Word)) {
    // SAFETY: forwarded from the caller.
    let (data, vtable) = unsafe { parts(word_to_ptr(word)) };
    // SAFETY: the payload of a dying handle, given up once (the free
    // worklist takes each object once).
    unsafe { (vtable.finalize)(data, release) };
}

unsafe fn selector_size(_: Word) -> u64 {
    SELECTOR_SIZE
}

/// Visits a selector node's five words.
///
/// Safety: `word` is a live (or dying) selector node.
unsafe fn selector_children(word: Word, visit: &mut dyn FnMut(Word)) {
    let obj = word_to_ptr(word);
    let mut offset = abi::HEADER_SIZE;
    for _ in 0..SELECTOR_WORDS {
        // SAFETY: the node's words follow its header.
        visit(unsafe { object::read_field(obj, object::to_usize(offset)) });
        offset = offset.saturating_add(abi::FIELD_SIZE);
    }
}

/// A handle kind with a payload.
const fn handle_kind(name: &'static str) -> CustomKind {
    CustomKind {
        name,
        size: handle_size,
        children: handle_children,
        equal: None,
        hash: None,
        debug: None,
        handle: true,
        finalize: Some(handle_finalize),
    }
}

static SENDER_KIND: CustomKind = handle_kind("Sender");
static RECEIVER_KIND: CustomKind = handle_kind("Receiver");
static FIBER_KIND: CustomKind = handle_kind("Fiber");
static SCOPE_KIND: CustomKind = handle_kind("Scope");

/// The selector node kind.
static SELECTOR_KIND: CustomKind = CustomKind {
    name: "Selector",
    size: selector_size,
    children: selector_children,
    equal: None,
    hash: None,
    debug: None,
    handle: true,
    finalize: None,
};

/// Registers the five kinds once per process. The layout table's builtin
/// initialization calls it, so they are known before any object exists.
pub(crate) fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for (id, kind) in [
            (ids::SENDER, &SENDER_KIND),
            (ids::RECEIVER, &RECEIVER_KIND),
            (ids::FIBER, &FIBER_KIND),
            (ids::SCOPE, &SCOPE_KIND),
            (ids::SELECTOR, &SELECTOR_KIND),
        ] {
            if let Err(error) = layout::register_builtin(id, LayoutInfo::custom(kind)) {
                fatal(format_args!("cannot register handle layout {id}: {error}"));
            }
        }
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::alloc::{Domain, with_domain};
    use crate::generic::{self, Reached};
    use crate::rc;
    use crate::values::{int_word, string, tuple};

    /// A payload that counts its finalizations and owns some words.
    #[derive(Debug)]
    pub(crate) struct Counted {
        pub(crate) finalized: Arc<AtomicUsize>,
        pub(crate) owned: Vec<Word>,
    }

    impl HandlePayload for Counted {
        fn finalize(self: Box<Self>, release: &mut dyn FnMut(Word)) {
            self.finalized.fetch_add(1, Ordering::Relaxed);
            for word in &self.owned {
                release(*word);
            }
        }
    }

    /// A handle of `kind` owning `owned`, counting into `finalized`.
    pub(crate) fn counted(
        kind: HandleKind,
        finalized: &Arc<AtomicUsize>,
        owned: Vec<Word>,
    ) -> Word {
        let payload = Box::new(Counted {
            finalized: Arc::clone(finalized),
            owned,
        });
        // SAFETY: a null context charges the host thread's domain.
        unsafe { new_handle(std::ptr::null_mut(), kind, payload) }
    }

    /// A selector node holding `words` (owned).
    pub(crate) fn selector(words: [Word; SELECTOR_WORDS]) -> Word {
        new_selector_node(words)
    }

    pub(crate) const KINDS: [HandleKind; 4] = [
        HandleKind::Sender,
        HandleKind::Receiver,
        HandleKind::Fiber,
        HandleKind::Scope,
    ];

    fn text(word: Word) -> String {
        // SAFETY: tests pass live words.
        unsafe { generic::debug_string(word) }
    }

    fn eq(a: Word, b: Word) -> Result<bool, Reached> {
        // SAFETY: tests pass live words.
        unsafe { generic::equal(a, b) }
    }

    fn hashed(word: Word) -> Result<u64, Reached> {
        // SAFETY: tests pass live words.
        unsafe { generic::hash_with_seed(word, 42) }
    }

    fn release(words: impl IntoIterator<Item = Word>) {
        for word in words {
            // SAFETY: tests release the references they own.
            unsafe { rc::drop(word) };
        }
    }

    #[test]
    fn the_kinds_are_registered_at_40_to_44() {
        // Any builtin lookup registers them.
        assert!(layout::get(ids::STRING).is_some());
        for (id, name) in [
            (ids::SENDER, "Sender"),
            (ids::RECEIVER, "Receiver"),
            (ids::FIBER, "Fiber"),
            (ids::SCOPE, "Scope"),
            (ids::SELECTOR, "Selector"),
        ] {
            let info = layout::get(id).unwrap();
            assert_eq!(&*info.type_name, name);
            assert!(info.is_external(), "{name}");
        }
        for kind in KINDS {
            assert_eq!(
                layout::get(kind.id()).map(|info| &*info.type_name),
                Some(format!("{kind:?}").as_str())
            );
        }
    }

    /// D-233 through the external-value path: `<Name>`, and `==` and
    /// hashing stop with the catchable `Reached::Function`, never the
    /// fatal error of a kind without equality.
    #[test]
    fn handles_print_their_name_and_stop_equality_and_hashing() {
        let domain = Domain::new();
        with_domain(Arc::clone(&domain), || {
            let finalized = Arc::new(AtomicUsize::new(0));
            let mut handles: Vec<(Word, &str)> = KINDS
                .iter()
                .zip(["<Sender>", "<Receiver>", "<Fiber>", "<Scope>"])
                .map(|(kind, name)| (counted(*kind, &finalized, Vec::new()), name))
                .collect();
            handles.push((selector([int_word(0); SELECTOR_WORDS]), "<Selector>"));
            for (word, name) in handles {
                assert_eq!(text(word), name);
                assert_eq!(eq(word, word), Err(Reached::Function), "{name}");
                assert_eq!(hashed(word), Err(Reached::Function), "{name}");
                // SAFETY: a live handle; the tuple takes the new reference.
                unsafe { rc::dup(word) };
                let pair = tuple(&[int_word(1), word]);
                assert_eq!(text(pair), format!("(1, {name})"));
                assert_eq!(eq(pair, pair), Err(Reached::Function));
                assert_eq!(hashed(pair), Err(Reached::Function));
                let other = tuple(&[int_word(2), string("x")]);
                assert_eq!(eq(pair, other), Ok(false));
                release([pair, other, word]);
            }
            assert_eq!(finalized.load(Ordering::Relaxed), 4);
        });
        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    }

    #[test]
    fn the_finalizer_runs_once_when_the_last_reference_goes() {
        let domain = Domain::new();
        with_domain(Arc::clone(&domain), || {
            for kind in KINDS {
                let finalized = Arc::new(AtomicUsize::new(0));
                let owned = string("owned by the payload");
                let word = counted(kind, &finalized, vec![owned, int_word(3)]);
                // SAFETY: a live handle: two more references, then three
                // drops.
                unsafe {
                    rc::dup(word);
                    rc::dup(word);
                    rc::drop(word);
                    rc::drop(word);
                }
                assert_eq!(finalized.load(Ordering::Relaxed), 0);
                assert_eq!(domain.live_objects(), 2);
                // SAFETY: the last reference.
                unsafe { rc::drop(word) };
                assert_eq!(finalized.load(Ordering::Relaxed), 1);
                assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
            }
        });
    }

    /// A shared handle (negative count) is finalized once, by whichever
    /// thread drops the last reference.
    #[test]
    fn a_shared_handle_is_finalized_once() {
        let domain = Domain::new();
        for kind in KINDS {
            let finalized = Arc::new(AtomicUsize::new(0));
            let word = with_domain(Arc::clone(&domain), || {
                let word = counted(kind, &finalized, vec![string("payload word")]);
                // SAFETY: a live handle only this thread uses so far.
                unsafe { rc::mark_shared(word) };
                // SAFETY: as above.
                assert_eq!(unsafe { object::load_rc(word_to_ptr(word)) }, -1);
                word
            });
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    // SAFETY: a live shared handle: one reference per thread.
                    unsafe { rc::dup(word) };
                    let domain = Arc::clone(&domain);
                    std::thread::spawn(move || {
                        with_domain(domain, || {
                            // SAFETY: this thread's reference.
                            unsafe { rc::drop(word) };
                        });
                    })
                })
                .collect();
            for thread in threads {
                thread.join().unwrap();
            }
            assert_eq!(finalized.load(Ordering::Relaxed), 0);
            with_domain(Arc::clone(&domain), || {
                // SAFETY: the last reference.
                unsafe { rc::drop(word) };
            });
            assert_eq!(finalized.load(Ordering::Relaxed), 1);
        }
        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    }

    #[test]
    fn payloads_are_read_back() {
        let finalized = Arc::new(AtomicUsize::new(0));
        let word = counted(HandleKind::Fiber, &finalized, vec![int_word(9)]);
        // SAFETY: a live `Fiber` handle with a `Counted` payload.
        let payload = unsafe { payload::<Counted>(word, HandleKind::Fiber) };
        assert_eq!(payload.owned, [int_word(9)]);
        assert!(Arc::ptr_eq(&payload.finalized, &finalized));
        release([word]);
        assert_eq!(finalized.load(Ordering::Relaxed), 1);
    }

    /// A selector node releases its five words when it is freed, and the
    /// handles among them are finalized.
    #[test]
    fn selector_nodes_release_their_words() {
        let domain = Domain::new();
        with_domain(Arc::clone(&domain), || {
            let finalized = Arc::new(AtomicUsize::new(0));
            let receiver = counted(HandleKind::Receiver, &finalized, Vec::new());
            let previous = selector([int_word(0); SELECTOR_WORDS]);
            let node = selector([
                receiver,
                string("apply"),
                previous,
                int_word(5),
                crate::object::nil(),
            ]);
            assert_eq!(domain.live_objects(), 4);
            release([node]);
            assert_eq!(finalized.load(Ordering::Relaxed), 1);
        });
        assert_eq!((domain.live_objects(), domain.live_bytes()), (0, 0));
    }

    /// The child side of `payload_checks_its_kind_and_type`: does nothing
    /// unless `CHEBY_RT_TEST_HANDLE` names a wrong read.
    #[test]
    fn payload_check_child() {
        #[derive(Debug)]
        struct Other;
        impl HandlePayload for Other {
            fn finalize(self: Box<Self>, _: &mut dyn FnMut(Word)) {}
        }
        let Ok(operation) = std::env::var("CHEBY_RT_TEST_HANDLE") else {
            return;
        };
        let finalized = Arc::new(AtomicUsize::new(0));
        let word = counted(HandleKind::Sender, &finalized, Vec::new());
        // SAFETY: deliberately not: each read is wrong, which debug builds
        // must catch before the payload is touched.
        unsafe {
            match operation.as_str() {
                "kind" => {
                    let _ = payload::<Counted>(word, HandleKind::Receiver);
                }
                "type" => {
                    let _ = payload::<Other>(word, HandleKind::Sender);
                }
                _ => {}
            }
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "starts a process")]
    fn payload_checks_its_kind_and_type() {
        if !cfg!(debug_assertions) {
            return;
        }
        for (operation, message) in [
            (
                "kind",
                "reading a `Receiver` handle reached an object of layout 40",
            ),
            (
                "type",
                "reading a `Sender` handle with the wrong payload type",
            ),
        ] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "handles::tests::payload_check_child",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("CHEBY_RT_TEST_HANDLE", operation)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{operation}: the child succeeded");
            assert!(
                stderr.contains(&format!("fatal runtime error: {message}")),
                "{operation}: {stderr}"
            );
        }
    }

    /// Debug builds count the live objects of each runtime kind per
    /// domain (D-711): one per handle and selector node made, one less
    /// when it is freed, and `leaked_kinds_line` names what is left
    /// (D-784).
    #[test]
    fn live_kinds_count_each_runtime_kind() {
        let domain = Domain::new();
        let finalized = Arc::new(AtomicUsize::new(0));
        with_domain(Arc::clone(&domain), || {
            let mut words: Vec<Word> = KINDS
                .iter()
                .map(|kind| counted(*kind, &finalized, Vec::new()))
                .collect();
            let receiver = counted(HandleKind::Receiver, &finalized, Vec::new());
            let empty = selector([
                int_word(0),
                int_word(2),
                int_word(0),
                int_word(0),
                int_word(0),
            ]);
            words.push(empty);
            let expected = if cfg!(debug_assertions) {
                [1, 2, 1, 1, 1]
            } else {
                [0; 5]
            };
            assert_eq!(domain.live_kinds(), expected);
            let line = alloc::leaked_kinds_line(domain.live_kinds(), domain.live_objects());
            if cfg!(debug_assertions) {
                assert_eq!(
                    line.as_deref(),
                    Some(
                        "note: leaked 1 <Sender>, 2 <Receiver>, 1 <Fiber>, 1 <Scope>, 1 <Selector>"
                    )
                );
            } else {
                assert_eq!(line, None);
            }
            release(words);
            let other = string("other");
            if cfg!(debug_assertions) {
                assert_eq!(domain.live_kinds(), [0, 1, 0, 0, 0]);
                assert_eq!(
                    alloc::leaked_kinds_line(domain.live_kinds(), domain.live_objects()).as_deref(),
                    Some("note: leaked 1 <Receiver>, and 1 object of other kinds")
                );
            }
            release([receiver, other]);
        });
        assert_eq!(domain.live_kinds(), [0; 5]);
        assert_eq!(domain.live_objects(), 0);
        assert_eq!(alloc::leaked_kinds_line(domain.live_kinds(), 0), None);
    }
}
