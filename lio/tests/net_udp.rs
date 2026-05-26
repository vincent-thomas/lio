#![cfg(feature = "high")]

use std::{cell::RefCell, net::SocketAddr, rc::Rc};

use lio::{
  Lio,
  api::Receiver,
  backend::ds::{DSConfig, DSNetworkFaults, DST},
  net::UdpSocket,
};

struct ReceiverNode {
  socket_rx: Option<Receiver<std::io::Result<UdpSocket>>>,
  socket: Option<UdpSocket>,
  recv_rx: Option<Receiver<(std::io::Result<i32>, Vec<u8>)>>,
  recv_done: Rc<RefCell<Option<Vec<u8>>>>,
}

impl ReceiverNode {
  fn new(recv_done: Rc<RefCell<Option<Vec<u8>>>>) -> Self {
    Self { socket_rx: None, socket: None, recv_rx: None, recv_done }
  }

  fn drive(&mut self, bind_addr: SocketAddr) {
    if self.socket_rx.is_none() && self.socket.is_none() {
      self.socket_rx = Some(UdpSocket::bind(bind_addr).send());
    }

    if self.socket.is_none()
      && let Some(result) = self.socket_rx.as_mut().and_then(Receiver::try_recv)
    {
      self.socket = Some(result.unwrap());
      self.socket_rx = None;
    }

    if self.recv_rx.is_none()
      && self.recv_done.borrow().is_none()
      && let Some(socket) = self.socket.as_ref()
    {
      self.recv_rx = Some(socket.recv(vec![0u8; 64]).send());
    }

    if let Some((result, buf)) =
      self.recv_rx.as_mut().and_then(Receiver::try_recv)
    {
      let len = result.unwrap() as usize;
      *self.recv_done.borrow_mut() = Some(buf[..len].to_vec());
      self.recv_rx = None;
    }
  }
}

struct DatagramReceiverNode {
  socket_rx: Option<Receiver<std::io::Result<UdpSocket>>>,
  socket: Option<UdpSocket>,
  recv_rx: Option<Receiver<(std::io::Result<i32>, Vec<u8>, Option<SocketAddr>)>>,
  recv_done: Rc<RefCell<Option<(Vec<u8>, Option<SocketAddr>)>>>,
}

impl DatagramReceiverNode {
  fn new(
    recv_done: Rc<RefCell<Option<(Vec<u8>, Option<SocketAddr>)>>>,
  ) -> Self {
    Self { socket_rx: None, socket: None, recv_rx: None, recv_done }
  }

  fn drive(&mut self, bind_addr: SocketAddr) {
    if self.socket_rx.is_none() && self.socket.is_none() {
      self.socket_rx = Some(UdpSocket::bind(bind_addr).send());
    }

    if self.socket.is_none()
      && let Some(result) = self.socket_rx.as_mut().and_then(Receiver::try_recv)
    {
      self.socket = Some(result.unwrap());
      self.socket_rx = None;
    }

    if self.recv_rx.is_none()
      && self.recv_done.borrow().is_none()
      && let Some(socket) = self.socket.as_ref()
    {
      self.recv_rx = Some(socket.recvfrom(vec![0u8; 64]).send());
    }

    if let Some((result, buf, peer_addr)) =
      self.recv_rx.as_mut().and_then(Receiver::try_recv)
    {
      let len = result.unwrap() as usize;
      *self.recv_done.borrow_mut() = Some((buf[..len].to_vec(), peer_addr));
      self.recv_rx = None;
    }
  }
}

struct SenderNode {
  socket_rx: Option<Receiver<std::io::Result<UdpSocket>>>,
  socket: Option<UdpSocket>,
  send_rx: Option<Receiver<(std::io::Result<i32>, Vec<u8>)>>,
  send_done: Rc<RefCell<bool>>,
}

impl SenderNode {
  fn new(send_done: Rc<RefCell<bool>>) -> Self {
    Self { socket_rx: None, socket: None, send_rx: None, send_done }
  }

  fn drive(&mut self, peer_addr: SocketAddr) {
    if self.socket_rx.is_none() && self.socket.is_none() {
      self.socket_rx = Some(UdpSocket::connect(peer_addr).send());
    }

    if self.socket.is_none()
      && let Some(result) = self.socket_rx.as_mut().and_then(Receiver::try_recv)
    {
      self.socket = Some(result.unwrap());
      self.socket_rx = None;
    }

    if self.send_rx.is_none()
      && !*self.send_done.borrow()
      && let Some(socket) = self.socket.as_ref()
    {
      self.send_rx = Some(socket.send(b"ping".to_vec()).send());
    }

    if let Some((result, buf)) =
      self.send_rx.as_mut().and_then(Receiver::try_recv)
    {
      assert_eq!(result.unwrap(), buf.len() as i32);
      *self.send_done.borrow_mut() = true;
      self.send_rx = None;
    }
  }
}

struct DatagramSenderNode {
  socket_rx: Option<Receiver<std::io::Result<UdpSocket>>>,
  socket: Option<UdpSocket>,
  send_rx: Option<Receiver<(std::io::Result<i32>, Vec<u8>)>>,
  send_done: Rc<RefCell<bool>>,
}

impl DatagramSenderNode {
  fn new(send_done: Rc<RefCell<bool>>) -> Self {
    Self { socket_rx: None, socket: None, send_rx: None, send_done }
  }

  fn drive(&mut self, bind_addr: SocketAddr, peer_addr: SocketAddr) {
    if self.socket_rx.is_none() && self.socket.is_none() {
      self.socket_rx = Some(UdpSocket::bind(bind_addr).send());
    }

    if self.socket.is_none()
      && let Some(result) = self.socket_rx.as_mut().and_then(Receiver::try_recv)
    {
      self.socket = Some(result.unwrap());
      self.socket_rx = None;
    }

    if self.send_rx.is_none()
      && !*self.send_done.borrow()
      && let Some(socket) = self.socket.as_ref()
    {
      self.send_rx = Some(socket.sendto(b"ping".to_vec(), peer_addr).send());
    }

    if let Some((result, buf)) =
      self.send_rx.as_mut().and_then(Receiver::try_recv)
    {
      assert_eq!(result.unwrap(), buf.len() as i32);
      *self.send_done.borrow_mut() = true;
      self.send_rx = None;
    }
  }
}

#[test]
fn udp_socket_bind_connect_send_recv_over_dst() {
  let bind_addr: SocketAddr = "127.0.0.1:7000".parse().unwrap();
  let recv_done = Rc::new(RefCell::new(None::<Vec<u8>>));
  let send_done = Rc::new(RefCell::new(false));

  let mut dst = DST::with_config(DSConfig {
    seed: 7,
    max_delay_ticks: 1,
    fault_every: 0,
    network_faults: DSNetworkFaults::Off,
  });

  dst
    .add_node(64, {
      let recv_done = recv_done.clone();
      let mut receiver = ReceiverNode::new(recv_done);
      move |lio: Lio| {
        loop {
          let before = receiver.recv_done.borrow().is_some();
          receiver.drive(bind_addr);
          let progressed = lio.try_run()? > 0;
          if before == receiver.recv_done.borrow().is_some() && !progressed {
            break;
          }
        }
        Ok(())
      }
    })
    .unwrap();

  for _ in 0..16 {
    if recv_done.borrow().is_some() {
      break;
    }
    dst.tick().unwrap();
  }

  dst
    .add_node(64, {
      let send_done = send_done.clone();
      let mut sender = SenderNode::new(send_done);
      move |lio: Lio| {
        loop {
          let before = *sender.send_done.borrow();
          sender.drive(bind_addr);
          let progressed = lio.try_run()? > 0;
          if before == *sender.send_done.borrow() && !progressed {
            break;
          }
        }
        Ok(())
      }
    })
    .unwrap();

  for _ in 0..32 {
    if *send_done.borrow() && recv_done.borrow().is_some() {
      break;
    }
    dst.tick().unwrap();
  }

  assert!(*send_done.borrow(), "sender should complete its datagram send");
  assert_eq!(recv_done.borrow().as_deref(), Some(b"ping".as_slice()));
}

#[test]
fn udp_socket_bind_sendto_recvfrom_over_dst() {
  let recv_addr: SocketAddr = "127.0.0.1:7100".parse().unwrap();
  let send_addr: SocketAddr = "127.0.0.1:7101".parse().unwrap();
  let recv_done = Rc::new(RefCell::new(None::<(Vec<u8>, Option<SocketAddr>)>));
  let send_done = Rc::new(RefCell::new(false));

  let mut dst = DST::with_config(DSConfig {
    seed: 13,
    max_delay_ticks: 1,
    fault_every: 0,
    network_faults: DSNetworkFaults::Off,
  });

  dst
    .add_node(64, {
      let recv_done = recv_done.clone();
      let mut receiver = DatagramReceiverNode::new(recv_done);
      move |lio: Lio| {
        loop {
          let before = receiver.recv_done.borrow().is_some();
          receiver.drive(recv_addr);
          let progressed = lio.try_run()? > 0;
          if before == receiver.recv_done.borrow().is_some() && !progressed {
            break;
          }
        }
        Ok(())
      }
    })
    .unwrap();

  for _ in 0..16 {
    if recv_done.borrow().is_some() {
      break;
    }
    dst.tick().unwrap();
  }

  dst
    .add_node(64, {
      let send_done = send_done.clone();
      let mut sender = DatagramSenderNode::new(send_done);
      move |lio: Lio| {
        loop {
          let before = *sender.send_done.borrow();
          sender.drive(send_addr, recv_addr);
          let progressed = lio.try_run()? > 0;
          if before == *sender.send_done.borrow() && !progressed {
            break;
          }
        }
        Ok(())
      }
    })
    .unwrap();

  for _ in 0..32 {
    if *send_done.borrow() && recv_done.borrow().is_some() {
      break;
    }
    dst.tick().unwrap();
  }

  assert!(*send_done.borrow(), "sender should complete its datagram send");
  let received = recv_done.borrow();
  let (payload, peer_addr) = received.as_ref().expect("receiver should finish");
  assert_eq!(payload.as_slice(), b"ping");
  assert_eq!(*peer_addr, Some(send_addr));
}
