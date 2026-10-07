//! A set of token kinds, as a bit set over the first 128 kinds.

use crate::SyntaxKind;

/// A set of token kinds, used by the parser for lookahead and recovery.
///
/// Only token kinds fit: every token kind is below 128 (§3.1 of the
/// phase-A design). A node kind is never contained, and adding one is a
/// no-op.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct TokenSet(u128);

impl TokenSet {
    /// The empty set.
    pub const EMPTY: Self = Self(0);

    /// The set of `kinds`.
    #[must_use]
    pub const fn new(kinds: &[SyntaxKind]) -> Self {
        let mut bits = 0_u128;
        let mut rest = kinds;
        while let [first, tail @ ..] = rest {
            bits |= bit(*first);
            rest = tail;
        }
        Self(bits)
    }

    /// The union of both sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether `kind` is in the set.
    #[must_use]
    pub const fn contains(self, kind: SyntaxKind) -> bool {
        self.0 & bit(kind) != 0
    }
}

/// The bit of `kind`, or 0 for a kind outside the set's range.
#[expect(
    clippy::as_conversions,
    reason = "widening `u16` to `u32` is lossless, and `From` is not callable in a `const fn`"
)]
const fn bit(kind: SyntaxKind) -> u128 {
    if !kind.is_token() {
        return 0;
    }
    match 1_u128.checked_shl(kind.to_u16() as u32) {
        Some(bit) => bit,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::TokenSet;
    use crate::SyntaxKind;

    #[test]
    fn contains_and_union() {
        let a = TokenSet::new(&[SyntaxKind::FN_KW, SyntaxKind::R_BRACE]);
        let b = TokenSet::new(&[SyntaxKind::EOF, SyntaxKind::WHITESPACE]);
        assert!(a.contains(SyntaxKind::FN_KW));
        assert!(a.contains(SyntaxKind::R_BRACE));
        assert!(!a.contains(SyntaxKind::EOF));
        let both = a.union(b);
        assert!(both.contains(SyntaxKind::EOF));
        assert!(both.contains(SyntaxKind::WHITESPACE));
        assert!(!TokenSet::EMPTY.contains(SyntaxKind::WHITESPACE));
    }

    #[test]
    fn nodes_are_never_contained() {
        let set = TokenSet::new(&[SyntaxKind::ERROR, SyntaxKind::SOURCE_FILE]);
        assert_eq!(set, TokenSet::EMPTY);
        assert!(!set.contains(SyntaxKind::ERROR));
    }
}
