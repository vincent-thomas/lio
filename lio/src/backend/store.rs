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
  /// generational ID. The slot is only claimed once `init` returns, so a
  /// panicking `init` leaves the store unchanged.
  ///
  /// The caller must have checked [`is_full`](Self::is_full) first.
  #[inline]
  pub fn insert_with_action(
    &mut self,
    init: impl FnOnce(&mut Bump) -> Registration,
  ) -> (u64, Option<Action>, &mut Bump) {
    let (key, slot) = self
      .slots
      .allocate_with(|slot| {
        slot.model_bump.reset();
        slot.step_bump.reset();
        slot.registration.write(init(&mut slot.model_bump));
      })
      .expect("insert_with_action on a full store");
    // SAFETY: `allocate_with` returned, so `init` wrote the registration.
    let registration = unsafe { slot.registration.assume_init_mut() };
    let action = registration.action();
    (key.as_u64(), action, &mut slot.step_bump)
  }

  /// Removes an operation from the store.
  ///
  /// The registration is moved out and its slot released before it is
  /// dropped, so a panicking destructor (user model, callback) cannot leave
  /// an occupied slot around a dropped registration. Its arenas are reset
  /// when the slot is next allocated, which keeps them valid for the drop.
  #[inline]
  pub fn remove(&mut self, id: u64) -> bool {
    let key = SlabKey::from_u64(id);
    self
      .slots
      .remove_with(key, |slot| {
        // SAFETY: occupied slots always contain an initialized registration,
        // and the slot no longer counts as initialized once this returns.
        unsafe { slot.registration.assume_init_read() }
      })
      .map(drop)
      .is_some()
  }

  /// Removes an operation whose ID was already validated by `get_mut` under
  /// the same exclusive store borrow. Drops it as [`remove`](Self::remove)
  /// does.
  #[inline]
  pub fn remove_known(&mut self, id: u64) {
    let key = SlabKey::from_u64(id);
    let registration = self.slots.remove_known_with(key, |slot| {
      // SAFETY: as in `remove`.
      unsafe { slot.registration.assume_init_read() }
    });
    drop(registration);
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
    let mut store = OpStore::new();
    let mut ids = HashSet::new();

    for _ in 0..1000 {
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
  fn dropping_the_store_drops_its_registrations() {
    let (tx, rx) = mpsc::channel::<std::io::Result<()>>();
    let mut store = OpStore::new();
    store.insert_with(|arena| {
      Registration::new_waker_in(arena, dummy_waker(), tx, Nop)
    });
    drop(store);
    assert!(rx.recv().is_err(), "the registration's sender was dropped");
  }

  #[test]
  fn test_try_insert_errors_at_capacity() {
    let mut store = OpStore::with_capacity(1);
    let _ = store.try_insert_with(dummy_stored_op).unwrap();
    let err = store.try_insert_with(dummy_stored_op).unwrap_err();
    assert_eq!(err.to_string(), "StoreAtCapacity");
  }
}
