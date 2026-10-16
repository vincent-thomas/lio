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

/// Where a slot is in its life cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
  /// Never allocated, or on the free list.
  Free,
  /// Allocated: its key resolves.
  Occupied,
  /// Removed, but not reusable until released.
  Retired,
}

/// Slot storage with generation tracking.
pub(crate) struct Slot<T> {
  generation: u32,
  next_free: u32,
  state: T,
  status: Status,
}

impl<T> Slot<T> {
  #[inline]
  fn occupied(&self) -> bool {
    self.status == Status::Occupied
  }
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
  /// Number of retired slots
  retired: u32,
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
        status: Status::Free,
      });
    }
    Self {
      slots,
      free_head: u32::MAX,
      next_slot: 0,
      capacity,
      len: 0,
      retired: 0,
    }
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
    // Before `init`: on a corrupt free list it would overwrite a live slot.
    assert_eq!(
      self.slots[slot_idx as usize].status,
      Status::Free,
      "slot pool: allocated slot {slot_idx}, which is not free"
    );
    init(&mut self.slots[slot_idx as usize].state);

    if self.free_head == slot_idx {
      self.free_head = self.slots[slot_idx as usize].next_free;
    } else {
      self.next_slot += 1;
    }
    let slot = &mut self.slots[slot_idx as usize];
    slot.status = Status::Occupied;
    if TRACK_LEN {
      self.len += 1;
    }
    let key = SlabKey { slot: slot_idx, generation: slot.generation };
    Some((key, &mut slot.state))
  }

  #[inline]
  pub(crate) fn get_mut(&mut self, key: SlabKey) -> Option<&mut T> {
    let slot = self.slots.get_mut(key.slot() as usize)?;
    if slot.generation == key.generation() && slot.occupied() {
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
    if slot.generation != key.generation() || !slot.occupied() {
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
    assert!(
      slot.generation == key.generation() && slot.occupied(),
      "slot pool: retired slot {}, which its key does not occupy",
      key.slot()
    );
    // Before `on_remove` takes the value: a wrapped generation would let a
    // key from 2^32 reuses ago resolve to whatever occupies the slot next.
    let Some(generation) = slot.generation.checked_add(1) else {
      panic!("slot pool: slot {} exhausted its generations", key.slot());
    };

    let removed = on_remove(&mut slot.state);
    slot.status = Status::Retired;
    self.retired += 1;
    if TRACK_LEN {
      self.len -= 1;
    }
    slot.generation = generation;
    removed
  }

  /// Makes a slot retired by [`retire_with`](Self::retire_with) available
  /// for allocation again.
  #[inline]
  pub(crate) fn release(&mut self, slot_idx: u32) {
    let slot = &mut self.slots[slot_idx as usize];
    assert_eq!(
      slot.status,
      Status::Retired,
      "slot pool: released slot {slot_idx}, which is not retired (released \
       twice, or while in use)"
    );
    slot.status = Status::Free;
    self.retired -= 1;
    slot.next_free = self.free_head;
    self.free_head = slot_idx;
  }

  /// Whether `key` names an occupied slot.
  #[cfg(test)]
  pub(crate) fn contains(&self, key: SlabKey) -> bool {
    self.slots.get(key.slot() as usize).is_some_and(|slot| {
      slot.occupied() && slot.generation == key.generation()
    })
  }

  /// Number of slots retired and not released yet.
  #[inline]
  pub(crate) fn retired(&self) -> usize {
    self.retired as usize
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
      if slot.occupied() {
        f(&mut slot.state);
      }
    }
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

  /// Keys of the values matching `pred`, by a linear scan of the slots in
  /// use so far. Meant for rare lookups, such as cancellation.
  pub fn keys_where<'a>(
    &'a self,
    mut pred: impl FnMut(&T) -> bool + 'a,
  ) -> impl Iterator<Item = SlabKey> + 'a {
    let used = &self.pool.slots[..self.pool.next_slot as usize];
    used.iter().enumerate().filter_map(move |(index, slot)| {
      // SAFETY: occupied slots always contain an initialized value.
      (slot.occupied() && pred(unsafe { slot.state.assume_init_ref() }))
        .then_some(SlabKey { slot: index as u32, generation: slot.generation })
    })
  }
}

impl<T> Drop for Slab<T> {
  fn drop(&mut self) {
    for slot in &mut self.pool.slots {
      if slot.occupied() {
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
  fn keys_where_finds_only_matching_live_values() {
    let mut slab: Slab<i32> = Slab::new(4);
    let removed = slab.insert(1).unwrap();
    let k2 = slab.insert(2).unwrap();
    let k3 = slab.insert(3).unwrap();
    slab.remove(removed);
    // The predicates ignore their argument: a removed slot holds no value.
    assert_eq!(slab.keys_where(|_| true).collect::<Vec<_>>(), [k2, k3]);
    assert_eq!(slab.keys_where(|_| false).count(), 0);
    assert_eq!(slab.keys_where(|&v| v == 3).collect::<Vec<_>>(), [k3]);
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
  #[should_panic(expected = "released slot 0, which is not retired")]
  fn releasing_a_slot_twice_panics() {
    let mut pool: SlotPool<()> = SlotPool::with_capacity(2, || ());
    let (key, _) = pool.allocate().unwrap();
    pool.remove_with(key, |_| ()).unwrap();
    pool.release(key.slot());
  }

  #[test]
  #[should_panic(expected = "released slot 0, which is not retired")]
  fn releasing_an_occupied_slot_panics() {
    let mut pool: SlotPool<()> = SlotPool::with_capacity(2, || ());
    let (key, _) = pool.allocate().unwrap();
    pool.release(key.slot());
  }

  #[test]
  #[should_panic(expected = "allocated slot 0, which is not free")]
  fn allocating_from_a_corrupt_free_list_panics() {
    let mut pool: SlotPool<u32> = SlotPool::with_capacity(2, || 0);
    let (_, value) = pool.allocate().unwrap();
    *value = 7;
    pool.free_head = 0;
    let _ = pool.allocate_with(|value| *value = 0);
  }

  #[test]
  fn a_retired_slot_is_reused_only_after_release() {
    let mut pool: SlotPool<()> = SlotPool::with_capacity(1, || ());
    let (key, _) = pool.allocate().unwrap();
    pool.retire_with(key, |_| ()).unwrap();
    assert!(pool.allocate().is_none(), "retired, not free");
    pool.release(key.slot());
    let (again, _) = pool.allocate().unwrap();
    assert_eq!((again.slot(), again.generation()), (0, 1));
  }

  #[test]
  #[should_panic(expected = "slot 0 exhausted its generations")]
  fn wrapping_a_generation_panics() {
    let mut pool: SlotPool<()> = SlotPool::with_capacity(1, || ());
    pool.slots[0].generation = u32::MAX;
    let (key, _) = pool.allocate().unwrap();
    pool.remove_with(key, |_| ()).unwrap();
  }

  #[test]
  fn the_last_generation_is_usable() {
    let mut pool: SlotPool<u32> = SlotPool::with_capacity(1, || 0);
    pool.slots[0].generation = u32::MAX - 1;
    let (key, _) = pool.allocate().unwrap();
    pool.remove_with(key, |_| ()).unwrap();
    let (last, value) = pool.allocate().unwrap();
    *value = 7;
    assert_eq!(last.generation(), u32::MAX);
    assert_eq!(pool.get_mut(last), Some(&mut 7));
    assert_eq!(pool.get_mut(key), None, "the previous key stays stale");
  }

  #[test]
  fn a_panicking_wrap_leaves_the_value_in_place() {
    let mut slab: Slab<Box<u32>> = Slab::new(1);
    slab.pool.slots[0].generation = u32::MAX;
    let key = slab.insert(Box::new(7)).unwrap();
    let removed =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        slab.remove(key);
      }));
    assert!(removed.is_err());
    // Still occupied with its value, dropped once with the slab.
    assert_eq!(slab.get_mut(key).map(|v| **v), Some(7));
  }

  /// Random step sequences against a reference model of the pool.
  mod model {
    use super::*;
    use proptest::prelude::*;

    #[derive(Clone, Debug)]
    enum Step {
      Allocate(u32),
      Remove(usize),
      Retire(usize),
      Release(usize),
      Get(usize),
      Stale(usize),
    }

    fn step() -> impl Strategy<Value = Step> {
      prop_oneof![
        4 => any::<u32>().prop_map(Step::Allocate),
        2 => any::<usize>().prop_map(Step::Remove),
        2 => any::<usize>().prop_map(Step::Retire),
        2 => any::<usize>().prop_map(Step::Release),
        1 => any::<usize>().prop_map(Step::Get),
        1 => any::<usize>().prop_map(Step::Stale),
      ]
    }

    #[derive(Default)]
    struct Model {
      live: Vec<(SlabKey, u32)>,
      retired: Vec<u32>,
      /// Every key ever handed out.
      issued: Vec<SlabKey>,
    }

    fn run(cap: usize, steps: Vec<Step>) {
      let mut pool: SlotPool<u32> = SlotPool::with_capacity(cap, || 0);
      let mut model = Model::default();
      for step in steps {
        match step {
          Step::Allocate(value) => {
            let free = model.live.len() + model.retired.len() < cap;
            let got = pool.allocate_with(|slot| *slot = value);
            assert_eq!(got.is_some(), free, "allocates iff a slot is free");
            if let Some((key, _)) = got {
              assert!(!model.issued.contains(&key), "{key:?} handed out twice");
              model.live.push((key, value));
              model.issued.push(key);
            }
          }
          Step::Remove(i) if !model.live.is_empty() => {
            let (key, value) = model.live.remove(i % model.live.len());
            assert_eq!(pool.remove_with(key, |slot| *slot), Some(value));
          }
          Step::Retire(i) if !model.live.is_empty() => {
            let (key, value) = model.live.remove(i % model.live.len());
            assert_eq!(pool.retire_with(key, |slot| *slot), Some(value));
            model.retired.push(key.slot());
          }
          Step::Release(i) if !model.retired.is_empty() => {
            pool.release(model.retired.remove(i % model.retired.len()));
          }
          Step::Get(i) if !model.live.is_empty() => {
            let (key, mut value) = model.live[i % model.live.len()];
            assert_eq!(pool.get_mut(key), Some(&mut value));
          }
          Step::Stale(i) => {
            let stale: Vec<_> = model
              .issued
              .iter()
              .filter(|key| !model.live.iter().any(|(live, _)| live == *key))
              .collect();
            if let Some(&&key) = stale.get(i % stale.len().max(1)) {
              assert!(!pool.contains(key));
              assert_eq!(pool.get_mut(key), None);
              assert_eq!(pool.retire_with(key, |_| ()), None);
            }
          }
          _ => {}
        }

        assert_eq!(pool.len(), model.live.len());
        assert_eq!(pool.retired(), model.retired.len());
        assert_eq!(
          pool.has_free_slot(),
          model.live.len() + model.retired.len() < cap
        );
        for &(key, mut value) in &model.live {
          assert!(pool.contains(key));
          assert_eq!(pool.get_mut(key), Some(&mut value));
        }
      }
    }

    /// Miri runs each step thousands of times slower.
    const STEPS: usize = if cfg!(miri) { 24 } else { 200 };

    proptest! {
      #[test]
      fn pool_matches_model(
        cap in 1..8usize,
        steps in prop::collection::vec(step(), 0..STEPS),
      ) {
        run(cap, steps);
      }
    }
  }
}
