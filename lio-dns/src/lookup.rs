//! Lookups: a name through the search list and the nameservers.

use std::{net::IpAddr, sync::Arc};

use hickory_proto::{
  op::{Message, Query, ResponseCode},
  rr::{Name, RData, RecordType},
};
use lio::model::{Cx, Join, OpModel, Step};

use crate::{Error, ResolverConfig, exchange::Exchange};

/// A lookup of one record type, from
/// [`Resolver::lookup`](crate::Resolver::lookup).
///
/// Each name of the search list is asked of every nameserver in turn,
/// `attempts` rounds over, until one answers it. A name that does not
/// exist, or has no records of the type, moves on to the next name. The
/// result is the first answer with records of the type.
pub struct Lookup {
  state: LookupState,
}

enum LookupState {
  /// Settled before anything was asked.
  Failed(Option<Error>),
  Asking(Box<Search>),
}

struct Search {
  config: Arc<ResolverConfig>,
  /// The name as given, for the error.
  asked: String,
  record_type: RecordType,
  candidates: Vec<Name>,
  candidate: usize,
  /// Which try of this candidate: `attempt * nameservers + nameserver`.
  try_index: usize,
  current: Exchange,
  /// How the last candidate that was given up on ended.
  last: Option<Error>,
}

impl Lookup {
  pub(crate) fn new(
    config: Arc<ResolverConfig>,
    name: &str,
    record_type: RecordType,
  ) -> Self {
    let state = match Search::new(config, name, record_type) {
      Ok(search) => LookupState::Asking(Box::new(search)),
      Err(err) => LookupState::Failed(Some(err)),
    };
    Self { state }
  }
}

impl Search {
  fn new(
    config: Arc<ResolverConfig>,
    name: &str,
    record_type: RecordType,
  ) -> Result<Self, Error> {
    let parsed = Name::from_utf8(name).map_err(Error::InvalidName)?;
    let candidates = config.candidates(&parsed);
    let Some(first) = candidates.first() else {
      unreachable!("Lookup: a name always has itself as a candidate")
    };
    let Some(&server) = config.nameservers.first() else {
      return Err(Error::NoNameservers);
    };
    let current = Exchange::new(
      server,
      Query::query(first.clone(), record_type),
      config.timeout,
    )?;
    Ok(Self {
      asked: name.to_owned(),
      record_type,
      candidates,
      candidate: 0,
      try_index: 0,
      current,
      last: None,
      config,
    })
  }

  fn tries(&self) -> usize {
    self.config.nameservers.len() * usize::from(self.config.attempts.max(1))
  }

  /// Gives up on this candidate, which ended with `outcome`. Whether
  /// another is left.
  fn next_candidate(&mut self, outcome: Error) -> bool {
    self.last = Some(outcome);
    self.candidate += 1;
    self.try_index = 0;
    self.candidate < self.candidates.len()
  }

  /// Gives up on this try, which failed with `err`. Whether another try,
  /// of this candidate or the next, is left.
  fn next_try(&mut self, err: Error) -> bool {
    self.try_index += 1;
    if self.try_index < self.tries() {
      return true;
    }
    self.next_candidate(err)
  }

  /// Starts the exchange for the current candidate and try.
  fn start(&mut self) -> Result<(), Error> {
    let servers = &self.config.nameservers;
    let (Some(name), Some(&server)) = (
      self.candidates.get(self.candidate),
      servers.get(self.try_index % servers.len()),
    ) else {
      unreachable!("Lookup: started past the last candidate or try")
    };
    self.current = Exchange::new(
      server,
      Query::query(name.clone(), self.record_type),
      self.config.timeout,
    )?;
    Ok(())
  }

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Result<Message, Error>> {
    loop {
      let more = match lio::ready!(self.current.poll(cx)) {
        Ok(message) => match message.metadata.response_code {
          ResponseCode::NoError
            if message
              .answers
              .iter()
              .any(|record| record.record_type() == self.record_type) =>
          {
            return Step::Done(Ok(message));
          }
          code @ (ResponseCode::NoError | ResponseCode::NXDomain) => self
            .next_candidate(Error::NoRecords {
              name: self.asked.clone(),
              code,
            }),
          code => self.next_try(Error::Server(code)),
        },
        Err(err) => self.next_try(err),
      };
      if !more {
        let Some(last) = self.last.take() else {
          unreachable!("Lookup: every candidate ends with an outcome")
        };
        return Step::Done(Err(last));
      }
      if let Err(err) = self.start() {
        return Step::Done(Err(err));
      }
    }
  }
}

impl OpModel for Lookup {
  /// The first answer with records of the type asked for.
  type Item = Result<Message, Error>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    match &mut self.state {
      LookupState::Failed(err) => {
        let Some(err) = err.take() else {
          unreachable!("Lookup: polled after it finished")
        };
        Step::Done(Err(err))
      }
      LookupState::Asking(search) => search.poll(cx),
    }
  }
}

/// A lookup of a host's addresses, from
/// [`Resolver::lookup_ip`](crate::Resolver::lookup_ip).
///
/// An address literal is its own answer, and a name in the host table is
/// answered from it. Otherwise the `A` and `AAAA` lookups run side by
/// side, and the IPv4 addresses come first.
pub struct LookupIp {
  state: LookupIpState,
}

enum LookupIpState {
  Known(Option<Result<Vec<IpAddr>, Error>>),
  Asking(Box<Join<Lookup, Lookup>>),
}

impl LookupIp {
  pub(crate) fn new(config: Arc<ResolverConfig>, host: &str) -> Self {
    if let Ok(ip) = host.parse::<IpAddr>() {
      return Self::known(Ok(vec![ip]));
    }
    if let Some(addrs) = config.hosts.get(host) {
      return Self::known(Ok(addrs.to_vec()));
    }
    let v4 = Lookup::new(Arc::clone(&config), host, RecordType::A);
    let v6 = Lookup::new(config, host, RecordType::AAAA);
    Self { state: LookupIpState::Asking(Box::new(Join::new(v4, v6))) }
  }

  fn known(result: Result<Vec<IpAddr>, Error>) -> Self {
    Self { state: LookupIpState::Known(Some(result)) }
  }
}

impl OpModel for LookupIp {
  type Item = Result<Vec<IpAddr>, Error>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    match &mut self.state {
      LookupIpState::Known(result) => {
        let Some(result) = result.take() else {
          unreachable!("LookupIp: polled after it finished")
        };
        Step::Done(result)
      }
      LookupIpState::Asking(join) => {
        let (v4, v6) = lio::ready!(join.poll(cx));
        Step::Done(combine(v4, v6))
      }
    }
  }
}

fn combine(
  v4: Result<Message, Error>,
  v6: Result<Message, Error>,
) -> Result<Vec<IpAddr>, Error> {
  let (v4, v6) = match (v4, v6) {
    (Err(a), Err(b)) => {
      // That a name has no records is the more certain of two answers:
      // report the other, which may pass.
      return Err(if matches!(a, Error::NoRecords { .. }) { b } else { a });
    }
    (v4, v6) => (v4.ok(), v6.ok()),
  };
  Ok(v4.iter().chain(v6.iter()).flat_map(addresses).collect())
}

fn addresses(message: &Message) -> impl Iterator<Item = IpAddr> + '_ {
  message.answers.iter().filter_map(|record| match &record.data {
    RData::A(a) => Some(IpAddr::V4(a.0)),
    RData::AAAA(aaaa) => Some(IpAddr::V6(aaaa.0)),
    _ => None,
  })
}
