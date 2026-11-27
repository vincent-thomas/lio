//! Storage for in-flight I/O operations.
//!
//! This module provides [`OpStore`], a fixed-capacity slot store for managing
//! in-flight I/O operations. Each slot owns both the [`Registration`] and a
//! persistent model-lifetime bump arena reused across registrations occupying
//! that slot. The per-step arena lives with the backend's table
//! ([`Tracked`](crate::backend::Tracked)), which hands it to the backend.

use std::mem::MaybeUninit;

use bumpalo::Bump;

#[cfg(test)]
use crate::api::op::Action;
use crate::backend::Keep;
use crate::registration::Registration;
use crate::slab::{SlabKey, SlotPool};

struct StoreSlot {
  registration: MaybeUninit<Registration>,
  model_bump: Bump,
  /// The driver's share of the lease on the memory of the slot's operation.
  /// A backend holds the other shares. It belongs to the slot, not to the
  /// registration in it, so using a slot again allocates nothing.
  lease: Keep,
}

/// Store for in-flight I/O operations using contiguous memory.
///
/// Uses fixed slots with free-list reuse for O(1) operations and cache-friendly
/// memory layout. Each slot owns a persistent model-lifetime bump, reset
/// before slot reuse.
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
        lease: Keep::new(),
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
  #[cfg(test)]
  #[inline]
  pub fn insert_with_action(
    &mut self,
    init: impl FnOnce(&mut Bump) -> Registration,
  ) -> (u64, Option<Action<'static>>) {
    let mut action = None;
    let Some((key, _)) = self.slots.allocate_with(|slot| {
      slot.model_bump.reset();
      let registration = slot.registration.write(init(&mut slot.model_bump));
      let mut guard = DropOnUnwind { registration, armed: true };
      action = guard.registration.action();
      // It returned: the slot gets claimed and owns the registration now.
      guard.armed = false;
    }) else {
      panic!("insert_with_action on a full store: check is_full first");
    };
    (key.as_u64(), action)
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

  /// Borrows a registration and the slot's lease together.
  ///
  /// The driver uses this to start another operation in a slot that is
  /// already taken, and hand it to the backend in the same call.
  #[inline]
  pub fn with_slot<R>(
    &mut self,
    id: u64,
    f: impl FnOnce(&mut Registration, &Keep) -> R,
  ) -> Option<R> {
    let key = SlabKey::from_u64(id);
    let slot = self.slots.get_mut(key)?;
    let lease = &slot.lease;
    // SAFETY: an occupied slot's registration is initialized.
    let registration = unsafe { slot.registration.assume_init_mut() };
    Some(f(registration, lease))
  }

  /// Gives slot `id`, which holds an operation and runs no model, the
  /// model `make` builds in its arena. Returns whether the slot is taken.
  ///
  /// The arena is reset first: the slot's last model is long gone.
  #[inline]
  pub fn adopt(
    &mut self,
    id: u64,
    make: impl FnOnce(&mut Bump) -> Registration,
  ) -> bool {
    let key = SlabKey::from_u64(id);
    let Some(slot) = self.slots.get_mut(key) else {
      return false;
    };
    slot.model_bump.reset();
    let made = make(&mut slot.model_bump);
    // SAFETY: an occupied slot's registration is initialized.
    unsafe { slot.registration.assume_init_mut() }.adopt(made);
    true
  }

  /// Whether a backend still holds a share of the lease of slot `id`: it
  /// may still use the memory of an operation that slot held.
  #[inline]
  pub fn lease_held(&mut self, id: u64) -> bool {
    let key = SlabKey::from_u64(id);
    self.slots.get_mut(key).is_some_and(|slot| slot.lease.held_elsewhere())
  }

  /// Whether a share of the lease of any slot is still held outside the
  /// store: by the table, for an operation that was not reported, or by a
  /// completion that is still alive. Slots whose registration was detached count too.
  pub fn any_lease_held(&mut self) -> bool {
    let mut held = false;
    self.slots.for_each_reserved(|_, slot| {
      if slot.lease.held_elsewhere() {
        held = true;
      }
    });
    held
  }

  /// Adds to `ids` every operation whose lease is still held outside the
  /// store, by the id it was submitted with. That includes operations
  /// whose registration was detached, which is when the id stops
  /// resolving here: the backend still knows them by it.
  pub fn leases_held(&mut self, ids: &mut Vec<u64>) {
    self.slots.for_each_reserved(|key, slot| {
      if slot.lease.held_elsewhere() {
        ids.push(key.as_u64());
      }
    });
  }

  /// Frees slot `id` and drops its registration where it is.
  ///
  /// For a registration whose drop runs no user code. Returns whether the
  /// slot was taken.
  #[inline]
  pub fn remove_in_place(&mut self, id: u64) -> bool {
    let key = SlabKey::from_u64(id);
    let removed = self.slots.remove_with(key, |slot| {
      // SAFETY: occupied slots always contain an initialized registration,
      // and the slot no longer counts as initialized once this returns.
      unsafe { slot.registration.assume_init_drop() }
    });
    removed.is_some()
  }

  /// Inserts an operation and submits it while its model is borrowed.
  ///
  /// `submit` returns whether the model produced an action. The slot stays
  /// free, and the registration is dropped, when `submit` returns `false` or
  /// panics before it asks the caller to keep the slot. Once `submit` has
  /// called [`keep_on_panic`](Insert::keep_on_panic), a later panic leaves the
  /// registration in the store: the backend may already hold its addresses.
  ///
  /// The caller must have checked [`is_full`](Self::is_full) first.
  #[inline]
  pub fn insert_submitting(
    &mut self,
    init: impl FnOnce(&mut Bump) -> Registration,
    submit: impl FnOnce(u64, &mut Registration, &Keep, &mut Insert),
  ) -> Option<u64> {
    let Some(slot_idx) = self.slots.peek_free() else {
      panic!("insert on a full store: check is_full first");
    };
    let slots = &mut self.slots as *mut SlotPool<StoreSlot, false>;
    // SAFETY: `slots` is this store's pool, and `slot_idx` came from
    // `peek_free`. The driver is borrowed for this insert, so nothing else
    // allocates a slot until `submit` returns or panics.
    unsafe {
      (*slots).assert_free(slot_idx);
      let slot = (*slots).state_mut(slot_idx);
      slot.model_bump.reset();
      slot.registration.write(init(&mut slot.model_bump));
    }
    // SAFETY: same pointer as above, still the only use of this pool.
    let key = unsafe { (*slots).pending_key(slot_idx) };
    let mut guard =
      InsertGuard { slots, slot_idx, drop_registration: true, keep: false };
    let id = key.as_u64();
    // SAFETY: the registration was just written, and the lease is a
    // different field. Both borrows end before `guard` drops the registration
    // or `commit` publishes the slot.
    unsafe {
      let slot = (*slots).state_mut(slot_idx);
      let registration = slot.registration.assume_init_mut();
      let lease = &slot.lease as *const Keep;
      submit(id, registration, &*lease, &mut guard.token());
    }
    if !guard.keep {
      if guard.drop_registration {
        guard.drop_registration = false;
        // SAFETY: `submit` did not keep the registration, and nothing else has
        // read it. The slot stays free. The flag is cleared first so a panic
        // in the destructor does not drop the registration twice.
        unsafe {
          std::ptr::drop_in_place(
            (*slots).state_mut(slot_idx).registration.as_mut_ptr(),
          );
        }
      }
      // Nothing unwound: the guard has nothing to do, and owns nothing.
      std::mem::forget(guard);
      return None;
    }
    // Publishing is the guard's job only if `submit` panics after
    // `keep_on_panic`. It did not: the guard owns nothing, and is let go
    // without running, so that the slot is not claimed twice.
    std::mem::forget(guard);
    // SAFETY: `slots` still points at this store's pool, and the slot is the
    // free one `peek_free` returned.
    unsafe { (*slots).commit(slot_idx) };
    Some(id)
  }

  /// Sets the generation of a slot, as if it had been used that many times.
  #[cfg(test)]
  pub fn set_generation(&mut self, slot: u32, generation: u32) {
    self.slots.set_generation(slot, generation);
  }
}

/// Tells [`OpStore::insert_submitting`] to keep the registration if `submit`
/// panics after this call.
pub(crate) struct Insert<'a> {
  drop_registration: &'a mut bool,
  keep: &'a mut bool,
}

impl Insert<'_> {
  /// The registration stays in the store if the caller panics after this.
  ///
  /// Call it once the backend is about to copy addresses out of the model,
  /// and not before: a panic earlier must free the slot.
  #[inline]
  pub(crate) fn keep_on_panic(&mut self) {
    *self.drop_registration = false;
    *self.keep = true;
  }
}

struct InsertGuard {
  slots: *mut SlotPool<StoreSlot, false>,
  slot_idx: u32,
  drop_registration: bool,
  keep: bool,
}

impl InsertGuard {
  #[inline]
  fn token(&mut self) -> Insert<'_> {
    Insert {
      drop_registration: &mut self.drop_registration,
      keep: &mut self.keep,
    }
  }
}

// Runs only when `submit` unwinds: `insert_submitting` forgets the guard
// once `submit` has returned.
impl Drop for InsertGuard {
  #[cold]
  fn drop(&mut self) {
    if self.drop_registration {
      // SAFETY: the registration was written and has not been claimed, so
      // nothing else will drop it. A panic before `keep_on_panic` must not
      // leave it in a free slot.
      unsafe {
        std::ptr::drop_in_place(
          (*self.slots).state_mut(self.slot_idx).registration.as_mut_ptr(),
        );
      }
    }
    if self.keep {
      // SAFETY: `keep_on_panic` ran, so the backend may hold addresses from
      // this registration. Publishing the slot keeps those addresses alive.
      // `slots` points at the pool this guard was created from.
      unsafe { (*self.slots).commit(self.slot_idx) };
    }
  }
}

/// Drops a registration that was written into a slot not claimed yet, if
/// whatever runs before the claim panics. Disarmed once that code returned.
#[cfg(test)]
struct DropOnUnwind<'a> {
  registration: &'a mut Registration,
  armed: bool,
}

#[cfg(test)]
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
  /// The driver only lets this run once no lease is held outside the store,
  /// so no operation can touch this memory any more; otherwise it leaks the
  /// store.
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
  use std::collections::HashSet;
  use std::sync::mpsc;

  fn dummy_stored_op(arena: &mut Bump) -> Registration {
    Registration::nop_in(arena, ())
  }

  /// A registration that holds `tx`: the receiver disconnects when the
  /// registration is dropped.
  fn holding(
    arena: &mut Bump,
    tx: mpsc::Sender<std::io::Result<()>>,
  ) -> Registration {
    Registration::nop_in(arena, tx)
  }

  #[test]
  fn held_leases_are_found_while_stored_and_after_detaching() {
    let mut store = OpStore::with_capacity(3);
    let first = store.insert_with(dummy_stored_op);
    let second = store.insert_with(dummy_stored_op);
    let third = store.insert_with(dummy_stored_op);
    let mut held = Vec::new();
    store.leases_held(&mut held);
    assert!(held.is_empty() && !store.any_lease_held());

    let share = |store: &mut OpStore, id| {
      store.with_slot(id, |_, lease| lease.clone()).unwrap()
    };
    let (first_share, second_share) =
      (share(&mut store, first), share(&mut store, second));
    store.leases_held(&mut held);
    assert_eq!(held, [first, second]);
    assert!(store.any_lease_held());
    assert!(store.lease_held(second) && !store.lease_held(third));

    // Detached, `second` stops resolving, and its lease still counts.
    let detached = store.detach(second).unwrap();
    assert!(!store.lease_held(second));
    held.clear();
    store.leases_held(&mut held);
    assert_eq!(held, [first, second], "listed by the id it had");
    drop(first_share);
    held.clear();
    store.leases_held(&mut held);
    assert_eq!(held, [second]);
    assert!(store.any_lease_held());
    drop(second_share);
    held.clear();
    store.leases_held(&mut held);
    assert!(held.is_empty() && !store.any_lease_held());

    store.release(detached.slot);
    drop(detached.registration);
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
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
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
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
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
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
  fn panicking_drop_on_remove_still_releases_the_slot() {
    let mut store = OpStore::with_capacity(1);
    let id = store.insert_with(|arena| {
      let bomb = PanicOnDrop;
      Registration::nop_in(arena, bomb)
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
    let Detached { registration, slot } = store.detach(spent).unwrap();
    drop(registration);
    assert!(store.is_full(), "held until released");
    store.release(slot);
    assert!(!store.is_full(), "a replacement is free");

    let (id, _) = store.insert_with_action(dummy_stored_op);
    let key = SlabKey::from_u64(id);
    assert_eq!((key.slot(), key.generation()), (1, 0));
    assert!(store.is_full(), "the capacity is still one");
    assert!(store.get_mut(spent).is_none());
    assert!(store.get_mut(id).is_some());
    assert!(store.remove(id));
  }

  #[test]
  fn dropping_the_store_drops_its_registrations() {
    let (tx, rx) = mpsc::channel::<std::io::Result<()>>();
    let mut store = OpStore::new();
    store.insert_with(|arena| holding(arena, tx));
    drop(store);
    // `try_recv`: a store that leaked it would make `recv` block forever.
    assert!(
      matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)),
      "the registration's sender was dropped"
    );
  }

  #[test]
  fn a_submission_that_does_not_keep_drops_the_registration() {
    let (tx, rx) = mpsc::channel::<std::io::Result<()>>();
    let mut store = OpStore::with_capacity(1);
    let id = store.insert_submitting(
      |arena| holding(arena, tx),
      |_id, _reg, _lease, _insert| {},
    );
    assert!(id.is_none());
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)));
    assert!(!store.is_full());
  }

  #[test]
  fn a_submission_that_keeps_claims_the_slot() {
    let (tx, rx) = mpsc::channel::<std::io::Result<()>>();
    let mut store = OpStore::with_capacity(1);
    let id = store
      .insert_submitting(
        |arena| holding(arena, tx),
        |_id, _reg, _lease, insert| insert.keep_on_panic(),
      )
      .expect("the slot was claimed");
    assert!(store.is_full());
    assert!(store.get_mut(id).is_some());
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
  }

  #[test]
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
  fn panic_before_keep_drops_the_registration_and_frees_the_slot() {
    let (tx, rx) = mpsc::channel::<std::io::Result<()>>();
    let mut store = OpStore::with_capacity(1);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      store.insert_submitting(
        |arena| holding(arena, tx),
        |_id, _reg, _lease, _insert| panic!("action failed"),
      );
    }));
    assert!(result.is_err());
    assert!(
      matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)),
      "the registration was dropped"
    );
    assert!(!store.is_full(), "the slot was not claimed");
  }

  #[test]
  #[cfg_attr(not(panic = "unwind"), ignore = "catches a panic")]
  fn panic_after_keep_leaves_the_registration_stored() {
    let (tx, rx) = mpsc::channel::<std::io::Result<()>>();
    let mut store = OpStore::with_capacity(1);
    let id = std::cell::Cell::new(0);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      store.insert_submitting(
        |arena| holding(arena, tx),
        |new_id, _reg, _lease, insert| {
          id.set(new_id);
          insert.keep_on_panic();
          panic!("push failed");
        },
      );
    }));
    assert!(result.is_err());
    assert!(
      matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
      "the registration is still in the store"
    );
    assert!(store.is_full());
    assert!(store.get_mut(id.get()).is_some());
    drop(store);
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)));
  }

  #[test]
  fn test_try_insert_errors_at_capacity() {
    let mut store = OpStore::with_capacity(1);
    let _ = store.try_insert_with(dummy_stored_op).unwrap();
    let err = store.try_insert_with(dummy_stored_op).unwrap_err();
    assert_eq!(err.to_string(), "StoreAtCapacity");
  }
}
