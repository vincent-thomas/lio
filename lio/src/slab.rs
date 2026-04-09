//! Generic slab allocator with bump allocation and slot reuse.
//!
//! Provides O(1) allocation and deallocation with:
//! - Bump allocation for fresh slots (cache-friendly sequential access)
//! - Free list for slot reuse (no malloc/free after warmup)
//! - Generational indices for ABA protection

use std::mem::MaybeUninit;

/// A generational index into a slab.
///
/// Combines a slot index with a generation counter to detect stale references.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SlabKey {
  slot: u32,
  generation: u32,
}

impl SlabKey {
  /// Pack into a u64 for storage in kernel user_data fields.
  #[inline]
  pub fn as_u64(self) -> u64 {
    ((self.generation as u64) << 32) | (self.slot as u64)
  }

  /// Unpack from a u64.
  #[inline]
  pub fn from_u64(packed: u64) -> Self {
    Self { slot: packed as u32, generation: (packed >> 32) as u32 }
  }

  /// Returns the slot index.
  #[inline]
  pub fn slot(self) -> u32 {
    self.slot
  }

  /// Returns the generation.
  #[inline]
  pub fn generation(self) -> u32 {
    self.generation
  }
}

/// Slot storage with generation tracking.
struct Slot<T> {
  generation: u32,
  value: MaybeUninit<T>,
  occupied: bool,
}

/// Generic slab allocator with generational indices.
///
/// Allocates items of type `T` in contiguous memory with O(1) insert/remove.
/// Uses bump allocation for new slots and a free list for reuse.
pub struct Slab<T> {
  slots: Vec<Slot<T>>,
  /// Free slot indices available for reuse (LIFO for cache locality)
  free_list: Vec<u32>,
  /// Next slot to bump-allocate
  next_slot: u32,
  /// Maximum capacity
  capacity: u32,
  /// Number of occupied slots
  len: u32,
}

impl<T> Slab<T> {
  /// Creates a new slab with the given capacity.
  ///
  /// Memory is allocated lazily as slots are used.
  pub fn new(capacity: usize) -> Self {
    let capacity = capacity.min(u32::MAX as usize) as u32;
    Self {
      slots: Vec::with_capacity(capacity as usize),
      free_list: Vec::new(),
      next_slot: 0,
      capacity,
      len: 0,
    }
  }

  /// Insert a value, returning its key.
  ///
  /// Returns `None` if at capacity.
  #[inline]
  pub fn insert(&mut self, value: T) -> Option<SlabKey> {
    // Try free list first (reuse)
    if let Some(slot_idx) = self.free_list.pop() {
      let slot = &mut self.slots[slot_idx as usize];
      debug_assert!(!slot.occupied);
      slot.value = MaybeUninit::new(value);
      slot.occupied = true;
      self.len += 1;
      return Some(SlabKey { slot: slot_idx, generation: slot.generation });
    }

    // Bump allocate
    if self.next_slot < self.capacity {
      let slot_idx = self.next_slot;
      self.next_slot += 1;

      // Grow slots vector if needed (lazy allocation)
      if self.slots.len() <= slot_idx as usize {
        self.slots.push(Slot {
          generation: 0,
          value: MaybeUninit::new(value),
          occupied: true,
        });
      } else {
        let slot = &mut self.slots[slot_idx as usize];
        slot.value = MaybeUninit::new(value);
        slot.occupied = true;
      }

      self.len += 1;
      return Some(SlabKey { slot: slot_idx, generation: 0 });
    }

    None // At capacity
  }

  /// Insert a value and return both its key and a mutable reference to it.
  ///
  /// Returns `None` if at capacity.
  #[inline]
  pub fn insert_get_mut(&mut self, value: T) -> Option<(SlabKey, &mut T)> {
    let key = self.insert(value)?;
    let value =
      self.get_mut(key).expect("just-inserted slab entry must be retrievable");
    Some((key, value))
  }

  /// Remove a value by key.
  ///
  /// Returns `true` if removed, `false` if key was invalid or stale.
  #[inline]
  pub fn remove(&mut self, key: SlabKey) -> bool {
    let slot = match self.slots.get_mut(key.slot as usize) {
      Some(s) => s,
      None => return false,
    };

    if slot.generation != key.generation || !slot.occupied {
      return false;
    }

    // SAFETY: slot.occupied is true, so value was initialized via insert()
    unsafe { slot.value.assume_init_drop() };
    slot.occupied = false;
    self.len -= 1;
    // Increment generation for ABA protection
    slot.generation = slot.generation.wrapping_add(1);
    // Return to free list
    self.free_list.push(key.slot);

    true
  }

  /// Remove a value by key and return it.
  ///
  /// Returns `None` if the key was invalid or stale.
  #[inline]
  pub fn remove_value(&mut self, key: SlabKey) -> Option<T> {
    let slot = self.slots.get_mut(key.slot as usize)?;

    if slot.generation != key.generation || !slot.occupied {
      return None;
    }

    slot.occupied = false;
    self.len -= 1;
    slot.generation = slot.generation.wrapping_add(1);
    self.free_list.push(key.slot);

    // SAFETY: slot.occupied was true, so value was initialized via insert().
    Some(unsafe { slot.value.assume_init_read() })
  }

  /// Get a mutable reference to a value by key.
  #[inline]
  pub fn get_mut(&mut self, key: SlabKey) -> Option<&mut T> {
    let slot = self.slots.get_mut(key.slot as usize)?;
    if slot.generation == key.generation && slot.occupied {
      // SAFETY: slot.occupied is true, so value was initialized via insert()
      Some(unsafe { slot.value.assume_init_mut() })
    } else {
      None
    }
  }

  #[inline]
  pub fn len(&self) -> usize {
    self.len as usize
  }

  #[inline]
  pub fn is_empty(&self) -> bool {
    self.len == 0
  }
}

impl<T> Drop for Slab<T> {
  fn drop(&mut self) {
    for slot in &mut self.slots {
      if slot.occupied {
        // SAFETY: slot.occupied is true, so value was initialized via insert()
        unsafe { slot.value.assume_init_drop() };
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_insert_and_get_mut() {
    let mut slab: Slab<i32> = Slab::new(10);
    let key = slab.insert(42).unwrap();
    assert_eq!(slab.get_mut(key), Some(&mut 42));
  }

  #[test]
  fn test_remove() {
    let mut slab: Slab<i32> = Slab::new(10);
    let key = slab.insert(42).unwrap();
    assert!(slab.remove(key));
    assert_eq!(slab.get_mut(key), None);
    // Double remove fails
    assert!(!slab.remove(key));
  }

  #[test]
  fn test_generation_increments() {
    let mut slab: Slab<i32> = Slab::new(10);

    let key1 = slab.insert(1).unwrap();
    slab.remove(key1);

    // Reuses slot 0 with incremented generation
    let key2 = slab.insert(2).unwrap();

    // Old key is stale
    assert_eq!(slab.get_mut(key1), None);
    assert_eq!(slab.get_mut(key2), Some(&mut 2));
  }

  #[test]
  fn test_capacity_limit() {
    let mut slab: Slab<i32> = Slab::new(2);
    let _k1 = slab.insert(1).unwrap();
    let _k2 = slab.insert(2).unwrap();
    assert!(slab.insert(3).is_none());
  }

  #[test]
  fn test_free_list_reuse() {
    let mut slab: Slab<i32> = Slab::new(10);

    let k1 = slab.insert(1).unwrap();
    let k2 = slab.insert(2).unwrap();
    let k3 = slab.insert(3).unwrap();

    // Remove middle
    slab.remove(k2);

    // Next insert reuses a slot
    let k4 = slab.insert(4).unwrap();

    // Others unchanged
    assert_eq!(slab.get_mut(k1), Some(&mut 1));
    assert_eq!(slab.get_mut(k3), Some(&mut 3));
    assert_eq!(slab.get_mut(k4), Some(&mut 4));
  }

  #[test]
  fn test_key_packing() {
    let key = SlabKey { slot: 0xDEAD, generation: 0xBEEF };
    let packed = key.as_u64();
    let unpacked = SlabKey::from_u64(packed);
    assert_eq!(key, unpacked);
  }

  #[test]
  fn test_drop_called() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let drop_count = Rc::new(RefCell::new(0));
    struct DropCounter(Rc<RefCell<i32>>);
    impl Drop for DropCounter {
      fn drop(&mut self) {
        *self.0.borrow_mut() += 1;
      }
    }

    {
      let mut slab: Slab<DropCounter> = Slab::new(10);
      let k1 = slab.insert(DropCounter(drop_count.clone())).unwrap();
      let _k2 = slab.insert(DropCounter(drop_count.clone())).unwrap();
      slab.remove(k1); // Should drop
      assert_eq!(*drop_count.borrow(), 1);
    }
    // Slab dropped, remaining item dropped
    assert_eq!(*drop_count.borrow(), 2);
  }
}
