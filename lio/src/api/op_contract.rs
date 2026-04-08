//! Self-contained illustration of a serial `OpModel` contract.
//!
//! This module is intentionally separate from the in-flight runtime refactor.
//! It demonstrates a minimal trait shape where implementors only define:
//! - which low-level I/O step to perform next
//! - how to interpret the completion of that step
//!
//! The examples here are meant to make the state management concrete.

use std::{collections::VecDeque, io, net::SocketAddr};

/// Illustration-only low-level op enum for the proposed serial contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
  Socket { domain: i32, ty: i32, proto: i32 },
  Bind { fd: i32, addr: SocketAddr },
  Listen { fd: i32, backlog: i32 },
  Read { fd: i32, len: usize },
}

/// Completion metadata produced by the backend for a single in-flight op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
  pub result: isize,
  pub flags: CompletionFlags,
}

impl Completion {
  pub const fn ok(result: isize) -> Self {
    Self { result, flags: CompletionFlags::empty() }
  }

  pub const fn err(errno: i32) -> Self {
    Self::ok(-(errno as isize))
  }
}

/// Minimal completion flags for the illustration contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionFlags(u32);

impl CompletionFlags {
  pub const MORE: Self = Self(1 << 0);

  pub const fn empty() -> Self {
    Self(0)
  }

  pub const fn contains(self, other: Self) -> bool {
    (self.0 & other.0) == other.0
  }
}

/// Result of interpreting one completion.
pub enum OpResult<T> {
  Again,
  Yield(T),
  Done(T),
}

/// Serial logical I/O model.
///
/// Runtime shape:
/// 1. call `op()`
/// 2. submit it
/// 3. wait for a completion
/// 4. call `complete(completion)`
/// 5. if `Again` or `Yield(_)`, repeat from step 1
/// 6. if `Done(_)`, retire the model
pub trait OpModel: Send + 'static {
  type Output: Send + 'static;

  fn op(&mut self) -> Op;
  fn complete(&mut self, completion: Completion) -> OpResult<Self::Output>;
}

/// One scripted contract step for a serial `OpModel`.
pub struct ContractStep<M: OpModel> {
  pub assert_op: fn(&Op) -> bool,
  pub before_complete: fn(&mut M),
  pub completion: Completion,
  pub assert_result: fn(&OpResult<M::Output>) -> bool,
}

impl<M: OpModel> ContractStep<M> {
  pub fn new(
    assert_op: fn(&Op) -> bool,
    completion: Completion,
    assert_result: fn(&OpResult<M::Output>) -> bool,
  ) -> Self {
    Self { assert_op, before_complete: |_| {}, completion, assert_result }
  }

  pub fn with_setup(
    assert_op: fn(&Op) -> bool,
    before_complete: fn(&mut M),
    completion: Completion,
    assert_result: fn(&OpResult<M::Output>) -> bool,
  ) -> Self {
    Self { assert_op, before_complete, completion, assert_result }
  }
}

/// Test harness implemented by the model type itself.
pub trait OpModelContract: OpModel + Sized {
  fn contract_model() -> Self;
  fn contract_steps() -> Vec<ContractStep<Self>>;
}

/// Contract test helper for serial `OpModel` implementations.
///
/// The model type under test provides its own fixture by implementing
/// [`OpModelContract`]. The macro only needs the type.
#[macro_export]
macro_rules! test_serial_op_model_contract {
  ($model_ty:ty) => {
    mod op_model_contract {
      use super::*;

      #[test]
      fn scripted_contract() {
        let mut model =
          <$model_ty as $crate::api::op_contract::OpModelContract>::contract_model();
        let steps =
          <$model_ty as $crate::api::op_contract::OpModelContract>::contract_steps();
        assert!(!steps.is_empty(), "contract script must not be empty");

        for step in steps {
          let op = model.op();
          assert!(
            (step.assert_op)(&op),
            "op() did not satisfy the model contract: {:?}",
            op
          );
          (step.before_complete)(&mut model);
          let result = model.complete(step.completion);
          assert!(
            (step.assert_result)(&result),
            "complete() did not satisfy the model contract"
          );
        }
      }
    }
  };
}

fn completion_to_io_error(completion: Completion) -> io::Result<isize> {
  if completion.result < 0 {
    Err(io::Error::from_raw_os_error((-completion.result) as i32))
  } else {
    Ok(completion.result)
  }
}

/// Illustration of `TcpListener::bind` as a serial `OpModel`.
pub struct TcpBindModel {
  addr: SocketAddr,
  backlog: i32,
  state: TcpBindState,
}

#[derive(Debug, Clone, Copy)]
enum TcpBindState {
  Socket,
  Bind { fd: i32 },
  Listen { fd: i32 },
  Done,
}

impl TcpBindModel {
  pub fn new(addr: SocketAddr, backlog: i32) -> Self {
    Self { addr, backlog, state: TcpBindState::Socket }
  }
}

impl OpModel for TcpBindModel {
  type Output = io::Result<TcpListenerBound>;

  fn op(&mut self) -> Op {
    match self.state {
      TcpBindState::Socket => Op::Socket {
        domain: if self.addr.is_ipv4() {
          libc::AF_INET
        } else {
          libc::AF_INET6
        },
        ty: libc::SOCK_STREAM,
        proto: libc::IPPROTO_TCP,
      },
      TcpBindState::Bind { fd } => Op::Bind { fd, addr: self.addr },
      TcpBindState::Listen { fd } => Op::Listen { fd, backlog: self.backlog },
      TcpBindState::Done => panic!("TcpBindModel polled after completion"),
    }
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Output> {
    match self.state {
      TcpBindState::Socket => match completion_to_io_error(completion) {
        Ok(fd) => {
          self.state = TcpBindState::Bind { fd: fd as i32 };
          OpResult::Again
        }
        Err(err) => {
          self.state = TcpBindState::Done;
          OpResult::Done(Err(err))
        }
      },
      TcpBindState::Bind { fd } => match completion_to_io_error(completion) {
        Ok(_) => {
          self.state = TcpBindState::Listen { fd };
          OpResult::Again
        }
        Err(err) => {
          self.state = TcpBindState::Done;
          OpResult::Done(Err(err))
        }
      },
      TcpBindState::Listen { fd } => match completion_to_io_error(completion) {
        Ok(_) => {
          self.state = TcpBindState::Done;
          OpResult::Done(Ok(TcpListenerBound { fd, addr: self.addr }))
        }
        Err(err) => {
          self.state = TcpBindState::Done;
          OpResult::Done(Err(err))
        }
      },
      TcpBindState::Done => {
        panic!("TcpBindModel received completion after finish")
      }
    }
  }
}

impl OpModelContract for TcpBindModel {
  fn contract_model() -> Self {
    Self::new("127.0.0.1:8080".parse().unwrap(), 128)
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![
      ContractStep::new(
        |op| {
          matches!(
            op,
            Op::Socket {
              domain: libc::AF_INET,
              ty: libc::SOCK_STREAM,
              proto: libc::IPPROTO_TCP,
            }
          )
        },
        Completion::ok(7),
        |result| matches!(result, OpResult::Again),
      ),
      ContractStep::new(
        |op| matches!(op, Op::Bind { fd: 7, addr: _ }),
        Completion::ok(0),
        |result| matches!(result, OpResult::Again),
      ),
      ContractStep::new(
        |op| matches!(op, Op::Listen { fd: 7, backlog: 128 }),
        Completion::ok(0),
        |result| {
          matches!(result, OpResult::Done(Ok(TcpListenerBound { fd: 7, .. })))
        },
      ),
    ]
  }
}

/// Tiny stand-in for a bound listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpListenerBound {
  pub fd: i32,
  pub addr: SocketAddr,
}

/// Illustration of `read_to_string()`.
///
/// The model owns the read buffer. Tests use `push_read_data()` to mimic the
/// backend having filled that buffer before the corresponding completion.
pub struct ReadToStringModel {
  fd: i32,
  scratch: Vec<u8>,
  assembled: Vec<u8>,
  pending_reads: VecDeque<Vec<u8>>,
  done: bool,
}

impl ReadToStringModel {
  pub fn new(fd: i32, chunk_size: usize) -> Self {
    Self {
      fd,
      scratch: vec![0; chunk_size],
      assembled: Vec::new(),
      pending_reads: VecDeque::new(),
      done: false,
    }
  }

  /// Test helper to simulate backend-written bytes into the model-owned buffer.
  pub fn push_read_data(&mut self, chunk: &[u8]) {
    self.pending_reads.push_back(chunk.to_vec());
  }
}

impl OpModel for ReadToStringModel {
  type Output = io::Result<String>;

  fn op(&mut self) -> Op {
    if self.done {
      panic!("ReadToStringModel polled after completion");
    }
    Op::Read { fd: self.fd, len: self.scratch.len() }
  }

  fn complete(&mut self, completion: Completion) -> OpResult<Self::Output> {
    let result = match completion_to_io_error(completion) {
      Ok(result) => result as usize,
      Err(err) => {
        self.done = true;
        return OpResult::Done(Err(err));
      }
    };

    if result == 0 {
      self.done = true;
      return OpResult::Done(
        String::from_utf8(std::mem::take(&mut self.assembled))
          .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
      );
    }

    let next = self
      .pending_reads
      .pop_front()
      .expect("missing simulated backend data for read completion");
    assert!(
      next.len() >= result,
      "simulated backend data shorter than completion result"
    );
    self.scratch[..result].copy_from_slice(&next[..result]);
    self.assembled.extend_from_slice(&self.scratch[..result]);
    OpResult::Again
  }
}

impl OpModelContract for ReadToStringModel {
  fn contract_model() -> Self {
    Self::new(11, 8)
  }

  fn contract_steps() -> Vec<ContractStep<Self>> {
    vec![
      ContractStep::with_setup(
        |op| matches!(op, Op::Read { fd: 11, len: 8 }),
        |model| model.push_read_data(b"hello "),
        Completion::ok(6),
        |result| matches!(result, OpResult::Again),
      ),
      ContractStep::with_setup(
        |op| matches!(op, Op::Read { fd: 11, len: 8 }),
        |model| model.push_read_data(b"world"),
        Completion::ok(5),
        |result| matches!(result, OpResult::Again),
      ),
      ContractStep::new(
        |op| matches!(op, Op::Read { fd: 11, len: 8 }),
        Completion::ok(0),
        |result| matches!(result, OpResult::Done(Ok(_))),
      ),
    ]
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  mod tcp_bind_contract {
    use super::*;

    crate::test_serial_op_model_contract!(TcpBindModel);
  }

  mod read_to_string_contract {
    use super::*;

    crate::test_serial_op_model_contract!(ReadToStringModel);
  }

  #[test]
  fn tcp_bind_error_path_finishes_immediately() {
    let mut model = TcpBindModel::new("127.0.0.1:8080".parse().unwrap(), 128);
    assert!(matches!(
      model.op(),
      Op::Socket {
        domain: libc::AF_INET,
        ty: libc::SOCK_STREAM,
        proto: libc::IPPROTO_TCP,
      }
    ));
    assert!(matches!(
      model.complete(Completion::err(libc::EACCES)),
      OpResult::Done(Err(_))
    ));
  }

  #[test]
  fn read_to_string_assembles_utf8() {
    let mut model = ReadToStringModel::new(11, 8);

    assert!(matches!(model.op(), Op::Read { fd: 11, len: 8 }));
    model.push_read_data(b"hello ");
    assert!(matches!(model.complete(Completion::ok(6)), OpResult::Again));

    assert!(matches!(model.op(), Op::Read { fd: 11, len: 8 }));
    model.push_read_data(b"world");
    assert!(matches!(model.complete(Completion::ok(5)), OpResult::Again));

    match model.complete(Completion::ok(0)) {
      OpResult::Done(Ok(value)) => assert_eq!(value, "hello world"),
      _ => panic!("expected final UTF-8 string"),
    }
  }
}
