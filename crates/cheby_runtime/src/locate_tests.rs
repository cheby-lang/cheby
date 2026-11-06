//! Tests of the code map and the frame-record walk (D-856,
//! D-867). Pure: no fiber, no JIT, no `mmap` and no `asm!`
//! except in `locate_strips_a_signed_return_address`, which Miri skips, so
//! the rest run under Miri (D-721). Stacks are synthetic: a map from
//! addresses to words, read through a reader that strips a fake
//! authentication tag and counts its strips.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::locate::{
    CallSiteDesc, CodeMap, CodeRangeDesc, FrameReader, LiveStack, Location, MAX_FRAMES,
    new_code_owner, register_code, unregister_code, walk,
};

/// The synthetic stack's bounds.
const LO: usize = 0x10_0000;
const HI: usize = 0x20_0000;

/// The bits the test reader's strip clears, standing for a pointer
/// authentication code.
const TAG_BITS: usize = 0xFFFF_0000_0000_0000;

/// A user function with two tagged calls.
const USER: usize = 0x4000_0000;
/// A std function: Cheby ABI, no sites.
const STD: usize = 0x4000_1000;
/// Another user function, which calls `USER`.
const OUTER: usize = 0x4000_2000;
/// An entry thunk: C ABI.
const THUNK: usize = 0x4000_3000;

fn site(ret_offset: u32, location: &str) -> CallSiteDesc {
    CallSiteDesc {
        ret_offset,
        location: location.to_owned(),
    }
}

fn range(start: usize, len: u32, cheby_abi: bool, sites: Vec<CallSiteDesc>) -> CodeRangeDesc {
    CodeRangeDesc {
        start,
        len,
        cheby_abi,
        sites,
        name: None,
        large_body: false,
    }
}

fn loc(path: &str, line: u32, column: u32) -> Location {
    Location {
        path: Arc::from(path),
        line,
        column,
    }
}

/// `base + offset`, for addresses.
fn at(base: usize, offset: usize) -> usize {
    base.checked_add(offset).unwrap()
}

/// The map most tests walk: `USER` with sites at 0x20 (5:22) and 0x40
/// (7:3), `STD`, `OUTER` with a site at 0x30, and `THUNK`.
fn program() -> CodeMap {
    let mut map = CodeMap::new();
    map.register(
        1,
        vec![
            range(
                USER,
                0x100,
                true,
                vec![
                    site(0x40, "src/main.cheby:7:3"),
                    site(0x20, "src/main.cheby:5:22"),
                ],
            ),
            range(STD, 0x80, true, vec![]),
            range(OUTER, 0x100, true, vec![site(0x30, "src/lib.cheby:12:5")]),
            range(THUNK, 0x40, false, vec![]),
        ],
    );
    map
}

/// A synthetic stack of frame records.
struct Stack {
    words: BTreeMap<usize, usize>,
    /// Or-ed into every return address written.
    tag: usize,
    /// Whether `strip` clears [`TAG_BITS`].
    strips_tags: bool,
    /// How many times `strip` ran.
    strips: Cell<usize>,
    /// How many words were read.
    reads: Cell<usize>,
}

impl Stack {
    fn new() -> Self {
        Self {
            words: BTreeMap::new(),
            tag: 0,
            strips_tags: true,
            strips: Cell::new(0),
            reads: Cell::new(0),
        }
    }

    /// Writes the frame record at `fp`: the caller's frame pointer, then
    /// the return address.
    fn record(mut self, fp: usize, caller: usize, ra: usize) -> Self {
        self.words.insert(fp, caller);
        self.words.insert(at(fp, 8), ra | self.tag);
        self
    }
}

impl FrameReader for Stack {
    fn word(&self, address: usize) -> Option<usize> {
        self.reads.set(self.reads.get().checked_add(1).unwrap());
        self.words.get(&address).copied()
    }

    fn strip(&self, ra: usize) -> usize {
        self.strips.set(self.strips.get().checked_add(1).unwrap());
        if self.strips_tags { ra & !TAG_BITS } else { ra }
    }
}

fn run(map: &CodeMap, stack: &Stack, frame: usize) -> Option<Location> {
    walk(map, stack, frame, LO, HI)
}

/// The wrapper's record returns into a user function at a tagged call:
/// that call is the answer (D-856).
#[test]
fn locate_finds_the_tagged_call_the_wrapper_returns_to() {
    let map = program();
    let stack = Stack::new().record(0x1F_0000, 0x1F_0100, at(USER, 0x20));
    assert_eq!(
        run(&map, &stack, 0x1F_0000),
        Some(loc("src/main.cheby", 5, 22))
    );
    let stack = Stack::new().record(0x1F_0000, 0x1F_0100, at(USER, 0x40));
    assert_eq!(
        run(&map, &stack, 0x1F_0000),
        Some(loc("src/main.cheby", 7, 3))
    );
}

/// The innermost tagged site wins over an outer one: the walk stops at the
/// first record whose return address is a recorded site.
#[test]
fn locate_the_innermost_tagged_site_wins() {
    let map = program();
    let stack = Stack::new()
        .record(0x1F_0000, 0x1F_0100, at(USER, 0x20))
        .record(0x1F_0100, 0x1F_0200, at(OUTER, 0x30));
    assert_eq!(
        run(&map, &stack, 0x1F_0000),
        Some(loc("src/main.cheby", 5, 22))
    );
    assert_eq!(stack.strips.get(), 1);
    // Started one record further out, the outer site is the answer.
    assert_eq!(
        run(&map, &stack, 0x1F_0100),
        Some(loc("src/lib.cheby", 12, 5))
    );
}

/// A return address into std (Cheby ABI, no sites) moves on to the
/// caller's record: std frames are passed through.
#[test]
fn locate_passes_through_std_frames() {
    let map = program();
    let stack = Stack::new()
        .record(0x1F_0000, 0x1F_0040, at(STD, 0x10))
        .record(0x1F_0040, 0x1F_0080, at(STD, 0x30))
        .record(0x1F_0080, 0x1F_0100, at(OUTER, 0x30));
    assert_eq!(
        run(&map, &stack, 0x1F_0000),
        Some(loc("src/lib.cheby", 12, 5))
    );
    assert_eq!(stack.strips.get(), 3);
}

/// An untagged offset in a user range (a call without a location, such as
/// a continuation's) is passed through like std.
#[test]
fn locate_passes_through_an_untagged_offset_of_a_user_function() {
    let map = program();
    let stack = Stack::new()
        .record(0x1F_0000, 0x1F_0100, at(USER, 0x24))
        .record(0x1F_0100, 0x1F_0200, at(OUTER, 0x30));
    assert_eq!(
        run(&map, &stack, 0x1F_0000),
        Some(loc("src/lib.cheby", 12, 5))
    );
}

/// A C-ABI range (an entry thunk, a trampoline, a `@blocking` shim) ends
/// the walk with `None`, even when a tagged record lies beyond it (D-856).
#[test]
fn locate_stops_at_a_c_abi_function() {
    let map = program();
    let stack = Stack::new()
        .record(0x1F_0000, 0x1F_0100, at(THUNK, 0x10))
        .record(0x1F_0100, 0x1F_0200, at(OUTER, 0x30));
    assert_eq!(run(&map, &stack, 0x1F_0000), None);
    // The record beyond the thunk was never read.
    assert_eq!(stack.reads.get(), 2);
}

/// A return address outside every range (Rust code, no JIT code) ends the
/// walk with `None`, and so do the addresses right after a range and 0.
#[test]
fn locate_stops_at_an_address_outside_jit_code() {
    let map = program();
    for ra in [0x1234_5678, at(STD, 0x81), at(THUNK, 0x1000), 0] {
        let stack = Stack::new().record(0x1F_0000, 0x1F_0100, ra).record(
            0x1F_0100,
            0x1F_0200,
            at(OUTER, 0x30),
        );
        assert_eq!(run(&map, &stack, 0x1F_0000), None, "{ra:#x}");
    }
}

/// A return address belongs to the function holding the byte before it:
/// one equal to a function's end is that function's last call, and one
/// equal to a function's start is not that function's.
#[test]
fn locate_a_return_address_belongs_to_the_byte_before_it() {
    let mut map = CodeMap::new();
    map.register(
        1,
        vec![
            range(0x5000, 0x40, true, vec![site(0x40, "a.cheby:1:1")]),
            range(0x5040, 0x40, false, vec![]),
        ],
    );
    let stack = Stack::new().record(0x1F_0000, 0x1F_0100, 0x5040);
    assert_eq!(run(&map, &stack, 0x1F_0000), Some(loc("a.cheby", 1, 1)));
}

/// Bad frame pointers end the walk with `None`: 0, not 16-byte aligned,
/// below the stack, with a record past its top, and the same for a
/// caller's frame pointer.
#[test]
fn locate_stops_at_bad_frame_pointers() {
    let map = program();
    let good = Stack::new().record(0x1F_0000, 0x1F_0100, at(USER, 0x20));
    for frame in [0, 0x1F_0008, 0x1F_0001, 0x0F_FFF0, HI, at(HI, 0x10_0000)] {
        assert_eq!(run(&map, &good, frame), None, "{frame:#x}");
    }
    // The last record that fits: its two words end at the top.
    let top = Stack::new().record(0x1F_FFF0, 0, at(USER, 0x20));
    assert_eq!(
        run(&map, &top, 0x1F_FFF0),
        Some(loc("src/main.cheby", 5, 22))
    );
    for caller in [0, 0x1F_0108, 0x20_0000, 0x30_0000] {
        let stack = Stack::new()
            .record(0x1F_0000, caller, at(STD, 0x10))
            .record(caller, 0x1F_FF00, at(OUTER, 0x30));
        assert_eq!(run(&map, &stack, 0x1F_0000), None, "{caller:#x}");
    }
}

/// The chain must grow toward the stack's top: a caller's frame pointer at
/// or below the current one ends the walk.
#[test]
fn locate_stops_when_the_chain_does_not_grow() {
    let map = program();
    let same = Stack::new().record(0x1F_0000, 0x1F_0000, at(STD, 0x10));
    assert_eq!(run(&map, &same, 0x1F_0000), None);
    assert_eq!(same.strips.get(), 1);
    let back = Stack::new()
        .record(0x1F_0100, 0x1F_0000, at(STD, 0x10))
        .record(0x1F_0000, 0x1F_0200, at(OUTER, 0x30));
    assert_eq!(run(&map, &back, 0x1F_0100), None);
}

/// A word the reader cannot read ends the walk.
#[test]
fn locate_stops_at_an_unreadable_word() {
    let map = program();
    let stack = Stack::new().record(0x1F_0000, 0x1F_0100, at(STD, 0x10));
    assert_eq!(run(&map, &stack, 0x1F_0000), None);
    assert_eq!(run(&map, &Stack::new(), 0x1F_0000), None);
}

/// The walk reads at most `MAX_FRAMES` records: a tagged call in the
/// 4096th record is found, one in the 4097th is not (plan §0 'Deep data').
#[test]
fn locate_stops_at_the_frame_cap() {
    let map = program();
    let chain = |depth: usize| {
        let mut stack = Stack::new();
        let mut fp = LO;
        for _ in 1..depth {
            stack = stack.record(fp, at(fp, 0x10), at(STD, 0x10));
            fp = at(fp, 0x10);
        }
        stack.record(fp, at(fp, 0x10), at(OUTER, 0x30))
    };
    let deepest = chain(MAX_FRAMES);
    assert_eq!(run(&map, &deepest, LO), Some(loc("src/lib.cheby", 12, 5)));
    assert_eq!(deepest.strips.get(), MAX_FRAMES);
    let past = chain(MAX_FRAMES.checked_add(1).unwrap());
    assert_eq!(run(&map, &past, LO), None);
    assert_eq!(past.strips.get(), MAX_FRAMES);
}

/// Every return address is stripped before its lookup: with tagged return
/// addresses the walk finds the site only through the strip, once per
/// record read; a reader that does not strip finds nothing.
#[test]
fn locate_strips_every_return_address_before_its_lookup() {
    let map = program();
    let mut stack = Stack::new();
    stack.tag = 0x2B00_0000_0000_0000;
    let stack = stack
        .record(0x1F_0000, 0x1F_0040, at(STD, 0x10))
        .record(0x1F_0040, 0x1F_0080, at(USER, 0x24))
        .record(0x1F_0080, 0x1F_0100, at(USER, 0x40));
    assert_eq!(
        run(&map, &stack, 0x1F_0000),
        Some(loc("src/main.cheby", 7, 3))
    );
    assert_eq!(stack.strips.get(), 3);
    let mut unstripped = stack;
    unstripped.strips_tags = false;
    unstripped.strips.set(0);
    assert_eq!(run(&map, &unstripped, 0x1F_0000), None);
    assert_eq!(unstripped.strips.get(), 1);
}

/// Registering keeps the ranges sorted and disjoint across batches: a new
/// range replaces the registered ranges it overlaps (their code memory was
/// reused), within a batch the first of two overlapping ranges wins, and
/// empty ranges are skipped.
#[test]
fn locate_registering_keeps_ranges_sorted_and_disjoint() {
    let mut map = CodeMap::new();
    map.register(
        1,
        vec![
            range(0x3000, 0x100, true, vec![]),
            range(0x1000, 0x100, true, vec![]),
            range(0x2000, 0, true, vec![]),
        ],
    );
    assert_eq!(map.spans(), [(0x1000, 0x1100, 1), (0x3000, 0x3100, 1)]);
    // A second batch interleaves without moving the first.
    map.register(
        2,
        vec![
            range(0x2000, 0x80, true, vec![]),
            range(0x1100, 0x10, false, vec![]),
        ],
    );
    assert_eq!(
        map.spans(),
        [
            (0x1000, 0x1100, 1),
            (0x1100, 0x1110, 2),
            (0x2000, 0x2080, 2),
            (0x3000, 0x3100, 1),
        ]
    );
    // An overlapping batch replaces what it overlaps, and the first of its
    // own overlapping ranges wins.
    map.register(
        3,
        vec![
            range(0x2040, 0x1000, true, vec![]),
            range(0x2050, 0x10, true, vec![]),
        ],
    );
    assert_eq!(
        map.spans(),
        [
            (0x1000, 0x1100, 1),
            (0x1100, 0x1110, 2),
            (0x2040, 0x3040, 3),
        ]
    );
    let spans = map.spans();
    for (before, after) in spans.iter().zip(spans.iter().skip(1)) {
        assert!(before.1 <= after.0);
    }
    // A range past the end of the address space is skipped.
    map.register(4, vec![range(usize::MAX, 2, true, vec![])]);
    assert_eq!(map.spans().len(), 3);
}

/// Unregistering removes exactly the owner's ranges, and the other owners'
/// sites still answer.
#[test]
fn locate_unregistering_removes_only_the_owners_ranges() {
    let mut map = program();
    map.register(
        2,
        vec![range(
            0x4100_0000,
            0x40,
            true,
            vec![site(0x10, "b.cheby:2:4")],
        )],
    );
    map.unregister(1);
    assert_eq!(map.spans(), [(0x4100_0000, 0x4100_0040, 2)]);
    let stack = Stack::new().record(0x1F_0000, 0x1F_0100, 0x4100_0010);
    assert_eq!(run(&map, &stack, 0x1F_0000), Some(loc("b.cheby", 2, 4)));
    let gone = Stack::new().record(0x1F_0000, 0x1F_0100, at(USER, 0x20));
    assert_eq!(run(&map, &gone, 0x1F_0000), None);
    map.unregister(2);
    assert_eq!(map.spans(), []);
}

/// A REPL location keeps the colon in its path: `<repl:3>:1:16` is the
/// path `<repl:3>`, line 1, column 16, and prints back the same (D-583).
#[test]
fn locate_repl_paths_keep_their_colon() {
    let mut map = CodeMap::new();
    map.register(
        1,
        vec![range(
            0x6000,
            0x40,
            true,
            vec![site(0x10, "<repl:3>:1:16"), site(0x20, "<repl:3>:2:1")],
        )],
    );
    let stack = Stack::new().record(0x1F_0000, 0x1F_0100, 0x6010);
    let found = run(&map, &stack, 0x1F_0000).unwrap();
    assert_eq!(found, loc("<repl:3>", 1, 16));
    assert_eq!(found.to_string(), "<repl:3>:1:16");
    assert_eq!(map.path_count(), 1);
}

/// Each path is interned once across sites, functions and batches, and a
/// location prints as `path:line:column` (D-858).
#[test]
fn locate_interns_each_path_once() {
    let mut map = program();
    map.register(
        2,
        vec![range(
            0x7000,
            0x40,
            true,
            vec![site(0x10, "src/main.cheby:9:9")],
        )],
    );
    assert_eq!(map.path_count(), 2);
    let stack = Stack::new().record(0x1F_0000, 0x1F_0100, 0x7010);
    let found = run(&map, &stack, 0x1F_0000).unwrap();
    assert_eq!(found.to_string(), "src/main.cheby:9:9");
    let first = Stack::new().record(0x1F_0000, 0x1F_0100, at(USER, 0x20));
    let earlier = run(&map, &first, 0x1F_0000).unwrap();
    assert!(Arc::ptr_eq(&found.path, &earlier.path));
}

/// A site whose location has no `path:line:column` form is the innermost
/// call outside std all the same: the walk ends there with `None` rather
/// than report an outer call. Sites past their function's end are dropped.
#[test]
fn locate_a_site_without_a_readable_location_reads_as_unknown() {
    let mut map = CodeMap::new();
    map.register(
        1,
        vec![
            range(
                0x8000,
                0x40,
                true,
                vec![
                    site(0x10, "no-line"),
                    site(0x18, ":1:2"),
                    site(0x20, "a.cheby:x:2"),
                    site(0x80, "a.cheby:1:1"),
                ],
            ),
            range(0x9000, 0x40, true, vec![site(0x10, "a.cheby:3:3")]),
        ],
    );
    for ra in [0x8010, 0x8018, 0x8020] {
        let stack = Stack::new()
            .record(0x1F_0000, 0x1F_0100, ra)
            .record(0x1F_0100, 0x1F_0200, 0x9010);
        assert_eq!(run(&map, &stack, 0x1F_0000), None, "{ra:#x}");
    }
    // Only `a.cheby` of the well-formed site was interned; the site past
    // the end was dropped.
    assert_eq!(map.path_count(), 1);
    let past = Stack::new().record(0x1F_0000, 0x1F_0100, 0x8080);
    assert_eq!(run(&map, &past, 0x1F_0000), None);
}

/// The live reader reads words through exposed provenance and only inside
/// its bounds; the walk over it finds a site in a map as over the
/// synthetic stacks. Owners are fresh, and the process's map answers
/// `None` for a frame of 0 and after its owner is unregistered.
#[test]
fn locate_reads_a_live_stack_and_the_process_map() {
    let mut words: Vec<usize> = vec![0; 8];
    let lo = words.as_ptr().addr();
    let hi = at(lo, 64);
    let fp = lo.next_multiple_of(16);
    let first = fp.checked_sub(lo).unwrap().checked_div(8).unwrap();
    // Two records: into std, then into `OUTER`'s tagged call. The buffer
    // is exposed after every write, as a stack's memory is exposed when a
    // fiber's slot is mapped.
    words[first] = at(fp, 16);
    words[at(first, 1)] = at(STD, 0x10);
    words[at(first, 2)] = 0;
    words[at(first, 3)] = at(OUTER, 0x30);
    assert_eq!(words.as_ptr().expose_provenance(), lo);
    let live = LiveStack { lo, hi };
    assert_eq!(live.word(at(lo, 1)), None);
    assert_eq!(live.word(hi), None);
    assert_eq!(live.word(lo.wrapping_sub(8)), None);
    assert_eq!(live.word(at(fp, 8)), Some(at(STD, 0x10)));
    assert_eq!(
        walk(&program(), &live, fp, lo, hi),
        Some(loc("src/lib.cheby", 12, 5))
    );

    // The same stack through `locate` and the process's map, whose only
    // ranges in this test binary are this test's.
    let (a, b) = (new_code_owner(), new_code_owner());
    assert_ne!(a, b);
    let base: usize = 0x7FFF_0000_0000;
    words[at(first, 1)] = at(base, 0x10);
    words[at(first, 3)] = at(base, 0x50);
    assert_eq!(words.as_ptr().expose_provenance(), lo);
    register_code(a, vec![range(base, 0x40, true, vec![])]);
    register_code(
        b,
        vec![range(
            at(base, 0x40),
            0x40,
            true,
            vec![site(0x10, "p.cheby:1:2")],
        )],
    );
    assert_eq!(
        crate::locate::locate(fp, lo, hi),
        Some(loc("p.cheby", 1, 2))
    );
    assert_eq!(crate::locate::locate(0, lo, hi), None);
    unregister_code(b);
    assert_eq!(crate::locate::locate(fp, lo, hi), None);
    unregister_code(a);
    drop(words);
}

/// On macOS arm64 a return address signed with the B key (as
/// cranelift-native's prologues sign it, `pacibz`) and one carrying any
/// pointer-authentication bits strip back to the real address in the live
/// reader. Processes without arm64e keys sign as a no-op, so a forged code
/// in the bits above the 47-bit address space is stripped too.
#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[cfg_attr(miri, ignore = "inline assembly")]
fn locate_strips_a_signed_return_address() {
    let ra: usize;
    let signed: usize;
    // SAFETY: `bl` to the next instruction only sets x30 to a real return
    // address, and `pacibz` (a hint) only rewrites x30, which is declared
    // clobbered; nothing touches memory or the stack.
    unsafe {
        std::arch::asm!(
            "bl 2f",
            "2:",
            "mov {ra}, x30",
            "pacibz",
            "mov {signed}, x30",
            ra = out(reg) ra,
            signed = out(reg) signed,
            out("x30") _,
            options(nomem, nostack),
        );
    }
    let live = LiveStack { lo: 0, hi: 0 };
    assert_eq!(live.strip(signed), ra);
    assert_eq!(live.strip(ra), ra);
    for code in [
        0x002B_8000_0000_0000_usize,
        0x5A00_0000_0000_0000,
        0xFF7F_8000_0000_0000,
    ] {
        assert_ne!(ra | code, ra);
        assert_eq!(live.strip(ra | code), ra, "{code:#x}");
    }
}
