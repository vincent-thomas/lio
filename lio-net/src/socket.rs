use std::{io, net::SocketAddr};

use lio::resource::Resource;

pub struct Socket(Resource);

impl Socket {
  pub async fn new(
    domain: libc::c_int,
    ty: libc::c_int,
    proto: libc::c_int,
  ) -> io::Result<Socket> {
    let resource = lio::socket(domain, ty, proto).await?;
    Ok(Self::from_resource_unchecked(resource))
  }

  pub fn from_resource_unchecked(resource: Resource) -> Self {
    Socket(resource)
  }

  pub fn as_resource(&self) -> &Resource {
    &self.0
  }

  pub fn bind(&self, addr: SocketAddr) -> impl Future<Output = io::Result<()>> {
    lio::bind(&self.0, addr).into_future()
  }

  pub fn listen(&self) -> impl Future<Output = io::Result<()>> {
    lio::listen(&self.0, 128).into_future()
  }

  pub async fn accept(&self) -> io::Result<(Socket, SocketAddr)> {
    let (resource, addr) = lio::accept(&self.0).await?;
    Ok((Socket::from_resource_unchecked(resource), addr))
  }

  pub fn connect(
    &self,
    addr: SocketAddr,
  ) -> impl Future<Output = io::Result<()>> {
    lio::connect(&self.0, addr).into_future()
  }

  pub fn recv(
    &self,
    vec: Vec<u8>,
  ) -> impl Future<Output = lio::BufResult<i32, Vec<u8>>> {
    lio::recv_with_buf(&self.0, vec, None).into_future()
  }

  pub fn send(
    &self,
    vec: Vec<u8>,
  ) -> impl Future<Output = lio::BufResult<i32, Vec<u8>>> {
    lio::send_with_buf(&self.0, vec, None).into_future()
  }
  pub fn shutdown(&self, how: i32) -> impl Future<Output = io::Result<()>> {
    lio::shutdown(&self.0, how).into_future()
  }
}
