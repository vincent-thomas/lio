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

  /// Inserts an operation and returns everything needed for its initial dispatch.
  ///
  /// Keeping the just-allocated slot borrowed avoids looking it up again by its
  /// generational ID. Arenas are reset before initialization, including after
  /// a previous initialization unwound.
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
      .expect("at capacity");
    // SAFETY: allocate_with commits only after writing the registration.
    // Unwinding in action() leaves this registration live for store cleanup.
    let registration = unsafe { slot.registration.assume_init_mut() };
    let action = registration.action();
    (key.as_u64(), action, &mut slot.step_bump)
  }

  /// Removes an operation from the store.
  #[inline]
  pub fn remove(&mut self, id: u64) -> bool {
    let key = SlabKey::from_u64(id);
    self
      .slots
      .remove_with(key, |slot| {
        // SAFETY: occupied slots always contain an initialized registration.
        unsafe { slot.registration.assume_init_drop() };
        slot.model_bump.reset();
        slot.step_bump.reset();
      })
      .is_some()
  }

  /// Removes an operation whose ID was already validated by `get_mut` under
  /// the same exclusive store borrow.
  #[inline]
  pub fn remove_known(&mut self, id: u64) {
    let key = SlabKey::from_u64(id);
    self.slots.remove_known_with(key, |slot| {
      // SAFETY: occupied slots always contain an initialized registration.
      unsafe { slot.registration.assume_init_drop() };
    });
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
  fn drop(&mut self) {
    // Only occupied slots own initialized registrations. Drop handlers while
    // their model bumps are still alive; free slots were already dropped by
    // remove/remove_known and must not be dropped twice.
    self.slots.for_each_occupied_mut(|slot| {
      // SAFETY: occupied slots contain an initialized registration, and
      // each is visited exactly once before the slot arenas are torn down.
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

  use crate::api::op::{Completion, OpModel, OpResult};
  use crate::backend::op::Op;
  use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
  };
  use std::task::Wake;

  struct DropCount(Arc<AtomicUsize>);
  impl Drop for DropCount {
    fn drop(&mut self) {
      self.0.fetch_add(1, Ordering::SeqCst);
    }
  }
  struct CountedModel(DropCount);
  impl OpModel for CountedModel {
    type Item = ();
    fn action(&mut self) -> Action {
      let _ = &self.0;
      Action::Io(Op::Nop)
    }
    fn complete(&mut self, _: Completion) -> OpResult<()> {
      OpResult::Done(())
    }
  }
  struct CountedWake(DropCount);
  impl Wake for CountedWake {
    fn wake(self: Arc<Self>) {
      let _ = &self.0;
    }
  }

  // Separate model and handler counters detect both a leaked bump payload and
  // an undropped registration (callback closure or stored waker).
  fn counted_registration(
    arena: &mut Bump,
    callback: bool,
    model_drops: &Arc<AtomicUsize>,
    handler_drops: &Arc<AtomicUsize>,
  ) -> Registration {
    let model = CountedModel(DropCount(Arc::clone(model_drops)));
    let guard = DropCount(Arc::clone(handler_drops));
    if callback {
      Registration::new_callback_in(
        arena,
        move |()| {
          let _ = &guard;
        },
        model,
      )
    } else {
      let (tx, _rx) = mpsc::channel();
      Registration::new_waker_in(
        arena,
        Waker::from(Arc::new(CountedWake(guard))),
        tx,
        model,
      )
    }
  }
  fn drops(count: &Arc<AtomicUsize>) -> usize {
    count.load(Ordering::SeqCst)
  }

  struct PanicModel {
    drops: Arc<AtomicUsize>,
    panic_on_drop: bool,
  }
  impl OpModel for PanicModel {
    type Item = ();
    fn action(&mut self) -> Action {
      Action::Io(Op::Nop)
    }
    fn complete(&mut self, _: Completion) -> OpResult<()> {
      OpResult::Done(())
    }
  }
  impl Drop for PanicModel {
    fn drop(&mut self) {
      self.drops.fetch_add(1, Ordering::SeqCst);
      assert!(!self.panic_on_drop, "model drop panic");
    }
  }
  fn panic_registration(
    arena: &mut Bump,
    drops: &Arc<AtomicUsize>,
    panic_on_drop: bool,
  ) -> Registration {
    Registration::new_callback_in(
      arena,
      |()| {},
      PanicModel { drops: drops.clone(), panic_on_drop },
    )
  }

  #[test]
  fn init_panic_does_not_publish_registration() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let panic_drops = Arc::new(AtomicUsize::new(0));
    let mut store = OpStore::with_capacity(1);
    assert!(
      catch_unwind(AssertUnwindSafe(|| store.insert_with(|_| panic!("init"))))
        .is_err()
    );
    assert_eq!(drops(&panic_drops), 0);
    let first =
      store.insert_with(|arena| panic_registration(arena, &panic_drops, false));
    assert!(store.remove(first));
    assert!(
      catch_unwind(AssertUnwindSafe(|| {
        store.insert_with_action(|_| panic!("init"));
      }))
      .is_err()
    );
    assert_eq!(drops(&panic_drops), 1);
    let second =
      store.insert_with(|arena| panic_registration(arena, &panic_drops, false));
    assert_eq!(
      SlabKey::from_u64(second).generation(),
      SlabKey::from_u64(first).generation() + 1
    );
    drop(store);
    assert_eq!(drops(&panic_drops), 2);
  }

  #[test]
  fn panicking_destructor_vacates_slot() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    for known in [false, true] {
      let panic_drops = Arc::new(AtomicUsize::new(0));
      let mut store = OpStore::with_capacity(1);
      let first = store
        .insert_with(|arena| panic_registration(arena, &panic_drops, true));
      assert!(
        catch_unwind(AssertUnwindSafe(|| {
          if known {
            store.remove_known(first)
          } else {
            store.remove(first);
          }
        }))
        .is_err()
      );
      assert!(store.get_mut(first).is_none());
      assert!(!store.remove(first));
      let second = store
        .insert_with(|arena| panic_registration(arena, &panic_drops, false));
      assert_ne!(first, second);
      drop(store);
      assert_eq!(drops(&panic_drops), 2);
    }
  }

  #[test]
  fn occupied_store_drops_only_live_registrations() {
    for callback in [false, true] {
      let models = Arc::new(AtomicUsize::new(0));
      let handlers = Arc::new(AtomicUsize::new(0));
      {
        let mut store = OpStore::with_capacity(3);
        for _ in 0..3 {
          store.insert_with(|arena| {
            counted_registration(arena, callback, &models, &handlers)
          });
        }
        assert_eq!((drops(&models), drops(&handlers)), (0, 0));
      }
      assert_eq!((drops(&models), drops(&handlers)), (3, 3));
    }
  }

  #[test]
  fn remove_and_stale_id_reuse_drop_once() {
    for callback in [false, true] {
      let models = Arc::new(AtomicUsize::new(0));
      let handlers = Arc::new(AtomicUsize::new(0));
      let mut store = OpStore::with_capacity(1);
      let first = store.insert_with(|arena| {
        counted_registration(arena, callback, &models, &handlers)
      });
      assert!(store.remove(first));
      assert_eq!((drops(&models), drops(&handlers)), (1, 1));
      let second = store.insert_with(|arena| {
        counted_registration(arena, callback, &models, &handlers)
      });
      assert_ne!(first, second);
      assert!(!store.remove(first));
      assert!(store.get_mut(first).is_none());
      assert_eq!((drops(&models), drops(&handlers)), (1, 1));
      drop(store);
      assert_eq!((drops(&models), drops(&handlers)), (2, 2));
    }
  }

  #[test]
  fn completed_registration_remove_known_drops_once() {
    for callback in [false, true] {
      let models = Arc::new(AtomicUsize::new(0));
      let handlers = Arc::new(AtomicUsize::new(0));
      let mut store = OpStore::with_capacity(1);
      let id = store.insert_with(|arena| {
        counted_registration(arena, callback, &models, &handlers)
      });
      assert!(
        store
          .get_mut(id)
          .unwrap()
          .on_driver_completion(Completion::new(0))
          .is_done()
      );
      assert_eq!(
        (drops(&models), drops(&handlers)),
        (0, if callback { 0 } else { 1 })
      );
      store.remove_known(id);
      assert_eq!((drops(&models), drops(&handlers)), (1, 1));
      drop(store);
      assert_eq!((drops(&models), drops(&handlers)), (1, 1));
    }
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
  fn test_try_insert_errors_at_capacity() {
    let mut store = OpStore::with_capacity(1);
    let _ = store.try_insert_with(dummy_stored_op).unwrap();
    let err = store.try_insert_with(dummy_stored_op).unwrap_err();
    assert_eq!(err.to_string(), "StoreAtCapacity");
  }
}
