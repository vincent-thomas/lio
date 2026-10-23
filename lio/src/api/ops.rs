//! I/O operation definitions.
//!
//! This module contains all the typed operation structs that implement `TypedOp`.

use std::ffi::{CString, OsString};
use std::io;
use std::mem;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::time::Duration;

use crate::{
  BufResult, IoBufMutVec, IoBufVec, IoSlicesMut,
  api::{
    op::{
      Action, Completion, CompletionFlags, OneshotOpModel, OpModel, OpResult,
      StreamOpModel,
    },
    resource::Resource,
  },
  backend::op::{
    FileMode, FileStat, MsgBuf, MsgBufMut, MsgRecv, MsgSend, Op, OpenFlags,
    RawBuf, ReadDirBuf, ReadFlags, RecvFlags, SendFlags, ShutdownHow,
    SockDomain, SockProto, SockType, SocketAddrBuf, SocketAddrFamily,
    StatTarget, UnlinkKind, WriteFlags, is_valid_file_offset,
    socket_addr_from_buf, socket_addr_into_buf,
  },
  buf::MAX_IOV_COUNT,
};
#[cfg(test)]
use lio_test::{ContractKind, ContractStep, OpModelContract};

/// Flags for contract tests: op models pass flags through untouched.
#[cfg(test)]
const TEST_RECV_FLAGS: RecvFlags = RecvFlags::PEEK;
#[cfg(test)]
const TEST_SEND_FLAGS: SendFlags = SendFlags::OOB;

fn os_string_from_bytes(bytes: &[u8]) -> OsString {
  #[cfg(unix)]
  {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes.to_vec())
  }
  #[cfg(not(unix))]
  {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
  }
}

fn cstring_to_osstring(path: &CString) -> OsString {
  #[cfg(unix)]
  {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(path.as_bytes().to_vec())
  }
  #[cfg(not(unix))]
  {
    OsString::from(path.to_string_lossy().as_ref())
  }
}
#[cfg(test)]
macro_rules! impl_op_model_contract_runtime {
  () => {
    type Action = Action;
    type Completion = Completion;
    type Result = OpResult<<Self as OpModel>::Item>;

    fn action(&mut self) -> Self::Action {
      <Self as OpModel>::action(self)
    }

    fn complete(&mut self, completion: Self::Completion) -> Self::Result {
      <Self as OpModel>::complete(self, completion)
    }

    fn is_again(result: &Self::Result) -> bool {
      matches!(result, OpResult::Again)
    }

    fn is_yield(result: &Self::Result) -> bool {
      matches!(result, OpResult::Yield(_))
    }

    fn is_done(result: &Self::Result) -> bool {
      matches!(result, OpResult::Done(_))
    }
  };
}

pub use crate::backend::op::LinkKind;

#[cfg(test)]
type TwoVecBufResult = BufResult<usize, (Vec<u8>, Vec<u8>)>;
#[cfg(test)]
type TwoVecOpResult = OpResult<TwoVecBufResult>;

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
    // SAFETY: a successful socket completion carries a new owned socket.
    OpResult::Done(unsafe { Resource::from_completion(&completion) })
  }
}

impl OneshotOpModel for Socket {}

#[cfg(test)]
impl OpModelContract for Socket {
  impl_op_model_contract_runtime!();
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
      // SAFETY: duplicating stdin in this test fixture yields a fresh owned fd.
      Completion::new(unsafe { libc::dup(libc::STDIN_FILENO) as isize }),
      #[cfg(windows)]
      Completion::new({
        use std::os::windows::io::IntoRawSocket;
        // A fresh owned socket for the resulting `Resource` to take over.
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().into_raw_socket()
          as isize
      }),
      |result| matches!(result, OpResult::Done(Ok(_))),
    )]
  }
}

// ============================================================================
// Accept
// ============================================================================

pub struct Accept {
  res: Resource,
  addr: SocketAddrBuf,
}

impl Accept {
  pub(crate) fn new(res: Resource) -> Self {
    Self { res, addr: SocketAddrBuf::unspecified() }
  }

  #[cfg(test)]
  fn stage_peer_addr(&mut self, addr: SocketAddr) {
    use crate::backend::op::socket_addr_into_buf;

    self.addr = socket_addr_into_buf(addr);
  }
}

impl OpModel for Accept {
  type Item = io::Result<(Resource, SocketAddr)>;

  fn action(&mut self) -> Action {
    Action::Io(Op::Accept {
      fd: self.res.clone(),
      addr: NonNull::from(&mut self.addr),
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let resource = match self.complete_resource(completion) {
      Ok(resource) => resource,
      Err(err) => return OpResult::Done(Err(err)),
    };

    let addr = match socket_addr_from_buf(&self.addr) {
      Ok(addr) => addr,
      Err(_) => {
        return OpResult::Done(Err(io::Error::new(
          io::ErrorKind::Unsupported,
          "not an IP socket address; use `accept_unix` for UNIX listeners",
        )));
      }
    };

    OpResult::Done(Ok((resource, addr)))
  }
}

impl Accept {
  /// Takes ownership of the accepted socket without decoding the peer address.
  pub(crate) fn complete_resource(
    &mut self,
    completion: Completion,
  ) -> io::Result<Resource> {
    // SAFETY: a successful accept completion carries a new owned socket.
    unsafe { Resource::from_completion(&completion) }
  }
}

impl OneshotOpModel for Accept {}

// ============================================================================
// AcceptUnix
// ============================================================================

/// The address of a UNIX-domain socket: a filesystem path, or unnamed.
///
/// A client that connects without binding first is unnamed, so that is what
/// [`accept_unix`](crate::api::accept_unix) usually reports for the peer.
/// Linux abstract-namespace addresses are reported as unnamed too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnixAddr {
  path: Option<PathBuf>,
}

impl UnixAddr {
  /// The filesystem path, or `None` if the address is unnamed.
  pub fn as_pathname(&self) -> Option<&Path> {
    self.path.as_deref()
  }

  /// Whether the socket has no filesystem path.
  pub fn is_unnamed(&self) -> bool {
    self.path.is_none()
  }

  /// Decodes an address a backend reported. A backend that reported none is
  /// treated as unnamed; an IP address is an error.
  pub(crate) fn from_buf(buf: &SocketAddrBuf) -> io::Result<Self> {
    match buf.family {
      SocketAddrFamily::Unix => {
        let len = (buf.unix_path_len as usize).min(buf.unix_path.len());
        let bytes = &buf.unix_path[..len];
        // The kernel may count the terminating NUL; an abstract address
        // starts with one.
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let path =
          (end > 0).then(|| PathBuf::from(os_string_from_bytes(&bytes[..end])));
        Ok(Self { path })
      }
      SocketAddrFamily::Unspecified => Ok(Self { path: None }),
      SocketAddrFamily::Ipv4 | SocketAddrFamily::Ipv6 => Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "not a UNIX socket address; use `accept` for IP listeners",
      )),
    }
  }
}

/// [`Accept`] on a UNIX-domain listener, reporting the peer's [`UnixAddr`].
pub struct AcceptUnix {
  inner: Accept,
}

impl AcceptUnix {
  pub(crate) fn new(res: Resource) -> Self {
    Self { inner: Accept::new(res) }
  }
}

impl OpModel for AcceptUnix {
  type Item = io::Result<(Resource, UnixAddr)>;

  fn action(&mut self) -> Action {
    OpModel::action(&mut self.inner)
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let resource = match self.inner.complete_resource(completion) {
      Ok(resource) => resource,
      Err(err) => return OpResult::Done(Err(err)),
    };
    OpResult::Done(
      UnixAddr::from_buf(&self.inner.addr).map(|addr| (resource, addr)),
    )
  }
}

impl OneshotOpModel for AcceptUnix {}

#[cfg(test)]
impl OpModelContract for AcceptUnix {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin())
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| matches!(action, Action::Io(Op::Accept { .. })),
      |model| {
        model.inner.addr =
          crate::backend::op::unix_socket_addr_buf(b"/tmp/peer.sock").unwrap();
      },
      #[cfg(unix)]
      // SAFETY: duplicating stdin in this test fixture yields a fresh owned fd.
      Completion::new(unsafe { libc::dup(libc::STDIN_FILENO) as isize }),
      #[cfg(windows)]
      Completion::new({
        use std::os::windows::io::IntoRawSocket;
        // A fresh owned socket for the resulting `Resource` to take over.
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().into_raw_socket()
          as isize
      }),
      |result| {
        matches!(
          result,
          OpResult::Done(Ok((_res, addr)))
            if addr.as_pathname() == Some(Path::new("/tmp/peer.sock"))
        )
      },
    )]
  }
}

#[cfg(test)]
impl OpModelContract for Accept {
  impl_op_model_contract_runtime!();
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
      // SAFETY: duplicating stdin in this test fixture yields a fresh owned fd.
      Completion::new(unsafe { libc::dup(libc::STDIN_FILENO) as isize }),
      #[cfg(windows)]
      Completion::new({
        use std::os::windows::io::IntoRawSocket;
        // A fresh owned socket for the resulting `Resource` to take over.
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().into_raw_socket()
          as isize
      }),
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
  addr: SocketAddrBuf,
}

impl Connect {
  pub(crate) fn new(res: Resource, addr: SocketAddr) -> Self {
    Self { res, addr: socket_addr_into_buf(addr) }
  }
}

impl OpModel for Connect {
  type Item = std::io::Result<()>;

  fn action(&mut self) -> Action {
    Action::Io(Op::Connect { fd: self.res.clone(), addr: self.addr })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    OpResult::Done(if completion.result < 0 {
      Err(completion.os_error())
    } else {
      Ok(())
    })
  }
}

impl OneshotOpModel for Connect {}

#[cfg(test)]
impl OpModelContract for Connect {
  impl_op_model_contract_runtime!();
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
// Bind
// ============================================================================

pub struct Bind {
  res: Resource,
  addr: SocketAddrBuf,
}

impl Bind {
  pub(crate) fn new(res: Resource, addr: SocketAddr) -> Self {
    Self::from_buf(res, socket_addr_into_buf(addr))
  }

  pub(crate) fn from_buf(res: Resource, addr: SocketAddrBuf) -> Self {
    Self { res, addr }
  }
}

impl OpModel for Bind {
  type Item = io::Result<()>;

  fn action(&mut self) -> Action {
    Action::Io(Op::Bind { fd: self.res.clone(), addr: self.addr })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    OpResult::Done(if completion.result < 0 {
      Err(completion.os_error())
    } else {
      Ok(())
    })
  }
}

impl OneshotOpModel for Bind {}

// ============================================================================
// Listen
// ============================================================================

pub struct Listen {
  res: Resource,
  backlog: i32,
}

impl Listen {
  pub(crate) fn new(res: Resource, backlog: i32) -> Self {
    Self { res, backlog }
  }
}

impl OpModel for Listen {
  type Item = io::Result<()>;

  fn action(&mut self) -> Action {
    Action::Io(Op::Listen { fd: self.res.clone(), backlog: self.backlog })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    OpResult::Done(if completion.result < 0 {
      Err(completion.os_error())
    } else {
      Ok(())
    })
  }
}

impl OneshotOpModel for Listen {}

// ============================================================================
// Shutdown
// ============================================================================

pub struct Shutdown {
  res: Resource,
  how: ShutdownHow,
}

impl Shutdown {
  pub(crate) fn new(res: Resource, how: ShutdownHow) -> Self {
    Self { res, how }
  }
}

impl OpModel for Shutdown {
  type Item = io::Result<()>;

  fn action(&mut self) -> Action {
    Action::Io(Op::Shutdown { fd: self.res.clone(), how: self.how })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    OpResult::Done(if completion.result < 0 {
      Err(completion.os_error())
    } else {
      Ok(())
    })
  }
}

impl OneshotOpModel for Shutdown {}

// ============================================================================
// Fsync
// ============================================================================

pub struct Fsync {
  res: Resource,
}

impl Fsync {
  pub(crate) fn new(res: Resource) -> Self {
    Self { res }
  }
}

impl OpModel for Fsync {
  type Item = io::Result<()>;

  fn action(&mut self) -> Action {
    Action::Io(Op::Fsync { fd: self.res.clone() })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    OpResult::Done(if completion.result < 0 {
      Err(completion.os_error())
    } else {
      Ok(())
    })
  }
}

impl OneshotOpModel for Fsync {}

#[cfg(test)]
impl OpModelContract for Fsync {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdout())
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| matches!(action, Action::Io(Op::Fsync { .. })),
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
    // A nop cancelled before it ran completes with an error (`-ECANCELED`).
    OpResult::Done(if completion.result < 0 {
      Err(completion.os_error())
    } else {
      Ok(())
    })
  }
}

impl OneshotOpModel for Nop {}

#[cfg(test)]
impl OpModelContract for Nop {
  impl_op_model_contract_runtime!();
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

/// Lowers the writable regions of `buf` into `out` with `to_raw`, returning
/// how many there are.
///
/// The regions stay valid in place for as long as the caller leaves `buf`
/// untouched.
fn collect_regions<B: IoBufMutVec, T>(
  buf: &mut B,
  out: &mut [T; MAX_IOV_COUNT],
  mut to_raw: impl FnMut(NonNull<u8>, usize) -> T,
) -> usize {
  let mut regions = IoSlicesMut::new();
  buf.bufs_mut(&mut regions);
  let count = regions.len();
  for (slot, region) in out.iter_mut().zip(regions) {
    let len = region.len();
    *slot = to_raw(NonNull::from(region).cast(), len);
  }
  count
}

#[cfg(test)]
fn stage_into<B: IoBufMutVec>(buf: &mut B, bytes: &[u8]) {
  let mut regions = IoSlicesMut::new();
  buf.bufs_mut(&mut regions);
  let mut copied = 0usize;

  for region in regions {
    let n = (bytes.len() - copied).min(region.len());
    for (dst, src) in region.iter_mut().zip(&bytes[copied..copied + n]) {
      dst.write(*src);
    }
    copied += n;
    if copied == bytes.len() {
      break;
    }
  }

  assert_eq!(copied, bytes.len(), "staged bytes exceed total buffer capacity");
}

/// The buffer an op holds from construction until `complete` hands it back.
///
/// The driver completes an op at most once and never asks it for an action
/// afterwards, so the buffer is always there when this is called.
fn held<T>(buf: Option<T>) -> T {
  let Some(buf) = buf else {
    unreachable!("buffer not available: the op was already completed");
  };
  buf
}

/// The error for a file offset past [`MAX_FILE_OFFSET`], which no kernel can
/// address.
///
/// [`MAX_FILE_OFFSET`]: crate::backend::op::MAX_FILE_OFFSET
fn offset_out_of_range_error() -> io::Error {
  io::Error::new(
    io::ErrorKind::InvalidInput,
    "file offset exceeds the largest offset the OS supports (i64::MAX)",
  )
}

// ============================================================================
// Read
// ============================================================================

pub struct Read<B: IoBufMutVec> {
  res: Resource,
  buf: Option<B>,
  raws: [RawBuf; MAX_IOV_COUNT],
  iov_count: usize,
  offset: Option<u64>,
}

impl<B: IoBufMutVec> Read<B> {
  /// Reads at `offset`, or from the current position when `None`.
  ///
  /// An offset past [`MAX_FILE_OFFSET`] submits no read and completes with
  /// `InvalidInput`.
  ///
  /// [`MAX_FILE_OFFSET`]: crate::backend::op::MAX_FILE_OFFSET
  pub(crate) fn new(res: Resource, buf: B, offset: Option<u64>) -> Self {
    Self {
      res,
      buf: Some(buf),
      // SAFETY: `RawBuf` is a plain pointer/length pair and is immediately
      // overwritten before submission.
      raws: unsafe { mem::zeroed() },
      iov_count: 0,
      offset,
    }
  }

  #[cfg(test)]
  fn stage_read_data(&mut self, bytes: &[u8]) {
    stage_into(held(self.buf.as_mut()), bytes);
  }
}

impl<B: IoBufMutVec> OpModel for Read<B> {
  type Item = BufResult<usize, B>;

  fn action(&mut self) -> Action {
    if !is_valid_file_offset(self.offset) {
      return Action::Io(Op::Nop);
    }
    let buf = held(self.buf.as_mut());
    self.iov_count = collect_regions(buf, &mut self.raws, |ptr, len| {
      // SAFETY: the region is model-owned storage that stays valid in place
      // until `complete()` consumes the buffer.
      unsafe { RawBuf::from_raw_parts(ptr.as_ptr(), len) }
    });

    Action::Io(Op::Read {
      fd: self.res.clone(),
      iovecs: NonNull::from(&mut self.raws).cast(),
      iov_count: self.iov_count,
      offset: self.offset,
      flags: ReadFlags::empty(),
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let mut buf = held(self.buf.take());

    let result = if !is_valid_file_offset(self.offset) {
      Err(offset_out_of_range_error())
    } else if completion.result < 0 {
      Err(completion.os_error())
    } else {
      let mut remaining = completion.result as usize;
      for i in 0..self.iov_count {
        let cap = self.raws[i].len;
        let len = remaining.min(cap);
        // SAFETY: the kernel filled the regions submitted in `action()` in
        // order, so the first `len <= cap` bytes of region `i` are initialized.
        unsafe { buf.set_buf_len(i, len) };
        remaining = remaining.saturating_sub(cap);
      }
      Ok(completion.result as usize)
    };

    OpResult::Done((result, buf))
  }
}

impl<B: IoBufMutVec> OneshotOpModel for Read<B> {}

#[cfg(test)]
impl OpModelContract for Read<Vec<u8>> {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), vec![0u8; 8], None)
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| {
        matches!(
          action,
          Action::Io(Op::Read {
            iov_count: 1,
            offset: None,
            flags,
            ..
          }) if flags.is_empty()
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
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), (vec![0u8; 3], vec![0u8; 5]), None)
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| {
        matches!(
          action,
          Action::Io(Op::Read {
            iov_count: 2,
            offset: None,
            flags,
            ..
          }) if flags.is_empty()
        )
      },
      |model| model.stage_read_data(b"abcde"),
      Completion::new(5),
      |result: &TwoVecOpResult| match result {
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
  offset: Option<u64>,
}

impl<B: IoBufVec + std::marker::Send + Sync> Write<B> {
  /// Writes at `offset`, or at the current position when `None`.
  ///
  /// An offset past [`MAX_FILE_OFFSET`] submits no write and completes with
  /// `InvalidInput`.
  ///
  /// [`MAX_FILE_OFFSET`]: crate::backend::op::MAX_FILE_OFFSET
  pub(crate) fn new(res: Resource, buf: B, offset: Option<u64>) -> Self {
    Self {
      res,
      buf: Some(buf),
      // SAFETY: `RawBuf` is a plain pointer/length pair and is immediately
      // overwritten before submission.
      raws: unsafe { mem::zeroed() },
      offset,
    }
  }
}

impl<B: IoBufVec + std::marker::Send + Sync + 'static> OpModel for Write<B> {
  type Item = BufResult<usize, B>;

  fn action(&mut self) -> Action {
    if !is_valid_file_offset(self.offset) {
      return Action::Io(Op::Nop);
    }
    let buf = held(self.buf.as_ref());
    let buf_count = buf.buf_count().min(MAX_IOV_COUNT);

    for i in 0..buf_count {
      self.raws[i] = RawBuf::new(buf.buf(i));
    }

    Action::Io(Op::Write {
      fd: self.res.clone(),
      iovecs: NonNull::from(&mut self.raws).cast(),
      iov_count: buf_count,
      offset: self.offset,
      flags: WriteFlags::empty(),
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let buf = held(self.buf.take());
    let result = if !is_valid_file_offset(self.offset) {
      Err(offset_out_of_range_error())
    } else if completion.result < 0 {
      Err(completion.os_error())
    } else {
      Ok(completion.result as usize)
    };
    OpResult::Done((result, buf))
  }
}

impl<B: IoBufVec + std::marker::Send + Sync + 'static> OneshotOpModel
  for Write<B>
{
}

#[cfg(test)]
impl OpModelContract for Write<Vec<u8>> {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), b"pong".to_vec(), None)
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| {
        matches!(
          action,
          Action::Io(Op::Write {
            iov_count: 1,
            offset: None,
            flags,
            ..
          }) if flags.is_empty()
        )
      },
      Completion::new(4),
      |result: &OpResult<BufResult<usize, Vec<u8>>>| match result {
        OpResult::Done((Ok(4), buf)) => buf.as_slice() == b"pong",
        _ => false,
      },
    )]
  }
}

#[cfg(test)]
impl OpModelContract for Write<(Vec<u8>, Vec<u8>)> {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), (b"ab".to_vec(), b"cde".to_vec()), None)
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| {
        matches!(
          action,
          Action::Io(Op::Write {
            iov_count: 2,
            offset: None,
            flags,
            ..
          }) if flags.is_empty()
        )
      },
      Completion::new(5),
      |result: &TwoVecOpResult| match result {
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

struct RecvCore<B: IoBufMutVec + std::marker::Send + Sync> {
  res: Resource,
  buf: Option<B>,
  bufs: [MsgBufMut; MAX_IOV_COUNT],
  iov_count: usize,
  flags: RecvFlags,
  from: Option<SocketAddrBuf>,
}

impl<B: IoBufMutVec + std::marker::Send + Sync> RecvCore<B> {
  fn new(
    res: Resource,
    buf: B,
    flags: Option<RecvFlags>,
    with_from: bool,
  ) -> Self {
    Self {
      res,
      buf: Some(buf),
      // SAFETY: a dangling pointer with zero length is a placeholder until
      // `action()` installs real buffer slices.
      bufs: [unsafe { MsgBufMut::from_raw_parts(NonNull::dangling(), 0) };
        MAX_IOV_COUNT],
      iov_count: 0,
      flags: flags.unwrap_or(RecvFlags::empty()),
      from: with_from.then(SocketAddrBuf::unspecified),
    }
  }

  fn action(&mut self) -> Action {
    let buf = held(self.buf.as_mut());
    let buf_count = collect_regions(buf, &mut self.bufs, |ptr, len| {
      // SAFETY: the region is model-owned storage that stays valid in place
      // until `complete_buf()` consumes the buffer.
      unsafe { MsgBufMut::from_raw_parts(ptr, len) }
    });
    self.iov_count = buf_count;
    // The contract takes at least one region: an empty collection receives
    // into the zero-length placeholder, as `recvmsg` would with no iovecs.
    let regions = &self.bufs[..buf_count.max(1)];

    let msg = match self.from.as_mut() {
      Some(addr) => MsgRecv::with_from(regions, NonNull::from(addr)),
      None => MsgRecv::new(regions),
    };

    Action::Io(Op::Recv { fd: self.res.clone(), msg, flags: self.flags })
  }

  fn complete_buf(
    &mut self,
    completion: Completion,
  ) -> (std::io::Result<usize>, B) {
    let mut buf = held(self.buf.take());

    let result = if completion.result < 0 {
      Err(completion.os_error())
    } else {
      let mut remaining = completion.result as usize;
      for i in 0..self.iov_count {
        let cap = self.bufs[i].len;
        let len = remaining.min(cap);
        // SAFETY: the kernel filled the regions submitted in `action()` in
        // order, so the first `len <= cap` bytes of region `i` are initialized.
        unsafe { buf.set_buf_len(i, len) };
        remaining = remaining.saturating_sub(cap);
      }
      Ok(completion.result as usize)
    };

    (result, buf)
  }

  #[cfg(test)]
  fn stage_recv_data(&mut self, bytes: &[u8]) {
    stage_into(held(self.buf.as_mut()), bytes);
  }

  #[cfg(test)]
  fn stage_from_addr(&mut self, addr: SocketAddr) {
    let Some(from) = self.from.as_mut() else {
      panic!("from address capture not enabled");
    };
    *from = socket_addr_into_buf(addr);
  }
}

pub struct Recv<B: IoBufMutVec + std::marker::Send + Sync> {
  core: RecvCore<B>,
}

impl<B: IoBufMutVec + std::marker::Send + Sync> Recv<B> {
  pub(crate) fn new(res: Resource, buf: B, flags: Option<RecvFlags>) -> Self {
    Self { core: RecvCore::new(res, buf, flags, false) }
  }
}

impl<B: IoBufMutVec + std::marker::Send + Sync> OpModel for Recv<B> {
  type Item = BufResult<usize, B>;

  fn action(&mut self) -> Action {
    self.core.action()
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    OpResult::Done(self.core.complete_buf(completion))
  }
}

impl<B: IoBufMutVec + std::marker::Send + Sync> OneshotOpModel for Recv<B> {}

pub struct RecvFrom<B: IoBufMutVec + std::marker::Send + Sync> {
  core: RecvCore<B>,
}

impl<B: IoBufMutVec + std::marker::Send + Sync> RecvFrom<B> {
  pub(crate) fn new(res: Resource, buf: B, flags: Option<RecvFlags>) -> Self {
    Self { core: RecvCore::new(res, buf, flags, true) }
  }
}

impl<B: IoBufMutVec + std::marker::Send + Sync> OpModel for RecvFrom<B> {
  type Item = (std::io::Result<usize>, B, Option<SocketAddr>);

  fn action(&mut self) -> Action {
    self.core.action()
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let (result, buf) = self.core.complete_buf(completion);
    let addr = result
      .as_ref()
      .ok()
      .and(self.core.from.as_ref())
      .and_then(|from| socket_addr_from_buf(from).ok());
    OpResult::Done((result, buf, addr))
  }
}

impl<B: IoBufMutVec + std::marker::Send + Sync> OneshotOpModel for RecvFrom<B> {}

#[cfg(test)]
impl OpModelContract for Recv<Vec<u8>> {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), vec![0u8; 8], Some(TEST_RECV_FLAGS))
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| match action {
        Action::Io(Op::Recv { flags, .. }) => *flags == TEST_RECV_FLAGS,
        _ => false,
      },
      |model| model.core.stage_recv_data(b"recv"),
      Completion::new(4),
      |result: &OpResult<BufResult<usize, Vec<u8>>>| match result {
        OpResult::Done((Ok(4), buf)) => buf.as_slice() == b"recv",
        _ => false,
      },
    )]
  }
}

#[cfg(test)]
impl OpModelContract for Recv<(Vec<u8>, Vec<u8>)> {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(
      Resource::stdin(),
      (vec![0u8; 2], vec![0u8; 4]),
      Some(TEST_RECV_FLAGS),
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| match action {
        Action::Io(Op::Recv { flags, .. }) => *flags == TEST_RECV_FLAGS,
        _ => false,
      },
      |model| model.core.stage_recv_data(b"hello"),
      Completion::new(5),
      |result: &TwoVecOpResult| match result {
        OpResult::Done((Ok(5), (a, b))) => {
          a.as_slice() == b"he" && b.as_slice() == b"llo"
        }
        _ => false,
      },
    )]
  }
}

#[cfg(test)]
impl OpModelContract for RecvFrom<Vec<u8>> {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), vec![0u8; 8], Some(TEST_RECV_FLAGS))
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| match action {
        Action::Io(Op::Recv { flags, .. }) => *flags == TEST_RECV_FLAGS,
        _ => false,
      },
      |model| {
        model.core.stage_recv_data(b"recv");
        model
          .core
          .stage_from_addr("127.0.0.1:7000".parse().expect("socket addr"));
      },
      Completion::new(4),
      |result| match result {
        OpResult::Done((Ok(4), buf, Some(addr))) => {
          buf.as_slice() == b"recv"
            && *addr == "127.0.0.1:7000".parse().unwrap()
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
  bufs: [MsgBuf; MAX_IOV_COUNT],
  flags: SendFlags,
  addr: Option<SocketAddr>,
}

// SAFETY: `Send` owns the buffer and the message slices only point into
// that owned buffer and into fields of the same struct. The operation is driven
// by a single owning thread.
unsafe impl<B: IoBufVec + std::marker::Send + Sync> std::marker::Send
  for Send<B>
{
}
// SAFETY: shared references do not permit mutation of the pointed-to storage
// outside the owning operation flow.
unsafe impl<B: IoBufVec + std::marker::Send + Sync> std::marker::Sync
  for Send<B>
{
}

impl<B: IoBufVec + std::marker::Send + Sync> Send<B> {
  pub(crate) fn new(
    res: Resource,
    buf: B,
    addr: Option<SocketAddr>,
    flags: Option<SendFlags>,
  ) -> Self {
    Self {
      res,
      buf: Some(buf),
      // SAFETY: a dangling pointer with zero length is a placeholder until
      // `action()` installs real buffer slices.
      bufs: [unsafe { MsgBuf::from_raw_parts(NonNull::dangling(), 0) };
        MAX_IOV_COUNT],
      flags: flags.unwrap_or(SendFlags::empty()),
      addr,
    }
  }

  fn hydrate_msg(&mut self) -> MsgSend {
    let buf = held(self.buf.as_ref());
    let buf_count = buf.buf_count().min(MAX_IOV_COUNT);

    for i in 0..buf_count {
      self.bufs[i] = MsgBuf::from_slice(buf.buf(i));
    }

    // The contract takes at least one region: an empty collection sends the
    // zero-length placeholder, as `sendmsg` would with no iovecs.
    MsgSend::new(&self.bufs[..buf_count.max(1)], self.addr)
  }
}

impl<B: IoBufVec + std::marker::Send + Sync + 'static> OpModel for Send<B> {
  type Item = BufResult<usize, B>;

  fn action(&mut self) -> Action {
    let msg = self.hydrate_msg();
    Action::Io(Op::Send { fd: self.res.clone(), msg, flags: self.flags })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let buf = held(self.buf.take());
    let result = if completion.result < 0 {
      Err(completion.os_error())
    } else {
      Ok(completion.result as usize)
    };
    OpResult::Done((result, buf))
  }
}

impl<B: IoBufVec + std::marker::Send + Sync + 'static> OneshotOpModel
  for Send<B>
{
}

#[cfg(test)]
impl OpModelContract for Send<Vec<u8>> {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), b"send".to_vec(), None, Some(TEST_SEND_FLAGS))
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| match action {
        Action::Io(Op::Send { flags, .. }) => *flags == TEST_SEND_FLAGS,
        _ => false,
      },
      Completion::new(4),
      |result: &OpResult<BufResult<usize, Vec<u8>>>| match result {
        OpResult::Done((Ok(4), buf)) => buf.as_slice() == b"send",
        _ => false,
      },
    )]
  }
}

#[cfg(test)]
impl OpModelContract for Send<(Vec<u8>, Vec<u8>)> {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(
      Resource::stdin(),
      (b"he".to_vec(), b"llo".to_vec()),
      None,
      Some(TEST_SEND_FLAGS),
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| match action {
        Action::Io(Op::Send { flags, .. }) => *flags == TEST_SEND_FLAGS,
        _ => false,
      },
      Completion::new(5),
      |result: &TwoVecOpResult| match result {
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
    assert!(completion.flags.contains(CompletionFlags::TIMER));

    let result =
      if completion.result == 0 { Ok(()) } else { Err(completion.os_error()) };

    OpResult::Done(result)
  }
}

impl OneshotOpModel for Sleep {}

#[cfg(test)]
impl OpModelContract for Sleep {
  impl_op_model_contract_runtime!();
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
    assert!(completion.flags.contains(CompletionFlags::TIMER));

    let result =
      if completion.result == 0 { Ok(()) } else { Err(completion.os_error()) };

    OpResult::Yield(result)
  }
}

impl StreamOpModel for Interval {}

#[cfg(test)]
impl OpModelContract for Interval {
  impl_op_model_contract_runtime!();
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
  flags: OpenFlags,
  mode: FileMode,
}

impl OpenAt {
  pub(crate) fn new(
    dir_res: Resource,
    pathname: CString,
    flags: OpenFlags,
    mode: FileMode,
  ) -> Self {
    Self { dir_res, pathname, flags, mode }
  }
}

impl OpModel for OpenAt {
  type Item = std::io::Result<Resource>;

  fn action(&mut self) -> Action {
    Action::Io(Op::OpenAt {
      dir_fd: self.dir_res.clone(),
      path: cstring_to_osstring(&self.pathname),
      flags: self.flags,
      mode: self.mode,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    // SAFETY: a successful open completion carries a new owned handle.
    OpResult::Done(unsafe { Resource::from_completion(&completion) })
  }
}

impl OneshotOpModel for OpenAt {}

pub struct Stat {
  target: StatModelTarget,
  out: FileStat,
}

enum StatModelTarget {
  Path {
    dir_res: Resource,
    pathname: CString,
    follow_symlinks: bool,
  },
  Fd {
    fd: Resource,
  },
  /// The path could not be represented; completes with `InvalidInput`.
  // Only `lio::fs` (feature `high`) takes paths that can contain NUL bytes.
  #[cfg_attr(not(feature = "high"), allow(dead_code))]
  InvalidPath,
}

/// The error for a path that contains a NUL byte, as `std::fs` reports it.
pub(crate) fn nul_path_error() -> std::io::Error {
  std::io::Error::new(
    std::io::ErrorKind::InvalidInput,
    "path contains an interior NUL byte",
  )
}

impl Stat {
  pub(crate) fn new_at(
    dir_res: Resource,
    pathname: CString,
    follow_symlinks: bool,
  ) -> Self {
    Self {
      target: StatModelTarget::Path { dir_res, pathname, follow_symlinks },
      out: FileStat::zeroed(),
    }
  }

  pub(crate) fn new_fd(fd: Resource) -> Self {
    Self { target: StatModelTarget::Fd { fd }, out: FileStat::zeroed() }
  }

  /// A stat that fails with `InvalidInput` without touching the file system.
  #[cfg_attr(not(feature = "high"), allow(dead_code))]
  pub(crate) fn invalid_path() -> Self {
    Self { target: StatModelTarget::InvalidPath, out: FileStat::zeroed() }
  }
}

impl OpModel for Stat {
  type Item = std::io::Result<FileStat>;

  fn action(&mut self) -> Action {
    let target = match &mut self.target {
      StatModelTarget::Path { dir_res, pathname, follow_symlinks } => {
        StatTarget::Path {
          dir_fd: dir_res.clone(),
          path: cstring_to_osstring(pathname),
          follow_symlinks: *follow_symlinks,
        }
      }
      StatModelTarget::Fd { fd } => StatTarget::Fd { fd: fd.clone() },
      StatModelTarget::InvalidPath => return Action::Io(Op::Nop),
    };
    Action::Io(Op::Stat { target, out: NonNull::from(&mut self.out) })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let res = if matches!(self.target, StatModelTarget::InvalidPath) {
      Err(nul_path_error())
    } else if completion.result < 0 {
      Err(completion.os_error())
    } else {
      Ok(self.out)
    };
    OpResult::Done(res)
  }
}

impl OneshotOpModel for Stat {}

pub struct ReadDir {
  fd: Resource,
  buf: ReadDirBuf,
}

impl ReadDir {
  pub(crate) fn new(fd: Resource, buf: ReadDirBuf) -> Self {
    Self { fd, buf }
  }
}

impl OpModel for ReadDir {
  type Item = std::io::Result<ReadDirBuf>;

  fn action(&mut self) -> Action {
    Action::Io(Op::ReadDir {
      fd: self.fd.clone(),
      raw_buf: NonNull::from(self.buf.raw.as_mut_slice()).cast(),
      raw_cap: self.buf.raw.len(),
      entries: NonNull::from(self.buf.entries.as_mut_slice()).cast(),
      entries_cap: self.buf.entries.len(),
      opaque: NonNull::from(&mut self.buf.opaque),
      opaque_drop: NonNull::from(&mut self.buf.opaque_drop),
      out: NonNull::from(&mut self.buf.result),
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let res = if completion.result < 0 {
      Err(completion.os_error())
    } else {
      Ok(std::mem::take(&mut self.buf))
    };
    OpResult::Done(res)
  }
}

impl OneshotOpModel for ReadDir {}

#[cfg(test)]
impl OpModelContract for ReadDir {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::cwd(), ReadDirBuf::with_capacity(64, 4))
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| matches!(action, Action::Io(Op::ReadDir { .. })),
      |model| {
        model.buf.raw[..5].copy_from_slice(b"child");
        model.buf.entries[0] = crate::backend::op::DirEntryRef {
          name_offset: 0,
          name_len: 5,
          file_type: Some(crate::backend::op::FileType::File),
          ino: Some(1),
        };
        model.buf.result = crate::backend::op::ReadDirResult {
          entries: 1,
          raw_written: 5,
          eof: true,
        };
      },
      Completion::new(0),
      |result| {
        matches!(
          result,
          OpResult::Done(Ok(buf))
            if buf.result.entries == 1
              && buf.iter().next().map(|entry| entry.name) == Some(&b"child"[..])
        )
      },
    )]
  }
}

#[cfg(test)]
impl OpModelContract for Stat {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new_at(Resource::cwd(), CString::new("file").expect("cstring"), true)
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| {
        matches!(
          action,
          Action::Io(Op::Stat {
            target: StatTarget::Path { follow_symlinks: true, .. },
            ..
          })
        )
      },
      |model| {
        // S_IFREG | 0o644
        let mode = 0o100644;
        model.out = FileStat {
          file_type: crate::backend::op::FileType::File,
          size: 7,
          permissions: 0o644,
          mode,
          nlink: 1,
          uid: 1000,
          gid: 1000,
        };
      },
      Completion::new(0),
      |result| {
        matches!(
          result,
          OpResult::Done(Ok(stat))
            if stat.is_file() && stat.len() == 7 && stat.permissions == 0o644
        )
      },
    )]
  }
}

pub struct UnlinkAt {
  dir_res: Resource,
  pathname: CString,
  kind: UnlinkKind,
}

impl UnlinkAt {
  pub(crate) fn new(
    dir_res: Resource,
    pathname: CString,
    kind: UnlinkKind,
  ) -> Self {
    Self { dir_res, pathname, kind }
  }
}

impl OpModel for UnlinkAt {
  type Item = std::io::Result<()>;

  fn action(&mut self) -> Action {
    Action::Io(Op::UnlinkAt {
      dir_fd: self.dir_res.clone(),
      path: cstring_to_osstring(&self.pathname),
      kind: self.kind,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let res =
      if completion.result < 0 { Err(completion.os_error()) } else { Ok(()) };
    OpResult::Done(res)
  }
}

impl OneshotOpModel for UnlinkAt {}

#[cfg(test)]
impl OpModelContract for UnlinkAt {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(
      Resource::cwd(),
      CString::new("tmp-file").expect("cstring"),
      UnlinkKind::Directory,
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| {
        matches!(
          action,
          Action::Io(Op::UnlinkAt { kind: UnlinkKind::Directory, .. })
        )
      },
      Completion::new(0),
      |result| matches!(result, OpResult::Done(Ok(()))),
    )]
  }
}

pub struct RenameAt {
  old_dir_res: Resource,
  old_pathname: CString,
  new_dir_res: Resource,
  new_pathname: CString,
}

impl RenameAt {
  pub(crate) fn new(
    old_dir_res: Resource,
    old_pathname: CString,
    new_dir_res: Resource,
    new_pathname: CString,
  ) -> Self {
    Self { old_dir_res, old_pathname, new_dir_res, new_pathname }
  }
}

impl OpModel for RenameAt {
  type Item = std::io::Result<()>;

  fn action(&mut self) -> Action {
    Action::Io(Op::RenameAt {
      old_dir_fd: self.old_dir_res.clone(),
      old_path: cstring_to_osstring(&self.old_pathname),
      new_dir_fd: self.new_dir_res.clone(),
      new_path: cstring_to_osstring(&self.new_pathname),
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let res =
      if completion.result < 0 { Err(completion.os_error()) } else { Ok(()) };
    OpResult::Done(res)
  }
}

impl OneshotOpModel for RenameAt {}

#[cfg(test)]
impl OpModelContract for RenameAt {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(
      Resource::cwd(),
      CString::new("old-name").expect("cstring"),
      Resource::cwd(),
      CString::new("new-name").expect("cstring"),
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| matches!(action, Action::Io(Op::RenameAt { .. })),
      Completion::new(0),
      |result| matches!(result, OpResult::Done(Ok(()))),
    )]
  }
}

pub struct MkdirAt {
  dir_res: Resource,
  pathname: CString,
  mode: FileMode,
}

impl MkdirAt {
  pub(crate) fn new(
    dir_res: Resource,
    pathname: CString,
    mode: FileMode,
  ) -> Self {
    Self { dir_res, pathname, mode }
  }
}

impl OpModel for MkdirAt {
  type Item = std::io::Result<()>;

  fn action(&mut self) -> Action {
    Action::Io(Op::MkdirAt {
      dir_fd: self.dir_res.clone(),
      path: cstring_to_osstring(&self.pathname),
      mode: self.mode,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let res =
      if completion.result < 0 { Err(completion.os_error()) } else { Ok(()) };
    OpResult::Done(res)
  }
}

impl OneshotOpModel for MkdirAt {}

#[cfg(test)]
impl OpModelContract for MkdirAt {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(
      Resource::cwd(),
      CString::new("new-dir").expect("cstring"),
      FileMode::from_octal(0o755).unwrap(),
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| matches!(action, Action::Io(Op::MkdirAt { mode, .. }) if mode.octal() == 0o755),
      Completion::new(0),
      |result| matches!(result, OpResult::Done(Ok(()))),
    )]
  }
}

pub struct LinkAt {
  source_dir_res: Resource,
  source_pathname: CString,
  new_dir_res: Resource,
  new_pathname: CString,
  kind: LinkKind,
}

impl LinkAt {
  pub(crate) fn new(
    source_dir_res: Resource,
    source_pathname: CString,
    new_dir_res: Resource,
    new_pathname: CString,
    kind: LinkKind,
  ) -> Self {
    Self { source_dir_res, source_pathname, new_dir_res, new_pathname, kind }
  }
}

impl OpModel for LinkAt {
  type Item = std::io::Result<()>;

  fn action(&mut self) -> Action {
    Action::Io(Op::LinkAt {
      kind: self.kind,
      source_dir_fd: self.source_dir_res.clone(),
      source_path: cstring_to_osstring(&self.source_pathname),
      new_dir_fd: self.new_dir_res.clone(),
      new_path: cstring_to_osstring(&self.new_pathname),
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let res =
      if completion.result < 0 { Err(completion.os_error()) } else { Ok(()) };
    OpResult::Done(res)
  }
}

impl OneshotOpModel for LinkAt {}

#[cfg(test)]
impl OpModelContract for LinkAt {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(
      Resource::cwd(),
      CString::new("old-name").expect("cstring"),
      Resource::cwd(),
      CString::new("new-name").expect("cstring"),
      LinkKind::Hard,
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| {
        matches!(action, Action::Io(Op::LinkAt { kind: LinkKind::Hard, .. }))
      },
      Completion::new(0),
      |result| matches!(result, OpResult::Done(Ok(()))),
    )]
  }
}

pub struct ReadlinkAt<B: IoBufMutVec + std::marker::Send + Sync> {
  dir_res: Resource,
  pathname: CString,
  buf: Option<B>,
  raw: RawBuf,
  raw_len: usize,
}

impl<B: IoBufMutVec + std::marker::Send + Sync> ReadlinkAt<B> {
  pub(crate) fn new(dir_res: Resource, pathname: CString, buf: B) -> Self {
    Self {
      dir_res,
      pathname,
      buf: Some(buf),
      // SAFETY: null with zero length is a placeholder until `action()`.
      raw: unsafe { RawBuf::from_raw_parts(ptr::null_mut(), 0) },
      raw_len: 0,
    }
  }
}

impl<B: IoBufMutVec + std::marker::Send + Sync> OpModel for ReadlinkAt<B> {
  type Item = BufResult<usize, B>;

  fn action(&mut self) -> Action {
    let buf = held(self.buf.as_mut());
    let mut regions = IoSlicesMut::new();
    buf.bufs_mut(&mut regions);
    let Some(region) = regions.into_iter().next() else {
      panic!("readlink needs a buffer with at least one region");
    };
    let len = region.len();
    let ptr = NonNull::from(region).cast::<u8>();
    // SAFETY: the region is model-owned storage that stays in place until
    // `complete()` consumes the buffer.
    self.raw = unsafe { RawBuf::from_raw_parts(ptr.as_ptr(), len) };
    self.raw_len = len;

    Action::Io(Op::ReadlinkAt {
      dir_fd: self.dir_res.clone(),
      path: cstring_to_osstring(&self.pathname),
      buf: ptr,
      buf_len: self.raw_len,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let mut buf = held(self.buf.take());
    let result = if completion.result < 0 {
      Err(completion.os_error())
    } else {
      let len = (completion.result as usize).min(self.raw_len);
      // SAFETY: the kernel wrote `len <= raw_len` bytes into the region
      // submitted in `action()`.
      unsafe { buf.set_buf_len(0, len) };
      Ok(completion.result as usize)
    };
    OpResult::Done((result, buf))
  }
}

impl<B: IoBufMutVec + std::marker::Send + Sync> OneshotOpModel
  for ReadlinkAt<B>
{
}

pub struct GetCwd<B: IoBufMutVec + std::marker::Send + Sync> {
  buf: Option<B>,
  out: std::ffi::OsString,
}

impl<B: IoBufMutVec + std::marker::Send + Sync> GetCwd<B> {
  pub(crate) fn new(buf: B) -> Self {
    Self { buf: Some(buf), out: std::ffi::OsString::new() }
  }
}

impl<B: IoBufMutVec + std::marker::Send + Sync> OpModel for GetCwd<B> {
  type Item = BufResult<usize, B>;

  fn action(&mut self) -> Action {
    Action::Io(Op::GetCwd { out: NonNull::from(&mut self.out) })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let mut buf = held(self.buf.take());
    let result = if completion.result < 0 {
      Err(completion.os_error())
    } else {
      let bytes = self.out.as_encoded_bytes();

      let mut regions = IoSlicesMut::new();
      buf.bufs_mut(&mut regions);
      let first = regions.into_iter().next();
      match first {
        Some(region) if bytes.len() <= region.len() => {
          for (dst, src) in region.iter_mut().zip(bytes) {
            dst.write(*src);
          }
          // SAFETY: the first `bytes.len()` bytes of region 0 were just
          // initialized above.
          unsafe { buf.set_buf_len(0, bytes.len()) };
          Ok(bytes.len())
        }
        _ => Err(std::io::Error::from_raw_os_error(
          crate::backend::native::BUFFER_TOO_SMALL,
        )),
      }
    };
    OpResult::Done((result, buf))
  }
}

impl<B: IoBufMutVec + std::marker::Send + Sync> OneshotOpModel for GetCwd<B> {}

#[cfg(test)]
impl OpModelContract for GetCwd<Vec<u8>> {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(vec![0; 32])
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| matches!(action, Action::Io(Op::GetCwd { .. })),
      |model| {
        #[cfg(unix)]
        {
          use std::os::unix::ffi::OsStringExt;
          model.out = std::ffi::OsString::from_vec(b"/tmp".to_vec());
        }
        #[cfg(not(unix))]
        {
          model.out = std::ffi::OsString::from("/tmp");
        }
      },
      Completion::new(0),
      |result| match result {
        OpResult::Done((Ok(bytes), buf)) => *bytes == 4 && &buf[..4] == b"/tmp",
        _ => false,
      },
    )]
  }
}

pub struct Spawn {
  spec: crate::backend::op::SpawnSpec,
}

impl Spawn {
  pub(crate) fn new(
    path: CString,
    argv: Vec<CString>,
    envp: Option<Vec<CString>>,
  ) -> Self {
    Self {
      spec: crate::backend::op::SpawnSpec {
        program: cstring_to_osstring(&path),
        args: argv.iter().map(cstring_to_osstring).collect(),
        env: envp.map(|vars| vars.iter().map(cstring_to_osstring).collect()),
      },
    }
  }
}

impl OpModel for Spawn {
  type Item = std::io::Result<crate::api::Pid>;

  fn action(&mut self) -> Action {
    Action::Io(Op::Spawn { spec: self.spec.clone() })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let result = if completion.result < 0 {
      Err(completion.os_error())
    } else {
      Ok(crate::api::Pid::from_raw(completion.result as i64))
    };
    OpResult::Done(result)
  }
}

impl OneshotOpModel for Spawn {}

#[cfg(test)]
impl OpModelContract for Spawn {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(
      CString::new("/bin/echo").expect("cstring"),
      vec![CString::new("echo").expect("cstring")],
      None,
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| matches!(action, Action::Io(Op::Spawn { .. })),
      Completion::new(1234),
      |result: &OpResult<std::io::Result<crate::api::Pid>>| matches!(result, OpResult::Done(Ok(pid)) if pid.as_raw() == 1234),
    )]
  }
}

#[cfg(test)]
impl OpModelContract for ReadlinkAt<Vec<u8>> {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(
      Resource::cwd(),
      CString::new("link-name").expect("cstring"),
      vec![0; 32],
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| matches!(action, Action::Io(Op::ReadlinkAt { .. })),
      |model| {
        model.buf.as_mut().expect("buffer available")[..4]
          .copy_from_slice(b"dest");
      },
      Completion::new(4),
      |result| match result {
        OpResult::Done((Ok(bytes), buf)) => *bytes == 4 && &buf[..4] == b"dest",
        _ => false,
      },
    )]
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  mod nop_contract {
    use super::*;

    lio_test::test_op_model_contract!(Nop);

    #[test]
    fn cancelled_nop_completes_with_its_error() {
      let OpResult::Done(Err(err)) =
        OpModel::complete(&mut Nop, Completion::new(-125))
      else {
        panic!("a failed nop must complete with an error");
      };
      assert_eq!(err.raw_os_error(), Some(125));
    }
  }

  mod socket_contract {
    use super::*;

    lio_test::test_op_model_contract!(Socket);
  }

  mod getcwd_contract {
    use super::*;

    lio_test::test_op_model_contract!(GetCwd<Vec<u8>>);
  }

  mod stat_contract {
    use super::*;

    lio_test::test_op_model_contract!(Stat);
  }

  mod recvfrom_contract {
    use super::*;

    lio_test::test_op_model_contract!(RecvFrom<Vec<u8>>);
  }

  mod readdir_contract {
    use super::*;

    lio_test::test_op_model_contract!(ReadDir);
  }

  mod spawn_contract {
    use super::*;

    lio_test::test_op_model_contract!(Spawn);
  }

  mod read_contract {
    use super::*;

    lio_test::test_op_model_contract!(Read<Vec<u8>>);
  }

  mod read_vectored_contract {
    use super::*;

    lio_test::test_op_model_contract!(Read<(Vec<u8>, Vec<u8>)>);
  }

  mod write_contract {
    use super::*;

    lio_test::test_op_model_contract!(Write<Vec<u8>>);
  }

  mod write_vectored_contract {
    use super::*;

    lio_test::test_op_model_contract!(Write<(Vec<u8>, Vec<u8>)>);
  }

  mod fsync_contract {
    use super::*;

    lio_test::test_op_model_contract!(Fsync);
  }

  mod unlinkat_contract {
    use super::*;

    lio_test::test_op_model_contract!(UnlinkAt);
  }

  mod renameat_contract {
    use super::*;

    lio_test::test_op_model_contract!(RenameAt);
  }

  mod mkdirat_contract {
    use super::*;

    lio_test::test_op_model_contract!(MkdirAt);
  }

  mod linkat_contract {
    use super::*;

    lio_test::test_op_model_contract!(LinkAt);
  }

  mod readlinkat_contract {
    use super::*;

    lio_test::test_op_model_contract!(ReadlinkAt<Vec<u8>>);
  }

  mod accept_contract {
    use super::*;

    lio_test::test_op_model_contract!(Accept);
  }

  mod accept_unix_contract {
    use super::*;

    lio_test::test_op_model_contract!(AcceptUnix);
  }

  mod connect_contract {
    use super::*;

    lio_test::test_op_model_contract!(Connect);
  }

  mod sleep_contract {
    use super::*;

    lio_test::test_op_model_contract!(Sleep);
  }

  mod interval_contract {
    use super::*;

    lio_test::test_op_model_contract!(Interval);
  }

  mod recv_contract {
    use super::*;

    lio_test::test_op_model_contract!(Recv<Vec<u8>>);
  }

  mod recv_vectored_contract {
    use super::*;

    lio_test::test_op_model_contract!(Recv<(Vec<u8>, Vec<u8>)>);
  }

  mod send_contract {
    use super::*;

    lio_test::test_op_model_contract!(Send<Vec<u8>>);
  }

  mod send_vectored_contract {
    use super::*;

    lio_test::test_op_model_contract!(Send<(Vec<u8>, Vec<u8>)>);
  }

  #[test]
  fn empty_buffer_collections_submit_one_empty_region() {
    let mut send =
      Send::new(Resource::stdin(), Vec::<Vec<u8>>::new(), None, None);
    let Action::Io(Op::Send { msg, .. }) = OpModel::action(&mut send) else {
      panic!("send must submit a send");
    };
    assert_eq!(msg.buf_count.get(), 1);
    // SAFETY: `msg` points into `send`, which is still alive.
    assert_eq!(unsafe { msg.bufs.as_ref() }.len, 0);

    let mut recv = Recv::new(Resource::stdin(), Vec::<Vec<u8>>::new(), None);
    let Action::Io(Op::Recv { msg, .. }) = OpModel::action(&mut recv) else {
      panic!("recv must submit a recv");
    };
    assert_eq!(msg.buf_count.get(), 1);
    // SAFETY: `msg` points into `recv`, which is still alive.
    assert_eq!(unsafe { msg.bufs.as_ref() }.len, 0);
    let OpResult::Done((Ok(0), bufs)) =
      OpModel::complete(&mut recv, Completion::new(0))
    else {
      panic!("an empty recv must complete with zero bytes");
    };
    assert!(bufs.is_empty());
  }
}
