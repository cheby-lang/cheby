//! What every program shares with its Cheby original: the generator, the
//! checksum's mixing step and a hasher with fixed keys.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, DefaultHasher};

/// Keeps the checksum small, as `modulus` in the Cheby programs.
pub(crate) const MODULUS: i64 = 1_000_000_007;

/// Keys are drawn below this bound where a program says `key_space`.
pub(crate) const KEY_SPACE: i64 = 1_000_000;

/// A `HashMap` whose hasher has fixed keys, so a run's work does not
/// depend on a random seed. No checksum depends on its order.
pub(crate) type FixedMap<K, V> = HashMap<K, V, BuildHasherDefault<DefaultHasher>>;

/// The pseudo-random number after `seed`, from the Cheby programs' linear
/// congruential generator. For seeds below 2^31 nothing overflows.
pub(crate) fn next(seed: i64) -> i64 {
    seed.wrapping_mul(1_103_515_245)
        .wrapping_add(12_345)
        .rem_euclid(2_147_483_648)
}

/// Adds `n` to the checksum `total`.
pub(crate) fn mix(total: i64, n: i64) -> i64 {
    total.wrapping_mul(31).wrapping_add(n).rem_euclid(MODULUS)
}

/// A count as the Cheby programs' `Int`.
pub(crate) fn int(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generator_and_the_mixing_step_match_the_cheby_programs() {
        assert_eq!(next(42), 1_250_496_027);
        assert_eq!(mix(0, 5), 5);
        assert_eq!(mix(1, MODULUS), 31);
    }
}
