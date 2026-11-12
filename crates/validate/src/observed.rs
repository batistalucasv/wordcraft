//! Bounded observations: only a complete collection can supply negative membership evidence.
use crate::resolution::Reason;
use std::collections::HashSet;
use std::hash::Hash;

/// Whether the observed value covers its whole declared collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Completeness {
    Complete,
    Incomplete(Reason),
}

/// A retained value and the completeness of that exact collection.
#[derive(Clone, Debug)]
pub(crate) struct Observed<T> {
    value: T,
    completeness: Completeness,
}
impl<T: Default> Default for Observed<T> {
    fn default() -> Self {
        Self::complete(T::default())
    }
}
impl<T> Observed<T> {
    /// Construct a value from completed observation, including a genuinely empty set.
    pub(crate) fn complete(value: T) -> Self {
        Self { value, completeness: Completeness::Complete }
    }
    /// Attach a collection's local completion evidence; never use report-level incompleteness here.
    pub(crate) fn with_status(value: T, completeness: Completeness) -> Self {
        Self { value, completeness }
    }
    /// Retained prefix for monotone presence, duplicate and count conclusions.
    pub(crate) fn prefix(&self) -> &T {
        &self.value
    }
    /// Whether absence can be established from this collection.
    pub(crate) fn is_complete(&self) -> bool {
        self.completeness == Completeness::Complete
    }
    /// A local source loss cannot later be undone by merging another complete observation.
    pub(crate) fn invalidate(&mut self, reason: Reason) {
        self.completeness = Completeness::Incomplete(reason);
    }
    /// Transform values without discarding their completeness evidence.
    pub(crate) fn project<U>(&self, map: impl FnOnce(&T) -> U) -> Observed<U> {
        Observed { value: map(&self.value), completeness: self.completeness }
    }
}
impl<T> Observed<Vec<T>> {
    /// Bound collection at extraction, retaining a prefix and recording local loss.
    pub(crate) fn push_bounded(&mut self, value: T, cap: usize) -> bool {
        if self.value.len() >= cap {
            self.invalidate("collection fact budget");
            true
        } else {
            self.value.push(value);
            false
        }
    }
    /// Apply the shared allocation allowance while retaining the loss at its contributing source.
    pub(crate) fn retain_budget(&mut self, held: &mut usize, cap: usize) -> bool {
        let left = cap.saturating_sub(*held);
        let dropped = self.value.len() > left;
        if dropped {
            self.value.truncate(left);
            self.invalidate("package collection fact budget");
        }
        *held = held.saturating_add(self.value.len());
        dropped
    }
    /// Assemble already-budgeted facts in an explicitly chosen scope.
    pub(crate) fn append(&mut self, from: &Self)
    where
        T: Clone,
    {
        self.value.extend_from_slice(&from.value);
        if !from.is_complete() {
            self.completeness = from.completeness;
        }
    }
    /// Retained length for allocation and presence decisions only.
    pub(crate) fn len(&self) -> usize {
        self.value.len()
    }
}

/// Proof-bearing definition set. Its only constructor consumes a complete observation.
#[derive(Clone, Debug)]
pub(crate) struct CompleteSet<K>(HashSet<K>);
impl<K: Eq + Hash> Observed<HashSet<K>> {
    /// Incomplete sets cannot be converted into negative membership evidence.
    pub(crate) fn into_complete_set(self) -> Option<CompleteSet<K>> {
        if self.is_complete() { Some(CompleteSet(self.value)) } else { None }
    }
}
impl<K: Eq + Hash> CompleteSet<K> {
    /// A negative membership conclusion backed by complete definitions.
    pub(crate) fn missing(&self, key: &K) -> bool {
        !self.0.contains(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_observations_keep_prefix_evidence_but_cannot_prove_absence() {
        let mut items = Observed::complete(vec![1, 1]);
        assert!(items.push_bounded(2, 2));
        assert_eq!(items.prefix(), &[1, 1]);
        assert!(items.project(|v| v.iter().copied().collect::<HashSet<_>>()).into_complete_set().is_none());
        items.append(&Observed::complete(vec![3]));
        assert!(!items.is_complete());
        let set = Observed::complete(HashSet::from([1])).into_complete_set().unwrap();
        assert!(!set.missing(&1));
        assert!(set.missing(&2));
    }
}
