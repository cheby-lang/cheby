//! The kernels of `std::random` (D-656, D-1048, D-1574): 64 fresh bits
//! from the operating system's random source, and the five `@std_context`
//! kernels of the seeded stream a fiber may hold. Everything else of the
//! module, the seeded `SplitMix64` generator above all (D-655), is Cheby
//! over these kernels, so the seeded functions run in constants and the
//! host ones do not (E0077, D-537).
//!
//! - **The host source.** `host_bits` is an ordinary kernel, not
//!   `@std_context`: it reads no fiber state. It draws through
//!   `getrandom`, which covers every native target, Windows included
//!   (D-714). The source makes no promise of cryptographic strength to
//!   Cheby programs (D-656).
//! - **The seeded stream** (D-1048, D-1574). D-653's "no state" holds
//!   except for one per-fiber stream, which `random::with_generator`
//!   provides and only this module reaches. Its fields are the fiber's
//!   (`context::FiberContext`), beside the bindings of `std::context`, so
//!   a draw under a stream is a load and a store, and no key names the
//!   stream. `int_between`, `fraction` and `from_host` first ask
//!   `sys_stream_on`; under a stream they draw in Cheby from the state
//!   `sys_stream_state` gives and write the advanced state back with
//!   `sys_stream_set`, which starts the count of spawns since the last
//!   draw again. `with_generator` is Cheby over `sys_stream_push` and
//!   `sys_stream_pop`, which put the outer stream aside in the fiber and
//!   take it back; when its body unwinds, the scope that catches the
//!   failure cuts the frames back instead (D-1573). The derivation of a
//!   spawned fiber's or a captured context's stream is in `context.rs`
//!   (`derive`), since no Cheby code runs in a spawn. None of the five
//!   waits or allocates, so they work on an isolated entry too (D-692).

use crate::context;
use crate::ctx::ExecCtx;
use crate::fiber_ops::running;
use crate::panic::fatal;
use crate::unwind::guard::guarded;

/// Appends every `std::random` kernel symbol and its address to `out`.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    out.extend(all().iter().map(|kernel| (kernel.0, kernel.1)));
}

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &["random"];

/// `random::host_bits`, private: 64 bits from the host's random source,
/// fresh on every call (D-656). A source that
/// fails is a broken host, not a condition a program can handle (the page
/// says no call fails or panics), so it ends the program with a fatal
/// runtime error naming the operating system's error (D-794).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn cheby_random_host_bits() -> u64 {
    guarded(None, || {
        getrandom::u64().unwrap_or_else(|error| {
            fatal(format_args!(
                "the operating system's random source failed: {error}"
            ))
        })
    })
}

/// `random::sys_stream_on`, private: whether the calling fiber has a
/// seeded stream (D-1048, D-1574).
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_random_sys_stream_on(ctx: *mut ExecCtx) -> bool {
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    context::stream_on(me)
}

/// `random::sys_stream_state`, private: the state of the calling fiber's
/// seeded stream, which the next draw starts from (D-1048). Called only
/// under a stream.
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_random_sys_stream_state(ctx: *mut ExecCtx) -> u64 {
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    context::stream_state(me)
}

/// `random::sys_stream_set`, private: stores `state`, the state a draw
/// advanced the calling fiber's stream to, and starts the count of spawns
/// and captures since the last draw again (D-1048, D-1567). Called only
/// under a stream.
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_random_sys_stream_set(ctx: *mut ExecCtx, state: u64) {
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    context::set_stream(me, state);
}

/// `random::sys_stream_push`, private, which `with_generator` calls before
/// its body: puts the calling fiber's stream aside and sets a stream that
/// starts at `state` (D-1048, D-1573).
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_random_sys_stream_push(ctx: *mut ExecCtx, state: u64) {
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    context::push_stream(me, true, state);
}

/// `random::sys_stream_pop`, private, which `with_generator` calls after
/// its body returned: the stream that `sys_stream_push` put aside is the
/// calling fiber's again, as far as it had advanced (D-1048, D-1573).
///
/// # Safety
///
/// `ctx` is the running fiber's context.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_random_sys_stream_pop(ctx: *mut ExecCtx) {
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    context::pop_stream(me);
}

/// One kernel: symbol, address, C parameter types (after the context
/// pointer of a `@std_context` kernel) and C result type.
type Entry = (
    &'static str,
    *const u8,
    &'static [&'static str],
    &'static str,
);

/// Every `std::random` kernel.
#[expect(
    clippy::as_conversions,
    reason = "taking the address of a function needs an `as` cast"
)]
fn all() -> [Entry; 6] {
    [
        (
            "cheby_random_host_bits",
            cheby_random_host_bits as *const u8,
            &[],
            "u64",
        ),
        (
            "cheby_random_sys_stream_on",
            cheby_random_sys_stream_on as *const u8,
            &[],
            "bool",
        ),
        (
            "cheby_random_sys_stream_state",
            cheby_random_sys_stream_state as *const u8,
            &[],
            "u64",
        ),
        (
            "cheby_random_sys_stream_set",
            cheby_random_sys_stream_set as *const u8,
            &["u64"],
            "()",
        ),
        (
            "cheby_random_sys_stream_push",
            cheby_random_sys_stream_push as *const u8,
            &["u64"],
            "()",
        ),
        (
            "cheby_random_sys_stream_pop",
            cheby_random_sys_stream_pop as *const u8,
            &[],
            "()",
        ),
    ]
}

/// The C signatures of the kernels, for the cross-check against `std/`
/// in `symbols.rs`.
#[cfg(test)]
pub(crate) fn signatures() -> Vec<(&'static str, &'static [&'static str], &'static str)> {
    all()
        .iter()
        .map(|kernel| (kernel.0, kernel.2, kernel.3))
        .collect()
}

/// The kernels that take the running fiber's context (`@std_context`,
/// D-692): the five of the seeded stream (D-1574).
#[cfg(test)]
pub(crate) const CONTEXT_KERNELS: [&str; 5] = [
    "cheby_random_sys_stream_on",
    "cheby_random_sys_stream_state",
    "cheby_random_sys_stream_set",
    "cheby_random_sys_stream_push",
    "cheby_random_sys_stream_pop",
];

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// D-656: every call draws fresh bits. Two draws of 64 bits are equal
    /// with probability 2⁻⁶⁴, so three equal in a row is a broken source.
    #[test]
    fn random_host_bits_differ_between_calls() {
        let first = cheby_random_host_bits();
        let differ = (0..3).any(|_| cheby_random_host_bits() != first);
        assert!(differ, "four host draws gave {first:#x}");
    }

    /// The reference `SplitMix64` of random.md (D-655), in Rust, so that
    /// std/random.cheby's Cheby and the page's numbers are checked from
    /// both sides. The context tests check the seeded streams' states
    /// against it (D-1048).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct Reference(pub(crate) u64);

    impl Reference {
        /// `from_seed`: the 64 bits of `seed` in two's complement.
        pub(crate) fn from_seed(seed: i64) -> Self {
            Self(u64::from_ne_bytes(seed.to_ne_bytes()))
        }

        /// The output function of a step, random.md's mixing.
        fn mixed(mut z: u64) -> u64 {
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// One step: the state advances by the golden gamma, and the
        /// output mixes the new state.
        pub(crate) fn step(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            Self::mixed(self.0)
        }

        /// The generator that random.md's "Seeded streams" gives the
        /// `n`-th spawn since the last draw: the state
        /// `s + n × 0xD1B54A32D192ED03`, mixed (D-1048).
        pub(crate) fn derived(self, n: u64) -> Self {
            Self(Self::mixed(
                self.0.wrapping_add(n.wrapping_mul(0xD1B5_4A32_D192_ED03)),
            ))
        }

        /// `next_int_between`: D-655's rejection mapping, bit for bit.
        fn int_between(&mut self, low: i64, high: i64) -> i64 {
            let (lo, hi) = if high < low { (high, low) } else { (low, high) };
            let to_bits = |value: i64| u64::from_ne_bytes(value.to_ne_bytes());
            let to_int = |bits: u64| i64::from_ne_bytes(bits.to_ne_bytes());
            let span = to_bits(hi).wrapping_sub(to_bits(lo)).wrapping_add(1);
            if span == 0 {
                return to_int(self.step());
            }
            // `span` is not 0 here, so neither remainder is `None`.
            let threshold = span.wrapping_neg().checked_rem(span).unwrap_or(0);
            loop {
                let x = self.step();
                if x >= threshold {
                    let offset = x.checked_rem(span).unwrap_or(0);
                    return to_int(to_bits(lo).wrapping_add(offset));
                }
            }
        }

        /// `next_fraction`: `x >> 11` over 2⁵³, exact.
        #[expect(
            clippy::as_conversions,
            clippy::cast_precision_loss,
            reason = "a 53-bit integer converts to f64 exactly"
        )]
        pub(crate) fn fraction(&mut self) -> f64 {
            ((self.step() >> 11) as f64) / 9_007_199_254_740_992.0
        }
    }

    /// `count` die rolls from `seed`.
    fn rolls(seed: i64, count: usize) -> Vec<i64> {
        let mut generator = Reference::from_seed(seed);
        (0..count).map(|_| generator.int_between(1, 6)).collect()
    }

    /// random.md, "`next_int_between` rolls a die from a seed".
    #[test]
    fn random_reference_rolls_a_die_from_a_seed() {
        assert_eq!(rolls(42, 10), [2, 2, 1, 1, 5, 1, 2, 3, 2, 3]);
        assert_eq!(rolls(2026, 5), [2, 6, 3, 1, 4]);
    }

    /// random.md, "a seed always gives the same numbers".
    #[test]
    fn random_reference_a_seed_always_gives_the_same_numbers() {
        assert_eq!(rolls(42, 1), rolls(42, 1));
        assert_eq!(Reference::from_seed(42), Reference::from_seed(42));
        assert_ne!(Reference::from_seed(42), Reference::from_seed(43));
        // Two's complement: a negative seed is a valid, distinct state.
        assert_eq!(Reference::from_seed(-1), Reference(u64::MAX));
    }

    /// random.md, "`next_int_between` treats its bounds alike in either
    /// order".
    #[test]
    fn random_reference_treats_its_bounds_alike_in_either_order() {
        let generator = Reference::from_seed(7);
        let (mut forward, mut backward) = (generator, generator);
        assert_eq!(forward.int_between(1, 6), backward.int_between(6, 1));
        assert_eq!(forward, backward);
        let mut zero = generator;
        assert_eq!(zero.int_between(0, 0), 0);
        // A span of every Int is 0 modulo 2⁶⁴: one step, read as an Int.
        let (mut whole, mut raw) = (generator, generator);
        let bits = raw.step();
        assert_eq!(
            whole.int_between(i64::MIN, i64::MAX),
            i64::from_ne_bytes(bits.to_ne_bytes())
        );
    }

    /// random.md's `split` and "Seeded streams": the first child of state
    /// 42 starts at 814261035509592648 (D-1048, D-1096), as `derive` does
    /// (I-J).
    #[test]
    fn random_reference_derives_the_seeded_streams_vector() {
        let parent = Reference::from_seed(42);
        assert_eq!(parent.derived(1), Reference(814_261_035_509_592_648));
        for n in [1, 2, 3, u64::MAX] {
            assert_eq!(parent.derived(n).0, context::derive(42, n), "n = {n}");
        }
    }

    /// random.md, "`next_fraction` draws from a seed".
    #[test]
    fn random_reference_next_fraction_draws_from_a_seed() {
        let mut generator = Reference::from_seed(42);
        let a = generator.fraction();
        let b = generator.fraction();
        assert_eq!(a.to_bits(), 0.741_564_878_771_823_3_f64.to_bits());
        assert_eq!(b.to_bits(), 0.159_910_392_876_920_1_f64.to_bits());
    }

    /// random.md, "`int_between` stays within its bounds" and "fraction is at
    /// least zero and below one": the host functions' properties hold for
    /// the reference over host-seeded generators.
    #[test]
    fn random_reference_host_draws_stay_within_their_bounds() {
        for _ in 0..100 {
            let mut generator = Reference(cheby_random_host_bits());
            let roll = generator.int_between(1, 6);
            assert!((1..=6).contains(&roll), "{roll}");
            assert_eq!(generator.int_between(0, 0), 0);
            let reversed = generator.int_between(10, 5);
            assert!((5..=10).contains(&reversed), "{reversed}");
            let x = generator.fraction();
            assert!((0.0..1.0).contains(&x), "{x}");
        }
    }
}
