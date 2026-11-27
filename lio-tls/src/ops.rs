//! The TLS operations, as lio [models](lio::model).
//!
//! You get these from [`TlsConnector::connect`](crate::TlsConnector::connect),
//! [`TlsAcceptor::accept`](crate::TlsAcceptor::accept),
//! [`TlsStream::send`] and [`TlsStream::recv`], wrapped in an
//! [`Io`](lio::api::Io).
//!
//! Each holds the stream while it runs and hands it back when it is done.
//! Polled after that, each panics.

use std::io;
use std::ops::DerefMut;

use lio::model::{Cx, OpModel, Step};
use rustls::{ConnectionCommon, SideData};

use crate::{Session, TlsStream};

/// The TLS handshake. Yields the stream, ready for application data.
#[must_use = "an operation does nothing until it is run or polled"]
pub struct Handshake<C> {
  state: Option<Result<Session<C>, io::Error>>,
}

impl<C, D> Handshake<C>
where
  C: DerefMut<Target = ConnectionCommon<D>>,
  D: SideData,
{
  /// A handshake over `stream`, or, given an error, one that fails with
  /// it when polled.
  pub(crate) fn new(stream: io::Result<TlsStream<C>>) -> Self {
    Self { state: Some(stream.map(Session::new)) }
  }
}

impl<C, D> OpModel for Handshake<C>
where
  C: DerefMut<Target = ConnectionCommon<D>>,
  D: SideData,
{
  type Item = io::Result<TlsStream<C>>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    let Some(state) = self.state.take() else {
      panic!("TLS handshake polled after it was done");
    };
    let mut session = match state {
      Ok(session) => session,
      Err(err) => return Step::Done(Err(err)),
    };
    match session.poll_handshake(cx) {
      Step::Pending => {
        self.state = Some(Ok(session));
        Step::Pending
      }
      Step::Done(result) => Step::Done(result.map(|()| session.into_stream())),
    }
  }
}

/// Sends a buffer of plaintext. Yields the number of bytes sent, the
/// buffer and the stream.
#[must_use = "an operation does nothing until it is run or polled"]
pub struct Send<C, B> {
  running: Option<(Session<C>, B)>,
  /// How much of the buffer rustls has taken.
  written: usize,
}

impl<C, D, B> Send<C, B>
where
  C: DerefMut<Target = ConnectionCommon<D>>,
  D: SideData,
{
  pub(crate) fn new(stream: TlsStream<C>, buf: B) -> Self {
    Self { running: Some((Session::new(stream), buf)), written: 0 }
  }
}

impl<C, D, B> OpModel for Send<C, B>
where
  C: DerefMut<Target = ConnectionCommon<D>>,
  D: SideData,
  B: AsRef<[u8]>,
{
  type Item = (io::Result<usize>, B, TlsStream<C>);

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    let Some((mut session, buf)) = self.running.take() else {
      panic!("TLS send polled after it was done");
    };
    match session.poll_send(cx, buf.as_ref(), &mut self.written) {
      Step::Pending => {
        self.running = Some((session, buf));
        Step::Pending
      }
      Step::Done(result) => Step::Done((result, buf, session.into_stream())),
    }
  }
}

/// Receives plaintext into a buffer. Yields the number of bytes received,
/// the buffer and the stream.
#[must_use = "an operation does nothing until it is run or polled"]
pub struct Recv<C, B> {
  running: Option<(Session<C>, B)>,
}

impl<C, D, B> Recv<C, B>
where
  C: DerefMut<Target = ConnectionCommon<D>>,
  D: SideData,
{
  pub(crate) fn new(stream: TlsStream<C>, buf: B) -> Self {
    Self { running: Some((Session::new(stream), buf)) }
  }
}

impl<C, D, B> OpModel for Recv<C, B>
where
  C: DerefMut<Target = ConnectionCommon<D>>,
  D: SideData,
  B: AsMut<[u8]>,
{
  type Item = (io::Result<usize>, B, TlsStream<C>);

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    let Some((mut session, mut buf)) = self.running.take() else {
      panic!("TLS recv polled after it was done");
    };
    match session.poll_recv(cx, buf.as_mut()) {
      Step::Pending => {
        self.running = Some((session, buf));
        Step::Pending
      }
      Step::Done(result) => Step::Done((result, buf, session.into_stream())),
    }
  }
}
