//! Where the generator's choices come from: a seeded generator for the
//! stable test, or the fuzzer's bytes.
//!
//! Every choice is "a number below `n`". The first option of a choice is
//! always the simplest one, and a byte source that has run out answers 0,
//! so a short input gives a small program and generation always ends.

/// The source of every choice the generator makes.
#[derive(Debug, Clone)]
pub(crate) enum Source {
    /// `SplitMix64` (as `bench/gen`'s `Rng`), from a seed.
    Seeded(u64),
    /// The fuzzer's bytes, read in order; 0 once they run out.
    Bytes { data: Vec<u8>, pos: usize },
}

impl Source {
    fn next_word(&mut self, bytes: usize) -> u64 {
        match self {
            Self::Seeded(state) => {
                *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = *state;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^ (z >> 31)
            }
            Self::Bytes { data, pos } => {
                let mut value = 0u64;
                for _ in 0..bytes {
                    let byte = data.get(*pos).copied().unwrap_or(0);
                    *pos = pos.saturating_add(1);
                    value = (value << 8) | u64::from(byte);
                }
                value
            }
        }
    }

    /// A number in `0..n`, or 0 if `n` is 0 or 1.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        let bytes = match n {
            0..=256 => 1,
            257..=65_536 => 2,
            _ => 4,
        };
        let bound = u64::try_from(n).unwrap_or(u64::MAX);
        let value = self.next_word(bytes).checked_rem(bound).unwrap_or(0);
        usize::try_from(value).unwrap_or(0)
    }

    /// True with about `percent` percent probability; never true for a
    /// byte source that has run out.
    pub(crate) fn chance(&mut self, percent: usize) -> bool {
        self.below(100) >= 100usize.saturating_sub(percent)
    }

    /// A number in `low..=high` (`low` if `high < low`).
    pub(crate) fn range(&mut self, low: usize, high: usize) -> usize {
        low.saturating_add(self.below(high.saturating_sub(low).saturating_add(1)))
    }

    /// One of `items` (the first for an exhausted byte source).
    pub(crate) fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        items.get(self.below(items.len()))
    }

    /// An index chosen by `weights` (each weight the relative chance of
    /// its index); 0 when every weight is 0.
    pub(crate) fn weighted(&mut self, weights: &[usize]) -> usize {
        let total: usize = weights.iter().sum();
        if total == 0 {
            return 0;
        }
        let mut roll = self.below(total);
        for (index, weight) in weights.iter().enumerate() {
            if roll < *weight {
                return index;
            }
            roll = roll.saturating_sub(*weight);
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::Source;

    #[test]
    fn progen_source_is_deterministic() {
        let mut a = Source::Seeded(7);
        let mut b = Source::Seeded(7);
        for n in 1..200 {
            assert_eq!(a.below(n), b.below(n));
        }
    }

    #[test]
    fn progen_exhausted_bytes_choose_the_first_option() {
        let mut s = Source::Bytes {
            data: vec![3],
            pos: 0,
        };
        assert_eq!(s.below(10), 3);
        assert_eq!(s.below(10), 0);
        assert!(!s.chance(99));
        assert_eq!(s.weighted(&[0, 5, 5]), 1);
    }
}
