use std::sync::atomic::{AtomicU32, Ordering};

use crossbeam_queue::{ArrayQueue, SegQueue};
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
    Index { slot: self.slot, generation: self.generation.wrapping_add(1) }
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
  fn test_generation_wrapping() {
    // Create an index with max generation
    let index = Index { slot: 5, generation: u32::MAX };

    let next = index.next_generation();
    assert_eq!(next.slot(), 5);
    assert_eq!(next.generation(), 0, "Generation should wrap to 0");
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
}
