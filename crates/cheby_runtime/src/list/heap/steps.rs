//! The steps of a direct call of `std::heap`'s updates and of
//! `heap::to_list`, whose comparison generated code calls.
//!
//! The kernels of this module's parent call `compare` back through a
//! trampoline from Rust (D-526) unless it is built in. The std intrinsics
//! that replace them at a call (`cheby_mir`'s `lower/intrinsic.rs`) run
//! D-1613's sifts in steps instead, and call `compare` themselves:
//!
//! ```text
//! slot = heap_slot()                        // a stack slot for a [`Steps`]
//! list = heap_push(items, item, max, by, slot)
//! at = heap_first(slot)                     // the word at offset 0
//! while at != 0 {
//!     order = by(pair_left(at), pair_right(at))
//!     at = heap_step(slot, tag(order))
//! }
//! // list is the result
//! ```
//!
//! `heap_pop` and `heap_heapify` start the same way. A start does the
//! kernel's work up to its sift and takes the list over as the kernel's
//! wrapper would leave it: the list it returns is the argument itself at
//! count one, updated in place, or a new root sharing the argument's tree,
//! which the first write under a shared node copies a path of. Generated
//! code holds that list until the end, so when `compare` panics or is
//! cancelled, the cleanup releases it, and the tree then holds each element
//! once, since a sift only exchanges two slots. The [`Steps`] in the slot
//! hold nothing of their own. When the element type's `compare` is built
//! in, the start sifts natively and the first step returns 0.
//!
//! `heap::to_list` pops a scratch copy, as the kernel does. Its state is a
//! `Bytes` object, like a sort's (`list/steps.rs`), with the copy and the
//! popped elements, borrowed from the list the finish is given.
//!
//! Each step's answer is the tag of the `Order` that `compare` returned
//! (0 `Less`, 1 `Equal`, 2 `Greater`): `before` holds when it is the
//! heap's direction's. The sifts below ask for the comparisons in the
//! order of the kernel's [`sift_up`] and [`sift_down`], so a `compare`
//! that is not a consistent order gives the same heap and sees the same
//! calls.

use std::cmp::Ordering;

use super::order::{self, Key};
use super::{Array, Before, Native, Scratch, Signed, Slots, heapify, root, sift_down, sift_up};
use crate::abi::{self, Word};
use crate::ctx::ExecCtx;
use crate::list::node::{drop_word, dup_word, empty};
use crate::list::ops;
use crate::list::steps::{Kind, Pair, address, buffers, header, new_state};
use crate::panic::fatal;
use crate::unwind::guard::{DropOnUnwind, guarded};

/// The tag of an `Order` in the heap's direction: `Less` for a min-heap
/// and `Greater` for a max-heap.
fn direction(max: bool) -> i64 {
    if max { 2 } else { 0 }
}

/// D-1613's sift-up, stopped at a comparison: the kernel's [`sift_up`]
/// with its loop turned inside out.
#[derive(Clone, Copy, Debug)]
struct Up<P> {
    i: usize,
    here: P,
    x: Word,
    parent: usize,
    above: P,
}

impl<P: Copy> Up<P> {
    /// The sift-up of the element at `at`.
    fn start(slots: &mut impl Slots<Slot = P>, at: usize) -> Self {
        let here = slots.slot(at);
        Self {
            i: at,
            here,
            x: slots.read(here),
            parent: 0,
            above: here,
        }
    }

    /// The next comparison, `before(x, parent)`, or `None` at the root.
    fn ask(&mut self, slots: &mut impl Slots<Slot = P>) -> Option<Pair> {
        if self.i == 0 {
            return None;
        }
        self.parent = self.i.saturating_sub(1) / 2;
        self.above = slots.slot(self.parent);
        Some([self.x, slots.read(self.above)])
    }

    /// The answer to the last comparison, then the next one.
    fn answer(&mut self, slots: &mut impl Slots<Slot = P>, before: bool) -> Option<Pair> {
        if !before {
            return None;
        }
        self.here = slots.swap(self.here, self.above);
        self.i = self.parent;
        self.ask(slots)
    }
}

/// D-1613's sift-down, stopped at a comparison: the kernel's
/// [`sift_down`] with its loop turned inside out.
#[derive(Clone, Copy, Debug)]
struct Down<P> {
    count: usize,
    position: usize,
    here: P,
    element: Word,
    /// The child chosen so far, where it is and what it holds.
    child: usize,
    below: P,
    chosen: Word,
    /// The right child, where it is and what it holds, while the choice
    /// between the children waits for its answer.
    right: usize,
    other: P,
    candidate: Word,
    choosing: bool,
}

impl<P: Copy> Down<P> {
    /// The sift-down of the element at `at`.
    fn start(slots: &mut impl Slots<Slot = P>, at: usize) -> Self {
        let here = slots.slot(at);
        let element = slots.read(here);
        Self {
            count: slots.len(),
            position: at,
            here,
            element,
            child: at,
            below: here,
            chosen: element,
            right: at,
            other: here,
            candidate: element,
            choosing: false,
        }
    }

    /// The next comparison: `before(right, left)` when the element has two
    /// children, else `before(child, element)`; `None` at a leaf.
    fn ask(&mut self, slots: &mut impl Slots<Slot = P>) -> Option<Pair> {
        let left = self.position.saturating_mul(2).saturating_add(1);
        if left >= self.count {
            return None;
        }
        let right = left.saturating_add(1);
        self.child = left;
        self.below = slots.slot(left);
        self.chosen = slots.read(self.below);
        if right < self.count {
            self.right = right;
            self.other = slots.slot(right);
            self.candidate = slots.read(self.other);
            self.choosing = true;
            return Some([self.candidate, self.chosen]);
        }
        self.choosing = false;
        Some([self.chosen, self.element])
    }

    /// The answer to the last comparison, then the next one.
    fn answer(&mut self, slots: &mut impl Slots<Slot = P>, before: bool) -> Option<Pair> {
        if self.choosing {
            self.choosing = false;
            if before {
                (self.child, self.below, self.chosen) = (self.right, self.other, self.candidate);
            }
            return Some([self.chosen, self.element]);
        }
        if !before {
            return None;
        }
        self.here = slots.swap(self.here, self.below);
        self.position = self.child;
        self.ask(slots)
    }
}

/// The sift a [`Steps`] is in.
#[derive(Clone, Copy, Debug)]
enum Task<P> {
    /// Between sifts.
    Idle,
    Up(Up<P>),
    Down(Down<P>),
}

impl<P: Copy> Task<P> {
    /// The next comparison of the sift, given the answer to its pending
    /// one when `answer` holds one.
    fn next(&mut self, slots: &mut impl Slots<Slot = P>, answer: Option<bool>) -> Option<Pair> {
        match (self, answer) {
            (Self::Idle, _) => None,
            (Self::Up(up), None) => up.ask(slots),
            (Self::Up(up), Some(before)) => up.answer(slots, before),
            (Self::Down(down), None) => down.ask(slots),
            (Self::Down(down), Some(before)) => down.answer(slots, before),
        }
    }
}

/// What a start leaves in its stack slot for the steps of a push, a pop or
/// a heapify (module documentation). `first` and `done` are written first
/// and read first: a start that sifted natively writes nothing else.
#[repr(C)]
pub(super) struct Steps {
    /// The address of the first pair to compare, or 0 when there is none,
    /// which generated code reads at offset 0 instead of a first step.
    first: Word,
    done: bool,
    pending: bool,
    /// The tag of the heap's direction.
    direction: i64,
    /// The heapify's positions still to sift down: the next is one less.
    heapify: usize,
    pair: Pair,
    task: Task<super::Place>,
    /// The list, which the steps borrow: it holds no reference of its
    /// own (generated code holds the list).
    array: Array,
}

/// The size of a [`Steps`] fits the stack slot generated code makes for it.
const _: () = assert!(size_of::<Steps>() <= abi::heap::STEPS_SIZE);

impl Steps {
    /// The next comparison, given the answer to the pending one, or `None`
    /// when the work is done.
    fn step(&mut self, tag: i64) -> Option<Pair> {
        let answer = self.pending.then_some(tag == self.direction);
        self.pending = false;
        let mut next = self.task.next(&mut self.array, answer);
        loop {
            if let Some(pair) = next {
                self.pending = true;
                return Some(pair);
            }
            if self.heapify == 0 {
                self.task = Task::Idle;
                return None;
            }
            self.heapify = self.heapify.saturating_sub(1);
            let mut down = Down::start(&mut self.array, self.heapify);
            next = down.ask(&mut self.array);
            self.task = Task::Down(down);
        }
    }
}

/// Writes a [`Steps`] that is done to the slot at `slot`.
///
/// # Safety
///
/// `slot` is a stack slot of at least [`abi::heap::STEPS_SIZE`] bytes,
/// 8-aligned.
#[expect(
    clippy::cast_ptr_alignment,
    reason = "generated code makes the slot 8-aligned (`abi::heap::STEPS_SLOT_SIZE`)"
)]
unsafe fn write_done(slot: u64) {
    let steps = crate::object::word_to_ptr(slot).cast::<Steps>();
    // SAFETY: forwarded from the caller; `first` and `done` are read
    // first.
    unsafe {
        std::ptr::addr_of_mut!((*steps).first).write(0);
        std::ptr::addr_of_mut!((*steps).done).write(true);
    }
}

/// Writes `steps` to the slot at `slot` and takes the first step, whose
/// pair's address it leaves in `first`.
///
/// # Safety
///
/// As for [`write_done`].
#[expect(
    clippy::cast_ptr_alignment,
    reason = "generated code makes the slot 8-aligned (`abi::heap::STEPS_SLOT_SIZE`)"
)]
unsafe fn write_steps(slot: u64, steps: Steps) {
    let at = crate::object::word_to_ptr(slot).cast::<Steps>();
    // SAFETY: forwarded from the caller.
    let steps = unsafe {
        at.write(steps);
        &mut *at
    };
    steps.first = steps.next(-1);
}

impl Array {
    /// Takes the list over for generated code: returns it, owned, and
    /// keeps borrowing it. A root this array made is given away; the
    /// argument's root gets a reference.
    fn detach(&mut self) -> Word {
        let word = self.root.word();
        if !self.owned {
            dup_word(word);
        }
        self.owned = false;
        word
    }
}

/// The steps of a start whose sift waits for `compare`.
fn waiting(array: Array, max: bool, task: Task<super::Place>, heapify: usize) -> Steps {
    Steps {
        first: 0,
        done: false,
        pending: false,
        direction: direction(max),
        heapify,
        pair: [0, 0],
        task,
        array,
    }
}

/// The built-in comparison of `by` for a heap in direction `max`, if the
/// element type's `compare` is built in (D-1613).
///
/// # Safety
///
/// `by` is a live closure.
unsafe fn native(by: Word, max: bool) -> Option<Builtin> {
    let first = if max {
        Ordering::Greater
    } else {
        Ordering::Less
    };
    // SAFETY: forwarded from the caller.
    match unsafe { order::native_key(by) } {
        Some(Key::Signed) => Some(Builtin::Signed(Signed(first))),
        Some(key @ (Key::Unsigned | Key::Text | Key::Tuple(_))) => {
            Some(Builtin::Native(Native(key, first)))
        }
        None => None,
    }
}

/// A built-in `before`.
enum Builtin {
    Signed(Signed),
    Native(Native),
}

impl Builtin {
    /// Runs `f` with this `before`.
    fn run<R>(&self, f: impl FnOnce(&dyn BeforeDyn) -> R) -> R {
        match self {
            Self::Signed(before) => f(before),
            Self::Native(before) => f(before),
        }
    }
}

/// [`Before`] as a trait object, for [`Builtin::run`].
trait BeforeDyn {
    /// Sifts up the element at `at` of `array`.
    fn up(&self, array: &mut Array, at: usize);
    /// Sifts down the element at `at` of `slots`.
    fn down(&self, array: &mut Array, at: usize);
    /// Heapifies `array`.
    fn heapify(&self, array: &mut Array);
    /// Pops `slots` into `popped` until it is empty, as `to_list` does.
    fn drain(&self, slots: &mut Scratch<'_>, popped: &mut [Word]);
}

impl<B: Before> BeforeDyn for B {
    fn up(&self, array: &mut Array, at: usize) {
        // SAFETY: a built-in comparison of live elements calls nothing back.
        unsafe { sift_up(array, at, self) };
    }

    fn down(&self, array: &mut Array, at: usize) {
        // SAFETY: as above.
        unsafe { sift_down(array, at, self) };
    }

    fn heapify(&self, array: &mut Array) {
        // SAFETY: as above.
        unsafe { heapify(array, self) };
    }

    fn drain(&self, slots: &mut Scratch<'_>, popped: &mut [Word]) {
        let mut count = 0;
        while slots.len > 0 {
            if let Some(out) = popped.get_mut(count) {
                *out = slots.read(0);
            }
            count = count.saturating_add(1);
            let end = slots.len.saturating_sub(1);
            slots.swap(0, end);
            slots.len = end;
            // SAFETY: as above.
            unsafe { sift_down(slots, 0, self) };
        }
    }
}

/// `HeapPush`: `heap::push`'s start: `items` with `item` added at the
/// end, owned, and in `slot` the steps of its sift-up. Consumes `items`
/// and `item`, even when it raises.
///
/// # Safety
///
/// `ctx` is the running fiber's control block; `items` is a live list
/// and `item` a live element, both owned; `by` is a live closure of two
/// elements that returns an `Order`; `slot` is a stack slot of
/// [`abi::heap::STEPS_SIZE`] bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_heap_push(
    ctx: *mut ExecCtx,
    items: u64,
    item: u64,
    max: bool,
    by: u64,
    slot: u64,
) -> u64 {
    guarded(Some(ctx), || {
        let held = DropOnUnwind::new([items, item]);
        // SAFETY: forwarded from the caller.
        let mut array = Array::new(unsafe { root(items) });
        dup_word(item);
        array.push(item);
        let last = array.len().saturating_sub(1);
        // SAFETY: forwarded from the caller.
        let builtin = unsafe { native(by, max) };
        let word = array.detach();
        if let Some(builtin) = builtin {
            builtin.run(|before| before.up(&mut array, last));
            // SAFETY: forwarded from the caller.
            unsafe { write_done(slot) };
        } else {
            let task = Task::Up(Up::start(&mut array, last));
            // SAFETY: forwarded from the caller.
            unsafe { write_steps(slot, waiting(array, max, task, 0)) };
        }
        held.disarm();
        drop_word(item);
        drop_word(items);
        word
    })
}

/// `HeapPop`: `heap::pop`'s start once the caller has read the root:
/// `items` without it, the last element moved there, owned, and in
/// `slot` the steps of its sift-down. Consumes `items`, even when it
/// raises.
///
/// # Safety
///
/// As for [`cheby_rt_heap_push`], without `item`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_heap_pop(
    ctx: *mut ExecCtx,
    items: u64,
    max: bool,
    by: u64,
    slot: u64,
) -> u64 {
    guarded(Some(ctx), || {
        let held = DropOnUnwind::new([items]);
        // SAFETY: forwarded from the caller.
        let list = unsafe { root(items) };
        if list.len() <= 1 {
            // SAFETY: forwarded from the caller.
            unsafe { write_done(slot) };
            held.disarm();
            drop_word(items);
            return empty();
        }
        let mut array = Array::new(list);
        let moved = array.pop_last();
        array.set(0, moved);
        // SAFETY: forwarded from the caller.
        let builtin = unsafe { native(by, max) };
        let word = array.detach();
        if let Some(builtin) = builtin {
            builtin.run(|before| before.down(&mut array, 0));
            // SAFETY: forwarded from the caller.
            unsafe { write_done(slot) };
        } else {
            let task = Task::Down(Down::start(&mut array, 0));
            // SAFETY: forwarded from the caller.
            unsafe { write_steps(slot, waiting(array, max, task, 0)) };
        }
        held.disarm();
        drop_word(items);
        word
    })
}

/// `HeapHeapify`: `heap::from_list`'s start: `items` in their order,
/// owned, and in `slot` the steps that sift down each position from
/// `n / 2 - 1` to `0`. Consumes `items`, even when it raises.
///
/// # Safety
///
/// As for [`cheby_rt_heap_pop`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_heap_heapify(
    ctx: *mut ExecCtx,
    items: u64,
    max: bool,
    by: u64,
    slot: u64,
) -> u64 {
    guarded(Some(ctx), || {
        let held = DropOnUnwind::new([items]);
        // SAFETY: forwarded from the caller.
        let mut array = Array::new(unsafe { root(items) });
        // SAFETY: forwarded from the caller.
        let builtin = unsafe { native(by, max) };
        let word = array.detach();
        if let Some(builtin) = builtin {
            builtin.run(|before| before.heapify(&mut array));
            // SAFETY: forwarded from the caller.
            unsafe { write_done(slot) };
        } else {
            let positions = array.len() / 2;
            // SAFETY: forwarded from the caller.
            unsafe { write_steps(slot, waiting(array, max, Task::Idle, positions)) };
        }
        held.disarm();
        drop_word(items);
        word
    })
}

/// `HeapStep`: the next step of the steps in `slot`, given the tag of
/// the answer to the pending comparison (any value for the first step):
/// the address of the next pair to compare, or 0 when the work is done. A
/// swap under a shared node copies its path, which may raise.
///
/// # Safety
///
/// `ctx` is the running fiber's control block; `slot` is the slot a start
/// wrote, whose list generated code still holds.
#[expect(
    clippy::cast_ptr_alignment,
    reason = "generated code makes the slot 8-aligned (`abi::heap::STEPS_SLOT_SIZE`)"
)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_heap_step(ctx: *mut ExecCtx, slot: u64, tag: i64) -> u64 {
    let steps = crate::object::word_to_ptr(slot).cast::<Steps>();
    // SAFETY: forwarded from the caller: `done` is always written.
    if unsafe { std::ptr::addr_of!((*steps).done).read() } {
        return 0;
    }
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller: a start wrote the whole steps.
        unsafe { &mut *steps }.next(tag)
    })
}

impl Steps {
    /// [`Steps::step`] as a step's result: the address of the next pair,
    /// or 0 once done.
    fn next(&mut self, tag: i64) -> Word {
        if let Some(pair) = self.step(tag) {
            self.pair = pair;
            address(&self.pair)
        } else {
            self.done = true;
            0
        }
    }
}

/// The header of `heap::to_list`'s state: the kernel's pops of a scratch
/// copy, stopped at a comparison. The copy's `n` words and the popped
/// elements' `n` follow it.
#[repr(C)]
struct List {
    kind: Kind,
    n: usize,
    /// The elements left in the copy.
    len: usize,
    /// The elements popped.
    popped: usize,
    direction: i64,
    pending: bool,
    sift: Task<usize>,
    pair: Pair,
}

/// `HeapListStart`: `heap::to_list`'s state for the borrowed `items`.
/// When the element type's `compare` is built in, it pops them all
/// natively and the first step returns 0.
///
/// # Safety
///
/// `ctx` is the running fiber's control block, `items` a live list and
/// `by` a live closure of two elements that returns an `Order`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_heap_list_start(
    ctx: *mut ExecCtx,
    items: u64,
    max: bool,
    by: u64,
) -> u64 {
    guarded(Some(ctx), || {
        // SAFETY: forwarded from the caller.
        let list = unsafe { root(items) };
        let n = list.len();
        // SAFETY: forwarded from the caller.
        let st = unsafe { new_state::<List>(ctx, n.saturating_mul(2)) };
        // SAFETY: a new state for a `List` with two buffers of `n`.
        let (head, words) =
            unsafe { (header::<List>(st), buffers::<List>(st, n.saturating_mul(2))) };
        let (copy, popped) = words.split_at_mut(n);
        for (slot, item) in copy.iter_mut().zip(ops::Iter::new(list)) {
            *slot = item;
        }
        *head = List {
            kind: Kind::HeapList,
            n,
            len: n,
            popped: 0,
            direction: direction(max),
            pending: false,
            sift: Task::Idle,
            pair: [0, 0],
        };
        // SAFETY: forwarded from the caller.
        if let Some(builtin) = unsafe { native(by, max) } {
            let mut slots = Scratch {
                items: copy,
                len: n,
            };
            builtin.run(|before| before.drain(&mut slots, popped));
            head.len = 0;
            head.popped = n;
        }
        st
    })
}

/// The next step of `heap::to_list`'s state `st` (`cheby_rt_buffer_step`).
///
/// # Safety
///
/// `st` is a live state of kind [`Kind::HeapList`], which only the caller
/// holds.
pub(crate) unsafe fn list_step(st: Word, tag: i64) -> u64 {
    // SAFETY: forwarded from the caller.
    let head = unsafe { header::<List>(st) };
    // SAFETY: as above.
    let words = unsafe { buffers::<List>(st, head.n.saturating_mul(2)) };
    let (copy, popped) = words.split_at_mut(head.n);
    let mut slots = Scratch {
        items: copy,
        len: head.len,
    };
    let answer = head.pending.then_some(tag == head.direction);
    head.pending = false;
    let mut next = head.sift.next(&mut slots, answer);
    let found = loop {
        if let Some(pair) = next {
            break Some(pair);
        }
        if slots.len == 0 {
            head.sift = Task::Idle;
            break None;
        }
        if let Some(out) = popped.get_mut(head.popped) {
            *out = slots.read(0);
        }
        head.popped = head.popped.saturating_add(1);
        let end = slots.len.saturating_sub(1);
        slots.swap(0, end);
        slots.len = end;
        let mut down = Down::start(&mut slots, 0);
        next = down.ask(&mut slots);
        head.sift = Task::Down(down);
    };
    head.len = slots.len;
    match found {
        Some(pair) => {
            head.pending = true;
            head.pair = pair;
            address(&head.pair)
        }
        None => 0,
    }
}

/// The popped elements of `heap::to_list`'s state `st`, once done
/// (`cheby_rt_buffer_finish`).
///
/// # Safety
///
/// As for [`list_step`]; the slice lives while `st` does.
pub(crate) unsafe fn list_result<'a>(st: Word) -> &'a [Word] {
    // SAFETY: forwarded from the caller.
    let head = unsafe { header::<List>(st) };
    // SAFETY: as above.
    let words = unsafe { buffers::<List>(st, head.n.saturating_mul(2)) };
    match words.get(head.n..head.n.saturating_add(head.popped)) {
        Some(popped) if head.len == 0 && !head.pending => popped,
        _ => fatal(format_args!("`heap::to_list` finished before its last pop")),
    }
}
