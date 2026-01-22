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
//! The store uses pre-allocated contiguous memory for cache-friendly access.

use std::collections::VecDeque;

use crate::registration::{Registration, StoredOp};

/// A slot in the store containing generation and optional entry.
struct Slot {
  /// Generation counter - incremented each time slot is reused
  generation: u32,
  /// The actual entry, None if slot is free
  entry: Option<Registration>,
}

/// A generational index combining a slot and generation counter.
///
/// The index is packed into a `u64`:
/// - High 32 bits: generation counter
/// - Low 32 bits: slot index
#[derive(Copy, Clone, Debug)]
struct Index {
  generation: u32,
  slot: u32,
}

impl Index {
  /// Packs generation and slot into a single u64.
  pub fn as_u64(&self) -> u64 {
    ((self.generation as u64) << 32) | (self.slot as u64)
  }

  /// Extracts an index from a packed u64 ID.
  pub fn from_u64(packed: u64) -> Self {
    Index {
      slot: (packed & 0xFFFFFFFF) as u32,
      generation: (packed >> 32) as u32,
    }
  }

  /// Returns the slot index.
  pub fn slot(&self) -> u32 {
    self.slot
  }

  /// Returns the generation counter.
  pub fn generation(&self) -> u32 {
    self.generation
  }
}

/// Store for in-flight I/O operations using contiguous memory.
///
/// Uses a pre-allocated Vec for O(1) indexed access and cache-friendly
/// memory layout. Generational indices prevent the ABA problem.
pub struct OpStore {
  /// Pre-allocated slots with generation tracking
  slots: Vec<Slot>,
  /// Free slot indices available for reuse
  free_list: VecDeque<u32>,
  /// Next slot to use when free_list is empty
  next_slot: u32,
  /// Maximum capacity
  capacity: u32,
}

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
    let capacity = cap.min(u32::MAX as usize) as u32;

    // Pre-allocate all slots
    let slots = (0..capacity)
      .map(|_| Slot { generation: 0, entry: None })
      .collect();

    Self {
      slots,
      free_list: VecDeque::with_capacity(cap),
      next_slot: 0,
      capacity,
    }
  }

  /// Allocates the next available slot index and generation.
  fn next_id(&mut self) -> Option<Index> {
    // First try to reuse a freed slot
    if let Some(slot_idx) = self.free_list.pop_front() {
      let slot = &self.slots[slot_idx as usize];
      // Generation was already incremented when slot was freed
      return Some(Index { slot: slot_idx, generation: slot.generation });
    }

    // Allocate a new slot if within capacity
    if self.next_slot < self.capacity {
      let slot_idx = self.next_slot;
      self.next_slot += 1;
      return Some(Index { slot: slot_idx, generation: 0 });
    }

    // Out of capacity
    None
  }

  /// Inserts an operation into the store and returns its ID.
  ///
  /// # Panics
  ///
  /// Panics if the store is at capacity.
  pub fn insert(&mut self, stored: StoredOp) -> u64 {
    let index = self.next_id().expect("OpStore: out of capacity");

    let slot = &mut self.slots[index.slot() as usize];
    debug_assert!(
      slot.entry.is_none(),
      "OpStore: slot {} should be empty",
      index.slot()
    );
    debug_assert_eq!(
      slot.generation,
      index.generation(),
      "OpStore: generation mismatch"
    );

    slot.entry = Some(Registration::new(stored));

    index.as_u64()
  }

  /// Removes an operation from the store.
  ///
  /// Returns `true` if the operation was found and removed, `false` if the ID
  /// was invalid (not found, already removed, or stale generation).
  pub fn remove(&mut self, id: u64) -> bool {
    let index = Index::from_u64(id);

    if index.slot() >= self.capacity {
      return false;
    }

    let slot = &mut self.slots[index.slot() as usize];

    // Check generation matches and slot is occupied
    if slot.generation == index.generation() && slot.entry.is_some() {
      // Remove the entry
      slot.entry = None;
      // Increment generation for next use (ABA protection)
      slot.generation = slot.generation.wrapping_add(1);
      // Return slot to free list
      self.free_list.push_back(index.slot());
      true
    } else {
      false
    }
  }

  /// Gets mutable access to an operation's registration.
  ///
  /// Returns `None` if the ID is invalid or refers to a stale generation.
  pub fn get_mut(&mut self, id: u64) -> Option<&mut Registration> {
    let index = Index::from_u64(id);

    if index.slot() >= self.capacity {
      return None;
    }

    let slot = &mut self.slots[index.slot() as usize];

    if slot.generation == index.generation() {
      slot.entry.as_mut()
    } else {
      None
    }
  }

  /// Gets read-only access to an operation's registration.
  ///
  /// Returns `None` if the ID is invalid or refers to a stale generation.
  pub fn get(&self, id: u64) -> Option<&Registration> {
    let index = Index::from_u64(id);

    if index.slot() >= self.capacity {
      return None;
    }

    let slot = &self.slots[index.slot() as usize];

    if slot.generation == index.generation() {
      slot.entry.as_ref()
    } else {
      None
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashSet;

  // Helper to create a dummy StoredOp
  fn dummy_stored_op() -> StoredOp {
    use crate::api::ops::Nop;
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
    let waker = unsafe { Waker::from_raw(raw_waker) };

    StoredOp::new_waker(Box::new(Nop), waker)
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
    let index1 = Index::from_u64(id1);
    assert_eq!(index1.generation(), 0);
    assert_eq!(index1.slot(), 0);

    store.remove(id1);

    // Get next ID - should reuse slot 0 with generation 1
    let id2 = store.insert(dummy_stored_op());
    let index2 = Index::from_u64(id2);
    assert_eq!(index2.slot(), 0, "Slot should be reused");
    assert_eq!(index2.generation(), 1, "Generation should increment");
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
  fn test_get_works() {
    let mut store = OpStore::new();
    let id = store.insert(dummy_stored_op());

    let registration = store.get(id);
    assert!(registration.is_some());
  }

  #[test]
  fn test_index_packing_unpacking() {
    let index = Index { slot: 42, generation: 123 };
    let packed = index.as_u64();
    let unpacked = Index::from_u64(packed);

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
    let idx1 = Index::from_u64(id1);
    let idx2 = Index::from_u64(id2);
    let idx3 = Index::from_u64(id3);

    assert_eq!(idx1.slot(), 0);
    assert_eq!(idx2.slot(), 0);
    assert_eq!(idx3.slot(), 0);

    assert_eq!(idx1.generation(), 0);
    assert_eq!(idx2.generation(), 1);
    assert_eq!(idx3.generation(), 2);

    // Old IDs should not work
    assert!(store.get(id1).is_none());
    assert!(store.get(id2).is_none());
    assert!(store.get(id3).is_some());
  }
}
