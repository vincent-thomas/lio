//! Values that belong to the thread the host runs on.

use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::task::{Context, Poll};

thread_local! {
  /// Its address names the thread.
  static HERE: u8 = const { 0 };
}

/// The thread this is called on, as a number no other live thread has.
#[inline]
fn here() -> usize {
  HERE.with(|here| std::ptr::from_ref(here) as usize)
}

/// A value that is only ever touched on the thread that made it.
///
/// lio's futures and the host's own bookkeeping belong to the driver's
/// thread, and wasmtime wants `Send` values. The whole host runs on that one
/// thread ([`crate::block_on`]), so this never has to refuse: it panics if
/// the value is reached, or dropped, from any other thread.
pub(crate) struct Local<T> {
  value: T,
  home: usize,
}

// SAFETY: the value is reached only through `check`, which panics on any
// thread but the one that made it, and that includes its drop.
unsafe impl<T> Send for Local<T> {}
// SAFETY: as above: a shared reference on another thread reaches nothing.
unsafe impl<T> Sync for Local<T> {}

impl<T> Local<T> {
  #[inline]
  pub(crate) fn new(value: T) -> Self {
    Self { value, home: here() }
  }

  #[inline]
  fn check(&self) {
    assert!(self.home == here(), "a value of the host's thread left it");
  }
}

impl<T> Deref for Local<T> {
  type Target = T;

  #[inline]
  fn deref(&self) -> &T {
    self.check();
    &self.value
  }
}

impl<T> DerefMut for Local<T> {
  #[inline]
  fn deref_mut(&mut self) -> &mut T {
    self.check();
    &mut self.value
  }
}

impl<T: Clone> Clone for Local<T> {
  #[inline]
  fn clone(&self) -> Self {
    Self::new((**self).clone())
  }
}

impl<T> Drop for Local<T> {
  #[inline]
  fn drop(&mut self) {
    // Dropping is a use like any other. Elsewhere, the value could not be
    // dropped safely, and unwinding out of a drop aborts anyway.
    if self.home != here() {
      std::process::abort();
    }
  }
}

impl<F: Future + Unpin> Future for Local<F> {
  type Output = F::Output;

  #[inline]
  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
    Pin::new(&mut **self.get_mut()).poll(cx)
  }
}
