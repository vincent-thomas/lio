//! The TLS operations run every way an lio operation does, not only as
//! futures.

mod common;

use std::{cell::RefCell, io, rc::Rc};

use lio::{
  Lio,
  api::{self, Io},
  install_global,
  model::{Cx, OpModel, Step},
  net::TcpStream,
  ready,
};
use lio_tls::{
  TlsConnector,
  ops::{Handshake, Recv, Send},
};
use rustls::ClientConnection;

#[test]
fn a_client_runs_on_callbacks_alone() -> io::Result<()> {
  let lio = Lio::new(64)?;
  let _guard = install_global(lio.clone());
  let (server_cfg, client_cfg) = common::configs();
  let (addr, server) = common::spawn_ping_pong_server(server_cfg)?;

  let got = Rc::new(RefCell::new(None));
  let sink = Rc::clone(&got);
  let connector = TlsConnector::new(client_cfg);
  TcpStream::connect(addr).when_done(move |tcp| {
    let sink = Rc::clone(&sink);
    connector.connect("localhost", tcp.unwrap()).when_done(move |tls| {
      let sink = Rc::clone(&sink);
      tls.unwrap().send(common::CLIENT_PING.to_vec()).when_done(
        move |(result, _, tls)| {
          result.unwrap();
          let sink = Rc::clone(&sink);
          tls.recv(vec![0; 64]).when_done(move |(result, mut buf, _)| {
            buf.truncate(result.unwrap());
            *sink.borrow_mut() = Some(buf);
          });
        },
      );
    });
  });

  common::run_until(&lio, || got.borrow().is_some());
  assert_eq!(got.borrow().as_deref(), Some(common::SERVER_PONG));
  common::join_io(server)
}

/// Connects, says ping and reads the answer: the TLS operations, polled
/// from a model of our own.
enum PingPong<T> {
  Connecting(T, TlsConnector),
  Handshaking(Io<Handshake<ClientConnection>>),
  Sending(Io<Send<ClientConnection, Vec<u8>>>),
  Receiving(Io<Recv<ClientConnection, Vec<u8>>>),
}

impl<T: OpModel<Item = io::Result<TcpStream>>> OpModel for PingPong<T> {
  type Item = io::Result<Vec<u8>>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    loop {
      *self = match self {
        PingPong::Connecting(connect, connector) => {
          match ready!(connect.poll(cx)) {
            Ok(tcp) => {
              PingPong::Handshaking(connector.connect("localhost", tcp))
            }
            Err(err) => return Step::Done(Err(err)),
          }
        }
        PingPong::Handshaking(handshake) => match ready!(handshake.poll(cx)) {
          Ok(tls) => PingPong::Sending(tls.send(common::CLIENT_PING.to_vec())),
          Err(err) => return Step::Done(Err(err)),
        },
        PingPong::Sending(send) => match ready!(send.poll(cx)) {
          (Ok(_), _, tls) => PingPong::Receiving(tls.recv(vec![0; 64])),
          (Err(err), _, _) => return Step::Done(Err(err)),
        },
        PingPong::Receiving(recv) => {
          let (result, mut buf, _) = ready!(recv.poll(cx));
          return Step::Done(result.map(|n| {
            buf.truncate(n);
            buf
          }));
        }
      };
    }
  }
}

#[test]
fn a_model_of_ours_polls_the_tls_operations() -> io::Result<()> {
  let lio = Lio::new(64)?;
  let (server_cfg, client_cfg) = common::configs();
  let (addr, server) = common::spawn_ping_pong_server(server_cfg)?;

  let got = Rc::new(RefCell::new(None));
  let sink = Rc::clone(&got);
  let model = PingPong::Connecting(
    TcpStream::connect(addr),
    TlsConnector::new(client_cfg),
  );
  api::run(model)
    .with_lio(&lio)
    .when_done(move |result| *sink.borrow_mut() = Some(result));

  common::run_until(&lio, || got.borrow().is_some());
  let Some(got) = got.take() else { unreachable!("waited for it above") };
  assert_eq!(got?, common::SERVER_PONG);
  common::join_io(server)
}

#[test]
fn recv_into_an_empty_buffer_yields_nothing_at_once() -> io::Result<()> {
  let lio = Lio::new(64)?;
  let _guard = install_global(lio.clone());
  let (server_cfg, client_cfg) = common::configs();
  let (addr, server) = common::spawn_ping_pong_server(server_cfg)?;

  common::block_on(&lio, async {
    let tcp = TcpStream::connect(addr).await?;
    let tls = TlsConnector::new(client_cfg).connect("localhost", tcp).await?;
    let (result, _, tls) = tls.recv(Vec::new()).await;
    assert_eq!(result?, 0);

    // The session is still whole.
    let (result, _, tls) = tls.send(common::CLIENT_PING.to_vec()).await;
    result?;
    let (result, buf, _tls) = tls.recv(vec![0; 64]).await;
    assert_eq!(&buf[..result?], common::SERVER_PONG);
    io::Result::Ok(())
  })?;
  common::join_io(server)
}
