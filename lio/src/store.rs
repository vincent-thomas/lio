use std::sync::atomic::{AtomicU32, Ordering};

use crossbeam_queue::ArrayQueue;
use scc::hash_map::Entry;

use crate::registration::{Registration, StoredOp};

struct Record {
  registration: Registration,
  generation: u32,
}

#[derive(Copy, Clone, Debug)]
struct Index {
  generation: u32,
  slot: u32,
}

impl Index {
  /// Pack generation and slot into a single u64
  /// High 32 bits = generation, low 32 bits = slot
  pub fn as_u64(&self) -> u64 {
    ((self.generation as u64) << 32) | (self.slot as u64)
  }

  /// Extract index from a packed u64 ID
  pub fn from_u64(packed: u64) -> Self {
    Index {
      slot: (packed & 0xFFFFFFFF) as u32,
      generation: (packed >> 32) as u32,
    }
  }

  pub fn slot(&self) -> u32 {
    self.slot
  }

  pub fn generation(&self) -> u32 {
    self.generation
  }

  /// Create a new index with incremented generation
  fn next_generation(self) -> Self {
    let generation = self
      .generation
      .checked_add(1)
      .unwrap_or_else(|| panic!("integer overflow when computing next generation in arena slot. Old: {}, add: 1", self.generation));

    Index { slot: self.slot, generation }
  }
}

pub struct OpStore {
  store: scc::HashMap<u32, Record>,
  next_slot: AtomicU32,
  free_slots: ArrayQueue<Index>,
}

impl OpStore {
  #[cfg(test)]
  pub fn new() -> OpStore {
    Self::with_capacity(1024)
  }

  pub fn with_capacity(cap: usize) -> OpStore {
    let adjusted_capacity =
      cap.min(1_usize << (usize::BITS - 2)).next_power_of_two();

    assert!(
      adjusted_capacity == cap,
      "capacity provided was not power of 2, provided value = {cap}"
    );

    Self {
      store: scc::HashMap::with_capacity(cap),
      next_slot: AtomicU32::new(0),
      free_slots: ArrayQueue::new(cap),
    }
  }

  fn next_id(&self) -> Index {
    match self.free_slots.pop() {
      Some(freed_index) => {
        // Reuse freed slot with incremented generation
        freed_index.next_generation()
      }
      None => {
        // Allocate new slot with generation 0
        Index {
          slot: self.next_slot.fetch_add(1, Ordering::AcqRel),
          generation: 0,
        }
      }
    }
  }

  pub fn insert(&self, stored: StoredOp) -> u64 {
    self.insert_with(|_| stored)
  }

  pub fn insert_with<F>(&self, f: F) -> u64
  where
    F: FnOnce(u64) -> StoredOp,
  {
    let result = self.insert_with_try(|idx| Ok::<_, ()>(f(idx)));
    unsafe { result.unwrap_unchecked() }
  }

  pub fn insert_with_try<F, E>(&self, f: F) -> Result<u64, E>
  where
    F: FnOnce(u64) -> Result<StoredOp, E>,
  {
    let index = self.next_id();
    let stored = f(index.as_u64())?;
    let record = Record {
      registration: Registration::new(stored),
      generation: index.generation(),
    };

    // Insert should always succeed since we have a unique index
    // If the slot exists, it means we have a generation counter bug.
    // upsert is used so we can assert.
    let previous = self.store.upsert_sync(index.slot(), record);
    assert!(
      previous.is_none(),
      "OpStore::insert: slot {} already exists (generation {}), previous generation: {:?}",
      index.slot(),
      index.generation(),
      previous.map(|r| r.generation)
    );

    Ok(index.as_u64())
  }

  pub fn remove(&self, id: u64) -> bool {
    let index = Index::from_u64(id);

    match self.store.entry_sync(index.slot()) {
      Entry::Occupied(entry) => {
        // Check generation while holding the entry
        if entry.get().generation == index.generation() {
          // Generation matches - remove it
          let _ = entry.remove();
          // Return slot to free list
          let _ = self.free_slots.push(index);
          true
        } else {
          // Stale ID - generation mismatch, don't remove
          false
        }
      }
      Entry::Vacant(_) => {
        // Slot doesn't exist
        false
      }
    }
  }

  pub fn get_mut<F, R>(&self, id: u64, f: F) -> Option<R>
  where
    F: FnOnce(&mut Registration) -> R,
  {
    let index = Index::from_u64(id);

    let mut entry = self.store.get_sync(&index.slot())?;

    // Verify generation matches
    if entry.generation == index.generation() {
      Some(f(&mut entry.registration))
    } else {
      None
    }
  }

  pub fn get<F, R>(&self, id: u64, f: F) -> Option<R>
  where
    F: FnOnce(&Registration) -> R,
  {
    let index = Index::from_u64(id);

    self
      .store
      .read_sync(&index.slot(), |_, rec| {
        if rec.generation == index.generation() {
          Some(f(&rec.registration))
        } else {
          None
        }
      })
      .flatten()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashSet;
  use std::sync::Arc;
  use std::thread;

  // Helper to create a dummy StoredOp
  fn dummy_stored_op() -> StoredOp {
    use crate::op::nop::Nop;
    StoredOp::new_waker(Nop)
  }

  #[test]
  fn test_basic_insert_and_remove() {
    let store = OpStore::new();
    let id = store.insert(dummy_stored_op());

    assert!(store.remove(id));
    assert!(!store.remove(id)); // Second remove should fail
  }

  #[test]
  fn test_sequential_ids_are_unique() {
    let store = OpStore::new();
    let mut ids = HashSet::new();

    for _ in 0..1000 {
      let id = store.insert(dummy_stored_op());
      assert!(ids.insert(id), "Generated duplicate ID: {}", id);
    }
  }

  #[test]
  fn test_slot_reuse_increments_generation() {
    let store = OpStore::new();

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
    let store = OpStore::new();

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
    let store = OpStore::new();

    let id1 = store.insert(dummy_stored_op());
    store.remove(id1);

    let id2 = store.insert(dummy_stored_op());

    // Try to access with old ID - should return None
    let result = store.get_mut(id1, |_| true);
    assert!(result.is_none(), "Stale ID should return None");

    // New ID should work
    let result = store.get_mut(id2, |_| true);
    assert!(result.is_some());
  }

  #[test]
  fn test_get_mut_works() {
    let store = OpStore::new();
    let id = store.insert(dummy_stored_op());

    let result = store.get_mut(id, |registration| {
      // Verify we can mutate through the closure
      let _m: &mut _ = registration;
      42
    });

    assert_eq!(result, Some(42));
  }

  #[test]
  #[should_panic]
  fn test_generation_panic() {
    // Create an index with max generation
    let index = Index { slot: 5, generation: u32::MAX };

    let _ = index.next_generation();
  }

  #[test]
  fn test_index_packing_unpacking() {
    let index = Index { generation: 0x12345678, slot: 0xABCDEF01 };

    let packed = index.as_u64();
    let unpacked = Index::from_u64(packed);

    assert_eq!(unpacked.generation(), index.generation());
    assert_eq!(unpacked.slot(), index.slot());
  }

  #[test]
  fn test_concurrent_insertions() {
    let store = Arc::new(OpStore::new());
    let num_threads = 10;
    let ops_per_thread = 100;

    let handles: Vec<_> = (0..num_threads)
      .map(|_| {
        let store = Arc::clone(&store);
        thread::spawn(move || {
          let mut ids = Vec::new();
          for _ in 0..ops_per_thread {
            let id = store.insert_with(|_| dummy_stored_op());
            ids.push(id);
          }
          ids
        })
      })
      .collect();

    let mut all_ids = HashSet::new();
    for handle in handles {
      let ids = handle.join().unwrap();
      for id in ids {
        assert!(all_ids.insert(id), "Duplicate ID generated: {}", id);
      }
    }

    assert_eq!(all_ids.len(), num_threads * ops_per_thread);
  }

  #[test]
  fn test_concurrent_insert_and_remove() {
    let store = Arc::new(OpStore::new());
    let num_threads = 8;
    let cycles = 50;

    let handles: Vec<_> = (0..num_threads)
      .map(|_| {
        let store = Arc::clone(&store);
        thread::spawn(move || {
          for _ in 0..cycles {
            let id = store.insert_with(|_| dummy_stored_op());
            assert!(store.remove(id));
          }
        })
      })
      .collect();

    for handle in handles {
      handle.join().unwrap();
    }
  }

  #[test]
  fn test_concurrent_reuse() {
    let store = Arc::new(OpStore::new());
    let num_threads = 10;
    let ops_per_thread = 50;

    // Pre-allocate and free some IDs to populate free list
    let mut initial_ids = Vec::new();
    for _ in 0..20 {
      let id = store.insert(dummy_stored_op());
      initial_ids.push(id);
    }
    for id in initial_ids {
      store.remove(id);
    }

    let handles: Vec<_> = (0..num_threads)
      .map(|_| {
        let store = Arc::clone(&store);
        thread::spawn(move || {
          for _ in 0..ops_per_thread {
            let id = store.insert(dummy_stored_op());

            // Sometimes remove immediately, sometimes keep
            if id % 2 == 0 {
              store.remove(id);
            }
          }
        })
      })
      .collect();

    for handle in handles {
      handle.join().unwrap();
    }
  }

  #[test]
  fn test_concurrent_get_mut() {
    let store = Arc::new(OpStore::new());
    let num_readers = 5;
    let num_writers = 3;

    // Insert some operations
    let mut ids = Vec::new();
    for _ in 0..100 {
      let id = store.insert_with(|_| dummy_stored_op());
      ids.push(id);
    }
    let ids = Arc::new(ids);

    let mut handles = Vec::new();

    // Spawn readers
    for _ in 0..num_readers {
      let store = Arc::clone(&store);
      let ids = Arc::clone(&ids);
      handles.push(thread::spawn(move || {
        for _ in 0..1000 {
          let id = ids[fastrand::usize(..ids.len())];
          let _ = store.get_mut(id, |_| {
            // Just read access
            thread::yield_now();
          });
        }
      }));
    }

    // Spawn writers that remove and re-add
    for _ in 0..num_writers {
      let store = Arc::clone(&store);
      let ids = Arc::clone(&ids);
      handles.push(thread::spawn(move || {
        for _ in 0..100 {
          let idx = fastrand::usize(..ids.len());
          let old_id = ids[idx];

          if store.remove(old_id) {
            let _id = store.insert_with(|_| dummy_stored_op());
          }
        }
      }));
    }

    for handle in handles {
      handle.join().unwrap();
    }
  }

  #[test]
  fn test_high_contention_same_slots() {
    let store = Arc::new(OpStore::new());
    let num_threads = 16;
    let cycles = 100;

    // Create just a few IDs so threads will contend for the same slots
    let mut initial_ids = Vec::new();
    for _ in 0..5 {
      let id = store.insert(dummy_stored_op());
      initial_ids.push(id);
    }

    // Remove them to populate free list
    for id in initial_ids {
      store.remove(id);
    }

    let handles: Vec<_> = (0..num_threads)
      .map(|_| {
        let store = Arc::clone(&store);
        thread::spawn(move || {
          let mut local_ids = Vec::new();

          for _ in 0..cycles {
            // Allocate
            let id = store.insert(dummy_stored_op());
            local_ids.push(id);

            // Sometimes free an old one
            if local_ids.len() > 3 && fastrand::bool() {
              let old_id = local_ids.remove(0);
              store.remove(old_id);
            }
          }

          // Cleanup
          for id in local_ids {
            store.remove(id);
          }
        })
      })
      .collect();

    for handle in handles {
      handle.join().unwrap();
    }
  }

  #[test]
  fn test_stress_alternating_alloc_free() {
    let store = Arc::new(OpStore::new());
    let num_threads = 8;

    let handles: Vec<_> = (0..num_threads)
      .map(|thread_id| {
        let store = Arc::clone(&store);
        thread::spawn(move || {
          for i in 0..1000 {
            let id = store.insert(dummy_stored_op());

            // Alternate: odd threads hold longer, even threads free immediately
            if thread_id % 2 == 0 || i % 10 == 0 {
              store.remove(id);
            }
          }
        })
      })
      .collect();

    for handle in handles {
      handle.join().unwrap();
    }
  }

  #[test]
  fn test_no_id_collision_after_many_operations() {
    let store = Arc::new(OpStore::new());
    let total_ops = 10000;

    let mut active_ids = HashSet::new();

    for i in 0..total_ops {
      let id = store.insert(dummy_stored_op());
      assert!(active_ids.insert(id), "ID collision at iteration {}: {}", i, id);

      // Periodically remove some IDs
      if i % 100 == 0 && !active_ids.is_empty() {
        let to_remove: Vec<_> = active_ids.iter().take(10).copied().collect();
        for id in to_remove {
          store.remove(id);
          active_ids.remove(&id);
        }
      }
    }
  }

  #[test]
  fn test_aba_protection() {
    let store = OpStore::new();

    // Thread 1: Get ID, insert, remove
    let id1 = store.insert(dummy_stored_op());
    let index1 = Index::from_u64(id1);
    store.remove(id1);

    // Thread 2: Gets the recycled slot (same slot, different generation)
    let id2 = store.insert(dummy_stored_op());
    let index2 = Index::from_u64(id2);

    assert_eq!(index1.slot(), index2.slot(), "Slot should be reused");
    assert_ne!(
      index1.generation(),
      index2.generation(),
      "Generation should differ"
    );

    // Thread 1: Tries to use stale ID - should fail (ABA protection)
    assert!(!store.remove(id1), "ABA: stale ID should be rejected");
    assert!(
      store.get_mut(id1, |_| ()).is_none(),
      "ABA: stale ID should return None"
    );

    // Thread 2: Uses current ID - should succeed
    assert!(store.remove(id2), "Current ID should work");
  }

  #[test]
  fn test_get_rejects_stale_generation() {
    let store = OpStore::new();

    // Insert and remove to create a stale ID
    let id1 = store.insert(dummy_stored_op());
    let index1 = Index::from_u64(id1);
    store.remove(id1);

    // Insert new operation with same slot but different generation
    let id2 = store.insert(dummy_stored_op());
    let index2 = Index::from_u64(id2);

    assert_eq!(index1.slot(), index2.slot(), "Slot should be reused");
    assert_ne!(
      index1.generation(),
      index2.generation(),
      "Generation should differ"
    );

    // store.get() with stale ID should return None (not access wrong generation)
    let result = store.get(id1, |_| 42);
    assert!(
      result.is_none(),
      "store.get() should reject stale ID, but got: {:?}",
      result
    );

    // store.get() with current ID should work
    let result = store.get(id2, |_| 42);
    assert_eq!(result, Some(42), "store.get() should work with current ID");
  }

  #[test]
  fn test_concurrent_aba_scenarios() {
    let store = Arc::new(OpStore::new());
    let num_threads = 8;

    let handles: Vec<_> = (0..num_threads)
      .map(|_| {
        let _store = store.clone();
        thread::spawn(move || {
          let mut stale_ids = Vec::new();

          for i in 0..100 {
            // Allocate
            let id = _store.insert(dummy_stored_op());
            if i < 5 {
              eprintln!("Thread {:?} inserted ID {}", std::thread::current().id(), id);
            }

            // Remove (makes it stale)
            let removed = _store.remove(id);
            if !removed {
              let index = Index::from_u64(id);

              // Try to find what generation is actually in the slot
              if let Some(entry) = _store.store.get_sync(&index.slot()) {
                eprintln!("Failed to remove id={} (slot={}, gen={}), actual gen in slot: {}",
                  id, index.slot(), index.generation(), entry.generation);
              } else {
                eprintln!("Failed to remove id={} (slot={}, gen={}), slot is EMPTY!",
                  id, index.slot(), index.generation());
              }
            }
            assert!(
              removed,
              "couldn't find item which was just inserted before. ID: {}", id
            );

            // Store as stale after removal
            if fastrand::bool() {
              stale_ids.push(id);
            }

            // Try to use stale IDs - should all fail
            for &stale_id in &stale_ids {
              assert!(
                !_store.remove(stale_id),
                "ABA bug: stale ID {} was accepted!",
                stale_id
              );
            }

            // Clear some stale IDs
            if stale_ids.len() > 10 {
              stale_ids.drain(0..5);
            }
          }
        })
      })
      .collect();

    for handle in handles {
      handle.join().unwrap();
    }
  }

  #[test]
  fn test_massive_parallel_churn() {
    let store = Arc::new(OpStore::new());
    let num_threads = 16;
    let duration = std::time::Duration::from_secs(2);
    let start = std::time::Instant::now();

    let handles: Vec<_> = (0..num_threads)
      .map(|_| {
        let _store = store.clone();
        thread::spawn(move || {
          let mut count = 0u64;
          let mut active = Vec::new();

          while start.elapsed() < duration {
            // Allocate some
            for _ in 0..5 {
              let id = _store
                .insert_with_try(|_| Ok::<_, ()>(dummy_stored_op()))
                .unwrap();
              active.push(id);
              count += 1;
            }

            // Free some
            if active.len() > 20 {
              for _ in 0..3 {
                let id = active.remove(fastrand::usize(..active.len()));
                _store.remove(id);
              }
            }

            // Randomly access some
            if !active.is_empty() && fastrand::bool() {
              let id = active[fastrand::usize(..active.len())];
              let _ = _store.get_mut(id, |_| ());
            }
          }

          // Cleanup
          for id in active {
            _store.remove(id);
          }

          count
        })
      })
      .collect();

    let total_ops: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    println!(
      "Completed {} operations across {} threads",
      total_ops, num_threads
    );
    assert!(total_ops > 0);
  }

  // Property-based tests using proptest
  use proptest::prelude::*;

  proptest! {
    /// Property: All generated IDs must be unique
    #[test]
    fn prop_ids_are_always_unique(count in 1usize..1000) {
      let store = OpStore::new();
      let mut ids = HashSet::new();

      for _ in 0..count {
        let id = store.insert(dummy_stored_op());
        prop_assert!(ids.insert(id), "Duplicate ID generated: {}", id);
      }

      prop_assert_eq!(ids.len(), count);
    }

    /// Property: Index packing and unpacking is bijective
    #[test]
    fn prop_index_packing_is_bijective(generation in any::<u32>(), slot in any::<u32>()) {
      let original = Index { generation, slot };
      let packed = original.as_u64();
      let unpacked = Index::from_u64(packed);

      prop_assert_eq!(unpacked.generation(), original.generation());
      prop_assert_eq!(unpacked.slot(), original.slot());
    }

    /// Property: Removing an ID twice should fail the second time
    #[test]
    fn prop_double_remove_always_fails(ops_before in 0usize..100) {
      let store = OpStore::new();

      // Insert some operations before
      for _ in 0..ops_before {
        store.insert(dummy_stored_op());
      }

      // Insert test operation
      let id = store.insert(dummy_stored_op());

      // First remove should succeed
      prop_assert!(store.remove(id));

      // Second remove should fail
      prop_assert!(!store.remove(id));
    }

    /// Property: get_mut returns None for removed IDs
    #[test]
    fn prop_get_mut_fails_after_remove(ops_count_exponent in 1u32..8, remove_index in 0usize..100) {
      let actual_count = 2usize.pow(ops_count_exponent);
      let store = OpStore::with_capacity(actual_count);
      let mut ids = Vec::new();

      for _ in 0..actual_count {
        ids.push(store.insert(dummy_stored_op()));
      }

      let remove_idx = remove_index % actual_count;
      let removed_id = ids[remove_idx];

      store.remove(removed_id);

      // Should return None after removal
      let result = store.get_mut(removed_id, |_| true);
      prop_assert!(result.is_none());
    }

    /// Property: Slot reuse always increments generation
    #[test]
    fn prop_slot_reuse_increments_generation(reuse_count in 1usize..50) {
      let store = OpStore::new();

      let mut prev_generation = 0u32;
      let mut prev_slot = 0u32;

      for i in 0..reuse_count {
        let id = store.insert(dummy_stored_op());
        let index = Index::from_u64(id);

        if i > 0 {
          // After first iteration, slot should be reused
          prop_assert_eq!(index.slot(), prev_slot, "Slot should be reused");
          prop_assert_eq!(
            index.generation(),
            prev_generation.wrapping_add(1),
            "Generation should increment"
          );
        }

        prev_slot = index.slot();
        prev_generation = index.generation();

        store.remove(id);
      }
    }

    /// Property: Stale IDs are always rejected
    #[test]
    fn prop_stale_ids_rejected(cycles in 1usize..20) {
      let store = OpStore::new();
      let mut stale_ids = Vec::new();

      for _ in 0..cycles {
        // Create and immediately remove to make it stale
        let id = store.insert(dummy_stored_op());
        store.remove(id);
        stale_ids.push(id);

        // Create a new ID (will reuse slot with new generation)
        let _new_id = store.insert(dummy_stored_op());

        // All stale IDs should be rejected
        for &stale_id in &stale_ids {
          prop_assert!(!store.remove(stale_id), "Stale ID should be rejected on remove");
          prop_assert!(
            store.get_mut(stale_id, |_| ()).is_none(),
            "Stale ID should be rejected on get_mut"
          );
        }
      }
    }

    /// Property: Generation wrapping works correctly
    #[test]
    fn prop_generation_wraps_correctly(slot in any::<u32>(), generation in any::<u32>()) {
      let index = Index { slot, generation };
      let next = index.next_generation();

      prop_assert_eq!(next.slot(), slot, "Slot should remain the same");
      prop_assert_eq!(
        next.generation(),
        generation.wrapping_add(1),
        "Generation should wrap correctly"
      );
    }

    /// Property: insert_with_try preserves error types
    #[test]
    fn prop_insert_with_try_error_handling(should_error: bool) {
      let store = OpStore::new();

      let result: Result<u64, &str> = store.insert_with_try(|_| {
        if should_error {
          Err("test error")
        } else {
          Ok(dummy_stored_op())
        }
      });

      if should_error {
        prop_assert!(result.is_err());
      } else {
        prop_assert!(result.is_ok());
      }
    }

    /// Property: IDs from concurrent insertions are unique
    #[test]
    fn prop_concurrent_ids_unique(thread_count in 2usize..10, ops_per_thread in 10usize..100) {
      let store = Arc::new(OpStore::new());
      let handles: Vec<_> = (0..thread_count)
        .map(|_| {
          let store = Arc::clone(&store);
          thread::spawn(move || {
            (0..ops_per_thread)
              .map(|_| store.insert(dummy_stored_op()))
              .collect::<Vec<_>>()
          })
        })
        .collect();

      let mut all_ids = HashSet::new();
      for handle in handles {
        let ids = handle.join().unwrap();
        for id in ids {
          prop_assert!(all_ids.insert(id), "Duplicate ID from concurrent insert: {}", id);
        }
      }

      prop_assert_eq!(all_ids.len(), thread_count * ops_per_thread);
    }

    /// Property: Active IDs can always be accessed via get_mut
    #[test]
    fn prop_active_ids_accessible(ops_count in 1usize..200) {
      let store = OpStore::new();
      let mut ids = Vec::new();

      for _ in 0..ops_count {
        ids.push(store.insert(dummy_stored_op()));
      }

      // All active IDs should be accessible
      for &id in &ids {
        let result = store.get_mut(id, |_| 42);
        prop_assert_eq!(result, Some(42), "Active ID should be accessible");
      }
    }

    /// Property: Interleaved insert/remove maintains uniqueness
    #[test]
    fn prop_interleaved_ops_maintain_uniqueness(
      operations in prop::collection::vec((any::<bool>(), any::<u8>()), 10..200)
    ) {
      let store = OpStore::new();
      let mut active_ids = Vec::new();
      let mut all_generated_ids = HashSet::new();

      for (should_insert, selector) in operations {
        if should_insert || active_ids.is_empty() {
          // Insert
          let id = store.insert(dummy_stored_op());
          prop_assert!(
            all_generated_ids.insert(id),
            "Generated duplicate ID: {}",
            id
          );
          active_ids.push(id);
        } else {
          // Remove
          let idx = (selector as usize) % active_ids.len();
          let id = active_ids.remove(idx);
          prop_assert!(store.remove(id), "Remove should succeed for active ID");
        }
      }
    }

    /// Property: Mixed operations with random IDs maintain consistency
    #[test]
    fn prop_random_operations_consistent(
      ops in prop::collection::vec((any::<u8>(), any::<u8>()), 50..300)
    ) {
      let store = OpStore::new();
      let mut active_ids = Vec::new();

      for (op_type, selector) in ops {
        match op_type % 3 {
          0 => {
            // Insert
            let id = store.insert(dummy_stored_op());
            active_ids.push(id);
          }
          1 if !active_ids.is_empty() => {
            // Remove
            let idx = (selector as usize) % active_ids.len();
            let id = active_ids.swap_remove(idx);
            prop_assert!(store.remove(id));
          }
          2 if !active_ids.is_empty() => {
            // Access
            let idx = (selector as usize) % active_ids.len();
            let id = active_ids[idx];
            let result = store.get_mut(id, |_| ());
            prop_assert!(result.is_some(), "Active ID should be accessible");
          }
          _ => {}
        }
      }

      // Verify all active IDs are still accessible
      for id in active_ids {
        prop_assert!(store.get_mut(id, |_| ()).is_some());
      }
    }

    /// Property: Capacity doesn't affect correctness
    #[test]
    fn prop_capacity_doesnt_affect_correctness(
      capacity_exp in 1u32..8,
      ops_count in 1usize..100
    ) {
      let cap = 2usize.pow(capacity_exp);
      let store = OpStore::with_capacity(cap);
      let mut ids = HashSet::new();

      for _ in 0..ops_count {
        let id = store.insert(dummy_stored_op());
        prop_assert!(ids.insert(id), "Duplicate ID with capacity {}: {}", cap, id);
      }

      prop_assert_eq!(ids.len(), ops_count);
    }

    /// Property: No ID collision even after wrapping scenarios
    #[test]
    fn prop_no_collision_with_wrapping(iterations in 1usize..100) {
      let store = OpStore::new();
      let mut all_current_ids = HashSet::new();

      for _ in 0..iterations {
        // Insert
        let id = store.insert(dummy_stored_op());
        prop_assert!(
          all_current_ids.insert(id),
          "ID collision detected: {}",
          id
        );

        // Immediately remove to force slot reuse
        store.remove(id);
        all_current_ids.remove(&id);

        // Insert again (should get same slot, different generation)
        let id2 = store.insert(dummy_stored_op());
        prop_assert!(
          all_current_ids.insert(id2),
          "ID collision after slot reuse: {}",
          id2
        );

        // Verify old ID is rejected
        prop_assert!(!store.remove(id), "Old ID should be rejected");
      }
    }
  }
}
