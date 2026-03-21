//! Storage for in-flight I/O operations.
//!
//! This module provides [`OpStore`], a data structure for managing in-flight
//! I/O operations with O(1) access via slot-based indexing.
//!
//! # Design
//!
//! The store uses a generational index scheme where each ID is composed of:
//! - **Slot**: The location in the underlying Vec (low 32 bits)
//! - **Generation**: A counter to detect stale references (high 32 bits)
//!
//! When a slot is freed and reused, its generation is incremented. This ensures
//! that old IDs referring to the same slot are rejected (ABA protection).
//!
//! The store uses bump allocation with slot reuse for cache-friendly access.

use crate::registration::Registration;
use crate::slab::{Slab, SlabKey};

/// Store for in-flight I/O operations using contiguous memory.
///
/// Uses bump allocation with free-list reuse for O(1) operations and
/// cache-friendly memory layout. Generational indices prevent the ABA problem.
pub(crate) struct OpStore {
  slab: Slab<Registration>,
}

#[derive(Debug)]
pub(crate) struct StoreAtCapacity;

impl std::fmt::Display for StoreAtCapacity {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("StoreAtCapacity")
  }
}

impl std::error::Error for StoreAtCapacity {}

impl OpStore {
  /// Creates a new OpStore with default capacity (1024).
  #[cfg(test)]
  pub fn new() -> OpStore {
    Self::with_capacity(1024)
  }

  /// Creates a new OpStore with the specified capacity.
  ///
  /// # Parameters
  ///
  /// - `cap`: The maximum number of concurrent operations.
  pub fn with_capacity(cap: usize) -> OpStore {
    Self { slab: Slab::new(cap) }
  }

  /// Inserts an operation into the store and returns its ID.
  ///
  /// # Panics
  ///
  /// Panics if the store is at capacity.
  pub fn insert(&mut self, reg: Registration) -> u64 {
    self.try_insert(reg).expect("at capacity")
  }

  /// Inserts an operation into the store and returns its ID.
  pub fn try_insert(
    &mut self,
    reg: Registration,
  ) -> Result<u64, StoreAtCapacity> {
    self.slab.insert(reg).map(|key| key.as_u64()).ok_or(StoreAtCapacity)
  }

  /// Removes an operation from the store.
  ///
  /// Returns `true` if the operation was found and removed, `false` if the ID
  /// was invalid (not found, already removed, or stale generation).
  pub fn remove(&mut self, id: u64) -> bool {
    self.slab.remove(SlabKey::from_u64(id))
  }

  /// Gets mutable access to an operation's registration.
  ///
  /// Returns `None` if the ID is invalid or refers to a stale generation.
  pub fn get_mut(&mut self, id: u64) -> Option<&mut Registration> {
    self.slab.get_mut(SlabKey::from_u64(id))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::slab::SlabKey;
  use std::collections::HashSet;

  // Helper to create a dummy StoredOp
  fn dummy_stored_op() -> Registration {
    use std::task::{RawWaker, RawWakerVTable, Waker};

    unsafe fn clone(_: *const ()) -> RawWaker {
      RawWaker::new(std::ptr::null(), &VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}

    const VTABLE: RawWakerVTable =
      RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    let raw_waker = RawWaker::new(std::ptr::null(), &VTABLE);
    // SAFETY: The vtable functions are valid no-ops
    let waker = unsafe { Waker::from_raw(raw_waker) };

    Registration::new_waker(waker)
  }

  #[test]
  fn test_basic_insert_and_remove() {
    let mut store = OpStore::new();
    let id = store.insert(dummy_stored_op());

    assert!(store.remove(id));
    assert!(!store.remove(id)); // Second remove should fail
  }

  #[test]
  fn test_sequential_ids_are_unique() {
    let mut store = OpStore::new();
    let mut ids = HashSet::new();

    for _ in 0..1000 {
      let id = store.insert(dummy_stored_op());
      assert!(ids.insert(id), "Generated duplicate ID: {}", id);
    }
  }

  #[test]
  fn test_slot_reuse_increments_generation() {
    let mut store = OpStore::new();

    // Get first ID (generation 0, slot 0)
    let id1 = store.insert(dummy_stored_op());
    let key1 = SlabKey::from_u64(id1);
    assert_eq!(key1.generation(), 0);
    assert_eq!(key1.slot(), 0);

    store.remove(id1);

    // Get next ID - should reuse slot 0 with generation 1
    let id2 = store.insert(dummy_stored_op());
    let key2 = SlabKey::from_u64(id2);
    assert_eq!(key2.slot(), 0, "Slot should be reused");
    assert_eq!(key2.generation(), 1, "Generation should increment");
  }

  #[test]
  fn test_stale_id_rejected_on_remove() {
    let mut store = OpStore::new();

    let id1 = store.insert(dummy_stored_op());
    store.remove(id1);

    // Get new ID with same slot but different generation
    let id2 = store.insert(dummy_stored_op());

    // Try to remove with old ID - should fail
    assert!(!store.remove(id1), "Stale ID should be rejected");

    // New ID should still work
    assert!(store.remove(id2));
  }

  #[test]
  fn test_stale_id_rejected_on_get_mut() {
    let mut store = OpStore::new();

    let id1 = store.insert(dummy_stored_op());
    store.remove(id1);

    let id2 = store.insert(dummy_stored_op());

    // Try to access with old ID - should return None
    assert!(store.get_mut(id1).is_none(), "Stale ID should return None");

    // New ID should work
    assert!(store.get_mut(id2).is_some());
  }

  #[test]
  fn test_get_mut_works() {
    let mut store = OpStore::new();
    let id = store.insert(dummy_stored_op());

    let registration = store.get_mut(id);
    assert!(registration.is_some());
  }

  #[test]
  fn test_key_packing_unpacking() {
    let key = SlabKey::from_u64(((123u64) << 32) | 42);
    let packed = key.as_u64();
    let unpacked = SlabKey::from_u64(packed);

    assert_eq!(unpacked.slot(), 42);
    assert_eq!(unpacked.generation(), 123);
  }

  #[test]
  fn test_capacity_limit() {
    let mut store = OpStore::with_capacity(4);

    // Fill to capacity
    let ids: Vec<_> = (0..4).map(|_| store.insert(dummy_stored_op())).collect();

    // Should panic on next insert
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      store.insert(dummy_stored_op());
    }));
    assert!(result.is_err(), "Should panic when over capacity");

    // Remove one and should work again
    store.remove(ids[0]);
    let _ = store.insert(dummy_stored_op()); // Should not panic
  }

  #[test]
  fn test_aba_protection() {
    let mut store = OpStore::with_capacity(4);

    // Allocate and free multiple times to cycle generations
    let id1 = store.insert(dummy_stored_op());
    store.remove(id1);

    let id2 = store.insert(dummy_stored_op());
    store.remove(id2);

    let id3 = store.insert(dummy_stored_op());

    // All IDs use slot 0 but have different generations
    let key1 = SlabKey::from_u64(id1);
    let key2 = SlabKey::from_u64(id2);
    let key3 = SlabKey::from_u64(id3);

    assert_eq!(key1.slot(), 0);
    assert_eq!(key2.slot(), 0);
    assert_eq!(key3.slot(), 0);

    assert_eq!(key1.generation(), 0);
    assert_eq!(key2.generation(), 1);
    assert_eq!(key3.generation(), 2);

    // Old IDs should not work
    assert!(store.get_mut(id1).is_none());
    assert!(store.get_mut(id2).is_none());
    assert!(store.get_mut(id3).is_some());
  }
}
