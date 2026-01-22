//! Worker module - deprecated in thread-per-core design.
//!
//! In the new thread-per-core model, there are no worker threads.
//! Each thread owns its own `Lio` instance directly.
//! This module is kept as a stub for backwards compatibility during migration.

// TODO: Remove this module once migration is complete
