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
pub(crate) struct Slot<T> {
  generation: u32,
  next_free: u32,
  state: T,
  occupied: bool,
}

/// Generic generational slot pool with stable per-slot state.
pub(crate) struct SlotPool<T, const TRACK_LEN: bool = true> {
  slots: Vec<Slot<T>>,
  /// Head of the intrusive free list, or `u32::MAX` when empty.
  free_head: u32,
  /// Next slot to bump-allocate
  next_slot: u32,
  /// Maximum capacity
  capacity: u32,
  /// Number of occupied slots
  len: u32,
}

impl<T, const TRACK_LEN: bool> SlotPool<T, TRACK_LEN> {
  pub(crate) fn with_capacity(
    capacity: usize,
    mut init_slot: impl FnMut() -> T,
  ) -> Self {
    let capacity = capacity.min(u32::MAX as usize) as u32;
    let mut slots = Vec::with_capacity(capacity as usize);
    for _ in 0..capacity {
      slots.push(Slot {
        generation: 0,
        next_free: u32::MAX,
        state: init_slot(),
        occupied: false,
      });
    }
    Self { slots, free_head: u32::MAX, next_slot: 0, capacity, len: 0 }
  }

  #[inline]
  pub(crate) fn allocate(&mut self) -> Option<(SlabKey, &mut T)> {
    self.allocate_with(|_| {})
  }

  /// Allocates a slot after `init` has prepared its state.
  ///
  /// The slot is only marked occupied once `init` returns, so a panicking
  /// `init` leaves the pool unchanged instead of holding an occupied slot
  /// whose state was never initialized.
  #[inline]
  pub(crate) fn allocate_with(
    &mut self,
    init: impl FnOnce(&mut T),
  ) -> Option<(SlabKey, &mut T)> {
    let slot_idx = if self.free_head != u32::MAX {
      self.free_head
    } else if self.next_slot < self.capacity {
      self.next_slot
    } else {
      return None;
    };
    init(&mut self.slots[slot_idx as usize].state);

    if self.free_head == slot_idx {
      self.free_head = self.slots[slot_idx as usize].next_free;
    } else {
      self.next_slot += 1;
    }
    let slot = &mut self.slots[slot_idx as usize];
    debug_assert!(!slot.occupied);
    slot.occupied = true;
    if TRACK_LEN {
      self.len += 1;
    }
    let key = SlabKey { slot: slot_idx, generation: slot.generation };
    Some((key, &mut slot.state))
  }

  #[inline]
  pub(crate) fn get_mut(&mut self, key: SlabKey) -> Option<&mut T> {
    let slot = self.slots.get_mut(key.slot() as usize)?;
    if slot.generation == key.generation() && slot.occupied {
      Some(&mut slot.state)
    } else {
      None
    }
  }

  /// Removes the slot `key` names, handing its state to `on_remove`.
  ///
  /// `on_remove` runs before the slot is released, so it must not panic.
  /// Move the value out and drop it after this returns: a destructor that
  /// panicked here would leave the slot occupied around a dropped value.
  #[inline]
  pub(crate) fn remove_with<R>(
    &mut self,
    key: SlabKey,
    on_remove: impl FnOnce(&mut T) -> R,
  ) -> Option<R> {
    let removed = self.retire_with(key, on_remove)?;
    self.release(key.slot());
    Some(removed)
  }

  /// Like [`remove_with`](Self::remove_with), but the slot is not reused
  /// until [`release`](Self::release)d: `key` stops resolving at once, while
  /// the slot's state stays untouched by allocation in the meantime.
  #[inline]
  pub(crate) fn retire_with<R>(
    &mut self,
    key: SlabKey,
    on_remove: impl FnOnce(&mut T) -> R,
  ) -> Option<R> {
    let slot = self.slots.get_mut(key.slot() as usize)?;
    if slot.generation != key.generation() || !slot.occupied {
      return None;
    }
    Some(self.retire_known_with(key, on_remove))
  }

  /// [`retire_with`](Self::retire_with) for a key already validated under
  /// the same exclusive pool borrow. `on_remove` must not panic.
  #[inline]
  pub(crate) fn retire_known_with<R>(
    &mut self,
    key: SlabKey,
    on_remove: impl FnOnce(&mut T) -> R,
  ) -> R {
    let slot = &mut self.slots[key.slot() as usize];
    assert!(slot.generation == key.generation() && slot.occupied);

    let removed = on_remove(&mut slot.state);
    slot.occupied = false;
    if TRACK_LEN {
      self.len -= 1;
    }
    slot.generation = slot.generation.wrapping_add(1);
    removed
  }

  /// Makes a slot retired by [`retire_with`](Self::retire_with) available
  /// for allocation again.
  #[inline]
  pub(crate) fn release(&mut self, slot_idx: u32) {
    let slot = &mut self.slots[slot_idx as usize];
    debug_assert!(!slot.occupied);
    slot.next_free = self.free_head;
    self.free_head = slot_idx;
  }

  /// Whether `allocate` would succeed.
  #[inline]
  pub(crate) fn has_free_slot(&self) -> bool {
    self.free_head != u32::MAX || self.next_slot < self.capacity
  }

  // Only the Unix backend implementations use the slab.
  #[cfg_attr(any(windows, not(feature = "backend_impls")), allow(dead_code))]
  #[inline]
  pub(crate) fn len(&self) -> usize {
    self.len as usize
  }

  /// Calls `f` on the state of every occupied slot.
  pub(crate) fn for_each_occupied(&mut self, mut f: impl FnMut(&mut T)) {
    for slot in &mut self.slots[..self.next_slot as usize] {
      if slot.occupied {
        f(&mut slot.state);
      }
    }
  }

  /// Whether `key` names an occupied slot.
  pub(crate) fn contains(&self, key: SlabKey) -> bool {
    self
      .slots
      .get(key.slot() as usize)
      .is_some_and(|slot| slot.occupied && slot.generation == key.generation())
  }

  /// Calls `f` with the key and state of every occupied slot.
  pub(crate) fn for_each_occupied_key(&self, mut f: impl FnMut(SlabKey, &T)) {
    for (index, slot) in
      self.slots[..self.next_slot as usize].iter().enumerate()
    {
      if slot.occupied {
        let key = SlabKey { slot: index as u32, generation: slot.generation };
        f(key, &slot.state);
      }
    }
  }

  /// Panics if the pool's bookkeeping is inconsistent.
  ///
  /// Returns the retired slots, in ascending order: neither occupied nor
  /// free, waiting for [`release`](Self::release).
  pub(crate) fn check_invariants(&self) -> Vec<u32> {
    assert_eq!(
      self.slots.len(),
      self.capacity as usize,
      "slot pool: slot count differs from the capacity"
    );
    assert!(
      self.next_slot <= self.capacity,
      "slot pool: bump pointer {} past the capacity {}",
      self.next_slot,
      self.capacity
    );
    let bumped = self.next_slot as usize;
    for (offset, slot) in self.slots[bumped..].iter().enumerate() {
      assert!(
        !slot.occupied && slot.generation == 0,
        "slot pool: slot {} past the bump pointer was used",
        bumped + offset
      );
    }

    let mut free = vec![false; bumped];
    let mut cursor = self.free_head;
    while cursor != u32::MAX {
      let index = cursor as usize;
      assert!(
        index < bumped,
        "slot pool: free list holds slot {index}, never allocated"
      );
      assert!(!free[index], "slot pool: free list reaches slot {index} twice");
      assert!(
        !self.slots[index].occupied,
        "slot pool: free list holds occupied slot {index}"
      );
      free[index] = true;
      cursor = self.slots[index].next_free;
    }

    if TRACK_LEN {
      let occupied = self.slots[..bumped].iter().filter(|s| s.occupied).count();
      assert_eq!(
        self.len as usize, occupied,
        "slot pool: len differs from the occupied slots"
      );
    }
    (0..bumped)
      .filter(|&index| !self.slots[index].occupied && !free[index])
      .map(|index| index as u32)
      .collect()
  }
}

/// Generic slab allocator with generational indices.
///
/// Allocates items of type `T` in contiguous memory with O(1) insert/remove.
/// Uses bump allocation for new slots and a free list for reuse.
#[cfg_attr(any(windows, not(feature = "backend_impls")), allow(dead_code))]
pub struct Slab<T> {
  pool: SlotPool<MaybeUninit<T>>,
}

#[cfg_attr(any(windows, not(feature = "backend_impls")), allow(dead_code))]
impl<T> Slab<T> {
  /// Creates a new slab with the given capacity.
  ///
  /// Memory is allocated lazily as slots are used.
  pub fn new(capacity: usize) -> Self {
    Self { pool: SlotPool::with_capacity(capacity, MaybeUninit::uninit) }
  }

  /// Insert a value, returning its key.
  ///
  /// Returns `None` if at capacity.
  #[inline]
  pub fn insert(&mut self, value: T) -> Option<SlabKey> {
    let (key, slot) = self.pool.allocate()?;
    slot.write(value);
    Some(key)
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
    // Drop the value only once its slot is released.
    self.remove_value(key).is_some()
  }

  /// Remove a value by key and return it.
  ///
  /// Returns `None` if the key was invalid or stale.
  #[inline]
  pub fn remove_value(&mut self, key: SlabKey) -> Option<T> {
    self.pool.remove_with(key, |slot| {
      // SAFETY: occupied slots always contain an initialized value.
      unsafe { slot.assume_init_read() }
    })
  }

  /// Get a mutable reference to a value by key.
  #[inline]
  pub fn get_mut(&mut self, key: SlabKey) -> Option<&mut T> {
    let slot = self.pool.get_mut(key)?;
    // SAFETY: occupied slots always contain an initialized value.
    Some(unsafe { slot.assume_init_mut() })
  }

  #[inline]
  pub fn len(&self) -> usize {
    self.pool.len()
  }

  /// Panics if the slab's bookkeeping is inconsistent.
  #[cfg(test)]
  pub fn check_invariants(&self) {
    let retired = self.pool.check_invariants();
    assert!(retired.is_empty(), "slab: slots {retired:?} were never freed");
  }

  /// Keys of the values matching `pred`, by a linear scan of the slots in
  /// use so far. Meant for rare lookups, such as cancellation.
  pub fn keys_where<'a>(
    &'a self,
    mut pred: impl FnMut(&T) -> bool + 'a,
  ) -> impl Iterator<Item = SlabKey> + 'a {
    let used = &self.pool.slots[..self.pool.next_slot as usize];
    used.iter().enumerate().filter_map(move |(index, slot)| {
      // SAFETY: occupied slots always contain an initialized value.
      (slot.occupied && pred(unsafe { slot.state.assume_init_ref() }))
        .then_some(SlabKey { slot: index as u32, generation: slot.generation })
    })
  }
}

impl<T> Drop for Slab<T> {
  fn drop(&mut self) {
    for slot in &mut self.pool.slots {
      if slot.occupied {
        // SAFETY: occupied slots always contain an initialized value.
        unsafe { slot.state.assume_init_drop() };
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

  #[test]
  fn invariants_hold_through_inserts_and_removes() {
    let mut slab: Slab<u32> = Slab::new(8);
    let mut keys = Vec::new();
    for round in 0..4 {
      while let Some(key) = slab.insert(round) {
        keys.push(key);
        slab.check_invariants();
      }
      for key in keys.drain(..).rev().step_by(2) {
        assert!(slab.remove(key));
        slab.check_invariants();
      }
    }
  }

  #[test]
  fn retired_slots_are_reported_until_released() {
    let mut pool: SlotPool<()> = SlotPool::with_capacity(4, || ());
    let (a, _) = pool.allocate().unwrap();
    let (b, _) = pool.allocate().unwrap();
    pool.retire_with(a, |_| ()).unwrap();
    pool.retire_with(b, |_| ()).unwrap();
    assert_eq!(pool.check_invariants(), vec![0, 1]);
    pool.release(a.slot());
    assert_eq!(pool.check_invariants(), vec![1]);
    assert!(!pool.contains(a) && !pool.contains(b));
  }

  #[test]
  #[should_panic(expected = "free list reaches slot 0 twice")]
  fn a_free_list_cycle_is_caught() {
    let mut pool: SlotPool<()> = SlotPool::with_capacity(2, || ());
    let (a, _) = pool.allocate().unwrap();
    pool.remove_with(a, |_| ()).unwrap();
    pool.slots[0].next_free = 0;
    pool.check_invariants();
  }

  #[test]
  #[should_panic(expected = "free list holds occupied slot 0")]
  fn an_occupied_slot_on_the_free_list_is_caught() {
    let mut pool: SlotPool<()> = SlotPool::with_capacity(2, || ());
    let (a, _) = pool.allocate().unwrap();
    pool.free_head = a.slot();
    pool.check_invariants();
  }

  #[test]
  #[should_panic(expected = "len differs from the occupied slots")]
  fn a_wrong_len_is_caught() {
    let mut pool: SlotPool<()> = SlotPool::with_capacity(2, || ());
    pool.allocate().unwrap();
    pool.len = 2;
    pool.check_invariants();
  }
}
