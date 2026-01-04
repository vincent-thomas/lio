#[cfg(linux)]
use io_uring::types::Fd;
use std::os::fd::AsRawFd;

use crate::buf::LentBuf;
use crate::driver::Driver;
use crate::{BufResult, buf::BufLike, resource::Resource};

use crate::op::{Operation, OperationExt, Progress};

#[test]
fn testing() {
  use std::os::fd::FromRawFd;
  let res = unsafe { Resource::from_raw_fd(1) };
  let whapam = ReadBuilder::new(&res).at(1).run().send();
  drop(whapam);
}

pub struct ReadBuilder<B, O> {
  res: Resource,
  buf: Option<B>,
  offset: O,
}

pub struct NoOffset;
pub struct Offset(i64);

impl<B> ReadBuilder<B, NoOffset> {
  /// Will return errn 22 "EINVAL" if offset < 0
  #[inline]
  pub fn new(res: impl Into<Resource>) -> Self {
    Self { res: res.into(), buf: None, offset: NoOffset }
  }
}

impl<T> ReadBuilder<T, NoOffset> {
  #[inline]
  pub fn at(self, offset: i64) -> ReadBuilder<T, Offset> {
    ReadBuilder { offset: Offset(offset), buf: self.buf, res: self.res }
  }
}

impl<B, O> ReadBuilder<B, O> {
  #[inline]
  pub fn buf(self, buf: B) -> ReadBuilder<B, O>
  where
    B: BufLike + Send + Sync,
  {
    assert!(self.buf.is_none());
    ReadBuilder { buf: Some(buf), res: self.res, offset: self.offset }
  }
  #[inline]
  pub fn buf_pooled<'a>(self) -> ReadBuilder<LentBuf<'a>, O> {
    assert!(self.buf.is_none());
    let pooled = Driver::get().try_lend_buf().unwrap();
    ReadBuilder { buf: Some(pooled), res: self.res, offset: self.offset }
  }
}

impl<B> ReadBuilder<B, NoOffset>
where
  B: BufLike + Send + Sync,
{
  pub fn run(self) -> Progress<Read<B>> {
    let op = Read { buf: self.buf, res: self.res };
    Progress::from_op(op)
  }
}

impl<B> ReadBuilder<B, Offset>
where
  B: BufLike + Send + Sync,
{
  pub fn run(self) -> Progress<ReadAt<B>> {
    let op = ReadAt { buf: self.buf, res: self.res, offset: self.offset.0 };
    Progress::from_op(op)
  }
}

pub struct Read<T>
where
  Self: Send + Sync,
  T: BufLike + Send + Sync,
{
  res: Resource,
  buf: Option<T>,
}

impl<T> OperationExt for Read<T>
where
  Self: Send + Sync,
  T: BufLike + Send + Sync,
{
  type Result = BufResult<i32, T>;
}

impl<T> Operation for Read<T>
where
  Self: Send + Sync,
  T: BufLike + Send + Sync,
{
  impl_result!(|this, res: isize| -> BufResult<i32, T> {
    let buf = this.buf.take().expect("ran Recv::result more than once.");
    let bytes = if res < 0 { 0 } else { res as usize };
    let out = buf.after(bytes);
    let result = if res < 0 {
      Err(std::io::Error::from_raw_os_error((-res) as i32))
    } else {
      Ok(res as i32)
    };
    (result, out)
  });

  impl_no_readyness!();

  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    let offset = self.offset.unwrap_or(-1);
    let buf_slice = self.buf.as_ref().unwrap().buf();

    io_uring::opcode::Read::new(
      Fd(self.res.as_raw_fd()),
      buf_slice.as_ptr().cast_mut(),
      buf_slice.len() as u32,
    )
    .offset(-1)
    .build()
  }

  fn run_blocking(&self) -> isize {
    let buf_slice = self.buf.as_ref().unwrap().buf();
    let ptr = buf_slice.as_ptr();
    let len = buf_slice.len();

    syscall_raw!(read(self.res.as_raw_fd(), ptr as *mut _, len))
  }
}

pub struct ReadAt<T>
where
  T: Send + Sync,
{
  res: Resource,
  buf: Option<T>,
  offset: i64,
}

impl<T> ReadAt<T>
where
  T: Send + Sync,
{
  /// Will return errn 22 "EINVAL" if offset < 0
  pub(crate) fn new(res: Resource, mem: T, offset: i64) -> Self
  where
    T: BufLike,
  {
    Self { res, buf: Some(mem), offset }
  }
}

impl<T> OperationExt for ReadAt<T>
where
  T: BufLike + Send + Sync,
{
  type Result = BufResult<i32, T>;
}

impl<T> Operation for ReadAt<T>
where
  T: BufLike + Send + Sync,
{
  impl_result!(|this, res: isize| -> BufResult<i32, T> {
    let buf = this.buf.take().expect("ran Recv::result more than once.");
    let bytes = if res < 0 { 0 } else { res as usize };
    let out = buf.after(bytes);
    let result = if res < 0 {
      Err(std::io::Error::from_raw_os_error((-res) as i32))
    } else {
      Ok(res as i32)
    };
    (result, out)
  });

  impl_no_readyness!();

  #[cfg(linux)]
  fn create_entry(&self) -> io_uring::squeue::Entry {
    let buf_slice = self.buf.as_ref().unwrap().buf();
    let ptr = buf_slice.as_ptr();
    let len = buf_slice.len();
    io_uring::opcode::Read::new(
      Fd(self.res.as_raw_fd()),
      ptr.cast_mut(),
      len as u32,
    )
    .offset(self.offset as u64)
    .build()
  }

  fn run_blocking(&self) -> isize {
    let buf_slice = self.buf.as_ref().unwrap().buf();
    let ptr = buf_slice.as_ptr();
    let len = buf_slice.len();
    syscall_raw!(pread(self.res.as_raw_fd(), ptr as *mut _, len, self.offset))
  }
}
