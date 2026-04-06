//! Internal operation types for filesystem I/O.
//!
//! This module contains specialized operation types that adapt low-level I/O operations
//! to work with the high-level [`File`] type. These types implement
//! the [`TypedOp`] trait and are used internally by the filesystem API.
//!
//! Most users will not need to use these types directly, as they are returned by
//! methods on [`File`] and [`OpenOptions`].
//!
//! [`File`]: super::File
//! [`TypedOp`]: crate::api::op::TypedOp
//! [`OpenOptions`]: super::OpenOptions

use std::{
  ffi::CString,
  io,
  os::{fd::FromRawFd, unix::ffi::OsStrExt},
  path::Path,
};

use crate::{
  api::{
    op::{OpModel, Step},
    ops,
    resource::FromResource,
    resource::Resource,
  },
  fs::{File, OpenOptions},
};

/// File open operation specialized for [`File`].
///
/// This type wraps the low-level [`OpenAt`](crate::api::ops::OpenAt) operation and adapts
/// its result to return a [`File`] instead of a raw [`Resource`].
///
/// You typically won't create this directly; it's returned by
/// [`File::open()`](crate::fs::File::open), [`File::create()`](crate::fs::File::create),
/// or [`OpenOptions::open()`](crate::fs::OpenOptions::open).
pub struct OpenAtFile {
  inner: Result<ops::OpenAt, io::Error>,
}

impl OpenAtFile {
  pub(crate) fn open(
    options: &OpenOptions,
    path: &Path,
  ) -> crate::api::io::Io<Self> {
    let inner = Self::build_inner(options, path);
    crate::api::io::Io::from_op(Self { inner })
  }

  fn build_inner(
    options: &OpenOptions,
    path: &Path,
  ) -> Result<ops::OpenAt, io::Error> {
    let (flags, mode) = options.to_flags()?;

    // Convert path to CString
    let path_bytes = path.as_os_str().as_bytes();
    let pathname = CString::new(path_bytes).map_err(|_| {
      io::Error::new(io::ErrorKind::InvalidInput, "path contains null byte")
    })?;

    // Use AT_FDCWD for current working directory
    // SAFETY: AT_FDCWD is a special constant that doesn't need to be closed
    let dir_res = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

    #[allow(clippy::unnecessary_cast)]
    Ok(ops::OpenAt::with_mode(dir_res, pathname, flags, mode as u32))
  }
}

// LEGACY `OpModel` impl parked during the serial-contract migration.
//
// impl OpModel for OpenAtFile {
//   type Item = io::Result<File>;
//
//   fn start(&mut self) -> Op {
//     match &mut self.inner {
//       Ok(inner) => inner.start(),
//       Err(_) => {
//         // Return a no-op that will fail immediately
//         // The actual error will be returned in result
//         crate::backend::op::Op::Nop
//       }
//     }
//   }
//
//   fn process(&mut self, res: isize) -> Step<Self::Item> {
//     let inner =
//       std::mem::replace(&mut self.inner, Err(io::Error::from_raw_os_error(0)));
//     match inner {
//       Ok(mut inner_op) => {
//         // Don't close AT_FDCWD - it's a special constant
//         // The inner OpenAt holds a clone of the Resource, and we need to
//         // prevent it from closing AT_FDCWD when dropped.
//         inner_op.process(res)
//       }
//       Err(e) => Step::Done(Err(e)),
//     }
//   }
// }
