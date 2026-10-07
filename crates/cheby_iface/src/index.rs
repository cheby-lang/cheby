//! Dense `u32` ids and vectors indexed by them.
//!
//! [`IndexVec`] has no `Index` impl on purpose: a missing id is an
//! [`Ice`](crate::Ice) that the caller reports, never a panic (D-423).

use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;

use crate::Ice;

/// A dense id that fits in a `u32`.
pub trait Idx: Copy + Eq + Hash + fmt::Debug {
    /// The id with number `index`, or `None` if it does not fit a `u32`.
    fn from_usize(index: usize) -> Option<Self>;

    /// The id's number as a `usize`.
    fn index(self) -> usize;
}

/// Defines a `u32` newtype id with [`Idx`]:
///
/// ```
/// cheby_iface::newtype_index! {
///     /// A local variable of a body.
///     pub struct LocalId;
/// }
/// let id = LocalId::from_u32(3);
/// assert_eq!(format!("{id:?}"), "LocalId(3)");
/// ```
#[macro_export]
macro_rules! newtype_index {
    ($(#[$meta:meta])* $vis:vis struct $name:ident;) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        $vis struct $name(u32);

        impl $name {
            /// The id with this number.
            #[must_use]
            pub const fn from_u32(value: u32) -> Self {
                Self(value)
            }

            /// The id's number.
            #[must_use]
            pub const fn as_u32(self) -> u32 {
                self.0
            }
        }

        impl $crate::Idx for $name {
            fn from_usize(index: usize) -> ::core::option::Option<Self> {
                u32::try_from(index).ok().map(Self)
            }

            fn index(self) -> usize {
                // A `u32` always fits a `usize` on the 64-bit hosts the
                // toolchain supports.
                usize::try_from(self.0).unwrap_or(usize::MAX)
            }
        }

        impl ::core::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }
    };
}

/// A vector indexed by the id type `I`.
pub struct IndexVec<I: Idx, T> {
    raw: Vec<T>,
    _marker: PhantomData<fn(&I)>,
}

impl<I: Idx, T> IndexVec<I, T> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            raw: Vec::new(),
            _marker: PhantomData,
        }
    }

    /// A vector holding `raw`, whose element `n` gets id `n`.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `raw` has more elements than `I` can number.
    pub fn from_raw(raw: Vec<T>) -> Result<Self, Ice> {
        if !raw.is_empty() && I::from_usize(raw.len().saturating_sub(1)).is_none() {
            return Err(Ice::new("index vector too long for its id type"));
        }
        Ok(Self {
            raw,
            _marker: PhantomData,
        })
    }

    /// Appends `value` and returns its id.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if the vector already has `u32::MAX + 1` elements.
    pub fn push(&mut self, value: T) -> Result<I, Ice> {
        let id = I::from_usize(self.raw.len())
            .ok_or_else(|| Ice::new("index vector too long for its id type"))?;
        self.raw.push(value);
        Ok(id)
    }

    #[must_use]
    pub fn get(&self, index: I) -> Option<&T> {
        self.raw.get(index.index())
    }

    #[must_use]
    pub fn get_mut(&mut self, index: I) -> Option<&mut T> {
        self.raw.get_mut(index.index())
    }

    /// The element at `index`, or an [`Ice`] naming `what` if there is none.
    ///
    /// # Errors
    ///
    /// An [`Ice`] if `index` is out of range.
    pub fn try_get(&self, index: I, what: &str) -> Result<&T, Ice> {
        self.get(index)
            .ok_or_else(|| Ice::new(format!("no {what} with id {index:?}")))
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.raw.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.raw.iter()
    }

    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.raw.iter_mut()
    }

    /// The elements with their ids, in id order.
    pub fn iter_enumerated(&self) -> impl DoubleEndedIterator<Item = (I, &T)> + '_ {
        // Every element's position fits `I`, because `push` and `from_raw`
        // refuse longer vectors, so `filter_map` drops nothing.
        self.raw
            .iter()
            .enumerate()
            .filter_map(|(n, value)| I::from_usize(n).map(|id| (id, value)))
    }

    /// The ids of the elements, in order.
    pub fn indices(&self) -> impl DoubleEndedIterator<Item = I> + '_ {
        (0..self.raw.len()).filter_map(I::from_usize)
    }

    /// The id the next `push` returns, if it fits.
    #[must_use]
    pub fn next_index(&self) -> Option<I> {
        I::from_usize(self.raw.len())
    }

    /// The elements as a slice, in id order.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.raw
    }

    #[must_use]
    pub fn into_raw(self) -> Vec<T> {
        self.raw
    }
}

impl<I: Idx, T> Default for IndexVec<I, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I: Idx, T: Clone> Clone for IndexVec<I, T> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            _marker: PhantomData,
        }
    }
}

impl<I: Idx, T: PartialEq> PartialEq for IndexVec<I, T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<I: Idx, T: Eq> Eq for IndexVec<I, T> {}

impl<I: Idx, T: Hash> Hash for IndexVec<I, T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<I: Idx, T: fmt::Debug> fmt::Debug for IndexVec<I, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter_enumerated()).finish()
    }
}

impl<'a, I: Idx, T> IntoIterator for &'a IndexVec<I, T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.raw.iter()
    }
}

impl<'a, I: Idx, T> IntoIterator for &'a mut IndexVec<I, T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.raw.iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::newtype_index! {
        struct TestId;
    }

    #[test]
    fn push_get_and_enumerate() {
        let mut v: IndexVec<TestId, &str> = IndexVec::new();
        assert!(v.is_empty());
        let a = v.push("a").unwrap();
        let b = v.push("b").unwrap();
        assert_eq!((a, b), (TestId::from_u32(0), TestId::from_u32(1)));
        assert_eq!(v.get(b), Some(&"b"));
        assert_eq!(v.get(TestId::from_u32(2)), None);
        assert!(v.try_get(TestId::from_u32(9), "thing").is_err());
        assert_eq!(v.next_index(), Some(TestId::from_u32(2)));
        let all: Vec<_> = v.iter_enumerated().collect();
        assert_eq!(all, [(a, &"a"), (b, &"b")]);
        assert_eq!(format!("{a:?}"), "TestId(0)");
        assert_eq!(b.as_u32(), 1);
        assert_eq!(v.indices().collect::<Vec<_>>(), [a, b]);
    }
}
