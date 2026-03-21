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
//! Lio is a non-blocking, platform and runtime-independent, I/O library that
//! provides control back to the user. It does so by giving raw access to the
//! syscall arguments, which it then builds upon. By default, it uses the most
//! efficient I/O available based on OS.
// ! - **Linux**: io_uring, epoll as backup.
// ! - **BSD/Apple**: kqueue.
// ! - **Windows**: I/O Completion Ports.
//!
//! This is of course customizable. You can even make your own I/O backend, just by
//! implementing [this trait](crate::backend::IoBackend).
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
//!
//! let mut lio = Lio::new(64).unwrap();
//! let resource = api::resource::Resource::stdout();
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
#[cfg(feature = "unstable_ffi")]
pub mod ffi;

pub mod buf;
#[cfg(feature = "high")]
pub mod fs;
pub mod io;
#[cfg(feature = "high")]
pub mod net;
#[cfg(feature = "high")]
pub mod process;
mod time;

pub use buf::{BufResult, IoBuf, IoBufMut, IoBufMutVec, IoBufVec};

mod registration;
pub mod slab;

pub mod backend;

pub mod api;

// Re-export core types
mod lio;
pub use lio::{Lio, install_global, uninstall_global};
