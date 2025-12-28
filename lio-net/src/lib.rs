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

pub struct Socket(Fd);

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

impl From<Fd> for Socket {
  fn from(value: Fd) -> Self {
    Socket(value)
  }
}
