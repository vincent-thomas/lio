//! One query to one nameserver: over UDP, then over TCP if the answer did
//! not fit, within a timeout.

use std::{
  io, mem,
  net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
  time::Duration,
};

use hickory_proto::op::{Edns, Message, MessageType, Query, ResponseCode};
use lio::{
  api::{
    self, Io, SockDomain, SockProto, SockType, ops,
    resource::{FromResource, Resource},
  },
  model::{Cx, OpModel, Step},
  net::UdpSocket,
};

use crate::Error;

/// The UDP payload size advertised with EDNS: what fits in one packet on
/// any path (the DNS flag day 2020 value).
const UDP_PAYLOAD: u16 = 1232;

/// Room for one datagram. Larger than advertised, for servers that ignore
/// the advertisement.
const UDP_RECV: usize = 4096;

/// Room for one read of a TCP answer.
const TCP_RECV: usize = 4096;

/// One query to one nameserver.
///
/// The answer comes over UDP unless it is truncated, then over TCP. A
/// datagram from another address, or one that does not answer this query,
/// is dropped and the next one awaited. The whole exchange, and the TCP
/// part again, gets `timeout`; when it runs out the operation in flight is
/// cancelled.
pub(crate) struct Exchange {
  server: SocketAddr,
  query: Query,
  id: u16,
  /// The encoded message, without the TCP length prefix.
  bytes: Vec<u8>,
  timeout: Duration,
  timer: Io<ops::Sleep>,
  state: State,
}

enum State {
  UdpSocket(Io<ops::Socket>),
  UdpBind(UdpSocket, Io<ops::Bind>),
  UdpSend(UdpSocket, Io<ops::Send<Vec<u8>>>),
  UdpRecv(UdpSocket, Io<ops::RecvFrom<Vec<u8>>>),
  TcpSocket(Io<ops::Socket>),
  TcpConnect(Resource, Io<ops::Connect>),
  TcpSend(Resource, Io<ops::Send<Vec<u8>>>),
  /// The bytes so far, length prefix included.
  TcpRecv(Resource, Io<ops::Recv<Vec<u8>>>, Vec<u8>),
  Finished,
}

/// What one step of the state machine did.
enum Advance {
  /// Moved on; poll the new state.
  Next(State),
  /// Waits for the operation in this state.
  Wait(State),
  Done(Result<Message, Error>),
}

impl Exchange {
  pub(crate) fn new(
    server: SocketAddr,
    query: Query,
    timeout: Duration,
  ) -> Result<Self, Error> {
    let mut message = Message::query();
    message.metadata.recursion_desired = true;
    message.add_query(query.clone());
    let mut edns = Edns::new();
    edns.set_max_payload(UDP_PAYLOAD);
    message.set_edns(edns);
    let bytes = message.to_vec().map_err(Error::Encode)?;
    Ok(Self {
      server,
      query,
      id: message.metadata.id,
      bytes,
      timeout,
      timer: api::sleep(timeout),
      state: State::UdpSocket(api::socket(
        domain(server),
        SockType::DGRAM,
        SockProto::UDP,
      )),
    })
  }

  /// Whether `message` answers this exchange's query.
  fn answers(&self, message: &Message) -> bool {
    let question = match message.queries.as_slice() {
      [question] => question == &self.query,
      // Some servers leave the question out of an error.
      [] => message.metadata.response_code != ResponseCode::NoError,
      _ => false,
    };
    message.metadata.id == self.id
      && message.metadata.message_type == MessageType::Response
      && question
  }

  fn recv_datagram(socket: UdpSocket) -> State {
    let recv = socket.recvfrom(Vec::with_capacity(UDP_RECV));
    State::UdpRecv(socket, recv)
  }

  fn advance(&mut self, state: State, cx: &mut Cx<'_>) -> Advance {
    use Advance::{Done, Next, Wait};

    /// The operation's result, or `Wait` with the state put back.
    macro_rules! wait {
      ($op:expr, $state:expr) => {
        match $op.poll(cx) {
          Step::Done(out) => out,
          Step::Pending => return Wait($state),
        }
      };
    }

    match state {
      State::UdpSocket(mut op) => match wait!(op, State::UdpSocket(op)) {
        Ok(resource) => {
          let socket = UdpSocket::from_resource(resource);
          let bind = api::bind(&socket, unspecified(self.server));
          Next(State::UdpBind(socket, bind))
        }
        Err(err) => Done(Err(Error::Io(err))),
      },
      State::UdpBind(socket, mut op) => {
        match wait!(op, State::UdpBind(socket, op)) {
          Ok(()) => {
            let send = socket.sendto(self.bytes.clone(), self.server);
            Next(State::UdpSend(socket, send))
          }
          Err(err) => Done(Err(Error::Io(err))),
        }
      }
      State::UdpSend(socket, mut op) => {
        match wait!(op, State::UdpSend(socket, op)) {
          (Ok(_), _bytes) => Next(Self::recv_datagram(socket)),
          (Err(err), _bytes) => Done(Err(Error::Io(err))),
        }
      }
      State::UdpRecv(socket, mut op) => {
        let (result, buf, from) = wait!(op, State::UdpRecv(socket, op));
        let len = match result {
          Ok(len) => len,
          Err(err) => return Done(Err(Error::Io(err))),
        };
        if from != Some(self.server) {
          return Next(Self::recv_datagram(socket));
        }
        let Some(message) = buf
          .get(..len)
          .and_then(|datagram| Message::from_vec(datagram).ok())
          .filter(|message| self.answers(message))
        else {
          return Next(Self::recv_datagram(socket));
        };
        if !message.metadata.truncation {
          return Done(Ok(message));
        }
        // The answer did not fit: ask again over TCP, with a fresh timeout.
        self.timer = api::sleep(self.timeout);
        Next(State::TcpSocket(api::socket(
          domain(self.server),
          SockType::STREAM,
          SockProto::TCP,
        )))
      }
      State::TcpSocket(mut op) => match wait!(op, State::TcpSocket(op)) {
        Ok(socket) => {
          let connect = api::connect(&socket, self.server);
          Next(State::TcpConnect(socket, connect))
        }
        Err(err) => Done(Err(Error::Io(err))),
      },
      State::TcpConnect(socket, mut op) => {
        match wait!(op, State::TcpConnect(socket, op)) {
          Ok(()) => {
            let Ok(len) = u16::try_from(self.bytes.len()) else {
              return Done(Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "query longer than a DNS message can be",
              ))));
            };
            let mut framed = Vec::with_capacity(2 + self.bytes.len());
            framed.extend_from_slice(&len.to_be_bytes());
            framed.extend_from_slice(&self.bytes);
            let send = api::send(&socket, framed, None);
            Next(State::TcpSend(socket, send))
          }
          Err(err) => Done(Err(Error::Io(err))),
        }
      }
      State::TcpSend(socket, mut op) => {
        match wait!(op, State::TcpSend(socket, op)) {
          (Ok(0), _) => Done(Err(Error::Io(io::ErrorKind::WriteZero.into()))),
          (Ok(sent), mut rest) if sent < rest.len() => {
            rest.drain(..sent);
            let send = api::send(&socket, rest, None);
            Next(State::TcpSend(socket, send))
          }
          (Ok(_), _) => {
            let recv = api::recv(&socket, Vec::with_capacity(TCP_RECV), None);
            Next(State::TcpRecv(socket, recv, Vec::new()))
          }
          (Err(err), _) => Done(Err(Error::Io(err))),
        }
      }
      State::TcpRecv(socket, mut op, mut got) => {
        let (result, buf) = wait!(op, State::TcpRecv(socket, op, got));
        match result {
          Ok(0) => {
            return Done(Err(Error::Io(io::ErrorKind::UnexpectedEof.into())));
          }
          Ok(len) => got.extend_from_slice(buf.get(..len).unwrap_or(&buf)),
          Err(err) => return Done(Err(Error::Io(err))),
        }
        let framed = match got.as_slice() {
          [hi, lo, rest @ ..] => {
            rest.get(..usize::from(u16::from_be_bytes([*hi, *lo])))
          }
          _ => None,
        };
        let Some(framed) = framed else {
          let recv = api::recv(&socket, Vec::with_capacity(TCP_RECV), None);
          return Next(State::TcpRecv(socket, recv, got));
        };
        match Message::from_vec(framed) {
          Ok(message) if self.answers(&message) => Done(Ok(message)),
          Ok(_) => Done(Err(invalid_data("the answer is to another query"))),
          Err(err) => Done(Err(invalid_data(err.to_string()))),
        }
      }
      State::Finished => {
        unreachable!("Exchange: polled after it finished")
      }
    }
  }
}

impl OpModel for Exchange {
  type Item = Result<Message, Error>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    loop {
      let state = mem::replace(&mut self.state, State::Finished);
      match self.advance(state, cx) {
        Advance::Next(next) => self.state = next,
        Advance::Wait(waiting) => {
          self.state = waiting;
          break;
        }
        Advance::Done(result) => return Step::Done(result),
      }
    }
    match self.timer.poll(cx) {
      Step::Pending => Step::Pending,
      // Dropping the state cancels the operation in flight.
      Step::Done(Ok(())) => Step::Done(Err(Error::Timeout)),
      Step::Done(Err(err)) => Step::Done(Err(Error::Io(err))),
    }
  }
}

fn domain(server: SocketAddr) -> SockDomain {
  match server {
    SocketAddr::V4(_) => SockDomain::IPV4,
    SocketAddr::V6(_) => SockDomain::IPV6,
  }
}

/// Any local address and port of `server`'s family.
fn unspecified(server: SocketAddr) -> SocketAddr {
  let ip = match server {
    SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
    SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
  };
  SocketAddr::new(ip, 0)
}

fn invalid_data(msg: impl Into<String>) -> Error {
  Error::Io(io::Error::new(io::ErrorKind::InvalidData, msg.into()))
}
