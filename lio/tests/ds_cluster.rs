#![allow(clippy::expect_fun_call)]

use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;

use lio::{
  Lio,
  api::{self, Receiver, SockDomain, SockProto, SockType, resource::Resource},
  backend::ds::{DSConfig, DSNetworkFaults, DST},
};

type MessageList = Rc<RefCell<Vec<Vec<u8>>>>;
type SendReceiver = Receiver<(std::io::Result<i32>, Vec<u8>)>;
type RecvReceiver = Receiver<(std::io::Result<i32>, Vec<u8>)>;

fn trace(line: impl AsRef<str>) {
  println!("{}", line.as_ref());
}

fn take_receiver_result<T>(rx: &mut Option<Receiver<T>>) -> Option<T> {
  let result = rx.as_mut().and_then(|rx| rx.try_recv());
  if result.is_some() {
    *rx = None;
  }
  result
}

fn simulation_done(
  server_received: &MessageList,
  client_received: &MessageList,
  expected_server: usize,
  expected_client: usize,
) -> bool {
  server_received.borrow().len() == expected_server
    && client_received.borrow().len() == expected_client
}

struct MessageExchange {
  role: &'static str,
  sends_submitted: bool,
  send_payloads: &'static [&'static [u8]],
  recv_payloads: &'static [&'static [u8]],
  send_rxs: Vec<SendReceiver>,
  recv_rx: Option<RecvReceiver>,
  received: MessageList,
}

impl MessageExchange {
  fn new(
    role: &'static str,
    send_payloads: &'static [&'static [u8]],
    recv_payloads: &'static [&'static [u8]],
    received: MessageList,
  ) -> Self {
    Self {
      role,
      sends_submitted: false,
      send_payloads,
      recv_payloads,
      send_rxs: Vec::new(),
      recv_rx: None,
      received,
    }
  }

  fn drive(&mut self, socket: &Resource) -> bool {
    let mut progressed = false;

    if !self.sends_submitted {
      for payload in self.send_payloads {
        trace(format!(
          "{}: submit send bytes={}",
          self.role,
          String::from_utf8_lossy(payload)
        ));
        self.send_rxs.push(api::send(socket, payload.to_vec(), None).send());
      }
      self.sends_submitted = true;
      progressed = true;
    }

    if self.recv_rx.is_none()
      && self.received.borrow().len() < self.recv_payloads.len()
    {
      let recv_idx = self.received.borrow().len();
      let recv_len = self.recv_payloads[recv_idx].len();
      trace(format!("{}: submit recv#{recv_idx}", self.role));
      self.recv_rx = Some(api::recv(socket, vec![0_u8; recv_len], None).send());
      progressed = true;
    }

    if let Some(rx) = self.recv_rx.as_mut()
      && let Some((result, buf)) = rx.try_recv()
    {
      let len = result.unwrap() as usize;
      let bytes = buf[..len].to_vec();
      self.received.borrow_mut().push(bytes.clone());
      trace(format!(
        "{}: recv complete bytes={}",
        self.role,
        String::from_utf8_lossy(&bytes)
      ));
      self.recv_rx = None;
      progressed = true;
    }

    let mut send_idx = 0;
    while send_idx < self.send_rxs.len() {
      if let Some((result, buf)) = self.send_rxs[send_idx].try_recv() {
        assert_eq!(result.unwrap(), buf.len() as i32);
        trace(format!(
          "{}: send complete bytes={}",
          self.role,
          String::from_utf8_lossy(&buf)
        ));
        self.send_rxs.remove(send_idx);
        progressed = true;
      } else {
        send_idx += 1;
      }
    }

    progressed
  }
}

struct ServerNode {
  listening: Rc<RefCell<bool>>,
  accepted_addr: Rc<RefCell<Option<SocketAddr>>>,
  listener: Option<Resource>,
  accepted: Option<Resource>,
  socket_rx: Option<Receiver<std::io::Result<Resource>>>,
  bind_rx: Option<Receiver<std::io::Result<()>>>,
  listen_rx: Option<Receiver<std::io::Result<()>>>,
  accept_rx: Option<Receiver<std::io::Result<(Resource, SocketAddr)>>>,
  socket_started: bool,
  bind_started: bool,
  listen_started: bool,
  accept_started: bool,
  exchange: MessageExchange,
}

impl ServerNode {
  fn new(
    listening: Rc<RefCell<bool>>,
    accepted_addr: Rc<RefCell<Option<SocketAddr>>>,
    received: MessageList,
    send_payloads: &'static [&'static [u8]],
    recv_payloads: &'static [&'static [u8]],
  ) -> Self {
    Self {
      listening,
      accepted_addr,
      listener: None,
      accepted: None,
      socket_rx: None,
      bind_rx: None,
      listen_rx: None,
      accept_rx: None,
      socket_started: false,
      bind_started: false,
      listen_started: false,
      accept_started: false,
      exchange: MessageExchange::new(
        "server",
        send_payloads,
        recv_payloads,
        received,
      ),
    }
  }

  fn drive(&mut self, listen_addr: SocketAddr) -> bool {
    let mut progressed = false;

    if self.listener.is_none() && !self.socket_started {
      trace("server: submit socket");
      self.socket_rx = Some(
        api::socket(SockDomain::IPV4, SockType::STREAM, SockProto::TCP).send(),
      );
      self.socket_started = true;
      progressed = true;
    }

    if let Some(result) = take_receiver_result(&mut self.socket_rx) {
      self.listener = Some(result.unwrap());
      trace("server: socket complete");
      progressed = true;
    }

    if let Some(listener_sock) = self.listener.as_ref() {
      if !self.bind_started {
        trace("server: submit bind");
        self.bind_rx = Some(api::bind(listener_sock, listen_addr).send());
        self.bind_started = true;
        progressed = true;
      }

      if let Some(result) = take_receiver_result(&mut self.bind_rx) {
        result.unwrap();
        trace("server: bind complete");
        progressed = true;
      }

      if self.bind_started && self.bind_rx.is_none() && !self.listen_started {
        trace("server: submit listen");
        self.listen_rx = Some(api::listen(listener_sock, 16).send());
        self.listen_started = true;
        progressed = true;
      }

      if let Some(result) = take_receiver_result(&mut self.listen_rx) {
        result.unwrap();
        trace("server: listen complete");
        *self.listening.borrow_mut() = true;
        progressed = true;
      }

      if *self.listening.borrow() && !self.accept_started {
        trace("server: submit accept");
        self.accept_rx = Some(api::accept(listener_sock).send());
        self.accept_started = true;
        progressed = true;
      }
    }

    if let Some(result) = take_receiver_result(&mut self.accept_rx) {
      let (accepted_sock, addr) = result.unwrap();
      self.accepted = Some(accepted_sock);
      *self.accepted_addr.borrow_mut() = Some(addr);
      trace(format!("server: accept complete peer={addr}"));
      progressed = true;
    }

    if let Some(accepted_sock) = self.accepted.as_ref() {
      progressed |= self.exchange.drive(accepted_sock);
    }

    progressed
  }
}

struct ClientNode {
  client: Option<Resource>,
  socket_rx: Option<Receiver<std::io::Result<Resource>>>,
  connect_rx: Option<Receiver<std::io::Result<()>>>,
  socket_started: bool,
  connect_started: bool,
  exchange: MessageExchange,
}

impl ClientNode {
  fn new(
    received: MessageList,
    send_payloads: &'static [&'static [u8]],
    recv_payloads: &'static [&'static [u8]],
  ) -> Self {
    Self {
      client: None,
      socket_rx: None,
      connect_rx: None,
      socket_started: false,
      connect_started: false,
      exchange: MessageExchange::new(
        "client",
        send_payloads,
        recv_payloads,
        received,
      ),
    }
  }

  fn drive(&mut self, listen_addr: SocketAddr) -> bool {
    let mut progressed = false;

    if self.client.is_none() && !self.socket_started {
      trace("client: submit socket");
      self.socket_rx = Some(
        api::socket(SockDomain::IPV4, SockType::STREAM, SockProto::TCP).send(),
      );
      self.socket_started = true;
      progressed = true;
    }

    if let Some(result) = take_receiver_result(&mut self.socket_rx) {
      self.client = Some(result.unwrap());
      trace("client: socket complete");
      progressed = true;
    }

    if let Some(client_sock) = self.client.as_ref() {
      if !self.connect_started {
        trace("client: submit connect");
        self.connect_rx = Some(api::connect(client_sock, listen_addr).send());
        self.connect_started = true;
        progressed = true;
      }

      if let Some(result) = take_receiver_result(&mut self.connect_rx) {
        result.unwrap();
        trace("client: connect complete");
        progressed = true;
      }

      if self.connect_started && self.connect_rx.is_none() {
        progressed |= self.exchange.drive(client_sock);
      }
    }

    progressed
  }
}

fn ds_seed_from_env() -> u64 {
  match std::env::var("LIO_DS_SEED") {
    Ok(value) => value.parse().expect("LIO_DS_SEED must parse as u64"),
    Err(_) => 7,
  }
}

#[test]
fn two_lio_instances_can_talk_over_a_simulated_network() {
  const CLIENT_MESSAGES: [&[u8]; 3] = [b"ping-1", b"ping-2", b"ping-3"];
  const SERVER_MESSAGES: [&[u8]; 3] = [b"pong-1", b"pong-2", b"pong-3"];

  let seed = ds_seed_from_env();
  let config = DSConfig {
    seed,
    max_delay_ticks: 2,
    fault_every: 0,
    network_faults: DSNetworkFaults::Off,
  };
  let mut dst = DST::with_config(config);
  let listen_addr: SocketAddr = "127.0.0.1:7000".parse().unwrap();

  println!("seed={seed}");

  let server_listening = Rc::new(RefCell::new(false));
  let accepted_addr = Rc::new(RefCell::new(None::<SocketAddr>));
  let server_received = Rc::new(RefCell::new(Vec::<Vec<u8>>::new()));
  let client_received = Rc::new(RefCell::new(Vec::<Vec<u8>>::new()));

  dst
    .add_node(64, {
      let mut server = ServerNode::new(
        server_listening.clone(),
        accepted_addr.clone(),
        server_received.clone(),
        &SERVER_MESSAGES,
        &CLIENT_MESSAGES,
      );

      move |lio: Lio| {
        loop {
          let mut progressed = server.drive(listen_addr);

          if lio.try_run()? > 0 {
            progressed = true;
          }

          if !progressed {
            break;
          }
        }
        Ok(())
      }
    })
    .unwrap();

  for _ in 0..32 {
    if *server_listening.borrow() {
      break;
    }
    trace("dst: tick");
    dst.tick().unwrap();
  }

  assert!(*server_listening.borrow(), "server should start listening");

  dst
    .add_node(64, {
      let mut client = ClientNode::new(
        client_received.clone(),
        &CLIENT_MESSAGES,
        &SERVER_MESSAGES,
      );

      move |lio: Lio| {
        loop {
          let mut progressed = client.drive(listen_addr);

          if lio.try_run()? > 0 {
            progressed = true;
          }

          if !progressed {
            break;
          }
        }
        Ok(())
      }
    })
    .unwrap();

  for _ in 0..64 {
    if simulation_done(
      &server_received,
      &client_received,
      CLIENT_MESSAGES.len(),
      SERVER_MESSAGES.len(),
    ) {
      break;
    }
    trace("dst: tick");
    dst.tick().unwrap();
  }

  let expected_server: Vec<Vec<u8>> =
    CLIENT_MESSAGES.iter().map(|bytes| bytes.to_vec()).collect();
  let expected_client: Vec<Vec<u8>> =
    SERVER_MESSAGES.iter().map(|bytes| bytes.to_vec()).collect();
  assert_eq!(*server_received.borrow(), expected_server);
  assert_eq!(*client_received.borrow(), expected_client);
  let accepted_addr =
    accepted_addr.borrow().expect("server should accept a peer");
  assert!(accepted_addr.ip().is_loopback());
  assert!(accepted_addr.port() >= 30_000);
}
