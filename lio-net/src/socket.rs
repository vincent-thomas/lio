use std::os::fd::RawFd;
#[cfg(unix)]
use std::os::fd::{AsFd, BorrowedFd, FromRawFd};

pub struct Socket(RawFd);

#[cfg(unix)]
impl FromRawFd for Socket {
  unsafe fn from_raw_fd(fd: std::os::unix::prelude::RawFd) -> Self {
    Self(fd)
  }
}

#[cfg(unix)]
impl AsFd for Socket {
  fn as_fd<'a>(&'a self) -> BorrowedFd<'a> {
    unsafe { BorrowedFd::borrow_raw(self.0) }
  }
}

impl Socket {
  pub async fn new(
    domain: libc::c_int,
    ty: libc::c_int,
    proto: libc::c_int,
  ) -> io::Result<Self> {
    let rawfd = lio::socket(domain, ty, proto).await?;
    // SAFETY: We literally just created it.
    let fd = unsafe { Fd::from_raw_fd(rawfd) };
    Ok(Socket(fd))
  }

  pub fn bind(&self, addr: SocketAddr) -> impl Future<Output = io::Result<()>> {
    lio::bind(self.0.0, addr).into_future()
  }

  pub fn listen(&self) -> impl Future<Output = io::Result<()>> {
    lio::listen(self.0.0, 128).into_future()
  }

  pub async fn accept(&self) -> io::Result<(Socket, SocketAddr)> {
    let (raw_fd, addr) = lio::accept(self.0.0).await?;
    let fd = unsafe { Fd::from_raw_fd(raw_fd) };

    Ok((Socket::from(fd), addr))
  }

  pub fn connect(
    &self,
    addr: SocketAddr,
  ) -> impl Future<Output = io::Result<()>> {
    lio::connect(self.0.0, addr).into_future()
  }

  pub fn recv(
    &self,
    vec: Vec<u8>,
  ) -> impl Future<Output = lio::BufResult<i32, Vec<u8>>> {
    lio::recv_with_buf(self.0.0, vec, None).into_future()
  }

  pub fn send(
    &self,
    vec: Vec<u8>,
  ) -> impl Future<Output = lio::BufResult<i32, Vec<u8>>> {
    lio::send_with_buf(self.0.0, vec, None).into_future()
  }
  pub fn shutdown(&self, how: i32) -> impl Future<Output = io::Result<()>> {
    lio::shutdown(self.0.0, how).into_future()
  }
}
