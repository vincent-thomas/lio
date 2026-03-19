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
  api::{op::TypedOp, ops, resource::FromResource, resource::Resource},
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

    Ok(ops::OpenAt::with_mode(dir_res, pathname, flags, mode as u32))
  }
}

impl TypedOp for OpenAtFile {
  type Result = io::Result<File>;

  fn into_op(&mut self) -> crate::backend::op::Op {
    match &mut self.inner {
      Ok(inner) => inner.into_op(),
      Err(_) => {
        // Return a no-op that will fail immediately
        // The actual error will be returned in extract_result
        crate::backend::op::Op::Nop
      }
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    match self.inner {
      Ok(inner) => {
        // Don't close AT_FDCWD - it's a special constant
        // The inner OpenAt holds a clone of the Resource, and we need to
        // prevent it from closing AT_FDCWD when dropped.
        // This is handled by the extract_result which consumes the inner.
        let resource = inner.extract_result(res)?;
        Ok(File::from_resource(resource))
      }
      Err(e) => Err(e),
    }
  }
}
