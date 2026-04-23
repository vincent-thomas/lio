#![allow(private_bounds)]
#![deny(
  clippy::unnecessary_safety_comment,
  clippy::unsafe_removed_from_name,
  clippy::unnecessary_safety_doc,
  clippy::not_unsafe_ptr_arg_deref,
  clippy::undocumented_unsafe_blocks
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = include_str!("../../book/src/crate.md")]

extern crate self as lio;

#[macro_use]
mod macros;

#[cfg(feature = "unstable_ffi")]
pub mod ffi;

pub mod buf;
// #[cfg(feature = "high")]
// pub mod fs;
pub mod io;
#[cfg(feature = "high")]
pub mod net;
// #[cfg(feature = "high")]
// pub mod process;
pub mod time;

pub use buf::{BufResult, IoBuf, IoBufMut, IoBufMutVec, IoBufVec};

mod registration;
mod slab;

pub mod backend;

pub mod api;

#[path = "lio.rs"]
mod driver;
pub use driver::{GlobalLioGuard, Lio, install_global, uninstall_global};
