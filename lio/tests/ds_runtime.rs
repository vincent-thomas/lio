use std::{
  collections::BTreeSet,
  env,
  ffi::CString,
  io,
  net::{Ipv4Addr, SocketAddr, SocketAddrV4},
  os::fd::{AsFd, AsRawFd},
  sync::mpsc,
  time::Duration,
  time::SystemTime,
};

use lio::api::Pid;
use lio::{
  Lio, api,
  api::resource::Resource,
  backend::{
    ds::{DSBackend, DSConfig, DSNetworkFaults, last_ds_trace_snapshot},
    op::{FileStat, LinkKind, SockDomain, SockProto, SockType},
  },
};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

#[derive(Debug)]
enum Event {
  Nop { id: usize, result: io::Result<()> },
  GetCwd { id: usize, result: io::Result<i32>, buf: Vec<u8> },
  Read { id: usize, result: io::Result<i32>, buf: Vec<u8> },
  Recv { id: usize, result: io::Result<i32>, buf: Vec<u8> },
  Write { id: usize, result: io::Result<i32>, buf: Vec<u8> },
  Send { id: usize, result: io::Result<i32>, buf: Vec<u8> },
  Socket { id: usize, result: io::Result<Resource> },
  Bind { id: usize, result: io::Result<()> },
  Listen { id: usize, result: io::Result<()> },
  Shutdown { id: usize, result: io::Result<()> },
  OpenAt { id: usize, result: io::Result<Resource> },
  StatAt { id: usize, result: io::Result<FileStat> },
  ReadlinkAt { id: usize, result: io::Result<i32>, buf: Vec<u8> },
  MkdirAt { id: usize, result: io::Result<()> },
  UnlinkAt { id: usize, result: io::Result<()> },
  RenameAt { id: usize, result: io::Result<()> },
  LinkAt { id: usize, result: io::Result<()> },
  Spawn { id: usize, result: io::Result<Pid> },
}

impl Event {
  fn id(&self) -> usize {
    match self {
      Self::Nop { id, .. }
      | Self::GetCwd { id, .. }
      | Self::Read { id, .. }
      | Self::Recv { id, .. }
      | Self::Write { id, .. }
      | Self::Send { id, .. }
      | Self::Socket { id, .. }
      | Self::Bind { id, .. }
      | Self::Listen { id, .. }
      | Self::Shutdown { id, .. }
      | Self::OpenAt { id, .. }
      | Self::StatAt { id, .. }
      | Self::ReadlinkAt { id, .. }
      | Self::MkdirAt { id, .. }
      | Self::UnlinkAt { id, .. }
      | Self::RenameAt { id, .. }
      | Self::LinkAt { id, .. }
      | Self::Spawn { id, .. } => *id,
    }
  }
}

#[derive(Clone, Debug)]
enum ScriptOp {
  Nop,
  GetCwd { len: usize },
  Read { len: usize },
  ReadOn { res: Resource, len: usize },
  Recv { len: usize },
  RecvOn { res: Resource, len: usize },
  Write { data: Vec<u8> },
  WriteOn { res: Resource, data: Vec<u8> },
  Send { data: Vec<u8> },
  SendOn { res: Resource, data: Vec<u8> },
  Socket { domain: SockDomain, ty: SockType, proto: SockProto },
  Bind { addr: SocketAddr },
  BindOn { res: Resource, addr: SocketAddr },
  Listen { backlog: i32 },
  ListenOn { res: Resource, backlog: i32 },
  Shutdown { how: i32 },
  ShutdownOn { res: Resource, how: i32 },
  OpenAt { path: CString, flags: i32, mode: u32 },
  StatAt { path: CString, follow_symlinks: bool },
  ReadlinkAt { path: CString, len: usize },
  MkdirAt { path: CString, mode: u32 },
  UnlinkAt { path: CString, flags: i32 },
  RenameAt { old_path: CString, new_path: CString },
  LinkAt { source_path: CString, new_path: CString, kind: LinkKind },
  Spawn { path: CString, argv: Vec<CString> },
}

#[derive(Debug)]
struct Prng {
  state: u64,
}

impl Prng {
  fn new(seed: u64) -> Self {
    let state = if seed == 0 { 0x9e37_79b9_7f4a_7c15 } else { seed };
    Self { state }
  }

  fn next_u64(&mut self) -> u64 {
    let mut x = self.state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    self.state = x;
    x.wrapping_mul(0x2545_f491_4f6c_dd1d)
  }

  fn range_usize(&mut self, upper_exclusive: usize) -> usize {
    if upper_exclusive <= 1 {
      0
    } else {
      (self.next_u64() % upper_exclusive as u64) as usize
    }
  }

  fn next_bool(&mut self) -> bool {
    self.next_u64() & 1 == 0
  }

  fn shuffle<T>(&mut self, items: &mut [T]) {
    for i in (1..items.len()).rev() {
      let j = self.range_usize(i + 1);
      items.swap(i, j);
    }
  }
}

#[test]
fn mixed_seeded_runtime_script_preserves_result_integrity() {
  for config in selected_configs() {
    eprintln!("ds_runtime: {}", config);
    let result = catch_unwind(AssertUnwindSafe(|| run_seed(config)));
    if let Err(payload) = result {
      eprintln!(
        "ds_runtime replay: LIO_DS_SEED={} LIO_DS_FAULT_EVERY={} LIO_DS_MAX_DELAY_TICKS={} cargo test -p lio --test ds_runtime -- --exact mixed_seeded_runtime_script_preserves_result_integrity",
        config.seed, config.fault_every, config.max_delay_ticks
      );
      if let Some(snapshot) = last_ds_trace_snapshot() {
        eprintln!("ds_runtime trace config: {}", snapshot.scenario);
        for line in snapshot.trace {
          eprintln!("ds_trace: {line}");
        }
      }
      resume_unwind(payload);
    }
  }
}

fn selected_configs() -> Vec<DSConfig> {
  if let Some(seed) = env_u64("LIO_DS_SEED") {
    return vec![DSConfig {
      seed,
      fault_every: env_u8("LIO_DS_FAULT_EVERY").unwrap_or(7),
      max_delay_ticks: env_u8("LIO_DS_MAX_DELAY_TICKS").unwrap_or(3),
      network_faults: DSNetworkFaults::Off,
    }];
  }

  if env_flag("LIO_DS_RANDOM") {
    let seed = randomish_seed();
    return vec![DSConfig {
      seed,
      fault_every: env_u8("LIO_DS_FAULT_EVERY").unwrap_or(7),
      max_delay_ticks: env_u8("LIO_DS_MAX_DELAY_TICKS").unwrap_or(3),
      network_faults: DSNetworkFaults::Off,
    }];
  }

  let runs = env_u64("LIO_DS_SMOKE_RUNS").unwrap_or(256);
  (0..runs)
    .map(|seed| DSConfig {
      seed,
      fault_every: 7,
      max_delay_ticks: 3,
      network_faults: DSNetworkFaults::Off,
    })
    .collect()
}

fn run_seed(config: DSConfig) {
  #[cfg(miri)]
  const INITIAL_SCRIPT_OPS: usize = 4;
  #[cfg(not(miri))]
  const INITIAL_SCRIPT_OPS: usize = 64;

  #[cfg(miri)]
  const MAX_EVENTS: usize = 8;
  #[cfg(not(miri))]
  const MAX_EVENTS: usize = 128;

  let mut lio =
    Lio::new_with_backend(DSBackend::with_config(config), 128).unwrap();
  let (tx, rx) = mpsc::channel();
  let seed = config.seed;
  let mut rng = Prng::new(seed ^ 0xa5a5_a5a5_5a5a_5a5a);
  let mut script = build_script(&mut rng, config, INITIAL_SCRIPT_OPS);
  rng.shuffle(&mut script);

  let mut expected = script.len();
  let mut next_id = expected;
  for (id, op) in script.into_iter().enumerate() {
    schedule_script_op(&lio, &tx, id, op);
  }

  let events = drain_all(
    &mut lio,
    &rx,
    &tx,
    &mut rng,
    &mut expected,
    &mut next_id,
    MAX_EVENTS,
  );
  drop(tx);
  assert_eq!(events.len(), expected, "seed {seed} did not resolve every op");

  let mut seen = BTreeSet::new();
  for event in &events {
    assert!(
      seen.insert(event.id()),
      "seed {seed} completed registration {} more than once",
      event.id()
    );
    assert_event_invariants(seed, event);
  }

  #[cfg(miri)]
  const QUIESCENCE_CHECKS: usize = 1;
  #[cfg(not(miri))]
  const QUIESCENCE_CHECKS: usize = 3;

  for _ in 0..QUIESCENCE_CHECKS {
    let progressed = lio.try_run().unwrap();
    assert_eq!(
      progressed, 0,
      "seed {seed} left ghost completions after draining",
    );
  }
}

fn build_script(
  rng: &mut Prng,
  config: DSConfig,
  count: usize,
) -> Vec<ScriptOp> {
  let mut script = Vec::with_capacity(count);
  for index in 0..count {
    let path = scripted_path(config.seed, index, "fuzz");
    let alt_path = scripted_path(config.seed, index, "alt");
    let len = 1 + rng.range_usize(128);
    let op = match rng.range_usize(18) {
      0 => ScriptOp::Nop,
      1 => ScriptOp::GetCwd { len },
      2 => ScriptOp::Read { len },
      3 => ScriptOp::Recv { len },
      4 => ScriptOp::Write { data: random_bytes(rng, len) },
      5 => ScriptOp::Send { data: random_bytes(rng, len) },
      6 => ScriptOp::Socket {
        domain: if rng.next_bool() {
          SockDomain::IPV4
        } else {
          SockDomain::IPV6
        },
        ty: if rng.next_bool() { SockType::STREAM } else { SockType::DGRAM },
        proto: if rng.next_bool() {
          SockProto::DEFAULT
        } else {
          SockProto::TCP
        },
      },
      7 => ScriptOp::Bind { addr: random_addr(rng) },
      8 => ScriptOp::Listen { backlog: 1 + rng.range_usize(32) as i32 },
      9 => ScriptOp::Shutdown {
        how: [libc::SHUT_RD, libc::SHUT_WR, libc::SHUT_RDWR]
          [rng.range_usize(3)],
      },
      10 => ScriptOp::OpenAt {
        path,
        flags: [libc::O_RDONLY, libc::O_WRONLY, libc::O_RDWR]
          [rng.range_usize(3)],
        mode: 0o600 + rng.range_usize(0o177) as u32,
      },
      11 => ScriptOp::StatAt { path, follow_symlinks: rng.next_bool() },
      12 => ScriptOp::ReadlinkAt { path, len },
      13 => {
        ScriptOp::MkdirAt { path, mode: 0o700 + rng.range_usize(0o77) as u32 }
      }
      14 => ScriptOp::UnlinkAt {
        path,
        flags: if rng.next_bool() { 0 } else { libc::AT_REMOVEDIR },
      },
      15 => ScriptOp::RenameAt { old_path: path, new_path: alt_path },
      16 => ScriptOp::LinkAt {
        source_path: path,
        new_path: alt_path,
        kind: if rng.next_bool() { LinkKind::Hard } else { LinkKind::Soft },
      },
      _ => ScriptOp::Spawn {
        path: CString::new("/bin/echo").unwrap(),
        argv: vec![CString::new("echo").unwrap(), path],
      },
    };
    script.push(op);
  }
  script
}

fn env_flag(name: &str) -> bool {
  match env::var(name) {
    Ok(value) => {
      matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES")
    }
    Err(_) => false,
  }
}

fn env_u64(name: &str) -> Option<u64> {
  env::var(name).ok()?.parse().ok()
}

fn env_u8(name: &str) -> Option<u8> {
  env::var(name).ok()?.parse().ok()
}

fn randomish_seed() -> u64 {
  let now = SystemTime::now()
    .duration_since(SystemTime::UNIX_EPOCH)
    .expect("system time before UNIX epoch")
    .as_nanos() as u64;
  now ^ (std::process::id() as u64).rotate_left(17)
}

fn schedule_script_op(
  lio: &Lio,
  tx: &mpsc::Sender<Event>,
  id: usize,
  op: ScriptOp,
) {
  let stdin = Resource::stdin();
  let stdout = Resource::stdout();
  let cwd = Resource::cwd();

  match op {
    ScriptOp::Nop => {
      let tx = tx.clone();
      api::nop().with_lio(lio).when_done(move |result| {
        tx.send(Event::Nop { id, result }).unwrap();
      });
    }
    ScriptOp::GetCwd { len } => {
      let tx = tx.clone();
      api::getcwd(vec![0; len]).with_lio(lio).when_done(
        move |(result, buf)| {
          tx.send(Event::GetCwd { id, result, buf }).unwrap();
        },
      );
    }
    ScriptOp::Read { len } => {
      let tx = tx.clone();
      api::read(&stdin, vec![0; len]).with_lio(lio).when_done(
        move |(result, buf)| {
          tx.send(Event::Read { id, result, buf }).unwrap();
        },
      );
    }
    ScriptOp::ReadOn { res, len } => {
      let tx = tx.clone();
      api::read(&res, vec![0; len]).with_lio(lio).when_done(
        move |(result, buf)| {
          tx.send(Event::Read { id, result, buf }).unwrap();
        },
      );
    }
    ScriptOp::Recv { len } => {
      let tx = tx.clone();
      api::recv(&stdin, vec![0; len], None).with_lio(lio).when_done(
        move |(result, buf)| {
          tx.send(Event::Recv { id, result, buf }).unwrap();
        },
      );
    }
    ScriptOp::RecvOn { res, len } => {
      let tx = tx.clone();
      api::recv(&res, vec![0; len], None).with_lio(lio).when_done(
        move |(result, buf)| {
          tx.send(Event::Recv { id, result, buf }).unwrap();
        },
      );
    }
    ScriptOp::Write { data } => {
      let tx = tx.clone();
      api::write(&stdout, data).with_lio(lio).when_done(
        move |(result, buf)| {
          tx.send(Event::Write { id, result, buf }).unwrap();
        },
      );
    }
    ScriptOp::WriteOn { res, data } => {
      let tx = tx.clone();
      api::write(&res, data).with_lio(lio).when_done(move |(result, buf)| {
        tx.send(Event::Write { id, result, buf }).unwrap();
      });
    }
    ScriptOp::Send { data } => {
      let tx = tx.clone();
      api::send(&stdout, data, None).with_lio(lio).when_done(
        move |(result, buf)| {
          tx.send(Event::Send { id, result, buf }).unwrap();
        },
      );
    }
    ScriptOp::SendOn { res, data } => {
      let tx = tx.clone();
      api::send(&res, data, None).with_lio(lio).when_done(
        move |(result, buf)| {
          tx.send(Event::Send { id, result, buf }).unwrap();
        },
      );
    }
    ScriptOp::Socket { domain, ty, proto } => {
      let tx = tx.clone();
      api::socket(domain, ty, proto).with_lio(lio).when_done(move |result| {
        tx.send(Event::Socket { id, result }).unwrap();
      });
    }
    ScriptOp::Bind { addr } => {
      let tx = tx.clone();
      api::bind(&stdin, addr).with_lio(lio).when_done(move |result| {
        tx.send(Event::Bind { id, result }).unwrap();
      });
    }
    ScriptOp::BindOn { res, addr } => {
      let tx = tx.clone();
      api::bind(&res, addr).with_lio(lio).when_done(move |result| {
        tx.send(Event::Bind { id, result }).unwrap();
      });
    }
    ScriptOp::Listen { backlog } => {
      let tx = tx.clone();
      api::listen(&stdin, backlog).with_lio(lio).when_done(move |result| {
        tx.send(Event::Listen { id, result }).unwrap();
      });
    }
    ScriptOp::ListenOn { res, backlog } => {
      let tx = tx.clone();
      api::listen(&res, backlog).with_lio(lio).when_done(move |result| {
        tx.send(Event::Listen { id, result }).unwrap();
      });
    }
    ScriptOp::Shutdown { how } => {
      let tx = tx.clone();
      api::shutdown(&stdin, how).with_lio(lio).when_done(move |result| {
        tx.send(Event::Shutdown { id, result }).unwrap();
      });
    }
    ScriptOp::ShutdownOn { res, how } => {
      let tx = tx.clone();
      api::shutdown(&res, how).with_lio(lio).when_done(move |result| {
        tx.send(Event::Shutdown { id, result }).unwrap();
      });
    }
    ScriptOp::OpenAt { path, flags, mode } => {
      let tx = tx.clone();
      api::openat(&cwd, path, flags, mode).with_lio(lio).when_done(
        move |result| {
          tx.send(Event::OpenAt { id, result }).unwrap();
        },
      );
    }
    ScriptOp::StatAt { path, follow_symlinks } => {
      let tx = tx.clone();
      api::statat(&cwd, path, follow_symlinks).with_lio(lio).when_done(
        move |result| {
          tx.send(Event::StatAt { id, result }).unwrap();
        },
      );
    }
    ScriptOp::ReadlinkAt { path, len } => {
      let tx = tx.clone();
      api::readlinkat(&cwd, path, vec![0; len]).with_lio(lio).when_done(
        move |(result, buf)| {
          tx.send(Event::ReadlinkAt { id, result, buf }).unwrap();
        },
      );
    }
    ScriptOp::MkdirAt { path, mode } => {
      let tx = tx.clone();
      api::mkdirat(&cwd, path, mode).with_lio(lio).when_done(move |result| {
        tx.send(Event::MkdirAt { id, result }).unwrap();
      });
    }
    ScriptOp::UnlinkAt { path, flags } => {
      let tx = tx.clone();
      api::unlinkat(&cwd, path, flags).with_lio(lio).when_done(move |result| {
        tx.send(Event::UnlinkAt { id, result }).unwrap();
      });
    }
    ScriptOp::RenameAt { old_path, new_path } => {
      let tx = tx.clone();
      api::renameat(&cwd, old_path, &cwd, new_path).with_lio(lio).when_done(
        move |result| {
          tx.send(Event::RenameAt { id, result }).unwrap();
        },
      );
    }
    ScriptOp::LinkAt { source_path, new_path, kind } => {
      let tx = tx.clone();
      api::linkat(&cwd, source_path, &cwd, new_path, kind)
        .with_lio(lio)
        .when_done(move |result| {
          tx.send(Event::LinkAt { id, result }).unwrap();
        });
    }
    ScriptOp::Spawn { path, argv } => {
      let tx = tx.clone();
      api::spawn(path, argv, None).with_lio(lio).when_done(move |result| {
        tx.send(Event::Spawn { id, result }).unwrap();
      });
    }
  }
}

fn drain_all(
  lio: &mut Lio,
  rx: &mpsc::Receiver<Event>,
  tx: &mpsc::Sender<Event>,
  rng: &mut Prng,
  expected: &mut usize,
  next_id: &mut usize,
  max_events: usize,
) -> Vec<Event> {
  #[cfg(miri)]
  const DRAIN_STEPS: usize = 64;
  #[cfg(not(miri))]
  const DRAIN_STEPS: usize = 1024;

  let mut events = Vec::with_capacity(*expected);

  for _ in 0..DRAIN_STEPS {
    while let Ok(event) = rx.try_recv() {
      maybe_schedule_followups(
        lio, tx, rng, next_id, expected, max_events, &event,
      );
      events.push(event);
      if events.len() == *expected {
        return events;
      }
    }

    drive_runtime_step(lio);
  }

  panic!(
    "runtime failed to drain scripted work: got {} of {} events",
    events.len(),
    *expected
  );
}

#[cfg(miri)]
fn drive_runtime_step(lio: &mut Lio) {
  lio.run_timeout(Duration::from_nanos(1)).unwrap();
}

#[cfg(not(miri))]
fn drive_runtime_step(lio: &mut Lio) {
  lio.run_timeout(Duration::from_millis(1)).unwrap();
}

fn maybe_schedule_followups(
  lio: &Lio,
  tx: &mpsc::Sender<Event>,
  rng: &mut Prng,
  next_id: &mut usize,
  expected: &mut usize,
  max_events: usize,
  event: &Event,
) {
  #[cfg(miri)]
  const MAX_FOLLOWUPS_PER_EVENT: usize = 1;
  #[cfg(not(miri))]
  const MAX_FOLLOWUPS_PER_EVENT: usize = 3;

  if *expected >= max_events {
    return;
  }

  let Some(resource) = (match event {
    Event::Socket { result: Ok(resource), .. }
    | Event::OpenAt { result: Ok(resource), .. } => Some(resource.clone()),
    _ => None,
  }) else {
    return;
  };

  let budget = (1 + rng.range_usize(MAX_FOLLOWUPS_PER_EVENT))
    .min(max_events.saturating_sub(*expected));
  for _ in 0..budget {
    let op = match rng.range_usize(7) {
      0 => {
        ScriptOp::ReadOn { res: resource.clone(), len: 1 + rng.range_usize(96) }
      }
      1 => {
        ScriptOp::RecvOn { res: resource.clone(), len: 1 + rng.range_usize(96) }
      }
      2 => {
        let len = 1 + rng.range_usize(96);
        ScriptOp::WriteOn {
          res: resource.clone(),
          data: random_bytes(rng, len),
        }
      }
      3 => {
        let len = 1 + rng.range_usize(96);
        ScriptOp::SendOn { res: resource.clone(), data: random_bytes(rng, len) }
      }
      4 => ScriptOp::BindOn { res: resource.clone(), addr: random_addr(rng) },
      5 => ScriptOp::ListenOn {
        res: resource.clone(),
        backlog: 1 + rng.range_usize(16) as i32,
      },
      _ => ScriptOp::ShutdownOn {
        res: resource.clone(),
        how: [libc::SHUT_RD, libc::SHUT_WR, libc::SHUT_RDWR]
          [rng.range_usize(3)],
      },
    };
    let id = *next_id;
    *next_id += 1;
    *expected += 1;
    schedule_script_op(lio, tx, id, op);
  }
}

fn assert_event_invariants(seed: u64, event: &Event) {
  match event {
    Event::Nop { result, .. } => {
      assert!(
        result.is_ok(),
        "seed {seed} produced invalid nop result: {result:?}"
      );
    }
    Event::GetCwd { result, buf, .. }
    | Event::ReadlinkAt { result, buf, .. } => match result {
      Ok(len) => {
        assert_buf_result(seed, *len, buf);
        if *len > 0 {
          assert_ne!(
            buf[0], 0,
            "seed {seed} returned an empty-looking successful path buffer"
          );
        }
      }
      Err(err) => {
        assert!(
          matches!(
            err.raw_os_error(),
            Some(libc::ERANGE) | Some(libc::ENOENT) | Some(libc::EINVAL)
          ),
          "seed {seed} produced invalid path-buffer error: {err:?}"
        );
      }
    },
    Event::Read { result, buf, .. } | Event::Recv { result, buf, .. } => {
      match result {
        Ok(len) => assert_buf_result(seed, *len, buf),
        Err(err) => {
          assert!(
            matches!(
              err.raw_os_error(),
              Some(libc::EIO)
                | Some(libc::EBADF)
                | Some(libc::EINVAL)
                | Some(libc::ENOTSUP)
                | Some(libc::ECONNRESET)
                | Some(libc::EAGAIN)
            ),
            "seed {seed} produced invalid read-style error: {err:?}"
          );
        }
      }
    }
    Event::Write { result, buf, .. } | Event::Send { result, buf, .. } => {
      match result {
        Ok(len) => {
          assert!(
            (*len as usize) <= buf.len(),
            "seed {seed} returned byte count {} > {}",
            len,
            buf.len()
          );
        }
        Err(err) => {
          assert!(
            matches!(
              err.raw_os_error(),
              Some(libc::EIO)
                | Some(libc::EBADF)
                | Some(libc::EINVAL)
                | Some(libc::EAGAIN)
                | Some(libc::EPIPE)
                | Some(libc::ECONNRESET)
                | Some(libc::ENOSPC)
                | Some(libc::EFBIG)
            ),
            "seed {seed} produced invalid write-style error: {err:?}"
          );
        }
      }
    }
    Event::Socket { result, .. } | Event::OpenAt { result, .. } => match result
    {
      Ok(resource) => {
        assert!(
          resource.as_fd().as_raw_fd() >= 0,
          "seed {seed} returned negative synthetic resource"
        );
      }
      Err(err) => {
        assert!(
          matches!(
            err.raw_os_error(),
            Some(libc::EMFILE)
              | Some(libc::ENFILE)
              | Some(libc::ENOENT)
              | Some(libc::EACCES)
              | Some(libc::EINVAL)
          ),
          "seed {seed} produced invalid resource-creation error: {err:?}"
        );
      }
    },
    Event::Bind { result, .. }
    | Event::Listen { result, .. }
    | Event::Shutdown { result, .. }
    | Event::MkdirAt { result, .. }
    | Event::UnlinkAt { result, .. }
    | Event::RenameAt { result, .. }
    | Event::LinkAt { result, .. } => {
      if let Err(err) = result {
        assert!(
          matches!(
            err.raw_os_error(),
            Some(libc::EADDRINUSE)
              | Some(libc::EACCES)
              | Some(libc::EINVAL)
              | Some(libc::ENOTSOCK)
              | Some(libc::ENOENT)
              | Some(libc::EEXIST)
          ),
          "seed {seed} produced invalid unit-op error: {err:?}"
        );
      }
    }
    Event::StatAt { result, .. } => match result {
      Ok(stat) => {
        assert!(stat.nlink >= 1, "seed {seed} returned invalid stat nlink");
        assert!(
          stat.is_file() || stat.is_dir() || stat.is_symlink(),
          "seed {seed} returned unexpected file type: {stat:?}"
        );
      }
      Err(err) => {
        assert!(
          matches!(err.raw_os_error(), Some(libc::ENOENT) | Some(libc::EACCES)),
          "seed {seed} produced invalid stat error: {err:?}"
        );
      }
    },
    Event::Spawn { result, .. } => match result {
      Ok(pid) => {
        assert!(pid.as_raw() > 0, "seed {seed} returned non-positive pid");
      }
      Err(err) => {
        assert!(
          matches!(err.raw_os_error(), Some(libc::ENOENT) | Some(libc::EACCES)),
          "seed {seed} produced invalid spawn error: {err:?}"
        );
      }
    },
  }
}

fn assert_buf_result(seed: u64, len: i32, buf: &[u8]) {
  assert!(len >= 0, "seed {seed} returned negative success length {len}");
  assert!(
    (len as usize) <= buf.len(),
    "seed {seed} returned byte count {} > {}",
    len,
    buf.len()
  );
}

fn random_bytes(rng: &mut Prng, len: usize) -> Vec<u8> {
  (0..len).map(|_| rng.next_u64() as u8).collect()
}

fn random_addr(rng: &mut Prng) -> SocketAddr {
  SocketAddr::V4(SocketAddrV4::new(
    Ipv4Addr::new(127, 0, 0, (rng.range_usize(253) as u8).saturating_add(1)),
    10_000 + rng.range_usize(40_000) as u16,
  ))
}

fn scripted_path(seed: u64, index: usize, tag: &str) -> CString {
  CString::new(format!("/tmp/lio-fuzz-{tag}-{seed:016x}-{index:04x}")).unwrap()
}
