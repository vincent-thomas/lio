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
//! All operations return a [`Io<T>`](crate::api::io::Io) which represents an in-flight I/O operation:
//!
//! ```
//! use lio::{Lio, api};
//! use std::os::fd::FromRawFd;
//!
//! let mut lio = Lio::new(64).unwrap();
//! // stdout (fd 1) is always valid
//! let resource = unsafe { lio::api::resource::Resource::from_raw_fd(1) };
//! let data = b"Hello\n".to_vec();
//!
//! // Callback-based
//! api::write(&resource, data).with_lio(&mut lio).when_done(|(result, buf)| {
//!     // result: io::Result<i32>, buf: the original Vec
//! });
//! lio.try_run().unwrap();
//! ```

#[macro_use]
mod macros;
pub mod buf;
#[cfg(feature = "unstable_ffi")]
pub mod ffi;
mod net_utils;

pub mod net;

pub mod fs;

pub use buf::BufResult;

pub mod op;
pub mod typed_op;

#[path = "registration/registration.rs"]
mod registration;

#[path = "backends/backends.rs"]
pub mod backends;

pub mod api;
#[cfg_attr(docsrs, doc(hidden))]
pub mod test_utils;

// Re-export core types
mod lio;
pub use lio::{Lio, install_global, uninstall_global};
