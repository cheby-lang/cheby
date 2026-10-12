//! The frame-pointer walk of the `try_call` prototype (D-696, D-700):
//! from the exit record, up the chain of frame records, to the first
//! registered frame with a handler for its return address.
//!
//! The algorithm is `wasmtime-internal-unwinder`'s (`Handler::find` over a
//! run of Cranelift frames), written here without depending on it. On
//! aarch64 and x86-64 alike a frame pointer points at a frame record: the
//! caller's frame pointer at offset 0 and the return address into the
//! caller at offset 8. Generated code keeps frame pointers
//! (`preserve_frame_pointers`), and the walk only ever reads frames of
//! generated code: the exit record names the frame that called an exit
//! stub, and every frame above it up to a catch point (an entry thunk or a
//! trampoline, which catch) is generated code too.
//!
//! The walk is iterative and allocates nothing, so it needs a small fixed
//! stack whatever the depth (D-700).

use super::registry::{Lookup, Tables};

/// Where an unwinding resumes: a handler's address, and the stack and
/// frame pointers of its frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Handler {
    pub(crate) pc: usize,
    pub(crate) sp: usize,
    pub(crate) fp: usize,
}

/// Why a walk found no handler. Every one is a broken invariant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WalkError {
    /// No exit record: no exit stub ran since the fiber started.
    NoExitRecord,
    /// A return address outside every registered function: the walk left
    /// generated code without meeting a catch point.
    NotGenerated { pc: usize },
    /// A frame pointer outside the stack, misaligned, or not above the
    /// frame before it.
    BadFrame { fp: usize },
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoExitRecord => write!(f, "an unwinding started with no exit record"),
            Self::NotGenerated { pc } => write!(
                f,
                "an unwinding reached the return address {pc:#x}, which is not in generated code"
            ),
            Self::BadFrame { fp } => {
                write!(f, "an unwinding reached the broken frame pointer {fp:#x}")
            }
        }
    }
}

/// Reads the words of frame records.
pub(crate) trait Frames {
    /// The word at `address`, which the walk checked lies in the stack and
    /// is aligned.
    ///
    /// # Safety
    ///
    /// `address` is readable.
    unsafe fn word(&self, address: usize) -> usize;
}

/// The offset of the caller's frame pointer in a frame record.
const SAVED_FP: usize = 0;
/// The offset of the return address in a frame record.
const RETURN_ADDRESS: usize = 8;
/// The size of a frame record: two words.
const RECORD_SIZE: usize = 16;
/// The low bits that are clear in a frame pointer: frame records are
/// 16-byte aligned on both architectures.
const FRAME_ALIGN_MASK: usize = 15;

/// Walks from the frame `fp`, stopped at the return address `pc`, to the
/// first frame with a handler for its return address. Frame pointers must
/// lie in `stack` (exclusive of its end, where no frame starts), be
/// aligned and grow towards its end.
///
/// # Errors
///
/// A [`WalkError`] when the chain breaks or leaves generated code before a
/// handler.
///
/// # Safety
///
/// Every frame record the chain reaches inside `stack` is readable through
/// `frames`.
pub(crate) unsafe fn find_handler(
    tables: &Tables<'_>,
    frames: &impl Frames,
    fp: usize,
    pc: usize,
    stack: std::ops::Range<usize>,
) -> Result<Handler, WalkError> {
    if fp == 0 || pc == 0 {
        return Err(WalkError::NoExitRecord);
    }
    let (mut fp, mut pc) = (fp, pc);
    loop {
        let record_end = fp.checked_add(RECORD_SIZE);
        if !stack.contains(&fp)
            || fp & FRAME_ALIGN_MASK != 0
            || record_end.is_none_or(|end| end > stack.end)
        {
            return Err(WalkError::BadFrame { fp });
        }
        match tables.lookup(pc) {
            Lookup::Handler { pc, frame_offset } => {
                let sp = fp
                    .checked_sub(frame_offset)
                    .ok_or(WalkError::BadFrame { fp })?;
                return Ok(Handler { pc, sp, fp });
            }
            Lookup::NoHandler => {}
            Lookup::Unknown => return Err(WalkError::NotGenerated { pc }),
        }
        // SAFETY: the record lies in `stack`, aligned (checked above), and
        // the caller guarantees it is readable.
        let (next_fp, next_pc) = unsafe {
            (
                frames.word(fp.wrapping_add(SAVED_FP)),
                frames.word(fp.wrapping_add(RETURN_ADDRESS)),
            )
        };
        if next_fp <= fp {
            return Err(WalkError::BadFrame { fp: next_fp });
        }
        (fp, pc) = (next_fp, next_pc);
    }
}

/// The frames of the running thread's stack, read in place.
#[derive(Debug)]
pub(crate) struct LiveFrames;

impl Frames for LiveFrames {
    unsafe fn word(&self, address: usize) -> usize {
        let pointer: *const usize = std::ptr::with_exposed_provenance(address);
        // SAFETY: the caller guarantees the word is readable, and the walk
        // checked that it is aligned.
        unsafe { pointer.read() }
    }
}

#[cfg(test)]
#[expect(
    clippy::arithmetic_side_effects,
    reason = "addresses of synthetic frames"
)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::unwind::try_call::{CallSite, register_code};

    /// A synthetic stack: words by address.
    struct Synthetic(BTreeMap<usize, usize>);

    impl Frames for Synthetic {
        unsafe fn word(&self, address: usize) -> usize {
            self.0.get(&address).copied().unwrap_or(0)
        }
    }

    impl Synthetic {
        /// A chain of frame records at `fps`, each frame's record holding
        /// the next frame pointer and the return address `pcs[i]` into it.
        fn chain(fps: &[usize], pcs: &[usize]) -> Self {
            let mut words = BTreeMap::new();
            for (i, &fp) in fps.iter().enumerate() {
                words.insert(fp, fps.get(i + 1).copied().unwrap_or(0));
                words.insert(fp + 8, pcs.get(i + 1).copied().unwrap_or(0));
            }
            Self(words)
        }
    }

    const CODE: usize = 0x7ffe_0000_0000;
    const STACK: std::ops::Range<usize> = 0x1000_0000..0x1001_0000;

    fn site(ret: u32, frame: u32, handler: u32) -> CallSite {
        CallSite {
            ret_offset: ret,
            frame_offset: frame,
            handler_offset: handler,
        }
    }

    #[test]
    fn the_first_frame_with_a_handler_catches() {
        // f calls an exit stub at 0x20 with a handler; g, its caller, has
        // one at 0x10.
        let _f = register_code(CODE, 0x100, vec![site(0x20, 0x40, 0xf0)].into_boxed_slice());
        let _g = register_code(
            CODE + 0x100,
            0x100,
            vec![site(0x10, 0x60, 0xe0)].into_boxed_slice(),
        );
        let fps = [0x1000_1000, 0x1000_2000];
        let pcs = [CODE + 0x20, CODE + 0x110];
        let frames = Synthetic::chain(&fps, &pcs);
        // SAFETY: synthetic frames, readable at every address.
        let found = unsafe { find_handler(&tables(), &frames, fps[0], pcs[0], STACK) };
        assert_eq!(
            found,
            Ok(Handler {
                pc: CODE + 0xf0,
                sp: 0x1000_1000 - 0x40,
                fp: 0x1000_1000,
            })
        );
    }

    #[test]
    fn frames_without_a_handler_are_passed() {
        // Three frames: a resuming function (no handler at its call of the
        // resume stub), one more without, then the catch point.
        let _a = register_code(
            CODE + 0x1000,
            0x80,
            vec![site(0x8, 0, 0x70)].into_boxed_slice(),
        );
        let _b = register_code(
            CODE + 0x1100,
            0x80,
            vec![site(0x8, 0, 0x70)].into_boxed_slice(),
        );
        let _c = register_code(
            CODE + 0x1200,
            0x80,
            vec![site(0x30, 0x20, 0x60)].into_boxed_slice(),
        );
        let fps = [0x1000_0100, 0x1000_0200, 0x1000_0400];
        let pcs = [CODE + 0x1040, CODE + 0x1140, CODE + 0x1230];
        let frames = Synthetic::chain(&fps, &pcs);
        // SAFETY: as above.
        let found = unsafe { find_handler(&tables(), &frames, fps[0], pcs[0], STACK) };
        assert_eq!(
            found,
            Ok(Handler {
                pc: CODE + 0x1260,
                sp: 0x1000_0400 - 0x20,
                fp: 0x1000_0400,
            })
        );
    }

    #[test]
    fn broken_chains_are_errors() {
        let _a = register_code(CODE + 0x2000, 0x80, Box::new([]));
        let tables = tables();
        let none = Synthetic(BTreeMap::new());
        // SAFETY: as above, for every call below.
        unsafe {
            assert_eq!(
                find_handler(&tables, &none, 0, CODE + 0x2010, STACK),
                Err(WalkError::NoExitRecord)
            );
            // Leaving generated code.
            let frames = Synthetic::chain(&[0x1000_0100, 0x1000_0200], &[CODE + 0x2010, 0x1234]);
            assert_eq!(
                find_handler(&tables, &frames, 0x1000_0100, CODE + 0x2010, STACK),
                Err(WalkError::NotGenerated { pc: 0x1234 })
            );
            // A frame pointer that does not grow, one outside the stack and
            // a misaligned one.
            let frames =
                Synthetic::chain(&[0x1000_0200, 0x1000_0100], &[CODE + 0x2010, CODE + 0x2010]);
            assert_eq!(
                find_handler(&tables, &frames, 0x1000_0200, CODE + 0x2010, STACK),
                Err(WalkError::BadFrame { fp: 0x1000_0100 })
            );
            assert_eq!(
                find_handler(&tables, &none, 0x2000_0000, CODE + 0x2010, STACK),
                Err(WalkError::BadFrame { fp: 0x2000_0000 })
            );
            assert_eq!(
                find_handler(&tables, &none, 0x1000_0108, CODE + 0x2010, STACK),
                Err(WalkError::BadFrame { fp: 0x1000_0108 })
            );
        }
    }

    use crate::unwind::try_call::registry::tables;
}
