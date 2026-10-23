//! Storage for in-flight I/O operations.
//!
//! This module provides [`OpStore`], a fixed-capacity slot store for managing
//! in-flight I/O operations. Each slot owns both the [`Registration`] and a
//! persistent model-lifetime bump arena reused across registrations occupying
//! that slot, plus a separate per-step bump reserved for lowered backend state.

use std::mem::MaybeUninit;

use bumpalo::Bump;

use crate::api::op::Action;
use crate::registration::Registration;
use crate::slab::{SlabKey, SlotPool};

struct StoreSlot {
  registration: MaybeUninit<Registration>,
  model_bump: Bump,
  step_bump: Bump,
}

/// Store for in-flight I/O operations using contiguous memory.
///
/// Uses fixed slots with free-list reuse for O(1) operations and cache-friendly
/// memory layout. Each slot owns:
/// - a persistent model-lifetime bump reset before slot reuse
/// - a step-lifetime bump reserved for backend-lowered state
pub(crate) struct OpStore {
  slots: SlotPool<StoreSlot, false>,
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct StoreAtCapacity;

#[cfg(test)]
impl std::fmt::Display for StoreAtCapacity {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("StoreAtCapacity")
  }
}

#[cfg(test)]
impl std::error::Error for StoreAtCapacity {}

impl OpStore {
  /// Creates a new OpStore with default capacity (1024).
  #[cfg(test)]
  pub fn new() -> OpStore {
    Self::with_capacity(1024)
  }

  /// Creates a new OpStore with the specified capacity.
  pub fn with_capacity(cap: usize) -> OpStore {
    OpStore {
      slots: SlotPool::with_capacity(cap, || StoreSlot {
        registration: MaybeUninit::uninit(),
        model_bump: Bump::new(),
        step_bump: Bump::new(),
      }),
    }
  }

  /// Inserts an operation built using the slot's model-lifetime bump arena.
  #[cfg(test)]
  pub fn insert_with(
    &mut self,
    init: impl FnOnce(&mut Bump) -> Registration,
  ) -> u64 {
    self.try_insert_with(init).expect("at capacity")
  }

  /// Inserts an operation built using the slot's model-lifetime bump arena.
  #[cfg(test)]
  pub fn try_insert_with(
    &mut self,
    init: impl FnOnce(&mut Bump) -> Registration,
  ) -> Result<u64, StoreAtCapacity> {
    let Some((key, _)) = self.slots.allocate_with(|slot| {
      slot.model_bump.reset();
      slot.step_bump.reset();
      slot.registration.write(init(&mut slot.model_bump));
    }) else {
      return Err(StoreAtCapacity);
    };
    Ok(key.as_u64())
  }

  /// Whether every slot is occupied.
  #[inline]
  pub fn is_full(&self) -> bool {
    !self.slots.has_free_slot()
  }

  /// Inserts an operation and returns everything needed for its initial dispatch.
  ///
  /// Keeping the just-allocated slot borrowed avoids looking it up again by its
  /// generational ID. The slot is only claimed once `init` and the first
  /// [`action`](Registration::action) return, so if either panics the store
  /// is left unchanged: a panicking `action` drops the registration instead of
  /// leaving it in a slot nothing would ever free.
  ///
  /// The caller must have checked [`is_full`](Self::is_full) first.
  #[inline]
  pub fn insert_with_action(
    &mut self,
    init: impl FnOnce(&mut Bump) -> Registration,
  ) -> (u64, Option<Action>, &mut Bump) {
    let mut action = None;
    let Some((key, slot)) = self.slots.allocate_with(|slot| {
      slot.model_bump.reset();
      slot.step_bump.reset();
      let registration = slot.registration.write(init(&mut slot.model_bump));
      let mut guard = DropOnUnwind { registration, armed: true };
      action = guard.registration.action();
      // It returned: the slot gets claimed and owns the registration now.
      guard.armed = false;
    }) else {
      panic!("insert_with_action on a full store: check is_full first");
    };
    (key.as_u64(), action, &mut slot.step_bump)
  }

  /// Removes an operation from the store and drops it.
  ///
  /// The driver uses [`detach`](Self::detach) instead, to drop registrations
  /// without itself borrowed.
  #[cfg(test)]
  pub fn remove(&mut self, id: u64) -> bool {
    let Some(Detached { registration, slot }) = self.detach(id) else {
      return false;
    };
    // Nothing can allocate the slot while `self` is borrowed here, so it can
    // be released first, which a panicking drop then cannot skip.
    self.release(slot);
    drop(registration);
    true
  }

  /// Takes an operation out of the store without freeing its slot.
  ///
  /// `id` stops resolving at once, but the slot is not reused, and its arenas
  /// (which the registration points into) are not reset, until it is
  /// [`release`](Self::release)d. That lets the caller drop the registration
  /// later, outside its own borrow of the store: dropping it runs user code.
  #[inline]
  pub fn detach(&mut self, id: u64) -> Option<Detached> {
    let key = SlabKey::from_u64(id);
    let registration = self.slots.retire_with(key, |slot| {
      // SAFETY: occupied slots always contain an initialized registration,
      // and the slot no longer counts as initialized once this returns.
      unsafe { slot.registration.assume_init_read() }
    })?;
    Some(Detached { registration, slot: key.slot() })
  }

  /// [`detach`](Self::detach) for an ID already validated by `get_mut`
  /// under the same exclusive store borrow.
  #[inline]
  pub fn detach_known(&mut self, id: u64) -> Detached {
    let key = SlabKey::from_u64(id);
    let registration = self.slots.retire_known_with(key, |slot| {
      // SAFETY: as in `detach`.
      unsafe { slot.registration.assume_init_read() }
    });
    Detached { registration, slot: key.slot() }
  }

  /// Makes a slot whose registration was [`detach`](Self::detach)ed and
  /// dropped available again.
  /// Number of detached registrations whose slot is not released yet.
  #[inline]
  pub fn retired(&self) -> usize {
    self.slots.retired()
  }

  #[inline]
  pub fn release(&mut self, slot: u32) {
    self.slots.release(slot);
  }

  /// Gets mutable access to an operation's registration.
  #[inline]
  pub fn get_mut(&mut self, id: u64) -> Option<&mut Registration> {
    let key = SlabKey::from_u64(id);
    let slot = self.slots.get_mut(key)?;
    // SAFETY: occupied slots always contain an initialized registration.
    Some(unsafe { slot.registration.assume_init_mut() })
  }

  /// Gets mutable access to an operation's per-step lowering arena.
  #[inline]
  pub fn step_bump_mut(&mut self, id: u64) -> Option<&mut Bump> {
    let key = SlabKey::from_u64(id);
    let slot = self.slots.get_mut(key)?;
    Some(&mut slot.step_bump)
  }

  /// Sets the generation of a slot, as if it had been used that many times.
  #[cfg(test)]
  pub fn set_generation(&mut self, slot: u32, generation: u32) {
    self.slots.set_generation(slot, generation);
  }
}

/// Drops a registration that was written into a slot not claimed yet, if
/// whatever runs before the claim panics. Disarmed once that code returned.
struct DropOnUnwind<'a> {
  registration: &'a mut Registration,
  armed: bool,
}

impl Drop for DropOnUnwind<'_> {
  fn drop(&mut self) {
    if self.armed {
      // SAFETY: still armed, so `insert_with_action` is unwinding. The slot
      // was never claimed, so nothing else will drop or read this value.
      unsafe { std::ptr::drop_in_place(self.registration) };
    }
  }
}

/// A registration taken out of the store whose slot stays reserved until
/// [`OpStore::release`]: the registration points into that slot's arenas.
pub(crate) struct Detached {
  pub registration: Registration,
  pub slot: u32,
}

impl Drop for OpStore {
  /// Drops the registrations still in the store: their buffers, resources
  /// and callbacks. Runs before the slots' arenas, which they point into.
  ///
  /// The driver only lets this run once its backend reported that no
  /// operation can touch this memory any more; otherwise it leaks the store.
  fn drop(&mut self) {
    self.slots.for_each_occupied(|slot| {
      // SAFETY: occupied slots always contain an initialized registration,
      // and the store is never used again.
      unsafe { slot.registration.assume_init_drop() };
    });
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::api::ops::Nop;
  use std::collections::HashSet;
  use std::sync::mpsc;
  use std::task::{RawWaker, RawWakerVTable, Waker};

  fn dummy_waker() -> Waker {
    unsafe fn clone(_: *const ()) -> RawWaker {
      RawWaker::new(std::ptr::null(), &VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}

    static VTABLE: RawWakerVTable =
      RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    let raw_waker = RawWaker::new(std::ptr::null(), &VTABLE);
    // SAFETY: `raw_waker` is built from a static vtable whose functions never
    // touch the null data pointer, so it is valid to materialize a `Waker`.
    unsafe { Waker::from_raw(raw_waker) }
  }

  fn dummy_stored_op(arena: &mut Bump) -> Registration {
    let (tx, _rx) = mpsc::channel();
    Registration::new_waker_in(arena, dummy_waker(), tx, Nop)
  }

  #[test]
  fn test_basic_insert_and_remove() {
    let mut store = OpStore::new();
    let id = store.insert_with(dummy_stored_op);

    assert!(store.remove(id));
    assert!(!store.remove(id));
  }

  #[test]
  fn test_sequential_ids_are_unique() {
    // Miri runs each insert thousands of times slower.
    let count = if cfg!(miri) { 64 } else { 1000 };
    let mut store = OpStore::with_capacity(count);
    let mut ids = HashSet::new();

    for _ in 0..count {
      let id = store.insert_with(dummy_stored_op);
      assert!(ids.insert(id), "Generated duplicate ID: {}", id);
    }
  }

  #[test]
  fn test_slot_reuse_increments_generation() {
    let mut store = OpStore::new();
    let id1 = store.insert_with(dummy_stored_op);
    let key1 = SlabKey::from_u64(id1);
    assert_eq!(key1.generation(), 0);
    assert_eq!(key1.slot(), 0);

    store.remove(id1);

    let id2 = store.insert_with(dummy_stored_op);
    let key2 = SlabKey::from_u64(id2);
    assert_eq!(key2.slot(), 0);
    assert_eq!(key2.generation(), 1);
  }

  #[test]
  fn test_stale_id_rejected_on_remove() {
    let mut store = OpStore::new();
    let id1 = store.insert_with(dummy_stored_op);
    store.remove(id1);
    let id2 = store.insert_with(dummy_stored_op);

    assert!(!store.remove(id1));
    assert!(store.remove(id2));
  }

  #[test]
  fn test_stale_id_rejected_on_get_mut() {
    let mut store = OpStore::new();
    let id1 = store.insert_with(dummy_stored_op);
    store.remove(id1);
    let id2 = store.insert_with(dummy_stored_op);

    assert!(store.get_mut(id1).is_none());
    assert!(store.get_mut(id2).is_some());
  }

  #[test]
  fn test_get_mut_works() {
    let mut store = OpStore::new();
    let id = store.insert_with(dummy_stored_op);
    assert!(store.get_mut(id).is_some());
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
    let _ids: Vec<_> =
      (0..4).map(|_| store.insert_with(dummy_stored_op)).collect();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      store.insert_with(dummy_stored_op);
    }));
    assert!(result.is_err());
  }

  #[test]
  fn panicking_init_leaves_the_slot_free() {
    let mut store = OpStore::with_capacity(1);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      store.insert_with(|_| panic!("init"))
    }));
    assert!(result.is_err());
    assert!(!store.is_full(), "the slot was never claimed");
    let id = store.insert_with(dummy_stored_op);
    assert!(store.remove(id));
  }

  /// Panics when dropped.
  struct PanicOnDrop;

  impl Drop for PanicOnDrop {
    fn drop(&mut self) {
      panic!("drop");
    }
  }

  #[test]
  fn panicking_drop_on_remove_still_releases_the_slot() {
    let mut store = OpStore::with_capacity(1);
    let id = store.insert_with(|arena| {
      let bomb = PanicOnDrop;
      Registration::new_callback_in(
        arena,
        move |_| {
          let _ = &bomb;
        },
        Nop,
      )
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      store.remove(id)
    }));
    assert!(result.is_err());
    assert!(store.get_mut(id).is_none(), "the stale id no longer resolves");
    assert!(!store.is_full(), "the slot is free again");
  }

  #[test]
  fn a_spent_slot_is_replaced_with_its_arenas() {
    let mut store = OpStore::with_capacity(1);
    store.set_generation(0, u32::MAX);
    let spent = store.insert_with(dummy_stored_op);
    store.step_bump_mut(spent).unwrap().alloc(0u8);
    let Detached { registration, slot } = store.detach(spent).unwrap();
    drop(registration);
    assert!(store.is_full(), "held until released");
    store.release(slot);
    assert!(!store.is_full(), "a replacement is free");

    let (id, _, bump) = store.insert_with_action(dummy_stored_op);
    assert!(bump.allocated_bytes() > 0, "the replacement took the arenas");
    let key = SlabKey::from_u64(id);
    assert_eq!((key.slot(), key.generation()), (1, 0));
    assert!(store.is_full(), "the capacity is still one");
    assert!(store.get_mut(spent).is_none());
    assert!(store.step_bump_mut(spent).is_none());
    assert!(store.get_mut(id).is_some());
    assert!(store.remove(id));
  }

  #[test]
  fn dropping_the_store_drops_its_registrations() {
    let (tx, rx) = mpsc::channel::<std::io::Result<()>>();
    let mut store = OpStore::new();
    store.insert_with(|arena| {
      Registration::new_waker_in(arena, dummy_waker(), tx, Nop)
    });
    drop(store);
    // `try_recv`: a store that leaked it would make `recv` block forever.
    assert!(
      matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)),
      "the registration's sender was dropped"
    );
  }

  #[test]
  fn test_try_insert_errors_at_capacity() {
    let mut store = OpStore::with_capacity(1);
    let _ = store.try_insert_with(dummy_stored_op).unwrap();
    let err = store.try_insert_with(dummy_stored_op).unwrap_err();
    assert_eq!(err.to_string(), "StoreAtCapacity");
  }
}
