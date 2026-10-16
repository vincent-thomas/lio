use super::*;
use crate::api::resource::Resource;
use crate::backend::op::{
  MsgBuf, MsgBufMut, MsgRecv, MsgSend, RecvFlags, SendFlags, SocketAddrBuf,
};
use std::net::UdpSocket;
use std::os::fd::{FromRawFd, IntoRawFd};

fn pair() -> (Resource, UdpSocket) {
  let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
  let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
  receiver.connect(sender.local_addr().unwrap()).unwrap();
  sender.connect(receiver.local_addr().unwrap()).unwrap();
  sender.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
  // SAFETY: ownership of this socket descriptor is transferred exactly once.
  (unsafe { Resource::from_raw_fd(receiver.into_raw_fd()) }, sender)
}

fn complete(backend: &mut IoUring, arena: &mut Bump, op: Op) -> isize {
  backend.push(123, op, arena);
  backend.flush().unwrap();
  let deadline = Instant::now() + Duration::from_secs(2);
  let mut completed = Vec::new();
  loop {
    backend.wait(Some(Duration::from_millis(10)), &mut completed).unwrap();
    if !completed.is_empty() {
      assert_eq!(completed.len(), 1);
      assert_eq!(completed[0].registration_id(), 123);
      let result = completed[0].result();
      backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
      assert!(completed.is_empty(), "completion must be delivered only once");
      return result;
    }
    assert!(Instant::now() < deadline, "socket operation timed out");
  }
}

#[test]
fn lowering_selects_single_buffers_without_arena_allocation() {
  let (fd, _) = pair();
  let arena = Bump::new();
  let mut data = [0; 4];
  let recv = [MsgBufMut::from_slice(&mut data)];
  let op = Op::Recv {
    fd: fd.clone(),
    msg: MsgRecv::new(&recv),
    flags: RecvFlags::from_bits(0).unwrap(),
  };
  assert!(matches!(
    LoweredState::lower_in(&op, &arena, true).unwrap(),
    LoweredState::SingleMsg { len: 4, .. }
  ));
  let send = [MsgBuf::from_slice(&data)];
  let op = Op::Send {
    fd,
    msg: MsgSend::new(&send, None),
    flags: SendFlags::from_bits(0).unwrap(),
  };
  assert!(matches!(
    LoweredState::lower_in(&op, &arena, true).unwrap(),
    LoweredState::SingleMsg { len: 4, .. }
  ));
  assert_eq!(arena.allocated_bytes(), 0);
}

#[test]
fn lowering_preserves_general_path_and_unsupported_kernel_fallback() {
  let (fd, peer) = pair();
  let arena = Bump::new();
  let data = [0; 4];
  let send = [MsgBuf::from_slice(&data), MsgBuf::from_slice(&data)];
  for (bufs, to, supported) in [
    (&send[..], None, true),
    (&send[..1], Some(peer.local_addr().unwrap()), true),
    (&send[..1], None, false),
  ] {
    let op = Op::Send {
      fd: fd.clone(),
      msg: MsgSend::new(bufs, to),
      flags: SendFlags::from_bits(0).unwrap(),
    };
    assert!(matches!(
      LoweredState::lower_in(&op, &arena, supported).unwrap(),
      LoweredState::Msg(_)
    ));
  }
  let mut data = [0; 4];
  let recv = [MsgBufMut::from_slice(&mut data)];
  let mut from = SocketAddrBuf::unspecified();
  let op = Op::Recv {
    fd: fd.clone(),
    msg: MsgRecv::with_from(&recv, NonNull::from(&mut from)),
    flags: RecvFlags::from_bits(0).unwrap(),
  };
  assert!(matches!(
    LoweredState::lower_in(&op, &arena, true).unwrap(),
    LoweredState::Msg(_)
  ));
  let op = Op::Recv {
    fd,
    msg: MsgRecv::new(&recv),
    flags: RecvFlags::from_bits(0).unwrap(),
  };
  assert!(matches!(
    LoweredState::lower_in(&op, &arena, false).unwrap(),
    LoweredState::Msg(_)
  ));
}

#[test]
fn oversized_buffers_are_not_truncated_to_u32() {
  let (fd, _) = pair();
  let arena = Bump::new();
  // Metadata-only check: these descriptors are never submitted or dereferenced
  // for payload access. Lowering only copies their pointer and length fields.
  let Some(len) = (u32::MAX as usize).checked_add(1) else { return };
  let bufs = [MsgBuf { ptr: NonNull::dangling(), len }];
  let op = Op::Send {
    fd: fd.clone(),
    msg: MsgSend::new(&bufs, None),
    flags: SendFlags::from_bits(0).unwrap(),
  };
  assert!(matches!(
    LoweredState::lower_in(&op, &arena, true).unwrap(),
    LoweredState::Msg(_)
  ));
  let bufs = [MsgBufMut { ptr: NonNull::dangling(), len }];
  let op = Op::Recv {
    fd,
    msg: MsgRecv::new(&bufs),
    flags: RecvFlags::from_bits(0).unwrap(),
  };
  assert!(matches!(
    LoweredState::lower_in(&op, &arena, true).unwrap(),
    LoweredState::Msg(_)
  ));
}

#[test]
fn udp_peek_truncation_and_zero_length_match_general_path() {
  for general in [false, true] {
    let mut backend = IoUring::default();
    backend.init(8).unwrap();
    if general {
      backend.single_socket = false;
    }
    let mut arena = Bump::new();
    let (fd, peer) = pair();
    peer.send(b"payload").unwrap();
    for (flags, expected) in [(libc::MSG_PEEK, 3), (libc::MSG_TRUNC, 7)] {
      let mut data = [0; 3];
      let bufs = [MsgBufMut::from_slice(&mut data)];
      let op = Op::Recv {
        fd: fd.clone(),
        msg: MsgRecv::new(&bufs),
        flags: RecvFlags::from_bits(flags).unwrap(),
      };
      assert_eq!(complete(&mut backend, &mut arena, op), expected);
      assert_eq!(&data, b"pay");
    }
    // A zero-length datagram must still be sent and consumed as a message.
    let bufs = [MsgBuf::from_slice(&[])];
    let op = Op::Send {
      fd: fd.clone(),
      msg: MsgSend::new(&bufs, None),
      flags: SendFlags::from_bits(0).unwrap(),
    };
    assert_eq!(complete(&mut backend, &mut arena, op), 0);
    assert_eq!(peer.recv(&mut [0; 1]).unwrap(), 0);
    peer.send(&[]).unwrap();
    let mut empty = [];
    let bufs = [MsgBufMut::from_slice(&mut empty)];
    let op = Op::Recv {
      fd,
      msg: MsgRecv::new(&bufs),
      flags: RecvFlags::from_bits(0).unwrap(),
    };
    assert_eq!(complete(&mut backend, &mut arena, op), 0);
  }
}
