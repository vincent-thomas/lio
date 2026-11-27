//! Lookups against nameservers played by std sockets on another thread.

use std::{
  future::{Future, IntoFuture},
  io::{Read, Write},
  net::{IpAddr, SocketAddr, TcpListener, UdpSocket},
  sync::{Arc, Mutex},
  task::{Context, Poll, Waker},
  thread::{self, JoinHandle},
  time::{Duration, Instant},
};

use lio::{Lio, install_global};
use lio_dns::{
  Error, Resolver, ResolverConfig,
  proto::{
    op::{Message, OpCode, ResponseCode},
    rr::{
      Name, RData, Record, RecordType,
      rdata::{A, AAAA, MX},
    },
  },
};

fn block_on<T>(lio: &Lio, fut: impl IntoFuture<Output = T>) -> T {
  let mut cx = Context::from_waker(Waker::noop());
  let mut fut = Box::pin(fut.into_future());
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    if let Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
      return value;
    }
    assert!(Instant::now() < deadline, "future timed out");
    lio.run_timeout(Duration::from_millis(5)).unwrap();
  }
}

fn name(s: &str) -> Name {
  Name::from_ascii(s).unwrap()
}

fn ip(s: &str) -> IpAddr {
  s.parse().unwrap()
}

/// The answer to `request` a nameserver would give.
fn reply(
  request: &Message,
  code: ResponseCode,
  answers: Vec<Record>,
) -> Message {
  let mut message = Message::response(request.metadata.id, OpCode::Query);
  message.metadata.response_code = code;
  message.metadata.recursion_available = true;
  message.add_queries(request.queries.clone());
  message.add_answers(answers);
  message
}

/// The records a small zone holds for the question in `request`.
fn zone(request: &Message) -> Message {
  let query = &request.queries[0];
  let owner = query.name().clone();
  let answers = match (owner.to_ascii().as_str(), query.query_type()) {
    ("host.test.", RecordType::A) => vec![Record::from_rdata(
      owner,
      60,
      RData::A(A("192.0.2.1".parse().unwrap())),
    )],
    ("host.test.", RecordType::AAAA) => vec![Record::from_rdata(
      owner,
      60,
      RData::AAAA(AAAA("2001:db8::1".parse().unwrap())),
    )],
    ("host.test.", RecordType::MX) => vec![Record::from_rdata(
      owner,
      60,
      RData::MX(MX::new(10, name("mail.test."))),
    )],
    ("v4only.test.", RecordType::A) => vec![Record::from_rdata(
      owner,
      60,
      RData::A(A("192.0.2.4".parse().unwrap())),
    )],
    ("v4only.test.", _) => {
      return reply(request, ResponseCode::NoError, vec![]);
    }
    _ => return reply(request, ResponseCode::NXDomain, vec![]),
  };
  reply(request, ResponseCode::NoError, answers)
}

/// A UDP nameserver that answers `count` queries with `answer`, which
/// sends whatever it likes back. It records the questions it was asked.
struct Server {
  addr: SocketAddr,
  asked: Arc<Mutex<Vec<String>>>,
  thread: JoinHandle<()>,
}

impl Server {
  fn udp(
    count: usize,
    mut answer: impl FnMut(&UdpSocket, SocketAddr, &Message) + Send + 'static,
  ) -> Self {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let addr = socket.local_addr().unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&asked);
    let thread = thread::spawn(move || {
      let mut buf = [0; 4096];
      for _ in 0..count {
        let Ok((len, from)) = socket.recv_from(&mut buf) else { return };
        let request = Message::from_vec(&buf[..len]).unwrap();
        let query = &request.queries[0];
        log.lock().unwrap().push(format!(
          "{} {}",
          query.name().to_ascii(),
          query.query_type()
        ));
        answer(&socket, from, &request);
      }
    });
    Self { addr, asked, thread }
  }

  /// Answers every query from [`zone`].
  fn zone(count: usize) -> Self {
    Self::udp(count, |socket, from, request| {
      socket.send_to(&zone(request).to_vec().unwrap(), from).unwrap();
    })
  }

  fn asked(self) -> Vec<String> {
    self.thread.join().unwrap();
    Arc::try_unwrap(self.asked).unwrap().into_inner().unwrap()
  }
}

fn resolver(servers: &[SocketAddr]) -> Resolver {
  let mut config = ResolverConfig::new(servers.to_vec());
  config.timeout = Duration::from_millis(300);
  config.attempts = 1;
  Resolver::new(config)
}

#[test]
fn lookup_ip_returns_ipv4_then_ipv6() {
  let lio = Lio::new(64).unwrap();
  let _guard = install_global(lio.clone());
  let server = Server::zone(2);
  let resolver = resolver(&[server.addr]);

  let addrs = block_on(&lio, resolver.lookup_ip("host.test")).unwrap();

  assert_eq!(addrs, [ip("192.0.2.1"), ip("2001:db8::1")]);
  let mut asked = server.asked();
  asked.sort();
  assert_eq!(asked, ["host.test. A", "host.test. AAAA"]);
}

#[test]
fn lookup_ip_without_ipv6_records_returns_ipv4() {
  let lio = Lio::new(64).unwrap();
  let _guard = install_global(lio.clone());
  let server = Server::zone(2);
  let resolver = resolver(&[server.addr]);

  let addrs = block_on(&lio, resolver.lookup_ip("v4only.test")).unwrap();

  assert_eq!(addrs, [ip("192.0.2.4")]);
  server.asked();
}

#[test]
fn lookup_returns_records_of_any_type() {
  let lio = Lio::new(64).unwrap();
  let _guard = install_global(lio.clone());
  let server = Server::zone(1);
  let resolver = resolver(&[server.addr]);

  let answer =
    block_on(&lio, resolver.lookup("host.test", RecordType::MX)).unwrap();

  let [record] = answer.answers.as_slice() else { panic!("{answer:?}") };
  let RData::MX(mx) = &record.data else { panic!("{record:?}") };
  assert_eq!(mx.exchange, name("mail.test."));
  server.asked();
}

#[test]
fn a_missing_name_is_no_records_with_nxdomain() {
  let lio = Lio::new(64).unwrap();
  let _guard = install_global(lio.clone());
  let server = Server::zone(1);
  let resolver = resolver(&[server.addr]);

  let err =
    block_on(&lio, resolver.lookup("nope.test", RecordType::A)).unwrap_err();

  let Error::NoRecords { name, code } = err else { panic!("{err:?}") };
  assert_eq!(name, "nope.test");
  assert_eq!(code, ResponseCode::NXDomain);
  server.asked();
}

#[test]
fn search_domains_are_tried_before_a_short_name() {
  let lio = Lio::new(64).unwrap();
  let _guard = install_global(lio.clone());
  // `host.corp.test.` does not exist; `host.test.` does.
  let server = Server::zone(2);
  let mut config = ResolverConfig::new(vec![server.addr]);
  config.search = vec![name("corp.test."), name("test.")];
  config.ndots = 2;
  let resolver = Resolver::new(config);

  let answer = block_on(&lio, resolver.lookup("host", RecordType::A)).unwrap();

  assert_eq!(answer.answers[0].name, name("host.test."));
  assert_eq!(server.asked(), ["host.corp.test. A", "host.test. A"]);
}

#[test]
fn stray_datagrams_are_dropped() {
  let lio = Lio::new(64).unwrap();
  let _guard = install_global(lio.clone());
  let server = Server::udp(1, |socket, from, request| {
    // From another address.
    let other = UdpSocket::bind("127.0.0.1:0").unwrap();
    other.send_to(&zone(request).to_vec().unwrap(), from).unwrap();
    // To another query.
    let mut wrong_id = zone(request);
    wrong_id.metadata.id = request.metadata.id.wrapping_add(1);
    socket.send_to(&wrong_id.to_vec().unwrap(), from).unwrap();
    // Not DNS at all.
    socket.send_to(b"garbage", from).unwrap();
    // The answer.
    let mut answer = zone(request);
    answer.answers[0].data = RData::A(A("192.0.2.99".parse().unwrap()));
    socket.send_to(&answer.to_vec().unwrap(), from).unwrap();
  });
  let resolver = resolver(&[server.addr]);

  let answer =
    block_on(&lio, resolver.lookup("host.test", RecordType::A)).unwrap();

  assert_eq!(
    answer.answers[0].data,
    RData::A(A("192.0.2.99".parse().unwrap()))
  );
  server.asked();
}

#[test]
fn a_silent_nameserver_times_out_and_the_next_is_asked() {
  let lio = Lio::new(64).unwrap();
  let _guard = install_global(lio.clone());
  let silent = Server::udp(1, |_, _, _| {});
  let server = Server::zone(1);
  let resolver = resolver(&[silent.addr, server.addr]);

  let started = Instant::now();
  let answer =
    block_on(&lio, resolver.lookup("host.test", RecordType::A)).unwrap();

  assert!(started.elapsed() >= Duration::from_millis(300));
  assert_eq!(answer.answers[0].name, name("host.test."));
  silent.asked();
  server.asked();
}

#[test]
fn only_silent_nameservers_is_a_timeout() {
  let lio = Lio::new(64).unwrap();
  let _guard = install_global(lio.clone());
  let silent = Server::udp(2, |_, _, _| {});
  let mut config = ResolverConfig::new(vec![silent.addr]);
  config.timeout = Duration::from_millis(100);
  config.attempts = 2;
  let resolver = Resolver::new(config);

  let err =
    block_on(&lio, resolver.lookup("host.test", RecordType::A)).unwrap_err();

  assert!(matches!(err, Error::Timeout), "{err:?}");
  assert_eq!(silent.asked(), ["host.test. A", "host.test. A"]);
}

#[test]
fn a_failing_nameserver_is_passed_over() {
  let lio = Lio::new(64).unwrap();
  let _guard = install_global(lio.clone());
  let failing = Server::udp(1, |socket, from, request| {
    let answer = reply(request, ResponseCode::ServFail, vec![]);
    socket.send_to(&answer.to_vec().unwrap(), from).unwrap();
  });
  let server = Server::zone(1);
  let resolver = resolver(&[failing.addr, server.addr]);

  let answer =
    block_on(&lio, resolver.lookup("host.test", RecordType::A)).unwrap();

  assert_eq!(answer.answers[0].name, name("host.test."));
  failing.asked();
  server.asked();
}

#[test]
fn a_truncated_answer_is_asked_again_over_tcp() {
  let lio = Lio::new(64).unwrap();
  let _guard = install_global(lio.clone());
  let server = Server::udp(1, |socket, from, request| {
    let mut truncated = reply(request, ResponseCode::NoError, vec![]);
    truncated.metadata.truncation = true;
    socket.send_to(&truncated.to_vec().unwrap(), from).unwrap();
  });
  let tcp = TcpListener::bind(server.addr).unwrap();
  let tcp_server = thread::spawn(move || {
    let (mut stream, _) = tcp.accept().unwrap();
    let mut len = [0; 2];
    stream.read_exact(&mut len).unwrap();
    let mut request = vec![0; usize::from(u16::from_be_bytes(len))];
    stream.read_exact(&mut request).unwrap();
    let request = Message::from_vec(&request).unwrap();
    let answer = zone(&request).to_vec().unwrap();
    let mut framed =
      u16::try_from(answer.len()).unwrap().to_be_bytes().to_vec();
    framed.extend_from_slice(&answer);
    // A byte at a time, so the answer arrives in pieces.
    for byte in framed {
      stream.write_all(&[byte]).unwrap();
      stream.flush().unwrap();
    }
  });
  let resolver = resolver(&[server.addr]);

  let answer =
    block_on(&lio, resolver.lookup("host.test", RecordType::A)).unwrap();

  assert_eq!(answer.answers[0].data, RData::A(A("192.0.2.1".parse().unwrap())));
  server.asked();
  tcp_server.join().unwrap();
}

#[test]
fn literals_and_the_host_table_need_no_nameserver() {
  let lio = Lio::new(64).unwrap();
  let _guard = install_global(lio.clone());
  let mut config = ResolverConfig::new(Vec::new());
  config.hosts.insert("box.local", ip("10.1.2.3"));
  let resolver = Resolver::new(config);

  let literal = block_on(&lio, resolver.lookup_ip("2001:db8::7")).unwrap();
  assert_eq!(literal, [ip("2001:db8::7")]);
  let hosted = block_on(&lio, resolver.lookup_ip("BOX.local.")).unwrap();
  assert_eq!(hosted, [ip("10.1.2.3")]);
  let err = block_on(&lio, resolver.lookup_ip("elsewhere")).unwrap_err();
  assert!(matches!(err, Error::NoNameservers), "{err:?}");
}

#[test]
fn an_invalid_name_fails_without_asking() {
  let lio = Lio::new(64).unwrap();
  let _guard = install_global(lio.clone());
  let resolver = resolver(&["127.0.0.1:9".parse().unwrap()]);

  let long_label = "a".repeat(64);
  let err =
    block_on(&lio, resolver.lookup(&long_label, RecordType::A)).unwrap_err();

  assert!(matches!(err, Error::InvalidName(_)), "{err:?}");
}

#[test]
fn a_lookup_runs_on_a_named_driver_through_a_channel() {
  let lio = Lio::new(64).unwrap();
  let server = Server::zone(2);
  let resolver = resolver(&[server.addr]);

  let mut receiver = resolver.lookup_ip("host.test").with_lio(&lio).send();
  let deadline = Instant::now() + Duration::from_secs(10);
  let addrs = loop {
    if let Some(addrs) = receiver.try_recv() {
      break addrs;
    }
    assert!(Instant::now() < deadline, "lookup timed out");
    lio.run_timeout(Duration::from_millis(5)).unwrap();
  };

  assert_eq!(addrs.unwrap(), [ip("192.0.2.1"), ip("2001:db8::1")]);
  server.asked();
}

#[test]
fn load_reads_resolv_conf_and_hosts() {
  let lio = Lio::new(64).unwrap();
  let _guard = install_global(lio.clone());
  let dir =
    std::env::temp_dir().join(format!("lio-dns-load-{}", std::process::id()));
  std::fs::create_dir_all(&dir).unwrap();
  let resolv_conf = dir.join("resolv.conf");
  let hosts = dir.join("hosts");
  std::fs::write(&resolv_conf, "nameserver 192.0.2.53\nsearch corp.test\n")
    .unwrap();
  std::fs::write(&hosts, "10.9.9.9 printer\n").unwrap();

  let resolver = block_on(&lio, Resolver::load(&resolv_conf, &hosts)).unwrap();
  let missing_hosts =
    block_on(&lio, Resolver::load(&resolv_conf, dir.join("absent"))).unwrap();
  let missing_conf =
    block_on(&lio, Resolver::load(dir.join("absent"), &hosts)).err().unwrap();
  std::fs::remove_dir_all(&dir).unwrap();

  let config = resolver.config();
  assert_eq!(
    config.nameservers,
    ["192.0.2.53:53".parse::<SocketAddr>().unwrap()]
  );
  assert_eq!(config.search, [name("corp.test.")]);
  assert_eq!(config.hosts.get("printer"), Some(&[ip("10.9.9.9")][..]));
  assert_eq!(missing_hosts.config().hosts.get("printer"), None);
  assert_eq!(missing_conf.kind(), std::io::ErrorKind::NotFound);
}
