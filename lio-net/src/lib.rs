use std::io::{self};
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::fd::{FromRawFd, RawFd};

pub struct Fd(RawFd);

impl FromRawFd for Fd {
  unsafe fn from_raw_fd(fd: RawFd) -> Self {
    Self(fd)
  }
}

impl Drop for Fd {
  fn drop(&mut self) {
    let _ = lio::close(self.0).send();
  }
}

pub struct TcpListener(Socket);

impl From<Socket> for TcpListener {
  fn from(value: Socket) -> Self {
    TcpListener(value)
  }
}

impl TcpListener {
  pub async fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
    for value in addr.to_socket_addrs()? {
      let domain = match value {
        SocketAddr::V6(_) => libc::AF_INET6,
        SocketAddr::V4(_) => libc::AF_INET,
      };
      let socket = Socket::new(domain, libc::SOCK_STREAM, 0).await?;

      socket.bind(value).await?;
      socket.listen().await?;
      return Ok(TcpListener(socket));
    }

    Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "could not resolve to any addresses",
    ))
  }

  pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
    let (rawfd, addr) = lio::accept(self.0.0.0).await?;

    let socket = Socket::from(unsafe { Fd::from_raw_fd(rawfd) });

    Ok((TcpStream(socket), addr))
  }
}

pub struct TcpStream(Socket);

impl TcpStream {
  pub async fn connect(addr: impl ToSocketAddrs) -> io::Result<Self> {
    for value in addr.to_socket_addrs()? {
      let domain = match value {
        SocketAddr::V6(_) => libc::AF_INET6,
        SocketAddr::V4(_) => libc::AF_INET,
      };
      let socket = Socket::new(domain, libc::SOCK_STREAM, 0).await?;

      socket.connect(value).await?;

      return Ok(TcpStream(socket));
    }

    Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "could not resolve to any addresses",
    ))
  }

  pub fn send(
    &self,
    vec: Vec<u8>,
  ) -> impl Future<Output = lio::BufResult<i32, Vec<u8>>> {
    self.0.send(vec)
  }

  pub fn recv(
    &self,
    vec: Vec<u8>,
  ) -> impl Future<Output = lio::BufResult<i32, Vec<u8>>> {
    self.0.recv(vec)
  }

  pub fn shutdown(&self, how: i32) -> impl Future<Output = io::Result<()>> {
    self.0.shutdown(how)
  }
}
