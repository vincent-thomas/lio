//! I/O operation definitions.
//!
//! This module contains all the typed operation structs that implement `TypedOp`.

use std::cell::UnsafeCell;
use std::ffi::CString;
use std::io;
use std::mem;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::ptr::NonNull;
#[cfg(unix)]
use std::os::fd::{FromRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::RawHandle;
use std::ptr;
use std::time::Duration;

use crate::{
  BufResult, IoBufMutVec, IoBufVec,
  api::{
    op::{
      Action, Completion, CompletionFlags, ContractKind, ContractStep,
      OneshotOpModel, OpModel, OpModelContract, OpResult, StreamOpModel,
    },
    resource::Resource,
  },
  backend::op::{
    MsgBuf, MsgBufMut, MsgRecv, MsgSend, Op, RawBuf, SockDomain, SockProto,
    SockType,
  },
  buf::MAX_IOV_COUNT,
};

#[cfg(target_os = "linux")]
const TIMER_FIRED_ERRNO: i32 = libc::ETIME;
#[cfg(any(
  target_os = "macos",
  target_os = "freebsd",
  target_os = "dragonfly"
))]
const TIMER_FIRED_ERRNO: i32 = libc::ETIMEDOUT;
#[cfg(not(any(
  target_os = "linux",
  target_os = "macos",
  target_os = "freebsd",
  target_os = "dragonfly"
)))]
const TIMER_FIRED_ERRNO: i32 = 0;

// ============================================================================
// Socket address conversion utilities
// ============================================================================

/// # Safety
/// `storage` must point to a valid, initialized `sockaddr_storage`.
unsafe fn libc_socketaddr_into_std_raw(
  storage: *const libc::sockaddr_storage,
) -> io::Result<SocketAddr> {
  // SAFETY: correct pointer.
  let sockaddr = unsafe { *storage };

  if sockaddr.ss_family == libc::AF_INET as libc::sa_family_t {
    let ipv4_ptr = storage.cast::<libc::sockaddr_in>();
    // SAFETY: We've verified ss_family is AF_INET, so the storage pointer can be safely
    // cast to sockaddr_in. The caller guarantees storage points to valid memory.
    let ipv4 = Ipv4Addr::from(unsafe { *ipv4_ptr }.sin_addr.s_addr.to_be());
    // SAFETY: Same as above - pointer is valid and properly aligned for sockaddr_in.
    let port = u16::from_be(unsafe { *ipv4_ptr }.sin_port);

    Ok(SocketAddr::from(SocketAddrV4::new(ipv4, port)))
  } else if sockaddr.ss_family == libc::AF_INET6 as libc::sa_family_t {
    let ipv6_ptr = storage.cast::<libc::sockaddr_in6>();
    // SAFETY: correct.
    let in6 = unsafe { *ipv6_ptr };
    let ipv6 =
      Ipv6Addr::from(u128::from_le_bytes(in6.sin6_addr.s6_addr).to_be());
    let port = u16::from_be(in6.sin6_port);

    Ok(SocketAddr::from(SocketAddrV6::new(
      ipv6,
      port,
      in6.sin6_flowinfo,
      in6.sin6_scope_id,
    )))
  } else {
    Err(io::Error::from_raw_os_error(libc::EAFNOSUPPORT))
  }
}

pub(crate) fn std_socketaddr_into_libc(
  addr: SocketAddr,
) -> libc::sockaddr_storage {
  // SAFETY: sockaddr_storage is a C struct designed to hold any socket address type.
  // Zero-initialization is valid - all fields are primitive types where zero is safe.
  let storage: UnsafeCell<libc::sockaddr_storage> =
    UnsafeCell::new(unsafe { mem::zeroed() });
  match addr {
    // SAFETY: copy_nonoverlapping is safe because:
    // 1. Source (&into_addr(v4)) is a valid, aligned sockaddr_in on the stack
    // 2. Destination (storage.get()) is valid - we just created it
    // 3. Size is correct (size_of::<sockaddr_in>())
    // 4. Regions don't overlap (source is on stack, dest is in UnsafeCell)
    // 5. sockaddr_in fits in sockaddr_storage by design
    SocketAddr::V4(v4) => unsafe {
      // We copy the bytes from the source pointer (&v4)
      // to the destination pointer (&mut storage)
      ptr::copy_nonoverlapping(
        &into_addr(v4) as *const _ as *const u8,
        storage.get() as *mut u8,
        // We calculate the size of the IPv4 address structure
        mem::size_of::<libc::sockaddr_in>(),
      );
    },
    // SAFETY: copy_nonoverlapping is safe because:
    // 1. Source (&into_addr6(v6)) is a valid, aligned sockaddr_in6 on the stack
    // 2. Destination (storage.get()) is valid - we just created it
    // 3. Size is correct (size_of::<sockaddr_in6>())
    // 4. Regions don't overlap (source is on stack, dest is in UnsafeCell)
    // 5. sockaddr_in6 fits in sockaddr_storage by design
    SocketAddr::V6(v6) => unsafe {
      // We copy the bytes from the source pointer (&v6)
      // to the destination pointer (&mut storage)
      ptr::copy_nonoverlapping(
        &into_addr6(v6) as *const _ as *const u8,
        storage.get() as *mut u8,
        // We calculate the size of the IPv6 address structure
        mem::size_of::<libc::sockaddr_in6>(),
      );
    },
  };

  storage.into_inner()
}

fn into_addr(addr: SocketAddrV4) -> libc::sockaddr_in {
  // SAFETY: sockaddr_in is a C struct with primitive integer fields.
  // Zero-initialization is safe - all fields accept zero as a valid value.
  let mut _addr: libc::sockaddr_in = unsafe { mem::zeroed() };

  #[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
  ))]
  {
    _addr.sin_len = mem::size_of::<libc::sockaddr_in>() as u8;
  }
  _addr.sin_family = libc::AF_INET as libc::sa_family_t;
  _addr.sin_port = addr.port().to_be();
  _addr.sin_addr = libc::in_addr { s_addr: u32::from(*addr.ip()).to_be() };

  _addr
}

fn into_addr6(addr: SocketAddrV6) -> libc::sockaddr_in6 {
  // SAFETY: sockaddr_in6 is a C struct with primitive integer/array fields.
  // Zero-initialization is safe - all fields accept zero as a valid value.
  let mut _addr: libc::sockaddr_in6 = unsafe { mem::zeroed() };

  #[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
  ))]
  {
    _addr.sin6_len = mem::size_of::<libc::sockaddr_in6>() as u8;
  }
  _addr.sin6_family = libc::AF_INET6 as libc::sa_family_t;
  _addr.sin6_port = addr.port().to_be();
  _addr.sin6_addr = libc::in6_addr { s6_addr: addr.ip().octets() };

  _addr
}

// ============================================================================
// Socket
// ============================================================================

pub struct Socket {
  domain: SockDomain,
  ty: SockType,
  proto: SockProto,
}

impl Socket {
  pub(crate) fn new(
    domain: SockDomain,
    ty: SockType,
    proto: SockProto,
  ) -> Self {
    Self { domain, ty, proto }
  }
}

#[cfg(unix)]
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
  let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
  if flags < 0 {
    return Err(io::Error::last_os_error());
  }

  let result =
    unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
  if result < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

impl OpModel for Socket {
  type Item = io::Result<Resource>;

  fn action(&mut self) -> Action {
    Action::Io(Op::Socket {
      domain: self.domain,
      ty: self.ty,
      proto: self.proto,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    if completion.result < 0 {
      return OpResult::Done(Err(io::Error::from_raw_os_error(
        (-completion.result) as i32,
      )));
    }

    #[cfg(unix)]
    {
      let fd = completion.result as RawFd;
      if let Err(err) = set_nonblocking(fd) {
        unsafe {
          libc::close(fd);
        }
        return OpResult::Done(Err(err));
      }

      // SAFETY: `fd` was returned by `socket(2)` and ownership transfers here.
      return OpResult::Done(Ok(unsafe { Resource::from_raw_fd(fd) }));
    }

    #[cfg(windows)]
    {
      // SAFETY: result is a valid socket handle returned by the backend.
      return OpResult::Done(Ok(unsafe {
        Resource::from_raw_handle(completion.result as RawHandle)
      }));
    }
  }
}

impl OneshotOpModel for Socket {}

#[cfg(test)]
impl OpModelContract for Socket {
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(SockDomain::IPV4, SockType::STREAM, SockProto::TCP)
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| {
        matches!(
          action,
          Action::Io(Op::Socket {
            domain: SockDomain::IPV4,
            ty: SockType::STREAM,
            proto: SockProto::TCP,
          })
        )
      },
      #[cfg(unix)]
      Completion::new(unsafe { libc::dup(libc::STDIN_FILENO) as isize }),
      #[cfg(windows)]
      Completion::new(1),
      |result| matches!(result, OpResult::Done(Ok(_))),
    )]
  }
}

// ============================================================================
// Accept
// ============================================================================

pub struct Accept {
  res: Resource,
  addr: libc::sockaddr_storage,
  len: libc::socklen_t,
}

impl Accept {
  pub(crate) fn new(res: Resource) -> Self {
    Self {
      res,
      // SAFETY: `sockaddr_storage` is a plain C output buffer.
      addr: unsafe { mem::zeroed() },
      len: mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
    }
  }

  #[cfg(test)]
  fn stage_peer_addr(&mut self, addr: SocketAddr) {
    self.addr = std_socketaddr_into_libc(addr);
    self.len = match addr {
      SocketAddr::V4(_) => mem::size_of::<libc::sockaddr_in>(),
      SocketAddr::V6(_) => mem::size_of::<libc::sockaddr_in6>(),
    } as libc::socklen_t;
  }
}

impl OpModel for Accept {
  type Item = io::Result<(Resource, SocketAddr)>;

  fn action(&mut self) -> Action {
    Action::Io(Op::Accept {
      fd: self.res.clone(),
      addr: &mut self.addr,
      len: &mut self.len,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    if completion.result < 0 {
      return OpResult::Done(Err(io::Error::from_raw_os_error(
        (-completion.result) as i32,
      )));
    }

    #[cfg(unix)]
    // SAFETY: successful accept returns a live file descriptor owned by us.
    let resource = unsafe { Resource::from_raw_fd(completion.result as RawFd) };

    let addr = match unsafe { libc_socketaddr_into_std_raw(&self.addr) } {
      Ok(addr) => addr,
      Err(err) => return OpResult::Done(Err(err)),
    };

    OpResult::Done(Ok((resource, addr)))
  }
}

impl OneshotOpModel for Accept {}

#[cfg(test)]
impl OpModelContract for Accept {
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin())
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| matches!(action, Action::Io(Op::Accept { .. })),
      |model| model.stage_peer_addr("127.0.0.1:8080".parse().unwrap()),
      #[cfg(unix)]
      Completion::new(unsafe { libc::dup(libc::STDIN_FILENO) as isize }),
      |result| {
        matches!(
          result,
          OpResult::Done(Ok((_res, addr)))
            if *addr == "127.0.0.1:8080".parse::<SocketAddr>().unwrap()
        )
      },
    )]
  }
}
//
// ============================================================================
// Connect
// ============================================================================

pub struct Connect {
  res: Resource,
  addr: libc::sockaddr_storage,
  len: libc::socklen_t,
}

impl Connect {
  pub(crate) fn new(res: Resource, addr: SocketAddr) -> Self {
    let addr = std_socketaddr_into_libc(addr);
    let len = if addr.ss_family == libc::AF_INET as libc::sa_family_t {
      mem::size_of::<libc::sockaddr_in>()
    } else if addr.ss_family == libc::AF_INET6 as libc::sa_family_t {
      mem::size_of::<libc::sockaddr_in6>()
    } else {
      mem::size_of::<libc::sockaddr_storage>()
    } as libc::socklen_t;
    Self { res, addr, len }
  }
}

impl OpModel for Connect {
  type Item = std::io::Result<()>;

  fn action(&mut self) -> Action {
    Action::Io(Op::Connect {
      fd: self.res.clone(),
      addr: &self.addr,
      len: self.len,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    OpResult::Done(if completion.result < 0 {
      Err(std::io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      Ok(())
    })
  }
}

impl OneshotOpModel for Connect {}

#[cfg(test)]
impl OpModelContract for Connect {
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), "127.0.0.1:8080".parse().unwrap())
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| matches!(action, Action::Io(Op::Connect { .. })),
      Completion::new(0),
      |result| matches!(result, OpResult::Done(Ok(()))),
    )]
  }
}

// ============================================================================
// Nop
// ============================================================================

pub struct Nop;

impl OpModel for Nop {
  type Item = std::io::Result<()>;

  fn action(&mut self) -> Action {
    Action::Io(Op::Nop)
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    assert_eq!(completion.result, 0);
    OpResult::Done(Ok(()))
  }
}

impl OneshotOpModel for Nop {}

impl OpModelContract for Nop {
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| matches!(action, Action::Io(Op::Nop)),
      Completion::new(0),
      |result| matches!(result, OpResult::Done(Ok(()))),
    )]
  }
}

// ============================================================================
// Read
// ============================================================================

pub struct Read<B: IoBufMutVec + std::marker::Send + Sync> {
  res: Resource,
  buf: Option<B>,
  raws: [RawBuf; MAX_IOV_COUNT],
  iov_count: usize,
  offset: i64,
}

impl<B: IoBufMutVec + std::marker::Send + Sync> Read<B> {
  pub(crate) fn new(res: Resource, buf: B) -> Self {
    let iov_count = buf.buf_count().min(MAX_IOV_COUNT);
    Self {
      res,
      buf: Some(buf),
      // SAFETY: `RawBuf` is repr(transparent) over `libc::iovec`, which is safe
      // to zero-initialize before the entries are filled in `op()`.
      raws: unsafe { mem::zeroed() },
      iov_count,
      offset: -1,
    }
  }

  pub(crate) fn at(mut self, offset: u32) -> Self {
    self.offset = offset as i64;
    self
  }

  #[cfg(test)]
  fn stage_read_data(&mut self, bytes: &[u8]) {
    let buf = self.buf.as_mut().expect("buffer not available");
    let mut copied = 0usize;

    for i in 0..self.iov_count {
      let (ptr, cap) = buf.buf_mut(i);
      let n = (bytes.len() - copied).min(cap);
      // SAFETY: each `ptr` points into model-owned mutable buffer storage, and we
      // copy at most the reported capacity for that segment.
      unsafe {
        std::ptr::copy_nonoverlapping(bytes[copied..].as_ptr(), ptr, n);
      }
      copied += n;
      if copied == bytes.len() {
        break;
      }
    }

    assert_eq!(
      copied,
      bytes.len(),
      "staged bytes exceed total buffer capacity"
    );
  }
}

impl<B: IoBufMutVec + std::marker::Send + Sync> OpModel for Read<B> {
  type Item = BufResult<i32, B>;

  fn action(&mut self) -> Action {
    let buf = self.buf.as_mut().expect("buffer not available");
    for i in 0..self.iov_count {
      let (ptr, len) = buf.buf_mut(i);
      // SAFETY: the pointer/capacity pair refers to model-owned mutable storage
      // that remains valid until `complete()` consumes the buffers.
      self.raws[i] = unsafe { RawBuf::from_raw_parts(ptr, len) };
    }

    Action::Io(Op::Read {
      fd: self.res.clone(),
      iovecs: self.raws.as_mut_ptr(),
      iov_count: self.iov_count,
      offset: self.offset,
      flags: 0,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let mut buf = self.buf.take().expect("buffer not available");

    let result = if completion.result < 0 {
      Err(io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      let mut remaining = completion.result as usize;
      for i in 0..self.iov_count {
        let (_, cap) = buf.buf_mut(i);
        let len = remaining.min(cap);
        buf.set_buf_len(i, len);
        remaining = remaining.saturating_sub(cap);
      }
      Ok(completion.result as i32)
    };

    OpResult::Done((result, buf))
  }
}

impl<B: IoBufMutVec + std::marker::Send + Sync> OneshotOpModel for Read<B> {}

#[cfg(test)]
impl OpModelContract for Read<Vec<u8>> {
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), vec![0u8; 8])
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| {
        matches!(
          action,
          Action::Io(Op::Read { iov_count: 1, offset: -1, flags: 0, .. })
        )
      },
      |model| model.stage_read_data(b"ping"),
      Completion::new(4),
      |result| {
        matches!(
          result,
          OpResult::Done((Ok(4), buf)) if buf.as_slice() == b"ping"
        )
      },
    )]
  }
}

#[cfg(test)]
impl OpModelContract for Read<(Vec<u8>, Vec<u8>)> {
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), (vec![0u8; 3], vec![0u8; 5]))
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| {
        matches!(
          action,
          Action::Io(Op::Read { iov_count: 2, offset: -1, flags: 0, .. })
        )
      },
      |model| model.stage_read_data(b"abcde"),
      Completion::new(5),
      |result: &OpResult<BufResult<i32, (Vec<u8>, Vec<u8>)>>| match result {
        OpResult::Done((Ok(5), (a, b))) => {
          a.as_slice() == b"abc" && b.as_slice() == b"de"
        }
        _ => false,
      },
    )]
  }
}

// ============================================================================
// Write
// ============================================================================

pub struct Write<B: IoBufVec + std::marker::Send + Sync> {
  res: Resource,
  buf: Option<B>,
  raws: [RawBuf; MAX_IOV_COUNT],
  iov_count: usize,
  offset: i64,
}

impl<B: IoBufVec + std::marker::Send + Sync> Write<B> {
  pub(crate) fn new(res: Resource, buf: B) -> Self {
    let iov_count = buf.buf_count().min(MAX_IOV_COUNT);
    Self {
      res,
      buf: Some(buf),
      // SAFETY: `RawBuf` is repr(transparent) over `libc::iovec`, which is safe
      // to zero-initialize before the entries are filled in `op()`.
      raws: unsafe { mem::zeroed() },
      iov_count,
      offset: -1,
    }
  }

  pub(crate) fn at(mut self, offset: u32) -> Self {
    self.offset = offset as i64;
    self
  }
}

impl<B: IoBufVec + std::marker::Send + Sync> OpModel for Write<B> {
  type Item = BufResult<i32, B>;

  fn action(&mut self) -> Action {
    let buf = self.buf.as_ref().expect("buffer not available");
    for i in 0..self.iov_count {
      let (ptr, len) = buf.buf(i);
      // SAFETY: `IoBufVec` guarantees each segment is valid for `len`
      // initialized bytes until completion.
      self.raws[i] = unsafe { RawBuf::from_raw_parts(ptr.cast_mut(), len) };
    }

    Action::Io(Op::Write {
      fd: self.res.clone(),
      iovecs: self.raws.as_ptr(),
      iov_count: self.iov_count,
      offset: self.offset,
      flags: 0,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let buf = self.buf.take().expect("buffer not available");
    let result = if completion.result < 0 {
      Err(io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      Ok(completion.result as i32)
    };
    OpResult::Done((result, buf))
  }
}

impl<B: IoBufVec + std::marker::Send + Sync> OneshotOpModel for Write<B> {}

#[cfg(test)]
impl OpModelContract for Write<Vec<u8>> {
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), b"pong".to_vec())
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| {
        matches!(
          action,
          Action::Io(Op::Write { iov_count: 1, offset: -1, flags: 0, .. })
        )
      },
      Completion::new(4),
      |result: &OpResult<BufResult<i32, Vec<u8>>>| match result {
        OpResult::Done((Ok(4), buf)) => buf.as_slice() == b"pong",
        _ => false,
      },
    )]
  }
}

#[cfg(test)]
impl OpModelContract for Write<(Vec<u8>, Vec<u8>)> {
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), (b"ab".to_vec(), b"cde".to_vec()))
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| {
        matches!(
          action,
          Action::Io(Op::Write { iov_count: 2, offset: -1, flags: 0, .. })
        )
      },
      Completion::new(5),
      |result: &OpResult<BufResult<i32, (Vec<u8>, Vec<u8>)>>| match result {
        OpResult::Done((Ok(5), (a, b))) => {
          a.as_slice() == b"ab" && b.as_slice() == b"cde"
        }
        _ => false,
      },
    )]
  }
}

// ============================================================================
// Recv
// ============================================================================

pub struct Recv<B: IoBufMutVec + std::marker::Send + Sync> {
  res: Resource,
  buf: Option<B>,
  flags: i32,
  bufs: [MsgBufMut; MAX_IOV_COUNT],
  from: bool,
}

// SAFETY: `Recv` owns the buffer and the message slices only point into
// that owned buffer and into fields of the same struct. The operation is driven
// by a single owning thread.
unsafe impl<B: IoBufMutVec + std::marker::Send + Sync> std::marker::Send
  for Recv<B>
{
}
// SAFETY: same reasoning as `Send`; shared references do not permit mutation of
// the pointed-to storage outside the owning operation flow.
unsafe impl<B: IoBufMutVec + std::marker::Send + Sync> std::marker::Sync
  for Recv<B>
{
}

impl<B: IoBufMutVec + std::marker::Send + Sync> Recv<B> {
  pub(crate) fn new(res: Resource, buf: B, flags: Option<i32>) -> Self {
    Self {
      res,
      flags: flags.unwrap_or(0),
      bufs: [unsafe { MsgBufMut::from_raw_parts(NonNull::dangling(), 0) };
        MAX_IOV_COUNT],
      from: false,
      buf: Some(buf),
    }
  }

  pub fn from(mut self) -> Self {
    self.from = true;
    self
  }

  fn hydrate_msg(&mut self) -> MsgRecv {
    let buf = self.buf.as_mut().expect("buffer not available");
    let buf_count = buf.buf_count().min(MAX_IOV_COUNT);

    for i in 0..buf_count {
      let (ptr, len) = buf.buf_mut(i);
      self.bufs[i] = unsafe {
        MsgBufMut::from_raw_parts(
          NonNull::new(ptr).expect("IoBufMutVec returned null ptr"),
          len,
        )
      };
    }

    MsgRecv::new(&self.bufs[..buf_count], self.from)
  }

  #[cfg(test)]
  fn stage_recv_data(&mut self, bytes: &[u8]) {
    let buf = self.buf.as_mut().expect("buffer not available");
    let mut copied = 0usize;
    let iov_count = buf.buf_count().min(MAX_IOV_COUNT);

    for i in 0..iov_count {
      let (ptr, cap) = buf.buf_mut(i);
      let n = (bytes.len() - copied).min(cap);
      // SAFETY: each `ptr` points into model-owned mutable receive storage.
      unsafe {
        std::ptr::copy_nonoverlapping(bytes[copied..].as_ptr(), ptr, n);
      }
      copied += n;
      if copied == bytes.len() {
        break;
      }
    }

    assert_eq!(
      copied,
      bytes.len(),
      "staged bytes exceed total buffer capacity"
    );
  }
}

impl<B: IoBufMutVec + std::marker::Send + Sync> OpModel for Recv<B> {
  type Item = BufResult<i32, B>;

  fn action(&mut self) -> Action {
    let msg = self.hydrate_msg();
    Action::Io(Op::Recv {
      fd: self.res.clone(),
      msg,
      flags: self.flags,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let mut buf = self.buf.take().expect("buffer not available");
    let result = if completion.result < 0 {
      Err(io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      let mut remaining = completion.result as usize;
      let buf_count = buf.buf_count().min(MAX_IOV_COUNT);
      for i in 0..buf_count {
        let (_, cap) = buf.buf_mut(i);
        let len = remaining.min(cap);
        buf.set_buf_len(i, len);
        remaining = remaining.saturating_sub(cap);
      }
      Ok(completion.result as i32)
    };
    OpResult::Done((result, buf))
  }
}

impl<B: IoBufMutVec + std::marker::Send + Sync> OneshotOpModel for Recv<B> {}

#[cfg(test)]
impl OpModelContract for Recv<Vec<u8>> {
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), vec![0u8; 8], Some(libc::MSG_DONTWAIT))
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| {
        matches!(action, Action::Io(Op::Recv { flags: libc::MSG_DONTWAIT, .. }))
      },
      |model| model.stage_recv_data(b"recv"),
      Completion::new(4),
      |result: &OpResult<BufResult<i32, Vec<u8>>>| match result {
        OpResult::Done((Ok(4), buf)) => buf.as_slice() == b"recv",
        _ => false,
      },
    )]
  }
}

#[cfg(test)]
impl OpModelContract for Recv<(Vec<u8>, Vec<u8>)> {
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(
      Resource::stdin(),
      (vec![0u8; 2], vec![0u8; 4]),
      Some(libc::MSG_DONTWAIT),
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| {
        matches!(action, Action::Io(Op::Recv { flags: libc::MSG_DONTWAIT, .. }))
      },
      |model| model.stage_recv_data(b"hello"),
      Completion::new(5),
      |result: &OpResult<BufResult<i32, (Vec<u8>, Vec<u8>)>>| match result {
        OpResult::Done((Ok(5), (a, b))) => {
          a.as_slice() == b"he" && b.as_slice() == b"llo"
        }
        _ => false,
      },
    )]
  }
}

// ============================================================================
// Send
// ============================================================================

pub struct Send<B: IoBufVec + std::marker::Send + Sync> {
  res: Resource,
  buf: Option<B>,
  flags: i32,
  bufs: [MsgBuf; MAX_IOV_COUNT],
  addr: Option<SocketAddr>,
}

// SAFETY: `Send` owns the buffer and the message slices only point into
// that owned buffer and into fields of the same struct. The operation is driven
// by a single owning thread.
unsafe impl<B: IoBufVec + std::marker::Send + Sync> std::marker::Send
  for Send<B>
{
}
// SAFETY: same reasoning as `Send`; shared references do not permit mutation of
// the pointed-to storage outside the owning operation flow.
unsafe impl<B: IoBufVec + std::marker::Send + Sync> std::marker::Sync
  for Send<B>
{
}

impl<B: IoBufVec + std::marker::Send + Sync> Send<B> {
  pub(crate) fn new(res: Resource, buf: B, flags: Option<i32>) -> Self {
    Self {
      res,
      flags: flags.unwrap_or(0),
      bufs: [unsafe { MsgBuf::from_raw_parts(NonNull::dangling(), 0) };
        MAX_IOV_COUNT],
      addr: None,
      buf: Some(buf),
    }
  }

  pub fn to(mut self, addr: SocketAddr) -> Self {
    self.addr = Some(addr);
    self
  }

  fn hydrate_msg(&mut self) -> MsgSend {
    let buf = self.buf.as_ref().expect("buffer not available");
    let buf_count = buf.buf_count().min(MAX_IOV_COUNT);

    for i in 0..buf_count {
      let (ptr, len) = buf.buf(i);
      self.bufs[i] = unsafe {
        MsgBuf::from_raw_parts(
          NonNull::new(ptr.cast_mut()).expect("IoBufVec returned null ptr"),
          len,
        )
      };
    }

    MsgSend::new(&self.bufs[..buf_count], self.addr)
  }
}

impl<B: IoBufVec + std::marker::Send + Sync> OpModel for Send<B> {
  type Item = BufResult<i32, B>;

  fn action(&mut self) -> Action {
    let msg = self.hydrate_msg();
    Action::Io(Op::Send {
      fd: self.res.clone(),
      msg,
      flags: self.flags,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let buf = self.buf.take().expect("buffer not available");
    let result = if completion.result < 0 {
      Err(io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      Ok(completion.result as i32)
    };
    OpResult::Done((result, buf))
  }
}

impl<B: IoBufVec + std::marker::Send + Sync> OneshotOpModel for Send<B> {}

#[cfg(test)]
impl OpModelContract for Send<Vec<u8>> {
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), b"send".to_vec(), Some(libc::MSG_NOSIGNAL))
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| {
        matches!(action, Action::Io(Op::Send { flags: libc::MSG_NOSIGNAL, .. }))
      },
      Completion::new(4),
      |result: &OpResult<BufResult<i32, Vec<u8>>>| match result {
        OpResult::Done((Ok(4), buf)) => buf.as_slice() == b"send",
        _ => false,
      },
    )]
  }
}

#[cfg(test)]
impl OpModelContract for Send<(Vec<u8>, Vec<u8>)> {
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(
      Resource::stdin(),
      (b"he".to_vec(), b"llo".to_vec()),
      Some(libc::MSG_NOSIGNAL),
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| {
        matches!(action, Action::Io(Op::Send { flags: libc::MSG_NOSIGNAL, .. }))
      },
      Completion::new(5),
      |result: &OpResult<BufResult<i32, (Vec<u8>, Vec<u8>)>>| match result {
        OpResult::Done((Ok(5), (a, b))) => {
          a.as_slice() == b"he" && b.as_slice() == b"llo"
        }
        _ => false,
      },
    )]
  }
}

// ============================================================================
// Sleep
// ============================================================================

pub struct Sleep {
  duration: Duration,
}

impl Sleep {
  pub(crate) fn new(duration: Duration) -> Self {
    Self { duration }
  }

  pub fn duration(&self) -> Duration {
    self.duration
  }
}

impl OpModel for Sleep {
  type Item = io::Result<()>;

  fn action(&mut self) -> Action {
    Action::Sleep(self.duration)
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    debug_assert!(completion.flags.contains(CompletionFlags::TIMER));

    let result = if completion.result == 0 {
      Ok(())
    } else {
      match completion.result.abs() as i32 {
        TIMER_FIRED_ERRNO => Ok(()),
        err => Err(io::Error::from_raw_os_error(err)),
      }
    };

    OpResult::Done(result)
  }
}

impl OneshotOpModel for Sleep {}

#[cfg(test)]
impl OpModelContract for Sleep {
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Duration::from_millis(10))
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| matches!(action, Action::Sleep(duration) if *duration == Duration::from_millis(10)),
      Completion::with_flags(0, CompletionFlags::TIMER),
      |result| matches!(result, OpResult::Done(Ok(()))),
    )]
  }
}

// ============================================================================
// Interval
// ============================================================================

pub struct Interval {
  period: Duration,
}

impl Interval {
  pub(crate) fn new(period: Duration) -> Self {
    Self { period }
  }

  pub fn period(&self) -> Duration {
    self.period
  }
}

impl OpModel for Interval {
  type Item = io::Result<()>;

  fn action(&mut self) -> Action {
    Action::Sleep(self.period)
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    debug_assert!(completion.flags.contains(CompletionFlags::TIMER));

    let result = if completion.result == 0 {
      Ok(())
    } else {
      match completion.result.abs() as i32 {
        TIMER_FIRED_ERRNO => Ok(()),
        err => Err(io::Error::from_raw_os_error(err)),
      }
    };

    OpResult::Yield(result)
  }
}

impl StreamOpModel for Interval {}

#[cfg(test)]
impl OpModelContract for Interval {
  fn contract_kind() -> ContractKind {
    ContractKind::Stream
  }

  fn contract_model() -> Self {
    Self::new(Duration::from_millis(5))
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![
      ContractStep::new(
        |action| matches!(action, Action::Sleep(duration) if *duration == Duration::from_millis(5)),
        Completion::with_flags(0, CompletionFlags::TIMER),
        |result| matches!(result, OpResult::Yield(Ok(()))),
      ),
      ContractStep::new(
        |action| matches!(action, Action::Sleep(duration) if *duration == Duration::from_millis(5)),
        Completion::with_flags(0, CompletionFlags::TIMER),
        |result| matches!(result, OpResult::Yield(Ok(()))),
      ),
    ]
  }
}

pub struct OpenAt {
  dir_res: Resource,
  pathname: CString,
  flags: i32,
  mode: u32,
}

impl OpenAt {
  pub(crate) fn new(
    dir_res: Resource,
    pathname: CString,
    flags: i32,
    mode: u32,
  ) -> Self {
    Self { dir_res, pathname, flags, mode }
  }
}

impl OpModel for OpenAt {
  type Item = std::io::Result<Resource>;

  fn action(&mut self) -> Action {
    Action::Io(Op::OpenAt {
      dir_fd: self.dir_res.clone(),
      path: self.pathname.as_ptr(),
      flags: self.flags,
      mode: self.mode,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    use std::os::fd::FromRawFd;
    let res = if completion.result < 0 {
      Err(std::io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      // SAFETY: result is a valid file descriptor returned by the kernel.
      Ok(unsafe { Resource::from_raw_fd(completion.result as i32) })
    };
    OpResult::Done(res)
  }
}

impl OneshotOpModel for OpenAt {}

#[cfg(test)]
mod tests {
  use super::*;

  mod nop_contract {
    use super::*;

    crate::test_op_model_contract!(Nop);
  }

  mod socket_contract {
    use super::*;

    crate::test_op_model_contract!(Socket);
  }

  mod read_contract {
    use super::*;

    crate::test_op_model_contract!(Read<Vec<u8>>);
  }

  mod read_vectored_contract {
    use super::*;

    crate::test_op_model_contract!(Read<(Vec<u8>, Vec<u8>)>);
  }

  mod write_contract {
    use super::*;

    crate::test_op_model_contract!(Write<Vec<u8>>);
  }

  mod write_vectored_contract {
    use super::*;

    crate::test_op_model_contract!(Write<(Vec<u8>, Vec<u8>)>);
  }

  mod accept_contract {
    use super::*;

    crate::test_op_model_contract!(Accept);
  }

  mod connect_contract {
    use super::*;

    crate::test_op_model_contract!(Connect);
  }

  mod sleep_contract {
    use super::*;

    crate::test_op_model_contract!(Sleep);
  }

  mod interval_contract {
    use super::*;

    crate::test_op_model_contract!(Interval);
  }

  mod recv_contract {
    use super::*;

    crate::test_op_model_contract!(Recv<Vec<u8>>);
  }

  mod recv_vectored_contract {
    use super::*;

    crate::test_op_model_contract!(Recv<(Vec<u8>, Vec<u8>)>);
  }

  mod send_contract {
    use super::*;

    crate::test_op_model_contract!(Send<Vec<u8>>);
  }

  mod send_vectored_contract {
    use super::*;

    crate::test_op_model_contract!(Send<(Vec<u8>, Vec<u8>)>);
  }
}
