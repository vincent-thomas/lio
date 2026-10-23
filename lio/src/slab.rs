//! Generic slab allocator with bump allocation and slot reuse.
//!
//! Provides O(1) allocation and deallocation with:
//! - Bump allocation for fresh slots (cache-friendly sequential access)
//! - Free list for slot reuse (no malloc/free after warmup)
//! - Generational indices for ABA protection
//!
//! A slot whose generation would wrap is retired for good and replaced by a
//! fresh one, so stale keys never resolve and the capacity stays the same.

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
  /// Removed with its last generation: released, it is replaced instead.
  Exhausted,
  /// Exhausted and released: never allocated again.
  Spent,
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
  /// Slots replacing spent ones, at the indices after `slots`. Boxed: this
  /// grows, and a slot must not move while its state is in use.
  spares: Vec<Box<Slot<T>>>,
  /// Makes the state of a slot.
  init_slot: fn() -> T,
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
  pub(crate) fn with_capacity(capacity: usize, init_slot: fn() -> T) -> Self {
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
      spares: Vec::new(),
      init_slot,
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
      self.at(slot_idx).status,
      Status::Free,
      "slot pool: allocated slot {slot_idx}, which is not free"
    );
    init(&mut self.at(slot_idx).state);

    if self.free_head == slot_idx {
      self.free_head = self.at(slot_idx).next_free;
    } else {
      self.next_slot += 1;
    }
    if TRACK_LEN {
      self.len += 1;
    }
    let slot = self.at(slot_idx);
    slot.status = Status::Occupied;
    let key = SlabKey { slot: slot_idx, generation: slot.generation };
    Some((key, &mut slot.state))
  }

  #[inline]
  pub(crate) fn get_mut(&mut self, key: SlabKey) -> Option<&mut T> {
    let slot = self.slot_mut(key.slot())?;
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
    let slot = self.slot_mut(key.slot())?;
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
    let slot = self.at(key.slot());
    assert!(
      slot.generation == key.generation() && slot.occupied(),
      "slot pool: retired slot {}, which its key does not occupy",
      key.slot()
    );

    let removed = on_remove(&mut slot.state);
    // A wrapped generation would let a key from 2^32 reuses ago resolve to
    // whatever occupies the slot next, so a slot is used up instead.
    match slot.generation.checked_add(1) {
      Some(generation) => {
        slot.generation = generation;
        slot.status = Status::Retired;
      }
      None => slot.status = Status::Exhausted,
    }
    self.retired += 1;
    if TRACK_LEN {
      self.len -= 1;
    }
    removed
  }

  /// Makes a slot retired by [`retire_with`](Self::retire_with) available
  /// for allocation again, or, if it used its last generation, a new slot
  /// in its place.
  #[inline]
  pub(crate) fn release(&mut self, slot_idx: u32) {
    let init_slot = self.init_slot;
    let slot = self.at(slot_idx);
    let free = match slot.status {
      Status::Retired => {
        slot.status = Status::Free;
        Some(slot_idx)
      }
      Status::Exhausted => {
        slot.status = Status::Spent;
        // The new slot takes over the state, such as arenas already grown.
        let state = std::mem::replace(&mut slot.state, init_slot());
        self.add_spare(state)
      }
      _ => panic!(
        "slot pool: released slot {slot_idx}, which is not retired (released \
         twice, or while in use)"
      ),
    };
    self.retired -= 1;
    if let Some(free) = free {
      self.at(free).next_free = self.free_head;
      self.free_head = free;
    }
  }

  /// Adds a free slot with `state` in place of a spent one, and returns its
  /// index. `None` once the indices run out, which only a pool of some 2^32
  /// slots can reach: each spare takes 2^32 uses of a slot.
  #[cold]
  fn add_spare(&mut self, state: T) -> Option<u32> {
    let index = self.slots.len() + self.spares.len();
    // `u32::MAX` ends the free list.
    let index = u32::try_from(index).ok().filter(|&index| index != u32::MAX)?;
    self.spares.push(Box::new(Slot {
      generation: 0,
      next_free: u32::MAX,
      state,
      status: Status::Free,
    }));
    Some(index)
  }

  #[inline]
  fn slot_mut(&mut self, index: u32) -> Option<&mut Slot<T>> {
    let index = index as usize;
    match index.checked_sub(self.slots.len()) {
      None => Some(&mut self.slots[index]),
      Some(spare) => self.spares.get_mut(spare).map(|spare| &mut **spare),
    }
  }

  /// The slot at `index`, which must exist.
  #[inline]
  #[expect(clippy::expect_used, reason = "callers pass an index they own")]
  fn at(&mut self, index: u32) -> &mut Slot<T> {
    self.slot_mut(index).expect("slot pool: no slot at this index")
  }

  /// The slots allocated so far, with their indices.
  fn used(&self) -> impl Iterator<Item = (u32, &Slot<T>)> {
    // The slots go first in each `zip`, so no index past the last is made.
    let slots = self.slots[..self.next_slot as usize].iter().zip(0..);
    let spares = self.spares.iter().map(|spare| &**spare);
    let spares = spares.zip(self.slots.len() as u32..);
    slots.chain(spares).map(|(slot, index)| (index, slot))
  }

  /// Whether `key` names an occupied slot.
  #[cfg(test)]
  pub(crate) fn contains(&self, key: SlabKey) -> bool {
    self.used().any(|(index, slot)| {
      index == key.slot()
        && slot.occupied()
        && slot.generation == key.generation()
    })
  }

  /// Sets the generation of the slot at `index`.
  #[cfg(test)]
  pub(crate) fn set_generation(&mut self, index: u32, generation: u32) {
    self.at(index).generation = generation;
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
    let spares = self.spares.iter_mut().map(|spare| &mut **spare);
    for slot in self.slots[..self.next_slot as usize].iter_mut().chain(spares) {
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
    #[expect(clippy::expect_used, reason = "the key was just inserted")]
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
    self.pool.used().filter_map(move |(index, slot)| {
      // SAFETY: occupied slots always contain an initialized value.
      (slot.occupied() && pred(unsafe { slot.state.assume_init_ref() }))
        .then_some(SlabKey { slot: index, generation: slot.generation })
    })
  }
}

impl<T> Drop for Slab<T> {
  fn drop(&mut self) {
    self.pool.for_each_occupied(|state| {
      // SAFETY: occupied slots always contain an initialized value.
      unsafe { state.assume_init_drop() };
    });
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
  fn the_last_generation_is_usable() {
    let mut pool: SlotPool<u32> = SlotPool::with_capacity(1, || 0);
    pool.set_generation(0, u32::MAX - 1);
    let (key, _) = pool.allocate().unwrap();
    pool.remove_with(key, |_| ()).unwrap();
    let (last, value) = pool.allocate().unwrap();
    *value = 7;
    assert_eq!(last.generation(), u32::MAX);
    assert_eq!(pool.get_mut(last), Some(&mut 7));
    assert_eq!(pool.get_mut(key), None, "the previous key stays stale");
  }

  /// A pool whose slot 0 holds the key of its last generation.
  fn on_last_generation(cap: usize) -> (SlotPool<u32>, SlabKey) {
    let mut pool: SlotPool<u32> = SlotPool::with_capacity(cap, || 0);
    pool.set_generation(0, u32::MAX);
    let (key, _) = pool.allocate_with(|value| *value = 7).unwrap();
    assert_eq!((key.slot(), key.generation()), (0, u32::MAX));
    (pool, key)
  }

  #[test]
  fn a_spent_slot_is_replaced_by_a_fresh_one() {
    let (mut pool, spent) = on_last_generation(1);
    assert_eq!(pool.remove_with(spent, |value| *value), Some(7));
    assert!(!pool.contains(spent));
    assert_eq!(pool.get_mut(spent), None);
    assert_eq!(pool.retire_with(spent, |_| ()), None);

    let (fresh, _) = pool.allocate().unwrap();
    assert_eq!((fresh.slot(), fresh.generation()), (1, 0));
    assert!(pool.allocate().is_none(), "the capacity is still one");
    assert_eq!(pool.get_mut(spent), None, "the old key stays stale");
    pool.remove_with(fresh, |_| ()).unwrap();
    let (again, _) = pool.allocate().unwrap();
    assert_eq!((again.slot(), again.generation()), (1, 1));
  }

  #[test]
  fn a_spent_slot_is_held_until_released() {
    let (mut pool, spent) = on_last_generation(1);
    assert_eq!(pool.retire_with(spent, |value| *value), Some(7));
    assert_eq!(pool.retired(), 1);
    assert!(!pool.has_free_slot(), "retired, not replaced yet");
    assert_eq!(pool.get_mut(spent), None);
    pool.release(spent.slot());
    assert_eq!(pool.retired(), 0);
    assert!(pool.has_free_slot());
    let (fresh, _) = pool.allocate().unwrap();
    assert_eq!(fresh.slot(), 1);
  }

  #[test]
  fn spending_a_slot_keeps_the_capacity() {
    let (mut pool, spent) = on_last_generation(3);
    pool.remove_with(spent, |_| ()).unwrap();
    let keys: Vec<_> =
      std::iter::from_fn(|| pool.allocate().map(|(key, _)| key)).collect();
    let slots: Vec<_> = keys.iter().map(|key| key.slot()).collect();
    assert_eq!(slots, [3, 1, 2], "the replacement first, then the rest");
    assert_eq!(pool.len(), 3);
    for key in keys {
      pool.remove_with(key, |_| ()).unwrap();
    }
    for _ in 0..3 {
      assert_ne!(pool.allocate().unwrap().0.slot(), 0, "never handed out");
    }
    assert!(pool.allocate().is_none());
  }

  #[test]
  fn a_replacement_slot_can_be_spent_too() {
    let (mut pool, spent) = on_last_generation(1);
    pool.remove_with(spent, |_| ()).unwrap();
    pool.set_generation(1, u32::MAX);
    let (last, _) = pool.allocate().unwrap();
    assert_eq!((last.slot(), last.generation()), (1, u32::MAX));
    pool.remove_with(last, |_| ()).unwrap();
    let (fresh, _) = pool.allocate().unwrap();
    assert_eq!((fresh.slot(), fresh.generation()), (2, 0));
    assert_eq!(pool.get_mut(last), None);
  }

  #[test]
  #[should_panic(expected = "released slot 0, which is not retired")]
  fn releasing_a_spent_slot_twice_panics() {
    let (mut pool, spent) = on_last_generation(2);
    pool.remove_with(spent, |_| ()).unwrap();
    pool.release(spent.slot());
  }

  #[test]
  fn a_slab_finds_and_drops_values_in_replacement_slots() {
    use std::rc::Rc;

    let value = Rc::new(());
    let mut slab: Slab<Rc<()>> = Slab::new(2);
    slab.pool.set_generation(0, u32::MAX);
    let spent = slab.insert(value.clone()).unwrap();
    assert!(slab.remove(spent));
    let first = slab.insert(value.clone()).unwrap();
    let second = slab.insert(value.clone()).unwrap();
    assert_eq!((first.slot(), second.slot()), (2, 1));
    assert_eq!(slab.keys_where(|_| true).collect::<Vec<_>>(), [second, first]);
    assert_eq!(slab.len(), 2);
    drop(slab);
    assert_eq!(Rc::strong_count(&value), 1, "both values were dropped");
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
      /// Gives a retired slot its last generation.
      Age(usize),
      Get(usize),
      Stale(usize),
    }

    fn step() -> impl Strategy<Value = Step> {
      prop_oneof![
        4 => any::<u32>().prop_map(Step::Allocate),
        2 => any::<usize>().prop_map(Step::Remove),
        2 => any::<usize>().prop_map(Step::Retire),
        2 => any::<usize>().prop_map(Step::Release),
        1 => any::<usize>().prop_map(Step::Age),
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
          Step::Age(i) if !model.retired.is_empty() => {
            let slot = model.retired[i % model.retired.len()];
            pool.set_generation(slot, u32::MAX);
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
