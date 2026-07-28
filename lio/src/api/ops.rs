#![allow(clippy::unnecessary_lazy_evaluations)]

//! I/O operation definitions.
//!
//! This module contains all the typed operation structs that implement `TypedOp`.

use std::ffi::{CString, OsString};
use std::io;
use std::mem;
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::fd::{FromRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::RawHandle;
use std::ptr::{self, NonNull};
use std::time::Duration;

use crate::{
  BufResult, IoBufMutVec, IoBufVec,
  api::{
    Pid,
    op::{
      Action, Completion, CompletionFlags, OneshotOpModel, OpModel, OpResult,
      StreamOpModel,
    },
    resource::Resource,
  },
  backend::op::{
    FileMode, FileStat, MsgBuf, MsgBufMut, MsgRecv, MsgSend, Op, OpenFlags,
    RawBuf, ReadDirBuf, ReadFlags, RecvFlags, SendFlags, ShutdownHow,
    SockDomain, SockProto, SockType, SocketAddrBuf, StatTarget, UnlinkKind,
    WriteFlags, socket_addr_from_buf, socket_addr_into_buf,
  },
  buf::MAX_IOV_COUNT,
};
#[cfg(test)]
use lio_test::{ContractKind, ContractStep, OpModelContract};

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
type TwoVecBufResult = BufResult<i32, (Vec<u8>, Vec<u8>)>;
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
    if completion.result < 0 {
      return OpResult::Done(Err(io::Error::from_raw_os_error(
        (-completion.result) as i32,
      )));
    }

    #[cfg(unix)]
    {
      // SAFETY: `fd` was returned by `socket(2)` and ownership transfers here.
      OpResult::Done(Ok(unsafe {
        Resource::from_raw_fd(completion.result as RawFd)
      }))
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
    if completion.result < 0 {
      return OpResult::Done(Err(io::Error::from_raw_os_error(
        (-completion.result) as i32,
      )));
    }

    #[cfg(unix)]
    // SAFETY: successful accept returns a live file descriptor owned by us.
    let resource = unsafe { Resource::from_raw_fd(completion.result as RawFd) };

    let addr = match socket_addr_from_buf(&self.addr) {
      Ok(addr) => addr,
      Err(err) => return OpResult::Done(Err(err)),
    };

    OpResult::Done(Ok((resource, addr)))
  }
}

impl OneshotOpModel for Accept {}

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
      Err(std::io::Error::from_raw_os_error((-completion.result) as i32))
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
  addr: SocketAddr,
}

impl Bind {
  pub(crate) fn new(res: Resource, addr: SocketAddr) -> Self {
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
      Err(io::Error::from_raw_os_error((-completion.result) as i32))
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
      Err(io::Error::from_raw_os_error((-completion.result) as i32))
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
      Err(io::Error::from_raw_os_error((-completion.result) as i32))
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
      Err(io::Error::from_raw_os_error((-completion.result) as i32))
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
    assert_eq!(completion.result, 0);
    OpResult::Done(Ok(()))
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

// ============================================================================
// Read
// ============================================================================

pub struct Read<B: IoBufMutVec> {
  res: Resource,
  buf: Option<B>,
  raws: [RawBuf; MAX_IOV_COUNT],
  offset: i64,
}

impl<B: IoBufMutVec> Read<B> {
  pub(crate) fn new(res: Resource, buf: B, offset: i64) -> Self {
    Self {
      res,
      buf: Some(buf),
      // SAFETY: `RawBuf` is a plain pointer/length pair and is immediately
      // overwritten before submission.
      raws: unsafe { mem::zeroed() },
      offset,
    }
  }

  #[cfg(test)]
  fn stage_read_data(&mut self, bytes: &[u8]) {
    let buf = self.buf.as_mut().expect("buffer not available");
    let buf_count = buf.buf_count().min(MAX_IOV_COUNT);
    let mut copied = 0usize;

    for i in 0..buf_count {
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

impl<B: IoBufMutVec> OpModel for Read<B> {
  type Item = BufResult<i32, B>;

  fn action(&mut self) -> Action {
    let buf = self.buf.as_mut().expect("buffer not available");
    let buf_count = buf.buf_count().min(MAX_IOV_COUNT);

    for i in 0..buf_count {
      let (ptr, len) = buf.buf_mut(i);
      // SAFETY: the pointer/capacity pair refers to model-owned mutable storage
      // that remains valid until `complete()` consumes the buffers.
      self.raws[i] = unsafe { RawBuf::from_raw_parts(ptr, len) };
    }

    Action::Io(Op::Read {
      fd: self.res.clone(),
      iovecs: NonNull::from(&mut self.raws[0]),
      iov_count: buf_count,
      offset: self.offset,
      flags: ReadFlags::EMPTY,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let mut buf = self.buf.take().expect("buffer not available");
    let buf_count = buf.buf_count().min(MAX_IOV_COUNT);

    let result = if completion.result < 0 {
      Err(io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      let mut remaining = completion.result as usize;
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

impl<B: IoBufMutVec> OneshotOpModel for Read<B> {}

#[cfg(test)]
impl OpModelContract for Read<Vec<u8>> {
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), vec![0u8; 8], -1)
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| {
        matches!(
          action,
          Action::Io(Op::Read {
            iov_count: 1,
            offset: -1,
            flags: ReadFlags::EMPTY,
            ..
          })
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
    Self::new(Resource::stdin(), (vec![0u8; 3], vec![0u8; 5]), -1)
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| {
        matches!(
          action,
          Action::Io(Op::Read {
            iov_count: 2,
            offset: -1,
            flags: ReadFlags::EMPTY,
            ..
          })
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
  offset: i64,
}

impl<B: IoBufVec + std::marker::Send + Sync> Write<B> {
  pub(crate) fn new(res: Resource, buf: B, offset: i64) -> Self {
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
  type Item = BufResult<i32, B>;

  fn action(&mut self) -> Action {
    let buf = self.buf.as_ref().expect("buffer not available");
    let buf_count = buf.buf_count().min(MAX_IOV_COUNT);

    for i in 0..buf_count {
      let (ptr, len) = buf.buf(i);
      // SAFETY: `IoBufVec` guarantees each segment is valid for `len`
      // initialized bytes until completion.
      self.raws[i] = unsafe { RawBuf::from_raw_parts(ptr.cast_mut(), len) };
    }

    Action::Io(Op::Write {
      fd: self.res.clone(),
      iovecs: NonNull::from(&mut self.raws[0]),
      iov_count: buf_count,
      offset: self.offset,
      flags: WriteFlags::EMPTY,
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
    Self::new(Resource::stdin(), b"pong".to_vec(), -1)
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| {
        matches!(
          action,
          Action::Io(Op::Write {
            iov_count: 1,
            offset: -1,
            flags: WriteFlags::EMPTY,
            ..
          })
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
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(Resource::stdin(), (b"ab".to_vec(), b"cde".to_vec()), -1)
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| {
        matches!(
          action,
          Action::Io(Op::Write {
            iov_count: 2,
            offset: -1,
            flags: WriteFlags::EMPTY,
            ..
          })
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
      flags: flags.unwrap_or(RecvFlags::EMPTY),
      from: with_from.then(SocketAddrBuf::unspecified),
    }
  }

  fn action(&mut self) -> Action {
    let buf = self.buf.as_mut().expect("buffer not available");
    let buf_count = buf.buf_count().min(MAX_IOV_COUNT);

    for i in 0..buf_count {
      let (ptr, len) = buf.buf_mut(i);
      // SAFETY: `buf_mut(i)` returns a valid writable region for this buffer slot.
      self.bufs[i] = unsafe {
        MsgBufMut::from_raw_parts(
          NonNull::new(ptr).expect("IoBufMutVec returned null ptr"),
          len,
        )
      };
    }

    let msg = match self.from.as_mut() {
      Some(addr) => {
        MsgRecv::with_from(&self.bufs[..buf_count], NonNull::from(addr))
      }
      None => MsgRecv::new(&self.bufs[..buf_count]),
    };

    Action::Io(Op::Recv { fd: self.res.clone(), msg, flags: self.flags })
  }

  fn complete_buf(
    &mut self,
    completion: Completion,
  ) -> (std::io::Result<i32>, B) {
    let mut buf = self.buf.take().expect("buffer not available");
    let buf_count = buf.buf_count().min(MAX_IOV_COUNT);

    let result = if completion.result < 0 {
      Err(io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      let mut remaining = completion.result as usize;
      for i in 0..buf_count {
        let (_, cap) = buf.buf_mut(i);
        let len = remaining.min(cap);
        buf.set_buf_len(i, len);
        remaining = remaining.saturating_sub(cap);
      }
      Ok(completion.result as i32)
    };

    (result, buf)
  }

  #[cfg(test)]
  fn stage_recv_data(&mut self, bytes: &[u8]) {
    let buf = self.buf.as_mut().expect("buffer not available");
    let buf_count = buf.buf_count().min(MAX_IOV_COUNT);
    let mut copied = 0usize;

    for i in 0..buf_count {
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
  type Item = BufResult<i32, B>;

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
  type Item = (std::io::Result<i32>, B, Option<SocketAddr>);

  fn action(&mut self) -> Action {
    self.core.action()
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let (result, buf) = self.core.complete_buf(completion);
    let addr = result
      .as_ref()
      .ok()
      .and_then(|_| self.core.from.as_ref())
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
    Self::new(
      Resource::stdin(),
      vec![0u8; 8],
      Some(RecvFlags::from_bits(libc::MSG_DONTWAIT).unwrap()),
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| match action {
        Action::Io(Op::Recv { flags, .. }) => {
          flags.bits() == libc::MSG_DONTWAIT
        }
        _ => false,
      },
      |model| model.core.stage_recv_data(b"recv"),
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
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(
      Resource::stdin(),
      (vec![0u8; 2], vec![0u8; 4]),
      Some(RecvFlags::from_bits(libc::MSG_DONTWAIT).unwrap()),
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| match action {
        Action::Io(Op::Recv { flags, .. }) => {
          flags.bits() == libc::MSG_DONTWAIT
        }
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
    Self::new(
      Resource::stdin(),
      vec![0u8; 8],
      Some(RecvFlags::from_bits(libc::MSG_DONTWAIT).unwrap()),
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::with_setup(
      |action| match action {
        Action::Io(Op::Recv { flags, .. }) => {
          flags.bits() == libc::MSG_DONTWAIT
        }
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
      flags: flags.unwrap_or(SendFlags::EMPTY),
      addr,
    }
  }

  fn hydrate_msg(&mut self) -> MsgSend {
    let buf = self.buf.as_ref().expect("buffer not available");
    let buf_count = buf.buf_count().min(MAX_IOV_COUNT);

    for i in 0..buf_count {
      let (ptr, len) = buf.buf(i);
      // SAFETY: `buf(i)` returns a valid readable region for this buffer slot.
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

impl<B: IoBufVec + std::marker::Send + Sync + 'static> OpModel for Send<B> {
  type Item = BufResult<i32, B>;

  fn action(&mut self) -> Action {
    let msg = self.hydrate_msg();
    Action::Io(Op::Send { fd: self.res.clone(), msg, flags: self.flags })
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
    Self::new(
      Resource::stdin(),
      b"send".to_vec(),
      None,
      Some(SendFlags::from_bits(libc::MSG_NOSIGNAL).unwrap()),
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| match action {
        Action::Io(Op::Send { flags, .. }) => {
          flags.bits() == libc::MSG_NOSIGNAL
        }
        _ => false,
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
  impl_op_model_contract_runtime!();
  fn contract_kind() -> ContractKind {
    ContractKind::Oneshot
  }

  fn contract_model() -> Self {
    Self::new(
      Resource::stdin(),
      (b"he".to_vec(), b"llo".to_vec()),
      None,
      Some(SendFlags::from_bits(libc::MSG_NOSIGNAL).unwrap()),
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| match action {
        Action::Io(Op::Send { flags, .. }) => {
          flags.bits() == libc::MSG_NOSIGNAL
        }
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
    debug_assert!(completion.flags.contains(CompletionFlags::TIMER));

    let result = if completion.result == 0 {
      Ok(())
    } else {
      Err(io::Error::from_raw_os_error(completion.result.abs() as i32))
    };

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
    debug_assert!(completion.flags.contains(CompletionFlags::TIMER));

    let result = if completion.result == 0 {
      Ok(())
    } else {
      Err(io::Error::from_raw_os_error(completion.result.abs() as i32))
    };

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
    flags: impl Into<OpenFlags>,
    mode: impl Into<FileMode>,
  ) -> Self {
    Self { dir_res, pathname, flags: flags.into(), mode: mode.into() }
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

pub struct Stat {
  target: StatModelTarget,
  out: FileStat,
}

enum StatModelTarget {
  Path { dir_res: Resource, pathname: CString, follow_symlinks: bool },
  Fd { fd: Resource },
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
    };
    Action::Io(Op::Stat { target, out: NonNull::from(&mut self.out) })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let res = if completion.result < 0 {
      Err(std::io::Error::from_raw_os_error((-completion.result) as i32))
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
      raw_buf: NonNull::new(self.buf.raw.as_mut_ptr()).expect("raw buffer"),
      raw_cap: self.buf.raw.len(),
      entries: NonNull::new(self.buf.entries.as_mut_ptr())
        .expect("entry buffer"),
      entries_cap: self.buf.entries.len(),
      opaque: NonNull::from(&mut self.buf.opaque),
      opaque_drop: NonNull::from(&mut self.buf.opaque_drop),
      out: NonNull::from(&mut self.buf.result),
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let res = if completion.result < 0 {
      Err(std::io::Error::from_raw_os_error((-completion.result) as i32))
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
        #[allow(clippy::unnecessary_cast)]
        let mode = (libc::S_IFREG as u32) | 0o644;
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
    kind: impl Into<UnlinkKind>,
  ) -> Self {
    Self { dir_res, pathname, kind: kind.into() }
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
    let res = if completion.result < 0 {
      Err(std::io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      Ok(())
    };
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
    let res = if completion.result < 0 {
      Err(std::io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      Ok(())
    };
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
    mode: impl Into<FileMode>,
  ) -> Self {
    Self { dir_res, pathname, mode: mode.into() }
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
    let res = if completion.result < 0 {
      Err(std::io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      Ok(())
    };
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
      FileMode::from_bits(0o755),
    )
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![ContractStep::new(
      |action| matches!(action, Action::Io(Op::MkdirAt { mode, .. }) if mode.bits() == 0o755),
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
    let res = if completion.result < 0 {
      Err(std::io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      Ok(())
    };
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
  type Item = BufResult<i32, B>;

  fn action(&mut self) -> Action {
    let buf = self.buf.as_mut().expect("buffer not available");
    let (ptr, len) = buf.buf_mut(0);
    // SAFETY: `buf_mut(0)` returns a valid writable region for the op lifetime.
    self.raw = unsafe { RawBuf::from_raw_parts(ptr, len) };
    self.raw_len = len;

    Action::Io(Op::ReadlinkAt {
      dir_fd: self.dir_res.clone(),
      path: cstring_to_osstring(&self.pathname),
      buf: NonNull::new(ptr).expect("readlink buffer pointer must be non-null"),
      buf_len: self.raw_len,
    })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let mut buf = self.buf.take().expect("buffer not available");
    let result = if completion.result < 0 {
      Err(std::io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      buf.set_buf_len(0, completion.result as usize);
      Ok(completion.result as i32)
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
  type Item = BufResult<i32, B>;

  fn action(&mut self) -> Action {
    Action::Io(Op::GetCwd { out: NonNull::from(&mut self.out) })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let mut buf = self.buf.take().expect("buffer not available");
    let result = if completion.result < 0 {
      Err(std::io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      #[cfg(unix)]
      let bytes = {
        use std::os::unix::ffi::OsStrExt;
        self.out.as_os_str().as_bytes()
      };
      #[cfg(not(unix))]
      let bytes = self.out.to_string_lossy().as_bytes();

      let (ptr, cap) = buf.buf_mut(0);
      if bytes.len() > cap {
        Err(std::io::Error::from_raw_os_error(libc::ERANGE))
      } else {
        // SAFETY: `ptr` points to writable storage of at least `cap` bytes.
        unsafe {
          std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len())
        };
        buf.set_buf_len(0, bytes.len());
        Ok(bytes.len() as i32)
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

#[cfg(unix)]
pub struct Spawn {
  spec: crate::backend::op::SpawnSpec,
}

#[cfg(unix)]
impl Spawn {
  pub(crate) fn new(
    path: CString,
    argv: Vec<CString>,
    envp: Option<Vec<CString>>,
  ) -> Self {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    Self {
      spec: crate::backend::op::SpawnSpec {
        program: OsString::from_vec(path.into_bytes()),
        args: argv
          .into_iter()
          .map(|arg| OsString::from_vec(arg.into_bytes()))
          .collect(),
        env: envp.map(|vars| {
          vars
            .into_iter()
            .map(|var| OsString::from_vec(var.into_bytes()))
            .collect()
        }),
      },
    }
  }
}

#[cfg(unix)]
impl OpModel for Spawn {
  type Item = std::io::Result<Pid>;

  fn action(&mut self) -> Action {
    Action::Io(Op::Spawn { spec: self.spec.clone() })
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Item> {
    let result = if completion.result < 0 {
      Err(std::io::Error::from_raw_os_error((-completion.result) as i32))
    } else {
      Ok(Pid::from_raw(completion.result as i64))
    };
    OpResult::Done(result)
  }
}

#[cfg(unix)]
impl OneshotOpModel for Spawn {}

#[cfg(all(test, unix))]
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
      |result: &OpResult<std::io::Result<Pid>>| matches!(result, OpResult::Done(Ok(pid)) if pid.as_raw() == 1234),
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

  #[cfg(unix)]
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
}
