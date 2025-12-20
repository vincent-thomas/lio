//! Synchronization primitives that wrap both `parking_lot` and `std::sync`.
//!
//! This module provides a unified API for synchronization primitives that:
//! - Uses `parking_lot` when the `parking_lot` feature is enabled
//! - Falls back to `std::sync` when the feature is disabled
//! - Removes poisoning by unwrapping poisoned locks

#[cfg(feature = "parking_lot")]
pub use parking_lot::{Mutex, MutexGuard};

#[cfg(not(feature = "parking_lot"))]
pub use self::std_sync::*;

#[cfg(not(feature = "parking_lot"))]
mod std_sync {
  use std::sync as std_sync;

  /// A mutual exclusion primitive that wraps `std::sync::Mutex`.
  ///
  /// Unlike `std::sync::Mutex`, this does not support poisoning.
  pub struct Mutex<T: ?Sized> {
    inner: std_sync::Mutex<T>,
  }

  impl<T> Mutex<T> {
    /// Creates a new mutex in an unlocked state ready for use.
    #[inline]
    pub const fn new(value: T) -> Self {
      Self { inner: std_sync::Mutex::new(value) }
    }
  }

  impl<T: ?Sized> Mutex<T> {
    /// Acquires a mutex, blocking the current thread until it is able to do so.
    ///
    /// This function does not propagate poisoning, so it will always succeed.
    #[inline]
    pub fn lock(&self) -> MutexGuard<'_, T> {
      MutexGuard { inner: self.inner.lock().unwrap_or_else(|e| e.into_inner()) }
    }

    /// Acquires a mutex, blocking the current thread until it is able to do so.
    ///
    /// This function does not propagate poisoning, so it will always succeed.
    #[inline]
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
      match self.inner.try_lock() {
        Ok(value) => Some(MutexGuard { inner: value }),
        Err(err) => match err {
          std_sync::TryLockError::WouldBlock => None,
          std_sync::TryLockError::Poisoned(e) => {
            Some(MutexGuard { inner: e.into_inner() })
          }
        },
      }
    }
  }

  /// An RAII implementation of a "scoped lock" of a mutex.
  ///
  /// When this structure is dropped (falls out of scope), the lock will be unlocked.
  pub struct MutexGuard<'a, T: ?Sized> {
    inner: std_sync::MutexGuard<'a, T>,
  }

  impl<T: ?Sized> std::ops::Deref for MutexGuard<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
      &self.inner
    }
  }

  impl<T: ?Sized> std::ops::DerefMut for MutexGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
      &mut self.inner
    }
  }
}
