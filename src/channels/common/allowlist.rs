//! Mutable, share-by-clone allow list for IM channels.
//!
//! Generic over the platform's identity type (Discord `u64`, Slack
//! `String`, …). Wraps an `Arc<RwLock<HashSet<T>>>` so every clone
//! observes the same underlying set — runtime mutations made through
//! one handle are visible to every reader.
//!
//! Empty list is **deny-all** by construction. A freshly registered
//! adapter with no ids configured therefore accepts no inbound
//! traffic until an operator command (or sidecar file) populates it.

use std::collections::HashSet;
use std::hash::Hash;
use std::sync::{Arc, RwLock};

/// Shared, mutable set of allowed identifiers.
///
/// Cloning the handle is cheap (one `Arc` clone); every clone shares
/// the same underlying lock + set.
pub struct AllowList<T> {
    ids: Arc<RwLock<HashSet<T>>>,
}

impl<T> Clone for AllowList<T> {
    fn clone(&self) -> Self {
        Self {
            ids: self.ids.clone(),
        }
    }
}

impl<T: Eq + Hash> AllowList<T> {
    /// Construct an allow list seeded from any iterable of ids.
    ///
    /// Named `with_initial` (rather than `from_iter`) to avoid
    /// shadowing the standard [`std::iter::FromIterator::from_iter`]
    /// — the seeding semantics are identical, but this method is
    /// usually called with a one-off vector and discoverability via
    /// the trait isn't valuable here.
    pub fn with_initial(ids: impl IntoIterator<Item = T>) -> Self {
        Self {
            ids: Arc::new(RwLock::new(ids.into_iter().collect())),
        }
    }

    /// Return `true` iff `id` is in the list.
    ///
    /// Empty list returns `false` for every id — deny by default.
    ///
    /// # Panics
    ///
    /// Panics if the inner lock has been poisoned by a prior writer
    /// crash. Recovery is irrecoverable so the panic is the honest
    /// answer rather than silently keeping a corrupted set alive.
    pub fn is_allowed(&self, id: &T) -> bool {
        self.ids.read().expect("allowlist poisoned").contains(id)
    }

    /// Insert `id`. Returns `true` when the id was newly added,
    /// `false` when it was already present.
    ///
    /// # Panics
    ///
    /// See [`Self::is_allowed`] — same lock-poisoning rationale.
    pub fn insert(&self, id: T) -> bool {
        self.ids.write().expect("allowlist poisoned").insert(id)
    }

    /// Remove `id`. Returns `true` when an entry was removed, `false`
    /// when the id was not present.
    ///
    /// # Panics
    ///
    /// See [`Self::is_allowed`].
    pub fn remove(&self, id: &T) -> bool {
        self.ids.write().expect("allowlist poisoned").remove(id)
    }
}

impl<T: Eq + Hash + Clone> AllowList<T> {
    /// Snapshot the current set as an unsorted vector. Callers sort
    /// when display order matters; the lock is held only for the copy.
    ///
    /// # Panics
    ///
    /// See [`Self::is_allowed`].
    #[must_use]
    pub fn snapshot(&self) -> Vec<T> {
        self.ids
            .read()
            .expect("allowlist poisoned")
            .iter()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::AllowList;

    #[test]
    fn empty_list_denies_everything() {
        let list: AllowList<u64> = AllowList::with_initial(std::iter::empty());
        assert!(!list.is_allowed(&42));
    }

    #[test]
    fn populated_list_admits_listed_ids_only() {
        let list = AllowList::with_initial([1u64, 2, 3]);
        assert!(list.is_allowed(&2));
        assert!(!list.is_allowed(&4));
    }

    #[test]
    fn insert_returns_true_only_for_new_ids() {
        let list = AllowList::with_initial([1u64]);
        assert!(!list.insert(1));
        assert!(list.insert(2));
        assert!(list.is_allowed(&2));
    }

    #[test]
    fn remove_returns_true_only_when_present() {
        let list = AllowList::with_initial([1u64, 2]);
        assert!(list.remove(&1));
        assert!(!list.remove(&1));
        assert!(!list.is_allowed(&1));
    }

    #[test]
    fn clones_share_underlying_state() {
        let a = AllowList::with_initial([1u64]);
        let b = a.clone();
        b.insert(99);
        assert!(a.is_allowed(&99));
    }

    #[test]
    fn snapshot_yields_full_set() {
        let list = AllowList::with_initial([3u64, 1, 2]);
        let mut snap = list.snapshot();
        snap.sort_unstable();
        assert_eq!(snap, vec![1, 2, 3]);
    }
}
