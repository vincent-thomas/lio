//! I/O backend implementations for lio.
//!
//! This module provides the abstraction layer for different I/O backends and their
//! implementations. It defines the core traits that all backends must implement and
//! manages the lifecycle of I/O operations.
//!
//! # Architecture
//!
//! The backend system is built around three main components:
//!
//! - **[`IoBackend`]**: The main trait that defines a backend's capabilities. Each platform
//!   has specific implementations (io_uring on Linux, kqueue on macOS/BSD, IOCP on Windows).
//! - **[`IoSubmitter`]**: Handles submitting operations to the backend.
//! - **[`IoDriver`]**: Processes completion events from the backend.
//!
//! # Custom Backends
//!
//! To implement a custom backend, you need to implement the [`IoBackend`], [`IoSubmitter`],
//! and [`IoDriver`] traits:
//!
//! ```rust,ignore
//! use lio::backends::{IoDriver, IoSubmitter, IoHandler, SubmitErr};
//! use std::io;
//!
//! struct MyBackend;
//!
//! impl IoBackend for MyBackend {
//!     type Submitter = MySubmitter;
//!     type Driver = MyDriver;
//!     type State = MyState;
//!
//!     fn new_state() -> io::Result<Self::State> {
//!         // Initialize backend state
//!         todo!()
//!     }
//!
//!     fn new(state: &'static mut Self::State) -> io::Result<(Self::Submitter, Self::Driver)> {
//!         // Create submitter and handler
//!         todo!()
//!     }
//! }
//! ```

#[cfg(test)]
#[macro_use]
pub mod test_macro;

pub use impls::*;
mod impls {

  #[cfg(test)]
  pub mod dummy;

  #[cfg(linux)]
  pub mod io_uring;

  #[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
  ))]
  pub mod pollingv2;

  #[cfg(windows)]
  mod iocp;
  #[cfg(windows)]
  pub use iocp::*;
}

mod store;
pub use store::*;

mod handler;
pub use handler::*;

mod submitter;
pub use submitter::*;

use std::{io, sync::Arc};

/// Error types that can occur when submitting operations to the backend.
#[derive(Debug)]
pub enum SubmitErr {
  /// An I/O error occurred during submission.
  Io(io::Error),
  /// The submission queue is full and cannot accept more operations.
  Full,
  /// The operation is not compatible with this backend.
  ///
  /// When this occurs, the operation should be retried with a different backend
  /// (typically the blocking worker).
  NotCompatible,
  /// The driver has been shut down and is no longer accepting operations.
  DriverShutdown,
}

impl From<io::Error> for SubmitErr {
  fn from(value: io::Error) -> Self {
    Self::Io(value)
  }
}

/// The main trait defining an I/O backend implementation.
///
/// `IoDriver` is the core abstraction for platform-specific I/O backends. Each backend
/// implementation (io_uring, kqueue, IOCP) implements this trait to provide async I/O
/// capabilities for lio.
///
/// # Associated Types
///
/// - `Submitter`: The type used to submit operations to the backend
/// - `Handler`: The type used to process completion events
/// - `State`: Shared state between the submitter and handler
///
/// Undefined behavior is allowed if caller drops State, before Submitter or Handler.
pub trait IoBackend {
  /// The submitter type for this backend.
  type Submitter: IoSubmitter + Send + 'static;

  /// The handler type for this backend.
  type Driver: IoDriver + Send + 'static;

  /// The shared state type for this backend.
  type State: Send + Sync + 'static;

  /// Creates a new instance of the backend state.
  ///
  /// This is called once when initializing the backend to allocate and initialize
  /// any shared state needed by both the submitter and handler.
  fn new_state() -> io::Result<Self::State>;

  /// Creates a new submitter and driver from the given state.
  ///
  /// This splits the backend into its two halves: the submitter (which runs on
  /// worker threads) and the driver (which processes completions).
  ///
  /// Both the submitter and driver will hold references to the state internally.
  ///
  /// # Parameters
  ///
  /// - `state`: A static reference to the backend state that will be shared
  ///
  /// # Returns
  ///
  /// A tuple of (Submitter, Driver) on success.
  fn new(
    state: Arc<Self::State>,
  ) -> io::Result<(Self::Submitter, Self::Driver)>;
}
