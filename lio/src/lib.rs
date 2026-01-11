#![allow(private_bounds)]
#![deny(
  clippy::unnecessary_safety_comment,
  clippy::unsafe_removed_from_name,
  clippy::unnecessary_safety_doc,
  clippy::not_unsafe_ptr_arg_deref,
  clippy::undocumented_unsafe_blocks
)]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! # Lio - Low-Level Async I/O Library
//!
//! Lio is a non-blocking, platform-independent, zero-copy (when possible)
//! I/O library that provides control back to the user. It does so by giving
//! raw access to the syscall arguments, which it then builds upon. For
//! example Lio (optionally) can [manage buffers in a zero-copy way](crate::buf),
//! and [can integrate with zeroize](crate::buf#`zeroize`).
//!
//! It uses the most efficient I/O available based on OS. It uses:
//! - **Linux**: io_uring, epoll as backup.
//! - **BSD/Apple**: kqueue.
//! - **Windows**: I/O Completion Ports.
//!
//! As lio is platform-independent, it needs to abstract over OS resources like
//! files/sockets. `lio` calls these [resources](crate::api::resource).
//! Resources are reference counted OS-native identifiers (fd's/handles/etc),
//! which means that cloning is cheap. `lio` will automatically drop/close these
//! on the last reference's drop.
//!
//! ### Example
//! All operations return a [`Progress<T>`](crate::operation::Progress) which represents an in-flight I/O operation:
//!
//! ```rust
//! use lio::api::{self, resource::Resource};
//!
//! lio::init();
//!
//! let resource: Resource = 1;
//! let data = b"Hello\n".to_vec();
//!
//! // 1. Async/await (requires async runtime, lio is runtime-independent)
//! async fn async_example(res: Resource, data: Vec<u8>) {
//!     let (result, buf) = api::write(res, data, 0).await;
//!     println!("Wrote {} bytes", result.unwrap());
//! }
//!
//! // 2. Blocking call
//! let (result, buf) = api::write(resource, data.clone(), 0).wait();
//!
//! // 3. Channel-based
//! let receiver = api::write(resource, data.clone(), 0).send();
//! let (result, buf) = receiver.recv();
//!
//! // 4. Callback-based
//! api::write(resource, data, 0).when_done(|(result, buf)| {
//!     println!("Operation completed: {:?}", result);
//! });
//!
//! lio::exit();
//! ```

#[macro_use]
mod macros;
pub mod buf;
// #[cfg(feature = "unstable_ffi")]
// pub mod ffi;
mod net_utils;

#[cfg(feature = "high")]
pub mod net;

#[cfg(feature = "high")]
pub mod fs;

pub use buf::BufResult;

mod driver;

pub mod operation;

#[path = "registration/registration.rs"]
mod registration;

#[path = "backends/backends.rs"]
pub mod backends;
mod worker;

pub mod api;
#[cfg_attr(docsrs, doc(hidden))]
pub mod test_utils;

use crate::{
  backends::IoBackend,
  buf::BufLike,
  driver::{Driver, TryInitError},
};

pub fn tick() {
  Driver::get().tick(false)
}

/// Deallocates the lio I/O driver, freeing all resources.
///
/// This must be called after `exit()` to properly clean up the driver.
/// Calling this before `exit()` or when the driver is not initialized will panic.
pub fn exit() {
  Driver::get().exit()
}

pub fn init() {
  crate::try_init().expect("lio is already initialised.");
}

// #[cfg(linux)]
// type Default = backends::io_uring::IoUring;
// #[cfg(not(linux))]
type Default = backends::pollingv2::Poller;

pub fn try_init() -> Result<(), TryInitError> {
  crate::try_init_with_driver::<Default>()
}

pub fn try_init_with_driver<B: IoBackend>() -> Result<(), TryInitError> {
  crate::try_init_with_driver_and_capacity::<B>(1024)
}

pub fn try_init_with_driver_and_capacity<D>(
  cap: usize,
) -> Result<(), TryInitError>
where
  D: IoBackend,
{
  Driver::try_init_with_capacity::<D>(cap)
}
